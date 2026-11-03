//! Codex 独立压缩。
//!
//! 官方客户端（openai/codex `compact_remote_v2`）已不再调用 `/responses/compact`：
//! 它在普通流式 `/responses` 请求的末尾追加 `compaction_trigger` item，从
//! `response.output_item.done` 取唯一的 compaction item，并由客户端自己拼装新窗口
//! （保留的用户消息 + compaction item）。ChatGPT 后端对 `/responses/compact` 返回 404。
//! 本模块把这套交互折叠回宿主期望的 `response.compaction` 资源，窗口拼装规则对齐官方客户端。

use serde_json::{Value, json};
use stravia_protocol_codec::codec::open_responses::parser::parse_compaction_response;
use stravia_runtime_contract::protocol::ir::{AiErrorKind, NativeCompactionResponse};
use stravia_vendor_common::common::model_error;
use stravia_vendor_sdk::{HttpResponse, PluginError, read_http_body};

/// compaction item 为不透明密文，单个窗口的整段 SSE 上限与宿主 unary 上限一致。
const MAX_COMPACTION_BODY: usize = 32 * 1024 * 1024;
/// 官方 `RETAINED_MESSAGE_TOKEN_BUDGET`：压缩后原样保留的用户消息预算。
const RETAINED_TOKEN_BUDGET: usize = 64_000;
const TRIGGER_TYPE: &str = "compaction_trigger";

/// 把触发器追加为最后一个 input item，并返回压缩后窗口里需要原样保留的用户消息。
pub(super) fn arm(body: &mut Value) -> Result<Vec<Value>, PluginError> {
    let input = body
        .get_mut("input")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| crate::invalid("Codex compaction requires an item list as input"))?;
    let retained = retained_user_messages(input);
    if input.last().and_then(|item| item["type"].as_str()) != Some(TRIGGER_TYPE) {
        input.push(json!({"type": TRIGGER_TYPE}));
    }
    Ok(retained)
}

/// 读取流式响应并折叠成 `response.compaction` 资源。非成功状态由调用方的错误保留包装处理。
pub(super) fn decode(
    response: HttpResponse,
    retained: Vec<Value>,
) -> Result<NativeCompactionResponse, PluginError> {
    let bytes = read_http_body(&response, MAX_COMPACTION_BODY)?;
    let (done_items, completed) = crate::codex::scan_responses_body(&bytes)?;
    let resource = window_resource(done_items, &completed, retained)?;
    parse_compaction_response(&resource).map_err(|error| {
        model_error(
            AiErrorKind::ServerError,
            format!("invalid Codex compaction response: {error}"),
        )
    })
}

fn window_resource(
    done_items: Vec<Value>,
    completed: &Value,
    mut window: Vec<Value>,
) -> Result<Value, PluginError> {
    // 官方只信 output_item.done；终态 output 只在没有逐项事件（JSON 体）时兜底。
    let from_completed = || {
        completed
            .get("output")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let mut states: Vec<Value> = done_items
        .into_iter()
        .filter(|item| item["type"] == "compaction")
        .collect();
    if states.is_empty() {
        states = from_completed()
            .into_iter()
            .filter(|item| item["type"] == "compaction")
            .collect();
    }
    if states.len() != 1 {
        return Err(model_error(
            AiErrorKind::ServerError,
            format!(
                "Codex compaction expected exactly one compaction item, got {}",
                states.len()
            ),
        ));
    }
    let id = completed
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            model_error(
                AiErrorKind::ServerError,
                "Codex compaction response has no id",
            )
        })?;
    let created_at = completed
        .get("created_at")
        .and_then(|value| value.as_i64().or_else(|| value.as_f64().map(|v| v as i64)))
        .unwrap_or_else(|| chrono::Utc::now().timestamp());
    window.extend(states);
    let mut resource = json!({
        "id": id,
        "object": "response.compaction",
        "created_at": created_at,
        "output": window,
    });
    if let Some(usage) = completed.get("usage").filter(|usage| !usage.is_null()) {
        resource["usage"] = usage.clone();
    }
    Ok(resource)
}

/// 与官方一致：窗口只带用户消息，从最新往旧按预算取，边界消息截断文本；
/// 其余（助手输出、推理、工具调用、旧 compaction）由新的 compaction item 承载。
/// 图片与音频不保留：它们不计入文本预算，原样保留会让压缩后的窗口无界增长。
fn retained_user_messages(input: &[Value]) -> Vec<Value> {
    let mut remaining = RETAINED_TOKEN_BUDGET;
    let mut kept = Vec::new();
    for item in input.iter().rev() {
        if remaining == 0 {
            break;
        }
        if let Some((message, used)) = fit_user_message(item, remaining) {
            remaining = remaining.saturating_sub(used);
            kept.push(message);
        }
    }
    kept.reverse();
    kept
}

