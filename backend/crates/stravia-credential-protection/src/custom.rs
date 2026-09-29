//! 用户自定义凭据规则：定义、校验与 SQLite/PostgreSQL 持久化。
//!
//! 规则文本本身就是用户要保护的秘密，因此这些类型刻意不实现 `Debug`。
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use stravia_runtime_contract::redaction::RedactionError;

use super::detection::CompiledCustomRules;

/// 自定义规则 ID 前缀；内置规则 ID 不使用该前缀，命中记录与匹配测试据此区分来源。
pub const ID_PREFIX: &str = "custom.";

const MAX_NAME_CHARS: usize = 80;
const MAX_DESCRIPTION_CHARS: usize = 300;
const MAX_TEXT_CHARS: usize = 4096;
const MAX_REGEX_CHARS: usize = 2048;
const MAX_KEYWORDS: usize = 20;
const MAX_KEYWORD_CHARS: usize = 100;
/// 字节熵的理论上限（每字节 8 bit）。
const MAX_MIN_ENTROPY: f64 = 8.0;

/// 自定义规则的两种匹配方式。
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum CustomRuleSpec {
    /// 简易模式：逐字符串精确匹配（区分大小写），命中即整段替换。
    Simple { text: String },
    /// 规则模式：与内置规则一致的正则、提取分组、关键词与最小熵条件。
    Pattern {
        regex: String,
        #[serde(default)]
        secret_group: usize,
        #[serde(default)]
        keywords: Vec<String>,
        #[serde(default)]
        min_entropy: Option<f64>,
    },
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomRuleInput {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub spec: CustomRuleSpec,
}

fn default_enabled() -> bool {
    true
}

#[derive(Clone, Serialize)]
pub struct CustomRule {
    pub id: String,
    pub name: String,
    pub description: String,
    pub enabled: bool,
    pub spec: CustomRuleSpec,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy, thiserror::Error)]
pub enum CustomRuleError {
    #[error("Custom credential rule field `{field}` is invalid ({reason})")]
    Invalid {
        field: &'static str,
        reason: &'static str,
    },
    #[error("Custom credential rule not found")]
    NotFound,
    #[error("Custom credential rule storage failed")]
    Storage,
}

pub(super) fn invalid(field: &'static str, reason: &'static str) -> CustomRuleError {
    CustomRuleError::Invalid { field, reason }
}

impl CustomRuleInput {
    /// 规整文本字段并以真实检测器编译一遍；保存后的规则一定能被检测路径加载。
    fn validated(mut self) -> Result<Self, CustomRuleError> {
        self.name = self.name.trim().to_owned();
        self.description = self.description.trim().to_owned();
        if self.name.is_empty() {
            return Err(invalid("name", "required"));
        }
        if self.name.chars().count() > MAX_NAME_CHARS {
            return Err(invalid("name", "too_long"));
        }
        if self.description.chars().count() > MAX_DESCRIPTION_CHARS {
            return Err(invalid("description", "too_long"));
        }
        match &mut self.spec {
            CustomRuleSpec::Simple { text } => {
                // 纯空白不是可辨识的凭据，整段替换会破坏所有请求的排版。
                if text.trim().is_empty() {
                    return Err(invalid("text", "required"));
                }
                if text.chars().count() > MAX_TEXT_CHARS {
                    return Err(invalid("text", "too_long"));
                }
            }
            CustomRuleSpec::Pattern {
                regex,
                keywords,
                min_entropy,
                ..
            } => {
                if regex.is_empty() {
                    return Err(invalid("regex", "required"));
                }
                if regex.chars().count() > MAX_REGEX_CHARS {
                    return Err(invalid("regex", "too_long"));
                }
                let mut normalized: Vec<String> = Vec::with_capacity(keywords.len());
                for keyword in keywords.iter() {
                    let keyword = keyword.trim();
                    if keyword.is_empty() || normalized.iter().any(|seen| seen == keyword) {
                        continue;
                    }
                    if keyword.chars().count() > MAX_KEYWORD_CHARS {
                        return Err(invalid("keywords", "too_long"));
                    }
                    normalized.push(keyword.to_owned());
                }
                if normalized.len() > MAX_KEYWORDS {
                    return Err(invalid("keywords", "too_many"));
                }
                *keywords = normalized;
                if let Some(entropy) = *min_entropy
                    && !(entropy.is_finite() && (0.0..=MAX_MIN_ENTROPY).contains(&entropy))
                {
                    return Err(invalid("min_entropy", "out_of_range"));
                }
            }
        }
        CompiledCustomRules::compile([("", &self.spec)])?;
        Ok(self)
    }
}

