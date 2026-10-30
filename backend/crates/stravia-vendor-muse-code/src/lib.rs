//! First-party Meta Responses transport for Muse Code subscriptions.
//! Contract: oh-my-pi v18.10.4 muse-code/meta provider and auth rules.
mod auth;
mod connection;
mod messages {
    include!(concat!(env!("OUT_DIR"), "/messages.rs"));
}

use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_vendor_common::common;
use stravia_vendor_sdk::*;

const ID: &str = "muse-code";
const BASE: &str = "https://api.meta.ai/v1";
const PROTOCOL: &str = "open-responses/responses/2026-04-24";
// 对齐契约客户端 OMP 18.10.4 所在 Bun 1.4.3 运行时的 fetch 默认 UA。Meta 会把无 UA 的
// 设备登录请求 302 到 facebook.com/unsupportedbrowser，宿主按未授权 origin 拒绝该跳转。
const CLIENT_USER_AGENT: &str = "Bun/1.4.3";

fn invalid(message: &str) -> PluginError {
    common::plugin_error(ErrorKind::Invalid, message)
}
fn auth_error(message: &str) -> PluginError {
    common::plugin_error(ErrorKind::Auth, message)
}
fn credential<'a>(provider: &'a ProviderSnapshot, key: &str) -> Option<&'a str> {
    provider
        .credentials
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
}
fn key(provider: &ProviderSnapshot) -> Result<&str, PluginError> {
    credential(provider, "apiKey")
        .filter(|key| !key.contains(['\r', '\n']))
        .ok_or_else(|| auth_error("Muse Code inference key is missing or invalid; sign in again"))
}
fn route(channel: &str, provider: &ProviderSnapshot) -> Result<(), PluginError> {
    if provider.provider_id != ID || channel != "subscription" || provider.channel != channel {
        return Err(common::unsupported("route", ID, channel));
    }
    if provider.base_url.trim_end_matches('/') != BASE {
        return Err(invalid("Muse Code requires https://api.meta.ai/v1"));
    }
    Ok(())
}
fn headers(token: &str) -> Vec<(String, String)> {
    vec![
        ("authorization".into(), format!("Bearer {token}")),
        ("x-api-version".into(), "1.0.0".into()),
        ("accept".into(), "application/json".into()),
        ("user-agent".into(), CLIENT_USER_AGENT.into()),
    ]
}

