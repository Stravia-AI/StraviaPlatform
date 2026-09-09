use crate::protocol::codec::anthropic::messages::decoder::AnthropicDecoder;
use crate::protocol::codec::anthropic::messages::encoder::AnthropicEncoder;
use crate::protocol::codec::anthropic::messages::stream::AnthropicResponseFormatter;
use crate::protocol::codec::google::gemini::encoder::GoogleEncoder;
use crate::protocol::codec::google::gemini::stream::GoogleStreamFormatter;
use crate::protocol::codec::open_responses::decoder::ResponsesDecoder;
use crate::protocol::codec::open_responses::encoder::ResponsesEncoder;
use crate::protocol::codec::open_responses::formatter::ResponsesResponseFormatter;
use crate::protocol::codec::open_responses::parser::{
    ResponsesResponseParser, ResponsesStreamParser,
};
use crate::protocol::codec::openai::compatible::encoder::OpenAIEncoder;
use crate::protocol::codec::openai::compatible::stream::OpenAIStreamFormatter;
use crate::protocol::codec::reasoning::normalize_response_reasoning;
use crate::protocol::codec::tool_correlation::normalize_request_tool_results;
use crate::protocol::transform::ProtocolTransform;
use serde_json::{Value, json};
use stravia_runtime_contract::protocol::ids::ANTHROPIC_MESSAGES_2023_06_01;
use stravia_runtime_contract::protocol::ids::BEDROCK_CONVERSE_V1;
use stravia_runtime_contract::protocol::ids::COHERE_CHAT_V2;
use stravia_runtime_contract::protocol::ids::GATEWAY_LANGUAGE_MODEL_V4;
use stravia_runtime_contract::protocol::ids::GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA;
use stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24;
use stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1;
use stravia_runtime_contract::protocol::ids::WATSONX_TEXT_CHAT_V1;
use stravia_runtime_contract::protocol::ir::AiItem;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::AiResponse as IrAiResponse;
use stravia_runtime_contract::protocol::ir::AiStreamDelta as IrStreamDelta;
use stravia_runtime_contract::protocol::ir::ContentBlock as IrContentBlock;
use stravia_runtime_contract::protocol::ir::MediaSource;
use stravia_runtime_contract::protocol::ir::MessageContent as IrMessageContent;
use stravia_runtime_contract::protocol::ir::Role as IrRole;
use stravia_runtime_contract::protocol::ir::StreamConfig;
use stravia_runtime_contract::protocol::ir::ToolCall;
use stravia_runtime_contract::protocol::ir::ToolSpec;
use stravia_runtime_contract::protocol::ir::usage::Usage;

fn responses_request(messages: Vec<AiItem>, stream: bool) -> AiRequest {
    let mut req = AiRequest::new("gpt-5.4", messages);
    req.stream = StreamConfig {
        enabled: stream,
        include_usage: false,
    };
    req.meta.source_protocol = Some(OPEN_RESPONSES_2026_04_24);
    req
}
fn responses_sse_event(event: &str, sequence_number: u64, payload: Value) -> String {
    let mut body = payload.as_object().expect("SSE payload object").clone();
    body.insert("type".into(), Value::String(event.to_owned()));
    body.insert("sequence_number".into(), sequence_number.into());
    format!("event: {event}\ndata: {}\n\n", Value::Object(body))
}

mod cross_protocol;
mod gemini;
mod open_responses;
mod thinking_markers;
