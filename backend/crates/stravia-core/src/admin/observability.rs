use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

use crate::pricing::{
    AttemptAmounts, BillingOutcome, CostBasis, CostPlanEntry, CustomCurrency, DisplayCurrency,
    DisplayMoney, format_amount, subscription_amortization,
};

use super::*;

impl AdminService {
    // ── Interaction Observation ──

    pub async fn client_errors(&self, query: ClientErrorQuery) -> anyhow::Result<ClientErrorPage> {
        self.gw.observation.client_errors(query).await
    }

    pub async fn client_error_detail(
        &self,
        kind: &str,
        id: &str,
    ) -> anyhow::Result<Option<ClientErrorDetail>> {
        let mut detail = self.gw.observation.client_error_detail(kind, id).await?;
        if let Some(detail) = &mut detail {
            let currency = self.gw.custom_currency_snapshot();
            project_attempt_amounts(&mut detail.events, currency.as_deref());
        }
        Ok(detail)
    }

    pub async fn upstream_errors(
        &self,
        query: UpstreamErrorQuery,
    ) -> anyhow::Result<UpstreamErrorPage> {
        self.gw.observation.upstream_errors(query).await
    }

    pub async fn upstream_error_detail(
        &self,
        id: &str,
    ) -> anyhow::Result<Option<UpstreamErrorDetail>> {
        let mut detail = self.gw.observation.upstream_error_detail(id).await?;
        if let Some(detail) = &mut detail {
            let currency = self.gw.custom_currency_snapshot();
            project_attempt_amounts(&mut detail.events, currency.as_deref());
        }
        Ok(detail)
    }

    pub async fn observation_forest(&self, query: ForestQuery) -> anyhow::Result<ForestPage> {
        self.gw.observation.query_forest(query).await
    }

    pub async fn observation_root_changes(
        &self,
        query: RootChangesQuery,
    ) -> anyhow::Result<RootChangesPage> {
        self.gw.observation.query_root_changes(query).await
    }

    pub async fn observation_interaction(
        &self,
        id: &str,
        filters: ForestQuery,
    ) -> anyhow::Result<Option<InteractionDetail>> {
        let mut detail = self.gw.observation.get_interaction(id, filters).await?;
        if let Some(detail) = &mut detail {
            let currency = self.gw.custom_currency_snapshot();
            for run in &mut detail.runs {
                project_attempt_amounts(&mut run.events, currency.as_deref());
            }
        }
        Ok(detail)
    }

    /// 显式等待异步观测 writer 排空；查询接口本身不提供该保证。
    pub async fn observation_flush(&self) -> anyhow::Result<()> {
        self.gw.observation.flush().await
    }

    pub async fn observation_interaction_events(
        &self,
        id: &str,
        query: InteractionEventsQuery,
    ) -> anyhow::Result<Option<InteractionEventsPage>> {
        let mut page = self
            .gw
            .observation
            .get_interaction_events(id, query)
            .await?;
        if let Some(page) = &mut page {
            let currency = self.gw.custom_currency_snapshot();
            for run in &mut page.runs {
                project_attempt_amounts(&mut run.events, currency.as_deref());
            }
        }
        Ok(page)
    }

    pub async fn observation_rejections(
        &self,
        query: RejectionQuery,
    ) -> anyhow::Result<RejectionPage> {
        self.gw.observation.query_rejections(query).await
    }

    pub async fn observation_rejection(&self, id: &str) -> anyhow::Result<Option<RejectionDetail>> {
        self.gw.observation.get_rejection(id).await
    }

    pub fn observation_subscribe(&self, after: i64) -> ObservationStream {
        self.gw.observation.subscribe(after)
    }

    pub fn observation_subscribe_live(&self, interaction_id: String) -> ObservationStream {
        self.gw.observation.subscribe_live(interaction_id)
    }

    pub fn observation_debug(&self) -> DebugState {
        self.gw.observation.debug_state()
    }

    pub fn performance_metrics(&self) -> Option<String> {
        if self.gw.observation.debug_enabled() {
            crate::performance::record_observation_queue_depth(
                self.gw.observation.writer_queue_depth(),
            );
        }
        crate::performance::metrics_snapshot()
    }

    pub fn performance_timeline(&self) -> serde_json::Value {
        crate::performance::timeline_snapshot()
    }

    pub fn set_observation_debug(&self, enabled: bool) -> DebugState {
        self.gw.observation.set_debug_enabled(enabled)
    }

