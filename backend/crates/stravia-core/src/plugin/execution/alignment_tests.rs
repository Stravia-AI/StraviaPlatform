//! Explicitly invoked, isolated live vendor alignment probe. Secrets enter only on stdin.

use super::*;
use crate::config::GatewayConfig;
use crate::db::models::{CreateProviderRecord, UpsertOAuthCredential};
use crate::plugin::ConfirmPluginUpdate;
use crate::provider_models::{
    NewProviderModelRecord, ProviderModelMetadata, ProviderModelPresence,
    ProviderModelSelectionPolicy, ProviderModelSourceKind, SnapshotState,
};
use serde::Deserialize;
use serde_json::json;
use std::io::{Read, Write};
use std::time::Duration;

#[derive(Deserialize)]
struct Input {
    wasm_path: String,
    plugin_id: String,
    provider: ProviderInput,
    outbound_proxy: Option<String>,
    oauth: Option<UpsertOAuthCredential>,
    model: Option<String>,
    model_metadata: Option<ProviderModelMetadata>,
    operation: String,
    input: Value,
    #[serde(default)]
    response_code_map: Option<Value>,
}

struct ResponseCodeMap {
    fields: Vec<String>,
    codes: std::collections::HashMap<i64, &'static str>,
}

impl ResponseCodeMap {
    fn parse(value: &Value) -> Result<Self, ()> {
        if serde_json::to_vec(value).map_err(|_| ())?.len() > 16 * 1024 {
            return Err(());
        }
        let map = value.as_object().ok_or(())?;
        if map.len() != 2 {
            return Err(());
        }
        let fields = map.get("fields").and_then(Value::as_array).ok_or(())?;
        if fields.len() > 4 {
            return Err(());
        }
        let mut selected = Vec::new();
        for field in fields {
            let field = field.as_str().ok_or(())?;
            if !["code", "type", "status", "error"].contains(&field)
                || selected.iter().any(|s| s == field)
            {
                return Err(());
            }
            selected.push(field.to_owned());
        }
        let entries = map.get("codes").and_then(Value::as_object).ok_or(())?;
        if entries.len() > 64 {
            return Err(());
        }
        let mut codes = std::collections::HashMap::new();
        for (key, value) in entries {
            let number = key.parse::<i64>().map_err(|_| ())?;
            if number.to_string() != *key {
                return Err(());
            }
            let category = match value.as_str().ok_or(())? {
                "unknown" => "unknown",
                "model" => "model",
                "authentication" => "authentication",
                "permission" => "permission",
                "quota" => "quota",
                "token_validation" => "token_validation",
                "session" => "session",
                "unsupported" => "unsupported",
                "login_expired" => "login_expired",
                "duplicate_request" => "duplicate_request",
                "quota_or_billing" => "quota_or_billing",
                "content_policy" => "content_policy",
                "custom_model_service" => "custom_model_service",
                "custom_model_auth" => "custom_model_auth",
                "custom_model_unavailable" => "custom_model_unavailable",
                "queued" => "queued",
                "input_limits" => "input_limits",
                "rate_limit" => "rate_limit",
                _ => return Err(()),
            };
            codes.insert(number, category);
        }
        Ok(Self {
            fields: selected,
            codes,
        })
    }
}

#[derive(Deserialize)]
struct ProviderInput {
    vendor: String,
    channel: String,
    protocol: String,
    base_url: String,
    models_source: Option<String>,
    static_models: Option<String>,
    #[serde(default)]
    api_key: String,
    #[serde(default = "empty_object")]
    adapter_credentials: Value,
    #[serde(default = "empty_object")]
    vendor_options: Value,
    auth_mode: String,
    #[serde(default)]
    use_proxy: bool,
}

struct ReportDetails {
    stage: &'static str,
    error_kind: Option<&'static str>,
    upstream_status: Option<u16>,
    operation_supported: Option<bool>,
    diagnostic_facts: DiagnosticFacts,
    selected_model_present: Option<bool>,
    model_selection_origin: Option<&'static str>,
    response_facts: Option<ResponseFacts>,
    model_error_kind: Option<stravia_runtime_contract::protocol::ir::AiErrorKind>,
    retry_after_ms: Option<u64>,
    http_observation: Option<crate::plugin::network::transport::DiagnosticObservation>,
}

impl ReportDetails {
    fn observe_error(
        &mut self,
        kind: Option<stravia_runtime_contract::protocol::ir::AiErrorKind>,
        retry: Option<Duration>,
    ) {
        use stravia_runtime_contract::protocol::ir::AiErrorKind;
        if (self.model_error_kind.is_none() || self.model_error_kind == Some(AiErrorKind::Unknown))
            && kind.is_some()
        {
            self.model_error_kind = kind;
        }
        if self.retry_after_ms.is_none() {
            self.retry_after_ms =
                retry.map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX));
        }
    }

    fn merge_events(
        &mut self,
        (facts, kind, retry): (
            DiagnosticFacts,
            Option<stravia_runtime_contract::protocol::ir::AiErrorKind>,
            Option<Duration>,
        ),
    ) {
        self.diagnostic_facts.merge(facts);
        self.observe_error(kind, retry);
    }
}

#[derive(Default, serde::Serialize)]
struct ResponseFacts {
    content_coding: &'static str,
    body_kind: &'static str,
    html: bool,
    keys: ResponseKeys,
    error_kind: &'static str,
    category: &'static str,
    truncated: bool,
}

#[derive(Default, serde::Serialize)]
struct ResponseKeys {
    error: bool,
    message: bool,
    code: bool,
    detail: bool,
    errors: bool,
    r#type: bool,
    status: bool,
    data: bool,
}

impl ResponseFacts {
    fn unknown(content_coding: &'static str, truncated: bool) -> Self {
        Self {
            content_coding,
            body_kind: "unknown",
            error_kind: "unknown",
            category: "unknown",
            truncated,
            ..Default::default()
        }
    }

