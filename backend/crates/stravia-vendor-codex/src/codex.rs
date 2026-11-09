use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use stravia_runtime_contract::protocol::{
    ids::OPEN_RESPONSES_2026_04_24,
    ir::{AiResponse, ServerToolUsage, Usage},
};
use stravia_vendor_sdk::{
    GuestHost, ProviderSnapshot, SearchRequest, SearchResponse, SearchSource, WsMessage,
};

use super::auth::account_id_from_jwt;
use super::{
    CLIENT_VERSION, configured_websocket_url, endpoint, ensure_success, invalid,
    require_codex_protocol, required_model, retryable, secret,
};

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
// Retire reusable sessions before the upstream's one-hour connection limit.
const WEBSOCKET_REUSE_MAX_AGE_MILLIS: u64 = 55 * 60 * 1000;
/// Interactive official client identity (rust-v0.167.0
/// login/src/auth/default_client.rs `get_codex_user_agent`). A Wasm guest
/// cannot observe the host OS or terminal, so the platform segment uses the
/// official fallbacks for an unidentified Linux release (`os_info` Unknown
/// version) and an undetected terminal ("unknown").
const ORIGINATOR: &str = "codex-tui";
const USER_AGENT_PLATFORM: &str = "(Linux Unknown; x86_64) unknown";
/// core/src/session/mod.rs `build_model_client_beta_features_header` always
/// advertises remote compaction; the guest enables no experimental features.
const BETA_FEATURES: &str = "remote_compaction_v2";
const TURN_METADATA_KEY: &str = "x-codex-turn-metadata";
/// core/src/client.rs: Responses Lite is a header on HTTP and a reserved
/// client_metadata key on WebSocket frames.
const RESPONSES_LITE_HEADER: &str = "x-openai-internal-codex-responses-lite";
const RESPONSES_LITE_WS_METADATA_KEY: &str =
    "ws_request_header_x_openai_internal_codex_responses_lite";
const WS_REQUEST_START_METADATA_KEY: &str = "x-codex-ws-stream-request-start-ms";
/// Only infrastructure cookies; authentication/session cookies are never admitted.
const COOKIE_POLICY: &str = r#"{"domains":["chat.openai.com","chatgpt.com","chatgpt-staging.com"],"names":["__cf_bm","__cflb","__cfruid","__cfseq","__cfwaitingroom","__oailb","_cfuvid","cf_clearance","cf_ob_info","cf_use_ob"],"prefixes":["cf_chl_"]}"#;
/// tools/src/tool_name.rs `DEFAULT_FUNCTION_NAMESPACE`.
const FUNCTION_NAMESPACE: &str = "functions";

/// One identity projection shared by the HTTP body, HTTP headers and WS handshake.
/// Only the host-scoped affinity may select a reusable native session.
pub(super) struct NativeSession {
    operation: BodyOperation,
    installation_id: Option<String>,
    /// The official client uses one UUID as both session and thread identity.
    thread_id: String,
    window_id: String,
    window_number: Option<u64>,
    turn_id: String,
    context_window_id: String,
    parent_thread_id: Option<String>,
    forked_from_thread_id: Option<String>,
    turn_started_at_unix_ms: i64,
    model: ModelProfile,
    pub(super) history_boundaries: Option<super::lite_history::BoundaryOffsets>,
}

/// Request-shaping facts from the official `/models` catalog row, which
/// discovery stores as model metadata extensions.
struct ModelProfile {
    responses_lite: bool,
    support_verbosity: Option<bool>,
    default_verbosity: Option<String>,
    default_reasoning_level: Option<String>,
    node_repl_auto_review_required: bool,
    node_repl_disabled: bool,
    incremental_tool_messages: Option<Value>,
}

impl ModelProfile {
    fn new(provider: &ProviderSnapshot, operation: BodyOperation) -> Self {
        Self {
            // Only native inference uses the Lite wire; the plugin-constructed
            // search and image requests keep the public Responses shape.
            responses_lite: operation.is_inference()
                && model_metadata(provider, "use_responses_lite").and_then(Value::as_bool)
                    == Some(true),
            support_verbosity: model_metadata(provider, "support_verbosity")
                .and_then(Value::as_bool),
            default_verbosity: model_text(provider, "default_verbosity"),
            default_reasoning_level: model_text(provider, "default_reasoning_level"),
            node_repl_auto_review_required: model_metadata(
                provider,
                "node_repl_auto_review_required",
            )
            .and_then(Value::as_bool)
                == Some(true),
            node_repl_disabled: model_metadata(provider, "node_repl_disabled")
                .and_then(Value::as_bool)
                == Some(true),
            incremental_tool_messages: model_metadata(provider, "model_messages")
                .and_then(|messages| messages.get("incremental_tools"))
                .cloned(),
        }
    }
}

impl NativeSession {
    pub(super) fn uses_lite_history(&self) -> bool {
        self.model.responses_lite
    }
    pub(super) fn history_window(&self) -> &str {
        &self.context_window_id
    }
    pub(super) fn history_turn(&self) -> &str {
        &self.turn_id
    }
    pub(super) fn incremental_message<'a>(&'a self, key: &str, default: &'a str) -> &'a str {
        self.model
            .incremental_tool_messages
            .as_ref()
            .and_then(|messages| messages.get(key))
            .and_then(Value::as_str)
            .filter(|text| text.len() <= 512)
            .unwrap_or(default)
    }

    pub(super) fn new(provider: &ProviderSnapshot, operation: BodyOperation) -> Self {
        let thread_id = scoped_affinity(provider, "client_conversation_id")
            .map(|id| native_uuid("openai-codex:conversation", id))
            .or_else(|| {
                scoped_affinity(provider, "transport_affinity")
                    .map(|affinity| native_uuid("openai-codex", affinity))
            })
            .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
        let window_number = provider
            .operation_metadata
            .get("context_window_index")
            .and_then(Value::as_u64);
        let window_id = window_number
            .map(|number| format!("{thread_id}:{number}"))
            .unwrap_or_default();
        let context_window_id = scoped_affinity(provider, "context_window_id")
            .unwrap_or_default()
            .to_owned();
        Self {
            operation,
            installation_id: None,
            // A window number is supplied only from a proven host checkpoint.
            window_id,
            window_number,
            // Canonical client turn/request labels are not host-scoped operation
            // identities. A newly constructed native turn remains independent.
            turn_id: uuid::Uuid::now_v7().to_string(),
            context_window_id,
            parent_thread_id: None,
            forked_from_thread_id: None,
            turn_started_at_unix_ms: chrono::Utc::now().timestamp_millis(),
            model: ModelProfile::new(provider, operation),
            history_boundaries: None,
            thread_id,
        }
    }

    /// Projects a Responses body onto the official rust-v0.167.0
    /// `ResponsesApiRequest` / `ResponseCreateWsRequest` (core/src/client.rs
    /// `build_responses_request`) and attaches this session's identity.
    pub(super) fn project_body(
        &self,
        body: &mut Value,
    ) -> Result<(), stravia_vendor_sdk::PluginError> {
        let instructions = body
            .as_object_mut()
            .ok_or_else(|| invalid("Codex Responses request body must be an object"))?
            .remove("instructions");
        prepare_operation_body(body, self.operation)?;
        let object = body
            .as_object_mut()
            .expect("projected Codex body is an object");
        // The official request input is always an item list.
        if let Some(Value::String(text)) = object.get_mut("input") {
            let text = std::mem::take(text);
            object.insert(
                "input".into(),
                json!([{
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": text}],
                }]),
            );
        }
        if self.operation.is_inference() {
            self.apply_model_defaults(object);
        }
        let tools = if self.model.responses_lite {
            object.insert("parallel_tool_calls".into(), Value::Bool(false));
            if let Some(reasoning) = object
                .entry("reasoning")
                .or_insert_with(|| json!({}))
                .as_object_mut()
            {
                reasoning.insert("context".into(), "all_turns".into());
            }
            object.remove("tools")
        } else {
            None
        };
        // A WS continuation resumes a response whose prompt already carried the
        // prefix; the official client sends only the newly appended items.
        if self.model.responses_lite || request_continuation_id(body).is_none() {
            self.prepend_prompt_prefix(body, tools, instructions)?;
        }
        let object = body
            .as_object_mut()
            .expect("projected Codex body is an object");
        if !object.contains_key("prompt_cache_key") {
            // Official `PromptCacheKey::SessionDerived`.
            object.insert("prompt_cache_key".into(), self.thread_id.clone().into());
        }
        self.apply_client_metadata(body)
    }

    /// Official model defaults fill controls the caller left unset.
    fn apply_model_defaults(&self, object: &mut serde_json::Map<String, Value>) {
        object.entry("tool_choice").or_insert_with(|| "auto".into());
        object.insert("include".into(), json!(["reasoning.encrypted_content"]));
        if let Some(effort) = &self.model.default_reasoning_level
            && let Some(reasoning) = object
                .entry("reasoning")
                .or_insert_with(|| json!({}))
                .as_object_mut()
        {
            reasoning
                .entry("effort")
                .or_insert_with(|| effort.clone().into());
        }
        match self.model.support_verbosity {
            Some(true) => {
                if let Some(verbosity) = &self.model.default_verbosity
                    && let Some(text) = object
                        .entry("text")
                        .or_insert_with(|| json!({}))
                        .as_object_mut()
                {
                    text.entry("verbosity")
                        .or_insert_with(|| verbosity.clone().into());
                }
            }
            // The official client drops verbosity for models that reject it.
            Some(false) => {
                if let Some(text) = object.get_mut("text").and_then(Value::as_object_mut) {
                    text.remove("verbosity");
                    if text.is_empty() {
                        object.remove("text");
                    }
                }
            }
            None => {}
        }
    }

