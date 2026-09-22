use std::collections::HashMap;

use serde_json::{Map, Value};
use stravia_vendor_sdk::{AllowanceAmount, AllowanceItem, AllowanceResponse, ModelAllowance};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Monitor {
    AnthropicClaudeCode,
    GitHubCopilot,
    KimiCoding,
    NanoGpt,
    ZaiCodingPlan,
    ZhipuAiCodingPlan,
    MiniMaxCodingPlan,
    MiniMaxCnCodingPlan,
    Wafer,
    OpenCodeGo,
    Crof,
    DeepSeek,
    NeuralWatt,
}

pub(super) fn parse(monitor: Monitor, body: &[u8]) -> Result<AllowanceResponse, ()> {
    let payload: Value = serde_json::from_slice(body).map_err(|_| ())?;
    let parsed = match monitor {
        Monitor::AnthropicClaudeCode => parse_anthropic(&payload),
        Monitor::GitHubCopilot => parse_github_copilot(&payload),
        Monitor::KimiCoding => parse_kimi(&payload),
        Monitor::NanoGpt => parse_nano_gpt(&payload),
        Monitor::ZaiCodingPlan => parse_zai(&payload),
        Monitor::ZhipuAiCodingPlan => parse_zhipu(&payload),
        Monitor::MiniMaxCodingPlan | Monitor::MiniMaxCnCodingPlan => parse_minimax(&payload, true),
        Monitor::Wafer => parse_wafer(&payload),
        Monitor::OpenCodeGo => parse_opencode_go(&payload),
        Monitor::Crof => parse_crof(&payload),
        Monitor::DeepSeek => parse_deepseek(&payload),
        Monitor::NeuralWatt => parse_neuralwatt(&payload),
    }?;
    require(parsed)
}

pub(super) fn parse_minimax_fallback(body: &[u8]) -> Result<AllowanceResponse, ()> {
    let payload: Value = serde_json::from_slice(body).map_err(|_| ())?;
    require(parse_minimax(&payload, false)?)
}

fn require(mut response: AllowanceResponse) -> Result<AllowanceResponse, ()> {
    if response.allowances.is_empty()
        && response
            .models
            .iter()
            .all(|model| model.allowances.is_empty())
    {
        return Err(());
    }
    for item in &mut response.allowances {
        item.condition = condition(item);
    }
    Ok(response)
}

fn response(allowances: Vec<AllowanceItem>) -> AllowanceResponse {
    AllowanceResponse {
        allowances,
        models: Vec::new(),
        plan_label: None,
    }
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

fn decimal(value: f64) -> String {
    value.to_string()
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
        .filter(|value: &f64| value.is_finite())
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
        "daily" => "Daily window".into(),
        "monthly" => "Monthly window".into(),
        "billing_cycle" => "Billing cycle".into(),
        "credits_balance" => "Credit balance".into(),
        "premium_interactions" => "Premium interactions".into(),
        "mcp_tools" => "MCP tools".into(),
        "extra_usage" => "Extra usage".into(),
        "tokens" => "Tokens".into(),
        other => other.into(),
    }
}

