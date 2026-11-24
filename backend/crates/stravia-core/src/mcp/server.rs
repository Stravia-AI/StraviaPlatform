use std::borrow::{Cow, Cow::Owned};
use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::any_service;
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
    DiscoverResult, Implementation, InitializeResult, JsonObject, ListToolsResult, MetaObject,
    PaginatedRequestParams, ProtocolVersion, ServerCapabilities, SubscriptionFilter, Tool,
};
use rmcp::service::{RequestContext, SubscriptionContext};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use serde_json::{Value, json};

use super::{McpContext, McpEndpointAccessError, McpTool, SUBSCRIPTION_POLL_INTERVAL};
use crate::Gateway;
use crate::error::{AuthFailure, GatewayError};
use crate::interaction_observation::{IngressStart, RejectedOutcome};
use crate::proxy::security::{ClientCredential, Security};
use crate::proxy::server::PROXY_JSON_BODY_LIMIT_BYTES;

const SERVER_INFO_META: &str = "io.modelcontextprotocol/serverInfo";
const ERROR_CODE_META: &str = "stravia/errorCode";
static SUPPORTED_PROTOCOL_VERSIONS: [ProtocolVersion; 3] = [
    ProtocolVersion::V_2025_06_18,
    ProtocolVersion::V_2025_11_25,
    ProtocolVersion::V_2026_07_28,
];

#[derive(Clone)]
struct AuthenticatedApiKey {
    id: String,
}

#[derive(Clone)]
struct ResolvedScope(String);

#[derive(Clone)]
struct StraviaMcpServer {
    gateway: Gateway,
    tool_definitions: Arc<HashMap<String, Tool>>,
}

pub(crate) fn router(gateway: Gateway) -> Router {
    let tool_definitions = Arc::new(
        gateway
            .mcp_registry
            .tools
            .values()
            .map(|tool| (tool.name().to_owned(), tool_definition(tool.as_ref())))
            .collect(),
    );
    let server = StraviaMcpServer {
        gateway: gateway.clone(),
        tool_definitions,
    };
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(true)
        .with_json_response(true)
        .with_stateless_protocol_metadata_required(true)
        // The core router does not know the administrator-selected standalone hostname.
        // Bearer authentication plus the loopback-only Origin policy below prevents browser
        // DNS-rebinding requests without breaking legitimate non-loopback deployments.
        .disable_allowed_hosts()
        .disable_allowed_origins();
    let mcp_cancellation = config.cancellation_token.clone();
    let gateway_cancellation = gateway.lifecycle.cancellation.clone();
    gateway.lifecycle.spawn(async move {
        gateway_cancellation.cancelled().await;
        mcp_cancellation.cancel();
    });
    let service = StreamableHttpService::new(
        move || Ok::<_, std::io::Error>(server.clone()),
        LocalSessionManager::default().into(),
        config,
    );

    Router::new()
        .route("/mcp", any_service(service.clone()))
        .route("/mcp/", any_service(service.clone()))
        .route("/mcp/{*rest}", any_service(service))
        .layer(middleware::from_fn_with_state(
            gateway,
            authenticate_request,
        ))
}

