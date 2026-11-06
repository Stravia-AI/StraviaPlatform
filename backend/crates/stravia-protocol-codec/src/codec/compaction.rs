//! Native compaction control classification on a decoded canonical request.
//!
//! Moved out of `stravia-core::compaction`: the persisted store stays host-side
//! while request classification is pure wire semantics shared with plugins.
//!
//! It also owns the stateless envelope that lets a client speaking one protocol carry
//! another protocol's native compaction state in its own opaque field.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};

use stravia_runtime_contract::protocol::ir::{
    AiError, AiErrorKind, AiItem, AiRequest, AiResponse, CompactionControl, ContentBlock,
    MessageContent, NativeCompactionResponse, Role,
};

mod responses;
pub use responses::parse_responses_compaction_stream;

/// Marks an opaque field value as a Stravia envelope. Native Anthropic signatures and
/// OpenAI `encrypted_content` never start with it, so unwrapped state is never misread.
const ENVELOPE_PREFIX: &str = "stravia.compaction.v1.";

/// Placeholder for the readable `content` of an Anthropic block that carries Responses
/// state. The summary is opaque to Stravia; the client must not treat this as one.
pub const ENVELOPE_PLACEHOLDER_CONTENT: &str = "[Stravia remote compaction: opaque state]";

/// Item metadata key holding the complete Anthropic `compaction` block, so replay is
/// byte-faithful (the API rejects any edited signature, content or stray field).
pub const ANTHROPIC_COMPACTION_BLOCK_META: &str = "__anthropic_compaction_block";

/// 缺省摘要任务指令；显式会话指令与 Anthropic 摘要指令均不被覆盖。
pub const DEFAULT_COMPACTION_INSTRUCTIONS: &str = concat!(
    "Summarize the conversation above as a concise, structured handoff for another assistant ",
    "to continue the task. Do not continue the conversation or answer questions in it.\n\n",
    "Preserve the user's goals, constraints, preferences, decisions, completed work, current ",
    "work, blockers, and next steps. Preserve unanswered questions and pending requests exactly. ",
    "Keep essential file paths, identifiers, error messages, command results, and repository ",
    "changes. Distinguish observed facts from assumptions.\n\n",
    "Use the sections Goal, Constraints, Progress, Decisions, Next Steps, and Critical Context ",
    "where applicable. The summary replaces the supplied history: anything omitted will be ",
    "lost. Do not call tools. Return only the summary.",
);

/// `anthropic-beta` value that enables on-demand compaction requests and blocks.
pub const ANTHROPIC_COMPACTION_BETA: &str = "compact-2026-09-04";

/// Stable Responses-side identity for an Anthropic compaction state. Anthropic blocks
/// have no id; deriving it from the signature keeps registration and replay aligned.
pub fn anthropic_state_id(signature: &str) -> String {
    format!("cmp_{}", short_hash(signature))
}

/// Foreign-protocol native state recovered from an opaque field.
#[derive(Debug, Clone, PartialEq)]
pub enum Envelope {
    /// A complete Anthropic `compaction` content block, carried in a Responses
    /// `encrypted_content`.
    Anthropic { block: Value },
    /// The `output` window of a Responses compact resource, carried in an Anthropic
    /// `compaction` block `signature`.
    Responses { output: Vec<Value> },
}

pub fn seal_anthropic(block: &Value) -> String {
    seal(&json!({"v": 1, "k": "anthropic", "block": block}))
}

pub fn seal_responses(output: &[Value]) -> String {
    seal(&json!({"v": 1, "k": "responses", "output": output}))
}