    /// Responses Lite records incremental tool and base-instruction items in
    /// history; `stabilize_lite_history` retains their first-record identities.
    /// Other models use request-time UUIDv5 items without turn or time stamps.
    fn prepend_prompt_prefix(
        &self,
        body: &mut Value,
        tools: Option<Value>,
        instructions: Option<Value>,
    ) -> Result<(), stravia_vendor_sdk::PluginError> {
        let mut prefix = Vec::with_capacity(2);
        // Only Responses Lite moves tool declarations into the prefix.
        if let Some(Value::Array(tools)) = tools
            && !tools.is_empty()
        {
            let tools = responses_lite_tools(tools);
            prefix.push(json!({
                "type": "additional_tools",
                "id": format!("at_{}", uuid::Uuid::now_v7()),
                "role": "developer",
                "tools": tools,
            }));
        }
        if let Some(Value::String(text)) = instructions
            && !text.is_empty()
        {
            let (id, passthrough) = if self.model.responses_lite {
                (
                    uuid::Uuid::now_v7().to_string(),
                    json!({
                        "turn_id": self.turn_id,
                        "create_time": response_item_create_time(),
                        "content_item_kinds": ["model.base_instructions"],
                        // Only Responses Lite accepts per-content attribution.
                        "content_item_metadata": [{"provenance": {"type": "harness", "hook": false}}],
                    }),
                )
            } else {
                (
                    request_prefix_uuid(&self.thread_id, text.as_bytes()),
                    json!({"content_item_kinds": ["model.base_instructions"]}),
                )
            };
            prefix.push(json!({
                "type": "message",
                "id": format!("msg_{id}"),
                "role": "developer",
                "content": [{"type": "input_text", "text": text}],
                "internal_chat_message_metadata_passthrough": passthrough,
            }));
        }
        if prefix.is_empty() {
            return Ok(());
        }
        body.get_mut("input")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| invalid("Codex Responses input must be an item list"))?
            .splice(0..0, prefix);
        Ok(())
    }

    /// core/src/responses_metadata.rs `CodexResponsesMetadata::client_metadata`
    /// plus core/src/client.rs guardian credits. The turn snapshot records the
    /// body's final model and reasoning.
    fn apply_client_metadata(
        &self,
        body: &mut Value,
    ) -> Result<(), stravia_vendor_sdk::PluginError> {
        let turn_metadata = self.turn_metadata(body)?;
        let mut metadata = serde_json::Map::new();
        if let Some(installation_id) = &self.installation_id {
            metadata.insert(
                "x-codex-installation-id".into(),
                installation_id.clone().into(),
            );
        }
        metadata.insert("session_id".into(), self.thread_id.clone().into());
        metadata.insert("thread_id".into(), self.thread_id.clone().into());
        metadata.insert("x-codex-window-id".into(), self.window_id.clone().into());
        metadata.insert("turn_id".into(), self.turn_id.clone().into());
        if let Some(parent_thread_id) = &self.parent_thread_id {
            metadata.insert(
                "x-codex-parent-thread-id".into(),
                parent_thread_id.clone().into(),
            );
        }
        if let Some(root_turn_id) = self.root_turn_id() {
            metadata.insert("root_turn_id".into(), root_turn_id.into());
        }
        metadata.insert(TURN_METADATA_KEY.into(), turn_metadata.into());
        // No MCP tool runs inside the guest, so attribution is always empty.
        metadata.insert("mcp_attribution".into(), r#"{"status":"none"}"#.into());
        metadata.insert("guardian_credits_requested".into(), "true".into());
        body["client_metadata"] = Value::Object(metadata);
        Ok(())
    }

    /// A root thread's turn is its own causal root; a child thread's root turn
    /// belongs to the parent and is not observable here.
    fn root_turn_id(&self) -> Option<&str> {
        self.parent_thread_id
            .is_none()
            .then_some(self.turn_id.as_str())
    }

    fn turn_metadata(&self, body: &Value) -> Result<String, stravia_vendor_sdk::PluginError> {
        let turn = self.operation != BodyOperation::Compaction;
        let payload = TurnMetadataPayload {
            installation_id: self.installation_id.as_deref(),
            session_id: &self.thread_id,
            thread_id: &self.thread_id,
            agent_name: None,
            turn_id: &self.turn_id,
            window_id: &self.window_id,
            window_number: self.window_number,
            context_window_id: &self.context_window_id,
            request_kind: if turn { "turn" } else { "compaction" },
            history_initialization: None,
            forked_from_thread_id: self.forked_from_thread_id.as_deref(),
            parent_thread_id: self.parent_thread_id.as_deref(),
            root_turn_id: self.root_turn_id(),
            thread_source: None,
            turn_trigger: None,
            // No sandbox or human trigger facts are inferred from CLI defaults.
            sandbox: None,
            sandbox_mode: None,
            auto_review_enabled: false,
            node_repl_auto_review_required: self.model.node_repl_auto_review_required,
            node_repl_disabled: self.model.node_repl_disabled,
            turn_started_at_unix_ms: self.turn_started_at_unix_ms,
            analytics_enabled: false,
            // The host does not identify the upstream's manual/automatic trigger.
            compaction: None,
            model: turn
                .then(|| body.get("model").and_then(Value::as_str))
                .flatten(),
            reasoning_effort: turn
                .then(|| body.pointer("/reasoning/effort").and_then(Value::as_str))
                .flatten(),
        };
        serde_json::to_string(&payload)
            .map_err(|_| invalid("Codex native turn metadata could not be encoded"))
    }

    pub(super) fn resolve_identity(
        &mut self,
        _host: &GuestHost,
        provider: &ProviderSnapshot,
    ) -> Result<(), stravia_vendor_sdk::PluginError> {
        self.apply_host_identity(provider)
    }

    fn apply_host_identity(
        &mut self,
        provider: &ProviderSnapshot,
    ) -> Result<(), stravia_vendor_sdk::PluginError> {
        let installation = scoped_affinity(provider, "client_installation_id")
            .ok_or_else(|| invalid("Codex installation identity is unavailable"))?;
        uuid::Uuid::parse_str(installation)
            .map_err(|_| invalid("Codex installation identity is invalid"))?;
        self.installation_id = Some(installation.to_owned());
        let window = scoped_affinity(provider, "context_window_id")
            .ok_or_else(|| invalid("Codex context window identity is unavailable"))?;
        uuid::Uuid::parse_str(window)
            .map_err(|_| invalid("Codex context window identity is invalid"))?;
        let number = provider
            .operation_metadata
            .get("context_window_index")
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid("Codex context window ordinal is unavailable"))?;
        self.context_window_id = window.to_owned();
        self.window_number = Some(number);
        self.window_id = format!("{}:{number}", self.thread_id);
        let prompt_id = scoped_affinity(provider, "logical_turn_id")
            .ok_or_else(|| invalid("Codex logical turn identity is unavailable"))?;
        uuid::Uuid::parse_str(prompt_id)
            .map_err(|_| invalid("Codex logical turn identity is invalid"))?;
        let started = provider
            .operation_metadata
            .get("logical_turn_started_at_ms")
            .and_then(Value::as_i64)
            .ok_or_else(|| invalid("Codex logical turn start is unavailable"))?;
        self.turn_id = native_uuid("openai-codex:logical-turn", prompt_id);
        self.turn_started_at_unix_ms = started;
        Ok(())
    }
}

/// Field order is the official `CodexTurnMetadataPayload` declaration order
/// (core/src/responses_metadata.rs); `model` and `reasoning_effort` are the
/// sorted flattened execution extras that follow it.
#[derive(Serialize)]
struct TurnMetadataPayload<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    installation_id: Option<&'a str>,
    session_id: &'a str,
    thread_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_name: Option<&'static str>,
    turn_id: &'a str,
    window_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    window_number: Option<u64>,
    context_window_id: &'a str,
    request_kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    history_initialization: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    forked_from_thread_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_thread_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    root_turn_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thread_source: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    turn_trigger: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sandbox: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sandbox_mode: Option<&'static str>,
    auto_review_enabled: bool,
    node_repl_auto_review_required: bool,
    node_repl_disabled: bool,
    turn_started_at_unix_ms: i64,
    analytics_enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    compaction: Option<CompactionMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'a str>,
}

#[derive(Serialize)]
struct CompactionMetadata {
    trigger: &'static str,
    reason: &'static str,
    implementation: &'static str,
    phase: &'static str,
    strategy: &'static str,
}

fn model_metadata<'a>(provider: &'a ProviderSnapshot, key: &str) -> Option<&'a Value> {
    provider.model_metadata.as_ref()?.extensions.get(key)
}

fn model_text(provider: &ProviderSnapshot, key: &str) -> Option<String> {
    model_metadata(provider, key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// tools/src/tool_spec.rs `create_tools_json_for_responses_lite`: function and
/// freeform tools join the default `functions` namespace at the position of
/// the first such tool; every other declaration is kept as is.
fn responses_lite_tools(tools: Vec<Value>) -> Vec<Value> {
    let mut functions = Vec::new();
    let mut description = String::new();
    let mut functions_index = None;
    let mut declarations = Vec::with_capacity(tools.len());
    for mut tool in tools {
        match tool.get("type").and_then(Value::as_str) {
            Some("function" | "custom") => functions.push(tool),
            Some("namespace")
                if tool.get("name").and_then(Value::as_str) == Some(FUNCTION_NAMESPACE) =>
            {
                if let Some(text) = tool
                    .get("description")
                    .and_then(Value::as_str)
                    .filter(|text| !text.trim().is_empty())
                {
                    description = text.to_owned();
                }
                if let Some(Value::Array(children)) = tool.get_mut("tools") {
                    functions.append(children);
                }
            }
            _ => {
                declarations.push(tool);
                continue;
            }
        }
        functions_index.get_or_insert(declarations.len());
    }
    if let Some(index) = functions_index
        && !functions.is_empty()
    {
        declarations.insert(
            index,
            json!({
                "type": "namespace",
                "name": FUNCTION_NAMESPACE,
                "description": description,
                "tools": functions,
            }),
        );
    }
    declarations
}

/// rust-v0.167.0 login/src/auth/default_client.rs `get_codex_user_agent`.
pub(super) fn user_agent() -> String {
    format!("{ORIGINATOR}/{CLIENT_VERSION} {USER_AGENT_PLATFORM} ({ORIGINATOR}; {CLIENT_VERSION})")
}

fn scoped_affinity<'a>(provider: &'a ProviderSnapshot, key: &str) -> Option<&'a str> {
    provider
        .operation_metadata
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

fn native_uuid(namespace: &str, affinity: &str) -> String {
    derived_uuid(&[namespace.as_bytes(), affinity.as_bytes()])
}

/// UUIDv7 bit representation only: hash-derived bits are not a wall clock and
/// do not preserve official UUIDv7 creation-time ordering. Parts are
/// NUL-separated so two-part derivations keep their existing values.
fn derived_uuid(parts: &[&[u8]]) -> String {
    let mut hash = Sha256::new();
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            hash.update([0]);
        }
        hash.update(part);
    }
    let digest = hash.finalize();
    let mut bytes: [u8; 16] = digest[..16].try_into().expect("digest has 16 bytes");
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}

/// core/src/client.rs request-time prefix: UUIDv5 of the payload in a
/// namespace that is itself the UUIDv5 of the thread ID under `NAMESPACE_OID`.
fn request_prefix_uuid(thread_id: &str, payload: &[u8]) -> String {
    let namespace = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, thread_id.as_bytes());
    uuid::Uuid::new_v5(&namespace, payload).to_string()
}

/// core/src/session/mod.rs `response_item_create_time`: fractional Unix
/// seconds as a JSON number, integral seconds if not representable.
fn response_item_create_time() -> Value {
    let now = chrono::Utc::now();
    let seconds = now.timestamp() as f64 + f64::from(now.timestamp_subsec_nanos()) / 1e9;
    serde_json::Number::from_f64(seconds).map_or_else(|| now.timestamp().into(), Value::Number)
}

