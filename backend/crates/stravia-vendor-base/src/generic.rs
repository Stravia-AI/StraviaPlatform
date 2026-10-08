use std::collections::BTreeMap;
use stravia_vendor_common::common;
use stravia_vendor_common::thinking;

use serde_json::{Value, json};
use stravia_protocol_codec::codec::openai::compatible::chat_completions::OpenAIChatCompletionsV1;
use stravia_protocol_codec::registry::ProtocolRegistry;
use stravia_runtime_contract::protocol::ids::{
    ANTHROPIC_MESSAGES_2023_06_01, COHERE_CHAT_V2, GATEWAY_LANGUAGE_MODEL_V4,
    GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA, OPEN_RESPONSES_2026_04_24,
    OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, OPENAI_COMPATIBLE_EMBEDDINGS_V1, Protocol,
};
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_vendor_sdk::{
    ConfigValidationResponse, DiscoverRequest, DiscoverResponse, DiscoveredModel, ErrorKind,
    GuestHost, HttpRequest, MODELS_SOURCE_CATALOG, Operation, OperationInput, OperationOutput,
    PluginError, ProviderSnapshot, ValidationIssue, read_http_body,
};

use crate::metadata::ACCOUNT_DISCOVERY_PROVIDER_IDS;

const MAX_JSON_BODY: usize = 8 * 1024 * 1024;
const OPENAI_CHAT_PROTOCOL: &str = "openai-compatible/chat-completions/v1";
const OPEN_RESPONSES_PROTOCOL: &str = "open-responses/responses/2026-04-24";
const ANTHROPIC_PROTOCOL: &str = "anthropic-messages/messages/2023-06-01";
const GEMINI_PROTOCOL: &str = "google-gemini/generate-content/v1beta";
const COHERE_PROTOCOL: &str = "cohere-chat/chat/v2";
const GATEWAY_PROTOCOL: &str = "gateway-language-model/language-model/v4";

pub(crate) fn execute(
    vendor_id: &str,
    host: &GuestHost,
    operation: Operation,
    channel: &str,
    input: OperationInput,
) -> Result<OperationOutput, PluginError> {
    match input {
        OperationInput::Infer {
            provider,
            mut request,
        } if operation == Operation::Infer => {
            check_channel(vendor_id, channel, &provider)?;
            let protocol = protocol_for(vendor_id, &provider)?;
            if vendor_id == "ollama" {
                prepare_ollama_request(host, &provider, &mut request)?;
            }
            execute_inference(vendor_id, host, &provider, request, &protocol)
        }
        OperationInput::Compact {
            mut provider,
            request,
        } if operation == Operation::Compact => {
            check_channel(vendor_id, channel, &provider)?;
            let protocol = protocol_for(vendor_id, &provider)?;
            if protocol != OPEN_RESPONSES_PROTOCOL {
                return Err(common::plugin_error(
                    ErrorKind::Unsupported,
                    "native compaction requires the Open Responses protocol",
                ));
            }
            provider.protocol = protocol;
            crate::openai::execute_compact(host, provider, request)
        }
        OperationInput::Discover { provider, request } if operation == Operation::Discover => {
            check_channel(vendor_id, channel, &provider)?;
            discover(vendor_id, host, &provider, request).map(OperationOutput::Discover)
        }
        OperationInput::ConfigValidation { provider, request }
            if operation == Operation::ConfigValidation && vendor_id == "cloudflare-ai-gateway" =>
        {
            check_channel(vendor_id, channel, &provider)?;
            Ok(OperationOutput::ConfigValidation(
                validate_cloudflare_config(&provider, &request.options),
            ))
        }
        OperationInput::ConfigValidation { provider, request }
            if operation == Operation::ConfigValidation && vendor_id == "custom" =>
        {
            check_channel(vendor_id, channel, &provider)?;
            let issues = selected_custom_reasoning_field(&request.options)
                .err()
                .map(|field| ValidationIssue {
                    field: Some(field.into()),
                    code: "invalid_reasoning_field".into(),
                    message: crate::messages::reasoning_field_invalid(),
                })
                .into_iter()
                .collect();
            Ok(OperationOutput::ConfigValidation(
                ConfigValidationResponse {
                    issues,
                    proposed_base_url: None,
                },
            ))
        }
        other => Err(common::unsupported(
            other.operation().as_str(),
            vendor_id,
            channel,
        )),
    }
}

pub(crate) fn execute_inference(
    vendor_id: &str,
    host: &GuestHost,
    provider: &ProviderSnapshot,
    mut request: AiRequest,
    protocol: &str,
) -> Result<OperationOutput, PluginError> {
    let model = provider
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| common::plugin_error(ErrorKind::Invalid, "inference requires a model"))?;
    request.model = model.to_owned();
    if !matches!(protocol, COHERE_PROTOCOL | GATEWAY_PROTOCOL) {
        thinking::apply(
            vendor_id,
            &provider.channel,
            provider,
            protocol,
            &mut request,
        )?;
    }
    let preserve_upstream_errors = protocol == OPEN_RESPONSES_PROTOCOL
        && stravia_protocol_codec::codec::compaction::native_compaction_requested(&request);
    let reasoning_source = if provider.provider_id == "custom" && protocol == OPENAI_CHAT_PROTOCOL {
        selected_custom_reasoning_field(&provider.options).map_err(|field| {
            common::plugin_error(
                ErrorKind::Invalid,
                format!("invalid Custom reasoning field configuration: {field}"),
            )
        })?
    } else {
        None
    };
    let mut encoded = crate::encode_inference_request(protocol, &request)?;
    prepare_custom_reasoning_history(provider, protocol, reasoning_source, &mut encoded.body);
    let mut headers = common::header_pairs(&encoded.headers)?;
    for (name, value) in &provider.client_headers {
        set_header(&mut headers, name, value.clone());
    }
    crate::apply_session_affinity(provider, &mut headers);
    set_header(&mut headers, "content-type", "application/json".into());
    apply_auth_headers(vendor_id, provider, protocol, &mut headers)?;
    let url = inference_url(vendor_id, provider, protocol, &encoded.path)?;
    prepare_gemini_imported_tool_history(protocol, model, &url, &mut encoded.body);
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
    if protocol == OPEN_RESPONSES_PROTOCOL {
        let decoded = if preserve_upstream_errors {
            stravia_vendor_common::common::decode_ai_response_preserving_upstream_errors(
                host,
                protocol,
                response,
                common::HttpResponseMode::HeaderDetected,
                classify_open_responses_error,
            )
        } else {
            stravia_vendor_common::common::decode_ai_response_with_error_classifier(
                host,
                protocol,
                response,
                common::HttpResponseMode::HeaderDetected,
                classify_open_responses_error,
            )
        };
        return decoded.map(Box::new).map(OperationOutput::Infer);
    }
    if let Some(field) = reasoning_source {
        let adapter = OpenAIChatCompletionsV1::with_response_reasoning_field(field);
        return crate::decode_inference_with_adapter(host, &adapter, response);
    }
    crate::decode_inference(host, protocol, response)
}

