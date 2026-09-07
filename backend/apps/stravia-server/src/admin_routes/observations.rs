use std::convert::Infallible;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde::Deserialize;
use stravia_core::Gateway;
use stravia_core::admin::{
    BundleRequest, BundleResourceKind, ForestQuery, ObservationUpdate, RejectionQuery,
};

#[derive(Debug, Deserialize)]
pub(super) struct EventQuery {
    #[serde(default)]
    after: i64,
}

#[derive(Debug, Deserialize)]
pub(super) struct DebugUpdate {
    enabled: bool,
    #[serde(default)]
    confirmed: bool,
}

#[derive(Debug, Deserialize)]
pub(super) struct BundleTicketInput {
    through_sequence: Option<i64>,
}

pub(super) async fn interaction_forest(
    State(gateway): State<Gateway>,
    Query(query): Query<ForestQuery>,
) -> Response {
    match gateway.admin().observation_forest(query).await {
        Ok(data) => Json(serde_json::json!({ "data": data })).into_response(),
        Err(_) => observation_unavailable(),
    }
}

pub(super) async fn interaction_detail(
    State(gateway): State<Gateway>,
    Path(id): Path<String>,
    Query(filters): Query<ForestQuery>,
) -> Response {
    match gateway.admin().observation_interaction(&id, filters).await {
        Ok(Some(data)) => Json(serde_json::json!({ "data": data })).into_response(),
        Ok(None) => not_found(),
        Err(_) => observation_unavailable(),
    }
}

pub(super) async fn rejection_list(
    State(gateway): State<Gateway>,
    Query(query): Query<RejectionQuery>,
) -> Response {
    match gateway.admin().observation_rejections(query).await {
        Ok(data) => Json(serde_json::json!({ "data": data })).into_response(),
        Err(_) => observation_unavailable(),
    }
}

pub(super) async fn rejection_detail(
    State(gateway): State<Gateway>,
    Path(id): Path<String>,
) -> Response {
    match gateway.admin().observation_rejection(&id).await {
        Ok(Some(data)) => Json(serde_json::json!({ "data": data })).into_response(),
        Ok(None) => not_found(),
        Err(_) => observation_unavailable(),
    }
}

pub(super) async fn observation_events(
    State(gateway): State<Gateway>,
    Query(query): Query<EventQuery>,
) -> Response {
    let stream = gateway
        .admin()
        .observation_subscribe(query.after)
        .map(|update| {
            let event = match update {
                ObservationUpdate::Event(observation) => Event::default()
                    .event("observation")
                    .id(observation.sequence.to_string())
                    .data(
                        serde_json::to_string(&observation).expect("ObservationEvent serializes"),
                    ),
                ObservationUpdate::ResetRequired { snapshot_sequence } => {
                    Event::default().event("reset_required").data(
                        serde_json::json!({
                            "reset_required": true,
                            "snapshot_sequence": snapshot_sequence,
                        })
                        .to_string(),
                    )
                }
            };
            Ok::<_, Infallible>(event)
        });

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

pub(super) async fn observation_debug(State(gateway): State<Gateway>) -> Response {
    Json(serde_json::json!({ "data": gateway.admin().observation_debug() })).into_response()
}

pub(super) async fn update_observation_debug(
    State(gateway): State<Gateway>,
    Json(update): Json<DebugUpdate>,
) -> Response {
    if update.enabled && !update.confirmed {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "debug_confirmation_required",
                "code": "debug_confirmation_required",
            })),
        )
            .into_response();
    }
    Json(serde_json::json!({
        "data": gateway.admin().set_observation_debug(update.enabled)
    }))
    .into_response()
}

pub(super) async fn clear_observation_history(State(gateway): State<Gateway>) -> Response {
    match gateway.admin().clear_observation_history().await {
        Ok(data) => Json(serde_json::json!({ "data": data })).into_response(),
        Err(_) => observation_unavailable(),
    }
}

pub(super) async fn issue_interaction_bundle_ticket(
    State(gateway): State<Gateway>,
    Path(id): Path<String>,
    Json(input): Json<BundleTicketInput>,
) -> Response {
    issue_bundle_ticket(
        gateway,
        BundleResourceKind::Interaction,
        id,
        input.through_sequence,
    )
    .await
}

pub(super) async fn issue_rejection_bundle_ticket(
    State(gateway): State<Gateway>,
    Path(id): Path<String>,
    Json(input): Json<BundleTicketInput>,
) -> Response {
    issue_bundle_ticket(
        gateway,
        BundleResourceKind::RejectedRequest,
        id,
        input.through_sequence,
    )
    .await
}

async fn issue_bundle_ticket(
    gateway: Gateway,
    kind: BundleResourceKind,
    resource_id: String,
    through_sequence: Option<i64>,
) -> Response {
    let mut response = match gateway
        .admin()
        .issue_observation_bundle_ticket(BundleRequest {
            kind,
            resource_id,
            through_sequence,
        })
        .await
    {
        Ok(data) => Json(serde_json::json!({ "data": data })).into_response(),
        Err(_) => bundle_unavailable(),
    };
    apply_download_safety_headers(response.headers_mut());
    response
}

pub(super) async fn consume_bundle_ticket(
    State(gateway): State<Gateway>,
    Path(ticket): Path<String>,
) -> Response {
    let mut response = match gateway
        .admin()
        .consume_observation_bundle_ticket(&ticket)
        .await
    {
        Ok(stream) => {
            let mut response = Response::new(Body::from_stream(stream));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/zip"),
            );
            response.headers_mut().insert(
                header::CONTENT_DISPOSITION,
                HeaderValue::from_static(
                    "attachment; filename=\"stravia-observation-debug-bundle.zip\"",
                ),
            );
            response
        }
        Err(_) => bundle_unavailable(),
    };
    apply_download_safety_headers(response.headers_mut());
    response
}

fn apply_download_safety_headers(headers: &mut HeaderMap) {
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
}

fn bundle_unavailable() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({
            "error": "bundle_unavailable",
            "code": "bundle_unavailable",
        })),
    )
        .into_response()
}

fn observation_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "error": "observation_unavailable",
            "code": "observation_unavailable",
        })),
    )
        .into_response()
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "not_found", "code": "not_found" })),
    )
        .into_response()
}
