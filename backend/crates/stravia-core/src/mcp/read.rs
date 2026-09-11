use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use super::{McpContext, McpTool, McpToolError, McpToolOutput};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use stravia_runtime_contract::{
    CancellationToken, Principal,
    artifact::{ArtifactId, ArtifactSettings, bytes_stream},
    hook::{
        ActionBatch, EventKind, Hook, HookAction, HookDescriptor, HookEvent, HookId, HookSession,
        PlatformTool, PlatformToolError, PlatformToolOutput, ReadExposureScope, RequestKind,
        SessionContext, StraviaReadDomain, ToolExecutionContext, ToolId,
    },
    protocol::ir::ContentBlock,
};
use stravia_web_search::host::PublicSearchHost;

pub(crate) const TOOL_ID: &str = "stravia-read";
pub(crate) const TOOL_NAME: &str = "StraviaRead";
const DOWNLOAD_DESCRIPTION: &str = "Read an owned https://stravia/artifact/<id> reference to obtain a temporary download URL and file metadata without model execution.";
const NETWORK_DESCRIPTION: &str = "Use query://<URL-encoded query> for a complete sourced research report, or a public HTTP(S) URL for webpage Markdown or file import. Search continuation accepts previous_turn_id and domain filters.";
const MEDIA_DESCRIPTION: &str = "Ask about a static JPEG, PNG or WebP Artifact with ?question=<URL-encoded question>. Public image URLs are stored, then described with readable text extracted.";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadInput {
    url: String,
    #[serde(default)]
    previous_turn_id: Option<String>,
    #[serde(default)]
    allowed_domains: Option<Vec<String>>,
    #[serde(default)]
    blocked_domains: Option<Vec<String>>,
}

pub(crate) fn input_schema() -> Value {
    json!({"type":"object", "properties": {
        "url":{"type":"string","minLength":1,"description":"Artifact Reference, public HTTP(S) URL, or query://<URL-encoded search query>. Only Artifact URLs interpret question."},
        "previous_turn_id":{"type":["string","null"],"description":"Prior Search Turn for query://, or prior Media Understanding Turn for an Artifact URL with question."},
        "allowed_domains":{"type":["array","null"],"items":{"type":"string"},"maxItems":20},
        "blocked_domains":{"type":["array","null"],"items":{"type":"string"},"maxItems":20}
    },"required":["url"],"additionalProperties":false})
}

#[derive(Clone)]
pub(crate) struct ReadTool {
    gateway: crate::Gateway,
    handlers: Arc<HashMap<StraviaReadDomain, Arc<dyn PlatformTool>>>,
    internal: bool,
}