fn seed(id: &str) -> Option<(&'static str, bool)> {
    Some(match id {
        "muse-spark-1.1" => ("Muse Spark 1.1", false),
        "muse-spark-1.2" => ("Muse Spark 1.2", false),
        "muse-spark-1.2-contributor" => ("Muse Spark 1.2 (C)", false),
        "muse-spark-1.3" => ("Muse Spark 1.3", true),
        "muse-spark-1.3-contributor" => ("Muse Spark 1.3 (C)", false),
        "muse-wonder-1.0" => ("Muse Wonder", true),
        "muse-wonder-1.0-contributor" => ("Muse Wonder (C)", true),
        "muse-wonder-full-1.0" => ("Muse Wonder Full", true),
        "muse-wonder-full-1.0-contributor" => ("Muse Wonder Full (C)", true),
        _ => return None,
    })
}
fn discover(host: &GuestHost, provider: &ProviderSnapshot) -> Result<OperationOutput, PluginError> {
    let response = host.http_start(HttpRequest {
        method: "GET".into(),
        url: format!("{BASE}/models"),
        headers: headers(key(provider)?),
        body: Vec::new(),
    })?;
    let status = response.status()?;
    let response_headers = response.headers()?;
    let bytes = read_http_body(&response, 4 * 1024 * 1024)?;
    if !(200..300).contains(&status) {
        return Err(common::upstream_error(status, &response_headers, &bytes));
    }
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|_| invalid("Muse model list is not valid JSON"))?;
    let entries = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("Muse model list has no data array"))?;
    let mut models = Vec::with_capacity(entries.len());
    for entry in entries {
        let id = entry
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| invalid("Muse model row has no id"))?;
        let mut metadata = BTreeMap::new();
        if let Some(created) = entry.get("created") {
            metadata.insert("created".into(), created.clone());
        }
        let mut capabilities = Vec::new();
        let name = if let Some((name, max)) = seed(id) {
            capabilities.extend(["reasoning".into(), "tool_call".into(), "image_input".into()]);
            metadata.insert("limit".into(), json!({"context":1048576,"output":131072}));
            metadata.insert("input_modalities".into(), json!(["text", "image"]));
            metadata.insert(
                "reasoning_efforts".into(),
                if max {
                    json!(["minimal", "low", "medium", "high", "xhigh", "max"])
                } else {
                    json!(["minimal", "low", "medium", "high", "xhigh"])
                },
            );
            metadata.insert("capability_source".into(), json!("oh-my-pi/v18.10.4"));
            name.to_owned()
        } else {
            metadata.insert("capabilities_verified".into(), json!(false));
            id.to_owned()
        };
        models.push(DiscoveredModel {
            id: id.into(),
            display_name: name,
            family: None,
            selector: None,
            capabilities,
            metadata,
        });
    }
    Ok(OperationOutput::Discover(DiscoverResponse {
        models,
        next_cursor: None,
    }))
}
// Meta's CreateResponse schema is not the complete OpenAI Responses surface.
// https://dev.meta.ai/docs/api-reference/responses/schemas#create-response
// https://dev.meta.ai/docs/protocols/responses#openai-compatibility
fn project_body(body: &mut Value, model: &str) -> Result<(), PluginError> {
    let object = body
        .as_object_mut()
        .ok_or_else(|| invalid("Responses body must be an object"))?;
    object.retain(|name, _| {
        matches!(
            name.as_str(),
            "model"
                | "input"
                | "instructions"
                | "stream"
                | "tools"
                | "tool_choice"
                | "parallel_tool_calls"
                | "reasoning"
                | "max_output_tokens"
                | "text"
                | "temperature"
                | "top_p"
                | "frequency_penalty"
                | "presence_penalty"
                | "store"
                | "previous_response_id"
                | "include"
                | "metadata"
                | "safety_identifier"
                | "prompt_cache_key"
                | "prompt_cache_retention"
                | "max_tool_calls"
                | "truncation"
        )
    });
    // Only auto is representable. Unsupported choice constraints become native
    // automatic selection; unsupported tool declarations are omitted, not recast
    // as function tools. Leave function parameters intact, including nullable types.
    if object.get("tool_choice").and_then(Value::as_str) != Some("auto") {
        object.remove("tool_choice");
    }
    if let Some(tools) = object.get_mut("tools").and_then(Value::as_array_mut) {
        tools.retain(|tool| tool.get("type").and_then(Value::as_str) == Some("function"));
        for tool in tools {
            if let Some(tool) = tool.as_object_mut() {
                tool.retain(|name, _| {
                    matches!(
                        name.as_str(),
                        "type" | "name" | "description" | "parameters" | "strict"
                    )
                });
            }
        }
    }
    let known = seed(model);
    if let Some(reasoning) = object.get_mut("reasoning").and_then(Value::as_object_mut) {
        reasoning.retain(|name, _| matches!(name.as_str(), "effort" | "summary"));
        let supports_max = known.is_some_and(|(_, max)| max);
        if reasoning
            .get("effort")
            .and_then(Value::as_str)
            .is_some_and(|effort| {
                !matches!(effort, "minimal" | "low" | "medium" | "high" | "xhigh")
                    && !(effort == "max" && supports_max)
            })
        {
            // Muse cannot disable reasoning or express a token thinking budget.
            // Omission restores its documented default internal reasoning.
            reasoning.remove("effort");
        }
        if reasoning.get("summary").is_some_and(|value| {
            !value.is_null() && !matches!(value.as_str(), Some("auto" | "concise" | "detailed"))
        }) {
            reasoning.remove("summary");
        }
    }
    if object
        .get("reasoning")
        .and_then(Value::as_object)
        .is_some_and(|value| value.is_empty())
    {
        object.remove("reasoning");
    }
    if let Some(limit) = object.get_mut("max_output_tokens")
        && let Some(tokens) = limit.as_u64()
    {
        // Meta's minimum includes reasoning and visible output. Only known
        // models have a verified upper bound; discovery does not invent one.
        *limit = json!(if known.is_some() {
            tokens.clamp(16, 131072)
        } else {
            tokens.max(16)
        });
    }
    // Sampling is documented for Muse Spark/Wonder, not arbitrary discovered IDs.
    if known.is_none() {
        for name in [
            "temperature",
            "top_p",
            "frequency_penalty",
            "presence_penalty",
        ] {
            object.remove(name);
        }
    }
    if let Some(text) = object.get_mut("text").and_then(Value::as_object_mut) {
        text.retain(|name, _| name == "format");
    }
    // Meta does not implement automatic truncation or background+stream. The
    // plugin is a foreground streaming transport and replays full history.
    if object.contains_key("truncation") {
        object.insert("truncation".into(), json!("disabled"));
    }
    object.insert("stream".into(), json!(true));
    // Storage is opt-in, never an incidental effect of an OpenAI default.
    object.entry("store").or_insert(json!(false));
    let continuation = object
        .get("previous_response_id")
        .is_some_and(|value| !value.is_null());
    let mut include = match object.remove("include") {
        Some(Value::Array(values)) => values,
        _ => Vec::new(),
    };
    include.retain(|value| {
        matches!(
            value.as_str(),
            Some(
                "web_search_call.action.sources"
                    | "message.input_image.image_url"
                    | "computer_call_output.output.image_url"
            )
        )
    });
    // Encrypted replay and server-managed history are mutually exclusive.
    if !continuation {
        include.push(json!("reasoning.encrypted_content"));
    }
    if !include.is_empty() {
        object.insert("include".into(), Value::Array(include));
    }
    Ok(())
}
fn infer(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    mut request: AiRequest,
) -> Result<OperationOutput, PluginError> {
    request.model = provider
        .model
        .as_deref()
        .filter(|m| !m.trim().is_empty())
        .ok_or_else(|| invalid("Muse inference requires a model"))?
        .into();
    request.stream.enabled = true;
    request.parameters.store.get_or_insert(false);
    request.parameters.native_tools.clear();
    if matches!(
        request.tool_choice,
        Some(stravia_runtime_contract::protocol::ir::ToolChoice::Native(
            _
        ))
    ) {
        request.tool_choice = None;
    }

    // Budget/adaptive controls from another target have no Muse equivalent.
    if !matches!(
        request.reasoning.target_control,
        None | Some(
            stravia_runtime_contract::thinking::TargetThinkingControl::Effort { .. }
                | stravia_runtime_contract::thinking::TargetThinkingControl::Disabled
        )
    ) {
        request.reasoning.target_control = None;
    }
    let mut encoded = common::encode_inference_request(PROTOCOL, request)?;
    project_body(
        &mut encoded.body,
        provider.model.as_deref().unwrap_or_default(),
    )?;
    let mut headers = headers(key(provider)?);
    headers.push(("content-type".into(), "application/json".into()));
    headers.retain(|(name, _)| name != "accept");
    headers.push(("accept".into(), "text/event-stream".into()));
    let body = serde_json::to_vec(&encoded.body)
        .map_err(|_| invalid("Responses request could not be encoded"))?;
    host.emit_started()?;
    let response = host.http_start(HttpRequest {
        method: "POST".into(),
        url: format!("{BASE}/responses"),
        headers,
        body,
    })?;
    common::decode_inference(host, PROTOCOL, response)
}

