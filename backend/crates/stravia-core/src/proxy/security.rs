//! Canonical client credential policy for inference, model visibility, MCP, and advanced capabilities.
//!
//! The module normalizes transport credentials, establishes a Principal, and
//! authorizes the post-Hook Model. It returns typed outcomes and leaves
//! lifecycle ordering, context mutation, logging, and transport rendering to
//! its callers.

use axum::http::{HeaderMap, header};
use chrono::{NaiveDateTime, Utc};

use crate::db::models::Route;
use crate::error::{AccessDenial, AuthFailure, GatewayError};
use crate::storage::traits::{ApiKeyAccessRecord, AuthAccessStore};

/// A normalized client credential. Its secret is intentionally opaque and is
/// never exposed through formatting traits or typed outcomes.
pub(crate) struct ClientCredential {
    secret: Option<String>,
}

impl ClientCredential {
    pub(crate) fn from_inference_headers(headers: &HeaderMap) -> Self {
        Self {
            secret: extract_api_key(headers),
        }
    }

    pub(crate) fn from_mcp_headers(headers: &HeaderMap) -> Self {
        let secret = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| {
                let mut parts = value.split_whitespace();
                let scheme = parts.next()?;
                let token = parts.next()?;
                (scheme.eq_ignore_ascii_case("Bearer") && parts.next().is_none())
                    .then(|| token.to_owned())
            });
        Self { secret }
    }

    fn secret(&self) -> Option<&str> {
        self.secret.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelAccessGrant {
    pub(crate) api_key_id: Option<String>,
    pub(crate) api_key_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthenticatedPrincipal {
    pub(crate) principal: crate::hook::Principal,
    pub(crate) api_key_name: String,
    pub(crate) concurrency_limit: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WebSearchAccessGrant {
    pub(crate) transparent_injection_enabled: bool,
}

pub(crate) struct Security<'a> {
    auth: Option<&'a dyn AuthAccessStore>,
}

impl<'a> Security<'a> {
    pub(crate) fn new(auth: Option<&'a dyn AuthAccessStore>) -> Self {
        Self { auth }
    }

    pub(crate) async fn required_principal(
        &self,
        credential: &ClientCredential,
    ) -> Result<crate::hook::Principal, GatewayError> {
        Ok(self.authenticated_principal(credential).await?.principal)
    }

    pub(crate) async fn authenticated_principal(
        &self,
        credential: &ClientCredential,
    ) -> Result<AuthenticatedPrincipal, GatewayError> {
        let raw_key = credential.secret().ok_or(GatewayError::Unauthorized {
            reason: AuthFailure::Missing,
        })?;
        let auth = self.auth.ok_or_else(|| {
            GatewayError::internal(anyhow::anyhow!("API key authentication is unavailable"))
        })?;
        let key = auth
            .find_api_key(raw_key)
            .await
            .map_err(auth_storage_error)?
            .ok_or(GatewayError::Unauthorized {
                reason: AuthFailure::Invalid,
            })?;
        validate_key_state(&key)?;
        Ok(AuthenticatedPrincipal {
            principal: crate::hook::Principal::new(key.id),
            api_key_name: key.name,
            concurrency_limit: key.concurrency_limit,
        })
    }

    pub(crate) async fn visible_model_ids(
        &self,
        credential: &ClientCredential,
    ) -> Result<Vec<String>, GatewayError> {
        let raw_key = credential.secret().ok_or(GatewayError::Unauthorized {
            reason: AuthFailure::Missing,
        })?;
        let auth = self.auth.ok_or_else(|| {
            GatewayError::internal(anyhow::anyhow!("API key authentication is unavailable"))
        })?;
        let key = auth
            .find_api_key(raw_key)
            .await
            .map_err(auth_storage_error)?
            .ok_or(GatewayError::Unauthorized {
                reason: AuthFailure::Invalid,
            })?;
        validate_key_state(&key)?;
        auth.list_bound_model_ids(&key.id)
            .await
            .map_err(auth_storage_error)
    }

    pub(crate) async fn authorize_principal_model(
        &self,
        principal: &crate::hook::Principal,
        model: &Route,
    ) -> Result<ModelAccessGrant, GatewayError> {
        let key = self.principal_key(principal).await?;
        self.authorize_key_model(&key, model).await
    }

    pub(crate) async fn authorize_principal_capability(
        &self,
        principal: &crate::hook::Principal,
    ) -> Result<ModelAccessGrant, GatewayError> {
        let key = self.principal_key(principal).await?;
        validate_key_state(&key)?;
        Ok(ModelAccessGrant {
            api_key_id: Some(key.id),
            api_key_name: Some(key.name),
        })
    }

    pub(crate) async fn authorize_principal_web_search(
        &self,
        principal: &crate::hook::Principal,
    ) -> Result<WebSearchAccessGrant, GatewayError> {
        let key = self.principal_key(principal).await?;
        validate_key_state(&key)?;
        Ok(WebSearchAccessGrant {
            transparent_injection_enabled: key.transparent_injection_enabled
                && key.inject_web_search,
        })
    }

    pub(crate) async fn media_transparent_injection_enabled(
        &self,
        principal: &crate::hook::Principal,
    ) -> Result<bool, GatewayError> {
        let key = self.principal_key(principal).await?;
        validate_key_state(&key)?;
        Ok(key.transparent_injection_enabled && key.inject_media_understanding)
    }

    async fn principal_key(
        &self,
        principal: &crate::hook::Principal,
    ) -> Result<ApiKeyAccessRecord, GatewayError> {
        let id = principal.api_key_id();
        let Some(auth) = self.auth else {
            return Err(GatewayError::Unauthorized {
                reason: AuthFailure::Invalid,
            });
        };
        auth.find_api_key_by_id(id)
            .await
            .map_err(auth_storage_error)?
            .ok_or(GatewayError::Unauthorized {
                reason: AuthFailure::Invalid,
            })
    }

    async fn authorize_key_model(
        &self,
        key: &ApiKeyAccessRecord,
        model: &Route,
    ) -> Result<ModelAccessGrant, GatewayError> {
        validate_key_state(key)?;
        let Some(auth) = self.auth else {
            return Err(GatewayError::Unauthorized {
                reason: AuthFailure::Invalid,
            });
        };
        if !auth
            .model_access_allowed(&key.id, &model.id)
            .await
            .map_err(auth_storage_error)?
        {
            return Err(GatewayError::Forbidden {
                reason: AccessDenial::ModelNotAllowed,
            });
        }
        Ok(ModelAccessGrant {
            api_key_id: Some(key.id.clone()),
            api_key_name: Some(key.name.clone()),
        })
    }
}

fn validate_key_state(key: &ApiKeyAccessRecord) -> Result<(), GatewayError> {
    if !key.is_enabled {
        return Err(GatewayError::Forbidden {
            reason: AccessDenial::Custom("api key disabled".into()),
        });
    }
    if let Some(expires_at) = key.expires_at.as_deref() {
        match is_key_expired(expires_at) {
            Ok(true) => {
                return Err(GatewayError::Unauthorized {
                    reason: AuthFailure::Expired,
                });
            }
            Ok(false) => {}
            Err(()) => {
                return Err(GatewayError::Unauthorized {
                    reason: AuthFailure::Invalid,
                });
            }
        }
    }
    Ok(())
}

fn auth_storage_error(error: anyhow::Error) -> GatewayError {
    GatewayError::internal(anyhow::anyhow!("auth db error: {error}"))
}

/// Extract a client API key using the Inference credential profile.
fn extract_api_key(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    {
        let mut parts = value.splitn(2, char::is_whitespace);
        let scheme = parts.next().unwrap_or_default();
        let token = parts.next().unwrap_or_default().trim();
        if scheme.eq_ignore_ascii_case("bearer") && !token.is_empty() {
            return Some(token.to_string());
        }
    }

    for header_name in ["x-api-key", "x-goog-api-key"] {
        if let Some(key) = headers
            .get(header_name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            return Some(key.to_string());
        }
    }

    None
}

pub(crate) fn is_key_expired(expires_at: &str) -> Result<bool, ()> {
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(expires_at) {
        return Ok(parsed.with_timezone(&Utc) <= Utc::now());
    }
    NaiveDateTime::parse_from_str(expires_at, "%Y-%m-%d %H:%M:%S")
        .map(|parsed| parsed.and_utc() <= Utc::now())
        .map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use axum::http::{HeaderMap, HeaderValue, header};

    use super::{ClientCredential, Security, validate_key_state};
    use crate::db::models::Route;
    use crate::error::{AccessDenial, AuthFailure, GatewayError};
    use crate::hook::Principal;
    use crate::storage::traits::{ApiKeyAccessRecord, AuthAccessStore};

    #[test]
    fn inference_profile_accepts_bearer_and_trims_token() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer   sk-openai"),
        );

        assert_eq!(
            ClientCredential::from_inference_headers(&headers).secret(),
            Some("sk-openai")
        );
    }

    #[test]
    fn inference_profile_accepts_anthropic_header() {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static(" sk-anthropic "));

        assert_eq!(
            ClientCredential::from_inference_headers(&headers).secret(),
            Some("sk-anthropic")
        );
    }

    #[test]
    fn inference_profile_accepts_google_genai_header() {
        let mut headers = HeaderMap::new();
        headers.insert("x-goog-api-key", HeaderValue::from_static(" sk-google "));

        assert_eq!(
            ClientCredential::from_inference_headers(&headers).secret(),
            Some("sk-google")
        );
    }

    #[test]
    fn malformed_api_key_expiration_fails_closed() {
        let mut store = TestStore::valid();
        let key = store.key.as_mut().expect("test key");
        key.expires_at = Some("not-a-date".into());
        assert!(matches!(
            validate_key_state(key),
            Err(GatewayError::Unauthorized {
                reason: AuthFailure::Invalid
            })
        ));
    }

    #[tokio::test]
    async fn web_search_authorization_returns_the_injection_grant() {
        let store = TestStore::valid();
        let grant = Security::new(Some(&store))
            .authorize_principal_web_search(&Principal::new("key-id"))
            .await
            .expect("Web Search access");

        assert!(grant.transparent_injection_enabled);
    }

    #[tokio::test]
    async fn web_search_authorization_uses_selection_only_for_injection() {
        let mut store = TestStore::valid();
        store.key.as_mut().expect("test key").inject_web_search = false;

        let grant = Security::new(Some(&store))
            .authorize_principal_web_search(&Principal::new("key-id"))
            .await
            .expect("valid principal");
        assert!(!grant.transparent_injection_enabled);
    }
    #[tokio::test]
    async fn capability_model_authorization_uses_platform_gate_not_direct_binding() {
        let mut store = TestStore::valid();
        store.binding = false;

        let grant = Security::new(Some(&store))
            .authorize_principal_capability(&Principal::new("key-id"))
            .await
            .expect("capability-owned hidden Model");

        assert_eq!(grant.api_key_id.as_deref(), Some("key-id"));
    }

    #[tokio::test]
    async fn media_injection_requires_the_master_and_media_selection() {
        let mut store = TestStore::valid();
        store
            .key
            .as_mut()
            .expect("test key")
            .inject_media_understanding = true;
        assert!(
            Security::new(Some(&store))
                .media_transparent_injection_enabled(&Principal::new("key-id"))
                .await
                .expect("valid principal")
        );

        store
            .key
            .as_mut()
            .expect("test key")
            .transparent_injection_enabled = false;
        assert!(
            !Security::new(Some(&store))
                .media_transparent_injection_enabled(&Principal::new("key-id"))
                .await
                .expect("valid principal")
        );
    }
    struct TestStore {
        key: Option<ApiKeyAccessRecord>,
        binding: bool,
        visible_model_ids: Vec<String>,
        fail_lookup: bool,
        fail_visibility: bool,
    }

    impl TestStore {
        fn valid() -> Self {
            Self {
                key: Some(ApiKeyAccessRecord {
                    id: "key-id".into(),
                    name: "Test key".into(),
                    is_enabled: true,
                    expires_at: None,
                    concurrency_limit: None,
                    inject_media_understanding: false,
                    transparent_injection_enabled: true,
                    inject_web_search: true,
                }),
                binding: true,
                visible_model_ids: vec!["protected-model-id".into()],
                fail_lookup: false,
                fail_visibility: false,
            }
        }

        fn expired() -> Self {
            let mut store = Self::valid();
            store.key.as_mut().expect("test key").expires_at = Some("2000-01-01T00:00:00Z".into());
            store
        }
    }

    #[async_trait]
    impl AuthAccessStore for TestStore {
        async fn find_api_key(&self, _raw_key: &str) -> anyhow::Result<Option<ApiKeyAccessRecord>> {
            if self.fail_lookup {
                anyhow::bail!("lookup unavailable");
            }
            Ok(self.key.clone())
        }

        async fn find_api_key_by_id(&self, id: &str) -> anyhow::Result<Option<ApiKeyAccessRecord>> {
            if self.fail_lookup {
                anyhow::bail!("lookup unavailable");
            }
            Ok(self.key.clone().filter(|key| key.id == id))
        }

        async fn model_access_allowed(
            &self,
            _api_key_id: &str,
            _model_id: &str,
        ) -> anyhow::Result<bool> {
            Ok(self.visible_model_ids.is_empty() || self.binding)
        }

        async fn list_bound_model_ids(&self, _api_key_id: &str) -> anyhow::Result<Vec<String>> {
            if self.fail_visibility {
                anyhow::bail!("visibility unavailable");
            }
            Ok(self.visible_model_ids.clone())
        }
    }

    fn protected_model() -> Route {
        Route {
            id: "protected-model-id".into(),
            model_id: "protected-model".into(),
            display_name: None,
            balance: "traffic_equalization".into(),
            target_provider: String::new(),
            target_model: String::new(),
            is_enabled: true,
            created_at: "2000-01-01T00:00:00Z".into(),
            targets: Vec::new(),
            supported_thinking_levels: sqlx::types::Json(Vec::new()),
            context_window: None,
            output_max_tokens: None,
            supports_image_input: false,
        }
    }

    fn credential() -> ClientCredential {
        ClientCredential {
            secret: Some("client-secret".into()),
        }
    }

    #[tokio::test]
    async fn model_visibility_requires_a_valid_key() {
        let credential = credential();
        let valid = TestStore::valid();
        assert_eq!(
            Security::new(Some(&valid))
                .visible_model_ids(&credential)
                .await
                .expect("valid key visibility"),
            ["protected-model-id"]
        );

        let mut invalid = TestStore::valid();
        invalid.key = None;
        assert!(matches!(
            Security::new(Some(&invalid))
                .visible_model_ids(&credential)
                .await,
            Err(GatewayError::Unauthorized {
                reason: AuthFailure::Invalid
            })
        ));

        let mut disabled = TestStore::valid();
        disabled.key.as_mut().expect("test key").is_enabled = false;
        assert!(matches!(
            Security::new(Some(&disabled))
                .visible_model_ids(&credential)
                .await,
            Err(GatewayError::Forbidden { .. })
        ));

        let expired = TestStore::expired();
        assert!(matches!(
            Security::new(Some(&expired))
                .visible_model_ids(&credential)
                .await,
            Err(GatewayError::Unauthorized {
                reason: AuthFailure::Expired
            })
        ));

        assert!(matches!(
            Security::new(Some(&valid))
                .visible_model_ids(&ClientCredential { secret: None })
                .await,
            Err(GatewayError::Unauthorized {
                reason: AuthFailure::Missing
            })
        ));
        assert!(
            Security::new(None)
                .visible_model_ids(&credential)
                .await
                .is_err()
        );

        let mut lookup_unavailable = TestStore::valid();
        lookup_unavailable.fail_lookup = true;
        assert!(
            Security::new(Some(&lookup_unavailable))
                .visible_model_ids(&credential)
                .await
                .is_err()
        );
        let mut visibility_unavailable = TestStore::valid();
        visibility_unavailable.fail_visibility = true;
        assert!(
            Security::new(Some(&visibility_unavailable))
                .visible_model_ids(&credential)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn principal_model_authorization_requires_a_live_bound_principal() {
        let mut invalid = TestStore::valid();
        invalid.key = None;
        let mut disabled = TestStore::valid();
        disabled.key.as_mut().expect("test key").is_enabled = false;
        let expired = TestStore::expired();
        let principal = Principal::new("key-id");

        assert!(matches!(
            Security::new(Some(&invalid))
                .authorize_principal_model(&principal, &protected_model())
                .await,
            Err(GatewayError::Unauthorized {
                reason: AuthFailure::Invalid
            })
        ));
        assert!(matches!(
            Security::new(Some(&disabled))
                .authorize_principal_model(&principal, &protected_model())
                .await,
            Err(GatewayError::Forbidden { .. })
        ));
        assert!(matches!(
            Security::new(Some(&expired))
                .authorize_principal_model(&principal, &protected_model())
                .await,
            Err(GatewayError::Unauthorized {
                reason: AuthFailure::Expired
            })
        ));

        let mut unbound = TestStore::valid();
        unbound.binding = false;
        assert!(matches!(
            Security::new(Some(&unbound))
                .authorize_principal_model(&principal, &protected_model())
                .await,
            Err(GatewayError::Forbidden {
                reason: AccessDenial::ModelNotAllowed
            })
        ));

        let valid = TestStore::valid();
        let grant = Security::new(Some(&valid))
            .authorize_principal_model(&principal, &protected_model())
            .await
            .expect("bound credential is authorized");
        assert_eq!(grant.api_key_id.as_deref(), Some("key-id"));
        assert_eq!(grant.api_key_name.as_deref(), Some("Test key"));
    }

    #[tokio::test]
    async fn principal_model_authorization_allows_every_model_when_unrestricted() {
        let mut unrestricted = TestStore::valid();
        unrestricted.binding = false;
        unrestricted.visible_model_ids.clear();

        let grant = Security::new(Some(&unrestricted))
            .authorize_principal_model(&Principal::new("key-id"), &protected_model())
            .await
            .expect("an empty model scope authorizes every model");

        assert_eq!(grant.api_key_id.as_deref(), Some("key-id"));
        assert_eq!(grant.api_key_name.as_deref(), Some("Test key"));
    }

    #[test]
    fn inference_header_precedence_is_bearer_then_anthropic_then_google() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer bearer-secret"),
        );
        headers.insert("x-api-key", HeaderValue::from_static("anthropic-secret"));
        headers.insert("x-goog-api-key", HeaderValue::from_static("google-secret"));
        assert_eq!(
            ClientCredential::from_inference_headers(&headers).secret(),
            Some("bearer-secret")
        );

        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer   "));
        assert_eq!(
            ClientCredential::from_inference_headers(&headers).secret(),
            Some("anthropic-secret")
        );
    }

    #[test]
    fn inference_header_profile_rejects_empty_and_illegal_values() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_bytes(&[0xff]).expect("opaque invalid UTF-8 header"),
        );
        headers.insert("x-api-key", HeaderValue::from_static(""));
        headers.insert("x-goog-api-key", HeaderValue::from_static("   "));

        assert!(
            ClientCredential::from_inference_headers(&headers)
                .secret()
                .is_none()
        );
    }

    #[tokio::test]
    async fn storage_failure_is_typed_without_exposing_the_credential() {
        let mut unavailable = TestStore::valid();
        unavailable.fail_lookup = true;
        let error = Security::new(Some(&unavailable))
            .authorize_principal_model(&Principal::new("key-id"), &protected_model())
            .await
            .expect_err("storage failure must reject");

        assert!(matches!(error, GatewayError::Internal { .. }));
        assert!(!format!("{error:?}").contains("key-id"));
        assert!(!error.message().contains("key-id"));
    }

    #[test]
    fn mcp_header_profile_accepts_only_a_single_bearer_credential() {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("anthropic-secret"));
        headers.insert("x-goog-api-key", HeaderValue::from_static("google-secret"));
        assert!(
            ClientCredential::from_mcp_headers(&headers)
                .secret()
                .is_none()
        );

        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("bearer mcp-secret"),
        );
        assert_eq!(
            ClientCredential::from_mcp_headers(&headers).secret(),
            Some("mcp-secret")
        );

        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer mcp-secret extra"),
        );
        assert!(
            ClientCredential::from_mcp_headers(&headers)
                .secret()
                .is_none()
        );

        headers.clear();
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer"));
        assert!(
            ClientCredential::from_mcp_headers(&headers)
                .secret()
                .is_none()
        );
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Basic mcp-secret"),
        );
        assert!(
            ClientCredential::from_mcp_headers(&headers)
                .secret()
                .is_none()
        );
    }

    #[tokio::test]
    async fn required_principal_covers_transport_authentication_states() {
        assert!(matches!(
            Security::new(Some(&TestStore::valid()))
                .required_principal(&ClientCredential { secret: None })
                .await,
            Err(GatewayError::Unauthorized {
                reason: AuthFailure::Missing
            })
        ));

        let mut invalid = TestStore::valid();
        invalid.key = None;
        assert!(matches!(
            Security::new(Some(&invalid))
                .required_principal(&credential())
                .await,
            Err(GatewayError::Unauthorized {
                reason: AuthFailure::Invalid
            })
        ));

        let mut disabled = TestStore::valid();
        disabled.key.as_mut().expect("test key").is_enabled = false;
        assert!(matches!(
            Security::new(Some(&disabled))
                .required_principal(&credential())
                .await,
            Err(GatewayError::Forbidden { .. })
        ));

        let expired = TestStore::expired();
        assert!(matches!(
            Security::new(Some(&expired))
                .required_principal(&credential())
                .await,
            Err(GatewayError::Unauthorized {
                reason: AuthFailure::Expired
            })
        ));

        assert!(matches!(
            Security::new(None).required_principal(&credential()).await,
            Err(GatewayError::Internal { .. })
        ));

        let valid = TestStore::valid();
        assert_eq!(
            Security::new(Some(&valid))
                .required_principal(&credential())
                .await
                .expect("valid required Principal"),
            Principal::new("key-id")
        );
    }
}