impl ReadTool {
    pub(crate) fn new(
        gateway: &crate::Gateway,
        tools: &mut Vec<Arc<dyn PlatformTool>>,
    ) -> anyhow::Result<Self> {
        if tools
            .iter()
            .any(|tool| tool.read_domain().is_none() && tool.external_name() == TOOL_NAME)
        {
            anyhow::bail!("StraviaRead is reserved for domain contributions");
        }
        let mut handlers = HashMap::new();
        let mut identities = std::collections::HashSet::new();
        tools.push(Arc::new(WebPageTool {
            gateway: gateway.clone(),
        }));
        for tool in tools.iter().filter(|tool| tool.read_domain().is_some()) {
            let domain = tool.read_domain().expect("filtered read contribution");
            tool.description()
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "StraviaRead contribution requires a description: {}",
                        tool.id()
                    )
                })?;
            if !identities.insert(tool.id()) || handlers.insert(domain, Arc::clone(tool)).is_some()
            {
                anyhow::bail!("ambiguous StraviaRead contribution for {domain:?}");
            }
        }
        tools.retain(|tool| tool.read_domain().is_none());
        Ok(Self {
            gateway: gateway.clone(),
            handlers: Arc::new(handlers),
            internal: false,
        })
    }

    fn read_description(&self, scope: ReadExposureScope) -> String {
        let mut text = description(scope);
        for domain in [
            StraviaReadDomain::Query,
            StraviaReadDomain::WebPage,
            StraviaReadDomain::Media,
        ] {
            let visible = match domain {
                StraviaReadDomain::Query | StraviaReadDomain::WebPage => scope.networking(),
                StraviaReadDomain::Media => scope.media(),
            };
            if visible {
                if let Some(description) = self
                    .handlers
                    .get(&domain)
                    .and_then(|tool| tool.description())
                {
                    text.push('\n');
                    text.push_str(description);
                }
            }
        }
        text
    }

    async fn capabilities(&self, principal: &Principal) -> ReadExposureScope {
        ReadExposureScope::new(
            networking_available(&self.gateway, principal).await,
            stravia_media::platform::is_available(&crate::media::runtime(&self.gateway), principal)
                .await,
        )
    }

    async fn dispatch(
        &self,
        domain: StraviaReadDomain,
        arguments: Value,
        context: ToolExecutionContext,
    ) -> Result<PlatformToolOutput, PlatformToolError> {
        let handler = self
            .handlers
            .get(&domain)
            .ok_or_else(|| PlatformToolError::new("StraviaRead handler unavailable"))?;
        handler.execute_result(arguments, context).await
    }

    async fn read(
        &self,
        arguments: Value,
        context: ToolExecutionContext,
    ) -> Result<PlatformToolOutput, PlatformToolError> {
        let input: ReadInput = serde_json::from_value(arguments).map_err(|error| {
            PlatformToolError::new(format!("invalid StraviaRead input: {error}"))
        })?;
        before_execution(&context.cancellation, async {
            crate::proxy::security::Security::new(self.gateway.storage.auth())
                .authorize_principal_capability(&context.principal)
                .await
                .map_err(|_| PlatformToolError::new("Principal authorization failed"))
        })
        .await??;
        if let Some(query) = crate::web_access::decode_query_url(&input.url) {
            require(
                context.read_scope.networking(),
                "Networking was not exposed for this run",
            )?;
            require(
                before_execution(
                    &context.cancellation,
                    networking_available(&self.gateway, &context.principal),
                )
                .await?,
                "Networking is unavailable",
            )?;
            return self.dispatch(StraviaReadDomain::Query, json!({"query":query,"previous_turn_id":input.previous_turn_id,"allowed_domains":input.allowed_domains,"blocked_domains":input.blocked_domains}), context).await;
        }
        require(
            input.allowed_domains.is_none() && input.blocked_domains.is_none(),
            "Search domain filters require query://",
        )?;
        let parsed =
            url::Url::parse(&input.url).map_err(|_| PlatformToolError::new("Invalid read URL"))?;
        if parsed.host_str() == Some("stravia") {
            let id = ArtifactId::from_reference(&input.url).map_err(artifact_error)?;
            let mut question = None;
            for (name, value) in parsed.query_pairs() {
                require(
                    name == "question" && question.is_none(),
                    "Artifact URLs accept one question parameter only",
                )?;
                require(
                    !value.trim().is_empty(),
                    "Artifact question cannot be empty",
                )?;
                question = Some(value.into_owned());
            }
            return match question {
                Some(question) => {
                    self.understand(id, question, input.previous_turn_id, context)
                        .await
                }
                None => {
                    require(
                        input.previous_turn_id.is_none(),
                        "A download does not accept continuation metadata",
                    )?;
                    before_execution(
                        &context.cancellation,
                        download(&self.gateway, &context.principal, &id, None),
                    )
                    .await?
                }
            };
        }
        require(
            input.previous_turn_id.is_none(),
            "Continuation requires query:// or an Artifact question",
        )?;
        require(
            matches!(parsed.scheme(), "http" | "https"),
            "Only Artifact, query:// and public HTTP(S) URLs are supported",
        )?;
        let enabled =
            before_execution(&context.cancellation, self.capabilities(&context.principal)).await?;
        require(
            (context.read_scope.networking() && enabled.networking())
                || (context.read_scope.media() && enabled.media()),
            "External reading is unavailable in this run",
        )?;
        let resource =
            crate::media::ingest::fetch_public_read_resource(&input.url, &context.cancellation)
                .await
                .map_err(|error| PlatformToolError::new(error.to_string()))?;
        let (mime, bytes) = match resource {
            crate::media::ingest::PublicReadResource::Html => {
                require(
                    context.read_scope.networking() && enabled.networking(),
                    "Networking is unavailable in this run",
                )?;
                if self.internal {
                    return fetch_page(&self.gateway, input.url, &context).await;
                }
                return self
                    .dispatch(
                        StraviaReadDomain::WebPage,
                        json!({"url":input.url}),
                        context,
                    )
                    .await;
            }
            crate::media::ingest::PublicReadResource::File(mime, bytes) => (mime, bytes),
        };
        let image = mime
            .split(';')
            .next()
            .unwrap_or(&mime)
            .trim()
            .starts_with("image/");
        if image {
            require(
                context.read_scope.media() && enabled.media(),
                "Media Understanding is unavailable in this run",
            )?;
        } else {
            require(
                context.read_scope.networking() && enabled.networking(),
                "Networking is unavailable in this run",
            )?;
        }
        let store = self
            .gateway
            .artifact_store()
            .ok_or_else(|| PlatformToolError::new("Artifact storage is unavailable"))?;
        let artifact = before_execution(&context.cancellation, async {
            store
                .ingest(
                    &context.principal,
                    &mime,
                    Some(bytes.len() as u64),
                    bytes_stream(bytes),
                    retention(&self.gateway).await?,
                )
                .await
                .map_err(artifact_error)
        })
        .await??;
        if image {
            self.understand(
                artifact.id,
                "Describe the image content and extract all readable text.".into(),
                None,
                context,
            )
            .await
        } else {
            let filename = parsed
                .path_segments()
                .and_then(|mut paths| paths.next_back())
                .filter(|name| !name.is_empty())
                .map(str::to_owned);
            before_execution(
                &context.cancellation,
                download(&self.gateway, &context.principal, &artifact.id, filename),
            )
            .await?
        }
    }

    async fn understand(
        &self,
        id: ArtifactId,
        question: String,
        previous_turn_id: Option<String>,
        context: ToolExecutionContext,
    ) -> Result<PlatformToolOutput, PlatformToolError> {
        require(
            context.read_scope.media(),
            "Media Understanding was not exposed for this run",
        )?;
        require(
            before_execution(
                &context.cancellation,
                stravia_media::platform::is_available(
                    &crate::media::runtime(&self.gateway),
                    &context.principal,
                ),
            )
            .await?,
            "Media Understanding is unavailable",
        )?;
        let store = self
            .gateway
            .artifact_store()
            .ok_or_else(|| PlatformToolError::new("Artifact storage is unavailable"))?;
        let reader = before_execution(&context.cancellation, async {
            store
                .extend_retention(&context.principal, &id, retention(&self.gateway).await?)
                .await
                .map_err(artifact_error)?;
            store
                .open(&context.principal, &id)
                .await
                .map_err(artifact_error)
        })
        .await??;
        let artifact_reference = reader.artifact.reference();
        // Media preprocessing acquires its own guard for the actual byte read.
        // Do not hold a second store connection while awaiting that execution.
        drop(reader);
        let mut output = self.dispatch(StraviaReadDomain::Media, json!({"prompt":question,"artifacts":[{"artifact_id":id.as_str()}],"previous_turn_id":previous_turn_id}), context).await?;
        if !output.is_error {
            let reference = json!(artifact_reference);
            output
                .metadata
                .insert("artifact_reference".into(), reference.clone());
            if let Some(media) = output
                .metadata
                .get_mut("stravia_media")
                .and_then(Value::as_object_mut)
            {
                media.insert("artifact_reference".into(), reference.clone());
            }
            for block in &mut output.content {
                if let ContentBlock::Unknown { raw } = block {
                    if let Some(result) = raw.as_object_mut() {
                        result.insert("artifact_reference".into(), reference.clone());
                    }
                }
            }
        }
        Ok(output)
    }
}

