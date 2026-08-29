use super::*;

// ── Providers ──

pub(super) fn provider_value(provider: Provider) -> serde_json::Value {
    serde_json::to_value(provider).expect("Provider serialization must succeed")
}

pub(super) async fn list_providers(State(gw): State<Gateway>) -> impl IntoResponse {
    match gw.admin().list_providers().await {
        Ok(providers) => Json(serde_json::json!({
            "data": providers.into_iter().map(provider_value).collect::<Vec<_>>()
        }))
        .into_response(),
        Err(e) => err(e),
    }
}

pub(super) async fn catalog_providers_handler(
    State(gw): State<Gateway>,
    headers: HeaderMap,
) -> axum::response::Response {
    let catalog = gw.admin().catalog_choices().await;
    catalog_json_response(&headers, &catalog.revision, &catalog)
}

pub(super) async fn canonical_models_handler(
    State(gw): State<Gateway>,
    headers: HeaderMap,
) -> axum::response::Response {
    let catalog = gw.provider_catalog.canonical_models().await;
    catalog_json_response(&headers, &catalog.revision, &catalog)
}

pub(super) async fn catalog_models_handler(
    State(gw): State<Gateway>,
    Path((provider_id, channel_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> axum::response::Response {
    match gw.provider_catalog.models(&provider_id, &channel_id).await {
        Ok(catalog) => catalog_json_response(&headers, &catalog.revision, &catalog),
        Err(error)
            if matches!(
                error.downcast_ref::<CatalogError>(),
                Some(CatalogError::ScopeRefresh { .. })
            ) =>
        {
            (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({
                    "code": "CATALOG_SCOPE_REFRESH_FAILED",
                    "error": "Provider Catalog scope refresh failed. Provider Models were not changed; retry the operation."
                })),
            )
                .into_response()
        }
        Err(_) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "code": "CATALOG_ENTRY_NOT_FOUND",
                "error": "Catalog provider or channel was not found."
            })),
        )
            .into_response(),
    }
}

pub(super) async fn catalog_refresh_handler(State(gw): State<Gateway>) -> axum::response::Response {
    match gw.provider_catalog.refresh().await {
        Ok(summary) => Json(summary).into_response(),
        Err(error) => {
            tracing::warn!(error = %error, "manual provider catalog refresh failed");
            (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({
                    "code": "CATALOG_REFRESH_FAILED",
                    "error": "Provider Catalog refresh failed. The last known good catalog remains active."
                })),
            )
                .into_response()
        }
    }
}

pub(super) async fn catalog_logo_handler(
    State(gw): State<Gateway>,
    Path(provider_id): Path<String>,
) -> axum::response::Response {
    match gw.provider_catalog.logo(&provider_id).await {
        Ok(body) => {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("image/svg+xml; charset=utf-8"),
            );
            headers.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, max-age=86400"),
            );
            headers.insert(
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'"),
            );
            (headers, body).into_response()
        }
        Err(_) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "code": "CATALOG_LOGO_UNAVAILABLE",
                "error": "Provider logo is unavailable."
            })),
        )
            .into_response(),
    }
}

pub(super) fn catalog_json_response<T: serde::Serialize>(
    request_headers: &HeaderMap,
    revision: &str,
    body: &T,
) -> axum::response::Response {
    let etag = format!("\"{revision}\"");
    if request_headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(',').any(|candidate| candidate.trim() == etag))
    {
        return StatusCode::NOT_MODIFIED.into_response();
    }
    let mut response = Json(body).into_response();
    if let Ok(value) = HeaderValue::from_str(&etag) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response
}

pub(super) async fn list_loaded_extensions(State(gw): State<Gateway>) -> impl IntoResponse {
    match gw.admin().list_loaded_extensions().await {
        Ok(v) => Json(serde_json::json!({ "data": v })).into_response(),
        Err(e) => err(e),
    }
}

pub(super) async fn list_vendor_metadata(State(gw): State<Gateway>) -> impl IntoResponse {
    match gw.admin().list_vendor_metadata().await {
        Ok(vendors) => Json(serde_json::json!({ "data": vendors })).into_response(),
        Err(e) => err(e),
    }
}

