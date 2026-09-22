use base64::Engine;
use serde_json::{Map, Value};
use stravia_vendor_sdk::{
    AllowanceAmount, AllowanceItem, AllowanceResponse, ErrorKind, GuestHost, PluginError,
    ProviderSnapshot,
};

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

pub(super) fn execute(
    host: &GuestHost,
    provider: &ProviderSnapshot,
) -> Result<AllowanceResponse, PluginError> {
    let access_token = ["access_token", "api_key", "apiKey", "token"]
        .into_iter()
        .find_map(|key| provider.credentials.get(key).and_then(non_empty))
        .ok_or_else(|| PluginError {
            kind: ErrorKind::Auth,
            message: "provider credential is missing".into(),
            upstream_status: None,
        })?;
    let mut headers = vec![
        ("authorization".into(), format!("Bearer {access_token}")),
        ("accept".into(), "application/json".into()),
    ];
    if let Some(account_id) = account_id(provider, access_token) {
        headers.push(("chatgpt-account-id".into(), account_id));
    }
    let response = host.http_start(stravia_vendor_sdk::wit::types::HttpRequest {
        method: "GET".into(),
        url: USAGE_URL.into(),
        headers,
        body: Vec::new(),
    })?;
    let status = response.status()?;
    if !(200..300).contains(&status) {
        return Err(PluginError {
            kind: if matches!(status, 401 | 403) {
                ErrorKind::Auth
            } else {
                ErrorKind::upstream_unknown()
            },
            message: "upstream allowance request failed".into(),
            upstream_status: Some(status),
        });
    }
    let body = stravia_vendor_sdk::read_http_body(&response, MAX_RESPONSE_BYTES)?;
    parse(&body).ok_or_else(|| PluginError {
        kind: ErrorKind::upstream_unknown(),
        message: "upstream allowance response has an unsupported shape".into(),
        upstream_status: None,
    })
}

fn account_id(provider: &ProviderSnapshot, access_token: &str) -> Option<String> {
    if let Some(value) = ["account_id", "chatgpt_account_id", "chatgpt-account-id"]
        .into_iter()
        .find_map(|key| provider.credentials.get(key).and_then(non_empty))
    {
        return Some(value.to_owned());
    }
    let payload = access_token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    claims
        .get("https://api.openai.com/auth")
        .and_then(Value::as_object)
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(non_empty)
        .or_else(|| {
            claims
                .get("https://api.openai.com/auth.chatgpt_account_id")
                .and_then(non_empty)
        })
        .map(str::to_owned)
}

fn parse(body: &[u8]) -> Option<AllowanceResponse> {
    let payload: Value = serde_json::from_slice(body).ok()?;
    let object = payload.as_object()?;
    let mut allowances = Vec::new();
    if let Some(rate_limit) = object.get("rate_limit").and_then(Value::as_object) {
        for field in ["primary_window", "secondary_window"] {
            let Some(window) = rate_limit.get(field).and_then(Value::as_object) else {
                continue;
            };
            let seconds = field_number(window, "limit_window_seconds")
                .filter(|value| *value >= 0.0 && *value <= u64::MAX as f64)
                .map(|value| value.round() as u64);
            let key = window_key(seconds);
            let mut allowance = item(&key, window_label(&key), "quota_window");
            set_percent(&mut allowance, field_number(window, "used_percent"));
            allowance.window_seconds = seconds;
            allowance.resets_at_unix_ms = field_timestamp(window, "reset_at");
            if allowance.used_percent.is_some()
                || allowance.resets_at_unix_ms.is_some()
                || seconds.is_some()
            {
                allowances.push(allowance);
            }
        }
    }
    if let Some(credits) = object.get("credits").and_then(Value::as_object) {
        let unlimited = credits.get("unlimited").and_then(Value::as_bool) == Some(true);
        let balance = field_number(credits, "balance");
        if unlimited || balance.is_some() {
            let key = if unlimited {
                "credits_unlimited"
            } else {
                "credits_balance"
            };
            let mut allowance = item(key, window_label(key), "balance");
            allowance.remaining = balance.map(|value| amount(value, "currency", Some("USD")));
            allowances.push(allowance);
        }
    }
    if let Some(spend) = payload
        .pointer("/spend_control/individual_limit")
        .and_then(Value::as_object)
    {
        let used = field_number(spend, "used");
        let limit = field_number(spend, "limit");
        let remaining = used.zip(limit).map(|(used, limit)| limit - used);
        let mut allowance = item("credits", "Credit limit", "balance");
        amount_fields(
            &mut allowance,
            used,
            remaining,
            limit,
            "currency",
            Some("USD"),
        );
        set_percent(
            &mut allowance,
            field_number(spend, "used_percent").or_else(|| percent_from(used, remaining, limit)),
        );
        if allowance.used.is_some() || allowance.limit.is_some() || allowance.used_percent.is_some()
        {
            allowances.push(allowance);
        }
    }
    if allowances.is_empty() {
        return None;
    }
    for allowance in &mut allowances {
        allowance.condition = condition(allowance);
    }
    Some(AllowanceResponse {
        allowances,
        models: Vec::new(),
        plan_label: None,
    })
}