pub(crate) async fn networking_available(gateway: &crate::Gateway, principal: &Principal) -> bool {
    let host = crate::web_search::host::SearchHost(gateway.clone());
    host.authorize(principal).await.is_some()
        && host.config().await.is_ok_and(|config| config.enabled)
}
fn require(value: bool, message: &str) -> Result<(), PlatformToolError> {
    if value {
        Ok(())
    } else {
        Err(PlatformToolError::new(message))
    }
}
async fn before_execution<T>(
    cancellation: &CancellationToken,
    work: impl Future<Output = T>,
) -> Result<T, PlatformToolError> {
    // The owning MCP/platform execution deadline cancels this same token.
    // Only preparation is dropped here; Agent dispatch retains cooperative cleanup.
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(PlatformToolError::new("StraviaRead cancelled")),
        result = work => Ok(result),
    }
}

fn artifact_error(error: stravia_runtime_contract::artifact::ArtifactError) -> PlatformToolError {
    PlatformToolError::new(error.to_string())
}
async fn retention(gateway: &crate::Gateway) -> Result<Duration, PlatformToolError> {
    let days = gateway
        .storage
        .settings()
        .get("log_retention_days")
        .await
        .map_err(|error| PlatformToolError::new(error.to_string()))?
        .map(|value| value.parse::<u32>())
        .transpose()
        .map_err(|_| PlatformToolError::new("Invalid retention setting"))?
        .unwrap_or(7);
    Ok(Duration::from_secs(u64::from(days) * 86400))
}
async fn download(
    gateway: &crate::Gateway,
    principal: &Principal,
    id: &ArtifactId,
    filename: Option<String>,
) -> Result<PlatformToolOutput, PlatformToolError> {
    let settings: ArtifactSettings = gateway
        .storage
        .settings()
        .get("artifact_settings")
        .await
        .map_err(|error| PlatformToolError::new(error.to_string()))?
        .map(|value| serde_json::from_str(&value))
        .transpose()
        .map_err(|error| PlatformToolError::new(format!("Invalid Artifact settings: {error}")))?
        .unwrap_or_default();
    let store = gateway
        .artifact_store()
        .ok_or_else(|| PlatformToolError::new("Artifact storage is unavailable"))?;
    let grant = store
        .download(principal, id, retention(gateway).await?, &settings)
        .await
        .map_err(artifact_error)?;
    Ok(output(
        json!({"artifact_reference":grant.artifact.reference(),"filename":filename.unwrap_or_else(||id.as_str().to_owned()),"artifact":grant.artifact,"download_url":grant.url,"expires_at":grant.expires_at}),
    ))
}
fn output(value: Value) -> PlatformToolOutput {
    PlatformToolOutput {
        content: vec![ContentBlock::Unknown { raw: value }],
        is_error: false,
        metadata: Default::default(),
    }
}

