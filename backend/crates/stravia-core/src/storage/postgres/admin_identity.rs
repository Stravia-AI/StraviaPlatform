use super::*;
use crate::storage::traits::{
    AdminIdentityRecord, AdminIdentityStore, AdminSessionRecord, NewAdminIdentity, NewAdminSession,
};

#[derive(Clone)]
pub(super) struct PostgresAdminIdentityStore {
    pub(super) pool: Pool<Postgres>,
}

#[async_trait]
impl AdminIdentityStore for PostgresAdminIdentityStore {
    async fn load_identity(&self) -> anyhow::Result<Option<AdminIdentityRecord>> {
        let row = sqlx::query_as::<_, (Option<String>, Option<String>, String, i64)>(
            "SELECT username, password_hash, jwt_secret, credential_revision \
             FROM admin_identity WHERE singleton_id = 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(
            |(username, password_hash, jwt_secret, credential_revision)| AdminIdentityRecord {
                username,
                password_hash,
                jwt_secret,
                credential_revision,
            },
        ))
    }

    async fn create_identity(&self, identity: NewAdminIdentity<'_>) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "INSERT INTO admin_identity \
             (singleton_id, username, password_hash, jwt_secret, credential_revision) \
             VALUES (1, $1, $2, $3, 1) ON CONFLICT DO NOTHING",
        )
        .bind(identity.username)
        .bind(identity.password_hash)
        .bind(identity.jwt_secret)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn create_session(&self, session: NewAdminSession<'_>) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "INSERT INTO admin_sessions \
             (id, identity_id, credential_revision, refresh_hash, expires_at, revoked) \
             SELECT $1, 1, credential_revision, $2, $3, FALSE FROM admin_identity \
             WHERE singleton_id = 1 AND credential_revision = $4",
        )
        .bind(session.id)
        .bind(session.refresh_hash)
        .bind(session.expires_at)
        .bind(session.credential_revision)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn load_session_by_id(&self, id: &str) -> anyhow::Result<Option<AdminSessionRecord>> {
        load_session(
            &self.pool,
            "SELECT id, credential_revision, refresh_hash, expires_at, revoked \
             FROM admin_sessions WHERE id = $1",
            id,
        )
        .await
    }

    async fn load_session_by_refresh_hash(
        &self,
        refresh_hash: &str,
    ) -> anyhow::Result<Option<AdminSessionRecord>> {
        load_session(
            &self.pool,
            "SELECT id, credential_revision, refresh_hash, expires_at, revoked \
             FROM admin_sessions WHERE refresh_hash = $1",
            refresh_hash,
        )
        .await
    }

    async fn rotate_refresh(
        &self,
        id: &str,
        expected_refresh_hash: &str,
        new_refresh_hash: &str,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE admin_sessions SET refresh_hash = $1 \
             WHERE id = $2 AND refresh_hash = $3 AND revoked = FALSE \
             AND credential_revision = (SELECT credential_revision FROM admin_identity WHERE singleton_id = 1)",
        )
        .bind(new_refresh_hash)
        .bind(id)
        .bind(expected_refresh_hash)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn revoke_session(&self, id: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE admin_sessions SET revoked = TRUE WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn update_credentials_and_revoke_all(
        &self,
        expected_revision: i64,
        username: &str,
        password_hash: &str,
    ) -> anyhow::Result<bool> {
        let mut transaction = self.pool.begin().await?;
        let result = sqlx::query(
            "UPDATE admin_identity SET username = $1, password_hash = $2, \
             credential_revision = credential_revision + 1 \
             WHERE singleton_id = 1 AND credential_revision = $3",
        )
        .bind(username)
        .bind(password_hash)
        .bind(expected_revision)
        .execute(&mut *transaction)
        .await?;
        if result.rows_affected() == 0 {
            transaction.rollback().await?;
            return Ok(false);
        }
        sqlx::query("UPDATE admin_sessions SET revoked = TRUE WHERE revoked = FALSE")
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(true)
    }

    async fn recover_credentials_and_revoke_all(
        &self,
        username: &str,
        password_hash: &str,
    ) -> anyhow::Result<bool> {
        let mut transaction = self.pool.begin().await?;
        let result = sqlx::query(
            "UPDATE admin_identity SET username = $1, password_hash = $2, \
             credential_revision = credential_revision + 1 WHERE singleton_id = 1",
        )
        .bind(username)
        .bind(password_hash)
        .execute(&mut *transaction)
        .await?;
        if result.rows_affected() == 0 {
            transaction.rollback().await?;
            return Ok(false);
        }
        sqlx::query("UPDATE admin_sessions SET revoked = TRUE WHERE revoked = FALSE")
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(true)
    }
}

async fn load_session(
    pool: &Pool<Postgres>,
    query: &'static str,
    value: &str,
) -> anyhow::Result<Option<AdminSessionRecord>> {
    let row = sqlx::query_as::<_, (String, i64, String, i64, bool)>(query)
        .bind(value)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(
        |(id, credential_revision, refresh_hash, expires_at, revoked)| AdminSessionRecord {
            id,
            credential_revision,
            refresh_hash,
            expires_at,
            revoked,
        },
    ))
}
