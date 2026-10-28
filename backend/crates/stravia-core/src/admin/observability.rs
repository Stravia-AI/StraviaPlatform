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
        self.gw.observation.client_error_detail(kind, id).await
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
        self.gw.observation.upstream_error_detail(id).await
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
        self.gw.observation.get_interaction(id, filters).await
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
        self.gw.observation.get_interaction_events(id, query).await
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

    pub async fn get_stats_overview(&self, window: StatsWindow) -> anyhow::Result<StatsOverview> {
        window.validate()?;
        self.gw.storage.usage_stats().stats_overview(window).await
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
        self.gw
            .storage
            .usage_stats()
            .stats_series(window, bucket_ms)
            .await
    }

    pub async fn get_stats_by_model(&self, window: StatsWindow) -> anyhow::Result<Vec<ModelStats>> {
        window.validate()?;
        self.gw.storage.usage_stats().stats_by_model(window).await
    }

    pub async fn get_stats_by_provider(
        &self,
        window: StatsWindow,
    ) -> anyhow::Result<Vec<ProviderStats>> {
        window.validate()?;
        self.gw
            .storage
            .usage_stats()
            .stats_by_provider(window)
            .await
    }

    pub async fn get_stats_by_api_key(
        &self,
        window: StatsWindow,
    ) -> anyhow::Result<Vec<ApiKeyStats>> {
        window.validate()?;
        self.gw.storage.usage_stats().stats_by_api_key(window).await
    }
}
