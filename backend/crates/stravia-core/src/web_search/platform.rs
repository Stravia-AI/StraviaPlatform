use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::Value;

use crate::hook::{
    ActionBatch, EventKind, Hook, HookAction, HookDescriptor, HookEvent, HookId, HookRejection,
    HookSession, PlatformTool, PlatformToolError, PlatformToolOutput, Principal, RequestKind,
    ResponsePatch, SessionContext, ToolExecutionContext, ToolId, ToolProgress, ToolProgressSink,
};
use crate::protocol::ir::{ContentBlock, ProtocolExt, ToolChoice};

use super::{SearchTurnId, WebSearchEvent, WebSearchInput, WebSearchRunPolicy, WebSearchRunner};

pub(crate) const PUBLIC_WEB_SEARCH_TOOL_ID: &str = "web-search";
pub(crate) const PUBLIC_WEB_SEARCH_TOOL_NAME: &str = "web_search";
const MAX_PUBLIC_DEADLINE: Duration = Duration::from_secs(15 * 60);

pub(crate) type BuiltinExtensions = (Vec<Arc<dyn Hook>>, Vec<Arc<dyn PlatformTool>>);

pub(crate) fn builtin_extensions(gateway: &crate::Gateway) -> BuiltinExtensions {
    (
        vec![Arc::new(WebSearchHook {
            gateway: gateway.clone(),
        })],
        vec![Arc::new(WebSearchPlatformTool {
            gateway: gateway.clone(),
        })],
    )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicSearchInput {
    query: String,
    #[serde(default)]
    previous_turn_id: Option<String>,
    #[serde(default)]
    allowed_domains: Option<Vec<String>>,
    #[serde(default)]
    blocked_domains: Option<Vec<String>>,
}

pub(crate) fn input_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "minLength": 1,
                "description": "The question or topic to search. UTF-8 encoding must not exceed 64 KiB."
            },
            "previous_turn_id": {
                "type": ["string", "null"],
                "minLength": 1,
                "maxLength": 128,
                "description": "A prior Search Turn to continue or branch from."
            },
            "allowed_domains": {
                "type": ["array", "null"],
                "maxItems": 20,
                "items": { "type": "string" }
            },
            "blocked_domains": {
                "type": ["array", "null"],
                "maxItems": 20,
                "items": { "type": "string" }
            }
        },
        "required": ["query", "previous_turn_id", "allowed_domains", "blocked_domains"],
        "additionalProperties": false
    })
}

pub(crate) async fn is_available(gateway: &crate::Gateway, principal: &Principal) -> bool {
    authorized_search_access(gateway, principal).await.is_some()
}

async fn authorized_search_access(
    gateway: &crate::Gateway,
    principal: &Principal,
) -> Option<crate::proxy::security::WebSearchAccessGrant> {
    let access = crate::proxy::security::Security::new(gateway.storage.auth())
        .authorize_principal_web_search(principal)
        .await
        .ok()?;
    use super::WebSearchConfigStore;
    let config_enabled = super::SettingsWebSearchConfigStore::new(gateway.storage.clone())
        .load()
        .await
        .is_ok_and(|config| config.enabled);
    if gateway.web_search_runner_state.read().await.is_none() || !config_enabled {
        return None;
    }
    Some(access)
}

pub(crate) async fn execute(
    gateway: &crate::Gateway,
    arguments: Value,
    principal: Principal,
    cancellation: crate::proxy::context::CancellationToken,
    progress: Option<Arc<dyn ToolProgressSink>>,
) -> Result<Value, Value> {
    if !is_available(gateway, &principal).await {
        return Err(unavailable_error());
    }
    let request: PublicSearchInput = serde_json::from_value(arguments).map_err(|error| {
        serde_json::json!({
            "error": {
                "code": "invalid_input",
                "message": format!("invalid web_search arguments: {error}")
            }
        })
    })?;
    let runner: WebSearchRunner = gateway
        .web_search_runner()
        .await
        .map_err(|_| unavailable_error())?;
    let policy = match (request.allowed_domains, request.blocked_domains) {
        (None, None) => None,
        (allowed_domains, blocked_domains) => Some(WebSearchRunPolicy {
            allowed_domains: allowed_domains.unwrap_or_default(),
            blocked_domains: blocked_domains.unwrap_or_default(),
        }),
    };
    let mut stream = runner.run(WebSearchInput {
        principal,
        query: request.query,
        previous_turn_id: request.previous_turn_id.map(SearchTurnId::new),
        policy,
        cancellation,
        deadline: Instant::now() + MAX_PUBLIC_DEADLINE,
    });
    while let Some(event) = stream.next().await {
        match event {
            WebSearchEvent::Completed(result) | WebSearchEvent::Partial(result) => {
                return serde_json::to_value(result).map_err(|_| {
                    serde_json::json!({
                        "error": {
                            "code": "result_encoding_failed",
                            "message": "Web Search result could not be encoded"
                        }
                    })
                });
            }
            WebSearchEvent::Failed(error) => {
                return Err(serde_json::json!({ "error": error }));
            }
            WebSearchEvent::Progress {
                call_id,
                phase,
                ordinal,
            } => {
                if let Some(progress) = progress.as_ref() {
                    progress.emit(ToolProgress {
                        call_id,
                        phase: search_phase_name(phase).into(),
                        ordinal,
                        payload: None,
                    });
                }
            }
            WebSearchEvent::RunStarted { .. } => {}
        }
    }
    Err(serde_json::json!({
        "error": {
            "code": "search_incomplete",
            "message": "Web Search ended without a terminal result"
        }
    }))
}