// sqlx 要求静态 SQL；列清单用宏拼接为字面量，避免动态字符串。
macro_rules! columns {
    () => {
        "id, name, description, enabled, spec, created_at, updated_at"
    };
}

#[derive(sqlx::FromRow)]
struct Row {
    id: String,
    name: String,
    description: String,
    enabled: bool,
    spec: String,
    created_at: i64,
    updated_at: i64,
}

impl Row {
    fn into_rule(self) -> Result<CustomRule, CustomRuleError> {
        Ok(CustomRule {
            spec: serde_json::from_str(&self.spec).map_err(|_| CustomRuleError::Storage)?,
            id: self.id,
            name: self.name,
            description: self.description,
            enabled: self.enabled,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

enum Backend {
    Sqlite(sqlx::SqlitePool),
    Postgres(sqlx::PgPool),
}

struct Cached {
    /// 规则 ID 与原始 spec；任一变化都会使已编译规则失效，避免依赖时间戳精度。
    fingerprint: Vec<(String, String)>,
    compiled: Arc<CompiledCustomRules>,
}

pub struct SqlCustomRuleStore {
    backend: Backend,
    cache: Mutex<Option<Cached>>,
}

fn storage(_: sqlx::Error) -> CustomRuleError {
    CustomRuleError::Storage
}

fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl SqlCustomRuleStore {
    pub fn sqlite(pool: sqlx::SqlitePool) -> Self {
        Self::new(Backend::Sqlite(pool))
    }

    pub fn postgres(pool: sqlx::PgPool) -> Self {
        Self::new(Backend::Postgres(pool))
    }

    fn new(backend: Backend) -> Self {
        Self {
            backend,
            cache: Mutex::new(None),
        }
    }

    async fn rows(&self) -> Result<Vec<Row>, CustomRuleError> {
        const SQL: &str = concat!(
            "SELECT ",
            columns!(),
            " FROM credential_custom_rules ORDER BY created_at, id"
        );
        match &self.backend {
            Backend::Sqlite(pool) => sqlx::query_as(SQL).fetch_all(pool).await,
            Backend::Postgres(pool) => sqlx::query_as(SQL).fetch_all(pool).await,
        }
        .map_err(storage)
    }

    pub async fn list(&self) -> Result<Vec<CustomRule>, CustomRuleError> {
        self.rows().await?.into_iter().map(Row::into_rule).collect()
    }

    pub async fn create(&self, input: CustomRuleInput) -> Result<CustomRule, CustomRuleError> {
        let input = input.validated()?;
        let id = format!("{ID_PREFIX}{}", uuid::Uuid::new_v4().simple());
        let spec = serde_json::to_string(&input.spec).map_err(|_| CustomRuleError::Storage)?;
        let now = now();
        match &self.backend {
            Backend::Sqlite(pool) => sqlx::query(concat!(
                "INSERT INTO credential_custom_rules (",
                columns!(),
                ") VALUES (?, ?, ?, ?, ?, ?, ?)"
            ))
            .bind(&id)
            .bind(&input.name)
            .bind(&input.description)
            .bind(input.enabled)
            .bind(&spec)
            .bind(now)
            .bind(now)
            .execute(pool)
            .await
            .map(|_| ()),
            Backend::Postgres(pool) => sqlx::query(concat!(
                "INSERT INTO credential_custom_rules (",
                columns!(),
                ") VALUES ($1, $2, $3, $4, $5, $6, $7)"
            ))
            .bind(&id)
            .bind(&input.name)
            .bind(&input.description)
            .bind(input.enabled)
            .bind(&spec)
            .bind(now)
            .bind(now)
            .execute(pool)
            .await
            .map(|_| ()),
        }
        .map_err(storage)?;
        Ok(CustomRule {
            id,
            name: input.name,
            description: input.description,
            enabled: input.enabled,
            spec: input.spec,
            created_at: now,
            updated_at: now,
        })
    }

    pub async fn update(
        &self,
        id: &str,
        input: CustomRuleInput,
    ) -> Result<CustomRule, CustomRuleError> {
        let input = input.validated()?;
        let spec = serde_json::to_string(&input.spec).map_err(|_| CustomRuleError::Storage)?;
        let row: Option<Row> =
            match &self.backend {
                Backend::Sqlite(pool) => sqlx::query_as(concat!(
                    "UPDATE credential_custom_rules SET name = ?, description = ?, enabled = ?, \
                     spec = ?, updated_at = ? WHERE id = ? RETURNING ",
                    columns!()
                ))
                .bind(&input.name)
                .bind(&input.description)
                .bind(input.enabled)
                .bind(&spec)
                .bind(now())
                .bind(id)
                .fetch_optional(pool)
                .await,
                Backend::Postgres(pool) => sqlx::query_as(concat!(
                    "UPDATE credential_custom_rules SET name = $1, description = $2, enabled = $3, \
                     spec = $4, updated_at = $5 WHERE id = $6 RETURNING ",
                    columns!()
                ))
                .bind(&input.name)
                .bind(&input.description)
                .bind(input.enabled)
                .bind(&spec)
                .bind(now())
                .bind(id)
                .fetch_optional(pool)
                .await,
            }
            .map_err(storage)?;
        row.ok_or(CustomRuleError::NotFound)?.into_rule()
    }

    pub async fn delete(&self, id: &str) -> Result<(), CustomRuleError> {
        let affected = match &self.backend {
            Backend::Sqlite(pool) => {
                sqlx::query("DELETE FROM credential_custom_rules WHERE id = ?")
                    .bind(id)
                    .execute(pool)
                    .await
                    .map(|result| result.rows_affected())
            }
            Backend::Postgres(pool) => {
                sqlx::query("DELETE FROM credential_custom_rules WHERE id = $1")
                    .bind(id)
                    .execute(pool)
                    .await
                    .map(|result| result.rows_affected())
            }
        }
        .map_err(storage)?;
        if affected == 0 {
            return Err(CustomRuleError::NotFound);
        }
        Ok(())
    }

    /// 当前已启用规则的编译结果。存储不可读或规则无法加载时失败，由调用方按保护失败处理。
    pub(super) async fn compiled(&self) -> Result<Arc<CompiledCustomRules>, RedactionError> {
        let mut rows = self.rows().await.map_err(|_| RedactionError::Storage)?;
        rows.retain(|row| row.enabled);
        let fingerprint: Vec<_> = rows
            .iter()
            .map(|row| (row.id.clone(), row.spec.clone()))
            .collect();
        if let Some(cached) = &*self.cache.lock()
            && cached.fingerprint == fingerprint
        {
            return Ok(Arc::clone(&cached.compiled));
        }
        let rules = rows
            .into_iter()
            .map(Row::into_rule)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| RedactionError::Storage)?;
        let compiled = Arc::new(
            CompiledCustomRules::compile(rules.iter().map(|rule| (rule.id.as_str(), &rule.spec)))
                .map_err(|_| RedactionError::Detection)?,
        );
        *self.cache.lock() = Some(Cached {
            fingerprint,
            compiled: Arc::clone(&compiled),
        });
        Ok(compiled)
    }
}
