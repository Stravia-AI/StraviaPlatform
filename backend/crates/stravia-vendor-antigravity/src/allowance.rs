use std::collections::BTreeMap;

use serde_json::{Map, Value, json};
use stravia_vendor_common::common::plugin_error;
use stravia_vendor_sdk::{
    AllowanceAmount, AllowanceItem, AllowanceResponse, ErrorKind, GuestHost, PluginError,
    ProviderSnapshot,
};

pub(crate) fn execute(
    host: &GuestHost,
    provider: &ProviderSnapshot,
) -> Result<AllowanceResponse, PluginError> {
    let project = crate::client::project(host, provider)?;
    let payload = crate::client::post_json(
        host,
        provider,
        "retrieveUserQuotaSummary",
        &json!({"project": project}),
    )?;
    parse(&payload).ok_or_else(|| {
        plugin_error(
            ErrorKind::upstream_unknown(),
            "Upstream quota summary has no valid allowances or an unsupported shape",
        )
    })
}

fn text<'a>(object: &'a Map<String, Value>, key: &str) -> Option<Option<&'a str>> {
    match object.get(key) {
        None => Some(None),
        Some(value) => Some(Some(value.as_str()?.trim()).filter(|value| !value.is_empty())),
    }
}

fn parse(payload: &Value) -> Option<AllowanceResponse> {
    let object = payload.as_object()?;
    let mut items = BTreeMap::new();
    // 先读分组，重复的顶层 bucket 不会抹掉账号共享池的显示标签。
    if let Some(groups) = object.get("groups") {
        for group in groups.as_array()? {
            let group = group.as_object()?;
            let label = text(group, "displayName")?;
            if let Some(buckets) = group.get("buckets") {
                for bucket in buckets.as_array()? {
                    insert(&mut items, bucket, label)?;
                }
            }
        }
    }
    if let Some(buckets) = object.get("buckets") {
        for bucket in buckets.as_array()? {
            insert(&mut items, bucket, None)?;
        }
    }
    if !items
        .values()
        .any(|item: &AllowanceItem| item.used_percent.is_some() || item.remaining.is_some())
    {
        return None;
    }
    Some(AllowanceResponse {
        allowances: items.into_values().collect(),
        models: Vec::new(),
        plan_label: None,
    })
}

