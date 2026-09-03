use std::collections::HashMap;

use serde_json::{Map, Value};

use super::{Allowance, AllowanceAmount, AllowanceKind, ModelAllowance, MonitorKind};

#[derive(Debug)]
pub(super) struct ParsedAllowance {
    pub allowances: Vec<Allowance>,
    pub models: Vec<ModelAllowance>,
    pub plan_label: Option<String>,
}

#[derive(Debug)]
pub(super) struct InvalidResponse;

pub(super) fn parse_monitor_response(
    monitor: MonitorKind,
    body: &[u8],
) -> Result<ParsedAllowance, InvalidResponse> {
    if monitor == MonitorKind::XaiGrok {
        return parse_xai(body);
    }

    let payload: Value = serde_json::from_slice(body).map_err(|_| InvalidResponse)?;
    let parsed = match monitor {
        MonitorKind::AnthropicClaudeCode => parse_anthropic(&payload),
        MonitorKind::OpenAiCodex => parse_codex(&payload),
        MonitorKind::GitHubCopilot => parse_github_copilot(&payload),
        MonitorKind::KimiForCoding => parse_kimi(&payload),
        MonitorKind::NanoGpt => parse_nano_gpt(&payload),
        MonitorKind::ZaiCodingPlan => parse_zai(&payload),
        MonitorKind::ZhipuAiCodingPlan => parse_zhipu(&payload),
        MonitorKind::MiniMaxCodingPlan | MonitorKind::MiniMaxCnCodingPlan => {
            parse_minimax(&payload, true)
        }
        MonitorKind::Wafer => parse_wafer(&payload),
        MonitorKind::OpenCodeGo => parse_opencode_go(&payload),
        MonitorKind::Crof => parse_crof(&payload),
        MonitorKind::DeepSeek => parse_deepseek(&payload),
        MonitorKind::NeuralWatt => parse_neuralwatt(&payload),
        MonitorKind::XaiGrok => unreachable!("handled above"),
    }?;
    require_allowance(parsed)
}

pub(super) fn parse_minimax_fallback(body: &[u8]) -> Result<ParsedAllowance, InvalidResponse> {
    let payload: Value = serde_json::from_slice(body).map_err(|_| InvalidResponse)?;
    require_allowance(parse_minimax(&payload, false)?)
}

fn require_allowance(parsed: ParsedAllowance) -> Result<ParsedAllowance, InvalidResponse> {
    if parsed.allowances.is_empty()
        && parsed
            .models
            .iter()
            .all(|model| model.allowances.is_empty())
    {
        Err(InvalidResponse)
    } else {
        Ok(parsed)
    }
}

fn parsed(allowances: Vec<Allowance>) -> ParsedAllowance {
    ParsedAllowance {
        allowances,
        models: Vec::new(),
        plan_label: None,
    }
}

fn allowance(key: impl Into<String>, label: impl Into<String>, kind: AllowanceKind) -> Allowance {
    Allowance {
        key: key.into(),
        label: label.into(),
        kind,
        used: None,
        remaining: None,
        limit: None,
        used_percent: None,
        window_seconds: None,
        reset_at: None,
        condition: None,
        forecast: Default::default(),
    }
}

fn amount(value: f64, unit: &str, currency: Option<&str>) -> AllowanceAmount {
    AllowanceAmount {
        value,
        unit: unit.to_string(),
        currency: currency.map(str::to_string),
    }
}

fn amount_fields(
    allowance: &mut Allowance,
    used: Option<f64>,
    remaining: Option<f64>,
    limit: Option<f64>,
    unit: &str,
    currency: Option<&str>,
) {
    allowance.used = used.map(|value| amount(value, unit, currency));
    allowance.remaining = remaining.map(|value| amount(value, unit, currency));
    allowance.limit = limit.map(|value| amount(value, unit, currency));
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse::<f64>().ok())
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

