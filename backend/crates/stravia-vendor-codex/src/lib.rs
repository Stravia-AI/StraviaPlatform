mod allowance;
mod auth;
mod codex;
mod media_generation;
mod messages {
    include!(concat!(env!("OUT_DIR"), "/messages.rs"));
}

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_vendor_common::{common, thinking};
#[cfg(target_arch = "wasm32")]
use stravia_vendor_sdk::VendorGuest;
use stravia_vendor_sdk::{
    AuthCallback, AuthCallbackPort, AuthDescriptor, AuthFlow, AuthManualInput, AuthManualInputType,
    CANONICAL_FORMAT_VERSION, Capability, ChannelDescriptor, ConfigField, ConfigFieldKind,
    ConfigGroup, ConfigValidationResponse, DataCompatibility, DiscoverRequest, DiscoverResponse,
    DiscoveredModel, ErrorKind, GuestHost, MODELS_SOURCE_CATALOG, NetworkDeclaration, Operation,
    OperationInput, OperationOutput, OriginDeclaration, PluginError, ProviderDescriptor,
    ProviderSnapshot, TransportPreference, ValidationIssue, VendorDescriptor, VendorKind,
};

const VENDOR_ID: &str = "openai-codex";
const CATALOG_ID: &str = "openai";
const CHANNEL: &str = "codex";
/// Spoofed Codex CLI version sent on every request. The backend version-gates
/// model availability for `/models` and `/responses` by exact client version
/// rather than the catalog's `minimal_client_version`; an older pin silently
/// hides newer SKUs such as gpt-6-sol and gpt-6.1-sol from discovery.
const CLIENT_VERSION: &str = "0.162.1";
const MAX_ERROR_BODY: usize = 256 * 1024;
const MAX_MODELS_BODY: usize = 4 * 1024 * 1024;

pub fn descriptor() -> VendorDescriptor {
    let capabilities = BTreeSet::from([
        Capability::Infer,
        Capability::Compact,
        Capability::Search,
        Capability::MediaImage,
        Capability::AuthOauth,
        Capability::ModelDiscovery,
        Capability::Allowance,
        Capability::ConfigValidation,
        Capability::ConnectionCheck,
    ]);
    VendorDescriptor {
        vendor_id: VENDOR_ID.into(),
        version: semver::Version::parse(env!("CARGO_PKG_VERSION"))
            .expect("package version must be valid semver"),
        display_name: "OpenAI Codex".into(),
        description: Some(
            "ChatGPT-backed Codex OAuth, Responses, hosted search, image generation, and usage."
                .into(),
        ),
        authors: vec!["Stravia".into()],
        canonical_format_version: CANONICAL_FORMAT_VERSION,
        kind: VendorKind::Dedicated,
        providers: vec![ProviderDescriptor {
            provider_id: VENDOR_ID.into(),
            catalog_id: Some(CATALOG_ID.into()),
            icon_svg: Some(
                include_str!("../../../../frontend/stravia-webui/src/assets/icons/openai.svg")
                    .to_owned(),
            ),
            display_name: "OpenAI Codex".into(),
            description: Some(
                "ChatGPT OAuth channel using Open Responses, hosted search, and image generation."
                    .into(),
            ),
            channels: vec![ChannelDescriptor {
                id: CHANNEL.into(),
                name: crate::messages::channel_codex(),
                description: Some(crate::messages::channel_description()),
                auth: Some(AuthDescriptor {
                    flow: AuthFlow::AuthorizationCode,
                    callback: Some(AuthCallback {
                        bind_host: "127.0.0.1".into(),
                        redirect_host: "localhost".into(),
                        path: "/auth/callback".into(),
                        port: AuthCallbackPort::Fixed {
                            primary: 1455,
                            fallback: Some(1456),
                        },
                        manual_redirect_uri: Some("http://localhost:1457/auth/callback".into()),
                        cancel_path: Some("/cancel".into()),
                    }),
                    manual_input: Some(AuthManualInput {
                        input_type: AuthManualInputType::CallbackUrl,
                        label: crate::messages::callback_url(),
                        description: Some(crate::messages::callback_description()),
                        secret: false,
                    }),
                }),
                protocol: Some("open-responses".into()),
                protocols: Vec::new(),
                default_base_url: Some("https://chatgpt.com/backend-api/codex".into()),
                default_models_source: None,
                consumes_catalog_models: false,
                capabilities: capabilities.clone(),
                // 会话头已由宿主归一为统一 session 字段，安装与 turn 身份由插件私有状态生成。
                client_header_names: BTreeSet::new(),
                model_capabilities: BTreeSet::new(),
            }],
            capabilities,
            website: None,
            implementation: None,
            config_groups: vec![ConfigGroup {
                id: "advanced".into(),
                label: crate::messages::advanced(),
            }],
            config_fields: vec![ConfigField {
                key: "websocket_url".into(),
                label: crate::messages::websocket_url(),
                description: Some(crate::messages::websocket_description()),
                kind: ConfigFieldKind::String { multiline: false },
                required: false,
                default_json: None,
                group: Some("advanced".into()),
                secret: false,
                min: None,
                max: None,
                max_length: Some(4096),
                pattern: Some(r"^wss?://.+".into()),
                visible_when: None,
            }],
            network: NetworkDeclaration {
                base_url_field: None,
                extra_origins: vec![
                    OriginDeclaration {
                        scheme: "https".into(),
                        host: "auth.openai.com".into(),
                        port: None,
                    },
                    OriginDeclaration {
                        scheme: "https".into(),
                        host: "chatgpt.com".into(),
                        port: None,
                    },
                    OriginDeclaration {
                        scheme: "wss".into(),
                        host: "chatgpt.com".into(),
                        port: None,
                    },
                ],
                field_origins: vec!["websocket_url".into()],
            },
            data_compat: DataCompatibility::default(),
        }],
    }
}

