use std::collections::BTreeMap;

use axum::{Json, http::StatusCode, response::IntoResponse};
use stravia_core::connect_client_apply::{ConnectClientApplyInput, preview_connect_client_apply};

pub(super) async fn preview_connect_client_handler(
    Json(input): Json<ConnectClientApplyInput>,
) -> axum::response::Response {
    let environment = std::env::vars().collect::<BTreeMap<_, _>>();
    match preview_connect_client_apply(&input, &environment) {
        Ok(plan) => Json(serde_json::json!({ "data": plan })).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "code": error.code,
                "error": error.message,
                "params": { "path": error.path },
            })),
        )
            .into_response(),
    }
}
