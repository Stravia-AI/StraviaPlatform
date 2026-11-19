use serde_json::{Value, json};
use stravia_protocol_codec::accumulator::StreamResponseAccumulator;
use stravia_protocol_codec::transform::{ProtocolTransform, StreamDecodeStage};
use stravia_runtime_contract::protocol::ids::GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA;
use stravia_runtime_contract::protocol::ir::{AiErrorKind, AiRequest, AiResponse, ToolChoice};
use stravia_vendor_common::{common, gemini};
use stravia_vendor_sdk::{ErrorKind, GuestHost, PluginError, ProviderSnapshot, read_http_body};

use crate::client;

const MAX_BODY: usize = 32 * 1024 * 1024;
const MAX_EVENT: usize = 16 * 1024 * 1024;

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct InferenceEnvelope<'a> {
    project: &'a str,
    model: &'a str,
    request: InferenceBody<'a>,
    user_agent: &'static str,
    request_type: &'static str,
    #[serde(serialize_with = "serialize_agent_request_id")]
    request_id: uuid::Uuid,
}

// `body` 已经投影并排除了下游 labels；仅添加本次请求可以证实的元数据。
#[derive(serde::Serialize)]
struct InferenceBody<'a> {
    #[serde(flatten)]
    body: &'a Value,
    labels: RequestLabels<'a>,
}

#[derive(serde::Serialize)]
struct RequestLabels<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_enum: Option<&'a str>,
    cascade_id: String,
    root_cascade_id: String,
    trajectory_id: uuid::Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_step_index: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_execution_id: Option<String>,
    used_claude: String,
    used_claude_conservative: String,
    used_non_gemini_model: String,
}

impl<'a> RequestLabels<'a> {
    fn new(labels: crate::lifecycle::Labels, model_enum: Option<&'a str>) -> Self {
        Self {
            request_id: labels.request_id,
            model_enum,
            root_cascade_id: labels.cascade_id.clone(),
            cascade_id: labels.cascade_id,
            trajectory_id: labels.trajectory_id,
            last_step_index: labels.last_step_index.map(|n| n.to_string()),
            last_execution_id: labels.last_execution_id,
            used_claude: labels.used_claude.to_string(),
            used_claude_conservative: labels.used_claude_conservative.to_string(),
            used_non_gemini_model: labels.used_non_gemini_model.to_string(),
        }
    }
}

fn serialize_agent_request_id<S: serde::Serializer>(
    id: &uuid::Uuid,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    // CLI 1.3.5 computeRequestID's fallback when native operation metadata
    // is absent is request-type/UUID, not the community proxy's agent-UUID.
    serializer.collect_str(&format_args!("agent/{id}"))
}

// CLI 1.3.5 codeassistclient.getSessionID hashes the complete source bytes
// with FNV-1a64 and formats the result as a signed decimal int64.
fn native_session_id(source: &str) -> String {
    let hash = source
        .as_bytes()
        .iter()
        .fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        });
    (hash as i64).to_string()
}