    fn from_bytes(
        vendor: &str,
        policy: Option<&ResponseCodeMap>,
        bytes: &[u8],
        truncated: bool,
    ) -> Self {
        let trimmed = bytes.trim_ascii_start();
        let html = trimmed.starts_with(b"<!DOCTYPE html")
            || trimmed.starts_with(b"<!doctype html")
            || trimmed.starts_with(b"<html")
            || trimmed.starts_with(b"<HTML");
        let parsed = serde_json::from_slice::<Value>(bytes).ok();
        let mut facts = Self {
            body_kind: if bytes.is_empty() {
                "empty"
            } else if parsed.as_ref().is_some_and(Value::is_object) {
                "json_object"
            } else if parsed.is_some() {
                "json_other"
            } else {
                "non_json"
            },
            html,
            error_kind: "absent",
            category: "unknown",
            truncated,
            ..Default::default()
        };
        if let Some(Value::Object(map)) = &parsed {
            facts.keys = ResponseKeys {
                error: map.contains_key("error"),
                message: map.contains_key("message"),
                code: map.contains_key("code"),
                detail: map.contains_key("detail"),
                errors: map.contains_key("errors"),
                r#type: map.contains_key("type"),
                status: map.contains_key("status"),
                data: map.contains_key("data"),
            };
            if let Some(error) = map.get("error") {
                facts.error_kind = match error {
                    Value::Null => "null",
                    Value::Bool(_) => "boolean",
                    Value::Number(_) => "number",
                    Value::String(_) => "string",
                    Value::Array(_) => "array",
                    Value::Object(_) => "object",
                };
            }
        }
        if let Some(value) = &parsed {
            facts.category = response_category(vendor, policy, value, 0).unwrap_or("unknown");
        }
        facts
    }
}

// Captured HTTP encodings are decoded only in the isolated probe. Both input
// and output are bounded; malformed, unsupported or oversized data is unknown.
async fn decode_diagnostic_body<'a>(
    coding: &str,
    body: &'a [u8],
    truncated: bool,
) -> Option<std::borrow::Cow<'a, [u8]>> {
    use async_compression::tokio::bufread::{BrotliDecoder, GzipDecoder, ZlibDecoder, ZstdDecoder};
    use tokio::io::{AsyncRead, AsyncReadExt};
    const LIMIT: usize = 256 * 1024;
    if truncated || body.len() > LIMIT {
        return None;
    }
    if coding == "identity" {
        return Some(std::borrow::Cow::Borrowed(body));
    }
    let mut decoder: Box<dyn AsyncRead + Unpin + '_> = match coding {
        "gzip" => Box::new(GzipDecoder::new(body)),
        "deflate" => Box::new(ZlibDecoder::new(body)),
        "br" => Box::new(BrotliDecoder::new(body)),
        "zstd" => Box::new(ZstdDecoder::new(body)),
        _ => return None,
    };
    let mut decoded = Vec::with_capacity(LIMIT);
    let mut chunk = [0u8; 8192];
    while decoded.len() < LIMIT {
        let available = (LIMIT - decoded.len()).min(chunk.len());
        let count = decoder.read(&mut chunk[..available]).await.ok()?;
        if count == 0 {
            return Some(std::borrow::Cow::Owned(decoded));
        }
        decoded.extend_from_slice(&chunk[..count]);
    }
    let mut extra = [0u8; 1];
    if decoder.read(&mut extra).await.ok()? == 0 {
        Some(std::borrow::Cow::Owned(decoded))
    } else {
        None
    }
}

// Only inspect structured fields and JSON-encoded nested values. Never return an
// upstream string, and do not interpret an opaque wire encoding as a code.
fn response_category(
    vendor: &str,
    policy: Option<&ResponseCodeMap>,
    value: &Value,
    depth: usize,
) -> Option<&'static str> {
    if depth >= 8 {
        return None;
    }
    match value {
        Value::Object(map) => {
            for key in ["code", "type", "status", "error"] {
                if let Some(value) = map.get(key) {
                    let code = match value {
                        Value::String(s) => Some(s.as_str()),
                        _ => None,
                    };
                    let numeric = value
                        .as_i64()
                        .or_else(|| code.and_then(|s| s.parse::<i64>().ok()));
                    if let Some(category) = policy
                        .filter(|policy| policy.fields.iter().any(|field| field == key))
                        .and_then(|policy| numeric.and_then(|number| policy.codes.get(&number)))
                    {
                        return Some(category);
                    }
                    if vendor == "command-code" {
                        let category = match code {
                            Some("model_not_found" | "invalid_model") => Some("model"),
                            Some("authentication_error" | "invalid_api_key" | "unauthorized") => {
                                Some("authentication")
                            }
                            Some("permission_denied" | "forbidden") => Some("permission"),
                            Some("quota_exceeded" | "insufficient_quota") => Some("quota"),
                            Some("invalid_token" | "token_validation_error") => {
                                Some("token_validation")
                            }
                            Some("session_expired" | "invalid_session") => Some("session"),
                            Some("unsupported" | "unsupported_operation") => Some("unsupported"),
                            _ => None,
                        };
                        if category.is_some() {
                            return category;
                        }
                    }
                }
            }
            map.values()
                .find_map(|child| response_category(vendor, policy, child, depth + 1))
        }
        Value::Array(values) => values
            .iter()
            .find_map(|child| response_category(vendor, policy, child, depth + 1)),
        Value::String(text) if text.len() <= 256 * 1024 => serde_json::from_str::<Value>(text)
            .ok()
            .filter(|parsed| !parsed.is_string())
            .and_then(|parsed| response_category(vendor, policy, &parsed, depth + 1)),
        _ => None,
    }
}

#[derive(Default, serde::Serialize)]
struct DiagnosticFacts {
    codex_history_boundaries_unavailable: bool,
    model_not_found: bool,
    output_token_parameter: bool,
    validation: bool,
    quota: bool,
}

impl DiagnosticFacts {
    fn observe(&mut self, text: &str) {
        self.codex_history_boundaries_unavailable |=
            text == "Codex canonical history boundaries are unavailable";
        let text = text.to_ascii_lowercase();
        self.model_not_found |= text.contains("model_not_found");
        self.output_token_parameter |= [
            "max_output_tokens",
            "max_completion_tokens",
            "max_tokens",
            "maxoutputtokens",
        ]
        .iter()
        .any(|term| text.contains(term));
        self.validation |= text.contains("validation");
        self.quota |= text.contains("quota");
    }

