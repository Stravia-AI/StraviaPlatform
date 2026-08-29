use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use super::definition_store::{AgentDefinitionStore, MemoryAgentDefinitionStore};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentDefinitionId(String);

impl AgentDefinitionId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentSlug(String);

impl AgentSlug {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn tool_name(&self) -> String {
        format!("agent_{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VersionedToolId {
    pub id: String,
    pub version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentBudgets {
    pub total_wall_time: Duration,
    pub working_wall_time: Duration,
    pub model_turns: u32,
    pub tool_calls: Option<u32>,
    pub tool_parallelism: Option<u32>,
    pub concurrent_runs: Option<u32>,
    pub total_tokens: Option<u32>,
    pub finalization_tokens: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactPolicy {
    pub max_artifacts: u32,
    pub max_bytes: u64,
    pub allowed_mime_types: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentDefinitionExposure {
    #[default]
    Public,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentDefinitionSpec {
    pub id: AgentDefinitionId,
    pub slug: AgentSlug,
    pub revision: u32,
    pub description: String,
    pub instructions: String,
    pub output_schema: Option<Value>,
    pub tools: Vec<VersionedToolId>,
    pub budgets: AgentBudgets,
    pub artifact_policy: ArtifactPolicy,
    pub repair_attempts: u32,
    #[serde(default)]
    pub exposure: AgentDefinitionExposure,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDefinitionConfig {
    pub enabled: bool,
    pub model_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentDefinitionRecord {
    pub spec: AgentDefinitionSpec,
    pub spec_hash: String,
    pub config: AgentDefinitionConfig,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AgentDefinitionError {
    #[error("invalid Agent Definition: {0}")]
    Invalid(String),
    #[error("Agent Definition storage failed: {0}")]
    Storage(String),
    #[error("Agent Definition not found")]
    NotFound,
}

#[derive(Clone)]
pub struct AgentDefinitionRegistry {
    store: Arc<dyn AgentDefinitionStore>,
    current: Arc<RwLock<HashMap<AgentDefinitionId, AgentDefinitionRecord>>>,
}

impl Default for AgentDefinitionRegistry {
    fn default() -> Self {
        Self::with_store(Arc::new(MemoryAgentDefinitionStore::default()))
    }
}

impl AgentDefinitionRegistry {
    pub(crate) fn with_store(store: Arc<dyn AgentDefinitionStore>) -> Self {
        Self {
            store,
            current: Arc::new(RwLock::new(HashMap::new())),
        }
    }
    pub(crate) fn sqlite(pool: sqlx::SqlitePool) -> Self {
        Self::with_store(Arc::new(
            super::definition_store::SqlAgentDefinitionStore::sqlite(pool),
        ))
    }

    pub(crate) fn postgres(pool: sqlx::PgPool) -> Self {
        Self::with_store(Arc::new(
            super::definition_store::SqlAgentDefinitionStore::postgres(pool),
        ))
    }

    pub async fn synchronize(
        &self,
        definitions: Vec<AgentDefinitionSpec>,
    ) -> Result<(), AgentDefinitionError> {
        let mut slugs = HashSet::with_capacity(definitions.len());
        let mut ids = HashSet::with_capacity(definitions.len());
        let mut validated = Vec::with_capacity(definitions.len());
        for definition in definitions {
            validate_definition(&definition)?;
            if !slugs.insert(definition.slug.clone()) {
                return Err(AgentDefinitionError::Invalid(format!(
                    "duplicate slug: {}",
                    definition.slug.as_str()
                )));
            }
            if !ids.insert(definition.id.clone()) {
                return Err(AgentDefinitionError::Invalid(format!(
                    "duplicate id: {}",
                    definition.id.as_str()
                )));
            }
            let bytes = serde_json::to_vec(&definition)
                .map_err(|error| AgentDefinitionError::Invalid(error.to_string()))?;
            validated.push((definition, definition_hash(&bytes)));
        }

        let mut by_id = HashMap::with_capacity(validated.len());
        for (definition, spec_hash) in validated {
            let config = self
                .store
                .synchronize_revision(&definition, &spec_hash)
                .await?;
            by_id.insert(
                definition.id.clone(),
                AgentDefinitionRecord {
                    spec: definition,
                    spec_hash,
                    config,
                },
            );
        }
        *self.current.write().await = by_id;
        Ok(())
    }

    pub async fn list(&self) -> Vec<AgentDefinitionRecord> {
        let mut records = self
            .current
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        records.sort_by(|left, right| left.spec.slug.as_str().cmp(right.spec.slug.as_str()));
        records
    }

    pub async fn list_public(&self) -> Vec<AgentDefinitionRecord> {
        self.list()
            .await
            .into_iter()
            .filter(|record| record.spec.exposure == AgentDefinitionExposure::Public)
            .collect()
    }

    pub async fn get_current(
        &self,
        id: &AgentDefinitionId,
    ) -> Result<AgentDefinitionRecord, AgentDefinitionError> {
        self.current
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or(AgentDefinitionError::NotFound)
    }

    pub async fn get_by_slug(
        &self,
        slug: &str,
    ) -> Result<AgentDefinitionRecord, AgentDefinitionError> {
        self.current
            .read()
            .await
            .values()
            .find(|record| record.spec.slug.as_str() == slug)
            .cloned()
            .ok_or(AgentDefinitionError::NotFound)
    }

    pub async fn load_revision(
        &self,
        id: &AgentDefinitionId,
        revision: u32,
    ) -> Result<AgentDefinitionSpec, AgentDefinitionError> {
        self.store
            .load_revision(id, revision)
            .await?
            .map(|(spec, _)| spec)
            .ok_or(AgentDefinitionError::NotFound)
    }

    pub async fn patch_config(
        &self,
        id: &AgentDefinitionId,
        config: AgentDefinitionConfig,
    ) -> Result<AgentDefinitionRecord, AgentDefinitionError> {
        if config.enabled && config.model_id.is_none() {
            return Err(AgentDefinitionError::Invalid(
                "enabled Definition requires a Model".into(),
            ));
        }
        let mut current = self.current.write().await;
        let record = current.get_mut(id).ok_or(AgentDefinitionError::NotFound)?;
        let config = self.store.patch_config(id, config).await?;
        record.config = config;
        Ok(record.clone())
    }
}

fn definition_hash(bytes: &[u8]) -> String {
    use std::fmt::Write;

    let digest = Sha256::digest(bytes);
    let mut hash = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut hash, "{byte:02x}").expect("writing to a String cannot fail");
    }
    hash
}

fn validate_definition(definition: &AgentDefinitionSpec) -> Result<(), AgentDefinitionError> {
    if definition.id.as_str().trim().is_empty() {
        return Err(AgentDefinitionError::Invalid("id cannot be empty".into()));
    }
    let slug = definition.slug.as_str();
    if slug.is_empty()
        || !slug
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(AgentDefinitionError::Invalid(format!(
            "invalid slug: {slug}"
        )));
    }
    if definition.revision == 0 {
        return Err(AgentDefinitionError::Invalid(
            "revision must be greater than zero".into(),
        ));
    }
    if definition.description.trim().is_empty() || definition.instructions.trim().is_empty() {
        return Err(AgentDefinitionError::Invalid(
            "description and instructions cannot be empty".into(),
        ));
    }
    if definition
        .output_schema
        .as_ref()
        .is_some_and(|schema| !schema.is_object())
    {
        return Err(AgentDefinitionError::Invalid(
            "output schema must be an object".into(),
        ));
    }
    if let Some(schema) = definition.output_schema.as_ref() {
        validate_output_schema(schema, "$")?;
    }
    let budgets = &definition.budgets;
    let invalid_finalization_reserve = match (budgets.total_tokens, budgets.finalization_tokens) {
        (Some(total), Some(finalization)) => finalization == 0 || finalization >= total,
        (None, Some(_)) => true,
        _ => false,
    };
    if budgets.total_wall_time.is_zero()
        || budgets.working_wall_time.is_zero()
        || budgets.working_wall_time >= budgets.total_wall_time
        || budgets.model_turns < 2
        || budgets.tool_parallelism.is_some_and(|value| value == 0)
        || budgets.concurrent_runs.is_some_and(|value| value == 0)
        || invalid_finalization_reserve
    {
        return Err(AgentDefinitionError::Invalid(
            "Agent budgets and finalization reserve are inconsistent".into(),
        ));
    }
    let artifacts_disabled = definition.artifact_policy.max_artifacts == 0
        && definition.artifact_policy.max_bytes == 0
        && definition.artifact_policy.allowed_mime_types.is_empty();
    if !artifacts_disabled
        && (definition.artifact_policy.max_artifacts == 0
            || definition.artifact_policy.max_bytes == 0)
    {
        return Err(AgentDefinitionError::Invalid(
            "Artifact policy limits must both be zero or both be greater than zero".into(),
        ));
    }
    if definition
        .tools
        .iter()
        .any(|tool| tool.id.trim().is_empty() || tool.version == 0)
    {
        return Err(AgentDefinitionError::Invalid(
            "Tool IDs and versions must be valid".into(),
        ));
    }
    let mut tool_ids = HashSet::with_capacity(definition.tools.len());
    if let Some(duplicate) = definition
        .tools
        .iter()
        .find(|tool| !tool_ids.insert(tool.id.as_str()))
    {
        return Err(AgentDefinitionError::Invalid(format!(
            "duplicate Tool ID in Definition: {}",
            duplicate.id
        )));
    }
    Ok(())
}
fn validate_output_schema(schema: &Value, path: &str) -> Result<(), AgentDefinitionError> {
    const SUPPORTED: &[&str] = &[
        "$id",
        "$schema",
        "title",
        "description",
        "default",
        "examples",
        "type",
        "properties",
        "required",
        "additionalProperties",
        "items",
        "enum",
        "const",
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "minLength",
        "maxLength",
        "minItems",
        "maxItems",
    ];
    let object = schema.as_object().ok_or_else(|| {
        AgentDefinitionError::Invalid(format!("{path} output schema must be an object"))
    })?;
    if let Some(keyword) = object.keys().find(|key| !SUPPORTED.contains(&key.as_str())) {
        return Err(AgentDefinitionError::Invalid(format!(
            "{path} uses unsupported JSON Schema keyword {keyword}"
        )));
    }
    if let Some(kind) = object.get("type") {
        let valid = kind.as_str().is_some_and(|kind| {
            matches!(
                kind,
                "object" | "array" | "string" | "number" | "integer" | "boolean" | "null"
            )
        });
        if !valid {
            return Err(AgentDefinitionError::Invalid(format!(
                "{path}.type must be a supported JSON Schema type"
            )));
        }
    }
    if let Some(required) = object.get("required")
        && !required
            .as_array()
            .is_some_and(|items| items.iter().all(Value::is_string))
    {
        return Err(AgentDefinitionError::Invalid(format!(
            "{path}.required must be an array of strings"
        )));
    }
    if let Some(values) = object.get("enum")
        && !values.as_array().is_some_and(|values| !values.is_empty())
    {
        return Err(AgentDefinitionError::Invalid(format!(
            "{path}.enum must be a non-empty array"
        )));
    }
    if let Some(properties) = object.get("properties") {
        let properties = properties.as_object().ok_or_else(|| {
            AgentDefinitionError::Invalid(format!("{path}.properties must be an object"))
        })?;
        for (name, child) in properties {
            validate_output_schema(child, &format!("{path}.properties.{name}"))?;
        }
    }
    if let Some(items) = object.get("items") {
        validate_output_schema(items, &format!("{path}.items"))?;
    }
    if let Some(additional) = object.get("additionalProperties")
        && !additional.is_boolean()
    {
        validate_output_schema(additional, &format!("{path}.additionalProperties"))?;
    }
    for keyword in ["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"] {
        if object.get(keyword).is_some_and(|value| !value.is_number()) {
            return Err(AgentDefinitionError::Invalid(format!(
                "{path}.{keyword} must be a number"
            )));
        }
    }
    for keyword in ["minLength", "maxLength", "minItems", "maxItems"] {
        if object
            .get(keyword)
            .is_some_and(|value| value.as_u64().is_none())
        {
            return Err(AgentDefinitionError::Invalid(format!(
                "{path}.{keyword} must be a non-negative integer"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::definition_store::SqlAgentDefinitionStore;

    fn definition(revision: u32) -> AgentDefinitionSpec {
        AgentDefinitionSpec {
            id: AgentDefinitionId::new("research"),
            slug: AgentSlug::new("research"),
            revision,
            description: "Research a question".into(),
            instructions: "Research carefully.".into(),
            output_schema: Some(serde_json::json!({
                "type": "object",
                "properties": {"answer": {"type": "string"}},
                "required": ["answer"],
                "additionalProperties": false
            })),
            tools: vec![VersionedToolId {
                id: "web.search".into(),
                version: 1,
            }],
            budgets: AgentBudgets {
                total_wall_time: Duration::from_secs(60),
                working_wall_time: Duration::from_secs(50),
                model_turns: 8,
                tool_calls: Some(12),
                tool_parallelism: Some(2),
                concurrent_runs: Some(2),
                total_tokens: Some(16_000),
                finalization_tokens: Some(2_000),
            },
            artifact_policy: ArtifactPolicy {
                max_artifacts: 4,
                max_bytes: 16 * 1024 * 1024,
                allowed_mime_types: vec!["image/png".into()],
            },
            repair_attempts: 1,
            exposure: AgentDefinitionExposure::Public,
        }
    }

    #[tokio::test]
    async fn synchronized_definition_is_disabled_and_unbound_by_default() {
        let registry = AgentDefinitionRegistry::default();
        registry
            .synchronize(vec![definition(1)])
            .await
            .expect("synchronize Definition");

        let records = registry.list().await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].spec.slug.tool_name(), "agent_research");
        assert!(!records[0].config.enabled);
        assert!(records[0].config.model_id.is_none());
        assert_eq!(records[0].spec_hash.len(), 64);
    }

    #[tokio::test]
    async fn definition_can_disable_artifacts_with_zero_limits() {
        let registry = AgentDefinitionRegistry::default();
        let mut spec = definition(1);
        spec.artifact_policy = ArtifactPolicy {
            max_artifacts: 0,
            max_bytes: 0,
            allowed_mime_types: Vec::new(),
        };

        registry
            .synchronize(vec![spec])
            .await
            .expect("artifact-free Definition");
    }

    #[tokio::test]
    async fn sqlite_registry_preserves_config_and_rejects_rewritten_revision() {
        let data_dir = tempfile::tempdir().expect("temporary data directory");
        let pool = crate::db::init_pool(data_dir.path())
            .await
            .expect("SQLite pool");
        crate::migrations::migrate_sqlite(&pool)
            .await
            .expect("SQLite migrations");
        sqlx::query(
            "INSERT INTO providers (id, name, protocol, base_url, api_key, auth_mode) \
             VALUES ('provider-1', 'Provider', 'openai-compatible', 'http://localhost', 'key', 'apikey')",
        )
        .execute(&pool)
        .await
        .expect("insert Provider");
        sqlx::query(
            "INSERT INTO models (id, name, target_provider, target_model) \
             VALUES ('model-1', 'Model', 'provider-1', 'upstream')",
        )
        .execute(&pool)
        .await
        .expect("insert Model");
        let registry = AgentDefinitionRegistry::with_store(Arc::new(
            SqlAgentDefinitionStore::sqlite(pool.clone()),
        ));
        registry
            .synchronize(vec![definition(1)])
            .await
            .expect("synchronize Definition");
        registry
            .patch_config(
                &AgentDefinitionId::new("research"),
                AgentDefinitionConfig {
                    enabled: true,
                    model_id: Some("model-1".into()),
                },
            )
            .await
            .expect("enable Definition");

        let reconstructed =
            AgentDefinitionRegistry::with_store(Arc::new(SqlAgentDefinitionStore::sqlite(pool)));
        reconstructed
            .synchronize(vec![definition(1)])
            .await
            .expect("reconstruct registry");
        let record = reconstructed
            .get_current(&AgentDefinitionId::new("research"))
            .await
            .expect("current Definition");
        assert!(record.config.enabled);
        assert_eq!(record.config.model_id.as_deref(), Some("model-1"));

        let mut rewritten = definition(1);
        rewritten.instructions = "Changed without a revision bump.".into();
        let error = reconstructed
            .synchronize(vec![rewritten])
            .await
            .expect_err("rewritten revision must fail");
        assert!(matches!(error, AgentDefinitionError::Invalid(_)));
    }
}
