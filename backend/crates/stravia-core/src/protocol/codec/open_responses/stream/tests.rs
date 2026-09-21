use super::*;
use stravia_runtime_contract::protocol::ir::ContentBlock;
use stravia_runtime_contract::protocol::ir::MessageContent;

#[test]
fn stream_formatter_classifies_failures_without_leaking_diagnostics() {
    use stravia_runtime_contract::protocol::ir::{AiError, AiErrorKind};

    let sensitive_diagnostic = "transport to wss://internal.example reset with secret=token";
    for (error, expected_code) in [
        (
            AiError::new(AiErrorKind::ServerError, sensitive_diagnostic),
            "server_error",
        ),
        (
            AiError::new(AiErrorKind::StreamMidError, sensitive_diagnostic).with_raw(
                serde_json::json!({
                    "error": {
                        "type": "invalid_request_error",
                        "code": "invalid_request",
                        "message": sensitive_diagnostic,
                        "param": null
                    }
                }),
            ),
            "invalid_request",
        ),
        (
            AiError::new(AiErrorKind::AuthenticationError, sensitive_diagnostic),
            "authentication_error",
        ),
        (
            AiError::new(AiErrorKind::QuotaExceeded, sensitive_diagnostic).with_raw(
                serde_json::json!({
                    "error": {
                        "type": "rate_limit_error",
                        "code": "insufficient_quota",
                        "message": sensitive_diagnostic,
                        "param": null
                    }
                }),
            ),
            "quota_exceeded",
        ),
        (
            AiError::new(AiErrorKind::StreamMidError, sensitive_diagnostic).with_raw(
                serde_json::json!({
                    "error": {
                        "type": "rate_limit_error",
                        "code": "insufficient_quota",
                        "message": sensitive_diagnostic,
                        "param": null
                    }
                }),
            ),
            "quota_exceeded",
        ),
    ] {
        let mut formatter = ResponsesStreamFormatter::new();
        let mut events = formatter.format_deltas(&[AiStreamDelta::StreamError { error }]);
        events.extend(formatter.format_done());

        assert_eq!(events.len(), 3);
        assert_eq!(events[0].event.as_deref(), Some("error"));
        assert_eq!(events[1].event.as_deref(), Some("response.failed"));
        assert_eq!(events[2].event, None);
        assert_eq!(events[2].data, "[DONE]");
        let error: serde_json::Value = serde_json::from_str(&events[0].data).expect("error JSON");
        let failed: serde_json::Value = serde_json::from_str(&events[1].data).expect("failed JSON");
        assert_eq!(error["type"], "error");
        assert_eq!(error["error"]["type"], expected_code);
        assert_eq!(error["error"]["code"], expected_code);
        assert!(
            error.get("code").is_none(),
            "error payload must remain nested"
        );
        assert!(!error.to_string().contains(sensitive_diagnostic));
        assert_eq!(error["sequence_number"], 0);
        assert_eq!(failed["type"], "response.failed");
        assert_eq!(failed["response"]["status"], "failed");
        assert_eq!(failed["response"]["error"], error["error"]);
        assert_eq!(failed["sequence_number"], 1);
    }
}

#[test]
fn response_profile_uses_effective_request_and_provider_confirmed_values() {
    let mut request =
        stravia_runtime_contract::protocol::ir::AiRequest::new("logical-model", Vec::new());
    request.instructions = Some("Be concise.".into());
    request.generation.temperature = Some(0.2);
    request.ext = Some(
        stravia_runtime_contract::protocol::ir::ProtocolExt::OpenResponses(
            stravia_runtime_contract::protocol::ir::OpenResponsesExt {
                store: Some(false),
                metadata: Some(serde_json::json!({"tenant": "acme"})),
                safety_identifier: Some("safe-user".into()),
                ..Default::default()
            },
        ),
    );
    let mut formatter = ResponsesStreamFormatter::new();
    formatter.set_response_profile_from_request(&request, Some("resp_parent"));
    let events = formatter.format_deltas(&[
        AiStreamDelta::ResponseMetadata {
            metadata: serde_json::json!({"temperature": 0.7}),
        },
        AiStreamDelta::MessageStart {
            id: "resp_gateway".into(),
            model: "logical-model".into(),
        },
    ]);
    let created: serde_json::Value =
        serde_json::from_str(&events[0].data).expect("response.created JSON");
    let response = &created["response"];

    assert_eq!(response["previous_response_id"], "resp_parent");
    assert_eq!(response["instructions"], "Be concise.");
    assert_eq!(response["temperature"], 0.7);
    assert_eq!(response["store"], false);
    assert_eq!(response["metadata"]["tenant"], "acme");
    assert_eq!(response["safety_identifier"], "safe-user");
}

