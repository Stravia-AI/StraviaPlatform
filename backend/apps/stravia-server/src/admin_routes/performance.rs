use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use stravia_core::Gateway;

/// Authenticated admin export: Prometheus text exposition format version 0.0.4.
pub(super) async fn metrics(State(gateway): State<Gateway>) -> Response {
    let response = match gateway.admin().performance_metrics() {
        Some(body) => (StatusCode::OK, body).into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "performance recorder unavailable",
        )
            .into_response(),
    };
    let mut response = response;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        "text/plain; version=0.0.4; charset=utf-8"
            .parse()
            .expect("static MIME"),
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        "attachment; filename=\"stravia-performance.prom\""
            .parse()
            .expect("static filename"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        "no-store".parse().expect("static policy"),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        "nosniff".parse().expect("static policy"),
    );
    response
}

/// 管理员导出有界 Chrome Trace 时间线，关闭 Debug 后仍可下载已有数据。
pub(super) async fn timeline(State(gateway): State<Gateway>) -> Response {
    let mut response = axum::Json(gateway.admin().performance_timeline()).into_response();
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        "attachment; filename=\"stravia-performance-trace.json\""
            .parse()
            .expect("static filename"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        "no-store".parse().expect("static policy"),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        "nosniff".parse().expect("static policy"),
    );
    response
}
