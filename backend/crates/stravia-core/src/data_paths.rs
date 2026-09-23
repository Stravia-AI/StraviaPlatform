//! 实例数据布局。宿主选择根目录，存储模块只管理自己的子目录。

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};

/// 展开开头的 `~`，并相对启动时的工作目录解析为绝对路径。
/// 不创建目录；空路径、主目录不可用或路径解析失败时返回错误。
pub fn resolve_data_dir(path: &Path) -> anyhow::Result<PathBuf> {
    if path.as_os_str().is_empty() {
        bail!("data directory must not be empty");
    }
    let expanded = if let Ok(suffix) = path.strip_prefix("~") {
        dirs::home_dir()
            .context("home directory is unavailable; specify an absolute data directory")?
            .join(suffix)
    } else {
        path.to_path_buf()
    };
    std::path::absolute(expanded).context("resolve data directory")
}

/// 已选根目录的托管布局；构造本身不读写文件系统。
#[derive(Clone, Copy)]
pub struct DataPaths<'a> {
    root: &'a Path,
}

impl<'a> DataPaths<'a> {
    pub fn new(root: &'a Path) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        self.root
    }

    pub fn database_dir(&self) -> PathBuf {
        self.root.join("db")
    }

    pub fn database(&self) -> PathBuf {
        self.database_dir().join("gateway.db")
    }

    pub fn server_config(&self) -> PathBuf {
        self.root.join("server.toml")
    }

    pub fn artifacts(&self) -> PathBuf {
        self.root.join("artifacts")
    }

    pub fn plugins(&self) -> PathBuf {
        self.root.join("plugins")
    }

    /// Observation 在此目录内管理 `observation-debug`。
    pub fn diagnostics(&self) -> PathBuf {
        self.root.join("diagnostics")
    }

    /// Provider Catalog 在此目录内管理 `catalog`。
    pub fn catalog_root(&self) -> PathBuf {
        self.root.join("cache")
    }

    pub fn web_access_profile(&self) -> PathBuf {
        self.root.join("state/web-access/browser-profile")
    }

    pub fn desktop_port(&self) -> PathBuf {
        self.root.join("state/desktop-port.json")
    }

    pub fn desktop_webview(&self) -> PathBuf {
        self.root.join("state/desktop-webview")
    }

    /// 检查旧布局后按需创建根目录，不迁移、覆盖或删除已有数据。
    /// 不可读取的路径与旧布局均失败，避免悄悄打开一个空数据库。
    pub fn prepare(&self) -> anyhow::Result<()> {
        for name in [
            "gateway.db",
            "gateway.db-wal",
            "gateway.db-shm",
            "catalog",
            "observation-debug",
            "web-access",
            "desktop-port.json",
        ] {
            if self
                .root
                .join(name)
                .try_exists()
                .context("inspect data layout")?
            {
                bail!(
                    "legacy data layout at {}; this release cannot upgrade it — move it aside or start with a fresh data directory",
                    self.root.display()
                );
            }
        }
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(self.root).context("create data directory")
    }

    /// 获取实例独占锁。调用者必须持有返回的文件直到实例或迁移结束。
    /// 不检查布局，以便迁移工具锁住旧实例；不会截断已有锁文件。
    pub fn lock(&self) -> anyhow::Result<File> {
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(self.root.join(".instance.lock"))
            .context("open instance lock")?;
        file.try_lock()
            .context("data directory is in use by another instance or migration")?;
        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_database_is_not_replaced_with_an_empty_layout() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let database = root.path().join("gateway.db");
        fs::write(&database, b"existing database")?;
        let paths = DataPaths::new(root.path());
        assert!(paths.prepare().is_err());
        assert_eq!(fs::read(database)?, b"existing database");
        assert!(!paths.database_dir().exists());
        Ok(())
    }

    #[test]
    fn instance_lock_is_exclusive_and_released_on_drop() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let paths = DataPaths::new(root.path());
        let lock = paths.lock()?;
        assert!(paths.lock().is_err());
        drop(lock);
        let _lock = paths.lock()?;
        Ok(())
    }

    #[tokio::test]
    async fn sqlite_reopens_persisted_data_after_moving_root() -> anyhow::Result<()> {
        let parent = tempfile::tempdir()?;
        let source = parent.path().join("source");
        let target = parent.path().join("数据 moved");
        let pool = crate::db::init_pool(&source).await?;
        sqlx::query("CREATE TABLE saved (value TEXT NOT NULL)")
            .execute(&pool)
            .await?;
        sqlx::query("INSERT INTO saved VALUES ('retained')")
            .execute(&pool)
            .await?;
        pool.close().await;
        fs::rename(&source, &target)?;
        let pool = crate::db::init_pool(&target).await?;
        let value: String = sqlx::query_scalar("SELECT value FROM saved")
            .fetch_one(&pool)
            .await?;
        assert_eq!(value, "retained");
        assert!(!source.exists());
        pool.close().await;
        Ok(())
    }
}
