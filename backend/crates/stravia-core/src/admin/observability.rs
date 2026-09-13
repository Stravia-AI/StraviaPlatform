use super::*;

impl AdminService {
    // ── Interaction Observation ──

    pub async fn observation_forest(&self, query: ForestQuery) -> anyhow::Result<ForestPage> {
        self.gw.observation.query_forest(query).await
    }

    pub async fn observation_interaction_summary(
        &self,
        id: &str,
        filters: ForestQuery,
    ) -> anyhow::Result<Option<InteractionSnapshot>> {
        self.gw
            .observation
            .get_interaction_summary(id, filters)
            .await
    }

    pub async fn observation_interaction(
        &self,
        id: &str,
        filters: ForestQuery,
    ) -> anyhow::Result<Option<InteractionDetail>> {
        self.gw.observation.get_interaction(id, filters).await
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

    pub fn observation_debug(&self) -> DebugState {
        self.gw.observation.debug_state()
    }

    pub fn set_observation_debug(&self, enabled: bool) -> DebugState {
        self.gw.observation.set_debug_enabled(enabled)
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

    fn normalize_hours(hours: Option<i32>) -> Option<i32> {
        hours.and_then(|value| (value > 0).then_some(value))
    }

    pub async fn get_stats_overview(&self, hours: Option<i32>) -> anyhow::Result<StatsOverview> {
        self.gw
            .storage
            .usage_stats()
            .stats_overview(Self::normalize_hours(hours).map(i64::from))
            .await
    }

    pub async fn get_stats_hourly(&self, hours: i32) -> anyhow::Result<Vec<StatsHourly>> {
        self.gw
            .storage
            .usage_stats()
            .stats_hourly(i64::from(hours.max(1)))
            .await
    }

    pub async fn get_stats_by_model(&self, hours: Option<i32>) -> anyhow::Result<Vec<ModelStats>> {
        self.gw
            .storage
            .usage_stats()
            .stats_by_model(Self::normalize_hours(hours).map(i64::from))
            .await
    }

    pub async fn get_stats_by_provider(
        &self,
        hours: Option<i32>,
    ) -> anyhow::Result<Vec<ProviderStats>> {
        self.gw
            .storage
            .usage_stats()
            .stats_by_provider(Self::normalize_hours(hours).map(i64::from))
            .await
    }

    pub async fn get_stats_by_api_key(
        &self,
        hours: Option<i32>,
    ) -> anyhow::Result<Vec<ApiKeyStats>> {
        self.gw
            .storage
            .usage_stats()
            .stats_by_api_key(Self::normalize_hours(hours).map(i64::from))
            .await
    }
}