pub fn select_protocol(
    operation: Operation,
    channel: &str,
    provider: &ProviderSnapshot,
    _request: &AiRequest,
) -> Result<String, PluginError> {
    require_route(channel, provider)?;
    if matches!(operation, Operation::Infer | Operation::Compact) {
        Ok(OPEN_RESPONSES_2026_04_24.to_string())
    } else {
        Ok(provider.protocol.clone())
    }
}

pub fn execute(
    host: &GuestHost,
    operation: Operation,
    channel: &str,
    input: OperationInput,
) -> Result<OperationOutput, PluginError> {
    if input.operation() != operation {
        return Err(invalid("operation input does not match operation"));
    }
    require_route(channel, input.provider())?;
    match input {
        OperationInput::Infer { provider, request } => {
            infer(host, provider, request, Operation::Infer)
        }
        OperationInput::Compact { provider, request } => {
            infer(host, provider, request, Operation::Compact)
        }
        OperationInput::Search { provider, request } => {
            codex::search(host, &provider, request).map(OperationOutput::Search)
        }
        OperationInput::MediaImage { provider, request } => {
            media_generation::generate(host, &provider, request).map(OperationOutput::MediaImage)
        }
        OperationInput::Auth { provider, request } => {
            auth::execute(host, &provider, request).map(OperationOutput::Auth)
        }
        OperationInput::Discover { provider, request } => discover(host, provider, request),
        OperationInput::Allowance {
            provider,
            request: _,
        } => allowance::execute(host, &provider).map(OperationOutput::Allowance),
        OperationInput::ConnectionCheck {
            provider,
            request: _,
        } => allowance::connection_check(host, &provider).map(OperationOutput::ConnectionCheck),
        OperationInput::ConfigValidation {
            provider: _,
            request,
        } => Ok(OperationOutput::ConfigValidation(
            ConfigValidationResponse {
                issues: validate_config(&request.options),
                proposed_base_url: None,
            },
        )),
    }
}

fn require_route(channel: &str, provider: &ProviderSnapshot) -> Result<(), PluginError> {
    if provider.provider_id != VENDOR_ID {
        return Err(unsupported(format!(
            "{VENDOR_ID} cannot execute provider `{}`",
            provider.provider_id
        )));
    }
    if channel != CHANNEL || provider.channel != CHANNEL {
        return Err(unsupported(format!(
            "{VENDOR_ID} only supports the `{CHANNEL}` channel"
        )));
    }
    Ok(())
}