impl ServerHandler for StraviaMcpServer {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
        .with_server_info(server_implementation())
        .with_protocol_version(ProtocolVersion::V_2026_07_28)
        .with_instructions(
            "Use tools/list to discover tools available to the authenticated API key.",
        )
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(&SUPPORTED_PROTOCOL_VERSIONS)
    }

    async fn discover(
        &self,
        _context: RequestContext<RoleServer>,
    ) -> Result<DiscoverResult, McpError> {
        Ok(
            DiscoverResult::from_server_info(SUPPORTED_PROTOCOL_VERSIONS.to_vec(), self.get_info())
                .with_ttl_ms(60_000)
                .with_cache_scope(CacheScope::Public),
        )
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        if request.is_some_and(|request| request.cursor.is_some()) {
            return Err(McpError::invalid_params("invalid cursor", None));
        }
        let (mcp_ctx, scope) = mcp_context_and_scope(&context)?;
        let visible = self
            .gateway
            .mcp_catalog_provider
            .list_visible_tools(&self.gateway, &mcp_ctx, &scope)
            .await
            .map_err(|error| McpError::internal_error(error.message, None))?;
        let tools: Vec<Tool> = visible.into_iter().map(|t| t.to_rmcp_tool()).collect();
        let mut result = ListToolsResult::with_all_items(tools)
            .with_ttl_ms(0)
            .with_cache_scope(CacheScope::Private);
        result.meta = Some(result_metadata(None));
        Ok(result)
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tool_definitions.get(name).cloned()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let request_cancellation = context.ct.clone();
        let (mut mcp_ctx, scope) = mcp_context_and_scope(&context)?;
        let cancellation = stravia_runtime_contract::CancellationToken::new();
        mcp_ctx.request_cancellation = Some(cancellation.clone());
        let arguments = Value::Object(request.arguments.unwrap_or_default());
        let call = crate::rpm::scope_root_request(
            crate::rpm::RootRequest::default(),
            self.gateway.mcp_catalog_provider.call_visible_tool(
                &self.gateway,
                request.name.as_ref(),
                arguments,
                &mcp_ctx,
                &scope,
            ),
        );
        tokio::pin!(call);
        let outcome = tokio::select! {
            biased;
            result = &mut call => result,
            _ = request_cancellation.cancelled() => {
                cancellation.cancel();
                call.await
            }
        };
        match outcome {
            Ok(output) => {
                let mut result = if output.is_error {
                    let mut err = CallToolResult::structured_error(output.structured_content);
                    if !output.content.is_empty() {
                        err.content = output.content;
                    }
                    err
                } else if output.content.is_empty() {
                    CallToolResult::structured(output.structured_content)
                } else {
                    let mut result = CallToolResult::success(output.content);
                    result.structured_content = Some(output.structured_content);
                    result
                };
                result = result.with_meta(Some(result_metadata(None)));
                Ok(result.into())
            }
            Err(error) if matches!(error.code, "tool_not_found" | "invalid_input") => {
                Err(McpError::invalid_params(error.message, None))
            }
            Err(error) => Ok(
                CallToolResult::error(vec![ContentBlock::text(error.message)])
                    .with_meta(Some(result_metadata(Some(error.code))))
                    .into(),
            ),
        }
    }

    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        (requested.tools_list_changed == Some(true))
            .then(|| SubscriptionFilter::builder().tools_list_changed().build())
    }

    async fn listen(&self, context: SubscriptionContext) -> Result<(), McpError> {
        let (initial_ctx, scope) = mcp_context_and_scope(context.request_context())?;
        let api_key_id = initial_ctx.api_key_id;
        let sink = context.sink().clone();
        let mut interval = tokio::time::interval(SUBSCRIPTION_POLL_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut previous: Option<Vec<(String, Option<String>, Value)>> = None;

        loop {
            tokio::select! {
                _ = context.cancelled() => return Ok(()),
                _ = interval.tick() => {
                    // Availability caches are request-scoped. A subscription poll is a new
                    // observation and must see permission, catalog, or provider changes made meanwhile.
                    let mcp_context = McpContext::new(api_key_id.clone());
                    let visible = tokio::time::timeout(
                        SUBSCRIPTION_POLL_INTERVAL,
                        self.gateway.mcp_catalog_provider.list_visible_tools(
                            &self.gateway,
                            &mcp_context,
                            &scope,
                        ),
                    )
                    .await;
                    let Ok(Ok(tools)) = visible else {
                        continue;
                    };
                    let snapshot: Vec<(String, Option<String>, Value)> = tools
                        .into_iter()
                        .map(|tool| (tool.exposed_name, tool.description, tool.input_schema))
                        .collect();
                    if previous.as_ref() == Some(&snapshot) {
                        continue;
                    }
                    previous = Some(snapshot);
                    if let Err(error) = sink.notify_tool_list_changed().await {
                        tracing::debug!(%error, "MCP tool-list subscription closed");
                        return Ok(());
                    }
                }
            }
        }
    }
}

fn mcp_context_and_scope(
    context: &RequestContext<RoleServer>,
) -> Result<(McpContext, String), McpError> {
    let parts = context
        .extensions
        .get::<axum::http::request::Parts>()
        .ok_or_else(|| {
            McpError::internal_error("authenticated request context is missing", None)
        })?;
    let principal = parts
        .extensions
        .get::<AuthenticatedApiKey>()
        .ok_or_else(|| {
            McpError::internal_error("authenticated API key context is missing", None)
        })?;
    let scope = parts
        .extensions
        .get::<ResolvedScope>()
        .map(|resolved| resolved.0.clone())
        .unwrap_or_else(|| "unified".to_owned());
    Ok((McpContext::new(principal.id.clone()), scope))
}

fn tool_definition(tool: &dyn McpTool) -> Tool {
    let input_schema = schema_object(tool.input_schema(), tool.name(), "input");
    let description = tool.description().map(|value| Owned(value.to_owned()));
    let mut definition = Tool::new_with_raw(tool.name().to_owned(), description, input_schema);
    if let Some(output_schema) = tool.output_schema() {
        definition =
            definition.with_raw_output_schema(schema_object(output_schema, tool.name(), "output"));
    }
    definition
}