pub(crate) fn infer(
    host: &GuestHost,
    mut provider: ProviderSnapshot,
    mut request: AiRequest,
) -> Result<AiResponse, PluginError> {
    request.model = provider
        .model
        .take()
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| invalid("Antigravity requires an upstream model"))?;
    // Model families are canonical here; native selectors may be opaque aliases.
    let imported_tool_history = client::root(&provider) == crate::BASE_URL
        && gemini::supports_imported_tool_history(&request.model);
    let has_explicit_thinking_control = request.reasoning.target_control.is_none()
        && (request.reasoning.mode.is_some()
            || request.reasoning.level.is_some()
            || request.reasoning.budget_tokens.is_some());
    let (selector_thinking, selector_max_output_tokens, model_enum, api_provider, model_provider) =
        if let Some(table) = provider
            .model_metadata
            .as_ref()
            .and_then(|metadata| metadata.extensions.get(crate::selector::EXTENSION_KEY))
        {
            let selection =
                crate::selector::resolve(table, request.reasoning.target_control.as_ref())?;
            request.model = selection.id.into();
            let effort = matches!(
                request.reasoning.target_control,
                Some(stravia_runtime_contract::thinking::TargetThinkingControl::Effort { .. })
            );
            if effort {
                // 真实 ID 与强度都来自所选档位；不能让通用 codec 再按 Effort 生成另一种控制。
                request.reasoning.target_control = None;
                request.reasoning.level = None;
                request.reasoning.budget_tokens = None;
            }
            (
                selection.thinking,
                selection.max_output_tokens,
                selection.model_enum,
                selection.api_provider,
                selection.model_provider,
            )
        } else {
            if let Some(selector) = provider
                .model_metadata
                .as_ref()
                .and_then(|metadata| metadata.selector.as_deref())
            {
                request.model = selector.into();
            }
            (None, None, None, None, None)
        };
    // 在跨协议校验前删除 CLI 没有承诺的控制；最后的递归投影仍是唯一出口。
    request.generation.seed = None;
    request.generation.presence_penalty = None;
    request.generation.frequency_penalty = None;
    request.parallel_tool_calls = None;

    // The public Gemini codec collapses these levels to HIGH, but CLI master
    // distinguishes them. Keep its budget/mode precedence and selector behavior.
    let explicit_extended_level = match request.reasoning.target_control.as_ref() {
        Some(stravia_runtime_contract::thinking::TargetThinkingControl::Effort { value }) => {
            match stravia_runtime_contract::thinking::ThinkingLevel::from_wire(value).ok() {
                Some(stravia_runtime_contract::thinking::ThinkingLevel::Xhigh) => {
                    Some("EXTRA_HIGH")
                }
                Some(stravia_runtime_contract::thinking::ThinkingLevel::Max) => Some("MAX"),
                _ => None,
            }
        }
        None if request.reasoning.budget_tokens.is_none()
            && !matches!(
                request.reasoning.mode.as_deref(),
                Some("auto" | "disabled" | "enabled")
            ) =>
        {
            match request.reasoning.level {
                Some(stravia_runtime_contract::thinking::ThinkingLevel::Xhigh) => {
                    Some("EXTRA_HIGH")
                }
                Some(stravia_runtime_contract::thinking::ThinkingLevel::Max) => Some("MAX"),
                _ => None,
            }
        }
        _ => None,
    };
    // CLI 使用 private master Schema，不能让公开 Gemini codec 的能力校验
    // 拒绝 strict 或移除已核对的 schema 约束；这些边界由下方显式构造。
    let tools = request.tools.take();
    let tool_choice = request.tool_choice.take();
    let response_format = request.response_format.take();
    let stop = request.generation.stop.take();
    request.stream.enabled = true;
    let protocol = GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA;
    let model = request.model.clone();
    let request_id = uuid::Uuid::new_v4();
    let lifecycle_input =
        crate::lifecycle::Input::new(&provider, &request, api_provider, model_provider)?;
    let mut body = common::encode_inference_request(&protocol.to_string(), request)?.body;
    let object = body
        .as_object_mut()
        .ok_or_else(|| invalid("Gemini request must be an object"))?;
    object
        .entry("generationConfig")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| invalid("generationConfig must be an object"))?
        .entry("maxOutputTokens")
        .or_insert_with(|| json!(selector_max_output_tokens.unwrap_or(65536)));
    if let Some(level) = explicit_extended_level
        && let Some(thinking) = object
            .get_mut("generationConfig")
            .and_then(|config| config.get_mut("thinkingConfig"))
            .and_then(Value::as_object_mut)
        && thinking.contains_key("thinkingLevel")
    {
        thinking.insert("thinkingLevel".into(), json!(level));
    }
    if let Some(control) = selector_thinking.filter(|_| !has_explicit_thinking_control) {
        let thinking = object
            .entry("generationConfig")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| invalid("generationConfig must be an object"))?
            .entry("thinkingConfig")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| invalid("thinkingConfig must be an object"))?;
        let (field, other, value) = match control {
            crate::selector::ThinkingControl::Level(level) => {
                ("thinkingLevel", "thinkingBudget", level)
            }
            crate::selector::ThinkingControl::Budget(budget) => {
                ("thinkingBudget", "thinkingLevel", budget)
            }
        };
        thinking.remove(other);
        thinking.insert(field.into(), json!(value));
    }
    // 同协议优先保留 raw 声明的 response schema 和原生工具；不重复添加 canonical 声明。
    if let Some(groups) = object.get_mut("tools").and_then(Value::as_array_mut)
        && groups.iter().any(|group| {
            group
                .get("functionDeclarations")
                .and_then(Value::as_array)
                .is_some_and(|declarations| !declarations.is_empty())
        })
    {
        split_raw_function_groups(groups);
    } else if let Some(tools) = tools.filter(|tools| !tools.is_empty()) {
        let groups = object
            .entry("tools")
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| invalid("tools must be an array"))?;
        groups.reserve(tools.len());
        for tool in tools {
            let mut declaration = serde_json::Map::new();
            declaration.insert("name".into(), Value::String(tool.name));
            declaration.insert("parameters".into(), tool.parameters);
            if let Some(description) = tool.description {
                declaration.insert("description".into(), Value::String(description));
            }
            groups.push(function_group(Value::Object(declaration)));
        }
    }
    if !object.contains_key("toolConfig") {
        let config = match tool_choice {
            Some(ToolChoice::Auto) => Some(("AUTO", None)),
            Some(ToolChoice::None) => Some(("NONE", None)),
            Some(ToolChoice::Required) => Some(("ANY", None)),
            Some(ToolChoice::Named { name }) => Some(("ANY", Some(vec![name]))),
            Some(ToolChoice::Allowed { names, required }) => {
                Some((if required { "ANY" } else { "AUTO" }, Some(names)))
            }
            Some(ToolChoice::Native(_)) | None => None,
        };
        if let Some((mode, name)) = config {
            let mut calling = serde_json::Map::new();
            calling.insert("mode".into(), Value::String(mode.into()));
            if let Some(names) = name {
                calling.insert(
                    "allowedFunctionNames".into(),
                    Value::Array(names.into_iter().map(Value::String).collect()),
                );
            }
            let mut config = serde_json::Map::new();
            config.insert("functionCallingConfig".into(), Value::Object(calling));
            object.insert("toolConfig".into(), Value::Object(config));
        }
    }
    if stop.is_some() || response_format.is_some() {
        let config = object
            .entry("generationConfig")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| invalid("generationConfig must be an object"))?;
        if let Some(stop) = stop {
            config.insert(
                "stopSequences".into(),
                Value::Array(stop.into_iter().map(Value::String).collect()),
            );
        }
        match response_format {
            Some(stravia_runtime_contract::protocol::ir::ResponseFormat::Text) => {
                config.insert("responseMimeType".into(), json!("text/plain"));
                config.remove("responseSchema");
            }
            Some(stravia_runtime_contract::protocol::ir::ResponseFormat::JsonObject) => {
                config.insert("responseMimeType".into(), json!("application/json"));
                config.remove("responseSchema");
            }
            Some(stravia_runtime_contract::protocol::ir::ResponseFormat::JsonSchema {
                schema,
                ..
            }) => {
                config.insert("responseMimeType".into(), json!("application/json"));
                config.insert("responseSchema".into(), schema);
            }
            None => {}
        }
    }
    sanitize_request(&mut body)?;
    normalize_system_instruction(&mut body);
    if imported_tool_history {
        gemini::prepare_imported_tool_history(&mut body);
    }
    // 会话、项目和请求身份由宿主/本连接生成，不能由下游额外字段指定。
    if let Some(session) = common::session_affinity(&provider) {
        body.as_object_mut()
            .expect("request was validated")
            .insert("sessionId".into(), json!(native_session_id(session)));
    }
    let project = client::project(host, &provider)?;
    let mut headers = stravia_vendor_sdk::net::Fields::new();
    for field in client::headers(&provider)?.into_vec() {
        if field.name.eq_ignore_ascii_case("content-length") {
            headers.append("Transfer-Encoding", b"chunked".as_slice());
        } else {
            headers.append(field.name, field.bytes);
        }
    }
    host.emit_started()?;
    let (reservation, lifecycle_labels) = crate::lifecycle::Reservation::begin(lifecycle_input)?;
    let envelope = InferenceEnvelope {
        project: &project,
        model: &model,
        request: InferenceBody {
            body: &body,
            labels: RequestLabels::new(lifecycle_labels, model_enum),
        },
        user_agent: "antigravity",
        request_type: "agent",
        request_id,
    };
    let result = (|| {
        let bytes = serde_json::to_vec(&envelope)
            .map_err(|_| invalid("Antigravity request could not be encoded"))?;
        let response = client::http_start(
            host,
            stravia_vendor_sdk::net::Request {
                method: "POST".into(),
                url: format!(
                    "{}?alt=sse",
                    client::endpoint(&provider, "streamGenerateContent")
                ),
                headers,
                body: bytes,
            },
        )?;
        read_inference_response(host, response)
    })();
    reservation.finish(
        result
            .as_ref()
            .ok()
            .map(|(response, _)| response)
            .filter(|response| !response.is_error()),
    )?;
    let (response, terminal_deltas) = result.map_err(|mut error| {
        if error.upstream_status == Some(429) {
            use std::fmt::Write;
            if let Some(id) = &envelope.request.labels.request_id {
                write!(
                    error.message,
                    " (Antigravity request_id: {id}, attempt_id: {request_id})"
                )
                .expect("writing to a String cannot fail");
            } else {
                write!(error.message, " (Antigravity attempt_id: {request_id})")
                    .expect("writing to a String cannot fail");
            }
        }
        error
    })?;
    for delta in &terminal_deltas {
        host.emit_delta(delta)?;
    }
    host.emit_completed(&response)?;
    Ok(response)
}

