use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use stravia_runtime_contract::protocol::{
    ids::OPEN_RESPONSES_2026_04_24,
    ir::{AiResponse, ServerToolUsage, Usage},
};
use stravia_vendor_sdk::{
    GuestHost, ProviderSnapshot, SearchRequest, SearchResponse, SearchSource, WsMessage, WsRequest,
};

use super::auth::account_id_from_jwt;
use super::{
    CLIENT_VERSION, configured_websocket_url, endpoint, ensure_success, invalid,
    require_codex_protocol, required_model, retryable, secret,
};

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
// Retire reusable sessions before the upstream's one-hour connection limit.
const WEBSOCKET_REUSE_MAX_AGE_MILLIS: u64 = 55 * 60 * 1000;
const ORIGINATOR: &str = "codex_cli_rs";

/// One identity projection shared by the HTTP body, HTTP headers and WS handshake.
/// Only the host-scoped affinity may select a reusable native session.
pub(super) struct NativeSession {
    metadata: Value,
    headers: Vec<(String, String)>,
}

impl NativeSession {
    pub(super) fn new(provider: &ProviderSnapshot) -> Self {
        let thread_id = scoped_affinity(provider, "transport_affinity")
            .map(|affinity| native_uuid("openai-codex", affinity))
            .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
        let session_id = thread_id.clone();
        // The SDK has no trusted native window ordinal. Keep the existing
        // transport-scoped lifecycle; do not invent a compaction counter.
        let window_id = format!("{thread_id}:0");
        // Canonical client turn/request labels are not host-scoped operation
        // identities. A newly constructed native turn remains independent.
        let turn_id = uuid::Uuid::now_v7().to_string();
        let mut metadata = json!({
            "session_id": session_id,
            "thread_id": thread_id,
            "x-codex-window-id": window_id,
            "turn_id": turn_id,
        });
        for (affinity_key, metadata_key) in [
            (
                "parent_thread_transport_affinity",
                "x-codex-parent-thread-id",
            ),
            (
                "forked_from_thread_transport_affinity",
                "forked_from_thread_id",
            ),
        ] {
            if let Some(affinity) = scoped_affinity(provider, affinity_key) {
                metadata[metadata_key] = native_uuid("openai-codex", affinity).into();
            }
        }
        if let Some(affinity) = scoped_affinity(provider, "context_window_transport_affinity") {
            metadata["context_window_id"] =
                native_uuid("openai-codex:context-window", affinity).into();
        }
        let mut headers = vec![
            ("session-id".into(), session_id),
            ("thread-id".into(), thread_id),
            ("x-codex-window-id".into(), window_id),
        ];
        if let Some(parent_thread) = metadata
            .get("x-codex-parent-thread-id")
            .and_then(Value::as_str)
        {
            headers.push(("x-codex-parent-thread-id".into(), parent_thread.into()));
        }
        let mut turn_metadata = serde_json::Map::new();
        for name in ["session_id", "thread_id", "turn_id"] {
            if let Some(value) = metadata.get(name) {
                turn_metadata.insert(name.into(), value.clone());
            }
        }
        if let Some(parent_thread) = metadata.get("x-codex-parent-thread-id") {
            turn_metadata.insert("parent_thread_id".into(), parent_thread.clone());
        }
        // These official lineage fields occur only inside the turn snapshot,
        // not as independent flat client_metadata compatibility keys.
        for name in ["forked_from_thread_id", "context_window_id"] {
            if let Some(value) = metadata
                .as_object_mut()
                .and_then(|metadata| metadata.remove(name))
            {
                turn_metadata.insert(name.into(), value);
            }
        }
        turn_metadata.insert("window_id".into(), metadata["x-codex-window-id"].clone());
        let turn_metadata = Value::Object(turn_metadata).to_string();
        headers.push(("x-codex-turn-metadata".into(), turn_metadata.clone()));
        metadata["x-codex-turn-metadata"] = turn_metadata.into();
        Self { metadata, headers }
    }

    pub(super) fn apply_body(&self, body: &mut Value) {
        body["client_metadata"] = self.metadata.clone();
    }