    pub async fn clear_observation_debug(&self) -> anyhow::Result<DebugState> {
        self.gw.observation.clear_debug().await
    }

    pub async fn clear_observation_history(&self) -> anyhow::Result<ClearHistoryResult> {
        self.gw.observation.clear_history().await
    }

    pub async fn issue_observation_bundle_ticket(
        &self,
        request: BundleRequest,
    ) -> anyhow::Result<DownloadTicket> {
        self.gw.observation.issue_bundle_ticket(request).await
    }

    pub async fn consume_observation_bundle_ticket(
        &self,
        ticket: &str,
    ) -> anyhow::Result<BundleStream> {
        self.gw.observation.consume_bundle_ticket(ticket).await
    }

    // ── Stats ──

    async fn provider_cost_plan_entries(
        &self,
    ) -> anyhow::Result<BTreeMap<String, Vec<CostPlanEntry>>> {
        let plans = self.gw.storage.money().all_provider_cost_plans().await?;
        group_cost_plans(&plans)
    }

    pub async fn get_stats_overview(&self, window: StatsWindow) -> anyhow::Result<StatsOverview> {
        window.validate()?;
        let mut stats = self.gw.storage.usage_stats().stats_overview(window).await?;
        let currency = self.gw.custom_currency_snapshot();
        let plans = self.provider_cost_plan_entries().await?;
        let (start, end) = amortization_window(window);
        let subscription = sum_subscription_costs(&plans, start, end, Utc::now());
        stats.actual_cost = actual_money(stats.money, subscription, currency.as_deref());
        stats.currency = DisplayCurrency::from_snapshot(currency.as_deref());
        Ok(stats)
    }

    pub async fn get_stats_series(
        &self,
        window: StatsWindow,
        bucket_seconds: Option<i64>,
    ) -> anyhow::Result<Vec<StatsSeries>> {
        window.validate()?;
        let seconds = bucket_seconds.unwrap_or(900);
        if seconds < 900 || seconds % 900 != 0 {
            return Err(
                StatsQueryError("bucket must be a positive multiple of 900 seconds").into(),
            );
        }
        let bucket_ms = seconds
            .checked_mul(1_000)
            .ok_or(StatsQueryError("bucket is too large"))?;
        let mut rows = self
            .gw
            .storage
            .usage_stats()
            .stats_series(window, bucket_ms)
            .await?;
        let currency = self.gw.custom_currency_snapshot();
        let plans = self.provider_cost_plan_entries().await?;
        let now = Utc::now();
        let (window_start, window_end) = amortization_window(window);
        let has_subscription = plans
            .values()
            .any(|entries| has_subscription_overlap(entries, window_start, window_end, now));
        if has_subscription {
            let Some((start, end, grid_ms)) = series_grid(window, &rows, &plans, bucket_ms, now)
            else {
                return Err(
                    StatsQueryError("subscription cost timeline is not representable").into(),
                );
            };
            let mut buckets: BTreeMap<i64, StatsSeries> = rows
                .into_iter()
                .map(|row| (row.bucket_start, row))
                .collect();
            let mut cursor = start;
            while cursor < end {
                let bucket_end = cursor.saturating_add(grid_ms).min(end);
                let row = buckets
                    .entry(cursor)
                    .or_insert_with(|| empty_series_bucket(cursor, grid_ms));
                let bucket_start = DateTime::<Utc>::from_timestamp_millis(cursor)
                    .unwrap_or(DateTime::<Utc>::MIN_UTC);
                let bucket_end = DateTime::<Utc>::from_timestamp_millis(bucket_end).unwrap_or(now);
                let subscription = sum_subscription_costs(&plans, bucket_start, bucket_end, now);
                row.actual_cost = actual_money(row.money, subscription, currency.as_deref());
                row.currency = DisplayCurrency::from_snapshot(currency.as_deref());
                cursor = cursor.saturating_add(grid_ms);
            }
            rows = buckets.into_values().collect();
        } else {
            for row in &mut rows {
                row.actual_cost = actual_money(
                    row.money,
                    Some((Decimal::ZERO, Decimal::ZERO)),
                    currency.as_deref(),
                );
                row.currency = DisplayCurrency::from_snapshot(currency.as_deref());
            }
        }
        Ok(rows)
    }