fn percent_from(used: Option<f64>, remaining: Option<f64>, limit: Option<f64>) -> Option<f64> {
    limit.filter(|limit| *limit > 0.0).and_then(|limit| {
        used.map(|used| used / limit * 100.0)
            .or_else(|| remaining.map(|remaining| (1.0 - remaining / limit) * 100.0))
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
        "daily" => "Daily window".into(),
        "monthly" => "Monthly window".into(),
        "billing_cycle" => "Billing cycle".into(),
        "credits_balance" => "Credit balance".into(),
        "premium_interactions" => "Premium interactions".into(),
        "mcp_tools" => "MCP tools".into(),
        "extra_usage" => "Extra usage".into(),
        "tokens" => "Tokens".into(),
        other => other.to_string(),
    }
}

fn parse_anthropic(payload: &Value) -> Result<ParsedAllowance, InvalidResponse> {
    let object = payload.as_object().ok_or(InvalidResponse)?;
    let mut allowances = Vec::new();
    let mut models_by_name: HashMap<String, Vec<Allowance>> = HashMap::new();
    let limits = object.get("limits").and_then(Value::as_array);

    if let Some(limits) = limits.filter(|limits| !limits.is_empty()) {
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
            let mut item = allowance(
                key.unwrap_or("7d"),
                window_label(key.unwrap_or("7d")),
                AllowanceKind::QuotaWindow,
            );
            item.used_percent = field_number(limit, "percent");
            item.reset_at = field_timestamp(limit, "resets_at");
            item.window_seconds = Some(if key == Some("5h") { 18_000 } else { 604_800 });

            if limit.get("kind").and_then(non_empty) == Some("weekly_scoped") {
                let model_name = value
                    .pointer("/scope/model/display_name")
                    .and_then(non_empty)
                    .ok_or(InvalidResponse)?;
                models_by_name
                    .entry(model_name.to_string())
                    .or_default()
                    .push(item);
            } else {
                allowances.push(item);
            }
        }
    } else {
        for (field, key, seconds) in [("five_hour", "5h", 18_000), ("seven_day", "7d", 604_800)] {
            let Some(limit) = object.get(field).and_then(Value::as_object) else {
                continue;
            };
            let mut item = allowance(key, window_label(key), AllowanceKind::QuotaWindow);
            item.used_percent = field_number(limit, "utilization");
            item.reset_at = field_timestamp(limit, "resets_at");
            item.window_seconds = Some(seconds);
            allowances.push(item);
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
            .and_then(|value| value.get("currency"))
            .and_then(non_empty)
            .or_else(|| {
                spend
                    .get("limit")
                    .and_then(|value| value.get("currency"))
                    .and_then(non_empty)
            });
        let mut item = allowance(
            "extra_usage",
            window_label("extra_usage"),
            AllowanceKind::Balance,
        );
        amount_fields(&mut item, used, remaining, limit, "currency", currency);
        item.used_percent =
            field_number(spend, "percent").or_else(|| percent_from(used, remaining, limit));
        allowances.push(item);
    }

    let mut models = models_by_name
        .into_iter()
        .map(|(model, allowances)| ModelAllowance { model, allowances })
        .collect::<Vec<_>>();
    models.sort_by(|left, right| left.model.cmp(&right.model));
    Ok(ParsedAllowance {
        allowances,
        models,
        plan_label: None,
    })
}

fn money_amount(value: &Value) -> Option<f64> {
    let object = value.as_object()?;
    let minor = field_number(object, "amount_minor")?;
    let exponent = field_number(object, "exponent").unwrap_or(2.0);
    Some(minor / 10_f64.powf(exponent))
}

fn parse_codex(payload: &Value) -> Result<ParsedAllowance, InvalidResponse> {
    let object = payload.as_object().ok_or(InvalidResponse)?;
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
            let mut item = allowance(&key, window_label(&key), AllowanceKind::QuotaWindow);
            item.used_percent = field_number(window, "used_percent");
            item.window_seconds = seconds;
            item.reset_at = field_timestamp(window, "reset_at");
            if item.used_percent.is_some()
                || item.reset_at.is_some()
                || item.window_seconds.is_some()
            {
                allowances.push(item);
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
            let mut item = allowance(key, window_label(key), AllowanceKind::Balance);
            item.remaining = balance.map(|value| amount(value, "currency", Some("USD")));
            allowances.push(item);
        }
    }

    if let Some(spend) = payload
        .pointer("/spend_control/individual_limit")
        .and_then(Value::as_object)
    {
        let used = field_number(spend, "used");
        let limit = field_number(spend, "limit");
        let remaining = used.zip(limit).map(|(used, limit)| limit - used);
        let mut item = allowance("credits", "Credit limit", AllowanceKind::Balance);
        amount_fields(&mut item, used, remaining, limit, "currency", Some("USD"));
        item.used_percent =
            field_number(spend, "used_percent").or_else(|| percent_from(used, remaining, limit));
        if item.used.is_some() || item.limit.is_some() || item.used_percent.is_some() {
            allowances.push(item);
        }
    }

    Ok(parsed(allowances))
}