fn parse_anthropic(payload: &Value) -> Result<AllowanceResponse, ()> {
    let object = payload.as_object().ok_or(())?;
    let mut allowances = Vec::new();
    let mut models_by_name: HashMap<String, Vec<AllowanceItem>> = HashMap::new();
    if let Some(limits) = object
        .get("limits")
        .and_then(Value::as_array)
        .filter(|v| !v.is_empty())
    {
        for value in limits {
            let Some(limit) = value.as_object() else {
                continue;
            };
            let key = match limit.get("kind").and_then(non_empty) {
                Some("session") => Some("5h"),
                Some("weekly_all") => Some("7d"),
                Some("weekly_scoped") => None,
                _ => continue,
            };
            let mut allowance = item(
                key.unwrap_or("7d"),
                window_label(key.unwrap_or("7d")),
                "quota_window",
            );
            set_percent(&mut allowance, field_number(limit, "percent"));
            allowance.resets_at_unix_ms = field_timestamp(limit, "resets_at");
            allowance.window_seconds = Some(if key == Some("5h") { 18_000 } else { 604_800 });
            if limit.get("kind").and_then(non_empty) == Some("weekly_scoped") {
                let model = value
                    .pointer("/scope/model/display_name")
                    .and_then(non_empty)
                    .ok_or(())?;
                models_by_name
                    .entry(model.into())
                    .or_default()
                    .push(allowance);
            } else {
                allowances.push(allowance);
            }
        }
    } else {
        for (field, key, seconds) in [("five_hour", "5h", 18_000), ("seven_day", "7d", 604_800)] {
            let Some(limit) = object.get(field).and_then(Value::as_object) else {
                continue;
            };
            let mut allowance = item(key, window_label(key), "quota_window");
            set_percent(&mut allowance, field_number(limit, "utilization"));
            allowance.resets_at_unix_ms = field_timestamp(limit, "resets_at");
            allowance.window_seconds = Some(seconds);
            allowances.push(allowance);
        }
    }
    if let Some(spend) = object.get("spend").and_then(Value::as_object)
        && spend.get("enabled").and_then(Value::as_bool) == Some(true)
    {
        let used = spend.get("used").and_then(money_amount);
        let limit = spend.get("limit").and_then(money_amount);
        let remaining = used.zip(limit).map(|(used, limit)| limit - used);
        let currency = spend
            .get("used")
            .and_then(|v| v.get("currency"))
            .and_then(non_empty)
            .or_else(|| {
                spend
                    .get("limit")
                    .and_then(|v| v.get("currency"))
                    .and_then(non_empty)
            });
        let mut allowance = item("extra_usage", window_label("extra_usage"), "balance");
        amount_fields(&mut allowance, used, remaining, limit, "currency", currency);
        let percent =
            field_number(spend, "percent").or_else(|| percent_from(used, remaining, limit));
        set_percent(&mut allowance, percent);
        allowances.push(allowance);
    }
    let mut models = models_by_name
        .into_iter()
        .map(|(model, allowances)| ModelAllowance { model, allowances })
        .collect::<Vec<_>>();
    models.sort_by(|left, right| left.model.cmp(&right.model));
    Ok(AllowanceResponse {
        allowances,
        models,
        plan_label: None,
    })
}

fn money_amount(value: &Value) -> Option<f64> {
    let object = value.as_object()?;
    Some(
        field_number(object, "amount_minor")?
            / 10_f64.powf(field_number(object, "exponent").unwrap_or(2.0)),
    )
}

fn parse_github_copilot(payload: &Value) -> Result<AllowanceResponse, ()> {
    let snapshot = payload
        .pointer("/quota_snapshots/premium_interactions")
        .and_then(Value::as_object)
        .ok_or(())?;
    let mut allowance = item(
        "premium_interactions",
        window_label("premium_interactions"),
        "request_allowance",
    );
    if snapshot.get("unlimited").and_then(Value::as_bool) != Some(true) {
        let limit = field_number(snapshot, "entitlement");
        let remaining = field_number(snapshot, "remaining");
        let used = limit
            .zip(remaining)
            .map(|(limit, remaining)| limit - remaining);
        amount_fields(&mut allowance, used, remaining, limit, "requests", None);
        set_percent(
            &mut allowance,
            percent_from(used, remaining, limit)
                .or_else(|| field_number(snapshot, "percent_remaining").map(|value| 100.0 - value)),
        );
    }
    allowance.resets_at_unix_ms = payload.get("quota_reset_date").and_then(timestamp_millis);
    Ok(response(vec![allowance]))
}

