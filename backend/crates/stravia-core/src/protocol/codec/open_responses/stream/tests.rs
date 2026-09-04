use super::*;
use crate::protocol::ir::{ContentBlock, MessageContent};

#[test]
fn stream_formatter_closes_failures_with_standard_terminal_sequence() {
    let mut formatter = ResponsesStreamFormatter::new();
    let mut events = formatter.format_deltas(&[AiStreamDelta::StreamError {
        error: crate::protocol::ir::AiError::new(
            crate::protocol::ir::AiErrorKind::StreamMidError,
            "stream aborted",
        ),
    }]);
    events.extend(formatter.format_done());

    assert_eq!(events.len(), 3);
    assert_eq!(events[0].event.as_deref(), Some("error"));
    assert_eq!(events[1].event.as_deref(), Some("response.failed"));
    assert_eq!(events[2].event, None);
    assert_eq!(events[2].data, "[DONE]");
    let error: serde_json::Value = serde_json::from_str(&events[0].data).expect("error JSON");
    let failed: serde_json::Value = serde_json::from_str(&events[1].data).expect("failed JSON");
    assert_eq!(error["type"], "error");
    assert_eq!(error["error"]["code"], "response_stream_failed");
    assert_eq!(error["error"]["message"], "The response stream failed.");
    assert!(!error.to_string().contains("stream aborted"));
    assert_eq!(error["sequence_number"], 0);
    assert_eq!(failed["type"], "response.failed");
    assert_eq!(failed["sequence_number"], 1);
}

#[test]
fn response_profile_uses_effective_request_and_provider_confirmed_values() {
    let mut request = crate::protocol::ir::AiRequest::new("logical-model", Vec::new());
    request.instructions = Some("Be concise.".into());
    request.generation.temperature = Some(0.2);
    request.ext = Some(crate::protocol::ir::ProtocolExt::OpenResponses(
        crate::protocol::ir::OpenResponsesExt {
            store: Some(false),
            metadata: Some(serde_json::json!({"tenant": "acme"})),
            safety_identifier: Some("safe-user".into()),
            ..Default::default()
        },
    ));
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
fn function_call_item_done_preserves_incomplete_status() {
    let mut formatter = ResponsesStreamFormatter::new();
    let completed = crate::protocol::ir::AiItem::function_call(crate::protocol::ir::ToolCall {
        id: "call_1".into(),
        name: "lookup".into(),
        arguments: "{}".into(),
    })
    .with_graph_metadata(
        Some("fc_provider".into()),
        Some(AiItemStatus::Incomplete),
        crate::protocol::ir::AiItemProvenance::Provider,
        crate::protocol::ir::AiItemAudience::Client,
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

#[test]
fn streams_platform_owned_result_as_indexed_output_item() {
    let mut formatter = ResponsesStreamFormatter::new();
    let events = formatter.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "resp-1".into(),
            model: "model-1".into(),
        },
        AiStreamDelta::Unknown {
            raw: r#"{"type":"stravia:agent_result","turn_id":"aturn_1"}"#.into(),
        },
        AiStreamDelta::Unknown {
            raw:
                r#"{"type":"stravia:media_result","turn_id":"aturn_media","completion":"complete"}"#
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
    assert_eq!(item_events[0]["item"]["turn_id"], "aturn_1");
    assert_eq!(item_events[1]["item"]["turn_id"], "aturn_1");
    let media_events = events
        .iter()
        .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
        .filter(|body| {
            matches!(
                body["type"].as_str(),
                Some("response.output_item.added" | "response.output_item.done")
            ) && body["item"]["type"] == "stravia:media_result"
        })
        .collect::<Vec<_>>();
    assert_eq!(media_events.len(), 2);
    assert_eq!(media_events[0]["output_index"], 1);
    assert_eq!(media_events[1]["output_index"], 1);
    assert_eq!(media_events[0]["item"]["turn_id"], "aturn_media");
    assert_eq!(media_events[0]["item"]["completion"], "complete");

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
            .any(|item| item["type"] == "stravia:agent_result" && item["turn_id"] == "aturn_1")
    );
    assert!(
        completed["response"]["output"]
            .as_array()
            .expect("response output")
            .iter()
            .any(|item| item["type"] == "stravia:media_result"
                && item["turn_id"] == "aturn_media"
                && item["completion"] == "complete")
    );
}

#[test]
fn terminal_message_clears_metadata_after_text_rewrite() {
    let mut formatter = ResponsesStreamFormatter::new();
    let mut completed = crate::protocol::ir::AiItem::output_text("before");
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
            item: crate::protocol::ir::AiItem::reasoning(
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
    let completed = crate::protocol::ir::AiItem::output_text("").with_graph_metadata(
        Some("msg_completed".into()),
        Some(crate::protocol::ir::AiItemStatus::Completed),
        crate::protocol::ir::AiItemProvenance::Provider,
        crate::protocol::ir::AiItemAudience::Client,
    );
    let incomplete = crate::protocol::ir::AiItem::output_text("partial").with_graph_metadata(
        Some("msg_incomplete".into()),
        Some(crate::protocol::ir::AiItemStatus::Incomplete),
        crate::protocol::ir::AiItemProvenance::Provider,
        crate::protocol::ir::AiItemAudience::Client,
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
    let mut completed = crate::protocol::ir::AiItem {
        role: crate::protocol::ir::Role::Assistant,
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
    let mut completed = crate::protocol::ir::AiItem::output_text("before");
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
            item: crate::protocol::ir::AiItem::output_text("provider secret"),
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
    let function_output = crate::protocol::ir::AiItem {
        role: crate::protocol::ir::Role::Tool,
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
        crate::protocol::ir::AiItemProvenance::Provider,
        crate::protocol::ir::AiItemAudience::Client,
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

    assert!(bodies.iter().any(|event| {
        event["type"] == "response.output_item.added"
            && event["output_index"] == 2
            && event["item"]["type"] == "function_call_output"
            && event["item"]["id"] == "fco_function_output_2"
    }));
    assert!(bodies.iter().any(|event| {
        event["type"] == "response.output_item.done"
            && event["output_index"] == 2
            && event["item"]["output"][0]["type"] == "input_text"
    }));
    let terminal = bodies
        .iter()
        .find(|event| event["type"] == "response.completed")
        .expect("terminal response");
    assert_eq!(
        terminal["response"]["output"][0]["type"],
        "function_call_output"
    );
    assert_eq!(terminal["response"]["output"][0]["call_id"], "call_1");
    assert_eq!(
        terminal["response"]["output"][0]["id"],
        "fco_function_output_2"
    );
}
