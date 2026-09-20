//! 离线复制迁移；结构优化仅在显式启用时作用于目标副本，源数据不改写。
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
    /// External legacy server.toml; relative SQLite paths resolve beside this file.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Explicit legacy platform WebView data directory; never inferred from the current user.
    #[arg(long)]
    webview_from: Option<PathBuf>,
    /// Deduplicate history and debug content in the destination copy, then reclaim SQLite free pages.
    #[arg(long)]
    optimize_storage: bool,
    #[arg(long, requires = "source_stopped")]
    apply: bool,
    /// Confirm every host using the source (including an external SQLite root) is stopped.
    #[arg(long, requires = "apply")]
    source_stopped: bool,
}

struct Plan {
    from: PathBuf,
    to: PathBuf,
    roots: BTreeSet<PathBuf>,
    copies: Vec<(PathBuf, PathBuf)>,
    database: Option<PathBuf>,
    config: Option<String>,
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
    if plan.config.is_some() {
        println!("Write converted server.toml (contents withheld)");
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
            "Destination only: apply schema migrations, deduplicate history/debug content without compression, verify restoration, and reclaim SQLite free pages."
        );
    } else {
        println!("No schema migration.");
    }
    println!("No PostgreSQL connection or S3 operation. Source data is never rewritten.");
    println!(
        "Apply may create .instance.lock in source roots and retains a sibling destination reservation lock; these coordination files contain no data."
    );
    if !args.apply {
        println!("Plan only. Stop all source hosts, then repeat with --apply --source-stopped.");
        return Ok(());
    }
    // Hosts and other migration processes share the same source root lock. Legacy
    // hosts predate the lock: the explicit operator stop confirmation is essential.
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
            config: None,
            skipped: Vec::new(),
        };
        let config_path = args
            .config
            .as_ref()
            .map(|p| absolute(p))
            .transpose()?
            .unwrap_or_else(|| from.join("server.toml"));
        reject_links(&config_path)?;
        ensure!(
            !args.config.is_some() || config_path.is_file(),
            "explicit config must exist"
        );
        if config_path != from.join("server.toml") && from.join("server.toml").exists() {
            bail!("both root and external server.toml exist; resolve this conflict explicitly");
        }
        let mut sqlite = true;
        let mut configured_db = None;
        if config_path.exists() {
            // Never include TOML parser errors: they may quote passwords or connection strings.
            let text = fs::read_to_string(&config_path).context("cannot read server config")?;
            let mut value: toml::Value = toml::from_str(&text)
                .map_err(|_| anyhow::anyhow!("invalid server TOML (contents withheld)"))?;
            let database = value
                .get_mut("database")
                .and_then(toml::Value::as_table_mut)
                .context("config requires a database table")?;
            match database.get("backend").and_then(toml::Value::as_str) {
                Some("sqlite") => {
                    if let Some(path) = database.remove("path") {
                        let path = path.as_str().context("SQLite path must be a string")?;
                        ensure!(!path.is_empty(), "SQLite path must not be empty");
                        let expanded = PathBuf::from(shellexpand::tilde(path).as_ref());
                        configured_db = Some(absolute(
                            &config_path
                                .parent()
                                .context("config parent missing")?
                                .join(expanded),
                        )?);
                    }
                    ensure!(
                        database.len() == 1,
                        "unknown SQLite configuration fields; refusing lossy conversion"
                    );
                }
                Some("postgres") => sqlite = false,
                _ => bail!("unsupported database backend"),
            }
            plan.config = Some(
                toml::to_string_pretty(&value)
                    .map_err(|_| anyhow::anyhow!("cannot serialize server config"))?,
            );
        }
        let webview = args
            .webview_from
            .as_ref()
            .map(|path| absolute(path))
            .transpose()?;
        let shared_webview_root = webview.as_ref() == Some(&from);
        let modern = from.join("db").exists()
            || [
                "cache/catalog",
                "diagnostics/observation-debug",
                "state/desktop-port.json",
                "state/web-access",
                "state/desktop-webview",
            ]
            .iter()
            .any(|path| from.join(path).exists())
            || (!shared_webview_root
                && ["cache", "diagnostics", "state"]
                    .iter()
                    .any(|path| from.join(path).exists()));
        let legacy = [
            "gateway.db",
            "catalog",
            "observation-debug",
            "web-access",
            "desktop-port.json",
        ]
        .iter()
        .any(|name| from.join(name).exists());
        ensure!(
            !(modern && legacy),
            "mixed old/new source layout; resolve conflicts before migration"
        );
        let db = configured_db.unwrap_or_else(|| {
            from.join(if modern {
                "db/gateway.db"
            } else {
                "gateway.db"
            })
        });
        reject_links(&db)?;
        let resource_root = if sqlite && !modern {
            db.parent()
                .context("database parent missing")?
                .to_path_buf()
        } else {
            from.clone()
        };
        if resource_root != from {
            disjoint(&resource_root, &plan.to)?;
            ensure!(
                !resource_root.starts_with(&from) && !from.starts_with(&resource_root),
                "nested external database roots are ambiguous"
            );
            plan.roots.insert(resource_root.clone());
        }
        if sqlite {
            ensure!(db.is_file(), "SQLite source is missing: {}", db.display());
            if modern {
                ensure!(
                    db == from.join("db/gateway.db"),
                    "new layout cannot reference an external SQLite database"
                );
            }
            plan.database = Some(db.clone());
        } else {
            ensure!(
                !from.join("gateway.db").exists() && !from.join("db").exists(),
                "PostgreSQL configuration conflicts with local SQLite data"
            );
        }
        if let Some(webview) = &webview {
            ensure!(
                webview.is_dir(),
                "WebView source must be an existing directory"
            );
            disjoint(webview, &plan.to)?;
            ensure!(
                !from.join("desktop-webview").exists()
                    && !from.join("state/desktop-webview").exists(),
                "explicit WebView source conflicts with existing desktop-webview state"
            );
        }
        let mut allowed = BTreeSet::from(["server.toml".to_owned(), ".instance.lock".to_owned()]);
        if config_path.parent() == Some(from.as_path()) {
            allowed.insert(
                config_path
                    .file_name()
                    .context("config filename missing")?
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        if modern {
            for (name, children) in [
                ("db", vec!["gateway.db", "gateway.db-wal", "gateway.db-shm"]),
                ("cache", vec!["catalog"]),
                ("diagnostics", vec!["observation-debug"]),
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
                        plan.add(&dir, Path::new(name))?;
                    }
                }
            }
        } else {
            for (old, new) in [
                ("desktop-port.json", "state/desktop-port.json"),
                ("desktop-webview", "state/desktop-webview"),
            ] {
                allowed.insert(old.to_owned());
                plan.add(&from.join(old), Path::new(new))?;
            }
            for (old, new) in [
                ("catalog", "cache/catalog"),
                ("observation-debug", "diagnostics/observation-debug"),
                ("web-access", "state/web-access"),
            ] {
                if resource_root != from {
                    ensure!(
                        !from.join(old).exists(),
                        "conflicting host/resource root entry: {old}"
                    );
                }
                allowed.insert(old.to_owned());
                plan.add(&resource_root.join(old), Path::new(new))?;
            }
        }
        allowed.insert("artifacts".to_owned());
        if resource_root != from {
            ensure!(
                !from.join("artifacts").exists(),
                "conflicting artifacts in host and database roots"
            );
        }
        let artifacts = resource_root.join("artifacts");
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
            let db_name = db
                .file_name()
                .context("database filename missing")?
                .to_string_lossy()
                .into_owned();
            let mut db_allowed = BTreeSet::from([
                db_name.clone(),
                format!("{db_name}-wal"),
                format!("{db_name}-shm"),
                "artifacts".into(),
                "catalog".into(),
                "observation-debug".into(),
                "web-access".into(),
                ".instance.lock".into(),
            ]);
            if config_path.parent() == Some(resource_root.as_path()) {
                db_allowed.insert(
                    config_path
                        .file_name()
                        .context("config filename missing")?
                        .to_string_lossy()
                        .into_owned(),
                );
            }
            if resource_root != from {
                if webview.as_ref() == Some(&resource_root) {
                    plan.add_webview_entries(&resource_root, &mut db_allowed)?;
                }
                check_children(&resource_root, &db_allowed)?;
            } else if !modern {
                allowed.extend(db_allowed);
            }
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
        if let Some(webview) = &webview {
            if webview == &from {
                plan.add_webview_entries(&from, &mut allowed)?;
            } else if webview != &resource_root {
                for root in &plan.roots {
                    disjoint(webview, root)?;
                }
                plan.add_webview_entries(webview, &mut BTreeSet::from([".instance.lock".into()]))?;
                plan.roots.insert(webview.clone());
            }
        }
        check_children(&from, &allowed)?;
        for root in &plan.roots {
            scan_tree(root)?;
        }
        if from.join(".instance.lock").exists() {
            plan.skipped.push(from.join(".instance.lock"));
        }
        ensure!(
            modern || legacy || plan.config.is_some() || plan.database.is_some(),
            "unrecognized source layout"
        );
        Ok(plan)
    }

    fn add_webview_entries(&mut self, root: &Path, managed: &mut BTreeSet<String>) -> Result<()> {
        // An explicit shared platform root authorizes only its otherwise-unmapped
        // entries as WebView state, never nested duplicates of DB/artifacts/config.
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("WebView entry name must be UTF-8"))?;
            if !managed.contains(&name) {
                self.add(
                    &entry.path(),
                    &Path::new("state/desktop-webview").join(&name),
                )?;
                managed.insert(name);
            }
        }
        Ok(())
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
        if let Some(config) = &self.config {
            write_private(&target.join("server.toml"), config.as_bytes())?;
        }
        Ok(())
    }
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

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    private_dir(path.parent().context("file parent missing")?)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
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
            config: None,
            webview_from: None,
            optimize_storage: false,
            apply: false,
            source_stopped: false,
        }
    }

    fn legacy(root: &Path) -> Result<MigrateDataArgs> {
        let args = args(root);
        fs::create_dir(&args.from)?;
        fs::write(args.from.join("gateway.db"), b"not yet a database")?;
        Ok(args)
    }

    #[test]
    fn rejects_mixed_layout_and_unknown_files() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let args = legacy(temp.path())?;
        fs::create_dir(args.from.join("cache"))?;
        assert!(Plan::build(&args).is_err());
        fs::remove_dir(args.from.join("cache"))?;
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
    fn converts_external_sqlite_path_and_preserves_postgres_secret() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut args = args(temp.path());
        fs::create_dir(&args.from)?;
        let config_dir = temp.path().join("config");
        fs::create_dir(&config_dir)?;
        let database_root = config_dir.join("data");
        fs::create_dir(&database_root)?;
        fs::write(database_root.join("custom.db"), b"database")?;
        fs::create_dir(database_root.join("catalog"))?;
        fs::create_dir_all(database_root.join("web-access/browser-profile"))?;
        let config = config_dir.join("server.toml");
        fs::write(
            &config,
            "[database]\nbackend = 'sqlite'\npath = 'data/custom.db'\n",
        )?;
        args.config = Some(config.clone());
        let plan = Plan::build(&args)?;
        assert_eq!(
            plan.database,
            Some(fs::canonicalize(database_root.join("custom.db"))?)
        );
        let converted: toml::Value =
            toml::from_str(&plan.config.context("missing converted config")?)?;
        assert!(converted["database"].get("path").is_none());
        assert!(
            plan.copies
                .iter()
                .any(|(_, path)| path == Path::new("cache/catalog"))
        );
        assert!(
            plan.copies
                .iter()
                .any(|(_, path)| path == Path::new("state/web-access"))
        );
        fs::create_dir(args.from.join("web-access"))?;
        assert!(Plan::build(&args).is_err());
        fs::remove_dir(args.from.join("web-access"))?;
        fs::write(
            &config,
            "[database]\nbackend = 'postgres'\nurl = 'postgres://user:secret@host/db'\nmax_connections = 7\n",
        )?;
        let plan = Plan::build(&args)?;
        assert!(plan.database.is_none());
        let converted: toml::Value =
            toml::from_str(&plan.config.context("missing converted config")?)?;
        assert_eq!(
            converted["database"]["url"].as_str(),
            Some("postgres://user:secret@host/db")
        );
        Ok(())
    }

    #[test]
    fn rejects_target_escape_and_resource_conflict() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut args = legacy(temp.path())?;
        args.to = args.from.join("nested");
        assert!(Plan::build(&args).is_err());
        args.to = temp.path().join("old/../new");
        assert!(Plan::build(&args).is_err());
        args.to = temp.path().join("new");
        fs::create_dir(&args.to)?;
        fs::write(args.to.join("valuable"), b"keep")?;
        assert!(Plan::build(&args).is_err());
        fs::remove_dir_all(&args.to)?;
        let external = temp.path().join("external");
        fs::create_dir(&external)?;
        fs::write(external.join("gateway.db"), b"database")?;
        let mut config: toml::Value = toml::from_str("[database]\nbackend = 'sqlite'\n")?;
        config["database"].as_table_mut().unwrap().insert(
            "path".into(),
            toml::Value::String(external.join("gateway.db").to_str().unwrap().into()),
        );
        fs::write(args.from.join("server.toml"), toml::to_string(&config)?)?;
        fs::create_dir(args.from.join("artifacts"))?;
        assert!(Plan::build(&args).is_err());
        Ok(())
    }

    #[test]
    fn explicit_webview_maps_shared_extras_without_duplicating_managed_data() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut args = legacy(temp.path())?;
        fs::create_dir_all(args.from.join("artifacts/objects"))?;
        fs::write(args.from.join("cookies"), b"cookie state")?;
        fs::create_dir(args.from.join("cache"))?;
        assert!(Plan::build(&args).is_err());
        args.webview_from = Some(args.from.clone());
        let plan = Plan::build(&args)?;
        assert!(
            plan.copies
                .iter()
                .any(|(_, target)| target == Path::new("state/desktop-webview/cookies"))
        );
        assert!(
            plan.copies
                .iter()
                .any(|(_, target)| target == Path::new("state/desktop-webview/cache"))
        );
        assert!(!plan.copies.iter().any(|(_, target)| target
            == Path::new("state/desktop-webview/gateway.db")
            || target == Path::new("state/desktop-webview/artifacts")));
        fs::remove_file(args.from.join("cookies"))?;
        fs::remove_dir(args.from.join("cache"))?;
        let platform = temp.path().join("platform-webview");
        fs::create_dir(&platform)?;
        fs::write(platform.join("cookies"), b"separate cookie state")?;
        args.webview_from = Some(platform.clone());
        let plan = Plan::build(&args)?;
        assert!(plan.roots.contains(&fs::canonicalize(&platform)?));
        assert!(
            plan.copies
                .iter()
                .any(|(_, target)| target == Path::new("state/desktop-webview/cookies"))
        );
        fs::create_dir(args.from.join("desktop-webview"))?;
        assert!(Plan::build(&args).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_inside_copied_tree() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let args = legacy(temp.path())?;
        fs::create_dir(args.from.join("catalog"))?;
        std::os::unix::fs::symlink(temp.path(), args.from.join("catalog/escape"))?;
        assert!(Plan::build(&args).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn snapshot_includes_committed_wal_without_changing_source() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let args = args(temp.path());
        fs::create_dir(&args.from)?;
        let database = args.from.join("gateway.db");
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
        fs::create_dir(&args.from)?;
        let mut connection = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(args.from.join("gateway.db"))
                .create_if_missing(true),
        )
        .await?;
        sqlx::query("CREATE TABLE sample (value INTEGER)")
            .execute(&mut connection)
            .await?;
        sqlx::query("INSERT INTO sample VALUES (42)")
            .execute(&mut connection)
            .await?;
        connection.close().await?;
        fs::create_dir_all(args.from.join("artifacts/objects"))?;
        fs::write(args.from.join("artifacts/objects/blob"), b"attachment")?;
        fs::create_dir(args.from.join("observation-debug"))?;
        fs::write(args.from.join("observation-debug/trace"), b"trace")?;
        fs::write(
            args.from.join("server.toml"),
            "[database]\nbackend='sqlite'\npath='gateway.db'\n",
        )?;
        args.apply = true;
        args.source_stopped = true;
        let first = args.to.clone();
        run(args).await?;
        let second = temp.path().join("second");
        run(MigrateDataArgs {
            from: first.clone(),
            to: second.clone(),
            config: None,
            webview_from: None,
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
        let config: toml::Value = toml::from_str(&fs::read_to_string(second.join("server.toml"))?)?;
        assert!(config["database"].get("path").is_none());
        let mut connection = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(second.join("db/gateway.db"))
                .read_only(true),
        )
        .await?;
        let value: (i64,) = sqlx::query_as("SELECT value FROM sample")
            .fetch_one(&mut connection)
            .await?;
        assert_eq!(value.0, 42);
        connection.close().await?;
        assert!(first.join("db/gateway.db").is_file());
        Ok(())
    }

    #[tokio::test]
    async fn default_is_read_only_and_failed_apply_never_publishes() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut args = legacy(temp.path())?;
        let original = fs::read(args.from.join("gateway.db"))?;
        run(MigrateDataArgs {
            from: args.from.clone(),
            to: args.to.clone(),
            config: None,
            webview_from: None,
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
        assert!(run(args).await.is_err());
        assert!(!target.exists());
        assert_eq!(fs::read(source.join("gateway.db"))?, original);
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
