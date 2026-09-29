//! Claude 订阅额度：`GET /api/oauth/usage`。响应形态与请求头跟随 oh-my-pi
//! v18.4.2 `packages/ai/src/usage/claude.ts`；新版以 `limits[]` 描述各窗口，
//! 旧版以 `five_hour` / `seven_day` 对象给出 `utilization` 百分比。

use std::collections::BTreeMap;

use serde_json::{Map, Value};
use stravia_vendor_common::common;
use stravia_vendor_sdk::{
    AllowanceAmount, AllowanceItem, AllowanceResponse, GuestHost, HttpRequest, ModelAllowance,
    PluginError, ProviderSnapshot, read_http_body,
};

use crate::request::{DEFAULT_CLIENT_VERSION, cli_user_agent};
use crate::{access_token, retryable};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const USAGE_BETAS: &str = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,advanced-tool-use-2025-11-20,effort-2025-11-24,extended-cache-ttl-2025-04-11";
const MAX_USAGE_BODY: usize = 1024 * 1024;
const FIVE_HOURS: u64 = 18_000;
const SEVEN_DAYS: u64 = 604_800;

pub(crate) fn execute(
    host: &GuestHost,
    provider: &ProviderSnapshot,
) -> Result<AllowanceResponse, PluginError> {
    let response = host.http_start(HttpRequest {
        method: "GET".into(),
        url: USAGE_URL.into(),
        headers: vec![
            ("accept".into(), "application/json, text/plain, */*".into()),
            ("anthropic-beta".into(), USAGE_BETAS.into()),
            ("content-type".into(), "application/json".into()),
            ("user-agent".into(), cli_user_agent(DEFAULT_CLIENT_VERSION)),
            (
                "authorization".into(),
                format!("Bearer {}", access_token(provider)?),
            ),
        ],
        body: Vec::new(),
    })?;
    let status = response.status()?;
    let headers = response.headers()?;
    let body = read_http_body(&response, MAX_USAGE_BODY)?;
    if !(200..300).contains(&status) {
        return Err(common::upstream_error(status, &headers, &body));
    }
    let payload: Value = serde_json::from_slice(&body)
        .map_err(|_| retryable("Claude usage returned invalid JSON"))?;
    parse(&payload).ok_or_else(|| retryable("Claude usage response has no allowance windows"))
}

fn parse(payload: &Value) -> Option<AllowanceResponse> {
    let object = payload.as_object()?;
    let mut allowances = Vec::new();
    let mut models: BTreeMap<String, Vec<AllowanceItem>> = BTreeMap::new();
    if let Some(limits) = object
        .get("limits")
        .and_then(Value::as_array)
        .filter(|limits| !limits.is_empty())
    {
        for limit in limits.iter().filter_map(Value::as_object) {
            let (key, seconds) = match limit.get("kind").and_then(Value::as_str) {
                Some("session") => ("5h", FIVE_HOURS),
                Some("weekly_all" | "weekly_scoped") => ("7d", SEVEN_DAYS),
                _ => continue,
            };
            let mut allowance = window(key, seconds, limit, "percent");
            if limit.get("kind").and_then(Value::as_str) == Some("weekly_scoped") {
                let Some(model) = limit
                    .get("scope")
                    .and_then(|scope| scope.pointer("/model/display_name"))
                    .and_then(non_empty)
                else {
                    continue;
                };
                allowance.condition = condition(&allowance);
                models.entry(model.to_owned()).or_default().push(allowance);
            } else {
                allowance.condition = condition(&allowance);
                allowances.push(allowance);
            }
        }
    } else {
        for (field, key, seconds) in [
            ("five_hour", "5h", FIVE_HOURS),
            ("seven_day", "7d", SEVEN_DAYS),
        ] {
            let Some(limit) = object.get(field).and_then(Value::as_object) else {
                continue;
            };
            let mut allowance = window(key, seconds, limit, "utilization");
            allowance.condition = condition(&allowance);
            allowances.push(allowance);
        }
    }
    if let Some(spend) = object.get("spend").and_then(Value::as_object)
        && spend.get("enabled").and_then(Value::as_bool) == Some(true)
    {
        allowances.push(extra_usage(spend));
    }
    if allowances.is_empty() && models.is_empty() {
        return None;
    }
    Some(AllowanceResponse {
        allowances,
        models: models
            .into_iter()
            .map(|(model, allowances)| ModelAllowance { model, allowances })
            .collect(),
        plan_label: None,
    })
}

fn window(
    key: &str,
    seconds: u64,
    limit: &Map<String, Value>,
    percent_field: &str,
) -> AllowanceItem {
    let mut allowance = item(
        key,
        if key == "5h" {
            "5-hour window"
        } else {
            "Weekly window"
        },
        "quota_window",
    );
    allowance.used_percent = limit.get(percent_field).and_then(number).map(decimal);
    allowance.resets_at_unix_ms = limit.get("resets_at").and_then(timestamp_millis);
    allowance.window_seconds = Some(seconds);
    allowance
}

