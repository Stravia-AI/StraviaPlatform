//! Request-only adaptation of imported Gemini tool history.

use serde_json::Value;

/// Recognizes Gemini 3+ versioned model names (`gemini-<major>-<name>` and
/// `gemini-<major>.<minor>-<name>` with `major >= 3`), excluding older
/// generations and unversioned aliases. The caller must additionally authorize
/// the endpoint's import-marker support.
pub fn supports_imported_tool_history(model: &str) -> bool {
    let Some(rest) = model
        .strip_prefix("models/")
        .unwrap_or(model)
        .strip_prefix("gemini-")
    else {
        return false;
    };
    let Some((version, name)) = rest.split_once('-') else {
        return false;
    };
    if name.trim().is_empty() {
        return false;
    }
    let (major, minor) = match version.split_once('.') {
        Some((major, minor)) => (major, Some(minor)),
        None => (version, None),
    };
    if major.is_empty()
        || major.starts_with('0')
        || !major.bytes().all(|byte| byte.is_ascii_digit())
    {
        return false;
    }
    let Ok(major) = major.parse::<u32>() else {
        return false;
    };
    if major < 3 {
        return false;
    }
    minor.is_none_or(|minor| !minor.is_empty() && minor.bytes().all(|byte| byte.is_ascii_digit()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn recognizes_gemini_3_and_newer_versioned_models_only() {
        for model in [
            "gemini-3-pro",
            "gemini-3-flash-preview",
            "gemini-3.1-pro-preview",
            "gemini-3.8-flash",
            "gemini-3.8-flash-high",
            "gemini-4-neon",
            "gemini-4-hydrogen",
            "gemini-4-hydrogen-medium",
            "gemini-4-carbon",
            "gemini-4-argon",
            "gemini-4.0-pro",
            "gemini-4.1-pro-preview",
            "models/gemini-3-pro-preview",
            "models/gemini-4-neon",
            "models/gemini-4.1-pro",
            "gemini-5-pro",
            "gemini-10.2-flash",
        ] {
            assert!(supports_imported_tool_history(model), "expected {model}");
        }
        for model in [
            "",
            "models/",
            "gemini-2.5-pro",
            "gemini-2.5-flash",
            "gemini-2.0-flash",
            "gemini-1.5-pro",
            "gemini-0-pro",
            "gemini-03-pro",
            "gemini-04-neon",
            "gemini-flash-latest",
            "gemini-pro-latest",
            "gemini-pro-agent",
            "gemini-exp-1206",
            "gemini-embedding-2",
            "gemini-robotics-er-2-preview",
            "gemini-3",
            "gemini-4",
            "gemini-3-",
            "gemini-4-",
            "gemini-4- ",
            "gemini-3.1",
            "gemini-3.-pro",
            "gemini-3.x-pro",
            "gemini-4.-neon",
            "gemini-4.x-neon",
            "gemini-4.1.2-pro",
            "gemini--pro",
            "gemini-.1-pro",
            "gemini-99999999999999999999-pro",
            "models/models/gemini-4-neon",
            "claude-sonnet-4-6",
            "account-model",
        ] {
            assert!(!supports_imported_tool_history(model), "rejected {model}");
        }
    }

    #[test]
    fn prepares_current_turn_unsigned_calls_including_folded_tool_continuation() {
        let mut body = json!({
            "contents": [
                {"role": "user", "parts": [{"text": "earlier turn"}]},
                {"role": "model", "parts": [{"functionCall": {"id": "old", "name": "read", "args": {}}}]},
                {"role": "user", "parts": [{"functionResponse": {"id": "old", "name": "read", "response": {"result": "ok"}}}]},
                {"role": "model", "parts": [{"text": "earlier answer"}]},
                {"role": "user", "parts": [{"text": "current task"}]},
                {"role": "model", "parts": [
                    {"functionCall": {"id": "c1", "name": "read", "args": {"path": "a.rs"}}},
                    {"functionCall": {"id": "c2", "name": "read", "args": {"path": "b.rs"}}}
                ]},
                {"role": "user", "parts": [
                    {"functionResponse": {"id": "c1", "name": "read", "response": {"result": "a"}}},
                    {"functionResponse": {"id": "c2", "name": "read", "response": {"result": "b"}}}
                ]},
                {"role": "model", "parts": [
                    {"functionCall": {"id": "c3", "name": "edit", "args": {}}, "thoughtSignature": "native-sig"}
                ]},
                {"role": "user", "parts": [
                    {"functionResponse": {"id": "c3", "name": "edit", "response": {"result": "done"}}}
                ]},
                {"role": "model", "parts": [
                    {"functionCall": {"id": "c4", "name": "test", "args": {}}, "thoughtSignature": ""}
                ]},
                {"role": "user", "parts": [
                    {"functionResponse": {"id": "c4", "name": "test", "response": {"result": "interrupted\n\n[user follow-up folded into tool result]"}}}
                ]}
            ]
        });
        let mut expected = body.clone();
        expected["contents"][5]["parts"][0]["thoughtSignature"] =
            json!("skip_thought_signature_validator");
        expected["contents"][9]["parts"][0]["thoughtSignature"] =
            json!("skip_thought_signature_validator");

        prepare_imported_tool_history(&mut body);

        assert_eq!(body, expected);
    }
}
