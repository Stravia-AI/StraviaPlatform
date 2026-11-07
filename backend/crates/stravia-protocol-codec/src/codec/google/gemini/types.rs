use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── Top-level request ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize)]
pub struct GoogleRequest {
    pub contents: Vec<GoogleContent>,
    #[serde(rename = "systemInstruction", skip_serializing_if = "Option::is_none")]
    pub system_instruction: Option<GoogleContent>,
    #[serde(rename = "generationConfig", skip_serializing_if = "Option::is_none")]
    pub generation_config: Option<GoogleGenerationConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<GoogleToolEntry>>,
    #[serde(
        rename = "toolConfig",
        default,
        deserialize_with = "present_json",
        skip_serializing_if = "Option::is_none"
    )]
    pub tool_config: Option<Value>,
}

// ── Content / parts ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize)]
pub struct GoogleContent {
    pub role: Option<String>,
    pub parts: Vec<GooglePart>,
}

/// All known Gemini Part shapes.  `Other` is a catch-all so the decoder never
/// rejects unknown part types introduced in future API versions.
#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum GooglePart {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thought: Option<bool>,
        #[serde(
            rename = "thoughtSignature",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        thought_signature: Option<String>,
    },
    InlineData {
        #[serde(rename = "inlineData")]
        inline_data: GoogleInlineData,
    },
    FileData {
        #[serde(rename = "fileData")]
        file_data: GoogleFileData,
    },
    FunctionCall {
        #[serde(rename = "functionCall")]
        function_call: GoogleFunctionCall,
        #[serde(
            rename = "thoughtSignature",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        thought_signature: Option<String>,
    },
    FunctionResponse {
        #[serde(rename = "functionResponse")]
        function_response: GoogleFunctionResponse,
    },
    ExecutableCode {
        #[serde(rename = "executableCode")]
        executable_code: GoogleExecutableCode,
    },
    CodeExecutionResult {
        #[serde(rename = "codeExecutionResult")]
        code_execution_result: GoogleCodeExecutionResult,
    },
    /// Passthrough for any unrecognised part shape (e.g. `thought`, video).
    Other(Value),
}

// ── Part subtypes ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize)]
pub struct GoogleInlineData {
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    pub data: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct GoogleFileData {
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(rename = "fileUri")]
    pub file_uri: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GoogleFunctionCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    pub args: Value,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct GoogleFunctionResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    pub response: Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<GooglePart>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct GoogleExecutableCode {
    pub language: Option<String>,
    pub code: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct GoogleCodeExecutionResult {
    pub outcome: Option<String>,
    pub output: Option<String>,
}

// ── generationConfig ──────────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct GoogleGenerationConfig {
    // Core fields shared with InternalRequest ──────────────────────────────────
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(rename = "maxOutputTokens", skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(rename = "topP", skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,

    // PR-11 additions ─────────────────────────────────────────────────────────
    #[serde(rename = "topK", skip_serializing_if = "Option::is_none")]
    pub top_k: Option<f64>,
    #[serde(rename = "candidateCount", skip_serializing_if = "Option::is_none")]
    pub candidate_count: Option<u32>,
    #[serde(rename = "stopSequences", skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(rename = "responseMimeType", skip_serializing_if = "Option::is_none")]
    pub response_mime_type: Option<String>,
    #[serde(rename = "responseModalities", skip_serializing_if = "Option::is_none")]
    pub response_modalities: Option<Vec<String>>,
    #[serde(
        rename = "responseSchema",
        default,
        deserialize_with = "present_json",
        skip_serializing_if = "Option::is_none"
    )]
    pub response_schema: Option<Value>,
    #[serde(
        rename = "responseJsonSchema",
        default,
        deserialize_with = "present_json",
        skip_serializing_if = "Option::is_none"
    )]
    pub response_json_schema: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    #[serde(rename = "presencePenalty", skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    #[serde(rename = "frequencyPenalty", skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    #[serde(rename = "responseLogprobs", skip_serializing_if = "Option::is_none")]
    pub response_logprobs: Option<bool>,
    #[serde(rename = "logprobs", skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<i32>,
    /// Gemini 2.5 thinking budget: `{ "thinkingBudget": N }`.
    #[serde(rename = "thinkingConfig", skip_serializing_if = "Option::is_none")]
    pub thinking_config: Option<Value>,
    #[serde(
        rename = "speechConfig",
        default,
        deserialize_with = "present_json",
        skip_serializing_if = "Option::is_none"
    )]
    pub speech_config: Option<Value>,
}

fn present_json<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