#[test]
fn stream_events_have_matching_names_and_strict_sequence_numbers() {
    let mut formatter = ResponsesStreamFormatter::new();
    let mut events = formatter.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "resp_gateway".into(),
            model: "logical-model".into(),
        },
        AiStreamDelta::TextDelta("hello".into()),
        AiStreamDelta::Done {
            stop_reason: "stop".into(),
        },
    ]);
    events.extend(formatter.format_done());

    let json_events = &events[..events.len() - 1];
    for (sequence, event) in json_events.iter().enumerate() {
        let body: serde_json::Value = serde_json::from_str(&event.data).expect("stream event JSON");
        assert_eq!(event.event.as_deref(), body["type"].as_str());
        assert_eq!(body["sequence_number"], sequence as u64);
    }
    assert_eq!(events.last().expect("DONE").data, "[DONE]");
}
#[test]
fn terminal_usage_distinguishes_known_zero_counts_from_missing_usage() {
    let mut known = ResponsesStreamFormatter::new();
    let events = known.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "resp-known".into(),
            model: "logical-model".into(),
        },
        AiStreamDelta::Usage(Usage {
            reasoning_tokens: Some(7),
            cache_read_tokens: Some(3),
            cache_creation_tokens: Some(5),
            required_components_known: true,
            ..Usage::default()
        }),
        AiStreamDelta::Done {
            stop_reason: "stop".into(),
        },
    ]);
    let completed = events
        .iter()
        .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
        .find(|body| body["type"] == "response.completed")
        .expect("known usage terminal");
    assert_eq!(completed["response"]["usage"]["input_tokens"], 0);
    assert_eq!(completed["response"]["usage"]["output_tokens"], 0);
    assert_eq!(completed["response"]["usage"]["total_tokens"], 0);
    assert_eq!(
        completed["response"]["usage"]["input_tokens_details"],
        serde_json::json!({"cached_tokens": 3, "cache_write_tokens": 5})
    );
    assert_eq!(
        completed["response"]["usage"]["output_tokens_details"]["reasoning_tokens"],
        7
    );

    let mut missing = ResponsesStreamFormatter::new();
    let events = missing.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "resp-missing".into(),
            model: "logical-model".into(),
        },
        AiStreamDelta::Done {
            stop_reason: "stop".into(),
        },
    ]);
    let completed = events
        .iter()
        .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
        .find(|body| body["type"] == "response.completed")
        .expect("missing usage terminal");
    assert!(completed["response"]["usage"].is_null());
}
#[test]
fn refusal_stream_emits_refusal_events_and_content() {
    let mut formatter = ResponsesStreamFormatter::new();
    let events = formatter.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "resp-refusal".into(),
            model: "logical-model".into(),
        },
        AiStreamDelta::RefusalDelta("cannot comply".into()),
        AiStreamDelta::Done {
            stop_reason: "stop".into(),
        },
    ]);
    let bodies = events
        .iter()
        .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
        .collect::<Vec<_>>();

    assert!(bodies.iter().any(|body| {
        body["type"] == "response.refusal.delta" && body["delta"] == "cannot comply"
    }));
    let completed = bodies
        .iter()
        .find(|body| body["type"] == "response.completed")
        .expect("completed response");
    assert_eq!(
        completed["response"]["output"][0]["content"][0],
        serde_json::json!({"type": "refusal", "refusal": "cannot comply"})
    );
    assert!(
        !bodies
            .iter()
            .any(|body| body["type"] == "response.output_text.delta")
    );
}

#[test]
fn tool_only_stream_does_not_emit_an_empty_message_item() {
    let mut formatter = ResponsesStreamFormatter::new();
    let events = formatter.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "resp-tool".into(),
            model: "logical-model".into(),
        },
        AiStreamDelta::ToolCallStart {
            index: 0,
            id: "call_1".into(),
            name: "lookup".into(),
        },
        AiStreamDelta::ToolCallDelta {
            index: 0,
            arguments: "{}".into(),
        },
        AiStreamDelta::Done {
            stop_reason: "tool_calls".into(),
        },
    ]);
    let bodies = events
        .iter()
        .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
        .collect::<Vec<_>>();
    assert!(!bodies.iter().any(|body| {
        body["item"]["type"] == "message"
            && body["item"]["content"]
                .as_array()
                .is_some_and(Vec::is_empty)
    }));
    let added = bodies
        .iter()
        .find(|body| {
            body["type"] == "response.output_item.added" && body["item"]["type"] == "function_call"
        })
        .expect("function call added");
    assert_eq!(added["output_index"], 0);
}

#[test]
fn empty_name_tool_call_starts_on_same_index_merge() {
    let mut formatter = ResponsesStreamFormatter::new();
    let events = formatter.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "resp-merge".into(),
            model: "logical-model".into(),
        },
        AiStreamDelta::ToolCallStart {
            index: 0,
            id: "call_00_read".into(),
            name: "read".into(),
        },
        AiStreamDelta::ToolCallStart {
            index: 0,
            id: String::new(),
            name: String::new(),
        },
        AiStreamDelta::ToolCallDelta {
            index: 0,
            arguments: r#"{"path":"."}"#.into(),
        },
        AiStreamDelta::Done {
            stop_reason: "tool_calls".into(),
        },
    ]);
    let bodies = events
        .iter()
        .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
        .collect::<Vec<_>>();
    let added: Vec<_> = bodies
        .iter()
        .filter(|body| {
            body["type"] == "response.output_item.added" && body["item"]["type"] == "function_call"
        })
        .collect();
    assert_eq!(added.len(), 1, "same index must stay one function_call");
    assert_eq!(added[0]["item"]["call_id"], "call_00_read");
    assert_eq!(added[0]["item"]["name"], "read");

    let done = bodies
        .iter()
        .find(|body| {
            body["type"] == "response.output_item.done" && body["item"]["type"] == "function_call"
        })
        .expect("function call done");
    assert_eq!(done["item"]["name"], "read");
    assert_eq!(done["item"]["arguments"], r#"{"path":"."}"#);
    assert_eq!(done["item"]["call_id"], "call_00_read");
}

#[test]
fn function_call_item_done_preserves_incomplete_status() {
    let mut formatter = ResponsesStreamFormatter::new();
    let completed = stravia_runtime_contract::protocol::ir::AiItem::function_call(
        stravia_runtime_contract::protocol::ir::ToolCall {
            id: "call_1".into(),
            name: "lookup".into(),
            arguments: "{}".into(),
        },
    )
    .with_graph_metadata(
        Some("fc_provider".into()),
        Some(AiItemStatus::Incomplete),
        stravia_runtime_contract::protocol::ir::AiItemProvenance::Provider,
        stravia_runtime_contract::protocol::ir::AiItemAudience::Client,
    );
    let events = formatter.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "resp-tool-status".into(),
            model: "logical-model".into(),
        },
        AiStreamDelta::ToolCallStart {
            index: 0,
            id: "call_1".into(),
            name: "lookup".into(),
        },
        AiStreamDelta::ItemDone {
            index: 0,
            item: completed,
        },
        AiStreamDelta::ResponseTerminal {
            status: "incomplete".into(),
            incomplete_details: Some(serde_json::json!({"reason": "max_output_tokens"})),
        },
    ]);

    let done = events
        .iter()
        .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
        .find(|event| {
            event["type"] == "response.output_item.done" && event["item"]["type"] == "function_call"
        })
        .expect("function call done");
    assert_eq!(done["item"]["status"], "incomplete");
    let terminal = events
        .iter()
        .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
        .find(|event| event["type"] == "response.incomplete")
        .expect("terminal response");
    assert_eq!(terminal["response"]["output"][0]["status"], "incomplete");
}

