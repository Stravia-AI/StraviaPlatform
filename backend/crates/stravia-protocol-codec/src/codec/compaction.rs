//! Native compaction control classification on a decoded canonical request.
//!
//! Moved out of `stravia-core::compaction`: the persisted store stays host-side
//! while request classification is pure wire semantics shared with plugins.
//!
//! It also owns the stateless envelope that lets a client speaking one protocol carry
//! another protocol's native compaction state in its own opaque field.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};

use stravia_runtime_contract::protocol::ir::{AiItem, AiRequest};

/// Marks an opaque field value as a Stravia envelope. Native Anthropic signatures and
/// OpenAI `encrypted_content` never start with it, so unwrapped state is never misread.
const ENVELOPE_PREFIX: &str = "stravia.compaction.v1.";

/// Placeholder for the readable `content` of an Anthropic block that carries Responses
/// state. The summary is opaque to Stravia; the client must not treat this as one.
pub const ENVELOPE_PLACEHOLDER_CONTENT: &str = "[Stravia remote compaction: opaque state]";

/// Item metadata key holding the complete Anthropic `compaction` block, so replay is
/// byte-faithful (the API rejects any edited signature, content or stray field).
pub const ANTHROPIC_COMPACTION_BLOCK_META: &str = "__anthropic_compaction_block";

/// Ingress bag key carrying the client's top-level Anthropic `compaction` request
/// object (`{type, instructions?}`), present only on standalone compaction requests.
pub const ANTHROPIC_COMPACTION_REQUEST_META: &str = "__anthropic_compaction_request";

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

/// Anthropic on-demand compaction Message → Responses `response.compaction` resource.
///
/// Identity is derived from the signature, so the vendor that registers the state and
/// the ingress that later answers the client agree on the same item without sharing
/// anything but the upstream body.
pub fn responses_compaction_from_anthropic(message: &Value) -> anyhow::Result<Value> {
    let block = message
        .get("content")
        .and_then(Value::as_array)
        .and_then(|content| {
            content
                .iter()
                .find(|block| block.get("type").and_then(Value::as_str) == Some("compaction"))
        })
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
    let seed = block["signature"].as_str().unwrap_or_default();
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
                        input + iteration["input_tokens"].as_u64().unwrap_or(0),
                        output + iteration["output_tokens"].as_u64().unwrap_or(0),
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
    Ok(json!({
        "id": format!("resp_cmp_{}", short_hash(seed)),
        "object": "response.compaction",
        "created_at": chrono::Utc::now().timestamp(),
        "output": [{
            "type": "compaction",
            "id": anthropic_state_id(seed),
            "encrypted_content": seal_anthropic(block),
        }],
        "usage": {
            "input_tokens": input,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens": output,
            "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": input + output,
        },
    }))
}

