use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use stravia_core::Gateway;

/// Authenticated admin export: Prometheus text exposition format version 0.0.4.
pub(super) async fn metrics(State(gateway): State<Gateway>) -> Response {
    let mut response = match gateway.admin().performance_metrics() {
        Some(body) => (StatusCode::OK, body).into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "performance recorder unavailable",
        )
            .into_response(),
    };
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        "text/plain; version=0.0.4; charset=utf-8"
            .parse()
            .expect("static MIME"),
    );
    attachment(response, "stravia-performance.prom")
}

/// 管理员导出本轮 Chrome Trace；磁盘历史读取不占用异步执行线程。
pub(super) async fn timeline(State(gateway): State<Gateway>) -> Response {
    let snapshot =
        tokio::task::spawn_blocking(move || gateway.admin().performance_timeline()).await;
    let response = match snapshot {
        Ok(snapshot) => axum::Json(snapshot).into_response(),
        Err(error) => {
            tracing::warn!(%error, "performance timeline export task failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "performance timeline unavailable",
            )
                .into_response()
        }
    };
    attachment(response, "stravia-performance-trace.json")
}

/// 管理员一次下载指标与时间线的 zip；读取磁盘历史、序列化与压缩均在阻塞线程完成。
pub(super) async fn export(State(gateway): State<Gateway>) -> Response {
    let archive =
        tokio::task::spawn_blocking(move || gateway.admin().performance_archive()).await;
    let response = match archive {
        Ok(Ok(Some(bytes))) => {
            ([(header::CONTENT_TYPE, "application/zip")], bytes).into_response()
        }
        Ok(Ok(None)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "performance recorder unavailable",
        )
            .into_response(),
        Ok(Err(error)) => {
            tracing::warn!(%error, "performance archive export failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "performance archive export failed",
            )
                .into_response()
        }
        Err(error) => {
            tracing::warn!(%error, "performance archive export task failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "performance archive unavailable",
            )
                .into_response()
        }
    };
    attachment(response, "stravia-performance.zip")
}

/// 性能导出一律作为不可缓存、不可嗅探的附件返回，失败响应也不例外。
fn attachment(mut response: Response, filename: &'static str) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_DISPOSITION,
        format!("attachment; filename=\"{filename}\"")
            .parse()
            .expect("static filename"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        "no-store".parse().expect("static policy"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        "nosniff".parse().expect("static policy"),
    );
    response
}