fn insert(
    items: &mut BTreeMap<String, AllowanceItem>,
    bucket: &Value,
    group: Option<&str>,
) -> Option<()> {
    let object = bucket.as_object()?;
    let id = text(object, "bucketId")?;
    let window = text(object, "window")?;
    let display = text(object, "displayName")?;
    let disabled = match object.get("disabled") {
        None => false,
        Some(value) => value.as_bool()?,
    };
    let fraction = match object.get("remainingFraction") {
        None => None,
        Some(value) => {
            let number = value.as_f64()?;
            if !number.is_finite() || !(0.0..=1.0).contains(&number) {
                return None;
            }
            Some(number)
        }
    };
    let amount = match object.get("remainingAmount") {
        None => None,
        Some(value) => {
            let number = if let Some(value) = value.as_str() {
                value.parse::<i64>().ok()?
            } else {
                value.as_i64()?
            };
            if number < 0 {
                return None;
            }
            Some(number)
        }
    };
    let reset = match object.get("resetTime") {
        None => None,
        Some(value) => Some(
            chrono::DateTime::parse_from_rfc3339(value.as_str()?)
                .ok()?
                .timestamp_millis(),
        ),
    };
    // 没有 fraction 不等于额度为零；仅 reset/window 也不是可用额度快照。
    if fraction.is_none() && amount.is_none() && !disabled {
        return Some(());
    }
    let key = match id {
        Some(id) => format!("bucket:{id}"),
        None => format!(
            "window:{}",
            serde_json::to_string(&(group.unwrap_or(""), window?)).ok()?
        ),
    };
    let name = display.or(window).or(id)?;
    let mut label = match group {
        Some(group) => format!("{group} / {name}"),
        None => name.to_owned(),
    };
    if let Some(window) = window
        && name != window
    {
        label.push_str(&format!(" ({window})"));
    }
    if disabled {
        label.push_str(" (disabled)");
    }
    // 百分比仅表示共享池使用率，不伪造 token 上限或货币单位。
    let used_percent = fraction.map(|fraction| decimal((1.0 - fraction) * 100.0));
    let condition = if disabled {
        None
    } else if fraction == Some(0.0) || (fraction.is_none() && amount == Some(0)) {
        Some("exhausted".into())
    } else {
        fraction.map(|fraction| {
            if fraction < 0.2 {
                "tight".into()
            } else {
                "normal".into()
            }
        })
    };
    let item = AllowanceItem {
        key: key.clone(),
        label,
        kind: "quota_window".into(),
        used: None,
        remaining: amount.map(|value| AllowanceAmount {
            value: value.to_string(),
            unit: "unknown".into(),
            currency: None,
        }),
        limit: None,
        used_percent,
        window_seconds: match window {
            Some("5h") => Some(5 * 60 * 60),
            Some("weekly") => Some(7 * 24 * 60 * 60),
            _ => None,
        },
        resets_at_unix_ms: reset,
        condition,
    };
    if let Some(existing) = items.get_mut(&key) {
        // 两处允许补齐省略值，但冲突不是可安全覆盖的成功快照。
        if existing.label.ends_with(" (disabled)") != disabled {
            return None;
        }
        merge(&mut existing.used_percent, item.used_percent, |a, b| a == b)?;
        merge(&mut existing.remaining, item.remaining, |a, b| {
            a.value == b.value && a.unit == b.unit && a.currency == b.currency
        })?;
        merge(
            &mut existing.resets_at_unix_ms,
            item.resets_at_unix_ms,
            |a, b| a == b,
        )?;
        merge(&mut existing.window_seconds, item.window_seconds, |a, b| {
            a == b
        })?;
        if !disabled {
            let used = existing
                .used_percent
                .as_deref()
                .and_then(|value| value.parse::<f64>().ok());
            let zero = existing
                .remaining
                .as_ref()
                .is_some_and(|value| value.value == "0");
            existing.condition = if used == Some(100.0) || (used.is_none() && zero) {
                Some("exhausted".into())
            } else {
                used.map(|used| {
                    if used > 80.0 {
                        "tight".into()
                    } else {
                        "normal".into()
                    }
                })
            };
        }
    } else {
        items.insert(key, item);
    }
    Some(())
}

fn merge<T>(
    target: &mut Option<T>,
    source: Option<T>,
    equal: impl FnOnce(&T, &T) -> bool,
) -> Option<()> {
    match (target.as_ref(), source) {
        (Some(existing), Some(source)) => {
            if !equal(existing, &source) {
                return None;
            }
        }
        (None, Some(source)) => *target = Some(source),
        _ => {}
    }
    Some(())
}