fn search_phase_name(phase: super::WebSearchPhase) -> &'static str {
    match phase {
        super::WebSearchPhase::Started => "started",
        super::WebSearchPhase::Searching => "searching",
        super::WebSearchPhase::Synthesizing => "synthesizing",
        super::WebSearchPhase::Completed => "completed",
        super::WebSearchPhase::Failed => "failed",
    }
}

fn unavailable_error() -> Value {
    serde_json::json!({
        "error": {
            "code": "web_search_unavailable",
            "message": "Web Search is unavailable"
        }
    })
}

struct WebSearchPlatformTool {
    gateway: crate::Gateway,
}

#[async_trait]
impl PlatformTool for WebSearchPlatformTool {
    fn id(&self) -> ToolId {
        ToolId::new(PUBLIC_WEB_SEARCH_TOOL_ID)
    }

    fn external_name(&self) -> &str {
        PUBLIC_WEB_SEARCH_TOOL_NAME
    }

    fn description(&self) -> Option<&str> {
        Some("Search the public web and return a complete sourced report.")
    }

    fn parameters(&self) -> Value {
        input_schema()
    }
    fn parallel_safe(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        arguments: Value,
        context: ToolExecutionContext,
    ) -> Result<Value, PlatformToolError> {
        execute(
            &self.gateway,
            arguments,
            context.principal,
            context.cancellation,
            context.progress,
        )
        .await
        .map_err(|error| PlatformToolError::new(error.to_string()))
    }

    async fn execute_result(
        &self,
        arguments: Value,
        context: ToolExecutionContext,
    ) -> Result<PlatformToolOutput, PlatformToolError> {
        match execute(
            &self.gateway,
            arguments,
            context.principal,
            context.cancellation,
            context.progress,
        )
        .await
        {
            Ok(result) => Ok(PlatformToolOutput {
                content: vec![ContentBlock::Unknown { raw: result }],
                is_error: false,
                metadata: serde_json::Map::new(),
            }),
            Err(error) => Ok(PlatformToolOutput {
                content: vec![ContentBlock::Unknown { raw: error }],
                is_error: true,
                metadata: serde_json::Map::new(),
            }),
        }
    }
}

struct WebSearchHook {
    gateway: crate::Gateway,
}

impl Hook for WebSearchHook {
    fn descriptor(&self) -> HookDescriptor {
        HookDescriptor {
            id: HookId::new("web-search"),
            request_kinds: vec![RequestKind::Generation],
            event_kinds: vec![EventKind::Request, EventKind::UpstreamResponse],
            requires_full_context: false,
            max_buffered_bytes: 0,
            max_delayed_events: 0,
        }
    }

    fn create_session(&self, context: &SessionContext) -> Box<dyn HookSession> {
        Box::new(WebSearchHookSession {
            gateway: self.gateway.clone(),
            principal: context.principal.clone(),
            resolved: false,
            native_filters: None,
        })
    }
}

struct WebSearchHookSession {
    gateway: crate::Gateway,
    principal: Principal,
    resolved: bool,
    native_filters: Option<DomainFilters>,
}

