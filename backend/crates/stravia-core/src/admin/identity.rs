use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use argon2::{
    Argon2,
    password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::storage::{
    AdminIdentityRecord, AdminIdentityStore, DynStorage, NewAdminIdentity, NewAdminSession,
};

const ACCESS_LIFETIME_SECONDS: i64 = 15 * 60;
const SESSION_LIFETIME_SECONDS: i64 = 7 * 24 * 60 * 60;
const ROLE_ADMIN: &str = "admin";

#[derive(Clone)]
pub struct AdminAuth {
    storage: DynStorage,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
}

#[derive(Clone, Serialize)]
pub struct SessionTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub access_expires_at: i64,
    pub session_expires_at: i64,
    pub session_id: String,
}

#[derive(Clone, Serialize)]
pub struct AdminSession {
    pub session_id: String,
    pub username: Option<String>,
    pub role: String,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("invalid administrator credentials")]
    InvalidCredentials,
    #[error("administrator authorization required")]
    Unauthorized,
    #[error("an administrator already exists")]
    Conflict,
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("administrator storage failure")]
    Storage(#[source] anyhow::Error),
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum TokenType {
    Access,
}

#[derive(Serialize, Deserialize)]
struct AccessClaims {
    typ: TokenType,
    sid: String,
    rev: i64,
    iat: i64,
    exp: i64,
}

impl AdminAuth {
    pub fn new(storage: DynStorage) -> Self {
        Self {
            storage,
            clock: Arc::new(system_time_seconds),
        }
    }

    #[cfg(test)]
    fn with_clock(storage: DynStorage, clock: Arc<dyn Fn() -> i64 + Send + Sync>) -> Self {
        Self { storage, clock }
    }

    pub async fn has_admin(&self) -> Result<bool, AuthError> {
        Ok(self
            .store()?
            .load_identity()
            .await
            .map_err(storage_error)?
            .is_some())
    }

    pub async fn create_admin(&self, username: &str, password: &str) -> Result<(), AuthError> {
        let username = validate_username(username)?;
        validate_password(password)?;
        let password_hash = hash_password(password).await?;
        let jwt_secret = random_token();
        let created = self
            .store()?
            .create_identity(NewAdminIdentity {
                username: Some(username),
                password_hash: Some(&password_hash),
                jwt_secret: &jwt_secret,
            })
            .await
            .map_err(storage_error)?;
        if !created {
            return Err(AuthError::Conflict);
        }
        Ok(())
    }

    pub async fn ensure_native_admin(&self) -> Result<(), AuthError> {
        if self
            .store()?
            .load_identity()
            .await
            .map_err(storage_error)?
            .is_some()
        {
            return Ok(());
        }
        let jwt_secret = random_token();
        self.store()?
            .create_identity(NewAdminIdentity {
                username: None,
                password_hash: None,
                jwt_secret: &jwt_secret,
            })
            .await
            .map_err(storage_error)?;
        Ok(())
    }

    pub async fn login(&self, username: &str, password: &str) -> Result<SessionTokens, AuthError> {
        let identity = self
            .store()?
            .load_identity()
            .await
            .map_err(storage_error)?
            .ok_or(AuthError::InvalidCredentials)?;
        let (stored_username, password_hash) = match (&identity.username, &identity.password_hash) {
            (Some(username), Some(password_hash)) => (username, password_hash),
            _ => return Err(AuthError::InvalidCredentials),
        };
        let password_valid = verify_password(password, password_hash.clone()).await?;
        if stored_username != username || !password_valid {
            return Err(AuthError::InvalidCredentials);
        }
        self.create_session(&identity).await
    }

    pub async fn login_native(&self) -> Result<SessionTokens, AuthError> {
        let identity = self
            .store()?
            .load_identity()
            .await
            .map_err(storage_error)?
            .ok_or(AuthError::Unauthorized)?;
        self.create_session(&identity).await
    }

    pub async fn refresh(&self, refresh_token: &str) -> Result<SessionTokens, AuthError> {
        if refresh_token.is_empty() {
            return Err(AuthError::Unauthorized);
        }
        let current_hash = hash_refresh_token(refresh_token);
        let session = self
            .store()?
            .load_session_by_refresh_hash(&current_hash)
            .await
            .map_err(storage_error)?
            .ok_or(AuthError::Unauthorized)?;
        let now = (self.clock)();
        if session.revoked || session.expires_at <= now {
            return Err(AuthError::Unauthorized);
        }
        let identity = self
            .store()?
            .load_identity()
            .await
            .map_err(storage_error)?
            .ok_or(AuthError::Unauthorized)?;
        if identity.credential_revision != session.credential_revision {
            return Err(AuthError::Unauthorized);
        }

        let new_refresh_token = random_token();
        let new_refresh_hash = hash_refresh_token(&new_refresh_token);
        let rotated = self
            .store()?
            .rotate_refresh(&session.id, &current_hash, &new_refresh_hash)
            .await
            .map_err(storage_error)?;
        if !rotated {
            return Err(AuthError::Unauthorized);
        }
        self.tokens_for_session(
            &identity,
            &session.id,
            session.expires_at,
            new_refresh_token,
            now,
        )
    }

    pub async fn authenticate(&self, access_token: &str) -> Result<AdminSession, AuthError> {
        let identity = self
            .store()?
            .load_identity()
            .await
            .map_err(storage_error)?
            .ok_or(AuthError::Unauthorized)?;
        let mut validation = Validation::new(Algorithm::HS256);
        validation.validate_exp = false;
        validation.leeway = 0;
        validation.set_required_spec_claims(&["exp"]);
        let claims = decode::<AccessClaims>(
            access_token,
            &DecodingKey::from_secret(identity.jwt_secret.as_bytes()),
            &validation,
        )
        .map_err(|_| AuthError::Unauthorized)?
        .claims;
        let now = (self.clock)();
        if claims.typ != TokenType::Access || claims.exp <= now || claims.iat > now {
            return Err(AuthError::Unauthorized);
        }
        let session = self
            .store()?
            .load_session_by_id(&claims.sid)
            .await
            .map_err(storage_error)?
            .ok_or(AuthError::Unauthorized)?;
        if session.revoked
            || session.expires_at <= now
            || session.credential_revision != claims.rev
            || identity.credential_revision != claims.rev
        {
            return Err(AuthError::Unauthorized);
        }
        Ok(AdminSession {
            session_id: session.id,
            username: identity.username,
            role: ROLE_ADMIN.to_string(),
        })
    }

    pub async fn logout(&self, session_id: &str) -> Result<(), AuthError> {
        self.store()?
            .revoke_session(session_id)
            .await
            .map_err(storage_error)
    }

    pub async fn change_credentials(
        &self,
        session: &AdminSession,
        current_password: &str,
        username: &str,
        password: &str,
    ) -> Result<(), AuthError> {
        let username = validate_username(username)?;
        validate_password(password)?;
        let stored_session = self
            .store()?
            .load_session_by_id(&session.session_id)
            .await
            .map_err(storage_error)?
            .ok_or(AuthError::Unauthorized)?;
        let now = (self.clock)();
        if stored_session.revoked || stored_session.expires_at <= now {
            return Err(AuthError::Unauthorized);
        }
        let identity = self
            .store()?
            .load_identity()
            .await
            .map_err(storage_error)?
            .ok_or(AuthError::Unauthorized)?;
        if identity.credential_revision != stored_session.credential_revision {
            return Err(AuthError::Unauthorized);
        }
        let password_hash = identity
            .password_hash
            .ok_or(AuthError::InvalidCredentials)?;
        if !verify_password(current_password, password_hash).await? {
            return Err(AuthError::InvalidCredentials);
        }
        let new_password_hash = hash_password(password).await?;
        let updated = self
            .store()?
            .update_credentials_and_revoke_all(
                identity.credential_revision,
                username,
                &new_password_hash,
            )
            .await
            .map_err(storage_error)?;
        if !updated {
            return Err(AuthError::Unauthorized);
        }
        Ok(())
    }

    pub async fn recover_credentials(
        &self,
        username: &str,
        password: &str,
    ) -> Result<(), AuthError> {
        let username = validate_username(username)?;
        validate_password(password)?;
        let password_hash = hash_password(password).await?;
        let updated = self
            .store()?
            .recover_credentials_and_revoke_all(username, &password_hash)
            .await
            .map_err(storage_error)?;
        if !updated {
            return Err(AuthError::Conflict);
        }
        Ok(())
    }

    async fn create_session(
        &self,
        identity: &AdminIdentityRecord,
    ) -> Result<SessionTokens, AuthError> {
        let now = (self.clock)();
        let session_expires_at = now
            .checked_add(SESSION_LIFETIME_SECONDS)
            .ok_or_else(|| AuthError::Storage(anyhow::anyhow!("system clock is out of range")))?;
        let session_id = uuid::Uuid::new_v4().to_string();
        let refresh_token = random_token();
        let refresh_hash = hash_refresh_token(&refresh_token);
        let created = self
            .store()?
            .create_session(NewAdminSession {
                id: &session_id,
                credential_revision: identity.credential_revision,
                refresh_hash: &refresh_hash,
                expires_at: session_expires_at,
            })
            .await
            .map_err(storage_error)?;
        if !created {
            return Err(AuthError::Unauthorized);
        }
        self.tokens_for_session(
            identity,
            &session_id,
            session_expires_at,
            refresh_token,
            now,
        )
    }

    fn tokens_for_session(
        &self,
        identity: &AdminIdentityRecord,
        session_id: &str,
        session_expires_at: i64,
        refresh_token: String,
        now: i64,
    ) -> Result<SessionTokens, AuthError> {
        let access_expires_at = now
            .checked_add(ACCESS_LIFETIME_SECONDS)
            .map(|expiry| expiry.min(session_expires_at))
            .ok_or_else(|| AuthError::Storage(anyhow::anyhow!("system clock is out of range")))?;
        if access_expires_at <= now {
            return Err(AuthError::Unauthorized);
        }
        let claims = AccessClaims {
            typ: TokenType::Access,
            sid: session_id.to_string(),
            rev: identity.credential_revision,
            iat: now,
            exp: access_expires_at,
        };
        let access_token = encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(identity.jwt_secret.as_bytes()),
        )
        .map_err(|error| {
            AuthError::Storage(
                anyhow::Error::new(error).context("failed to sign administrator access token"),
            )
        })?;
        Ok(SessionTokens {
            access_token,
            refresh_token,
            access_expires_at,
            session_expires_at,
            session_id: session_id.to_string(),
        })
    }

    fn store(&self) -> Result<&dyn AdminIdentityStore, AuthError> {
        self.storage.admin_identity().ok_or_else(|| {
            AuthError::Storage(anyhow::anyhow!(
                "selected storage backend does not support administrator identity"
            ))
        })
    }
}

