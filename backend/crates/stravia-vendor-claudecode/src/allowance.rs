//! Claude 订阅额度：`GET /api/oauth/usage`。响应形态与请求头跟随 oh-my-pi
//! v18.4.2 `packages/ai/src/usage/claude.ts`；新版以 `limits[]` 描述各窗口，
//! 旧版以 `five_hour` / `seven_day` 对象给出 `utilization` 百分比。
//! usage 响应不含订阅等级，等级另从 `GET /api/oauth/profile` 读取。

use std::collections::BTreeMap;

use serde_json::{Map, Value};
use stravia_vendor_common::account::first_account_label;
use stravia_vendor_common::common;
use stravia_vendor_common::connection_check::{self, invalid_response};
use stravia_vendor_common::plan::display_name;
use stravia_vendor_sdk::{
    AllowanceAmount, AllowanceBilling, AllowanceBillingKind, AllowanceBillingPeriod,
    AllowanceBillingPrice, AllowanceItem, AllowanceResponse, ConnectionCheckResponse,
    ConnectionCheckVerification, GuestHost, HttpRequest, ModelAllowance, PluginError,
    ProviderSnapshot, read_http_body,
};

use crate::request::{DEFAULT_CLIENT_VERSION, cli_user_agent};
use crate::{access_token, retryable};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
const USAGE_BETAS: &str = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,advanced-tool-use-2025-11-20,effort-2025-11-24,extended-cache-ttl-2025-04-11";
const MAX_USAGE_BODY: usize = 1024 * 1024;
const MAX_PROFILE_BODY: usize = 512 * 1024;
const FIVE_HOURS: u64 = 18_000;
const SEVEN_DAYS: u64 = 604_800;

pub(crate) fn execute(
    host: &GuestHost,
    provider: &ProviderSnapshot,
) -> Result<AllowanceResponse, PluginError> {
    let access_token = access_token(provider)?;
    let response = host.http_start(usage_request(access_token))?;
    let status = response.status()?;
    let headers = response.headers()?;
    let body = read_http_body(&response, MAX_USAGE_BODY)?;
    if !(200..300).contains(&status) {
        return Err(common::upstream_error(status, &headers, &body));
    }
    let payload: Value = serde_json::from_slice(&body)
        .map_err(|_| retryable("Claude usage returned invalid JSON"))?;
    let mut response = parse(&payload)
        .ok_or_else(|| retryable("Claude usage response has no allowance windows"))?;
    // 订阅等级不在 usage 响应里；Claude Code 自己登录/刷新时也向
    // `/api/oauth/profile` 取 `organization_type` 与 `rate_limit_tier`。
    // 该接口失败不影响额度展示，因此失败时保持 None。
    let plan = fetch_profile_plan(host, access_token);
    response.plan_label = plan.as_deref().and_then(display_name);
    // 等级能确定时才给出计费建议；价格为 Anthropic 官方页面公布的美元月费。
    response.billing = plan.as_deref().and_then(billing_for_plan);
    // 只取登录时保存的邮箱；`organization_name` 是组织名，不是账号标识。
    response.account_label =
        first_account_label([provider.credentials.get("email").and_then(Value::as_str)]);
    Ok(response)
}

/// 固定 Anthropic 账号 usage 端点只验证 OAuth 凭据；它不请求配置的 Base URL，
/// 因此自定义端点不会被这次检查确认。
pub(crate) fn connection_check(
    host: &GuestHost,
    provider: &ProviderSnapshot,
) -> Result<ConnectionCheckResponse, PluginError> {
    let value = connection_check::json(host, usage_request(access_token(provider)?))?;
    validate_usage(&value)?;
    Ok(ConnectionCheckResponse {
        verified: ConnectionCheckVerification::Credentials,
    })
}

fn usage_request(access_token: &str) -> HttpRequest {
    HttpRequest {
        method: "GET".into(),
        url: USAGE_URL.into(),
        headers: vec![
            ("accept".into(), "application/json, text/plain, */*".into()),
            ("anthropic-beta".into(), USAGE_BETAS.into()),
            ("content-type".into(), "application/json".into()),
            ("user-agent".into(), cli_user_agent(DEFAULT_CLIENT_VERSION)),
            ("authorization".into(), format!("Bearer {access_token}")),
        ],
        body: Vec::new(),
    }
}