    pub async fn get_stats_by_model(&self, window: StatsWindow) -> anyhow::Result<Vec<ModelStats>> {
        window.validate()?;
        let mut rows = self.gw.storage.usage_stats().stats_by_model(window).await?;
        let currency = self.gw.custom_currency_snapshot();
        for row in &mut rows {
            let money = row.money;
            row.cost = display_money(
                Some(money.cost_custom_nanos),
                Some(money.cost_usd_unconverted_nanos),
                money.cost_known_count,
                money.cost_unknown_count,
                currency.as_deref(),
            );
            row.billing = display_money(
                Some(money.billing_custom_nanos),
                Some(money.billing_usd_unconverted_nanos),
                money.billing_charged_count,
                money.billing_unknown_count,
                currency.as_deref(),
            );
            row.currency = DisplayCurrency::from_snapshot(currency.as_deref());
        }
        Ok(rows)
    }

    pub async fn get_stats_by_provider(
        &self,
        window: StatsWindow,
    ) -> anyhow::Result<Vec<ProviderStats>> {
        window.validate()?;
        let mut rows = self
            .gw
            .storage
            .usage_stats()
            .stats_by_provider(window)
            .await?;
        let currency = self.gw.custom_currency_snapshot();
        let plans = self.provider_cost_plan_entries().await?;
        let (start, end) = amortization_window(window);
        let now = Utc::now();
        let mut amortized = BTreeMap::new();
        let mut failed_subscription = std::collections::BTreeSet::new();
        let mut subscription_providers = std::collections::BTreeSet::new();
        for (provider_id, entries) in &plans {
            if has_subscription_overlap(entries, start, end, now) {
                subscription_providers.insert(provider_id.clone());
                match subscription_amortization(entries, start, end, now) {
                    Some(cost) => {
                        amortized.insert(provider_id.clone(), cost);
                    }
                    None => {
                        tracing::warn!(
                            provider_id = %provider_id,
                            "subscription amortization exceeds representable range"
                        );
                        failed_subscription.insert(provider_id.clone());
                    }
                }
            }
        }
        for row in &mut rows {
            let money = row.money;
            let provider_id = row.provider_id.as_deref().unwrap_or_default();
            let subscription = if failed_subscription.contains(provider_id) {
                None
            } else {
                Some(amortized.get(provider_id).copied().unwrap_or_default())
            };
            row.actual_cost = actual_money(
                money,
                subscription.map(|cost| (cost.custom_nanos, cost.usd_unconverted_nanos)),
                currency.as_deref(),
            );
            let equivalent_custom =
                sub_nanos(money.cost_custom_nanos, money.payg_cost_custom_nanos);
            let equivalent_usd = sub_nanos(
                money.cost_usd_unconverted_nanos,
                money.payg_cost_usd_unconverted_nanos,
            );
            let equivalent_known = sub_nanos(money.cost_known_count, money.payg_cost_known_count);
            let equivalent_unknown =
                sub_nanos(money.cost_unknown_count, money.payg_cost_unknown_count);
            row.usage_equivalent_cost = (subscription_providers.contains(provider_id)
                || equivalent_known.is_some_and(|count| count > 0)
                || equivalent_unknown.is_some_and(|count| count > 0))
            .then(|| {
                display_money(
                    equivalent_custom,
                    equivalent_usd,
                    equivalent_known.unwrap_or(0),
                    equivalent_unknown.unwrap_or(0),
                    currency.as_deref(),
                )
            });
            row.currency = DisplayCurrency::from_snapshot(currency.as_deref());
        }
        let plan_only: std::collections::BTreeSet<String> = amortized
            .keys()
            .chain(failed_subscription.iter())
            .cloned()
            .collect();
        let existing_provider_ids: std::collections::BTreeSet<String> = rows
            .iter()
            .filter_map(|row| row.provider_id.clone())
            .collect();
        let provider_names: BTreeMap<String, String> = if plan_only.is_empty() {
            BTreeMap::new()
        } else {
            self.gw
                .storage
                .providers()
                .list()
                .await?
                .into_iter()
                .map(|provider| (provider.id, provider.name))
                .collect()
        };
        for provider_id in plan_only {
            if existing_provider_ids.contains(&provider_id) {
                continue;
            }
            let provider_name = provider_names.get(&provider_id);
            let subscription = amortized.get(&provider_id).copied();
            let actual_cost = if failed_subscription.contains(&provider_id) {
                unknown_amortization_money(0, 0)
            } else {
                actual_money(
                    UsageMoneyTotals::default(),
                    subscription.map(|cost| (cost.custom_nanos, cost.usd_unconverted_nanos)),
                    currency.as_deref(),
                )
            };
            rows.push(ProviderStats {
                provider_id: Some(provider_id.clone()),
                deleted: provider_name.is_none(),
                provider: provider_name
                    .cloned()
                    .unwrap_or_else(|| provider_id.clone()),
                upstream_request_count: 0,
                upstream_error_count: 0,
                avg_duration_ms: None,
                avg_output_tps: None,
                money: UsageMoneyTotals::default(),
                actual_cost,
                usage_equivalent_cost: Some(display_money(
                    Some(0),
                    Some(0),
                    0,
                    0,
                    currency.as_deref(),
                )),
                currency: DisplayCurrency::from_snapshot(currency.as_deref()),
            });
        }
        Ok(rows)
    }

