use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

#[cfg(test)]
use async_trait::async_trait;
use futures::FutureExt;
use serde_json::Value;

#[cfg(test)]
use stravia_runtime_contract::Principal;
use stravia_runtime_contract::protocol::ir::ContentBlock;
use stravia_runtime_contract::protocol::ir::ToolResultContentKind;
use stravia_runtime_contract::protocol::ir::ToolSpec;

use stravia_runtime_contract::hook::tool::*;

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
                content_kind: ToolResultContentKind::Json,
                is_error: true,
                metadata: serde_json::Map::new(),
            };
        };
        match std::panic::AssertUnwindSafe(async {
            let output = tool.execute_result(arguments, context).await?;
            let (content, content_kind) = blocks_to_value(output.content)?;
            Ok::<_, PlatformToolError>((content, content_kind, output.is_error, output.metadata))
        })
        .catch_unwind()
        .await
        {
            Ok(Ok((content, content_kind, is_error, metadata))) => PlatformToolResult {
                tool_id: id.clone(),
                call_id,
                content,
                content_kind,
                is_error,
                metadata,
            },
            Ok(Err(error)) => PlatformToolResult {
                tool_id: id.clone(),
                call_id,
                content: Value::String(error.to_string()),
                content_kind: ToolResultContentKind::Json,
                is_error: true,
                metadata: serde_json::Map::new(),
            },
            Err(_) => PlatformToolResult {
                tool_id: id.clone(),
                call_id,
                content: Value::String("platform tool panicked".into()),
                content_kind: ToolResultContentKind::Json,
                is_error: true,
                metadata: serde_json::Map::new(),
            },
        }
    }
}

pub(crate) fn blocks_to_value(
    mut blocks: Vec<ContentBlock>,
) -> Result<(Value, ToolResultContentKind), PlatformToolError> {
    if blocks.len() == 1 {
        match &mut blocks[0] {
            ContentBlock::Unknown { raw } => {
                return Ok((raw.take(), ToolResultContentKind::Json));
            }
            ContentBlock::Text { text, .. } => {
                return Ok((
                    Value::String(std::mem::take(text)),
                    ToolResultContentKind::Json,
                ));
            }
            _ => {}
        }
    }
    serde_json::to_value(blocks)
        .map(|content| (content, ToolResultContentKind::ContentBlocks))
        .map_err(|error| {
            PlatformToolError::new(format!("tool output serialization failed: {error}"))
        })
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
                    cancellation: stravia_runtime_contract::CancellationToken::new(),
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
