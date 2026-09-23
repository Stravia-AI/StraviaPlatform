mod allowance;

use std::collections::{BTreeMap, BTreeSet};

use base64::Engine as _;
use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_vendor_common::{common, thinking};
#[cfg(target_arch = "wasm32")]
use stravia_vendor_sdk::VendorGuest;
use stravia_vendor_sdk::{
    AuthDescriptor, AuthFlow, AuthRequest, AuthResponse, AuthStep, CANONICAL_FORMAT_VERSION,
    Capability, ChannelDescriptor, DataCompatibility, DiscoverRequest, DiscoverResponse,
    DiscoveredModel, ErrorKind, GuestHost, HttpRequest, NetworkDeclaration, Operation,
    OperationInput, OperationOutput, OriginDeclaration, PluginError, ProviderDescriptor,
    ProviderSnapshot, VendorDescriptor, VendorKind, read_http_body,
};

const VENDOR_ID: &str = "xai-grok";
const CHANNEL_ID: &str = "grok";
const OPEN_RESPONSES_PROTOCOL: &str = "open-responses/responses/2026-04-24";
const XAI_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const XAI_SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const XAI_CLIENT_VERSION: &str = "0.2.120";
const XAI_DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const MAX_JSON_BODY: usize = 8 * 1024 * 1024;

pub fn select_protocol(
    operation: Operation,
    channel: &str,
    provider: &ProviderSnapshot,
    _request: &AiRequest,
) -> Result<String, PluginError> {
    check_provider(channel, provider)?;
    if operation != Operation::Infer {
        return Err(common::unsupported(operation.as_str(), VENDOR_ID, channel));
    }
    Ok(OPEN_RESPONSES_2026_04_24.to_string())
}

pub fn execute(
    host: &GuestHost,
    operation: Operation,
    channel: &str,
    input: OperationInput,
) -> Result<OperationOutput, PluginError> {
    if input.operation() != operation {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "operation kind does not match typed operation input",
        ));
    }
    check_provider(channel, input.provider())?;
    match input {
        OperationInput::Infer {
            provider,
            mut request,
        } => execute_inference(host, &provider, &mut request),
        OperationInput::Auth { provider, request } => {
            xai_auth(host, &provider, request).map(OperationOutput::Auth)
        }
        OperationInput::Discover { provider, request } => {
            discover(host, &provider, request).map(OperationOutput::Discover)
        }
        OperationInput::Allowance { provider, request } => {
            allowance::execute(host, provider, request).map(OperationOutput::Allowance)
        }
        other => Err(common::unsupported(
            other.operation().as_str(),
            VENDOR_ID,
            channel,
        )),
    }
}

pub fn descriptor() -> VendorDescriptor {
    let capabilities = BTreeSet::from([
        Capability::Infer,
        Capability::AuthOauth,
        Capability::ModelDiscovery,
        Capability::Allowance,
    ]);
    VendorDescriptor {
        vendor_id: VENDOR_ID.into(),
        version: Version::parse(env!("CARGO_PKG_VERSION"))
            .expect("package version must be valid semver"),
        display_name: "xAI Grok".into(),
        description: Some("Dedicated xAI Grok OAuth vendor component".into()),
        authors: vec!["Stravia".into()],
        canonical_format_version: CANONICAL_FORMAT_VERSION,
        kind: VendorKind::Dedicated,
        providers: vec![ProviderDescriptor {
            provider_id: VENDOR_ID.into(),
            catalog_id: Some("xai".into()),
            display_name: "xAI Grok".into(),
            description: Some("Grok OAuth access through the xAI CLI service".into()),
            channels: vec![ChannelDescriptor {
                id: CHANNEL_ID.into(),
                name: "Grok OAuth".into(),
                description: None,
                auth: Some(AuthDescriptor {
                    flow: AuthFlow::DeviceCode,
                    callback: None,
                    manual_input: None,
                }),
                protocol: Some("open-responses".into()),
                protocols: Vec::new(),
                default_base_url: Some("https://cli-chat-proxy.grok.com/v1".into()),
                default_models_source: None,
                capabilities: capabilities.clone(),
                model_capabilities: BTreeSet::new(),
                search_model_required: false,
            }],
            capabilities,
            website: None,
            implementation: None,
            config_fields: Vec::new(),
            network: NetworkDeclaration {
                base_url_field: None,
                extra_origins: [
                    "auth.x.ai",
                    "api.x.ai",
                    "cli-chat-proxy.grok.com",
                    "grok.com",
                ]
                .into_iter()
                .map(|host| OriginDeclaration {
                    scheme: "https".into(),
                    host: host.into(),
                    port: None,
                })
                .collect(),
                field_origins: Vec::new(),
            },
            data_compat: DataCompatibility::default(),
        }],
    }
}

