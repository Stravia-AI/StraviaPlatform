use super::SqliteStorage;
use crate::storage::traits::{PromptAdmission, PromptAdmissionStore};
use async_trait::async_trait;
use sqlx::{QueryBuilder, Row, Sqlite};

#[async_trait]
impl PromptAdmissionStore for SqliteStorage {
    async fn history_epoch(&self) -> anyhow::Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT epoch FROM prompt_admission_epoch WHERE id = 1")
                .fetch_one(&self.pool)
                .await?,
        )
    }

    async fn insert_at_epoch(
        &self,
        a: &PromptAdmission,
        expected_epoch: i64,
    ) -> anyhow::Result<()> {
        let mut connection = self.pool.acquire().await?;
        let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
        let epoch: i64 =
            sqlx::query_scalar("SELECT epoch FROM prompt_admission_epoch WHERE id = 1")
                .fetch_one(&mut *tx)
                .await?;
        anyhow::ensure!(
            epoch == expected_epoch,
            "history was cleared during admission"
        );
        let context_window_index = crate::storage::validate_admission(a)?;
        let context_window_id = a.context_window_id.map(|id| id.to_string());
        anyhow::ensure!(
            a.ordinal >= 0
                && a.history_count >= 0
                && a.user_turn_index.is_none_or(|index| index >= 0)
                && a.dialogue_prompt_index.is_none_or(|index| index >= 0),
            "invalid admission position or history count"
        );
        let result = sqlx::query("INSERT INTO prompt_admissions (id, principal, parent_id, ordinal, history_count, prefix_digest, controls_digest, created_at_ms, conversation_id, conversation_created_at_ms, user_prompt_id, user_prompt_created_at_ms, user_turn_index, dialogue_prompt_index, atomic_lineage_created, context_window_id, context_window_index, logical_turn_id, logical_turn_created_at_ms) SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19 WHERE ?3 IS NULL OR EXISTS (SELECT 1 FROM prompt_admissions WHERE id = ?3 AND principal = ?2 AND ordinal <= ?4 AND (user_turn_index IS NULL OR ?13 IS NULL OR user_turn_index <= ?13) AND (dialogue_prompt_index IS NULL OR ?14 IS NULL OR dialogue_prompt_index <= ?14) AND conversation_id = ?9 AND conversation_created_at_ms = ?10 AND (ordinal < ?4 OR (user_prompt_id IS ?11 AND user_prompt_created_at_ms IS ?12 AND logical_turn_id = ?18 AND logical_turn_created_at_ms = ?19)))")
            .bind(&a.id).bind(&a.principal).bind(&a.parent_id).bind(a.ordinal).bind(a.history_count)
            .bind(&a.prefix_digest).bind(&a.controls_digest).bind(a.created_at_ms)
            .bind(&a.conversation_id).bind(a.conversation_created_at_ms).bind(&a.user_prompt_id).bind(a.user_prompt_created_at_ms).bind(a.user_turn_index).bind(a.dialogue_prompt_index).bind(a.atomic_lineage_created).bind(context_window_id).bind(context_window_index).bind(&a.logical_turn_id).bind(a.logical_turn_created_at_ms)
            .execute(&mut *tx).await?;
        anyhow::ensure!(
            result.rows_affected() == 1,
            "admission parent not found or ordinal precedes parent"
        );
        tx.commit().await?;
        Ok(())
    }

    async fn get(&self, principal: &str, id: &str) -> anyhow::Result<Option<PromptAdmission>> {
        let row = sqlx::query("SELECT id, principal, parent_id, atomic_lineage_created, ordinal, user_turn_index, dialogue_prompt_index, context_window_id, context_window_index, created_at_ms, conversation_id, conversation_created_at_ms, user_prompt_id, user_prompt_created_at_ms, logical_turn_id, logical_turn_created_at_ms, history_count, prefix_digest, controls_digest FROM prompt_admissions WHERE principal = ?1 AND id = ?2")
            .bind(principal).bind(id).fetch_optional(&self.pool).await?;
        row.map(decode).transpose()
    }

    async fn find_prefixes(
        &self,
        principal: &str,
        controls: &str,
        prefixes: &[(i64, String)],
    ) -> anyhow::Result<Vec<PromptAdmission>> {
        if prefixes.is_empty() {
            return Ok(Vec::new());
        }
        let unique: std::collections::HashSet<_> = prefixes.iter().collect();
        let prefixes: Vec<_> = unique.into_iter().collect();
        let mut found = Vec::new();
        let mut tx = self.pool.begin().await?;
        for chunk in prefixes.chunks(300) {
            let mut query = QueryBuilder::<Sqlite>::new(
                "SELECT id, principal, parent_id, atomic_lineage_created, ordinal, user_turn_index, dialogue_prompt_index, context_window_id, context_window_index, created_at_ms, conversation_id, conversation_created_at_ms, user_prompt_id, user_prompt_created_at_ms, logical_turn_id, logical_turn_created_at_ms, history_count, prefix_digest, controls_digest FROM prompt_admissions WHERE principal = ",
            );
            query
                .push_bind(principal)
                .push(" AND controls_digest = ")
                .push_bind(controls)
                .push(" AND (");
            let mut separated = query.separated(" OR ");
            for (count, digest) in chunk {
                separated
                    .push("(history_count = ")
                    .push_bind_unseparated(*count)
                    .push_unseparated(" AND prefix_digest = ")
                    .push_bind_unseparated(digest)
                    .push_unseparated(")");
            }
            separated.push_unseparated(")");
            for row in query.build().fetch_all(&mut *tx).await? {
                found.push(decode(row)?);
            }
        }
        tx.commit().await?;
        Ok(found)
    }

    async fn prune_before(&self, cutoff_ms: i64) -> anyhow::Result<u64> {
        let result = sqlx::query("WITH RECURSIVE live(id, parent_id) AS (SELECT id, parent_id FROM prompt_admissions WHERE created_at_ms >= ?1 UNION SELECT p.id, p.parent_id FROM prompt_admissions p JOIN live l ON p.id = l.parent_id) DELETE FROM prompt_admissions WHERE created_at_ms < ?1 AND id NOT IN (SELECT id FROM live)")
            .bind(cutoff_ms).execute(&self.pool).await?;
        Ok(result.rows_affected())
    }

    async fn purge_principal(&self, principal: &str) -> anyhow::Result<u64> {
        Ok(
            sqlx::query("DELETE FROM prompt_admissions WHERE principal = ?1")
                .bind(principal)
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }
    async fn clear(&self) -> anyhow::Result<u64> {
        let mut connection = self.pool.acquire().await?;
        let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
        let changed = sqlx::query("UPDATE prompt_admission_epoch SET epoch = epoch + 1 WHERE id = 1 AND epoch < 9223372036854775807").execute(&mut *tx).await?;
        anyhow::ensure!(
            changed.rows_affected() == 1,
            "history epoch unavailable or exhausted"
        );
        let result = sqlx::query("DELETE FROM prompt_admissions")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(result.rows_affected())
    }
}