fn read_inference_response(
    host: &GuestHost,
    response: stravia_vendor_sdk::HttpResponse,
) -> Result<
    (
        AiResponse,
        Vec<stravia_runtime_contract::protocol::ir::AiStreamDelta>,
    ),
    PluginError,
> {
    let protocol = GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA;
    let status = response.status()?;
    let headers = response.headers()?;
    if !(200..300).contains(&status) {
        let bytes = read_http_body(&response, MAX_EVENT)?;
        return Err(common::upstream_error(status, &headers, &bytes));
    }
    let streaming = headers.iter().any(|field| {
        field.name.eq_ignore_ascii_case("content-type")
            && String::from_utf8_lossy(&field.bytes)
                .to_ascii_lowercase()
                .contains("text/event-stream")
    });
    if !streaming {
        let bytes = read_http_body(&response, MAX_BODY)?;
        let mut value: Value = serde_json::from_slice(&bytes)
            .map_err(|_| invalid_upstream("Antigravity returned invalid JSON"))?;
        if let Some(error) = response_error(&value, &headers) {
            return Err(error);
        }
        let mut inner = value.get_mut("response").map(Value::take).unwrap_or(value);
        normalize_final_usage(&mut inner);
        return ProtocolTransform::global()
            .bind(protocol, protocol)
            .and_then(|pair| pair.decode_response(inner))
            .map(|response| (response, Vec::new()))
            .map_err(common::map_response_transform_error);
    }
    let mut decoder = ProtocolTransform::global()
        .decode_stream(protocol)
        .map_err(common::map_response_transform_error)?;
    let mut accumulator = StreamResponseAccumulator::default();
    let mut framing = EventFraming::default();
    let mut terminal = false;
    let mut terminal_deltas = Vec::new();
    while let Some(chunk) = response.read_body()? {
        framing.push(&chunk, |payload| {
            decode_event(
                host,
                &mut decoder,
                &mut accumulator,
                &mut terminal,
                &mut terminal_deltas,
                payload,
                &headers,
            )
        })?;
    }
    framing.finish(|payload| {
        decode_event(
            host,
            &mut decoder,
            &mut accumulator,
            &mut terminal,
            &mut terminal_deltas,
            payload,
            &headers,
        )
    })?;
    let deltas = decoder
        .finish()
        .map_err(common::map_response_transform_error)?;
    emit_nonterminal_deltas(host, &mut accumulator, &deltas, &mut terminal_deltas)?;
    if !terminal {
        return Err(common::model_error(
            AiErrorKind::UnexpectedEof,
            "Antigravity stream ended before a finish reason",
        ));
    }
    Ok((accumulator.into_ai_response(), terminal_deltas))
}

