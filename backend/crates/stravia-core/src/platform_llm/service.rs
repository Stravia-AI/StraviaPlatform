use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use stravia_runtime_contract::model_turn::{CanonicalEvent, ModelTurnError};
use stravia_runtime_contract::protocol::ir::{AiItem, AiRequest, MessageContent, Role};
use stravia_runtime_contract::{Deadline, Principal};

use super::config::resolve_active_route;
use super::{
    ConversationTitleInput, ConversationTitleResult, PlatformLlmError, PlatformLlmTask,
    PlatformLlmTaskSpec,
};
use crate::Gateway;
use crate::model_turn::{ModelTurnAuthorization, TurnInput};

const TITLE_TIMEOUT: Duration = Duration::from_secs(20);
const TITLE_MAX_TOKENS: u32 = 64;
const TITLE_INPUT_CHAR_LIMIT: usize = 1500;
const TITLE_OUTPUT_CHAR_LIMIT: usize = 80;
const TITLE_OUTPUT_WORD_LIMIT: usize = 12;

fn title_language_instruction(locale: Option<&str>) -> String {
    let Some(locale) = locale.map(str::trim).filter(|value| !value.is_empty()) else {
        return "Use the same language as the user's message.".to_owned();
    };
    let lower = locale.to_ascii_lowercase();
    if lower == "zh-tw" || lower == "zh-hk" || lower.starts_with("zh-hant") {
        "Write the title in Traditional Chinese.".to_owned()
    } else if lower == "zh" || lower.starts_with("zh-") {
        "Write the title in Simplified Chinese.".to_owned()
    } else if lower == "en" || lower.starts_with("en-") {
        "Write the title in English.".to_owned()
    } else {
        format!("Write the title in the language for locale `{locale}`.")
    }
}

fn conversation_title_instructions(locale: Option<&str>) -> String {
    format!(
        "Generate a concise, specific title for the conversation below, wrapped in <title>...</title>. \
{} \
Keep it brief (under 10 words or 24 Chinese characters), single line, with no quotes, markdown, or trailing punctuation. \
If the conversation has no concrete topic yet (for example a bare greeting or connectivity check), output <title/>.",
        title_language_instruction(locale)
    )
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_owned();
    }
    trimmed.chars().take(max_chars).collect()
}

fn extract_tagged_or_json_title(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.contains("<title/>") || lower.contains("<title />") {
        return None;
    }
    if let Some(start) = lower.find("<title>") {
        let content_start = start + "<title>".len();
        let rest = &trimmed[content_start..];
        let rest_lower = &lower[content_start..];
        let extracted = match rest_lower.find("</title>") {
            Some(end) => &rest[..end],
            None => rest,
        };
        return Some(extracted.to_owned());
    }
    if trimmed.starts_with('{')
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed)
        && let Some(title) = value.get("title").and_then(serde_json::Value::as_str)
    {
        return Some(title.to_owned());
    }
    Some(trimmed.to_owned())
}

pub fn sanitize_title(raw: &str) -> Option<String> {
    let candidate = extract_tagged_or_json_title(raw)?;
    let first_line = candidate
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let without_heading = first_line.trim_start_matches('#').trim();
    let without_label = without_heading
        .strip_prefix("Title:")
        .or_else(|| without_heading.strip_prefix("标题："))
        .or_else(|| without_heading.strip_prefix("标题:"))
        .map(str::trim)
        .unwrap_or(without_heading);
    let is_wrapper = |ch: char| {
        matches!(
            ch,
            '"' | '\'' | '`' | '“' | '”' | '‘' | '’' | '《' | '》' | '「' | '」' | '*' | '_'
        )
    };
    let is_trailing_punct =
        |ch: char| matches!(ch, '.' | '。' | '!' | '！' | ':' | '：' | ';' | '；');
    let stripped = without_label
        .trim_end_matches(is_trailing_punct)
        .trim()
        .trim_matches(is_wrapper)
        .trim_end_matches(is_trailing_punct)
        .trim_matches(is_wrapper);
    let collapsed = stripped.split_whitespace().collect::<Vec<_>>().join(" ");
    let cleaned = collapsed.trim();
    if cleaned.is_empty() || cleaned.eq_ignore_ascii_case("none") {
        return None;
    }
    if cleaned.chars().count() > TITLE_OUTPUT_CHAR_LIMIT
        || cleaned.split_whitespace().count() > TITLE_OUTPUT_WORD_LIMIT
    {
        return None;
    }
    Some(cleaned.to_owned())
}