    fn merge(&mut self, other: Self) {
        self.codex_history_boundaries_unavailable |= other.codex_history_boundaries_unavailable;
        self.model_not_found |= other.model_not_found;
        self.output_token_parameter |= other.output_token_parameter;
        self.validation |= other.validation;
        self.quota |= other.quota;
    }
}

#[test]
fn response_facts_are_closed_and_exact() {
    let policy = ResponseCodeMap::parse(
        &json!({"fields":["code"], "codes":{"42":"rate_limit", "43":"quota"}}),
    )
    .unwrap();
    let facts = ResponseFacts::from_bytes(
        "synthetic",
        Some(&policy),
        br#"{"data":"{\"code\":42}","error":"private"}"#,
        false,
    );
    assert_eq!(facts.body_kind, "json_object");
    assert_eq!(facts.category, "rate_limit");
    assert_eq!(facts.error_kind, "string");
    assert!(facts.keys.data && facts.keys.error);
    let encoded = ResponseFacts::from_bytes("synthetic", Some(&policy), b"opaque 42", false);
    assert_eq!(encoded.body_kind, "non_json");
    assert_eq!(encoded.category, "unknown");
    assert_eq!(
        ResponseFacts::from_bytes("synthetic", Some(&policy), br#"{"status":42}"#, false).category,
        "unknown"
    );
    assert_eq!(
        ResponseFacts::from_bytes(
            "synthetic",
            Some(&policy),
            br#"{"status":43,"code":42}"#,
            false
        )
        .category,
        "rate_limit"
    );
    let command = ResponseFacts::from_bytes(
        "command-code",
        None,
        br#"{"error":{"code":"model_not_found"}}"#,
        false,
    );
    assert_eq!(command.category, "model");
    assert!(
        ResponseCodeMap::parse(&json!({"fields":["message"],"codes":{"42":"rate_limit"}})).is_err()
    );
    assert!(ResponseCodeMap::parse(&json!({"fields":["code"],"codes":{"42":"private"}})).is_err());
    assert!(ResponseCodeMap::parse(&json!({"fields":["code"],"codes":{"042":"quota"}})).is_err());
    assert!(ResponseCodeMap::parse(&json!({"fields":["code"],"codes":{},"extra":true})).is_err());
    let entries: serde_json::Map<String, Value> =
        (0..65).map(|n| (n.to_string(), json!("quota"))).collect();
    assert!(ResponseCodeMap::parse(&json!({"fields":["code"],"codes":entries})).is_err());
    assert!(
        ResponseCodeMap::parse(
            &json!({"fields":["code"],"codes":{},"padding":"x".repeat(16 * 1024)})
        )
        .is_err()
    );
    let mut deep = json!({"code":42});
    for _ in 0..8 {
        deep = json!([deep]);
    }
    assert_eq!(
        response_category("synthetic", Some(&policy), &deep, 0),
        None
    );
    let mut facts = DiagnosticFacts::default();
    facts.observe("prefix Codex canonical history boundaries are unavailable suffix");
    assert!(!facts.codex_history_boundaries_unavailable);
    facts.observe("Codex canonical history boundaries are unavailable");
    assert!(facts.codex_history_boundaries_unavailable);
}

#[test]
fn runtime_and_event_facts_preserve_specific_errors() {
    use stravia_runtime_contract::protocol::ir::AiErrorKind;
    let mut details = ReportDetails {
        stage: "execute",
        error_kind: None,
        upstream_status: None,
        operation_supported: None,
        diagnostic_facts: DiagnosticFacts::default(),
        selected_model_present: None,
        model_selection_origin: None,
        response_facts: None,
        model_error_kind: None,
        retry_after_ms: None,
        http_observation: None,
    };
    let capture = crate::plugin::network::transport::DiagnosticCapture::default();
    capture.observe_head(200, &[]);
    details.http_observation = capture.observation();
    details.merge_events((
        DiagnosticFacts::default(),
        Some(AiErrorKind::RateLimitError),
        Some(Duration::from_millis(250)),
    ));
    let error = anyhow::Error::new(RuntimeError::from_guest(
        ErrorKind::upstream(None, None),
        String::new(),
        Some(403),
    ));
    assert_eq!(
        execution_failure_status(&error, &mut details),
        "execution_failed"
    );
    assert_eq!(details.upstream_status, Some(403));
    assert_eq!(
        serde_json::to_value(&details.http_observation).unwrap(),
        json!({"response_count": 1, "last_status": 200, "subscription_exhausted": false})
    );
    assert_eq!(details.model_error_kind, Some(AiErrorKind::RateLimitError));
    assert_eq!(details.retry_after_ms, Some(250));
    let error = anyhow::Error::new(RuntimeError::from_guest(
        ErrorKind::upstream(
            Some(AiErrorKind::QuotaExceeded),
            Some(Duration::from_secs(3)),
        ),
        String::new(),
        Some(429),
    ));
    let mut other = ReportDetails {
        model_error_kind: None,
        retry_after_ms: None,
        upstream_status: None,
        ..details
    };
    execution_failure_status(&error, &mut other);
    assert_eq!(other.model_error_kind, Some(AiErrorKind::QuotaExceeded));
    assert_eq!(other.retry_after_ms, Some(3000));
}

#[tokio::test]
async fn diagnostic_decoding_is_bounded_and_closed() {
    use tokio::io::AsyncWriteExt;
    let input = br#"{"error":"max_tokens validation quota"}"#;
    let mut encoder = async_compression::tokio::write::GzipEncoder::new(Vec::new());
    encoder.write_all(input).await.unwrap();
    encoder.shutdown().await.unwrap();
    let compressed = encoder.into_inner();
    let decoded = decode_diagnostic_body("gzip", &compressed, false)
        .await
        .unwrap();
    assert_eq!(decoded.as_ref(), input);
    let mut facts = DiagnosticFacts::default();
    facts.observe(std::str::from_utf8(&decoded).unwrap());
    assert!(facts.output_token_parameter && facts.validation && facts.quota);
    assert!(
        decode_diagnostic_body("gzip", b"invalid", false)
            .await
            .is_none()
    );
    assert!(
        decode_diagnostic_body("unknown", input, false)
            .await
            .is_none()
    );
    assert!(
        decode_diagnostic_body("identity", input, true)
            .await
            .is_none()
    );
    let mut encoder = async_compression::tokio::write::GzipEncoder::new(Vec::new());
    encoder
        .write_all(&vec![b'x'; 256 * 1024 + 1])
        .await
        .unwrap();
    encoder.shutdown().await.unwrap();
    assert!(
        decode_diagnostic_body("gzip", &encoder.into_inner(), false)
            .await
            .is_none()
    );
}

fn empty_object() -> Value {
    json!({})
}

// Metadata may itself contain credential material. Never admit a refresh token,
// including inside JSON-encoded database columns or nested metadata.
fn contains_refresh_token(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(key, value)| {
            key.chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .flat_map(char::to_lowercase)
                .collect::<String>()
                == "refreshtoken"
                || contains_refresh_token(value)
        }),
        Value::Array(values) => values.iter().any(contains_refresh_token),
        Value::String(text) => serde_json::from_str::<Value>(text)
            .ok()
            .filter(|parsed| !parsed.is_string())
            .is_some_and(|parsed| contains_refresh_token(&parsed)),
        _ => false,
    }
}