pub(super) async fn get_provider_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match gw.admin().get_provider(&id).await {
        Ok(v) => Json(serde_json::json!({ "data": provider_value(v) })).into_response(),
        Err(e) => err(e),
    }
}

pub(super) async fn create_provider_handler(
    State(gw): State<Gateway>,
    Json(input): Json<CreateProvider>,
) -> impl IntoResponse {
    match gw.admin().create_provider(input).await {
        Ok(v) => Json(serde_json::json!({ "data": provider_value(v) })).into_response(),
        Err(e) if e.to_string().contains("AUTH_SESSION_REQUIRED") => oauth_err(e),
        Err(e) if e.to_string().contains("refresh and select") => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "code": "CATALOG_FINGERPRINT_STALE",
                "error": "The selected Catalog channel changed. Refresh and select it again."
            })),
        )
            .into_response(),
        Err(e) => err(e),
    }
}

#[derive(Deserialize)]
pub(super) struct PreviewProviderBaseUrlRequest {
    vendor_id: String,
    #[serde(default)]
    adapter_credentials: std::collections::BTreeMap<String, String>,
    base_url: Option<String>,
}

pub(super) async fn preview_provider_base_url_handler(
    State(gw): State<Gateway>,
    Json(input): Json<PreviewProviderBaseUrlRequest>,
) -> impl IntoResponse {
    match gw.admin().preview_provider_base_url(
        &input.vendor_id,
        input.adapter_credentials,
        input.base_url.as_deref(),
    ) {
        Ok(base_url) => Json(serde_json::json!({
            "data": { "base_url": base_url }
        }))
        .into_response(),
        Err(error) => err(error),
    }
}

pub(super) async fn copy_provider_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
    options: Option<Json<CopyProviderOptions>>,
) -> impl IntoResponse {
    let options = options.map(|Json(options)| options).unwrap_or_default();
    match gw.admin().copy_provider_with_options(&id, options).await {
        Ok(v) => Json(serde_json::json!({ "data": provider_value(v) })).into_response(),
        Err(e) => err(e),
    }
}

pub(super) async fn update_provider_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
    Json(input): Json<UpdateProvider>,
) -> impl IntoResponse {
    match gw.admin().update_provider(&id, input).await {
        Ok(v) => Json(serde_json::json!({ "data": provider_value(v) })).into_response(),
        Err(e) => err(e),
    }
}

pub(super) async fn delete_provider_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match gw.admin().delete_provider(&id).await {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => err(e),
    }
}

pub(super) async fn test_provider_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match gw.admin().test_provider(&id).await {
        Ok(v) => Json(serde_json::json!({ "data": v })).into_response(),
        Err(e) => err(e),
    }
}

pub(super) async fn test_provider_models_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match gw.admin().test_provider_models(&id).await {
        Ok(v) => Json(serde_json::json!({ "data": v })).into_response(),
        Err(e) => err(e),
    }
}

pub(super) async fn provider_models_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
) -> axum::response::Response {
    match gw.admin().list_provider_models(&id).await {
        Ok(models) => Json(serde_json::json!({ "data": models })).into_response(),
        Err(error) => provider_model_err(error),
    }
}

pub(super) async fn sync_provider_models_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
) -> axum::response::Response {
    match gw.admin().sync_provider_models(&id).await {
        Ok(summary) => Json(serde_json::json!({ "data": summary })).into_response(),
        Err(error) => provider_model_err(error),
    }
}

pub(super) async fn prepare_provider_model_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
    Json(input): Json<PrepareProviderModelRequest>,
) -> axum::response::Response {
    match gw
        .admin()
        .prepare_provider_model(&id, &input.model_id, input.template_id.as_deref())
        .await
    {
        Ok(model) => Json(serde_json::json!({ "data": model })).into_response(),
        Err(error) => provider_model_err(error),
    }
}