fn parse_kimi(payload: &Value) -> Result<AllowanceResponse, ()> {
    let object = payload.as_object().ok_or(())?;
    let mut allowances = Vec::new();
    if let Some(usage) = object.get("usage").and_then(Value::as_object) {
        let limit = field_number(usage, "limit");
        let used = field_number(usage, "used");
        let remaining =
            field_number(usage, "remaining").or_else(|| used.zip(limit).map(|(u, l)| l - u));
        let mut allowance = item("weekly", window_label("weekly"), "quota_window");
        amount_fields(&mut allowance, used, remaining, limit, "units", None);
        set_percent(&mut allowance, percent_from(used, remaining, limit));
        allowance.resets_at_unix_ms = field_timestamp(usage, "resetTime");
        if allowance.used.is_some()
            || allowance.remaining.is_some()
            || allowance.resets_at_unix_ms.is_some()
        {
            allowances.push(allowance);
        }
    }
    if let Some(limits) = object.get("limits").and_then(Value::as_array) {
        for entry in limits {
            let (Some(window), Some(detail)) = (
                entry.get("window").and_then(Value::as_object),
                entry.get("detail").and_then(Value::as_object),
            ) else {
                continue;
            };
            let seconds = duration_seconds(
                field_number(window, "duration"),
                window.get("timeUnit").and_then(non_empty),
            );
            let key = window_key(seconds);
            let limit = field_number(detail, "limit");
            let used = field_number(detail, "used");
            let remaining =
                field_number(detail, "remaining").or_else(|| used.zip(limit).map(|(u, l)| l - u));
            let mut allowance = item(&key, window_label(&key), "quota_window");
            amount_fields(&mut allowance, used, remaining, limit, "units", None);
            set_percent(&mut allowance, percent_from(used, remaining, limit));
            allowance.window_seconds = seconds;
            allowance.resets_at_unix_ms = field_timestamp(detail, "resetTime");
            if allowance.used.is_some()
                || allowance.remaining.is_some()
                || allowance.resets_at_unix_ms.is_some()
            {
                allowances.push(allowance);
            }
        }
    }
    Ok(response(allowances))
}

fn duration_seconds(duration: Option<f64>, unit: Option<&str>) -> Option<u64> {
    let multiplier = match unit?.to_ascii_lowercase().as_str() {
        "second" | "seconds" => 1.0,
        "minute" | "minutes" => 60.0,
        "hour" | "hours" => 3_600.0,
        "day" | "days" => 86_400.0,
        "week" | "weeks" => 604_800.0,
        _ => return None,
    };
    let seconds = duration? * multiplier;
    (seconds.is_finite() && seconds >= 0.0 && seconds <= u64::MAX as f64)
        .then_some(seconds.round() as u64)
}

fn parse_nano_gpt(payload: &Value) -> Result<AllowanceResponse, ()> {
    let object = payload.as_object().ok_or(())?;
    let mut allowances = Vec::new();
    for (field, key, seconds) in [
        ("daily", "daily", Some(86_400)),
        ("monthly", "monthly", None),
    ] {
        let Some(window) = object.get(field).and_then(Value::as_object) else {
            continue;
        };
        let used = field_number(window, "used");
        let limit = field_number(window, "limit").or_else(|| {
            window
                .get("limits")
                .and_then(Value::as_object)?
                .get(field)
                .and_then(number)
        });
        let remaining = used.zip(limit).map(|(u, l)| l - u);
        let mut allowance = item(key, window_label(key), "quota_window");
        amount_fields(&mut allowance, used, remaining, limit, "units", None);
        set_percent(
            &mut allowance,
            field_number(window, "percentUsed")
                .map(|v| v * 100.0)
                .or_else(|| percent_from(used, remaining, limit)),
        );
        allowance.window_seconds = seconds;
        allowance.resets_at_unix_ms = field_timestamp(window, "resetAt").or_else(|| {
            (field == "monthly")
                .then(|| {
                    payload
                        .pointer("/period/currentPeriodEnd")
                        .and_then(timestamp_millis)
                })
                .flatten()
        });
        if allowance.used_percent.is_some()
            || allowance.used.is_some()
            || allowance.resets_at_unix_ms.is_some()
        {
            allowances.push(allowance);
        }
    }
    Ok(response(allowances))
}

