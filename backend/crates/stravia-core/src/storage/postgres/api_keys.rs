use super::*;

#[derive(Clone)]
pub(super) struct PostgresApiKeyStore {
    pub(super) pool: Pool<Postgres>,
}

#[async_trait]
impl ApiKeyStore for PostgresApiKeyStore {
    async fn list(&self) -> anyhow::Result<Vec<ApiKeyWithBindings>> {
        let rows = sqlx::query_as::<_, ApiKey>(sqlx::AssertSqlSafe(api_key_select(None)))
            .fetch_all(&self.pool)
            .await?;
        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            let model_ids = list_api_key_model_ids(&self.pool, &row.id).await?;
            items.push(api_key_with_bindings(row, model_ids));
        }
        Ok(items)
    }

    async fn get(&self, id: &str) -> anyhow::Result<Option<ApiKeyWithBindings>> {
        let row =
            sqlx::query_as::<_, ApiKey>(sqlx::AssertSqlSafe(api_key_select(Some("WHERE id = $1"))))
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let model_ids = list_api_key_model_ids(&self.pool, id).await?;
        Ok(Some(api_key_with_bindings(row, model_ids)))
    }

    async fn create(&self, input: CreateApiKey) -> anyhow::Result<ApiKeyWithBindings> {
        let id = uuid::Uuid::new_v4().to_string();
        let key = input
            .key
            .unwrap_or_else(|| format!("sk-{}", uuid::Uuid::new_v4().simple()));
        sqlx::query(
            "INSERT INTO api_keys (id, token, name, concurrency_limit, mcp_access_enabled, transparent_injection_enabled, inject_media_understanding, inject_web_search, expires_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NULLIF($9, '')::timestamptz)",
        )
        .bind(&id)
        .bind(&key)
        .bind(input.name.trim())
        .bind(input.concurrency_limit)
        .bind(input.mcp_access_enabled)
        .bind(input.transparent_injection_enabled)
        .bind(input.inject_media_understanding)
        .bind(input.inject_web_search)
        .bind(input.expires_at.as_deref().map(str::trim).unwrap_or(""))
        .execute(&self.pool)
        .await?;
        replace_api_key_models(&self.pool, &id, &input.model_ids).await?;
        self.get(&id).await?.context("api key missing after create")
    }

    async fn update(&self, id: &str, input: UpdateApiKey) -> anyhow::Result<ApiKeyWithBindings> {
        let current =
            sqlx::query_as::<_, ApiKey>(sqlx::AssertSqlSafe(api_key_select(Some("WHERE id = $1"))))
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
            "UPDATE api_keys SET token=$1, name=$2, concurrency_limit=$3, is_enabled=$4, mcp_access_enabled=$5, transparent_injection_enabled=$6, inject_media_understanding=$7, inject_web_search=$8, expires_at=NULLIF($9, '')::timestamptz, updated_at=CURRENT_TIMESTAMP WHERE id=$10",
        )
        .bind(key)
        .bind(name.trim())
        .bind(concurrency_limit)
        .bind(is_enabled)
        .bind(mcp_access_enabled)
        .bind(transparent_injection_enabled)
        .bind(inject_media_understanding)
        .bind(inject_web_search)
        .bind(expires_at.as_deref().map(str::trim).unwrap_or(""))
        .bind(id)
        .execute(&self.pool)
        .await?;

        if let Some(model_ids) = input.model_ids {
            replace_api_key_models(&self.pool, id, &model_ids).await?;
        }
        self.get(id).await?.context("api key missing after update")
    }

    async fn delete(&self, id: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM api_keys WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn exists_by_name(&self, name: &str, exclude_id: Option<&str>) -> anyhow::Result<bool> {
        let row = if let Some(exclude_id) = exclude_id {
            sqlx::query_scalar::<_, String>(
                "SELECT id FROM api_keys WHERE lower(trim(name)) = lower(trim($1)) AND id != $2 LIMIT 1",
            )
            .bind(name)
            .bind(exclude_id)
            .fetch_optional(&self.pool)
            .await?
        } else {
            sqlx::query_scalar::<_, String>(
                "SELECT id FROM api_keys WHERE lower(trim(name)) = lower(trim($1)) LIMIT 1",
            )
            .bind(name)
            .fetch_optional(&self.pool)
            .await?
        };
        Ok(row.is_some())
    }

    async fn exists_by_key(&self, key: &str, exclude_id: Option<&str>) -> anyhow::Result<bool> {
        let row = if let Some(exclude_id) = exclude_id {
            sqlx::query_scalar::<_, String>(
                "SELECT id FROM api_keys WHERE token = $1 AND id != $2 LIMIT 1",
            )
            .bind(key)
            .bind(exclude_id)
            .fetch_optional(&self.pool)
            .await?
        } else {
            sqlx::query_scalar::<_, String>("SELECT id FROM api_keys WHERE token = $1 LIMIT 1")
                .bind(key)
                .fetch_optional(&self.pool)
                .await?
        };
        Ok(row.is_some())
    }
}

#[derive(Clone)]
pub(super) struct PostgresAuthAccessStore {
    pub(super) pool: Pool<Postgres>,
}

#[async_trait]
impl AuthAccessStore for PostgresAuthAccessStore {
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
        >(
            "SELECT id, COALESCE(name, '') AS name, COALESCE(is_enabled, TRUE) AS is_enabled, to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS expires_at, concurrency_limit, COALESCE(transparent_injection_enabled, FALSE) AS transparent_injection_enabled, COALESCE(inject_media_understanding, FALSE) AS inject_media_understanding, COALESCE(inject_web_search, FALSE) AS inject_web_search FROM api_keys WHERE token = $1",
        )
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
        >(
            "SELECT id, COALESCE(name, '') AS name, COALESCE(is_enabled, TRUE) AS is_enabled, to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS expires_at, concurrency_limit, COALESCE(transparent_injection_enabled, FALSE) AS transparent_injection_enabled, COALESCE(inject_media_understanding, FALSE) AS inject_media_understanding, COALESCE(inject_web_search, FALSE) AS inject_web_search FROM api_keys WHERE id = $1",
        )
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

    async fn model_binding_exists(&self, api_key_id: &str, model_id: &str) -> anyhow::Result<bool> {
        let count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM api_key_models WHERE api_key_id = $1 AND model_id = $2",
        )
        .bind(api_key_id)
        .bind(model_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(count > 0)
    }

    async fn list_bound_model_ids(&self, api_key_id: &str) -> anyhow::Result<Vec<String>> {
        list_api_key_model_ids(&self.pool, api_key_id).await
    }
}

pub(super) async fn list_api_key_model_ids(
    pool: &Pool<Postgres>,
    api_key_id: &str,
) -> anyhow::Result<Vec<String>> {
    Ok(sqlx::query_scalar::<_, String>(
        "SELECT model_id FROM api_key_models WHERE api_key_id = $1 ORDER BY model_id ASC",
    )
    .bind(api_key_id)
    .fetch_all(pool)
    .await?)
}

pub(super) async fn replace_api_key_models(
    pool: &Pool<Postgres>,
    api_key_id: &str,
    model_ids: &[String],
) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM api_key_models WHERE api_key_id = $1")
        .bind(api_key_id)
        .execute(&mut *tx)
        .await?;

    for model_id in model_ids.iter().filter(|id| !id.trim().is_empty()) {
        sqlx::query(
            "INSERT INTO api_key_models (api_key_id, model_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(api_key_id)
        .bind(model_id.trim())
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}