fn parse_github_copilot(payload: &Value) -> Result<ParsedAllowance, InvalidResponse> {
    let snapshot = payload
        .pointer("/quota_snapshots/premium_interactions")
        .and_then(Value::as_object)
        .ok_or(InvalidResponse)?;
    let reset_at = payload.get("quota_reset_date").and_then(timestamp_millis);
    let mut item = allowance(
        "premium_interactions",
        window_label("premium_interactions"),
        AllowanceKind::RequestAllowance,
    );

    if snapshot.get("unlimited").and_then(Value::as_bool) != Some(true) {
        let limit = field_number(snapshot, "entitlement");
        let remaining = field_number(snapshot, "remaining");
        let used = limit
            .zip(remaining)
            .map(|(limit, remaining)| limit - remaining);
        amount_fields(&mut item, used, remaining, limit, "requests", None);
        item.used_percent = percent_from(used, remaining, limit).or_else(|| {
            field_number(snapshot, "percent_remaining").map(|remaining| 100.0 - remaining)
        });
    }
    item.reset_at = reset_at;
    Ok(parsed(vec![item]))
}

fn parse_kimi(payload: &Value) -> Result<ParsedAllowance, InvalidResponse> {
    let object = payload.as_object().ok_or(InvalidResponse)?;
    let mut allowances = Vec::new();
    if let Some(usage) = object.get("usage").and_then(Value::as_object) {
        let limit = field_number(usage, "limit");
        let used = field_number(usage, "used");
        let remaining = field_number(usage, "remaining")
            .or_else(|| used.zip(limit).map(|(used, limit)| limit - used));
        let mut item = allowance("weekly", window_label("weekly"), AllowanceKind::QuotaWindow);
        amount_fields(&mut item, used, remaining, limit, "units", None);
        item.used_percent = percent_from(used, remaining, limit);
        item.reset_at = field_timestamp(usage, "resetTime");
        if item.used.is_some() || item.remaining.is_some() || item.reset_at.is_some() {
            allowances.push(item);
        }
    }

    if let Some(limits) = object.get("limits").and_then(Value::as_array) {
        for entry in limits {
            let Some(window) = entry.get("window").and_then(Value::as_object) else {
                continue;
            };
            let Some(detail) = entry.get("detail").and_then(Value::as_object) else {
                continue;
            };
            let seconds = duration_seconds(
                field_number(window, "duration"),
                window.get("timeUnit").and_then(non_empty),
            );
            let key = window_key(seconds);
            let limit = field_number(detail, "limit");
            let used = field_number(detail, "used");
            let remaining = field_number(detail, "remaining")
                .or_else(|| used.zip(limit).map(|(used, limit)| limit - used));
            let mut item = allowance(&key, window_label(&key), AllowanceKind::QuotaWindow);
            amount_fields(&mut item, used, remaining, limit, "units", None);
            item.used_percent = percent_from(used, remaining, limit);
            item.window_seconds = seconds;
            item.reset_at = field_timestamp(detail, "resetTime");
            if item.used.is_some() || item.remaining.is_some() || item.reset_at.is_some() {
                allowances.push(item);
            }
        }
    }
    Ok(parsed(allowances))
}

fn duration_seconds(duration: Option<f64>, unit: Option<&str>) -> Option<u64> {
    let duration = duration?;
    let multiplier = match unit?.to_ascii_lowercase().as_str() {
        "second" | "seconds" => 1.0,
        "minute" | "minutes" => 60.0,
        "hour" | "hours" => 3_600.0,
        "day" | "days" => 86_400.0,
        "week" | "weeks" => 604_800.0,
        _ => return None,
    };
    let seconds = duration * multiplier;
    (seconds.is_finite() && seconds >= 0.0 && seconds <= u64::MAX as f64)
        .then_some(seconds.round() as u64)
}