fn check_provider(channel: &str, provider: &ProviderSnapshot) -> Result<(), PluginError> {
    if provider.provider_id != VENDOR_ID {
        return Err(common::plugin_error(
            ErrorKind::Unsupported,
            format!(
                "dedicated vendor `{VENDOR_ID}` cannot handle provider `{}`",
                provider.provider_id
            ),
        ));
    }
    if channel != CHANNEL_ID {
        return Err(common::unsupported("channel", VENDOR_ID, channel));
    }
    if provider.channel != channel {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "provider snapshot channel does not match dispatch channel",
        ));
    }
    Ok(())
}

fn execute_inference(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    request: &mut AiRequest,
) -> Result<OperationOutput, PluginError> {
    let model = provider
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| common::plugin_error(ErrorKind::Invalid, "inference requires a model"))?;
    request.model = model.to_owned();
    thinking::apply(
        VENDOR_ID,
        CHANNEL_ID,
        provider,
        OPEN_RESPONSES_PROTOCOL,
        request,
    )?;
    let encoded = common::encode_inference_request(OPEN_RESPONSES_PROTOCOL, request)?;
    let mut headers = common::header_pairs(&encoded.headers)?;
    headers.push(("content-type".into(), "application/json".into()));
    apply_auth_headers(provider, &mut headers)?;
    let url = common::endpoint_url(&provider.base_url, &encoded.path)?;
    let body = serde_json::to_vec(&encoded.body).map_err(|error| {
        common::plugin_error(
            ErrorKind::Invalid,
            format!("failed to serialize codec request: {error}"),
        )
    })?;
    host.emit_started()?;
    let response = host.http_start(HttpRequest {
        method: "POST".into(),
        url,
        headers,
        body,
    })?;
    common::decode_inference(host, OPEN_RESPONSES_PROTOCOL, response)
}

fn apply_auth_headers(
    provider: &ProviderSnapshot,
    headers: &mut Vec<(String, String)>,
) -> Result<(), PluginError> {
    headers.extend([
        (
            "authorization".into(),
            format!("Bearer {}", access_token(provider)?),
        ),
        ("x-xai-token-auth".into(), "xai-grok-cli".into()),
        ("x-grok-client-version".into(), XAI_CLIENT_VERSION.into()),
        (
            "user-agent".into(),
            format!("xai-grok-workspace/{XAI_CLIENT_VERSION}"),
        ),
        ("x-grok-client-identifier".into(), "grok-shell".into()),
        (
            "x-authenticateresponse".into(),
            "authenticate-response".into(),
        ),
    ]);
    Ok(())
}

fn discover(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    request: DiscoverRequest,
) -> Result<DiscoverResponse, PluginError> {
    if let Some(response) = explicit_discovery(provider)? {
        return Ok(response);
    }
    let mut headers = vec![("accept".into(), "application/json".into())];
    apply_auth_headers(provider, &mut headers)?;
    let configured_source = provider
        .operation_metadata
        .get("models_source")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|source| !source.is_empty() && *source != "catalog");
    let mut url = configured_source
        .map(str::to_owned)
        .unwrap_or_else(|| "https://api.x.ai/v1/models".into());
    if let Some(cursor) = request
        .cursor
        .as_deref()
        .map(str::trim)
        .filter(|cursor| !cursor.is_empty())
    {
        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str("after=");
        url.push_str(&urlencoding::encode(cursor));
    }
    let response = host.http_start(HttpRequest {
        method: "GET".into(),
        url,
        headers,
        body: Vec::new(),
    })?;
    let status = response.status()?;
    let response_headers = response.headers()?;
    let body = read_http_body(&response, MAX_JSON_BODY)?;
    if !(200..300).contains(&status) {
        return Err(common::upstream_error(status, &response_headers, &body));
    }
    let value: Value = serde_json::from_slice(&body).map_err(|error| {
        common::plugin_error(
            ErrorKind::upstream_unknown(),
            format!("invalid model discovery response: {error}"),
        )
    })?;
    let rows = value
        .get("data")
        .or_else(|| value.get("models"))
        .and_then(Value::as_array)
        .ok_or_else(|| {
            common::plugin_error(
                ErrorKind::upstream_unknown(),
                "model discovery response has no data/models array",
            )
        })?;
    let models = rows
        .iter()
        .filter(|value| {
            value
                .get("visibility")
                .and_then(Value::as_str)
                .is_none_or(|visibility| visibility.eq_ignore_ascii_case("list"))
        })
        .map(discovered_model)
        .collect::<Result<Vec<_>, _>>()?;
    let next_cursor = value
        .get("next_cursor")
        .or_else(|| value.get("nextPageToken"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            value
                .get("has_more")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                .then(|| models.last().map(|model| model.id.clone()))
                .flatten()
        });
    Ok(DiscoverResponse {
        models,
        next_cursor,
    })
}

