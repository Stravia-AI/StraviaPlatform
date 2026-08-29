use crate::db::models::{CreateWebProvider, UpdateWebProvider, WebAccessSettings, WebProvider};
use crate::storage::traits::{
    ProviderTestResult, WebAccessApiKeyPermissions, WebAccessRuntimeConfig, WebProviderStore,
    validate_web_access_provider_lists,
};
use anyhow::Context;
use async_trait::async_trait;
use sqlx::{Pool, Postgres};

const SEARCH_IDS_KEY: &str = "web_access_search_provider_ids";
const ENABLED_KEY: &str = "web_access_enabled";
const FETCH_IDS_KEY: &str = "web_access_fetch_provider_ids";
const SELECT_WEB_PROVIDER: &str = "SELECT id, name, kind, api_key, last_test_success, to_char(last_test_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS last_test_at, to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS created_at, to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS updated_at FROM web_providers";

#[derive(Clone)]
pub(super) struct PostgresWebProviderStore {
    pub(super) pool: Pool<Postgres>,
}

#[async_trait]
impl WebProviderStore for PostgresWebProviderStore {
    async fn list(&self) -> anyhow::Result<Vec<WebProvider>> {
        Ok(
            sqlx::query_as::<_, WebProvider>(sqlx::AssertSqlSafe(format!(
                "{SELECT_WEB_PROVIDER} ORDER BY created_at DESC"
            )))
            .fetch_all(&self.pool)
            .await?,
        )
    }

    async fn get(&self, id: &str) -> anyhow::Result<Option<WebProvider>> {
        Ok(
            sqlx::query_as::<_, WebProvider>(sqlx::AssertSqlSafe(format!(
                "{SELECT_WEB_PROVIDER} WHERE id = $1"
            )))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?,
        )
    }

    async fn create(&self, input: CreateWebProvider) -> anyhow::Result<WebProvider> {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO web_providers (id, name, kind, api_key) VALUES ($1, $2, $3, $4)")
            .bind(&id)
            .bind(input.name.trim())
            .bind(input.kind)
            .bind(input.api_key)
            .execute(&self.pool)
            .await?;
        self.get(&id)
            .await?
            .context("Web Provider missing after create")
    }

    async fn update(&self, id: &str, input: UpdateWebProvider) -> anyhow::Result<WebProvider> {
        let mut tx = self.pool.begin().await?;
        let current = sqlx::query_as::<_, WebProvider>(sqlx::AssertSqlSafe(format!(
            "{SELECT_WEB_PROVIDER} WHERE id = $1"
        )))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .context("Web Provider not found for update")?;
        let name = input.name.unwrap_or(current.name);
        let api_key = match input.api_key {
            Some(value) => value,
            None => current.api_key,
        };
        sqlx::query(
            "UPDATE web_providers SET name = $1, api_key = $2, updated_at = CURRENT_TIMESTAMP WHERE id = $3",
        )
        .bind(name.trim())
        .bind(api_key)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        self.get(id)
            .await?
            .context("Web Provider missing after update")
    }