fn schema_object(value: Value, tool_name: &str, kind: &str) -> Arc<JsonObject> {
    Arc::new(value.as_object().cloned().unwrap_or_else(|| {
        panic!("MCP tool {tool_name} {kind} schema was validated during registration")
    }))
}

fn server_implementation() -> Implementation {
    Implementation::new("stravia", env!("CARGO_PKG_VERSION"))
}

fn result_metadata(error_code: Option<&str>) -> MetaObject {
    let mut meta = MetaObject::new();
    meta.insert(
        SERVER_INFO_META.to_owned(),
        serde_json::to_value(server_implementation())
            .expect("MCP server implementation metadata must serialize"),
    );
    if let Some(error_code) = error_code {
        meta.insert(
            ERROR_CODE_META.to_owned(),
            Value::String(error_code.to_owned()),
        );
    }
    meta
}

/// 超限返回 413；其它读取失败（如压缩数据损坏）属于客户端请求错误。
fn exceeds_body_limit(error: &axum::Error) -> bool {
    let mut cause = std::error::Error::source(error);
    while let Some(current) = cause {
        if current.is::<http_body_util::LengthLimitError>() {
            return true;
        }
        cause = current.source();
    }
    false
}

fn is_mcp_tool_call(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .is_ok_and(|request| request.get("method").and_then(Value::as_str) == Some("tools/call"))
}

async fn authenticate_request(
    State(gateway): State<Gateway>,
    headers: HeaderMap,
    request: Request<Body>,
    next: Next,
) -> Response {
    let authenticated = match authenticate(&gateway, &headers).await {
        Ok(principal) => principal,
        Err(response) => return *response,
    };
    if let Err(response) = validate_origin(&headers) {
        return *response;
    }

    let Some(scope) = gateway
        .mcp_catalog_provider
        .parse_scope(request.uri().path())
    else {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({
                "error": {
                    "code": "mcp_endpoint_not_found",
                    "message": format!("Unknown MCP endpoint: {}", request.uri().path())
                }
            })),
        )
            .into_response();
    };

    if let Err(error) = gateway
        .mcp_catalog_provider
        .authorize_endpoint(&gateway, authenticated.principal.api_key_id(), &scope)
        .await
    {
        return match error {
            McpEndpointAccessError::NotFound(message) => (
                StatusCode::NOT_FOUND,
                axum::Json(json!({
                    "error": { "code": "mcp_endpoint_not_found", "message": message }
                })),
            )
                .into_response(),
            McpEndpointAccessError::Forbidden { code, message } => (
                StatusCode::FORBIDDEN,
                axum::Json(json!({
                    "error": { "code": code, "message": message }
                })),
            )
                .into_response(),
            McpEndpointAccessError::Internal(message) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({
                    "error": { "code": "mcp_catalog_error", "message": message }
                })),
            )
                .into_response(),
        };
    }

    let execution_policy = match gateway
        .policy_resolver
        .resolve(&crate::policy::PolicyRequestContext::mcp(
            authenticated.principal.api_key_id(),
        ))
        .await
    {
        Ok(policy) if policy.allows_capability(crate::policy::PolicyCapability::McpAccess) => {
            policy
        }
        Ok(_) => {
            return (
                StatusCode::FORBIDDEN,
                axum::Json(serde_json::json!({
                    "error": {
                        "code": "policy_capability_denied",
                        "message": "MCP access denied by execution policy"
                    }
                })),
            )
                .into_response();
        }
        Err(denial) => {
            return (
                denial.status_code(),
                axum::Json(serde_json::json!({
                    "error": {
                        "code": denial.code,
                        "message": denial.message
                    }
                })),
            )
                .into_response();
        }
    };
    let effective_spend_limit_nanos =
        execution_policy.tighten_spend_limit(authenticated.spend_limit_nanos);
    let effective_rpm_limit = execution_policy.tighten_rpm_limit(authenticated.rpm_limit);

    let (mut parts, body) = request.into_parts();
    if parts.method == Method::POST {
        let body = match to_bytes(body, PROXY_JSON_BODY_LIMIT_BYTES).await {
            Ok(body) => body,
            Err(error) if exceeds_body_limit(&error) => {
                return StatusCode::PAYLOAD_TOO_LARGE.into_response();
            }
            Err(_) => return StatusCode::BAD_REQUEST.into_response(),
        };
        let is_tool_call = is_mcp_tool_call(&body);
        parts.extensions.insert(AuthenticatedApiKey {
            id: authenticated.principal.api_key_id().to_owned(),
        });
        parts.extensions.insert(ResolvedScope(scope));
        let request = Request::from_parts(parts, Body::from(body));
        // Spend admission precedes RPM for `tools/call`, so a rejected call
        // never consumes an RPM window. Other MCP methods are not model calls.
        let admission = if is_tool_call {
            match gateway
                .check_api_key_spend_limit(
                    authenticated.principal.api_key_id(),
                    effective_spend_limit_nanos,
                )
                .await
            {
                Ok(()) => gateway
                    .principal_admission
                    .acquire(&authenticated.principal, effective_rpm_limit),
                Err(error) => Err(error),
            }
        } else {
            Ok(())
        };
        return match admission {
            Ok(_) => next.run(request).await,
            Err(error) => {
                let spend_rejected = matches!(&error, GatewayError::ApiKeySpendLimitReached);
                let response = error.render(None);
                if spend_rejected {
                    record_spend_rejection(
                        &gateway,
                        authenticated.principal.api_key_id(),
                        &authenticated.api_key_name,
                        request.method(),
                        request.uri().path(),
                        &response,
                    );
                }
                response
            }
        };
    }

    parts.extensions.insert(AuthenticatedApiKey {
        id: authenticated.principal.api_key_id().to_owned(),
    });
    parts.extensions.insert(ResolvedScope(scope));
    next.run(Request::from_parts(parts, body)).await
}