fn event_bodies(events: &[SseEvent]) -> Vec<serde_json::Value> {
    events
        .iter()
        .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
        .collect()
}

fn function_call_item(
    id: &str,
    name: &str,
    arguments: &str,
) -> stravia_runtime_contract::protocol::ir::AiItem {
    stravia_runtime_contract::protocol::ir::AiItem::function_call(
        stravia_runtime_contract::protocol::ir::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
        },
    )
}

#[test]
fn function_call_item_done_emits_arguments_done_before_terminal() {
    let mut formatter = ResponsesStreamFormatter::new();
    let live = formatter.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "resp-live-tool".into(),
            model: "logical-model".into(),
        },
        AiStreamDelta::ToolCallStart {
            index: 0,
            id: "call_1".into(),
            name: "read".into(),
        },
        AiStreamDelta::ToolCallDelta {
            index: 0,
            arguments: r#"{"path":"a"}"#.into(),
        },
        AiStreamDelta::ItemDone {
            index: 0,
            item: function_call_item("call_1", "read", r#"{"path":"a"}"#),
        },
    ]);
    let live_bodies = event_bodies(&live);
    assert!(
        live_bodies.iter().any(|body| {
            body["type"] == "response.function_call_arguments.done"
                && body["arguments"] == r#"{"path":"a"}"#
        }),
        "client tool arguments must complete as soon as ItemDone arrives: {live_bodies:?}"
    );
    assert!(
        live_bodies.iter().any(|body| {
            body["type"] == "response.output_item.done" && body["item"]["type"] == "function_call"
        }),
        "client tool item must close before the terminal response: {live_bodies:?}"
    );
    assert!(
        !live_bodies
            .iter()
            .any(|body| body["type"] == "response.completed"),
        "ItemDone must not wait for the terminal response"
    );

    let terminal = formatter.format_deltas(&[AiStreamDelta::ResponseTerminal {
        status: "completed".into(),
        incomplete_details: None,
    }]);
    let terminal_bodies = event_bodies(&terminal);
    assert_eq!(
        terminal_bodies
            .iter()
            .filter(|body| body["type"] == "response.function_call_arguments.done")
            .count(),
        0,
        "already forwarded function-call done must not be repeated at terminal: {terminal_bodies:?}"
    );
    assert!(
        terminal_bodies
            .iter()
            .any(|body| body["type"] == "response.completed")
    );
}

#[test]
fn multi_function_calls_emit_done_as_each_item_completes() {
    let mut formatter = ResponsesStreamFormatter::new();
    let first = formatter.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "resp-multi-tool".into(),
            model: "logical-model".into(),
        },
        AiStreamDelta::ToolCallStart {
            index: 1,
            id: "call_1".into(),
            name: "read".into(),
        },
        AiStreamDelta::ToolCallDelta {
            index: 1,
            arguments: r#"{"path":"a"}"#.into(),
        },
        AiStreamDelta::ItemDone {
            index: 1,
            item: function_call_item("call_1", "read", r#"{"path":"a"}"#),
        },
    ]);
    let first_bodies = event_bodies(&first);
    assert!(first_bodies.iter().any(|body| {
        body["type"] == "response.function_call_arguments.done"
            && body["arguments"] == r#"{"path":"a"}"#
    }));
    assert!(
        !first_bodies
            .iter()
            .any(|body| body["type"] == "response.completed")
    );

    let second = formatter.format_deltas(&[
        AiStreamDelta::ToolCallStart {
            index: 2,
            id: "call_2".into(),
            name: "grep".into(),
        },
        AiStreamDelta::ToolCallDelta {
            index: 2,
            arguments: r#"{"pattern":"x"}"#.into(),
        },
    ]);
    let second_bodies = event_bodies(&second);
    assert!(second_bodies.iter().any(|body| {
        body["type"] == "response.output_item.added"
            && body["item"]["call_id"] == "call_2"
            && body["item"]["name"] == "grep"
    }));
    assert!(
        !second_bodies
            .iter()
            .any(|body| { body["type"] == "response.function_call_arguments.done" }),
        "the first client tool must already have completed before later tools start: {second_bodies:?}"
    );

    let tail = formatter.format_deltas(&[
        AiStreamDelta::ItemDone {
            index: 2,
            item: function_call_item("call_2", "grep", r#"{"pattern":"x"}"#),
        },
        AiStreamDelta::ResponseTerminal {
            status: "completed".into(),
            incomplete_details: None,
        },
    ]);
    let tail_bodies = event_bodies(&tail);
    assert_eq!(
        tail_bodies
            .iter()
            .filter(|body| body["type"] == "response.function_call_arguments.done")
            .count(),
        1,
        "only the still-open client tool should complete in the tail: {tail_bodies:?}"
    );
    assert_eq!(
        tail_bodies
            .iter()
            .find(|body| { body["type"] == "response.function_call_arguments.done" })
            .map(|body| body["arguments"].as_str()),
        Some(Some(r#"{"pattern":"x"}"#))
    );
}

#[test]
fn tool_call_complete_emits_arguments_done_before_terminal() {
    let mut formatter = ResponsesStreamFormatter::new();
    let live = formatter.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "resp-complete-tool".into(),
            model: "logical-model".into(),
        },
        AiStreamDelta::ToolCallStart {
            index: 0,
            id: "call_1".into(),
            name: "eval".into(),
        },
        AiStreamDelta::ToolCallDelta {
            index: 0,
            arguments: r#"{"language":"py"}"#.into(),
        },
        AiStreamDelta::ToolCallComplete {
            index: 0,
            tool_call: stravia_runtime_contract::protocol::ir::ToolCall {
                id: "call_1".into(),
                name: "eval".into(),
                arguments: r#"{"language":"py"}"#.into(),
            },
        },
    ]);
    let live_bodies = event_bodies(&live);
    assert!(live_bodies.iter().any(|body| {
        body["type"] == "response.function_call_arguments.done"
            && body["arguments"] == r#"{"language":"py"}"#
    }));
    assert!(
        !live_bodies
            .iter()
            .any(|body| body["type"] == "response.completed")
    );
}

