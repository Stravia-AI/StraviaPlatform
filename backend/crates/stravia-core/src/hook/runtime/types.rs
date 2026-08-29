use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HookId(String);

impl HookId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HookId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RequestKind {
    Generation,
    Embeddings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    Request,
    UpstreamResponse,
    ToolResult,
    ClientOutput,
    Stream,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransportKind {
    Http,
    WebSocket,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Principal {
    api_key_id: String,
}

impl Principal {
    pub(crate) fn new(api_key_id: impl Into<String>) -> Self {
        let api_key_id = api_key_id.into();
        assert!(
            !api_key_id.is_empty() && api_key_id != "anonymous",
            "Principal requires an authenticated API key identity"
        );
        Self { api_key_id }
    }

    pub fn api_key_id(&self) -> &str {
        &self.api_key_id
    }

    pub(crate) fn continuation_key(&self) -> String {
        format!("api-key:{}", self.api_key_id)
    }
}

#[derive(Debug, Clone)]
pub struct HookDescriptor {
    pub id: HookId,
    pub request_kinds: Vec<RequestKind>,
    pub event_kinds: Vec<EventKind>,
    pub requires_full_context: bool,
    pub max_buffered_bytes: usize,
    pub max_delayed_events: usize,
}

impl HookDescriptor {
    pub fn all(id: impl Into<String>) -> Self {
        Self {
            id: HookId::new(id),
            request_kinds: vec![RequestKind::Generation, RequestKind::Embeddings],
            event_kinds: vec![
                EventKind::Request,
                EventKind::UpstreamResponse,
                EventKind::ToolResult,
                EventKind::ClientOutput,
                EventKind::Stream,
            ],
            requires_full_context: false,
            max_buffered_bytes: 64 * 1024,
            max_delayed_events: 64,
        }
    }

    pub(super) fn accepts(&self, request_kind: RequestKind, event_kind: EventKind) -> bool {
        self.request_kinds.contains(&request_kind) && self.event_kinds.contains(&event_kind)
    }
}

#[derive(Debug, Clone)]
pub struct SessionContext {
    pub request_id: String,
    pub run_id: String,
    pub request_kind: RequestKind,
    pub ingress: ProtocolId,
    pub transport: TransportKind,
    pub principal: Principal,
    pub cancellation: crate::proxy::context::CancellationToken,
    pub inherited_media_turns: Vec<(usize, Vec<String>)>,
    pub response_id: Option<String>,
    pub previous_response_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RouteContext {
    pub model_id: String,
    pub provider_id: String,
    pub target_id: String,
    pub egress: ProtocolId,
}
pub enum HookEvent<'a> {
    Request {
        session: &'a SessionContext,
        original: &'a ContextSnapshot,
        current: &'a AiRequest,
        context: &'a ContextSnapshot,
        route: Option<&'a RouteContext>,
        round: u32,
    },
    UpstreamResponse {
        session: &'a SessionContext,
        request: &'a AiRequest,
        response: &'a AiResponse,
        classified: &'a ClassifiedToolCalls,
        route: &'a RouteContext,
        round: u32,
    },
    ToolResult {
        session: &'a SessionContext,
        result: &'a PlatformToolResult,
        route: &'a RouteContext,
        round: u32,
    },
    ClientOutput {
        session: &'a SessionContext,
        response: &'a AiResponse,
        route: &'a RouteContext,
        round: u32,
    },
}

impl HookEvent<'_> {
    pub fn kind(&self) -> EventKind {
        match self {
            Self::Request { .. } => EventKind::Request,
            Self::UpstreamResponse { .. } => EventKind::UpstreamResponse,
            Self::ToolResult { .. } => EventKind::ToolResult,
            Self::ClientOutput { .. } => EventKind::ClientOutput,
        }
    }
}

#[derive(Debug, Clone)]
pub enum RequestPatch {
    ReplaceCanonical(Box<AiRequest>),
    SetModel(String),
    SetSystem(Option<String>),
    SetGeneration(GenerationConfig),
    ReplaceTools(Option<Vec<ToolSpec>>),
    SetToolChoice(Option<ToolChoice>),
    SetEmbeddingInput(EmbeddingInput),
    SetProtocolExtension(Option<Box<ProtocolExt>>),
    ReplaceContextSpans(Vec<ReplaceContextSpan>),
}

#[derive(Debug, Clone)]
pub enum ResponsePatch {
    ReplaceCanonical(Box<AiResponse>),
    SetContent(String),
    SetReasoning(Option<String>),
    ReplaceItems(Vec<AiItem>),
    SetEmbeddingOutput(crate::protocol::ir::response::EmbeddingOutput),
    SetToolArguments { call_id: String, arguments: String },
}

#[derive(Debug, Clone)]
pub enum ToolResultPatch {
    SetContent(serde_json::Value),
    SetError(bool),
    SetMetadata(serde_json::Map<String, serde_json::Value>),
}

#[derive(Debug, Clone)]
pub struct HookRejection {
    pub status: u16,
    pub code: String,
    pub message: String,
}
#[derive(Debug, Clone)]
pub enum HookAction {
    PatchRequest(Box<RequestPatch>),
    PatchResponse(ResponsePatch),
    PatchToolResult(ToolResultPatch),
    ExposeTool(ToolId),
    Respond(Box<AiResponse>),
    Reject(HookRejection),
    StreamAbort { message: String },
}

#[derive(Debug, Clone, Default)]
pub struct ActionBatch {
    pub actions: Vec<HookAction>,
}

impl ActionBatch {
    pub fn one(action: HookAction) -> Self {
        Self {
            actions: vec![action],
        }
    }
}

#[derive(Debug, Clone)]
pub enum HookControl {
    Continue,
    Respond(Box<AiResponse>),
    Reject(HookRejection),
    StreamAbort { message: String },
}

pub(crate) struct ResponseHookOutcome {
    pub(crate) control: HookControl,
    pub(crate) modified: bool,
}
#[derive(Debug, Clone)]
pub struct PlatformToolCall {
    pub tool_id: ToolId,
    pub call: ToolCall,
}

#[derive(Debug, Clone, Default)]
pub struct ClassifiedToolCalls {
    pub platform: Vec<PlatformToolCall>,
    pub client: Vec<ToolCall>,
}

#[derive(Debug, thiserror::Error)]
pub enum HookError {
    #[error("hook {hook_id} failed: {message}")]
    Failed { hook_id: HookId, message: String },
    #[error("hook {hook_id} cancelled")]
    Cancelled { hook_id: HookId },
    #[error("hook {hook_id} returned an action invalid for {event:?}: {message}")]
    InvalidAction {
        hook_id: HookId,
        event: EventKind,
        message: String,
    },
    #[error("hook runtime state is invalid: {message}")]
    Runtime { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookSkip {
    pub hook_id: HookId,
    pub event: EventKind,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SemanticVariant {
    Text,
    Refusal,
    Thinking,
    ReasoningSummary,
    ToolCall(usize),
}

#[async_trait]
pub trait HookSession: Send {
    async fn handle(&mut self, event: HookEvent<'_>) -> Result<ActionBatch, String>;

    fn stream_transformer(&mut self) -> Option<&mut dyn StreamTransformer> {
        None
    }

    fn requires_terminal_buffering(&self) -> bool {
        true
    }
}

pub trait Hook: Send + Sync + 'static {
    fn descriptor(&self) -> HookDescriptor;
    fn create_session(&self, context: &SessionContext) -> Box<dyn HookSession>;
}

#[derive(Clone, Default)]
pub struct HookRuntime {
    pub(super) hooks: Arc<[Arc<dyn Hook>]>,
    pub(super) tools: PlatformToolRegistry,
}
