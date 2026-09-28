//! Cline Pass subscription quota: `GET {base}/users/me/plan/usage-limits`.
//!
//! The gateway answers `{ "success": bool, "data": { "limits": [
//! { "type", "percentUsed", "resetsAt" } ] } }`, where `type` is `five_hour`,
//! `weekly`, or `monthly`. The `data` envelope is not contractual — the bare
//! `{ "limits": … }` shape is accepted as well, matching the upstream
//! reference. Window types the gateway adds later pass through with their raw
//! `type` as the item key instead of being dropped.
//!
//! This stays a dedicated monitor instead of joining the generic monitor
//! loop: the usage-limits URL follows the connection's configured base URL
//! rather than a fixed origin, and `success: false` bodies carry upstream
//! error detail that the generic `Err(())` parser contract cannot represent.

use serde_json::Value;
use stravia_vendor_common::common;
use stravia_vendor_sdk::wit::types::HttpRequest;
use stravia_vendor_sdk::{
    AllowanceRequest, AllowanceResponse, ErrorKind, GuestHost, PluginError, ProviderSnapshot,
    read_http_body,
};

use super::parsers;

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const USAGE_LIMITS_PATH: &str = "/users/me/plan/usage-limits";
const DEFAULT_BASE_URL: &str = "https://api.cline.bot/api/v1";

pub(super) fn supports(vendor_id: &str, channel: &str) -> bool {
    vendor_id == "cline-pass" && channel == "default"
}

pub(super) fn execute(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    _request: AllowanceRequest,
) -> Result<AllowanceResponse, PluginError> {
    let credential = api_key(provider)?;
    let response = host.http_start(HttpRequest {
        method: "GET".into(),
        url: format!("{}{USAGE_LIMITS_PATH}", base_url(provider)),
        headers: vec![
            ("accept".into(), "application/json".into()),
            ("authorization".into(), format!("Bearer {credential}")),
        ],
        body: Vec::new(),
    })?;
    let status = response.status()?;
    let headers = response.headers()?;
    let body = read_http_body(&response, MAX_RESPONSE_BYTES)?;
    if !(200..300).contains(&status) {
        return Err(common::upstream_error(status, &headers, &body));
    }
    let value: Value = serde_json::from_slice(&body).map_err(|_| invalid_response(None))?;
    parse_limits(&value)
}

fn parse_limits(value: &Value) -> Result<AllowanceResponse, PluginError> {
    // The envelope wrapper is not contractual: read the payload whether the
    // gateway nests it under `data` or answers with the window list itself.
    let payload = value
        .get("data")
        .filter(|data| data.is_object())
        .unwrap_or(value);
    let detail = [payload, value]
        .into_iter()
        .filter_map(|layer| {
            layer
                .get("error")
                .or_else(|| layer.get("message"))
                .and_then(error_text)
        })
        .next();
    if value.get("success").and_then(Value::as_bool) == Some(false)
        || payload.get("success").and_then(Value::as_bool) == Some(false)
    {
        return Err(invalid_response(detail));
    }
    let Some(limits) = payload.get("limits").and_then(Value::as_array) else {
        return Err(invalid_response(detail));
    };
    let mut allowances = Vec::new();
    for limit in limits {
        let Some(kind) = limit.get("type").and_then(parsers::non_empty) else {
            continue;
        };
        let label = match kind {
            "five_hour" => "Five-hour window",
            "weekly" => "Weekly window",
            "monthly" => "Monthly window",
            other => other,
        };
        let mut item = parsers::item(kind, label, "quota_window");
        item.used_percent = limit
            .get("percentUsed")
            .and_then(parsers::number)
            .map(parsers::decimal);
        item.window_seconds = window_seconds(kind);
        item.resets_at_unix_ms = limit.get("resetsAt").and_then(parsers::timestamp_millis);
        item.condition = parsers::condition(&item);
        allowances.push(item);
    }
    Ok(parsers::response(allowances))
}

/// Nominal window length in seconds for the documented types; unknown types
/// stay unknown rather than guessing a duration.
fn window_seconds(kind: &str) -> Option<u64> {
    match kind {
        "five_hour" => Some(18_000),
        "weekly" => Some(604_800),
        "monthly" => Some(2_592_000),
        _ => None,
    }
}