#[test]
fn streams_agent_result_and_drops_deleted_media_result() {
    let mut formatter = ResponsesStreamFormatter::new();
    let events = formatter.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "resp-1".into(),
            model: "model-1".into(),
        },
        AiStreamDelta::Unknown {
            raw: r#"{"type":"stravia:agent_result","path":"stravia://turns/aaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#.into(),
        },
        AiStreamDelta::Unknown {
            raw:
                r#"{"type":"stravia:media_result","path":"stravia://turns/bbbbbbbbbbbbbbbbbbbbbbbbbbbb","completion":"complete"}"#
                    .into(),
        },
        AiStreamDelta::Done {
            stop_reason: "stop".into(),
        },
    ]);

    let item_events = events
        .iter()
        .filter_map(|event| {
            let body = serde_json::from_str::<serde_json::Value>(&event.data).ok()?;
            let is_item_event = matches!(
                body["type"].as_str(),
                Some("response.output_item.added" | "response.output_item.done")
            );
            is_item_event.then_some(body)
        })
        .filter(|body| body["item"]["type"] == "stravia:agent_result")
        .collect::<Vec<_>>();
    assert_eq!(item_events.len(), 2);
    assert_eq!(item_events[0]["output_index"], 0);
    assert_eq!(item_events[1]["output_index"], 0);
    assert_eq!(
        item_events[0]["item"]["path"],
        "stravia://turns/aaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    );
    assert_eq!(
        item_events[1]["item"]["path"],
        "stravia://turns/aaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    );
    assert!(
        !events
            .iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
            .any(|body| { body["item"]["type"] == "stravia:media_result" })
    );

    let completed = events
        .iter()
        .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
        .find(|body| body["type"] == "response.completed")
        .expect("response.completed event");
    assert!(
        completed["response"]["output"]
            .as_array()
            .expect("response output")
            .iter()
            .any(|item| {
                item["type"] == "stravia:agent_result"
                    && item["path"] == "stravia://turns/aaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            })
    );
    assert!(
        !completed["response"]["output"]
            .as_array()
            .expect("response output")
            .iter()
            .any(|item| item["type"] == "stravia:media_result")
    );
}

#[test]
fn terminal_message_clears_metadata_after_text_rewrite() {
    let mut formatter = ResponsesStreamFormatter::new();
    let mut completed = stravia_runtime_contract::protocol::ir::AiItem::output_text("before");
    completed.meta = Some(serde_json::json!({
        "__open_responses_content": [{
            "type": "output_text",
            "text": "before",
            "annotations": [{"type": "url_citation", "url": "https://example.test"}],
            "logprobs": [{"token": "before", "logprob": -0.1}]
        }]
    }));
    let events = formatter.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "resp-1".into(),
            model: "model-1".into(),
        },
        AiStreamDelta::TextDelta("after".into()),
        AiStreamDelta::ItemDone {
            index: 0,
            item: completed,
        },
        AiStreamDelta::Done {
            stop_reason: "stop".into(),
        },
    ]);

    let terminal = events
        .iter()
        .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
        .find(|event| event["type"] == "response.completed")
        .expect("response.completed");
    let content = &terminal["response"]["output"][0]["content"][0];
    assert_eq!(content["text"], "after");
    assert_eq!(content["annotations"], serde_json::json!([]));
    assert_eq!(content["logprobs"], serde_json::json!([]));
}

#[test]
fn private_extension_progress_is_not_exposed() {
    let mut formatter = ResponsesStreamFormatter::new();
    let events = formatter.format_deltas(&[AiStreamDelta::Unknown {
        raw: serde_json::json!({
            "type": "stravia_web_search_activity",
            "call_id": "call_1",
            "phase": "searching",
            "ordinal": 2
        })
        .to_string(),
    }]);

    assert!(events.is_empty());
}

#[test]
fn text_delta_preserves_logprobs_and_obfuscation() {
    let mut formatter = ResponsesStreamFormatter::new();
    let events = formatter.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "resp-1".into(),
            model: "model-1".into(),
        },
        AiStreamDelta::TextDeltaWithMetadata {
            text: "hello".into(),
            logprobs: vec![serde_json::json!({
                "token": "hello",
                "logprob": -0.1,
                "bytes": [104, 101, 108, 108, 111],
                "top_logprobs": []
            })],
            obfuscation: Some("pad".into()),
            output_index: None,
            content_index: None,
        },
    ]);

    let delta = events
        .iter()
        .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
        .find(|event| event["type"] == "response.output_text.delta")
        .expect("output text delta");
    assert_eq!(delta["logprobs"][0]["token"], "hello");
    assert_eq!(delta["obfuscation"], "pad");
}

