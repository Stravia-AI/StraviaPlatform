//! 所有 Responses V2 压缩出口共用的 SSE 解码器。
//!
//! 上游返回普通 `POST /responses` 流式响应，而不是 `response.compaction`
//! 资源。SSE 帧只解析一次：每个 payload 直接交给标准 Responses 流解码器取得
//! typed IR，同时从同一批 payload 读取终端资源身份与逐项交付的原生状态。
//! 只接受明确完成的终端事件与恰好一个带非空密文的 compaction 状态。

use serde_json::Value;

use crate::accumulator::StreamResponseAccumulator;
use crate::codec::open_responses::parser::ResponsesStreamParser;
use stravia_runtime_contract::protocol::ir::{AiStreamDelta, NativeCompactionResponse};

/// 把一段完整的 V2 压缩 SSE 解码为唯一规范状态、实际用量与终端资源身份。
pub fn parse_responses_compaction_stream(body: &[u8]) -> anyhow::Result<NativeCompactionResponse> {
    let text = std::str::from_utf8(body)?;
    let mut decoder = ResponsesStreamParser::new();
    let mut accumulator = StreamResponseAccumulator::default();
    let mut terminal = None;
    let mut completed = false;
    let mut buffer = String::new();
    let mut consume = |frame: &str| -> anyhow::Result<()> {
        let mut event = None;
        let mut data: Option<std::borrow::Cow<'_, str>> = None;
        for line in frame.lines() {
            if let Some(name) = line.strip_prefix("event:") {
                event = Some(name.trim());
            } else if let Some(part) = line.strip_prefix("data:") {
                let part = part.strip_prefix(' ').unwrap_or(part);
                match &mut data {
                    None => data = Some(std::borrow::Cow::Borrowed(part)),
                    Some(data) => {
                        data.to_mut().push('\n');
                        data.to_mut().push_str(part);
                    }
                }
            }
        }
        let Some(data) = data else {
            return Ok(());
        };
        if data.trim() == "[DONE]" {
            anyhow::ensure!(
                completed,
                "Responses compaction ended before a completed terminal response"
            );
            return Ok(());
        }
        let mut payload: Value = serde_json::from_str(&data)?;
        anyhow::ensure!(
            event.is_some() && event == payload.get("type").and_then(Value::as_str),
            "Responses compaction SSE event name does not match its body"
        );
        for delta in decoder.parse_websocket_event(&payload)? {
            if let AiStreamDelta::StreamError { error } = delta {
                return Err(anyhow::Error::new(error));
            }
            match &delta {
                AiStreamDelta::UnexpectedEof => {
                    anyhow::bail!("Responses compaction stream ended unexpectedly")
                }
                AiStreamDelta::Done { stop_reason } => {
                    anyhow::ensure!(
                        stop_reason == "stop",
                        "Responses compaction was not completed: {stop_reason}"
                    );
                    completed = true;
                }
                AiStreamDelta::ResponseTerminal { status, .. } if status != "completed" => {
                    return Err(anyhow::Error::new(
                        stravia_runtime_contract::protocol::ir::AiError::new(
                            stravia_runtime_contract::protocol::ir::AiErrorKind::ServerError,
                            format!("Responses compaction response status is {status}"),
                        )
                        .with_raw(payload),
                    ));
                }
                _ => {}
            }
            accumulator.apply_with_identity(&delta);
        }
        if event == Some("response.completed") {
            terminal = payload.get_mut("response").map(Value::take);
        }
        Ok(())
    };
    crate::consume_sse_frames(&mut buffer, text, &mut consume)?;
    if !buffer.trim().is_empty() {
        consume(&buffer)?;
    }
    for delta in decoder.finish()? {
        if matches!(delta, AiStreamDelta::UnexpectedEof) {
            anyhow::bail!("Responses compaction stream ended unexpectedly");
        }
        accumulator.apply_with_identity(&delta);
    }
    anyhow::ensure!(
        completed,
        "Responses compaction stream ended without a completed terminal response"
    );
    let response = accumulator.into_ai_response();
    anyhow::ensure!(
        response.items.len() == 1 && response.items[0].is_compaction(),
        "Responses compaction requires exactly one native state"
    );
    let stravia_runtime_contract::protocol::ir::MessageContent::Blocks(blocks) =
        &response.items[0].content
    else {
        unreachable!("canonical compaction item");
    };
    let [
        stravia_runtime_contract::protocol::ir::ContentBlock::Compaction {
            encrypted_content, ..
        },
    ] = blocks.as_slice()
    else {
        unreachable!("canonical compaction item");
    };
    anyhow::ensure!(
        !encrypted_content.is_empty(),
        "Responses compaction state has empty encrypted_content"
    );
    Ok(NativeCompactionResponse {
        wire: terminal.ok_or_else(|| {
            anyhow::anyhow!("Responses compaction stream has no terminal response")
        })?,
        items: response.items,
        usage: response
            .usage
            .required_components_known
            .then_some(response.usage),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use stravia_runtime_contract::protocol::ir::AiError;

    fn resource(status: &str, output: Value, id: &str) -> Value {
        json!({
            "id": id, "object": "response", "created_at": 1790129800,
            "completed_at": 1790129810, "status": status, "model": "gpt-6-astra",
            "incomplete_details": null, "previous_response_id": null, "instructions": null,
            "output": output, "error": null, "tools": [], "tool_choice": "auto",
            "truncation": "disabled", "parallel_tool_calls": true,
            "text": {"format": {"type": "text"}}, "top_p": null,
            "presence_penalty": null, "frequency_penalty": null, "top_logprobs": null,
            "temperature": null, "reasoning": {"effort": "high", "summary": "auto"},
            "usage": {"input_tokens": 9, "output_tokens": 2, "total_tokens": 11,
                "input_tokens_details": {"cached_tokens": 0},
                "output_tokens_details": {"reasoning_tokens": 0}},
            "max_output_tokens": null, "max_tool_calls": null, "store": false,
            "background": false, "service_tier": "default", "metadata": {},
            "safety_identifier": null, "prompt_cache_key": null
        })
    }

    fn event(name: &str, data: Value) -> String {
        format!("event: {name}\ndata: {data}\n\n")
    }

    fn state() -> Value {
        json!({"type": "compaction", "id": "cmp_1", "encrypted_content": "cipher"})
    }

    fn success(terminal_output: Value, done_items: &[Value]) -> String {
        let mut stream = event(
            "response.created",
            json!({"type": "response.created", "sequence_number": 0,
                "response": resource("in_progress", json!([]), "resp_cmp")}),
        );
        for (index, item) in done_items.iter().enumerate() {
            let sequence = index * 2 + 1;
            stream.push_str(&event(
                "response.output_item.added",
                json!({"type": "response.output_item.added", "sequence_number": sequence,
                    "output_index": index, "item": {"type": item["type"], "id": item["id"]}}),
            ));
            stream.push_str(&event(
                "response.output_item.done",
                json!({"type": "response.output_item.done", "sequence_number": sequence + 1,
                    "output_index": index, "item": item}),
            ));
        }
        let terminal_sequence = done_items.len() * 2 + 1;
        stream.push_str(&event(
            "response.completed",
            json!({"type": "response.completed", "sequence_number": terminal_sequence,
                "response": resource("completed", terminal_output, "resp_cmp")}),
        ));
        stream
    }

    #[test]
    fn item_done_state_is_kept_when_terminal_output_is_empty() {
        let result =
            parse_responses_compaction_stream(success(json!([]), &[state()]).as_bytes()).unwrap();
        assert_eq!(result.wire["id"], "resp_cmp");
        assert_eq!(result.wire["created_at"], 1790129800);
        assert_eq!(result.wire["output"], json!([]));
        assert_eq!(
            stravia_runtime_contract::protocol::ir::canonical::native_compaction_item(
                &result.items[0]
            ),
            Some(state())
        );
        assert_eq!(result.usage.unwrap().total_tokens, 11);
    }

    #[test]
    fn terminal_completed_output_is_the_compaction_state_carrier() {
        let result =
            parse_responses_compaction_stream(success(json!([state()]), &[]).as_bytes()).unwrap();
        assert_eq!(result.items.len(), 1);
        assert!(result.items[0].is_compaction());
    }

    #[test]
    fn matching_item_done_and_terminal_output_are_accepted() {
        let result =
            parse_responses_compaction_stream(success(json!([state()]), &[state()]).as_bytes())
                .unwrap();
        assert_eq!(result.items.len(), 1);
    }

    #[test]
    fn duplicate_mismatched_unrelated_or_empty_states_are_rejected() {
        assert!(
            parse_responses_compaction_stream(success(json!([]), &[state(), state()]).as_bytes())
                .is_err()
        );
        let mut changed = state();
        changed["encrypted_content"] = "other".into();
        assert!(
            parse_responses_compaction_stream(success(json!([changed]), &[state()]).as_bytes())
                .is_err()
        );
        assert!(
            parse_responses_compaction_stream(
                success(json!([]), &[json!({"type": "compaction_trigger"})]).as_bytes()
            )
            .is_err()
        );
        assert!(
            parse_responses_compaction_stream(
                success(json!([]), &[json!({"type": "compaction"})]).as_bytes()
            )
            .is_err()
        );
        assert!(
            parse_responses_compaction_stream(
                success(
                    json!([]),
                    &[json!({
                        "type": "compaction", "encrypted_content": ""
                    })]
                )
                .as_bytes()
            )
            .is_err()
        );
    }

    #[test]
    fn incomplete_error_and_truncated_streams_are_rejected() {
        let incomplete = success(json!([state()]), &[])
            .replace("event: response.completed", "event: response.incomplete")
            .replace(
                "\"type\":\"response.completed\"",
                "\"type\":\"response.incomplete\"",
            )
            .replace("\"status\":\"completed\"", "\"status\":\"incomplete\"");
        assert!(parse_responses_compaction_stream(incomplete.as_bytes()).is_err());
        let error = event(
            "error",
            json!({"type": "error", "sequence_number": 0,
                "error": {"message": "refused"}}),
        );
        let mut with_error = success(json!([state()]), &[]);
        with_error.insert_str(0, &error);
        let failure = parse_responses_compaction_stream(with_error.as_bytes()).unwrap_err();
        let ai_error = failure.downcast_ref::<AiError>().expect("typed AiError");
        assert_eq!(ai_error.raw, Some(json!({"error": {"message": "refused"}})));
        assert!(
            parse_responses_compaction_stream(
                event(
                    "response.created",
                    json!({"type": "response.created", "sequence_number": 0,
                    "response": resource("in_progress", json!([]), "resp_cmp")}),
                )
                .as_bytes()
            )
            .is_err()
        );
    }
}