fn parse_nano_gpt(payload: &Value) -> Result<ParsedAllowance, InvalidResponse> {
    let object = payload.as_object().ok_or(InvalidResponse)?;
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
                .and_then(Value::as_object)
                .and_then(|limits| limits.get(field))
                .and_then(number)
        });
        let remaining = used.zip(limit).map(|(used, limit)| limit - used);
        let mut item = allowance(key, window_label(key), AllowanceKind::QuotaWindow);
        amount_fields(&mut item, used, remaining, limit, "units", None);
        item.used_percent = field_number(window, "percentUsed")
            .map(|value| value * 100.0)
            .or_else(|| percent_from(used, remaining, limit));
        item.window_seconds = seconds;
        item.reset_at = field_timestamp(window, "resetAt").or_else(|| {
            (field == "monthly")
                .then(|| {
                    payload
                        .pointer("/period/currentPeriodEnd")
                        .and_then(timestamp_millis)
                })
                .flatten()
        });
        if item.used_percent.is_some() || item.used.is_some() || item.reset_at.is_some() {
            allowances.push(item);
        }
    }
    Ok(parsed(allowances))
}

fn parse_zai(payload: &Value) -> Result<ParsedAllowance, InvalidResponse> {
    let data = payload
        .get("data")
        .and_then(Value::as_object)
        .ok_or(InvalidResponse)?;
    let limits = data
        .get("limits")
        .and_then(Value::as_array)
        .ok_or(InvalidResponse)?;
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
                    .or_else(|| used.zip(total).map(|(used, total)| total - used));
                let unit = if limit.get("type").and_then(non_empty) == Some("CREDIT_LIMIT") {
                    "credits"
                } else {
                    "units"
                };
                let mut item = allowance(&key, window_label(&key), AllowanceKind::QuotaWindow);
                amount_fields(&mut item, used, remaining, total, unit, None);
                item.used_percent = field_number(limit, "percentage")
                    .or_else(|| percent_from(used, remaining, total));
                item.window_seconds = seconds;
                item.reset_at = field_timestamp(limit, "nextResetTime");
                allowances.push(item);
            }
            Some("TIME_LIMIT") => {
                let mut item = allowance(
                    "mcp_tools",
                    window_label("mcp_tools"),
                    AllowanceKind::RequestAllowance,
                );
                item.used_percent = field_number(limit, "percentage");
                item.window_seconds = Some(30 * 86_400);
                item.reset_at = field_timestamp(limit, "nextResetTime");
                allowances.push(item);
            }
            _ => {}
        }
    }
    Ok(ParsedAllowance {
        allowances,
        models: Vec::new(),
        plan_label: data.get("level").and_then(non_empty).map(str::to_string),
    })
}

fn zai_window_seconds(limit: &Map<String, Value>) -> Option<u64> {
    let number = field_number(limit, "number")?;
    let base = match field_number(limit, "unit")? as i64 {
        3 => 3_600.0,
        6 => 604_800.0,
        _ => return None,
    };
    let seconds = number * base;
    (seconds.is_finite() && seconds > 0.0 && seconds <= u64::MAX as f64)
        .then_some(seconds.round() as u64)
}

fn parse_zhipu(payload: &Value) -> Result<ParsedAllowance, InvalidResponse> {
    let limits = payload
        .pointer("/data/limits")
        .and_then(Value::as_array)
        .ok_or(InvalidResponse)?;
    let mut allowances = Vec::new();
    for value in limits {
        let Some(limit) = value.as_object() else {
            continue;
        };
        match limit.get("type").and_then(non_empty) {
            Some("TOKENS_LIMIT") => {
                let seconds = zai_window_seconds(limit);
                let key = window_key(seconds);
                let mut item = allowance(&key, window_label(&key), AllowanceKind::QuotaWindow);
                item.used_percent = field_number(limit, "percentage");
                item.window_seconds = seconds;
                item.reset_at = field_timestamp(limit, "nextResetTime");
                allowances.push(item);
            }
            Some("TIME_LIMIT") => {
                let mut item = allowance(
                    "mcp_tools",
                    window_label("mcp_tools"),
                    AllowanceKind::RequestAllowance,
                );
                item.used_percent = field_number(limit, "percentage");
                item.window_seconds = Some(30 * 86_400);
                item.reset_at = field_timestamp(limit, "nextResetTime");
                allowances.push(item);
            }
            _ => {}
        }
    }
    Ok(parsed(allowances))
}

