use super::*;

// ── Logs ──

#[derive(Deserialize, Default)]
pub(super) struct LogQueryParams {
    limit: Option<i64>,
    offset: Option<i64>,
    provider: Option<String>,
    model: Option<String>,
    status_min: Option<i32>,
    status_max: Option<i32>,
    api_key: Option<String>,
}

pub(super) async fn get_log_handler(
    State(gw): State<Gateway>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    match gw.admin().get_log(&id).await {
        Ok(Some(v)) => Json(serde_json::json!({ "data": v })).into_response(),
        Ok(None) => (
            axum::http::StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "not found" })),
        )
            .into_response(),
        Err(e) => err(e),
    }
}

pub(super) async fn query_logs_handler(
    State(gw): State<Gateway>,
    Query(params): Query<LogQueryParams>,
) -> impl IntoResponse {
    let q = LogQuery {
        limit: params.limit,
        offset: params.offset,
        provider: params.provider,
        model: params.model,
        status_min: params.status_min,
        status_max: params.status_max,
        api_key: params.api_key,
    };
    match gw.admin().query_logs(q).await {
        Ok(v) => Json(serde_json::json!({ "data": v })).into_response(),
        Err(e) => err(e),
    }
}

pub(super) async fn clear_logs_handler(State(gw): State<Gateway>) -> impl IntoResponse {
    match gw.admin().clear_logs().await {
        Ok(deleted) => Json(serde_json::json!({ "data": { "deleted": deleted } })).into_response(),
        Err(e) => err(e),
    }
}
