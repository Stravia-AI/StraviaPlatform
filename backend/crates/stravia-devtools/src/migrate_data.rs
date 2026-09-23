//! 离线复制受支持的迁移前缀；目标副本由正常启动流程升级，源数据不改写。
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use clap::Args;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};

#[derive(Args)]
pub struct MigrateDataArgs {
    #[arg(long)]
    from: PathBuf,
    #[arg(long)]
    to: PathBuf,
    /// Deduplicate retained debug traces in the destination copy, then reclaim SQLite free pages.
    #[arg(long)]
    optimize_storage: bool,
    #[arg(long, requires = "source_stopped")]
    apply: bool,
    /// Confirm every host using the source is stopped.
    #[arg(long, requires = "apply")]
    source_stopped: bool,
}

struct Plan {
    from: PathBuf,
    to: PathBuf,
    roots: BTreeSet<PathBuf>,
    copies: Vec<(PathBuf, PathBuf)>,
    database: Option<PathBuf>,
    skipped: Vec<PathBuf>,
}

pub async fn run(args: MigrateDataArgs) -> Result<()> {
    ensure!(
        !args.apply || args.source_stopped,
        "--apply requires --source-stopped"
    );
    let plan = Plan::build(&args)?;
    println!("{} -> {}", plan.from.display(), plan.to.display());
    for (source, target) in &plan.copies {
        println!("Copy {} -> {}", source.display(), target.display());
    }
    if let Some(db) = &plan.database {
        println!(
            "SQLite consistent snapshot {} -> db/gateway.db (integrity_check required)",
            db.display()
        );
    }
    for path in &plan.skipped {
        println!("Skip rebuildable lock/SQLite sidecar: {}", path.display());
    }
    if args.optimize_storage {
        ensure!(
            plan.database.is_some(),
            "--optimize-storage requires a local SQLite database; external databases are never modified"
        );
        println!(
            "Destination only: deduplicate retained debug traces, verify restoration, and reclaim SQLite free pages."
        );
    }
    println!("No PostgreSQL connection or S3 operation. Source data is never rewritten.");
    println!(
        "Apply may create .instance.lock in source roots and retains a sibling destination reservation lock; these coordination files contain no data."
    );
    if !args.apply {
        println!("Plan only. Stop all source hosts, then repeat with --apply --source-stopped.");
        return Ok(());
    }
    // Hosts and other migration processes share the same source root lock; the
    // explicit operator stop confirmation is essential.
    let _source_locks: Vec<File> = plan
        .roots
        .iter()
        .map(|root| lock(root))
        .collect::<Result<_>>()?;
    let parent = plan.to.parent().context("target needs a parent")?;
    ensure!(parent.is_dir(), "target parent must already exist");
    let reservation = parent.join(format!(
        ".{}.migration.lock",
        plan.to
            .file_name()
            .context("target needs a name")?
            .to_string_lossy()
    ));
    reject_links(&reservation)?;
    let _reservation = lock_file(&reservation)?;
    // Replan under locks: this also checks source and target have not changed during confirmation.
    let plan = Plan::build(&args)?;
    if let Some(database) = &plan.database {
        check_source_schema(database).await?;
    }
    let staging = tempfile::Builder::new()
        .prefix(".stravia-migrate-")
        .tempdir_in(parent)?;
    secure_staging(staging.path())?;
    plan.populate(staging.path()).await?;
    if args.optimize_storage {
        let report = stravia_core::storage::maintenance::optimize_data_copy(staging.path()).await?;
        println!("{}", serde_json::to_string_pretty(&report)?);
    }
    // A failed population leaves no target and TempDir removes the private staging tree.
    // The sibling reservation serializes publication. An open child lock would
    // prevent directory rename on Windows; no host can see the target before rename.
    target_empty(&plan.to)?;
    if plan.to.exists() {
        fs::remove_dir(&plan.to).context("target is no longer empty")?;
    }
    fs::rename(staging.path(), &plan.to).context("could not atomically publish migration")?;
    println!(
        "Published {}. Original data retained; start hosts with this data root.",
        plan.to.display()
    );
    Ok(())
}

