//! OpenCode Zen 免费层专属 Vendor 插件。
//!
//! 免费层使用私有客户端门控；本插件按已抓包并实调的官方版本构造 UA、
//! 确定性 ses 身份、新建时间编码 msg、stream 与工具声明，tool_choice 归一为 auto。
//! 缺失的 bash/read 只补声明并追加不可调用禁令，不提供工具执行器，不覆盖
//! 客户端同名工具。成功实调不代表全部模型或未来门控都可用；原生拒绝照常返回。
//! 依据与实测：docs/research/opencode-free-models.md §3.4。
//!
//! 本插件独立发布：不负责 `/zen/go/v1` 付费面（该面反向拒绝带 tools 的
//! 请求），也不接管 base 的 `opencode` Profile——免费通道是一个单独的
//! `opencode-free` Provider Profile，由管理员导入本插件后新增。

mod connection;
mod native_identity;
mod native_tools;

mod messages {
    include!(concat!(env!("OUT_DIR"), "/messages.rs"));
}

use std::collections::{BTreeMap, BTreeSet};

use semver::Version;
use serde_json::Value;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_vendor_common::common;
#[cfg(target_arch = "wasm32")]
use stravia_vendor_sdk::VendorGuest;
use stravia_vendor_sdk::{
    CANONICAL_FORMAT_VERSION, Capability, ChannelDescriptor, ConfigField, ConfigFieldKind,
    ConfigGroup, DataCompatibility, DiscoverResponse, DiscoveredModel, ErrorKind, GuestHost,
    HttpRequest, NetworkDeclaration, Operation, OperationInput, OperationOutput, PluginError,
    ProviderDescriptor, ProviderSnapshot, VendorDescriptor, VendorKind, read_http_body,
};

const VENDOR_ID: &str = "opencode-free";
const CHANNEL_ID: &str = "default";
/// 本插件已验证的 Chat Completions 面，不把其他模型协议宣称为已支持。
const PROTOCOL: &str = "openai-compatible/chat-completions/v1";
const DEFAULT_BASE_URL: &str = "https://opencode.ai/inference/v1";
const MAX_MODELS_BODY: usize = 4 * 1024 * 1024;

const FREE_MODEL_SUFFIX: &str = "-free";
/// 不带 `-free` 后缀的免费模型。免费名单随上游轮换；轮换失效时上游返回
/// 明确的模型不可用错误，此处不扩列兜底。
const UNSUFFIXED_FREE_MODELS: &[&str] = &["big-pickle"];

/// 官方 1.18.25 隔离抓包及真实上游验证过的完整组合，不能独立猜测 SDK 版本。
const CLIENT_USER_AGENT: &str = "opencode/1.18.25 ai-sdk/provider-utils/4.0.23 runtime/bun/1.3.14";

fn is_free_tier_model(model: &str) -> bool {
    model.ends_with(FREE_MODEL_SUFFIX) || UNSUFFIXED_FREE_MODELS.contains(&model)
}

fn set_header(headers: &mut Vec<(String, String)>, name: &str, value: String) {
    headers.retain(|(candidate, _)| !candidate.eq_ignore_ascii_case(name));
    headers.push((name.to_owned(), value));
}

fn setting<'a>(provider: &'a ProviderSnapshot, key: &str) -> Option<&'a str> {
    provider
        .credentials
        .get(key)
        .or_else(|| provider.options.get(key))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn apply_auth(provider: &ProviderSnapshot, headers: &mut Vec<(String, String)>) {
    // Zen 官方客户端无 key 时发送字面量 `Bearer public`，上游对
    // `allowAnonymous` 免费模型放行并改用 IP 限流。
    let token = setting(provider, "apiKey").unwrap_or("public");
    set_header(headers, "authorization", format!("Bearer {token}"));
}

fn check_provider(channel: &str, provider: &ProviderSnapshot) -> Result<(), PluginError> {
    if provider.provider_id != VENDOR_ID {
        return Err(common::plugin_error(
            ErrorKind::Unsupported,
            format!(
                "dedicated vendor `{VENDOR_ID}` cannot handle provider `{}`",
                provider.provider_id
            ),
        ));
    }
    if channel != CHANNEL_ID {
        return Err(common::unsupported("channel", VENDOR_ID, channel));
    }
    if provider.channel != channel {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "provider snapshot channel does not match dispatch channel",
        ));
    }
    Ok(())
}

