use axum::response::Response;
use stravia_core::admin::CredentialDiscoveryQuery;
use stravia_credential_protection::{CustomRuleError, CustomRuleInput};

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
    match gateway
        .admin()
        .credential_protection_discoveries(query)
        .await
    {
        Ok(data) => Json(serde_json::json!({ "data": data })).into_response(),
        Err(_) => unavailable("observation_unavailable"),
    }
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