fn seal(payload: &Value) -> String {
    let bytes = serde_json::to_vec(payload).expect("JSON values always serialize");
    format!("{ENVELOPE_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes))
}

/// Returns `None` for any value that is not a well-formed v1 envelope, including native
/// state, so callers can fall through to the protocol's own interpretation.
pub fn open(token: &str) -> Option<Envelope> {
    let payload = URL_SAFE_NO_PAD
        .decode(token.strip_prefix(ENVELOPE_PREFIX)?)
        .ok()?;
    let mut payload: Value = serde_json::from_slice(&payload).ok()?;
    if payload.get("v").and_then(Value::as_u64) != Some(1) {
        return None;
    }
    match payload.get("k").and_then(Value::as_str)? {
        "anthropic" => {
            let block = payload.get_mut("block")?.take();
            (block.get("type").and_then(Value::as_str) == Some("compaction")
                && block.get("content").is_some_and(Value::is_string)
                && block.get("signature").is_some_and(Value::is_string))
            .then_some(Envelope::Anthropic { block })
        }
        "responses" => {
            let Value::Array(output) = payload.get_mut("output")?.take() else {
                return None;
            };
            output
                .iter()
                .any(|item| item.get("type").and_then(Value::as_str) == Some("compaction"))
                .then_some(Envelope::Responses { output })
        }
        _ => None,
    }
}

fn short_hash(seed: &str) -> String {
    use stravia_runtime_contract::protocol::ir::canonical::{hash_bytes, hash_hex};
    hash_hex(&hash_bytes(seed.as_bytes()))[..24].to_owned()
}

fn anthropic_compaction_parts(message: &Value) -> anyhow::Result<(&Value, u64, u64)> {
    let mut blocks = message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("compaction"));
    let block = blocks
        .next()
        .filter(|block| {
            block
                .get("content")
                .and_then(Value::as_str)
                .is_some_and(|content| !content.is_empty())
                && block
                    .get("signature")
                    .and_then(Value::as_str)
                    .is_some_and(|signature| !signature.is_empty())
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Anthropic produced no compaction summary (stop_reason: {})",
                message
                    .get("stop_reason")
                    .and_then(Value::as_str)
                    .unwrap_or("none")
            )
        })?;
    anyhow::ensure!(
        blocks.next().is_none(),
        "Anthropic compaction returned multiple native states"
    );
    anyhow::ensure!(
        message.get("stop_reason").and_then(Value::as_str) == Some("compaction"),
        "Anthropic compaction stopped with stop_reason: {}",
        message["stop_reason"]
    );
    // Compaction billing lives in `usage.iterations`; the top level is zeroed.
    let (input, output) = message
        .pointer("/usage/iterations")
        .and_then(Value::as_array)
        .map(|iterations| {
            iterations
                .iter()
                .filter(|iteration| {
                    iteration.get("type").and_then(Value::as_str) == Some("compaction")
                })
                .fold((0u64, 0u64), |(input, output), iteration| {
                    (
                        input.saturating_add(iteration["input_tokens"].as_u64().unwrap_or(0)),
                        output.saturating_add(iteration["output_tokens"].as_u64().unwrap_or(0)),
                    )
                })
        })
        .unwrap_or_else(|| {
            (
                message
                    .pointer("/usage/input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                message
                    .pointer("/usage/output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            )
        });
    Ok((block, input, output))
}

/// SSE events for an Anthropic compaction Message. The API delivers the block whole in
/// one `content_block_start` with no deltas, so a unary upstream result streams exactly
/// like the native one.
pub fn anthropic_compaction_events(message: &Value) -> Vec<(&'static str, Value)> {
    let mut start = message.clone();
    if let Some(object) = start.as_object_mut() {
        object.insert("content".into(), json!([]));
        object.insert("stop_reason".into(), Value::Null);
        object.insert("stop_sequence".into(), Value::Null);
        object.insert(
            "usage".into(),
            json!({"input_tokens": 0, "output_tokens": 0}),
        );
    }
    let blocks = message
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut events = vec![(
        "message_start",
        json!({"type": "message_start", "message": start}),
    )];
    for (index, block) in blocks.into_iter().enumerate() {
        events.push((
            "content_block_start",
            json!({"type": "content_block_start", "index": index, "content_block": block}),
        ));
        events.push((
            "content_block_stop",
            json!({"type": "content_block_stop", "index": index}),
        ));
    }
    events.push((
        "message_delta",
        json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": message.get("stop_reason").cloned().unwrap_or(Value::Null),
                "stop_sequence": null,
            },
            "usage": message.get("usage").cloned().unwrap_or_else(|| json!({})),
        }),
    ));
    events.push(("message_stop", json!({"type": "message_stop"})));
    events
}

/// 把完整的 Anthropic Messages SSE 响应折叠回原始 `message` 对象。
///
/// 原生压缩强制 `stream: true`：上游在 `content_block_start` 中一次给出完整的
/// `compaction` 块，在 `message_delta.usage` 中给出 `iterations` 计费明细。这里
/// 直接聚合原始 SSE 信封，不经过普通的 `AiStreamDelta` 解码，因此签名块里的所有
/// 未知字段（`signature`、`tool_changes` 等）与 usage 明细都会原样保留，再由
/// [`parse_anthropic_compaction`] 密封给宿主。
///
/// 只有收到 `message_stop` 且 `stop_reason == "compaction"` 才算完成；流内
/// `error` 事件、非法 JSON、未终止流都返回错误，不把截断结果当成成功摘要。
pub fn fold_anthropic_compaction_stream(body: &[u8]) -> anyhow::Result<Value> {
    let text = std::str::from_utf8(body)
        .map_err(|error| anyhow::anyhow!("Anthropic compaction stream is not UTF-8: {error}"))?;
    let mut fold = AnthropicCompactionFold::default();
    let mut buffer = String::new();
    crate::consume_sse_frames(&mut buffer, text, |frame| fold.push_frame(frame))?;
    if !buffer.trim().is_empty() {
        // 末帧可以省略结尾空行；补上后仍要求 `message_stop`，真正的截断继续失败。
        buffer.push_str("\n\n");
        crate::consume_sse_frames(&mut buffer, "", |frame| fold.push_frame(frame))?;
    }
    fold.finish()
}

#[derive(Default)]
struct AnthropicCompactionFold {
    message: Option<Value>,
    blocks: Vec<Value>,
    saw_message_stop: bool,
}