// OpenCode's OpenAI-compatible AI SDK adapter emits this closed Chat surface.
// Function schemas and arguments are application data, not request controls.
fn project_body(body: &mut Value) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    object.retain(|name, _| {
        matches!(
            name.as_str(),
            "model"
                | "messages"
                | "stream"
                | "stream_options"
                | "temperature"
                | "top_p"
                | "max_tokens"
                | "stop"
                | "frequency_penalty"
                | "presence_penalty"
                | "seed"
                | "response_format"
                | "tools"
                | "tool_choice"
                | "reasoning_effort"
                | "verbosity"
                | "user"
        )
    });
    if let Some(options) = object
        .get_mut("stream_options")
        .and_then(Value::as_object_mut)
    {
        options.retain(|name, _| name == "include_usage");
    }
    if let Some(tools) = object.get_mut("tools").and_then(Value::as_array_mut) {
        tools.retain(|tool| tool.get("type").and_then(Value::as_str) == Some("function"));
        for tool in tools {
            if let Some(tool) = tool.as_object_mut() {
                tool.retain(|name, _| matches!(name.as_str(), "type" | "function"));
                if let Some(function) = tool.get_mut("function").and_then(Value::as_object_mut) {
                    function.retain(|name, _| {
                        matches!(
                            name.as_str(),
                            "name" | "description" | "parameters" | "strict"
                        )
                    });
                }
            }
        }
    }
}

fn execute_inference(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    mut request: AiRequest,
) -> Result<OperationOutput, PluginError> {
    let model = provider
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| common::plugin_error(ErrorKind::Invalid, "inference requires a model"))?;
    request.model = model.to_owned();
    if matches!(
        request.tool_choice,
        Some(stravia_runtime_contract::protocol::ir::ToolChoice::Native(
            _
        ))
    ) {
        request.tool_choice = None;
    }
    request.parameters.native_tools.clear();
    if is_free_tier_model(model) {
        native_tools::prepare(&mut request);
    }
    let mut encoded = common::encode_inference_request(PROTOCOL, request)?;
    project_body(&mut encoded.body);
    let mut headers = Vec::new();
    native_identity::apply(provider, &mut headers)?;
    set_header(&mut headers, "content-type", "application/json".into());
    apply_auth(provider, &mut headers);
    let url = common::endpoint_url(&provider.base_url, &encoded.path)?;
    let body = serde_json::to_vec(&encoded.body).map_err(|error| {
        common::plugin_error(
            ErrorKind::Invalid,
            format!("failed to serialize codec request: {error}"),
        )
    })?;
    host.emit_started()?;
    let response = host.http_start(HttpRequest {
        method: "POST".to_owned(),
        url,
        headers,
        body,
    })?;
    common::decode_inference(host, PROTOCOL, response)
}

fn discover(host: &GuestHost, provider: &ProviderSnapshot) -> Result<OperationOutput, PluginError> {
    let url = format!("{}/models", provider.base_url.trim_end_matches('/'));
    let mut headers = vec![("accept".to_owned(), "application/json".to_owned())];
    native_identity::apply(provider, &mut headers)?;
    apply_auth(provider, &mut headers);
    let response = host.http_start(HttpRequest {
        method: "GET".to_owned(),
        url,
        headers,
        body: Vec::new(),
    })?;
    let status = response.status()?;
    let response_headers = response.headers()?;
    let body = read_http_body(&response, MAX_MODELS_BODY)?;
    if !(200..300).contains(&status) {
        return Err(common::upstream_error(status, &response_headers, &body));
    }
    let value: Value = serde_json::from_slice(&body)
        .map_err(|_| common::plugin_error(ErrorKind::Invalid, "model list is not valid JSON"))?;
    // 本插件只暴露免费模型：付费模型走 base 的 `opencode` Profile。
    let models = value
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("id").and_then(Value::as_str))
        .filter(|id| is_free_tier_model(id))
        .map(|id| DiscoveredModel {
            id: id.to_owned(),
            display_name: id.to_owned(),
            family: None,
            selector: None,
            capabilities: Vec::new(),
            metadata: BTreeMap::new(),
        })
        .collect();
    Ok(OperationOutput::Discover(DiscoverResponse {
        models,
        next_cursor: None,
    }))
}

pub fn select_protocol(
    _operation: Operation,
    channel: &str,
    provider: &ProviderSnapshot,
    _request: &AiRequest,
) -> Result<String, PluginError> {
    check_provider(channel, provider)?;
    Ok(PROTOCOL.to_owned())
}

pub fn execute(
    host: &GuestHost,
    operation: Operation,
    channel: &str,
    input: OperationInput,
) -> Result<OperationOutput, PluginError> {
    match input {
        OperationInput::Infer { provider, request } if operation == Operation::Infer => {
            check_provider(channel, &provider)?;
            execute_inference(host, &provider, request)
        }
        OperationInput::Discover { provider, .. } if operation == Operation::Discover => {
            check_provider(channel, &provider)?;
            discover(host, &provider)
        }
        OperationInput::ConnectionCheck { provider, .. }
            if operation == Operation::ConnectionCheck =>
        {
            check_provider(channel, &provider)?;
            connection::check(host, &provider).map(OperationOutput::ConnectionCheck)
        }
        other => Err(common::unsupported(
            other.operation().as_str(),
            VENDOR_ID,
            channel,
        )),
    }
}