fn fit_user_message(item: &Value, budget: usize) -> Option<(Value, usize)> {
    if item["role"] != "user" || item.get("type").is_some_and(|kind| kind != "message") {
        return None;
    }
    let mut parts: Vec<Value> = match item.get("content")? {
        Value::String(text) => vec![json!({"type": "input_text", "text": text})],
        Value::Array(parts) => parts.clone(),
        _ => return None,
    };
    let mut left = budget;
    parts.retain_mut(|part| {
        if !matches!(part["type"].as_str(), Some("input_text" | "text")) {
            return false;
        }
        let Some(text) = part["text"].as_str() else {
            return false;
        };
        if left == 0 {
            return false;
        }
        let tokens = approx_tokens(text);
        if tokens <= left {
            left -= tokens;
        } else {
            part["text"] = truncate_to_tokens(text, left).into();
            left = 0;
        }
        !part["text"].as_str().unwrap_or_default().is_empty()
    });
    if parts.is_empty() {
        return None;
    }
    let mut message = item.clone();
    message["content"] = Value::Array(parts);
    Some((message, budget - left))
}

/// 官方 `approx_token_count`：4 字节一个 token，向上取整。
fn approx_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

fn truncate_to_tokens(text: &str, tokens: usize) -> &str {
    let mut end = tokens.saturating_mul(4).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> Value {
        json!({"role": "user", "content": [{"type": "input_text", "text": text}]})
    }

    #[test]
    fn trigger_is_the_last_item_and_only_user_text_is_retained() {
        let mut body = json!({"model": "m", "input": [
            user("first"),
            {"type": "reasoning", "id": "r1", "summary": []},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "answer"}]},
            {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "rules"}]},
            {"type": "compaction", "encrypted_content": "old"},
            {"type": "function_call_output", "call_id": "c", "output": "x"},
            {"type": "message", "role": "user", "content": [
                {"type": "input_image", "image_url": "data:image/png;base64,AAAA"},
                {"type": "input_text", "text": "second"}
            ]},
        ]});
        let retained = arm(&mut body).unwrap();
        assert_eq!(
            retained,
            vec![
                user("first"),
                json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "second"}]}),
            ]
        );
        let input = body["input"].as_array().unwrap();
        assert_eq!(
            input.last().unwrap(),
            &json!({"type": "compaction_trigger"})
        );
        assert_eq!(input.len(), 8, "history must be sent unchanged");
    }

    #[test]
    fn an_existing_trailing_trigger_is_not_duplicated() {
        let mut body = json!({"input": [user("a"), {"type": "compaction_trigger"}]});
        arm(&mut body).unwrap();
        assert_eq!(body["input"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn non_list_input_is_rejected() {
        assert!(arm(&mut json!({"input": "text"})).is_err());
    }

    #[test]
    fn retention_keeps_the_newest_messages_and_truncates_the_boundary_on_char_boundaries() {
        let old = "o".repeat(RETAINED_TOKEN_BUDGET * 4);
        let boundary = "文".repeat(RETAINED_TOKEN_BUDGET * 2); // 3 字节/字，远超剩余预算
        let input = [user(&old), user(&boundary), user("newest")];
        let retained = retained_user_messages(&input);
        assert_eq!(retained.len(), 2, "oldest message no longer fits");
        assert_eq!(retained[1], user("newest"));
        let kept = retained[0]["content"][0]["text"].as_str().unwrap();
        assert!(kept.chars().all(|c| c == '文') && !kept.is_empty());
        let spent = approx_tokens(kept) + approx_tokens("newest");
        assert!(spent <= RETAINED_TOKEN_BUDGET, "{spent}");
    }

    #[test]
    fn window_is_retained_messages_then_the_single_compaction_item() {
        let state = json!({"type": "compaction", "id": "cmp_1", "encrypted_content": "cipher", "rolling_identity": {"version": 2}});
        let completed = json!({
            "id": "resp_1", "created_at": 1790129810,
            "output": [{"type": "message", "role": "assistant", "content": []}],
            "usage": {"input_tokens": 9, "output_tokens": 2, "total_tokens": 11,
                "input_tokens_details": {"cached_tokens": 0}, "output_tokens_details": {"reasoning_tokens": 0}},
        });
        let ignored = json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "ignored"}]});
        let resource =
            window_resource(vec![ignored, state.clone()], &completed, vec![user("kept")]).unwrap();
        assert_eq!(resource["object"], "response.compaction");
        assert_eq!(resource["id"], "resp_1");
        assert_eq!(resource["created_at"], 1790129810);
        assert_eq!(resource["output"], json!([user("kept"), state]));
        let parsed = parse_compaction_response(&resource).unwrap();
        assert!(parsed.items.iter().any(|item| item.is_compaction()));
        assert_eq!(parsed.usage.unwrap().total_tokens, 11);
    }

    #[test]
    fn compaction_item_falls_back_to_the_terminal_output_for_json_bodies() {
        let state = json!({"type": "compaction", "encrypted_content": "cipher"});
        let completed = json!({"id": "resp_1", "created_at": 1, "output": [state.clone()]});
        let resource = window_resource(Vec::new(), &completed, Vec::new()).unwrap();
        assert_eq!(resource["output"], json!([state]));
    }

    #[test]
    fn zero_or_several_compaction_items_are_rejected() {
        let completed = json!({"id": "resp_1", "created_at": 1, "output": []});
        assert!(window_resource(Vec::new(), &completed, Vec::new()).is_err());
        let state = json!({"type": "compaction", "encrypted_content": "cipher"});
        assert!(window_resource(vec![state.clone(), state], &completed, Vec::new()).is_err());
    }
}
