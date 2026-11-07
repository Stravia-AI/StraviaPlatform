use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

use crate::pricing::{
    AttemptAmounts, BillingOutcome, CostBasis, CostPlanEntry, CustomCurrency, DisplayCurrency,
    DisplayMoney, SubscriptionCost, format_amount, subscription_amortization,
    subscription_value_multiple,
};

use super::*;

const PERFORMANCE_ARCHIVE_METRICS_ENTRY: &str = "stravia-performance-metrics.prom";
const PERFORMANCE_ARCHIVE_TIMELINE_ENTRY: &str = "stravia-performance-trace.json";

impl AdminService {
    // ── Interaction Observation ──

    pub async fn client_errors(&self, query: ClientErrorQuery) -> anyhow::Result<ClientErrorPage> {
        let mut page = self.gw.observation.client_errors(query).await?;
        let ids = page
            .items
            .iter()
            .filter_map(|item| item.interaction_id.clone())
            .collect();
        let masker = self.observation_masker(ids).await?;
        masker.mask_client_error_page(&mut page);
        Ok(page)
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
        let ids = detail
            .iter()
            .filter_map(|detail| detail.request.interaction_id.clone())
            .collect();
        let masker = self.observation_masker(ids).await?;
        if let Some(detail) = &mut detail {
            masker.mask_client_error_detail(detail);
        }
        Ok(detail)
    }

    pub async fn upstream_errors(
        &self,
        query: UpstreamErrorQuery,
    ) -> anyhow::Result<UpstreamErrorPage> {
        let mut page = self.gw.observation.upstream_errors(query).await?;
        let ids = page
            .items
            .iter()
            .filter_map(|item| item.interaction_id.clone())
            .collect();
        let masker = self.observation_masker(ids).await?;
        masker.mask_upstream_error_page(&mut page);
        Ok(page)
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
        let ids = detail
            .iter()
            .filter_map(|detail| detail.error.interaction_id.clone())
            .collect();
        let masker = self.observation_masker(ids).await?;
        if let Some(detail) = &mut detail {
            masker.mask_upstream_error_detail(detail);
        }
        Ok(detail)
    }

    pub async fn observation_forest(&self, query: ForestQuery) -> anyhow::Result<ForestPage> {
        let mut page = self.gw.observation.query_forest(query).await?;
        let ids = page
            .roots
            .iter()
            .flat_map(|root| root.interactions.iter())
            .map(|summary| summary.id.clone())
            .collect();
        let masker = self.observation_masker(ids).await?;
        masker.mask_forest(&mut page);
        Ok(page)
    }

    pub async fn observation_root_changes(
        &self,
        query: RootChangesQuery,
    ) -> anyhow::Result<RootChangesPage> {
        let mut page = self.gw.observation.query_root_changes(query).await?;
        let ids = page
            .changes
            .iter()
            .flat_map(|change| change.interactions.iter())
            .map(|summary| summary.id.clone())
            .collect();
        let masker = self.observation_masker(ids).await?;
        masker.mask_root_changes(&mut page);
        Ok(page)
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
        let ids = detail
            .iter()
            .flat_map(|detail| {
                std::iter::once(detail.interaction.id.clone()).chain(
                    detail
                        .root
                        .interactions
                        .iter()
                        .map(|summary| summary.id.clone()),
                )
            })
            .collect();
        let masker = self.observation_masker(ids).await?;
        if let Some(detail) = &mut detail {
            masker.mask_interaction_detail(detail);
        }
        Ok(detail)
    }

