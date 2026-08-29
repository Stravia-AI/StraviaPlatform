use super::*;

pub(super) fn validate_upload_request(
    request: &ArtifactUploadRequest,
) -> Result<(), ArtifactError> {
    if request.size == 0 || request.size > MAX_ARTIFACT_BYTES {
        return Err(ArtifactError::Invalid(format!(
            "Artifact size must be between 1 and {MAX_ARTIFACT_BYTES} bytes"
        )));
    }
    if request.policy.max_artifacts == 0
        || request.size > request.policy.max_bytes
        || !request
            .policy
            .allowed_mime_types
            .iter()
            .any(|allowed| mime_type_matches(allowed, &request.mime_type))
    {
        return Err(ArtifactError::Invalid(
            "Artifact violates the Agent Definition policy".into(),
        ));
    }
    if request.idle_ttl.is_zero() || request.retention_ttl.is_zero() {
        return Err(ArtifactError::Invalid(
            "Artifact TTL must be greater than zero".into(),
        ));
    }
    Ok(())
}

pub(super) fn mime_type_matches(allowed: &str, actual: &str) -> bool {
    allowed == "*/*"
        || allowed.eq_ignore_ascii_case(actual)
        || allowed.strip_suffix("/*").is_some_and(|prefix| {
            actual
                .get(..prefix.len())
                .is_some_and(|actual_prefix| actual_prefix.eq_ignore_ascii_case(prefix))
                && actual.as_bytes().get(prefix.len()) == Some(&b'/')
        })
}

pub(super) fn validate_staging_quota(
    active_uploads: i64,
    staged_bytes: i64,
    requested_bytes: u64,
) -> Result<(), ArtifactError> {
    let staged_bytes = u64::try_from(staged_bytes).unwrap_or(u64::MAX);
    if active_uploads >= MAX_PRINCIPAL_STAGING_UPLOADS
        || staged_bytes.saturating_add(requested_bytes) > MAX_PRINCIPAL_STAGING_BYTES
    {
        return Err(ArtifactError::Invalid(
            "Artifact staging quota exceeded".into(),
        ));
    }
    Ok(())
}
