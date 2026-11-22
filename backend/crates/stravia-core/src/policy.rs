//! Execution policy extension seam for inference, MCP, and platform capabilities.
//!
//! Community Edition installs [`AllowAllPolicyResolver`], which imposes no extra
//! restrictions beyond existing API key, route, spend, and RPM rules. Commercial
//! or custom hosts can inject a [`PolicyResolver`] via [`crate::GatewayBuilder::policy_resolver`].
//! All enforcement points are strictly fail-closed: any [`PolicyDenial`] or
//! explicitly disallowed route/capability rejects the request.

use std::collections::HashSet;

use async_trait::async_trait;
use axum::http::StatusCode;

/// Per-capability overrides returned by a [`PolicyResolver`].
///
/// `Some(false)` explicitly denies the capability even if the instance and API key
/// would otherwise allow it. `None` or `Some(true)` defers to existing checks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CapabilityOverrides {
    pub web_search: Option<bool>,
    pub media_understanding: Option<bool>,
    pub media_generation: Option<bool>,
    pub mcp_access: Option<bool>,
}

/// Effective execution policy resolved for a single authenticated request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecutionPolicy {
    pub tenant_scope: Option<String>,
    pub allowed_routes: Option<HashSet<String>>,
    pub capability_overrides: Option<CapabilityOverrides>,
    pub effective_spend_limit_nanos: Option<i64>,
    pub effective_rpm_limit: Option<u32>,
}

impl ExecutionPolicy {
    /// Returns `true` when the requested route is permitted by this policy.
    pub fn allows_route(&self, route: &str) -> bool {
        self.allowed_routes
            .as_ref()
            .is_none_or(|allowed| allowed.contains(route))
    }

    /// Returns `true` when the capability is not explicitly denied by this policy.
    pub fn allows_capability(&self, capability: PolicyCapability) -> bool {
        let Some(overrides) = self.capability_overrides.as_ref() else {
            return true;
        };
        let decision = match capability {
            PolicyCapability::WebSearch => overrides.web_search,
            PolicyCapability::MediaUnderstanding => overrides.media_understanding,
            PolicyCapability::MediaGeneration => overrides.media_generation,
            PolicyCapability::McpAccess => overrides.mcp_access,
        };
        decision != Some(false)
    }

    /// Intersects the policy spend limit with the API key's configured spend limit.
    /// Stricter (lower) limit wins.
    pub fn tighten_spend_limit(&self, current: Option<i64>) -> Option<i64> {
        match (current, self.effective_spend_limit_nanos) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }

    /// Intersects the policy RPM limit with the API key's configured RPM limit.
    /// Stricter (lower) limit wins.
    pub fn tighten_rpm_limit(&self, current: Option<i32>) -> Option<i32> {
        let policy_limit = self
            .effective_rpm_limit
            .map(|limit| i32::try_from(limit).unwrap_or(i32::MAX));
        match (current, policy_limit) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }
}

/// Entry surface requesting policy resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyRequestKind {
    Inference,
    Mcp,
    Capability,
}

/// Platform capability identifier evaluated by [`PolicyResolver`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyCapability {
    WebSearch,
    MediaUnderstanding,
    MediaGeneration,
    McpAccess,
}

/// Read-only context supplied to [`PolicyResolver::resolve`].
#[derive(Clone, Debug)]
pub struct PolicyRequestContext<'a> {
    pub api_key_id: &'a str,
    pub route_name: Option<&'a str>,
    pub capability: Option<PolicyCapability>,
    pub kind: PolicyRequestKind,
}

impl<'a> PolicyRequestContext<'a> {
    pub fn inference(api_key_id: &'a str, route_name: &'a str) -> Self {
        Self {
            api_key_id,
            route_name: Some(route_name),
            capability: None,
            kind: PolicyRequestKind::Inference,
        }
    }

    pub fn mcp(api_key_id: &'a str) -> Self {
        Self {
            api_key_id,
            route_name: None,
            capability: Some(PolicyCapability::McpAccess),
            kind: PolicyRequestKind::Mcp,
        }
    }

    pub fn capability(api_key_id: &'a str, capability: PolicyCapability) -> Self {
        Self {
            api_key_id,
            route_name: None,
            capability: Some(capability),
            kind: PolicyRequestKind::Capability,
        }
    }
}

/// Structured fail-closed denial returned by [`PolicyResolver::resolve`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct PolicyDenial {
    pub status: u16,
    pub code: String,
    pub message: String,
}

