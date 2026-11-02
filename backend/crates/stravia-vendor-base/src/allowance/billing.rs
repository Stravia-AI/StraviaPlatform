//! 额度快照附带的计费建议。
//!
//! 价格只取供应商官网公开的美元标价，并随本插件维护；额度响应本身不带价格。
//! 识别不到具体套餐时不猜价格：能确定是订阅就只给订阅与账期，否则保持未知。

use stravia_vendor_common::plan::display_name;
use stravia_vendor_sdk::{
    AllowanceBilling, AllowanceBillingKind, AllowanceBillingPeriod, AllowanceBillingPrice,
};

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

pub(super) fn usage() -> AllowanceBilling {
    AllowanceBilling {
        kind: AllowanceBillingKind::Usage,
        period: None,
        price: None,
    }
}

/// Copilot 用 `access_type_sku` 区分 Pro/Pro+（两者都可能报 `copilot_plan: individual`）。
pub(super) fn copilot(sku: Option<&str>, plan: Option<&str>) -> Option<AllowanceBilling> {
    let plan = match sku {
        Some("monthly_subscriber_quota") => "pro",
        Some("plus_monthly_subscriber_quota") => "pro_plus",
        Some("copilot_standalone_seat_quota" | "copilot_business_seat") => "business",
        Some("copilot_enterprise_seat_quota" | "copilot_enterprise_seat") => "enterprise",
        Some("free_limited_copilot" | "free_educational_quota") => return None,
        _ => plan?,
    };
    // 价格来源 https://github.com/features/copilot/plans ，2026-11-02 核对：
    // Pro $10/月、Pro+ $39/月、Max $100/月。Business/Enterprise 按席位计价且官网只留
    // “联系销售”，不写价格；`copilot_plan: individual` 同时对应 Pro 与 Pro+，无法区分。
    match display_name(plan)?.as_str() {
        "Pro" => Some(subscription(Some("10"))),
        "Pro Plus" => Some(subscription(Some("39"))),
        "Max" | "Individual Max" => Some(subscription(Some("100"))),
        "Individual" | "Business" | "Enterprise" => Some(subscription(None)),
        _ => None,
    }
}

/// Kimi 会员等级即付费订阅，但官网只列人民币价格。
pub(super) fn kimi(plan: Option<&str>) -> Option<AllowanceBilling> {
    // 会员价只有人民币（¥49–¥1399/月），来源
    // https://www.kimi.com/en/help/membership/membership-pricing ，2026-11-02 核对。
    match plan? {
        "Basic" | "Intermediate" | "Advanced" => Some(subscription(None)),
        _ => None,
    }
}

/// NanoGPT 只有一个 Pro 订阅，订阅用量接口返回窗口即说明账户在该订阅上。
pub(super) fn nano_gpt(subscribed: bool) -> Option<AllowanceBilling> {
    // 价格来源 https://nano-gpt.com/pricing ，2026-11-02 核对：Pro $12/月。
    subscribed.then(|| subscription(Some("12")))
}

pub(super) fn zai(plan: Option<&str>) -> Option<AllowanceBilling> {
    // 月付标价来源 https://z.ai/subscribe ，2026-11-02 核对：
    // Lite $18、Pro $80、Max $168。季付/年付是折算价，额度响应没有账期字段，
    // 因此按官方月付标价给月度建议。
    match plan? {
        "Lite" => Some(subscription(Some("18"))),
        "Pro" => Some(subscription(Some("80"))),
        "Max" => Some(subscription(Some("168"))),
        _ => None,
    }
}

/// BigModel（智谱）只有人民币价格。
pub(super) fn zhipu(plan: Option<&str>) -> Option<AllowanceBilling> {
    // 月付标价来源 https://zcode.z.ai/cn （BigModel 套餐页），2026-11-02 核对：
    // Lite ¥118、Pro ¥538、Max ¥1078，没有美元价格。
    match plan? {
        "Lite" | "Pro" | "Max" => Some(subscription(None)),
        _ => None,
    }
}

/// MiniMax 国际站用美元，国内站只有人民币；两站共用同一套解析。
pub(super) fn minimax(plan: Option<&str>, usd: bool) -> Option<AllowanceBilling> {
    let plan = plan?;
    if !usd {
        return matches!(plan, "Go" | "Explore" | "Build" | "Plus" | "Max" | "Ultra")
            .then(|| subscription(None));
    }
    // 月付标价来源 https://www.minimax.io/m-plan ，2026-11-02 核对：
    // Go $22、Explore $55、Build $132。Token Plan（Plus/Max/Ultra）已停售，
    // 当前官网不再列价，只标记订阅。
    match plan {
        "Go" => Some(subscription(Some("22"))),
        "Explore" => Some(subscription(Some("55"))),
        "Build" => Some(subscription(Some("132"))),
        "Plus" | "Max" | "Ultra" => Some(subscription(None)),
        _ => None,
    }
}