fn parse_zai(payload: &Value) -> Result<AllowanceResponse, ()> {
    let data = payload.get("data").and_then(Value::as_object).ok_or(())?;
    let limits = data.get("limits").and_then(Value::as_array).ok_or(())?;
    let mut allowances = Vec::new();
    for value in limits {
        let Some(limit) = value.as_object() else {
            continue;
        };
        match limit.get("type").and_then(non_empty) {
            Some("TOKENS_LIMIT" | "CREDIT_LIMIT") => {
                let seconds = zai_window_seconds(limit);
                let key = window_key(seconds);
                let used = field_number(limit, "currentValue");
                let total = field_number(limit, "usage");
                let remaining = field_number(limit, "remaining")
                    .or_else(|| used.zip(total).map(|(u, t)| t - u));
                let unit = if limit.get("type").and_then(non_empty) == Some("CREDIT_LIMIT") {
                    "credits"
                } else {
                    "units"
                };
                let mut allowance = item(&key, window_label(&key), "quota_window");
                amount_fields(&mut allowance, used, remaining, total, unit, None);
                set_percent(
                    &mut allowance,
                    field_number(limit, "percentage")
                        .or_else(|| percent_from(used, remaining, total)),
                );
                allowance.window_seconds = seconds;
                allowance.resets_at_unix_ms = field_timestamp(limit, "nextResetTime");
                allowances.push(allowance);
            }
            Some("TIME_LIMIT") => {
                let mut allowance =
                    item("mcp_tools", window_label("mcp_tools"), "request_allowance");
                set_percent(&mut allowance, field_number(limit, "percentage"));
                allowance.window_seconds = Some(30 * 86_400);
                allowance.resets_at_unix_ms = field_timestamp(limit, "nextResetTime");
                allowances.push(allowance);
            }
            _ => {}
        }
    }
    Ok(AllowanceResponse {
        allowances,
        models: Vec::new(),
        plan_label: data.get("level").and_then(non_empty).map(str::to_owned),
    })
}

fn zai_window_seconds(limit: &Map<String, Value>) -> Option<u64> {
    let base = match field_number(limit, "unit")? as i64 {
        3 => 3_600.0,
        6 => 604_800.0,
        _ => return None,
    };
    let seconds = field_number(limit, "number")? * base;
    (seconds.is_finite() && seconds > 0.0 && seconds <= u64::MAX as f64)
        .then_some(seconds.round() as u64)
}

fn parse_zhipu(payload: &Value) -> Result<AllowanceResponse, ()> {
    let limits = payload
        .pointer("/data/limits")
        .and_then(Value::as_array)
        .ok_or(())?;
    let mut allowances = Vec::new();
    for value in limits {
        let Some(limit) = value.as_object() else {
            continue;
        };
        match limit.get("type").and_then(non_empty) {
            Some("TOKENS_LIMIT") => {
                let seconds = zai_window_seconds(limit);
                let key = window_key(seconds);
                let mut allowance = item(&key, window_label(&key), "quota_window");
                set_percent(&mut allowance, field_number(limit, "percentage"));
                allowance.window_seconds = seconds;
                allowance.resets_at_unix_ms = field_timestamp(limit, "nextResetTime");
                allowances.push(allowance);
            }
            Some("TIME_LIMIT") => {
                let mut allowance =
                    item("mcp_tools", window_label("mcp_tools"), "request_allowance");
                set_percent(&mut allowance, field_number(limit, "percentage"));
                allowance.window_seconds = Some(30 * 86_400);
                allowance.resets_at_unix_ms = field_timestamp(limit, "nextResetTime");
                allowances.push(allowance);
            }
            _ => {}
        }
    }
    Ok(response(allowances))
}