impl AnthropicCompactionFold {
    fn push_frame(&mut self, frame: &str) -> anyhow::Result<()> {
        let Some((event, mut data)) = parse_anthropic_compaction_event(frame)? else {
            return Ok(());
        };
        let event = event
            .or_else(|| data.get("type").and_then(Value::as_str))
            .ok_or_else(|| anyhow::anyhow!("Anthropic compaction stream event has no type"))?;
        match event {
            "message_start" => {
                if self.message.is_some() {
                    anyhow::bail!("Anthropic compaction stream repeated message_start");
                }
                let message = data.get_mut("message").map(Value::take).ok_or_else(|| {
                    anyhow::anyhow!("Anthropic compaction message_start has no message")
                })?;
                anyhow::ensure!(
                    message.is_object(),
                    "Anthropic compaction message_start message is not an object"
                );
                self.message = Some(message);
            }
            "content_block_start" => {
                anyhow::ensure!(
                    self.message.is_some(),
                    "Anthropic compaction content arrived before message_start"
                );
                anyhow::ensure!(
                    data.get("index").and_then(Value::as_u64) == Some(self.blocks.len() as u64),
                    "Anthropic compaction content block index is out of order"
                );
                let block = data
                    .get_mut("content_block")
                    .map(Value::take)
                    .ok_or_else(|| {
                        anyhow::anyhow!("Anthropic compaction content_block_start has no block")
                    })?;
                anyhow::ensure!(
                    block.is_object(),
                    "Anthropic compaction content block is not an object"
                );
                self.blocks.push(block);
            }
            // on-demand 压缩只交付完整块，不复用普通生成的文本/工具增量拼接。
            "content_block_delta" => {
                anyhow::bail!("Anthropic on-demand compaction returned an unexpected block delta")
            }
            "message_delta" => {
                let message = self.message.as_mut().ok_or_else(|| {
                    anyhow::anyhow!("Anthropic compaction content arrived before message_start")
                })?;
                let object = message.as_object_mut().ok_or_else(|| {
                    anyhow::anyhow!("Anthropic compaction message is not an object")
                })?;
                if let Some(delta) = data.get_mut("delta") {
                    let Value::Object(delta) = delta.take() else {
                        anyhow::bail!("Anthropic compaction message_delta is not an object");
                    };
                    object.extend(delta);
                }
                if let Some(usage) = data.get_mut("usage") {
                    let Value::Object(usage) = usage.take() else {
                        anyhow::bail!("Anthropic compaction usage is not an object");
                    };
                    let target = object
                        .entry("usage")
                        .or_insert_with(|| Value::Object(Default::default()));
                    let target = target.as_object_mut().ok_or_else(|| {
                        anyhow::anyhow!("Anthropic compaction message usage is not an object")
                    })?;
                    target.extend(usage);
                }
            }
            "message_stop" => self.saw_message_stop = true,
            "error" => {
                let error = data.get("error").unwrap_or(&data);
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| error.to_string());
                return Err(AiError::new(AiErrorKind::ServerError, message)
                    .with_raw(data)
                    .into());
            }
            // 心跳和块结束不改变整块交付的签名载荷。
            _ => {}
        }
        Ok(())
    }

    fn finish(mut self) -> anyhow::Result<Value> {
        if !self.saw_message_stop {
            anyhow::bail!("Anthropic compaction stream ended before message_stop");
        }
        let mut message = self.message.take().ok_or_else(|| {
            anyhow::anyhow!("Anthropic compaction stream ended before message_start")
        })?;
        let stop_reason = message.get("stop_reason").and_then(Value::as_str);
        if stop_reason != Some("compaction") {
            anyhow::bail!(
                "Anthropic compaction stream stopped without a compaction summary (stop_reason: {})",
                stop_reason.unwrap_or("none")
            );
        }
        message
            .as_object_mut()
            .expect("message is an object")
            .insert("content".into(), Value::Array(self.blocks));
        Ok(message)
    }
}

/// 解析一帧 SSE；单行 data 借用原缓冲区，多行时才分配拼接。
fn parse_anthropic_compaction_event(frame: &str) -> anyhow::Result<Option<(Option<&str>, Value)>> {
    let mut event = None;
    let mut data: Option<std::borrow::Cow<'_, str>> = None;
    for line in frame.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(name) = line.strip_prefix("event:") {
            event = Some(name.trim());
        } else if let Some(payload) = line.strip_prefix("data:") {
            let payload = payload.strip_prefix(' ').unwrap_or(payload);
            match &mut data {
                Some(data) => {
                    let data = data.to_mut();
                    data.push('\n');
                    data.push_str(payload);
                }
                None => data = Some(std::borrow::Cow::Borrowed(payload)),
            }
        }
    }
    let Some(data) = data else {
        return Ok(None);
    };
    let data: Value = serde_json::from_str(&data).map_err(|error| {
        anyhow::anyhow!("Anthropic compaction stream returned invalid JSON event: {error}")
    })?;
    Ok(Some((event, data)))
}

