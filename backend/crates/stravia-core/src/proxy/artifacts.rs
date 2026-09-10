use std::time::Duration;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde::Deserialize;

use crate::Gateway;
use crate::proxy::security::{ClientCredential, Security};
use stravia_runtime_contract::agent::ArtifactPolicy;
use stravia_runtime_contract::artifact::ArtifactError;
use stravia_runtime_contract::artifact::ArtifactUploadRequest;
use stravia_runtime_contract::artifact::UploadedArtifactPart;

#[derive(Deserialize)]
pub struct CreateArtifactUpload {
    pub mime_type: String,
    pub size: u64,
}

#[derive(Deserialize)]
pub struct CompleteArtifactUpload {
    pub upload_token: String,
    pub parts: Vec<UploadedArtifactPart>,
}

pub async fn create_upload(
    State(gateway): State<Gateway>,
    headers: HeaderMap,
    Json(input): Json<CreateArtifactUpload>,
) -> Response {
    let principal = match required_principal(&gateway, &headers).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let Some(store) = gateway.artifact_store() else {
        return unavailable();
    };
    match store
        .create_upload(
            &principal,
            ArtifactUploadRequest {
                mime_type: input.mime_type,
                size: input.size,
                idle_ttl: Duration::from_secs(60 * 60),
                retention_ttl: Duration::from_secs(7 * 24 * 60 * 60),
                policy: ArtifactPolicy {
                    max_artifacts: 1,
                    max_bytes: stravia_runtime_contract::artifact::MAX_ARTIFACT_BYTES,
                    allowed_mime_types: vec!["*/*".into()],
                },
            },
        )
        .await
    {
        Ok(upload) => (StatusCode::CREATED, Json(upload)).into_response(),
        Err(error) => artifact_error(error),
    }
}

pub async fn upload_part(
    State(gateway): State<Gateway>,
    Path((upload_id, part_number)): Path<(String, u32)>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let principal = match required_principal(&gateway, &headers).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let Some(token) = headers
        .get("x-upload-token")
        .and_then(|value| value.to_str().ok())
        .filter(|token| !token.is_empty())
    else {
        return (StatusCode::UNAUTHORIZED, "missing x-upload-token").into_response();
    };
    let Some(store) = gateway.artifact_store() else {
        return unavailable();
    };
    match store
        .upload_part(
            &principal,
            &upload_id,
            token,
            part_number,
            Box::pin(
                body.into_data_stream()
                    .map(|chunk| chunk.map_err(|error| ArtifactError::Storage(error.to_string()))),
            ),
        )
        .await
    {
        Ok(part) => Json(part).into_response(),
        Err(error) => artifact_error(error),
    }
}

pub async fn complete_upload(
    State(gateway): State<Gateway>,
    Path(upload_id): Path<String>,
    headers: HeaderMap,
    Json(input): Json<CompleteArtifactUpload>,
) -> Response {
    let principal = match required_principal(&gateway, &headers).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let Some(store) = gateway.artifact_store() else {
        return unavailable();
    };
    match store
        .complete_upload(&principal, &upload_id, &input.upload_token, &input.parts)
        .await
    {
        Ok(artifact) => Json(serde_json::json!({
            "id": artifact.id,
            "mime_type": artifact.mime_type,
            "size": artifact.size,
            "reference": artifact.reference(),
        }))
        .into_response(),
        Err(error) => artifact_error(error),
    }
}

async fn required_principal(
    gateway: &Gateway,
    headers: &HeaderMap,
) -> Result<stravia_runtime_contract::Principal, Response> {
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
            .map_err(|_| {
                (StatusCode::UNAUTHORIZED, "upload principal is unavailable").into_response()
            })?;
        return Ok(principal);
    }
    security
        .required_principal(&credential)
        .await
        .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid api key").into_response())
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
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "Artifact storage is unavailable",
    )
        .into_response()
}

fn artifact_error(error: ArtifactError) -> Response {
    let status = match error {
        ArtifactError::Invalid(_) => StatusCode::BAD_REQUEST,
        ArtifactError::NotFound => StatusCode::NOT_FOUND,
        ArtifactError::Forbidden => StatusCode::FORBIDDEN,
        ArtifactError::Unauthorized => StatusCode::UNAUTHORIZED,
        ArtifactError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, error.to_string()).into_response()
}