/// Responses `response.compaction` resource → Anthropic Message with one sealed block.
///
/// The whole replacement window travels in the signature; the readable `content` is a
/// fixed marker because a Responses window has no summary text to show.
pub fn anthropic_message_from_responses_compaction(
    resource: &Value,
    model: &str,
) -> anyhow::Result<Value> {
    let output = resource
        .get("output")
        .and_then(Value::as_array)
        .filter(|output| {
            output
                .iter()
                .any(|item| item.get("type").and_then(Value::as_str) == Some("compaction"))
        })
        .ok_or_else(|| anyhow::anyhow!("compact resource has no native compaction state"))?;
    let signature = seal_responses(output);
    let (input, output_tokens) = (
        resource
            .pointer("/usage/input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        resource
            .pointer("/usage/output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    );
    Ok(json!({
        "id": format!("msg_cmp_{}", short_hash(&signature)),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": [{
            "type": "compaction",
            "content": ENVELOPE_PLACEHOLDER_CONTENT,
            "signature": signature,
        }],
        "stop_reason": "compaction",
        "stop_sequence": null,
        "usage": {
            "input_tokens": 0,
            "output_tokens": 0,
            "iterations": [{
                "type": "compaction",
                "input_tokens": input,
                "output_tokens": output_tokens,
            }],
        },
    }))
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

/// Native standalone compaction result from an Anthropic upstream.
///
/// `items` are decoded from the same Responses resource the Responses client will be
/// shown, so registration and later replay see identical state. `wire` keeps the
/// upstream body so an Anthropic client can be answered without translation.
pub fn parse_anthropic_compaction(
    message: Value,
) -> anyhow::Result<stravia_runtime_contract::protocol::ir::NativeCompactionResponse> {
    let resource = responses_compaction_from_anthropic(&message)?;
    let mut response = crate::codec::open_responses::parser::parse_compaction_response(&resource)?;
    response.wire = message;
    Ok(response)
}

/// The compact resource the ingress client expects, whatever protocol the upstream spoke.
///
/// `wire` identifies itself (`response.compaction` vs `message`); a matching ingress
/// gets the upstream body byte-for-byte.
pub fn client_compaction_wire(
    ingress: stravia_runtime_contract::protocol::ids::ProtocolEndpoint,
    wire: &Value,
    model: &str,
) -> anyhow::Result<Value> {
    use stravia_runtime_contract::protocol::ids::{
        ANTHROPIC_MESSAGES_2023_06_01, OPEN_RESPONSES_2026_04_24,
    };
    let from_responses = wire.get("object").and_then(Value::as_str) == Some("response.compaction");
    let from_anthropic = wire.get("type").and_then(Value::as_str) == Some("message");
    if ingress == OPEN_RESPONSES_2026_04_24 && from_anthropic {
        responses_compaction_from_anthropic(wire)
    } else if ingress == ANTHROPIC_MESSAGES_2023_06_01 && from_responses {
        anthropic_message_from_responses_compaction(wire, model)
    } else {
        Ok(wire.clone())
    }
}

/// Remove and return the client's Anthropic `compaction` control, if the request carried
/// one. Requests that came from another protocol have none; the API then summarizes with
/// its default instructions.
pub fn take_anthropic_compaction_control(request: &mut AiRequest) -> Option<Value> {
    request
        .meta
        .vendor
        .ingress
        .remove(ANTHROPIC_COMPACTION_REQUEST_META)
}

/// Turn an encoded Anthropic Messages body into a standalone compaction request.
///
/// The API rejects `compaction` together with `context_management`, `stop_sequences`,
/// `output_config.format` and forced `tool_choice`, and a compaction result is only
/// meaningful as a unary message, so those are stripped rather than forwarded to a
/// guaranteed 400. Everything else (system, tools, thinking) stays as encoded.
pub fn prepare_anthropic_compaction_body(
    body: &mut Value,
    control: Option<Value>,
) -> anyhow::Result<()> {
    let object = body
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Anthropic compaction request body must be an object"))?;
    for key in [
        "stream",
        "stop_sequences",
        "tool_choice",
        "context_management",
    ] {
        object.remove(key);
    }
    if let Some(config) = object
        .get_mut("output_config")
        .and_then(Value::as_object_mut)
    {
        config.remove("format");
        if config.is_empty() {
            object.remove("output_config");
        }
    }
    let control = match control {
        Some(Value::Object(mut control)) => {
            control
                .entry("type")
                .or_insert_with(|| Value::String("summarize".into()));
            Value::Object(control)
        }
        Some(_) => anyhow::bail!("Anthropic `compaction` must be an object"),
        None => json!({"type": "summarize"}),
    };
    object.insert("compaction".into(), control);
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
            request.parameters.compaction = Some(CompactionControl { threshold_tokens });
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
        let resource = responses_compaction_from_anthropic(&anthropic_message()).unwrap();
        assert_eq!(resource["object"], "response.compaction");
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
        let error = responses_compaction_from_anthropic(&message).unwrap_err();
        assert!(error.to_string().contains("max_tokens"), "{error}");
    }

    #[test]
    fn responses_window_travels_inside_one_anthropic_block() {
        let resource = json!({
            "id": "resp_1",
            "object": "response.compaction",
            "output": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                {"type": "compaction", "id": "cmp_1", "encrypted_content": "enc"}
            ],
            "usage": {"input_tokens": 10, "output_tokens": 4}
        });
        let message = anthropic_message_from_responses_compaction(&resource, "claude-x").unwrap();
        assert_eq!(message["stop_reason"], "compaction");
        assert_eq!(message["usage"]["iterations"][0]["input_tokens"], 10);
        let block = &message["content"][0];
        assert_eq!(block["content"], ENVELOPE_PLACEHOLDER_CONTENT);
        let Some(Envelope::Responses { output }) = open(block["signature"].as_str().unwrap())
        else {
            panic!("window must be sealed");
        };
        assert_eq!(output.len(), 2);
    }

    #[test]
    fn client_wire_is_translated_only_across_protocols() {
        let message = anthropic_message();
        let resource = client_compaction_wire(OPEN_RESPONSES_2026_04_24, &message, "m").unwrap();
        assert_eq!(resource["object"], "response.compaction");
        assert_eq!(
            client_compaction_wire(ANTHROPIC_MESSAGES_2023_06_01, &message, "m").unwrap(),
            message
        );
        assert_eq!(
            client_compaction_wire(OPEN_RESPONSES_2026_04_24, &resource, "m").unwrap(),
            resource
        );
        let back = client_compaction_wire(ANTHROPIC_MESSAGES_2023_06_01, &resource, "m").unwrap();
        assert_eq!(back["type"], "message");
        assert_eq!(back["content"][0]["type"], "compaction");
    }

    #[test]
    fn upstream_anthropic_state_registers_the_item_a_responses_client_replays() {
        use stravia_runtime_contract::protocol::ir::canonical::native_compaction_item;
        let registered = parse_anthropic_compaction(anthropic_message()).unwrap();
        let resource = responses_compaction_from_anthropic(&anthropic_message()).unwrap();
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
    fn compaction_request_drops_controls_the_api_rejects_and_keeps_existing_betas() {
        let mut body = json!({
            "model": "claude-x",
            "max_tokens": 1000,
            "stream": true,
            "stop_sequences": ["x"],
            "tool_choice": {"type": "any"},
            "context_management": {"edits": []},
            "output_config": {"format": {"type": "json_schema"}},
            "messages": []
        });
        prepare_anthropic_compaction_body(&mut body, Some(json!({"instructions": "Be brief"})))
            .unwrap();
        for key in [
            "stream",
            "stop_sequences",
            "tool_choice",
            "context_management",
            "output_config",
        ] {
            assert!(body.get(key).is_none(), "{key} must be stripped");
        }
        assert_eq!(
            body["compaction"],
            json!({"type": "summarize", "instructions": "Be brief"})
        );
        prepare_anthropic_compaction_body(&mut body, None).unwrap();
        assert_eq!(body["compaction"], json!({"type": "summarize"}));

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