fn decode_event(
    host: &GuestHost,
    decoder: &mut StreamDecodeStage,
    accumulator: &mut StreamResponseAccumulator,
    terminal: &mut bool,
    terminal_deltas: &mut Vec<stravia_runtime_contract::protocol::ir::AiStreamDelta>,
    payload: &[u8],
    headers: &stravia_vendor_sdk::net::Fields,
) -> Result<(), PluginError> {
    if payload == b"[DONE]" {
        return Ok(());
    }
    let mut value: Value = serde_json::from_slice(payload)
        .map_err(|_| invalid_upstream("Antigravity stream contained invalid JSON"))?;
    if let Some(error) = response_error(&value, headers) {
        return Err(error);
    }
    let mut inner = value.get_mut("response").map(Value::take).unwrap_or(value);
    *terminal |= inner
        .get("candidates")
        .and_then(Value::as_array)
        .is_some_and(|candidates| {
            candidates.iter().any(|candidate| {
                candidate
                    .get("finishReason")
                    .and_then(Value::as_str)
                    .is_some_and(|reason| !reason.is_empty())
            })
        });
    if *terminal {
        normalize_final_usage(&mut inner);
    }
    let mut event = Vec::with_capacity(payload.len() + 8);
    event.extend_from_slice(b"data: ");
    serde_json::to_writer(&mut event, &inner)
        .map_err(|_| invalid_upstream("Antigravity stream response could not be decoded"))?;
    event.extend_from_slice(b"\n\n");
    let deltas = decoder
        .decode_chunk(&event)
        .map_err(common::map_response_transform_error)?;
    emit_nonterminal_deltas(host, accumulator, &deltas, terminal_deltas)
}

fn emit_nonterminal_deltas(
    host: &GuestHost,
    accumulator: &mut StreamResponseAccumulator,
    deltas: &[stravia_runtime_contract::protocol::ir::AiStreamDelta],
    terminal: &mut Vec<stravia_runtime_contract::protocol::ir::AiStreamDelta>,
) -> Result<(), PluginError> {
    use stravia_runtime_contract::protocol::ir::AiStreamDelta;
    for delta in deltas {
        if matches!(
            delta,
            AiStreamDelta::Done { .. } | AiStreamDelta::ResponseTerminal { .. }
        ) {
            accumulator.apply(delta);
            terminal.push(delta.clone());
        } else {
            common::emit_deltas(host, accumulator, std::slice::from_ref(delta))?;
        }
    }
    Ok(())
}

pub(crate) fn normalize_final_usage(response: &mut Value) {
    let Some(usage) = response
        .get_mut("usageMetadata")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    let Some(prompt) = usage.get("promptTokenCount").and_then(Value::as_u64) else {
        return;
    };
    let output_known = usage
        .get("candidatesTokenCount")
        .and_then(Value::as_u64)
        .is_some()
        || usage
            .get("totalTokenCount")
            .and_then(Value::as_u64)
            .is_some_and(|total| total >= prompt);
    if !output_known {
        return;
    }
    // Official CLI 1.2.16's private StreamGenerateContent response wraps
    // aiplatform.master.GenerateContentResponse. Its proto3 UsageMetadata
    // cached_content_token_count (int32, field 5) has implicit presence:
    // omission in a complete usage snapshot means zero, not unknown.
    // Do not apply that default to partial frames or absent/invalid usage.
    usage
        .entry("cachedContentTokenCount")
        .or_insert_with(|| json!(0));
}

pub(crate) fn response_error(
    value: &Value,
    headers: &stravia_vendor_sdk::net::Fields,
) -> Option<PluginError> {
    let error = value
        .get("error")
        .or_else(|| value.pointer("/response/error"))?;
    let status = error
        .get("code")
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
        .filter(|value| (400..600).contains(value))
        .unwrap_or(502);
    Some(common::upstream_error(
        status,
        headers,
        &serde_json::to_vec(&json!({"error": error})).expect("JSON value encodes"),
    ))
}

pub(crate) fn invalid(message: &str) -> PluginError {
    common::plugin_error(ErrorKind::Invalid, message)
}
fn invalid_upstream(message: &str) -> PluginError {
    common::model_error(AiErrorKind::ServerError, message)
}

fn normalize_system_instruction(request: &mut Value) {
    // 同连接单变量对照定位到四句 OMP 指令；等义改写仅作用于出站系统文本。
    // 不插入已实测无效的零宽字符，也不改工具标识、代码、普通消息或权威历史。
    const REWRITES: &[(&str, &str)] = &[
        (
            "RFC 2119 keywords: MUST, REQUIRED, SHOULD, RECOMMENDED, MAY, OPTIONAL.",
            "",
        ),
        (
            "You are omp's trusted coding assistant.",
            "You are a trusted coding assistant for omp.",
        ),
        (
            "Write JSON args as `content` to xd://<tool> via `write`. Invalid args return schema in error → fix/retry.",
            "Send JSON tool arguments in the `content` field of `write`, addressed to `xd://<tool>`. Invalid arguments return the schema; correct the arguments before retrying.",
        ),
        (
            "Read xd://<tool> for full docs + JSON schema before first use.",
            "Before using a tool for the first time, read `xd://<tool>` to load its complete documentation and JSON schema.",
        ),
        (
            "To commit your changes, use the eval `commit` namespace (`d = await commit.draft()`, then `await d.commit(\"subject\", [\"- body\"])`), not `git add`/`git commit`: it selects only the lines you and your subagents wrote and never touches other agents' staged or unstaged work.",
            "Commit changes through the eval `commit` namespace: call `d = await commit.draft()`, then `await d.commit(\"subject\", [\"- body\"])`. Do not use `git add` or `git commit`. This commits only lines written by you or your subagents and leaves other agents' staged and unstaged edits untouched.",
        ),
    ];
    let Some(parts) = request
        .get_mut("systemInstruction")
        .and_then(|system| system.get_mut("parts"))
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    if parts.is_empty() {
        return;
    }
    parts.retain_mut(|part| {
        let Some(Value::String(text)) = part.get_mut("text") else {
            return true;
        };
        let mut removed = false;
        for &(fragment, replacement) in REWRITES {
            let mut offset = 0;
            while let Some(index) = text[offset..].find(fragment) {
                offset += index;
                text.replace_range(offset..offset + fragment.len(), replacement);
                offset += replacement.len();
                removed |= replacement.is_empty();
            }
        }
        if removed && text.trim().is_empty() {
            let object = part.as_object_mut().expect("system part was validated");
            object.remove("text");
            return !object.is_empty();
        }
        true
    });
    if parts.is_empty() {
        request
            .as_object_mut()
            .expect("request was validated")
            .remove("systemInstruction");
    }
}