#[async_trait]
impl PlatformTool for ReadTool {
    fn id(&self) -> ToolId {
        ToolId::new(TOOL_ID)
    }
    fn external_name(&self) -> &str {
        TOOL_NAME
    }
    fn description(&self) -> Option<&str> {
        Some(DOWNLOAD_DESCRIPTION)
    }
    fn parameters(&self) -> Value {
        let mut schema = input_schema();
        // Strict 模型工具以 null 表示可选值；MCP 仍允许省略这些字段。
        schema["required"] = json!([
            "url",
            "previous_turn_id",
            "allowed_domains",
            "blocked_domains"
        ]);
        schema
    }
    fn parallel_safe(&self) -> bool {
        true
    }
    fn execution_limit(&self) -> Option<Duration> {
        Some(Duration::from_secs(15 * 60))
    }
    async fn execute(
        &self,
        arguments: Value,
        context: ToolExecutionContext,
    ) -> Result<Value, PlatformToolError> {
        let result = self.read(arguments, context).await?;
        let (value, _) = crate::hook::tool::blocks_to_value(result.content)?;
        Ok(value)
    }
    async fn execute_result(
        &self,
        arguments: Value,
        context: ToolExecutionContext,
    ) -> Result<PlatformToolOutput, PlatformToolError> {
        self.read(arguments, context).await
    }
}