fn decimal(value: f64) -> String {
    // 上游 proto float 的百分比保留六位小数，避免 30.000000000000004 一类展示噪声。
    let formatted = format!("{value:.6}");
    // 极小的真实剩余额度不能因展示舍入变成耗尽，极小使用量也不能变成零。
    if (formatted == "100.000000" && value < 100.0) || (formatted == "0.000000" && value > 0.0) {
        return value.to_string();
    }
    formatted
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinguishes_missing_zero_disabled_and_unknown_windows() {
        let result = parse(&json!({"groups": [{"displayName": "Shared pool", "buckets": [
            {"bucketId": "missing", "window": "5h"},
            {"bucketId": "zero", "window": "weekly", "remainingFraction": 0},
            {"bucketId": "disabled", "window": "5h", "remainingFraction": 0, "disabled": true},
            {"bucketId": "future", "window": "quarterly", "remainingAmount": "9223372036854775807"}
        ]}]})).unwrap();
        assert_eq!(result.allowances.len(), 3);
        assert!(
            !result
                .allowances
                .iter()
                .any(|item| item.key == "bucket:missing")
        );
        let zero = result
            .allowances
            .iter()
            .find(|item| item.key == "bucket:zero")
            .unwrap();
        assert_eq!(zero.used_percent.as_deref(), Some("100"));
        assert_eq!(zero.condition.as_deref(), Some("exhausted"));
        let disabled = result
            .allowances
            .iter()
            .find(|item| item.key == "bucket:disabled")
            .unwrap();
        assert_eq!(disabled.condition, None);
        let future = result
            .allowances
            .iter()
            .find(|item| item.key == "bucket:future")
            .unwrap();
        assert!(future.label.contains("Shared pool / quarterly"));
        assert_eq!(future.window_seconds, None);
        assert_eq!(future.used_percent, None);
        assert_eq!(
            future.remaining.as_ref().unwrap().value,
            "9223372036854775807"
        );
        assert_eq!(future.remaining.as_ref().unwrap().unit, "unknown");
        assert!(future.limit.is_none());
    }

    #[test]
    fn deduplicates_shared_buckets_and_preserves_precision_and_reset() {
        let bucket = json!({"bucketId": "gemini-5h", "window": "5h", "remainingFraction": 0.7, "resetTime": "2026-01-01T01:00:00+01:00"});
        let result = parse(&json!({"groups": [{"displayName": "Gemini", "buckets": [bucket.clone()]}], "buckets": [bucket]})).unwrap();
        assert_eq!(result.allowances.len(), 1);
        let item = &result.allowances[0];
        assert_eq!(item.label, "Gemini / 5h");
        assert_eq!(item.used_percent.as_deref(), Some("30"));
        assert_eq!(item.resets_at_unix_ms, Some(1767225600000));
        assert!(item.limit.is_none());
    }

    #[test]
    fn rejects_invalid_values_instead_of_clamping_or_empty_success() {
        for field in [
            json!({"remainingFraction": -0.1}),
            json!({"remainingFraction": 1.1}),
            json!({"remainingFraction": "NaN"}),
            json!({"remainingFraction": "Infinity"}),
            json!({"remainingFraction": null}),
            json!({"remainingFraction": 0.5, "resetTime": "invalid"}),
            json!({"remainingAmount": "9223372036854775808"}),
            json!({"remainingAmount": -1}),
        ] {
            let mut bucket = field.as_object().unwrap().clone();
            bucket.insert("bucketId".into(), json!("bad"));
            assert!(parse(&json!({"buckets": [bucket]})).is_none());
        }
        for payload in [
            json!({}),
            json!({"groups": {}}),
            json!({"buckets": []}),
            json!({"buckets": [{"bucketId": "x", "window": "weekly"}]}),
        ] {
            assert!(parse(&payload).is_none());
        }
        assert!(parse(&json!({"buckets": [{"bucketId": "same", "remainingFraction": 0.5}, {"bucketId": "same", "remainingFraction": 0.6}]})).is_none());
    }

    #[test]
    fn fraction_precedes_opaque_amount_and_rounding_does_not_exhaust() {
        let result = parse(&json!({"buckets": [
            {"bucketId": "fraction", "remainingFraction": 0.5, "remainingAmount": "0"},
            {"bucketId": "tiny", "remainingFraction": 0.000000001},
            {"bucketId": "tiny", "remainingFraction": 0.000000001}
        ]}))
        .unwrap();
        let fraction = result
            .allowances
            .iter()
            .find(|item| item.key == "bucket:fraction")
            .unwrap();
        assert_eq!(fraction.condition.as_deref(), Some("normal"));
        let tiny = result
            .allowances
            .iter()
            .find(|item| item.key == "bucket:tiny")
            .unwrap();
        assert_eq!(tiny.condition.as_deref(), Some("tight"));
        assert_ne!(tiny.used_percent.as_deref(), Some("100"));
    }
}