fn error_text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| {
            value
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

fn invalid_response(detail: Option<String>) -> PluginError {
    let message = match detail {
        Some(detail) => format!("Cline Pass usage-limits request failed: {detail}"),
        None => "Cline Pass usage-limits response has an unsupported shape".into(),
    };
    PluginError {
        kind: ErrorKind::upstream_unknown(),
        message,
        upstream_status: None,
    }
}

fn api_key(provider: &ProviderSnapshot) -> Result<&str, PluginError> {
    provider
        .credentials
        .get("apiKey")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .ok_or_else(|| error(ErrorKind::Auth, "Cline Pass API key is missing"))
}

fn base_url(provider: &ProviderSnapshot) -> &str {
    let base = provider.base_url.trim();
    if base.is_empty() {
        DEFAULT_BASE_URL
    } else {
        base.trim_end_matches('/')
    }
}

fn error(kind: ErrorKind, message: impl Into<String>) -> PluginError {
    PluginError {
        kind,
        message: message.into(),
        upstream_status: None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;

    fn provider(base_url: &str, api_key: Option<&str>) -> ProviderSnapshot {
        ProviderSnapshot {
            provider_id: "cline-pass".into(),
            channel: "default".into(),
            base_url: base_url.into(),
            protocol: "openai-compatible".into(),
            options: BTreeMap::new(),
            credentials: api_key
                .map(|key| BTreeMap::from([("apiKey".to_owned(), Value::from(key))]))
                .unwrap_or_default(),
            model: None,
            model_metadata: None,
            client_headers: Vec::new(),
            operation_metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn parses_enveloped_and_bare_limit_lists() {
        for body in [
            json!({"success": true, "data": {"limits": [
                {"type": "five_hour", "percentUsed": 12.5, "resetsAt": "2026-09-27T20:00:00Z"},
                {"type": "weekly", "percentUsed": 0, "resetsAt": "2026-10-01T00:00:00Z"},
            ]}}),
            json!({"limits": [
                {"type": "five_hour", "percentUsed": 12.5, "resetsAt": "2026-09-27T20:00:00Z"},
                {"type": "weekly", "percentUsed": 0, "resetsAt": "2026-10-01T00:00:00Z"},
            ]}),
        ] {
            let response = parse_limits(&body).expect("limits decode");
            assert_eq!(response.allowances.len(), 2);
            let five_hour = &response.allowances[0];
            assert_eq!(five_hour.key, "five_hour");
            assert_eq!(five_hour.kind, "quota_window");
            assert_eq!(five_hour.used_percent.as_deref(), Some("12.5"));
            assert_eq!(five_hour.window_seconds, Some(18_000));
            assert_eq!(
                five_hour.resets_at_unix_ms,
                Some(
                    chrono::DateTime::parse_from_rfc3339("2026-09-27T20:00:00Z")
                        .unwrap()
                        .timestamp_millis()
                )
            );
            let weekly = &response.allowances[1];
            assert_eq!(weekly.window_seconds, Some(604_800));
        }
    }

    #[test]
    fn rejects_error_and_missing_limits_shapes() {
        let refusal = json!({"success": false, "error": {"message": "unauthorized"}});
        assert!(parse_limits(&refusal).is_err());
        let malformed = json!({"data": {"unexpected": true}});
        assert!(parse_limits(&malformed).is_err());
    }

    #[test]
    fn surfaces_upstream_error_detail() {
        // `error`/`message` may sit on either the envelope or the payload; a
        // `success: false` refusal must not be flattened into a shape error.
        for (body, detail) in [
            (
                json!({"success": false, "error": {"message": "unauthorized"}}),
                "unauthorized",
            ),
            (
                json!({"data": {"success": false, "error": "quota service down"}}),
                "quota service down",
            ),
            (json!({"success": false, "message": "  denied  "}), "denied"),
        ] {
            let error = parse_limits(&body).expect_err("failure carries detail");
            assert!(
                error.message.contains(detail),
                "{} lacks {detail}",
                error.message
            );
            assert!(error.kind.is_upstream_failure(), "{}", error.message);
        }
        // A malformed payload with a readable `message` still reports it.
        let error = parse_limits(&json!({"message": "bad request", "data": {"oops": true}}))
            .expect_err("malformed with detail");
        assert!(error.message.contains("bad request"), "{}", error.message);
    }

    #[test]
    fn unknown_window_types_pass_through_with_raw_key() {
        let response = parse_limits(&json!({"limits": [
            {"type": "5h", "percentUsed": 40},
            {"type": "monthly", "percentUsed": 97},
        ]}))
        .expect("limits decode");
        assert_eq!(response.allowances.len(), 2);
        let hourly = &response.allowances[0];
        assert_eq!(hourly.key, "5h");
        assert_eq!(hourly.label, "5h");
        assert_eq!(hourly.kind, "quota_window");
        assert_eq!(hourly.window_seconds, None);
        assert_eq!(hourly.condition.as_deref(), Some("normal"));
        let monthly = &response.allowances[1];
        assert_eq!(monthly.window_seconds, Some(2_592_000));
        assert_eq!(monthly.condition.as_deref(), Some("tight"));
    }

    #[test]
    fn skips_limits_without_a_type() {
        let response = parse_limits(&json!({"limits": [
            {"percentUsed": 50},
            {"type": "  "},
            {"type": "weekly"},
        ]}))
        .expect("limits decode");
        assert_eq!(response.allowances.len(), 1);
        assert_eq!(response.allowances[0].key, "weekly");
        assert_eq!(response.allowances[0].condition, None);
    }

    #[test]
    fn accepts_string_numbers_and_epoch_resets() {
        let response = parse_limits(&json!({"limits": [
            {"type": "five_hour", "percentUsed": "12.5", "resetsAt": 1_790_000_000},
            {"type": "weekly", "percentUsed": "not-a-number", "resetsAt": 1_790_000_000_000_i64},
            {"type": "monthly", "resetsAt": "not a date"},
        ]}))
        .expect("limits decode");
        let five_hour = &response.allowances[0];
        assert_eq!(five_hour.used_percent.as_deref(), Some("12.5"));
        assert_eq!(five_hour.resets_at_unix_ms, Some(1_790_000_000_000));
        let weekly = &response.allowances[1];
        assert_eq!(weekly.used_percent, None);
        assert_eq!(weekly.condition, None);
        assert_eq!(weekly.resets_at_unix_ms, Some(1_790_000_000_000));
        assert_eq!(response.allowances[2].resets_at_unix_ms, None);
    }

    #[test]
    fn condition_thresholds() {
        let condition_for = |percent: f64| {
            parse_limits(&json!({"limits": [{"type": "weekly", "percentUsed": percent}]}))
                .expect("limits decode")
                .allowances
                .remove(0)
                .condition
        };
        assert_eq!(condition_for(100.0).as_deref(), Some("exhausted"));
        assert_eq!(condition_for(150.0).as_deref(), Some("exhausted"));
        assert_eq!(condition_for(85.0).as_deref(), Some("tight"));
        assert_eq!(condition_for(80.0).as_deref(), Some("normal"));
        assert_eq!(condition_for(0.0).as_deref(), Some("normal"));
    }

    #[test]
    fn bare_limits_beside_non_object_data_still_parse() {
        let response = parse_limits(&json!({
            "data": "unavailable",
            "limits": [{"type": "monthly", "percentUsed": 10}]
        }))
        .expect("bare limits win when data is not an object");
        assert_eq!(response.allowances[0].key, "monthly");
    }

    #[test]
    fn empty_limit_list_is_a_valid_empty_response() {
        let response = parse_limits(&json!({"success": true, "data": {"limits": []}}))
            .expect("empty limits decode");
        assert!(response.allowances.is_empty());
        assert!(response.models.is_empty());
    }

    #[test]
    fn missing_and_blank_credentials_fail_authentication() {
        let missing = provider("https://x.test", None);
        let error = api_key(&missing).expect_err("missing key fails auth");
        assert!(matches!(error.kind, ErrorKind::Auth));
        let blank = provider("https://x.test", Some("   "));
        let error = api_key(&blank).expect_err("blank key fails auth");
        assert!(matches!(error.kind, ErrorKind::Auth));
    }
}