pub(super) async fn get_provider_model_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
    Query(query): Query<ProviderModelQuery>,
) -> axum::response::Response {
    match gw.admin().get_provider_model(&id, &query.model).await {
        Ok(model) => Json(serde_json::json!({ "data": model })).into_response(),
        Err(error) => provider_model_err(error),
    }
}

pub(super) async fn create_provider_model_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
    Json(input): Json<CreateProviderModelRequest>,
) -> axum::response::Response {
    match gw
        .admin()
        .create_manual_provider_model(
            &id,
            &input.model_id,
            CreateManualProviderModel {
                metadata: input.metadata,
            },
        )
        .await
    {
        Ok(model) => (
            StatusCode::CREATED,
            Json(serde_json::json!({ "data": model })),
        )
            .into_response(),
        Err(error) => provider_model_err(error),
    }
}

pub(super) async fn update_provider_model_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
    Json(input): Json<UpdateProviderModelRequest>,
) -> axum::response::Response {
    match gw
        .admin()
        .update_provider_model(
            &id,
            &input.model_id,
            UpdateProviderModel {
                metadata: input.metadata,
                revision: input.revision,
            },
        )
        .await
    {
        Ok(model) => Json(serde_json::json!({ "data": model })).into_response(),
        Err(error) => provider_model_err(error),
    }
}

pub(super) async fn update_provider_model_selection_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
    Json(input): Json<UpdateProviderModelSelectionRequest>,
) -> axum::response::Response {
    match gw
        .admin()
        .update_provider_model_selection(
            &id,
            &input.model_id,
            UpdateProviderModelSelection {
                policy: input.policy,
                revision: input.revision,
            },
        )
        .await
    {
        Ok(model) => Json(serde_json::json!({ "data": model })).into_response(),
        Err(error) => provider_model_err(error),
    }
}

pub(super) async fn reimport_provider_model_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
    Json(input): Json<ProviderModelRevisionRequest>,
) -> axum::response::Response {
    match gw
        .admin()
        .reimport_provider_model(&id, &input.model_id, input.revision)
        .await
    {
        Ok(model) => Json(serde_json::json!({ "data": model })).into_response(),
        Err(error) => provider_model_err(error),
    }
}

pub(super) async fn delete_provider_model_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
    Query(query): Query<ProviderModelQuery>,
) -> axum::response::Response {
    match gw
        .admin()
        .delete_manual_provider_model(&id, &query.model)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => provider_model_err(error),
    }
}

#[derive(Deserialize)]
pub(super) struct ProviderModelQuery {
    model: String,
}

#[derive(Deserialize)]
pub(super) struct PrepareProviderModelRequest {
    model_id: String,
    template_id: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct CreateProviderModelRequest {
    model_id: String,
    metadata: serde_json::Value,
}

#[derive(Deserialize)]
pub(super) struct UpdateProviderModelRequest {
    model_id: String,
    metadata: serde_json::Value,
    revision: i64,
}

#[derive(Deserialize)]
pub(super) struct UpdateProviderModelSelectionRequest {
    model_id: String,
    policy: ProviderModelSelectionPolicy,
    revision: i64,
}

#[derive(Deserialize)]
pub(super) struct ProviderModelRevisionRequest {
    model_id: String,
    revision: i64,
}

pub(super) async fn provider_model_capabilities_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
    Query(query): Query<ModelCapabilitiesQuery>,
) -> impl IntoResponse {
    match gw.admin().get_model_capabilities(&id, &query.model).await {
        Ok(v) => Json(serde_json::json!({ "data": v })).into_response(),
        Err(e) => err(e),
    }
}

#[derive(Deserialize)]
pub(super) struct ModelCapabilitiesQuery {
    model: String,
}