fn infer(
    host: &GuestHost,
    provider: ProviderSnapshot,
    mut request: AiRequest,
    operation: Operation,
) -> Result<OperationOutput, PluginError> {
    let model = required_model(&provider)?;
    request.model = model.to_owned();
    require_codex_protocol(&provider)?;
    for item in &mut request.items {
        if item.role == stravia_runtime_contract::protocol::ir::Role::System {
            item.role = stravia_runtime_contract::protocol::ir::Role::Developer;
        }
    }
    request.parameters.store = Some(false);
    let mut session = codex::NativeSession::new(&provider);
    session.resolve_identity(host, &provider)?;

    let protocol = OPEN_RESPONSES_2026_04_24;
    let preserve_upstream_errors = operation == Operation::Infer
        && stravia_protocol_codec::codec::compaction::native_compaction_requested(&request);
    let websocket = operation == Operation::Infer
        && provider.transport_preference() != TransportPreference::HttpOnly
        && codex::websocket_available(&provider);
    if operation == Operation::Infer {
        require_native_continuation_transport(&request, websocket)?;
    }
    let native_tools = codex::encode_tools(&request, codex::BodyOperation::Http);
    // 原生工具只编码一次；通用 codec 仍负责历史与统一控制的协议投影。
    request.tools = None;
    request.parameters.native_tools.clear();
    let encoded = common::encode_inference_request(&protocol.to_string(), request)?;
    let mut body = encoded.body;
    body["tools"] = native_tools.into();
    let codec_headers = encoded.headers;
    let egress_path = encoded.path;
    prepare_responses_body(&provider, &mut body)?;
    if operation == Operation::Compact {
        codex::prepare_operation_body(&mut body, codex::BodyOperation::Compact)?;
    } else if websocket {
        codex::prepare_operation_body(&mut body, codex::BodyOperation::WebSocket)?;
    } else {
        codex::prepare_body(&mut body)?;
    }
    if operation != Operation::Compact {
        session.apply_body(&mut body);
    }
    if websocket {
        return codex::infer_websocket(host, &provider, &body, &session, preserve_upstream_errors)
            .map(Box::new)
            .map(OperationOutput::Infer);
    }
    let path = if operation == Operation::Compact {
        "/responses/compact".to_owned()
    } else {
        strip_v1_prefix(&egress_path).to_owned()
    };
    let mut headers = common::header_pairs(&codec_headers)?;
    headers.retain(|(name, _)| {
        !name.eq_ignore_ascii_case("content-type") && !name.eq_ignore_ascii_case("accept")
    });
    headers.push(("content-type".into(), "application/json".into()));
    headers.push((
        "accept".into(),
        "text/event-stream, application/json".into(),
    ));
    codex::append_runtime_headers(
        &provider,
        &body,
        &session,
        if operation == Operation::Compact {
            codex::BodyOperation::Compact
        } else {
            codex::BodyOperation::Http
        },
        &mut headers,
    )?;
    host.emit_started()?;
    let response = host.http_start(stravia_vendor_sdk::wit::types::HttpRequest {
        method: "POST".into(),
        url: endpoint(&provider.base_url, &path),
        headers,
        body: serde_json::to_vec(&body)
            .map_err(|_| invalid("Codex request could not be encoded"))?,
    })?;
    match operation {
        Operation::Infer => {
            // Codex 流式请求的成功响应按 SSE 解码，不依赖上游是否返回 Content-Type。
            let response_mode = if body.get("stream").and_then(Value::as_bool) == Some(true) {
                common::HttpResponseMode::Streaming
            } else {
                common::HttpResponseMode::HeaderDetected
            };
            let decoded = if preserve_upstream_errors {
                common::decode_ai_response_preserving_upstream_errors(
                    host,
                    &protocol.to_string(),
                    response,
                    response_mode,
                    classify_responses_stream_error,
                )
            } else {
                ensure_success(&response, "Codex inference")?;
                common::decode_ai_response_with_error_classifier(
                    host,
                    &protocol.to_string(),
                    response,
                    response_mode,
                    classify_responses_stream_error,
                )
            };
            decoded.map(Box::new).map(OperationOutput::Infer)
        }
        Operation::Compact => common::decode_compaction_preserving_upstream_errors(host, response)
            .map(OperationOutput::Compact),
        _ => Err(invalid(
            "Codex codec returned an unexpected operation result",
        )),
    }
}

fn require_native_continuation_transport(
    request: &AiRequest,
    websocket: bool,
) -> Result<(), PluginError> {
    if !websocket
        && request
            .parameters
            .continuation
            .as_ref()
            .and_then(|continuation| continuation.previous_response_id())
            .is_some_and(|id| !id.trim().is_empty())
    {
        // The host may have replaced full history with a continuation delta.
        // Reject before dispatch so its existing seam restores retained history.
        return Err(PluginError {
            kind: ErrorKind::ContinuationUnavailable,
            message: "Codex HTTP requires the host's full-history request".into(),
            upstream_status: None,
        });
    }
    Ok(())
}

fn prepare_responses_body(
    provider: &ProviderSnapshot,
    body: &mut Value,
) -> Result<(), PluginError> {
    let object = body
        .as_object_mut()
        .ok_or_else(|| invalid("Codex Responses request body must be an object"))?;
    let include = object
        .entry("include")
        .or_insert_with(|| Value::Array(Vec::new()));
    if include.is_null() {
        *include = Value::Array(Vec::new());
    }
    let include = include
        .as_array_mut()
        .ok_or_else(|| invalid("Codex Responses include must be an array"))?;
    // Codex CLI 不请求 logprobs；订阅推理端点会拒绝公共 Responses 派生的该项。
    include.retain(|value| value.as_str() == Some("reasoning.encrypted_content"));
    if !object.contains_key("prompt_cache_key")
        && let Some(affinity) = provider
            .operation_metadata
            .get("transport_affinity")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
    {
        object.insert(
            "prompt_cache_key".into(),
            Value::String(affinity.to_owned()),
        );
    }
    Ok(())
}

fn discover(
    host: &GuestHost,
    provider: ProviderSnapshot,
    request: DiscoverRequest,
) -> Result<OperationOutput, PluginError> {
    if let Some(response) = explicit_discovery(&provider)? {
        return Ok(OperationOutput::Discover(response));
    }
    if request.cursor.is_some() {
        return Err(unsupported(
            "Codex model discovery does not support cursor continuation",
        ));
    }
    require_codex_protocol(&provider)?;
    let mut url = provider
        .operation_metadata
        .get("models_source")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|source| !source.is_empty() && *source != MODELS_SOURCE_CATALOG)
        .map(str::to_owned)
        .unwrap_or_else(|| endpoint(&provider.base_url, "/models"));
    url.push(if url.contains('?') { '&' } else { '?' });
    url.push_str("client_version=");
    url.push_str(CLIENT_VERSION);
    let mut headers = vec![("accept".into(), "application/json".into())];
    codex::append_identity_headers(&provider, &mut headers)?;
    let response = host.http_start(stravia_vendor_sdk::wit::types::HttpRequest {
        method: "GET".into(),
        url,
        headers,
        body: Vec::new(),
    })?;
    ensure_success(&response, "Codex model discovery")?;
    let body = stravia_vendor_sdk::read_http_body(&response, MAX_MODELS_BODY)?;
    let value: Value = serde_json::from_slice(&body)
        .map_err(|_| retryable("Codex model discovery returned invalid JSON"))?;
    let entries = value
        .get("data")
        .or_else(|| value.get("models"))
        .and_then(Value::as_array)
        .ok_or_else(|| retryable("Codex model discovery response has no model list"))?;
    let models = entries
        .iter()
        .filter_map(discovered_model)
        .collect::<Vec<_>>();
    Ok(OperationOutput::Discover(DiscoverResponse {
        models,
        next_cursor: None,
    }))
}

