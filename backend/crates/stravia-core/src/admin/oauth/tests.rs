use super::*;

fn credential(connection_id: &str, access_token: &str, status_version: i32) -> OAuthCredential {
    OAuthCredential {
        provider_id: "provider".to_string(),
        connection_id: connection_id.to_string(),
        driver_key: "codex".to_string(),
        scheme: "oauth".to_string(),
        access_token: access_token.to_string(),
        refresh_token: Some("refresh".to_string()),
        expires_at: Some("2000-01-01T00:00:00Z".to_string()),
        resource_url: None,
        subject_id: None,
        scopes: "[]".to_string(),
        meta: "{}".to_string(),
        status: "connected".to_string(),
        status_version,
        last_error: None,
        last_refresh_at: None,
        created_at: "created".to_string(),
        updated_at: "updated".to_string(),
    }
}

#[test]
fn snapshot_generation_identity_ignores_token_and_status_version_changes() {
    let snapshot = credential("generation-a", "expired", 1);
    let current = credential("generation-a", "fresh", 3);
    assert_ne!(snapshot, current);
    assert!(same_oauth_connection_generation(&snapshot, Some(&current)));

    let reconnected = credential("generation-b", "fresh", 0);
    assert!(!same_oauth_connection_generation(
        &snapshot,
        Some(&reconnected)
    ));
    assert!(!same_oauth_connection_generation(&snapshot, None));
}