impl Plan {
    fn build(args: &MigrateDataArgs) -> Result<Self> {
        let from = absolute(&args.from)?;
        let to = absolute(&args.to)?;
        ensure!(from.is_dir(), "source must be an existing directory");
        reject_links(&from)?;
        reject_links(&to)?;
        disjoint(&from, &to)?;
        target_empty(&to)?;
        let mut plan = Self {
            from: from.clone(),
            to,
            roots: BTreeSet::from([from.clone()]),
            copies: Vec::new(),
            database: None,
            skipped: Vec::new(),
        };
        // 旧布局条目只可能来自本工具无法升级的版本，直接拒绝而不是猜测映射。
        ensure!(
            ![
                "gateway.db",
                "catalog",
                "observation-debug",
                "web-access",
                "desktop-port.json",
                "desktop-webview",
            ]
            .iter()
            .any(|name| from.join(name).exists()),
            "source uses a data layout from an incompatible older version; this release cannot upgrade it"
        );
        let config_path = from.join("server.toml");
        let mut sqlite = true;
        if config_path.is_file() {
            // Never include TOML parser errors: they may quote passwords or connection strings.
            let text = fs::read_to_string(&config_path).context("cannot read server config")?;
            let value: toml::Value = toml::from_str(&text)
                .map_err(|_| anyhow::anyhow!("invalid server TOML (contents withheld)"))?;
            let database = value
                .get("database")
                .and_then(toml::Value::as_table)
                .context("config requires a database table")?;
            ensure!(
                database.get("path").is_none(),
                "server.toml uses the removed database.path key; the source predates the current layout"
            );
            match database.get("backend").and_then(toml::Value::as_str) {
                Some("sqlite") => {}
                Some("postgres") => sqlite = false,
                _ => bail!("unsupported database backend"),
            }
            plan.add(&config_path, Path::new("server.toml"))?;
        }
        let modern = from.join("db").exists()
            || ["cache", "diagnostics", "plugins", "state"]
                .iter()
                .any(|path| from.join(path).exists());
        ensure!(
            modern || config_path.is_file(),
            "unrecognized source layout"
        );
        let db = from.join("db/gateway.db");
        reject_links(&db)?;
        if sqlite {
            ensure!(db.is_file(), "SQLite source is missing: {}", db.display());
            plan.database = Some(db.clone());
        } else {
            ensure!(
                !db.exists() && !from.join("db").exists(),
                "PostgreSQL configuration conflicts with local SQLite data"
            );
        }
        let mut allowed = BTreeSet::from(["server.toml".to_owned(), ".instance.lock".to_owned()]);
        for (name, children) in [
            ("db", vec!["gateway.db", "gateway.db-wal", "gateway.db-shm"]),
            ("cache", vec!["catalog"]),
            ("diagnostics", vec!["observation-debug"]),
            ("plugins", vec!["artifacts"]),
            (
                "state",
                vec!["desktop-port.json", "web-access", "desktop-webview"],
            ),
        ] {
            allowed.insert(name.to_owned());
            let dir = from.join(name);
            if dir.exists() {
                check_children(&dir, &children.iter().map(|s| s.to_string()).collect())?;
                if name != "db" {
                    for child in &children {
                        plan.add(&dir.join(child), &Path::new(name).join(child))?;
                    }
                }
            }
        }
        allowed.insert("artifacts".to_owned());
        let artifacts = from.join("artifacts");
        if artifacts.exists() {
            check_children(
                &artifacts,
                &BTreeSet::from(["objects".into(), "staging".into(), "locks".into()]),
            )?;
            plan.add(&artifacts.join("objects"), Path::new("artifacts/objects"))?;
            plan.add(&artifacts.join("staging"), Path::new("artifacts/staging"))?;
            if artifacts.join("locks").exists() {
                plan.skipped.push(artifacts.join("locks"));
            }
        }
        if sqlite {
            for suffix in ["-wal", "-shm"] {
                let sidecar = sidecar(&db, suffix);
                if sidecar.exists() {
                    plan.skipped.push(sidecar);
                }
            }
            ensure!(
                !sidecar(&db, "-journal").exists(),
                "rollback journal present; cleanly stop/checkpoint source before migration"
            );
        }
        check_children(&from, &allowed)?;
        scan_tree(&from)?;
        if from.join(".instance.lock").exists() {
            plan.skipped.push(from.join(".instance.lock"));
        }
        Ok(plan)
    }

    fn add(&mut self, source: &Path, target: &Path) -> Result<()> {
        if source.exists() {
            ensure!(
                !self.copies.iter().any(|(_, dest)| dest == target),
                "ambiguous target mapping"
            );
            self.copies.push((source.to_owned(), target.to_owned()));
        }
        Ok(())
    }