fn explicit_discovery(
    provider: &ProviderSnapshot,
) -> Result<Option<DiscoverResponse>, PluginError> {
    let Some(static_models) = provider.operation_metadata.get("static_models") else {
        return Ok(None);
    };
    let values = static_models
        .as_array()
        .ok_or_else(|| invalid("operation_metadata.static_models must be a JSON array"))?;
    let mut models = BTreeMap::new();
    for (index, value) in values.iter().enumerate() {
        let id = value.as_str().ok_or_else(|| {
            invalid(format!(
                "operation_metadata.static_models[{index}] must be a string"
            ))
        })?;
        let id = id.trim();
        if id.is_empty() {
            continue;
        }
        let mut model = DiscoveredModel {
            id: id.to_owned(),
            display_name: id.to_owned(),
            family: None,
            selector: None,
            capabilities: Vec::new(),
            metadata: BTreeMap::new(),
        };
        decorate_discovered_model(&mut model);
        models.entry(id.to_owned()).or_insert(model);
    }
    Ok(Some(DiscoverResponse {
        models: models.into_values().collect(),
        next_cursor: None,
    }))
}

fn discovered_model(entry: &Value) -> Option<DiscoveredModel> {
    if entry
        .get("visibility")
        .and_then(Value::as_str)
        .is_some_and(|visibility| !visibility.eq_ignore_ascii_case("list"))
    {
        return None;
    }
    let id = entry
        .get("id")
        .or_else(|| entry.get("slug"))
        .or_else(|| entry.get("name"))
        .and_then(Value::as_str)?
        .trim();
    if id.is_empty() {
        return None;
    }
    let mut model = DiscoveredModel {
        id: id.to_owned(),
        display_name: entry
            .get("display_name")
            .or_else(|| entry.get("name"))
            .and_then(Value::as_str)
            .unwrap_or(id)
            .to_owned(),
        family: entry
            .get("family")
            .and_then(Value::as_str)
            .map(str::to_owned),
        selector: entry
            .get("selector")
            .and_then(Value::as_str)
            .map(str::to_owned),
        capabilities: entry
            .get("capabilities")
            .or_else(|| entry.get("supported_endpoints"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        metadata: entry
            .as_object()
            .map(stravia_vendor_common::thinking::source_metadata)
            .unwrap_or_default(),
    };
    apply_declared_specifications(&mut model);
    decorate_discovered_model(&mut model);
    Some(model)
}

/// `/models` 以 Codex 字段声明的每模型规格是权威数据；翻成宿主 metadata
/// schema 后由宿主合并，覆盖目录与默认填充。
fn apply_declared_specifications(model: &mut DiscoveredModel) {
    let input = declared_string_list(model.metadata.get("input_modalities"));
    let output = declared_string_list(model.metadata.get("output_modalities"));
    if input.is_some() || output.is_some() {
        let declared = model.metadata.get("modalities");
        let input = input
            .or_else(|| declared_string_list(declared.and_then(|value| value.get("input"))))
            .unwrap_or_else(|| vec!["text".into()]);
        let output = output
            .or_else(|| declared_string_list(declared.and_then(|value| value.get("output"))))
            .unwrap_or_else(|| vec!["text".into()]);
        model.metadata.insert(
            "modalities".into(),
            serde_json::json!({ "input": input, "output": output }),
        );
    }
    let efforts = model
        .metadata
        .get("supported_reasoning_levels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|level| {
            level
                .as_str()
                .or_else(|| level.get("effort").and_then(Value::as_str))
        })
        .map(str::trim)
        .filter(|effort| {
            !effort.is_empty()
                && !effort.eq_ignore_ascii_case("default")
                && !effort.eq_ignore_ascii_case("null")
        })
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if !efforts.is_empty() {
        model
            .metadata
            .insert("reasoning_efforts".into(), serde_json::json!(efforts));
    }
}

fn declared_string_list(value: Option<&Value>) -> Option<Vec<String>> {
    let values = value?.as_array()?;
    let list = values
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    Some(list)
}

fn decorate_discovered_model(model: &mut DiscoveredModel) {
    let has_supplier_capabilities = model
        .capabilities
        .iter()
        .any(|capability| !capability.trim().is_empty());
    thinking::decorate_discovered_model(CATALOG_ID, model);
    let output = model
        .metadata
        .get("modalities")
        .and_then(|value| value.get("output"));
    let has_image_output = output
        .and_then(Value::as_array)
        .is_some_and(|output| output.iter().any(|modality| modality == "image"));
    let has_text_output = output
        .and_then(Value::as_array)
        .is_none_or(|output| output.iter().any(|modality| modality == "text"));
    // Codex ModelInfo 的 input_modalities / supports_image_detail_original
    // 只声明输入；只有明确列出的 image_generation 工具能证明托管图片支持。
    let has_image_tool = model
        .metadata
        .get("experimental_supported_tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| tools.iter().any(|tool| tool == "image_generation"));
    let has_image_capability = model
        .capabilities
        .iter()
        .any(|capability| capability == Capability::MediaImage.as_str());
    if !has_supplier_capabilities && has_text_output {
        model.capabilities.push(Capability::Infer.as_str().into());
        model.capabilities.push(Capability::Search.as_str().into());
    }
    if !(has_image_output || has_image_tool || has_image_capability) {
        return;
    }

    // 仅使用上游明确的图片输出或工具能力，不从型号或视觉输入推断。
    if !model
        .capabilities
        .iter()
        .any(|capability| capability == Capability::MediaImage.as_str())
    {
        model
            .capabilities
            .push(Capability::MediaImage.as_str().into());
    }
    let modalities = model
        .metadata
        .entry("modalities".into())
        .or_insert_with(|| serde_json::json!({"input": ["text"], "output": ["text"]}));
    if !modalities.is_object() {
        *modalities = serde_json::json!({"input": ["text"], "output": ["text"]});
    }
    let output = modalities
        .as_object_mut()
        .expect("normalized model modalities")
        .entry("output")
        .or_insert_with(|| serde_json::json!(["text"]));
    if let Value::Array(output) = output {
        if !output.iter().any(|modality| modality == "image") {
            output.push(serde_json::json!("image"));
        }
    } else {
        *output = serde_json::json!(["text", "image"]);
    }
}

fn validate_config(options: &BTreeMap<String, Value>) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();
    if let Some(value) = options.get("websocket_url")
        && !value.as_str().is_some_and(|value| {
            value.trim().is_empty()
                || url::Url::parse(value).is_ok_and(|url| matches!(url.scheme(), "ws" | "wss"))
        })
    {
        issues.push(ValidationIssue {
            field: Some("websocket_url".into()),
            code: "invalid_websocket_url".into(),
            message: crate::messages::invalid_websocket_url(),
        });
    }
    issues
}

pub(crate) fn secret<'a>(
    provider: &'a ProviderSnapshot,
    key: &str,
) -> Result<&'a str, PluginError> {
    provider
        .credentials
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| auth_error(format!("missing credential `{key}`")))
}