fn parse_minimax(payload: &Value, token_plan: bool) -> Result<ParsedAllowance, InvalidResponse> {
    let object = payload.as_object().ok_or(InvalidResponse)?;
    if let Some(base) = object.get("base_resp").and_then(Value::as_object)
        && field_number(base, "status_code") != Some(0.0)
    {
        return Err(InvalidResponse);
    }
    let models = object
        .get("model_remains")
        .and_then(Value::as_array)
        .ok_or(InvalidResponse)?;
    let model = pick_minimax_model(models).ok_or(InvalidResponse)?;
    let mut allowances = Vec::new();

    let interval_total = field_number(model, "current_interval_total_count");
    let interval_raw = field_number(model, "current_interval_usage_count");
    let interval_remaining = if token_plan {
        interval_raw
    } else {
        interval_total
            .zip(interval_raw)
            .map(|(total, used)| total - used)
    };
    let interval_used = interval_total
        .zip(interval_remaining)
        .map(|(total, remaining)| total - remaining);
    let interval_percent = field_number(model, "current_interval_remaining_percent")
        .map(|remaining| 100.0 - remaining)
        .or_else(|| percent_from(interval_used, interval_remaining, interval_total));
    let interval_reset = field_timestamp(model, "end_time");
    let interval_seconds = minimax_window_seconds(
        field_timestamp(model, "start_time"),
        interval_reset,
        field_number(model, "remains_time"),
    );
    let mut interval = allowance("5h", window_label("5h"), AllowanceKind::QuotaWindow);
    amount_fields(
        &mut interval,
        interval_used,
        interval_remaining,
        interval_total,
        "units",
        None,
    );
    interval.used_percent = interval_percent;
    interval.window_seconds = interval_seconds;
    interval.reset_at = interval_reset;
    if interval.used_percent.is_some() || interval.used.is_some() || interval.reset_at.is_some() {
        allowances.push(interval);
    }

    let weekly_status = field_number(model, "current_weekly_status");
    if weekly_status != Some(3.0) {
        let total = field_number(model, "current_weekly_total_count");
        let raw = field_number(model, "current_weekly_usage_count");
        let remaining = if token_plan {
            raw
        } else {
            total.zip(raw).map(|(total, used)| total - used)
        };
        let used = total
            .zip(remaining)
            .map(|(total, remaining)| total - remaining);
        let used_percent = field_number(model, "current_weekly_remaining_percent")
            .map(|remaining| 100.0 - remaining)
            .or_else(|| percent_from(used, remaining, total));
        if used_percent.is_some() || total.is_some() {
            let reset_at = field_timestamp(model, "weekly_end_time");
            let mut weekly =
                allowance("weekly", window_label("weekly"), AllowanceKind::QuotaWindow);
            amount_fields(&mut weekly, used, remaining, total, "units", None);
            weekly.used_percent = used_percent;
            weekly.window_seconds = minimax_window_seconds(
                field_timestamp(model, "weekly_start_time"),
                reset_at,
                field_number(model, "weekly_remains_time"),
            );
            weekly.reset_at = reset_at;
            allowances.push(weekly);
        }
    }
    Ok(parsed(allowances))
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
                .is_some_and(|name| name.to_ascii_lowercase().starts_with("minimax-m"))
                && field_number(model, "current_interval_total_count")
                    .is_some_and(|value| value > 0.0)
        })
        .or_else(|| {
            objects.iter().copied().find(|model| {
                model
                    .get("model_name")
                    .and_then(non_empty)
                    .is_some_and(|name| {
                        matches!(
                            name.to_ascii_lowercase().as_str(),
                            "general" | "chat" | "text"
                        )
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
    start_at: Option<i64>,
    reset_at: Option<i64>,
    remains_time_ms: Option<f64>,
) -> Option<u64> {
    if let Some((start, reset)) = start_at.zip(reset_at)
        && reset > start
    {
        return u64::try_from((reset - start) / 1000).ok();
    }
    remains_time_ms
        .filter(|value| *value > 0.0 && *value <= u64::MAX as f64 * 1000.0)
        .map(|value| (value / 1000.0).floor() as u64)
}

fn parse_wafer(payload: &Value) -> Result<ParsedAllowance, InvalidResponse> {
    let object = payload.as_object().ok_or(InvalidResponse)?;
    let remaining = field_number(object, "remaining_included_requests");
    let limit = field_number(object, "included_request_limit");
    let overage = field_number(object, "overage_request_count");
    let used_percent = field_number(object, "current_period_used_percent");
    if remaining.is_none() && limit.is_none() && overage.is_none() && used_percent.is_none() {
        return Err(InvalidResponse);
    }
    let used = limit
        .zip(remaining)
        .map(|(limit, remaining)| limit - remaining + overage.unwrap_or(0.0).max(0.0));
    let start = field_timestamp(object, "window_start");
    let reset_at = field_timestamp(object, "window_end");
    let seconds = start
        .zip(reset_at)
        .filter(|(start, end)| end > start)
        .and_then(|(start, end)| u64::try_from((end - start) / 1000).ok())
        .or(Some(18_000));
    let key = window_key(seconds);
    let mut item = allowance(&key, window_label(&key), AllowanceKind::RequestAllowance);
    amount_fields(&mut item, used, remaining, limit, "requests", None);
    item.used_percent = used_percent.or_else(|| percent_from(used, remaining, limit));
    item.window_seconds = seconds;
    item.reset_at = reset_at;
    Ok(ParsedAllowance {
        allowances: vec![item],
        models: Vec::new(),
        plan_label: object
            .get("plan_tier")
            .and_then(non_empty)
            .map(str::to_string),
    })
}

fn parse_opencode_go(payload: &Value) -> Result<ParsedAllowance, InvalidResponse> {
    let usage = payload
        .get("usage")
        .and_then(Value::as_object)
        .ok_or(InvalidResponse)?;
    let mut allowances = Vec::new();
    for (field, key) in [
        ("rolling", "5h"),
        ("weekly", "weekly"),
        ("monthly", "monthly"),
    ] {
        let Some(window) = usage.get(field).and_then(Value::as_object) else {
            continue;
        };
        let Some(used_percent) = field_number(window, "percent") else {
            continue;
        };
        let Some(reset_at) = field_timestamp(window, "resetsAt") else {
            continue;
        };
        let mut item = allowance(key, window_label(key), AllowanceKind::QuotaWindow);
        item.used_percent = Some(used_percent);
        item.reset_at = Some(reset_at);
        allowances.push(item);
    }
    Ok(parsed(allowances))
}

fn parse_crof(payload: &Value) -> Result<ParsedAllowance, InvalidResponse> {
    let credits = payload
        .get("credits")
        .and_then(number)
        .ok_or(InvalidResponse)?;
    let mut item = allowance("credits", "Credits", AllowanceKind::Balance);
    item.remaining = Some(amount(credits, "currency", Some("USD")));
    Ok(parsed(vec![item]))
}

fn parse_deepseek(payload: &Value) -> Result<ParsedAllowance, InvalidResponse> {
    let balances = payload
        .get("balance_infos")
        .and_then(Value::as_array)
        .ok_or(InvalidResponse)?;
    let selected = ["USD", "CNY"]
        .into_iter()
        .find_map(|currency| {
            balances.iter().find_map(|value| {
                let object = value.as_object()?;
                (object.get("currency").and_then(non_empty) == Some(currency)).then_some(object)
            })
        })
        .or_else(|| {
            balances.iter().find_map(|value| {
                let object = value.as_object()?;
                field_number(object, "total_balance")
                    .is_some()
                    .then_some(object)
            })
        })
        .ok_or(InvalidResponse)?;
    let balance = field_number(selected, "total_balance").ok_or(InvalidResponse)?;
    let currency = selected.get("currency").and_then(non_empty);
    let mut item = allowance(
        "credits_balance",
        window_label("credits_balance"),
        AllowanceKind::Balance,
    );
    item.remaining = Some(amount(balance, "currency", currency));
    Ok(parsed(vec![item]))
}

fn parse_neuralwatt(payload: &Value) -> Result<ParsedAllowance, InvalidResponse> {
    let object = payload.as_object().ok_or(InvalidResponse)?;
    let mut allowances = Vec::new();
    let credits_remaining = payload
        .pointer("/balance/credits_remaining_usd")
        .and_then(number);

    if let Some(subscription) = object.get("subscription").and_then(Value::as_object) {
        let included = field_number(subscription, "kwh_included");
        let used = field_number(subscription, "kwh_used");
        let remaining = included.zip(used).map(|(included, used)| included - used);
        let plan = subscription
            .get("plan")
            .and_then(non_empty)
            .unwrap_or("plan_limit");
        let key = slug_key(plan);
        let mut item = allowance(&key, plan, AllowanceKind::QuotaWindow);
        amount_fields(&mut item, used, remaining, included, "kWh", None);
        item.used_percent = percent_from(used, remaining, included).or_else(|| {
            (subscription.get("in_overage").and_then(Value::as_bool) == Some(true)).then_some(100.0)
        });
        item.reset_at = field_timestamp(subscription, "kwh_reset_date")
            .or_else(|| field_timestamp(subscription, "current_period_end"));
        if item.used.is_some() || item.used_percent.is_some() || item.reset_at.is_some() {
            allowances.push(item);
        }
    }

    if let Some(key_allowance) = payload.pointer("/key/allowance").and_then(Value::as_object) {
        let spent = field_number(key_allowance, "spent_usd");
        let configured_limit = field_number(key_allowance, "limit_usd");
        let effective_limit = match (configured_limit, credits_remaining, spent) {
            (Some(limit), Some(credits), Some(spent)) => Some(limit.min(credits + spent)),
            (Some(limit), _, _) => Some(limit),
            (None, Some(credits), _) => Some(credits + spent.unwrap_or(0.0)),
            _ => None,
        };
        let remaining = spent
            .zip(effective_limit)
            .map(|(spent, limit)| limit - spent);
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
            .map(str::to_string)
            .unwrap_or_else(|| window_label(key));
        let mut item = allowance(key, label, AllowanceKind::Balance);
        amount_fields(
            &mut item,
            spent,
            remaining,
            effective_limit,
            "currency",
            Some("USD"),
        );
        item.used_percent = if key_allowance.get("blocked").and_then(Value::as_bool) == Some(true) {
            Some(100.0)
        } else {
            percent_from(spent, remaining, effective_limit)
        };
        item.window_seconds = period_window_seconds(period);
        item.reset_at = field_timestamp(key_allowance, "reset_at");
        if item.used.is_some() || item.used_percent.is_some() || item.reset_at.is_some() {
            allowances.push(item);
        }
    } else if let Some(balance) = credits_remaining {
        let mut item = allowance(
            "credits_balance",
            window_label("credits_balance"),
            AllowanceKind::Balance,
        );
        item.remaining = Some(amount(balance, "currency", Some("USD")));
        allowances.push(item);
    }

    Ok(parsed(allowances))
}

fn slug_key(value: &str) -> String {
    let mut key = String::with_capacity(value.len());
    let mut previous_separator = false;
    for character in value.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            key.push(character);
            previous_separator = false;
        } else if !previous_separator && !key.is_empty() {
            key.push('_');
            previous_separator = true;
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

#[derive(Debug)]
struct Fixed32Field {
    path: Vec<u32>,
    value: f32,
    order: usize,
}

#[derive(Debug)]
struct VarintField {
    path: Vec<u32>,
    value: u64,
}

#[derive(Default)]
struct ProtobufScan {
    fixed32: Vec<Fixed32Field>,
    varints: Vec<VarintField>,
    order: usize,
}

fn parse_xai(body: &[u8]) -> Result<ParsedAllowance, InvalidResponse> {
    let payloads = grpc_web_payloads(body)?;
    let mut scan = ProtobufScan::default();
    for payload in payloads {
        scan_protobuf(payload, &[], 0, &mut scan)?;
    }
    let mut percentages = scan
        .fixed32
        .iter()
        .filter(|field| {
            matches!(field.path.as_slice(), [1] | [1, 1])
                && field.value.is_finite()
                && (0.0..=100.0).contains(&field.value)
        })
        .collect::<Vec<_>>();
    percentages.sort_by_key(|field| (field.path.len(), field.order));
    let used_percent = percentages.first().map(|field| f64::from(field.value));

    let now = chrono::Utc::now().timestamp_millis();
    let mut resets = scan
        .varints
        .iter()
        .filter(|field| (1_700_000_000..=2_100_000_000).contains(&field.value))
        .map(|field| {
            (
                field.path.as_slice() == [1, 5, 1],
                field.value as i64 * 1000,
            )
        })
        .filter(|(_, reset)| *reset > now)
        .collect::<Vec<_>>();
    resets.sort_by_key(|(preferred, reset)| (!*preferred, *reset));
    let reset_at = resets.first().map(|(_, reset)| *reset);
    let has_usage_period = scan.varints.iter().any(|field| {
        (field.path.len() >= 2 && field.path[0] == 1 && field.path[1] == 6)
            || (field.path.as_slice() == [1, 8, 1] && matches!(field.value, 1 | 2))
    });
    let used_percent = used_percent.or_else(|| {
        (scan.fixed32.is_empty() && reset_at.is_some() && has_usage_period).then_some(0.0)
    });
    let mut item = allowance(
        "billing_cycle",
        window_label("billing_cycle"),
        AllowanceKind::QuotaWindow,
    );
    item.used_percent = Some(used_percent.ok_or(InvalidResponse)?);
    item.reset_at = reset_at;
    Ok(parsed(vec![item]))
}

fn grpc_web_payloads(body: &[u8]) -> Result<Vec<&[u8]>, InvalidResponse> {
    if body.len() < 5 || body[0] & 0x7f != 0 {
        return looks_like_protobuf(body)
            .then_some(vec![body])
            .ok_or(InvalidResponse);
    }
    let mut payloads = Vec::new();
    let mut index = 0;
    let mut trailers_started = false;
    while index < body.len() {
        if index + 5 > body.len() {
            return Err(InvalidResponse);
        }
        let flags = body[index];
        index += 1;
        if flags & 0x7f != 0 {
            return Err(InvalidResponse);
        }
        let trailer = flags & 0x80 != 0;
        if trailers_started && !trailer {
            return Err(InvalidResponse);
        }
        let length = u32::from_be_bytes(
            body[index..index + 4]
                .try_into()
                .map_err(|_| InvalidResponse)?,
        ) as usize;
        index += 4;
        let end = index
            .checked_add(length)
            .filter(|end| *end <= body.len())
            .ok_or(InvalidResponse)?;
        if trailer {
            trailers_started = true;
            validate_grpc_trailer(&body[index..end])?;
        } else {
            payloads.push(&body[index..end]);
        }
        index = end;
    }
    (!payloads.is_empty())
        .then_some(payloads)
        .ok_or(InvalidResponse)
}

fn validate_grpc_trailer(body: &[u8]) -> Result<(), InvalidResponse> {
    let text = std::str::from_utf8(body).map_err(|_| InvalidResponse)?;
    for line in text.lines().filter(|line| !line.is_empty()) {
        let (key, value) = line.split_once(':').ok_or(InvalidResponse)?;
        if key.trim().eq_ignore_ascii_case("grpc-status")
            && value.trim().parse::<u32>().map_err(|_| InvalidResponse)? != 0
        {
            return Err(InvalidResponse);
        }
    }
    Ok(())
}

fn looks_like_protobuf(body: &[u8]) -> bool {
    body.first().is_some_and(|byte| {
        let field = byte >> 3;
        let wire = byte & 0x07;
        field > 0 && matches!(wire, 0 | 1 | 2 | 5)
    })
}

fn scan_protobuf(
    body: &[u8],
    path: &[u32],
    depth: usize,
    scan: &mut ProtobufScan,
) -> Result<(), InvalidResponse> {
    let mut index = 0;
    while index < body.len() {
        let key = read_varint(body, &mut index)?;
        let field = u32::try_from(key >> 3).map_err(|_| InvalidResponse)?;
        let wire = u8::try_from(key & 0x07).map_err(|_| InvalidResponse)?;
        if field == 0 || field > 0x1fff_ffff {
            return Err(InvalidResponse);
        }
        let mut field_path = path.to_vec();
        field_path.push(field);
        match wire {
            0 => scan.varints.push(VarintField {
                path: field_path,
                value: read_varint(body, &mut index)?,
            }),
            1 => {
                index = index
                    .checked_add(8)
                    .filter(|end| *end <= body.len())
                    .ok_or(InvalidResponse)?;
            }
            2 => {
                let length =
                    usize::try_from(read_varint(body, &mut index)?).map_err(|_| InvalidResponse)?;
                let end = index
                    .checked_add(length)
                    .filter(|end| *end <= body.len())
                    .ok_or(InvalidResponse)?;
                if depth >= 4 && length != 0 {
                    return Err(InvalidResponse);
                }
                if depth < 4 && length != 0 {
                    scan_protobuf(&body[index..end], &field_path, depth + 1, scan)?;
                }
                index = end;
            }
            5 => {
                let end = index
                    .checked_add(4)
                    .filter(|end| *end <= body.len())
                    .ok_or(InvalidResponse)?;
                let bytes: [u8; 4] = body[index..end].try_into().map_err(|_| InvalidResponse)?;
                scan.fixed32.push(Fixed32Field {
                    path: field_path,
                    value: f32::from_le_bytes(bytes),
                    order: scan.order,
                });
                scan.order += 1;
                index = end;
            }
            _ => return Err(InvalidResponse),
        }
    }
    Ok(())
}

fn read_varint(body: &[u8], index: &mut usize) -> Result<u64, InvalidResponse> {
    let mut value = 0_u64;
    for shift in (0..64).step_by(7) {
        let byte = *body.get(*index).ok_or(InvalidResponse)?;
        *index += 1;
        if shift == 63 && byte & 0x7e != 0 {
            return Err(InvalidResponse);
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(InvalidResponse)
}