/// 接受 `limits` 的空列表和缺省窗口；只要出现的已知字段类型正确即可通过，
/// 不用任意 2xx JSON 冒充 usage 响应。
fn validate_usage(value: &Value) -> Result<(), PluginError> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid_response("Claude usage response must be a JSON object"))?;
    let mut recognized = false;
    if let Some(limits) = object.get("limits").filter(|value| !value.is_null()) {
        recognized = true;
        let limits = limits
            .as_array()
            .ok_or_else(|| invalid_response("Claude usage `limits` must be an array"))?;
        if limits.iter().any(|limit| !limit.is_object()) {
            return Err(invalid_response(
                "Claude usage `limits` entries must be objects",
            ));
        }
    }
    for field in ["five_hour", "seven_day", "spend"] {
        if let Some(window) = object.get(field).filter(|value| !value.is_null()) {
            recognized = true;
            if !window.is_object() {
                return Err(invalid_response(format!(
                    "Claude usage `{field}` must be an object"
                )));
            }
        }
    }
    if !recognized {
        return Err(invalid_response(
            "Claude usage response has no recognized fields",
        ));
    }
    Ok(())
}

/// 取规范化订阅标识（见 `profile_plan`）。profile 接口只用于展示与计费建议，
/// 失败或无 `user:profile` 权限时降级为 `None`，不让一次附加请求拖垮额度刷新。
fn fetch_profile_plan(host: &GuestHost, access_token: &str) -> Option<String> {
    let response = host
        .http_start(HttpRequest {
            method: "GET".into(),
            url: PROFILE_URL.into(),
            headers: vec![
                ("accept".into(), "application/json, text/plain, */*".into()),
                ("anthropic-beta".into(), "oauth-2025-04-20".into()),
                ("content-type".into(), "application/json".into()),
                ("user-agent".into(), cli_user_agent(DEFAULT_CLIENT_VERSION)),
                ("authorization".into(), format!("Bearer {access_token}")),
            ],
            body: Vec::new(),
        })
        .ok()?;
    if !(200..300).contains(&response.status().ok()?) {
        return None;
    }
    let body = read_http_body(&response, MAX_PROFILE_BODY).ok()?;
    profile_plan(&serde_json::from_slice(&body).ok()?)
}

/// 依据 `/api/oauth/profile` 得到规范化订阅标识：`free` / `pro` / `max` /
/// `max_<倍率>` / `team` / `enterprise`。展示名和计费表都从这里派生。
///
/// Claude Code 客户端 `fetchProfileInfo` 把 `organization.organization_type` 映射为
/// `claude_max` → max、`claude_pro` → pro、`claude_team` → team、
/// `claude_enterprise` → enterprise；Max 的 5x/20x 倍率只出现在
/// `organization.rate_limit_tier`（`default_claude_max_5x` / `_20x`）。
/// 组织字段缺失时按 `rate_limit_tier`、再按账号的 `has_claude_max` /
/// `has_claude_pro` 兜底；两个布尔值都明确为 false 时才是 Free。
fn profile_plan(profile: &Value) -> Option<String> {
    let organization = profile.get("organization");
    let organization_type = organization
        .and_then(|value| value.get("organization_type"))
        .and_then(non_empty);
    let rate_limit_tier = organization
        .and_then(|value| value.get("rate_limit_tier"))
        .and_then(non_empty);
    if let Some(plan) = organization_type.and_then(plan_from_organization) {
        return max_plan(plan, rate_limit_tier);
    }
    if let Some(tier) = rate_limit_tier
        && let Some(plan) = plan_from_rate_limit_tier(tier)
    {
        return Some(plan);
    }
    let account = profile.get("account");
    let flag = |field: &str| {
        account
            .and_then(|value| value.get(field))
            .and_then(Value::as_bool)
    };
    match (flag("has_claude_max"), flag("has_claude_pro")) {
        (Some(true), _) => max_plan("max", rate_limit_tier),
        (_, Some(true)) => Some("pro".into()),
        (Some(false), Some(false)) => Some("free".into()),
        _ => None,
    }
}

fn plan_from_organization(organization_type: &str) -> Option<&'static str> {
    match organization_type.trim().to_ascii_lowercase().as_str() {
        "claude_max" => Some("max"),
        "claude_pro" => Some("pro"),
        "claude_team" => Some("team"),
        "claude_enterprise" => Some("enterprise"),
        _ => None,
    }
}

/// 只有组织类型缺失时才直接读 tier；Team Premium 也会带
/// `default_claude_max_5x`，所以有组织类型时必须优先信组织类型。
fn plan_from_rate_limit_tier(rate_limit_tier: &str) -> Option<String> {
    let tier = rate_limit_tier.trim().to_ascii_lowercase();
    if tier.contains("max") {
        return max_plan("max", Some(rate_limit_tier));
    }
    if tier.contains("pro") {
        return Some("pro".into());
    }
    if tier.contains("team") {
        return Some("team".into());
    }
    if tier.contains("enterprise") {
        return Some("enterprise".into());
    }
    None
}