    async fn delete(&self, id: &str) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM web_providers WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        for key in [SEARCH_IDS_KEY, FETCH_IDS_KEY] {
            sqlx::query(
                "UPDATE settings AS s
                 SET value = (
                     SELECT COALESCE(
                         jsonb_agg(entry.value ORDER BY entry.ordinality),
                         '[]'::jsonb
                     )::text
                     FROM jsonb_array_elements_text(s.value::jsonb)
                         WITH ORDINALITY AS entry(value, ordinality)
                     WHERE entry.value <> $2
                 ),
                 updated_at = CURRENT_TIMESTAMP
                 WHERE s.name = $1",
            )
            .bind(key)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn exists_by_name(&self, name: &str, exclude_id: Option<&str>) -> anyhow::Result<bool> {
        let id = if let Some(exclude_id) = exclude_id {
            sqlx::query_scalar::<_, String>(
                "SELECT id FROM web_providers WHERE lower(trim(name)) = lower(trim($1)) AND id != $2 LIMIT 1",
            )
            .bind(name)
            .bind(exclude_id)
            .fetch_optional(&self.pool)
            .await?
        } else {
            sqlx::query_scalar::<_, String>(
                "SELECT id FROM web_providers WHERE lower(trim(name)) = lower(trim($1)) LIMIT 1",
            )
            .bind(name)
            .fetch_optional(&self.pool)
            .await?
        };
        Ok(id.is_some())
    }

    async fn record_test_result(
        &self,
        provider_id: &str,
        result: ProviderTestResult,
    ) -> anyhow::Result<()> {
        let _ = result.tested_at;
        sqlx::query(
            "UPDATE web_providers SET last_test_success = $1, last_test_at = CURRENT_TIMESTAMP, updated_at = CURRENT_TIMESTAMP WHERE id = $2",
        )
        .bind(result.success)
        .bind(provider_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
    async fn load_settings(&self) -> anyhow::Result<WebAccessSettings> {
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT name, value FROM settings WHERE name IN ($1, $2, $3)",
        )
        .bind(ENABLED_KEY)
        .bind(SEARCH_IDS_KEY)
        .bind(FETCH_IDS_KEY)
        .fetch_all(&self.pool)
        .await?;
        let values = rows
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>();
        Ok(WebAccessSettings {
            enabled: values
                .get(ENABLED_KEY)
                .is_some_and(|value| matches!(value.trim(), "true" | "1")),
            search_provider_ids: parse_ids(values.get(SEARCH_IDS_KEY)),
            fetch_provider_ids: parse_ids(values.get(FETCH_IDS_KEY)),
        })
    }

    async fn load_runtime_config(
        &self,
        api_key_id: &str,
    ) -> anyhow::Result<WebAccessRuntimeConfig> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT name, value FROM settings WHERE name IN ($1, $2, $3)",
        )
        .bind(ENABLED_KEY)
        .bind(SEARCH_IDS_KEY)
        .bind(FETCH_IDS_KEY)
        .fetch_all(&mut *tx)
        .await?;
        let web_providers = sqlx::query_as::<_, WebProvider>(sqlx::AssertSqlSafe(format!(
            "{SELECT_WEB_PROVIDER} ORDER BY created_at DESC"
        )))
        .fetch_all(&mut *tx)
        .await?;
        let api_key_permissions = sqlx::query_scalar::<_, bool>(
            "SELECT COALESCE(is_enabled, TRUE) FROM api_keys WHERE id = $1",
        )
        .bind(api_key_id)
        .fetch_optional(&mut *tx)
        .await?
        .map(|api_key_enabled| WebAccessApiKeyPermissions { api_key_enabled })
        .unwrap_or_default();
        tx.commit().await?;

        let values = rows
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>();
        let settings = WebAccessSettings {
            enabled: values
                .get(ENABLED_KEY)
                .is_some_and(|value| matches!(value.trim(), "true" | "1")),
            search_provider_ids: parse_ids(values.get(SEARCH_IDS_KEY)),
            fetch_provider_ids: parse_ids(values.get(FETCH_IDS_KEY)),
        };
        Ok(WebAccessRuntimeConfig {
            settings,
            web_providers,
            api_key_permissions,
        })
    }

    async fn save_settings(&self, settings: &WebAccessSettings) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        let providers = sqlx::query_as::<_, WebProvider>(sqlx::AssertSqlSafe(format!(
            "{SELECT_WEB_PROVIDER} FOR UPDATE"
        )))
        .fetch_all(&mut *tx)
        .await?;
        validate_web_access_provider_lists(&providers, settings)?;
        for (key, value) in [
            (ENABLED_KEY, settings.enabled.to_string()),
            (
                SEARCH_IDS_KEY,
                serde_json::to_string(&settings.search_provider_ids)?,
            ),
            (
                FETCH_IDS_KEY,
                serde_json::to_string(&settings.fetch_provider_ids)?,
            ),
        ] {
            sqlx::query(
                "INSERT INTO settings (name, value) VALUES ($1, $2) ON CONFLICT(name) DO UPDATE SET value = EXCLUDED.value, updated_at = CURRENT_TIMESTAMP",
            )
            .bind(key)
            .bind(value)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

fn parse_ids(value: Option<&String>) -> Vec<String> {
    value
        .and_then(|value| serde_json::from_str::<Vec<String>>(value).ok())
        .unwrap_or_default()
}
