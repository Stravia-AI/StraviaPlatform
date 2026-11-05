use axum::response::Response;
use stravia_core::admin::identity::AdminSession;
use stravia_core::admin::{
    CredentialDiscoveryError, CredentialDiscoveryQuery, CredentialDiscoveryReuseQuery,
    CredentialVisibility,
};
use stravia_credential_protection::{CustomRuleError, CustomRuleInput, DisabledRulesError};

use super::*;

// 不实现 Debug：提交文本不得进入请求诊断或错误日志。
#[derive(Deserialize)]
pub(super) struct CredentialTestInput {
    text: String,
}

pub(super) async fn credential_rules(State(gateway): State<Gateway>) -> Response {
    match gateway.admin().credential_protection_rules().await {
        Ok(data) => Json(serde_json::json!({ "data": data })).into_response(),
        Err(_) => unavailable("credential_rules_unavailable"),
    }
}

#[derive(Deserialize)]
pub(super) struct DisabledRulesInput {
    rule_ids: Vec<String>,
}

pub(super) async fn set_disabled_credential_rules(
    State(gateway): State<Gateway>,
    Json(input): Json<DisabledRulesInput>,
) -> Response {
    match gateway
        .admin()
        .set_credential_disabled_rules(input.rule_ids)
        .await
    {
        Ok(rule_ids) => {
            Json(serde_json::json!({ "data": { "rule_ids": rule_ids } })).into_response()
        }
        Err(DisabledRulesError::Unknown(rule_id)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "code": "credential_rule_unknown",
                "error": DisabledRulesError::Unknown(rule_id.clone()).to_string(),
                "params": { "rule_id": rule_id },
            })),
        )
            .into_response(),
        Err(DisabledRulesError::Storage) => unavailable("credential_disabled_rules_unavailable"),
    }
}

pub(super) async fn test_credentials(
    State(gateway): State<Gateway>,
    Json(input): Json<CredentialTestInput>,
) -> Response {
    match gateway.admin().test_credential_protection(input.text).await {
        Ok(matches) => Json(serde_json::json!({ "data": { "matches": matches } })).into_response(),
        Err(_) => unavailable("credential_test_failed"),
    }
}

pub(super) async fn credential_discoveries(
    State(gateway): State<Gateway>,
    Query(query): Query<CredentialDiscoveryQuery>,
) -> Response {
    discovery_response(
        gateway
            .admin()
            .credential_protection_discoveries(query)
            .await,
    )
}

// 不实现 Debug：请求参数不进入诊断。
#[derive(Deserialize)]
pub(super) struct CredentialHitQuery {
    #[serde(default)]
    reveal: bool,
}

pub(super) async fn credential_discovery_hit(
    State(gateway): State<Gateway>,
    Extension(session): Extension<AdminSession>,
    Path((id, index)): Path<(String, String)>,
    Query(query): Query<CredentialHitQuery>,
) -> Response {
    let Some(visibility) = credential_visibility(query.reveal, &session) else {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "credential_reveal_forbidden",
                "code": "credential_reveal_forbidden",
            })),
        )
            .into_response();
    };
    discovery_response(
        gateway
            .admin()
            .credential_protection_discovery_hit(&id, &index, visibility)
            .await,
    )
}

/// 只有显式 `reveal=true` 且会话允许时才返回明文；其余一律遮蔽。
fn credential_visibility(reveal: bool, session: &AdminSession) -> Option<CredentialVisibility> {
    match reveal {
        false => Some(CredentialVisibility::Masked),
        true => session
            .may_reveal_credentials()
            .then_some(CredentialVisibility::Revealed),
    }
}

pub(super) async fn credential_discovery_reuses(
    State(gateway): State<Gateway>,
    Path(id): Path<String>,
    Query(query): Query<CredentialDiscoveryReuseQuery>,
) -> Response {
    discovery_response(
        gateway
            .admin()
            .credential_protection_discovery_reuses(&id, query)
            .await,
    )
}

pub(super) async fn list_custom_credential_rules(State(gateway): State<Gateway>) -> Response {
    custom_rule_response(gateway.admin().credential_custom_rules().await)
}

// 不实现 Debug：规则文本即用户要保护的秘密，不得进入请求诊断。
pub(super) async fn create_custom_credential_rule(
    State(gateway): State<Gateway>,
    Json(input): Json<CustomRuleInput>,
) -> Response {
    custom_rule_response(gateway.admin().create_credential_custom_rule(input).await)
}