fn object_column(value: &Value) -> anyhow::Result<String> {
    let parsed = match value {
        Value::String(text) => serde_json::from_str::<Value>(text)?,
        other => other.clone(),
    };
    anyhow::ensure!(parsed.is_object());
    Ok(serde_json::to_string(&parsed)?)
}

#[test]
#[ignore = "explicit response code map probe"]
fn response_code_map_probe() {
    let fd = std::env::var("STRAVIA_ALIGNMENT_REPORT_FD")
        .unwrap()
        .parse::<u32>()
        .unwrap();
    assert!(fd >= 3);
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(512 * 1024)
        .read_to_end(&mut bytes)
        .unwrap();
    let input: Value = serde_json::from_slice(&bytes).unwrap();
    let policy =
        ResponseCodeMap::parse(&input["response_code_map"]).expect("invalid response code map");
    let category = response_category("", Some(&policy), &input["body"], 0).unwrap_or("unknown");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(format!("/proc/self/fd/{fd}"))
        .unwrap();
    serde_json::to_writer(&mut file, &json!({"category": category})).unwrap();
}

#[tokio::test]
#[ignore = "explicit live vendor alignment only"]
async fn live_alignment() {
    let fd = std::env::var("STRAVIA_ALIGNMENT_REPORT_FD")
        .expect("missing alignment report descriptor")
        .parse::<u32>()
        .ok()
        .filter(|fd| *fd >= 3)
        .expect("invalid alignment report descriptor");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(format!("/proc/self/fd/{fd}"))
        .expect("open alignment report descriptor");
    let mut details = ReportDetails {
        stage: "prepare",
        error_kind: None,
        upstream_status: None,
        operation_supported: None,
        diagnostic_facts: DiagnosticFacts::default(),
        selected_model_present: None,
        model_selection_origin: None,
        response_facts: None,
        model_error_kind: None,
        retry_after_ms: None,
        http_observation: None,
    };
    let result = async {
        let mut bytes = Vec::new();
        std::io::stdin()
            .take(8 * 1024 * 1024)
            .read_to_end(&mut bytes)
            .map_err(|_| "invalid_input")?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| "invalid_input")?;
        if contains_refresh_token(&value) {
            return Err("invalid_input");
        }
        let input: Input = serde_json::from_value(value).map_err(|_| "invalid_input")?;
        run(input, &mut details).await
    }
    .await;
    let (status, count) = match result {
        Ok(count) => ("ok", count),
        Err(status) => (status, 0),
    };
    if details.error_kind.is_none() {
        details.error_kind = match status {
            "invalid_input" => Some("invalid_input"),
            "setup_failed" => Some("setup"),
            "execution_failed" => Some("other"),
            "invalid_output" => Some("invalid_output"),
            "unauthorized" => Some("auth"),
            _ => None,
        };
    }
    let report = json!({"status":status, "count":count, "stage":details.stage,
        "error_kind":details.error_kind, "upstream_status":details.upstream_status,
        "operation_supported":details.operation_supported,
        "diagnostic_facts":details.diagnostic_facts,
        "selected_model_present":details.selected_model_present,
        "model_selection_origin":details.model_selection_origin,
        "response_facts":details.response_facts,
        "model_error_kind":details.model_error_kind,
        "retry_after_ms":details.retry_after_ms,
        "http_observation":details.http_observation});
    let mut line = serde_json::to_vec(&report).expect("serialize alignment report");
    line.push(b'\n');
    file.write_all(&line).expect("write alignment report");
}

fn execution_failure_status(error: &anyhow::Error, details: &mut ReportDetails) -> &'static str {
    if let Some(error) = error.downcast_ref::<RuntimeError>() {
        if let Some(message) = error.diagnostic_message() {
            details.diagnostic_facts.observe(message);
        }
        details.observe_error(error.model_error_kind(), error.retry_after());
        if let Some(status) = error
            .upstream_status()
            .filter(|status| (100..=599).contains(status))
        {
            details.upstream_status.get_or_insert(status);
        }
        details.error_kind = Some(match error {
            RuntimeError::Plugin {
                kind: ErrorKind::Unsupported,
                ..
            } => "unsupported",
            RuntimeError::Plugin {
                kind: ErrorKind::Auth,
                ..
            } => "auth",
            RuntimeError::Plugin {
                kind: ErrorKind::Invalid,
                ..
            } => "invalid_input",
            RuntimeError::Plugin {
                kind: ErrorKind::InvalidResponse,
                ..
            } => "invalid_response",
            RuntimeError::Plugin {
                kind: ErrorKind::Upstream(_),
                ..
            } => "upstream",
            RuntimeError::Cancelled => "cancelled",
            RuntimeError::DeadlineExceeded => "deadline",
            RuntimeError::ResourceExhausted => "resource_exhausted",
            RuntimeError::Trapped => "trapped",
            RuntimeError::InvalidOutput => "invalid_output",
            _ => "other",
        });
        if error.is_upstream_failure() && error.upstream_status() == Some(401) {
            return "unauthorized";
        }
    }
    "execution_failed"
}

