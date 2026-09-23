use super::*;

#[derive(Clone)]
pub(super) struct PostgresOAuthCredentialStore {
    pub(super) pool: Pool<Postgres>,
}

#[async_trait]
impl OAuthCredentialStore for PostgresOAuthCredentialStore {
    async fn get(&self, provider_id: &str) -> anyhow::Result<Option<OAuthCredential>> {
        Ok(sqlx::query_as::<_, OAuthCredential>(
            "SELECT provider_id, connection_id, driver_key, scheme, access_token, refresh_token, to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS expires_at, resource_url, subject_id, scopes, meta, status, status_version, last_error, to_char(last_refresh_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS last_refresh_at, to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS created_at, to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS updated_at FROM provider_oauth_credentials WHERE provider_id = $1",
        )
        .bind(provider_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn upsert(
        &self,
        provider_id: &str,
        input: UpsertOAuthCredential,
    ) -> anyhow::Result<OAuthCredential> {
        let connection_id = stravia_runtime_contract::identifier::new_id();
        sqlx::query(
            "INSERT INTO provider_oauth_credentials (provider_id, connection_id, driver_key, scheme, access_token, refresh_token, expires_at, resource_url, subject_id, scopes, meta, status, status_version, last_error) VALUES ($1, $2, $3, $4, $5, $6, NULLIF($7, '')::timestamptz, $8, $9, $10, $11, 'connected', 0, NULL) ON CONFLICT(provider_id) DO UPDATE SET connection_id=EXCLUDED.connection_id, driver_key=EXCLUDED.driver_key, scheme=EXCLUDED.scheme, access_token=EXCLUDED.access_token, refresh_token=EXCLUDED.refresh_token, expires_at=EXCLUDED.expires_at, resource_url=EXCLUDED.resource_url, subject_id=EXCLUDED.subject_id, scopes=EXCLUDED.scopes, meta=EXCLUDED.meta, status='connected', status_version=provider_oauth_credentials.status_version+1, last_error=NULL, updated_at=CURRENT_TIMESTAMP",
        )
        .bind(provider_id)
        .bind(&connection_id)
        .bind(&input.driver_key)
        .bind(&input.scheme)
        .bind(&input.access_token)
        .bind(&input.refresh_token)
        .bind(bindable_timestamptz(input.expires_at.as_deref()))
        .bind(&input.resource_url)
        .bind(&input.subject_id)
        .bind(input.scopes.as_deref().unwrap_or("[]"))
        .bind(input.meta.as_deref().unwrap_or("{}"))
        .execute(&self.pool)
        .await?;
        self.get(provider_id)
            .await?
            .context("credential not found after upsert")
    }

    async fn delete(&self, provider_id: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM provider_oauth_credentials WHERE provider_id = $1")
            .bind(provider_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn try_begin_refresh(
        &self,
        provider_id: &str,
        expected_version: i32,
    ) -> anyhow::Result<Option<OAuthCredential>> {
        let result = sqlx::query(
            "UPDATE provider_oauth_credentials SET status='refreshing', status_version=status_version+1, updated_at=CURRENT_TIMESTAMP WHERE provider_id=$1 AND status='connected' AND status_version=$2",
        )
        .bind(provider_id)
        .bind(expected_version)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() > 0 {
            Ok(self.get(provider_id).await?)
        } else {
            Ok(None)
        }
    }

    async fn cancel_refresh(
        &self,
        provider_id: &str,
        expected_version: i32,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE provider_oauth_credentials SET status='connected', status_version=status_version+1, updated_at=CURRENT_TIMESTAMP WHERE provider_id=$1 AND status='refreshing' AND status_version=$2",
        )
        .bind(provider_id)
        .bind(expected_version)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn complete_refresh(
        &self,
        provider_id: &str,
        expected_version: i32,
        input: UpsertOAuthCredential,
    ) -> anyhow::Result<OAuthCredential> {
        let result = sqlx::query(
            "UPDATE provider_oauth_credentials SET driver_key=$1, scheme=$2, access_token=$3, refresh_token=$4, expires_at=NULLIF($5, '')::timestamptz, resource_url=$6, subject_id=$7, scopes=$8, meta=$9, status='connected', status_version=status_version+1, last_error=NULL, last_refresh_at=CURRENT_TIMESTAMP, updated_at=CURRENT_TIMESTAMP WHERE provider_id=$10 AND status='refreshing' AND status_version=$11",
        )
        .bind(&input.driver_key)
        .bind(&input.scheme)
        .bind(&input.access_token)
        .bind(&input.refresh_token)
        .bind(bindable_timestamptz(input.expires_at.as_deref()))
        .bind(&input.resource_url)
        .bind(&input.subject_id)
        .bind(input.scopes.as_deref().unwrap_or("[]"))
        .bind(input.meta.as_deref().unwrap_or("{}"))
        .bind(provider_id)
        .bind(expected_version)
        .execute(&self.pool)
        .await?;
        anyhow::ensure!(
            result.rows_affected() == 1,
            "credential refresh lease is no longer current"
        );
        self.get(provider_id)
            .await?
            .context("credential not found after complete_refresh")
    }

    async fn fail_refresh(
        &self,
        provider_id: &str,
        expected_version: i32,
        error_message: &str,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE provider_oauth_credentials SET status='error', last_error=$1, status_version=status_version+1, updated_at=CURRENT_TIMESTAMP WHERE provider_id=$2 AND status='refreshing' AND status_version=$3",
        )
        .bind(error_message)
        .bind(provider_id)
        .bind(expected_version)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn list_expiring(&self, before: Duration) -> anyhow::Result<Vec<OAuthCredential>> {
        let seconds = before.as_secs() as i64;
        Ok(sqlx::query_as::<_, OAuthCredential>(
            "SELECT provider_id, connection_id, driver_key, scheme, access_token, refresh_token, to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS expires_at, resource_url, subject_id, scopes, meta, status, status_version, last_error, to_char(last_refresh_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS last_refresh_at, to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS created_at, to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS updated_at FROM provider_oauth_credentials WHERE status='connected' AND expires_at IS NOT NULL AND expires_at <= CURRENT_TIMESTAMP + ($1 * INTERVAL '1 second')",
        )
        .bind(seconds)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn recover_stale_refreshing(&self, timeout: Duration) -> anyhow::Result<u64> {
        let seconds = timeout.as_secs() as i64;
        let result = sqlx::query(
            "UPDATE provider_oauth_credentials SET status='connected', last_error='refresh lease expired; retrying is allowed', status_version=status_version+1, updated_at=CURRENT_TIMESTAMP WHERE status='refreshing' AND updated_at + ($1 * INTERVAL '1 second') < CURRENT_TIMESTAMP",
        )
        .bind(seconds)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

// expires_at 以 text 参数进入（RFC3339 或读回格式 `YYYY-MM-DD HH24:MI:SS`）。
// timestamptz 列不接受 text 赋值，先归一化为带偏移的 RFC3339 再显式转换；
// naive 格式按 UTC 解释，与读取侧 `AT TIME ZONE 'UTC'` 对称。
fn bindable_timestamptz(value: Option<&str>) -> String {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return String::new();
    };
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|parsed| parsed.with_timezone(&chrono::Utc))
        .ok()
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
                .ok()
                .map(|parsed| chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(parsed, chrono::Utc))
        })
        .map(|parsed| parsed.to_rfc3339())
        .unwrap_or_else(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bindable_timestamptz_normalizes_to_rfc3339() {
        assert_eq!(bindable_timestamptz(None), "");
        assert_eq!(bindable_timestamptz(Some("  ")), "");
        assert_eq!(
            bindable_timestamptz(Some("2025-01-02 03:04:05")),
            "2025-01-02T03:04:05+00:00"
        );
        assert_eq!(
            bindable_timestamptz(Some("2025-01-02T03:04:05+08:00")),
            "2025-01-01T19:04:05+00:00"
        );
        // 无法解析的值原样下传，由数据库拒绝而非静默改写
        assert_eq!(bindable_timestamptz(Some("garbage")), "garbage");
    }
}