/// Native standalone compaction result from an Anthropic upstream.
///
/// `items` 用于原生状态登记与协议转换；`wire` 保留身份和原生用量扩展。
pub fn parse_anthropic_compaction(
    message: Value,
) -> anyhow::Result<stravia_runtime_contract::protocol::ir::NativeCompactionResponse> {
    let (block, input, output) = anthropic_compaction_parts(&message)?;
    let signature = block["signature"].as_str().expect("validated signature");
    let mut item = AiItem {
        role: Role::Assistant,
        content: MessageContent::Blocks(vec![ContentBlock::Compaction {
            encrypted_content: signature.to_owned(),
            summary: Some(
                block["content"]
                    .as_str()
                    .expect("validated summary")
                    .to_owned(),
            ),
        }]),
        tool_calls: None,
        tool_call_id: None,
        meta: Some(
            stravia_runtime_contract::protocol::ir::AiItemMetadata::boxed(
                json!({ANTHROPIC_COMPACTION_BLOCK_META: block}),
            ),
        ),
    };
    item.set_graph_metadata(
        Some(anthropic_state_id(signature)),
        None,
        stravia_runtime_contract::protocol::ir::AiItemProvenance::Provider,
        stravia_runtime_contract::protocol::ir::AiItemAudience::Client,
    );
    let prompt_tokens = u32::try_from(input)?;
    let completion_tokens = u32::try_from(output)?;
    let usage = stravia_runtime_contract::protocol::ir::Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens: u32::try_from(input.saturating_add(output))?,
        required_components_known: true,
        cache_read_tokens: Some(0),
        reasoning_tokens: Some(0),
        ..Default::default()
    };
    Ok(NativeCompactionResponse {
        wire: message,
        items: vec![item],
        usage: Some(usage),
    })
}

/// 将上游压缩结果投影为可交付的 IR；V2 客户端自行重建保留消息窗口。
pub fn compaction_response_ir(
    response: NativeCompactionResponse,
    model: &str,
) -> anyhow::Result<AiResponse> {
    let id = response
        .wire
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Compaction response has no identity"))?;
    let mut canonical = AiResponse::new(id, model);
    let mut items = response.items;
    items.retain(AiItem::is_compaction);
    anyhow::ensure!(!items.is_empty(), "Compaction response has no native state");
    anyhow::ensure!(
        items.len() == 1,
        "Compaction response has multiple native states"
    );
    canonical.items = items;
    canonical.stop_reason = Some("compaction".into());
    if let Some(usage) = response.usage {
        canonical.usage = usage;
    }
    if let Some(created_at) = response.wire.get("created_at") {
        canonical.vendor.ingress.insert(
            "__open_responses_effective_request".into(),
            json!({"created_at": created_at}),
        );
    }
    if response.wire.get("type").and_then(Value::as_str) == Some("message")
        && let Some(usage) = response.wire.get("usage")
    {
        canonical
            .vendor
            .egress
            .insert("__anthropic_compaction_usage".into(), usage.clone());
    }
    Ok(canonical)
}

/// 补全独立压缩任务；显式会话指令保持原样，不与摘要专用指令混用。
pub fn prepare_compaction_request(request: &mut AiRequest) {
    if request.instructions.is_none() {
        request.instructions = Some(DEFAULT_COMPACTION_INSTRUCTIONS.into());
    }
    if !standalone_compaction_requested(request) {
        request.items.push(AiItem {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::CompactionTrigger {}]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        });
    }
}

/// V2 仅有会话级 instructions 载体；原生摘要指令以独立段落保留，不能覆盖会话。
pub fn prepare_responses_compaction_request(request: &mut AiRequest) {
    if let Some(instructions) = request
        .parameters
        .compaction
        .take()
        .and_then(|control| control.instructions)
    {
        match &mut request.instructions {
            Some(session) => {
                session.push_str("\n\nCompaction instructions:\n");
                session.push_str(&instructions);
            }
            None => request.instructions = Some(instructions),
        }
    }
    prepare_compaction_request(request);
    request.stream.enabled = true;
    request.parameters.store = Some(false);
}

/// 消费 IR 中的摘要控制与触发器，生成 Anthropic 原生请求控制。
pub fn take_anthropic_compaction_control(request: &mut AiRequest) -> CompactionControl {
    if request.instructions.is_none() {
        request.instructions = Some(DEFAULT_COMPACTION_INSTRUCTIONS.into());
    }
    request.items.retain(|item| !item.is_compaction_trigger());
    request.parameters.compaction.take().unwrap_or_default()
}

pub fn standalone_compaction_requested(request: &AiRequest) -> bool {
    request.items.iter().any(AiItem::is_compaction_trigger)
}

