use super::*;

#[derive(Clone)]
pub(super) struct SqliteOAuthCredentialStore {
    pub(super) pool: SqlitePool,
}

#[async_trait]
impl OAuthCredentialStore for SqliteOAuthCredentialStore {
    async fn get(&self, provider_id: &str) -> anyhow::Result<Option<OAuthCredential>> {
        let row = sqlx::query_as::<_, OAuthCredential>(
            r#"SELECT provider_id, connection_id, driver_key, scheme, access_token, refresh_token,
                      expires_at, resource_url, subject_id, scopes, meta,
                      status, status_version, last_error, last_refresh_at,
                      created_at, updated_at
               FROM provider_oauth_credentials WHERE provider_id = ?"#,
        )
        .bind(provider_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn upsert(
        &self,
        provider_id: &str,
        input: UpsertOAuthCredential,
    ) -> anyhow::Result<OAuthCredential> {
        let connection_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            r#"INSERT INTO provider_oauth_credentials
                   (provider_id, connection_id, driver_key, scheme, access_token, refresh_token,
                    expires_at, resource_url, subject_id, scopes, meta,
                    status, status_version, last_error, created_at, updated_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'connected', 0, NULL, datetime('now'), datetime('now'))
               ON CONFLICT(provider_id) DO UPDATE SET
                   connection_id = excluded.connection_id,
                   driver_key = excluded.driver_key,
                   scheme = excluded.scheme,
                   access_token = excluded.access_token,
                   refresh_token = excluded.refresh_token,
                   expires_at = excluded.expires_at,
                   resource_url = excluded.resource_url,
                   subject_id = excluded.subject_id,
                   scopes = excluded.scopes,
                   meta = excluded.meta,
                   status = 'connected',
                   status_version = status_version + 1,
                   last_error = NULL,
                   updated_at = datetime('now')
            "#,
        )
        .bind(provider_id)
        .bind(&connection_id)
        .bind(&input.driver_key)
        .bind(&input.scheme)
        .bind(&input.access_token)
        .bind(&input.refresh_token)
        .bind(&input.expires_at)
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
        sqlx::query("DELETE FROM provider_oauth_credentials WHERE provider_id = ?")
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
            r#"UPDATE provider_oauth_credentials
               SET status = 'refreshing', status_version = status_version + 1,
                   updated_at = datetime('now')
               WHERE provider_id = ? AND status = 'connected' AND status_version = ?"#,
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
            r#"UPDATE provider_oauth_credentials
               SET status = 'connected', status_version = status_version + 1,
                   updated_at = datetime('now')
               WHERE provider_id = ? AND status = 'refreshing' AND status_version = ?"#,
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
            r#"UPDATE provider_oauth_credentials SET
                   driver_key = ?, scheme = ?,
                   access_token = ?, refresh_token = ?, expires_at = ?,
                   resource_url = ?, subject_id = ?,
                   scopes = ?, meta = ?,
                   status = 'connected', status_version = status_version + 1,
                   last_error = NULL, last_refresh_at = datetime('now'),
                   updated_at = datetime('now')
               WHERE provider_id = ? AND status = 'refreshing' AND status_version = ?"#,
        )
        .bind(&input.driver_key)
        .bind(&input.scheme)
        .bind(&input.access_token)
        .bind(&input.refresh_token)
        .bind(&input.expires_at)
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
            r#"UPDATE provider_oauth_credentials SET
                   status = 'error', last_error = ?,
                   status_version = status_version + 1,
                   updated_at = datetime('now')
               WHERE provider_id = ? AND status = 'refreshing' AND status_version = ?"#,
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
        let rows = sqlx::query_as::<_, OAuthCredential>(
            r#"SELECT provider_id, connection_id, driver_key, scheme, access_token, refresh_token,
                      expires_at, resource_url, subject_id, scopes, meta,
                      status, status_version, last_error, last_refresh_at,
                      created_at, updated_at
               FROM provider_oauth_credentials
               WHERE status = 'connected'
                 AND expires_at IS NOT NULL
                 AND datetime(expires_at) <= datetime('now', '+' || ? || ' seconds')"#,
        )
        .bind(seconds)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn recover_stale_refreshing(&self, timeout: Duration) -> anyhow::Result<u64> {
        let seconds = timeout.as_secs() as i64;
        let result = sqlx::query(
            r#"UPDATE provider_oauth_credentials SET
                   status = 'connected',
                   last_error = 'refresh lease expired; retrying is allowed',
                   status_version = status_version + 1,
                   updated_at = datetime('now')
               WHERE status = 'refreshing'
                 AND datetime(updated_at, '+' || ? || ' seconds') < datetime('now')"#,
        )
        .bind(seconds)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}
