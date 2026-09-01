use crate::db::models::{CreateWebProvider, UpdateWebProvider, WebAccessSettings, WebProvider};
use crate::storage::traits::{
    ProviderTestResult, WebAccessApiKeyPermissions, WebAccessRuntimeConfig, WebProviderStore,
    validate_web_access_provider_lists,
};
use anyhow::Context;
use async_trait::async_trait;
use sqlx::{SqlitePool, types::Json};

const ENABLED_KEY: &str = "web_access_enabled";
const SEARCH_IDS_KEY: &str = "web_access_search_provider_ids";
const FETCH_IDS_KEY: &str = "web_access_fetch_provider_ids";

#[derive(Clone)]
pub(super) struct SqliteWebProviderStore {
    pub(super) pool: SqlitePool,
}

#[async_trait]
impl WebProviderStore for SqliteWebProviderStore {
    async fn list(&self) -> anyhow::Result<Vec<WebProvider>> {
        Ok(sqlx::query_as::<_, WebProvider>(
            "SELECT id, name, kind, api_key, use_proxy, local_engines, last_test_success, last_test_at, created_at, updated_at FROM web_providers ORDER BY created_at DESC",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    async fn get(&self, id: &str) -> anyhow::Result<Option<WebProvider>> {
        Ok(sqlx::query_as::<_, WebProvider>(
            "SELECT id, name, kind, api_key, use_proxy, local_engines, last_test_success, last_test_at, created_at, updated_at FROM web_providers WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn create(&self, input: CreateWebProvider) -> anyhow::Result<WebProvider> {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO web_providers (id, name, kind, api_key, use_proxy, local_engines)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(input.name.trim())
        .bind(input.kind)
        .bind(input.api_key)
        .bind(input.use_proxy)
        .bind(input.local_engines.map(Json))
        .execute(&self.pool)
        .await?;
        self.get(&id)
            .await?
            .context("Web Provider missing after create")
    }

    async fn update(&self, id: &str, input: UpdateWebProvider) -> anyhow::Result<WebProvider> {
        let current = self
            .get(id)
            .await?
            .context("Web Provider not found for update")?;
        let name = input.name.unwrap_or(current.name);
        let api_key = match input.api_key {
            Some(value) => value,
            None => current.api_key,
        };
        let use_proxy = input.use_proxy.unwrap_or(current.use_proxy);
        let local_engines = match input.local_engines {
            Some(value) => value.map(Json),
            None => current.local_engines,
        };
        sqlx::query(
            "UPDATE web_providers
             SET name = ?, api_key = ?, use_proxy = ?, local_engines = ?, updated_at = datetime('now')
             WHERE id = ?",
        )
        .bind(name.trim())
        .bind(api_key)
        .bind(use_proxy)
        .bind(local_engines)
        .bind(id)
        .execute(&self.pool)
        .await?;
        self.get(id)
            .await?
            .context("Web Provider missing after update")
    }

    async fn delete(&self, id: &str) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM web_providers WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        for key in [SEARCH_IDS_KEY, FETCH_IDS_KEY] {
            let current =
                sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE name = ?")
                    .bind(key)
                    .fetch_optional(&mut *tx)
                    .await?;
            let Some(current) = current else { continue };
            let pruned = sqlx::query_scalar::<_, String>(
                "SELECT CASE
                    WHEN json_valid(?) THEN COALESCE(
                        (SELECT json_group_array(ordered.value)
                           FROM (
                               SELECT entry.value AS value
                                 FROM json_each(?) AS entry
                                WHERE entry.value <> ?
                                ORDER BY CAST(entry.key AS INTEGER)
                           ) AS ordered),
                        '[]'
                    )
                    ELSE '[]'
                END",
            )
            .bind(&current)
            .bind(&current)
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            sqlx::query(
                "UPDATE settings SET value = ?, updated_at = datetime('now') WHERE name = ?",
            )
            .bind(pruned)
            .bind(key)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn exists_by_name(&self, name: &str, exclude_id: Option<&str>) -> anyhow::Result<bool> {
        let id = if let Some(exclude_id) = exclude_id {
            sqlx::query_scalar::<_, String>(
                "SELECT id FROM web_providers WHERE lower(trim(name)) = lower(trim(?)) AND id != ? LIMIT 1",
            )
            .bind(name)
            .bind(exclude_id)
            .fetch_optional(&self.pool)
            .await?
        } else {
            sqlx::query_scalar::<_, String>(
                "SELECT id FROM web_providers WHERE lower(trim(name)) = lower(trim(?)) LIMIT 1",
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
        sqlx::query(
            "UPDATE web_providers SET last_test_success = ?, last_test_at = ?, updated_at = datetime('now') WHERE id = ?",
        )
        .bind(result.success)
        .bind(result.tested_at)
        .bind(provider_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn load_settings(&self) -> anyhow::Result<WebAccessSettings> {
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT name, value FROM settings WHERE name IN (?, ?, ?)",
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
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT name, value FROM settings WHERE name IN (?, ?, ?)",
        )
        .bind(ENABLED_KEY)
        .bind(SEARCH_IDS_KEY)
        .bind(FETCH_IDS_KEY)
        .fetch_all(&mut *tx)
        .await?;
        let web_providers = sqlx::query_as::<_, WebProvider>(
            "SELECT id, name, kind, api_key, use_proxy, local_engines, last_test_success, last_test_at, created_at, updated_at FROM web_providers ORDER BY created_at DESC",
        )
        .fetch_all(&mut *tx)
        .await?;
        let api_key_permissions = sqlx::query_scalar::<_, bool>(
            "SELECT COALESCE(is_enabled, 1) FROM api_keys WHERE id = ?",
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
        let mut conn = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
        let result: anyhow::Result<()> = async {
            let providers = sqlx::query_as::<_, WebProvider>(
                "SELECT id, name, kind, api_key, use_proxy, local_engines, last_test_success, last_test_at, created_at, updated_at FROM web_providers",
            )
            .fetch_all(&mut *conn)
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
                    "INSERT INTO settings (name, value) VALUES (?, ?) ON CONFLICT(name) DO UPDATE SET value = excluded.value, updated_at = datetime('now')",
                )
                .bind(key)
                .bind(value)
                .execute(&mut *conn)
                .await?;
            }
            Ok(())
        }
        .await;
        match result {
            Ok(()) => {
                sqlx::query("COMMIT").execute(&mut *conn).await?;
                Ok(())
            }
            Err(error) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                Err(error)
            }
        }
    }
}

fn parse_ids(value: Option<&String>) -> Vec<String> {
    value
        .and_then(|value| serde_json::from_str::<Vec<String>>(value).ok())
        .unwrap_or_default()
}