    pub(super) fn resolve_identity(
        &mut self,
        host: &GuestHost,
        provider: &ProviderSnapshot,
    ) -> Result<(), stravia_vendor_sdk::PluginError> {
        let (mut root, mut identities) = read_identities(host)?;
        let key = credential_key(provider)?;
        let created = !identities.contains_key(&key);
        let identity = identities.entry(key).or_insert_with(NativeIdentity::new);
        if uuid::Uuid::parse_str(&identity.installation_id).is_err() {
            return Err(invalid("Codex native installation identity is invalid"));
        }
        let installation = identity.installation_id.clone();
        if created {
            write_identities(host, &mut root, identities)?;
        }
        self.headers
            .push(("x-codex-installation-id".into(), installation.clone()));
        self.metadata["x-codex-installation-id"] = installation.clone().into();
        let mut turn_metadata: Value = serde_json::from_str(
            self.metadata["x-codex-turn-metadata"]
                .as_str()
                .expect("native turn metadata"),
        )
        .map_err(|_| invalid("Codex native turn metadata is invalid"))?;
        turn_metadata["installation_id"] = installation.into();
        let turn_metadata = turn_metadata.to_string();
        set_header(
            &mut self.headers,
            "x-codex-turn-metadata",
            turn_metadata.clone(),
        );
        self.metadata["x-codex-turn-metadata"] = turn_metadata.into();
        Ok(())
    }
}

fn scoped_affinity<'a>(provider: &'a ProviderSnapshot, key: &str) -> Option<&'a str> {
    provider
        .operation_metadata
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

/// UUIDv7 bit representation only: hash-derived bits are not a wall clock and
/// do not preserve official UUIDv7 creation-time ordering.
fn native_uuid(namespace: &str, affinity: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(namespace.as_bytes());
    hash.update([0]);
    hash.update(affinity.as_bytes());
    let digest = hash.finalize();
    let mut bytes: [u8; 16] = digest[..16].try_into().expect("digest has 16 bytes");
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}

#[derive(Serialize, Deserialize)]
// https://github.com/openai/codex/blob/main/codex-rs/core/src/installation_id.rs
// 官方安装 UUID 持久化；新操作没有可信的旧 turn 可复用。
struct NativeIdentity {
    installation_id: String,
}

impl NativeIdentity {
    fn new() -> Self {
        Self {
            installation_id: uuid::Uuid::new_v4().to_string(),
        }
    }
}

type Identities = std::collections::BTreeMap<String, NativeIdentity>;

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

fn read_identities(
    host: &GuestHost,
) -> Result<(serde_json::Map<String, Value>, Identities), stravia_vendor_sdk::PluginError> {
    let mut root = match host.read_private_state()? {
        Some(bytes) => serde_json::from_slice::<serde_json::Map<String, Value>>(&bytes)
            .map_err(|_| invalid("Codex native identity state is invalid"))?,
        None => Default::default(),
    };
    let identities = root
        .remove("native_identities")
        .map(serde_json::from_value)
        .transpose()
        .map_err(|_| invalid("Codex native identity state is invalid"))?
        .unwrap_or_default();
    Ok((root, identities))
}