/// MCP admission rejections take the same observation path as proxy ingress:
/// one rejected row, with the stable reason code rather than the wire code.
fn record_spend_rejection(
    gateway: &Gateway,
    api_key_id: &str,
    api_key_name: &str,
    method: &Method,
    path: &str,
    response: &Response,
) {
    let mut observer = gateway.observation.observe_ingress(IngressStart {
        id: stravia_runtime_contract::identifier::new_id(),
        method: method.as_str().to_owned(),
        path: path.to_owned(),
        protocol: "mcp".into(),
    });
    observer.set_authenticated_source(api_key_id, api_key_name);
    let failure = response
        .extensions()
        .get::<crate::interaction_observation::FailureDiagnostic>()
        .cloned()
        .map(|mut diagnostic| {
            diagnostic.code = Some("spend_limit".into());
            diagnostic
        });
    observer.reject(RejectedOutcome {
        stage: "admission".into(),
        code: "spend_limit".into(),
        status_code: response.status().as_u16(),
        failure,
    });
}

async fn authenticate(
    gateway: &Gateway,
    headers: &HeaderMap,
) -> Result<crate::proxy::security::AuthenticatedPrincipal, Box<Response>> {
    let credential = ClientCredential::from_mcp_headers(headers);
    Security::new(gateway.storage.auth())
        .authenticated_principal(&credential)
        .await
        .map_err(|error| Box::new(render_authentication_error(error)))
}

fn render_authentication_error(error: GatewayError) -> Response {
    match error {
        GatewayError::Unauthorized {
            reason: AuthFailure::Missing,
        } => auth_error(
            StatusCode::UNAUTHORIZED,
            "invalid_api_key",
            "Bearer API key is required",
        ),
        GatewayError::Unauthorized {
            reason: AuthFailure::Invalid,
        } => auth_error(
            StatusCode::UNAUTHORIZED,
            "invalid_api_key",
            "invalid API key",
        ),
        GatewayError::Unauthorized {
            reason: AuthFailure::Expired,
        } => auth_error(
            StatusCode::UNAUTHORIZED,
            "api_key_expired",
            "API key is expired",
        ),
        GatewayError::Forbidden { .. } => auth_error(
            StatusCode::FORBIDDEN,
            "api_key_disabled",
            "API key is disabled",
        ),
        GatewayError::Internal { .. } => auth_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "auth_unavailable",
            "API key authentication is unavailable",
        ),
        _ => auth_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "auth_unavailable",
            "API key authentication is unavailable",
        ),
    }
}

fn validate_origin(headers: &HeaderMap) -> Result<(), Box<Response>> {
    let Some(origin) = header_value(headers, header::ORIGIN) else {
        return Ok(());
    };
    let allowed = reqwest::Url::parse(origin)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https" | "tauri"))
        .and_then(|url| url.host_str().map(ToString::to_string))
        .is_some_and(|host| {
            let host = host.trim_end_matches('.').to_ascii_lowercase();
            host == "localhost"
                || host.ends_with(".localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        });
    if allowed {
        Ok(())
    } else {
        Err(Box::new(
            (
                StatusCode::FORBIDDEN,
                axum::Json(json!({
                    "error": { "code": "invalid_origin", "message": "Origin is not allowed" }
                })),
            )
                .into_response(),
        ))
    }
}

fn header_value(headers: &HeaderMap, name: impl axum::http::header::AsHeaderName) -> Option<&str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn auth_error(status: StatusCode, code: &str, message: &str) -> Response {
    let mut response = (
        status,
        axum::Json(json!({ "error": { "code": code, "message": message } })),
    )
        .into_response();
    response
        .headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    response
}