fn selected_custom_reasoning_field(
    options: &BTreeMap<String, Value>,
) -> Result<Option<&str>, &'static str> {
    let field = match options.get("reasoning_field") {
        None => return Ok(None),
        Some(Value::String(value)) => match value.as_str() {
            "protocol_default" => return Ok(None),
            "reasoning_content" => "reasoning_content",
            "reasoning" => "reasoning",
            "custom" => options
                .get("custom_reasoning_field")
                .and_then(Value::as_str)
                .filter(|field| !field.is_empty() && field.trim() == *field)
                .ok_or("custom_reasoning_field")?,
            _ => return Err("reasoning_field"),
        },
        Some(_) => return Err("reasoning_field"),
    };
    if field.starts_with("__")
        || matches!(
            field,
            "role"
                | "content"
                | "name"
                | "tool_calls"
                | "tool_call_id"
                | "function_call"
                | "refusal"
                | "audio"
                | "annotations"
                | "reasoning_details"
        )
    {
        return Err("custom_reasoning_field");
    }
    Ok(Some(field))
}

fn prepare_custom_reasoning_history(
    provider: &ProviderSnapshot,
    protocol: &str,
    reasoning_source: Option<&str>,
    body: &mut Value,
) {
    if provider.provider_id != "custom" || protocol != OPENAI_CHAT_PROTOCOL {
        return;
    }
    let field = reasoning_source.unwrap_or("reasoning_content");
    let ensure = provider
        .options
        .get("ensure_reasoning_field")
        .and_then(Value::as_bool)
        == Some(true);
    if field == "reasoning_content" && !ensure {
        return;
    }
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for message in messages {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(fields) = message.as_object_mut() else {
            continue;
        };
        if field != "reasoning_content"
            && let Some(reasoning) = fields.remove("reasoning_content")
        {
            // canonical 思考是权威来源，只在出站边界换成上游选择的载体。
            fields.insert(field.into(), reasoning);
        }
        if ensure && fields.get(field).is_none_or(Value::is_null) {
            // 只补齐上游模板要求的字段，不伪造推理，也不改写权威历史。
            fields.insert(field.into(), Value::String(String::new()));
        }
    }
}

/// Google's documented imported-history validator control, not a native signature.
/// This is request-only and deliberately excludes Vertex and unverified proxies.
fn prepare_gemini_imported_tool_history(protocol: &str, model: &str, url: &str, body: &mut Value) {
    if protocol != GEMINI_PROTOCOL {
        return;
    }
    let Some(version_suffix) = model
        .strip_prefix("models/")
        .unwrap_or(model)
        .strip_prefix("gemini-3")
    else {
        return;
    };
    // Accept the major-only name and numeric minor versions, not aliases or
    // a prefix match on a different major (e.g. gemini-30 or gemini-4).
    let major_three = if let Some(name) = version_suffix.strip_prefix('-') {
        !name.is_empty()
    } else if let Some((minor, name)) = version_suffix
        .strip_prefix('.')
        .and_then(|suffix| suffix.split_once('-'))
    {
        !minor.is_empty() && minor.bytes().all(|byte| byte.is_ascii_digit()) && !name.is_empty()
    } else {
        false
    };
    if !major_three {
        return;
    }
    // Ordinary text requests need no endpoint parsing or allocations here.
    if !body
        .get("contents")
        .and_then(Value::as_array)
        .is_some_and(|contents| {
            contents.iter().any(|content| {
                content.get("role").and_then(Value::as_str) == Some("model")
                    && content
                        .get("parts")
                        .and_then(Value::as_array)
                        .is_some_and(|parts| {
                            parts.iter().any(|part| part.get("functionCall").is_some())
                        })
            })
        })
    {
        return;
    }
    let Ok(endpoint) = url::Url::parse(url) else {
        return;
    };
    if endpoint.scheme() != "https"
        || endpoint.host_str() != Some("generativelanguage.googleapis.com")
        || endpoint.port_or_known_default() != Some(443)
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || !endpoint
            .path()
            .strip_prefix("/v1beta/models/")
            .is_some_and(|path| {
                !path.contains('/')
                    && (path.ends_with(":generateContent")
                        || path.ends_with(":streamGenerateContent"))
            })
    {
        return;
    }
    let Some(contents) = body.get_mut("contents").and_then(Value::as_array_mut) else {
        return;
    };
    // Tool results use the user role too, but do not start a new user turn.
    let start = contents
        .iter()
        .rposition(|content| {
            content.get("role").and_then(Value::as_str) == Some("user")
                && content
                    .get("parts")
                    .and_then(Value::as_array)
                    .is_some_and(|parts| {
                        parts.iter().any(|part| {
                            part.as_object().is_some_and(|fields| {
                                fields.keys().any(|key| {
                                    matches!(
                                        key.as_str(),
                                        "text" | "inlineData" | "fileData" | "videoMetadata"
                                    )
                                })
                            })
                        })
                    })
        })
        .map_or(0, |index| index + 1);
    for content in &mut contents[start..] {
        if content.get("role").and_then(Value::as_str) != Some("model") {
            continue;
        }
        let Some(parts) = content.get_mut("parts").and_then(Value::as_array_mut) else {
            continue;
        };
        let Some(first_call) = parts
            .iter_mut()
            .find(|part| part.get("functionCall").is_some())
        else {
            continue;
        };
        let missing = match first_call.get("thoughtSignature") {
            None | Some(Value::Null) => true,
            Some(Value::String(signature)) => signature.is_empty(),
            _ => false,
        };
        if missing && let Some(fields) = first_call.as_object_mut() {
            fields.insert(
                "thoughtSignature".into(),
                Value::String("skip_thought_signature_validator".into()),
            );
        }
    }
}

fn classify_open_responses_error(value: &Value, saw_response_event: bool) -> Option<PluginError> {
    if saw_response_event {
        return None;
    }
    let code = value
        .pointer("/error/code")
        .or_else(|| value.pointer("/error/type"))
        .or_else(|| value.pointer("/response/error/code"))
        .or_else(|| value.pointer("/response/error/type"))
        .or_else(|| value.get("code"))
        .or_else(|| value.get("type"))
        .and_then(Value::as_str);
    (code == Some("invalid_encrypted_content")).then(|| {
        common::plugin_error(
            ErrorKind::ProtectedReasoningRejected,
            "upstream rejected protected reasoning replay",
        )
    })
}

/// 选择该协议上游错误体的受保护推理拒绝识别器。返回值是 `fn` 指针，
/// 与 `decode_ai_response_with_error_classifier` 的参数形状一致；
/// 不识别该形态的协议返回恒 `None` 的占位实现。
pub(crate) fn inference_error_classifier_for(
    protocol: Protocol,
) -> fn(&Value, bool) -> Option<PluginError> {
    match protocol {
        Protocol::AnthropicMessages => classify_anthropic_error,
        Protocol::GoogleGemini => classify_gemini_error,
        Protocol::BedrockConverse => classify_bedrock_error,
        _ => no_inference_error_classification,
    }
}

pub(crate) fn no_inference_error_classification(_: &Value, _: bool) -> Option<PluginError> {
    None
}

fn protected_reasoning_rejected(vendor: &str) -> PluginError {
    common::plugin_error(
        ErrorKind::ProtectedReasoningRejected,
        format!("{vendor} rejected protected reasoning replay"),
    )
}

pub(crate) use common::classify_anthropic_error;