pub(super) async fn update_custom_credential_rule(
    State(gateway): State<Gateway>,
    Path(id): Path<String>,
    Json(input): Json<CustomRuleInput>,
) -> Response {
    custom_rule_response(
        gateway
            .admin()
            .update_credential_custom_rule(&id, input)
            .await,
    )
}

pub(super) async fn delete_custom_credential_rule(
    State(gateway): State<Gateway>,
    Path(id): Path<String>,
) -> Response {
    match gateway.admin().delete_credential_custom_rule(&id).await {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(error) => custom_rule_error(error),
    }
}

fn discovery_response<T: serde::Serialize>(result: anyhow::Result<T>) -> Response {
    match result {
        Ok(data) => Json(serde_json::json!({ "data": data })).into_response(),
        Err(error) => match error.downcast::<CredentialDiscoveryError>() {
            Ok(error) => discovery_error(error),
            Err(_) => unavailable("observation_unavailable"),
        },
    }
}

fn discovery_error(error: CredentialDiscoveryError) -> Response {
    let (status, code) = match error {
        CredentialDiscoveryError::InvalidCursor => (
            StatusCode::BAD_REQUEST,
            "credential_discovery_cursor_invalid",
        ),
        CredentialDiscoveryError::InvalidReference => (
            StatusCode::BAD_REQUEST,
            "credential_discovery_reference_invalid",
        ),
        CredentialDiscoveryError::NotFound => {
            (StatusCode::NOT_FOUND, "credential_discovery_not_found")
        }
    };
    (
        status,
        Json(serde_json::json!({ "code": code, "error": error.to_string() })),
    )
        .into_response()
}

fn custom_rule_response<T: serde::Serialize>(result: Result<T, CustomRuleError>) -> Response {
    match result {
        Ok(data) => Json(serde_json::json!({ "data": data })).into_response(),
        Err(error) => custom_rule_error(error),
    }
}

fn custom_rule_error(error: CustomRuleError) -> Response {
    let (status, code, params) = match error {
        CustomRuleError::Invalid { field, reason } => (
            StatusCode::BAD_REQUEST,
            "custom_credential_rule_invalid",
            serde_json::json!({ "field": field, "reason": reason }),
        ),
        CustomRuleError::NotFound => (
            StatusCode::NOT_FOUND,
            "custom_credential_rule_not_found",
            serde_json::Value::Null,
        ),
        CustomRuleError::Storage => (
            StatusCode::SERVICE_UNAVAILABLE,
            "custom_credential_rule_unavailable",
            serde_json::Value::Null,
        ),
    };
    (
        status,
        Json(serde_json::json!({ "code": code, "error": error.to_string(), "params": params })),
    )
        .into_response()
}

fn unavailable(code: &'static str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({ "error": code, "code": code })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use stravia_core::admin::identity::AdminSession;
    use stravia_core::config::GatewayConfig;
    use tower::ServiceExt;

    fn session(role: &str) -> AdminSession {
        AdminSession {
            session_id: "test-session".into(),
            username: None,
            role: role.into(),
        }
    }

    #[test]
    fn only_administrators_may_reveal_while_everyone_can_read_the_masked_view() {
        assert_eq!(
            credential_visibility(false, &session("admin")),
            Some(CredentialVisibility::Masked)
        );
        assert_eq!(
            credential_visibility(true, &session("admin")),
            Some(CredentialVisibility::Revealed)
        );
        assert_eq!(
            credential_visibility(false, &session("viewer")),
            Some(CredentialVisibility::Masked)
        );
        assert_eq!(credential_visibility(true, &session("viewer")), None);
    }

    #[tokio::test]
    async fn reveal_from_a_non_privileged_session_is_forbidden() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let gateway = Gateway::new(GatewayConfig {
            data_dir: directory.path().to_path_buf(),
            ..Default::default()
        })
        .await?;
        let app =
            super::super::create_unprotected_router(gateway).layer(Extension(session("viewer")));
        let response = app
            .oneshot(
                Request::get("/api/v1/reversible-redaction/discoveries/1.0/hits/0?reveal=true")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await?)?;
        assert_eq!(body["error"], "credential_reveal_forbidden");
        assert_eq!(body["code"], "credential_reveal_forbidden");
        Ok(())
    }
}