fn credential_key(provider: &ProviderSnapshot) -> Result<String, stravia_vendor_sdk::PluginError> {
    use base64::Engine;
    use sha2::{Digest, Sha256};
    let token = secret(provider, "access_token")?;
    // 账号身份跨 OAuth 刷新保持稳定；没有账号声明的凭据只按自身散列隔离。
    let account = provider
        .credentials
        .get("account_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| account_id_from_jwt(token));
    let identity = account.as_deref().unwrap_or(token);
    Ok(
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(identity.as_bytes())),
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum BodyOperation {
    Http,
    WebSocket,
    /// Standalone remote compaction over the HTTP Responses wire.
    Compaction,
    Search,
    Image,
}

impl BodyOperation {
    /// Requests the official client itself sends from a conversation turn.
    fn is_inference(self) -> bool {
        matches!(self, Self::Http | Self::WebSocket | Self::Compaction)
    }
}

pub(super) fn encode_tools(
    request: &stravia_runtime_contract::protocol::ir::AiRequest,
    operation: BodyOperation,
) -> Vec<Value> {
    use stravia_runtime_contract::protocol::ir::{NativeTool, native_tool_to_responses};
    let functions = request.tools.as_deref().unwrap_or_default();
    let mut tools = Vec::with_capacity(functions.len() + request.parameters.native_tools.len());
    for tool in functions {
        let mut encoded =
            json!({"type":"function", "name":tool.name, "parameters":tool.parameters});
        if let Some(description) = &tool.description {
            encoded["description"] = description.clone().into();
        }
        if let Some(strict) = tool.strict {
            encoded["strict"] = strict.into();
        }
        if let Some(defer_loading) = tool.defer_loading {
            encoded["defer_loading"] = defer_loading.into();
        }
        tools.push(encoded);
    }
    for tool in &request.parameters.native_tools {
        if matches!(tool, NativeTool::CodeExecution(_))
            || (matches!(tool, NativeTool::ImageGeneration(_)) && operation != BodyOperation::Image)
        {
            continue;
        }
        if let Some(mut encoded) = native_tool_to_responses(tool)
            && restrict_native_tool(&mut encoded, operation, false)
        {
            tools.push(encoded);
        }
    }
    tools
}

// Closed wire contract in declaration order: rust-v0.167.0
// codex-api/src/common.rs `ResponseCreateWsRequest` after its `type` tag;
// `ResponsesApiRequest` is the same sequence without the WS-only fields. The
// official client keeps routing fields ahead of the potentially multi-megabyte
// input so gateways can inspect them incrementally. That release has no
// top-level `instructions`; NativeSession::project_body moves them into the
// input prefix.
const WIRE_FIELDS: &[&str] = &[
    "type",
    "model",
    "stream",
    "service_tier",
    "previous_response_id",
    "input",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "reasoning",
    "store",
    "stream_options",
    "include",
    "prompt_cache_key",
    "text",
    "generate",
    "client_metadata",
    "access_programs",
];
const WS_ONLY_FIELDS: &[&str] = &["previous_response_id", "generate"];
// Plugin-owned or unsupported: never forwarded from the caller. Client
// metadata has one authority, NativeSession::apply_client_metadata.
const SESSION_FIELDS: &[&str] = &[
    "type",
    "stream_options",
    "client_metadata",
    "access_programs",
];

/// A projected body serialized in the official field order. A `Value` map
/// sorts its keys, which would put `input` ahead of `model`.
pub(super) struct WireBody<'a>(pub(super) &'a Value);

impl Serialize for WireBody<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{Error, SerializeMap};
        let object = self
            .0
            .as_object()
            .ok_or_else(|| S::Error::custom("Codex request body must be an object"))?;
        if let Some(key) = object
            .keys()
            .find(|key| !WIRE_FIELDS.contains(&key.as_str()))
        {
            return Err(S::Error::custom(format!(
                "Codex request field {key} is outside the official wire contract"
            )));
        }
        let mut map = serializer.serialize_map(Some(object.len()))?;
        for key in WIRE_FIELDS {
            if let Some(value) = object.get(*key) {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

/// Closed-field projection only; NativeSession::project_body owns the rest of
/// the official request shape.
pub(super) fn prepare_operation_body(
    body: &mut Value,
    operation: BodyOperation,
) -> Result<(), stravia_vendor_sdk::PluginError> {
    let object = body
        .as_object_mut()
        .ok_or_else(|| invalid("Codex Responses request body must be an object"))?;
    object.retain(|key, value| {
        let key = key.as_str();
        !value.is_null()
            && WIRE_FIELDS.contains(&key)
            && !SESSION_FIELDS.contains(&key)
            && (operation == BodyOperation::WebSocket || !WS_ONLY_FIELDS.contains(&key))
    });
    object.insert("store".into(), Value::Bool(false));
    // The native endpoint rejects stream:false. Client delivery remains
    // host-owned; even a unary client request uses the official SSE wire.
    object.insert("stream".into(), Value::Bool(true));
    // Filter control envelopes, never user input, tool schemas, grammars or
    // image/search business data.
    retain_control_fields(object.get_mut("reasoning"), &["effort", "summary"]);
    if let Some(reasoning) = object.get_mut("reasoning").and_then(Value::as_object_mut)
        && reasoning.get("summary").and_then(Value::as_str) == Some("auto")
    {
        reasoning.remove("summary");
        if reasoning.is_empty() {
            object.remove("reasoning");
        }
    }
    if let Some(text) = object.get_mut("text").and_then(Value::as_object_mut) {
        text.retain(|key, _| matches!(key.as_str(), "verbosity" | "format"));
        retain_control_fields(
            text.get_mut("format"),
            &["type", "strict", "schema", "name"],
        );
    }
    let dropped_all_tools = if let Some(Value::Array(tools)) = object.get_mut("tools") {
        let had_tools = !tools.is_empty();
        tools.retain_mut(|tool| restrict_native_tool(tool, operation, false));
        had_tools && tools.is_empty()
    } else {
        false
    };
    if dropped_all_tools {
        object.remove("tool_choice");
    }
    if let Some(choice) = object.get_mut("tool_choice") {
        let supported = match choice {
            Value::String(choice) => matches!(choice.as_str(), "auto" | "none" | "required"),
            Value::Object(choice) => {
                let tool_type = choice.get("type").and_then(Value::as_str);
                let supported = matches!(
                    (operation, tool_type),
                    (_, Some("web_search" | "tool_search"))
                        | (BodyOperation::Image, Some("image_generation"))
                );
                if supported {
                    choice.retain(|key, _| key == "type");
                }
                supported
            }
            _ => false,
        };
        if !supported {
            object.remove("tool_choice");
        }
    }
    if object.get("service_tier").and_then(Value::as_str) == Some("auto") {
        object.remove("service_tier");
    }
    Ok(())
}

// https://github.com/openai/codex/blob/main/codex-rs/tools/src/tool_spec.rs
// Official codex-tools ToolSpec/ResponsesApiNamespaceTool. Auxiliary hosted
// image generation has its own fixed constructor, not client inference access.
fn restrict_native_tool(tool: &mut Value, operation: BodyOperation, namespaced: bool) -> bool {
    let Some(tool) = tool.as_object_mut() else {
        return false;
    };
    let fields: &[&str] = match tool.get("type").and_then(Value::as_str) {
        Some("function") => &[
            "type",
            "name",
            "description",
            "strict",
            "defer_loading",
            "parameters",
        ],
        Some("custom") => &["type", "name", "description", "defer_loading", "format"],
        Some("namespace") if !namespaced => &["type", "name", "description", "tools"],
        Some("tool_search") if !namespaced => &["type", "execution", "description", "parameters"],
        Some("web_search") if !namespaced => &[
            "type",
            "external_web_access",
            "indexed_web_access",
            "filters",
            "user_location",
            "search_context_size",
            "search_content_types",
        ],
        Some("image_generation") if !namespaced && operation == BodyOperation::Image => {
            &["type", "action", "output_format", "size", "quality"]
        }
        _ => return false,
    };
    tool.retain(|key, _| fields.contains(&key.as_str()));
    retain_control_fields(tool.get_mut("format"), &["type", "syntax", "definition"]);
    retain_control_fields(tool.get_mut("filters"), &["allowed_domains"]);
    retain_control_fields(
        tool.get_mut("user_location"),
        &["type", "country", "region", "city", "timezone"],
    );
    if let Some(Value::Array(children)) = tool.get_mut("tools") {
        children.retain_mut(|child| restrict_native_tool(child, operation, true));
        return !children.is_empty();
    }
    true
}

fn retain_control_fields(value: Option<&mut Value>, fields: &[&str]) {
    if let Some(object) = value.and_then(Value::as_object_mut) {
        object.retain(|key, value| !value.is_null() && fields.contains(&key.as_str()));
    }
}

/// Header order follows the official 0.167.0 HTTP capture: core/src/client.rs
/// request headers, transport headers, then codex-api auth and identity.
pub(super) fn append_runtime_headers(
    provider: &ProviderSnapshot,
    body: &Value,
    session: &NativeSession,
    headers: &mut Vec<(String, String)>,
) -> Result<(), stravia_vendor_sdk::PluginError> {
    // Codec/ingress headers are not native identity authority. Build this
    // target's complete header set rather than retaining alternate session keys.
    let mut transport = std::mem::take(headers);
    transport.retain(|(name, _)| {
        name.eq_ignore_ascii_case("accept") || name.eq_ignore_ascii_case("content-type")
    });
    headers.push(("version".into(), CLIENT_VERSION.into()));
    headers.push(("x-codex-beta-features".into(), BETA_FEATURES.into()));
    headers.push(("x-codex-window-id".into(), session.window_id.clone()));
    let turn_metadata = body
        .pointer("/client_metadata/x-codex-turn-metadata")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("Codex native turn metadata is missing"))?;
    headers.push((TURN_METADATA_KEY.into(), turn_metadata.to_owned()));
    if session.operation != BodyOperation::WebSocket && session.model.responses_lite {
        headers.push((RESPONSES_LITE_HEADER.into(), "true".into()));
    }
    headers.push(("x-codex-routing-hint".into(), routing_hint(body)?));
    // The official client binds the request ID to the thread on both HTTP
    // (codex-api/endpoint/responses.rs) and WS (core/client.rs).
    headers.push(("x-client-request-id".into(), session.thread_id.clone()));
    headers.push(("session-id".into(), session.thread_id.clone()));
    headers.push(("thread-id".into(), session.thread_id.clone()));
    if let Some(parent_thread_id) = &session.parent_thread_id {
        headers.push(("x-codex-parent-thread-id".into(), parent_thread_id.clone()));
    }
    headers.append(&mut transport);
    append_identity_headers(provider, headers)
}

fn set_header(headers: &mut Vec<(String, String)>, name: &str, value: String) {
    headers.retain(|(candidate, _)| !candidate.eq_ignore_ascii_case(name));
    headers.push((name.into(), value));
}

/// codex-api bearer auth plus the default client's originator and user agent.
pub(super) fn append_identity_headers(
    provider: &ProviderSnapshot,
    headers: &mut Vec<(String, String)>,
) -> Result<(), stravia_vendor_sdk::PluginError> {
    let access_token = secret(provider, "access_token")?;
    set_header(headers, "authorization", format!("Bearer {access_token}"));
    if let Some(account_id) = provider
        .credentials
        .get("account_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| account_id_from_jwt(access_token))
    {
        set_header(headers, "chatgpt-account-id", account_id);
    }
    set_header(headers, "originator", ORIGINATOR.into());
    set_header(headers, "user-agent", user_agent());
    Ok(())
}

pub(super) fn fields(headers: Vec<(String, String)>) -> stravia_vendor_sdk::net::Fields {
    let mut fields = stravia_vendor_sdk::net::Fields::new();
    for (name, value) in headers {
        fields.append(name, value.into_bytes());
    }
    fields
}

pub(super) fn http_fields(mut headers: Vec<(String, String)>) -> stravia_vendor_sdk::net::Fields {
    headers.push(("accept-encoding".into(), "identity".into()));
    headers.push(("cookie".into(), String::new()));
    fields(headers)
}

pub(super) fn http_options()
-> Result<stravia_vendor_sdk::net::Options, stravia_vendor_sdk::PluginError> {
    let mut options = stravia_vendor_sdk::net::Options::new();
    options
        .shape("codex-http")
        .map_err(|e| invalid(e.to_string()))?;
    options
        .set(stravia_vendor_sdk::net::keys::COOKIES, COOKIE_POLICY)
        .map_err(|e| invalid(e.to_string()))?;
    Ok(options)
}

/// The official client zstd-encodes Responses bodies only for ChatGPT auth on
/// the built-in OpenAI provider; a custom base URL is a different provider.
#[derive(Serialize, Deserialize)]
struct StickyTurnState {
    key: String,
    value: String,
}

fn sticky_turn_key(
    provider: &ProviderSnapshot,
) -> Result<Option<String>, stravia_vendor_sdk::PluginError> {
    let Some(turn) = scoped_affinity(provider, "logical_turn_id") else {
        return Ok(None);
    };
    let token = secret(provider, "access_token")?;
    Ok(Some(derived_uuid(&[
        b"codex-turn-state",
        turn.as_bytes(),
        token.as_bytes(),
    ])))
}

fn load_sticky_turn_state(
    provider: &ProviderSnapshot,
) -> Result<Option<String>, stravia_vendor_sdk::PluginError> {
    let Some(key) = sticky_turn_key(provider)? else {
        return Ok(None);
    };
    let cell = stravia_vendor_sdk::state::open(
        stravia_vendor_sdk::state::Scope::Conversation,
        "codex-turn-state",
    )?;
    let current = cell.load()?;
    let state: Option<StickyTurnState> = current
        .payload
        .as_deref()
        .map(serde_json::from_slice)
        .transpose()
        .map_err(|_| invalid("Codex turn state is invalid"))?;
    Ok(state
        .filter(|state| state.key == key)
        .map(|state| state.value))
}

fn record_sticky_turn_state(
    provider: &ProviderSnapshot,
    headers: &stravia_vendor_sdk::net::Fields,
) -> Result<(), stravia_vendor_sdk::PluginError> {
    let Some(key) = sticky_turn_key(provider)? else {
        return Ok(());
    };
    let Some(value) = headers
        .get("x-codex-turn-state")
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
    else {
        return Ok(());
    };
    let cell = stravia_vendor_sdk::state::open(
        stravia_vendor_sdk::state::Scope::Conversation,
        "codex-turn-state",
    )?;
    loop {
        let current = cell.load()?;
        let prior: Option<StickyTurnState> = current
            .payload
            .as_deref()
            .map(serde_json::from_slice)
            .transpose()
            .map_err(|_| invalid("Codex turn state is invalid"))?;
        if prior.as_ref().is_some_and(|state| state.key == key) {
            return Ok(());
        }
        let payload = serde_json::to_vec(&StickyTurnState {
            key: key.clone(),
            value: value.to_owned(),
        })
        .map_err(|_| invalid("Codex turn state could not be encoded"))?;
        match cell.compare_exchange(current.revision, Some(&payload)) {
            Ok(()) => return Ok(()),
            Err(stravia_vendor_sdk::state::Error::Conflict) => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

pub(super) fn start_responses_http(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    mut request: stravia_vendor_sdk::net::Request,
) -> Result<stravia_vendor_sdk::HttpResponse, stravia_vendor_sdk::PluginError> {
    let mut options = http_options()?;
    let compressed = official_endpoint(&provider.base_url);
    let sticky = load_sticky_turn_state(provider)?;
    let mut headers = stravia_vendor_sdk::net::Fields::new();
    for field in std::mem::take(&mut request.headers).into_vec() {
        if field.name.eq_ignore_ascii_case("accept-encoding") {
            continue;
        }
        if compressed && field.name.eq_ignore_ascii_case("content-type") {
            headers.append("content-encoding", b"zstd".to_vec());
        }
        let beta = field.name.eq_ignore_ascii_case("x-codex-beta-features");
        headers.append(field.name, field.bytes);
        if beta && let Some(value) = &sticky {
            headers.append("x-codex-turn-state", value.as_bytes().to_vec());
        }
    }
    headers.slot("host");
    headers.slot("content-length");
    request.headers = headers;
    if compressed {
        options
            .request_encoding(stravia_vendor_sdk::net::Coding::Zstd)
            .map_err(|e| invalid(e.to_string()))?;
    }
    let response = host.http_start(request, &options)?;
    let response_headers = response.headers()?;
    if (200..300).contains(&response.status()?) {
        record_sticky_turn_state(provider, &response_headers)?;
    }
    Ok(response)
}

fn official_endpoint(base_url: &str) -> bool {
    url::Url::parse(base_url).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str() == Some("chatgpt.com")
            && url.port_or_known_default() == Some(443)
    })
}

fn routing_hint(body: &Value) -> Result<String, stravia_vendor_sdk::PluginError> {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid("Codex Responses request is missing model"))?;
    Ok(
        match body
            .get("service_tier")
            .and_then(Value::as_str)
            .map(str::trim)
            // 首轮省略的默认 tier 会由上游响应补全；两者必须保持同一连接身份。
            .filter(|value| !value.is_empty() && *value != "default")
        {
            Some(tier) => format!("model={model};tier={tier}"),
            None => format!("model={model}"),
        },
    )
}

pub(super) fn websocket_available(provider: &ProviderSnapshot) -> bool {
    configured_websocket_url(provider).is_some() || official_endpoint(&provider.base_url)
}

#[derive(Clone, Serialize, Deserialize)]
struct WebSocketHandshakeSnapshot {
    turn_metadata: String,
    created_at_ms: i64,
}

type WebSocketHandshakeSnapshots = std::collections::BTreeMap<String, WebSocketHandshakeSnapshot>;

fn websocket_snapshot_key(
    provider: &ProviderSnapshot,
    session: &NativeSession,
    response_id: &str,
) -> Result<String, stravia_vendor_sdk::PluginError> {
    let credential = credential_key(provider)?;
    Ok(derived_uuid(&[
        credential.as_bytes(),
        session.thread_id.as_bytes(),
        response_id.as_bytes(),
    ]))
}

fn read_websocket_snapshots() -> Result<WebSocketHandshakeSnapshots, stravia_vendor_sdk::PluginError>
{
    let cell = stravia_vendor_sdk::state::open_volatile("codex-ws-handshakes")?;
    let snapshot = cell.load()?;
    snapshot
        .payload
        .map(|payload| {
            serde_json::from_slice(&payload)
                .map_err(|_| invalid("Codex WebSocket handshake state is invalid"))
        })
        .transpose()
        .map(Option::unwrap_or_default)
}

fn save_websocket_snapshot(
    key: String,
    snapshot: WebSocketHandshakeSnapshot,
) -> Result<(), stravia_vendor_sdk::PluginError> {
    let cell = stravia_vendor_sdk::state::open_volatile("codex-ws-handshakes")?;
    let now = chrono::Utc::now().timestamp_millis();
    loop {
        let current = cell.load()?;
        let mut snapshots: WebSocketHandshakeSnapshots = current
            .payload
            .as_deref()
            .map(serde_json::from_slice)
            .transpose()
            .map_err(|_| invalid("Codex WebSocket handshake state is invalid"))?
            .unwrap_or_default();
        snapshots.retain(|_, snapshot| {
            now.saturating_sub(snapshot.created_at_ms) < WEBSOCKET_REUSE_MAX_AGE_MILLIS as i64
        });
        snapshots.insert(key.clone(), snapshot.clone());
        let payload = serde_json::to_vec(&snapshots)
            .map_err(|_| invalid("Codex WebSocket handshake state could not be encoded"))?;
        match cell.compare_exchange(current.revision, Some(&payload)) {
            Ok(()) => return Ok(()),
            Err(stravia_vendor_sdk::state::Error::Conflict) => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

/// The native continuation the frame actually asks the upstream to resume.
pub(super) fn request_continuation_id(body: &Value) -> Option<&str> {
    (body.get("store").and_then(Value::as_bool) == Some(false))
        .then(|| body.get("previous_response_id").and_then(Value::as_str))
        .flatten()
        .map(str::trim)
        .filter(|id| !id.is_empty())
}

pub(super) fn infer_websocket(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    body: &Value,
    session: &NativeSession,
    preserve_upstream_errors: bool,
) -> Result<AiResponse, stravia_vendor_sdk::PluginError> {
    let mut headers = vec![
        ("Host".into(), String::new()),
        ("Connection".into(), "Upgrade".into()),
        ("Upgrade".into(), "websocket".into()),
        ("Sec-WebSocket-Version".into(), "13".into()),
        ("Sec-WebSocket-Key".into(), String::new()),
    ];
    let mut runtime = Vec::new();
    append_runtime_headers(provider, body, session, &mut runtime)?;
    let continuation_id = request_continuation_id(body).map(str::to_owned);
    let now = chrono::Utc::now().timestamp_millis();
    let mut snapshot = WebSocketHandshakeSnapshot {
        turn_metadata: runtime
            .iter()
            .find(|(name, _)| name == TURN_METADATA_KEY)
            .map(|(_, value)| value.clone())
            .ok_or_else(|| invalid("Codex native turn metadata is missing"))?,
        created_at_ms: now,
    };
    if let Some(id) = &continuation_id {
        let snapshots = read_websocket_snapshots()?;
        let key = websocket_snapshot_key(provider, session, id)?;
        if let Some(previous) = snapshots.get(&key).filter(|previous| {
            now.saturating_sub(previous.created_at_ms) < WEBSOCKET_REUSE_MAX_AGE_MILLIS as i64
        }) {
            snapshot = previous.clone();
            set_header(
                &mut runtime,
                TURN_METADATA_KEY,
                snapshot.turn_metadata.clone(),
            );
        } else {
            return Err(stravia_vendor_sdk::PluginError {
                kind: stravia_vendor_sdk::ErrorKind::ContinuationUnavailable,
                message: "Codex WebSocket continuation snapshot is unavailable".into(),
                upstream_status: None,
            });
        }
    }
    headers.append(&mut runtime);
    set_header(
        &mut headers,
        "openai-beta",
        "responses_websockets=2026-02-06".into(),
    );
    headers.push((
        "sec-websocket-extensions".into(),
        "permessage-deflate; client_max_window_bits".into(),
    ));
    let order = [
        "host",
        "connection",
        "upgrade",
        "sec-websocket-version",
        "sec-websocket-key",
        "chatgpt-account-id",
        "authorization",
        "user-agent",
        "originator",
        "openai-beta",
        "version",
        "x-codex-beta-features",
        "x-client-request-id",
        "session-id",
        "thread-id",
        "x-codex-window-id",
        "x-codex-turn-metadata",
        "x-codex-routing-hint",
        "x-codex-parent-thread-id",
        "sec-websocket-extensions",
    ];
    headers.sort_unstable_by_key(|(name, _)| {
        order
            .iter()
            .position(|entry| name.eq_ignore_ascii_case(entry))
            .unwrap_or(order.len())
    });

    let url = if let Some(url) = configured_websocket_url(provider) {
        url.to_owned()
    } else {
        if !official_endpoint(&provider.base_url) {
            return Err(invalid(
                "custom Codex base URLs require an authorized websocket_url option",
            ));
        }
        let mut url = url::Url::parse(&endpoint(&provider.base_url, "/responses"))
            .map_err(|_| invalid("Codex base URL is invalid"))?;
        url.set_scheme("wss")
            .map_err(|_| invalid("Codex WebSocket URL could not be constructed"))?;
        url.to_string()
    };
    headers.push(("cookie".into(), String::new()));
    let continuation_requested = continuation_id.is_some();
    let mut options = stravia_vendor_sdk::net::Options::new();
    options
        .shape("codex-ws")
        .map_err(|e| invalid(e.to_string()))?;
    options
        .set(stravia_vendor_sdk::net::keys::COOKIES, COOKIE_POLICY)
        .map_err(|e| invalid(e.to_string()))?;
    options
        .ws_compression(stravia_vendor_sdk::net::WsCompression::Preferred)
        .map_err(|e| invalid(e.to_string()))?;
    options
        .reuse_max_age(std::time::Duration::from_millis(
            WEBSOCKET_REUSE_MAX_AGE_MILLIS,
        ))
        .map_err(|e| invalid(e.to_string()))?;
    if let Some(id) = &continuation_id {
        options
            .reuse_only(true)
            .map_err(|e| invalid(e.to_string()))?;
        options
            .continuation(id)
            .map_err(|e| invalid(e.to_string()))?;
    }
    let connection = host.ws_connect(
        stravia_vendor_sdk::net::ConnectRequest::new(url.clone()).fields(fields(headers)),
        &options,
    )?;
    if connection.is_fresh_handshake()? {
        let headers = connection.handshake_headers();
        record_sticky_turn_state(provider, &headers)?;
    }
    let mut frame = body.clone();
    super::lite_history::stabilize(session, &mut frame)?;
    let object = frame
        .as_object_mut()
        .ok_or_else(|| invalid("Codex Responses request body must be an object"))?;
    // This body was projected as WebSocket before the scoped identity was
    // attached. The WS schema includes stream and service_tier as well.
    object.insert("type".into(), Value::String("response.create".into()));
    // core/src/client.rs: WS frames carry the HTTP-only Lite header and the
    // dispatch time as reserved client_metadata keys.
    let metadata = object
        .get_mut("client_metadata")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| invalid("Codex native client metadata is missing"))?;
    metadata.insert(
        WS_REQUEST_START_METADATA_KEY.into(),
        chrono::Utc::now().timestamp_millis().to_string().into(),
    );
    if let Some(value) = load_sticky_turn_state(provider)? {
        metadata.insert("x-codex-turn-state".into(), value.into());
    }
    if session.model.responses_lite {
        metadata.insert(RESPONSES_LITE_WS_METADATA_KEY.into(), "true".into());
    }
    host.emit_started()?;
    connection.send(&WsMessage::Text(
        serde_json::to_string(&WireBody(&frame))
            .map_err(|_| invalid("Codex WebSocket request could not be encoded"))?,
    ))?;

    let mut observe_metadata = |event: &Value| {
        if event.get("type").and_then(Value::as_str) == Some("response.metadata")
            && let Some(headers) = event.get("headers").and_then(Value::as_object)
        {
            let mut fields = stravia_vendor_sdk::net::Fields::new();
            for (name, value) in headers {
                if let Some(value) = value.as_str() {
                    fields.append(name, value.as_bytes());
                }
            }
            record_sticky_turn_state(provider, &fields)?;
        }
        Ok(())
    };
    let response =
        stravia_vendor_common::common::decode_ai_response_websocket_with_error_classifier(
            host,
            &OPEN_RESPONSES_2026_04_24.to_string(),
            connection,
            preserve_upstream_errors,
            continuation_requested,
            |value, saw_response_event| {
                super::classify_codex_websocket_error(
                    value,
                    saw_response_event,
                    continuation_requested,
                )
            },
            Some(&mut observe_metadata),
        )?;
    if !response.id.is_empty() {
        let key = websocket_snapshot_key(provider, session, &response.id)?;
        save_websocket_snapshot(key, snapshot)?;
    }
    Ok(response)
}

pub(super) fn search(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    request: SearchRequest,
) -> Result<SearchResponse, stravia_vendor_sdk::PluginError> {
    if request.query.trim().is_empty() {
        return Err(invalid("Codex Search query must not be empty"));
    }
    if request.max_sources == Some(0) {
        return Err(invalid(
            "Codex Search max_sources must be greater than zero",
        ));
    }
    require_codex_protocol(provider)?;
    let model = required_model(provider)?;
    let mut tool = json!({
        "type": "web_search",
        "external_web_access": true
    });
    if !request.allowed_domains.is_empty() {
        tool["filters"] = json!({ "allowed_domains": request.allowed_domains });
    }
    let input = match request
        .language
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        Some(language) => json!({"query": request.query, "language": language}),
        None => json!({"query": request.query}),
    };
    let mut body = json!({
        "model": model,
        "instructions": "Perform speed-first Web Search. Distinguish verified facts, inference, disagreement, and uncertainty. Follow the requested language; otherwise follow the query language; use English when ambiguous.",
        "input": [{
            "role": "user",
            "content": [{"type":"input_text", "text": input.to_string()}]
        }],
        "tools": [tool],
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "reasoning": {"effort":"medium", "summary":"auto"},
        "store": false,
        "stream": true,
        "include": ["reasoning.encrypted_content", "web_search_call.action.sources"]
    });
    let mut session = NativeSession::new(provider, BodyOperation::Search);
    session.resolve_identity(host, provider)?;
    session.project_body(&mut body)?;
    let mut headers = vec![
        ("accept".into(), "text/event-stream".into()),
        ("content-type".into(), "application/json".into()),
    ];
    append_runtime_headers(provider, &body, &session, &mut headers)?;
    host.emit_started()?;
    let response = start_responses_http(
        host,
        provider,
        stravia_vendor_sdk::net::Request {
            method: "POST".into(),
            url: endpoint(&provider.base_url, "/responses"),
            headers: http_fields(headers),
            body: serde_json::to_vec(&WireBody(&body))
                .map_err(|_| invalid("Codex Search request could not be encoded"))?,
        },
    )?;
    ensure_success(&response, "Codex Search")?;
    let bytes = stravia_vendor_sdk::read_http_body(&response, MAX_RESPONSE_BYTES)?;
    let response = parse_responses_body(&bytes)?;
    let report_id = provider
        .operation_metadata
        .get("turn_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("source");
    normalize_search_response(&response, request.max_sources, report_id)
}

pub(super) fn parse_responses_body(bytes: &[u8]) -> Result<Value, stravia_vendor_sdk::PluginError> {
    let (output_items, mut response) = scan_responses_body(bytes)?;
    if response
        .get("output")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
        && !output_items.is_empty()
    {
        response["output"] = Value::Array(output_items);
    }
    Ok(response)
}

/// 返回流内逐项完成的 output item 与终态 response；非 SSE 的 JSON 体没有逐项事件。
pub(super) fn scan_responses_body(
    bytes: &[u8],
) -> Result<(Vec<Value>, Value), stravia_vendor_sdk::PluginError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| retryable("Codex returned non-UTF-8 response data"))?;
    if !text.trim_start().starts_with("data:") && !text.contains("\nevent:") {
        let response =
            serde_json::from_str(text).map_err(|_| retryable("Codex returned malformed JSON"))?;
        return Ok((Vec::new(), response));
    }
    let normalized = text.replace("\r\n", "\n");
    let mut output_items = Vec::new();
    for block in normalized.split("\n\n") {
        let mut event = None;
        let mut data = String::new();
        for line in block.lines() {
            if let Some(value) = line.strip_prefix("event:") {
                event = Some(value.trim());
            } else if let Some(value) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(value.trim_start());
            }
        }
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let payload: Value =
            serde_json::from_str(&data).map_err(|_| retryable("Codex returned malformed SSE"))?;
        let event = event
            .or_else(|| payload.get("type").and_then(Value::as_str))
            .unwrap_or_default();
        match event {
            "response.output_item.done" => {
                if let Some(item) = payload.get("item") {
                    output_items.push(item.clone());
                }
            }
            "response.completed" | "response.done" => {
                let response = payload.get("response").cloned().unwrap_or(payload);
                return Ok((output_items, response));
            }
            "error" | "response.failed" | "response.incomplete" => {
                return Err(stravia_vendor_common::common::model_error(
                    response_failure_kind(&payload),
                    "Codex response stream failed",
                ));
            }
            _ => {}
        }
    }
    Err(stravia_vendor_common::common::model_error(
        stravia_runtime_contract::protocol::ir::AiErrorKind::UnexpectedEof,
        "Codex stream ended without a terminal response",
    ))
}

fn response_failure_kind(payload: &Value) -> stravia_runtime_contract::protocol::ir::AiErrorKind {
    use stravia_runtime_contract::protocol::ir::AiErrorKind;

    let error = payload.get("error").unwrap_or(payload);
    let body = error.to_string().to_ascii_lowercase();
    if ["insufficient_quota", "quota_exceeded", "billing_hard_limit"]
        .iter()
        .any(|marker| body.contains(marker))
    {
        return AiErrorKind::QuotaExceeded;
    }
    let discriminator = error
        .get("code")
        .or_else(|| error.get("type"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    match discriminator {
        "authentication_error" | "invalid_api_key" => AiErrorKind::AuthenticationError,
        "authorization_error" | "permission_error" | "permission_denied" => {
            AiErrorKind::AuthorizationError
        }
        "not_found" | "not_found_error" | "model_not_found" => AiErrorKind::NotFoundError,
        "rate_limit_error" | "too_many_requests" => AiErrorKind::RateLimitError,
        "invalid_request" | "invalid_request_error" => AiErrorKind::InvalidRequest,
        "server_error" | "model_error" => AiErrorKind::ServerError,
        "service_unavailable" => AiErrorKind::ServiceUnavailable,
        "timeout" => AiErrorKind::Timeout,
        "content_filtered" => AiErrorKind::ContentFiltered,
        "context_length_exceeded" => AiErrorKind::ContextLengthExceeded,
        _ => AiErrorKind::StreamMidError,
    }
}

#[derive(Debug)]
struct Annotation {
    start: usize,
    end: usize,
    url: String,
    title: Option<String>,
}

fn normalize_search_response(
    response: &Value,
    requested_max_sources: Option<u32>,
    report_id: &str,
) -> Result<SearchResponse, stravia_vendor_sdk::PluginError> {
    let mut answer = String::new();
    let mut annotations = Vec::new();
    for content in response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
    {
        let Some(text) = content.get("text").and_then(Value::as_str) else {
            continue;
        };
        if !answer.is_empty() {
            answer.push('\n');
        }
        let base = answer.len();
        answer.push_str(text);
        for annotation in content
            .get("annotations")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if !matches!(
                annotation.get("type").and_then(Value::as_str),
                Some("url_citation") | Some("url_annotation")
            ) {
                continue;
            }
            let start_index = annotation
                .get("start_index")
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| retryable("Codex citation start is invalid"))?;
            let end_index = annotation
                .get("end_index")
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| retryable("Codex citation end is invalid"))?;
            if start_index >= end_index {
                return Err(retryable("Codex citation span is invalid"));
            }
            let (Some(start), Some(end)) = (
                character_offset_to_byte(text, start_index),
                character_offset_to_byte(text, end_index),
            ) else {
                return Err(retryable("Codex citation span is invalid"));
            };
            let url = annotation
                .get("url")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|url| url.starts_with("https://") || url.starts_with("http://"))
                .ok_or_else(|| retryable("Codex citation URL is invalid"))?;
            annotations.push(Annotation {
                start: base + start,
                end: base + end,
                url: url.to_owned(),
                title: annotation
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            });
        }
    }
    if answer.is_empty() || annotations.is_empty() {
        return Err(retryable("Codex Search returned no cited answer"));
    }
    annotations.sort_by_key(|annotation| (annotation.start, annotation.end));
    let limit = requested_max_sources.unwrap_or(20).clamp(1, 20) as usize;
    let mut source_ids = HashMap::new();
    let mut sources = Vec::new();
    for annotation in &annotations {
        if !source_ids.contains_key(&annotation.url) {
            if sources.len() == limit {
                return Err(retryable(
                    "Codex Search returned more cited sources than allowed",
                ));
            }
            let id = format!("{report_id}:{}", sources.len() + 1);
            source_ids.insert(annotation.url.clone(), id.clone());
            sources.push(SearchSource {
                id,
                url: annotation.url.clone(),
                title: annotation.title.clone(),
                snippet: None,
                published_at: None,
            });
        }
    }
    let mut insertions = annotations
        .iter()
        .map(|annotation| {
            (
                annotation.end,
                format!(" [sc:{}]", source_ids[&annotation.url]),
            )
        })
        .collect::<Vec<_>>();
    insertions.sort_by_key(|(offset, _)| std::cmp::Reverse(*offset));
    for (offset, marker) in insertions {
        answer.insert_str(offset, &marker);
    }
    Ok(SearchResponse {
        answer,
        sources,
        limitations: Vec::new(),
        usage: usage_from_response(response),
    })
}