    /// 批量读取 Observation 引用的正文；顺序按请求，不可见或不存在的内容直接省略。
    /// id 非法（空、超过上限或含非正数）返回 [`ObservationQueryError::InvalidContentIds`]。
    pub async fn observation_contents(
        &self,
        ids: &[i64],
    ) -> anyhow::Result<ObservationContentBatch> {
        let visible = self.gw.observation.observation_contents(ids).await?;
        let digests: Vec<_> = visible.iter().map(|visible| visible.digest).collect();
        let mut contents: Vec<_> = visible.into_iter().map(|visible| visible.content).collect();
        let owners = self.gw.observation.content_interactions(ids).await?;
        let interactions: Vec<String> = owners.values().flatten().cloned().collect();
        let masker = self.observation_masker(interactions).await?;
        masker.mask_contents(&mut contents, &owners);
        // ETag 同时覆盖返回正文与当前映射：正文按内容摘要，映射按主体版本。
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        hasher.update(masker.version().as_bytes());
        for digest in &digests {
            hasher.update(digest);
        }
        let etag = crate::interaction_observation::hex(&hasher.finalize());
        Ok(ObservationContentBatch { contents, etag })
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
        let ids = page
            .as_ref()
            .map(|_| vec![id.to_owned()])
            .unwrap_or_default();
        let masker = self.observation_masker(ids).await?;
        if let Some(page) = &mut page {
            masker.mask_interaction_events_page(page, id);
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

    /// 建立实时订阅；存在有效映射时按主体脱敏，映射刷新失败会以 `live_gap` 结束订阅。
    pub async fn observation_subscribe_live(
        &self,
        interaction_id: String,
    ) -> anyhow::Result<ObservationStream> {
        self.masked_live_observation(interaction_id).await
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

    /// 同一时刻的性能指标与时间线打包为 Deflate 压缩的 zip，管理员一次下载即可完整诊断。
    ///
    /// 条目为 `stravia-performance-metrics.prom`（Prometheus 文本）与
    /// `stravia-performance-trace.json`（Chrome Trace JSON）。时间线可达数十 MiB，
    /// 序列化与压缩是 CPU 密集的同步工作，调用方须在阻塞线程执行。性能记录器未安装时返回
    /// `None`；压缩或序列化失败返回错误，不产出残缺归档。
    pub fn performance_archive(&self) -> anyhow::Result<Option<Vec<u8>>> {
        use std::io::Write as _;
        use zip::write::SimpleFileOptions;

        let Some(metrics) = self.performance_metrics() else {
            return Ok(None);
        };
        let timeline = self.performance_timeline();
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        let mut archive = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        archive.start_file(PERFORMANCE_ARCHIVE_METRICS_ENTRY, options)?;
        archive.write_all(metrics.as_bytes())?;
        archive.start_file(PERFORMANCE_ARCHIVE_TIMELINE_ENTRY, options)?;
        serde_json::to_writer(&mut archive, &timeline)?;
        Ok(Some(archive.finish()?.into_inner()))
    }

    pub fn set_observation_debug(&self, enabled: bool) -> DebugState {
        self.gw.observation.set_debug_enabled(enabled)
    }

    pub async fn clear_observation_debug(&self) -> anyhow::Result<DebugState> {
        self.gw.observation.clear_debug().await
    }

    pub async fn clear_observation_history(&self) -> anyhow::Result<ClearHistoryResult> {
        self.gw.history_maintenance().clear_history().await
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

    pub async fn get_stats_overview(
        &self,
        window: StatsWindow,
        filter: StatsFilter,
    ) -> anyhow::Result<StatsOverview> {
        window.validate()?;
        filter.validate()?;
        let currency = self.gw.custom_currency_snapshot();
        let mut plans = self.provider_cost_plan_entries().await?;
        filter_stats_plans(&mut plans, &filter);
        let mut stats = self
            .gw
            .storage
            .usage_stats()
            .stats_overview(window, filter)
            .await?;
        let now = Utc::now();
        let (start, end) = amortization_window(window);
        let subscription = sum_subscription_costs(&plans, start, end, now);
        stats.actual_cost = actual_money(stats.money, subscription, currency.as_deref());
        stats.subscription = plans
            .values()
            .any(|entries| has_subscription_overlap(entries, start, end, now))
            .then(|| subscription_value(stats.money, subscription, currency.as_deref()));
        stats.currency = DisplayCurrency::from_snapshot(currency.as_deref());
        Ok(stats)
    }

    pub async fn get_stats_series(
        &self,
        window: StatsWindow,
        filter: StatsFilter,
        bucket_seconds: Option<i64>,
    ) -> anyhow::Result<Vec<StatsSeries>> {
        window.validate()?;
        filter.validate()?;
        let seconds = bucket_seconds.unwrap_or(900);
        if seconds < 900 || seconds % 900 != 0 {
            return Err(
                StatsQueryError("bucket must be a positive multiple of 900 seconds").into(),
            );
        }
        let bucket_ms = seconds
            .checked_mul(1_000)
            .ok_or(StatsQueryError("bucket is too large"))?;
        let currency = self.gw.custom_currency_snapshot();
        let mut plans = self.provider_cost_plan_entries().await?;
        filter_stats_plans(&mut plans, &filter);
        let mut rows = self
            .gw
            .storage
            .usage_stats()
            .stats_series(window, filter, bucket_ms)
            .await?;
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
                    Some(SubscriptionCost::default()),
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
            let subscription_cost = if failed_subscription.contains(provider_id) {
                None
            } else {
                Some(amortized.get(provider_id).copied().unwrap_or_default())
            };
            let has_subscription = subscription_providers.contains(provider_id);
            // 既无受追踪尝试也无订阅段重叠时不展示成本（不计算成本的 Provider）。
            let tracked = money.cost_known_count > 0 || money.cost_unknown_count > 0;
            row.cost = (tracked || has_subscription).then(|| {
                display_money(
                    Some(money.cost_custom_nanos),
                    Some(money.cost_usd_unconverted_nanos),
                    money.cost_known_count,
                    money.cost_unknown_count,
                    currency.as_deref(),
                )
            });
            row.subscription = has_subscription
                .then(|| subscription_value(money, subscription_cost, currency.as_deref()));
            let payg_tracked = money.payg_cost_known_count > 0 || money.payg_cost_unknown_count > 0;
            row.allocated_cost = (payg_tracked || has_subscription)
                .then(|| actual_money(money, subscription_cost, currency.as_deref()));
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
            let subscription_cost = if failed_subscription.contains(&provider_id) {
                None
            } else {
                Some(amortized.get(&provider_id).copied().unwrap_or_default())
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
                // 仅有摊销记录的 Provider 没有尝试成本，展示 0 而不是隐藏。
                cost: Some(display_money(Some(0), Some(0), 0, 0, currency.as_deref())),
                subscription: Some(subscription_value(
                    UsageMoneyTotals::default(),
                    subscription_cost,
                    currency.as_deref(),
                )),
                allocated_cost: Some(actual_money(
                    UsageMoneyTotals::default(),
                    subscription_cost,
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
    subscription: Option<SubscriptionCost>,
    currency: Option<&CustomCurrency>,
) -> DisplayMoney {
    let Some(subscription) = subscription else {
        return unknown_amortization_money(
            money.payg_cost_known_count,
            money.payg_cost_unknown_count,
        );
    };
    let Some(custom_nanos) =
        Decimal::from(money.payg_cost_custom_nanos).checked_add(subscription.custom_nanos)
    else {
        tracing::warn!("payg and subscription custom amounts overflowed");
        return unknown_amortization_money(
            money.payg_cost_known_count,
            money.payg_cost_unknown_count,
        );
    };
    let Some(usd_unconverted_nanos) = Decimal::from(money.payg_cost_usd_unconverted_nanos)
        .checked_add(subscription.usd_unconverted_nanos)
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

/// 同窗口内按订阅基准完成的尝试的等价按量成本：全部成本桶减去按量桶。
fn usage_equivalent_money(
    money: UsageMoneyTotals,
    currency: Option<&CustomCurrency>,
) -> DisplayMoney {
    let custom = sub_nanos(money.cost_custom_nanos, money.payg_cost_custom_nanos);
    let usd = sub_nanos(
        money.cost_usd_unconverted_nanos,
        money.payg_cost_usd_unconverted_nanos,
    );
    let known = sub_nanos(money.cost_known_count, money.payg_cost_known_count);
    let unknown = sub_nanos(money.cost_unknown_count, money.payg_cost_unknown_count);
    display_money(
        custom,
        usd,
        known.unwrap_or(0),
        unknown.unwrap_or(0),
        currency,
    )
}

/// 价值倍数用展示币种的精确十进制计算；任一侧不可表示时返回 `None`。
fn usage_equivalent_multiple(
    money: UsageMoneyTotals,
    subscription: Option<SubscriptionCost>,
    currency: Option<&CustomCurrency>,
) -> Option<String> {
    let subscription = subscription?;
    let equivalent_custom = Decimal::from(sub_nanos(
        money.cost_custom_nanos,
        money.payg_cost_custom_nanos,
    )?);
    let equivalent_usd = Decimal::from(sub_nanos(
        money.cost_usd_unconverted_nanos,
        money.payg_cost_usd_unconverted_nanos,
    )?);
    subscription_value_multiple(
        equivalent_custom,
        equivalent_usd,
        subscription.custom_nanos,
        subscription.usd_unconverted_nanos,
        currency,
    )
}

/// 订阅价值对比：摊销费用、同窗口等价按量成本与两者之比。
/// 摊销溢出时费用记为未知，倍数为 null。
fn subscription_value(
    money: UsageMoneyTotals,
    subscription: Option<SubscriptionCost>,
    currency: Option<&CustomCurrency>,
) -> SubscriptionValue {
    let (cost, multiple) = match subscription {
        Some(subscription) => (
            display_money_decimal(
                subscription.custom_nanos,
                subscription.usd_unconverted_nanos,
                0,
                0,
                currency,
            ),
            usage_equivalent_multiple(money, Some(subscription), currency),
        ),
        None => (unknown_amortization_money(0, 0), None),
    };
    SubscriptionValue {
        cost,
        usage_equivalent_cost: usage_equivalent_money(money, currency),
        multiple,
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
        recovered_request_count: 0,
        recovery_known_request_count: 0,
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

/// 过滤口径下的订阅计划：Provider 筛选只保留所选 Provider 的计划；
/// API key 无法归属订阅成本，因此清空计划（只展示按量成本）。
fn filter_stats_plans(plans: &mut BTreeMap<String, Vec<CostPlanEntry>>, filter: &StatsFilter) {
    match filter {
        StatsFilter::All => {}
        StatsFilter::Providers(ids) => {
            let selected: std::collections::BTreeSet<&str> =
                ids.iter().map(String::as_str).collect();
            plans.retain(|provider_id, _| selected.contains(provider_id.as_str()));
        }
        StatsFilter::ApiKeys(_) => plans.clear(),
    }
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
) -> Option<SubscriptionCost> {
    let mut total = SubscriptionCost::default();
    for entries in plans.values() {
        let Some(cost) = subscription_amortization(entries, start, end, now) else {
            tracing::warn!("subscription amortization exceeds representable range");
            return None;
        };
        total.custom_nanos = total
            .custom_nanos
            .checked_add(cost.custom_nanos)
            .or_else(|| {
                tracing::warn!("subscription amortization total overflowed");
                None
            })?;
        total.usd_unconverted_nanos = total
            .usd_unconverted_nanos
            .checked_add(cost.usd_unconverted_nanos)
            .or_else(|| {
                tracing::warn!("subscription amortization total overflowed");
                None
            })?;
    }
    Some(total)
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
        let payload = match event.kind.as_str() {
            "target_attempt_started" => Some(&event.payload),
            "model_turn_started" => event.payload.get("attempt"),
            _ => None,
        };
        let Some(payload) = payload.and_then(serde_json::Value::as_object) else {
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
        match event.kind.as_str() {
            "target_attempt_finished" => {
                if let Some(payload) = event.payload.as_object_mut() {
                    project_attempt_amount(payload, &attempt_bases, currency, &display_currency);
                }
            }
            // 合并的 Turn 结束行把最后一个 attempt 放在 `attempt` 下，投影规则保持一致。
            "model_turn_finished" => {
                if let Some(attempt) = event
                    .payload
                    .get_mut("attempt")
                    .and_then(serde_json::Value::as_object_mut)
                {
                    project_attempt_amount(attempt, &attempt_bases, currency, &display_currency);
                }
            }
            _ => {}
        }
    }
}

fn project_attempt_amount(
    payload: &mut serde_json::Map<String, serde_json::Value>,
    attempt_bases: &BTreeMap<String, String>,
    currency: Option<&CustomCurrency>,
    display_currency: &DisplayCurrency,
) {
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
            // 不计算成本的尝试没有成本基准，投影为 null。
            let cost_basis = match amounts.cost_basis.attempt_basis() {
                Some(basis) => match serde_json::to_value(basis) {
                    Ok(value) => Some(value),
                    Err(error) => {
                        tracing::warn!(%error, "failed to serialize attempt cost basis");
                        None
                    }
                },
                None => None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::SqliteStorage;
    use std::sync::Arc;

    async fn insert_bucket(
        pool: &sqlx::SqlitePool,
        (scope, identity): (&str, &str),
        bucket_start: i64,
        request_count: i64,
        upstream_request_count: i64,
        payg_usd_unconverted_nanos: i64,
        payg_known_count: i64,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO stats_usage_buckets
             (scope,identity,bucket_start,entity_id,label,request_count,upstream_request_count,
              payg_cost_usd_unconverted_nanos,payg_cost_known_count,last_used_at)
             VALUES (?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(scope)
        .bind(identity)
        .bind(bucket_start)
        .bind(identity)
        .bind(identity)
        .bind(request_count)
        .bind(upstream_request_count)
        .bind(payg_usd_unconverted_nanos)
        .bind(payg_known_count)
        .bind(bucket_start)
        .execute(pool)
        .await?;
        Ok(())
    }

    /// 订阅摊销只跟随所选 Provider；API key 无法归属订阅，只保留按量成本。
    #[tokio::test]
    async fn filtered_stats_include_only_selected_provider_subscriptions() -> anyhow::Result<()> {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let end = Utc::now().timestamp_millis() / StatsWindow::BASE_BUCKET_MS
            * StatsWindow::BASE_BUCKET_MS
            - StatsWindow::BASE_BUCKET_MS;
        let start = end - 3_600_000;
        insert_bucket(
            &pool,
            ("provider", "provider-a"),
            start,
            0,
            1,
            1_000_000_000,
            1,
        )
        .await?;
        insert_bucket(&pool, ("provider", "provider-b"), start, 0, 1, 0, 0).await?;
        insert_bucket(&pool, ("api_key", "key-a"), start, 1, 0, 500_000_000, 1).await?;
        // 365/12 天月价 2,628,000 USD 即每秒 1 USD；一小时窗口摊销 3,600 USD。
        for (id, provider_id) in [("plan-a", "provider-a"), ("plan-b", "provider-b")] {
            sqlx::query(
                "INSERT INTO provider_cost_plans
                 (id,provider_id,effective_from,basis,period,price_usd_nanos,price_custom_nanos,price_currency,created_at)
                 VALUES (?,?,?,?,?,?,NULL,NULL,'2026-01-01T00:00:00Z')",
            )
            .bind(id)
            .bind(provider_id)
            .bind(start - 1_000)
            .bind("subscription")
            .bind("month")
            .bind(2_628_000_000_000_000_i64)
            .execute(&pool)
            .await?;
        }
        let directory = tempfile::tempdir()?;
        let storage = Arc::new(SqliteStorage::from_pool(pool.clone()));
        let gateway = Gateway::from_storage(
            crate::config::GatewayConfig {
                data_dir: directory.path().to_path_buf(),
                ..Default::default()
            },
            storage,
        )
        .await?;
        let admin = gateway.admin();
        let window = StatsWindow {
            start_at: Some(start),
            end_at: Some(end),
        };

        let selected = admin
            .get_stats_overview(
                window,
                StatsFilter::Providers(vec!["provider-a".to_owned()]),
            )
            .await?;
        assert_eq!(selected.total_requests, 1);
        assert_eq!(
            selected
                .subscription
                .as_ref()
                .map(|value| value.cost.amount.as_str()),
            Some("3600")
        );
        assert_eq!(selected.actual_cost.amount, "3601");

        let all = admin.get_stats_overview(window, StatsFilter::All).await?;
        assert_eq!(
            all.subscription
                .as_ref()
                .map(|value| value.cost.amount.as_str()),
            Some("7200")
        );

        let api_key = admin
            .get_stats_overview(window, StatsFilter::ApiKeys(vec!["key-a".to_owned()]))
            .await?;
        assert_eq!(api_key.total_requests, 1);
        assert!(api_key.subscription.is_none());
        assert_eq!(api_key.actual_cost.amount, "0.5");

        gateway.shutdown().await;
        pool.close().await;
        Ok(())
    }
}
