pub mod adapter;
pub mod decoder;
pub mod encoder;
pub mod formatter;
pub mod parser;
pub mod stream;

use stravia_runtime_contract::protocol::ir::{AiError, AiErrorKind};

pub(super) fn stream_error_kind(error: &AiError) -> AiErrorKind {
    if !matches!(error.kind, AiErrorKind::StreamMidError) {
        return error.kind.clone();
    }
    if let Some(status) = error.status_code.filter(|status| *status >= 400) {
        return AiError::kind_from_status(status, error.raw.as_ref());
    }
    let Some(upstream) = error
        .raw
        .as_ref()
        .and_then(|raw| raw.pointer("/response/error").or_else(|| raw.get("error")))
    else {
        return error.kind.clone();
    };
    if AiError::kind_from_status(429, upstream.get("code")) == AiErrorKind::QuotaExceeded {
        return AiErrorKind::QuotaExceeded;
    }
    for discriminator in [upstream.get("type"), upstream.get("code")]
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
    {
        let classified = match discriminator {
            "authentication_error" => Some(AiErrorKind::AuthenticationError),
            "authorization_error" | "permission_error" | "permission_denied" => {
                Some(AiErrorKind::AuthorizationError)
            }
            "not_found" | "not_found_error" | "model_not_found" => Some(AiErrorKind::NotFoundError),
            "rate_limit_error" | "too_many_requests" => Some(AiErrorKind::RateLimitError),
            "quota_exceeded" => Some(AiErrorKind::QuotaExceeded),
            "invalid_request" | "invalid_request_error" | "protocol_lossy_rejected" => {
                Some(AiErrorKind::InvalidRequest)
            }
            "server_error" | "model_error" => Some(AiErrorKind::ServerError),
            "service_unavailable" => Some(AiErrorKind::ServiceUnavailable),
            "timeout" => Some(AiErrorKind::Timeout),
            "content_filtered" => Some(AiErrorKind::ContentFiltered),
            _ => None,
        };
        if let Some(kind) = classified {
            return kind;
        }
    }
    AiErrorKind::Unknown
}

const REGISTERED_EXTENSION_ITEM_TYPES: &[&str] = &["stravia:agent_result", "stravia:media_result"];

pub(crate) fn is_registered_extension_item(item_type: &str) -> bool {
    REGISTERED_EXTENSION_ITEM_TYPES.contains(&item_type)
}

pub(super) fn is_hosted_image_generation_item(item_type: &str) -> bool {
    item_type == "image_generation_call"
}

pub fn hosted_image_generation_requested(
    request: &stravia_runtime_contract::protocol::ir::AiRequest,
) -> bool {
    matches!(
        request.ext.as_ref(),
        Some(stravia_runtime_contract::protocol::ir::ProtocolExt::OpenResponses(extension))
            if extension.passthrough_tools.iter().any(|tool| {
                tool.get("type").and_then(serde_json::Value::as_str)
                    == Some("image_generation")
            })
    )
}

pub(super) fn validate_hosted_image_generation_item(
    item: &serde_json::Value,
    require_completed: bool,
) -> anyhow::Result<()> {
    let object = item
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("image generation output item must be an object"))?;
    if object.get("type").and_then(serde_json::Value::as_str) != Some("image_generation_call") {
        anyhow::bail!("image generation output item has an invalid type");
    }
    if object
        .get("id")
        .and_then(serde_json::Value::as_str)
        .is_none_or(str::is_empty)
    {
        anyhow::bail!("image generation output item is missing id");
    }
    let status = object.get("status").and_then(serde_json::Value::as_str);
    if !matches!(
        status,
        Some("in_progress" | "generating" | "completed" | "failed")
    ) {
        anyhow::bail!("image generation output item has an invalid status");
    }
    if require_completed && status != Some("completed") {
        anyhow::bail!("image generation output item did not complete");
    }
    if status == Some("completed")
        && object
            .get("result")
            .and_then(serde_json::Value::as_str)
            .is_none_or(str::is_empty)
    {
        anyhow::bail!("completed image generation output item has no result");
    }
    if let Some(result) = object.get("result")
        && !result.is_null()
        && !result.is_string()
    {
        anyhow::bail!("image generation output item result must be a string or null");
    }
    Ok(())
}

pub(super) fn validate_extension_item(
    item: &serde_json::Value,
    require_completed: bool,
) -> anyhow::Result<()> {
    let object = item
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("extension item must be an object"))?;
    let item_type = object
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("extension item type is missing"))?;
    if !is_registered_extension_item(item_type) {
        anyhow::bail!("unregistered Open Responses output extension: {item_type}");
    }
    let required = match item_type {
        "stravia:agent_result" => &["id", "type", "status", "turn_id"][..],
        "stravia:media_result" => &["id", "type", "status", "turn_id", "completion"][..],
        _ => unreachable!("registered extension item lacks a schema"),
    };
    for field in required {
        if !object.contains_key(*field) {
            anyhow::bail!("{item_type} missing required field '{field}'");
        }
    }
    let status = object.get("status").and_then(serde_json::Value::as_str);
    if require_completed && status != Some("completed") {
        anyhow::bail!("{item_type} final status must be completed");
    }
    if !matches!(status, Some("in_progress" | "completed")) {
        anyhow::bail!("{item_type} has an invalid status");
    }
    for field in ["id", "turn_id", "completion", "data", "media_type"] {
        if let Some(value) = object.get(field)
            && value.as_str().is_none_or(str::is_empty)
        {
            anyhow::bail!("{item_type} field '{field}' must be a non-empty string");
        }
    }
    Ok(())
}

use stravia_runtime_contract::protocol::ir::canonical::native_compaction_item;

/// OpenAI server-side compaction permits dropping input/output before the latest
/// newly emitted state (https://developers.openai.com/api/docs/guides/compaction).
/// Callers supply registration receipts: replayed input and standalone windows
/// must never activate this rule merely because they contain encrypted content.
pub fn inline_compaction_boundary(
    items: &[stravia_runtime_contract::protocol::ir::AiItem],
    fresh_states: &[stravia_runtime_contract::protocol::ir::AiItem],
) -> Option<usize> {
    items.iter().rposition(|item| {
        item.is_compaction()
            && native_compaction_item(item).is_some_and(|wire| {
                fresh_states
                    .iter()
                    .any(|state| native_compaction_item(state).as_ref() == Some(&wire))
            })
    })
}

fn is_namespaced_extension(value: &str) -> bool {
    value.contains(':')
}

#[cfg(test)]
mod schema_tests;