fn character_offset_to_byte(value: &str, offset: usize) -> Option<usize> {
    value
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(value.len()))
        .nth(offset)
}

pub(super) fn usage_from_response(response: &Value) -> Option<Usage> {
    let usage = response.get("usage")?;
    let prompt_tokens: u32 = usage.get("input_tokens")?.as_u64()?.try_into().ok()?;
    let completion_tokens: u32 = usage.get("output_tokens")?.as_u64()?.try_into().ok()?;
    let total_tokens = usage
        .get("total_tokens")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or_else(|| prompt_tokens.saturating_add(completion_tokens));
    let web_search_requests = response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("web_search_call"))
        .count()
        .min(u32::MAX as usize) as u32;
    Some(Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens,
        required_components_known: true,
        cache_read_tokens: usage
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        reasoning_tokens: usage
            .pointer("/output_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        cache_creation_tokens: usage
            .pointer("/input_tokens_details/cache_write_tokens")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        server_tool_use: (web_search_requests > 0).then_some(ServerToolUsage {
            web_search_requests,
            web_fetch_requests: 0,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn canonical_native_choice_selects_search_but_not_unsupported_execution() {
        use stravia_runtime_contract::protocol::ir::{
            AiItem, AiRequest, MessageContent, NativeTool, NativeToolKind, Role, ToolChoice,
            WebSearchTool,
        };
        let mut request = AiRequest::new(
            "gpt-5.4",
            vec![AiItem {
                role: Role::User,
                content: MessageContent::Text("hello".to_owned().into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            }],
        );
        request
            .parameters
            .native_tools
            .push(NativeTool::WebSearch(WebSearchTool::default()));
        request.tool_choice = Some(ToolChoice::Native(NativeToolKind::WebSearch));
        let mut body = stravia_vendor_common::common::encode_inference_request(
            &OPEN_RESPONSES_2026_04_24.to_string(),
            request.clone(),
        )
        .unwrap()
        .body;
        body["tools"] = encode_tools(&request, BodyOperation::Http).into();
        prepare_operation_body(&mut body, BodyOperation::Http).unwrap();
        assert_eq!(body["tools"][0]["type"], "web_search");
        assert_eq!(body["tool_choice"], json!({"type":"web_search"}));
        body["tool_choice"] = json!({"type":"code_interpreter"});
        prepare_operation_body(&mut body, BodyOperation::Http).unwrap();
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn native_operation_allowlists_ignore_unknown_controls_without_touching_business_json() {
        let original = json!({
            "model":"gpt-5.4", "instructions":"native",
            "input":[{"role":"user","content":[{"type":"input_text","text":"hi"}]}],
            "tools":[{"type":"function","name":"business","parameters":{
                "type":"object","properties":{"private_parameter":{"type":"string"}}
            }}],
            "tool_choice":"auto", "parallel_tool_calls":false,
            "reasoning":{"effort":"high","summary":"auto","context":"all_turns","private_parameter":1},
            "text":{"verbosity":"low","format":{"type":"json_schema","name":"out","strict":true,
                "schema":{"type":"object","properties":{"private_parameter":{"type":"string"}}},
                "private_parameter":1},"private_parameter":1},
            "stream":true, "stream_options":{"reasoning_summary_delivery":"sequential_cutoff","private_parameter":1},
            "access_programs":{"cyber":"standard","private_parameter":1},
            "service_tier":"priority","include":["reasoning.encrypted_content"],
            "prompt_cache_key":"cache","store":true,
            "previous_response_id":"previous","generate":false,
            "metadata":{"private_parameter":1},"prompt_cache_retention":"24h",
            "client_metadata":{"session_id":"spoofed"},"private_parameter":{"arbitrary":true}
        });
        for operation in [
            BodyOperation::Http,
            BodyOperation::WebSocket,
            BodyOperation::Compaction,
            BodyOperation::Search,
            BodyOperation::Image,
        ] {
            let mut body = original.clone();
            prepare_operation_body(&mut body, operation).unwrap();
            // 0.167.0 has no top-level instructions or caller-owned stream
            // options; access programs are session-owned.
            for absent in [
                "private_parameter",
                "metadata",
                "prompt_cache_retention",
                "client_metadata",
                "instructions",
                "stream_options",
                "access_programs",
            ] {
                assert!(body.get(absent).is_none(), "{absent}");
            }
            assert_eq!(body["tools"], original["tools"]);
            assert_eq!(body["input"], original["input"]);
            assert_eq!(
                body["text"]["format"]["schema"],
                original["text"]["format"]["schema"]
            );
            assert!(body["reasoning"].get("private_parameter").is_none());
            assert!(body["text"]["format"].get("private_parameter").is_none());
            assert!(body["text"].get("private_parameter").is_none());
            if operation == BodyOperation::WebSocket {
                assert_eq!(body["previous_response_id"], "previous");
                assert_eq!(body["generate"], false);
            } else {
                assert!(body.get("previous_response_id").is_none());
                assert!(body.get("generate").is_none());
            }
            assert_eq!(body["store"], false);
            assert_eq!(body["service_tier"], "priority");
        }
    }

    fn provider() -> ProviderSnapshot {
        ProviderSnapshot {
            provider_id: "openai-codex".into(),
            channel: "codex".into(),
            base_url: "https://chatgpt.com/backend-api/codex".into(),
            protocol: "open-responses".into(),
            options: BTreeMap::new(),
            credentials: BTreeMap::from([("access_token".into(), "test-token".into())]),
            model: Some("gpt-6-astra".into()),
            model_metadata: None,
            client_headers: vec![
                ("session-id".into(), "untrusted-session".into()),
                ("thread-id".into(), "untrusted-thread".into()),
                ("x-client-request-id".into(), "untrusted-request".into()),
                ("x-codex-window-id".into(), "untrusted-window".into()),
                ("x-codex-beta-features".into(), "native-beta".into()),
                ("x-codex-turn-state".into(), "native-turn-state".into()),
                (
                    "x-codex-turn-metadata".into(),
                    json!({"thread_id":"spoofed", "session_id":"spoofed",
                        "turn_id":"spoofed", "custom_hint":"preserved"})
                    .to_string(),
                ),
            ],
            operation_metadata: BTreeMap::from([
                ("transport_affinity".into(), "principal-target-scope".into()),
                (
                    "context_window_id".into(),
                    "00000000-0000-7000-8000-000000000003".into(),
                ),
                ("context_window_index".into(), json!(0)),
                ("thread_id".into(), "not-a-scoped-key".into()),
            ]),
        }
    }

    /// The official `/models` row of a Responses Lite model.
    fn lite_provider() -> ProviderSnapshot {
        let mut provider = provider();
        provider.model_metadata = Some(stravia_vendor_sdk::ModelMetadata {
            extensions: BTreeMap::from([
                ("use_responses_lite".into(), json!(true)),
                ("support_verbosity".into(), json!(true)),
                ("default_verbosity".into(), json!("low")),
                ("default_reasoning_level".into(), json!("medium")),
                ("node_repl_auto_review_required".into(), json!(true)),
                ("node_repl_disabled".into(), json!(false)),
            ]),
            ..Default::default()
        });
        provider
    }

    fn turn_body() -> Value {
        json!({
            "model":"gpt-6-astra",
            "instructions":"base instructions",
            "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],
            "tools":[
                {"type":"web_search"},
                {"type":"function","name":"read","parameters":{"type":"object"}},
                {"type":"namespace","name":"clock","description":"clock tools","tools":[
                    {"type":"function","name":"now","parameters":{"type":"object"}}
                ]},
                {"type":"custom","name":"exec","format":{"type":"text"}}
            ],
            "parallel_tool_calls":true,
            "reasoning":{"effort":"low"}
        })
    }

    fn project(session: &NativeSession, mut body: Value) -> Value {
        session.project_body(&mut body).unwrap();
        body
    }

    fn client_metadata(session: &NativeSession) -> Value {
        project(session, json!({"model":"gpt-6-astra", "input":[]}))["client_metadata"].clone()
    }

    fn turn_metadata(metadata: &Value) -> Value {
        serde_json::from_str(metadata["x-codex-turn-metadata"].as_str().unwrap()).unwrap()
    }

    #[test]
    fn lite_turn_moves_tools_and_instructions_into_the_input_prefix() {
        let provider = lite_provider();
        let session = NativeSession::new(&provider, BodyOperation::Http);
        let body = project(&session, turn_body());
        for absent in ["tools", "instructions"] {
            assert!(body.get(absent).is_none(), "{absent}");
        }
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(
            body["reasoning"],
            json!({"effort":"low", "context":"all_turns"})
        );
        assert_eq!(body["text"], json!({"verbosity":"low"}));
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert!(body.get("access_programs").is_none());
        assert_eq!(
            body["prompt_cache_key"],
            body["client_metadata"]["thread_id"]
        );
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 3);
        let declarations = &input[0];
        assert_eq!(declarations["type"], "additional_tools");
        assert_eq!(declarations["role"], "developer");
        assert!(declarations["id"].as_str().unwrap().starts_with("at_"));
        // Function and freeform tools join `functions` where the first of them
        // appeared; other declarations keep their relative order.
        assert_eq!(
            declarations["tools"],
            json!([
                {"type":"web_search"},
                {"type":"namespace","name":"functions","description":"","tools":[
                    {"type":"function","name":"read","parameters":{"type":"object"}},
                    {"type":"custom","name":"exec","format":{"type":"text"}}
                ]},
                {"type":"namespace","name":"clock","description":"clock tools","tools":[
                    {"type":"function","name":"now","parameters":{"type":"object"}}
                ]}
            ])
        );
        let instructions = &input[1];
        assert_eq!(instructions["role"], "developer");
        assert!(instructions["id"].as_str().unwrap().starts_with("msg_"));
        assert_eq!(
            instructions["content"],
            json!([{"type":"input_text","text":"base instructions"}])
        );
        let passthrough = &instructions["internal_chat_message_metadata_passthrough"];
        let create_time = passthrough["create_time"].as_f64().unwrap();
        let started = turn_metadata(&body["client_metadata"])["turn_started_at_unix_ms"]
            .as_i64()
            .unwrap();
        // Fractional Unix seconds stamped when this request recorded the item.
        assert!(create_time * 1000.0 >= started as f64 - 1.0);
        assert!(create_time < 1e11);
        assert_eq!(
            passthrough,
            &json!({
                "turn_id": body["client_metadata"]["turn_id"],
                "create_time": passthrough["create_time"],
                "content_item_kinds": ["model.base_instructions"],
                "content_item_metadata": [{"provenance":{"type":"harness","hook":false}}],
            })
        );
        assert_eq!(input[2]["role"], "user");

        // Projection creates fresh candidate IDs; the journal retains whichever
        // candidate was first recorded for the active window.
        let retry = project(
            &NativeSession::new(&provider, BodyOperation::Http),
            turn_body(),
        );
        assert_ne!(retry["input"][0]["id"], input[0]["id"]);
        assert_ne!(retry["input"][1]["id"], input[1]["id"]);
    }

    #[test]
    fn websocket_continuation_sends_only_the_appended_items() {
        let session = NativeSession::new(&lite_provider(), BodyOperation::WebSocket);
        let mut body = turn_body();
        body["previous_response_id"] = "resp_previous".into();
        let body = project(&session, body);
        assert_eq!(body["previous_response_id"], "resp_previous");
        // Candidate updates are reduced to the new suffix by the history journal.
        assert_eq!(body["input"][0]["type"], "additional_tools");
        assert_eq!(
            &body["input"].as_array().unwrap()[2..],
            turn_body()["input"].as_array().unwrap().as_slice()
        );
        assert!(body.get("tools").is_none());
        assert!(body.get("instructions").is_none());
        assert_eq!(body["reasoning"]["context"], "all_turns");

        let full = project(&session, turn_body());
        assert_eq!(full["input"][0]["type"], "additional_tools");
    }

    /// Top-level keys in wire order; a `Value` would sort them.
    fn wire_keys(body: &Value) -> Vec<String> {
        struct Keys(Vec<String>);
        impl<'de> Deserialize<'de> for Keys {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                struct Visitor;
                impl<'de> serde::de::Visitor<'de> for Visitor {
                    type Value = Keys;
                    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                        formatter.write_str("an object")
                    }
                    fn visit_map<A: serde::de::MapAccess<'de>>(
                        self,
                        mut map: A,
                    ) -> Result<Keys, A::Error> {
                        let mut keys = Vec::new();
                        while let Some((key, _)) =
                            map.next_entry::<String, serde::de::IgnoredAny>()?
                        {
                            keys.push(key);
                        }
                        Ok(Keys(keys))
                    }
                }
                deserializer.deserialize_map(Visitor)
            }
        }
        let encoded = serde_json::to_string(&WireBody(body)).unwrap();
        serde_json::from_str::<Keys>(&encoded).unwrap().0
    }

    #[test]
    fn wire_body_keeps_the_official_declaration_order() {
        let http = project(
            &NativeSession::new(&lite_provider(), BodyOperation::Http),
            turn_body(),
        );
        assert_eq!(
            wire_keys(&http),
            [
                "model",
                "stream",
                "input",
                "tool_choice",
                "parallel_tool_calls",
                "reasoning",
                "store",
                "include",
                "prompt_cache_key",
                "text",
                "client_metadata",
            ]
        );

        let mut body = turn_body();
        body["service_tier"] = "priority".into();
        body["text"] = json!({"verbosity":"low"});
        body["previous_response_id"] = "resp_previous".into();
        body["generate"] = false.into();
        let mut frame = project(
            &NativeSession::new(&provider(), BodyOperation::WebSocket),
            body,
        );
        frame["type"] = "response.create".into();
        assert_eq!(
            wire_keys(&frame),
            [
                "type",
                "model",
                "stream",
                "service_tier",
                "previous_response_id",
                "input",
                "tools",
                "tool_choice",
                "parallel_tool_calls",
                "reasoning",
                "store",
                "include",
                "prompt_cache_key",
                "text",
                "generate",
                "client_metadata",
            ]
        );

        frame["instructions"] = "unprojected".into();
        assert!(serde_json::to_string(&WireBody(&frame)).is_err());
    }

    #[test]
    fn non_lite_turn_keeps_top_level_tools_but_moves_instructions() {
        let session = NativeSession::new(&provider(), BodyOperation::Http);
        let body = project(&session, turn_body());
        assert_eq!(body["tools"].as_array().unwrap().len(), 4);
        assert_eq!(body["parallel_tool_calls"], true);
        assert_eq!(body["reasoning"], json!({"effort":"low"}));
        assert!(body.get("instructions").is_none());
        let instructions = &body["input"][0];
        assert_eq!(instructions["role"], "developer");
        // Request-time prefix: UUIDv5 in the thread namespace, without the
        // history-only turn and time stamps or Lite-only attribution. The
        // expected ID is an independent Python `uuid.uuid5` computation.
        assert_eq!(
            body["client_metadata"]["thread_id"],
            "cc39dd30-b6dd-72f8-b778-d6c48c3af5fa"
        );
        assert_eq!(
            instructions["id"],
            "msg_4c3b149e-bd99-5f82-9911-26ce2fcc49d7"
        );
        assert_eq!(
            instructions["internal_chat_message_metadata_passthrough"],
            json!({"content_item_kinds": ["model.base_instructions"]})
        );
        assert_eq!(body["input"][1]["role"], "user");
    }

    #[test]
    fn default_summary_and_access_program_are_omitted_from_inference() {
        for operation in [
            BodyOperation::Http,
            BodyOperation::WebSocket,
            BodyOperation::Compaction,
        ] {
            let session = NativeSession::new(&provider(), operation);
            let body = project(
                &session,
                json!({"model":"gpt-6-astra", "input":[], "reasoning":{"effort":"high", "summary":"auto"}}),
            );
            assert_eq!(body["reasoning"], json!({"effort":"high"}));
            assert!(body.get("access_programs").is_none());

            let body = project(
                &session,
                json!({"model":"gpt-6-astra", "input":[], "reasoning":{"summary":"auto"}}),
            );
            assert!(body.get("reasoning").is_none());

            let body = project(
                &session,
                json!({"model":"gpt-6-astra", "input":[], "reasoning":{"effort":"medium", "summary":"detailed"}}),
            );
            assert_eq!(
                body["reasoning"],
                json!({"effort":"medium", "summary":"detailed"})
            );
        }
    }

    #[test]
    fn model_defaults_fill_only_unset_controls() {
        let provider = lite_provider();
        let session = NativeSession::new(&provider, BodyOperation::Http);
        let body = project(
            &session,
            json!({"model":"gpt-6-astra", "input":"hello", "text":{"verbosity":"high"}}),
        );
        assert_eq!(body["reasoning"]["effort"], "medium");
        assert_eq!(body["text"]["verbosity"], "high");
        assert_eq!(
            body["input"],
            json!([{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}])
        );

        let mut unsupported = provider.clone();
        unsupported
            .model_metadata
            .as_mut()
            .unwrap()
            .extensions
            .insert("support_verbosity".into(), json!(false));
        let session = NativeSession::new(&unsupported, BodyOperation::Http);
        let body = project(
            &session,
            json!({"model":"gpt-6-astra", "input":[], "text":{"verbosity":"high"}}),
        );
        assert!(body.get("text").is_none());

        // Auxiliary constructors own their controls and keep the public wire.
        let session = NativeSession::new(&provider, BodyOperation::Image);
        let body = project(
            &session,
            json!({"model":"gpt-6-astra", "input":[], "tools":[{"type":"image_generation"}],
                "include":[]}),
        );
        assert!(body.get("reasoning").is_none());
        assert!(body.get("text").is_none());
        assert!(body.get("access_programs").is_none());
        assert_eq!(body["include"], json!([]));
        assert_eq!(body["tools"][0]["type"], "image_generation");
    }

    #[test]
    fn turn_metadata_follows_the_official_snapshot() {
        let session = NativeSession::new(&lite_provider(), BodyOperation::Http);
        let body = project(&session, turn_body());
        let metadata = &body["client_metadata"];
        let raw = metadata["x-codex-turn-metadata"].as_str().unwrap();
        let turn = turn_metadata(metadata);
        // Header and client_metadata carry the same string, so field order is
        // part of the wire contract (official CodexTurnMetadataPayload).
        let order = [
            "session_id",
            "thread_id",
            "turn_id",
            "window_id",
            "window_number",
            "context_window_id",
            "request_kind",
            "root_turn_id",
            "auto_review_enabled",
            "node_repl_auto_review_required",
            "node_repl_disabled",
            "turn_started_at_unix_ms",
            "analytics_enabled",
            "model",
            "reasoning_effort",
        ];
        let positions: Vec<_> = order
            .iter()
            .map(|key| raw.find(&format!("\"{key}\":")).unwrap())
            .collect();
        assert!(positions.is_sorted(), "{raw}");
        assert_eq!(turn.as_object().unwrap().len(), order.len());
        assert_eq!(turn["request_kind"], "turn");
        for key in [
            "agent_name",
            "history_initialization",
            "thread_source",
            "turn_trigger",
            "sandbox",
            "sandbox_mode",
        ] {
            assert!(turn.get(key).is_none(), "{key}");
        }
        assert_eq!(turn["root_turn_id"], turn["turn_id"]);
        assert_eq!(turn["analytics_enabled"], false);
        assert_eq!(turn["auto_review_enabled"], false);
        assert_eq!(turn["node_repl_auto_review_required"], true);
        assert_eq!(turn["model"], "gpt-6-astra");
        assert_eq!(turn["reasoning_effort"], "low");
        assert_eq!(metadata["root_turn_id"], turn["turn_id"]);
        assert_eq!(metadata["mcp_attribution"], r#"{"status":"none"}"#);
        assert_eq!(metadata["guardian_credits_requested"], "true");
    }

    #[test]
    fn compaction_reports_a_standalone_remote_compaction() {
        let session = NativeSession::new(&lite_provider(), BodyOperation::Compaction);
        let body = project(&session, turn_body());
        let turn = turn_metadata(&body["client_metadata"]);
        assert_eq!(turn["request_kind"], "compaction");
        assert!(turn.get("compaction").is_none());
        for absent in ["turn_trigger", "model", "reasoning_effort"] {
            assert!(turn.get(absent).is_none(), "{absent}");
        }
        assert!(body.get("access_programs").is_none());
        assert_eq!(body["input"][0]["type"], "additional_tools");
    }

    #[test]
    fn client_lineage_hints_do_not_prove_a_native_fork() {
        let mut provider = provider();
        provider.operation_metadata.insert(
            "parent_thread_transport_affinity".into(),
            "scoped-parent".into(),
        );
        provider.operation_metadata.insert(
            "forked_from_thread_transport_affinity".into(),
            "scoped-fork".into(),
        );
        let metadata = client_metadata(&NativeSession::new(&provider, BodyOperation::Http));
        let turn = turn_metadata(&metadata);
        for absent in [
            "parent_thread_id",
            "forked_from_thread_id",
            "history_initialization",
            "thread_source",
        ] {
            assert!(turn.get(absent).is_none(), "{absent}");
        }
        assert!(metadata.get("x-codex-parent-thread-id").is_none());
        assert_eq!(
            turn["context_window_id"],
            "00000000-0000-7000-8000-000000000003"
        );
    }

    #[test]
    fn native_identity_is_shared_and_scoped_not_raw_client_identity() {
        let provider = lite_provider();
        let session = NativeSession::new(&provider, BodyOperation::Http);
        let body = project(
            &session,
            json!({"model":"gpt-6-astra", "input":[], "client_metadata":{"session_id":"spoofed"}}),
        );
        let mut headers = vec![
            ("session_id".into(), "codec-raw-session".into()),
            ("conversation_id".into(), "codec-raw-conversation".into()),
            ("x-unapproved".into(), "not-native".into()),
            ("accept".into(), "text/event-stream".into()),
            ("content-type".into(), "application/json".into()),
        ];
        append_runtime_headers(&provider, &body, &session, &mut headers).unwrap();
        // Official 0.167.0 HTTP capture order.
        assert_eq!(
            headers
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            [
                "version",
                "x-codex-beta-features",
                "x-codex-window-id",
                "x-codex-turn-metadata",
                "x-openai-internal-codex-responses-lite",
                "x-codex-routing-hint",
                "x-client-request-id",
                "session-id",
                "thread-id",
                "accept",
                "content-type",
                "authorization",
                "originator",
                "user-agent",
            ]
        );
        let headers: BTreeMap<_, _> = headers.into_iter().collect();
        for (header, key) in [
            ("session-id", "session_id"),
            ("thread-id", "thread_id"),
            ("x-codex-window-id", "x-codex-window-id"),
            ("x-codex-turn-metadata", "x-codex-turn-metadata"),
        ] {
            assert_eq!(
                body["client_metadata"][key].as_str(),
                Some(headers[header].as_str())
            );
        }
        assert_eq!(
            headers["session-id"],
            "cc39dd30-b6dd-72f8-b778-d6c48c3af5fa"
        );
        assert_eq!(headers["thread-id"], headers["session-id"]);
        assert_eq!(headers["x-client-request-id"], headers["thread-id"]);
        assert_eq!(
            headers["x-codex-window-id"],
            format!("{}:0", headers["thread-id"])
        );
        assert_eq!(headers["x-codex-beta-features"], "remote_compaction_v2");
        assert_eq!(headers["version"], "0.167.0");
        assert_eq!(headers["originator"], "codex-tui");
        assert_eq!(
            headers["user-agent"],
            "codex-tui/0.167.0 (Linux Unknown; x86_64) unknown (codex-tui; 0.167.0)"
        );
        assert_eq!(headers["x-codex-routing-hint"], "model=gpt-6-astra");
        let turn = turn_metadata(&body["client_metadata"]);
        assert_eq!(turn["thread_id"], body["client_metadata"]["thread_id"]);
        assert_eq!(turn["session_id"], body["client_metadata"]["session_id"]);
        assert_ne!(turn["turn_id"], "canonical-turn");
        assert_eq!(
            uuid::Uuid::parse_str(turn["turn_id"].as_str().unwrap())
                .unwrap()
                .get_version_num(),
            7
        );
        assert!(turn.get("custom_hint").is_none());
        assert!(body["client_metadata"].get("custom_hint").is_none());

        // The opening handshake carries its turn snapshot; later frames on the
        // same socket carry their own snapshots, without a second handshake.
        let session = NativeSession::new(&provider, BodyOperation::WebSocket);
        let body = project(&session, json!({"model":"gpt-6-astra", "input":[]}));
        let mut headers = Vec::new();
        append_runtime_headers(&provider, &body, &session, &mut headers).unwrap();
        let headers: BTreeMap<_, _> = headers.into_iter().collect();
        assert_eq!(
            headers["x-codex-turn-metadata"],
            body["client_metadata"]["x-codex-turn-metadata"]
                .as_str()
                .unwrap()
        );
        assert!(!headers.contains_key("x-openai-internal-codex-responses-lite"));
        assert_eq!(headers["x-codex-beta-features"], "remote_compaction_v2");
    }

    #[test]
    fn scoped_identity_preserves_full_utf8_and_independent_turns_at_wire_boundary() {
        let mut provider = provider();
        let source = " 空白\0\r\n会话😀 ";
        provider
            .operation_metadata
            .insert("transport_affinity".into(), source.into());
        provider
            .operation_metadata
            .insert("context_window_transport_affinity".into(), source.into());
        let first = NativeSession::new(&provider, BodyOperation::Http);
        let first_metadata = client_metadata(&first);
        let reloaded = client_metadata(&NativeSession::new(&provider.clone(), BodyOperation::Http));
        assert_eq!(
            first_metadata["thread_id"],
            "5d42eca3-00ce-7fd9-879a-2f189010d915"
        );
        assert_eq!(first_metadata["thread_id"], reloaded["thread_id"]);
        assert_ne!(first_metadata["turn_id"], reloaded["turn_id"]);
        let turn = turn_metadata(&first_metadata);
        assert_eq!(
            turn["context_window_id"],
            "00000000-0000-7000-8000-000000000003"
        );
        assert!(turn.get("parent_turn_id").is_none());
        for operation in [
            BodyOperation::Http,
            BodyOperation::WebSocket,
            BodyOperation::Compaction,
            BodyOperation::Search,
            BodyOperation::Image,
        ] {
            let session = NativeSession::new(&provider, operation);
            let body = project(&session, json!({"model":"gpt-6-astra", "input":[]}));
            let mut headers = provider.client_headers.clone();
            append_runtime_headers(&provider, &body, &session, &mut headers).unwrap();
            let headers: BTreeMap<_, _> = headers.into_iter().collect();
            assert_eq!(headers["thread-id"], "5d42eca3-00ce-7fd9-879a-2f189010d915");
            assert_eq!(headers["x-client-request-id"], headers["thread-id"]);
            assert!(
                headers
                    .values()
                    .all(|value| !value.contains(['\0', '\r', '\n']))
            );
        }
        for other in [source.trim(), "空白\0\r\n会话😀", " ", "会话😀", "会话😁"] {
            provider
                .operation_metadata
                .insert("transport_affinity".into(), other.into());
            assert_ne!(
                client_metadata(&NativeSession::new(&provider, BodyOperation::Http))["thread_id"],
                first_metadata["thread_id"]
            );
        }
        let prefix = "会话😀".repeat(4096);
        assert_ne!(
            native_uuid("openai-codex", &format!("{prefix}a")),
            native_uuid("openai-codex", &format!("{prefix}b"))
        );
        provider
            .operation_metadata
            .insert("transport_affinity".into(), "".into());
        assert_ne!(
            client_metadata(&NativeSession::new(&provider, BodyOperation::Http))["thread_id"],
            client_metadata(&NativeSession::new(&provider, BodyOperation::Http))["thread_id"]
        );
    }

    #[test]
    fn checkpoint_and_logical_turn_identity_survive_model_retries() {
        let mut provider = provider();
        provider.operation_metadata.insert(
            "client_installation_id".into(),
            json!("00000000-0000-4000-8000-000000000001"),
        );
        provider.operation_metadata.insert(
            "client_conversation_id".into(),
            json!("00000000-0000-7000-8000-000000000004"),
        );
        provider.operation_metadata.insert(
            "logical_turn_id".into(),
            json!("00000000-0000-4000-8000-000000000002"),
        );
        provider
            .operation_metadata
            .insert("logical_turn_started_at_ms".into(), json!(123456_i64));
        let make = |provider: &ProviderSnapshot| {
            let mut session = NativeSession::new(provider, BodyOperation::Http);
            session.apply_host_identity(provider).unwrap();
            client_metadata(&session)
        };
        let first = make(&provider);
        let retry = make(&provider);
        assert_eq!(
            uuid::Uuid::parse_str(first["turn_id"].as_str().unwrap())
                .unwrap()
                .get_version_num(),
            7
        );
        assert_eq!(first["turn_id"], retry["turn_id"]);
        assert_eq!(first["thread_id"], retry["thread_id"]);
        assert_eq!(turn_metadata(&first)["turn_started_at_unix_ms"], 123456);
        provider
            .operation_metadata
            .insert("context_window_index".into(), json!(1));
        provider.operation_metadata.insert(
            "context_window_id".into(),
            json!("00000000-0000-7000-8000-000000000005"),
        );
        let compacted = make(&provider);
        assert_eq!(compacted["thread_id"], first["thread_id"]);
        assert_eq!(turn_metadata(&compacted)["window_number"], 1);
        assert_ne!(compacted["x-codex-window-id"], first["x-codex-window-id"]);
        provider.operation_metadata.remove("context_window_index");
        let mut missing = NativeSession::new(&provider, BodyOperation::Http);
        assert!(missing.apply_host_identity(&provider).is_err());
    }

    #[test]
    fn scoped_sessions_are_stable_but_native_turns_are_distinct() {
        let mut provider = provider();
        let first = client_metadata(&NativeSession::new(&provider, BodyOperation::Http));
        let second = client_metadata(&NativeSession::new(&provider, BodyOperation::Http));
        assert_eq!(first["session_id"], second["session_id"]);
        assert_eq!(first["thread_id"], second["thread_id"]);
        assert_ne!(first["turn_id"], second["turn_id"]);
        provider
            .operation_metadata
            .insert("transport_affinity".into(), "other-principal-target".into());
        let other = client_metadata(&NativeSession::new(&provider, BodyOperation::Http));
        assert_ne!(first["session_id"], other["session_id"]);
        provider.operation_metadata.remove("transport_affinity");
        let isolated = client_metadata(&NativeSession::new(&provider, BodyOperation::Http));
        let another = client_metadata(&NativeSession::new(&provider, BodyOperation::Http));
        assert_ne!(isolated["session_id"], another["session_id"]);
    }
}
