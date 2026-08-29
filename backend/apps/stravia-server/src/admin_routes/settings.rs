use super::*;

// ── Settings ──

pub(super) async fn get_setting(
    State(gw): State<Gateway>,
    Path(key): Path<String>,
) -> impl IntoResponse {
    match gw.admin().get_setting(&key).await {
        Ok(v) => Json(serde_json::json!({ "data": v })).into_response(),
        Err(e) => err(e),
    }
}

#[derive(Deserialize)]
pub(super) struct SettingBody {
    value: String,
}

pub(super) async fn set_setting(
    State(gw): State<Gateway>,
    Path(key): Path<String>,
    Json(body): Json<SettingBody>,
) -> impl IntoResponse {
    match gw.admin().set_setting(&key, &body.value).await {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => err(e),
    }
}
