// 缺口是写入失败的诊断证据，不能由同一次失败的统计写入回滚。
// 正常事实事务内一起刷新；失败标记先独立提交，再在独立写事务中重算覆盖。
// 长期桶不在这里改：checkpoint 的 UPDATE 会触发 stats_usage_delta，按 NEW − OLD 在同一事务内结转。
const CHECKPOINT: &str = "UPDATE stats_usage_checkpoints AS c
SET recovered_request_count = recovery.recovered_request_count,
    recovery_known_request_count = recovery.recovery_known_request_count
FROM stats_usage_run_recovery_source AS recovery
JOIN inference_run_observations AS r ON r.id = recovery.run_id
WHERE c.source_kind = 'run' AND c.scope = 'global'
  AND c.source_id = r.id AND r.interaction_id = $1
  AND (c.recovered_request_count <> recovery.recovered_request_count
       OR c.recovery_known_request_count <> recovery.recovery_known_request_count)";

/// 调用方持有写事务，确保来源 checkpoint 与触发器结转的长期桶原子更新；错误原样向上传播。
pub(crate) async fn refresh_sqlite(
    connection: &mut sqlx::SqliteConnection<'_>,
    interaction_id: &str,
) -> anyhow::Result<()> {
    sqlx::query(CHECKPOINT)
        .bind(interaction_id)
        .execute(connection)
        .await?;
    Ok(())
}

/// 调用方持有写事务，确保来源 checkpoint 与触发器结转的长期桶原子更新；错误原样向上传播。
pub(crate) async fn refresh_postgres(
    connection: &mut sqlx::PgConnection<'_>,
    interaction_id: &str,
) -> anyhow::Result<()> {
    sqlx::query(CHECKPOINT)
        .bind(interaction_id)
        .execute(connection)
        .await?;
    Ok(())
}