async fn run(input: Input, details: &mut ReportDetails) -> Result<usize, &'static str> {
    let response_code_map = input
        .response_code_map
        .as_ref()
        .map(ResponseCodeMap::parse)
        .transpose()
        .map_err(|_| "invalid_input")?;
    if input.provider.use_proxy {
        let settings = input.outbound_proxy.as_deref().ok_or("invalid_input")?;
        crate::outbound_proxy::OutboundProxySettings::from_setting(Some(settings))
            .and_then(|config| config.reqwest_proxy())
            .map_err(|_| "invalid_input")?;
    }
    let url = url::Url::parse(&input.provider.base_url).map_err(|_| "invalid_input")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("invalid_input");
    }
    let empty_input = input
        .input
        .as_object()
        .is_some_and(|fields| fields.is_empty());
    let mut request = match input.operation.as_str() {
        "infer" if empty_input => {
            use stravia_runtime_contract::protocol::ir::{AiItem, MessageContent, Role};
            let model = input
                .model
                .as_deref()
                .filter(|model| !model.trim().is_empty())
                .ok_or("invalid_input")?;
            let item = AiItem {
                role: Role::User,
                content: MessageContent::Text(Arc::new("Reply with a short greeting.".into())),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            };
            VendorRequest::Infer(Arc::new(AiRequest::new(model, vec![item])))
        }
        "infer" => VendorRequest::Infer(Arc::new(
            serde_json::from_value(input.input.clone()).map_err(|_| "invalid_input")?,
        )),
        "discover" => VendorRequest::Discover(
            serde_json::from_value(input.input.clone()).map_err(|_| "invalid_input")?,
        ),
        "allowance" => VendorRequest::Allowance(
            serde_json::from_value(input.input.clone()).map_err(|_| "invalid_input")?,
        ),
        "media-image" if empty_input => VendorRequest::MediaImage(MediaImageRequest {
            prompt: "A simple blue circle on a white background.".into(),
            references: Vec::new(),
            aspect_ratio: None,
            resolution: None,
        }),
        "media-image" => VendorRequest::MediaImage(
            serde_json::from_value(input.input.clone()).map_err(|_| "invalid_input")?,
        ),
        _ => return Err("invalid_input"),
    };
    let has_model = input
        .model
        .as_deref()
        .is_some_and(|model| !model.trim().is_empty());
    if matches!(request, VendorRequest::Infer(_)) && !has_model {
        return Err("invalid_input");
    }
    if matches!(request, VendorRequest::MediaImage(_)) && has_model {
        details.model_selection_origin = Some("explicit");
    }
    match (input.provider.auth_mode.as_str(), &input.oauth) {
        ("apikey", None) => {}
        ("oauth", Some(oauth)) => {
            if let Some(expiry) = oauth.expires_at.as_deref() {
                let expiry =
                    chrono::DateTime::parse_from_rfc3339(expiry).map_err(|_| "invalid_input")?;
                if expiry <= chrono::Utc::now() {
                    return Err("invalid_input");
                }
            }
            if oauth.access_token.trim().is_empty() || oauth.refresh_token.is_some() {
                return Err("invalid_input");
            }
        }
        _ => return Err("invalid_input"),
    }
    let adapter_credentials =
        object_column(&input.provider.adapter_credentials).map_err(|_| "invalid_input")?;
    let vendor_options =
        object_column(&input.provider.vendor_options).map_err(|_| "invalid_input")?;
    details.stage = "gateway_init";
    let directory = tempfile::tempdir().map_err(|_| "setup_failed")?;
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .map_err(|_| "setup_failed")?;
    crate::migrations::migrate_sqlite(&pool, None)
        .await
        .map_err(|_| "setup_failed")?;
    let storage = Arc::new(crate::storage::sqlite::SqliteStorage::from_pool(pool));
    let gateway = Gateway::from_storage(
        GatewayConfig {
            data_dir: directory.path().to_path_buf(),
            catalog_background_refresh: false,
            ..Default::default()
        },
        storage,
    )
    .await
    .map_err(|_| "setup_failed")?;
    details.stage = "background_stop";
    gateway.stop_background_tasks_for_test().await;
    let capture = gateway.vendor_transport.enable_diagnostic_capture();
    let diagnostic_vendor = input.provider.vendor.clone();
    let result = async {
        let setup = async {
            if let Some(settings) = input.outbound_proxy.as_deref() {
                details.stage = "proxy_setting";
                gateway
                    .admin()
                    .set_setting(crate::outbound_proxy::SETTINGS_KEY, settings)
                    .await?;
            }
            details.stage = "plugin_read";
            let component = std::fs::read(&input.wasm_path)?;
            details.stage = "plugin_preview";
            let preview = gateway.admin().preview_vendor_plugin(component).await?;
            anyhow::ensure!(preview.vendor_id == input.plugin_id);
            details.stage = "plugin_confirm";
            gateway
                .admin()
                .confirm_vendor_plugin(ConfirmPluginUpdate {
                    preview_id: preview.id,
                    allow_data_discard: false,
                })
                .await?;
            details.stage = "provider_create";
            let provider = gateway
                .storage
                .providers()
                .create(CreateProviderRecord {
                    name: "alignment".into(),
                    vendor: Some(input.provider.vendor.clone()),
                    protocol: input.provider.protocol,
                    base_url: input.provider.base_url,
                    preset_key: None,
                    channel: Some(input.provider.channel),
                    models_source: input.provider.models_source,
                    static_models: input.provider.static_models,
                    api_key: input.provider.api_key,
                    adapter_credentials,
                    vendor_options,
                    auth_mode: input.provider.auth_mode,
                    use_proxy: input.provider.use_proxy,
                })
                .await?;
            if let Some(oauth) = input.oauth {
                details.stage = "oauth_store";
                gateway
                    .storage
                    .oauth_credentials()
                    .upsert(&provider.id, oauth)
                    .await?;
            }
            if let (Some(model), Some(metadata)) = (&input.model, input.model_metadata) {
                details.stage = "model_create";
                gateway
                    .storage
                    .provider_models()
                    .create(
                        NewProviderModelRecord {
                            provider_id: provider.id.clone(),
                            model_id: model.clone(),
                            source_kind: ProviderModelSourceKind::Manual,
                            snapshot_state: SnapshotState::Local,
                            source_metadata: None,
                            metadata_source_provider_id: None,
                            presence: ProviderModelPresence::Present,
                            selection_policy: ProviderModelSelectionPolicy::Auto,
                            metadata,
                        },
                        None,
                    )
                    .await?;
            }
            let mut context = VendorCallContext::new(
                CancellationToken::new(),
                Deadline::from_now(Duration::from_secs(120)),
            );
            // Use Core's principal and Generation Chain admission, rather than
            // synthesizing prompt/session provenance in the test client.
            let mut admitted_turn = None;
            if let VendorRequest::Infer(infer) = &mut request {
                details.stage = "principal_create";
                let key = gateway
                    .admin()
                    .create_api_key(crate::db::models::CreateApiKey {
                        key: None,
                        name: "alignment".into(),
                        rpm_limit: None,
                        expires_at: None,
                        mcp_access_enabled: false,
                        transparent_injection_enabled: false,
                        inject_web_search: false,
                        inject_media_generation: false,
                        model_ids: Vec::new(),
                        inject_media_understanding: false,
                        spend_limit: None,
                    })
                    .await?;
                details.stage = "principal_auth";
                let mut headers = axum::http::HeaderMap::new();
                headers.insert(
                    axum::http::header::AUTHORIZATION,
                    format!("Bearer {}", key.token).parse()?,
                );
                let credential =
                    crate::proxy::security::ClientCredential::from_inference_headers(&headers);
                let authenticated = crate::proxy::security::Security::new(gateway.storage.auth())
                    .authenticated_principal(&credential)
                    .await?;
                let request = Arc::make_mut(infer);
                request.meta.source_protocol =
                    Some(stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24);
                details.stage = "chain_begin";
                let source_request = request.clone();
                let mut turn = gateway
                    .generation_chains
                    .begin(authenticated.principal.clone(), source_request.clone())
                    .await?;
                if crate::generation_chain::generation_session_fingerprint(request).is_none() {
                    crate::generation_chain::set_generation_session_id(request, turn.root_id());
                }
                details.stage = "prompt_admission";
                gateway
                    .principal_admission
                    .acquire(&authenticated.principal, authenticated.rpm_limit)?;
                turn.admit_user_prompt().await?;
                let provenance = turn
                    .user_prompt_metadata()
                    .ok_or_else(|| anyhow::anyhow!("prompt admission unavailable"))?;
                if let Value::Object(fields) = serde_json::to_value(provenance)? {
                    context
                        .metadata
                        .extend(fields.into_iter().filter(|(_, value)| !value.is_null()));
                }
                if let Some(logical) = turn.logical_turn() {
                    context
                        .metadata
                        .insert("logical_turn_id".into(), json!(logical.id.to_string()));
                    context.metadata.insert(
                        "logical_turn_started_at_ms".into(),
                        json!(logical.created_at_ms),
                    );
                }
                context.atomic_state = turn.atomic_conversation_context();
                context.history_boundary_candidate = turn.history_boundary_candidate();
                let candidate = context
                    .history_boundary_candidate
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("history boundary candidate unavailable"))?;
                // A new principal in an empty isolated store has no retained
                // checkpoints; project the actual admitted source, never a
                // fabricated empty boundary map.
                let map = crate::generation_chain::project_history_boundaries(
                    &source_request,
                    candidate,
                    &[],
                )
                .ok_or_else(|| anyhow::anyhow!("history boundary projection unavailable"))?;
                context
                    .metadata
                    .insert("history_boundaries".into(), serde_json::to_value(map)?);
                if let Some(model) = input.model.as_deref() {
                    let target = crate::router::target_key(&provider.id, model);
                    // 与正式 Model Turn 共用身份派生；只绑定真实准入的链路根，
                    // 不采用客户端自报 session，也不仿造已有上游会话。
                    context.metadata.insert(
                        "session_affinity".into(),
                        json!(crate::model_turn::generation_session_affinity(
                            &authenticated.principal,
                            &target,
                            turn.root_id(),
                        )),
                    );
                }
                admitted_turn = Some(turn);
            }
            let (events, receiver) = mpsc::channel::<VendorEvent>(64);
            context.events = Some(events);
            details.stage = "plugin_acquire";
            let (plugin, operation, data_epoch) =
                gateway.vendor_plugins.acquire(&input.provider.vendor)?;
            details.stage = "connection_snapshot";
            let connection = gateway
                .read_connection_after_admission(
                    &provider.id,
                    &input.provider.vendor,
                    input.model.as_deref(),
                )
                .await?;
            if let Some(expiry) = connection
                .oauth
                .as_ref()
                .and_then(|oauth| oauth.expires_at.as_deref())
            {
                anyhow::ensure!(chrono::DateTime::parse_from_rfc3339(expiry)? > chrono::Utc::now());
            }
            details.stage = "provider_snapshot";
            let descriptor = plugin
                .component
                .descriptor()
                .provider(&input.provider.vendor)
                .ok_or_else(|| anyhow::anyhow!("profile unavailable"))?;
            if let Some(channel) = descriptor
                .channels
                .iter()
                .find(|channel| Some(channel.id.as_str()) == connection.provider.channel.as_deref())
            {
                details.operation_supported = request
                    .operation()
                    .required_capability()
                    .map(|capability| channel.capabilities.contains(&capability));
            }
            let (mut snapshot, use_proxy, origins) = provider_snapshot(
                descriptor,
                &connection,
                input.model.as_deref(),
                request.operation(),
                connection.provider.models_source.as_deref(),
                &context,
            )?;
            anyhow::ensure!(origins.allows(&url::Url::parse(&snapshot.base_url)?));
            snapshot.operation_metadata.insert(
                "client_installation_id".into(),
                Value::String(gateway.client_installation_id.to_string()),
            );
            let prepared = PreparedVendorExecution {
                plugin,
                operation,
                data_epoch,
                vendor_id: input.provider.vendor,
                provider_id: provider.id,
                provider: snapshot,
                oauth_connection_id: connection
                    .oauth
                    .as_ref()
                    .map(|oauth| oauth.connection_id.clone()),
                use_proxy,
                origins,
                kind: request.operation(),
                credential_version: ProviderCredentialVersion {
                    provider_revision: connection.provider.revision,
                    oauth_status_version: connection
                        .oauth
                        .as_ref()
                        .map(|oauth| oauth.status_version),
                },
            };
            Ok::<_, anyhow::Error>((prepared, context, receiver, admitted_turn))
        }
        .await
        .map_err(|error| {
            for cause in error.chain() {
                details.diagnostic_facts.observe(&cause.to_string());
            }
            "setup_failed"
        })?;
        let (mut prepared, context, mut receiver, _admitted_turn) = setup;
        details.stage = "execute";
        let event_failed = Arc::new(AtomicBool::new(false));
        let event_status = Arc::new(std::sync::atomic::AtomicU16::new(0));
        let failed = event_failed.clone();
        let status = event_status.clone();
        let event_task = tokio::spawn(async move {
            let mut facts = DiagnosticFacts::default();
            let mut model_kind = None;
            let mut retry_after = None;
            while let Some(event) = receiver.recv().await {
                let upstream_status = match event.event {
                    RuntimeEvent::Failed {
                        kind,
                        upstream_status,
                        message,
                    } => {
                        facts.observe(&message);
                        let candidate = kind.model_error_kind();
                        if (model_kind.is_none()
                            || model_kind
                                == Some(
                                    stravia_runtime_contract::protocol::ir::AiErrorKind::Unknown,
                                ))
                            && candidate.is_some()
                        {
                            model_kind = candidate;
                        }
                        if retry_after.is_none() {
                            retry_after = kind.retry_after();
                        }
                        Some(upstream_status)
                    }
                    RuntimeEvent::Delta(
                        stravia_runtime_contract::protocol::ir::AiStreamDelta::StreamError {
                            error,
                        },
                    ) => {
                        facts.observe(&error.message);
                        if model_kind.is_none()
                            || model_kind
                                == Some(
                                    stravia_runtime_contract::protocol::ir::AiErrorKind::Unknown,
                                )
                        {
                            model_kind = Some(error.kind.clone());
                        }
                        if let Some(raw) = &error.raw {
                            facts.observe(&raw.to_string());
                        }
                        Some(error.status_code)
                    }
                    _ => None,
                };
                if let Some(upstream_status) = upstream_status {
                    failed.store(true, Ordering::Release);
                    if let Some(code @ 100..=599) = upstream_status {
                        status.store(code, Ordering::Release);
                    }
                }
            }
            (facts, model_kind, retry_after)
        });
        if matches!(request, VendorRequest::MediaImage(_)) && !has_model {
            details.stage = "discover_image_model";
            let mut discovery = prepared.clone();
            discovery.kind = Operation::Discover;
            discovery.provider.model = None;
            discovery.provider.model_metadata = None;
            let selection = match gateway
                .execute_prepared_vendor_once(
                    discovery,
                    VendorRequest::Discover(DiscoverRequest::default()),
                    &context,
                )
                .await
            {
                Ok(result) => match result.output {
                    OperationOutput::Discover(response) => {
                        Ok(response.models.into_iter().find(|model| {
                            !model.id.trim().is_empty()
                                && (model.capabilities.iter().any(|capability| {
                                    matches!(capability.as_str(), "media_image" | "image_output")
                                }) || model
                                    .metadata
                                    .get("capabilities")
                                    .and_then(Value::as_array)
                                    .is_some_and(|capabilities| {
                                        capabilities.iter().any(|capability| {
                                            matches!(
                                                capability.as_str(),
                                                Some("media_image" | "image_output")
                                            )
                                        })
                                    })
                                    || model
                                        .metadata
                                        .get("modalities")
                                        .and_then(|modalities| modalities.get("output"))
                                        .and_then(Value::as_array)
                                        .is_some_and(|outputs| {
                                            outputs
                                                .iter()
                                                .any(|output| output.as_str() == Some("image"))
                                        }))
                        }))
                    }
                    _ => Err("invalid_output"),
                },
                Err(error) => Err(execution_failure_status(&error, details)),
            };
            match selection {
                Ok(Some(model)) if !event_failed.load(Ordering::Acquire) => {
                    prepared.provider.model = Some(model.id.clone());
                    prepared.provider.model_metadata = Some(stravia_vendor_sdk::ModelMetadata {
                        id: Some(model.id),
                        family: model.family,
                        selector: model.selector,
                        capabilities: model.capabilities,
                        extensions: model.metadata,
                    });
                    details.model_selection_origin = Some("discovery");
                }
                other => {
                    drop(context);
                    details.merge_events(event_task.await.map_err(|_| "execution_failed")?);
                    let status = event_status.load(Ordering::Acquire);
                    if status != 0 {
                        details.upstream_status = Some(status);
                    }
                    if status == 401 {
                        details.error_kind = Some("auth");
                        return Err("unauthorized");
                    }
                    if event_failed.load(Ordering::Acquire) {
                        return Err("execution_failed");
                    }
                    return match other {
                        Err(status) => Err(status),
                        _ => {
                            details.error_kind = Some("image_model_unavailable");
                            Err("image_model_unavailable")
                        }
                    };
                }
            }
            details.stage = "execute";
        }
        // Discovery and inference share the same pinned component, credentials and
        // declared network policy. Never infer an OpenCode model on an assumed price.
        if prepared.vendor_id == "opencode-free"
            && let VendorRequest::Infer(infer) = &request
        {
            let model = &infer.model;
            let eligible = prepared.provider.model.as_deref() == Some(model.as_str())
                && (model.ends_with("-free") || model == "big-pickle");
            let mut discovery = prepared.clone();
            discovery.kind = Operation::Discover;
            discovery.provider.model = None;
            discovery.provider.model_metadata = None;
            let allowed = if eligible {
                match gateway
                    .execute_prepared_vendor_once(
                        discovery,
                        VendorRequest::Discover(DiscoverRequest::default()),
                        &context,
                    )
                    .await
                {
                    Ok(result) => Ok(matches!(result.output, OperationOutput::Discover(response)
                    if response.models.iter().any(|entry| entry.id == *model))),
                    Err(error) => Err(execution_failure_status(&error, details)),
                }
            } else {
                Ok(false)
            };
            if !matches!(allowed, Ok(true)) {
                drop(context);
                details.merge_events(event_task.await.map_err(|_| "execution_failed")?);
                let status = event_status.load(Ordering::Acquire);
                if status != 0 {
                    details.upstream_status = Some(status);
                }
                if status == 401 {
                    details.error_kind = Some("auth");
                    return Err("unauthorized");
                }
                if event_failed.load(Ordering::Acquire) {
                    return Err("execution_failed");
                }
                return Err(allowed.err().unwrap_or("invalid_output"));
            }
        }
        let selected_command_model = if prepared.vendor_id == "command-code"
            && matches!(request, VendorRequest::Discover(_))
        {
            prepared
                .provider
                .model
                .clone()
                .filter(|model| !model.trim().is_empty())
        } else {
            None
        };
        capture.clear();
        let execution = gateway
            .execute_prepared_vendor_once(prepared, request, &context)
            .await;
        details.http_observation = capture.observation();
        drop(context);
        details.merge_events(event_task.await.map_err(|_| "execution_failed")?);
        let status = event_status.load(Ordering::Acquire);
        if status != 0 {
            details.upstream_status = Some(status);
        }
        let execution = match execution {
            Ok(execution) => execution,
            Err(error) => {
                let outcome = execution_failure_status(&error, details);
                return Err(if status == 401 {
                    "unauthorized"
                } else {
                    outcome
                });
            }
        };
        if let OperationOutput::Infer(response) = &execution.output
            && let Some(error) = &response.error
        {
            details.observe_error(Some(error.kind.clone()), None);
            if let Some(status) = error
                .status_code
                .filter(|status| (100..=599).contains(status))
            {
                details.upstream_status.get_or_insert(status);
            }
        }
        if status == 401 {
            details.error_kind.get_or_insert("auth");
            return Err("unauthorized");
        }
        if event_failed.load(Ordering::Acquire) {
            return Err("execution_failed");
        }
        details.stage = "decode";
        if let OperationOutput::Infer(response) = &execution.output
            && let Some(error) = &response.error
        {
            details.diagnostic_facts.observe(&error.message);
            if let Some(raw) = &error.raw {
                details.diagnostic_facts.observe(&raw.to_string());
            }
        }
        let count = match execution.output {
            OperationOutput::Infer(response)
                if response
                    .error
                    .as_ref()
                    .is_some_and(|error| error.status_code == Some(401)) =>
            {
                details.upstream_status = Some(401);
                details.error_kind = Some("auth");
                return Err("unauthorized");
            }
            OperationOutput::Infer(response) if !response.is_error() => response
                .output_texts()
                .filter(|text| !text.trim().is_empty())
                .count(),
            OperationOutput::Infer(response) => {
                if let Some(status) = response
                    .error
                    .as_ref()
                    .and_then(|error| error.status_code)
                    .filter(|status| (100..=599).contains(status))
                {
                    details.upstream_status.get_or_insert(status);
                }
                details.error_kind.get_or_insert("upstream");
                return Err("execution_failed");
            }
            OperationOutput::Discover(response) => {
                if let Some(model) = selected_command_model.as_deref() {
                    details.selected_model_present =
                        Some(response.models.iter().any(|entry| entry.id == model));
                }
                response
                    .models
                    .iter()
                    .filter(|model| !model.id.trim().is_empty())
                    .count()
            }
            OperationOutput::Allowance(response) => {
                response.allowances.len()
                    + response
                        .models
                        .iter()
                        .map(|model| model.allowances.len())
                        .sum::<usize>()
            }
            OperationOutput::MediaImage(response) => response
                .artifacts
                .iter()
                .filter(|artifact| {
                    !artifact.bytes.is_empty() && artifact.media_type.starts_with("image/")
                })
                .count(),
            _ => 0,
        };
        if count == 0 {
            Err("invalid_output")
        } else {
            Ok(count)
        }
    }
    .await;
    if result.is_err()
        && let Some(response) = capture.take()
        && response.failure_count == 1
        && details
            .upstream_status
            .is_none_or(|status| status == response.status)
    {
        details.upstream_status = Some(response.status);
        use crate::plugin::network::transport::DiagnosticContentCoding;
        let coding = match response.content_coding {
            DiagnosticContentCoding::Identity => "identity",
            DiagnosticContentCoding::Gzip => "gzip",
            DiagnosticContentCoding::Deflate => "deflate",
            DiagnosticContentCoding::Br => "br",
            DiagnosticContentCoding::Zstd => "zstd",
            DiagnosticContentCoding::Unknown => "unknown",
        };
        details.response_facts = Some(
            match decode_diagnostic_body(coding, &response.body, response.truncated).await {
                Some(decoded) => {
                    if let Ok(text) = std::str::from_utf8(&decoded) {
                        details.diagnostic_facts.observe(text);
                    }
                    let mut facts = ResponseFacts::from_bytes(
                        &diagnostic_vendor,
                        response_code_map.as_ref(),
                        &decoded,
                        response.truncated,
                    );
                    facts.content_coding = coding;
                    facts
                }
                None => ResponseFacts::unknown(coding, response.truncated),
            },
        );
    }
    gateway.shutdown().await;
    result
}
