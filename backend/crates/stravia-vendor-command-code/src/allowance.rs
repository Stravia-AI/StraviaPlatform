use serde_json::{Map, Value};
use stravia_vendor_sdk::wit::types::HttpRequest;
use stravia_vendor_sdk::{
    AllowanceAmount, AllowanceItem, AllowanceResponse, ErrorKind, GuestHost, PluginError,
    ProviderSnapshot, read_http_body,
};

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const DEFAULT_BASE_URL: &str = "https://api.commandcode.ai";
const PROTOCOL_VERSION: &str = "1.53.1";

pub(crate) fn execute(
    host: &GuestHost,
    provider: &ProviderSnapshot,
) -> Result<AllowanceResponse, PluginError> {
    let credential = super::api_key(provider)?;
    // The billing control plane is fixed even when inference uses a custom base URL.
    let base_url = DEFAULT_BASE_URL.to_string();
    let whoami = execute_request(
        host,
        commandcode_request(&base_url, credential, "/alpha/whoami", &[]),
    )?;
    let org_id = commandcode_org_id(&whoami);
    let org_param = org_id.as_deref().map(|value| ("orgId", value));
    let params = org_param.into_iter().collect::<Vec<_>>();
    let credits = execute_request(
        host,
        commandcode_request(&base_url, credential, "/alpha/billing/credits", &params),
    )?;
    let mut parsed = parse_credits(&credits).ok_or_else(invalid_response)?;

    let subscription_body = optional_request(
        host,
        commandcode_request(
            &base_url,
            credential,
            "/alpha/billing/subscriptions",
            &params,
        ),
    );
    let subscription = subscription_body
        .as_deref()
        .and_then(commandcode_subscription);
    if let Some(subscription) = &subscription {
        parsed.plan_label = subscription.plan_label.clone();
    }

    if let Some(subscription) = &subscription
        && let Some(period_start) = subscription.period_start_raw.as_deref()
    {
        let mut summary_params = params.clone();
        summary_params.push(("since", period_start));
        if let Some(summary) = optional_request(
            host,
            commandcode_request(
                &base_url,
                credential,
                "/alpha/usage/summary",
                &summary_params,
            ),
        ) && let Some(spent) = commandcode_summary_cost(&summary)
            && let Some(remaining) = parsed
                .allowances
                .iter()
                .find(|allowance| allowance.key == "credits_balance")
                .and_then(|allowance| allowance.remaining.as_ref())
                .and_then(|amount| amount.value.parse::<f64>().ok())
            && let Some(period_end) = subscription.period_end
        {
            let window_seconds = subscription
                .period_start
                .filter(|start| period_end > *start)
                .and_then(|start| u64::try_from((period_end - start) / 1000).ok());
            parsed.allowances.push(commandcode_billing_cycle(
                spent,
                remaining,
                Some(period_end),
                window_seconds,
            ));
        }
    }
    Ok(parsed)
}

fn commandcode_request(
    base_url: &str,
    credential: &str,
    path: &str,
    params: &[(&str, &str)],
) -> HttpRequest {
    let mut url = format!("{}{path}", base_url.trim_end_matches('/'));
    if !params.is_empty() {
        url.push('?');
        for (index, (key, value)) in params.iter().enumerate() {
            if index != 0 {
                url.push('&');
            }
            url.push_str(&percent_encode(key));
            url.push('=');
            url.push_str(&percent_encode(value));
        }
    }
    HttpRequest {
        method: "GET".into(),
        url,
        headers: vec![
            ("authorization".into(), format!("Bearer {credential}")),
            ("accept".into(), "application/json".into()),
            ("user-agent".into(), "cli".into()),
            ("x-command-code-version".into(), PROTOCOL_VERSION.into()),
            ("x-cli-environment".into(), "production".into()),
        ],
        body: vec![],
    }
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[(byte >> 4) as usize]));
            encoded.push(char::from(HEX[(byte & 0x0f) as usize]));
        }
    }
    encoded
}

fn execute_request(host: &GuestHost, request: HttpRequest) -> Result<Vec<u8>, PluginError> {
    let response = host.http_start(request)?;
    let status = response.status()?;
    if !(200..300).contains(&status) {
        return Err(status_error(status));
    }
    read_http_body(&response, MAX_RESPONSE_BYTES)
}

fn optional_request(host: &GuestHost, request: HttpRequest) -> Option<Vec<u8>> {
    execute_request(host, request).ok()
}

