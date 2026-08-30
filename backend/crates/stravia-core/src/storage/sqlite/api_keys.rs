use super::*;

#[derive(Clone)]
pub(super) struct SqliteApiKeyStore {
    pub(super) pool: SqlitePool,
}

#[async_trait]
impl ApiKeyStore for SqliteApiKeyStore {
    async fn list(&self) -> anyhow::Result<Vec<ApiKeyWithBindings>> {
        let rows = sqlx::query_as::<_, ApiKey>(
            "SELECT id, token, name, concurrency_limit, COALESCE(is_enabled, 1) AS is_enabled, COALESCE(mcp_access_enabled, 0) AS mcp_access_enabled, COALESCE(transparent_injection_enabled, 0) AS transparent_injection_enabled, COALESCE(inject_media_understanding, 0) AS inject_media_understanding, COALESCE(inject_web_search, 0) AS inject_web_search, expires_at, created_at, updated_at FROM api_keys ORDER BY created_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;

        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            let model_ids = list_api_key_model_ids(&self.pool, &row.id).await?;
            items.push(ApiKeyWithBindings {
                id: row.id,
                token: row.token,
                name: row.name,
                concurrency_limit: row.concurrency_limit,
                is_enabled: row.is_enabled,
                mcp_access_enabled: row.mcp_access_enabled,
                transparent_injection_enabled: row.transparent_injection_enabled,
                inject_media_understanding: row.inject_media_understanding,
                inject_web_search: row.inject_web_search,
                expires_at: row.expires_at,
                created_at: row.created_at,
                updated_at: row.updated_at,
                model_ids,
            });
        }
        Ok(items)
    }

