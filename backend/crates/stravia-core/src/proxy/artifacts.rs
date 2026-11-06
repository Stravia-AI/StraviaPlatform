use axum::Json;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;

use crate::Gateway;
use crate::agent::artifact::artifact_error_mapping;
use crate::proxy::security::{ClientCredential, Security};
use stravia_runtime_contract::artifact::ArtifactError;

/// 单请求上传：请求体就是文件字节，`Content-Type` 就是文件 MIME。
///
/// 鉴权在读取请求体之前完成；限制、暂存额度与内容身份全部由 `ArtifactStore::ingest` 执行。
pub async fn upload(State(gateway): State<Gateway>, headers: HeaderMap, body: Body) -> Response {
    let principal = match required_principal(&gateway, &headers).await {
        Ok(principal) => principal,
        Err(response) => return *response,
    };
    let mime_type = match declared_mime(&headers) {
        Ok(mime_type) => mime_type,
        Err(message) => return unsupported_media_type(message),
    };
    let Some(store) = gateway.artifact_store() else {
        return unavailable();
    };
    let retention = match crate::media::ingest::retention(&gateway).await {
        Ok(retention) => retention,
        Err(error) => return artifact_error(error),
    };
    let size = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    let stream = body
        .into_data_stream()
        .map(|chunk| chunk.map_err(|error| ArtifactError::Storage(error.to_string())));
    match store
        .ingest(&principal, mime_type, size, Box::pin(stream), retention)
        .await
    {
        Ok(artifact) => (
            StatusCode::CREATED,
            Json(serde_json::json!({
                "path": artifact.reference(),
                "mime_type": artifact.mime_type,
                "size": artifact.size,
            })),
        )
            .into_response(),
        Err(error) => artifact_error(error),
    }
}

/// `Content-Type` 必须是文件自身的 MIME。curl 的 `--data-binary` 在没有 `-H` 时默认发送
/// `application/x-www-form-urlencoded`，multipart 则会把边界写进文件内容，两者都拒绝。
fn declared_mime(headers: &HeaderMap) -> Result<&str, &'static str> {
    let mime_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or("Content-Type is required and must be the MIME type of the uploaded file")?;
    let essence = mime_type.split(';').next().unwrap_or_default().trim();
    let is_multipart = essence
        .get(.."multipart/".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("multipart/"));
    if essence.eq_ignore_ascii_case("application/x-www-form-urlencoded") || is_multipart {
        return Err("Content-Type must be the MIME type of the uploaded file; \
             application/x-www-form-urlencoded and multipart/* are not accepted");
    }
    Ok(mime_type)
}

fn unsupported_media_type(message: &str) -> Response {
    json_error(
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "STRAVIA_UNSUPPORTED_MEDIA_TYPE",
        "unsupported_media_type",
        message,
    )
}

/// 与网关错误相同的 `{"error":{message,type,code}}` 外形；不借用模型路由的错误变体，
/// 否则 404 会被渲染成“找不到模型”。
fn json_error(
    status: StatusCode,
    kind: &'static str,
    diagnostic_code: &'static str,
    message: &str,
) -> Response {
    let mut response = (
        status,
        Json(serde_json::json!({
            "error": {
                "message": message,
                "type": kind,
                "code": status.as_u16(),
            }
        })),
    )
        .into_response();
    response
        .extensions_mut()
        .insert(crate::interaction_observation::FailureDiagnostic::platform(
            diagnostic_code,
            message,
            status.as_u16(),
        ));
    response
}

async fn required_principal(
    gateway: &Gateway,
    headers: &HeaderMap,
) -> Result<stravia_runtime_contract::Principal, Box<Response>> {
    let credential = ClientCredential::from_inference_headers(headers);
    let security = Security::new(gateway.storage.auth());
    if let Some(key) = credential
        .secret()
        .filter(|key| key.starts_with("stravia_upload_"))
    {
        let principal = gateway
            .upload_grants
            .authenticate(key)
            .map_err(artifact_error)?;
        security
            .authorize_principal_capability(&principal)
            .await
            .map_err(|_| artifact_error(ArtifactError::Unauthorized))?;
        return Ok(principal);
    }
    security
        .required_principal(&credential)
        .await
        .map_err(|_| Box::new(artifact_error(ArtifactError::Unauthorized)))
}