#[async_trait]
impl McpTool for ReadTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }
    fn description(&self) -> Option<&str> {
        Some(DOWNLOAD_DESCRIPTION)
    }
    async fn description_for(&self, context: &McpContext) -> Option<String> {
        Some(
            self.read_description(
                self.capabilities(&Principal::new(context.api_key_id.clone()))
                    .await,
            ),
        )
    }
    fn input_schema(&self) -> Value {
        input_schema()
    }
    async fn input_schema_for(&self, context: &McpContext) -> Value {
        let mut schema = input_schema();
        schema["description"] = json!(description(
            self.capabilities(&Principal::new(context.api_key_id.clone()))
                .await
        ));
        schema
    }
    fn deadline(&self) -> Duration {
        Duration::from_secs(15 * 60)
    }
    fn await_cancellation_cleanup(&self) -> bool {
        true
    }
    async fn available(&self, context: &McpContext) -> Result<bool, McpToolError> {
        let Some(keys) = self.gateway.storage.api_keys() else {
            return Ok(false);
        };
        let key = keys
            .get(&context.api_key_id)
            .await
            .map_err(|error| McpToolError::new("mcp_access_check_failed", error.to_string()))?;
        if !key.is_some_and(|key| key.is_enabled && key.mcp_access_enabled) {
            return Ok(false);
        }
        Ok(
            crate::proxy::security::Security::new(self.gateway.storage.auth())
                .authorize_principal_capability(&Principal::new(context.api_key_id.clone()))
                .await
                .is_ok(),
        )
    }
    async fn call(
        &self,
        arguments: Value,
        context: &McpContext,
    ) -> Result<McpToolOutput, McpToolError> {
        let (cancellation, _) = context.execution().unwrap_or_else(|| {
            (
                CancellationToken::new(),
                Instant::now() + Duration::from_secs(900),
            )
        });
        let run_id = uuid::Uuid::new_v4().to_string();
        let context = ToolExecutionContext {
            request_id: run_id.clone(),
            run_id,
            principal: Principal::new(context.api_key_id.clone()),
            read_scope: ReadExposureScope::FULL,
            cancellation,
            progress: None,
        };
        match self.read(arguments, context).await {
            Ok(result) => {
                let (mut value, _) =
                    crate::hook::tool::blocks_to_value(result.content).map_err(|error| {
                        McpToolError::new("result_encoding_failed", error.to_string())
                    })?;
                if let Some(reference) = result.metadata.get("artifact_reference") {
                    value["artifact_reference"] = reference.clone();
                }
                Ok(if result.is_error {
                    McpToolOutput::execution_error(value)
                } else {
                    McpToolOutput::success(value)
                })
            }
            Err(error) => Ok(McpToolOutput::execution_error(
                json!({"error":{"code":"read_failed","message":error.to_string()}}),
            )),
        }
    }
}

struct WebPageTool {
    gateway: crate::Gateway,
}
#[async_trait]
impl PlatformTool for WebPageTool {
    fn id(&self) -> ToolId {
        ToolId::new("stravia-read-page")
    }
    fn external_name(&self) -> &str {
        TOOL_NAME
    }
    fn read_domain(&self) -> Option<StraviaReadDomain> {
        Some(StraviaReadDomain::WebPage)
    }
    fn description(&self) -> Option<&str> {
        Some("Read public webpages as Markdown with sources and extraction limitations.")
    }
    fn parameters(&self) -> Value {
        input_schema()
    }
    async fn execute(
        &self,
        arguments: Value,
        context: ToolExecutionContext,
    ) -> Result<Value, PlatformToolError> {
        let result = self.execute_result(arguments, context).await?;
        crate::hook::tool::blocks_to_value(result.content).map(|(value, _)| value)
    }
    async fn execute_result(
        &self,
        arguments: Value,
        mut context: ToolExecutionContext,
    ) -> Result<PlatformToolOutput, PlatformToolError> {
        context.run_id = uuid::Uuid::new_v4().to_string();
        let url = arguments
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| PlatformToolError::new("Missing webpage URL"))?;
        let service = self.gateway.web_access();
        before_execution(
            &context.cancellation,
            service.capture_run_snapshot(&context.run_id, context.principal.api_key_id()),
        )
        .await?
        .map_err(|error| PlatformToolError::new(error.to_string()))?;
        let result = fetch_page(&self.gateway, url.to_owned(), &context).await;
        service.release_run_snapshot(&context.run_id);
        result
    }
}
async fn fetch_page(
    gateway: &crate::Gateway,
    url: String,
    context: &ToolExecutionContext,
) -> Result<PlatformToolOutput, PlatformToolError> {
    let request = serde_json::from_value(json!({"urls":[url],"max_characters":50000}))
        .map_err(|error| PlatformToolError::new(error.to_string()))?;
    let result = before_execution(&context.cancellation, async {
        gateway
            .web_access()
            .fetch_in_run(&context.run_id, context.principal.api_key_id(), request)
            .await
    })
    .await?
    .map_err(|error| PlatformToolError::new(error.to_string()))?;
    let is_error = result.is_execution_error();
    let mut output = output(
        serde_json::to_value(result).map_err(|error| PlatformToolError::new(error.to_string()))?,
    );
    output.is_error = is_error;
    Ok(output)
}