// 字段来自 CLI 1.2.16 的 protobuf 序列化边界；不允许未知 raw 扩展穿过。
// args/response/default/example 是应用 JSON 数据，不是协议参数。
pub(crate) fn sanitize_request(value: &mut Value) -> Result<(), PluginError> {
    let object = value
        .as_object_mut()
        .ok_or_else(|| invalid("Gemini request must be an object"))?;
    object.retain(|key, _| {
        matches!(
            key.as_str(),
            "contents"
                | "systemInstruction"
                | "generationConfig"
                | "tools"
                | "toolConfig"
                | "safetySettings"
        )
    });
    for (key, value) in object {
        match key.as_str() {
            "contents" => array(value, |v| content(v, false))?,
            "systemInstruction" => content(value, true)?,
            "generationConfig" => generation(value)?,
            "tools" => array(value, tool)?,
            "toolConfig" => {
                project(value, &["functionCallingConfig"])?;
                if let Some(config) = value.get_mut("functionCallingConfig") {
                    project(config, &["mode", "allowedFunctionNames"])?;
                }
            }
            "safetySettings" => array(value, |v| project(v, &["category", "threshold", "method"]))?,
            _ => unreachable!(),
        }
    }
    Ok(())
}

fn project(value: &mut Value, fields: &[&str]) -> Result<(), PluginError> {
    let object = value
        .as_object_mut()
        .ok_or_else(|| invalid("CLI request field must be an object"))?;
    object.retain(|key, _| fields.contains(&key.as_str()));
    Ok(())
}
fn array(
    value: &mut Value,
    mut visit: impl FnMut(&mut Value) -> Result<(), PluginError>,
) -> Result<(), PluginError> {
    for value in value
        .as_array_mut()
        .ok_or_else(|| invalid("CLI request field must be an array"))?
    {
        visit(value)?;
    }
    Ok(())
}
fn content(value: &mut Value, system: bool) -> Result<(), PluginError> {
    project(
        value,
        if system {
            &["parts"]
        } else {
            &["role", "parts"]
        },
    )?;
    if let Some(parts) = value.get_mut("parts") {
        array(parts, part)?;
    }
    Ok(())
}
fn part(value: &mut Value) -> Result<(), PluginError> {
    project(
        value,
        &[
            "text",
            "inlineData",
            "fileData",
            "functionCall",
            "functionResponse",
            "thought",
            "thoughtSignature",
            "executableCode",
            "codeExecutionResult",
            "videoMetadata",
            "mediaResolution",
        ],
    )?;
    for (key, value) in value.as_object_mut().expect("validated part") {
        match key.as_str() {
            "inlineData" => project(value, &["mimeType", "data", "displayName"])?,
            "fileData" => project(value, &["mimeType", "fileUri", "displayName"])?,
            "functionCall" => project(value, &["id", "name", "args"])?,
            "functionResponse" => {
                project(value, &["id", "name", "response", "parts"])?;
                if let Some(parts) = value.get_mut("parts") {
                    array(parts, |p| {
                        project(p, &["inlineData", "fileData"])?;
                        if let Some(blob) = p.get_mut("inlineData") {
                            project(blob, &["mimeType", "data", "displayName"])?;
                        }
                        if let Some(file) = p.get_mut("fileData") {
                            project(file, &["mimeType", "fileUri", "displayName"])?;
                        }
                        Ok(())
                    })?;
                }
            }
            "executableCode" => project(value, &["language", "code"])?,
            "codeExecutionResult" => project(value, &["outcome", "output"])?,
            "videoMetadata" => project(value, &["startOffset", "endOffset", "fps"])?,
            "mediaResolution" => project(value, &["level", "numTokens"])?,
            _ => {}
        }
    }
    Ok(())
}
fn generation(value: &mut Value) -> Result<(), PluginError> {
    project(
        value,
        &[
            "temperature",
            "topP",
            "topK",
            "candidateCount",
            "maxOutputTokens",
            "stopSequences",
            "thinkingConfig",
            "responseMimeType",
            "responseSchema",
        ],
    )?;
    if let Some(thinking) = value.get_mut("thinkingConfig") {
        project(
            thinking,
            &["includeThoughts", "thinkingBudget", "thinkingLevel"],
        )?;
        // Protojson names known master values and preserves unknown int32 values.
        if let Some(level) = thinking.get_mut("thinkingLevel") {
            let canonical = match &*level {
                Value::String(value)
                    if matches!(
                        value.as_str(),
                        "THINKING_LEVEL_UNSPECIFIED"
                            | "LOW"
                            | "MEDIUM"
                            | "HIGH"
                            | "MINIMAL"
                            | "EXTRA_HIGH"
                            | "MAX"
                            | "XHIGH"
                    ) =>
                {
                    None
                }
                Value::Number(value) => {
                    match value.as_i64().and_then(|value| i32::try_from(value).ok()) {
                        Some(0) => Some("THINKING_LEVEL_UNSPECIFIED"),
                        Some(1) => Some("LOW"),
                        Some(2) => Some("MEDIUM"),
                        Some(3) => Some("HIGH"),
                        Some(4) => Some("MINIMAL"),
                        Some(5) => Some("EXTRA_HIGH"),
                        Some(6) => Some("MAX"),
                        Some(7) => Some("XHIGH"),
                        Some(_) => None,
                        None => {
                            return Err(invalid(
                                "Antigravity thinkingLevel must be an int32 or CLI ThinkingLevel enum",
                            ));
                        }
                    }
                }
                _ => {
                    return Err(invalid(
                        "Antigravity thinkingLevel must be an int32 or CLI ThinkingLevel enum",
                    ));
                }
            };
            if let Some(canonical) = canonical {
                *level = json!(canonical);
            }
        }
        if thinking.get("thinkingBudget").is_some_and(|value| {
            value
                .as_i64()
                .and_then(|value| i32::try_from(value).ok())
                .is_none_or(|value| value < -1)
        }) {
            return Err(invalid(
                "Antigravity thinkingBudget must be an int32 greater than or equal to -1",
            ));
        }
    }
    if let Some(schema) = value.get_mut("responseSchema") {
        schema_projection(schema)?;
    }
    Ok(())
}
fn function_group(declaration: Value) -> Value {
    Value::Object(serde_json::Map::from_iter([(
        "functionDeclarations".into(),
        Value::Array(vec![declaration]),
    )]))
}