/// Bedrock Converse：校验失败有两种外形——HTTP 错误体
/// `{"message": .., "__type"/"code": "..ValidationException"}`（异常类型常在
/// `x-amzn-ErrorType` 头里，body 可能不带类型字段），以及流内异常事件
/// `{"validationException": {"message": ..}}`。Bedrock 原样转发 Claude 的
/// thinking 签名校验文案，message 命中同一组特征即可判定；若 body 带了
/// 异常类型却不是 ValidationException，则不归类为受保护推理拒绝。
pub(crate) fn classify_bedrock_error(
    value: &Value,
    saw_response_event: bool,
) -> Option<PluginError> {
    if saw_response_event {
        return None;
    }
    let mut candidates: Vec<(Option<&str>, &str)> = Vec::new();
    if let Some(message) = value
        .get("message")
        .or_else(|| value.get("Message"))
        .and_then(Value::as_str)
    {
        let exception_type = ["__type", "code", "errorType", "type"]
            .iter()
            .find_map(|key| value.get(*key).and_then(Value::as_str));
        candidates.push((exception_type, message));
    }
    if let Some(object) = value.as_object() {
        for (key, inner) in object {
            // ConverseStream 异常事件以异常名为键、内嵌 message。
            if !key.to_ascii_lowercase().ends_with("exception") {
                continue;
            }
            if let Some(message) = inner
                .get("message")
                .or_else(|| inner.get("Message"))
                .and_then(Value::as_str)
            {
                candidates.push((Some(key.as_str()), message));
            }
        }
    }
    candidates
        .into_iter()
        .any(|(exception_type, message)| {
            exception_type.is_none_or(|ty| {
                ty.to_ascii_lowercase()
                    .replace(['_', '-', '.', '#'], "")
                    .contains("validationexception")
            }) && common::anthropic_protected_reasoning_message(message)
        })
        .then(|| protected_reasoning_rejected("Bedrock"))
}

/// Gemini generateContent / Vertex：400 错误体
/// `{"error":{"code":400,"status":"INVALID_ARGUMENT","message":..}}`。
/// 官方文档（ai.google.dev/gemini-api/docs/thought-signatures）与社区实录的
/// 拒绝文案为 "Invalid thought signature" / "Corrupted thought signature" /
/// "Thought signature is not valid"；大小写不敏感地匹配 thought signature
/// （含 thought_signature）+ invalid/not valid/corrupt。缺失签名
/// （missing_thought_signature）不是回放载荷被拒，不在此列。
pub(crate) fn classify_gemini_error(
    value: &Value,
    saw_response_event: bool,
) -> Option<PluginError> {
    if saw_response_event {
        return None;
    }
    let error = value.get("error")?;
    let status = error.get("status").and_then(Value::as_str);
    let code = error.get("code").and_then(Value::as_u64);
    let invalid_argument =
        status == Some("INVALID_ARGUMENT") || (status.is_none() && code == Some(400));
    if !invalid_argument {
        return None;
    }
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();
    let mentions_signature =
        message.contains("thought signature") || message.contains("thought_signature");
    let rejected =
        message.contains("invalid") || message.contains("not valid") || message.contains("corrupt");
    (mentions_signature && rejected).then(|| protected_reasoning_rejected("Gemini"))
}

pub(crate) fn select_protocol(provider: &ProviderSnapshot, request: &AiRequest) -> String {
    if request.embedding.is_some()
        && ProtocolRegistry::global()
            .resolve_alias(&provider.protocol)
            .is_some_and(|endpoint| endpoint.protocol == OPENAI_COMPATIBLE_EMBEDDINGS_V1.protocol)
    {
        OPENAI_COMPATIBLE_EMBEDDINGS_V1.to_string()
    } else {
        provider.protocol.clone()
    }
}

fn protocol_for(vendor_id: &str, provider: &ProviderSnapshot) -> Result<String, PluginError> {
    let fixed = match vendor_id {
        "kimi-for-coding" | "minimax-coding-plan" | "minimax-cn-coding-plan" => {
            Some(ANTHROPIC_PROTOCOL)
        }
        "github-copilot"
        | "nano-gpt"
        | "zai-coding-plan"
        | "zhipuai-coding-plan"
        | "wafer.ai"
        | "opencode-go"
        | "crof"
        | "neuralwatt" => Some(OPENAI_CHAT_PROTOCOL),
        "cohere" => Some(COHERE_PROTOCOL),
        "gateway" => Some(GATEWAY_PROTOCOL),
        _ => None,
    };
    let selected = fixed.unwrap_or(provider.protocol.trim());
    if selected.is_empty() {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            format!("provider `{vendor_id}` has no selected egress protocol"),
        ));
    }
    // Resolve here so an installed vendor cannot silently substitute a family
    // default when the saved connection names an unsupported endpoint.
    let endpoint = match selected {
        COHERE_PROTOCOL => COHERE_CHAT_V2,
        GATEWAY_PROTOCOL => GATEWAY_LANGUAGE_MODEL_V4,
        _ => common::endpoint(selected)?,
    };
    let supported = match vendor_id {
        "google" => endpoint == GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        "openai-compatible" => matches!(
            endpoint,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1 | OPENAI_COMPATIBLE_EMBEDDINGS_V1
        ),
        "kimi-for-coding" | "minimax-coding-plan" | "minimax-cn-coding-plan" => {
            endpoint == ANTHROPIC_MESSAGES_2023_06_01
        }
        "custom" => matches!(
            endpoint,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1
                | OPENAI_COMPATIBLE_EMBEDDINGS_V1
                | OPEN_RESPONSES_2026_04_24
                | ANTHROPIC_MESSAGES_2023_06_01
                | GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA
        ),
        "xai" => matches!(
            endpoint,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1 | OPEN_RESPONSES_2026_04_24
        ),
        "cohere" => endpoint == COHERE_CHAT_V2,
        "gateway" => endpoint == GATEWAY_LANGUAGE_MODEL_V4,
        _ => endpoint == OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
    };
    if !supported {
        return Err(common::plugin_error(
            ErrorKind::Unsupported,
            format!("vendor `{vendor_id}` does not support protocol `{selected}`"),
        ));
    }
    Ok(endpoint.to_string())
}