#[test]
fn reasoning_summary_and_content_stream_as_distinct_events() {
    let mut formatter = ResponsesStreamFormatter::new();
    let events = formatter.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "resp-1".into(),
            model: "model-1".into(),
        },
        AiStreamDelta::ReasoningSummaryDelta {
            text: "summary".into(),
            obfuscation: Some("summary-pad".into()),
            output_index: None,
            content_index: None,
        },
        AiStreamDelta::ThinkingDeltaWithMetadata {
            text: "full reasoning".into(),
            obfuscation: Some("content-pad".into()),
            output_index: None,
            content_index: None,
        },
        AiStreamDelta::Done {
            stop_reason: "stop".into(),
        },
    ]);
    let bodies = events
        .iter()
        .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
        .collect::<Vec<_>>();

    let summary_delta = bodies
        .iter()
        .find(|event| event["type"] == "response.reasoning_summary_text.delta")
        .expect("summary delta");
    assert_eq!(summary_delta["obfuscation"], "summary-pad");
    let content_delta = bodies
        .iter()
        .find(|event| event["type"] == "response.reasoning_text.delta")
        .expect("reasoning content delta");
    assert_eq!(content_delta["obfuscation"], "content-pad");
    let content_done = bodies
        .iter()
        .find(|event| event["type"] == "response.reasoning_text.done")
        .expect("reasoning content done");
    assert_eq!(content_done["text"], "full reasoning");
    let terminal = bodies
        .iter()
        .find(|event| event["type"] == "response.completed")
        .expect("response completed");
    let reasoning = &terminal["response"]["output"][0];
    assert_eq!(reasoning["summary"][0]["text"], "summary");
    assert_eq!(reasoning["content"][0]["text"], "full reasoning");
}

#[test]
fn closes_each_reasoning_summary_part_before_starting_the_next() {
    let mut formatter = ResponsesStreamFormatter::new();
    let events = formatter.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "resp_summary_parts".into(),
            model: "model".into(),
        },
        AiStreamDelta::ReasoningSummaryDelta {
            text: "first".into(),
            obfuscation: None,
            output_index: Some(0),
            content_index: Some(0),
        },
        AiStreamDelta::ReasoningSummaryDelta {
            text: "second".into(),
            obfuscation: None,
            output_index: Some(0),
            content_index: Some(1),
        },
        AiStreamDelta::Done {
            stop_reason: "stop".into(),
        },
    ]);
    let bodies = events
        .iter()
        .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
        .collect::<Vec<_>>();
    let lifecycle = bodies
        .iter()
        .filter_map(|event| {
            let event_type = event["type"].as_str()?;
            event_type
                .starts_with("response.reasoning_summary_")
                .then(|| format!("{event_type}:{}", event["summary_index"]))
        })
        .collect::<Vec<_>>();

    assert_eq!(
        lifecycle,
        [
            "response.reasoning_summary_part.added:0",
            "response.reasoning_summary_text.delta:0",
            "response.reasoning_summary_text.done:0",
            "response.reasoning_summary_part.done:0",
            "response.reasoning_summary_part.added:1",
            "response.reasoning_summary_text.delta:1",
            "response.reasoning_summary_text.done:1",
            "response.reasoning_summary_part.done:1",
        ]
    );
    let terminal = bodies
        .iter()
        .find(|event| event["type"] == "response.completed")
        .expect("response completed");
    assert_eq!(
        terminal["response"]["output"][0]["summary"],
        serde_json::json!([
            {"type": "summary_text", "text": "first"},
            {"type": "summary_text", "text": "second"}
        ])
    );
}

#[test]
fn completed_item_forwards_encrypted_only_reasoning() {
    let mut formatter = ResponsesStreamFormatter::new();
    let events = formatter.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "resp-1".into(),
            model: "model-1".into(),
        },
        AiStreamDelta::ItemDone {
            index: 0,
            item: stravia_runtime_contract::protocol::ir::AiItem::reasoning(
                Vec::new(),
                Vec::new(),
                Some("opaque".into()),
            ),
        },
        AiStreamDelta::Done {
            stop_reason: "stop".into(),
        },
    ]);
    let terminal = events
        .iter()
        .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
        .find(|event| event["type"] == "response.completed")
        .expect("response completed");

    assert_eq!(
        terminal["response"]["output"][0]["encrypted_content"],
        "opaque"
    );
}
#[test]
fn preserves_multiple_message_output_indices() {
    let mut formatter = ResponsesStreamFormatter::new();
    let events = formatter.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "resp-1".into(),
            model: "model-1".into(),
        },
        AiStreamDelta::TextDeltaWithMetadata {
            text: "first".into(),
            logprobs: Vec::new(),
            obfuscation: None,
            output_index: Some(0),
            content_index: Some(0),
        },
        AiStreamDelta::RefusalDeltaWithIndex {
            text: "second".into(),
            output_index: 1,
            content_index: 0,
        },
        AiStreamDelta::Done {
            stop_reason: "stop".into(),
        },
    ]);
    let bodies = events
        .iter()
        .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
        .collect::<Vec<_>>();
    let semantic_deltas = bodies
        .iter()
        .filter(|event| {
            matches!(
                event["type"].as_str(),
                Some("response.output_text.delta" | "response.refusal.delta")
            )
        })
        .map(|event| {
            (
                event["output_index"].as_u64().expect("output index"),
                event["delta"].as_str().expect("text"),
            )
        })
        .collect::<Vec<_>>();

    assert_eq!(semantic_deltas, [(0, "first"), (1, "second")]);
    let terminal = bodies
        .iter()
        .find(|event| event["type"] == "response.completed")
        .expect("response completed");
    assert_eq!(
        terminal["response"]["output"][0]["content"][0]["text"],
        "first"
    );
    assert_eq!(
        terminal["response"]["output"][1]["content"][0]["refusal"],
        "second"
    );
}
#[test]
fn preserves_multiple_reasoning_output_indices() {
    let mut formatter = ResponsesStreamFormatter::new();
    let events = formatter.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "resp_reasoning".into(),
            model: "model".into(),
        },
        AiStreamDelta::ReasoningSummaryDelta {
            text: "first".into(),
            obfuscation: None,
            output_index: Some(0),
            content_index: Some(0),
        },
        AiStreamDelta::ReasoningSummaryDelta {
            text: "second".into(),
            obfuscation: None,
            output_index: Some(1),
            content_index: Some(0),
        },
        AiStreamDelta::Done {
            stop_reason: "stop".into(),
        },
    ]);
    let terminal = events
        .iter()
        .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
        .find(|event| event["type"] == "response.completed")
        .expect("response completed");
    assert_eq!(
        terminal["response"]["output"][0]["summary"][0]["text"],
        "first"
    );
    assert_eq!(
        terminal["response"]["output"][1]["summary"][0]["text"],
        "second"
    );
}