pub(crate) fn configured_websocket_url(provider: &ProviderSnapshot) -> Option<&str> {
    provider
        .options
        .get("websocket_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| value.starts_with("ws://") || value.starts_with("wss://"))
}

pub(crate) fn require_codex_protocol(provider: &ProviderSnapshot) -> Result<(), PluginError> {
    let protocol = stravia_protocol_codec::registry::ProtocolRegistry::global()
        .resolve_alias(&provider.protocol)
        .ok_or_else(|| unsupported("Codex provider protocol is not registered"))?;
    if protocol != OPEN_RESPONSES_2026_04_24 {
        return Err(unsupported("Codex requires the Open Responses protocol"));
    }
    Ok(())
}

pub(crate) fn required_model(provider: &ProviderSnapshot) -> Result<&str, PluginError> {
    provider
        .model
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "*")
        .ok_or_else(|| invalid("Codex operation requires an upstream model"))
}

pub(crate) fn endpoint(base_url: &str, path: &str) -> String {
    let base = base_url.trim_end_matches('/');
    let mut path = path.trim_start_matches('/');
    if base.ends_with("/v1") {
        path = path.strip_prefix("v1/").unwrap_or(path);
    }
    format!("{base}/{path}")
}

fn strip_v1_prefix(path: &str) -> &str {
    path.strip_prefix("/v1/")
        .unwrap_or(path)
        .trim_start_matches('/')
}

pub(crate) fn ensure_success(
    response: &stravia_vendor_sdk::HttpResponse,
    label: &str,
) -> Result<u16, PluginError> {
    let status = response.status()?;
    if (200..300).contains(&status) {
        return Ok(status);
    }
    let headers = response.headers()?;
    let body = stravia_vendor_sdk::read_http_body(response, MAX_ERROR_BODY)?;
    if protected_reasoning_rejected(&body) {
        return Err(PluginError {
            kind: ErrorKind::ProtectedReasoningRejected,
            message: "Codex rejected protected reasoning replay".into(),
            upstream_status: Some(status),
        });
    }
    let mut error = common::upstream_error(status, &headers, &body);
    error.message = format!("{label} failed: {}", error.message);
    Err(error)
}

fn protected_reasoning_rejected(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .as_ref()
        .is_some_and(protected_reasoning_rejection_event)
}

