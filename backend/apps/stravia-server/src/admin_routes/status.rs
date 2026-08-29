use super::*;

// ── Status ──

pub(super) async fn get_status() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "running",
    }))
}

pub(super) fn provider_model_err(error: anyhow::Error) -> axum::response::Response {
    if let Some(catalog_error) = error.downcast_ref::<CatalogError>() {
        let (status, code, message) = match catalog_error {
            CatalogError::ScopeRefresh { .. } => (
                StatusCode::BAD_GATEWAY,
                "CATALOG_SCOPE_REFRESH_FAILED",
                "Provider Catalog scope refresh failed. Provider Models were not changed; retry the operation.",
            ),
            CatalogError::ModelNotFound { .. } => (
                StatusCode::NOT_FOUND,
                "CATALOG_MODEL_NOT_FOUND",
                "Canonical Model was not found in the active catalog revision.",
            ),
            CatalogError::EntryNotFound { .. } => (
                StatusCode::NOT_FOUND,
                "CATALOG_ENTRY_NOT_FOUND",
                "Provider Catalog Entry was not found in the active catalog revision.",
            ),
        };
        return (
            status,
            Json(serde_json::json!({
                "code": code,
                "error": message,
            })),
        )
            .into_response();
    }
    let message = error.to_string();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&message)
        && let Some(code) = value.get("code").and_then(serde_json::Value::as_str)
    {
        let status = match code {
            "PROVIDER_MODEL_NOT_FOUND" => StatusCode::NOT_FOUND,
            "PROVIDER_MODEL_CONFLICT" => StatusCode::CONFLICT,
            _ => StatusCode::BAD_REQUEST,
        };
        return (
            status,
            Json(serde_json::json!({
                "code": code,
                "error": value.get("message").and_then(serde_json::Value::as_str)
                    .unwrap_or("Provider Model request failed"),
                "params": value.get("params").cloned().unwrap_or(serde_json::Value::Null),
            })),
        )
            .into_response();
    }
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": message })),
    )
        .into_response()
}

pub(super) fn err(e: anyhow::Error) -> axum::response::Response {
    Json(serde_json::json!({ "error": e.to_string() })).into_response()
}

pub(super) fn oauth_err(e: anyhow::Error) -> axum::response::Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": e.to_string() })),
    )
        .into_response()
}