    async fn get(&self, id: &str) -> anyhow::Result<Option<ApiKeyWithBindings>> {
        let row = sqlx::query_as::<_, ApiKey>(
            "SELECT id, token, name, concurrency_limit, COALESCE(is_enabled, 1) AS is_enabled, COALESCE(mcp_access_enabled, 0) AS mcp_access_enabled, COALESCE(transparent_injection_enabled, 0) AS transparent_injection_enabled, COALESCE(inject_media_understanding, 0) AS inject_media_understanding, COALESCE(inject_web_search, 0) AS inject_web_search, expires_at, created_at, updated_at FROM api_keys WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };
        let model_ids = list_api_key_model_ids(&self.pool, id).await?;
        Ok(Some(ApiKeyWithBindings {
            id: row.id,
            token: row.token,
            name: row.name,
            concurrency_limit: row.concurrency_limit,
            is_enabled: row.is_enabled,
            mcp_access_enabled: row.mcp_access_enabled,
            transparent_injection_enabled: row.transparent_injection_enabled,
            inject_media_understanding: row.inject_media_understanding,
            inject_web_search: row.inject_web_search,
            expires_at: row.expires_at,
            created_at: row.created_at,
            updated_at: row.updated_at,
            model_ids,
        }))
    }

    async fn create(&self, input: CreateApiKey) -> anyhow::Result<ApiKeyWithBindings> {
        let id = uuid::Uuid::new_v4().to_string();
        let key = input
            .key
            .unwrap_or_else(|| format!("sk-{}", uuid::Uuid::new_v4().simple()));
        sqlx::query(
            "INSERT INTO api_keys (id, token, name, concurrency_limit, mcp_access_enabled, transparent_injection_enabled, inject_media_understanding, inject_web_search, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&key)
        .bind(input.name.trim())
        .bind(input.concurrency_limit)
        .bind(input.mcp_access_enabled)
        .bind(input.transparent_injection_enabled)
        .bind(input.inject_media_understanding)
        .bind(input.inject_web_search)
        .bind(input.expires_at.as_ref().map(|v| v.trim()).filter(|v| !v.is_empty()))
        .execute(&self.pool)
        .await?;

        replace_api_key_models(&self.pool, &id, &input.model_ids).await?;
        self.get(&id).await?.context("api key missing after create")
    }

    async fn update(&self, id: &str, input: UpdateApiKey) -> anyhow::Result<ApiKeyWithBindings> {
        let current = sqlx::query_as::<_, ApiKey>(
            "SELECT id, token, name, concurrency_limit, COALESCE(is_enabled, 1) AS is_enabled, COALESCE(mcp_access_enabled, 0) AS mcp_access_enabled, COALESCE(transparent_injection_enabled, 0) AS transparent_injection_enabled, COALESCE(inject_media_understanding, 0) AS inject_media_understanding, COALESCE(inject_web_search, 0) AS inject_web_search, expires_at, created_at, updated_at FROM api_keys WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .context("api key not found for update")?;

        let name = input.name.unwrap_or(current.name);
        let key = input.key.unwrap_or(current.token);
        let concurrency_limit = input.concurrency_limit.unwrap_or(current.concurrency_limit);
        let is_enabled = input.is_enabled.unwrap_or(current.is_enabled);
        let mcp_access_enabled = input
            .mcp_access_enabled
            .unwrap_or(current.mcp_access_enabled);
        let transparent_injection_enabled = input
            .transparent_injection_enabled
            .unwrap_or(current.transparent_injection_enabled);
        let inject_media_understanding = input
            .inject_media_understanding
            .unwrap_or(current.inject_media_understanding);
        let inject_web_search = input.inject_web_search.unwrap_or(current.inject_web_search);
        let expires_at = input.expires_at.or(current.expires_at);

        sqlx::query(
            "UPDATE api_keys SET token=?, name=?, concurrency_limit=?, is_enabled=?, mcp_access_enabled=?, transparent_injection_enabled=?, inject_media_understanding=?, inject_web_search=?, expires_at=?, updated_at=datetime('now') WHERE id=?",
        )
        .bind(key)
        .bind(name.trim())
        .bind(concurrency_limit)
        .bind(is_enabled)
        .bind(mcp_access_enabled)
        .bind(transparent_injection_enabled)
        .bind(inject_media_understanding)
        .bind(inject_web_search)
        .bind(expires_at.as_ref().map(|v| v.trim()).filter(|v| !v.is_empty()))
        .bind(id)
        .execute(&self.pool)
        .await?;

        if let Some(model_ids) = input.model_ids {
            replace_api_key_models(&self.pool, id, &model_ids).await?;
        }

        self.get(id).await?.context("api key missing after update")
    }

    async fn delete(&self, id: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM api_keys WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn exists_by_name(&self, name: &str, exclude_id: Option<&str>) -> anyhow::Result<bool> {
        let sql = if exclude_id.is_some() {
            "SELECT id FROM api_keys WHERE lower(trim(name)) = lower(trim(?)) AND id != ? LIMIT 1"
        } else {
            "SELECT id FROM api_keys WHERE lower(trim(name)) = lower(trim(?)) LIMIT 1"
        };

        let row = if let Some(exclude_id) = exclude_id {
            sqlx::query_scalar::<_, String>(sql)
                .bind(name)
                .bind(exclude_id)
                .fetch_optional(&self.pool)
                .await?
        } else {
            sqlx::query_scalar::<_, String>(sql)
                .bind(name)
                .fetch_optional(&self.pool)
                .await?
        };
        Ok(row.is_some())
    }

    async fn exists_by_key(&self, key: &str, exclude_id: Option<&str>) -> anyhow::Result<bool> {
        let row = if let Some(exclude_id) = exclude_id {
            sqlx::query_scalar::<_, String>(
                "SELECT id FROM api_keys WHERE token = ? AND id != ? LIMIT 1",
            )
            .bind(key)
            .bind(exclude_id)
            .fetch_optional(&self.pool)
            .await?
        } else {
            sqlx::query_scalar::<_, String>("SELECT id FROM api_keys WHERE token = ? LIMIT 1")
                .bind(key)
                .fetch_optional(&self.pool)
                .await?
        };
        Ok(row.is_some())
    }
}

#[derive(Clone)]
pub(super) struct SqliteAuthAccessStore {
    pub(super) pool: SqlitePool,
}

#[async_trait]
impl AuthAccessStore for SqliteAuthAccessStore {
    async fn find_api_key(&self, raw_key: &str) -> anyhow::Result<Option<ApiKeyAccessRecord>> {
        let row = sqlx::query_as::<
            _,
            (
                String,
                String,
                bool,
                Option<String>,
                Option<i32>,
                bool,
                bool,
                bool,
            ),
        >("SELECT id, COALESCE(name, '') AS name, COALESCE(is_enabled, 1) AS is_enabled, expires_at, concurrency_limit, COALESCE(transparent_injection_enabled, 0) AS transparent_injection_enabled, COALESCE(inject_media_understanding, 0) AS inject_media_understanding, COALESCE(inject_web_search, 0) AS inject_web_search FROM api_keys WHERE token = ?")
        .bind(raw_key)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(
            |(
                id,
                name,
                is_enabled,
                expires_at,
                concurrency_limit,
                transparent_injection_enabled,
                inject_media_understanding,
                inject_web_search,
            )| ApiKeyAccessRecord {
                id,
                name,
                is_enabled,
                expires_at,
                concurrency_limit,
                transparent_injection_enabled,
                inject_media_understanding,
                inject_web_search,
            },
        ))
    }

    async fn find_api_key_by_id(&self, id: &str) -> anyhow::Result<Option<ApiKeyAccessRecord>> {
        let row = sqlx::query_as::<
            _,
            (
                String,
                String,
                bool,
                Option<String>,
                Option<i32>,
                bool,
                bool,
                bool,
            ),
        >("SELECT id, COALESCE(name, '') AS name, COALESCE(is_enabled, 1) AS is_enabled, expires_at, concurrency_limit, COALESCE(transparent_injection_enabled, 0) AS transparent_injection_enabled, COALESCE(inject_media_understanding, 0) AS inject_media_understanding, COALESCE(inject_web_search, 0) AS inject_web_search FROM api_keys WHERE id = ?")
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(
            |(
                id,
                name,
                is_enabled,
                expires_at,
                concurrency_limit,
                transparent_injection_enabled,
                inject_media_understanding,
                inject_web_search,
            )| ApiKeyAccessRecord {
                id,
                name,
                is_enabled,
                expires_at,
                concurrency_limit,
                transparent_injection_enabled,
                inject_media_understanding,
                inject_web_search,
            },
        ))
    }

    async fn model_access_allowed(&self, api_key_id: &str, model_id: &str) -> anyhow::Result<bool> {
        sqlx::query_scalar::<_, bool>(
            "SELECT
                EXISTS (SELECT 1 FROM api_keys WHERE id = ?)
                AND (
                    NOT EXISTS (SELECT 1 FROM api_key_models WHERE api_key_id = ?)
                    OR EXISTS (
                        SELECT 1 FROM api_key_models WHERE api_key_id = ? AND model_id = ?
                    )
                )",
        )
        .bind(api_key_id)
        .bind(api_key_id)
        .bind(api_key_id)
        .bind(model_id)
        .fetch_one(&self.pool)
        .await
        .map_err(Into::into)
    }

    async fn list_bound_model_ids(&self, api_key_id: &str) -> anyhow::Result<Vec<String>> {
        list_api_key_model_ids(&self.pool, api_key_id).await
    }
}

pub(super) async fn list_api_key_model_ids(
    pool: &SqlitePool,
    api_key_id: &str,
) -> anyhow::Result<Vec<String>> {
    Ok(sqlx::query_scalar::<_, String>(
        "SELECT model_id FROM api_key_models WHERE api_key_id = ? ORDER BY model_id ASC",
    )
    .bind(api_key_id)
    .fetch_all(pool)
    .await?)
}

pub(super) async fn replace_api_key_models(
    pool: &SqlitePool,
    api_key_id: &str,
    model_ids: &[String],
) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM api_key_models WHERE api_key_id = ?")
        .bind(api_key_id)
        .execute(&mut *tx)
        .await?;

    for model_id in model_ids.iter().filter(|id| !id.trim().is_empty()) {
        sqlx::query("INSERT OR IGNORE INTO api_key_models (api_key_id, model_id) VALUES (?, ?)")
            .bind(api_key_id)
            .bind(model_id.trim())
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;
    Ok(())
}
