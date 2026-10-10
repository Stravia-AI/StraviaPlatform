use serde_json::{Value, json};
use stravia_protocol_codec::accumulator::StreamResponseAccumulator;
use stravia_protocol_codec::transform::{ProtocolTransform, StreamDecodeStage};
use stravia_runtime_contract::protocol::ids::GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA;
use stravia_runtime_contract::protocol::ir::{AiErrorKind, AiRequest, AiResponse, ToolChoice};
use stravia_vendor_common::common;
use stravia_vendor_sdk::{ErrorKind, GuestHost, PluginError, ProviderSnapshot, read_http_body};

use crate::client;

const MAX_BODY: usize = 32 * 1024 * 1024;
const MAX_EVENT: usize = 16 * 1024 * 1024;

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct InferenceEnvelope<'a> {
    project: &'a str,
    model: &'a str,
    request: &'a Value,
    user_agent: &'static str,
    request_type: &'static str,
    #[serde(serialize_with = "serialize_agent_request_id")]
    request_id: uuid::Uuid,
}

fn serialize_agent_request_id<S: serde::Serializer>(
    id: &uuid::Uuid,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_str(&format_args!("agent-{id}"))
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
    let (raw_thinking_level, raw_thinking_budget) = if request.reasoning.target_control.is_none() {
        let raw = request
            .meta
            .vendor
            .ingress
            .get("__google_generation_config")
            .and_then(|config| config.get("thinkingConfig"));
        (
            raw.and_then(|thinking| thinking.get("thinkingLevel"))
                .cloned(),
            raw.and_then(|thinking| thinking.get("thinkingBudget"))
                .cloned(),
        )
    } else {
        (None, None)
    };
    let has_raw_thinking_control = raw_thinking_level.is_some() || raw_thinking_budget.is_some();
    let selector_budget = if let Some(table) = provider
        .model_metadata
        .as_ref()
        .and_then(|metadata| metadata.extensions.get(crate::selector::EXTENSION_KEY))
    {
        let selection = crate::selector::resolve(table, request.reasoning.target_control.as_ref())?;
        request.model = selection.id.into();
        let effort = matches!(
            request.reasoning.target_control,
            Some(stravia_runtime_contract::thinking::TargetThinkingControl::Effort { .. })
        );
        if effort {
            // CLI 同时选择真实 ID 与该档位的目录预算；不再生成第二个 level 控制。
            request.reasoning.target_control = None;
            request.reasoning.effort = None;
            request.reasoning.budget_tokens = None;
        }
        selection.thinking_budget
    } else {
        if let Some(selector) = provider
            .model_metadata
            .as_ref()
            .and_then(|metadata| metadata.selector.as_deref())
        {
            request.model = selector.into();
        }
        None
    };
    if request.embedding.is_some() {
        return Err(common::plugin_error(
            ErrorKind::Unsupported,
            "Antigravity does not provide embeddings",
        ));
    }
    // 在跨协议校验前删除 CLI 没有承诺的控制；最后的递归投影仍是唯一出口。
    request.generation.seed = None;
    request.generation.presence_penalty = None;
    request.generation.frequency_penalty = None;
    request.parallel_tool_calls = None;
    request.disable_parallel_tool_calls = None;
    request.ext = request.ext.take().filter(|extension| {
        matches!(
            extension,
            stravia_runtime_contract::protocol::ir::ext::ProtocolExt::Google(_)
        )
    });
    // CLI 使用 private master Schema，不能让公开 Gemini codec 的能力校验
    // 拒绝 strict 或移除已核对的 schema 约束；这些边界由下方显式构造。
    let tools = request.tools.take();
    let tool_choice = request.tool_choice.take();
    let response_format = request.response_format.take();
    let stop = request.generation.stop.take();
    request.stream.enabled = true;
    let protocol = GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA;
    let model = request.model.clone();
    let mut body = common::encode_inference_request(&protocol.to_string(), request)?.body;
    let object = body
        .as_object_mut()
        .ok_or_else(|| invalid("Gemini request must be an object"))?;
    if let Some(thinking) = object
        .get_mut("generationConfig")
        .and_then(|config| config.get_mut("thinkingConfig"))
        .and_then(Value::as_object_mut)
    {
        for (field, value) in [
            ("thinkingLevel", raw_thinking_level),
            ("thinkingBudget", raw_thinking_budget),
        ] {
            if let Some(value) = value {
                thinking.insert(field.into(), value);
            }
        }
        if let Some(budget) = selector_budget.filter(|_| !has_raw_thinking_control) {
            thinking.remove("thinkingLevel");
            thinking.insert("thinkingBudget".into(), json!(budget));
        }
    }
    // 同协议 raw tools 保留原始 function response 与内置工具，再统一投影。
    if let Some(tools) = tools.filter(|tools| !tools.is_empty())
        && !object.contains_key("tools")
    {
        let mut declarations = Vec::with_capacity(tools.len());
        for tool in tools {
            let mut declaration = serde_json::Map::new();
            declaration.insert("name".into(), Value::String(tool.name));
            declaration.insert("parameters".into(), tool.parameters);
            if let Some(description) = tool.description {
                declaration.insert("description".into(), Value::String(description));
            }
            declarations.push(Value::Object(declaration));
        }
        let mut tool = serde_json::Map::new();
        tool.insert("functionDeclarations".into(), Value::Array(declarations));
        object.insert("tools".into(), Value::Array(vec![Value::Object(tool)]));
    }
    if !object.contains_key("toolConfig") {
        let config = match tool_choice {
            Some(ToolChoice::Auto) => Some(("AUTO", None)),
            Some(ToolChoice::None) => Some(("NONE", None)),
            Some(ToolChoice::Required) => Some(("ANY", None)),
            Some(ToolChoice::Named { name }) => Some(("ANY", Some(name))),
            Some(ToolChoice::Raw(_)) | None => None,
        };
        if let Some((mode, name)) = config {
            let mut calling = serde_json::Map::new();
            calling.insert("mode".into(), Value::String(mode.into()));
            if let Some(name) = name {
                calling.insert(
                    "allowedFunctionNames".into(),
                    Value::Array(vec![Value::String(name)]),
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
    // 会话、项目和请求身份由宿主/本连接生成，不能由下游额外字段指定。
    if let Some(session) = common::session_affinity(&provider) {
        body.as_object_mut()
            .expect("request was validated")
            .insert("sessionId".into(), json!(session));
    }
    let project = client::project(host, &provider)?;
    let envelope = InferenceEnvelope {
        project: &project,
        model: &model,
        request: &body,
        user_agent: "antigravity",
        request_type: "agent",
        request_id: uuid::Uuid::new_v4(),
    };
    let headers = client::headers(&provider)?;
    let bytes = serde_json::to_vec(&envelope)
        .map_err(|_| invalid("Antigravity request could not be encoded"))?;
    host.emit_started()?;
    let response = host.http_start(stravia_vendor_sdk::wit::types::HttpRequest {
        method: "POST".into(),
        url: format!(
            "{}?alt=sse",
            client::endpoint(&provider, "streamGenerateContent")
        ),
        headers,
        body: bytes,
    })?;
    let status = response.status()?;
    let headers = response.headers()?;
    if !(200..300).contains(&status) {
        let bytes = read_http_body(&response, MAX_EVENT)?;
        return Err(common::upstream_error(status, &headers, &bytes));
    }
    let streaming = headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("content-type")
            && value.to_ascii_lowercase().contains("text/event-stream")
    });
    if !streaming {
        let bytes = read_http_body(&response, MAX_BODY)?;
        let mut value: Value = serde_json::from_slice(&bytes)
            .map_err(|_| invalid_upstream("Antigravity returned invalid JSON"))?;
        if let Some(error) = response_error(&value) {
            return Err(error);
        }
        let mut inner = value.get_mut("response").map(Value::take).unwrap_or(value);
        normalize_final_usage(&mut inner);
        return ProtocolTransform::global()
            .bind(protocol, protocol)
            .and_then(|pair| pair.decode_response(inner))
            .map_err(common::map_response_transform_error);
    }
    let mut decoder = ProtocolTransform::global()
        .decode_stream(protocol)
        .map_err(common::map_response_transform_error)?;
    let mut accumulator = StreamResponseAccumulator::default();
    let mut framing = EventFraming::default();
    let mut terminal = false;
    while let Some(chunk) = response.read_body()? {
        framing.push(&chunk, |payload| {
            decode_event(host, &mut decoder, &mut accumulator, &mut terminal, payload)
        })?;
    }
    framing.finish(|payload| {
        decode_event(host, &mut decoder, &mut accumulator, &mut terminal, payload)
    })?;
    let deltas = decoder
        .finish()
        .map_err(common::map_response_transform_error)?;
    common::emit_deltas(host, &mut accumulator, &deltas)?;
    if !terminal {
        return Err(common::model_error(
            AiErrorKind::UnexpectedEof,
            "Antigravity stream ended before a finish reason",
        ));
    }
    let complete = accumulator.into_ai_response();
    host.emit_completed(&complete)?;
    Ok(complete)
}

fn decode_event(
    host: &GuestHost,
    decoder: &mut StreamDecodeStage,
    accumulator: &mut StreamResponseAccumulator,
    terminal: &mut bool,
    payload: &[u8],
) -> Result<(), PluginError> {
    if payload == b"[DONE]" {
        return Ok(());
    }
    let mut value: Value = serde_json::from_slice(payload)
        .map_err(|_| invalid_upstream("Antigravity stream contained invalid JSON"))?;
    if let Some(error) = response_error(&value) {
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
    common::emit_deltas(host, accumulator, &deltas)
}

fn normalize_final_usage(response: &mut Value) {
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

fn response_error(value: &Value) -> Option<PluginError> {
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
        &[],
        &serde_json::to_vec(&json!({"error": error})).expect("JSON value encodes"),
    ))
}

fn invalid(message: &str) -> PluginError {
    common::plugin_error(ErrorKind::Invalid, message)
}
fn invalid_upstream(message: &str) -> PluginError {
    common::model_error(AiErrorKind::ServerError, message)
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
        // CLI 1.3.2 master 的真实 enum；非法显式值不能静默丢弃。
        if let Some(level) = thinking.get("thinkingLevel") {
            let valid = match level {
                Value::String(value) => matches!(
                    value.as_str(),
                    "UNSPECIFIED" | "LOW" | "MEDIUM" | "HIGH" | "MINIMAL" | "EXTRA_HIGH" | "MAX"
                ),
                Value::Number(value) => {
                    value.as_i64().is_some_and(|value| (0..=6).contains(&value))
                }
                _ => false,
            };
            if !valid {
                return Err(invalid(
                    "Antigravity thinkingLevel must be a CLI ThinkingLevel enum",
                ));
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
            project(function, &["name", "description", "parameters", "response"])?;
            if let Some(parameters) = function.get_mut("parameters") {
                schema_projection(parameters)?;
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

#[derive(Default)]
struct EventFraming {
    pending: Vec<u8>,
    data: Vec<u8>,
    has_data: bool,
}
impl EventFraming {
    fn push(
        &mut self,
        bytes: &[u8],
        mut emit: impl FnMut(&[u8]) -> Result<(), PluginError>,
    ) -> Result<(), PluginError> {
        self.pending.extend_from_slice(bytes);
        let mut consumed = 0;
        while let Some(end) = self.pending[consumed..]
            .iter()
            .position(|byte| *byte == b'\n')
        {
            let end = consumed + end;
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
            if self.data.len() > MAX_EVENT {
                return Err(common::plugin_error(
                    ErrorKind::ResourceExhausted,
                    "Antigravity SSE event exceeds limit",
                ));
            }
            consumed = end + 1;
        }
        self.pending.drain(..consumed);
        if self.pending.len() > MAX_EVENT {
            return Err(common::plugin_error(
                ErrorKind::ResourceExhausted,
                "Antigravity SSE line exceeds limit",
            ));
        }
        Ok(())
    }
    fn finish(
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
    fn thinking_projection_accepts_cli_enum_and_rejects_invalid_explicit_controls() {
        for level in [
            json!("UNSPECIFIED"),
            json!("LOW"),
            json!("MEDIUM"),
            json!("HIGH"),
            json!("MINIMAL"),
            json!("EXTRA_HIGH"),
            json!("MAX"),
            json!(0),
            json!(1),
            json!(2),
            json!(3),
            json!(4),
            json!(5),
            json!(6),
        ] {
            let mut body = json!({"generationConfig":{"thinkingConfig":{
                "thinkingLevel":level, "includeThoughts":false
            }}});
            sanitize_request(&mut body).unwrap();
            assert_eq!(
                body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
                level
            );
            assert_eq!(
                body["generationConfig"]["thinkingConfig"]["includeThoughts"],
                false
            );
        }
        for level in [
            json!("TURBO"),
            json!("XHIGH"),
            json!(-1),
            json!(7),
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