fn explicit_discovery(
    provider: &ProviderSnapshot,
) -> Result<Option<DiscoverResponse>, PluginError> {
    let Some(static_models) = provider.operation_metadata.get("static_models") else {
        return Ok(None);
    };
    let values = static_models.as_array().ok_or_else(|| {
        common::plugin_error(
            ErrorKind::Invalid,
            "operation_metadata.static_models must be a JSON array",
        )
    })?;
    let mut models = BTreeMap::new();
    for (index, value) in values.iter().enumerate() {
        let id = value.as_str().ok_or_else(|| {
            common::plugin_error(
                ErrorKind::Invalid,
                format!("operation_metadata.static_models[{index}] must be a string"),
            )
        })?;
        let id = id.trim();
        if id.is_empty() {
            continue;
        }
        let mut model = DiscoveredModel {
            id: id.into(),
            display_name: id.into(),
            family: None,
            selector: None,
            capabilities: Vec::new(),
            metadata: BTreeMap::new(),
        };
        thinking::decorate_discovered_model(VENDOR_ID, &mut model);
        models.entry(id.to_owned()).or_insert(model);
    }
    Ok(Some(DiscoverResponse {
        models: models.into_values().collect(),
        next_cursor: None,
    }))
}

fn discovered_model(value: &Value) -> Result<DiscoveredModel, PluginError> {
    let raw_id = value
        .as_str()
        .or_else(|| {
            value
                .get("id")
                .or_else(|| value.get("name"))
                .and_then(Value::as_str)
        })
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            common::plugin_error(
                ErrorKind::upstream_unknown(),
                "model discovery returned a row without an id/name",
            )
        })?;
    let id = raw_id.strip_prefix("models/").unwrap_or(raw_id).to_owned();
    let display_name = value
        .get("display_name")
        .or_else(|| value.get("displayName"))
        .and_then(Value::as_str)
        .unwrap_or(&id)
        .to_owned();
    let metadata = value.as_object().cloned().unwrap_or_default();
    let mut model = DiscoveredModel {
        id,
        display_name,
        family: value
            .get("family")
            .and_then(Value::as_str)
            .map(str::to_owned),
        selector: value
            .get("selector")
            .and_then(Value::as_str)
            .map(str::to_owned),
        capabilities: value
            .get("capabilities")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        metadata: metadata.into_iter().collect(),
    };
    thinking::decorate_discovered_model(VENDOR_ID, &mut model);
    Ok(model)
}

#[derive(Debug, Serialize, Deserialize)]
struct XaiDeviceState {
    device_code: String,
    token_endpoint: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    interval_seconds: u32,
}

fn xai_auth(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    request: AuthRequest,
) -> Result<AuthResponse, PluginError> {
    match request.step {
        AuthStep::Start { .. } => xai_auth_start(host),
        AuthStep::Poll => xai_auth_poll(host),
        AuthStep::Refresh => xai_auth_refresh(host, provider),
        AuthStep::Exchange { .. } | AuthStep::ManualInput { .. } | AuthStep::Revoke => {
            Err(common::plugin_error(
                ErrorKind::Unsupported,
                "xAI Grok uses device authorization and does not support this auth step",
            ))
        }
    }
}