/// Max 保留倍率（`max_5x` / `max_20x`）：不同倍率价格不同，不能合并成一个等级。
fn max_plan(plan: &str, rate_limit_tier: Option<&str>) -> Option<String> {
    if plan != "max" {
        return Some(plan.into());
    }
    Some(match rate_limit_tier.and_then(max_multiplier) {
        Some(multiplier) => format!("max_{multiplier}"),
        None => "max".into(),
    })
}

/// 按规范化订阅标识给出计费建议。
///
/// Anthropic 官方 claude.ai 个人方案价格（美元、按月）：Pro $20/月；
/// Max 5x（`default_claude_max_5x`）$100/月；Max 20x（页面现展示为 Ultra，
/// 升级链接带 `plan=max_20x`）$200/月；Team 按席位（Standard $25/席/月、
/// Premium $125/席/月），Enterprise 为席位费加 API 用量，都没有单一账号价格。
/// 来源：https://claude.com/pricing（www.anthropic.com/pricing 跳转），2026-11-02 核对。
fn billing_for_plan(plan: &str) -> Option<AllowanceBilling> {
    match plan {
        "pro" => Some(subscription(Some("20.00"))),
        "max_5x" => Some(subscription(Some("100.00"))),
        "max_20x" => Some(subscription(Some("200.00"))),
        // 只知道是 Max、不知道 5x/20x 时两条价格都可能，留给用户选择。
        "max" => Some(subscription(None)),
        // Team / Enterprise 按席位或用量计价，价格随组织规模变化。
        "team" | "enterprise" => Some(subscription(None)),
        // Free 与未知等级都不能断言是付费订阅。
        _ => None,
    }
}

fn subscription(amount: Option<&str>) -> AllowanceBilling {
    AllowanceBilling {
        kind: AllowanceBillingKind::Subscription,
        period: Some(AllowanceBillingPeriod::Month),
        price: amount.map(|amount| AllowanceBillingPrice {
            amount: amount.into(),
            currency: "USD".into(),
        }),
    }
}

