use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Extension, Json};
use futures::stream;
use std::{convert::Infallible, sync::Arc};
use stravia_core::Gateway;
use stravia_core::admin::identity::AdminSession;
use stravia_core::plugin::ConfirmPluginUpdate;
use stravia_core::plugin::marketplace::{MarketError, PreviewInstallRequest};

const MAX_COMPONENT_BYTES: usize = 64 * 1024 * 1024;

pub(super) fn routes() -> Router<Gateway> {
    Router::new()
        .route("/plugin-sources", get(list_sources).post(confirm_source))
        .route("/plugin-sources/preview", post(preview_source))
        .route(
            "/plugin-sources/{source_id}",
            axum::routing::patch(patch_source).delete(delete_source),
        )
        .route(
            "/plugin-sources/{source_id}/trust-preview",
            post(trust_preview),
        )
        .route(
            "/plugin-sources/{source_id}/trust-confirm",
            post(trust_confirm),
        )
        .route("/plugin-marketplace", get(marketplace_view))
        .route("/plugin-marketplace/refresh", post(refresh_marketplace))
        .route("/plugin-marketplace/preview-install", post(preview_install))
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

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SourcePreviewInput {
    url: String,
    fingerprint: Option<String>,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviewIdInput {
    preview_id: String,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SourcePatchInput {
    enabled: Option<bool>,
    include_prerelease: Option<bool>,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RefreshInput {
    source_id: Option<String>,
}

fn market_result<T: serde::Serialize>(result: Result<T, MarketError>) -> Response {
    match result {
        Ok(data) => plugin_json(data),
        Err(error) => market_error(error),
    }
}
fn market_error(error: MarketError) -> Response {
    let status = match error.code.as_str() {
        "preview_expired" => StatusCode::GONE,
        "rate_limited" => StatusCode::TOO_MANY_REQUESTS,
        "network" => StatusCode::BAD_GATEWAY,
        "resource_limit" => StatusCode::PAYLOAD_TOO_LARGE,
        _ => StatusCode::BAD_REQUEST,
    };
    (status, Json(serde_json::json!({"error": error}))).into_response()
}
async fn list_sources(State(gw): State<Gateway>) -> Response {
    market_result(gw.marketplace.list_sources().await)
}
async fn preview_source(
    State(gw): State<Gateway>,
    Extension(session): Extension<AdminSession>,
    Json(input): Json<SourcePreviewInput>,
) -> Response {
    market_result(
        gw.marketplace
            .source_preview(
                &session.session_id,
                &input.url,
                input.fingerprint.as_deref(),
            )
            .await,
    )
}
async fn confirm_source(
    State(gw): State<Gateway>,
    Extension(session): Extension<AdminSession>,
    Json(input): Json<PreviewIdInput>,
) -> Response {
    market_result(
        gw.marketplace
            .source_confirm(&session.session_id, &input.preview_id)
            .await,
    )
}
async fn patch_source(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
    Json(input): Json<SourcePatchInput>,
) -> Response {
    market_result(
        gw.marketplace
            .patch_source(&id, input.enabled, input.include_prerelease)
            .await,
    )
}
async fn delete_source(State(gw): State<Gateway>, Path(id): Path<String>) -> Response {
    market_result(gw.marketplace.delete_source(&id).await)
}
async fn trust_preview(
    State(gw): State<Gateway>,
    Extension(session): Extension<AdminSession>,
    Path(id): Path<String>,
) -> Response {
    market_result(gw.marketplace.trust_preview(&session.session_id, &id).await)
}
async fn trust_confirm(
    State(gw): State<Gateway>,
    Extension(session): Extension<AdminSession>,
    Path(id): Path<String>,
    Json(input): Json<PreviewIdInput>,
) -> Response {
    market_result(
        gw.marketplace
            .trust_confirm(&session.session_id, &id, &input.preview_id)
            .await,
    )
}
async fn marketplace_view(State(gw): State<Gateway>) -> Response {
    market_result(gw.marketplace.view(&gw).await)
}
async fn refresh_marketplace(
    State(gw): State<Gateway>,
    Json(input): Json<RefreshInput>,
) -> Response {
    market_result(
        gw.marketplace
            .refresh(&gw, input.source_id.as_deref())
            .await,
    )
}
async fn preview_install(
    State(gw): State<Gateway>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Json(input): Json<PreviewInstallRequest>,
) -> Response {
    let wants_stream = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .any(|v| v.trim().split(';').next() == Some("text/event-stream"))
        });
    if !wants_stream {
        return market_result(
            gw.marketplace
                .preview_install(&gw, &session.session_id, input, None)
                .await,
        );
    }
    let (tx, rx) = tokio::sync::mpsc::channel(32);
    let progress = Arc::new(move |progress| {
        let _ = tx.try_send(progress);
    });
    let future = Box::pin(async move {
        gw.marketplace
            .preview_install(&gw, &session.session_id, input, Some(progress))
            .await
    });
    // The preparation future belongs to the response stream, so dropping the stream cancels it.
    let events = stream::unfold((Some(future), rx), |(future, mut rx)| async move {
        let mut future = future?;
        tokio::select! {
            result = &mut future => {
                let (name, data) = match result {
                    Ok(preview) => ("preview", serde_json::to_string(&preview).unwrap_or_default()),
                    Err(error) => ("error", serde_json::to_string(&error).unwrap_or_default()),
                };
                Some((Ok::<_, Infallible>(Event::default().event(name).data(data)), (None, rx)))
            }
            Some(progress) = rx.recv() => {
                Some((Ok(Event::default().event("progress").data(serde_json::to_string(&progress).unwrap_or_default())), (Some(future), rx)))
            }
        }
    });
    Sse::new(events).into_response()
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
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    let compressed = match content_type {
        Some(value) if value.eq_ignore_ascii_case("application/wasm") => false,
        Some(value) if value.eq_ignore_ascii_case("application/zstd") => true,
        _ => {
            return error_response(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Content-Type must be application/wasm or application/zstd",
            );
        }
    };
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

    let zstd_magic = body.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]);
    if compressed != zstd_magic {
        return error_response(
            StatusCode::BAD_REQUEST,
            "Content-Type does not match component format",
        );
    }
    match gateway
        .admin()
        .preview_vendor_plugin_import(body.to_vec(), compressed)
        .await
    {
        Ok(preview) => plugin_json(preview),
        Err(error) => plugin_error(StatusCode::BAD_REQUEST, error),
    }
}

async fn confirm_vendor_plugin(
    State(gateway): State<Gateway>,
    Extension(session): Extension<AdminSession>,
    Json(input): Json<ConfirmPluginUpdate>,
) -> Response {
    if let Err(error) = gateway
        .marketplace
        .authorize_install_preview(&session.session_id, &input.preview_id)
    {
        return market_error(error);
    }
    match gateway.admin().confirm_vendor_plugin(input).await {
        Ok(plugin) => plugin_json(plugin),
        Err(error) => match stravia_core::plugin::marketplace::confirm_error(&error) {
            Some(error) => market_error(error),
            None => plugin_error(StatusCode::BAD_REQUEST, error),
        },
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
    if response.status().is_client_error() || response.status().is_server_error() {
        let is_json = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("application/json"));
        if !is_json {
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap_or_default();
            let message = String::from_utf8_lossy(&body);
            response = (
                status,
                Json(serde_json::json!({
                    "error": { "code": "invalid_request", "message": message }
                })),
            )
                .into_response();
        }
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