    pub async fn get_stats_by_api_key(
        &self,
        window: StatsWindow,
    ) -> anyhow::Result<Vec<ApiKeyStats>> {
        window.validate()?;
        let mut rows = self
            .gw
            .storage
            .usage_stats()
            .stats_by_api_key(window)
            .await?;
        let currency = self.gw.custom_currency_snapshot();
        for row in &mut rows {
            let money = row.money;
            row.cost = display_money(
                Some(money.cost_custom_nanos),
                Some(money.cost_usd_unconverted_nanos),
                money.cost_known_count,
                money.cost_unknown_count,
                currency.as_deref(),
            );
            row.billing = display_money(
                Some(money.billing_custom_nanos),
                Some(money.billing_usd_unconverted_nanos),
                money.billing_charged_count,
                money.billing_unknown_count,
                currency.as_deref(),
            );
            row.currency = DisplayCurrency::from_snapshot(currency.as_deref());
        }
        Ok(rows)
    }
}

fn display_money(
    custom_nanos: Option<i64>,
    usd_unconverted_nanos: Option<i64>,
    known_count: i64,
    unknown_count: i64,
    currency: Option<&CustomCurrency>,
) -> DisplayMoney {
    match (custom_nanos, usd_unconverted_nanos) {
        (Some(custom_nanos), Some(usd_unconverted_nanos)) => display_money_decimal(
            Decimal::from(custom_nanos),
            Decimal::from(usd_unconverted_nanos),
            known_count,
            unknown_count,
            currency,
        ),
        _ => unknown_money(known_count, unknown_count),
    }
}

/// Adds exact nanos from payg buckets and subscription amortization, converts
/// with the frozen first rate, and rounds to whole nanos exactly once.
fn display_money_decimal(
    custom_nanos: Decimal,
    usd_unconverted_nanos: Decimal,
    known_count: i64,
    unknown_count: i64,
    currency: Option<&CustomCurrency>,
) -> DisplayMoney {
    DisplayMoney::from_decimal_totals(
        custom_nanos,
        usd_unconverted_nanos,
        known_count.max(0) as u64,
        unknown_count.max(0) as u64,
        currency,
    )
    .unwrap_or_else(|| {
        tracing::warn!(
            %custom_nanos,
            %usd_unconverted_nanos,
            "usage statistics amount exceeds display range"
        );
        unknown_money(known_count, unknown_count)
    })
}

fn actual_money(
    money: UsageMoneyTotals,
    subscription: Option<(Decimal, Decimal)>,
    currency: Option<&CustomCurrency>,
) -> DisplayMoney {
    let Some((subscription_custom, subscription_usd)) = subscription else {
        return unknown_amortization_money(
            money.payg_cost_known_count,
            money.payg_cost_unknown_count,
        );
    };
    let Some(custom_nanos) =
        Decimal::from(money.payg_cost_custom_nanos).checked_add(subscription_custom)
    else {
        tracing::warn!("payg and subscription custom amounts overflowed");
        return unknown_amortization_money(
            money.payg_cost_known_count,
            money.payg_cost_unknown_count,
        );
    };
    let Some(usd_unconverted_nanos) =
        Decimal::from(money.payg_cost_usd_unconverted_nanos).checked_add(subscription_usd)
    else {
        tracing::warn!("payg and subscription USD amounts overflowed");
        return unknown_amortization_money(
            money.payg_cost_known_count,
            money.payg_cost_unknown_count,
        );
    };
    display_money_decimal(
        custom_nanos,
        usd_unconverted_nanos,
        money.payg_cost_known_count,
        money.payg_cost_unknown_count,
        currency,
    )
}

fn unknown_money(known_count: i64, unknown_count: i64) -> DisplayMoney {
    DisplayMoney {
        amount: "0".into(),
        known_count: 0,
        unknown_count: (known_count.max(0) + unknown_count.max(0)) as u64,
    }
}

