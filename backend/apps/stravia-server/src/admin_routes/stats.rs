use super::*;

// ── Stats ──

#[derive(Deserialize, Default)]
pub(super) struct StatsRangeParams {
    hours: Option<i32>,
}

pub(super) async fn stats_overview(
    State(gw): State<Gateway>,
    Query(params): Query<StatsRangeParams>,
) -> impl IntoResponse {
    match gw.admin().get_stats_overview(params.hours).await {
        Ok(v) => Json(serde_json::json!({ "data": v })).into_response(),
        Err(e) => err(e),
    }
}

#[derive(Deserialize)]
pub(super) struct HourlyParams {
    #[serde(default = "default_hours")]
    hours: i32,
}

pub(super) fn default_hours() -> i32 {
    24
}

pub(super) async fn stats_hourly(
    State(gw): State<Gateway>,
    Query(params): Query<HourlyParams>,
) -> impl IntoResponse {
    match gw.admin().get_stats_hourly(params.hours).await {
        Ok(v) => Json(serde_json::json!({ "data": v })).into_response(),
        Err(e) => err(e),
    }
}

pub(super) async fn stats_by_model(
    State(gw): State<Gateway>,
    Query(params): Query<StatsRangeParams>,
) -> impl IntoResponse {
    match gw.admin().get_stats_by_model(params.hours).await {
        Ok(v) => Json(serde_json::json!({ "data": v })).into_response(),
        Err(e) => err(e),
    }
}

pub(super) async fn stats_by_provider(
    State(gw): State<Gateway>,
    Query(params): Query<StatsRangeParams>,
) -> impl IntoResponse {
    match gw.admin().get_stats_by_provider(params.hours).await {
        Ok(v) => Json(serde_json::json!({ "data": v })).into_response(),
        Err(e) => err(e),
    }
}

pub(super) async fn stats_by_api_key(
    State(gw): State<Gateway>,
    Query(params): Query<StatsRangeParams>,
) -> impl IntoResponse {
    match gw.admin().get_stats_by_api_key(params.hours).await {
        Ok(v) => Json(serde_json::json!({ "data": v })).into_response(),
        Err(e) => err(e),
    }
}