fn decode(row: sqlx::sqlite::SqliteRow) -> anyhow::Result<PromptAdmission> {
    Ok(PromptAdmission {
        id: row.try_get("id")?,
        principal: row.try_get("principal")?,
        parent_id: row.try_get("parent_id")?,
        atomic_lineage_created: row.try_get("atomic_lineage_created")?,
        ordinal: row.try_get("ordinal")?,
        user_turn_index: row.try_get("user_turn_index")?,
        dialogue_prompt_index: row.try_get("dialogue_prompt_index")?,
        context_window_id: row
            .try_get::<Option<String>, _>("context_window_id")?
            .map(|id| uuid::Uuid::parse_str(&id))
            .transpose()?,
        context_window_index: row
            .try_get::<Option<i64>, _>("context_window_index")?
            .map(u64::try_from)
            .transpose()?,
        created_at_ms: row.try_get("created_at_ms")?,
        conversation_id: row.try_get("conversation_id")?,
        conversation_created_at_ms: row.try_get("conversation_created_at_ms")?,
        user_prompt_id: row.try_get("user_prompt_id")?,
        user_prompt_created_at_ms: row.try_get("user_prompt_created_at_ms")?,
        logical_turn_id: row.try_get("logical_turn_id")?,
        logical_turn_created_at_ms: row.try_get("logical_turn_created_at_ms")?,
        history_count: row.try_get("history_count")?,
        prefix_digest: row.try_get("prefix_digest")?,
        controls_digest: row.try_get("controls_digest")?,
    })
}
