use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::FutureExt;
use serde_json::Value;

use super::Principal;
use crate::protocol::ir::{ContentBlock, ToolSpec};
use crate::proxy::context::CancellationToken;

pub const DEFAULT_PLATFORM_TOOL_EXECUTION_LIMIT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolId(String);

impl ToolId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ToolId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone)]
pub struct ToolExecutionContext {
    pub request_id: String,
    pub run_id: String,
    pub principal: Principal,
    pub cancellation: CancellationToken,
    pub progress: Option<Arc<dyn ToolProgressSink>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolProgress {
    pub call_id: String,
    pub phase: String,
    pub ordinal: u32,
    pub payload: Option<Value>,
}

pub trait ToolProgressSink: Send + Sync {
    fn emit(&self, progress: ToolProgress);
}

#[derive(Debug, Clone)]
pub struct PlatformToolOutput {
    pub content: Vec<ContentBlock>,
    pub is_error: bool,
    pub metadata: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlatformToolResult {
    pub tool_id: ToolId,
    pub call_id: String,
    pub content: Value,
    pub is_error: bool,
    pub metadata: serde_json::Map<String, Value>,
}
impl PlatformToolResult {
    pub fn content_block(&self) -> ContentBlock {
        ContentBlock::ToolResult {
            tool_use_id: self.call_id.clone(),
            content: self.content.clone(),
            is_error: Some(self.is_error),
            cache_control: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct PlatformToolError {
    pub message: String,
}

impl PlatformToolError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[async_trait]
pub trait PlatformTool: Send + Sync + 'static {
    fn id(&self) -> ToolId;
    fn external_name(&self) -> &str;
    fn description(&self) -> Option<&str> {
        None
    }
    fn activity_label(&self) -> &str {
        "Running a platform tool"
    }
    fn execution_limit(&self) -> Option<Duration> {
        None
    }
    fn parallel_safe(&self) -> bool {
        false
    }
    fn parameters(&self) -> Value;

    async fn execute(
        &self,
        arguments: Value,
        context: ToolExecutionContext,
    ) -> Result<Value, PlatformToolError>;

    async fn execute_blocks(
        &self,
        arguments: Value,
        context: ToolExecutionContext,
    ) -> Result<Vec<ContentBlock>, PlatformToolError> {
        let content = self.execute(arguments, context).await?;
        Ok(vec![ContentBlock::Unknown { raw: content }])
    }
    async fn execute_result(
        &self,
        arguments: Value,
        context: ToolExecutionContext,
    ) -> Result<PlatformToolOutput, PlatformToolError> {
        Ok(PlatformToolOutput {
            content: self.execute_blocks(arguments, context).await?,
            is_error: false,
            metadata: serde_json::Map::new(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct ExposedPlatformTool {
    pub id: ToolId,
    pub provider_name: String,
    pub spec: ToolSpec,
}

#[derive(Clone, Default)]
pub struct PlatformToolRegistry {
    tools: Arc<HashMap<ToolId, Arc<dyn PlatformTool>>>,
}

impl PlatformToolRegistry {
    pub fn new(tools: Vec<Arc<dyn PlatformTool>>) -> Result<Self, PlatformToolError> {
        let mut registered = HashMap::new();
        for tool in tools {
            let id = tool.id();
            if id.as_str().trim().is_empty() {
                return Err(PlatformToolError::new("tool id cannot be empty"));
            }
            if registered.insert(id.clone(), tool).is_some() {
                return Err(PlatformToolError::new(format!("duplicate tool id: {id}")));
            }
            let activity = registered
                .get(&id)
                .expect("registered Platform Tool")
                .activity_label();
            if activity.trim() != activity
                || activity.is_empty()
                || activity.chars().count() > 120
                || activity
                    .chars()
                    .any(|character| character.is_control() || matches!(character, '<' | '>' | '`'))
            {
                return Err(PlatformToolError::new(format!(
                    "platform tool activity label is not safe Markdown: {id}"
                )));
            }
        }
        Ok(Self {
            tools: Arc::new(registered),
        })
    }
    pub fn parallel_safe(&self, id: &ToolId) -> bool {
        self.tools.get(id).is_some_and(|tool| tool.parallel_safe())
    }

    pub fn activity_label(&self, id: &ToolId) -> Option<&str> {
        self.tools.get(id).map(|tool| tool.activity_label())
    }

    pub fn execution_limit(&self, id: &ToolId) -> Duration {
        self.tools
            .get(id)
            .and_then(|tool| tool.execution_limit())
            .filter(|limit| !limit.is_zero())
            .unwrap_or(DEFAULT_PLATFORM_TOOL_EXECUTION_LIMIT)
    }

    pub fn expose(
        &self,
        id: &ToolId,
        existing_names: &HashSet<String>,
    ) -> Result<ExposedPlatformTool, PlatformToolError> {
        let tool = self
            .tools
            .get(id)
            .ok_or_else(|| PlatformToolError::new(format!("platform tool not found: {id}")))?;
        let base = provider_safe_name(tool.external_name());
        let provider_name = if existing_names.contains(&base) {
            (2_u32..)
                .map(|suffix| format!("{base}_{suffix}"))
                .find(|candidate| !existing_names.contains(candidate))
                .expect("the numeric provider-name suffix space is unbounded")
        } else {
            base
        };
        Ok(ExposedPlatformTool {
            id: id.clone(),
            provider_name: provider_name.clone(),
            spec: ToolSpec {
                name: provider_name,
                description: tool.description().map(str::to_string),
                parameters: tool.parameters(),
                strict: Some(true),
                cache_control: None,
                meta: None,
            },
        })
    }

    pub async fn execute(
        &self,
        id: &ToolId,
        call_id: String,
        arguments: Value,
        context: ToolExecutionContext,
    ) -> PlatformToolResult {
        let Some(tool) = self.tools.get(id) else {
            return PlatformToolResult {
                tool_id: id.clone(),
                call_id,
                content: Value::String(format!("platform tool not found: {id}")),
                is_error: true,
                metadata: serde_json::Map::new(),
            };
        };
        match std::panic::AssertUnwindSafe(tool.execute_result(arguments, context))
            .catch_unwind()
            .await
        {
            Ok(Ok(output)) => PlatformToolResult {
                tool_id: id.clone(),
                call_id,
                content: blocks_to_value(output.content),
                is_error: output.is_error,
                metadata: output.metadata,
            },
            Ok(Err(error)) => PlatformToolResult {
                tool_id: id.clone(),
                call_id,
                content: Value::String(error.to_string()),
                is_error: true,
                metadata: serde_json::Map::new(),
            },
            Err(_) => PlatformToolResult {
                tool_id: id.clone(),
                call_id,
                content: Value::String("platform tool panicked".into()),
                is_error: true,
                metadata: serde_json::Map::new(),
            },
        }
    }
}

fn blocks_to_value(blocks: Vec<ContentBlock>) -> Value {
    if let [ContentBlock::Unknown { raw }] = blocks.as_slice() {
        raw.clone()
    } else if let [ContentBlock::Text { text, .. }] = blocks.as_slice() {
        Value::String(text.clone())
    } else {
        serde_json::to_value(blocks).unwrap_or(Value::Null)
    }
}

fn provider_safe_name(name: &str) -> String {
    let mut sanitized = String::with_capacity(name.len().min(48));
    for character in name.chars().take(48) {
        if character.is_ascii_alphanumeric() || character == '_' {
            sanitized.push(character.to_ascii_lowercase());
        } else if !sanitized.ends_with('_') {
            sanitized.push('_');
        }
    }
    let sanitized = sanitized.trim_matches('_');
    if sanitized.is_empty() {
        "stravia__tool".to_string()
    } else {
        format!("stravia__{sanitized}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoTool;

    #[async_trait]
    impl PlatformTool for EchoTool {
        fn id(&self) -> ToolId {
            ToolId::new("image-understanding")
        }

        fn external_name(&self) -> &str {
            "understand_image"
        }

        fn description(&self) -> Option<&str> {
            Some("Understand an image")
        }

        fn parameters(&self) -> Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(
            &self,
            arguments: Value,
            _context: ToolExecutionContext,
        ) -> Result<Value, PlatformToolError> {
            Ok(arguments)
        }

        fn parallel_safe(&self) -> bool {
            true
        }
    }

    struct PanicTool;

    #[async_trait]
    impl PlatformTool for PanicTool {
        fn id(&self) -> ToolId {
            ToolId::new("panic")
        }

        fn external_name(&self) -> &str {
            "panic"
        }

        fn description(&self) -> Option<&str> {
            None
        }

        fn parameters(&self) -> Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(
            &self,
            _arguments: Value,
            _context: ToolExecutionContext,
        ) -> Result<Value, PlatformToolError> {
            panic!("boom")
        }
    }

    #[test]
    fn exposed_tool_uses_reserved_collision_free_provider_name() {
        let registry = PlatformToolRegistry::new(vec![Arc::new(EchoTool)]).unwrap();
        assert!(registry.parallel_safe(&ToolId::new("image-understanding")));
        assert!(
            !PlatformToolRegistry::new(vec![Arc::new(PanicTool)])
                .unwrap()
                .parallel_safe(&ToolId::new("panic"))
        );
        let existing = HashSet::from([
            "understand_image".to_string(),
            "stravia__understand_image".to_string(),
        ]);

        let exposed = registry
            .expose(&ToolId::new("image-understanding"), &existing)
            .unwrap();

        assert!(
            exposed
                .provider_name
                .starts_with("stravia__understand_image_")
        );
        assert!(!existing.contains(&exposed.provider_name));
        assert_eq!(exposed.spec.name, exposed.provider_name);
    }

    #[tokio::test]
    async fn executor_panic_becomes_a_tool_error_result() {
        let registry = PlatformToolRegistry::new(vec![Arc::new(PanicTool)]).unwrap();

        let result = registry
            .execute(
                &ToolId::new("panic"),
                "call-1".into(),
                Value::Null,
                ToolExecutionContext {
                    request_id: "request".into(),
                    run_id: "run".into(),
                    principal: Principal::new("test-key"),
                    cancellation: crate::proxy::context::CancellationToken::new(),
                    progress: None,
                },
            )
            .await;

        assert!(result.is_error);
        assert_eq!(
            result.content,
            Value::String("platform tool panicked".into())
        );
    }
}
