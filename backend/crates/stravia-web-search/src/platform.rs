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
use stravia_runtime_contract::protocol::ir::{
    ContentBlock, NativeTool, NativeToolKind, ToolChoice, WebSearchTool,
};

use stravia_web_access_contract::read_path::{ReadInput, ReadTarget, parse_read_input};

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
                let native = native_web_search_tool(current);
                let filters = match native.map(DomainFilters::from_native_tool).transpose() {
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
                        Some(ToolChoice::Native(NativeToolKind::WebSearch))
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
                    description: "Use StraviaRead with path stravia://search and the query in question for complete sourced research, or a public HTTP(S) URL for webpage Markdown and file import.".into(),
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
                    let Ok(arguments) = serde_json::from_str::<serde_json::Map<String, Value>>(
                        &platform_call.call.arguments,
                    ) else {
                        continue;
                    };
                    let gateway = self.gateway.as_ref();
                    let principal = &self.principal;
                    let Some(rewritten) =
                        rewritten_arguments(&arguments, filters, |turn_id| async move {
                            match gateway.runner().await {
                                Ok(runner) => runner.is_search_turn(principal, &turn_id).await,
                                Err(_) => false,
                            }
                        })
                        .await
                    else {
                        continue;
                    };
                    actions.push(HookAction::PatchResponse(ResponsePatch::SetToolArguments {
                        call_id: platform_call.call.id.clone().into_string(),
                        arguments: Value::Object(rewritten).to_string(),
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
    native_web_search_tool(request).is_some()
}

fn native_web_search_tool(
    request: &stravia_runtime_contract::protocol::ir::AiRequest,
) -> Option<&WebSearchTool> {
    request
        .parameters
        .native_tools
        .iter()
        .find_map(|tool| match tool {
            NativeTool::WebSearch(tool) => Some(tool),
            _ => None,
        })
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

    if !native_web_search_requested(request) {
        return Some(ActionBatch::default());
    }

    let mut parameters = request.parameters.clone();
    parameters
        .native_tools
        .retain(|tool| !matches!(tool, NativeTool::WebSearch(_)));
    let mut actions = vec![HookAction::PatchRequest(Box::new(
        stravia_runtime_contract::hook::RequestPatch::SetParameters(Box::new(parameters)),
    ))];
    if matches!(
        &request.tool_choice,
        Some(ToolChoice::Native(NativeToolKind::WebSearch))
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

/// 用原生 web_search 声明的域名限制替换 StraviaRead 参数里的 `allowed_domains`。
///
/// 只改写搜索入口：`path` 为搜索 path，或为 `is_search_turn` 确认属于当前
/// Principal 的 Web Search 轮；媒体轮、资源 path 和无法解析的 path 保持原样。
/// 原生声明没有域名限制时同样不改写。只替换这一个字段，不注入其他字段。
async fn rewritten_arguments<F, Fut>(
    arguments: &serde_json::Map<String, Value>,
    filters: &DomainFilters,
    is_search_turn: F,
) -> Option<serde_json::Map<String, Value>>
where
    F: FnOnce(SearchTurnId) -> Fut,
    Fut: Future<Output = bool>,
{
    let allowed_domains = filters.allowed_domains.clone()?;
    let path = arguments.get("path")?.as_str()?;
    // 只用解析器分类 path 形式；探测用的 question 不会被读取。
    let probe = ReadInput {
        path: path.to_owned(),
        question: Some("probe".to_owned()),
        attachments: None,
        allowed_domains: None,
        wait: None,
    };
    let applies = match parse_read_input(&probe).ok()? {
        ReadTarget::Search(_) => true,
        ReadTarget::Continuation(continuation) => {
            let turn_id = SearchTurnId::from_reference(&continuation.turn_path).ok()?;
            is_search_turn(turn_id).await
        }
        ReadTarget::Resource(_) | ReadTarget::ToolExecution(_) => false,
    };
    if !applies {
        return None;
    }
    let mut rewritten = arguments.clone();
    rewritten.insert("allowed_domains".into(), serde_json::json!(allowed_domains));
    Some(rewritten)
}

impl DomainFilters {
    fn from_native_tool(tool: &WebSearchTool) -> Result<Self, String> {
        if !tool.blocked_domains.is_empty() {
            return Err("blocked_domains is not supported".into());
        }
        if tool.allowed_domains.is_empty() {
            return Ok(Self::default());
        }
        if tool.allowed_domains.len() > 20 {
            return Err("allowed_domains cannot contain more than 20 entries".into());
        }
        let allowed_domains =
            stravia_web_access_contract::normalize_domains(tool.allowed_domains.clone())
                .map_err(|error| error.message)?;
        Ok(Self {
            allowed_domains: Some(allowed_domains),
        })
    }
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
    use stravia_runtime_contract::protocol::ir::{AiItem, AiRequest, ToolSpec};

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
        let tool = WebSearchTool {
            blocked_domains: vec!["spam.org".into()],
            ..Default::default()
        };
        assert!(DomainFilters::from_native_tool(&tool).is_err());
    }

    const SEARCH_PATH: &str = "stravia://search";
    const TURN_PATH: &str = "stravia://turns/abcdefghijklmnopqrstuvwxyzab";

    fn arguments(path: &str) -> serde_json::Map<String, Value> {
        serde_json::json!({
            "path": path,
            "question": "climate policy",
            "attachments": null,
            "allowed_domains": ["other.org"],
        })
        .as_object()
        .expect("arguments")
        .clone()
    }

    fn example_filters() -> DomainFilters {
        DomainFilters::from_native_tool(&WebSearchTool {
            allowed_domains: vec!["Example.COM".into(), "example.com".into()],
            ..Default::default()
        })
        .expect("native allowed domains")
    }

    #[tokio::test]
    async fn native_empty_domain_filters_leave_model_arguments_unchanged() {
        let filters = DomainFilters::from_native_tool(&WebSearchTool::default())
            .expect("empty canonical filters");
        assert!(
            rewritten_arguments(&arguments(SEARCH_PATH), &filters, |_| async { true })
                .await
                .is_none()
        );
    }

    #[test]
    fn native_search_is_detected_across_ingress_protocols() {
        use stravia_runtime_contract::protocol::ir::{
            native_tool_from_anthropic, native_tool_from_chat_web_search_options,
            native_tool_from_gemini, native_tool_from_responses,
        };

        let tools = [
            native_tool_from_responses(&serde_json::json!({"type": "web_search"})),
            native_tool_from_responses(&serde_json::json!({"type": "web_search_preview"})),
            native_tool_from_anthropic(&serde_json::json!({
                "type": "web_search_20250305",
                "name": "web_search"
            })),
            native_tool_from_gemini(&serde_json::json!({"googleSearch": {}})),
            native_tool_from_gemini(&serde_json::json!({"googleSearchRetrieval": {}})),
            native_tool_from_chat_web_search_options(&serde_json::json!({})),
        ];
        for tool in tools {
            let mut request = AiRequest::new("model", Vec::<AiItem>::new());
            assert!(!native_web_search_requested(&request));
            request
                .parameters
                .native_tools
                .push(tool.expect("native search"));
            assert!(native_web_search_requested(&request));
        }
    }

    #[test]
    fn native_allowed_domain_filters_keep_validation_limits() {
        for domains in [
            vec!["https://example.com/path".into()],
            vec!["example.com".into(); 21],
        ] {
            let tool = WebSearchTool {
                allowed_domains: domains,
                ..Default::default()
            };
            assert!(DomainFilters::from_native_tool(&tool).is_err());
        }
    }

    #[tokio::test]
    async fn native_allowed_domains_replace_only_that_field_of_a_search() {
        let original = arguments(SEARCH_PATH);
        let rewritten = rewritten_arguments(&original, &example_filters(), |_| async {
            panic!("a new search needs no Turn lookup")
        })
        .await
        .expect("search override");

        assert_eq!(
            rewritten["allowed_domains"],
            serde_json::json!(["example.com"])
        );
        let mut expected = original.clone();
        expected.insert("allowed_domains".into(), serde_json::json!(["example.com"]));
        assert_eq!(rewritten, expected);
    }

    #[tokio::test]
    async fn native_allowed_domains_replace_those_of_a_search_continuation() {
        let rewritten = rewritten_arguments(
            &arguments(TURN_PATH),
            &example_filters(),
            |turn_id| async move { turn_id.reference() == TURN_PATH },
        )
        .await
        .expect("continuation override");

        assert_eq!(rewritten["path"], TURN_PATH);
        assert_eq!(
            rewritten["allowed_domains"],
            serde_json::json!(["example.com"])
        );
    }

    #[tokio::test]
    async fn native_allowed_domains_leave_media_turns_resources_and_invalid_paths_alone() {
        let filters = example_filters();
        // 媒体轮或不可见的 Turn：查询结果不是搜索轮。
        assert!(
            rewritten_arguments(&arguments(TURN_PATH), &filters, |_| async { false })
                .await
                .is_none()
        );
        for path in [
            "https://example.com/page".to_owned(),
            format!("stravia://artifacts/{}", "a".repeat(55)),
            "stravia://tool-executions/abcdefghijklmnopqrstuvwxyzab".to_owned(),
            "search://plain".to_owned(),
            "not a path".to_owned(),
        ] {
            let mut input = arguments(&path);
            input.remove("question");
            assert!(
                rewritten_arguments(&input, &filters, |_| async { true })
                    .await
                    .is_none(),
                "{path}"
            );
        }
        let mut without_path = arguments(SEARCH_PATH);
        without_path.remove("path");
        assert!(
            rewritten_arguments(&without_path, &filters, |_| async { true })
                .await
                .is_none()
        );
    }

    fn client_web_search_request() -> AiRequest {
        let mut request = AiRequest::new("model", Vec::<AiItem>::new());
        request.tools = Some(vec![ToolSpec {
            name: PUBLIC_WEB_SEARCH_TOOL_NAME.into(),
            description: None,
            parameters: serde_json::json!({"type": "object"}),
            strict: None,
            defer_loading: None,
        }]);
        request
    }

    #[test]
    fn client_function_wins_over_native_web_search_declaration() {
        let mut request = client_web_search_request();
        request
            .parameters
            .native_tools
            .push(NativeTool::WebSearch(WebSearchTool::default()));
        request
            .parameters
            .native_tools
            .push(NativeTool::CodeExecution(
                stravia_runtime_contract::protocol::ir::CodeExecutionTool::default(),
            ));
        request.tool_choice = Some(ToolChoice::Native(NativeToolKind::WebSearch));

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
                    stravia_runtime_contract::hook::RequestPatch::SetParameters(parameters)
                        if matches!(
                            parameters.native_tools.as_slice(),
                            [NativeTool::CodeExecution(_)]
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
