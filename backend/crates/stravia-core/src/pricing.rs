//! 纯计价模块：上游按量成本、Route 计费与订阅摊销。
//!
//! 输入价格、已确认用量、尝试开始时间与币种快照，输出可能未知的双币种金额。
//! WebUI 不复制这里的任何规则。

use std::str::FromStr;

use chrono::{DateTime, Datelike, Timelike, Utc};
use chrono_tz::Tz;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};

pub use crate::interaction_observation::ConfirmedUsage;
use crate::provider_models::ModelCost;

const NANOS_PER_UNIT: i64 = 1_000_000_000;
/// 每百万 Token 单价 × Token 数 → 纳单位金额的系数（10⁹ / 10⁶）。
const NANOS_SCALE_PER_MILLION_TOKENS: i64 = 1_000;
const CONTEXT_OVER_200K_THRESHOLD: u64 = 200_000;
const MILLIS_PER_SECOND: i64 = 1_000;
const SECONDS_PER_DAY: i64 = 86_400;

mod decimal_string {
    use std::str::FromStr;

    use rust_decimal::Decimal;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &Decimal, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Decimal, D::Error> {
        let value = String::deserialize(deserializer)?;
        Decimal::from_str(&value).map_err(serde::de::Error::custom)
    }
}

/// 一笔已冻结的双币种金额，单位为纳（10⁻⁹ 币种单位）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Amount {
    pub usd_nanos: i64,
    /// 已设定自定义币种时按当时汇率冻结的自定义币种金额。
    pub custom_nanos: Option<i64>,
}

impl Amount {
    pub const ZERO: Self = Self {
        usd_nanos: 0,
        custom_nanos: None,
    };

    /// 单笔记录的展示金额（币种单位）。有自定义金额时用它，否则美元 × 首次汇率。
    /// 换算溢出时返回 `None`，不伪造金额。
    pub fn display(self, currency: Option<&CustomCurrency>) -> Option<Decimal> {
        if self.usd_nanos < 0 || self.custom_nanos.is_some_and(|nanos| nanos < 0) {
            tracing::warn!("amount is negative");
            return None;
        }
        let nanos = match currency {
            None => Decimal::from(self.usd_nanos),
            Some(currency) => match self.custom_nanos {
                Some(custom_nanos) => Decimal::from(custom_nanos),
                None => mul_checked(Decimal::from(self.usd_nanos), currency.first_rate)?,
            },
        };
        Some(round_away(nanos) / Decimal::from(NANOS_PER_UNIT))
    }

    /// 单笔记录的展示金额（纳单位）；溢出时返回 `None`。
    pub fn display_nanos(self, currency: Option<&CustomCurrency>) -> Option<i64> {
        if self.usd_nanos < 0 || self.custom_nanos.is_some_and(|nanos| nanos < 0) {
            tracing::warn!("amount is negative");
            return None;
        }
        match currency {
            None => Some(self.usd_nanos),
            Some(currency) => match self.custom_nanos {
                Some(custom_nanos) => Some(custom_nanos),
                None => round_away(mul_checked(
                    Decimal::from(self.usd_nanos),
                    currency.first_rate,
                )?)
                .to_i64(),
            },
        }
    }
}

/// 实例唯一的自定义币种。汇率表示「1 美元 = X 自定义币种」。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomCurrency {
    pub symbol: String,
    #[serde(with = "decimal_string")]
    pub usd_rate: Decimal,
    /// 首次设定时的汇率，写入后永不改变。
    #[serde(with = "decimal_string")]
    pub first_rate: Decimal,
    pub configured_at: String,
}

impl CustomCurrency {
    pub fn validate(&self) -> anyhow::Result<()> {
        let symbol = self.symbol.trim();
        if symbol.is_empty() || symbol.chars().count() > 8 || symbol.chars().any(char::is_control) {
            anyhow::bail!("custom currency symbol must be 1-8 characters after trimming");
        }
        if self.usd_rate <= Decimal::ZERO {
            anyhow::bail!("custom currency rate must be greater than 0");
        }
        if self.first_rate <= Decimal::ZERO {
            anyhow::bail!("custom currency first rate must be greater than 0");
        }
        if DateTime::parse_from_rfc3339(&self.configured_at).is_err() {
            anyhow::bail!("custom currency configured_at must be an RFC 3339 timestamp");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CurrencyKind {
    Usd,
    Custom,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostBasis {
    #[default]
    Usage,
    Subscription,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionPeriod {
    Month,
    Year,
}

/// `ModelCost::custom_currency_fields` 的取值。`Tiers` 同时覆盖
/// `context_over_200k` 与全部分档。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceField {
    Input,
    Output,
    CacheRead,
    CacheWrite,
    Tiers,
}

impl PriceField {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
            Self::CacheRead => "cache_read",
            Self::CacheWrite => "cache_write",
            Self::Tiers => "tiers",
        }
    }
}

/// 至多一条峰谷规则。时段为本地时间 `HH:MM`，起点含、终点不含，终点可为 `24:00`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeakPricing {
    pub timezone: String,
    /// ISO 星期，1 = 周一。
    pub weekdays: Vec<u8>,
    pub ranges: Vec<PeakWindow>,
    #[serde(with = "decimal_string")]
    pub multiplier: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeakWindow {
    pub start: String,
    pub end: String,
}

impl PeakPricing {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.timezone.trim().is_empty() || Tz::from_str(&self.timezone).is_err() {
            anyhow::bail!("peak pricing timezone is not a valid IANA timezone");
        }
        if self.weekdays.is_empty() {
            anyhow::bail!("peak pricing requires at least one weekday");
        }
        let mut seen_weekdays = [false; 8];
        for weekday in &self.weekdays {
            if !(1..=7).contains(weekday) {
                anyhow::bail!("peak pricing weekdays must be ISO weekdays 1-7");
            }
            if std::mem::replace(&mut seen_weekdays[*weekday as usize], true) {
                anyhow::bail!("peak pricing weekdays must not repeat");
            }
        }
        if self.ranges.is_empty() {
            anyhow::bail!("peak pricing requires at least one time window");
        }
        let mut windows = Vec::with_capacity(self.ranges.len());
        for range in &self.ranges {
            let Some(start) = parse_hhmm(&range.start, false) else {
                anyhow::bail!("peak pricing windows must use HH:MM");
            };
            let Some(end) = parse_hhmm(&range.end, true) else {
                anyhow::bail!("peak pricing windows must use HH:MM");
            };
            if start >= end {
                anyhow::bail!("peak pricing windows must not cross midnight");
            }
            windows.push((start, end));
        }
        windows.sort_unstable();
        for pair in windows.windows(2) {
            if pair[0].1 > pair[1].0 {
                anyhow::bail!("peak pricing windows must not overlap");
            }
        }
        if self.multiplier <= Decimal::ZERO {
            anyhow::bail!("peak pricing multiplier must be greater than 0");
        }
        Ok(())
    }

    /// 尝试开始时刻（UTC）换算到规则时区后是否命中峰谷时段。
    pub fn matches(&self, started_at: DateTime<Utc>) -> bool {
        let Ok(timezone) = Tz::from_str(&self.timezone) else {
            return false;
        };
        let local = started_at.with_timezone(&timezone);
        let weekday = local.weekday().number_from_monday() as u8;
        if !self.weekdays.contains(&weekday) {
            return false;
        }
        let minutes = local.hour() * 60 + local.minute();
        self.ranges.iter().any(|range| {
            let (Some(start), Some(end)) = (
                parse_hhmm(&range.start, false),
                parse_hhmm(&range.end, true),
            ) else {
                return false;
            };
            u32::from(start) <= minutes && minutes < u32::from(end)
        })
    }
}

fn parse_hhmm(value: &str, allow_end_of_day: bool) -> Option<u16> {
    let bytes = value.as_bytes();
    if bytes.len() != 5 || bytes[2] != b':' {
        return None;
    }
    if !bytes[..2].iter().all(u8::is_ascii_digit) || !bytes[3..].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let hour = u16::from(bytes[0] - b'0') * 10 + u16::from(bytes[1] - b'0');
    let minute = u16::from(bytes[3] - b'0') * 10 + u16::from(bytes[4] - b'0');
    if minute >= 60 {
        return None;
    }
    if hour == 24 {
        return (allow_end_of_day && minute == 0).then_some(24 * 60);
    }
    if hour > 23 {
        return None;
    }
    Some(hour * 60 + minute)
}

/// Route 计费设置；`models.billing_json` 为 `NULL` 表示不计费。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum BillingConfig {
    PerCall {
        #[serde(with = "decimal_string")]
        amount: Decimal,
        currency: CurrencyKind,
    },
    PerToken {
        price: std::sync::Arc<ModelCost>,
    },
}