#[test]
fn item_done_creates_empty_messages_and_preserves_item_status() {
    let mut formatter = ResponsesStreamFormatter::new();
    let completed = stravia_runtime_contract::protocol::ir::AiItem::output_text("")
        .with_graph_metadata(
            Some("msg_completed".into()),
            Some(stravia_runtime_contract::protocol::ir::AiItemStatus::Completed),
            stravia_runtime_contract::protocol::ir::AiItemProvenance::Provider,
            stravia_runtime_contract::protocol::ir::AiItemAudience::Client,
        );
    let incomplete = stravia_runtime_contract::protocol::ir::AiItem::output_text("partial")
        .with_graph_metadata(
            Some("msg_incomplete".into()),
            Some(stravia_runtime_contract::protocol::ir::AiItemStatus::Incomplete),
            stravia_runtime_contract::protocol::ir::AiItemProvenance::Provider,
            stravia_runtime_contract::protocol::ir::AiItemAudience::Client,
        );
    let events = formatter.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "resp_messages".into(),
            model: "model".into(),
        },
        AiStreamDelta::ItemDone {
            index: 0,
            item: completed,
        },
        AiStreamDelta::TextDeltaWithMetadata {
            text: "partial".into(),
            logprobs: Vec::new(),
            obfuscation: None,
            output_index: Some(1),
            content_index: Some(0),
        },
        AiStreamDelta::ItemDone {
            index: 1,
            item: incomplete,
        },
        AiStreamDelta::ResponseTerminal {
            status: "incomplete".into(),
            incomplete_details: Some(serde_json::json!({"reason": "max_output_tokens"})),
        },
    ]);
    let terminal = events
        .iter()
        .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
        .find(|event| event["type"] == "response.incomplete")
        .expect("response incomplete");
    assert_eq!(terminal["response"]["output"][0]["status"], "completed");
    assert_eq!(
        terminal["response"]["output"][0]["content"],
        serde_json::json!([])
    );
    assert_eq!(terminal["response"]["output"][1]["status"], "incomplete");
    assert_eq!(
        terminal["response"]["output"][1]["content"][0]["text"],
        "partial"
    );
}

#[test]
fn annotation_stays_on_its_unchanged_indexed_message() {
    let mut formatter = ResponsesStreamFormatter::new();
    let mut completed = stravia_runtime_contract::protocol::ir::AiItem {
        role: stravia_runtime_contract::protocol::ir::Role::Assistant,
        content: MessageContent::Blocks(vec![
            ContentBlock::Text {
                text: String::new(),
                cache_control: None,
            },
            ContentBlock::Text {
                text: String::new(),
                cache_control: None,
            },
            ContentBlock::Text {
                text: "answer".into(),
                cache_control: None,
            },
        ]),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    };
    completed.meta = Some(serde_json::json!({
        "__open_responses_content": [
            {"type": "output_text", "text": "", "annotations": [], "logprobs": []},
            {"type": "output_text", "text": "", "annotations": [], "logprobs": []},
            {
                "type": "output_text",
                "text": "answer",
                "annotations": [{"type": "url_citation", "url": "https://example.test"}],
                "logprobs": []
            }
        ]
    }));
    let events = formatter.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "resp_annotation".into(),
            model: "model".into(),
        },
        AiStreamDelta::TextDeltaWithMetadata {
            text: "answer".into(),
            logprobs: Vec::new(),
            obfuscation: None,
            output_index: Some(3),
            content_index: Some(2),
        },
        AiStreamDelta::Unknown {
            raw: serde_json::json!({
                "__open_responses_event": {
                    "type": "response.output_text.annotation.added",
                    "sequence_number": 4,
                    "item_id": "provider-message",
                    "output_index": 3,
                    "content_index": 2,
                    "annotation_index": 0,
                    "annotation": {
                        "type": "url_citation",
                        "url": "https://example.test",
                        "title": "source",
                        "start_index": 0,
                        "end_index": 6
                    }
                }
            })
            .to_string(),
        },
        AiStreamDelta::ItemDone {
            index: 3,
            item: completed,
        },
    ]);
    let annotation = events
        .iter()
        .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
        .find(|event| event["type"] == "response.output_text.annotation.added")
        .expect("annotation event");
    assert_eq!(annotation["output_index"], 3);
    assert_eq!(annotation["content_index"], 2);
    let part_added = events
        .iter()
        .position(|event| event.event.as_deref() == Some("response.content_part.added"))
        .expect("content part added");
    let annotation_added = events
        .iter()
        .position(|event| event.event.as_deref() == Some("response.output_text.annotation.added"))
        .expect("annotation added");
    assert!(part_added < annotation_added);
    assert_eq!(formatter.indexed_messages.len(), 1);
    assert_eq!(formatter.message_output_index, None);
}