fn write_identities(
    host: &GuestHost,
    root: &mut serde_json::Map<String, Value>,
    identities: Identities,
) -> Result<(), stravia_vendor_sdk::PluginError> {
    root.insert(
        "native_identities".into(),
        serde_json::to_value(identities)
            .map_err(|_| invalid("Codex native identity state could not be encoded"))?,
    );
    host.write_private_state(
        &serde_json::to_vec(root)
            .map_err(|_| invalid("Codex native identity state could not be encoded"))?,
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum BodyOperation {
    Http,
    WebSocket,
    Search,
    Image,
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

// Closed wire contracts: codex-api/src/common.rs ResponsesApiRequest and
// ResponseCreateWsRequest; installed official codex-cli 0.159.2 HTTP capture.
// `instructions` is emitted by that release (newer main uses input items).
const RESPONSE_FIELDS: &[&str] = &[
    "model",
    "instructions",
    "input",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "reasoning",
    "store",
    "stream",
    "include",
    "service_tier",
    "prompt_cache_key",
    "text",
    "client_metadata",
];

pub(super) fn prepare_body(body: &mut Value) -> Result<(), stravia_vendor_sdk::PluginError> {
    prepare_operation_body(body, BodyOperation::Http)
}

pub(super) fn prepare_operation_body(
    body: &mut Value,
    operation: BodyOperation,
) -> Result<(), stravia_vendor_sdk::PluginError> {
    let object = body
        .as_object_mut()
        .ok_or_else(|| invalid("Codex Responses request body must be an object"))?;
    object.retain(|key, value| {
        !value.is_null()
            && (RESPONSE_FIELDS.contains(&key.as_str())
                || (operation == BodyOperation::WebSocket
                    && matches!(key.as_str(), "previous_response_id" | "generate")))
    });
    object.insert("store".into(), Value::Bool(false));
    // The native endpoint rejects stream:false. Client delivery remains
    // host-owned; even a unary client request uses the official SSE wire.
    object.insert("stream".into(), Value::Bool(true));
    // Filter control envelopes, never user input, tool schemas, grammars or
    // image/search business data.
    retain_control_fields(object.get_mut("reasoning"), &["effort", "summary"]);
    if let Some(text) = object.get_mut("text").and_then(Value::as_object_mut) {
        text.retain(|key, _| matches!(key.as_str(), "verbosity" | "format"));
        retain_control_fields(
            text.get_mut("format"),
            &["type", "strict", "schema", "name"],
        );
    }
    // Client metadata has one authority: NativeSession::apply_body.
    object.remove("client_metadata");
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

pub(super) fn append_runtime_headers(
    provider: &ProviderSnapshot,
    body: &Value,
    session: &NativeSession,
    operation: BodyOperation,
    headers: &mut Vec<(String, String)>,
) -> Result<(), stravia_vendor_sdk::PluginError> {
    // Codec/ingress headers are not native identity authority. Build this
    // target's complete header set rather than retaining alternate session keys.
    headers.retain(|(name, _)| {
        name.eq_ignore_ascii_case("content-type") || name.eq_ignore_ascii_case("accept")
    });
    append_identity_headers(provider, headers)?;
    let websocket = matches!(operation, BodyOperation::WebSocket);
    for (name, value) in &session.headers {
        // 连接跨 turn 复用；动态快照和 sticky state 只随每帧 client_metadata 发送。
        if websocket
            && matches!(
                name.as_str(),
                "x-codex-turn-metadata" | "x-codex-turn-state"
            )
        {
            continue;
        }
        set_header(headers, name, value.clone());
    }
    // rust-v0.162.1 binds this header to the thread on both Responses HTTP
    // (codex-api/endpoint/responses.rs) and WS (core/client.rs).
    let request_id = session.metadata["thread_id"]
        .as_str()
        .expect("native thread ID");
    set_header(headers, "x-client-request-id", request_id.to_owned());
    set_header(headers, "x-codex-routing-hint", routing_hint(body)?);
    Ok(())
}

fn set_header(headers: &mut Vec<(String, String)>, name: &str, value: String) {
    headers.retain(|(candidate, _)| !candidate.eq_ignore_ascii_case(name));
    headers.push((name.into(), value));
}

pub(super) fn append_identity_headers(
    provider: &ProviderSnapshot,
    headers: &mut Vec<(String, String)>,
) -> Result<(), stravia_vendor_sdk::PluginError> {
    let access_token = secret(provider, "access_token")?;
    set_header(headers, "authorization", format!("Bearer {access_token}"));
    set_header(headers, "originator", ORIGINATOR.into());
    set_header(
        headers,
        "user-agent",
        format!("{ORIGINATOR}/{CLIENT_VERSION}"),
    );
    set_header(headers, "version", CLIENT_VERSION.into());
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
    Ok(())
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
    configured_websocket_url(provider).is_some()
        || url::Url::parse(&provider.base_url).is_ok_and(|url| {
            url.scheme() == "https"
                && url.host_str() == Some("chatgpt.com")
                && url.port_or_known_default() == Some(443)
        })
}

/// The native continuation the frame actually asks the upstream to resume.
fn request_continuation_id(body: &Value) -> Option<&str> {
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
    let mut headers = Vec::new();
    append_runtime_headers(
        provider,
        body,
        session,
        BodyOperation::WebSocket,
        &mut headers,
    )?;
    set_header(
        &mut headers,
        "openai-beta",
        "responses_websockets=2026-02-06".into(),
    );

    let url = if let Some(url) = configured_websocket_url(provider) {
        url.to_owned()
    } else {
        let mut url = url::Url::parse(&endpoint(&provider.base_url, "/responses"))
            .map_err(|_| invalid("Codex base URL is invalid"))?;
        if url.scheme() != "https"
            || url.host_str() != Some("chatgpt.com")
            || url.port_or_known_default() != Some(443)
        {
            return Err(invalid(
                "custom Codex base URLs require an authorized websocket_url option",
            ));
        }
        url.set_scheme("wss")
            .map_err(|_| invalid("Codex WebSocket URL could not be constructed"))?;
        url.to_string()
    };
    let continuation_id = request_continuation_id(body).map(str::to_owned);
    let continuation_requested = continuation_id.is_some();
    let connection = host.ws_connect_with_compression(
        WsRequest {
            url,
            headers,
            protocols: Vec::new(),
            continuation_id,
            reuse_max_age_millis: Some(WEBSOCKET_REUSE_MAX_AGE_MILLIS),
        },
        stravia_vendor_sdk::WsCompression::PermessageDeflate(
            stravia_vendor_sdk::CompressionNegotiation::Preferred,
        ),
    )?;
    let mut frame = body.clone();
    let object = frame
        .as_object_mut()
        .ok_or_else(|| invalid("Codex Responses request body must be an object"))?;
    // This body was projected as WebSocket before the scoped identity was
    // attached. The WS schema includes stream and service_tier as well.
    object.insert("type".into(), Value::String("response.create".into()));
    host.emit_started()?;
    connection
        .send(&WsMessage::Text(serde_json::to_string(&frame).map_err(
            |_| invalid("Codex WebSocket request could not be encoded"),
        )?))?;

    stravia_vendor_common::common::decode_ai_response_websocket_with_error_classifier(
        host,
        &OPEN_RESPONSES_2026_04_24.to_string(),
        connection,
        preserve_upstream_errors,
        continuation_requested,
        |value, saw_response_event| {
            super::classify_codex_websocket_error(value, saw_response_event, continuation_requested)
        },
    )
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
    prepare_operation_body(&mut body, BodyOperation::Search)?;
    let mut session = NativeSession::new(provider);
    session.resolve_identity(host, provider)?;
    session.apply_body(&mut body);
    let mut headers = vec![
        ("content-type".into(), "application/json".into()),
        ("accept".into(), "text/event-stream".into()),
    ];
    append_runtime_headers(
        provider,
        &body,
        &session,
        BodyOperation::Search,
        &mut headers,
    )?;
    host.emit_started()?;
    let response = host.http_start(stravia_vendor_sdk::wit::types::HttpRequest {
        method: "POST".into(),
        url: endpoint(&provider.base_url, "/responses"),
        headers,
        body: serde_json::to_vec(&body)
            .map_err(|_| invalid("Codex Search request could not be encoded"))?,
    })?;
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
        prepare_body(&mut body).unwrap();
        assert_eq!(body["tools"][0]["type"], "web_search");
        assert_eq!(body["tool_choice"], json!({"type":"web_search"}));
        body["tool_choice"] = json!({"type":"code_interpreter"});
        prepare_body(&mut body).unwrap();
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
            BodyOperation::Search,
            BodyOperation::Image,
        ] {
            let mut body = original.clone();
            prepare_operation_body(&mut body, operation).unwrap();
            for absent in [
                "private_parameter",
                "metadata",
                "prompt_cache_retention",
                "client_metadata",
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
                ("thread_id".into(), "not-a-scoped-key".into()),
            ]),
        }
    }

    #[test]
    fn scoped_parent_thread_and_fork_lineage_match_previous_native_thread_identity() {
        let mut provider = provider();
        provider.operation_metadata.insert(
            "parent_thread_transport_affinity".into(),
            "scoped-parent".into(),
        );
        provider.operation_metadata.insert(
            "forked_from_thread_transport_affinity".into(),
            "scoped-fork".into(),
        );
        provider.operation_metadata.insert(
            "context_window_transport_affinity".into(),
            "scoped-window".into(),
        );
        let child = NativeSession::new(&provider);
        let mut earlier_parent = provider.clone();
        earlier_parent
            .operation_metadata
            .insert("transport_affinity".into(), "scoped-parent".into());
        let parent = NativeSession::new(&earlier_parent);
        assert_eq!(
            child.metadata["x-codex-parent-thread-id"],
            parent.metadata["thread_id"]
        );
        let turn: Value =
            serde_json::from_str(child.metadata["x-codex-turn-metadata"].as_str().unwrap())
                .unwrap();
        assert_eq!(turn["parent_thread_id"], parent.metadata["thread_id"]);
        assert_eq!(
            turn["forked_from_thread_id"],
            native_uuid("openai-codex", "scoped-fork")
        );
        assert!(turn.get("root_turn_id").is_none());
        assert!(uuid::Uuid::parse_str(turn["context_window_id"].as_str().unwrap()).is_ok());
        assert!(child.metadata.get("forked_from_thread_id").is_none());
        assert!(child.metadata.get("context_window_id").is_none());
        provider.operation_metadata.insert(
            "parent_thread_transport_affinity".into(),
            "other-principal-target-parent".into(),
        );
        let other = NativeSession::new(&provider);
        assert_ne!(
            other.metadata["x-codex-parent-thread-id"],
            child.metadata["x-codex-parent-thread-id"]
        );
    }

    #[test]
    fn native_identity_is_shared_and_scoped_not_raw_client_identity() {
        let provider = provider();
        let session = NativeSession::new(&provider);
        let mut body = json!({"model":"gpt-6-astra", "client_metadata":{"session_id":"spoofed"}});
        session.apply_body(&mut body);
        let mut headers = vec![
            ("session_id".into(), "codec-raw-session".into()),
            ("conversation_id".into(), "codec-raw-conversation".into()),
            ("x-unapproved".into(), "not-native".into()),
        ];
        append_runtime_headers(
            &provider,
            &body,
            &session,
            BodyOperation::Http,
            &mut headers,
        )
        .unwrap();
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
        assert!(!headers.contains_key("x-codex-beta-features"));
        assert!(!headers.contains_key("x-codex-turn-state"));
        assert!(!headers.contains_key("x-openai-subagent"));
        assert!(!headers.contains_key("session_id"));
        assert!(!headers.contains_key("conversation_id"));
        assert!(!headers.contains_key("x-unapproved"));
        assert!(!headers.contains_key("openai-beta"));
        let turn: Value = serde_json::from_str(&headers["x-codex-turn-metadata"]).unwrap();
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
        let first = NativeSession::new(&provider);
        let reloaded = NativeSession::new(&provider.clone());
        assert_eq!(
            first.metadata["thread_id"],
            "5d42eca3-00ce-7fd9-879a-2f189010d915"
        );
        assert_eq!(first.metadata["thread_id"], reloaded.metadata["thread_id"]);
        assert_ne!(first.metadata["turn_id"], reloaded.metadata["turn_id"]);
        let turn: Value =
            serde_json::from_str(first.metadata["x-codex-turn-metadata"].as_str().unwrap())
                .unwrap();
        assert_eq!(
            turn["context_window_id"],
            "8d48e1ac-0b84-75f1-b2c8-f17313c5b686"
        );
        assert!(turn.get("parent_turn_id").is_none());
        assert!(turn.get("root_turn_id").is_none());
        for operation in [
            BodyOperation::Http,
            BodyOperation::WebSocket,
            BodyOperation::Search,
            BodyOperation::Image,
        ] {
            let mut body = json!({"model":"gpt-6-astra"});
            first.apply_body(&mut body);
            let mut headers = provider.client_headers.clone();
            append_runtime_headers(&provider, &body, &first, operation, &mut headers).unwrap();
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
                NativeSession::new(&provider).metadata["thread_id"],
                first.metadata["thread_id"]
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
            NativeSession::new(&provider).metadata["thread_id"],
            NativeSession::new(&provider).metadata["thread_id"]
        );
    }

    #[test]
    fn scoped_sessions_are_stable_but_native_turns_are_distinct() {
        let mut provider = provider();
        let first = NativeSession::new(&provider);
        let second = NativeSession::new(&provider);
        assert_eq!(first.metadata["session_id"], second.metadata["session_id"]);
        assert_eq!(first.metadata["thread_id"], second.metadata["thread_id"]);
        assert_ne!(first.metadata["turn_id"], second.metadata["turn_id"]);
        provider
            .operation_metadata
            .insert("transport_affinity".into(), "other-principal-target".into());
        let other = NativeSession::new(&provider);
        assert_ne!(first.metadata["session_id"], other.metadata["session_id"]);
        provider.operation_metadata.remove("transport_affinity");
        let isolated = NativeSession::new(&provider);
        let another = NativeSession::new(&provider);
        assert_ne!(
            isolated.metadata["session_id"],
            another.metadata["session_id"]
        );
    }
}