fn item(key: impl Into<String>, label: impl Into<String>, kind: &str) -> AllowanceItem {
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

fn amount(value: f64, unit: &str, currency: Option<&str>) -> AllowanceAmount {
    AllowanceAmount {
        value: value.to_string(),
        unit: unit.into(),
        currency: currency.map(str::to_owned),
    }
}

fn amount_fields(
    item: &mut AllowanceItem,
    used: Option<f64>,
    remaining: Option<f64>,
    limit: Option<f64>,
    unit: &str,
    currency: Option<&str>,
) {
    item.used = used.map(|value| amount(value, unit, currency));
    item.remaining = remaining.map(|value| amount(value, unit, currency));
    item.limit = limit.map(|value| amount(value, unit, currency));
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

fn field_number(object: &Map<String, Value>, key: &str) -> Option<f64> {
    object.get(key).and_then(number)
}

fn field_timestamp(object: &Map<String, Value>, key: &str) -> Option<i64> {
    let value = object.get(key)?;
    if let Some(value) = number(value) {
        let millis = if value.abs() < 1_000_000_000_000.0 {
            value * 1000.0
        } else {
            value
        };
        return (millis >= i64::MIN as f64 && millis <= i64::MAX as f64)
            .then_some(millis.round() as i64);
    }
    chrono::DateTime::parse_from_rfc3339(value.as_str()?)
        .ok()
        .map(|value| value.timestamp_millis())
}

fn set_percent(item: &mut AllowanceItem, value: Option<f64>) {
    item.used_percent = value.map(|value| value.to_string());
}

fn percent_from(used: Option<f64>, remaining: Option<f64>, limit: Option<f64>) -> Option<f64> {
    limit.filter(|limit| *limit > 0.0).and_then(|limit| {
        used.map(|used| used / limit * 100.0)
            .or_else(|| remaining.map(|remaining| (1.0 - remaining / limit) * 100.0))
    })
}

fn condition(item: &AllowanceItem) -> Option<String> {
    let used = item
        .used_percent
        .as_deref()
        .and_then(|value| value.parse::<f64>().ok());
    let remaining = item
        .remaining
        .as_ref()
        .and_then(|value| value.value.parse::<f64>().ok());
    if used.is_some_and(|value| value.is_finite() && value >= 100.0)
        || remaining.is_some_and(|value| value.is_finite() && value <= 0.0)
    {
        return Some("exhausted".into());
    }
    let remaining_percent = used
        .filter(|value| value.is_finite())
        .map(|value| 100.0 - value)
        .or_else(|| {
            let limit = item.limit.as_ref()?.value.parse::<f64>().ok()?;
            let remaining = remaining?;
            (limit.is_finite() && remaining.is_finite() && limit > 0.0)
                .then_some(remaining / limit * 100.0)
        })?;
    Some(if remaining_percent < 20.0 {
        "tight".into()
    } else {
        "normal".into()
    })
}

fn window_key(window_seconds: Option<u64>) -> String {
    match window_seconds {
        Some(604_800) => "weekly".into(),
        Some(seconds) if seconds % 86_400 == 0 => format!("{}d", seconds / 86_400),
        Some(seconds) if seconds % 3_600 == 0 => format!("{}h", seconds / 3_600),
        Some(seconds) => format!("{seconds}s"),
        None => "tokens".into(),
    }
}

fn window_label(key: &str) -> String {
    match key {
        "5h" => "5-hour window".into(),
        "7d" | "weekly" => "Weekly window".into(),
        "credits_balance" => "Credit balance".into(),
        "tokens" => "Tokens".into(),
        other => other.into(),
    }
}
