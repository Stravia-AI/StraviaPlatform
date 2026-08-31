use super::*;

#[derive(Clone)]
pub(super) struct PostgresBootstrap {
    pub(super) adapter: PostgresAdapter,
}

#[async_trait]
impl StorageBootstrap for PostgresBootstrap {
    async fn health(&self) -> anyhow::Result<StorageHealth> {
        let health = self.adapter.health().await;
        Ok(StorageHealth {
            backend: StorageBackend::Postgres,
            can_connect: health.can_connect,
            schema_compatible: health.schema_compatible,
            writable: health.can_connect,
        })
    }
}

pub(super) async fn pg_table_exists(
    pool: &Pool<Postgres>,
    table_name: &str,
) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT 1
            FROM information_schema.tables
            WHERE table_schema = current_schema()
              AND table_name = $1
        )",
    )
    .bind(table_name)
    .fetch_one(pool)
    .await?)
}

pub(super) fn provider_select(suffix: Option<&str>) -> String {
    let mut sql = String::from(
        "SELECT id, name, vendor, protocol, base_url, preset_key, channel, models_source, static_models, api_key, COALESCE(adapter_credentials, '{}') AS adapter_credentials, COALESCE(auth_mode, 'apikey') AS auth_mode, COALESCE(use_proxy, FALSE) AS use_proxy, last_test_success, to_char(last_test_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS last_test_at, COALESCE(is_enabled, TRUE) AS is_enabled, to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS created_at, to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS updated_at FROM providers",
    );
    if let Some(suffix) = suffix {
        sql.push(' ');
        sql.push_str(suffix);
    } else {
        sql.push_str(" ORDER BY created_at DESC");
    }
    sql
}

pub(super) fn api_key_select(suffix: Option<&str>) -> String {
    let mut sql = String::from(
        "SELECT id, token, name, concurrency_limit, COALESCE(is_enabled, TRUE) AS is_enabled, COALESCE(mcp_access_enabled, FALSE) AS mcp_access_enabled, COALESCE(transparent_injection_enabled, FALSE) AS transparent_injection_enabled, COALESCE(inject_media_understanding, FALSE) AS inject_media_understanding, COALESCE(inject_web_search, FALSE) AS inject_web_search, to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS expires_at, to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS created_at, to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS updated_at FROM api_keys",
    );
    if let Some(suffix) = suffix {
        sql.push(' ');
        sql.push_str(suffix);
    } else {
        sql.push_str(" ORDER BY created_at DESC");
    }
    sql
}

pub(super) fn api_key_with_bindings(row: ApiKey, model_ids: Vec<String>) -> ApiKeyWithBindings {
    ApiKeyWithBindings {
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
    }
}

pub(super) fn normalize_provider_vendor(vendor: Option<&str>) -> Option<String> {
    vendor
        .map(str::trim)
        .filter(|v| !v.is_empty() && *v != "custom")
        .map(|v| v.to_lowercase())
}