fn storage_error(error: anyhow::Error) -> AuthError {
    AuthError::Storage(error)
}

fn validate_username(username: &str) -> Result<&str, AuthError> {
    let username = username.trim();
    if username.is_empty() {
        return Err(AuthError::InvalidInput(
            "username must not be empty".to_string(),
        ));
    }
    if username.len() > 256 {
        return Err(AuthError::InvalidInput("username is too long".to_string()));
    }
    Ok(username)
}

fn validate_password(password: &str) -> Result<(), AuthError> {
    if password.is_empty() {
        return Err(AuthError::InvalidInput(
            "password must not be empty".to_string(),
        ));
    }
    if password.len() > 4096 {
        return Err(AuthError::InvalidInput("password is too long".to_string()));
    }
    Ok(())
}

async fn hash_password(password: &str) -> Result<String, AuthError> {
    let password = password.as_bytes().to_vec();
    tokio::task::spawn_blocking(move || {
        Argon2::default()
            .hash_password(&password)
            .map(|hash| hash.to_string())
            .map_err(|error| anyhow::anyhow!("failed to hash administrator password: {error}"))
    })
    .await
    .map_err(|error| AuthError::Storage(anyhow::Error::new(error)))?
    .map_err(AuthError::Storage)
}

async fn verify_password(password: &str, password_hash: String) -> Result<bool, AuthError> {
    let password = password.as_bytes().to_vec();
    tokio::task::spawn_blocking(move || {
        let parsed = PasswordHash::new(&password_hash).map_err(|error| {
            anyhow::anyhow!("invalid stored administrator password hash: {error}")
        })?;
        Ok(Argon2::default()
            .verify_password(&password, &parsed)
            .is_ok())
    })
    .await
    .map_err(|error| AuthError::Storage(anyhow::Error::new(error)))?
    .map_err(AuthError::Storage)
}