fn parse_credits(body: &[u8]) -> Option<AllowanceResponse> {
    let payload: Value = serde_json::from_slice(body).ok()?;
    let object = payload.as_object()?;
    let mut allowances = Vec::new();
    if let Some(limits) = object.get("windowLimits").and_then(Value::as_object) {
        for (field, key, seconds) in [("fiveHour", "5h", 18_000), ("weekly", "weekly", 604_800)] {
            let Some(window) = limits.get(field).and_then(Value::as_object) else {
                continue;
            };
            let used = field_number(window, "used");
            let cap = field_number(window, "cap");
            if used.is_none() && cap.is_none() {
                continue;
            }
            let remaining = used.zip(cap).map(|(used, cap)| cap - used);
            let mut allowance = item(key, window_label(key), "quota_window");
            amount_fields(&mut allowance, used, remaining, cap, "credits", None);
            set_percent(&mut allowance, percent_from(used, remaining, cap));
            allowance.window_seconds = Some(seconds);
            allowance.resets_at_unix_ms = field_timestamp(window, "resetAt");
            allowance.condition = condition(&allowance);
            allowances.push(allowance);
        }
    }
    if let Some(credits) = object.get("credits").and_then(Value::as_object) {
        let buckets = [
            field_number(credits, "monthlyCredits"),
            field_number(credits, "purchasedCredits"),
            field_number(credits, "freeCredits"),
        ];
        if buckets.iter().any(Option::is_some) {
            let remaining = buckets.iter().flatten().sum();
            let mut allowance = item(
                "credits_balance",
                window_label("credits_balance"),
                "balance",
            );
            allowance.remaining = Some(amount(remaining, "currency", Some("USD")));
            allowance.resets_at_unix_ms = field_timestamp(credits, "monthlyResetAt");
            allowance.condition = condition(&allowance);
            allowances.push(allowance);
        }
    }
    (!allowances.is_empty()).then_some(AllowanceResponse {
        allowances,
        models: Vec::new(),
        plan_label: None,
    })
}

fn commandcode_org_id(body: &[u8]) -> Option<String> {
    let payload: Value = serde_json::from_slice(body).ok()?;
    payload
        .pointer("/org/id")
        .and_then(non_empty)
        .map(str::to_owned)
}

struct CommandCodeSubscription {
    plan_label: Option<String>,
    period_start_raw: Option<String>,
    period_start: Option<i64>,
    period_end: Option<i64>,
}

fn commandcode_subscription(body: &[u8]) -> Option<CommandCodeSubscription> {
    let payload: Value = serde_json::from_slice(body).ok()?;
    let data = payload.get("data").and_then(Value::as_object)?;
    let plan = data.get("planId").and_then(non_empty);
    let status = data.get("status").and_then(non_empty);
    let plan_label = match (plan, status) {
        (Some(plan), Some(status)) => Some(format!("{plan} ({status})")),
        (Some(plan), None) => Some(plan.into()),
        (None, Some(status)) => Some(status.into()),
        _ => None,
    };
    let period_start_raw = data.get("currentPeriodStart").and_then(|value| {
        non_empty(value)
            .map(str::to_owned)
            .or_else(|| number(value).map(decimal))
    });
    let result = CommandCodeSubscription {
        plan_label,
        period_start_raw,
        period_start: field_timestamp(data, "currentPeriodStart"),
        period_end: field_timestamp(data, "currentPeriodEnd"),
    };
    (result.plan_label.is_some() || result.period_start.is_some() || result.period_end.is_some())
        .then_some(result)
}

fn commandcode_summary_cost(body: &[u8]) -> Option<f64> {
    let payload: Value = serde_json::from_slice(body).ok()?;
    payload.get("totalCost").and_then(number)
}

fn commandcode_billing_cycle(
    spent: f64,
    remaining: f64,
    reset: Option<i64>,
    seconds: Option<u64>,
) -> AllowanceItem {
    let limit = spent + remaining;
    let mut allowance = item(
        "billing_cycle",
        window_label("billing_cycle"),
        "quota_window",
    );
    amount_fields(
        &mut allowance,
        Some(spent),
        Some(remaining),
        Some(limit),
        "currency",
        Some("USD"),
    );
    set_percent(
        &mut allowance,
        percent_from(Some(spent), Some(remaining), Some(limit)),
    );
    allowance.window_seconds = seconds;
    allowance.resets_at_unix_ms = reset;
    allowance.condition = condition(&allowance);
    allowance
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
        value: decimal(value),
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

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse().ok())
        .filter(|value| value.is_finite())
}

fn field_number(object: &Map<String, Value>, key: &str) -> Option<f64> {
    object.get(key).and_then(number)
}

fn non_empty(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn timestamp_millis(value: &Value) -> Option<i64> {
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

fn field_timestamp(object: &Map<String, Value>, key: &str) -> Option<i64> {
    object.get(key).and_then(timestamp_millis)
}

fn set_percent(item: &mut AllowanceItem, value: Option<f64>) {
    item.used_percent = value.map(decimal);
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
    Some(
        if remaining_percent < 20.0 {
            "tight"
        } else {
            "normal"
        }
        .into(),
    )
}

fn window_label(key: &str) -> String {
    match key {
        "5h" => "5-hour window".into(),
        "weekly" => "Weekly window".into(),
        "billing_cycle" => "Billing cycle".into(),
        "credits_balance" => "Credit balance".into(),
        other => other.into(),
    }
}

fn decimal(value: f64) -> String {
    value.to_string()
}

fn status_error(status: u16) -> PluginError {
    let kind = match status {
        401 | 403 => ErrorKind::Auth,
        _ => ErrorKind::upstream_unknown(),
    };
    PluginError {
        kind,
        message: "upstream allowance request failed".into(),
        upstream_status: Some(status),
    }
}

fn invalid_response() -> PluginError {
    PluginError {
        kind: ErrorKind::upstream_unknown(),
        message: "upstream allowance response has an unsupported shape".into(),
        upstream_status: None,
    }
}