fn parse_minimax(payload: &Value, token_plan: bool) -> Result<AllowanceResponse, ()> {
    let object = payload.as_object().ok_or(())?;
    if let Some(base) = object.get("base_resp").and_then(Value::as_object)
        && field_number(base, "status_code") != Some(0.0)
    {
        return Err(());
    }
    let models = object
        .get("model_remains")
        .and_then(Value::as_array)
        .ok_or(())?;
    let model = pick_minimax_model(models).ok_or(())?;
    let mut allowances = Vec::new();
    let total = field_number(model, "current_interval_total_count");
    let raw = field_number(model, "current_interval_usage_count");
    let remaining = if token_plan {
        raw
    } else {
        total.zip(raw).map(|(t, u)| t - u)
    };
    let used = total.zip(remaining).map(|(t, r)| t - r);
    let reset = field_timestamp(model, "end_time");
    let mut interval = item("5h", window_label("5h"), "quota_window");
    amount_fields(&mut interval, used, remaining, total, "units", None);
    set_percent(
        &mut interval,
        field_number(model, "current_interval_remaining_percent")
            .map(|r| 100.0 - r)
            .or_else(|| percent_from(used, remaining, total)),
    );
    interval.window_seconds = minimax_window_seconds(
        field_timestamp(model, "start_time"),
        reset,
        field_number(model, "remains_time"),
    );
    interval.resets_at_unix_ms = reset;
    if interval.used_percent.is_some()
        || interval.used.is_some()
        || interval.resets_at_unix_ms.is_some()
    {
        allowances.push(interval);
    }
    if field_number(model, "current_weekly_status") != Some(3.0) {
        let total = field_number(model, "current_weekly_total_count");
        let raw = field_number(model, "current_weekly_usage_count");
        let remaining = if token_plan {
            raw
        } else {
            total.zip(raw).map(|(t, u)| t - u)
        };
        let used = total.zip(remaining).map(|(t, r)| t - r);
        let percent = field_number(model, "current_weekly_remaining_percent")
            .map(|r| 100.0 - r)
            .or_else(|| percent_from(used, remaining, total));
        if percent.is_some() || total.is_some() {
            let reset = field_timestamp(model, "weekly_end_time");
            let mut weekly = item("weekly", window_label("weekly"), "quota_window");
            amount_fields(&mut weekly, used, remaining, total, "units", None);
            set_percent(&mut weekly, percent);
            weekly.window_seconds = minimax_window_seconds(
                field_timestamp(model, "weekly_start_time"),
                reset,
                field_number(model, "weekly_remains_time"),
            );
            weekly.resets_at_unix_ms = reset;
            allowances.push(weekly);
        }
    }
    Ok(response(allowances))
}

fn pick_minimax_model(models: &[Value]) -> Option<&Map<String, Value>> {
    let objects = models
        .iter()
        .filter_map(Value::as_object)
        .collect::<Vec<_>>();
    objects
        .iter()
        .copied()
        .find(|model| {
            model
                .get("model_name")
                .and_then(non_empty)
                .is_some_and(|n| n.to_ascii_lowercase().starts_with("minimax-m"))
                && field_number(model, "current_interval_total_count").is_some_and(|v| v > 0.0)
        })
        .or_else(|| {
            objects.iter().copied().find(|model| {
                model
                    .get("model_name")
                    .and_then(non_empty)
                    .is_some_and(|n| {
                        matches!(n.to_ascii_lowercase().as_str(), "general" | "chat" | "text")
                    })
            })
        })
        .or_else(|| {
            objects
                .iter()
                .copied()
                .find(|model| field_number(model, "current_interval_remaining_percent").is_some())
        })
        .or_else(|| objects.first().copied())
}

fn minimax_window_seconds(
    start: Option<i64>,
    reset: Option<i64>,
    remains: Option<f64>,
) -> Option<u64> {
    if let Some((start, reset)) = start.zip(reset)
        && reset > start
    {
        return u64::try_from((reset - start) / 1000).ok();
    }
    remains
        .filter(|v| *v > 0.0 && *v <= u64::MAX as f64 * 1000.0)
        .map(|v| (v / 1000.0).floor() as u64)
}