pub(crate) async fn execute_internal_read(
    gateway: &crate::Gateway,
    url: String,
    mut context: ToolExecutionContext,
) -> Result<PlatformToolOutput, PlatformToolError> {
    context.read_scope = ReadExposureScope::FULL;
    let mut tools = crate::media::platform_tools(gateway);
    let mut router = ReadTool::new(gateway, &mut tools)
        .map_err(|error| PlatformToolError::new(error.to_string()))?;
    router.internal = true;
    router.read(json!({"url":url}), context).await
}

fn description(scope: ReadExposureScope) -> String {
    let mut text = DOWNLOAD_DESCRIPTION.to_owned();
    if scope.networking() {
        text.push('\n');
        text.push_str(NETWORK_DESCRIPTION);
    }
    if scope.media() {
        text.push('\n');
        text.push_str(MEDIA_DESCRIPTION);
    }
    text
}
pub(crate) struct ReadPlanningHook {
    pub(crate) tool: ReadTool,
}
impl Hook for ReadPlanningHook {
    fn descriptor(&self) -> HookDescriptor {
        HookDescriptor {
            id: HookId::new("stravia-read"),
            request_kinds: vec![RequestKind::Generation],
            event_kinds: vec![EventKind::Request],
            requires_full_context: false,
            max_buffered_bytes: 0,
            max_delayed_events: 0,
        }
    }
    fn create_session(&self, context: &SessionContext) -> Box<dyn HookSession> {
        Box::new(ReadPlanningSession {
            tool: self.tool.clone(),
            principal: context.principal.clone(),
            resolved: context.tools_fixed,
        })
    }
}
struct ReadPlanningSession {
    tool: ReadTool,
    principal: Principal,
    resolved: bool,
}
#[async_trait]
impl HookSession for ReadPlanningSession {
    async fn handle(&mut self, event: HookEvent<'_>) -> Result<ActionBatch, String> {
        let HookEvent::Request { read_scope, .. } = event else {
            return Ok(ActionBatch::default());
        };
        if self.resolved {
            return Ok(ActionBatch::default());
        }
        self.resolved = true;
        let enabled = self.tool.capabilities(&self.principal).await;
        let explicit = read_scope == ReadExposureScope::FULL;
        let security = crate::proxy::security::Security::new(self.tool.gateway.storage.auth());
        let networking = enabled.networking()
            && security
                .authorize_principal_web_search(&self.principal)
                .await
                .is_ok_and(|access| access.transparent_injection_enabled);
        let media = enabled.media()
            && security
                .media_transparent_injection_enabled(&self.principal)
                .await
                .unwrap_or(false);
        let scope = if explicit {
            ReadExposureScope::FULL
        } else {
            ReadExposureScope::new(networking, media)
        };
        if !explicit && !networking && !media {
            return Ok(ActionBatch::default());
        }
        let visible = if explicit { enabled } else { scope };
        Ok(ActionBatch::one(HookAction::ExposeRead {
            scope,
            description: self.tool.read_description(visible),
        }))
    }
}