#[test]
fn rewritten_text_drops_stale_annotation_events() {
    let mut formatter = ResponsesStreamFormatter::new();
    let mut completed = stravia_runtime_contract::protocol::ir::AiItem::output_text("before");
    completed.meta = Some(serde_json::json!({
        "__open_responses_content": [{
            "type": "output_text",
            "text": "before",
            "annotations": [{"type": "url_citation", "url": "https://example.test"}],
            "logprobs": []
        }]
    }));
    let events = formatter.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "resp_rewritten".into(),
            model: "model".into(),
        },
        AiStreamDelta::TextDeltaWithMetadata {
            text: "after".into(),
            logprobs: Vec::new(),
            obfuscation: None,
            output_index: Some(0),
            content_index: Some(0),
        },
        AiStreamDelta::Unknown {
            raw: serde_json::json!({
                "__open_responses_event": {
                    "type": "response.output_text.annotation.added",
                    "item_id": "provider-message",
                    "output_index": 0,
                    "content_index": 0,
                    "annotation_index": 0,
                    "annotation": {
                        "type": "url_citation",
                        "url": "https://example.test"
                    }
                }
            })
            .to_string(),
        },
        AiStreamDelta::ItemDone {
            index: 0,
            item: completed,
        },
        AiStreamDelta::Done {
            stop_reason: "stop".into(),
        },
    ]);

    assert!(
        events.iter().all(|event| {
            event.event.as_deref() != Some("response.output_text.annotation.added")
        })
    );
    let terminal = events
        .iter()
        .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
        .find(|event| event["type"] == "response.completed")
        .expect("terminal response");
    assert_eq!(
        terminal["response"]["output"][0]["content"][0]["annotations"],
        serde_json::json!([])
    );
}
#[test]
fn item_done_does_not_restore_semantic_text_removed_by_a_hook() {
    let mut formatter = ResponsesStreamFormatter::new();
    let events = formatter.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "resp_redacted".into(),
            model: "model".into(),
        },
        AiStreamDelta::ItemDone {
            index: 0,
            item: stravia_runtime_contract::protocol::ir::AiItem::output_text("provider secret"),
        },
        AiStreamDelta::Done {
            stop_reason: "stop".into(),
        },
    ]);
    let terminal = events
        .iter()
        .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
        .find(|event| event["type"] == "response.completed")
        .expect("terminal response");

    assert_eq!(
        terminal["response"]["output"][0]["content"],
        serde_json::json!([])
    );
}

#[test]
fn function_output_item_done_emits_lifecycle_and_terminal_item() {
    let mut formatter = ResponsesStreamFormatter::new();
    let function_output = stravia_runtime_contract::protocol::ir::AiItem {
        role: stravia_runtime_contract::protocol::ir::Role::Tool,
        content: MessageContent::Blocks(vec![ContentBlock::Text {
            text: "tool output".into(),
            cache_control: None,
        }]),
        tool_calls: None,
        tool_call_id: Some("call_1".into()),
        meta: None,
    }
    .with_graph_metadata(
        Some("fco_provider".into()),
        Some(AiItemStatus::Completed),
        stravia_runtime_contract::protocol::ir::AiItemProvenance::Provider,
        stravia_runtime_contract::protocol::ir::AiItemAudience::Client,
    );
    let events = formatter.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "resp_function_output".into(),
            model: "model".into(),
        },
        AiStreamDelta::ItemDone {
            index: 2,
            item: function_output,
        },
        AiStreamDelta::Done {
            stop_reason: "stop".into(),
        },
    ]);
    let bodies = events
        .iter()
        .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
        .collect::<Vec<_>>();

    let added = bodies
        .iter()
        .find(|event| {
            event["type"] == "response.output_item.added"
                && event["item"]["type"] == "function_call_output"
        })
        .expect("function output added");
    let done = bodies
        .iter()
        .find(|event| {
            event["type"] == "response.output_item.done"
                && event["item"]["type"] == "function_call_output"
        })
        .expect("function output done");
    assert_eq!(added["output_index"], 2);
    assert_eq!(done["output_index"], added["output_index"]);
    assert_eq!(done["item"]["id"], added["item"]["id"]);
    assert_eq!(done["item"]["call_id"], added["item"]["call_id"]);
    assert_eq!(done["item"]["output"][0]["type"], "input_text");

    let item_id = added["item"]["id"].as_str().expect("function output ID");
    assert_eq!(
        crate::protocol::codec::open_responses::formatter::response_id_from_gateway_item_id(
            item_id
        ),
        Some("resp_function_output".into())
    );

    let terminal = bodies
        .iter()
        .find(|event| event["type"] == "response.completed")
        .expect("terminal response");
    let terminal_item = terminal["response"]["output"]
        .as_array()
        .expect("terminal output")
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .expect("terminal function output");
    assert_eq!(terminal_item["id"], added["item"]["id"]);
    assert_eq!(terminal_item["call_id"], added["item"]["call_id"]);
    assert_eq!(terminal_item["call_id"], "call_1");
}