    async fn populate(&self, target: &Path) -> Result<()> {
        for (source, relative) in &self.copies {
            copy_tree(source, &target.join(relative))?;
        }
        if let Some(database) = &self.database {
            let scratch = tempfile::Builder::new()
                .prefix(".sqlite-snapshot-")
                .tempdir_in(target)?;
            private_dir(scratch.path())?;
            let scratch_db = scratch.path().join("source.db");
            copy_tree(database, &scratch_db)?;
            // WAL is replayed by SQLite on the scratch copy; SHM is rebuilt, never copied.
            let wal = sidecar(database, "-wal");
            if wal.exists() {
                copy_tree(&wal, &sidecar(&scratch_db, "-wal"))?;
            }
            let destination = target.join("db/gateway.db");
            private_dir(destination.parent().context("database parent missing")?)?;
            snapshot(&scratch_db, &destination).await?;
            private_file(&destination)?;
        }
        Ok(())
    }
}

/// Verify source history without applying migrations or changing the source.
async fn check_source_schema(database: &Path) -> Result<()> {
    let mut connection = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(database)
            .read_only(true)
            .create_if_missing(false),
    )
    .await
    .context("open source SQLite database")?;
    let result = stravia_core::migrations::check_sqlite_source_history(&mut connection).await;
    connection.close().await?;
    result.context("source SQLite migration history is not a supported prefix")
}

async fn snapshot(source: &Path, destination: &Path) -> Result<()> {
    let options = SqliteConnectOptions::new()
        .filename(source)
        .create_if_missing(false);
    let mut connection = SqliteConnection::connect_with(&options)
        .await
        .context("open SQLite scratch snapshot")?;
    let destination = destination
        .to_str()
        .context("SQLite destination must be UTF-8")?;
    sqlx::query("VACUUM INTO ?")
        .bind(destination)
        .execute(&mut connection)
        .await
        .context("SQLite consistent snapshot failed")?;
    connection.close().await?;
    let mut connection = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(destination)
            .read_only(true),
    )
    .await?;
    let rows: Vec<(String,)> = sqlx::query_as("PRAGMA integrity_check")
        .fetch_all(&mut connection)
        .await?;
    ensure!(
        rows.len() == 1 && rows[0].0 == "ok",
        "SQLite snapshot integrity_check failed"
    );
    connection.close().await?;
    Ok(())
}

fn absolute(path: &Path) -> Result<PathBuf> {
    let text = path.to_str().context("paths must be UTF-8")?;
    let expanded = PathBuf::from(shellexpand::tilde(text).as_ref());
    ensure!(
        !expanded
            .components()
            .any(|c| matches!(c, Component::ParentDir)),
        "parent traversal (..) is not allowed"
    );
    let absolute = std::path::absolute(expanded)?;
    // Reject rather than normalize parent components, avoiding symlink/.. ambiguity.
    ensure!(
        !absolute
            .components()
            .any(|c| matches!(c, Component::ParentDir)),
        "parent traversal (..) is not allowed"
    );
    reject_links(&absolute)?;
    // Canonicalize the existing ancestor to detect Windows casing/short-name aliases.
    let mut ancestor = absolute.as_path();
    let mut suffix = Vec::new();
    while !ancestor.exists() {
        suffix.push(
            ancestor
                .file_name()
                .context("path has no existing ancestor")?
                .to_owned(),
        );
        ancestor = ancestor.parent().context("path has no parent")?;
    }
    let mut resolved = fs::canonicalize(ancestor)?;
    for part in suffix.into_iter().rev() {
        resolved.push(part);
    }
    Ok(resolved)
}

fn disjoint(left: &Path, right: &Path) -> Result<()> {
    ensure!(
        !left.starts_with(right) && !right.starts_with(left),
        "source and target must be distinct, non-nested roots"
    );
    Ok(())
}