fn validate_cloudflare_config(
    provider: &ProviderSnapshot,
    proposed: &BTreeMap<String, Value>,
) -> ConfigValidationResponse {
    let merged = |key: &str| {
        proposed
            .get(key)
            .or_else(|| provider.options.get(key))
            .or_else(|| provider.credentials.get(key))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    let mut issues = Vec::new();
    for (field, message) in [
        (
            "accountId",
            crate::messages::cloudflare_account_id_required(),
        ),
        (
            "gatewayId",
            crate::messages::cloudflare_gateway_id_required(),
        ),
    ] {
        if merged(field).is_none() {
            issues.push(ValidationIssue {
                field: Some(field.to_owned()),
                code: "required".to_owned(),
                message,
            });
        }
    }
    let proposed_base_url = if issues.is_empty() && provider.base_url.trim().is_empty() {
        merged("accountId")
            .zip(merged("gatewayId"))
            .map(|(account, gateway)| cloudflare_base_url(account, gateway))
    } else {
        None
    };
    ConfigValidationResponse {
        issues,
        proposed_base_url,
    }
}

fn cloudflare_base_url(account: &str, gateway: &str) -> String {
    format!(
        "https://gateway.ai.cloudflare.com/v1/{}/{}/compat",
        urlencoding::encode(account.trim()),
        urlencoding::encode(gateway.trim())
    )
}

fn inference_url(
    vendor_id: &str,
    provider: &ProviderSnapshot,
    protocol: &str,
    path: &str,
) -> Result<String, PluginError> {
    if vendor_id == "cloudflare-ai-gateway" {
        let base = if provider.base_url.trim().is_empty() {
            let account = setting(provider, "accountId").unwrap_or_default();
            let gateway = setting(provider, "gatewayId").unwrap_or_default();
            if account.trim().is_empty() || gateway.trim().is_empty() {
                return Err(common::plugin_error(
                    ErrorKind::Invalid,
                    "Cloudflare accountId and gatewayId are required",
                ));
            }
            cloudflare_base_url(account, gateway)
        } else {
            provider.base_url.trim().to_owned()
        };
        let compat_path = path.strip_prefix("/v1/").unwrap_or(path);
        return common::endpoint_url(&base, compat_path);
    }
    let mut url = common::endpoint_url(&provider.base_url, path)?;
    if protocol == GEMINI_PROTOCOL
        && matches!(vendor_id, "google" | "custom")
        && let Some(key) = setting(provider, "apiKey").filter(|key| !key.trim().is_empty())
    {
        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str("key=");
        url.push_str(&urlencoding::encode(key));
    }
    Ok(url)
}

fn set_header(headers: &mut Vec<(String, String)>, name: &str, value: String) {
    headers.retain(|(candidate, _)| !candidate.eq_ignore_ascii_case(name));
    headers.push((name.to_owned(), value));
}

fn apply_auth_headers(
    vendor_id: &str,
    provider: &ProviderSnapshot,
    protocol: &str,
    headers: &mut Vec<(String, String)>,
) -> Result<(), PluginError> {
    headers.retain(|(name, _)| !name.eq_ignore_ascii_case("authorization"));
    if protocol == GEMINI_PROTOCOL && matches!(vendor_id, "google" | "custom") {
        return Ok(());
    }
    if vendor_id == "cloudflare-ai-gateway" {
        headers.push((
            "cf-aig-authorization".to_owned(),
            format!("Bearer {}", credential(provider, "apiToken")?),
        ));
        return Ok(());
    }
    let token = setting(provider, "apiKey").filter(|token| !token.trim().is_empty());
    if protocol == ANTHROPIC_PROTOCOL {
        if let Some(token) = token {
            headers.push(("x-api-key".to_owned(), token.to_owned()));
        }
        headers.push(("anthropic-version".to_owned(), "2023-06-01".to_owned()));
    } else if let Some(token) = token {
        headers.push(("authorization".to_owned(), format!("Bearer {token}")));
    }
    if vendor_id == "openrouter" {
        if let Some(value) = setting(provider, "httpReferer").filter(|v| !v.trim().is_empty()) {
            headers.push(("HTTP-Referer".to_owned(), value.to_owned()));
        }
        if let Some(value) = setting(provider, "xTitle").filter(|v| !v.trim().is_empty()) {
            headers.push(("X-Title".to_owned(), value.to_owned()));
        }
    }
    Ok(())
}

pub(crate) fn explicit_discovery(
    vendor_id: &str,
    provider: &ProviderSnapshot,
) -> Result<Option<DiscoverResponse>, PluginError> {
    if let Some(static_models) = provider.operation_metadata.get("static_models") {
        let values = static_models.as_array().ok_or_else(|| {
            common::plugin_error(
                ErrorKind::Invalid,
                "operation_metadata.static_models must be a JSON array",
            )
        })?;
        let mut models = BTreeMap::new();
        for (index, value) in values.iter().enumerate() {
            let id = value.as_str().ok_or_else(|| {
                common::plugin_error(
                    ErrorKind::Invalid,
                    format!("operation_metadata.static_models[{index}] must be a string"),
                )
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
            thinking::decorate_discovered_model(vendor_id, &mut model);
            models.entry(id.to_owned()).or_insert(model);
        }
        return Ok(Some(DiscoverResponse {
            models: models.into_values().collect(),
            next_cursor: None,
        }));
    }

    let catalog_selected = provider
        .operation_metadata
        .get("models_source")
        .and_then(Value::as_str)
        .map(str::trim)
        == Some(MODELS_SOURCE_CATALOG);
    // 历史原生连接也保存 catalog 来源标记，但账户清单或渠道静态清单
    // 仍决定可调用范围。目录别名不能继承另一供应商的账户发现策略。
    if !catalog_selected || ACCOUNT_DISCOVERY_PROVIDER_IDS.contains(&vendor_id) {
        return Ok(None);
    }
    let sources = provider
        .operation_metadata
        .get("catalog_models")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            common::plugin_error(
                ErrorKind::Invalid,
                "catalog model discovery requires operation_metadata.catalog_models",
            )
        })?;
    let mut models = BTreeMap::new();
    for (index, metadata) in sources.iter().enumerate() {
        if !metadata.is_object() {
            return Err(common::plugin_error(
                ErrorKind::Invalid,
                format!("operation_metadata.catalog_models[{index}] must be an object"),
            ));
        }
        if !metadata
            .get("id")
            .or_else(|| metadata.get("name"))
            .and_then(Value::as_str)
            .is_some_and(|id| !id.trim().is_empty())
        {
            return Err(common::plugin_error(
                ErrorKind::Invalid,
                format!("operation_metadata.catalog_models[{index}] has no model id"),
            ));
        }
        let mut model = discovered_model(metadata)?;
        thinking::decorate_discovered_model(vendor_id, &mut model);
        models.insert(model.id.clone(), model);
    }
    Ok(Some(DiscoverResponse {
        models: models.into_values().collect(),
        next_cursor: None,
    }))
}

fn model_discovery_request(
    vendor_id: &str,
    provider: &ProviderSnapshot,
    request: &DiscoverRequest,
) -> Result<HttpRequest, PluginError> {
    let mut headers = vec![("accept".to_owned(), "application/json".to_owned())];
    let configured_source = provider
        .operation_metadata
        .get("models_source")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|source| !source.is_empty() && *source != MODELS_SOURCE_CATALOG);
    let protocol = protocol_for(vendor_id, provider)?;
    let google = protocol == GEMINI_PROTOCOL;
    let mut native_google_models = google && configured_source.is_none();
    let mut url = if let Some(source) = configured_source {
        let mut url = url::Url::parse(source).map_err(|_| {
            common::plugin_error(ErrorKind::Invalid, "model discovery URL is invalid")
        })?;
        let official_google_native = (google || vendor_id == "google")
            && url.scheme() == "https"
            && url.host_str() == Some("generativelanguage.googleapis.com")
            && url.port_or_known_default() == Some(443)
            && url.path() == "/v1beta/models";
        if official_google_native {
            native_google_models = true;
            if let Some(key) = setting(provider, "apiKey").filter(|key| !key.trim().is_empty()) {
                url.query_pairs_mut().append_pair("key", key);
            }
        } else if google {
            // 非官方显式目录保留原有 Bearer 约定，不能仅凭路径推断 Google 认证。
            if let Some(token) = setting(provider, "apiKey").filter(|key| !key.trim().is_empty()) {
                headers.push(("authorization".to_owned(), format!("Bearer {token}")));
            }
        } else {
            apply_auth_headers(vendor_id, provider, &protocol, &mut headers)?;
        }
        url.into()
    } else if google {
        let mut url = common::endpoint_url(&provider.base_url, "/v1beta/models")?;
        if let Some(key) = setting(provider, "apiKey").filter(|key| !key.trim().is_empty()) {
            url.push_str("?key=");
            url.push_str(&urlencoding::encode(key));
        }
        url
    } else {
        apply_auth_headers(vendor_id, provider, &protocol, &mut headers)?;
        common::model_discovery_url(&provider.base_url)?
    };
    if let Some(cursor) = request
        .cursor
        .as_deref()
        .filter(|cursor| !cursor.trim().is_empty())
    {
        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str(if native_google_models {
            "pageToken="
        } else {
            "after="
        });
        url.push_str(&urlencoding::encode(cursor));
    }
    Ok(HttpRequest {
        method: "GET".to_owned(),
        url,
        headers,
        body: Vec::new(),
    })
}