pub fn select_protocol(
    operation: Operation,
    channel: &str,
    provider: &ProviderSnapshot,
    _request: &AiRequest,
) -> Result<String, PluginError> {
    route(channel, provider)?;
    if operation != Operation::Infer {
        return Err(common::unsupported(operation.as_str(), ID, channel));
    }
    Ok(PROTOCOL.into())
}
pub fn execute(
    host: &GuestHost,
    operation: Operation,
    channel: &str,
    input: OperationInput,
) -> Result<OperationOutput, PluginError> {
    if operation != input.operation() {
        return Err(invalid("operation input does not match operation"));
    }
    route(channel, input.provider())?;
    match input {
        OperationInput::Infer { provider, request } => infer(host, &provider, request),
        OperationInput::Discover { provider, request } if request.cursor.is_none() => {
            discover(host, &provider)
        }
        OperationInput::Auth { provider, request } => {
            auth::execute(host, &provider, request).map(OperationOutput::Auth)
        }
        OperationInput::ConnectionCheck { provider, .. } => {
            connection::check(host, &provider).map(OperationOutput::ConnectionCheck)
        }
        other => Err(common::unsupported(other.operation().as_str(), ID, channel)),
    }
}
pub fn descriptor() -> VendorDescriptor {
    let capabilities = BTreeSet::from([
        Capability::Infer,
        Capability::ModelDiscovery,
        Capability::AuthOauth,
        Capability::ConnectionCheck,
    ]);
    let channel = ChannelDescriptor {
        id: "subscription".into(),
        name: messages::subscription(),
        description: Some(messages::subscription_description()),
        auth: Some(AuthDescriptor {
            flow: AuthFlow::DeviceCode,
            callback: None,
            manual_input: None,
        }),
        protocol: Some("open-responses".into()),
        protocols: Vec::new(),
        default_base_url: Some(BASE.into()),
        default_models_source: None,
        consumes_catalog_models: false,
        capabilities: capabilities.clone(),
        client_header_names: BTreeSet::new(),
        model_capabilities: BTreeSet::new(),
    };
    VendorDescriptor {
        vendor_id: ID.into(),
        version: semver::Version::parse(env!("CARGO_PKG_VERSION")).expect("package semver"),
        display_name: "Muse Code".into(),
        description: Some(
            "Meta Muse Code subscription device login; automatic tool selection only.".into(),
        ),
        authors: vec!["Stravia".into()],
        canonical_format_version: CANONICAL_FORMAT_VERSION,
        kind: VendorKind::Dedicated,
        providers: vec![ProviderDescriptor {
            provider_id: ID.into(),
            catalog_id: Some("meta".into()),
            icon_svg: Some(
                include_str!("../../../../frontend/stravia-webui/src/assets/icons/meta.svg")
                    .to_owned(),
            ),
            display_name: "Muse Code".into(),
            description: Some(messages::compatibility().english_text().to_owned()),
            channels: vec![channel],
            capabilities,
            website: Some("https://dev.meta.ai/docs/muse-code/auth".into()),
            implementation: None,
            config_groups: Vec::new(),
            config_fields: Vec::new(),
            network: NetworkDeclaration {
                base_url_field: None,
                extra_origins: vec![
                    OriginDeclaration {
                        scheme: "https".into(),
                        host: "api.meta.ai".into(),
                        port: None,
                    },
                    OriginDeclaration {
                        scheme: "https".into(),
                        host: "auth.meta.com".into(),
                        port: None,
                    },
                ],
                field_origins: Vec::new(),
            },
            data_compat: DataCompatibility::default(),
        }],
    }
}
#[cfg(target_arch = "wasm32")]
struct MuseCodeVendor;
#[cfg(target_arch = "wasm32")]
impl VendorGuest for MuseCodeVendor {
    fn descriptor() -> VendorDescriptor {
        descriptor()
    }
    fn select_protocol(
        operation: Operation,
        channel: &str,
        provider: &ProviderSnapshot,
        request: &AiRequest,
    ) -> Result<String, PluginError> {
        select_protocol(operation, channel, provider, request)
    }
    fn execute(
        host: &GuestHost,
        operation: Operation,
        channel: &str,
        input: OperationInput,
    ) -> Result<OperationOutput, PluginError> {
        execute(host, operation, channel, input)
    }
}
#[cfg(target_arch = "wasm32")]
stravia_vendor_sdk::export_vendor!(MuseCodeVendor);