fn reject_links(path: &Path) -> Result<()> {
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(meta) => {
                ensure!(
                    !meta.file_type().is_symlink(),
                    "symbolic link rejected: {}",
                    ancestor.display()
                );
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    ensure!(
                        meta.file_attributes() & 0x400 == 0,
                        "reparse point rejected: {}",
                        ancestor.display()
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn scan_tree(root: &Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        reject_links(entry.path())?;
        ensure!(
            entry.file_type().is_dir() || entry.file_type().is_file(),
            "special file rejected: {}",
            entry.path().display()
        );
    }
    Ok(())
}

fn check_children(root: &Path, allowed: &BTreeSet<String>) -> Result<()> {
    let mut unknown = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !allowed.contains(&entry.file_name().to_string_lossy().into_owned()) {
            unknown.push(entry.path());
        }
    }
    ensure!(
        unknown.is_empty(),
        "unknown source entries (nothing copied): {}",
        unknown
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(())
}

fn target_empty(target: &Path) -> Result<()> {
    reject_links(target)?;
    if target.exists() {
        ensure!(
            target.is_dir() && fs::read_dir(target)?.next().is_none(),
            "target must not exist or must be empty"
        );
    }
    Ok(())
}

fn sidecar(database: &Path, suffix: &str) -> PathBuf {
    let mut name = database.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn lock(root: &Path) -> Result<File> {
    lock_file(&root.join(".instance.lock"))
}
fn lock_file(path: &Path) -> Result<File> {
    reject_links(path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    file.try_lock()
        .context("data root or migration is already locked")?;
    Ok(file)
}

fn secure_staging(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(windows)]
    {
        // Use the actual process identity, not spoofable USERNAME environment data.
        // Children inherit only this identity's full-control ACE, including secret files.
        let identity = std::process::Command::new("whoami.exe")
            .output()
            .context("resolve Windows migration identity")?;
        ensure!(
            identity.status.success(),
            "cannot resolve Windows migration identity"
        );
        let identity = String::from_utf8(identity.stdout).context("invalid Windows identity")?;
        let grant = format!("{}:(OI)(CI)F", identity.trim());
        let result = std::process::Command::new("icacls.exe")
            .arg(path)
            .args(["/inheritance:r", "/grant:r", &grant])
            .output()
            .context("restrict staging ACL")?;
        ensure!(
            result.status.success(),
            "cannot restrict staging ACL; migration not applied"
        );
    }
    Ok(())
}

fn private_dir(path: &Path) -> Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    if let Some(parent) = path.parent()
        && !parent.exists()
    {
        private_dir(parent)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(path)?;
    }
    #[cfg(not(unix))]
    {
        fs::create_dir(path)?;
    }
    Ok(())
}

fn private_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    reject_links(source)?;
    if source.is_dir() {
        private_dir(target)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(&entry.path(), &target.join(entry.file_name()))?;
        }
    } else {
        ensure!(source.is_file(), "unsupported source file");
        private_dir(target.parent().context("copy parent missing")?)?;
        let mut input = File::open(source)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = options.open(target)?;
        std::io::copy(&mut input, &mut output)?;
        output.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(root: &Path) -> MigrateDataArgs {
        MigrateDataArgs {
            from: root.join("old"),
            to: root.join("new"),
            optimize_storage: false,
            apply: false,
            source_stopped: false,
        }
    }

    fn modern(root: &Path) -> Result<MigrateDataArgs> {
        let args = args(root);
        fs::create_dir_all(args.from.join("db"))?;
        fs::write(args.from.join("db/gateway.db"), b"not yet a database")?;
        Ok(args)
    }

    async fn create_database(path: &Path) -> Result<()> {
        let mut connection = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true),
        )
        .await?;
        sqlx::query("CREATE TABLE sample (value TEXT NOT NULL)")
            .execute(&mut connection)
            .await?;
        sqlx::query(
            "CREATE TABLE _sqlx_migrations (\
             version BIGINT PRIMARY KEY, description TEXT NOT NULL, \
             installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP, \
             success BOOLEAN NOT NULL, checksum BLOB NOT NULL, execution_time BIGINT NOT NULL)",
        )
        .execute(&mut connection)
        .await?;
        let baseline = sqlx::migrate!("../stravia-core/migrations/sqlite");
        let baseline = baseline.iter().next().expect("frozen baseline migration");
        sqlx::query(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
             VALUES (?, 'baseline', 1, ?, 0)",
        )
        .bind(baseline.version)
        .bind(baseline.checksum.as_ref())
        .execute(&mut connection)
        .await?;
        connection.close().await?;
        Ok(())
    }

    #[test]
    fn rejects_legacy_layout_and_unknown_files() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let args = args(temp.path());
        fs::create_dir(&args.from)?;
        fs::write(args.from.join("gateway.db"), b"old root database")?;
        let error = Plan::build(&args)
            .err()
            .context("legacy layout accepted")?
            .to_string();
        assert!(error.contains("incompatible older version"), "{error}");

        let args = modern(temp.path())?;
        fs::remove_file(args.from.join("gateway.db"))?;
        fs::write(args.from.join("unmapped-secret"), b"secret")?;
        let error = Plan::build(&args)
            .err()
            .context("unknown entry accepted")?
            .to_string();
        assert!(error.contains("unmapped-secret"));
        assert!(!args.to.exists());
        Ok(())
    }

    #[test]
    fn rejects_removed_config_path_key() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let args = modern(temp.path())?;
        fs::write(
            args.from.join("server.toml"),
            "[database]\nbackend = 'sqlite'\npath = 'gateway.db'\n",
        )?;
        let error = Plan::build(&args)
            .err()
            .context("legacy config accepted")?
            .to_string();
        assert!(error.contains("database.path"), "{error}");
        Ok(())
    }

    #[test]
    fn rejects_target_escape_and_conflicts() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut args = modern(temp.path())?;
        args.to = args.from.join("nested");
        assert!(Plan::build(&args).is_err());
        args.to = temp.path().join("old/../new");
        assert!(Plan::build(&args).is_err());
        args.to = temp.path().join("new");
        fs::create_dir(&args.to)?;
        fs::write(args.to.join("valuable"), b"keep")?;
        assert!(Plan::build(&args).is_err());
        Ok(())
    }

    #[test]
    fn modern_layout_maps_managed_entries() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let args = modern(temp.path())?;
        fs::create_dir_all(args.from.join("cache/catalog"))?;
        fs::create_dir_all(args.from.join("diagnostics/observation-debug"))?;
        fs::create_dir_all(args.from.join("state/web-access"))?;
        fs::create_dir_all(args.from.join("plugins/artifacts"))?;
        let plan = Plan::build(&args)?;
        for target in [
            "cache/catalog",
            "diagnostics/observation-debug",
            "state/web-access",
            "plugins/artifacts",
        ] {
            assert!(
                plan.copies
                    .iter()
                    .any(|(_, path)| path == Path::new(target)),
                "missing {target}"
            );
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_inside_copied_tree() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let args = modern(temp.path())?;
        fs::create_dir_all(args.from.join("cache/catalog"))?;
        std::os::unix::fs::symlink(temp.path(), args.from.join("cache/catalog/escape"))?;
        assert!(Plan::build(&args).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn rejects_database_without_current_history() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let args = modern(temp.path())?;
        let database = args.from.join("db/gateway.db");
        fs::remove_file(&database)?;
        create_database(&database).await?;
        let mut connection = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&database)
                .create_if_missing(false),
        )
        .await?;
        sqlx::query("DELETE FROM _sqlx_migrations")
            .execute(&mut connection)
            .await?;
        sqlx::query("INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES (2, 'historical', 1, X'00', 0)")
            .execute(&mut connection)
            .await?;
        connection.close().await?;
        check_source_schema(&database)
            .await
            .expect_err("a migration history with a missing baseline must be rejected");
        Ok(())
    }

    #[tokio::test]
    async fn rejects_source_with_tampered_migration_checksum() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let args = modern(temp.path())?;
        let database = args.from.join("db/gateway.db");
        fs::remove_file(&database)?;
        create_database(&database).await?;
        let mut connection = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&database)
                .create_if_missing(false),
        )
        .await?;
        sqlx::query("UPDATE _sqlx_migrations SET checksum=X'00' WHERE version=1")
            .execute(&mut connection)
            .await?;
        connection.close().await?;
        check_source_schema(&database)
            .await
            .expect_err("a tampered migration checksum must be rejected");
        Ok(())
    }

    #[tokio::test]
    async fn snapshot_includes_committed_wal_without_changing_source() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let args = args(temp.path());
        fs::create_dir_all(args.from.join("db"))?;
        let database = args.from.join("db/gateway.db");
        let mut writer = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&database)
                .create_if_missing(true),
        )
        .await?;
        sqlx::query("PRAGMA journal_mode=WAL")
            .execute(&mut writer)
            .await?;
        sqlx::query("PRAGMA wal_autocheckpoint=0")
            .execute(&mut writer)
            .await?;
        sqlx::query("CREATE TABLE sample (value TEXT NOT NULL)")
            .execute(&mut writer)
            .await?;
        sqlx::query("INSERT INTO sample VALUES ('committed in WAL')")
            .execute(&mut writer)
            .await?;
        let before = fs::read(&database)?;
        let wal_before = fs::read(sidecar(&database, "-wal"))?;
        assert!(!wal_before.is_empty());
        let plan = Plan::build(&args)?;
        fs::create_dir(&args.to)?;
        plan.populate(&args.to).await?;
        let mut result = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(args.to.join("db/gateway.db"))
                .read_only(true),
        )
        .await?;
        let value: (String,) = sqlx::query_as("SELECT value FROM sample")
            .fetch_one(&mut result)
            .await?;
        assert_eq!(value.0, "committed in WAL");
        result.close().await?;
        assert_eq!(fs::read(&database)?, before);
        assert_eq!(fs::read(sidecar(&database, "-wal"))?, wal_before);
        writer.close().await?;
        Ok(())
    }

    #[tokio::test]
    async fn publishes_complete_root_and_can_move_it_again() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut args = args(temp.path());
        fs::create_dir_all(args.from.join("db"))?;
        let database = args.from.join("db/gateway.db");
        create_database(&database).await?;
        let mut connection = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&database)
                .create_if_missing(false),
        )
        .await?;
        sqlx::query("INSERT INTO sample VALUES (42)")
            .execute(&mut connection)
            .await?;
        connection.close().await?;
        fs::create_dir_all(args.from.join("artifacts/objects"))?;
        fs::write(args.from.join("artifacts/objects/blob"), b"attachment")?;
        fs::create_dir_all(args.from.join("diagnostics/observation-debug"))?;
        fs::write(
            args.from.join("diagnostics/observation-debug/trace"),
            b"trace",
        )?;
        fs::write(
            args.from.join("server.toml"),
            "[database]\nbackend='sqlite'\n",
        )?;
        args.apply = true;
        args.source_stopped = true;
        let first = args.to.clone();
        run(args).await?;
        fs::create_dir_all(first.join("plugins/artifacts"))?;
        fs::write(
            first.join("plugins/artifacts/component.wasm"),
            b"installed component",
        )?;
        let second = temp.path().join("second");
        run(MigrateDataArgs {
            from: first.clone(),
            to: second.clone(),
            optimize_storage: false,
            apply: true,
            source_stopped: true,
        })
        .await?;
        assert_eq!(
            fs::read(second.join("artifacts/objects/blob"))?,
            b"attachment"
        );
        assert_eq!(
            fs::read(second.join("diagnostics/observation-debug/trace"))?,
            b"trace"
        );
        assert_eq!(
            fs::read(second.join("plugins/artifacts/component.wasm"))?,
            b"installed component"
        );
        let config: toml::Value = toml::from_str(&fs::read_to_string(second.join("server.toml"))?)?;
        assert_eq!(config["database"]["backend"].as_str(), Some("sqlite"));
        let mut connection = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(second.join("db/gateway.db"))
                .read_only(true),
        )
        .await?;
        let value: (String,) = sqlx::query_as("SELECT value FROM sample")
            .fetch_one(&mut connection)
            .await?;
        assert_eq!(value.0, "42");
        connection.close().await?;
        assert!(first.join("db/gateway.db").is_file());
        Ok(())
    }

    #[tokio::test]
    async fn default_is_read_only_and_failed_apply_never_publishes() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut args = modern(temp.path())?;
        let original = fs::read(args.from.join("db/gateway.db"))?;
        run(MigrateDataArgs {
            from: args.from.clone(),
            to: args.to.clone(),
            optimize_storage: false,
            apply: false,
            source_stopped: false,
        })
        .await?;
        assert!(!args.to.exists());
        assert!(!args.from.join(".instance.lock").exists());
        args.apply = true;
        args.source_stopped = true;
        let target = args.to.clone();
        let source = args.from.clone();
        // The placeholder file is not a database: schema verification must fail
        // before staging or publishing anything.
        assert!(run(args).await.is_err());
        assert!(!target.exists());
        assert_eq!(fs::read(source.join("db/gateway.db"))?, original);
        assert!(!fs::read_dir(temp.path())?.any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".stravia-migrate-")
        }));
        Ok(())
    }
}