fn protected_reasoning_rejection_event(value: &Value) -> bool {
    value
        .pointer("/error/code")
        .or_else(|| value.pointer("/response/error/code"))
        .or_else(|| value.get("code"))
        .and_then(Value::as_str)
        == Some("invalid_encrypted_content")
}

pub(crate) fn classify_responses_stream_error(
    value: &Value,
    saw_response_event: bool,
) -> Option<PluginError> {
    (!saw_response_event && protected_reasoning_rejection_event(value)).then(|| PluginError {
        kind: ErrorKind::ProtectedReasoningRejected,
        message: "Codex rejected protected reasoning replay".into(),
        upstream_status: None,
    })
}

pub(crate) fn classify_codex_websocket_error(
    value: &Value,
    saw_response_event: bool,
    continuation_requested: bool,
) -> Option<PluginError> {
    if let Some(error) = classify_responses_stream_error(value, saw_response_event) {
        return Some(error);
    }
    (continuation_requested
        && !saw_response_event
        && value.get("type").and_then(Value::as_str) == Some("error")
        && value.get("status").and_then(Value::as_u64) == Some(400)
        && value.pointer("/error/type").and_then(Value::as_str) == Some("invalid_request_error")
        && value.pointer("/error/code").is_none_or(Value::is_null)
        && value.get("code").is_none_or(Value::is_null)
        && value
            .pointer("/error/message")
            .and_then(Value::as_str)
            .is_some_and(|message| {
                matches!(
                    message.trim(),
                    "Invalid `previous_response_id`." | "Invalid `previous_response_id`"
                )
            }))
    .then(|| PluginError {
        kind: ErrorKind::ContinuationNotFound,
        message: "upstream continuation is no longer available".into(),
        upstream_status: value
            .get("status")
            .and_then(Value::as_u64)
            .and_then(|status| u16::try_from(status).ok()),
    })
}

pub(crate) fn invalid(message: impl Into<String>) -> PluginError {
    PluginError {
        kind: ErrorKind::Invalid,
        message: message.into(),
        upstream_status: None,
    }
}

pub(crate) fn auth_error(message: impl Into<String>) -> PluginError {
    PluginError {
        kind: ErrorKind::Auth,
        message: message.into(),
        upstream_status: None,
    }
}

pub(crate) fn retryable(message: impl Into<String>) -> PluginError {
    common::model_error(
        stravia_runtime_contract::protocol::ir::AiErrorKind::ServerError,
        message,
    )
}

pub(crate) fn unsupported(message: impl Into<String>) -> PluginError {
    PluginError {
        kind: ErrorKind::Unsupported,
        message: message.into(),
        upstream_status: None,
    }
}

#[cfg(target_arch = "wasm32")]
struct CodexVendor;

#[cfg(target_arch = "wasm32")]
impl VendorGuest for CodexVendor {
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
stravia_vendor_sdk::export_vendor!(CodexVendor);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn http_continuation_requires_host_full_history_before_dispatch() {
        let mut request = AiRequest::new("gpt-5.4", Vec::new());
        request.parameters.continuation = Some(
            stravia_runtime_contract::protocol::ir::Continuation::PreviousResponse(
                "resp_previous".into(),
            ),
        );
        let error = require_native_continuation_transport(&request, false).unwrap_err();
        assert!(matches!(error.kind, ErrorKind::ContinuationUnavailable));
        assert!(require_native_continuation_transport(&request, true).is_ok());
        request.parameters.continuation = None;
        assert!(require_native_continuation_transport(&request, false).is_ok());
    }