impl PolicyDenial {
    pub fn forbidden(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN.as_u16(),
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn unavailable(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE.as_u16(),
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn status_code(&self) -> StatusCode {
        StatusCode::from_u16(self.status).unwrap_or(StatusCode::FORBIDDEN)
    }
}

/// Extension trait resolving per-request execution policies.
#[async_trait]
pub trait PolicyResolver: Send + Sync {
    async fn resolve(
        &self,
        ctx: &PolicyRequestContext<'_>,
    ) -> Result<ExecutionPolicy, PolicyDenial>;
}

/// Zero-overhead Community Edition default policy resolver imposing no extra restrictions.
#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAllPolicyResolver;

#[async_trait]
impl PolicyResolver for AllowAllPolicyResolver {
    async fn resolve(
        &self,
        _ctx: &PolicyRequestContext<'_>,
    ) -> Result<ExecutionPolicy, PolicyDenial> {
        Ok(ExecutionPolicy::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    use crate::GatewayBuilder;
    use crate::config::GatewayConfig;
    use crate::proxy::server::create_router;

    struct StaticPolicyResolver {
        result: Result<ExecutionPolicy, PolicyDenial>,
    }

    #[async_trait]
    impl PolicyResolver for StaticPolicyResolver {
        async fn resolve(
            &self,
            _ctx: &PolicyRequestContext<'_>,
        ) -> Result<ExecutionPolicy, PolicyDenial> {
            self.result.clone()
        }
    }

    async fn build_gateway_with_key(
        resolver: Option<Arc<dyn PolicyResolver>>,
    ) -> (crate::Gateway, String, String, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = GatewayConfig {
            data_dir: dir.path().to_path_buf(),
            catalog_background_refresh: false,
            catalog_base_url: None,
            ..Default::default()
        };
        let mut builder = GatewayBuilder::new(config);
        if let Some(resolver) = resolver {
            builder = builder.policy_resolver(resolver);
        }
        let gateway = builder.build().await.expect("build gateway");
        let created = gateway
            .admin()
            .create_api_key(crate::db::models::CreateApiKey {
                key: None,
                name: "policy-test-key".to_string(),
                rpm_limit: None,
                mcp_access_enabled: true,
                transparent_injection_enabled: false,
                transparent_injections: crate::db::models::TransparentInjectionSet::default(),
                expires_at: None,
                model_ids: Vec::new(),
                spend_limit: None,
            })
            .await
            .expect("create api key");
        (gateway, created.id, created.token, dir)
    }

    #[test]
    fn execution_policy_tightens_limits_to_stricter_intersection() {
        let policy = ExecutionPolicy {
            effective_spend_limit_nanos: Some(500),
            effective_rpm_limit: Some(10),
            ..Default::default()
        };
        assert_eq!(policy.tighten_spend_limit(Some(1000)), Some(500));
        assert_eq!(policy.tighten_spend_limit(Some(200)), Some(200));
        assert_eq!(policy.tighten_spend_limit(None), Some(500));
        assert_eq!(policy.tighten_rpm_limit(Some(20)), Some(10));
        assert_eq!(policy.tighten_rpm_limit(Some(5)), Some(5));
        assert_eq!(policy.tighten_rpm_limit(None), Some(10));
    }

    #[tokio::test]
    async fn inference_fails_closed_when_policy_resolver_denies() {
        let resolver = Arc::new(StaticPolicyResolver {
            result: Err(PolicyDenial::unavailable(
                "policy_backend_down",
                "Policy evaluation unavailable",
            )),
        });
        let (gateway, _key_id, raw_key, _dir) = build_gateway_with_key(Some(resolver)).await;
        let app = create_router(gateway);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header(header::AUTHORIZATION, format!("Bearer {raw_key}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), 1024 * 64).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["code"], "policy_backend_down");
    }

    #[tokio::test]
    async fn inference_denies_route_outside_policy_allowlist() {
        let mut allowed = HashSet::new();
        allowed.insert("allowed-route".to_string());
        let resolver = Arc::new(StaticPolicyResolver {
            result: Ok(ExecutionPolicy {
                allowed_routes: Some(allowed),
                ..Default::default()
            }),
        });
        let (gateway, _key_id, raw_key, _dir) = build_gateway_with_key(Some(resolver)).await;
        let app = create_router(gateway);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header(header::AUTHORIZATION, format!("Bearer {raw_key}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"model":"blocked-route","messages":[{"role":"user","content":"hi"}]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = to_bytes(response.into_body(), 1024 * 64).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["code"], "policy_route_denied");
    }

    #[tokio::test]
    async fn mcp_fails_closed_when_policy_denies_mcp_access() {
        let resolver = Arc::new(StaticPolicyResolver {
            result: Ok(ExecutionPolicy {
                capability_overrides: Some(CapabilityOverrides {
                    mcp_access: Some(false),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        });
        let (gateway, _key_id, raw_key, _dir) = build_gateway_with_key(Some(resolver)).await;
        let app = create_router(gateway);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header(header::AUTHORIZATION, format!("Bearer {raw_key}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = to_bytes(response.into_body(), 1024 * 64).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["code"], "policy_capability_denied");
    }

    #[tokio::test]
    async fn capability_gate_fails_closed_on_denial_and_override() {
        let resolver = Arc::new(StaticPolicyResolver {
            result: Ok(ExecutionPolicy {
                capability_overrides: Some(CapabilityOverrides {
                    web_search: Some(false),
                    media_understanding: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        });
        let (gateway, key_id, _raw_key, _dir) = build_gateway_with_key(Some(resolver)).await;
        assert!(
            gateway
                .policy_allows_capability(&key_id, PolicyCapability::WebSearch)
                .await
                .is_err()
        );
        assert!(
            gateway
                .policy_allows_capability(&key_id, PolicyCapability::MediaUnderstanding)
                .await
                .is_ok()
        );
    }
}