#[derive(Deserialize)]
pub(super) struct InitOAuthSessionRequest {
    vendor: String,
    #[serde(default)]
    use_proxy: bool,
    callback_mode: OAuthCallbackMode,
    locale: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct CreateOAuthProviderRequest {
    session_id: String,
    input: CreateProvider,
}

pub(super) async fn init_oauth_session_handler(
    Extension(callbacks): Extension<OAuthCallbackManager>,
    Json(input): Json<InitOAuthSessionRequest>,
) -> impl IntoResponse {
    match callbacks
        .init_session(
            &input.vendor,
            input.use_proxy,
            input.callback_mode,
            input.locale.as_deref(),
        )
        .await
    {
        Ok(v) => Json(serde_json::json!({ "data": v })).into_response(),
        Err(e) => oauth_err(e),
    }
}

pub(super) async fn get_oauth_session_status_handler(
    State(gw): State<Gateway>,
    Extension(callbacks): Extension<OAuthCallbackManager>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match gw.admin().get_oauth_session_status(&id).await {
        Ok(v) => {
            if matches!(
                &v,
                AuthSessionStatusData::Ready { .. } | AuthSessionStatusData::Error { .. }
            ) {
                callbacks.release_if_terminal(&id).await;
            }
            Json(serde_json::json!({ "data": v })).into_response()
        }
        Err(e) => oauth_err(e),
    }
}

pub(super) async fn cancel_oauth_session_handler(
    Extension(callbacks): Extension<OAuthCallbackManager>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match callbacks.cancel_session(&id).await {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => oauth_err(e),
    }
}

pub(super) async fn complete_oauth_session_handler(
    State(gw): State<Gateway>,
    Extension(callbacks): Extension<OAuthCallbackManager>,
    Path(id): Path<String>,
    Json(input): Json<AuthExchangeInput>,
) -> impl IntoResponse {
    match gw.admin().complete_oauth_session(&id, input).await {
        Ok(v) => {
            callbacks.release_if_terminal(&id).await;
            Json(serde_json::json!({ "data": v })).into_response()
        }
        Err(e) => {
            if matches!(
                gw.admin().get_oauth_session_status(&id).await,
                Ok(AuthSessionStatusData::Error { .. })
            ) {
                callbacks.release_if_terminal(&id).await;
            }
            oauth_err(e)
        }
    }
}

#[derive(Deserialize)]
pub(super) struct UpdateOAuthSessionRequest {
    use_proxy: bool,
}

pub(super) async fn update_oauth_session_proxy_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
    Json(input): Json<UpdateOAuthSessionRequest>,
) -> impl IntoResponse {
    match gw
        .admin()
        .update_oauth_session_proxy(&id, input.use_proxy)
        .await
    {
        Ok(v) => Json(serde_json::json!({ "data": v })).into_response(),
        Err(e) => oauth_err(e),
    }
}

pub(super) async fn create_oauth_provider_handler(
    State(gw): State<Gateway>,
    Json(input): Json<CreateOAuthProviderRequest>,
) -> impl IntoResponse {
    match gw
        .admin()
        .create_provider_with_oauth_session(&input.session_id, input.input)
        .await
    {
        Ok(v) => Json(serde_json::json!({ "data": provider_value(v) })).into_response(),
        Err(e) => err(e),
    }
}

pub(super) async fn get_provider_oauth_status_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match gw.admin().get_provider_oauth_status(&id).await {
        Ok(v) => Json(serde_json::json!({ "data": v })).into_response(),
        Err(e) => err(e),
    }
}

pub(super) async fn reconnect_provider_oauth_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match gw.admin().reconnect_provider_oauth(&id).await {
        Ok(v) => Json(serde_json::json!({ "data": v })).into_response(),
        Err(e) => err(e),
    }
}

pub(super) async fn logout_provider_oauth_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match gw.admin().logout_provider_oauth(&id).await {
        Ok(v) => Json(serde_json::json!({ "data": v })).into_response(),
        Err(e) => err(e),
    }
}

#[derive(serde::Deserialize)]
pub(super) struct BindProviderOAuthRequest {
    session_id: String,
}

pub(super) async fn bind_provider_oauth_handler(
    State(gw): State<Gateway>,
    Path(id): Path<String>,
    Json(input): Json<BindProviderOAuthRequest>,
) -> impl IntoResponse {
    match gw
        .admin()
        .bind_provider_with_oauth_session(&id, &input.session_id)
        .await
    {
        Ok(v) => Json(serde_json::json!({ "data": v })).into_response(),
        Err(e) => err(e),
    }
}