fn discover(
    vendor_id: &str,
    host: &GuestHost,
    provider: &ProviderSnapshot,
    request: DiscoverRequest,
) -> Result<DiscoverResponse, PluginError> {
    let response = host.http_start(model_discovery_request(vendor_id, provider, &request)?)?;
    let status = response.status()?;
    let headers = response.headers()?;
    let body = read_http_body(&response, MAX_JSON_BODY)?;
    if !(200..300).contains(&status) {
        return Err(common::upstream_error(status, &headers, &body));
    }
    let value: Value = serde_json::from_slice(&body).map_err(|error| {
        common::plugin_error(
            ErrorKind::upstream_unknown(),
            format!("invalid model discovery response: {error}"),
        )
    })?;
    let rows = value
        .get("data")
        .or_else(|| value.get("models"))
        .and_then(Value::as_array)
        .ok_or_else(|| {
            common::plugin_error(
                ErrorKind::upstream_unknown(),
                "model discovery response has no data/models array",
            )
        })?;
    let models = rows
        .iter()
        .filter(|value| {
            value
                .get("visibility")
                .and_then(Value::as_str)
                .is_none_or(|visibility| visibility.eq_ignore_ascii_case("list"))
        })
        .map(|value| {
            let mut model = discovered_model(value)?;
            thinking::decorate_discovered_model(vendor_id, &mut model);
            Ok(model)
        })
        .collect::<Result<Vec<_>, PluginError>>()?;
    let next_cursor = value
        .get("next_cursor")
        .or_else(|| value.get("nextPageToken"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            value
                .get("has_more")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                .then(|| models.last().map(|model| model.id.clone()))
                .flatten()
        });
    Ok(DiscoverResponse {
        models,
        next_cursor,
    })
}

fn discovered_model(value: &Value) -> Result<DiscoveredModel, PluginError> {
    let raw_id = value
        .as_str()
        .or_else(|| {
            value
                .get("id")
                .or_else(|| value.get("name"))
                .and_then(Value::as_str)
        })
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            common::plugin_error(
                ErrorKind::upstream_unknown(),
                "model discovery returned a row without an id/name",
            )
        })?;
    let id = raw_id.strip_prefix("models/").unwrap_or(raw_id).to_owned();
    let display_name = value
        .get("display_name")
        .or_else(|| value.get("displayName"))
        .and_then(Value::as_str)
        .unwrap_or(&id)
        .to_owned();
    let metadata = value
        .as_object()
        .map(stravia_vendor_common::thinking::source_metadata)
        .unwrap_or_default();
    let family = value
        .get("family")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let selector = value
        .get("selector")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let capabilities = value
        .get("capabilities")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    Ok(DiscoveredModel {
        id,
        display_name,
        family,
        selector,
        capabilities,
        metadata: metadata.into_iter().collect(),
    })
}

fn prepare_ollama_request(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    request: &mut AiRequest,
) -> Result<(), PluginError> {
    if request.tools.as_ref().is_none_or(Vec::is_empty) && request.tool_choice.is_none() {
        return Ok(());
    }
    let model = provider
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| common::plugin_error(ErrorKind::Invalid, "inference requires a model"))?;
    let mut base = url::Url::parse(&provider.base_url).map_err(|error| {
        common::plugin_error(
            ErrorKind::Invalid,
            format!("invalid Ollama base URL: {error}"),
        )
    })?;
    let path = base.path().trim_end_matches('/');
    let path = path.strip_suffix("/v1").unwrap_or(path);
    let endpoint = format!("{path}/api/show");
    base.set_path(&endpoint);
    base.set_query(None);
    let mut headers = vec![("content-type".to_owned(), "application/json".to_owned())];
    if let Some(api_key) = setting(provider, "apiKey").filter(|key| !key.trim().is_empty()) {
        headers.push(("authorization".to_owned(), format!("Bearer {api_key}")));
    }
    let body = serde_json::to_vec(&json!({"name": model})).map_err(|error| {
        common::plugin_error(ErrorKind::Trapped, format!("encode Ollama probe: {error}"))
    })?;
    let response = match host.http_start(HttpRequest {
        method: "POST".to_owned(),
        url: base.to_string(),
        headers,
        body,
    }) {
        Ok(response) => response,
        Err(error)
            if !matches!(
                error.kind,
                ErrorKind::Cancelled | ErrorKind::DeadlineExceeded
            ) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let status = match response.status() {
        Ok(status) => status,
        Err(error)
            if !matches!(
                error.kind,
                ErrorKind::Cancelled | ErrorKind::DeadlineExceeded
            ) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    if !(200..300).contains(&status) {
        return Ok(());
    }
    let body = match read_http_body(&response, MAX_JSON_BODY) {
        Ok(body) => body,
        Err(error)
            if !matches!(
                error.kind,
                ErrorKind::Cancelled | ErrorKind::DeadlineExceeded
            ) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let supports_tools = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|value| value.get("capabilities").and_then(Value::as_array).cloned())
        .is_some_and(|capabilities| {
            capabilities
                .iter()
                .filter_map(Value::as_str)
                .any(|capability| capability == "tools")
        });
    if !supports_tools {
        request.tools = None;
        request.tool_choice = None;
        request.meta.vendor.ingress.remove("tools");
        request.meta.vendor.ingress.remove("tool_choice");
    }
    Ok(())
}

fn credential<'a>(provider: &'a ProviderSnapshot, key: &str) -> Result<&'a str, PluginError> {
    setting(provider, key)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            common::plugin_error(ErrorKind::Auth, format!("credential `{key}` is missing"))
        })
}

fn setting<'a>(provider: &'a ProviderSnapshot, key: &str) -> Option<&'a str> {
    provider
        .credentials
        .get(key)
        .or_else(|| provider.options.get(key))
        .and_then(Value::as_str)
}