fn split_raw_function_groups(groups: &mut Vec<Value>) {
    let extra: usize = groups
        .iter()
        .filter_map(|group| group.get("functionDeclarations")?.as_array())
        .map(|declarations| declarations.len().saturating_sub(1))
        .sum();
    if extra == 0 {
        return;
    }
    // 从尾端展开，保留声明顺序，不复制 schema；同组的原生工具字段只保留在首项。
    let original_len = groups.len();
    let mut end = original_len + extra;
    groups.resize_with(end, || Value::Null);
    for index in (0..original_len).rev() {
        let mut group = groups[index].take();
        if let Some(declarations) = group
            .get_mut("functionDeclarations")
            .and_then(Value::as_array_mut)
        {
            while declarations.len() > 1 {
                end -= 1;
                groups[end] = function_group(declarations.pop().expect("multiple declarations"));
            }
        }
        end -= 1;
        groups[end] = group;
    }
}

fn tool(value: &mut Value) -> Result<(), PluginError> {
    project(
        value,
        &[
            "functionDeclarations",
            "googleSearch",
            "googleSearchRetrieval",
            "codeExecution",
            "urlContext",
        ],
    )?;
    if let Some(functions) = value.get_mut("functionDeclarations") {
        array(functions, |function| {
            project(
                function,
                &[
                    "name",
                    "description",
                    "parameters",
                    "parametersJsonSchema",
                    "response",
                ],
            )?;
            // The Gemini decoder gives parametersJsonSchema precedence when both are set.
            let declaration = function
                .as_object_mut()
                .expect("validated function declaration");
            if let Some(schema) = declaration.remove("parametersJsonSchema") {
                declaration.insert("parameters".into(), schema);
            }
            if let Some(parameters) = function.get_mut("parameters") {
                tool_schema_projection(parameters)?;
            }
            if let Some(response) = function.get_mut("response") {
                schema_projection(response)?;
            }
            Ok(())
        })?;
    }
    if let Some(search) = value.get_mut("googleSearch") {
        project(
            search,
            &[
                "excludeDomains",
                "blockingConfidence",
                "includedDomains",
                "timeRangeFilter",
            ],
        )?;
        if let Some(range) = search.get_mut("timeRangeFilter") {
            project(range, &["startTime", "endTime"])?;
        }
    }
    if let Some(search) = value.get_mut("googleSearchRetrieval") {
        project(search, &["dynamicRetrievalConfig"])?;
        if let Some(config) = search.get_mut("dynamicRetrievalConfig") {
            project(config, &["mode", "dynamicThreshold"])?;
        }
    }
    if let Some(code) = value.get_mut("codeExecution") {
        project(code, &["languages"])?;
    }
    if let Some(url) = value.get_mut("urlContext") {
        project(url, &[])?;
    }
    Ok(())
}
// Tool parameters use the CLI's narrower Schema subset, independently of response schemas.
fn tool_schema_projection(value: &mut Value) -> Result<(), PluginError> {
    if value.is_boolean() {
        *value = json!({});
    }
    project(
        value,
        &[
            "type",
            "format",
            "description",
            "default",
            "enum",
            "properties",
            "required",
            "items",
            "minimum",
            "maximum",
            "minItems",
            "maxItems",
            "minLength",
            "maxLength",
            "pattern",
        ],
    )?;
    let object = value.as_object_mut().expect("validated schema");
    match object.get_mut("type") {
        Some(Value::String(kind)) => kind.make_ascii_uppercase(),
        Some(_) => {
            object.remove("type");
        }
        None => {}
    }
    // Native enum members must be nonempty strings. The CLI incorrectly emits empty
    // strings for numeric members, which upstream rejects; omit the whole enum rather
    // than narrowing its accepted values.
    if object.get("enum").is_some_and(|value| {
        value.as_array().is_none_or(|values| {
            values
                .iter()
                .any(|value| value.as_str().is_none_or(str::is_empty))
        })
    }) {
        object.remove("enum");
    }
    for key in ["minItems", "maxItems", "minLength", "maxLength"] {
        if let Some(value @ Value::Number(_)) = object.get_mut(key) {
            *value = Value::String(value.to_string());
        }
    }
    if let Some(properties) = object.get_mut("properties") {
        let properties = properties
            .as_object_mut()
            .ok_or_else(|| invalid("schema properties must be an object"))?;
        properties.retain(|_, schema| !schema.is_boolean());
        for schema in properties.values_mut() {
            tool_schema_projection(schema)?;
        }
    }
    if object.get("items").is_some_and(Value::is_boolean) {
        object.remove("items");
    }
    if let Some(items) = object.get_mut("items") {
        tool_schema_projection(items)?;
    }
    Ok(())
}

