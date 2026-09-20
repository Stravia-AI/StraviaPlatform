//! 离线副本优化；宿主仍负责源实例停写、完整复制和校验后发布。
use std::path::Path;

use anyhow::{Context, ensure};
use serde::Serialize;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

#[derive(Debug, Serialize)]
pub struct StorageOptimizationReport {
    pub history_nodes_rewritten: u64,
    pub trace_directories: usize,
    pub database_bytes_before: u64,
    pub database_bytes_after: u64,
    pub trace_bytes_after: u64,
}

/// 对独占、可丢弃的现代布局副本执行 schema 升级、结构去重和 SQLite 空闲页回收。
/// 不访问配置中的外部数据库或对象存储，不删除内容或缩短保留期。
/// 调用方必须在出错时丢弃副本，只有成功后才能发布它；本函数不为原地操作创建备份。
/// 缺少数据库、无法独占、损坏记录、还原不一致及 I/O 错误均显式返回。
pub async fn optimize_data_copy(root: &Path) -> anyhow::Result<StorageOptimizationReport> {
    let paths = crate::data_paths::DataPaths::new(root);
    let database = paths.database();
    ensure!(
        database.is_file(),
        "offline copy is missing its SQLite database"
    );
    let _lock = paths.lock()?;
    let before = database.metadata()?.len();
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(&database)
                .create_if_missing(false)
                .foreign_keys(true),
        )
        .await?;
    let result = async {
        crate::migrations::migrate_sqlite(&pool).await?;
        let rewritten = crate::turn_chain::SqlTurnChainStore::sqlite(pool.clone())
            .optimize_storage()
            .await?;
        let traces = paths.diagnostics().join("observation-debug");
        let reports = tokio::task::spawn_blocking(move || {
            crate::interaction_observation::optimize_trace_directory(&traces)
        })
        .await??;
        let mut transaction = pool.begin().await?;
        for (id, bytes) in &reports {
            sqlx::query(
                "UPDATE debug_trace_manifests SET bytes_written = $1 WHERE relative_directory = $2",
            )
            .bind(i64::try_from(*bytes)?)
            .bind(id)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        let checkpoint: (i64, i64, i64) = sqlx::query_as("PRAGMA wal_checkpoint(TRUNCATE)")
            .fetch_one(&pool)
            .await?;
        ensure!(checkpoint.0 == 0, "offline SQLite checkpoint is busy");
        sqlx::query("VACUUM")
            .execute(&pool)
            .await
            .context("reclaim offline SQLite free pages")?;
        let rows: Vec<String> = sqlx::query_scalar("PRAGMA integrity_check")
            .fetch_all(&pool)
            .await?;
        ensure!(rows == ["ok"], "optimized SQLite integrity check failed");
        let violations = sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&pool)
            .await?;
        ensure!(
            violations.is_empty(),
            "optimized SQLite reference check failed"
        );
        Ok::<_, anyhow::Error>((rewritten, reports))
    }
    .await;
    pool.close().await;
    let (rewritten, reports) = result?;
    Ok(StorageOptimizationReport {
        history_nodes_rewritten: rewritten,
        trace_directories: reports.len(),
        database_bytes_before: before,
        database_bytes_after: database.metadata()?.len(),
        trace_bytes_after: reports.iter().map(|(_, bytes)| bytes).sum(),
    })
}
