use std::time::Duration;

use async_trait::async_trait;

use super::RedactionError;
use stravia_runtime_contract::Principal;

const PENDING_RETENTION: Duration = Duration::from_secs(60 * 60);

const PUBLISHED_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

// Intentionally no Debug: mapping plaintext must never enter diagnostics.
#[derive(Clone, sqlx::FromRow)]
pub struct Mapping {
    pub reference: String,
    pub secret: String,
    pub expires_at: i64,
    /// 创建映射时检测出它的规则；创建之前的映射为空，无法归因。
    #[sqlx(try_from = "String")]
    pub rule_ids: RuleIds,
}

/// 映射表里以 JSON 数组文本保存的规则 ID。
#[derive(Clone, Default, PartialEq, Eq)]
pub struct RuleIds(Vec<String>);

impl RuleIds {
    pub fn as_slice(&self) -> &[String] {
        &self.0
    }
}

impl From<Vec<String>> for RuleIds {
    fn from(rule_ids: Vec<String>) -> Self {
        Self(rule_ids)
    }
}

impl TryFrom<String> for RuleIds {
    type Error = serde_json::Error;

    fn try_from(encoded: String) -> Result<Self, Self::Error> {
        serde_json::from_str(&encoded).map(Self)
    }
}

/// 待登记的秘密及检测出它的规则；规则只在新建映射时写入，已有映射保持原样。
pub struct DetectedSecret {
    pub secret: String,
    pub rule_ids: Vec<String>,
}

impl From<&str> for DetectedSecret {
    fn from(secret: &str) -> Self {
        Self {
            secret: secret.to_owned(),
            rule_ids: Vec::new(),
        }
    }
}

// 新建下标只在事务成功提交后返回；调用者不能用扫描前状态推断创建归属。
pub struct InternedMappings {
    pub mappings: Vec<Mapping>,
    pub created: Vec<usize>,
}

#[async_trait]
pub trait MappingStore: Send + Sync {
    async fn active(&self, principal: &Principal) -> Result<Vec<Mapping>, RedactionError>;
    async fn intern(
        &self,
        principal: &Principal,
        secrets: &[DetectedSecret],
    ) -> Result<InternedMappings, RedactionError>;
    async fn publish(
        &self,
        principal: &Principal,
        references: &[String],
        retention: Duration,
    ) -> Result<(), RedactionError>;
    async fn extend_retention(
        &self,
        principal: &Principal,
        references: &[String],
        retention: Duration,
    ) -> Result<(), RedactionError>;
    async fn cleanup_expired(&self) -> Result<u64, RedactionError>;
    /// 按占位符读取未过期映射的明文。占位符是映射的唯一标识，观察事件的 principal
    /// 只保存 API Key ID，与映射表的 `api-key:<id>` 存储键并不相同。
    async fn plaintext(&self, reference: &str) -> Result<Option<String>, RedactionError>;
}

#[derive(Clone)]
pub enum SqlMappingStore {
    Sqlite(sqlx::SqlitePool),
    Postgres(sqlx::PgPool),
}

fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn after(now: i64, retention: Duration) -> i64 {
    now.saturating_add(i64::try_from(retention.as_millis()).unwrap_or(i64::MAX))
}

impl SqlMappingStore {
    pub fn sqlite(pool: sqlx::SqlitePool) -> Self {
        Self::Sqlite(pool)
    }

    pub fn postgres(pool: sqlx::PgPool) -> Self {
        Self::Postgres(pool)
    }

    fn storage(&self, _error: sqlx::Error) -> RedactionError {
        RedactionError::Storage
    }