fn parse_wafer(payload: &Value) -> Result<AllowanceResponse, ()> {
    let object = payload.as_object().ok_or(())?;
    let remaining = field_number(object, "remaining_included_requests");
    let limit = field_number(object, "included_request_limit");
    let overage = field_number(object, "overage_request_count");
    let reported = field_number(object, "current_period_used_percent");
    if remaining.is_none() && limit.is_none() && overage.is_none() && reported.is_none() {
        return Err(());
    }
    let used = limit
        .zip(remaining)
        .map(|(l, r)| l - r + overage.unwrap_or(0.0).max(0.0));
    let start = field_timestamp(object, "window_start");
    let reset = field_timestamp(object, "window_end");
    let seconds = start
        .zip(reset)
        .filter(|(s, e)| e > s)
        .and_then(|(s, e)| u64::try_from((e - s) / 1000).ok())
        .or(Some(18_000));
    let key = window_key(seconds);
    let mut allowance = item(&key, window_label(&key), "request_allowance");
    amount_fields(&mut allowance, used, remaining, limit, "requests", None);
    set_percent(
        &mut allowance,
        reported.or_else(|| percent_from(used, remaining, limit)),
    );
    allowance.window_seconds = seconds;
    allowance.resets_at_unix_ms = reset;
    Ok(AllowanceResponse {
        allowances: vec![allowance],
        models: Vec::new(),
        plan_label: object
            .get("plan_tier")
            .and_then(non_empty)
            .map(str::to_owned),
    })
}

fn parse_opencode_go(payload: &Value) -> Result<AllowanceResponse, ()> {
    let usage = payload.get("usage").and_then(Value::as_object).ok_or(())?;
    let mut allowances = Vec::new();
    for (field, key) in [
        ("rolling", "5h"),
        ("weekly", "weekly"),
        ("monthly", "monthly"),
    ] {
        let Some(window) = usage.get(field).and_then(Value::as_object) else {
            continue;
        };
        let Some(percent) = field_number(window, "percent") else {
            continue;
        };
        let Some(reset) = field_timestamp(window, "resetsAt") else {
            continue;
        };
        let mut allowance = item(key, window_label(key), "quota_window");
        set_percent(&mut allowance, Some(percent));
        allowance.resets_at_unix_ms = Some(reset);
        allowances.push(allowance);
    }
    Ok(response(allowances))
}

fn parse_crof(payload: &Value) -> Result<AllowanceResponse, ()> {
    let credits = payload.get("credits").and_then(number).ok_or(())?;
    let mut allowance = item("credits", "Credits", "balance");
    allowance.remaining = Some(amount(credits, "currency", Some("USD")));
    Ok(response(vec![allowance]))
}

fn parse_deepseek(payload: &Value) -> Result<AllowanceResponse, ()> {
    let balances = payload
        .get("balance_infos")
        .and_then(Value::as_array)
        .ok_or(())?;
    let entries = balances
        .iter()
        .filter_map(|value| {
            let object = value.as_object()?;
            Some((
                field_number(object, "total_balance")?,
                object.get("currency").and_then(non_empty),
            ))
        })
        .collect::<Vec<_>>();
    if entries.is_empty() {
        return Err(());
    }
    let mut allowances = entries
        .iter()
        .filter(|(balance, _)| *balance > 0.0)
        .map(|(balance, currency)| deepseek_balance_item(*balance, *currency))
        .collect::<Vec<_>>();
    if allowances.is_empty() {
        let (balance, currency) = entries
            .iter()
            .find(|(_, c)| *c == Some("USD"))
            .unwrap_or(&entries[0]);
        allowances.push(deepseek_balance_item(*balance, *currency));
    }
    Ok(response(allowances))
}

fn deepseek_balance_item(balance: f64, currency: Option<&str>) -> AllowanceItem {
    let key = currency
        .map(|c| slug_key(&format!("credits_balance_{c}")))
        .unwrap_or_else(|| "credits_balance".into());
    let mut allowance = item(key, window_label("credits_balance"), "balance");
    allowance.remaining = Some(amount(balance, "currency", currency));
    allowance
}