fn check_channel(
    vendor_id: &str,
    channel: &str,
    provider: &ProviderSnapshot,
) -> Result<(), PluginError> {
    if provider.channel != channel {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "provider snapshot channel does not match dispatch channel",
        ));
    }
    let supported = channel == "default";
    supported
        .then_some(())
        .ok_or_else(|| common::unsupported("channel", vendor_id, channel))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(protocol: &str, api_key: &str) -> ProviderSnapshot {
        ProviderSnapshot {
            provider_id: "custom".into(),
            channel: "default".into(),
            base_url: "https://upstream.test/v1".into(),
            protocol: protocol.into(),
            options: BTreeMap::new(),
            credentials: BTreeMap::from([("apiKey".into(), Value::String(api_key.into()))]),
            model: Some("model".into()),
            model_metadata: None,
            client_headers: Vec::new(),
            operation_metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn google_explicit_models_source_keeps_native_and_custom_auth_separate() {
        let mut snapshot = provider(GEMINI_PROTOCOL, "key +&");
        snapshot.operation_metadata.insert(
            "models_source".into(),
            json!("https://generativelanguage.googleapis.com/v1beta/models?pageSize=10"),
        );
        let cursor = DiscoverRequest {
            cursor: Some("next +&".into()),
        };
        let request = model_discovery_request("google", &snapshot, &cursor).unwrap();
        let url = url::Url::parse(&request.url).unwrap();
        let query: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query.get("key").map(String::as_str), Some("key +&"));
        assert_eq!(query.get("pageToken").map(String::as_str), Some("next +&"));
        assert_eq!(query.get("pageSize").map(String::as_str), Some("10"));
        assert!(
            !request
                .headers
                .iter()
                .any(|(name, _)| name == "authorization")
        );

        snapshot.operation_metadata.insert(
            "models_source".into(),
            json!("https://catalog.example.test/v1beta/models?pageSize=10"),
        );
        let request = model_discovery_request("google", &snapshot, &cursor).unwrap();
        let url = url::Url::parse(&request.url).unwrap();
        let query: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        assert!(!query.contains_key("key"));
        assert_eq!(query.get("after").map(String::as_str), Some("next +&"));
        assert!(
            request
                .headers
                .iter()
                .any(|(name, value)| name == "authorization" && value == "Bearer key +&")
        );
    }

    #[test]
    fn custom_vendor_applies_the_selected_protocol_auth_shape() {
        let mut openai_headers = Vec::new();
        apply_auth_headers(
            "custom",
            &provider(OPENAI_CHAT_PROTOCOL, "sk-test"),
            OPENAI_CHAT_PROTOCOL,
            &mut openai_headers,
        )
        .unwrap();
        assert_eq!(
            openai_headers,
            vec![("authorization".into(), "Bearer sk-test".into())]
        );

        let mut anthropic_headers = Vec::new();
        apply_auth_headers(
            "custom",
            &provider(ANTHROPIC_PROTOCOL, "sk-ant"),
            ANTHROPIC_PROTOCOL,
            &mut anthropic_headers,
        )
        .unwrap();
        assert_eq!(
            anthropic_headers,
            vec![
                ("x-api-key".into(), "sk-ant".into()),
                ("anthropic-version".into(), "2023-06-01".into()),
            ]
        );

        let custom_gemini = provider(GEMINI_PROTOCOL, "sk-custom-gemini");
        let mut custom_gemini_headers = Vec::new();
        apply_auth_headers(
            "custom",
            &custom_gemini,
            GEMINI_PROTOCOL,
            &mut custom_gemini_headers,
        )
        .unwrap();
        assert!(custom_gemini_headers.is_empty());
        assert_eq!(
            inference_url("custom", &custom_gemini, GEMINI_PROTOCOL, "/v1beta/models").unwrap(),
            "https://upstream.test/v1/models?key=sk-custom-gemini"
        );
    }

    #[test]
    fn optional_credentials_suppress_default_auth() {
        let openai = provider("openai-compatible", "");
        let openai_protocol = protocol_for("custom", &openai).unwrap();
        let mut headers = Vec::new();
        apply_auth_headers("custom", &openai, &openai_protocol, &mut headers).unwrap();
        assert!(headers.is_empty());

        let anthropic = provider("anthropic-messages", "");
        let anthropic_protocol = protocol_for("custom", &anthropic).unwrap();
        apply_auth_headers("custom", &anthropic, &anthropic_protocol, &mut headers).unwrap();
        assert_eq!(
            headers,
            vec![("anthropic-version".into(), "2023-06-01".into())]
        );

        let google = provider("google-gemini", "");
        let google_protocol = protocol_for("google", &google).unwrap();
        assert_eq!(
            inference_url("google", &google, &google_protocol, "/v1beta/models").unwrap(),
            "https://upstream.test/v1/models"
        );
    }

    #[test]
    fn custom_and_standard_profiles_accept_only_their_complete_standard_families() {
        for protocol in [
            OPENAI_CHAT_PROTOCOL,
            "openai-compatible/embeddings/v1",
            OPEN_RESPONSES_PROTOCOL,
            ANTHROPIC_PROTOCOL,
            GEMINI_PROTOCOL,
        ] {
            assert_eq!(
                protocol_for("custom", &provider(protocol, "")).unwrap(),
                protocol
            );
        }

        let embeddings = provider("openai-compatible", "");
        let mut request = AiRequest::new("model", Vec::new());
        request.embedding = Some(stravia_runtime_contract::protocol::ir::EmbeddingRequest {
            input: stravia_runtime_contract::protocol::ir::EmbeddingInput::Text("input".into()),
            dimensions: None,
            encoding_format: None,
            user: None,
        });
        assert_eq!(
            select_protocol(&embeddings, &request),
            OPENAI_COMPATIBLE_EMBEDDINGS_V1.to_string()
        );
        assert_eq!(
            protocol_for(
                "custom",
                &provider(&OPENAI_COMPATIBLE_EMBEDDINGS_V1.to_string(), "")
            )
            .unwrap(),
            OPENAI_COMPATIBLE_EMBEDDINGS_V1.to_string()
        );
        assert_eq!(
            protocol_for("custom", &provider("google-gemini", "")).unwrap(),
            GEMINI_PROTOCOL
        );
        assert!(protocol_for("custom", &provider(COHERE_PROTOCOL, "")).is_err());
    }

    #[test]
    fn google_alias_resolves_before_query_auth_is_selected() {
        let provider = provider("google-gemini", "AIza+/=?");
        let protocol = protocol_for("google", &provider).unwrap();
        assert_eq!(protocol, GEMINI_PROTOCOL);
        assert_eq!(
            inference_url("google", &provider, &protocol, "/v1beta/models").unwrap(),
            "https://upstream.test/v1/models?key=AIza%2B%2F%3D%3F"
        );
    }

    #[test]
    fn cloudflare_config_validation_derives_an_encoded_base_without_replacing_overrides() {
        let mut provider = provider(OPENAI_CHAT_PROTOCOL, "unused");
        provider.base_url = "  ".into();
        provider.options.extend([
            ("accountId".into(), json!("account/one")),
            ("gatewayId".into(), json!("gateway two")),
        ]);
        let validation = validate_cloudflare_config(&provider, &BTreeMap::new());
        assert!(validation.issues.is_empty());
        assert_eq!(
            validation.proposed_base_url.as_deref(),
            Some("https://gateway.ai.cloudflare.com/v1/account%2Fone/gateway%20two/compat")
        );

        provider.base_url = "http://127.0.0.1:8080/cloudflare".into();
        let validation = validate_cloudflare_config(&provider, &BTreeMap::new());
        assert!(validation.issues.is_empty());
        assert_eq!(validation.proposed_base_url, None);

        provider.base_url.clear();
        provider.options.remove("gatewayId");
        let validation = validate_cloudflare_config(&provider, &BTreeMap::new());
        assert_eq!(validation.proposed_base_url, None);
        let issue = validation
            .issues
            .iter()
            .find(|issue| issue.field.as_deref() == Some("gatewayId") && issue.code == "required")
            .expect("missing gateway has a stable field and code");
        let message = serde_json::to_value(&issue.message).unwrap();
        for locale in ["en-US", "zh-CN"] {
            assert!(
                message
                    .get(locale)
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.is_empty())
            );
        }
    }

    #[test]
    fn cloudflare_inference_appends_the_chat_completions_codec_path() {
        let mut provider = provider(OPENAI_CHAT_PROTOCOL, "unused");
        provider.base_url.clear();
        provider.options.extend([
            ("accountId".into(), json!("account_1")),
            ("gatewayId".into(), json!("gateway_1")),
        ]);
        assert_eq!(
            inference_url(
                "cloudflare-ai-gateway",
                &provider,
                OPENAI_CHAT_PROTOCOL,
                "/v1/chat/completions",
            )
            .unwrap(),
            "https://gateway.ai.cloudflare.com/v1/account_1/gateway_1/compat/chat/completions"
        );

        for explicit in [
            "http://127.0.0.1:8080/cloudflare",
            "https://proxy.example.test/cloudflare/",
        ] {
            provider.base_url = explicit.into();
            assert_eq!(
                inference_url(
                    "cloudflare-ai-gateway",
                    &provider,
                    OPENAI_CHAT_PROTOCOL,
                    "/v1/chat/completions",
                )
                .unwrap(),
                format!("{}/chat/completions", explicit.trim_end_matches('/'))
            );
        }
    }

    fn strict_gemini_consumer(body: &Value) -> Result<(), &'static str> {
        let contents = body["contents"].as_array().unwrap();
        let boundary = contents
            .iter()
            .rposition(|content| {
                content["role"] == "user"
                    && content["parts"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|part| part.get("functionResponse").is_none())
            })
            .unwrap_or(0);
        for content in &contents[boundary..] {
            if content["role"] != "model" {
                continue;
            }
            if let Some(call) = content["parts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|part| part.get("functionCall").is_some())
                && call["thoughtSignature"].as_str().is_none_or(str::is_empty)
            {
                return Err("missing current-turn function-call signature");
            }
        }
        Ok(())
    }

    #[test]
    fn imported_gemini_tool_steps_continue_with_exact_parallel_associations() {
        let mut body = json!({"contents": [
            {"role":"user","parts":[{"text":"old request"}]},
            {"role":"model","parts":[{"functionCall":{"id":"old","name":"old","args":{}}}]},
            {"role":"user","parts":[{"text":"current request"}]},
            {"role":"model","parts":[{"text":"planning"},{"functionCall":{"id":"a","name":"weather","args":{"city":"Paris"}}},{"functionCall":{"id":"b","name":"weather","args":{"city":"London"}}}]},
            {"role":"user","parts":[{"functionResponse":{"id":"a","name":"weather","response":{"temp":15}}},{"functionResponse":{"id":"b","name":"weather","response":{"temp":12}}}]},
            {"role":"model","parts":[{"functionCall":{"id":"c","name":"book","args":{}},"thoughtSignature":null}]},
            {"role":"user","parts":[{"functionResponse":{"id":"c","name":"book","response":{"ok":true}}}]}
        ]});
        assert!(strict_gemini_consumer(&body).is_err());
        let mut expected = body.clone();
        expected["contents"][3]["parts"][1]["thoughtSignature"] =
            json!("skip_thought_signature_validator");
        expected["contents"][5]["parts"][0]["thoughtSignature"] =
            json!("skip_thought_signature_validator");
        prepare_gemini_imported_tool_history(
            GEMINI_PROTOCOL,
            "gemini-3-flash-preview",
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-3-flash-preview:generateContent?key=test",
            &mut body,
        );
        assert_eq!(strict_gemini_consumer(&body), Ok(()));
        assert_eq!(body, expected);
    }

    #[test]
    fn gemini_import_control_handles_empty_signature_and_latest_standard_user_content() {
        let mut body = json!({"contents":[
            {"role":"model","parts":[{"functionCall":{"name":"past","args":{}}}]},
            {"role":"user","parts":[{"functionResponse":{"name":"past","response":{}}},{"text":"new turn"}]},
            {"role":"model","parts":[{"functionCall":{"name":"current","args":{}},"thoughtSignature":""}]}
        ]});
        let mut expected = body.clone();
        expected["contents"][2]["parts"][0]["thoughtSignature"] =
            json!("skip_thought_signature_validator");
        prepare_gemini_imported_tool_history(
            GEMINI_PROTOCOL,
            "models/gemini-3-pro-preview",
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-3-pro-preview:generateContent",
            &mut body,
        );
        assert_eq!(strict_gemini_consumer(&body), Ok(()));
        assert_eq!(body, expected);
    }

    #[test]
    fn gemini_import_control_preserves_native_and_unsigned_non_required_history() {
        let original = json!({"contents":[
            {"role":"user","parts":[{"text":"request"}]},
            {"role":"model","parts":[{"functionCall":{"name":"tool","args":{}},"thoughtSignature":"opaque+/=="},{"functionCall":{"name":"parallel","args":{}}}]},
            {"role":"user","parts":[{"functionResponse":{"name":"tool","response":{}}}]},
            {"role":"model","parts":[{"text":"done"}]}
        ]});
        let mut body = original.clone();
        prepare_gemini_imported_tool_history(
            GEMINI_PROTOCOL,
            "gemini-3-pro",
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-3-pro:streamGenerateContent",
            &mut body,
        );
        assert_eq!(strict_gemini_consumer(&body), Ok(()));
        assert_eq!(body, original);
        for (protocol, model, url) in [
            (
                GEMINI_PROTOCOL,
                "gemini-3-pro",
                "https://aiplatform.googleapis.com/v1/projects/p/locations/l/publishers/google/models/gemini-3-pro:generateContent",
            ),
            (
                GEMINI_PROTOCOL,
                "gemini-3-pro",
                "http://127.0.0.1:8080/v1beta/models/gemini-3-pro:generateContent",
            ),
            (
                GEMINI_PROTOCOL,
                "gemini-2.5-pro",
                "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:generateContent",
            ),
            (
                GEMINI_PROTOCOL,
                "gemini-3-pro",
                "https://generativelanguage.googleapis.com/v1/models/gemini-3-pro:generateContent",
            ),
            (
                GEMINI_PROTOCOL,
                "gemini-3-pro",
                "https://generativelanguage.googleapis.com:8443/v1beta/models/gemini-3-pro:generateContent",
            ),
            (
                GEMINI_PROTOCOL,
                "gemini-3-pro",
                "https://generativelanguage.googleapis.com/v1beta/models/gemini-3-pro:countTokens",
            ),
            (
                GEMINI_PROTOCOL,
                "gemini-3-pro",
                "http://generativelanguage.googleapis.com/v1beta/models/gemini-3-pro:generateContent",
            ),
            (
                OPENAI_CHAT_PROTOCOL,
                "gemini-3-pro",
                "https://generativelanguage.googleapis.com/v1beta/models/gemini-3-pro:generateContent",
            ),
        ] {
            let mut unsigned = original.clone();
            unsigned["contents"][1]["parts"][0]
                .as_object_mut()
                .unwrap()
                .remove("thoughtSignature");
            let unchanged = unsigned.clone();
            prepare_gemini_imported_tool_history(protocol, model, url, &mut unsigned);
            assert_eq!(unsigned, unchanged);
            assert!(strict_gemini_consumer(&unsigned).is_err());
        }
    }

    #[test]
    fn gemini_import_control_supports_major_three_minor_versions_only() {
        let original = json!({"contents":[
            {"role":"user","parts":[{"text":"weather"}]},
            {"role":"model","parts":[{"functionCall":{"id":"a","name":"weather","args":{"city":"Paris"}}},{"functionCall":{"id":"b","name":"weather","args":{"city":"London"}}}]},
            {"role":"user","parts":[{"functionResponse":{"id":"a","name":"weather","response":{"temp":15}}},{"functionResponse":{"id":"b","name":"weather","response":{"temp":12}}}]}
        ]});
        for model in ["gemini-3.8-flash", "gemini-3.1-pro-preview"] {
            let mut body = original.clone();
            let url = format!(
                "https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent"
            );
            prepare_gemini_imported_tool_history(GEMINI_PROTOCOL, model, &url, &mut body);
            assert_eq!(strict_gemini_consumer(&body), Ok(()), "{model}");
            let mut expected = original.clone();
            expected["contents"][1]["parts"][0]["thoughtSignature"] =
                json!("skip_thought_signature_validator");
            assert_eq!(body, expected);
        }
        for model in [
            "gemini-30-flash",
            "gemini-2.5-pro",
            "gemini-4.1-pro",
            "gemini-flash-latest",
            "gemini-3.x-pro",
            "gemini-3.-pro",
        ] {
            let mut body = original.clone();
            let url = format!(
                "https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent"
            );
            prepare_gemini_imported_tool_history(GEMINI_PROTOCOL, model, &url, &mut body);
            assert_eq!(body, original, "{model}");
            assert!(strict_gemini_consumer(&body).is_err());
        }
    }
    fn assert_protected_reasoning(error: Option<PluginError>) {
        assert!(matches!(
            error.expect("expected ProtectedReasoningRejected").kind,
            ErrorKind::ProtectedReasoningRejected
        ));
    }

    #[test]
    fn classifies_anthropic_thinking_signature_rejection() {
        // Anthropic 官方文案（含新版 bound-to-conversation 变体的前缀）。
        assert_protected_reasoning(classify_anthropic_error(
            &json!({
                "type": "error",
                "error": {
                    "type": "invalid_request_error",
                    "message": "messages.0.content.0: Invalid `signature` in `thinking` block"
                }
            }),
            false,
        ));
        assert_protected_reasoning(classify_anthropic_error(
            &json!({
                "type": "error",
                "error": {
                    "type": "invalid_request_error",
                    "message": "messages.0.content.0: Invalid `signature` in `thinking` block. The block is bound to a different conversation."
                }
            }),
            false,
        ));
        assert_protected_reasoning(classify_anthropic_error(
            &json!({
                "type": "error",
                "error": {
                    "type": "invalid_request_error",
                    "message": "messages.0.content.1: Invalid `data` in `redacted_thinking` block"
                }
            }),
            false,
        ));
    }

    #[test]
    fn ignores_anthropic_errors_unrelated_to_reasoning() {
        for message in [
            "messages: roles must alternate between \"user\" and \"assistant\"",
            "model: invalid model ID",
        ] {
            assert!(
                classify_anthropic_error(
                    &json!({
                        "type": "error",
                        "error": {"type": "invalid_request_error", "message": message}
                    }),
                    false,
                )
                .is_none()
            );
        }
        // 非 invalid_request_error 类型不参与剥离重试。
        assert!(
            classify_anthropic_error(
                &json!({
                    "type": "error",
                    "error": {"type": "rate_limit_error", "message": "Invalid `signature` in `thinking` block"}
                }),
                false,
            )
            .is_none()
        );
        // 已有输出之后的错误不能触发剥离重试。
        assert!(
            classify_anthropic_error(
                &json!({
                    "type": "error",
                    "error": {"type": "invalid_request_error", "message": "Invalid `signature` in `thinking` block"}
                }),
                true,
            )
            .is_none()
        );
    }

    #[test]
    fn classifies_bedrock_validation_exception_signature_rejection() {
        // HTTP 错误体：异常类型在 __type，文案由 Claude 透传。
        assert_protected_reasoning(classify_bedrock_error(
            &json!({
                "__type": "com.amazonaws.bedrock.runtime.ValidationException",
                "message": "Invalid `signature` in `thinking` block"
            }),
            false,
        ));
        // 异常类型也可只在响应头里，body 仅带 message。
        assert_protected_reasoning(classify_bedrock_error(
            &json!({"message": "Invalid `signature` in `thinking` block"}),
            false,
        ));
        // ConverseStream 流内异常事件外形。
        assert_protected_reasoning(classify_bedrock_error(
            &json!({
                "validationException": {
                    "message": "Invalid `signature` in `thinking` block"
                }
            }),
            false,
        ));
    }

    #[test]
    fn ignores_bedrock_errors_unrelated_to_reasoning() {
        assert!(
            classify_bedrock_error(
                &json!({
                    "__type": "ValidationException",
                    "message": "The model is missing required parameters"
                }),
                false,
            )
            .is_none()
        );
        // 异常类型不是 ValidationException 时不归类。
        assert!(
            classify_bedrock_error(
                &json!({
                    "__type": "AccessDeniedException",
                    "message": "Invalid `signature` in `thinking` block"
                }),
                false,
            )
            .is_none()
        );
        assert!(
            classify_bedrock_error(
                &json!({"message": "Invalid `signature` in `thinking` block"}),
                true,
            )
            .is_none()
        );
    }

    #[test]
    fn classifies_gemini_thought_signature_rejection() {
        for message in [
            "Invalid thought signature",
            "Corrupted thought signature",
            "Thought signature is not valid",
        ] {
            assert_protected_reasoning(classify_gemini_error(
                &json!({
                    "error": {"code": 400, "status": "INVALID_ARGUMENT", "message": message}
                }),
                false,
            ));
        }
    }

    #[test]
    fn ignores_gemini_errors_unrelated_to_thought_signatures() {
        // 普通 INVALID_ARGUMENT 不误判。
        assert!(
            classify_gemini_error(
                &json!({
                    "error": {"code": 400, "status": "INVALID_ARGUMENT", "message": "Request is missing required field"}
                }),
                false,
            )
            .is_none()
        );
        // 非 400/INVALID_ARGUMENT 不归类。
        assert!(
            classify_gemini_error(
                &json!({
                    "error": {"code": 500, "status": "INTERNAL", "message": "Invalid thought signature"}
                }),
                false,
            )
            .is_none()
        );
        assert!(
            classify_gemini_error(
                &json!({
                    "error": {"code": 400, "status": "INVALID_ARGUMENT", "message": "Corrupted thought signature"}
                }),
                true,
            )
            .is_none()
        );
    }
}