fn xai_auth_start(host: &GuestHost) -> Result<AuthResponse, PluginError> {
    let discovery = json_request(
        host,
        "GET",
        "https://auth.x.ai/.well-known/openid-configuration",
        Vec::new(),
        Vec::new(),
    )?;
    let device_url = required_json_string(&discovery, "device_authorization_endpoint")?;
    let token_endpoint = required_json_string(&discovery, "token_endpoint")?;
    validate_xai_url(device_url)?;
    validate_xai_url(token_endpoint)?;
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", XAI_CLIENT_ID)
        .append_pair("scope", XAI_SCOPE)
        .finish()
        .into_bytes();
    let device = json_request(
        host,
        "POST",
        device_url,
        vec![(
            "content-type".into(),
            "application/x-www-form-urlencoded".into(),
        )],
        body,
    )?;
    let state = XaiDeviceState {
        device_code: required_json_string(&device, "device_code")?.into(),
        token_endpoint: token_endpoint.into(),
        user_code: required_json_string(&device, "user_code")?.into(),
        verification_uri: required_json_string(&device, "verification_uri")?.into(),
        verification_uri_complete: device
            .get("verification_uri_complete")
            .and_then(Value::as_str)
            .map(str::to_owned),
        interval_seconds: device
            .get("interval")
            .and_then(Value::as_u64)
            .unwrap_or(5)
            .max(5)
            .try_into()
            .unwrap_or(u32::MAX),
    };
    host.write_private_state(&serde_json::to_vec(&state).map_err(|error| {
        common::plugin_error(
            ErrorKind::Trapped,
            format!("encode xAI auth state: {error}"),
        )
    })?)?;
    Ok(AuthResponse::Authorization {
        url: state
            .verification_uri_complete
            .clone()
            .unwrap_or_else(|| state.verification_uri.clone()),
        user_code: Some(state.user_code),
        verification_uri: Some(state.verification_uri),
        interval_seconds: Some(state.interval_seconds),
    })
}

fn xai_auth_poll(host: &GuestHost) -> Result<AuthResponse, PluginError> {
    let state: XaiDeviceState =
        serde_json::from_slice(&host.read_private_state()?.ok_or_else(|| {
            common::plugin_error(ErrorKind::Invalid, "xAI auth session is missing")
        })?)
        .map_err(|error| {
            common::plugin_error(
                ErrorKind::Invalid,
                format!("invalid xAI auth state: {error}"),
            )
        })?;
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", XAI_DEVICE_GRANT)
        .append_pair("device_code", &state.device_code)
        .append_pair("client_id", XAI_CLIENT_ID)
        .finish()
        .into_bytes();
    match token_request(host, &state.token_endpoint, body, None)? {
        TokenResult::Ready(response) => Ok(response),
        TokenResult::Pending(slow_down) => Ok(AuthResponse::Pending {
            retry_after_seconds: Some(state.interval_seconds + if slow_down { 5 } else { 0 }),
        }),
    }
}

fn xai_auth_refresh(
    host: &GuestHost,
    provider: &ProviderSnapshot,
) -> Result<AuthResponse, PluginError> {
    let refresh_token = setting(provider, "refreshToken")
        .or_else(|| setting(provider, "refresh_token"))
        .ok_or_else(|| common::plugin_error(ErrorKind::Auth, "xAI refresh token is missing"))?;
    let token_endpoint =
        setting(provider, "token_endpoint").unwrap_or("https://auth.x.ai/oauth2/token");
    validate_xai_url(token_endpoint)?;
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "refresh_token")
        .append_pair("client_id", XAI_CLIENT_ID)
        .append_pair("refresh_token", refresh_token)
        .finish()
        .into_bytes();
    match token_request(host, token_endpoint, body, Some(refresh_token))? {
        TokenResult::Ready(response) => Ok(response),
        TokenResult::Pending(_) => Err(common::plugin_error(
            ErrorKind::Auth,
            "xAI refresh unexpectedly remained pending",
        )),
    }
}

enum TokenResult {
    Ready(AuthResponse),
    Pending(bool),
}

