use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OverviewParams {
    start_at: Option<i64>,
    end_at: Option<i64>,
    provider_ids: Option<String>,
    api_key_ids: Option<String>,
}

pub(super) async fn stats_overview(
    State(gw): State<Gateway>,
    Query(params): Query<OverviewParams>,
) -> impl IntoResponse {
    let window = StatsWindow {
        start_at: params.start_at,
        end_at: params.end_at,
    };
    let filter = match StatsFilter::from_query_params(
        params.provider_ids.as_deref(),
        params.api_key_ids.as_deref(),
    ) {
        Ok(filter) => filter,
        Err(error) => return stats_error(error.into()),
    };
    match gw.admin().get_stats_overview(window, filter).await {
        Ok(v) => Json(serde_json::json!({ "data": v })).into_response(),
        Err(e) => stats_error(e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SeriesParams {
    start_at: Option<i64>,
    end_at: Option<i64>,
    bucket: Option<i64>,
    provider_ids: Option<String>,
    api_key_ids: Option<String>,
}

pub(super) async fn stats_series(
    State(gw): State<Gateway>,
    Query(params): Query<SeriesParams>,
) -> impl IntoResponse {
    let window = StatsWindow {
        start_at: params.start_at,
        end_at: params.end_at,
    };
    let filter = match StatsFilter::from_query_params(
        params.provider_ids.as_deref(),
        params.api_key_ids.as_deref(),
    ) {
        Ok(filter) => filter,
        Err(error) => return stats_error(error.into()),
    };
    match gw
        .admin()
        .get_stats_series(window, filter, params.bucket)
        .await
    {
        Ok(v) => Json(serde_json::json!({ "data": v })).into_response(),
        Err(e) => stats_error(e),
    }
}

pub(super) async fn stats_by_model(
    State(gw): State<Gateway>,
    Query(window): Query<StatsWindow>,
) -> impl IntoResponse {
    match gw.admin().get_stats_by_model(window).await {
        Ok(v) => Json(serde_json::json!({ "data": v })).into_response(),
        Err(e) => stats_error(e),
    }
}

pub(super) async fn stats_by_provider(
    State(gw): State<Gateway>,
    Query(window): Query<StatsWindow>,
) -> impl IntoResponse {
    match gw.admin().get_stats_by_provider(window).await {
        Ok(v) => Json(serde_json::json!({ "data": v })).into_response(),
        Err(e) => stats_error(e),
    }
}

pub(super) async fn stats_by_api_key(
    State(gw): State<Gateway>,
    Query(window): Query<StatsWindow>,
) -> impl IntoResponse {
    match gw.admin().get_stats_by_api_key(window).await {
        Ok(v) => Json(serde_json::json!({ "data": v })).into_response(),
        Err(e) => stats_error(e),
    }
}

fn stats_error(error: anyhow::Error) -> axum::response::Response {
    if let Some(error) = error.downcast_ref::<StatsQueryError>() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": error.to_string(),
                "code": "invalid_stats_query",
            })),
        )
            .into_response();
    }
    tracing::warn!("usage statistics query failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": "Usage statistics could not be loaded",
            "code": "stats_query_failed",
        })),
    )
        .into_response()
}