/// `default_claude_max_5x` / `default_claude_max_20x` 中 `max` 后面的倍率。
/// 只接受 `<数字>x`，避免把其它 tier 词误当成倍率。
fn max_multiplier(rate_limit_tier: &str) -> Option<String> {
    let mut words = rate_limit_tier
        .split(['_', '-'])
        .filter(|word| !word.is_empty());
    words.find(|word| word.eq_ignore_ascii_case("max"))?;
    let multiplier = words.next()?.to_ascii_lowercase();
    let digits = multiplier.strip_suffix('x')?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(multiplier)
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
        account_label: None,
        // 计费建议要等 profile 判定订阅等级后才能填；usage 响应本身不含计费信息。
        billing: None,
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

    /// 展示名和计费建议都由同一个规范化等级派生，旧用例通过这里继续校验展示名。
    fn profile_plan_label(profile: &Value) -> Option<String> {
        profile_plan(profile).as_deref().and_then(display_name)
    }

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

    #[test]
    fn profile_plan_label_uses_organization_and_max_multiplier() {
        let profile = |organization_type: &str, rate_limit_tier: &str| {
            json!({
                "account": {"uuid": "acc", "has_claude_max": false, "has_claude_pro": false},
                "organization": {
                    "uuid": "org",
                    "organization_type": organization_type,
                    "rate_limit_tier": rate_limit_tier
                }
            })
        };
        assert_eq!(
            profile_plan_label(&profile("claude_max", "default_claude_max_20x")).as_deref(),
            Some("Max 20x")
        );
        assert_eq!(
            profile_plan_label(&profile("claude_max", "default_claude_max_5x")).as_deref(),
            Some("Max 5x")
        );
        assert_eq!(
            profile_plan_label(&profile("claude_max", "")).as_deref(),
            Some("Max")
        );
        assert_eq!(
            profile_plan_label(&profile("claude_pro", "default_claude_pro")).as_deref(),
            Some("Pro")
        );
        assert_eq!(
            profile_plan_label(&profile("claude_team", "default_claude_team")).as_deref(),
            Some("Team")
        );
        // Team Premium 也带 max_5x tier，组织类型必须优先，不能误报成 Max。
        assert_eq!(
            profile_plan_label(&profile("claude_team", "default_claude_max_5x")).as_deref(),
            Some("Team")
        );
        assert_eq!(
            profile_plan_label(&profile("claude_enterprise", "default_claude_enterprise"))
                .as_deref(),
            Some("Enterprise")
        );
    }

    #[test]
    fn profile_plan_label_falls_back_to_flags_and_rejects_unknown() {
        let profile = |account: Value, organization: Value| json!({"account": account, "organization": organization});
        assert_eq!(
            profile_plan_label(&profile(
                json!({"has_claude_max": true, "has_claude_pro": true}),
                json!({"rate_limit_tier": "default_claude_max_5x"})
            ))
            .as_deref(),
            Some("Max 5x")
        );
        assert_eq!(
            profile_plan_label(&profile(json!({"has_claude_pro": true}), json!({}))).as_deref(),
            Some("Pro")
        );
        assert_eq!(
            profile_plan_label(&profile(
                json!({"has_claude_max": false, "has_claude_pro": false}),
                json!({"organization_type": "claude_free"})
            ))
            .as_deref(),
            Some("Free")
        );
        assert_eq!(
            profile_plan_label(&profile(json!({"has_claude_pro": false}), json!({}))),
            None
        );
        assert_eq!(profile_plan_label(&json!({})), None);
    }

    #[test]
    fn billing_for_plan_uses_published_claude_ai_prices() {
        for (plan, amount) in [
            ("pro", "20.00"),
            ("max_5x", "100.00"),
            ("max_20x", "200.00"),
        ] {
            let billing = billing_for_plan(plan).unwrap();
            assert_eq!(billing.kind, AllowanceBillingKind::Subscription);
            assert_eq!(billing.period, Some(AllowanceBillingPeriod::Month));
            let price = billing.price.unwrap();
            assert_eq!(
                (price.amount.as_str(), price.currency.as_str()),
                (amount, "USD")
            );
        }
    }

    #[test]
    fn seat_priced_and_unknown_multiplier_plans_are_subscriptions_without_a_price() {
        // Team/Enterprise 按席位或用量计价；`max` 未给出倍率时 5x 与 20x 两种价格都可能。
        for plan in ["team", "enterprise", "max"] {
            let billing = billing_for_plan(plan).unwrap();
            assert_eq!(billing.kind, AllowanceBillingKind::Subscription);
            assert_eq!(billing.period, Some(AllowanceBillingPeriod::Month));
            assert!(billing.price.is_none(), "{plan}");
        }
    }

    #[test]
    fn free_and_unknown_plans_have_no_billing() {
        for plan in ["free", "max_100x", "", "claude_max", "Pro"] {
            assert!(billing_for_plan(plan).is_none(), "{plan}");
        }
    }

    #[test]
    fn profile_billing_uses_the_same_normalized_plan_as_the_label() {
        let billing = |organization_type: &str, rate_limit_tier: &str| {
            let profile = json!({
                "account": {"has_claude_max": false, "has_claude_pro": false},
                "organization": {
                    "organization_type": organization_type,
                    "rate_limit_tier": rate_limit_tier
                }
            });
            profile_plan(&profile)
                .as_deref()
                .and_then(billing_for_plan)
                .and_then(|billing| billing.price)
                .map(|price| price.amount)
        };
        assert_eq!(
            billing("claude_pro", "default_claude_pro").as_deref(),
            Some("20.00")
        );
        assert_eq!(
            billing("claude_max", "default_claude_max_5x").as_deref(),
            Some("100.00")
        );
        assert_eq!(
            billing("claude_max", "default_claude_max_20x").as_deref(),
            Some("200.00")
        );
        // Team Premium 也带 max_5x tier，但组织类型是 Team，不能套用 Max 的价格。
        assert_eq!(billing("claude_team", "default_claude_max_5x"), None);
        // 没有倍率的 Max 只知道是订阅，不知道是 5x 还是 20x。
        assert_eq!(billing("claude_max", ""), None);
    }

    #[test]
    fn connection_check_accepts_empty_limits_and_rejects_unknown_shapes() {
        assert!(validate_usage(&json!({"limits": []})).is_ok());
        assert!(
            validate_usage(&json!({"limits": [{"kind": "session"}], "five_hour": null})).is_ok()
        );
        assert!(validate_usage(&json!({"five_hour": {"utilization": 1.5}})).is_ok());
        assert!(validate_usage(&json!({"spend": {"enabled": false}})).is_ok());
        assert!(validate_usage(&json!({})).is_err());
        assert!(validate_usage(&json!([])).is_err());
        assert!(validate_usage(&json!({"limits": {}})).is_err());
        assert!(validate_usage(&json!({"limits": [1]})).is_err());
        assert!(validate_usage(&json!({"five_hour": 1})).is_err());
    }
}