    async fn retain(
        &self,
        principal: &Principal,
        references: &[String],
        retention: Duration,
        publish: bool,
    ) -> Result<(), RedactionError> {
        if references.is_empty() {
            return Ok(());
        }
        let mut references: Vec<_> = references.iter().map(String::as_str).collect();
        references.sort_unstable();
        references.dedup();
        let principal = principal.continuation_key();
        let retention = if publish {
            retention.max(PUBLISHED_RETENTION)
        } else {
            retention
        };
        match self {
            Self::Sqlite(pool) => {
                let mut connection = pool
                    .acquire_write()
                    .await
                    .map_err(|error| self.storage(error))?;
                let mut transaction = connection
                    .begin_with("BEGIN IMMEDIATE")
                    .await
                    .map_err(|error| self.storage(error))?;
                let now = now();
                let expires_at = after(now, retention);
                // Match the 400-key IN batches used by durable history storage.
                for references in references.chunks(400) {
                    let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
                        "UPDATE reversible_redaction_mappings SET published_at = CASE WHEN ",
                    );
                    query
                        .push_bind(publish)
                        .push(" THEN COALESCE(published_at, ")
                        .push_bind(now)
                        .push(") ELSE published_at END, expires_at = MAX(expires_at, ")
                        .push_bind(expires_at)
                        .push("), updated_at = ")
                        .push_bind(now)
                        .push(" WHERE principal = ")
                        .push_bind(&principal)
                        .push(" AND expires_at > ")
                        .push_bind(now)
                        .push(" AND (")
                        .push_bind(publish)
                        .push(" OR published_at IS NOT NULL) AND reference IN (");
                    let mut list = query.separated(", ");
                    for reference in references {
                        list.push_bind(*reference);
                    }
                    list.push_unseparated(")");
                    query
                        .build()
                        .execute(&mut *transaction)
                        .await
                        .map_err(|error| self.storage(error))?;
                }
                transaction
                    .commit()
                    .await
                    .map_err(|error| self.storage(error))?;
                connection
                    .finish()
                    .await
                    .map_err(|error| self.storage(error))?;
            }
            Self::Postgres(pool) => {
                let mut transaction = pool.begin().await.map_err(|error| self.storage(error))?;
                // The same principal lock as intern makes lifecycle decisions after any writer wait.
                sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
                    .bind(&principal)
                    .execute(&mut *transaction)
                    .await
                    .map_err(|error| self.storage(error))?;
                let now = now();
                let expires_at = after(now, retention);
                for references in references.chunks(400) {
                    let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new(
                        "UPDATE reversible_redaction_mappings SET published_at = CASE WHEN ",
                    );
                    query
                        .push_bind(publish)
                        .push(" THEN COALESCE(published_at, ")
                        .push_bind(now)
                        .push(") ELSE published_at END, expires_at = GREATEST(expires_at, ")
                        .push_bind(expires_at)
                        .push("), updated_at = ")
                        .push_bind(now)
                        .push(" WHERE principal = ")
                        .push_bind(&principal)
                        .push(" AND expires_at > ")
                        .push_bind(now)
                        .push(" AND (")
                        .push_bind(publish)
                        .push(" OR published_at IS NOT NULL) AND reference IN (");
                    let mut list = query.separated(", ");
                    for reference in references {
                        list.push_bind(*reference);
                    }
                    list.push_unseparated(")");
                    query
                        .build()
                        .execute(&mut *transaction)
                        .await
                        .map_err(|error| self.storage(error))?;
                }
                transaction
                    .commit()
                    .await
                    .map_err(|error| self.storage(error))?;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl MappingStore for SqlMappingStore {
    async fn active(&self, principal: &Principal) -> Result<Vec<Mapping>, RedactionError> {
        let principal = principal.continuation_key();
        match self {
            Self::Sqlite(pool) => sqlx::query_as::<_, Mapping>(
                "SELECT reference, secret, expires_at, rule_ids FROM reversible_redaction_mappings \
                 WHERE principal = ? AND expires_at > ? ORDER BY reference",
            )
            .bind(principal)
            .bind(now())
            .fetch_all(pool)
            .await
            .map_err(|error| self.storage(error)),
            Self::Postgres(pool) => sqlx::query_as::<_, Mapping>(
                "SELECT reference, secret, expires_at, rule_ids FROM reversible_redaction_mappings \
                 WHERE principal = $1 AND expires_at > $2 ORDER BY reference",
            )
            .bind(principal)
            .bind(now())
            .fetch_all(pool)
            .await
            .map_err(|error| self.storage(error)),
        }
    }

    async fn intern(
        &self,
        principal: &Principal,
        secrets: &[DetectedSecret],
    ) -> Result<InternedMappings, RedactionError> {
        if secrets.is_empty() {
            return Ok(InternedMappings {
                mappings: Vec::new(),
                created: Vec::new(),
            });
        }
        let principal = principal.continuation_key();
        let mut mappings = Vec::with_capacity(secrets.len());
        let mut created = Vec::new();
        // Serialize lookup-and-insert across processes, not just this store instance.
        // Secrets can be arbitrarily long (e.g. private keys), so do not B-tree index them.
        match self {
            Self::Sqlite(pool) => {
                let mut connection = pool
                    .acquire_write()
                    .await
                    .map_err(|error| self.storage(error))?;
                let mut transaction = connection
                    .begin_with("BEGIN IMMEDIATE")
                    .await
                    .map_err(|error| self.storage(error))?;
                let now = now();
                for secret in secrets {
                    let existing = sqlx::query_as::<_, Mapping>(
                        "SELECT reference, secret, expires_at, rule_ids FROM reversible_redaction_mappings \
                         WHERE principal = ? AND secret = ? AND expires_at > ?",
                    )
                    .bind(&principal)
                    .bind(&secret.secret)
                    .bind(now)
                    .fetch_optional(&mut *transaction)
                    .await
                    .map_err(|error| self.storage(error))?;
                    let mapping = match existing {
                        Some(mapping) => mapping,
                        None => {
                            created.push(mappings.len());
                            let reference = super::marker::new_reference();
                            let rule_ids = serde_json::to_string(&secret.rule_ids)
                                .map_err(|_| RedactionError::Storage)?;
                            sqlx::query(
                                "INSERT INTO reversible_redaction_mappings \
                                 (reference, principal, secret, rule_ids, created_at, updated_at, expires_at) \
                                 VALUES (?, ?, ?, ?, ?, ?, ?)",
                            ).bind(&reference).bind(&principal).bind(&secret.secret).bind(&rule_ids).bind(now).bind(now)
                                .bind(after(now, PENDING_RETENTION))
                                .execute(&mut *transaction).await.map_err(|error| self.storage(error))?;
                            Mapping {
                                reference,
                                secret: secret.secret.clone(),
                                expires_at: after(now, PENDING_RETENTION),
                                rule_ids: secret.rule_ids.clone().into(),
                            }
                        }
                    };
                    mappings.push(mapping);
                }
                transaction
                    .commit()
                    .await
                    .map_err(|error| self.storage(error))?;
                connection
                    .finish()
                    .await
                    .map_err(|error| self.storage(error))?;
            }
            Self::Postgres(pool) => {
                let mut transaction = pool.begin().await.map_err(|error| self.storage(error))?;
                sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
                    .bind(&principal)
                    .execute(&mut *transaction)
                    .await
                    .map_err(|error| self.storage(error))?;
                let now = now();
                for secret in secrets {
                    let existing = sqlx::query_as::<_, Mapping>(
                        "SELECT reference, secret, expires_at, rule_ids FROM reversible_redaction_mappings \
                         WHERE principal = $1 AND secret = $2 AND expires_at > $3",
                    )
                    .bind(&principal)
                    .bind(&secret.secret)
                    .bind(now)
                    .fetch_optional(&mut *transaction)
                    .await
                    .map_err(|error| self.storage(error))?;
                    let mapping = match existing {
                        Some(mapping) => mapping,
                        None => {
                            created.push(mappings.len());
                            let reference = super::marker::new_reference();
                            let rule_ids = serde_json::to_string(&secret.rule_ids)
                                .map_err(|_| RedactionError::Storage)?;
                            sqlx::query(
                                "INSERT INTO reversible_redaction_mappings \
                                 (reference, principal, secret, rule_ids, created_at, updated_at, expires_at) \
                                 VALUES ($1, $2, $3, $4, $5, $5, $6)",
                            ).bind(&reference).bind(&principal).bind(&secret.secret).bind(&rule_ids).bind(now)
                                .bind(after(now, PENDING_RETENTION))
                                .execute(&mut *transaction).await.map_err(|error| self.storage(error))?;
                            Mapping {
                                reference,
                                secret: secret.secret.clone(),
                                expires_at: after(now, PENDING_RETENTION),
                                rule_ids: secret.rule_ids.clone().into(),
                            }
                        }
                    };
                    mappings.push(mapping);
                }
                transaction
                    .commit()
                    .await
                    .map_err(|error| self.storage(error))?;
            }
        }
        Ok(InternedMappings { mappings, created })
    }

    async fn publish(
        &self,
        principal: &Principal,
        references: &[String],
        retention: Duration,
    ) -> Result<(), RedactionError> {
        self.retain(principal, references, retention, true).await
    }

    async fn extend_retention(
        &self,
        principal: &Principal,
        references: &[String],
        retention: Duration,
    ) -> Result<(), RedactionError> {
        self.retain(principal, references, retention, false).await
    }

    async fn plaintext(&self, reference: &str) -> Result<Option<String>, RedactionError> {
        match self {
            Self::Sqlite(pool) => sqlx::query_scalar(
                "SELECT secret FROM reversible_redaction_mappings \
                 WHERE reference = ? AND expires_at > ?",
            )
            .bind(reference)
            .bind(now())
            .fetch_optional(pool)
            .await
            .map_err(|error| self.storage(error)),
            Self::Postgres(pool) => sqlx::query_scalar(
                "SELECT secret FROM reversible_redaction_mappings \
                 WHERE reference = $1 AND expires_at > $2",
            )
            .bind(reference)
            .bind(now())
            .fetch_optional(pool)
            .await
            .map_err(|error| self.storage(error)),
        }
    }

    async fn cleanup_expired(&self) -> Result<u64, RedactionError> {
        match self {
            Self::Sqlite(pool) => {
                let mut connection = pool
                    .acquire_write()
                    .await
                    .map_err(|error| self.storage(error))?;
                let result =
                    sqlx::query("DELETE FROM reversible_redaction_mappings WHERE expires_at <= ?")
                        .bind(now())
                        .execute(&mut *connection)
                        .await
                        .map_err(|error| self.storage(error))?;
                connection
                    .finish()
                    .await
                    .map_err(|error| self.storage(error))?;
                Ok(result.rows_affected())
            }
            Self::Postgres(pool) => {
                sqlx::query("DELETE FROM reversible_redaction_mappings WHERE expires_at <= $1")
                    .bind(now())
                    .execute(pool)
                    .await
                    .map(|result| result.rows_affected())
                    .map_err(|error| self.storage(error))
            }
        }
    }
}
