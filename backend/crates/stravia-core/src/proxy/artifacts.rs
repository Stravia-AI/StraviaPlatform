use std::time::Duration;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde::Deserialize;

use crate::Gateway;
use crate::agent::{ArtifactError, ArtifactPolicy, ArtifactUploadRequest, UploadedArtifactPart};
use crate::proxy::security::{ClientCredential, Security};

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
                    max_bytes: crate::agent::MAX_ARTIFACT_BYTES,
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
        Ok(artifact) => Json(artifact).into_response(),
        Err(error) => artifact_error(error),
    }
}

async fn required_principal(
    gateway: &Gateway,
    headers: &HeaderMap,
) -> Result<crate::hook::Principal, Response> {
    Security::new(gateway.storage.auth())
        .required_principal(&ClientCredential::from_inference_headers(headers))
        .await
        .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid api key").into_response())
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
