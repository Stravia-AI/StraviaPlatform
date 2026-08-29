pub mod adapter;
pub mod decoder;
pub mod encoder;
pub mod formatter;
pub mod parser;
pub mod stream;

const REGISTERED_EXTENSION_ITEM_TYPES: &[&str] = &["stravia:agent_result", "stravia:media_result"];

pub(crate) fn is_registered_extension_item(item_type: &str) -> bool {
    REGISTERED_EXTENSION_ITEM_TYPES.contains(&item_type)
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

fn is_namespaced_extension(value: &str) -> bool {
    value.contains(':')
}

#[cfg(test)]
mod schema_tests;
