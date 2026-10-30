//! Request-only adaptation of imported Gemini tool history.

use serde_json::Value;

/// Recognizes Gemini 3 model names, including numeric minor versions, not aliases.
/// The caller must additionally authorize the endpoint's import-marker support.
pub fn supports_imported_tool_history(model: &str) -> bool {
    let Some(version_suffix) = model
        .strip_prefix("models/")
        .unwrap_or(model)
        .strip_prefix("gemini-3")
    else {
        return false;
    };
    if let Some(name) = version_suffix.strip_prefix('-') {
        !name.is_empty()
    } else if let Some((minor, name)) = version_suffix
        .strip_prefix('.')
        .and_then(|suffix| suffix.split_once('-'))
    {
        !minor.is_empty() && minor.bytes().all(|byte| byte.is_ascii_digit()) && !name.is_empty()
    } else {
        false
    }
}

/// Adds Google's import control to the first unsigned call of each current-turn
/// model step. Existing signatures and parallel-call associations are preserved.
/// The caller must authorize the model and endpoint; this mutates only wire data,
/// never canonical history, and does not claim to recover a native signature.
pub fn prepare_imported_tool_history(body: &mut Value) {
    let Some(contents) = body.get_mut("contents").and_then(Value::as_array_mut) else {
        return;
    };
    // Tool results use the user role too, but do not start a new user turn.
    let start = contents
        .iter()
        .rposition(|content| {
            content.get("role").and_then(Value::as_str) == Some("user")
                && content
                    .get("parts")
                    .and_then(Value::as_array)
                    .is_some_and(|parts| {
                        parts.iter().any(|part| {
                            part.as_object().is_some_and(|fields| {
                                fields.keys().any(|key| {
                                    matches!(
                                        key.as_str(),
                                        "text" | "inlineData" | "fileData" | "videoMetadata"
                                    )
                                })
                            })
                        })
                    })
        })
        .map_or(0, |index| index + 1);
    for content in &mut contents[start..] {
        if content.get("role").and_then(Value::as_str) != Some("model") {
            continue;
        }
        let Some(parts) = content.get_mut("parts").and_then(Value::as_array_mut) else {
            continue;
        };
        let Some(first_call) = parts
            .iter_mut()
            .find(|part| part.get("functionCall").is_some())
        else {
            continue;
        };
        let missing = match first_call.get("thoughtSignature") {
            None | Some(Value::Null) => true,
            Some(Value::String(signature)) => signature.is_empty(),
            _ => false,
        };
        if missing && let Some(fields) = first_call.as_object_mut() {
            fields.insert(
                "thoughtSignature".into(),
                Value::String("skip_thought_signature_validator".into()),
            );
        }
    }
}