fn parse_neuralwatt(payload: &Value) -> Result<AllowanceResponse, ()> {
    let object = payload.as_object().ok_or(())?;
    let mut allowances = Vec::new();
    let credits = payload
        .pointer("/balance/credits_remaining_usd")
        .and_then(number);
    if let Some(subscription) = object.get("subscription").and_then(Value::as_object) {
        let included = field_number(subscription, "kwh_included");
        let used = field_number(subscription, "kwh_used");
        let remaining = included.zip(used).map(|(i, u)| i - u);
        let plan = subscription
            .get("plan")
            .and_then(non_empty)
            .unwrap_or("plan_limit");
        let mut allowance = item(slug_key(plan), plan, "quota_window");
        amount_fields(&mut allowance, used, remaining, included, "kWh", None);
        set_percent(
            &mut allowance,
            percent_from(used, remaining, included).or_else(|| {
                (subscription.get("in_overage").and_then(Value::as_bool) == Some(true))
                    .then_some(100.0)
            }),
        );
        allowance.resets_at_unix_ms = field_timestamp(subscription, "kwh_reset_date")
            .or_else(|| field_timestamp(subscription, "current_period_end"));
        if allowance.used.is_some()
            || allowance.used_percent.is_some()
            || allowance.resets_at_unix_ms.is_some()
        {
            allowances.push(allowance);
        }
    }
    if let Some(key_allowance) = payload.pointer("/key/allowance").and_then(Value::as_object) {
        let spent = field_number(key_allowance, "spent_usd");
        let configured = field_number(key_allowance, "limit_usd");
        let limit = match (configured, credits, spent) {
            (Some(l), Some(c), Some(s)) => Some(l.min(c + s)),
            (Some(l), _, _) => Some(l),
            (None, Some(c), _) => Some(c + spent.unwrap_or(0.0)),
            _ => None,
        };
        let remaining = spent.zip(limit).map(|(s, l)| l - s);
        let period = key_allowance
            .get("period")
            .and_then(non_empty)
            .unwrap_or("billing_cycle");
        let key = match period {
            "month" => "monthly",
            "daily" | "weekly" | "monthly" => period,
            _ => "billing_cycle",
        };
        let label = payload
            .pointer("/key/name")
            .and_then(non_empty)
            .map(str::to_owned)
            .unwrap_or_else(|| window_label(key));
        let mut allowance = item(key, label, "balance");
        amount_fields(
            &mut allowance,
            spent,
            remaining,
            limit,
            "currency",
            Some("USD"),
        );
        set_percent(
            &mut allowance,
            if key_allowance.get("blocked").and_then(Value::as_bool) == Some(true) {
                Some(100.0)
            } else {
                percent_from(spent, remaining, limit)
            },
        );
        allowance.window_seconds = period_window_seconds(period);
        allowance.resets_at_unix_ms = field_timestamp(key_allowance, "reset_at");
        if allowance.used.is_some()
            || allowance.used_percent.is_some()
            || allowance.resets_at_unix_ms.is_some()
        {
            allowances.push(allowance);
        }
    } else if let Some(balance) = credits {
        let mut allowance = item(
            "credits_balance",
            window_label("credits_balance"),
            "balance",
        );
        allowance.remaining = Some(amount(balance, "currency", Some("USD")));
        allowances.push(allowance);
    }
    Ok(response(allowances))
}

fn slug_key(value: &str) -> String {
    let mut key = String::with_capacity(value.len());
    let mut separator = false;
    for ch in value.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            key.push(ch);
            separator = false
        } else if !separator && !key.is_empty() {
            key.push('_');
            separator = true
        }
    }
    while key.ends_with('_') {
        key.pop();
    }
    if key.is_empty() {
        "plan_limit".into()
    } else {
        key
    }
}
fn period_window_seconds(period: &str) -> Option<u64> {
    match period {
        "daily" => Some(86_400),
        "weekly" => Some(604_800),
        "monthly" | "month" => Some(30 * 86_400),
        "yearly" | "year" => Some(365 * 86_400),
        _ => None,
    }
}
