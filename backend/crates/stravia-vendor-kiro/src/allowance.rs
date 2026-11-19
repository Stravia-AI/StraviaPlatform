//! Kiro CLI 3.4.0 GetUsageLimits (bearer control plane).

use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::json;
use stravia_vendor_common::account::first_account_label;
use stravia_vendor_common::connection_check::invalid_response;
use stravia_vendor_sdk::{
    AllowanceAmount, AllowanceItem, AllowanceResponse, GuestHost, PluginError, ProviderSnapshot,
    read_http_body,
};

use crate::transport::{self, Action};

pub(super) fn execute(
    host: &GuestHost,
    provider: &ProviderSnapshot,
) -> Result<AllowanceResponse, PluginError> {
    let request = transport::rpc(
        provider,
        Action::GetUsageLimits,
        &json!({
            "origin": "KIRO_CLI",
            "isEmailRequired": true,
        }),
    )?;
    let response = transport::send(host, request)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok());
    parse(&read_http_body(&response, 1024 * 1024)?, now)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Usage {
    usage_breakdown_list: Vec<Breakdown>,
    next_date_reset: Option<f64>,
    subscription_info: Option<Subscription>,
    user_info: Option<User>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Subscription {
    subscription_title: Option<String>,
}

#[derive(Deserialize)]
struct User {
    email: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Breakdown {
    resource_type: Option<String>,
    dimension_type: Option<String>,
    unit: Option<String>,
    #[serde(default, with = "rust_decimal::serde::arbitrary_precision_option")]
    current_usage: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::arbitrary_precision_option")]
    current_usage_with_precision: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::arbitrary_precision_option")]
    usage_limit: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::arbitrary_precision_option")]
    usage_limit_with_precision: Option<Decimal>,
    next_date_reset: Option<f64>,
    free_trial_info: Option<Trial>,
    #[serde(default)]
    bonuses: Vec<Bonus>,
    #[serde(default, with = "rust_decimal::serde::arbitrary_precision_option")]
    current_overages: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::arbitrary_precision_option")]
    current_overages_with_precision: Option<Decimal>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Trial {
    free_trial_status: Option<String>,
    #[serde(default, with = "rust_decimal::serde::arbitrary_precision_option")]
    current_usage: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::arbitrary_precision_option")]
    current_usage_with_precision: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::arbitrary_precision_option")]
    usage_limit: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::arbitrary_precision_option")]
    usage_limit_with_precision: Option<Decimal>,
    free_trial_expiry: Option<f64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Bonus {
    bonus_code: Option<String>,
    status: Option<String>,
    #[serde(default, with = "rust_decimal::serde::arbitrary_precision_option")]
    current_usage: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::arbitrary_precision_option")]
    usage_limit: Option<Decimal>,
    expires_at: Option<f64>,
}

fn parse(bytes: &[u8], now: Option<i64>) -> Result<AllowanceResponse, PluginError> {
    let usage: Usage = serde_json::from_slice(bytes)
        .map_err(|e| invalid_response(format!("invalid Kiro usage response: {e}")))?;
    let mut allowances = Vec::new();
    let mut identities = HashSet::new();
    for breakdown in &usage.usage_breakdown_list {
        let resource = breakdown
            .resource_type
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid_response("Kiro usage breakdown missing resource type"))?;
        // Encode components separately: concatenation and punctuation cannot
        // make different upstream identities alias the same guard key.
        let identity = format!(
            "{}_{}",
            key_component(resource),
            key_component(breakdown.dimension_type.as_deref().unwrap_or(""))
        );
        if !identities.insert(identity.clone()) {
            return Err(invalid_response("duplicate Kiro usage breakdown identity"));
        }
        let unit = match breakdown.unit.as_deref() {
            Some(u) if u.eq_ignore_ascii_case("credit") || u.eq_ignore_ascii_case("credits") => {
                "credits"
            }
            // Production reports the CREDIT resource with the Smithy meter unit INVOCATIONS.
            Some("INVOCATIONS") if resource == "CREDIT" => "credits",
            None if resource == "AGENTIC_REQUEST" || resource == "CREDIT" => "credits",
            _ => "units",
        };
        push_item(
            &mut allowances,
            format!("monthly_{identity}"),
            unit,
            breakdown
                .current_usage_with_precision
                .or(breakdown.current_usage),
            breakdown
                .usage_limit_with_precision
                .or(breakdown.usage_limit),
            breakdown.next_date_reset.or(usage.next_date_reset),
        );
        if let Some(trial) = &breakdown.free_trial_info {
            if trial.free_trial_status.as_deref() == Some("ACTIVE")
                && unexpired(trial.free_trial_expiry, now)
            {
                push_item(
                    &mut allowances,
                    format!("trial_{identity}"),
                    unit,
                    trial.current_usage_with_precision.or(trial.current_usage),
                    trial.usage_limit_with_precision.or(trial.usage_limit),
                    trial.free_trial_expiry,
                );
            }
        }
        let mut bonus_codes = HashSet::new();
        for bonus in &breakdown.bonuses {
            if bonus.status.as_deref() != Some("ACTIVE") || !unexpired(bonus.expires_at, now) {
                continue;
            }
            let code = bonus
                .bonus_code
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| invalid_response("active Kiro bonus missing bonus code"))?;
            if !bonus_codes.insert(code) {
                return Err(invalid_response("duplicate Kiro bonus code"));
            }
            push_item(
                &mut allowances,
                format!("bonus_{identity}_{}", key_component(code)),
                unit,
                bonus.current_usage,
                bonus.usage_limit,
                bonus.expires_at,
            );
        }
        // Neither the schema nor the capture establishes the unit of the
        // overage cap. Do not combine it with the current overage or mark the
        // latter as credits; charges are a separate monetary field.
        push_item(
            &mut allowances,
            format!("extra_usage_{identity}"),
            "units",
            breakdown
                .current_overages_with_precision
                .or(breakdown.current_overages),
            None,
            breakdown.next_date_reset.or(usage.next_date_reset),
        );
    }
    Ok(AllowanceResponse {
        resets: None,
        reset_result: None,
        allowances,
        models: Vec::new(),
        plan_label: usage
            .subscription_info
            .and_then(|s| nonempty(s.subscription_title)),
        account_label: first_account_label([usage
            .user_info
            .as_ref()
            .and_then(|u| u.email.as_deref())]),
        billing: None,
    })
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|s| !s.trim().is_empty())
}

fn key_component(value: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(value.len() * 2);
    for byte in value.as_bytes() {
        write!(&mut out, "{byte:02x}").expect("writing to a String cannot fail");
    }
    out
}

fn unexpired(expiry: Option<f64>, now: Option<i64>) -> bool {
    match expiry {
        None => true,
        Some(expiry) => timestamp_millis(expiry)
            .zip(now)
            .is_some_and(|(expiry, now)| expiry > now),
    }
}

fn push_item(
    items: &mut Vec<AllowanceItem>,
    key: String,
    unit: &str,
    used: Option<Decimal>,
    limit: Option<Decimal>,
    reset: Option<f64>,
) {
    let used = used.filter(|n| *n >= Decimal::ZERO);
    let limit = limit.filter(|n| *n >= Decimal::ZERO);
    if used.is_none() && limit.is_none() {
        return;
    }
    let amount = |n: Decimal| AllowanceAmount {
        value: n.normalize().to_string(),
        unit: unit.into(),
        currency: None,
    };
    let remainder = used
        .zip(limit)
        .and_then(|(used, limit)| limit.checked_sub(used))
        .map(|n| n.max(Decimal::ZERO));
    let remaining = remainder.map(amount);
    let condition = used.zip(limit).map(|(used, limit)| {
        if used >= limit {
            "exhausted"
        } else if used > limit * Decimal::new(8, 1) {
            "tight"
        } else {
            "normal"
        }
        .to_owned()
    });
    let label = if key.starts_with("monthly") {
        "monthly"
    } else if key.starts_with("trial") {
        "trial"
    } else if key.starts_with("bonus") {
        "bonus"
    } else {
        "extra_usage"
    };
    items.push(AllowanceItem {
        label: label.into(),
        key,
        kind: "quota_window".into(),
        used: used.map(amount),
        remaining,
        limit: limit.map(amount),
        used_percent: None,
        window_seconds: None,
        resets_at_unix_ms: reset.and_then(timestamp_millis),
        condition,
    });
}

// Smithy timestamp JSON is Unix seconds (potentially fractional), not ms.
fn timestamp_millis(seconds: f64) -> Option<i64> {
    let ms = seconds * 1000.0;
    (ms.is_finite() && ms >= 0.0 && ms < i64::MAX as f64).then(|| ms as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    const NOW: Option<i64> = Some(1700000000000);

    #[test]
    fn parses_separate_credit_pools_and_seconds() {
        let response = parse(br#"{"nextDateReset":1750000000,"subscriptionInfo":{"subscriptionTitle":"Kiro Pro"},"usageBreakdownList":[{"resourceType":"AGENTIC_REQUEST","currentUsage":8,"currentUsageWithPrecision":8.25,"usageLimit":100,"freeTrialInfo":{"freeTrialStatus":"ACTIVE","currentUsage":1.5,"usageLimit":50,"freeTrialExpiry":1760000000},"bonuses":[{"bonusCode":"welcome","status":"ACTIVE","currentUsage":2,"usageLimit":10,"expiresAt":1770000000}],"currentOveragesWithPrecision":3.5,"overageCapWithPrecision":20}]}"#, NOW).unwrap();
        assert_eq!(response.plan_label.as_deref(), Some("Kiro Pro"));
        assert_eq!(response.allowances.len(), 4);
        assert_eq!(response.allowances[0].used.as_ref().unwrap().value, "8.25");
        assert_eq!(
            response.allowances[0].remaining.as_ref().unwrap().value,
            "91.75"
        );
        assert_eq!(
            response.allowances[0].resets_at_unix_ms,
            Some(1750000000000)
        );
        assert_eq!(
            response.allowances[1].resets_at_unix_ms,
            Some(1760000000000)
        );
        assert_eq!(
            response.allowances[2].resets_at_unix_ms,
            Some(1770000000000)
        );
        assert_eq!(response.allowances[3].used.as_ref().unwrap().unit, "units");
        assert!(response.allowances[3].limit.is_none());
        assert!(response.allowances.iter().all(|x| x.used_percent.is_none()));
    }

    #[test]
    fn exact_decimal_and_stable_identities_survive_reordering() {
        let a = br#"{"usageBreakdownList":[{"resourceType":"AGENTIC_REQUEST","currentUsage":9.9,"usageLimit":10,"bonuses":[{"bonusCode":"a","status":"ACTIVE","currentUsage":0,"usageLimit":1},{"bonusCode":"b","status":"ACTIVE","currentUsage":0,"usageLimit":2}]},{"resourceType":"OTHER","currentUsage":1}]}"#;
        let b = br#"{"usageBreakdownList":[{"resourceType":"OTHER","currentUsage":1},{"resourceType":"AGENTIC_REQUEST","currentUsage":9.9,"usageLimit":10,"bonuses":[{"bonusCode":"b","status":"ACTIVE","currentUsage":0,"usageLimit":2},{"bonusCode":"a","status":"ACTIVE","currentUsage":0,"usageLimit":1}]}]}"#;
        let first = parse(a, NOW).unwrap();
        let second = parse(b, NOW).unwrap();
        assert_eq!(first.allowances[0].remaining.as_ref().unwrap().value, "0.1");
        let mut first_keys: Vec<_> = first.allowances.iter().map(|a| &a.key).collect();
        let mut second_keys: Vec<_> = second.allowances.iter().map(|a| &a.key).collect();
        first_keys.sort();
        second_keys.sort();
        assert_eq!(first_keys, second_keys);
        assert!(
            parse(
                br#"{"usageBreakdownList":[{"resourceType":"CREDIT"},{"resourceType":"CREDIT"}]}"#,
                NOW
            )
            .is_err()
        );
        assert!(parse(br#"{"usageBreakdownList":[{"resourceType":"CREDIT","bonuses":[{"bonusCode":"a","status":"ACTIVE"},{"bonusCode":"a","status":"ACTIVE"}]}]}"#, NOW).is_err());
    }

    #[test]
    fn inactive_expired_or_unidentified_entitlements_have_no_available_balance() {
        let response = parse(br#"{"usageBreakdownList":[{"resourceType":"CREDIT","freeTrialInfo":{"freeTrialStatus":"EXPIRED","currentUsage":0,"usageLimit":10},"bonuses":[{"bonusCode":"a","status":"EXPIRED","usageLimit":10},{"bonusCode":"b","status":"EXHAUSTED","usageLimit":10},{"bonusCode":"c","status":"ACTIVE","expiresAt":1600000000,"usageLimit":10},{"bonusCode":"d","usageLimit":10}]}]}"#, NOW).unwrap();
        assert!(response.allowances.is_empty());
        assert!(parse(br#"{"usageBreakdownList":[{"resourceType":"CREDIT","bonuses":[{"status":"ACTIVE","usageLimit":10}]}]}"#, NOW).is_err());
        let no_clock = parse(br#"{"usageBreakdownList":[{"resourceType":"CREDIT","freeTrialInfo":{"freeTrialStatus":"ACTIVE","usageLimit":10,"freeTrialExpiry":1800000000}}]}"#, None).unwrap();
        assert!(no_clock.allowances.is_empty());
    }

    #[test]
    fn unknowns_remain_unknown() {
        let response = parse(br#"{"usageBreakdownList":[{"resourceType":"OTHER","currentUsage":2}],"limits":[{"percentUsed":50}]}"#, NOW).unwrap();
        assert!(response.plan_label.is_none());
        assert!(response.billing.is_none());
        assert!(response.allowances[0].limit.is_none());
        assert!(response.allowances[0].remaining.is_none());
        assert_eq!(response.allowances[0].used.as_ref().unwrap().unit, "units");
        assert!(parse(br#"{}"#, NOW).is_err());
        assert!(parse(br#"{"usageBreakdownList":"bad"}"#, NOW).is_err());
        assert_eq!(timestamp_millis(1750000000.125), Some(1750000000125));
        assert_eq!(timestamp_millis(-1.0), None);
    }

    #[test]
    fn extracts_trimmed_user_email_or_none() {
        let with_email = parse(
            br#"{"usageBreakdownList":[],"userInfo":{"userId":"opaque-id","email":"  alice@example.test  "}}"#,
            NOW,
        )
        .unwrap();
        assert_eq!(
            with_email.account_label.as_deref(),
            Some("alice@example.test")
        );

        for body in [
            br#"{"usageBreakdownList":[],"userInfo":{"userId":"opaque-id","email":"   "}}"#.as_slice(),
            br#"{"usageBreakdownList":[],"userInfo":{"userId":"opaque-id"}}"#.as_slice(),
            br#"{"usageBreakdownList":[]}"#.as_slice(),
        ] {
            let parsed = parse(body, NOW).unwrap();
            assert_eq!(parsed.account_label, None);
        }
    }
}