pub async fn download(State(gateway): State<Gateway>, Path(token): Path<String>) -> Response {
    let Some(store) = gateway.artifact_store() else {
        return unavailable();
    };
    let reader = match store.read_download(&token).await {
        Ok(reader) => reader,
        Err(error) => return artifact_error(error),
    };
    let mime = match header::HeaderValue::from_str(&reader.artifact.mime_type) {
        Ok(mime) => mime,
        Err(_) => return artifact_error(ArtifactError::Storage("invalid stored MIME type".into())),
    };
    let size = header::HeaderValue::from_str(&reader.artifact.size.to_string())
        .expect("decimal Artifact size is a valid header");
    let body = match reader.source {
        stravia_runtime_contract::artifact::ArtifactSource::LocalPath(path) => {
            let file = match tokio::fs::File::open(path).await {
                Ok(file) => file,
                Err(error) => return artifact_error(ArtifactError::Storage(error.to_string())),
            };
            Body::from_stream(futures::stream::try_unfold(
                (file, reader.guard),
                |(mut file, guard)| async move {
                    use tokio::io::AsyncReadExt;
                    let mut buffer = vec![0; 64 * 1024];
                    let count = file.read(&mut buffer).await?;
                    if count == 0 {
                        Ok::<_, std::io::Error>(None)
                    } else {
                        buffer.truncate(count);
                        Ok(Some((bytes::Bytes::from(buffer), (file, guard))))
                    }
                },
            ))
        }
        stravia_runtime_contract::artifact::ArtifactSource::HttpsUrl(url) => {
            let response = match gateway.http_client.get(url).send().await {
                Ok(response) if response.status().is_success() => response,
                Ok(_) => {
                    return artifact_error(ArtifactError::Storage(
                        "Artifact backend download failed".into(),
                    ));
                }
                Err(_) => {
                    return artifact_error(ArtifactError::Storage(
                        "Artifact backend is unavailable".into(),
                    ));
                }
            };
            let guard = reader.guard;
            Body::from_stream(response.bytes_stream().map(move |chunk| {
                let _hold = &guard;
                chunk.map_err(|_| std::io::Error::other("Artifact backend stream failed"))
            }))
        }
    };
    let mut response = body.into_response();
    response.headers_mut().insert(header::CONTENT_TYPE, mime);
    response.headers_mut().insert(header::CONTENT_LENGTH, size);
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("private, no-store"),
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        header::HeaderValue::from_static("attachment"),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    response
}

fn unavailable() -> Response {
    artifact_error(ArtifactError::Storage(
        "Artifact storage is unavailable".into(),
    ))
}

/// 所有 Artifact HTTP 处理函数共用的 JSON 错误映射；状态码分类见 `artifact_error_mapping`。
fn artifact_error(error: ArtifactError) -> Response {
    let mapping = artifact_error_mapping(&error);
    if matches!(error, ArtifactError::Storage(_)) {
        tracing::warn!(reason = %mapping.diagnostic_message, "Artifact request failed");
    }
    let status = StatusCode::from_u16(mapping.status).expect("Artifact error has a valid status");
    let (kind, diagnostic_code) = match error {
        ArtifactError::Invalid(_) => ("STRAVIA_BAD_REQUEST", "artifact_invalid"),
        ArtifactError::NotFound => ("STRAVIA_NOT_FOUND", "artifact_not_found"),
        ArtifactError::Forbidden => ("STRAVIA_FORBIDDEN", "artifact_forbidden"),
        ArtifactError::Unauthorized => ("STRAVIA_AUTH_ERROR", "artifact_unauthorized"),
        ArtifactError::Storage(_) => ("STRAVIA_INTERNAL_ERROR", "artifact_storage_failed"),
    };
    json_error(status, kind, diagnostic_code, &mapping.public_message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_error_mapping_preserves_classification_without_exposing_credentials() {
        for (error, status) in [
            (
                ArtifactError::Invalid("bad input".into()),
                StatusCode::BAD_REQUEST,
            ),
            (ArtifactError::NotFound, StatusCode::NOT_FOUND),
            (ArtifactError::Forbidden, StatusCode::FORBIDDEN),
            (ArtifactError::Unauthorized, StatusCode::UNAUTHORIZED),
        ] {
            assert_eq!(artifact_error_mapping(&error).status, status.as_u16());
        }

        let secret = "artifact-error-secret";
        let mapping = artifact_error_mapping(&ArtifactError::Storage(format!(
            "database is locked at postgres://admin:{secret}@db.example/internal"
        )));

        assert_eq!(mapping.status, StatusCode::INTERNAL_SERVER_ERROR.as_u16());
        assert!(!mapping.public_message.contains(secret));
        assert!(!mapping.diagnostic_message.contains(secret));
        assert!(mapping.diagnostic_message.contains("database is locked"));
    }
}
