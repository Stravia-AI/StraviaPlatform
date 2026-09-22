use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::Value;

use stravia_runtime_contract::Principal;
use stravia_runtime_contract::hook::{
    ActionBatch, EventKind, Hook, HookAction, HookDescriptor, HookEvent, HookId, HookRejection,
    HookSession, PlatformTool, PlatformToolError, PlatformToolOutput, RequestKind, ResponsePatch,
    SessionContext, ToolExecutionContext, ToolId, ToolProgress, ToolProgressSink,
};
use stravia_runtime_contract::protocol::ir::{ContentBlock, ProtocolExt, ToolChoice};

use stravia_web_access_contract::read_path::{ReadTarget, format_search_path, parse_read_path};

use super::{SearchTurnId, WebSearchEvent, WebSearchInput, WebSearchRunPolicy, WebSearchRunner};

pub const PUBLIC_WEB_SEARCH_TOOL_ID: &str = "stravia-read";
pub const PUBLIC_WEB_SEARCH_TOOL_NAME: &str = "StraviaRead";
const MAX_PUBLIC_DEADLINE: Duration = Duration::from_secs(15 * 60);

pub type BuiltinExtensions = (Vec<Arc<dyn Hook>>, Vec<Arc<dyn PlatformTool>>);

pub fn builtin_extensions(gateway: Arc<dyn crate::host::PublicSearchHost>) -> BuiltinExtensions {
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
    previous_path: Option<String>,
    #[serde(default)]
    allowed_domains: Option<Vec<String>>,
}

pub fn input_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "minLength": 1,
                "description": "The question or topic to search. UTF-8 encoding must not exceed 64 KiB."
            },
            "previous_path": {
                "type": ["string", "null"],
                "pattern": "^stravia://turns/[a-z]{28}$",
                "maxLength": 128,
                "description": "An exact prior stravia://turns/<turn-id> path to continue or branch from."
            },
            "allowed_domains": {
                "type": ["array", "null"],
                "maxItems": 20,
                "items": { "type": "string" }
            }
        },
        "required": ["query", "previous_path", "allowed_domains"],
        "additionalProperties": false
    })
}

pub async fn is_available(
    gateway: &dyn crate::host::PublicSearchHost,
    principal: &Principal,
) -> bool {
    authorized_search_access(gateway, principal).await.is_some()
}

async fn authorized_search_access(
    gateway: &dyn crate::host::PublicSearchHost,
    principal: &Principal,
) -> Option<crate::host::SearchAccess> {
    let access = gateway.authorize(principal).await?;
    if !gateway.runner_ready().await || !gateway.config().await.ok()?.enabled {
        return None;
    }
    Some(access)
}

pub async fn execute(
    gateway: &dyn crate::host::PublicSearchHost,
    arguments: Value,
    principal: Principal,
    cancellation: stravia_runtime_contract::CancellationToken,
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
    let runner: WebSearchRunner = gateway.runner().await.map_err(|_| unavailable_error())?;
    let previous_turn_id = request
        .previous_path
        .as_deref()
        .map(SearchTurnId::from_reference)
        .transpose()
        .map_err(|error| {
            serde_json::json!({
                "error": { "code": "invalid_input", "message": error.to_string() }
            })
        })?;
    let policy = request
        .allowed_domains
        .map(|allowed_domains| WebSearchRunPolicy { allowed_domains });
    let mut stream = runner.run(WebSearchInput {
        principal,
        query: request.query,
        previous_turn_id,
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
    gateway: Arc<dyn crate::host::PublicSearchHost>,
}

#[async_trait]
impl PlatformTool for WebSearchPlatformTool {
    fn read_domain(&self) -> Option<stravia_runtime_contract::hook::StraviaReadDomain> {
        Some(stravia_runtime_contract::hook::StraviaReadDomain::Query)
    }

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
            self.gateway.as_ref(),
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
            self.gateway.as_ref(),
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
    gateway: Arc<dyn crate::host::PublicSearchHost>,
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
            resolved: context.tools_fixed,
            native_filters: None,
        })
    }
}