/// An overflowing subscription amortization is explicitly unknown rather than
/// silently dropping the known subscription and showing zero.
fn unknown_amortization_money(known_count: i64, unknown_count: i64) -> DisplayMoney {
    DisplayMoney {
        amount: "0".into(),
        known_count: known_count.max(0) as u64,
        unknown_count: (unknown_count.max(0) + 1) as u64,
    }
}

fn sub_nanos(total: i64, part: i64) -> Option<i64> {
    total.checked_sub(part).or_else(|| {
        tracing::warn!(total, part, "usage statistics amount underflowed i64 nanos");
        None
    })
}

fn display_bucket_ms(start: i64, end: i64, requested: i64) -> i64 {
    let span = i128::from(end) - i128::from(start);
    let minimum = ((span + 511) / 512).max(i128::from(requested));
    (((minimum + i128::from(StatsWindow::BASE_BUCKET_MS) - 1)
        / i128::from(StatsWindow::BASE_BUCKET_MS))
        * i128::from(StatsWindow::BASE_BUCKET_MS)) as i64
}

fn empty_series_bucket(bucket_start: i64, bucket_ms: i64) -> StatsSeries {
    StatsSeries {
        bucket_start,
        bucket_ms,
        request_count: 0,
        terminal_request_count: 0,
        failed_request_count: 0,
        upstream_request_count: 0,
        upstream_error_count: 0,
        total_input_tokens: None,
        total_output_tokens: None,
        total_cache_read_tokens: None,
        total_cache_write_tokens: None,
        total_reasoning_tokens: None,
        avg_duration_ms: None,
        avg_first_token_ms: None,
        avg_output_tps: None,
        money: UsageMoneyTotals::default(),
        actual_cost: DisplayMoney::default(),
        currency: DisplayCurrency::default(),
    }
}

fn amortization_window(window: StatsWindow) -> (DateTime<Utc>, DateTime<Utc>) {
    let now = Utc::now();
    let start = window
        .start_at
        .and_then(DateTime::<Utc>::from_timestamp_millis)
        .unwrap_or(DateTime::<Utc>::MIN_UTC);
    let end = window
        .end_at
        .and_then(DateTime::<Utc>::from_timestamp_millis)
        .unwrap_or(now);
    (start, end)
}

/// A bounded bucket grid for subscription-only timelines. A fixed window keeps
/// its storage coarsening; an open window spans existing usage (or the oldest
/// subscription segment) through now, still capped at 512 buckets.
fn series_grid(
    window: StatsWindow,
    rows: &[StatsSeries],
    plans: &BTreeMap<String, Vec<CostPlanEntry>>,
    requested_bucket_ms: i64,
    now: DateTime<Utc>,
) -> Option<(i64, i64, i64)> {
    if let (Some(start), Some(end)) = (window.start_at, window.end_at) {
        return Some((
            start,
            end,
            display_bucket_ms(start, end, requested_bucket_ms),
        ));
    }
    if let (Some(first), Some(last)) = (rows.first(), rows.last()) {
        let start = first.bucket_start;
        let end = last.bucket_start.saturating_add(last.bucket_ms);
        if end <= start {
            return None;
        }
        return Some((start, end, first.bucket_ms));
    }
    let start = plans
        .values()
        .flatten()
        .map(|entry| entry.effective_from.timestamp_millis())
        .min()?;
    let end = now.timestamp_millis();
    if end <= start {
        return None;
    }
    Some((
        start,
        end,
        display_bucket_ms(start, end, requested_bucket_ms),
    ))
}

fn group_cost_plans(
    plans: &[crate::storage::traits::ProviderCostPlan],
) -> anyhow::Result<BTreeMap<String, Vec<CostPlanEntry>>> {
    let mut grouped: BTreeMap<String, Vec<CostPlanEntry>> = BTreeMap::new();
    for plan in plans {
        let effective_from = DateTime::<Utc>::from_timestamp_millis(plan.effective_from)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "provider cost plan {} has unrepresentable effective_from {}",
                    plan.id,
                    plan.effective_from
                )
            })?;
        grouped
            .entry(plan.provider_id.clone())
            .or_default()
            .push(CostPlanEntry {
                effective_from,
                basis: plan.basis,
                period: plan.period,
                price: plan.price,
            });
    }
    Ok(grouped)
}

