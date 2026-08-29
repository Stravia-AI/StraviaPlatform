use jsonschema::Validator;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::formatter::ResponsesResponseFormatter;
use super::stream::ResponsesStreamFormatter;
use crate::protocol::ir::{AiItem, AiResponse, AiStreamDelta};

const OPENAPI: &str =
    include_str!("../../../../tests/fixtures/open_responses_2026_04_24.openapi.json");
const OPENAPI_SHA256: &str = "d598753c3a86fd8a2434828fe39dcc8786a7a694bbe46080a0d24c9fa40e72df";

fn schema(name: &str) -> Validator {
    let openapi: Value = serde_json::from_str(OPENAPI).expect("vendored OpenAPI JSON");
    let components = openapi
        .get("components")
        .cloned()
        .expect("OpenAPI components");
    let root = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$ref": format!("#/components/schemas/{name}"),
        "components": components,
    });
    jsonschema::validator_for(&root).expect("valid vendored schema")
}

fn assert_valid(name: &str, instance: &Value) {
    if let Err(error) = schema(name).validate(instance) {
        panic!(
            "{name} contract violation at {}: {error}",
            error.instance_path()
        );
    }
}
fn event_schema_name(event_type: &str) -> &'static str {
    match event_type {
        "error" => "ErrorStreamingEvent",
        "response.completed" => "ResponseCompletedStreamingEvent",
        "response.content_part.added" => "ResponseContentPartAddedStreamingEvent",
        "response.content_part.done" => "ResponseContentPartDoneStreamingEvent",
        "response.created" => "ResponseCreatedStreamingEvent",
        "response.failed" => "ResponseFailedStreamingEvent",
        "response.function_call_arguments.delta" => {
            "ResponseFunctionCallArgumentsDeltaStreamingEvent"
        }
        "response.function_call_arguments.done" => {
            "ResponseFunctionCallArgumentsDoneStreamingEvent"
        }
        "response.in_progress" => "ResponseInProgressStreamingEvent",
        "response.incomplete" => "ResponseIncompleteStreamingEvent",
        "response.output_item.added" => "ResponseOutputItemAddedStreamingEvent",
        "response.output_item.done" => "ResponseOutputItemDoneStreamingEvent",
        "response.output_text.annotation.added" => {
            "ResponseOutputTextAnnotationAddedStreamingEvent"
        }
        "response.output_text.delta" => "ResponseOutputTextDeltaStreamingEvent",
        "response.output_text.done" => "ResponseOutputTextDoneStreamingEvent",
        "response.queued" => "ResponseQueuedStreamingEvent",
        "response.reasoning.delta" => "ResponseReasoningDeltaStreamingEvent",
        "response.reasoning.done" => "ResponseReasoningDoneStreamingEvent",
        "response.reasoning_summary_part.added" => {
            "ResponseReasoningSummaryPartAddedStreamingEvent"
        }
        "response.reasoning_summary_part.done" => "ResponseReasoningSummaryPartDoneStreamingEvent",
        "response.reasoning_summary_text.delta" => "ResponseReasoningSummaryDeltaStreamingEvent",
        "response.reasoning_summary_text.done" => "ResponseReasoningSummaryDoneStreamingEvent",
        "response.refusal.delta" => "ResponseRefusalDeltaStreamingEvent",
        "response.refusal.done" => "ResponseRefusalDoneStreamingEvent",
        other => panic!("dated event has no schema mapping: {other}"),
    }
}
fn assert_valid_events(events: impl IntoIterator<Item = crate::protocol::SseEvent>) {
    for event in events {
        if event.data == "[DONE]" {
            continue;
        }
        let body: Value = serde_json::from_str(&event.data).expect("SSE JSON body");
        let event_type = body["type"].as_str().expect("event type");
        assert_eq!(event.event.as_deref(), Some(event_type));
        assert_valid(event_schema_name(event_type), &body);
    }
}