struct WebSearchHookSession {
    gateway: Arc<dyn crate::host::PublicSearchHost>,
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
                let filters = match native.map(DomainFilters::from_hosted_tool).transpose() {
                    Ok(filters) => filters,
                    Err(message) => return Ok(reject(400, "invalid_input", &message)),
                };
                if let Some(batch) = client_web_search_precedence(current) {
                    return Ok(batch);
                }
                let Some(_access) =
                    authorized_search_access(self.gateway.as_ref(), &self.principal).await
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
                if native.is_none() {
                    return Ok(ActionBatch::default());
                }
                let mut actions = Vec::with_capacity(2);
                if native.is_some() {
                    self.native_filters = filters;
                    if matches!(
                        &current.tool_choice,
                        Some(ToolChoice::Raw(value)) if is_native_web_search_choice(value)
                    ) {
                        actions.push(HookAction::PatchRequest(Box::new(
                            stravia_runtime_contract::hook::RequestPatch::SetToolChoice(Some(
                                ToolChoice::Named {
                                    name: PUBLIC_WEB_SEARCH_TOOL_NAME.into(),
                                },
                            )),
                        )));
                    }
                }
                actions.push(HookAction::ExposeRead {
                    scope: stravia_runtime_contract::hook::ReadExposureScope::new(true, false),
                    description: "Use StraviaRead with a single path: search://<URL-encoded query> for complete sourced research, or public HTTP(S) URLs for webpage Markdown and file import.".into(),
                });
                Ok(ActionBatch { actions })
            }
            HookEvent::Request { current, .. } if self.native_filters.is_some() => {
                if matches!(
                    &current.tool_choice,
                    Some(ToolChoice::Named { .. }) | Some(ToolChoice::Required)
                ) {
                    return Ok(ActionBatch::one(HookAction::PatchRequest(Box::new(
                        stravia_runtime_contract::hook::RequestPatch::SetToolChoice(Some(
                            ToolChoice::Auto,
                        )),
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
                    let Some(path) = arguments.get("path").and_then(Value::as_str) else {
                        continue;
                    };
                    let Some(path) = rewritten_search_path(path, filters) else {
                        continue;
                    };
                    arguments.insert("path".into(), serde_json::json!(path));
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

pub fn native_web_search_requested(
    request: &stravia_runtime_contract::protocol::ir::AiRequest,
) -> bool {
    matches!(
        request.ext.as_ref(),
        Some(ProtocolExt::OpenResponses(extension)) if extension.native_web_search.is_some()
    )
}

fn client_web_search_precedence(
    request: &stravia_runtime_contract::protocol::ir::AiRequest,
) -> Option<ActionBatch> {
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
        stravia_runtime_contract::hook::RequestPatch::SetProtocolExtension(Some(Box::new(
            ProtocolExt::OpenResponses(extension),
        ))),
    ))];
    if matches!(
        &request.tool_choice,
        Some(ToolChoice::Raw(value)) if is_native_web_search_choice(value)
    ) {
        actions.push(HookAction::PatchRequest(Box::new(
            stravia_runtime_contract::hook::RequestPatch::SetToolChoice(Some(ToolChoice::Named {
                name: PUBLIC_WEB_SEARCH_TOOL_NAME.into(),
            })),
        )));
    }
    Some(ActionBatch { actions })
}

#[derive(Default)]
struct DomainFilters {
    allowed_domains: Option<Vec<String>>,
}

/// Re-encodes a recognized search path with the native allowed domains
/// replacing the model's own list. Returns `None` when the path is not a
/// valid search target or the native declaration carries no domain override.
/// No extra top-level fields are injected into the tool arguments.
fn rewritten_search_path(path: &str, filters: &DomainFilters) -> Option<String> {
    let ReadTarget::Search(mut search) = parse_read_path(path).ok()? else {
        return None;
    };
    search.allowed_domains = Some(filters.allowed_domains.clone()?);
    Some(format_search_path(&search))
}

impl DomainFilters {
    fn from_hosted_tool(tool: &Value) -> Result<Self, String> {
        if tool.get("blocked_domains").is_some()
            || tool
                .get("filters")
                .and_then(|filters| filters.get("blocked_domains"))
                .is_some()
        {
            return Err("blocked_domains is not supported".into());
        }
        let allowed_domains = normalized_hosted_domain_list(tool, "allowed_domains")?;
        if allowed_domains.as_ref().is_some_and(Vec::is_empty) {
            return Err("allowed_domains cannot be empty".into());
        }
        Ok(Self { allowed_domains })
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
    stravia_web_access_contract::normalize_domains(domains)
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

pub fn output_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "pattern": "^stravia://turns/[a-z]{28}$" },
            "completion": { "type": "string", "enum": ["complete", "partial"] },
            "report": crate::local::search_report_schema(),
            "pagination": stravia_web_access_contract::read_path::pagination_schema()
        },
        "required": ["path", "completion", "report"],
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use stravia_runtime_contract::protocol::ir::{AiItem, AiRequest, OpenResponsesExt, ToolSpec};

    #[test]
    fn input_schema_is_strict_function_compatible() {
        let schema = input_schema();
        let properties = schema["properties"]
            .as_object()
            .expect("input schema properties must be an object");

        assert_eq!(
            schema["required"],
            serde_json::json!(["query", "previous_path", "allowed_domains"])
        );
        assert_eq!(
            properties["previous_path"]["type"],
            serde_json::json!(["string", "null"])
        );
        assert_eq!(
            properties["allowed_domains"]["type"],
            serde_json::json!(["array", "null"])
        );
        assert!(properties.get("blocked_domains").is_none());
    }

    #[test]
    fn public_search_input_rejects_blocked_domains_as_an_unknown_field() {
        assert!(
            serde_json::from_value::<PublicSearchInput>(serde_json::json!({
                "query": "Search the claim",
                "previous_path": None::<String>,
                "allowed_domains": None::<Vec<String>>,
                "blocked_domains": []
            }))
            .is_err()
        );
    }

    #[test]
    fn native_blocked_domain_filters_are_rejected_as_unsupported() {
        for tool in [
            serde_json::json!({
                "type": "web_search",
                "filters": {"allowed_domains": ["example.com"], "blocked_domains": ["spam.org"]}
            }),
            serde_json::json!({"type": "web_search", "blocked_domains": ["spam.org"]}),
            serde_json::json!({"type": "web_search", "filters": {"blocked_domains": []}}),
            serde_json::json!({"type": "web_search", "blocked_domains": null}),
            serde_json::json!({"type": "web_search", "filters": {"blocked_domains": "ignored?"}}),
        ] {
            assert!(DomainFilters::from_hosted_tool(&tool).is_err());
        }
    }

    #[test]
    fn native_empty_allowed_domain_filters_are_rejected() {
        for tool in [
            serde_json::json!({"type": "web_search", "filters": {"allowed_domains": []}}),
            serde_json::json!({"type": "web_search", "allowed_domains": []}),
        ] {
            assert!(DomainFilters::from_hosted_tool(&tool).is_err());
        }
    }

    #[test]
    fn native_allowed_domains_replace_the_model_search_path() {
        let filters = DomainFilters::from_hosted_tool(&serde_json::json!({
            "type": "web_search",
            "filters": {"allowed_domains": ["Example.COM", "example.com"]}
        }))
        .expect("native allowed domains");
        let previous_path = "stravia://turns/abcdefghijklmnopqrstuvwxyzab";
        let rewritten = rewritten_search_path(
            "search://climate%20policy?allowed_domains=other.org&previous_path=stravia%3A%2F%2Fturns%2Fabcdefghijklmnopqrstuvwxyzab",
            &filters,
        )
        .expect("search path override");
        let ReadTarget::Search(search) =
            parse_read_path(&rewritten).expect("rewritten path must reparse")
        else {
            unreachable!("re-encoded search path must parse as a search target")
        };
        assert_eq!(search.query, "climate policy");
        assert_eq!(search.allowed_domains, Some(vec!["example.com".to_owned()]));
        assert_eq!(search.previous_path.as_deref(), Some(previous_path));

        // Without a native override the model path is left untouched.
        assert_eq!(
            rewritten_search_path("search://plain", &DomainFilters::default()),
            None
        );
        // Non-search paths are never rewritten.
        let override_filters = DomainFilters {
            allowed_domains: Some(vec!["example.com".to_owned()]),
        };
        assert_eq!(
            rewritten_search_path("https://example.com/page", &override_filters),
            None
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
                    stravia_runtime_contract::hook::RequestPatch::SetProtocolExtension(Some(extension))
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
                    stravia_runtime_contract::hook::RequestPatch::SetToolChoice(
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
