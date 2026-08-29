use std::borrow::{Cow, Cow::Owned};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use futures::Stream;
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
    DiscoverResult, Implementation, JsonObject, ListToolsResult, MetaObject,
    PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerInfo, SubscriptionFilter,
    Tool,
};
use rmcp::service::{RequestContext, SubscriptionContext};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use serde_json::{Value, json};

use super::{McpContext, McpTool, SUBSCRIPTION_POLL_INTERVAL};
use crate::Gateway;
use crate::error::{AuthFailure, GatewayError};
use crate::proxy::security::{ClientCredential, Security};

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

struct McpAdmissionLeaseStream {
    inner: Pin<Box<dyn Stream<Item = Result<bytes::Bytes, axum::Error>> + Send>>,
    admission: Option<crate::admission::PrincipalAdmissionLease>,
}

impl Stream for McpAdmissionLeaseStream {
    type Item = Result<bytes::Bytes, axum::Error>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(context) {
            Poll::Ready(None) => {
                self.admission.take();
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(error))) => {
                self.admission.take();
                Poll::Ready(Some(Err(error)))
            }
            other => other,
        }
    }
}

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
    let service = StreamableHttpService::new(
        move || Ok::<_, std::io::Error>(server.clone()),
        LocalSessionManager::default().into(),
        config,
    );

    Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn_with_state(
            gateway,
            authenticate_request,
        ))
}

impl ServerHandler for StraviaMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
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
        let context = mcp_context(&context)?;
        let available = self.gateway.mcp_registry.available(&context).await;
        let mut tools = Vec::with_capacity(available.len());
        for tool in available {
            let input_schema = tool.input_schema_for(&context).await;
            tools.push(tool_definition_with_input(tool.as_ref(), input_schema));
        }
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
        let context = mcp_context(&context)?;
        let arguments = Value::Object(request.arguments.unwrap_or_default());
        match self
            .gateway
            .mcp_registry
            .call(request.name.as_ref(), arguments, &context)
            .await
        {
            Ok(output) => {
                let mut result = if output.is_error {
                    CallToolResult::structured_error(output.structured_content)
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
        let api_key_id = mcp_context(context.request_context())?.api_key_id;
        let sink = context.sink().clone();
        let mut interval = tokio::time::interval(SUBSCRIPTION_POLL_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut previous: Option<Vec<(String, Value)>> = None;

        loop {
            tokio::select! {
                _ = context.cancelled() => return Ok(()),
                _ = interval.tick() => {
                    // Availability caches are request-scoped. A subscription poll is a new
                    // observation and must see permission or provider changes made meanwhile.
                    let mcp_context = McpContext::new(api_key_id.clone());
                    let available = tokio::time::timeout(
                        SUBSCRIPTION_POLL_INTERVAL,
                        self.gateway.mcp_registry.available(&mcp_context),
                    ).await;
                    let Ok(tools) = available else {
                        continue;
                    };
                    let mut tools_with_schema = Vec::with_capacity(tools.len());
                    for tool in tools {
                        tools_with_schema.push((
                            tool.name().to_owned(),
                            tool.input_schema_for(&mcp_context).await,
                        ));
                    }
                    if previous.as_ref() == Some(&tools_with_schema) {
                        continue;
                    }
                    previous = Some(tools_with_schema);
                    if let Err(error) = sink.notify_tool_list_changed().await {
                        tracing::debug!(%error, "MCP tool-list subscription closed");
                        return Ok(());
                    }
                }
            }
        }
    }
}

fn mcp_context(context: &RequestContext<RoleServer>) -> Result<McpContext, McpError> {
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
    Ok(McpContext::new(principal.id.clone()))
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

fn tool_definition_with_input(tool: &dyn McpTool, input_schema: Value) -> Tool {
    let input_schema = schema_object(input_schema, tool.name(), "input");
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

fn wrap_mcp_delivery(
    response: Response,
    admission: crate::admission::PrincipalAdmissionLease,
) -> Response {
    let (parts, body) = response.into_parts();
    let stream = McpAdmissionLeaseStream {
        inner: Box::pin(body.into_data_stream()),
        admission: Some(admission),
    };
    Response::from_parts(parts, Body::from_stream(stream))
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

    let (mut parts, body) = request.into_parts();
    if parts.method == Method::POST {
        let body = match to_bytes(body, usize::MAX).await {
            Ok(body) => body,
            Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        };
        let is_tool_call = is_mcp_tool_call(&body);
        parts.extensions.insert(AuthenticatedApiKey {
            id: authenticated.principal.api_key_id().to_owned(),
        });
        let request = Request::from_parts(parts, Body::from(body));
        let admission = is_tool_call
            .then(|| {
                gateway
                    .principal_admission
                    .acquire(&authenticated.principal, authenticated.concurrency_limit)
            })
            .transpose();
        return match admission {
            Ok(Some(admission)) => wrap_mcp_delivery(next.run(request).await, admission),
            Ok(None) => next.run(request).await,
            Err(error) => error.render(None),
        };
    }

    parts.extensions.insert(AuthenticatedApiKey {
        id: authenticated.principal.api_key_id().to_owned(),
    });
    next.run(Request::from_parts(parts, body)).await
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
