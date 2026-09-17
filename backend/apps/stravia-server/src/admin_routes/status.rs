use super::*;

// ── Status ──

pub(super) async fn get_status() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "running",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// 统一把类型化 CatalogError 映射为带 code/params 的 HTTP 响应,
/// 供 provider 创建、模型目录等管理面接口共用。
pub(super) fn catalog_error_response(error: &CatalogError) -> axum::response::Response {
    let (status, code, message, params) = match error {
        CatalogError::ScopeRefresh { provider_id, .. } => (
            StatusCode::BAD_GATEWAY,
            "CATALOG_SCOPE_REFRESH_FAILED",
            "Provider Catalog scope refresh failed. Provider Models were not changed; retry the operation.",
            serde_json::json!({ "provider_id": provider_id }),
        ),
        CatalogError::ModelNotFound { id } => (
            StatusCode::NOT_FOUND,
            "CATALOG_MODEL_NOT_FOUND",
            "Canonical Model was not found in the active catalog revision.",
            serde_json::json!({ "model_id": id }),
        ),
        CatalogError::EntryNotFound {
            provider_id,
            model_id,
        } => (
            StatusCode::NOT_FOUND,
            "CATALOG_ENTRY_NOT_FOUND",
            "Provider Catalog Entry was not found in the active catalog revision.",
            serde_json::json!({ "provider_id": provider_id, "model_id": model_id }),
        ),
        CatalogError::ProviderNotFound { provider_id } => (
            StatusCode::NOT_FOUND,
            "CATALOG_PROVIDER_NOT_FOUND",
            "Catalog provider was not found in the active catalog revision.",
            serde_json::json!({ "provider_id": provider_id }),
        ),
        CatalogError::ChannelNotFound {
            provider_id,
            channel_id,
        } => (
            StatusCode::NOT_FOUND,
            "CATALOG_CHANNEL_NOT_FOUND",
            "Catalog channel was not found in the active catalog revision.",
            serde_json::json!({ "provider_id": provider_id, "channel_id": channel_id }),
        ),
        CatalogError::ChannelChanged {
            provider_id,
            channel_id,
        } => (
            StatusCode::CONFLICT,
            "CATALOG_FINGERPRINT_STALE",
            "The selected Catalog channel changed. Refresh and select it again.",
            serde_json::json!({ "provider_id": provider_id, "channel_id": channel_id }),
        ),
    };
    (
        status,
        Json(serde_json::json!({
            "code": code,
            "error": message,
            "params": params,
        })),
    )
        .into_response()
}

pub(super) fn provider_model_err(error: anyhow::Error) -> axum::response::Response {
    if let Some(catalog_error) = error.downcast_ref::<CatalogError>() {
        return catalog_error_response(catalog_error);
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
    if let Some(catalog_error) = e.downcast_ref::<CatalogError>() {
        return catalog_error_response(catalog_error);
    }
    Json(serde_json::json!({ "error": e.to_string() })).into_response()
}

pub(super) fn oauth_err(e: anyhow::Error) -> axum::response::Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": e.to_string() })),
    )
        .into_response()
}
