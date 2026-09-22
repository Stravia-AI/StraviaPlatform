use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use stravia_core::Gateway;
use stravia_core::plugin::ConfirmPluginUpdate;

const MAX_COMPONENT_BYTES: usize = 64 * 1024 * 1024;

pub(super) fn routes() -> Router<Gateway> {
    Router::new()
        .route("/vendor-plugins", get(list_vendor_plugins))
        .route(
            "/vendor-plugins/import",
            post(import_vendor_plugin).layer(DefaultBodyLimit::max(MAX_COMPONENT_BYTES)),
        )
        .route("/vendor-plugins/confirm", post(confirm_vendor_plugin))
        .route(
            "/vendor-plugins/{vendor_id}",
            delete(uninstall_vendor_plugin),
        )
        .route(
            "/vendor-plugins/{vendor_id}/restore",
            post(restore_vendor_plugin),
        )
        .layer(middleware::from_fn(no_store))
}

async fn list_vendor_plugins(State(gateway): State<Gateway>) -> Response {
    match gateway.admin().list_vendor_plugins().await {
        Ok(plugins) => plugin_json(plugins),
        Err(error) => plugin_error(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

async fn import_vendor_plugin(
    State(gateway): State<Gateway>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    if headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_none_or(|value| !value.eq_ignore_ascii_case("application/wasm"))
    {
        return error_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Content-Type must be application/wasm",
        );
    }

    let body = match body {
        Ok(body) => body,
        Err(rejection) => return rejection.into_response(),
    };
    if body.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "plugin component body must not be empty",
        );
    }

    match gateway.admin().preview_vendor_plugin(body.to_vec()).await {
        Ok(preview) => plugin_json(preview),
        Err(error) => plugin_error(StatusCode::BAD_REQUEST, error),
    }
}

async fn confirm_vendor_plugin(
    State(gateway): State<Gateway>,
    Json(input): Json<ConfirmPluginUpdate>,
) -> Response {
    match gateway.admin().confirm_vendor_plugin(input).await {
        Ok(plugin) => plugin_json(plugin),
        Err(error) => plugin_error(StatusCode::BAD_REQUEST, error),
    }
}

async fn uninstall_vendor_plugin(
    State(gateway): State<Gateway>,
    Path(vendor_id): Path<String>,
) -> Response {
    match gateway.admin().uninstall_vendor_plugin(&vendor_id).await {
        Ok(()) => plugin_json(()),
        Err(error) => plugin_error(StatusCode::BAD_REQUEST, error),
    }
}

async fn restore_vendor_plugin(
    State(gateway): State<Gateway>,
    Path(vendor_id): Path<String>,
) -> Response {
    match gateway
        .admin()
        .preview_builtin_vendor_plugin(&vendor_id)
        .await
    {
        Ok(preview) => plugin_json(preview),
        Err(error) => plugin_error(StatusCode::BAD_REQUEST, error),
    }
}

fn plugin_json<T>(value: T) -> Response
where
    T: serde::Serialize,
{
    Json(serde_json::json!({ "data": value })).into_response()
}

fn plugin_error(status: StatusCode, error: anyhow::Error) -> Response {
    error_response(status, error.to_string())
}

fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": message.into() }))).into_response()
}

async fn no_store(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