// Normalize the two official schema dialects without resolving references. In
// particular, visiting only schema-bearing keywords leaves property names and
// literal enum values untouched.
pub(super) fn normalize_json_schema(mut schema: Value) -> Value {
    fn pointer_key(key: &str) -> String {
        key.replace('~', "~0").replace('/', "~1")
    }

    fn normalize(value: &mut Value, path: &str, refs: &mut Vec<(String, String)>) {
        let Some(map) = value.as_object_mut() else {
            return;
        };
        if let Some(Value::String(kind)) = map.get_mut("type") {
            *kind = kind.to_ascii_lowercase();
        }
        if map.remove("nullable").and_then(|v| v.as_bool()) == Some(true) {
            if let Some(kind) = map.get_mut("type") {
                match kind {
                    Value::Array(kinds) => {
                        if !kinds.iter().any(|kind| kind == "null") {
                            kinds.push(Value::String("null".into()));
                        }
                    }
                    Value::String(name) if name != "null" => {
                        let original = std::mem::take(kind);
                        *kind = Value::Array(vec![original, Value::String("null".into())]);
                    }
                    _ => {}
                }
            } else {
                let original = Value::Object(std::mem::take(map));
                map.insert(
                    "anyOf".into(),
                    serde_json::json!([original, {"type": "null"}]),
                );
            }
        }
        // OpenAPI Schema represents these int64 fields as decimal strings.
        for key in ["minItems", "maxItems"] {
            if let Some(value) = map.get_mut(key)
                && let Some(number) = value.as_str().and_then(|s| s.parse::<u64>().ok())
            {
                *value = number.into();
            }
        }
        if let Some(Value::Object(definitions)) = map.remove("definitions") {
            let defs = map.entry("$defs").or_insert_with(|| serde_json::json!({}));
            if let Some(defs) = defs.as_object_mut() {
                for (key, definition) in definitions {
                    let mut target = key.clone();
                    while defs.contains_key(&target) {
                        target.insert(0, '_');
                    }
                    refs.push((
                        format!("#{path}/definitions/{}", pointer_key(&key)),
                        format!("#{path}/$defs/{}", pointer_key(&target)),
                    ));
                    defs.insert(target, definition);
                }
            }
        }
        for key in ["properties", "$defs"] {
            if let Some(Value::Object(children)) = map.get_mut(key) {
                for (name, child) in children {
                    normalize(child, &format!("{path}/{key}/{}", pointer_key(name)), refs);
                }
            }
        }
        for key in ["items", "additionalProperties"] {
            if let Some(child) = map.get_mut(key) {
                normalize(child, &format!("{path}/{key}"), refs);
            }
        }
        for key in ["prefixItems", "anyOf", "oneOf", "allOf"] {
            if let Some(Value::Array(children)) = map.get_mut(key) {
                for (index, child) in children.iter_mut().enumerate() {
                    normalize(child, &format!("{path}/{key}/{index}"), refs);
                }
            }
        }
    }

    fn rewrite_refs(value: &mut Value, refs: &[(String, String)]) {
        match value {
            Value::Object(map) => {
                if let Some(Value::String(reference)) = map.get_mut("$ref") {
                    while let Some((from, to)) = refs.iter().find(|(from, _)| {
                        reference.as_str() == from.as_str()
                            || reference
                                .strip_prefix(from.as_str())
                                .is_some_and(|suffix| suffix.starts_with('/'))
                    }) {
                        *reference = format!("{to}{}", &reference[from.len()..]);
                    }
                }
                for key in ["properties", "$defs"] {
                    if let Some(Value::Object(children)) = map.get_mut(key) {
                        for child in children.values_mut() {
                            rewrite_refs(child, refs);
                        }
                    }
                }
                for key in [
                    "items",
                    "additionalProperties",
                    "prefixItems",
                    "anyOf",
                    "oneOf",
                    "allOf",
                ] {
                    if let Some(child) = map.get_mut(key) {
                        rewrite_refs(child, refs);
                    }
                }
            }
            Value::Array(children) => {
                for child in children {
                    rewrite_refs(child, refs);
                }
            }
            _ => {}
        }
    }

    let mut refs = Vec::new();
    normalize(&mut schema, "", &mut refs);
    if !refs.is_empty() {
        // A reference to a nested definition takes precedence over its parent.
        refs.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
        rewrite_refs(&mut schema, &refs);
    }
    schema
}

// ── Tools ─────────────────────────────────────────────────────────────────────

/// A single entry in the `tools` array.  May contain function declarations
/// and/or built-in tool flags such as `googleSearch` and `codeExecution`.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct GoogleToolEntry {
    #[serde(
        rename = "functionDeclarations",
        skip_serializing_if = "Option::is_none"
    )]
    pub function_declarations: Option<Vec<GoogleFunctionDecl>>,
    #[serde(
        rename = "googleSearch",
        default,
        deserialize_with = "present_json",
        skip_serializing_if = "Option::is_none"
    )]
    pub google_search: Option<Value>,
    #[serde(
        rename = "codeExecution",
        default,
        deserialize_with = "present_json",
        skip_serializing_if = "Option::is_none"
    )]
    pub code_execution: Option<Value>,
    #[serde(
        rename = "googleSearchRetrieval",
        default,
        deserialize_with = "present_json",
        skip_serializing_if = "Option::is_none"
    )]
    pub google_search_retrieval: Option<Value>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct GoogleFunctionDecl {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(
        default,
        deserialize_with = "present_json",
        skip_serializing_if = "Option::is_none"
    )]
    pub parameters: Option<Value>,
    #[serde(
        rename = "parametersJsonSchema",
        default,
        deserialize_with = "present_json",
        skip_serializing_if = "Option::is_none"
    )]
    pub parameters_json_schema: Option<Value>,
}