fn extra_usage(spend: &Map<String, Value>) -> AllowanceItem {
    let used = spend.get("used").and_then(money_amount);
    let limit = spend.get("limit").and_then(money_amount);
    let remaining = used.zip(limit).map(|(used, limit)| limit - used);
    let currency = ["used", "limit"]
        .iter()
        .find_map(|field| spend.get(*field)?.get("currency").and_then(non_empty));
    let money = |value: f64| AllowanceAmount {
        value: decimal(value),
        unit: "currency".into(),
        currency: currency.map(str::to_owned),
    };
    let mut allowance = item("extra_usage", "Extra usage", "balance");
    allowance.used = used.map(money);
    allowance.remaining = remaining.map(money);
    allowance.limit = limit.map(money);
    allowance.used_percent = spend
        .get("percent")
        .and_then(number)
        .or_else(|| {
            used.zip(limit)
                .filter(|(_, limit)| *limit > 0.0)
                .map(|(used, limit)| used / limit * 100.0)
        })
        .map(decimal);
    allowance.condition = condition(&allowance);
    allowance
}

fn item(key: &str, label: &str, kind: &str) -> AllowanceItem {
    AllowanceItem {
        key: key.into(),
        label: label.into(),
        kind: kind.into(),
        used: None,
        remaining: None,
        limit: None,
        used_percent: None,
        window_seconds: None,
        resets_at_unix_ms: None,
        condition: None,
    }
}

fn condition(item: &AllowanceItem) -> Option<String> {
    let used = item.used_percent.as_deref()?.parse::<f64>().ok()?;
    Some(
        if used >= 100.0 {
            "exhausted"
        } else if used > 80.0 {
            "tight"
        } else {
            "normal"
        }
        .into(),
    )
}

fn money_amount(value: &Value) -> Option<f64> {
    let object = value.as_object()?;
    let exponent = object.get("exponent").and_then(number).unwrap_or(2.0);
    Some(object.get("amount_minor").and_then(number)? / 10_f64.powf(exponent))
}

fn decimal(value: f64) -> String {
    value.to_string()
}

fn non_empty(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse().ok())
        .filter(|value: &f64| value.is_finite())
}

/// `resets_at` 为 RFC 3339 时间（例如 `2026-09-29T05:00:00.123456+00:00`）或
/// 秒/毫秒时间戳。
fn timestamp_millis(value: &Value) -> Option<i64> {
    if let Some(text) = value.as_str() {
        return chrono::DateTime::parse_from_rfc3339(text.trim())
            .ok()
            .map(|time| time.timestamp_millis());
    }
    let number = number(value)?;
    let millis = if number.abs() < 1_000_000_000_000.0 {
        number * 1000.0
    } else {
        number
    };
    Some(millis.round() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn limits_payload_maps_shared_windows_scoped_models_and_extra_usage() {
        let response = parse(&json!({
            "limits": [
                {"kind": "session", "percent": 42, "resets_at": "2026-09-29T05:00:00.5+00:00"},
                {"kind": "weekly_all", "percent": 100, "resets_at": "2026-10-01T00:00:00Z"},
                {"kind": "weekly_scoped", "percent": 85, "scope": {"model": {"display_name": "Opus"}}},
                {"kind": "unknown", "percent": 1}
            ],
            "spend": {
                "enabled": true,
                "used": {"amount_minor": 250, "currency": "USD"},
                "limit": {"amount_minor": 1000, "exponent": 2, "currency": "USD"}
            }
        }))
        .unwrap();
        assert_eq!(response.allowances.len(), 3);
        let session = &response.allowances[0];
        assert_eq!(
            (session.key.as_str(), session.window_seconds),
            ("5h", Some(FIVE_HOURS))
        );
        assert_eq!(session.used_percent.as_deref(), Some("42"));
        assert_eq!(session.resets_at_unix_ms, Some(1_790_658_000_500));
        assert_eq!(session.condition.as_deref(), Some("normal"));
        assert_eq!(
            response.allowances[1].condition.as_deref(),
            Some("exhausted")
        );
        assert_eq!(
            response.allowances[1].resets_at_unix_ms,
            Some(1_790_812_800_000)
        );
        let extra = &response.allowances[2];
        assert_eq!(
            extra.remaining.as_ref().map(|amount| amount.value.as_str()),
            Some("7.5")
        );
        assert_eq!(extra.used_percent.as_deref(), Some("25"));
        assert_eq!(response.models.len(), 1);
        assert_eq!(response.models[0].model, "Opus");
        assert_eq!(
            response.models[0].allowances[0].condition.as_deref(),
            Some("tight")
        );
    }

    #[test]
    fn legacy_payload_uses_utilization_and_empty_payload_is_rejected() {
        let response = parse(&json!({
            "five_hour": {"utilization": 12.5, "resets_at": null},
            "seven_day": {"utilization": 3}
        }))
        .unwrap();
        let keys: Vec<_> = response
            .allowances
            .iter()
            .map(|item| item.key.as_str())
            .collect();
        assert_eq!(keys, ["5h", "7d"]);
        assert_eq!(response.allowances[0].used_percent.as_deref(), Some("12.5"));
        assert!(parse(&json!({"spend": {"enabled": false}})).is_none());
    }
}