/// Wafer Pass 是订阅制，但官网没有公开价目表。
pub(super) fn wafer(plan: Option<&str>) -> Option<AllowanceBilling> {
    // 订阅方式（周/月/年）来源 https://www.wafer.ai/terms ，2026-11-02 核对；
    // 价格未公开，因此只标记订阅。
    plan.map(|_| subscription(None))
}

/// OpenCode Go 与 Go Plus 共用同一用量接口，响应不带套餐标识。
pub(super) fn opencode_go(subscribed: bool) -> Option<AllowanceBilling> {
    // 档位价格来源 https://opencode.ai/console/go ，2026-11-02 核对：
    // Go $10/月、Go Plus $40/月；无法确定当前档位，不写价格。
    subscribed.then(|| subscription(None))
}

/// ClinePass 只有一个公开月付价，其他显示名（年付或后续档位）只标记订阅。
pub(super) fn cline(plan: Option<&str>) -> Option<AllowanceBilling> {
    let plan = plan?;
    // 价格来源 https://docs.cline.bot/getting-started/clinepass ，
    // 2026-11-02 核对：ClinePass $9.99/月。
    if plan == "ClinePass Monthly" {
        Some(subscription(Some("9.99")))
    } else {
        Some(subscription(None))
    }
}

/// Neuralwatt 的订阅档位名就是 `subscription.plan`。
pub(super) fn neuralwatt(plan: Option<&str>) -> Option<AllowanceBilling> {
    // 月付标价来源 https://portal.neuralwatt.com/pricing ，2026-11-02 核对：
    // Basic $20、Standard $50、Pro $100、Max $200。
    match plan {
        Some("Basic") => Some(subscription(Some("20"))),
        Some("Standard") => Some(subscription(Some("50"))),
        Some("Pro") => Some(subscription(Some("100"))),
        Some("Max") => Some(subscription(Some("200"))),
        // 订阅存在但档位未知（或季度/年付档）：只标记订阅。
        Some(_) => Some(subscription(None)),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use stravia_vendor_sdk::{AllowanceBillingKind, AllowanceBillingPeriod};

    use super::{
        cline, copilot, kimi, minimax, nano_gpt, neuralwatt, opencode_go, usage, wafer, zai, zhipu,
    };

    #[test]
    fn priced_plans_carry_the_published_monthly_price() {
        for (billing, expected) in [
            (zai(Some("Lite")), "18"),
            (zai(Some("Pro")), "80"),
            (zai(Some("Max")), "168"),
            (nano_gpt(true), "12"),
            (minimax(Some("Go"), true), "22"),
            (minimax(Some("Explore"), true), "55"),
            (minimax(Some("Build"), true), "132"),
            (neuralwatt(Some("Pro")), "100"),
            (cline(Some("ClinePass Monthly")), "9.99"),
            (
                copilot(Some("monthly_subscriber_quota"), Some("individual")),
                "10",
            ),
            (copilot(Some("plus_monthly_subscriber_quota"), None), "39"),
            (copilot(None, Some("individual_max")), "100"),
        ] {
            let billing = billing.expect("identified plan");
            assert_eq!(billing.kind, AllowanceBillingKind::Subscription);
            assert_eq!(billing.period, Some(AllowanceBillingPeriod::Month));
            let price = billing.price.expect("price");
            assert_eq!(price.amount, expected);
            assert_eq!(price.currency, "USD");
        }
    }

    #[test]
    fn identified_subscriptions_without_a_usd_price_stay_period_only() {
        for billing in [
            zhipu(Some("Pro")),
            kimi(Some("Intermediate")),
            minimax(Some("Plus"), true),
            minimax(Some("Go"), false),
            wafer(Some("wafer_lite")),
            opencode_go(true),
            cline(Some("ClinePass Annual")),
            neuralwatt(Some("Team")),
            copilot(None, Some("business")),
            copilot(None, Some("individual")),
        ] {
            let billing = billing.expect("identified subscription");
            assert_eq!(billing.kind, AllowanceBillingKind::Subscription);
            assert_eq!(billing.period, Some(AllowanceBillingPeriod::Month));
            assert_eq!(billing.price, None);
        }
    }

    #[test]
    fn unknown_or_free_plans_have_no_billing() {
        assert_eq!(zai(Some("Team")), None);
        assert_eq!(zhipu(None), None);
        assert_eq!(kimi(Some("Unspecified")), None);
        assert_eq!(minimax(Some("Free"), true), None);
        assert_eq!(wafer(None), None);
        assert_eq!(opencode_go(false), None);
        assert_eq!(nano_gpt(false), None);
        assert_eq!(cline(None), None);
        assert_eq!(neuralwatt(None), None);
        assert_eq!(copilot(Some("free_limited_copilot"), Some("free")), None);
        assert_eq!(copilot(None, Some("individual_edu")), None);
    }

    #[test]
    fn usage_billing_has_no_period_or_price() {
        let billing = usage();
        assert_eq!(billing.kind, AllowanceBillingKind::Usage);
        assert_eq!(billing.period, None);
        assert_eq!(billing.price, None);
    }
}
