use axum::response::Response;
use stravia_core::admin::CredentialDiscoveryQuery;

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

fn unavailable(code: &'static str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({ "error": code, "code": code })),
    )
        .into_response()
}