fn schema_projection(value: &mut Value) -> Result<(), PluginError> {
    if let Value::Bool(allowed) = value {
        // CLI 只接受 Schema 对象；等价转换保留布尔 schema 的允许/禁止语义。
        *value = if *allowed {
            json!({})
        } else {
            json!({"not": {}})
        };
        return Ok(());
    }
    let object = value
        .as_object_mut()
        .ok_or_else(|| invalid("schema must be an object"))?;
    for (source, target) in [("$ref", "ref"), ("$defs", "defs")] {
        if let Some(value) = object.remove(source) {
            object.entry(target).or_insert(value);
        }
    }
    if let Some(Value::String(reference)) = object.get_mut("ref")
        && reference.starts_with("#/$defs/")
    {
        reference.remove(2);
    }
    if let Some(Value::Array(types)) = object.get_mut("type") {
        let types = std::mem::take(types);
        if types.is_empty() {
            return Err(invalid("schema type union must not be empty"));
        }
        let mut branches = Vec::with_capacity(types.len());
        for kind in types {
            let Value::String(mut kind) = kind else {
                return Err(invalid("schema type union must contain strings"));
            };
            kind.make_ascii_uppercase();
            let mut branch = serde_json::Map::new();
            branch.insert("type".into(), Value::String(kind));
            branches.push(Value::Object(branch));
        }
        object.remove("type");
        let mut union = serde_json::Map::new();
        union.insert("anyOf".into(), Value::Array(branches));
        object
            .entry("allOf")
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| invalid("schema allOf must be an array"))?
            .push(Value::Object(union));
    }
    project(
        value,
        &[
            "type",
            "format",
            "title",
            "description",
            "nullable",
            "default",
            "items",
            "prefixItems",
            "minItems",
            "maxItems",
            "enum",
            "properties",
            "propertyOrdering",
            "required",
            "minProperties",
            "maxProperties",
            "minimum",
            "maximum",
            "minLength",
            "maxLength",
            "pattern",
            "example",
            "oneOf",
            "anyOf",
            "allOf",
            "not",
            "additionalProperties",
            "additionalPropertiesSchema",
            "ref",
            "defs",
        ],
    )?;
    let object = value.as_object_mut().expect("validated schema");
    if let Some(Value::String(kind)) = object.get_mut("type") {
        kind.make_ascii_uppercase();
    }
    for key in [
        "minItems",
        "maxItems",
        "minProperties",
        "maxProperties",
        "minLength",
        "maxLength",
    ] {
        if let Some(value @ Value::Number(_)) = object.get_mut(key) {
            *value = Value::String(value.to_string());
        }
    }
    for (key, value) in object {
        match key.as_str() {
            "properties" | "defs" => {
                for child in value
                    .as_object_mut()
                    .ok_or_else(|| invalid("schema properties must be an object"))?
                    .values_mut()
                {
                    schema_projection(child)?;
                }
            }
            "items" | "not" | "additionalPropertiesSchema" => schema_projection(value)?,
            "additionalProperties" if value.is_object() => schema_projection(value)?,
            "prefixItems" | "oneOf" | "anyOf" | "allOf" => array(value, schema_projection)?,
            _ => {}
        }
    }
    Ok(())
}

pub(crate) struct EventFraming {
    pending: Vec<u8>,
    scanned: usize,
    data: Vec<u8>,
    has_data: bool,
    limit: usize,
}
impl Default for EventFraming {
    fn default() -> Self {
        Self::with_limit(MAX_EVENT)
    }
}
impl EventFraming {
    pub(crate) fn with_limit(limit: usize) -> Self {
        Self {
            pending: Vec::new(),
            scanned: 0,
            data: Vec::new(),
            has_data: false,
            limit,
        }
    }