impl BillingConfig {
    pub fn validate(&self, currency: Option<&CustomCurrency>) -> anyhow::Result<()> {
        match self {
            Self::PerCall {
                amount,
                currency: kind,
            } => {
                if amount.is_sign_negative() {
                    anyhow::bail!("billing amount must be non-negative");
                }
                if *kind == CurrencyKind::Custom && currency.is_none() {
                    anyhow::bail!(
                        "billing in custom currency requires a configured custom currency"
                    );
                }
            }
            Self::PerToken { price } => {
                price.validate()?;
                price.validate_currency(currency)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", content = "amount", rename_all = "snake_case")]
pub enum BillingOutcome {
    Charged(Amount),
    Unknown,
}

/// 一次 Target 尝试结束时要写入观测的成本与计费事实。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttemptAmounts {
    pub cost_basis: CostBasis,
    /// `None` = 成本未知。
    pub cost: Option<Amount>,
    /// `None` = 不计费。
    pub billing: Option<BillingOutcome>,
}

/// 订阅生效段。传入完整历史（含 `usage` 记录）并按 `effective_from` 升序排列；
/// 每段止于下一条记录的生效时间，最新一段止于 `now`。
#[derive(Debug, Clone, PartialEq)]
pub struct CostPlanEntry {
    pub effective_from: DateTime<Utc>,
    pub basis: CostBasis,
    pub period: Option<SubscriptionPeriod>,
    pub price: Option<Amount>,
}

/// 订阅摊销结果，按展示口径拆成两列，单位为纳。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubscriptionCost {
    /// 带有冻结自定义金额的订阅段之和。
    pub custom_nanos: Decimal,
    /// 没有冻结自定义金额的订阅段的美元金额之和。
    pub usd_unconverted_nanos: Decimal,
}

impl SubscriptionCost {
    /// 展示金额（纳单位），按首次汇率换算并四舍五入（远离零）一次。
    /// 换算溢出时返回 `None`，不伪造金额。
    pub fn display_nanos(&self, currency: Option<&CustomCurrency>) -> Option<Decimal> {
        display_nanos_amount(self.custom_nanos, self.usd_unconverted_nanos, currency)
    }

    /// 展示金额（币种单位）。
    pub fn display(&self, currency: Option<&CustomCurrency>) -> Option<Decimal> {
        Some(self.display_nanos(currency)? / Decimal::from(NANOS_PER_UNIT))
    }
}

/// 估算一次上游尝试的按量成本。未知用量或缺少所需价格时返回 `None`。
pub fn estimate_cost(
    cost: &ModelCost,
    usage: &ConfirmedUsage,
    started_at: DateTime<Utc>,
    currency: Option<&CustomCurrency>,
) -> Option<Amount> {
    let input = known_tokens(usage.input_tokens)?;
    let output = known_tokens(usage.output_tokens)?;
    let cache_read = known_tokens(usage.cache_read_tokens).unwrap_or(0);
    let cache_write = known_tokens(usage.cache_write_tokens).unwrap_or(0);
    let normal_input = input.saturating_sub(cache_read).saturating_sub(cache_write);

    let prices = select_prices(cost, input);
    let tier_selected = !matches!(&prices, SelectedPrices::Base(_));
    let multiplier = cost
        .peak
        .as_ref()
        .filter(|peak| peak.matches(started_at))
        .map(|peak| peak.multiplier)
        .unwrap_or(Decimal::ONE);
    if multiplier <= Decimal::ZERO {
        tracing::warn!("peak pricing multiplier is not positive");
        return None;
    }

    let mut usd_parts = Decimal::ZERO;
    let mut custom_parts = Decimal::ZERO;
    for (tokens, field) in [
        (normal_input, PriceField::Input),
        (output, PriceField::Output),
        (cache_read, PriceField::CacheRead),
        (cache_write, PriceField::CacheWrite),
    ] {
        if tokens == 0 {
            continue;
        }
        let unit_price = prices.price(field)?;
        if unit_price.is_sign_negative() {
            return None;
        }
        let nanos = mul_checked(Decimal::from(tokens), unit_price)?;
        let nanos = mul_checked(nanos, Decimal::from(NANOS_SCALE_PER_MILLION_TOKENS))?;
        let custom = if tier_selected {
            cost.custom_currency_fields.contains(&PriceField::Tiers)
        } else {
            cost.custom_currency_fields.contains(&field)
        };
        if custom {
            custom_parts = add_checked(custom_parts, nanos)?;
        } else {
            usd_parts = add_checked(usd_parts, nanos)?;
        }
    }

    if multiplier != Decimal::ONE {
        usd_parts = mul_checked(usd_parts, multiplier)?;
        custom_parts = mul_checked(custom_parts, multiplier)?;
    }

    let (usd_nanos, custom_nanos) = convert_parts(
        usd_parts,
        custom_parts,
        currency,
        !cost.custom_currency_fields.is_empty(),
    )?;
    Some(Amount {
        usd_nanos,
        custom_nanos,
    })
}

/// 计算一次已完成尝试的 Route 计费；不计费由调用方以 `None` 表示。
pub fn bill(
    billing: &BillingConfig,
    usage: Option<&ConfirmedUsage>,
    started_at: DateTime<Utc>,
    currency: Option<&CustomCurrency>,
) -> BillingOutcome {
    match billing {
        BillingConfig::PerCall {
            amount,
            currency: kind,
        } => match freeze_amount(*amount, *kind, currency) {
            Some(amount) => BillingOutcome::Charged(amount),
            None => {
                tracing::warn!("per-call billing amount could not be converted");
                BillingOutcome::Unknown
            }
        },
        BillingConfig::PerToken { price } => {
            match usage.and_then(|usage| estimate_cost(price, usage, started_at, currency)) {
                Some(amount) => BillingOutcome::Charged(amount),
                None => BillingOutcome::Unknown,
            }
        }
    }
}

/// Freeze a price entered in one currency into both sides. The entered side is
/// exact; the other side uses the current rate and rounds once.
pub fn freeze_amount(
    amount: Decimal,
    kind: CurrencyKind,
    currency: Option<&CustomCurrency>,
) -> Option<Amount> {
    if amount.is_sign_negative() {
        return None;
    }
    if currency.is_some_and(|currency| currency.usd_rate <= Decimal::ZERO) {
        return None;
    }
    let units = mul_checked(amount, Decimal::from(NANOS_PER_UNIT))?;
    match kind {
        CurrencyKind::Usd => {
            let usd_nanos = round_to_i64(units)?;
            let custom_nanos = match currency {
                Some(currency) => Some(round_to_i64(mul_checked(units, currency.usd_rate)?)?),
                None => None,
            };
            Some(Amount {
                usd_nanos,
                custom_nanos,
            })
        }
        CurrencyKind::Custom => {
            let currency = currency?;
            if currency.usd_rate <= Decimal::ZERO {
                return None;
            }
            let custom_nanos = round_to_i64(units)?;
            let usd_nanos = round_to_i64(div_checked(units, currency.usd_rate)?)?;
            Some(Amount {
                usd_nanos,
                custom_nanos: Some(custom_nanos),
            })
        }
    }
}

/// 订阅成本按生效段与查询窗口的重叠时长线性摊销。
///
/// 月按 365/12 天、年按 365 天；最新一段止于 `now`。各段先在十进制中精确累加，
/// 展示时只取整一次。
pub fn subscription_amortization(
    entries: &[CostPlanEntry],
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Option<SubscriptionCost> {
    let mut total = SubscriptionCost::default();
    for (index, entry) in entries.iter().enumerate() {
        if entry.basis != CostBasis::Subscription {
            continue;
        }
        let (Some(period), Some(price)) = (entry.period, entry.price) else {
            continue;
        };
        if price.usd_nanos < 0 || price.custom_nanos.is_some_and(|nanos| nanos < 0) {
            tracing::warn!("subscription price is negative");
            return None;
        }
        let segment_end = entries
            .get(index + 1)
            .map(|next| next.effective_from)
            .unwrap_or(now)
            .min(now);
        let overlap_start = entry.effective_from.max(window_start);
        let overlap_end = segment_end.min(window_end);
        if overlap_end <= overlap_start {
            continue;
        }
        let overlap_millis = overlap_end
            .signed_duration_since(overlap_start)
            .num_milliseconds();
        if overlap_millis <= 0 {
            continue;
        }
        let period_millis = period_millis(period);
        let Some(fraction) =
            div_checked(Decimal::from(overlap_millis), Decimal::from(period_millis))
        else {
            continue;
        };
        let scaled = if let Some(custom_nanos) = price.custom_nanos {
            mul_checked(Decimal::from(custom_nanos), fraction).map(|value| (true, value))
        } else {
            mul_checked(Decimal::from(price.usd_nanos), fraction).map(|value| (false, value))
        };
        let Some((is_custom, value)) = scaled else {
            tracing::warn!("subscription amortization overflowed");
            return None;
        };
        if is_custom {
            total.custom_nanos = add_checked(total.custom_nanos, value)?;
        } else {
            total.usd_unconverted_nanos = add_checked(total.usd_unconverted_nanos, value)?;
        }
    }
    Some(total)
}

fn period_millis(period: SubscriptionPeriod) -> i64 {
    match period {
        SubscriptionPeriod::Month => 365 * MILLIS_PER_SECOND * SECONDS_PER_DAY / 12,
        SubscriptionPeriod::Year => 365 * MILLIS_PER_SECOND * SECONDS_PER_DAY,
    }
}

/// 把填写币种的价格换算成美元；自定义币种价格在未设定币种时不可用。
pub fn usd_price(
    value: Decimal,
    custom: bool,
    currency: Option<&CustomCurrency>,
) -> Option<Decimal> {
    if !custom {
        return Some(value);
    }
    let currency = currency?;
    if currency.usd_rate <= Decimal::ZERO {
        return None;
    }
    div_checked(value, currency.usd_rate)
}

/// 统计桶的展示金额（纳单位）：自定义金额 + 未换算美元 × 首次汇率，只取整一次。
pub fn display_nanos(
    custom_nanos: i64,
    usd_unconverted_nanos: i64,
    currency: Option<&CustomCurrency>,
) -> Option<i64> {
    display_nanos_amount(
        Decimal::from(custom_nanos),
        Decimal::from(usd_unconverted_nanos),
        currency,
    )?
    .to_i64()
}

/// 统计桶的展示金额（币种单位）。换算溢出时返回 `None`，不伪造金额。
pub fn display_decimal(
    custom_nanos: i64,
    usd_unconverted_nanos: i64,
    currency: Option<&CustomCurrency>,
) -> Option<Decimal> {
    display_decimal_amount(
        Decimal::from(custom_nanos),
        Decimal::from(usd_unconverted_nanos),
        currency,
    )
}

/// 统计桶的展示金额（币种单位），输入为精确的 Decimal 纳单位。
/// 未设定自定义币种时只用美元列；已设定时自定义列 + 美元列 × 首次汇率。
/// 负数或溢出返回 `None`；最终只按「中点远离零」取整一次。
pub fn display_decimal_amount(
    custom_nanos: Decimal,
    usd_unconverted_nanos: Decimal,
    currency: Option<&CustomCurrency>,
) -> Option<Decimal> {
    Some(
        display_nanos_amount(custom_nanos, usd_unconverted_nanos, currency)?
            / Decimal::from(NANOS_PER_UNIT),
    )
}

fn display_nanos_amount(
    custom_nanos: Decimal,
    usd_unconverted_nanos: Decimal,
    currency: Option<&CustomCurrency>,
) -> Option<Decimal> {
    if custom_nanos.is_sign_negative() || usd_unconverted_nanos.is_sign_negative() {
        tracing::warn!("display amount is negative");
        return None;
    }
    let total = match currency {
        Some(currency) => {
            if currency.first_rate <= Decimal::ZERO {
                return None;
            }
            add_checked(
                custom_nanos,
                mul_checked(usd_unconverted_nanos, currency.first_rate)?,
            )?
        }
        None => usd_unconverted_nanos,
    };
    Some(round_away(total))
}

/// 把币种单位的十进制金额换算成纳单位，最终只按「中点远离零」取整一次。
/// 负数或溢出返回 `None`。
pub fn decimal_to_nanos(value: Decimal) -> Option<i64> {
    if value.is_sign_negative() {
        return None;
    }
    round_to_i64(mul_checked(value, Decimal::from(NANOS_PER_UNIT))?)
}

/// 金额的十进制字符串；纳单位转成最多 9 位小数并去除尾零。
pub fn nanos_to_decimal_string(nanos: i64) -> String {
    format_amount(Decimal::new(nanos, 9))
}

/// 规范化十进制字符串，去除尾零；零输出 `"0"`。
pub fn format_amount(value: Decimal) -> String {
    value.normalize().to_string()
}

/// 管理 API 的金额展示：金额、已知计数与未知计数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayMoney {
    pub amount: String,
    pub known_count: u64,
    pub unknown_count: u64,
}

impl Default for DisplayMoney {
    fn default() -> Self {
        Self {
            amount: "0".to_owned(),
            known_count: 0,
            unknown_count: 0,
        }
    }
}

impl DisplayMoney {
    pub fn from_totals(
        custom_nanos: i64,
        usd_unconverted_nanos: i64,
        known_count: u64,
        unknown_count: u64,
        currency: Option<&CustomCurrency>,
    ) -> Option<Self> {
        Self::from_decimal_totals(
            Decimal::from(custom_nanos),
            Decimal::from(usd_unconverted_nanos),
            known_count,
            unknown_count,
            currency,
        )
    }

    /// 与 [`Self::from_totals`] 相同，但接受订阅摊销产生的精确 Decimal 纳单位。
    pub fn from_decimal_totals(
        custom_nanos: Decimal,
        usd_unconverted_nanos: Decimal,
        known_count: u64,
        unknown_count: u64,
        currency: Option<&CustomCurrency>,
    ) -> Option<Self> {
        Some(Self {
            amount: format_amount(display_decimal_amount(
                custom_nanos,
                usd_unconverted_nanos,
                currency,
            )?),
            known_count,
            unknown_count,
        })
    }
}

/// 展示币种：未设定自定义币种时为美元。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayCurrency {
    pub kind: CurrencyKind,
    pub symbol: String,
}

impl Default for DisplayCurrency {
    fn default() -> Self {
        Self {
            kind: CurrencyKind::Usd,
            symbol: "$".to_owned(),
        }
    }
}

impl DisplayCurrency {
    pub fn from_snapshot(currency: Option<&CustomCurrency>) -> Self {
        match currency {
            Some(currency) => Self {
                kind: CurrencyKind::Custom,
                symbol: currency.symbol.clone(),
            },
            None => Self::default(),
        }
    }
}

/// 目录导入与模型发现得到的价格必须去掉只属于管理写入的本地字段。
pub fn strip_source_local_cost_fields(cost: &mut Option<ModelCost>) {
    if let Some(cost) = cost {
        cost.custom_currency_fields.clear();
        cost.peak = None;
    }
}

fn known_tokens(value: Option<i64>) -> Option<u64> {
    value.and_then(|value| u64::try_from(value).ok())
}

enum SelectedPrices<'a> {
    Base(&'a crate::provider_models::PriceComponents),
    Context(&'a crate::provider_models::PriceComponents),
    Tier(&'a crate::provider_models::PriceComponents),
}

impl SelectedPrices<'_> {
    fn price(&self, field: PriceField) -> Option<Decimal> {
        let prices = match self {
            Self::Base(prices) | Self::Context(prices) | Self::Tier(prices) => prices,
        };
        match field {
            PriceField::Input => prices.input,
            PriceField::Output => prices.output,
            PriceField::CacheRead => prices.cache_read,
            PriceField::CacheWrite => prices.cache_write,
            PriceField::Tiers => None,
        }
    }
}

fn select_prices(cost: &ModelCost, input_tokens: u64) -> SelectedPrices<'_> {
    let mut selected = SelectedPrices::Base(&cost.prices);
    let mut best_threshold = None;
    if let Some(prices) = &cost.context_over_200k
        && input_tokens > CONTEXT_OVER_200K_THRESHOLD
    {
        selected = SelectedPrices::Context(prices);
        best_threshold = Some(CONTEXT_OVER_200K_THRESHOLD);
    }
    for tier in &cost.tiers {
        let threshold = tier.tier.size;
        if threshold < input_tokens && best_threshold.is_none_or(|best| threshold >= best) {
            selected = SelectedPrices::Tier(&tier.prices);
            best_threshold = Some(threshold);
        }
    }
    selected
}

fn convert_parts(
    usd_parts: Decimal,
    custom_parts: Decimal,
    currency: Option<&CustomCurrency>,
    has_custom_fields: bool,
) -> Option<(i64, Option<i64>)> {
    if let Some(currency) = currency {
        if currency.usd_rate <= Decimal::ZERO {
            tracing::warn!("custom currency rate is not positive");
            return None;
        }
        let usd_side = add_checked(usd_parts, div_checked(custom_parts, currency.usd_rate)?)?;
        let custom_side = add_checked(custom_parts, mul_checked(usd_parts, currency.usd_rate)?)?;
        Some((round_to_i64(usd_side)?, Some(round_to_i64(custom_side)?)))
    } else {
        if has_custom_fields {
            tracing::warn!("custom currency price fields require a configured custom currency");
            return None;
        }
        Some((round_to_i64(usd_parts)?, None))
    }
}

fn round_to_i64(value: Decimal) -> Option<i64> {
    let result = round_away(value).to_i64();
    if result.is_none() {
        tracing::warn!("cost amount overflowed i64 nanoseconds");
    }
    result
}

/// 金额取整只使用「中点远离零」，与设计文档一致。
fn round_away(value: Decimal) -> Decimal {
    value.round_dp_with_strategy(0, RoundingStrategy::MidpointAwayFromZero)
}

/// 非负 Decimal 乘法；溢出返回 `None`，避免 rust_decimal 的 panic 路径。
fn mul_checked(left: Decimal, right: Decimal) -> Option<Decimal> {
    debug_assert!(!left.is_sign_negative() && !right.is_sign_negative());
    left.checked_mul(right)
}

/// 非负 Decimal 加法；溢出返回 `None`。
fn add_checked(left: Decimal, right: Decimal) -> Option<Decimal> {
    debug_assert!(!left.is_sign_negative() && !right.is_sign_negative());
    left.checked_add(right)
}

/// 非负 Decimal 除法；溢出或除零返回 `None`。
fn div_checked(left: Decimal, right: Decimal) -> Option<Decimal> {
    debug_assert!(!left.is_sign_negative() && !right.is_sign_negative());
    left.checked_div(right)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_models::{
        ModelCost, ModelCostTier, ModelCostTierThreshold, PriceComponents,
    };
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn dec(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    fn usage(
        input: Option<i64>,
        output: Option<i64>,
        cache_read: Option<i64>,
        cache_write: Option<i64>,
    ) -> ConfirmedUsage {
        ConfirmedUsage {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
            reasoning_tokens: None,
            coverage: None,
        }
    }

    fn currency(usd_rate: &str, first_rate: &str) -> CustomCurrency {
        CustomCurrency {
            symbol: "¥".to_owned(),
            usd_rate: dec(usd_rate),
            first_rate: dec(first_rate),
            configured_at: "2026-10-28T08:00:00Z".to_owned(),
        }
    }

    fn cost(input: Option<&str>, output: Option<&str>) -> ModelCost {
        ModelCost {
            prices: PriceComponents {
                input: input.map(dec),
                output: output.map(dec),
                ..PriceComponents::default()
            },
            ..ModelCost::default()
        }
    }

    fn peak(ranges: &[(&str, &str)], weekdays: &[u8], multiplier: &str) -> PeakPricing {
        PeakPricing {
            timezone: "Asia/Shanghai".to_owned(),
            weekdays: weekdays.to_vec(),
            ranges: ranges
                .iter()
                .map(|(start, end)| PeakWindow {
                    start: (*start).to_owned(),
                    end: (*end).to_owned(),
                })
                .collect(),
            multiplier: dec(multiplier),
        }
    }

    fn utc(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn exact_threshold_uses_base_price() {
        let mut cost = cost(Some("1"), Some("0"));
        cost.context_over_200k = Some(PriceComponents {
            input: Some(dec("2")),
            ..PriceComponents::default()
        });
        let at = utc("2026-10-28T00:00:00Z");

        let at_threshold =
            estimate_cost(&cost, &usage(Some(200_000), Some(0), None, None), at, None)
                .expect("known cost");
        assert_eq!(at_threshold.usd_nanos, 200_000 * 1_000);

        let over_threshold =
            estimate_cost(&cost, &usage(Some(200_001), Some(0), None, None), at, None)
                .expect("known cost");
        assert_eq!(over_threshold.usd_nanos, 200_001 * 2_000);
    }

    #[test]
    fn largest_satisfied_tier_wins() {
        let mut cost = cost(Some("1"), Some("0"));
        cost.tiers = vec![
            ModelCostTier {
                tier: ModelCostTierThreshold {
                    kind: "context".to_owned(),
                    size: 100_000,
                },
                prices: PriceComponents {
                    input: Some(dec("2")),
                    ..PriceComponents::default()
                },
            },
            ModelCostTier {
                tier: ModelCostTierThreshold {
                    kind: "context".to_owned(),
                    size: 300_000,
                },
                prices: PriceComponents {
                    input: Some(dec("3")),
                    ..PriceComponents::default()
                },
            },
        ];
        let at = utc("2026-10-28T00:00:00Z");

        let middle = estimate_cost(&cost, &usage(Some(250_000), Some(0), None, None), at, None)
            .expect("known cost");
        assert_eq!(middle.usd_nanos, 250_000 * 2_000);

        let exactly_300k =
            estimate_cost(&cost, &usage(Some(300_000), Some(0), None, None), at, None)
                .expect("known cost");
        assert_eq!(exactly_300k.usd_nanos, 300_000 * 2_000);

        let over_300k = estimate_cost(&cost, &usage(Some(300_001), Some(0), None, None), at, None)
            .expect("known cost");
        assert_eq!(over_300k.usd_nanos, 300_001 * 3_000);
    }

    #[test]
    fn zero_token_components_need_no_price() {
        let at = utc("2026-10-28T00:00:00Z");
        let cost = ModelCost::default();
        let amount = estimate_cost(&cost, &usage(Some(0), Some(0), None, None), at, None)
            .expect("zero tokens are a known zero cost");
        assert_eq!(amount, Amount::ZERO);
    }

    #[test]
    fn missing_price_for_used_component_is_unknown() {
        let at = utc("2026-10-28T00:00:00Z");
        let cost = cost(Some("1"), None);
        assert!(estimate_cost(&cost, &usage(Some(10), Some(0), None, None), at, None).is_some());
        assert!(estimate_cost(&cost, &usage(Some(10), Some(1), None, None), at, None).is_none());
        assert!(estimate_cost(&cost, &usage(Some(10), Some(0), Some(5), None), at, None).is_none());
    }

    #[test]
    fn unknown_cache_tokens_are_priced_as_normal_input() {
        let at = utc("2026-10-28T00:00:00Z");
        let cost = cost(Some("1"), Some("0"));
        let amount =
            estimate_cost(&cost, &usage(Some(1_000), Some(0), None, None), at, None).unwrap();
        assert_eq!(amount.usd_nanos, 1_000 * 1_000);
    }

    #[test]
    fn unknown_input_or_output_is_unknown() {
        let at = utc("2026-10-28T00:00:00Z");
        let cost = cost(Some("1"), Some("2"));
        assert!(estimate_cost(&cost, &usage(None, Some(1), None, None), at, None).is_none());
        assert!(estimate_cost(&cost, &usage(Some(1), None, None, None), at, None).is_none());
    }

    #[test]
    fn all_components_sum_with_cache_prices() {
        let at = utc("2026-10-28T00:00:00Z");
        let mut cost = cost(Some("1"), Some("2"));
        cost.prices.cache_read = Some(dec("3"));
        cost.prices.cache_write = Some(dec("4"));
        let amount = estimate_cost(
            &cost,
            &usage(Some(1_000), Some(100), Some(300), Some(200)),
            at,
            None,
        )
        .unwrap();
        let expected = (500 + 100 * 2 + 300 * 3 + 200 * 4) * 1_000;
        assert_eq!(amount.usd_nanos, expected);
    }

    #[test]
    fn cache_tokens_exceeding_input_do_not_underflow() {
        let at = utc("2026-10-28T00:00:00Z");
        let mut cost = cost(Some("1"), Some("0"));
        cost.prices.cache_read = Some(dec("3"));
        cost.prices.cache_write = Some(dec("4"));
        let amount = estimate_cost(
            &cost,
            &usage(Some(100), Some(0), Some(90), Some(90)),
            at,
            None,
        )
        .unwrap();
        assert_eq!(amount.usd_nanos, (90 * 3 + 90 * 4) * 1_000);
    }

    #[test]
    fn peak_window_is_start_inclusive_and_end_exclusive() {
        let mut cost = cost(Some("1"), Some("0"));
        cost.peak = Some(peak(&[("09:00", "12:00")], &[1, 2, 3, 4, 5], "2"));

        let at_start = utc("2026-10-28T01:00:00Z"); // 09:00 Asia/Shanghai
        let before_end = utc("2026-10-28T03:59:00Z"); // 11:59
        let at_end = utc("2026-10-28T04:00:00Z"); // 12:00
        let before_start = utc("2026-10-28T00:59:00Z"); // 08:59

        for at in [at_start, before_end] {
            let amount =
                estimate_cost(&cost, &usage(Some(1_000), Some(0), None, None), at, None).unwrap();
            assert_eq!(amount.usd_nanos, 2_000_000);
        }
        for at in [at_end, before_start] {
            let amount =
                estimate_cost(&cost, &usage(Some(1_000), Some(0), None, None), at, None).unwrap();
            assert_eq!(amount.usd_nanos, 1_000_000);
        }
    }

    #[test]
    fn peak_weekday_filter_uses_local_time() {
        let mut cost = cost(Some("1"), Some("0"));
        cost.peak = Some(peak(&[("09:00", "12:00")], &[1, 2, 3, 4, 5], "2"));
        // 2026-10-31 is Saturday in Asia/Shanghai.
        let saturday = utc("2026-10-31T01:00:00Z");
        let amount = estimate_cost(
            &cost,
            &usage(Some(1_000), Some(0), None, None),
            saturday,
            None,
        )
        .unwrap();
        assert_eq!(amount.usd_nanos, 1_000_000);
    }

    #[test]
    fn peak_respects_daylight_saving_time() {
        let mut cost = cost(Some("1"), Some("0"));
        cost.peak = Some(PeakPricing {
            timezone: "America/New_York".to_owned(),
            weekdays: vec![1],
            ranges: vec![PeakWindow {
                start: "09:00".to_owned(),
                end: "10:00".to_owned(),
            }],
            multiplier: dec("2"),
        });

        // Winter: 14:00 UTC = 09:00 EST.
        let winter = utc("2026-01-05T14:00:00Z");
        // Summer: 13:00 UTC = 09:00 EDT, 14:00 UTC = 10:00 EDT.
        let summer_match = utc("2026-07-06T13:00:00Z");
        let summer_miss = utc("2026-07-06T14:00:00Z");

        for at in [winter, summer_match] {
            let amount =
                estimate_cost(&cost, &usage(Some(1_000), Some(0), None, None), at, None).unwrap();
            assert_eq!(amount.usd_nanos, 2_000_000);
        }
        let amount = estimate_cost(
            &cost,
            &usage(Some(1_000), Some(0), None, None),
            summer_miss,
            None,
        )
        .unwrap();
        assert_eq!(amount.usd_nanos, 1_000_000);
    }

    #[test]
    fn peak_end_of_day_window_excludes_midnight() {
        let mut cost = cost(Some("1"), Some("0"));
        cost.peak = Some(peak(&[("09:00", "24:00")], &[1, 2, 3, 4, 5, 6, 7], "2"));
        let at_2359 = utc("2026-10-28T15:59:00Z"); // 23:59 Asia/Shanghai
        let at_midnight = utc("2026-10-28T16:00:00Z"); // 00:00 next day
        assert_eq!(
            estimate_cost(
                &cost,
                &usage(Some(1_000), Some(0), None, None),
                at_2359,
                None
            )
            .unwrap()
            .usd_nanos,
            2_000_000
        );
        assert_eq!(
            estimate_cost(
                &cost,
                &usage(Some(1_000), Some(0), None, None),
                at_midnight,
                None
            )
            .unwrap()
            .usd_nanos,
            1_000_000
        );
    }

    #[test]
    fn peak_multiplier_applies_to_all_components_and_tiers() {
        let mut cost = cost(Some("1"), Some("2"));
        cost.prices.cache_read = Some(dec("3"));
        cost.prices.cache_write = Some(dec("4"));
        cost.peak = Some(peak(&[("00:00", "24:00")], &[1, 2, 3, 4, 5, 6, 7], "2"));
        let amount = estimate_cost(
            &cost,
            &usage(Some(1_000), Some(100), Some(300), Some(200)),
            utc("2026-10-28T00:00:00Z"),
            None,
        )
        .unwrap();
        assert_eq!(amount.usd_nanos, 2 * (500 + 200 + 900 + 800) * 1_000);
    }

    #[test]
    fn mixed_currency_prices_convert_each_side_once() {
        let mut cost = cost(Some("7"), Some("2"));
        cost.custom_currency_fields = vec![PriceField::Input];
        let currency = currency("7.1", "7.2");
        let amount = estimate_cost(
            &cost,
            &usage(Some(1_000_000), Some(1_000_000), None, None),
            utc("2026-10-28T00:00:00Z"),
            Some(&currency),
        )
        .unwrap();

        // USD side: 2e9 nanos + 7e9 / 7.1 = 2_985_915_492.9577... -> 2_985_915_493.
        assert_eq!(amount.usd_nanos, 2_985_915_493);
        // Custom side: 7e9 + 2e9 * 7.1 = 21_200_000_000.
        assert_eq!(amount.custom_nanos, Some(21_200_000_000));
    }

    #[test]
    fn custom_price_without_snapshot_is_unknown() {
        let mut cost = cost(Some("1"), Some("0"));
        cost.custom_currency_fields = vec![PriceField::Input];
        let amount = estimate_cost(
            &cost,
            &usage(Some(0), Some(0), None, None),
            utc("2026-10-28T00:00:00Z"),
            None,
        );
        assert!(amount.is_none());
    }

    #[test]
    fn custom_tiers_marker_applies_to_context_prices() {
        let mut cost = cost(Some("1"), Some("0"));
        cost.context_over_200k = Some(PriceComponents {
            input: Some(dec("7")),
            ..PriceComponents::default()
        });
        cost.custom_currency_fields = vec![PriceField::Tiers];
        let currency = currency("7.1", "7.1");
        let amount = estimate_cost(
            &cost,
            &usage(Some(200_001), Some(0), None, None),
            utc("2026-10-28T00:00:00Z"),
            Some(&currency),
        )
        .unwrap();
        assert_eq!(amount.custom_nanos, Some(200_001 * 7_000));
        let expected_usd = round_away(dec("1400007000") / dec("7.1")).to_i64().unwrap();
        assert_eq!(amount.usd_nanos, expected_usd);
    }

    #[test]
    fn display_conversion_uses_first_rate_for_unfrozen_usd() {
        let currency = currency("7.1", "7.2");
        let amount = Amount {
            usd_nanos: 1_000_000_000,
            custom_nanos: None,
        };
        assert_eq!(amount.display(Some(&currency)), Some(dec("7.2")));
        assert_eq!(amount.display_nanos(Some(&currency)), Some(7_200_000_000));

        let frozen = Amount {
            usd_nanos: 1_000_000_000,
            custom_nanos: Some(6_000_000_000),
        };
        assert_eq!(frozen.display(Some(&currency)), Some(dec("6")));

        assert_eq!(
            display_nanos(7_000_000_000, 1_000_000_000, Some(&currency)),
            Some(14_200_000_000)
        );
        assert_eq!(
            display_decimal(7_000_000_000, 1_000_000_000, None),
            Some(dec("1"))
        );
    }

    #[test]
    fn freeze_amount_rounds_half_away_from_zero_once() {
        assert_eq!(
            freeze_amount(dec("0.0000000005"), CurrencyKind::Usd, None),
            Some(Amount {
                usd_nanos: 1,
                custom_nanos: None,
            })
        );
    }

    #[test]
    fn display_rounds_half_away_from_zero_once() {
        let currency = currency("0.5", "0.5");
        let amount = Amount {
            usd_nanos: 1,
            custom_nanos: None,
        };
        assert_eq!(amount.display_nanos(Some(&currency)), Some(1));
        assert_eq!(display_nanos(0, 1, Some(&currency)), Some(1));
    }

    #[test]
    fn display_overflow_is_unknown_not_saturated() {
        let currency = CustomCurrency {
            symbol: "¥".to_owned(),
            usd_rate: dec("1"),
            first_rate: dec("100000000000000000000"),
            configured_at: "2026-10-28T08:00:00Z".to_owned(),
        };
        assert!(display_decimal(i64::MAX, i64::MAX, Some(&currency)).is_none());
        assert!(DisplayMoney::from_totals(i64::MAX, i64::MAX, 1, 0, Some(&currency)).is_none());
        assert!(
            Amount {
                usd_nanos: i64::MAX,
                custom_nanos: None,
            }
            .display(Some(&currency))
            .is_none()
        );
    }

    #[test]
    fn money_formatting_trims_zeroes() {
        assert_eq!(nanos_to_decimal_string(0), "0");
        assert_eq!(nanos_to_decimal_string(1_000_000_000), "1");
        assert_eq!(nanos_to_decimal_string(1_500_000_000), "1.5");
        assert_eq!(nanos_to_decimal_string(1), "0.000000001");
        let money = DisplayMoney::from_totals(
            7_000_000_000,
            1_000_000_000,
            3,
            2,
            Some(&currency("7.1", "7.2")),
        )
        .unwrap();
        assert_eq!(money.amount, "14.2");
        assert_eq!(money.known_count, 3);
        assert_eq!(money.unknown_count, 2);
        assert_eq!(DisplayMoney::default().amount, "0");
        assert_eq!(DisplayCurrency::default().symbol, "$");
        assert_eq!(
            DisplayCurrency::from_snapshot(Some(&currency("7.1", "7.2"))).symbol,
            "¥"
        );
    }

    #[test]
    fn overflow_is_unknown_instead_of_panicking() {
        let mut cost = cost(Some("100000000000000000000"), Some("0"));
        cost.prices.input = Some(dec("100000000000000000000"));
        let amount = estimate_cost(
            &cost,
            &usage(Some(i64::MAX), Some(0), None, None),
            utc("2026-10-28T00:00:00Z"),
            None,
        );
        assert!(amount.is_none());
    }

    #[test]
    fn billing_per_call_converts_both_currencies() {
        let at = utc("2026-10-28T00:00:00Z");
        let currency = currency("7.1", "7.2");
        let usd = BillingConfig::PerCall {
            amount: dec("0.02"),
            currency: CurrencyKind::Usd,
        };
        assert_eq!(
            bill(&usd, None, at, Some(&currency)),
            BillingOutcome::Charged(Amount {
                usd_nanos: 20_000_000,
                custom_nanos: Some(142_000_000),
            })
        );

        let custom = BillingConfig::PerCall {
            amount: dec("7.1"),
            currency: CurrencyKind::Custom,
        };
        assert_eq!(
            bill(&custom, None, at, Some(&currency)),
            BillingOutcome::Charged(Amount {
                usd_nanos: 1_000_000_000,
                custom_nanos: Some(7_100_000_000),
            })
        );

        assert_eq!(bill(&custom, None, at, None), BillingOutcome::Unknown);
    }

    #[test]
    fn billing_per_token_reuses_usage_pricing() {
        let at = utc("2026-10-28T00:00:00Z");
        let billing = BillingConfig::PerToken {
            price: std::sync::Arc::new(cost(Some("1"), Some("2"))),
        };
        assert_eq!(
            bill(
                &billing,
                Some(&usage(Some(1_000), Some(0), None, None)),
                at,
                None
            ),
            BillingOutcome::Charged(Amount {
                usd_nanos: 1_000_000,
                custom_nanos: None,
            })
        );
        assert_eq!(bill(&billing, None, at, None), BillingOutcome::Unknown);
        assert_eq!(
            bill(&billing, Some(&usage(None, Some(0), None, None)), at, None),
            BillingOutcome::Unknown
        );
    }

    #[test]
    fn subscription_amortizes_partial_window() {
        let entries = vec![CostPlanEntry {
            effective_from: utc("2026-01-01T00:00:00Z"),
            basis: CostBasis::Subscription,
            period: Some(SubscriptionPeriod::Month),
            price: Some(Amount {
                usd_nanos: 31_500_000_000,
                custom_nanos: None,
            }),
        }];
        let cost = subscription_amortization(
            &entries,
            utc("2026-01-01T00:00:00Z"),
            utc("2026-01-16T05:00:00Z"),
            utc("2026-01-20T00:00:00Z"),
        )
        .unwrap();
        assert_eq!(cost.usd_unconverted_nanos, dec("15750000000"));
        assert_eq!(cost.custom_nanos, Decimal::ZERO);
        assert_eq!(
            cost.display(Some(&currency("7.1", "7.2"))),
            Some(dec("113.4"))
        );
    }

    #[test]
    fn subscription_latest_segment_stops_at_now() {
        let entries = vec![CostPlanEntry {
            effective_from: utc("2026-01-01T00:00:00Z"),
            basis: CostBasis::Subscription,
            period: Some(SubscriptionPeriod::Month),
            price: Some(Amount {
                usd_nanos: 31_500_000_000,
                custom_nanos: None,
            }),
        }];
        let cost = subscription_amortization(
            &entries,
            utc("2026-01-01T00:00:00Z"),
            utc("2026-02-01T00:00:00Z"),
            utc("2026-01-16T05:00:00Z"),
        )
        .unwrap();
        assert_eq!(cost.usd_unconverted_nanos, dec("15750000000"));
    }

    #[test]
    fn subscription_segments_end_at_next_record_of_any_basis() {
        let entries = vec![
            CostPlanEntry {
                effective_from: utc("2026-01-01T00:00:00Z"),
                basis: CostBasis::Subscription,
                period: Some(SubscriptionPeriod::Month),
                price: Some(Amount {
                    usd_nanos: 31_500_000_000,
                    custom_nanos: None,
                }),
            },
            CostPlanEntry {
                effective_from: utc("2026-01-31T10:00:00Z"),
                basis: CostBasis::Subscription,
                period: Some(SubscriptionPeriod::Month),
                price: Some(Amount {
                    usd_nanos: 63_000_000_000,
                    custom_nanos: None,
                }),
            },
            CostPlanEntry {
                effective_from: utc("2026-03-02T20:00:00Z"),
                basis: CostBasis::Usage,
                period: None,
                price: None,
            },
        ];
        let cost = subscription_amortization(
            &entries,
            utc("2026-01-01T00:00:00Z"),
            utc("2026-03-02T20:00:00Z"),
            utc("2026-03-15T00:00:00Z"),
        )
        .unwrap();
        assert_eq!(cost.usd_unconverted_nanos, dec("94500000000"));
    }

    #[test]
    fn subscription_uses_frozen_custom_side() {
        let entries = vec![CostPlanEntry {
            effective_from: utc("2026-01-01T00:00:00Z"),
            basis: CostBasis::Subscription,
            period: Some(SubscriptionPeriod::Month),
            price: Some(Amount {
                usd_nanos: 2_100_000_000,
                custom_nanos: Some(15_000_000_000),
            }),
        }];
        let cost = subscription_amortization(
            &entries,
            utc("2026-01-01T00:00:00Z"),
            utc("2026-01-16T05:00:00Z"),
            utc("2026-01-20T00:00:00Z"),
        )
        .unwrap();
        assert_eq!(cost.custom_nanos, dec("7500000000"));
        assert_eq!(cost.usd_unconverted_nanos, Decimal::ZERO);
        assert_eq!(
            cost.display(Some(&currency("7.1", "7.2"))),
            Some(dec("7.5"))
        );
    }

    #[test]
    fn subscription_mixed_frozen_and_unfrozen_segments_keep_split() {
        let entries = vec![
            CostPlanEntry {
                effective_from: utc("2026-01-01T00:00:00Z"),
                basis: CostBasis::Subscription,
                period: Some(SubscriptionPeriod::Month),
                price: Some(Amount {
                    usd_nanos: 31_500_000_000,
                    custom_nanos: None,
                }),
            },
            CostPlanEntry {
                effective_from: utc("2026-01-31T10:00:00Z"),
                basis: CostBasis::Subscription,
                period: Some(SubscriptionPeriod::Month),
                price: Some(Amount {
                    usd_nanos: 1_000_000_000,
                    custom_nanos: Some(7_100_000_000),
                }),
            },
        ];
        let cost = subscription_amortization(
            &entries,
            utc("2026-01-01T00:00:00Z"),
            utc("2026-03-02T20:00:00Z"),
            utc("2026-03-02T20:00:00Z"),
        )
        .unwrap();
        assert_eq!(cost.usd_unconverted_nanos, dec("31500000000"));
        assert_eq!(cost.custom_nanos, dec("7100000000"));
        assert_eq!(
            cost.display(Some(&currency("7.1", "7.2"))),
            Some(dec("233.9"))
        );
    }

    #[test]
    fn subscription_window_before_history_is_zero() {
        let entries = vec![CostPlanEntry {
            effective_from: utc("2026-01-01T00:00:00Z"),
            basis: CostBasis::Subscription,
            period: Some(SubscriptionPeriod::Year),
            price: Some(Amount {
                usd_nanos: 31_500_000_000,
                custom_nanos: None,
            }),
        }];
        let cost = subscription_amortization(
            &entries,
            utc("2025-01-01T00:00:00Z"),
            utc("2025-12-31T00:00:00Z"),
            utc("2026-01-02T00:00:00Z"),
        )
        .unwrap();
        assert_eq!(cost, SubscriptionCost::default());
    }

    #[test]
    fn peak_validation_rejects_ambiguous_rules() {
        let mut overlapping = peak(&[("09:00", "12:00"), ("11:00", "13:00")], &[1], "2");
        assert!(overlapping.validate().is_err());
        overlapping.ranges = vec![
            PeakWindow {
                start: "09:00".to_owned(),
                end: "12:00".to_owned(),
            },
            PeakWindow {
                start: "12:00".to_owned(),
                end: "13:00".to_owned(),
            },
        ];
        assert!(overlapping.validate().is_ok());

        let cross_midnight = peak(&[("22:00", "02:00")], &[1], "2");
        assert!(cross_midnight.validate().is_err());
        let reversed = peak(&[("12:00", "09:00")], &[1], "2");
        assert!(reversed.validate().is_err());
        let zero_multiplier = peak(&[("09:00", "12:00")], &[1], "0");
        assert!(zero_multiplier.validate().is_err());
        let bad_timezone = PeakPricing {
            timezone: "Not/AZone".to_owned(),
            ..peak(&[("09:00", "12:00")], &[1], "2")
        };
        assert!(bad_timezone.validate().is_err());
        let bad_weekday = peak(&[("09:00", "12:00")], &[0], "2");
        assert!(bad_weekday.validate().is_err());
        let duplicate_weekday = peak(&[("09:00", "12:00")], &[1, 1], "2");
        assert!(duplicate_weekday.validate().is_err());
        let bad_format = peak(&[("9:00", "12:00")], &[1], "2");
        assert!(bad_format.validate().is_err());
        let end_of_day = peak(&[("09:00", "24:00")], &[1], "2");
        assert!(end_of_day.validate().is_ok());
        let past_end_of_day = peak(&[("09:00", "24:01")], &[1], "2");
        assert!(past_end_of_day.validate().is_err());
    }

    #[test]
    fn cost_validation_rejects_duplicate_custom_fields_and_bad_currency() {
        let mut duplicate_fields = cost(Some("1"), Some("0"));
        duplicate_fields.custom_currency_fields = vec![PriceField::Input, PriceField::Input];
        assert!(duplicate_fields.validate().is_err());

        let mut cost = cost(Some("1"), Some("0"));
        cost.custom_currency_fields = vec![PriceField::Input];
        assert!(cost.validate_currency(None).is_err());
        assert!(
            cost.validate_currency(Some(&currency("7.1", "7.2")))
                .is_ok()
        );
    }

    #[test]
    fn billing_validation_matches_currency_snapshot() {
        let custom = BillingConfig::PerCall {
            amount: dec("-1"),
            currency: CurrencyKind::Usd,
        };
        assert!(custom.validate(None).is_err());

        let custom_without_snapshot = BillingConfig::PerCall {
            amount: dec("1"),
            currency: CurrencyKind::Custom,
        };
        assert!(custom_without_snapshot.validate(None).is_err());
        assert!(
            custom_without_snapshot
                .validate(Some(&currency("7.1", "7.2")))
                .is_ok()
        );

        let mut price = cost(Some("1"), Some("0"));
        price.custom_currency_fields = vec![PriceField::Output];
        let per_token = BillingConfig::PerToken {
            price: std::sync::Arc::new(price),
        };
        assert!(per_token.validate(None).is_err());
        assert!(per_token.validate(Some(&currency("7.1", "7.2"))).is_ok());
    }

    #[test]
    fn custom_currency_validation() {
        assert!(currency("7.1", "7.2").validate().is_ok());
        let mut invalid = currency("0", "7.2");
        assert!(invalid.validate().is_err());
        invalid = currency("7.1", "0");
        assert!(invalid.validate().is_err());
        invalid = currency("7.1", "7.2");
        invalid.symbol = "   ".to_owned();
        assert!(invalid.validate().is_err());
        invalid.symbol = "123456789".to_owned();
        assert!(invalid.validate().is_err());
        invalid.symbol = "¥".to_owned();
        invalid.configured_at = "not-a-time".to_owned();
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn source_local_fields_are_stripped() {
        let mut cost = Some({
            let mut cost = cost(Some("1"), Some("0"));
            cost.custom_currency_fields = vec![PriceField::Input];
            cost.peak = Some(peak(&[("09:00", "12:00")], &[1], "2"));
            cost
        });
        strip_source_local_cost_fields(&mut cost);
        let cost = cost.unwrap();
        assert!(cost.custom_currency_fields.is_empty());
        assert!(cost.peak.is_none());
    }

    #[test]
    fn usd_price_converts_custom_fields() {
        let currency = currency("7.1", "7.2");
        assert_eq!(usd_price(dec("7.1"), true, Some(&currency)), Some(dec("1")));
        assert_eq!(usd_price(dec("7.1"), true, None), None);
        assert_eq!(usd_price(dec("2"), false, None), Some(dec("2")));
    }

    #[test]
    fn model_cost_serialization_keeps_local_fields() {
        let mut cost = cost(Some("1"), Some("0"));
        cost.custom_currency_fields = vec![PriceField::Input, PriceField::Tiers];
        cost.peak = Some(peak(&[("09:00", "12:00")], &[1, 2, 3, 4, 5], "2"));
        let value = serde_json::to_value(&cost).unwrap();
        assert_eq!(value["input"], serde_json::json!("1"));
        assert_eq!(value["output"], serde_json::json!("0"));
        assert_eq!(
            value["custom_currency_fields"],
            serde_json::json!(["input", "tiers"])
        );
        assert_eq!(value["peak"]["timezone"], "Asia/Shanghai");
        assert_eq!(value["peak"]["multiplier"], "2");
        let decoded: ModelCost = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, cost);
    }

    #[test]
    fn model_cost_decodes_catalog_numbers_losslessly() {
        let decoded: ModelCost = serde_json::from_str(
            r#"{
                "input": 0.1234567890123456789012345678,
                "output": 2,
                "context_over_200k": {"input": 0.25},
                "tiers": [{
                    "tier": {"type": "context", "size": 200000},
                    "input": 0.5
                }]
            }"#,
        )
        .unwrap();
        assert_eq!(
            decoded.prices.input,
            Some(dec("0.1234567890123456789012345678"))
        );
        assert_eq!(decoded.prices.output, Some(dec("2")));
        assert_eq!(
            decoded
                .context_over_200k
                .as_ref()
                .and_then(|prices| prices.input),
            Some(dec("0.25"))
        );
        assert_eq!(decoded.tiers[0].prices.input, Some(dec("0.5")));
    }
}