#[test]
fn reasoning_item_seals_before_a_later_function_call_completes() {
    // Regression: a reasoning item used to stay open until the terminal flush,
    // so in-stream function_call dones reached clients first. Clients that
    // persist output items in done-arrival order (e.g. omp) then replayed a
    // reordered history that broke generation-chain prefix discovery.
    let mut formatter = ResponsesStreamFormatter::new();
    let events = formatter.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "resp-reasoning-tool".into(),
            model: "model".into(),
        },
        AiStreamDelta::ThinkingDelta("chain".into()),
        AiStreamDelta::ToolCallStart {
            index: 0,
            id: "call_1".into(),
            name: "read".into(),
        },
        AiStreamDelta::ToolCallDelta {
            index: 0,
            arguments: r#"{"path":"a"}"#.into(),
        },
        AiStreamDelta::ToolCallComplete {
            index: 0,
            tool_call: stravia_runtime_contract::protocol::ir::ToolCall {
                id: "call_1".into(),
                name: "read".into(),
                arguments: r#"{"path":"a"}"#.into(),
            },
        },
        AiStreamDelta::Done {
            stop_reason: "tool_calls".into(),
        },
    ]);
    let bodies = event_bodies(&events);

    let added_order = bodies
        .iter()
        .filter(|body| body["type"] == "response.output_item.added")
        .map(|body| body["output_index"].as_u64())
        .collect::<Vec<_>>();
    assert_eq!(added_order, [Some(0), Some(1)]);

    let done_order = bodies
        .iter()
        .filter(|body| body["type"] == "response.output_item.done")
        .map(|body| body["output_index"].as_u64())
        .collect::<Vec<_>>();
    assert_eq!(
        done_order,
        [Some(0), Some(1)],
        "reasoning must seal before the function call completes: {bodies:?}"
    );

    let reasoning_done = bodies
        .iter()
        .find(|body| {
            body["type"] == "response.output_item.done" && body["item"]["type"] == "reasoning"
        })
        .expect("reasoning done");
    assert_eq!(reasoning_done["item"]["content"][0]["text"], "chain");

    let terminal = bodies
        .iter()
        .find(|body| body["type"] == "response.completed")
        .expect("response completed");
    let output_types = terminal["response"]["output"]
        .as_array()
        .expect("terminal output")
        .iter()
        .map(|item| item["type"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(output_types, [Some("reasoning"), Some("function_call")]);
}

#[test]
fn indexed_reasoning_item_done_seals_before_tool_completion() {
    use stravia_runtime_contract::protocol::ir::{AiItem, ToolCall};

    let mut formatter = ResponsesStreamFormatter::new();
    let open = formatter.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "resp-indexed-reasoning-tool".into(),
            model: "model".into(),
        },
        AiStreamDelta::ThinkingDeltaWithMetadata {
            text: "Inspect the fixture.".into(),
            obfuscation: None,
            output_index: Some(0),
            content_index: Some(0),
        },
        AiStreamDelta::ToolCallStart {
            index: 1,
            id: "call_1".into(),
            name: "read".into(),
        },
        AiStreamDelta::ToolCallDelta {
            index: 1,
            arguments: r#"{"path":"fixture.txt"}"#.into(),
        },
    ]);
    assert!(
        event_bodies(&open)
            .iter()
            .all(|event| event["type"] != "response.output_item.done"),
        "工具开始不代表思考签名已经完整，不能提前封口"
    );

    let completed = AiItem::reasoning(
        Vec::new(),
        vec!["Inspect the fixture.".into()],
        Some("late-signature".into()),
    );
    let closed = formatter.format_deltas(&[
        AiStreamDelta::ItemDone {
            index: 0,
            item: completed.clone(),
        },
        AiStreamDelta::ToolCallComplete {
            index: 1,
            tool_call: ToolCall {
                id: "call_1".into(),
                name: "read".into(),
                arguments: r#"{"path":"fixture.txt"}"#.into(),
            },
        },
    ]);
    let bodies = event_bodies(&closed);
    let completed_items = bodies
        .iter()
        .filter(|event| event["type"] == "response.output_item.done")
        .collect::<Vec<_>>();
    assert_eq!(
        completed_items
            .iter()
            .map(|event| event["output_index"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [0, 1],
        "权威思考项必须在工具完成前交付，不依赖整轮终态"
    );
    assert_eq!(
        completed_items[0]["item"]["encrypted_content"],
        "late-signature"
    );
    assert_eq!(
        completed_items[0]["item"]["content"][0]["text"],
        "Inspect the fixture."
    );

    let terminal = formatter.format_deltas(&[
        AiStreamDelta::ItemDone {
            index: 0,
            item: completed,
        },
        AiStreamDelta::Done {
            stop_reason: "tool_calls".into(),
        },
    ]);
    let terminal = event_bodies(&terminal);
    assert!(
        terminal
            .iter()
            .all(|event| event["type"] != "response.output_item.done"),
        "重复 ItemDone 与终帧不能再次交付已关闭项"
    );
    let output = &terminal
        .iter()
        .find(|event| event["type"] == "response.completed")
        .unwrap()["response"]["output"];
    assert_eq!(
        output,
        &serde_json::Value::Array(
            completed_items
                .iter()
                .map(|event| event["item"].clone())
                .collect()
        ),
        "终态快照必须保留已交付项及相同顺序"
    );
}

#[test]
fn thinking_resuming_after_a_function_call_opens_a_new_reasoning_item() {
    let mut formatter = ResponsesStreamFormatter::new();
    let events = formatter.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "resp-reasoning-resume".into(),
            model: "model".into(),
        },
        AiStreamDelta::ThinkingDelta("first".into()),
        AiStreamDelta::ToolCallStart {
            index: 0,
            id: "call_1".into(),
            name: "read".into(),
        },
        AiStreamDelta::ToolCallComplete {
            index: 0,
            tool_call: stravia_runtime_contract::protocol::ir::ToolCall {
                id: "call_1".into(),
                name: "read".into(),
                arguments: "{}".into(),
            },
        },
        AiStreamDelta::ThinkingDelta("second".into()),
        AiStreamDelta::Done {
            stop_reason: "tool_calls".into(),
        },
    ]);
    let bodies = event_bodies(&events);

    let reasoning_items = bodies
        .iter()
        .filter(|body| {
            body["type"] == "response.output_item.added" && body["item"]["type"] == "reasoning"
        })
        .map(|body| body["output_index"].as_u64())
        .collect::<Vec<_>>();
    assert_eq!(
        reasoning_items,
        [Some(0), Some(2)],
        "resumed thinking must open a fresh reasoning item, not append to the sealed one: {bodies:?}"
    );

    let done_order = bodies
        .iter()
        .filter(|body| body["type"] == "response.output_item.done")
        .map(|body| body["output_index"].as_u64())
        .collect::<Vec<_>>();
    assert_eq!(done_order, [Some(0), Some(1), Some(2)]);

    let terminal = bodies
        .iter()
        .find(|body| body["type"] == "response.completed")
        .expect("response completed");
    let output = terminal["response"]["output"].as_array().expect("output");
    let reasoning_texts = output
        .iter()
        .filter(|item| item["type"] == "reasoning")
        .map(|item| item["content"][0]["text"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(reasoning_texts, [Some("first"), Some("second")]);
}