#[test]
fn vendored_openapi_has_recorded_sha256() {
    let canonical = OPENAPI.replace("\r\n", "\n");
    let actual = Sha256::digest(canonical.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert_eq!(actual, OPENAPI_SHA256);
}

#[test]
fn dated_event_matrix_resolves_every_parser_event_to_a_schema() {
    let openapi: Value = serde_json::from_str(OPENAPI).expect("vendored OpenAPI JSON");
    let schemas = openapi["components"]["schemas"]
        .as_object()
        .expect("component schemas");
    for event_type in super::parser::DATED_EVENT_TYPES {
        let schema_name = event_schema_name(event_type);
        assert!(
            schemas.contains_key(schema_name),
            "{event_type} maps to missing schema {schema_name}"
        );
    }
}

#[test]
fn owned_request_fixture_matches_dated_schema() {
    assert_valid(
        "CreateResponseBody",
        &json!({
            "model": "logical-model",
            "input": [{
                "type": "message",
                "role": "developer",
                "content": [{"type": "input_text", "text": "Use repository conventions."}]
            }],
            "store": true,
            "stream": false
        }),
    );
}

#[test]
fn websocket_error_fixture_matches_dated_schema() {
    assert_valid(
        "WebSocketErrorEvent",
        &json!({
            "type": "error",
            "status": 409,
            "error": {
                "type": "invalid_request",
                "code": "response_in_progress",
                "message": "A response is already in progress.",
                "param": null
            }
        }),
    );
}

#[test]
fn formatted_response_matches_dated_schema() {
    let mut response = AiResponse::new("resp_gateway", "logical-model");
    response.items = vec![
        AiItem::output_text("done"),
        AiItem::thinking("summary", Some("opaque".into())),
    ];
    response.stop_reason = Some("stop".into());

    let formatted = ResponsesResponseFormatter.format_response(&response);
    assert_valid("ResponseResource", &formatted);
}

#[test]
fn formatted_sse_lifecycle_matches_dated_event_schemas() {
    let mut formatter = ResponsesStreamFormatter::new();
    let mut events = formatter.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "resp_gateway".into(),
            model: "logical-model".into(),
        },
        AiStreamDelta::TextDelta("done".into()),
        AiStreamDelta::Done {
            stop_reason: "stop".into(),
        },
    ]);
    events.extend(formatter.format_done());

    assert_valid_events(events);
}

#[test]
fn reasoning_and_metadata_deltas_match_dated_event_schemas() {
    let mut formatter = ResponsesStreamFormatter::new();
    let events = formatter.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "resp_gateway".into(),
            model: "logical-model".into(),
        },
        AiStreamDelta::ReasoningSummaryDelta {
            text: "summary".into(),
            obfuscation: Some("summary-pad".into()),
            output_index: Some(0),
            content_index: Some(0),
        },
        AiStreamDelta::ThinkingDeltaWithMetadata {
            text: "full reasoning".into(),
            obfuscation: Some("reasoning-pad".into()),
            output_index: Some(0),
            content_index: Some(0),
        },
        AiStreamDelta::TextDeltaWithMetadata {
            text: "done".into(),
            logprobs: vec![json!({
                "token": "done",
                "logprob": -0.1,
                "bytes": [100, 111, 110, 101],
                "top_logprobs": []
            })],
            obfuscation: Some("text-pad".into()),
            output_index: None,
            content_index: None,
        },
        AiStreamDelta::Done {
            stop_reason: "stop".into(),
        },
    ]);

    assert_valid_events(events);
}
#[test]
fn indexed_message_deltas_match_dated_event_schemas() {
    let mut formatter = ResponsesStreamFormatter::new();
    let events = formatter.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "resp-indexed".into(),
            model: "logical-model".into(),
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

    assert_valid_events(events);
}

#[test]
fn formatted_failure_and_incomplete_events_match_dated_schemas() {
    let mut failed = ResponsesStreamFormatter::new();
    let mut failed_events = failed.format_deltas(&[AiStreamDelta::StreamError {
        error: crate::protocol::ir::AiError::new(
            crate::protocol::ir::AiErrorKind::StreamMidError,
            "upstream failed",
        ),
    }]);
    failed_events.extend(failed.format_done());
    assert_valid_events(failed_events);

    let mut incomplete = ResponsesStreamFormatter::new();
    let mut incomplete_events = incomplete.format_deltas(&[
        AiStreamDelta::MessageStart {
            id: "resp_incomplete".into(),
            model: "logical-model".into(),
        },
        AiStreamDelta::Done {
            stop_reason: "length".into(),
        },
    ]);
    incomplete_events.extend(incomplete.format_done());
    assert_valid_events(incomplete_events);
}