fn string_field(
    key: &str,
    label: stravia_vendor_sdk::LocalizedText,
    description: Option<stravia_vendor_sdk::LocalizedText>,
    secret: bool,
    group: &str,
) -> ConfigField {
    ConfigField {
        key: key.to_owned(),
        label,
        description,
        kind: ConfigFieldKind::String { multiline: false },
        required: false,
        default_json: None::<Value>,
        group: Some(group.to_owned()),
        secret,
        min: None,
        max: None,
        max_length: Some(16_384),
        pattern: None,
        visible_when: None,
    }
}

pub fn descriptor() -> VendorDescriptor {
    let capabilities = BTreeSet::from([
        Capability::Infer,
        Capability::ModelDiscovery,
        Capability::ConnectionCheck,
    ]);
    VendorDescriptor {
        vendor_id: VENDOR_ID.into(),
        version: Version::parse(env!("CARGO_PKG_VERSION"))
            .expect("package version must be valid semver"),
        display_name: "OpenCode Zen Free".into(),
        description: Some(
            "OpenCode Zen free-tier models (anonymous or Zen key) over the chat-completions surface."
                .into(),
        ),
        authors: vec!["Stravia".into()],
        canonical_format_version: CANONICAL_FORMAT_VERSION,
        kind: VendorKind::Dedicated,
        providers: vec![ProviderDescriptor {
            provider_id: VENDOR_ID.into(),
            // 目录 ID 复用 `opencode` 以共享目录图标；本插件不消费目录模型。
            catalog_id: Some("opencode".into()),
            icon_svg: Some(
                stravia_vendor_common::icons::provider_svg("opencode")
                    .expect("OpenCode brand SVG is embedded")
                    .into(),
            ),
            display_name: "OpenCode Zen Free".into(),
            description: Some(
                "Free-tier models on OpenCode Zen. Works without an API key; anonymous use is IP-rate-limited upstream."
                    .into(),
            ),
            channels: vec![ChannelDescriptor {
                id: CHANNEL_ID.into(),
                name: messages::default_channel(),
                description: None,
                auth: None,
                protocol: Some("openai-compatible".into()),
                protocols: Vec::new(),
                default_base_url: Some(DEFAULT_BASE_URL.into()),
                default_models_source: None,
                consumes_catalog_models: false,
                capabilities: capabilities.clone(),
                client_header_names: BTreeSet::new(),
                model_capabilities: BTreeSet::new(),

            }],
            capabilities,
            website: Some("https://opencode.ai".into()),
            implementation: None,
            config_groups: vec![
                ConfigGroup {
                    id: "credentials".into(),
                    label: messages::credentials_group(),
                },
            ],
            config_fields: vec![
                string_field(
                    "apiKey",
                    messages::api_key(),
                    Some(messages::api_key_description()),
                    true,
                    "credentials",
                ),
            ],
            network: NetworkDeclaration::default(),
            data_compat: DataCompatibility::default(),
        }],
    }
}

#[cfg(target_arch = "wasm32")]
struct OpencodeFreeVendor;

#[cfg(target_arch = "wasm32")]
impl VendorGuest for OpencodeFreeVendor {
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
stravia_vendor_sdk::export_vendor!(OpencodeFreeVendor);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn non_free_model_decision() {
        assert!(is_free_tier_model("mimo-v2.5-free"));
        assert!(is_free_tier_model("big-pickle"));
        assert!(!is_free_tier_model("gpt-5.5"));
        assert!(!is_free_tier_model("freedom"));
    }

    #[test]
    fn native_body_projection_drops_unknown_controls_not_schema_data() {
        let mut body = json!({
            "model":"big-pickle","messages":[{"role":"user","content":"hello"}],
            "stream":true,"stream_options":{"include_usage":true,"future":true},
            "verbosity":"low","top_k":99,"parallel_tool_calls":false,
            "extra_body":{"tools":[{"type":"computer"}]},"metadata":{"injected":true},
            "tools":[{"type":"function","future":true,"function":{
                "name":"read","future":true,
                "parameters":{"type":"object","properties":{"future":{"type":"string"}}}
            }},{"type":"computer","name":"foreign"}]
        });
        project_body(&mut body);
        assert!(body.get("extra_body").is_none());
        assert!(body.get("metadata").is_none());
        assert!(body.get("top_k").is_none());
        assert!(body.get("parallel_tool_calls").is_none());
        assert_eq!(body["verbosity"], "low");
        assert_eq!(body["stream_options"], json!({"include_usage":true}));
        assert_eq!(body["tools"].as_array().unwrap().len(), 1);
        assert!(body["tools"][0].get("future").is_none());
        assert!(body["tools"][0]["function"].get("future").is_none());
        assert_eq!(
            body["tools"][0]["function"]["parameters"]["properties"]["future"]["type"],
            "string"
        );
    }
}