fn token_request(
    host: &GuestHost,
    endpoint: &str,
    body: Vec<u8>,
    fallback_refresh: Option<&str>,
) -> Result<TokenResult, PluginError> {
    let response = host.http_start(HttpRequest {
        method: "POST".into(),
        url: endpoint.into(),
        headers: vec![
            (
                "content-type".into(),
                "application/x-www-form-urlencoded".into(),
            ),
            ("accept".into(), "application/json".into()),
        ],
        body,
    })?;
    let status = response.status()?;
    let headers = response.headers()?;
    let bytes = read_http_body(&response, MAX_JSON_BODY)?;
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        return match value.get("error").and_then(Value::as_str) {
            Some("authorization_pending") => Ok(TokenResult::Pending(false)),
            Some("slow_down") => Ok(TokenResult::Pending(true)),
            _ => Err(common::upstream_error(status, &headers, &bytes)),
        };
    }
    let access_token = required_json_string(&value, "access_token")?.to_owned();
    let refresh_token = value
        .get("refresh_token")
        .and_then(Value::as_str)
        .or(fallback_refresh)
        .map(str::to_owned);
    let expires = value
        .get("expires_in")
        .and_then(Value::as_i64)
        .unwrap_or(3600)
        .max(1);
    let now = chrono::Utc::now().timestamp_millis();
    let mut values = BTreeMap::new();
    values.insert("access_token".into(), Value::String(access_token));
    if let Some(refresh_token) = refresh_token {
        values.insert("refresh_token".into(), Value::String(refresh_token));
    }
    values.insert("token_endpoint".into(), Value::String(endpoint.to_owned()));
    values.insert(
        "resource_url".into(),
        Value::String("https://cli-chat-proxy.grok.com/v1".into()),
    );
    if let Some(scope) = value.get("scope").and_then(Value::as_str) {
        values.insert(
            "scopes".into(),
            Value::Array(
                scope
                    .split_ascii_whitespace()
                    .map(|value| Value::String(value.to_owned()))
                    .collect(),
            ),
        );
    }
    if let Some(subject) = value
        .get("id_token")
        .and_then(Value::as_str)
        .and_then(jwt_subject)
    {
        values.insert("subject_id".into(), Value::String(subject));
    }
    values.insert("raw".into(), value);
    Ok(TokenResult::Ready(AuthResponse::Credentials {
        values,
        expires_at_unix_ms: now.checked_add(expires.saturating_mul(1000)),
    }))
}

fn json_request(
    host: &GuestHost,
    method: &str,
    url: &str,
    mut headers: Vec<(String, String)>,
    body: Vec<u8>,
) -> Result<Value, PluginError> {
    headers.push(("accept".into(), "application/json".into()));
    let response = host.http_start(HttpRequest {
        method: method.into(),
        url: url.into(),
        headers,
        body,
    })?;
    let status = response.status()?;
    let response_headers = response.headers()?;
    let bytes = read_http_body(&response, MAX_JSON_BODY)?;
    if !(200..300).contains(&status) {
        return Err(common::upstream_error(status, &response_headers, &bytes));
    }
    serde_json::from_slice(&bytes).map_err(|error| {
        common::plugin_error(
            ErrorKind::upstream_unknown(),
            format!("upstream returned invalid JSON: {error}"),
        )
    })
}

fn validate_xai_url(value: &str) -> Result<(), PluginError> {
    let url = url::Url::parse(value)
        .map_err(|error| common::plugin_error(ErrorKind::Invalid, error.to_string()))?;
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    if url.scheme() != "https" || (host != "x.ai" && !host.ends_with(".x.ai")) {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "xAI OAuth endpoint must use HTTPS on x.ai",
        ));
    }
    Ok(())
}

fn required_json_string<'a>(value: &'a Value, key: &str) -> Result<&'a str, PluginError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            common::plugin_error(
                ErrorKind::upstream_unknown(),
                format!("upstream response is missing `{key}`"),
            )
        })
}

fn jwt_subject(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    claims
        .get("email")
        .or_else(|| claims.get("sub"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn access_token(provider: &ProviderSnapshot) -> Result<&str, PluginError> {
    for key in ["accessToken", "access_token", "token", "apiKey"] {
        if let Some(value) = setting(provider, key).filter(|value| !value.trim().is_empty()) {
            return Ok(value);
        }
    }
    Err(common::plugin_error(
        ErrorKind::Auth,
        "OAuth access token is missing",
    ))
}

fn setting<'a>(provider: &'a ProviderSnapshot, key: &str) -> Option<&'a str> {
    provider
        .credentials
        .get(key)
        .or_else(|| provider.options.get(key))
        .and_then(Value::as_str)
}

#[cfg(target_arch = "wasm32")]
struct GrokVendor;

#[cfg(target_arch = "wasm32")]
impl VendorGuest for GrokVendor {
    fn descriptor() -> VendorDescriptor {
        descriptor()
    }

    fn select_protocol(
        operation: Operation,
        channel: &str,
        provider: &ProviderSnapshot,
        request: &AiRequest,
    ) -> Result<String, PluginError> {
        select_protocol(operation, channel, provider, request)
    }

    fn execute(
        host: &GuestHost,
        operation: Operation,
        channel: &str,
        input: OperationInput,
    ) -> Result<OperationOutput, PluginError> {
        execute(host, operation, channel, input)
    }
}

#[cfg(target_arch = "wasm32")]
stravia_vendor_sdk::export_vendor!(GrokVendor);