    pub(crate) fn push(
        &mut self,
        bytes: &[u8],
        mut emit: impl FnMut(&[u8]) -> Result<(), PluginError>,
    ) -> Result<(), PluginError> {
        self.pending.extend_from_slice(bytes);
        let mut consumed = 0;
        let mut scanned = self.scanned;
        while let Some(end) = self.pending[scanned..]
            .iter()
            .position(|byte| *byte == b'\n')
        {
            let end = scanned + end;
            let line = &self.pending[consumed..end];
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.is_empty() {
                if self.has_data {
                    emit(&self.data)?;
                    self.data.clear();
                    self.has_data = false;
                }
            } else if let Some(data) = line.strip_prefix(b"data:") {
                if self.has_data {
                    self.data.push(b'\n');
                }
                self.has_data = true;
                self.data
                    .extend_from_slice(data.strip_prefix(b" ").unwrap_or(data));
            }
            if self.data.len() > self.limit {
                return Err(common::plugin_error(
                    ErrorKind::ResourceExhausted,
                    "Antigravity SSE event exceeds limit",
                ));
            }
            consumed = end + 1;
            scanned = consumed;
        }
        // 图片 base64 可能跨很多 HTTP chunk；只扫描新到的字节。
        self.scanned = self.pending.len() - consumed;
        self.pending.drain(..consumed);
        if self.pending.len() > self.limit {
            return Err(common::plugin_error(
                ErrorKind::ResourceExhausted,
                "Antigravity SSE line exceeds limit",
            ));
        }
        Ok(())
    }
    pub(crate) fn finish(
        &mut self,
        mut emit: impl FnMut(&[u8]) -> Result<(), PluginError>,
    ) -> Result<(), PluginError> {
        self.push(b"\n\n", |payload| emit(payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_identity_matches_cli_135_representation() {
        assert_eq!(native_session_id(""), "-3750763034362895579");
        assert_eq!(
            native_session_id(" principal/目标/会话 "),
            "-2713670015314466186"
        );
        assert_eq!(
            native_session_id("principal/目标/会话"),
            "-7112187058504807916"
        );
        assert_eq!(native_session_id(&"a".repeat(64)), "-2665998720392315931");
        assert_eq!(
            native_session_id(&format!("{}尾", "a".repeat(64))),
            "7809677453811392938"
        );
        let request = json!({});
        let id = uuid::Uuid::parse_str("00112233-4455-4677-8899-aabbccddeeff").unwrap();
        let labels = RequestLabels::new(
            crate::lifecycle::Labels {
                request_id: None,
                cascade_id: "conversation".into(),
                trajectory_id: id,
                last_step_index: None,
                last_execution_id: None,
                used_claude: false,
                used_claude_conservative: true,
                used_non_gemini_model: true,
            },
            None,
        );
        let envelope = serde_json::to_value(InferenceEnvelope {
            project: "synthetic-project",
            model: "synthetic-model",
            request: InferenceBody {
                body: &request,
                labels,
            },
            user_agent: "antigravity",
            request_type: "agent",
            request_id: id,
        })
        .unwrap();
        assert_eq!(
            envelope["requestId"],
            "agent/00112233-4455-4677-8899-aabbccddeeff"
        );
        assert_eq!(
            envelope["request"]["labels"],
            json!({
                "cascade_id": "conversation", "root_cascade_id": "conversation", "trajectory_id": id,
                "used_claude": "false", "used_claude_conservative": "true", "used_non_gemini_model": "true"
            })
        );
    }

    #[test]
    fn thinking_projection_accepts_cli_enum_and_rejects_invalid_explicit_controls() {
        for (level, expected) in [
            (
                json!("THINKING_LEVEL_UNSPECIFIED"),
                json!("THINKING_LEVEL_UNSPECIFIED"),
            ),
            (json!("LOW"), json!("LOW")),
            (json!("MEDIUM"), json!("MEDIUM")),
            (json!("HIGH"), json!("HIGH")),
            (json!("MINIMAL"), json!("MINIMAL")),
            (json!("EXTRA_HIGH"), json!("EXTRA_HIGH")),
            (json!("MAX"), json!("MAX")),
            (json!("XHIGH"), json!("XHIGH")),
            (json!(0), json!("THINKING_LEVEL_UNSPECIFIED")),
            (json!(1), json!("LOW")),
            (json!(2), json!("MEDIUM")),
            (json!(3), json!("HIGH")),
            (json!(4), json!("MINIMAL")),
            (json!(5), json!("EXTRA_HIGH")),
            (json!(6), json!("MAX")),
            (json!(7), json!("XHIGH")),
            (json!(100), json!(100)),
            (json!(-1), json!(-1)),
        ] {
            let mut body = json!({"generationConfig":{"thinkingConfig":{
                "thinkingLevel":level, "includeThoughts":false
            }}});
            sanitize_request(&mut body).unwrap();
            assert_eq!(
                body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
                expected
            );
            assert_eq!(
                body["generationConfig"]["thinkingConfig"]["includeThoughts"],
                false
            );
        }
        for level in [
            json!("TURBO"),
            json!("UNSPECIFIED"),
            json!(2147483648_i64),
            json!(1.5),
            json!(true),
        ] {
            let mut body = json!({"generationConfig":{"thinkingConfig":{"thinkingLevel":level}}});
            assert!(sanitize_request(&mut body).is_err());
        }
        for budget in [json!(-2), json!(2147483648_i64), json!("4000")] {
            let mut body = json!({"generationConfig":{"thinkingConfig":{"thinkingBudget":budget}}});
            assert!(sanitize_request(&mut body).is_err());
        }
    }

    #[test]
    fn recursive_projection_drops_protocol_extensions_not_application_data() {
        let mut request = json!({
            "project":"injected", "model":"injected", "cachedContent":"foreign", "metadata":{"injected":true},
            "contents":[{"role":"user","vendorExtra":true,"parts":[{
                "functionCall":{"id":"call-1","name":"read","future":true,"args":{"seed":4,"metadata":{"future":true}}},
                "thoughtSignature":"real-signature","unknown":"remove"
            }]}],
            "tools":[{"future":true,"functionDeclarations":[{"name":"read","strict":true,"parameters":{
                "type":"object","future":true,"properties":{"metadata":{"type":"string","future":true}},"required":["metadata"]
            }}]}],
            "generationConfig":{"temperature":0.5,"seed":9,"future":true,"thinkingConfig":{"thinkingBudget":2048,"future":true}}
        });
        sanitize_request(&mut request).unwrap();
        assert_eq!(
            request,
            json!({
                "contents":[{"role":"user","parts":[{"functionCall":{"id":"call-1","name":"read","args":{"seed":4,"metadata":{"future":true}}},"thoughtSignature":"real-signature"}]}],
                "tools":[{"functionDeclarations":[{"name":"read","parameters":{"type":"OBJECT","properties":{"metadata":{"type":"STRING"}},"required":["metadata"]}}]}],
                "generationConfig":{"temperature":0.5,"thinkingConfig":{"thinkingBudget":2048}}
            })
        );
    }

    #[test]
    fn event_framing_handles_utf8_crlf_multiline_and_eof_fragments() {
        let input = "event: ignored\r\ndata: {\"response\":\r\ndata: {\"text\":\"中文\"}}\r\n\r\ndata: [DONE]";
        let mut parser = EventFraming::default();
        let mut events = Vec::new();
        for byte in input.as_bytes() {
            parser
                .push(&[*byte], |payload| {
                    events.push(payload.to_vec());
                    Ok(())
                })
                .unwrap();
        }
        parser
            .finish(|payload| {
                events.push(payload.to_vec());
                Ok(())
            })
            .unwrap();
        assert_eq!(
            events,
            vec![
                "{\"response\":\n{\"text\":\"中文\"}}".as_bytes().to_vec(),
                b"[DONE]".to_vec()
            ]
        );
    }
}