fn random_token() -> String {
    let mut bytes = [0_u8; 32];
    rand::fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn hash_refresh_token(refresh_token: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(refresh_token.as_bytes()))
}

fn system_time_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicI64, Ordering};

    use sqlx::sqlite::SqlitePoolOptions;

    use super::*;
    use crate::storage::SqliteStorage;

    async fn auth_at(now: i64) -> (AdminAuth, Arc<AtomicI64>) {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory SQLite");
        crate::migrations::migrate_sqlite(&pool)
            .await
            .expect("migrations");
        let storage: DynStorage = Arc::new(SqliteStorage::from_pool(pool));
        let now = Arc::new(AtomicI64::new(now));
        let clock_now = Arc::clone(&now);
        let clock = Arc::new(move || clock_now.load(Ordering::SeqCst));
        (AdminAuth::with_clock(storage, clock), now)
    }

    #[tokio::test]
    async fn access_and_session_expire_at_the_exact_contract_boundaries() {
        let start = 1_900_000_000;
        let (auth, now) = auth_at(start).await;
        auth.create_admin("admin", "correct horse battery staple")
            .await
            .expect("create administrator");
        let first = auth
            .login("admin", "correct horse battery staple")
            .await
            .expect("login");

        now.store(start + ACCESS_LIFETIME_SECONDS - 1, Ordering::SeqCst);
        auth.authenticate(&first.access_token)
            .await
            .expect("access valid immediately before expiry");
        now.store(start + ACCESS_LIFETIME_SECONDS, Ordering::SeqCst);
        assert!(matches!(
            auth.authenticate(&first.access_token).await,
            Err(AuthError::Unauthorized)
        ));

        now.store(start + SESSION_LIFETIME_SECONDS - 1, Ordering::SeqCst);
        let final_tokens = auth
            .refresh(&first.refresh_token)
            .await
            .expect("refresh immediately before session expiry");
        assert_eq!(
            final_tokens.access_expires_at,
            start + SESSION_LIFETIME_SECONDS
        );
        now.store(start + SESSION_LIFETIME_SECONDS, Ordering::SeqCst);
        assert!(matches!(
            auth.authenticate(&final_tokens.access_token).await,
            Err(AuthError::Unauthorized)
        ));
        assert!(matches!(
            auth.refresh(&final_tokens.refresh_token).await,
            Err(AuthError::Unauthorized)
        ));
    }

    #[tokio::test]
    async fn concurrent_refresh_has_one_winner_without_revoking_its_session() {
        let (auth, _now) = auth_at(1_900_000_000).await;
        auth.create_admin("admin", "correct horse battery staple")
            .await
            .expect("create administrator");
        let initial = auth
            .login("admin", "correct horse battery staple")
            .await
            .expect("login");

        let left_auth = auth.clone();
        let left_token = initial.refresh_token.clone();
        let right_auth = auth.clone();
        let right_token = initial.refresh_token;
        let (left, right) = tokio::join!(
            left_auth.refresh(&left_token),
            right_auth.refresh(&right_token)
        );
        let winner = match (left, right) {
            (Ok(tokens), Err(AuthError::Unauthorized))
            | (Err(AuthError::Unauthorized), Ok(tokens)) => tokens,
            _ => panic!("exactly one concurrent refresh must succeed"),
        };

        auth.authenticate(&winner.access_token)
            .await
            .expect("winning refresh keeps session valid");
        auth.refresh(&winner.refresh_token)
            .await
            .expect("winning refresh token remains usable");
    }

    #[tokio::test]
    async fn concurrent_creation_preserves_the_single_administrator() {
        let (auth, _now) = auth_at(1_900_000_000).await;
        let left = auth.clone();
        let right = auth.clone();
        let (left, right) = tokio::join!(
            left.create_admin("left", "left-password"),
            right.create_admin("right", "right-password")
        );
        assert!(matches!(
            (&left, &right),
            (Ok(()), Err(AuthError::Conflict)) | (Err(AuthError::Conflict), Ok(()))
        ));
        let left_login = auth.login("left", "left-password").await;
        let right_login = auth.login("right", "right-password").await;
        assert!(left_login.is_ok() ^ right_login.is_ok());
    }
}