    #[test]
    fn native_projection_keeps_supported_controls_and_canonical_history() {
        let mut request = AiRequest::new("gpt-6-astra", Vec::new());
        request.stream.enabled = false;
        request.generation.temperature = Some(0.2);
        request.generation.top_p = Some(0.9);
        request.generation.max_tokens = Some(256);
        request.parallel_tool_calls = Some(false);
        request.parameters.metadata = Some(BTreeMap::from([(
            "verification".into(),
            "canonical-final".into(),
        )]));
        request.parameters.cache.key = Some("canonical-final-smoke".into());
        request.parameters.cache.retention =
            Some(stravia_runtime_contract::protocol::ir::CacheRetention::Extended);
        request.parameters.generate = Some(false);
        request.parameters.continuation = Some(
            stravia_runtime_contract::protocol::ir::Continuation::PreviousResponse(
                "resp_previous".into(),
            ),
        );
        let original = request.clone();
        let mut body = common::encode_inference_request(
            &OPEN_RESPONSES_2026_04_24.to_string(),
            request.clone(),
        )
        .unwrap()
        .body;
        assert_eq!(body["metadata"], json!({"verification":"canonical-final"}));
        assert_eq!(body["prompt_cache_retention"], "24h");
        codex::prepare_body(&mut body).unwrap();
        for unsupported in [
            "metadata",
            "prompt_cache_retention",
            "temperature",
            "top_p",
            "max_output_tokens",
        ] {
            assert!(
                body.get(unsupported).is_none(),
                "unsupported native control: {unsupported}"
            );
        }
        assert_eq!(body["stream"], true);
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(body["prompt_cache_key"], "canonical-final-smoke");
        assert!(body.get("previous_response_id").is_none());
        assert_eq!(body["store"], false);
        assert!(
            body.get("generate").is_none(),
            "generate is a WS-only control"
        );
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            serde_json::to_value(original).unwrap()
        );
    }

    #[test]
    fn native_tool_projection_keeps_cli_definitions_and_omits_api_only_controls() {
        let schema = json!({
            "type":"object","properties":{"unregistered_parameter":{"type":"string"}}
        });
        let request = stravia_protocol_codec::codec::open_responses::decoder::ResponsesDecoder
            .decode_request(json!({
                "model":"gpt-6-astra","input":"hello",
                "tool_choice":{"type":"function","name":"business"},
                "tools":[
                    {"type":"function","name":"business","description":"business input",
                        "strict":false,"defer_loading":true,"parameters":schema},
                    {"type":"namespace","name":"business_namespace","description":"business tools",
                        "tools":[
                            {"type":"function","name":"nested","parameters":schema,"strict":false},
                            {"type":"custom","name":"grammar","description":"grammar tool",
                                "defer_loading":false,"format":{"type":"grammar","syntax":"lark",
                                    "definition":"start: /.+/","unregistered_parameter":true}},
                            {"type":"web_search"}
                        ]},
                    {"type":"tool_search","execution":"client","description":"find tools",
                        "parameters":schema,"unregistered_parameter":true},
                    {"type":"web_search","external_web_access":false,"indexed_web_access":true,
                        "search_content_types":["text","image"],
                        "filters":{"allowed_domains":["example.test"],"unregistered_parameter":true},
                        "user_location":{"type":"approximate","city":"Paris","unregistered_parameter":true},
                        "unregistered_parameter":true},
                    {"type":"mcp","server_label":"public-api-only","server_url":"https://example.test/mcp"}
                ]
            }))
            .unwrap();
        let tools = codex::encode_tools(&request, codex::BodyOperation::Http);
        let mut body =
            common::encode_inference_request(&OPEN_RESPONSES_2026_04_24.to_string(), request)
                .unwrap()
                .body;
        body["tools"] = tools.into();
        codex::prepare_body(&mut body).unwrap();
        assert!(
            body.get("tool_choice").is_none(),
            "CLI inference uses string tool choices"
        );
        let tools = body["tools"].as_array().unwrap();
        assert!(!tools.iter().any(|tool| tool["type"] == "mcp"));
        let function = tools
            .iter()
            .find(|tool| tool["name"] == "business")
            .unwrap();
        assert_eq!(function["parameters"], schema);
        assert_eq!(function["defer_loading"], true);
        assert_eq!(function["strict"], false);
        let namespace = tools
            .iter()
            .find(|tool| tool["type"] == "namespace")
            .unwrap();
        let children = namespace["tools"].as_array().unwrap();
        assert!(!children.iter().any(|tool| tool["type"] == "web_search"));
        assert_eq!(children[0]["parameters"], schema);
        assert_eq!(children[1]["defer_loading"], false);
        assert_eq!(
            children[1]["format"],
            json!({
                "type":"grammar","syntax":"lark","definition":"start: /.+/"
            })
        );
        let search = tools
            .iter()
            .find(|tool| tool["type"] == "tool_search")
            .unwrap();
        assert_eq!(search["execution"], "client");
        assert_eq!(search["parameters"], schema);
        assert!(search.get("unregistered_parameter").is_none());
        let web = tools
            .iter()
            .find(|tool| tool["type"] == "web_search")
            .unwrap();
        assert_eq!(web["filters"], json!({"allowed_domains":["example.test"]}));
        assert_eq!(
            web["user_location"],
            json!({"type":"approximate","city":"Paris"})
        );
        assert!(web.get("unregistered_parameter").is_none());
        for tools in [
            json!([{"type":"mcp","server_label":"API-only","server_url":"https://example.test"}]),
            json!([{"type":"namespace","name":"empty_after_projection","tools":[{"type":"web_search"}]}]),
        ] {
            let mut body = json!({
                "model":"gpt-6-astra", "input":"hello", "tools":tools, "tool_choice":"required"
            });
            codex::prepare_body(&mut body).unwrap();
            assert_eq!(body["input"], "hello");
            assert_eq!(body["tools"], json!([]));
            assert!(body.get("tool_choice").is_none());
        }
    }

    #[test]
    fn discovered_model_translates_declared_specifications() {
        let entry = json!({
            "slug": "gpt-6-luna",
            "display_name": "GPT-6-Luna",
            "visibility": "list",
            "capabilities": ["infer", "media_image", "search"],
            "input_modalities": ["text", "image"],
            "context_window": 272000,
            "supported_reasoning_levels": [
                {"effort": "low", "description": "Fast responses with lighter reasoning"},
                {"effort": "medium", "description": "Balances speed and reasoning depth"},
                {"effort": "high", "description": "Greater reasoning depth"},
                {"effort": "xhigh", "description": "Extra high reasoning depth"},
                {"effort": "max", "description": "Maximum reasoning depth"}
            ]
        });
        let model = discovered_model(&entry).expect("listed model");

        assert_eq!(model.id, "gpt-6-luna");
        assert_eq!(
            model.metadata["modalities"],
            json!({"input": ["text", "image"], "output": ["text", "image"]})
        );
        assert_eq!(model.metadata["context_window"], json!(272000));
        assert_eq!(
            model.metadata["reasoning_efforts"],
            json!(["low", "medium", "high", "xhigh", "max"])
        );
    }

    #[test]
    fn discovered_model_registers_hosted_image_output() {
        for entry in [
            json!({"slug": "future-capability", "capabilities": ["media_image"]}),
            json!({"slug": "future-tool", "experimental_supported_tools": ["image_generation"]}),
            json!({"slug": "future-output", "output_modalities": ["text", "image"]}),
            json!({"slug": "future-mixed", "capabilities": ["infer", "media_image"],
                "modalities": {"input": ["text"], "output": ["text", "audio"]}}),
            json!({"slug": "future-nested", "input_modalities": ["text"],
                "modalities": {"input": ["text"], "output": ["text", "image"]}}),
        ] {
            let model = discovered_model(&entry).expect("listed model");
            assert_eq!(
                model
                    .capabilities
                    .iter()
                    .filter(|capability| capability.as_str() == Capability::MediaImage.as_str())
                    .count(),
                1,
                "{} must declare the hosted image adapter exactly once",
                model.id
            );
            let output = model.metadata["modalities"]["output"]
                .as_array()
                .expect("registered output modalities");
            assert!(output.contains(&json!("text")), "{}", model.id);
            assert!(output.contains(&json!("image")), "{}", model.id);
            if model.id == "future-mixed" {
                assert!(output.contains(&json!("audio")));
            }
        }
    }

    #[test]
    fn discovered_model_image_input_does_not_qualify_unsupported_models() {
        for id in ["future-vision", "gpt-5", "gpt-6.1-sol", "future-unknown"] {
            let model = discovered_model(&json!({
                "slug": id,
                "input_modalities": ["text", "image"]
            }))
            .expect("listed model");
            assert!(
                !model
                    .capabilities
                    .iter()
                    .any(|capability| capability == Capability::MediaImage.as_str()),
                "{id}"
            );
            assert_eq!(model.metadata["modalities"]["output"], json!(["text"]));
        }
    }

    #[test]
    fn discovered_image_only_model_preserves_output_without_chat_capability() {
        let model = discovered_model(&json!({
            "slug": "future-image-only",
            "selector": "exact/upstream/selection",
            "input_modalities": ["text"],
            "output_modalities": ["image"]
        }))
        .expect("listed model");
        assert_eq!(model.id, "future-image-only");
        assert_eq!(model.selector.as_deref(), Some("exact/upstream/selection"));
        assert_eq!(model.metadata["modalities"]["output"], json!(["image"]));
        assert_eq!(model.capabilities, vec![Capability::MediaImage.as_str()]);
    }

    #[test]
    fn discovered_unknown_capabilities_do_not_imply_image_output() {
        let model = discovered_model(&json!({
            "slug": "future-unknown",
            "capabilities": ["unknown"],
            "experimental_supported_tools": ["unknown"],
            "supports_image_detail_original": true
        }))
        .expect("listed model");
        assert_eq!(model.capabilities, vec!["unknown"]);
        assert!(!model.metadata.contains_key("modalities"));
    }

    #[test]
    fn discovered_empty_output_does_not_gain_chat_or_image_capability() {
        let model = discovered_model(&json!({
            "slug":"future-empty-output",
            "input_modalities":["text"],
            "output_modalities":[]
        }))
        .expect("listed model");
        assert_eq!(model.metadata["modalities"]["output"], json!([]));
        assert!(model.capabilities.is_empty());
    }

    #[test]
    fn websocket_continuation_error_requires_exact_unstarted_rejection() {
        let rejection = json!({
            "type": "error",
            "status": 400,
            "error": {
                "type": "invalid_request_error",
                "message": "Invalid `previous_response_id`."
            }
        });
        assert!(matches!(
            classify_codex_websocket_error(&rejection, false, true).map(|error| error.kind),
            Some(ErrorKind::ContinuationNotFound)
        ));
        assert!(classify_codex_websocket_error(&rejection, false, false).is_none());
        assert!(classify_codex_websocket_error(&rejection, true, true).is_none());
        let ordinary_bad_request = json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": "Invalid `input`: unsupported content"
            }
        });
        assert!(classify_codex_websocket_error(&ordinary_bad_request, false, true).is_none());
        let mut unrelated_code = rejection.clone();
        unrelated_code["error"]["code"] = json!("invalid_parameter");
        assert!(classify_codex_websocket_error(&unrelated_code, false, true).is_none());
        let mut extended_message = rejection.clone();
        extended_message["error"]["message"] =
            json!("Invalid `previous_response_id`: unsupported syntax");
        assert!(classify_codex_websocket_error(&extended_message, false, true).is_none());
    }
}