/// 从规范状态生成原生 Anthropic 块；未知签名字段保留，已知字段以 IR 为准。
pub(crate) fn anthropic_compaction_block(item: &AiItem) -> Option<Value> {
    let MessageContent::Blocks(blocks) = &item.content else {
        return None;
    };
    let [
        ContentBlock::Compaction {
            encrypted_content,
            summary,
        },
    ] = blocks.as_slice()
    else {
        return None;
    };
    Some(if let Some(summary) = summary {
        let mut block = item
            .meta
            .as_ref()
            .and_then(|meta| meta.get(ANTHROPIC_COMPACTION_BLOCK_META))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        block.insert("type".into(), "compaction".into());
        block.insert("content".into(), summary.clone().into());
        block.insert("signature".into(), encrypted_content.clone().into());
        Value::Object(block)
    } else {
        let wire = stravia_runtime_contract::protocol::ir::canonical::native_compaction_item(item)?;
        json!({
            "type": "compaction", "content": ENVELOPE_PLACEHOLDER_CONTENT,
            "signature": seal_responses(&[wire]),
        })
    })
}

/// Turn an encoded Anthropic Messages body into a standalone compaction request.
///
/// 与 OMP 的原生压缩请求一致：始终 `stream: true`，让上游在 SSE 信封中一次交付完整
/// 签名块与 `usage.iterations`。API 拒绝 `compaction` 与 `context_management`、
/// `stop_sequences`、`output_config.format`、强制 `tool_choice` 同时出现，并要求
/// `output_config.task_budget.remaining` 不随压缩请求发送，因此这里只删除这些互斥
/// 字段；`model`、`system`、`tools`、`thinking`、`effort` 与 task budget 的其余
/// 字段继续按编码结果发送。
pub fn prepare_anthropic_compaction_body(
    body: &mut Value,
    control: Option<CompactionControl>,
) -> anyhow::Result<()> {
    let object = body
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Anthropic compaction request body must be an object"))?;
    for key in ["stop_sequences", "tool_choice", "context_management"] {
        object.remove(key);
    }
    object.insert("stream".into(), Value::Bool(true));
    if let Some(config) = object
        .get_mut("output_config")
        .and_then(Value::as_object_mut)
    {
        config.remove("format");
        if let Some(budget) = config.get_mut("task_budget").and_then(Value::as_object_mut) {
            budget.remove("remaining");
        }
        if config.is_empty() {
            object.remove("output_config");
        }
    }
    let instructions = control
        .and_then(|control| control.instructions)
        .unwrap_or_else(|| DEFAULT_COMPACTION_INSTRUCTIONS.into());
    object.insert(
        "compaction".into(),
        json!({"type": "summarize", "instructions": instructions}),
    );
    Ok(())
}

/// Add the compaction beta to an `anthropic-beta` header list without dropping betas
/// that are already enabled.
pub fn add_anthropic_compaction_beta(headers: &mut Vec<(String, String)>) {
    match headers
        .iter_mut()
        .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
    {
        Some((_, value)) => {
            if !value
                .split(',')
                .any(|beta| beta.trim() == ANTHROPIC_COMPACTION_BETA)
            {
                if !value.trim().is_empty() {
                    value.push(',');
                }
                value.push_str(ANTHROPIC_COMPACTION_BETA);
            }
        }
        None => headers.push(("anthropic-beta".into(), ANTHROPIC_COMPACTION_BETA.into())),
    }
}