fn sum_subscription_costs(
    plans: &BTreeMap<String, Vec<CostPlanEntry>>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Option<(Decimal, Decimal)> {
    let mut custom_nanos = Decimal::ZERO;
    let mut usd_unconverted_nanos = Decimal::ZERO;
    for entries in plans.values() {
        let Some(cost) = subscription_amortization(entries, start, end, now) else {
            tracing::warn!("subscription amortization exceeds representable range");
            return None;
        };
        custom_nanos = custom_nanos.checked_add(cost.custom_nanos).or_else(|| {
            tracing::warn!("subscription amortization total overflowed");
            None
        })?;
        usd_unconverted_nanos = usd_unconverted_nanos
            .checked_add(cost.usd_unconverted_nanos)
            .or_else(|| {
                tracing::warn!("subscription amortization total overflowed");
                None
            })?;
    }
    Some((custom_nanos, usd_unconverted_nanos))
}

fn has_subscription_overlap(
    entries: &[CostPlanEntry],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    now: DateTime<Utc>,
) -> bool {
    for (index, entry) in entries.iter().enumerate() {
        if entry.basis != CostBasis::Subscription {
            continue;
        }
        let segment_end = entries
            .get(index + 1)
            .map(|next| next.effective_from)
            .unwrap_or(now)
            .min(end)
            .min(now);
        if segment_end > entry.effective_from.max(start) {
            return true;
        }
    }
    false
}

fn project_attempt_amounts(events: &mut [ObservationEvent], currency: Option<&CustomCurrency>) {
    let display_currency = DisplayCurrency::from_snapshot(currency);
    let mut attempt_bases: BTreeMap<String, String> = BTreeMap::new();
    for event in events.iter() {
        if event.kind != "target_attempt_started" {
            continue;
        }
        let Some(payload) = event.payload.as_object() else {
            continue;
        };
        if let (Some(attempt_id), Some(cost_basis)) = (
            payload
                .get("attempt_id")
                .and_then(serde_json::Value::as_str),
            payload
                .get("cost_basis")
                .and_then(serde_json::Value::as_str),
        ) {
            attempt_bases.insert(attempt_id.to_owned(), cost_basis.to_owned());
        }
    }
    for event in events {
        if event.kind != "target_attempt_finished" {
            continue;
        }
        let Some(payload) = event.payload.as_object_mut() else {
            continue;
        };
        let amounts = payload.remove("amounts").and_then(|value| {
            match serde_json::from_value::<AttemptAmounts>(value) {
                Ok(amounts) => Some(amounts),
                Err(error) => {
                    tracing::warn!(%error, "malformed stored attempt amounts");
                    None
                }
            }
        });
        let (cost_basis, cost, billing, billing_state) = match amounts {
            Some(amounts) => {
                let cost_basis = match serde_json::to_value(amounts.cost_basis) {
                    Ok(value) => Some(value),
                    Err(error) => {
                        tracing::warn!(%error, "failed to serialize attempt cost basis");
                        None
                    }
                };
                let cost = amounts.cost.and_then(|amount| {
                    amount.display(currency).map(|display| {
                        serde_json::json!({
                            "amount": format_amount(display),
                            "currency": display_currency.clone(),
                        })
                    })
                });
                let (billing, billing_state) = match amounts.billing {
                    Some(BillingOutcome::Charged(amount)) => match amount.display(currency) {
                        Some(display) => (
                            Some(serde_json::json!({
                                "amount": format_amount(display),
                                "currency": display_currency.clone(),
                            })),
                            Some("charged"),
                        ),
                        None => (None, Some("unknown")),
                    },
                    Some(BillingOutcome::Unknown) => (None, Some("unknown")),
                    None => (None, None),
                };
                (cost_basis, cost, billing, billing_state)
            }
            None => {
                let cost_basis = payload
                    .get("attempt_id")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|attempt_id| attempt_bases.get(attempt_id))
                    .map(|basis| serde_json::Value::String(basis.clone()));
                (cost_basis, None, None, None)
            }
        };
        payload.insert(
            "cost_basis".into(),
            cost_basis.unwrap_or(serde_json::Value::Null),
        );
        payload.insert("cost".into(), cost.unwrap_or(serde_json::Value::Null));
        payload.insert("billing".into(), billing.unwrap_or(serde_json::Value::Null));
        payload.insert(
            "billing_state".into(),
            billing_state
                .map(|state| serde_json::Value::String(state.into()))
                .unwrap_or(serde_json::Value::Null),
        );
    }
}