impl PlatformLlmTask {
    pub fn to_spec(&self) -> Result<PlatformLlmTaskSpec, PlatformLlmError> {
        match self {
            PlatformLlmTask::ConversationTitle {
                user_message,
                assistant_message,
                locale,
                thinking_level,
            } => {
                let user = truncate_chars(user_message, TITLE_INPUT_CHAR_LIMIT);
                if user.is_empty() {
                    return Err(PlatformLlmError::new(
                        "platform_llm_input_invalid",
                        "user_message cannot be empty",
                    ));
                }
                let assistant = assistant_message
                    .as_deref()
                    .map(|text| truncate_chars(text, TITLE_INPUT_CHAR_LIMIT))
                    .filter(|text| !text.is_empty());
                let input = match assistant {
                    Some(reply) => format!("User:\n{user}\n\nAssistant:\n{reply}"),
                    None => format!("User:\n{user}"),
                };
                Ok(PlatformLlmTaskSpec {
                    instructions: conversation_title_instructions(locale.as_deref()),
                    input,
                    max_tokens: Some(TITLE_MAX_TOKENS),
                    thinking_level: *thinking_level,
                    timeout: TITLE_TIMEOUT,
                })
            }
        }
    }
}

fn map_model_turn_error(error: ModelTurnError) -> PlatformLlmError {
    let code = match error.code.as_str() {
        "unauthorized" | "invalid_api_key" | "authentication_error" | "forbidden" => {
            "platform_llm_unauthorized"
        }
        "model_not_found" => "platform_llm_route_unavailable",
        _ => "platform_llm_execution_failed",
    };
    PlatformLlmError::new(code, error.message)
}

/// Execute a platform-internal text task using the configured platform model.
pub async fn execute_text_task(
    gateway: &Gateway,
    principal: Principal,
    spec: PlatformLlmTaskSpec,
) -> Result<String, PlatformLlmError> {
    let route = resolve_active_route(gateway).await?;

    let user_item = AiItem {
        role: Role::User,
        content: MessageContent::Text(Arc::new(spec.input)),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    };
    let mut request = AiRequest::new(route.id.as_str(), vec![user_item]);
    request.instructions = Some(spec.instructions);
    request.generation.max_tokens = spec.max_tokens;
    request.tools = None;
    request.parallel_tool_calls = None;
    // Only override reasoning.level when the task specifies one; `None` lets the
    // bound Route's `default_thinking_level` apply unchanged.
    if let Some(level) = spec.thinking_level {
        request.reasoning.level = Some(level);
    }

    let mut turn_input = TurnInput::new(principal, request);
    turn_input.authorization = ModelTurnAuthorization::CapabilityGrant;
    let deadline = Deadline::from_now(spec.timeout);
    turn_input.deadline = deadline.clone();

    let turn = gateway
        .model_turn
        .execute(turn_input)
        .await
        .map_err(map_model_turn_error)?;

    let mut stream = turn.output;
    let mut final_text = None;
    loop {
        let next_event = tokio::select! {
            () = deadline.wait() => {
                return Err(PlatformLlmError::new(
                    "platform_llm_execution_failed",
                    "Platform LLM task timed out",
                ));
            }
            event = stream.next() => event,
        };
        let Some(event) = next_event else {
            break;
        };
        match event.map_err(map_model_turn_error)? {
            CanonicalEvent::Delta(_) => {}
            CanonicalEvent::Compacted(_) => {
                return Err(PlatformLlmError::new(
                    "platform_llm_execution_failed",
                    "Unexpected compaction result from platform LLM task",
                ));
            }
            CanonicalEvent::Completed(response) => {
                final_text = Some(response.output_text());
            }
        }
    }

    final_text.ok_or_else(|| {
        PlatformLlmError::new(
            "platform_llm_execution_failed",
            "Platform LLM task ended without a completed response",
        )
    })
}

pub async fn generate_conversation_title(
    gateway: &Gateway,
    input: ConversationTitleInput,
) -> Result<ConversationTitleResult, PlatformLlmError> {
    let api_key_id = input.api_key_id.trim();
    if api_key_id.is_empty() {
        return Err(PlatformLlmError::new(
            "platform_llm_input_invalid",
            "api_key_id cannot be empty",
        ));
    }
    let task = PlatformLlmTask::ConversationTitle {
        user_message: input.user_message,
        assistant_message: input.assistant_message,
        locale: input.locale,
        thinking_level: input.thinking_level,
    };
    let spec = task.to_spec()?;
    let raw = execute_text_task(gateway, Principal::new(api_key_id.to_owned()), spec).await?;
    Ok(ConversationTitleResult {
        title: sanitize_title(&raw),
    })
}