#[async_trait]
impl HookSession for WebSearchHookSession {
    async fn handle(&mut self, event: HookEvent<'_>) -> Result<ActionBatch, String> {
        match event {
            HookEvent::Request { current, .. } if !self.resolved => {
                self.resolved = true;
                let native = match current.ext.as_ref() {
                    Some(ProtocolExt::OpenResponses(extension)) => {
                        extension.native_web_search.as_ref()
                    }
                    _ => None,
                };
                if let Some(batch) = client_web_search_precedence(current) {
                    return Ok(batch);
                }
                let Some(access) = authorized_search_access(&self.gateway, &self.principal).await
                else {
                    return if native.is_some() {
                        Ok(reject(
                            403,
                            "web_search_unavailable",
                            "Web Search is unavailable",
                        ))
                    } else {
                        Ok(ActionBatch::default())
                    };
                };
                if native.is_none() && !access.transparent_injection_enabled {
                    return Ok(ActionBatch::default());
                }
                let mut actions = Vec::with_capacity(2);
                if let Some(native) = native {
                    self.native_filters = match DomainFilters::from_hosted_tool(native) {
                        Ok(filters) => Some(filters),
                        Err(message) => return Ok(reject(400, "invalid_input", &message)),
                    };
                    if matches!(
                        &current.tool_choice,
                        Some(ToolChoice::Raw(value)) if is_native_web_search_choice(value)
                    ) {
                        actions.push(HookAction::PatchRequest(Box::new(
                            crate::hook::RequestPatch::SetToolChoice(Some(ToolChoice::Named {
                                name: PUBLIC_WEB_SEARCH_TOOL_NAME.into(),
                            })),
                        )));
                    }
                }
                actions.push(HookAction::ExposeTool(ToolId::new(
                    PUBLIC_WEB_SEARCH_TOOL_ID,
                )));
                Ok(ActionBatch { actions })
            }
            HookEvent::Request { current, .. } if self.native_filters.is_some() => {
                if matches!(
                    &current.tool_choice,
                    Some(ToolChoice::Named { .. }) | Some(ToolChoice::Required)
                ) {
                    return Ok(ActionBatch::one(HookAction::PatchRequest(Box::new(
                        crate::hook::RequestPatch::SetToolChoice(Some(ToolChoice::Auto)),
                    ))));
                }
                Ok(ActionBatch::default())
            }
            HookEvent::UpstreamResponse { classified, .. } => {
                let Some(filters) = self.native_filters.as_ref() else {
                    return Ok(ActionBatch::default());
                };
                let mut actions = Vec::new();
                for platform_call in &classified.platform {
                    if platform_call.tool_id.as_str() != PUBLIC_WEB_SEARCH_TOOL_ID {
                        continue;
                    }
                    let Ok(mut arguments) = serde_json::from_str::<serde_json::Map<String, Value>>(
                        &platform_call.call.arguments,
                    ) else {
                        continue;
                    };
                    if let Some(allowed_domains) = filters.allowed_domains.as_ref() {
                        arguments
                            .insert("allowed_domains".into(), serde_json::json!(allowed_domains));
                    }
                    if let Some(blocked_domains) = filters.blocked_domains.as_ref() {
                        arguments
                            .insert("blocked_domains".into(), serde_json::json!(blocked_domains));
                    }
                    actions.push(HookAction::PatchResponse(ResponsePatch::SetToolArguments {
                        call_id: platform_call.call.id.clone(),
                        arguments: Value::Object(arguments).to_string(),
                    }));
                }
                Ok(ActionBatch { actions })
            }
            _ => Ok(ActionBatch::default()),
        }
    }

    fn requires_terminal_buffering(&self) -> bool {
        self.native_filters.is_some()
    }
}

pub(crate) fn native_web_search_requested(request: &crate::protocol::ir::AiRequest) -> bool {
    matches!(
        request.ext.as_ref(),
        Some(ProtocolExt::OpenResponses(extension)) if extension.native_web_search.is_some()
    )
}

fn client_web_search_precedence(request: &crate::protocol::ir::AiRequest) -> Option<ActionBatch> {
    let client_owns_web_search = request
        .tools
        .iter()
        .flatten()
        .any(|tool| tool.name == PUBLIC_WEB_SEARCH_TOOL_NAME);
    if !client_owns_web_search {
        return None;
    }

    let Some(ProtocolExt::OpenResponses(extension)) = request.ext.as_ref() else {
        return Some(ActionBatch::default());
    };
    if extension.native_web_search.is_none() {
        return Some(ActionBatch::default());
    }

    let mut extension = extension.clone();
    extension.native_web_search = None;
    let mut actions = vec![HookAction::PatchRequest(Box::new(
        crate::hook::RequestPatch::SetProtocolExtension(Some(Box::new(
            ProtocolExt::OpenResponses(extension),
        ))),
    ))];
    if matches!(
        &request.tool_choice,
        Some(ToolChoice::Raw(value)) if is_native_web_search_choice(value)
    ) {
        actions.push(HookAction::PatchRequest(Box::new(
            crate::hook::RequestPatch::SetToolChoice(Some(ToolChoice::Named {
                name: PUBLIC_WEB_SEARCH_TOOL_NAME.into(),
            })),
        )));
    }
    Some(ActionBatch { actions })
}

#[derive(Default)]
struct DomainFilters {
    allowed_domains: Option<Vec<String>>,
    blocked_domains: Option<Vec<String>>,
}