/// Whether a decoded request asks for native compaction.
///
/// A unified compaction control or an explicit trigger activates compaction.
pub fn native_compaction_requested(request: &AiRequest) -> bool {
    if request.items.iter().any(AiItem::is_compaction_trigger) {
        return true;
    }
    request.parameters.compaction.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use stravia_runtime_contract::protocol::ids::{
        ANTHROPIC_MESSAGES_2023_06_01, OPEN_RESPONSES_2026_04_24,
    };
    use stravia_runtime_contract::protocol::ir::CompactionControl;

    fn responses_result(message: Value) -> Value {
        let canonical =
            compaction_response_ir(parse_anthropic_compaction(message).unwrap(), "m").unwrap();
        crate::transform::ProtocolTransform::global()
            .bind(OPEN_RESPONSES_2026_04_24, ANTHROPIC_MESSAGES_2023_06_01)
            .unwrap()
            .encode_response(&canonical)
            .unwrap()
    }

    fn anthropic_message() -> Value {
        json!({
            "id": "msg_01",
            "type": "message",
            "role": "assistant",
            "model": "claude-opus",
            "content": [{"type": "compaction", "content": "Summary of the work", "signature": "sig-abc"}],
            "stop_reason": "compaction",
            "stop_sequence": null,
            "usage": {
                "input_tokens": 0,
                "output_tokens": 0,
                "iterations": [
                    {"type": "compaction", "input_tokens": 1200, "output_tokens": 300},
                    {"type": "message", "input_tokens": 5, "output_tokens": 1}
                ]
            }
        })
    }

    #[test]
    fn unified_compaction_control_activates_with_or_without_threshold() {
        let mut request = AiRequest::new("model", vec![]);
        assert!(!native_compaction_requested(&request));
        for threshold_tokens in [None, Some(100_000)] {
            request.parameters.compaction = Some(CompactionControl {
                threshold_tokens,
                instructions: None,
            });
            assert!(native_compaction_requested(&request));
        }
    }

    #[test]
    fn envelope_round_trips_and_ignores_native_state() {
        let block = json!({"type": "compaction", "content": "s", "signature": "sig"});
        assert_eq!(
            open(&seal_anthropic(&block)),
            Some(Envelope::Anthropic { block })
        );
        let output = vec![json!({"type": "compaction", "encrypted_content": "enc"})];
        assert_eq!(
            open(&seal_responses(&output)),
            Some(Envelope::Responses { output })
        );
        assert_eq!(open("gAAAAABnative-openai-ciphertext"), None);
        assert_eq!(open(&format!("{ENVELOPE_PREFIX}not-base64!")), None);
    }

    #[test]
    fn anthropic_result_becomes_a_responses_resource_with_billed_compaction_tokens() {
        let resource = responses_result(anthropic_message());
        assert_eq!(resource["object"], "response");
        assert_eq!(resource["output"][0]["id"], anthropic_state_id("sig-abc"));
        assert_eq!(resource["usage"]["input_tokens"], 1200);
        assert_eq!(resource["usage"]["output_tokens"], 300);
        let Some(Envelope::Anthropic { block }) =
            open(resource["output"][0]["encrypted_content"].as_str().unwrap())
        else {
            panic!("state must be sealed");
        };
        assert_eq!(block["signature"], "sig-abc");
        assert_eq!(block["content"], "Summary of the work");
    }

    #[test]
    fn anthropic_result_without_a_summary_is_an_error() {
        let mut message = anthropic_message();
        message["content"] = json!([]);
        message["stop_reason"] = json!("max_tokens");
        let error = parse_anthropic_compaction(message).unwrap_err();
        assert!(error.to_string().contains("max_tokens"), "{error}");
    }

    #[test]
    fn responses_state_travels_inside_one_anthropic_block() {
        let items = crate::codec::open_responses::parser::decode_compaction_window(&[
            json!({"type": "compaction", "id": "cmp_1", "encrypted_content": "enc"}),
        ])
        .unwrap();
        let canonical = compaction_response_ir(
            NativeCompactionResponse {
                wire: json!({"id": "resp_1"}),
                items,
                usage: Some(stravia_runtime_contract::protocol::ir::Usage {
                    prompt_tokens: 10,
                    completion_tokens: 4,
                    total_tokens: 14,
                    required_components_known: true,
                    ..Default::default()
                }),
            },
            "claude-x",
        )
        .unwrap();
        let message = crate::transform::ProtocolTransform::global()
            .bind(ANTHROPIC_MESSAGES_2023_06_01, OPEN_RESPONSES_2026_04_24)
            .unwrap()
            .encode_response(&canonical)
            .unwrap();
        assert_eq!(message["stop_reason"], "compaction");
        assert_eq!(message["usage"]["iterations"][0]["input_tokens"], 10);
        let block = &message["content"][0];
        assert_eq!(block["content"], ENVELOPE_PLACEHOLDER_CONTENT);
        let Some(Envelope::Responses { output }) = open(block["signature"].as_str().unwrap())
        else {
            panic!("window must be sealed");
        };
        assert_eq!(
            output,
            vec![json!({"type": "compaction", "id": "cmp_1", "encrypted_content": "enc"})]
        );
    }

    #[test]
    fn compaction_delivery_encodes_ir_state_in_both_protocols() {
        let message = anthropic_message();
        let canonical =
            compaction_response_ir(parse_anthropic_compaction(message.clone()).unwrap(), "m")
                .unwrap();
        let transform = crate::transform::ProtocolTransform::global();
        let responses = transform
            .bind(OPEN_RESPONSES_2026_04_24, ANTHROPIC_MESSAGES_2023_06_01)
            .unwrap()
            .encode_response(&canonical)
            .unwrap();
        assert_eq!(responses["object"], "response");
        assert_eq!(responses["status"], "completed");
        let Some(Envelope::Anthropic { block }) = open(
            responses["output"][0]["encrypted_content"]
                .as_str()
                .unwrap(),
        ) else {
            panic!("signed native block must survive IR delivery");
        };
        assert_eq!(block, message["content"][0]);
        let native = transform
            .bind(ANTHROPIC_MESSAGES_2023_06_01, ANTHROPIC_MESSAGES_2023_06_01)
            .unwrap()
            .encode_response(&canonical)
            .unwrap();
        assert_eq!(native["content"], message["content"]);
        assert_eq!(native["usage"], message["usage"]);
        assert_eq!(native["stop_reason"], "compaction");
    }

    #[test]
    fn standalone_defaults_do_not_override_session_or_summary_instructions() {
        for session in [None, Some("Follow repository policy")] {
            for summary in [None, Some("Preserve pending questions")] {
                let mut request = AiRequest::new(
                    "claude-x",
                    vec![AiItem {
                        role: Role::User,
                        content: MessageContent::Text("Task history".to_owned().into()),
                        tool_calls: None,
                        tool_call_id: None,
                        meta: None,
                    }],
                );
                request.instructions = session.map(str::to_owned);
                request.parameters.compaction = Some(CompactionControl {
                    threshold_tokens: None,
                    instructions: summary.map(str::to_owned),
                });
                let control = take_anthropic_compaction_control(&mut request);
                let mut body = crate::codec::anthropic::messages::encoder::AnthropicEncoder
                    .encode_request(request)
                    .unwrap()
                    .0;
                prepare_anthropic_compaction_body(&mut body, Some(control)).unwrap();
                assert_eq!(
                    body["system"],
                    session.unwrap_or(DEFAULT_COMPACTION_INSTRUCTIONS)
                );
                assert_eq!(
                    body["compaction"]["instructions"],
                    summary.unwrap_or(DEFAULT_COMPACTION_INSTRUCTIONS)
                );
            }
        }
    }

    #[test]
    fn responses_compaction_keeps_session_and_summary_intent_without_inline_control() {
        let pair = crate::transform::ProtocolTransform::global()
            .bind(ANTHROPIC_MESSAGES_2023_06_01, OPEN_RESPONSES_2026_04_24)
            .unwrap();
        let mut request = pair
            .decode_request(json!({
                "model": "model", "system": "Follow repository policy",
                "messages": [{"role": "user", "content": "Find the regression"}],
                "compaction": {"type": "summarize", "instructions": "Preserve pending questions"},
            }))
            .unwrap();
        prepare_responses_compaction_request(&mut request);
        let body = pair.encode_request(request).unwrap().body;
        let instructions = body["instructions"].as_str().unwrap();
        assert!(instructions.contains("Follow repository policy"));
        assert!(instructions.contains("Preserve pending questions"));
        assert!(body.get("context_management").is_none());
        assert_eq!(
            body["input"].as_array().unwrap().last().unwrap()["type"],
            "compaction_trigger"
        );
    }

    fn sse(event: &str, data: Value) -> String {
        format!("event: {event}\ndata: {data}\n\n")
    }

    #[test]
    fn native_compaction_stream_preserves_the_full_signed_block_and_iteration_usage() {
        let block = json!({
            "type": "compaction",
            "content": "Summary of the work",
            "signature": "sig-stream",
            "tool_changes": [{"name": "read", "opaque": ["unchanged"]}],
            "opaque_future": {"keep": true}
        });
        let body = [
            sse(
                "message_start",
                json!({
                    "type": "message_start",
                    "message": {
                        "id": "msg_compact",
                        "type": "message",
                        "role": "assistant",
                        "model": "claude-x",
                        "content": [],
                        "stop_reason": null,
                        "stop_sequence": null,
                        "usage": {
                            "input_tokens": 0,
                            "output_tokens": 0,
                            "cache_read_input_tokens": 7
                        }
                    }
                }),
            ),
            sse(
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": block.clone()
                }),
            ),
            sse(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 0}),
            ),
            sse(
                "message_delta",
                json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": "compaction", "stop_sequence": null},
                    "usage": {
                        "input_tokens": 0,
                        "output_tokens": 0,
                        "iterations": [
                            {
                                "type": "compaction",
                                "input_tokens": 64,
                                "output_tokens": 2002,
                                "cache_creation_input_tokens": 80082
                            },
                            {"type": "message", "input_tokens": 5, "output_tokens": 1}
                        ],
                        "opaque_usage": {"keep": true}
                    }
                }),
            ),
            sse("message_stop", json!({"type": "message_stop"})),
        ]
        .concat();

        let message = fold_anthropic_compaction_stream(body.as_bytes()).unwrap();
        assert_eq!(message["content"], json!([block.clone()]));
        assert_eq!(message["stop_reason"], "compaction");
        assert_eq!(message["usage"]["cache_read_input_tokens"], 7);
        assert_eq!(message["usage"]["iterations"].as_array().unwrap().len(), 2);
        assert_eq!(message["usage"]["opaque_usage"]["keep"], true);

        let resource = responses_result(message.clone());
        let Some(Envelope::Anthropic { block: sealed }) =
            open(resource["output"][0]["encrypted_content"].as_str().unwrap())
        else {
            panic!("native stream signature state must be sealed");
        };
        assert_eq!(sealed["tool_changes"], block["tool_changes"]);
        assert_eq!(sealed["opaque_future"], block["opaque_future"]);
        assert_eq!(resource["usage"]["input_tokens"], 64);
        assert_eq!(resource["usage"]["output_tokens"], 2002);

        // 末尾事件可以省略空行；只要 message_stop 完整仍可解析。
        let message = fold_anthropic_compaction_stream(body.trim_end().as_bytes()).unwrap();
        assert_eq!(message["content"][0]["signature"], "sig-stream");
        // 标准 SSE 也允许 CRLF 行尾。
        let message =
            fold_anthropic_compaction_stream(body.replace('\n', "\r\n").as_bytes()).unwrap();
        assert_eq!(message["content"][0]["signature"], "sig-stream");
    }

    #[test]
    fn native_compaction_stream_rejects_truncation_stream_errors_and_other_stops() {
        let start = sse(
            "message_start",
            json!({
                "type": "message_start",
                "message": {"id": "msg_compact", "model": "claude-x", "content": []}
            }),
        );
        let block = sse(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {
                    "type": "compaction",
                    "content": "Summary",
                    "signature": "sig-stream"
                }
            }),
        );
        let stop = sse("message_stop", json!({"type": "message_stop"}));
        for (body, expected) in [
            (format!("{start}{block}"), "message_stop"),
            (
                format!(
                    "{start}{block}{}{stop}",
                    sse(
                        "error",
                        json!({
                            "type": "error",
                            "error": {"type": "overloaded_error", "message": "Overloaded"}
                        }),
                    ),
                ),
                "Overloaded",
            ),
            (
                format!(
                    "{start}{block}{}{stop}",
                    sse(
                        "message_delta",
                        json!({
                            "type": "message_delta",
                            "delta": {"stop_reason": "end_turn"}
                        }),
                    ),
                ),
                "end_turn",
            ),
            (format!("{start}data: {{not-json\n\n"), "invalid JSON"),
        ] {
            let error = fold_anthropic_compaction_stream(body.as_bytes()).unwrap_err();
            assert!(
                error.to_string().contains(expected),
                "expected {expected:?}, got {error}"
            );
        }
    }

    #[test]
    fn upstream_anthropic_state_registers_the_item_a_responses_client_replays() {
        use stravia_runtime_contract::protocol::ir::canonical::native_compaction_item;
        let registered = parse_anthropic_compaction(anthropic_message()).unwrap();
        let resource = responses_result(anthropic_message());
        let replayed =
            crate::codec::open_responses::decoder::decode_input_item(&resource["output"][0])
                .unwrap()
                .unwrap();
        assert_eq!(
            native_compaction_item(&registered.items[0]),
            native_compaction_item(&replayed),
            "the item registered at compaction time must fingerprint like the replayed one"
        );
        assert_eq!(registered.wire["type"], "message");
    }

    #[test]
    fn compaction_request_streams_and_drops_controls_the_api_rejects() {
        let mut body = json!({
            "model": "claude-x",
            "max_tokens": 1000,
            "stream": false,
            "system": [{"type": "text", "text": "stable system"}],
            "tools": [{"name": "read", "input_schema": {"type": "object"}}],
            "thinking": {"type": "enabled", "budget_tokens": 1024},
            "stop_sequences": ["x"],
            "tool_choice": {"type": "any"},
            "context_management": {"edits": []},
            "output_config": {
                "format": {"type": "json_schema"},
                "effort": "high",
                "task_budget": {"type": "tokens", "total": 64000, "remaining": 48000}
            },
            "messages": []
        });
        prepare_anthropic_compaction_body(
            &mut body,
            Some(CompactionControl {
                instructions: Some("Be brief".into()),
                ..Default::default()
            }),
        )
        .unwrap();
        for key in ["stop_sequences", "tool_choice", "context_management"] {
            assert!(body.get(key).is_none(), "{key} must be stripped");
        }
        assert_eq!(
            body["stream"],
            json!(true),
            "native compaction always streams"
        );
        assert_eq!(
            body["output_config"],
            json!({
                "effort": "high",
                "task_budget": {"type": "tokens", "total": 64000}
            }),
            "format/remaining conflict with compaction; effort/total do not"
        );
        assert_eq!(
            body["compaction"],
            json!({"type": "summarize", "instructions": "Be brief"})
        );
        // 模型、system、tools、thinking 继续随压缩请求发送。
        assert_eq!(body["system"][0]["text"], "stable system");
        assert_eq!(body["tools"][0]["name"], "read");
        assert_eq!(body["thinking"]["type"], "enabled");

        let mut without_stream = json!({
            "model": "claude-x",
            "max_tokens": 1000,
            "messages": []
        });
        prepare_anthropic_compaction_body(&mut without_stream, None).unwrap();
        assert_eq!(without_stream["stream"], json!(true));
        assert_eq!(without_stream["compaction"]["type"], "summarize");
        assert_eq!(
            without_stream["compaction"]["instructions"],
            DEFAULT_COMPACTION_INSTRUCTIONS
        );
        assert!(without_stream.get("output_config").is_none());

        let mut format_only = json!({
            "output_config": {"format": {"type": "json_schema"}},
            "messages": []
        });
        prepare_anthropic_compaction_body(&mut format_only, None).unwrap();
        assert!(
            format_only.get("output_config").is_none(),
            "an output_config left empty by removing format is dropped"
        );

        let mut headers = vec![(
            "Anthropic-Beta".to_owned(),
            "prompt-caching-2024-07-31".to_owned(),
        )];
        add_anthropic_compaction_beta(&mut headers);
        add_anthropic_compaction_beta(&mut headers);
        assert_eq!(
            headers,
            [(
                "Anthropic-Beta".to_owned(),
                format!("prompt-caching-2024-07-31,{ANTHROPIC_COMPACTION_BETA}")
            )]
        );
    }
}