impl DomainFilters {
    fn from_hosted_tool(tool: &Value) -> Result<Self, String> {
        let allowed_domains = normalized_hosted_domain_list(tool, "allowed_domains")?;
        let blocked_domains = normalized_hosted_domain_list(tool, "blocked_domains")?;
        if allowed_domains.as_ref().is_some_and(|allowed| {
            blocked_domains
                .as_ref()
                .is_some_and(|blocked| allowed.iter().any(|domain| blocked.contains(domain)))
        }) {
            return Err("domain appears in allowed_domains and blocked_domains".into());
        }
        Ok(Self {
            allowed_domains,
            blocked_domains,
        })
    }
}

fn normalized_hosted_domain_list(tool: &Value, key: &str) -> Result<Option<Vec<String>>, String> {
    let value = tool
        .get("filters")
        .and_then(Value::as_object)
        .and_then(|filters| filters.get(key))
        .or_else(|| tool.get(key));
    let Some(value) = value else {
        return Ok(None);
    };
    let values = value
        .as_array()
        .ok_or_else(|| format!("{key} must be an array of domain names"))?;
    if values.len() > 20 {
        return Err(format!("{key} cannot contain more than 20 entries"));
    }
    let domains = values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{key} must contain only domain names"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    crate::web_access::normalize_domains(domains)
        .map(Some)
        .map_err(|error| error.message)
}

fn is_native_web_search_choice(value: &Value) -> bool {
    value
        .as_str()
        .or_else(|| value.get("type").and_then(Value::as_str))
        .is_some_and(|kind| matches!(kind, "web_search" | "web_search_2025_08_26"))
}

fn reject(status: u16, code: &str, message: &str) -> ActionBatch {
    ActionBatch::one(HookAction::Reject(HookRejection {
        status,
        code: code.into(),
        message: message.into(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ir::{AiItem, AiRequest, OpenResponsesExt, ToolSpec};

    #[test]
    fn input_schema_is_strict_function_compatible() {
        let schema = input_schema();
        let properties = schema["properties"]
            .as_object()
            .expect("input schema properties must be an object");

        assert_eq!(
            schema["required"],
            serde_json::json!([
                "query",
                "previous_turn_id",
                "allowed_domains",
                "blocked_domains"
            ])
        );
        assert_eq!(
            properties["previous_turn_id"]["type"],
            serde_json::json!(["string", "null"])
        );
        assert_eq!(
            properties["allowed_domains"]["type"],
            serde_json::json!(["array", "null"])
        );
        assert_eq!(
            properties["blocked_domains"]["type"],
            serde_json::json!(["array", "null"])
        );
    }

    fn client_web_search_request() -> AiRequest {
        let mut request = AiRequest::new("model", Vec::<AiItem>::new());
        request.tools = Some(vec![ToolSpec {
            name: PUBLIC_WEB_SEARCH_TOOL_NAME.into(),
            description: None,
            parameters: serde_json::json!({"type": "object"}),
            strict: None,
            cache_control: None,
            meta: None,
        }]);
        request
    }

    #[test]
    fn client_function_wins_over_hosted_web_search_declaration() {
        let mut request = client_web_search_request();
        request.ext = Some(ProtocolExt::OpenResponses(OpenResponsesExt {
            native_web_search: Some(serde_json::json!({"type": "web_search"})),
            ..Default::default()
        }));
        request.tool_choice = Some(ToolChoice::Raw(serde_json::json!({
            "type": "web_search"
        })));

        let batch = client_web_search_precedence(&request).expect("client-owned collision");

        assert_eq!(batch.actions.len(), 2);
        assert!(
            batch
                .actions
                .iter()
                .all(|action| !matches!(action, HookAction::Reject(_)))
        );
        assert!(batch.actions.iter().any(|action| matches!(
            action,
            HookAction::PatchRequest(patch)
                if matches!(
                    patch.as_ref(),
                    crate::hook::RequestPatch::SetProtocolExtension(Some(extension))
                        if matches!(
                            extension.as_ref(),
                            ProtocolExt::OpenResponses(extension)
                                if extension.native_web_search.is_none()
                        )
                )
        )));
        assert!(batch.actions.iter().any(|action| matches!(
            action,
            HookAction::PatchRequest(patch)
                if matches!(
                    patch.as_ref(),
                    crate::hook::RequestPatch::SetToolChoice(
                        Some(ToolChoice::Named { name })
                    ) if name == PUBLIC_WEB_SEARCH_TOOL_NAME
                )
        )));
    }

    #[test]
    fn client_function_suppresses_transparent_web_search_injection() {
        let request = client_web_search_request();

        let batch = client_web_search_precedence(&request).expect("client-owned collision");

        assert!(batch.actions.is_empty());
    }
}
