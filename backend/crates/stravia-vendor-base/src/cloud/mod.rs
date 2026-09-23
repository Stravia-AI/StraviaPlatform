use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use stravia_vendor_common::common;

use aws_credential_types::Credentials;
use aws_sigv4::http_request::{SignableBody, SignableRequest, SigningSettings, sign};
use aws_sigv4::sign::v4;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use stravia_runtime_contract::protocol::ids::{
    OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, OPENAI_COMPATIBLE_EMBEDDINGS_V1,
};
use stravia_vendor_sdk::wit::types::HttpRequest;
use stravia_vendor_sdk::{
    AuthResponse, AuthStep, Capability, ChannelDescriptor, ConfigField, ConfigFieldKind,
    ConfigValidationResponse, DataCompatibility, DefaultModelsSource, DiscoverResponse,
    DiscoveredModel, ErrorKind, GuestHost, NetworkDeclaration, Operation, OperationInput,
    OperationOutput, OriginDeclaration, PluginError, ProviderDescriptor, ProviderSnapshot,
    ValidationIssue, read_http_body,
};
use url::Url;

const MAX_JSON_BODY: usize = 8 * 1024 * 1024;
const GOOGLE_TOKEN_ORIGIN: &str = "oauth2.googleapis.com";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const IBM_IAM_URL: &str = "https://iam.cloud.ibm.com/identity/token";

pub(crate) fn descriptor(vendor_id: &str) -> Option<ProviderDescriptor> {
    match vendor_id {
        "azure" => Some(azure_descriptor()),
        "amazon-bedrock" => Some(bedrock_descriptor()),
        "google-vertex" => Some(vertex_descriptor()),
        "google-vertex-anthropic" => Some(vertex_anthropic_descriptor()),
        "sap-ai-core" => Some(sap_descriptor()),
        "gitlab" => Some(gitlab_descriptor()),
        "watsonx" => Some(watsonx_descriptor()),
        _ => None,
    }
}

pub(crate) fn execute(
    vendor_id: &str,
    host: &GuestHost,
    operation: Operation,
    channel: &str,
    input: OperationInput,
) -> Result<OperationOutput, PluginError> {
    if input.provider().channel != channel {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "provider channel does not match requested channel",
        ));
    }
    if !descriptor(vendor_id).is_some_and(|descriptor| {
        descriptor
            .channels
            .iter()
            .any(|declared| declared.id == channel)
    }) {
        return Err(common::unsupported(operation.as_str(), vendor_id, channel));
    }
    match operation {
        Operation::Infer => {
            let OperationInput::Infer {
                provider,
                mut request,
            } = input
            else {
                return Err(input_mismatch());
            };
            match vendor_id {
                "azure" if channel == "default" => infer_azure(host, &provider, &mut request),
                "amazon-bedrock" if channel == "default" => {
                    infer_bedrock(host, &provider, &mut request)
                }
                "google-vertex" if matches!(channel, "native" | "openai") => {
                    infer_vertex(host, channel, &provider, &mut request)
                }
                "google-vertex-anthropic" if channel == "default" => {
                    infer_vertex_anthropic(host, &provider, &mut request)
                }
                "sap-ai-core" if channel == "default" => infer_sap(host, &provider, &mut request),
                "gitlab" if channel == "default" => infer_gitlab(host, &provider, &mut request),
                "watsonx" if channel == "default" => infer_watsonx(host, &provider, &mut request),
                _ => Err(common::unsupported(operation.as_str(), vendor_id, channel)),
            }
        }
        Operation::Discover => {
            let OperationInput::Discover { provider, request } = input else {
                return Err(input_mismatch());
            };
            match vendor_id {
                "azure" if channel == "default" => discover_openai(host, &provider),
                "google-vertex" if matches!(channel, "native" | "openai") => discover_vertex(
                    host,
                    &provider,
                    false,
                    channel == "openai",
                    request.cursor.as_deref(),
                ),
                "google-vertex-anthropic" if channel == "default" => {
                    discover_vertex(host, &provider, true, false, request.cursor.as_deref())
                }
                "sap-ai-core" if channel == "default" => discover_sap(host, &provider),
                "gitlab" if channel == "default" => discover_gitlab(host, &provider),
                _ => Err(common::unsupported(operation.as_str(), vendor_id, channel)),
            }
        }
        Operation::Auth if vendor_id == "gitlab" && channel == "default" => {
            let OperationInput::Auth { provider, request } = input else {
                return Err(input_mismatch());
            };
            refresh_gitlab(host, &provider, request.step)
        }
        Operation::ConfigValidation => {
            let OperationInput::ConfigValidation { provider, request } = input else {
                return Err(input_mismatch());
            };
            Ok(OperationOutput::ConfigValidation(validate_config(
                vendor_id,
                &provider,
                &request.options,
            )))
        }
        _ => Err(common::unsupported(operation.as_str(), vendor_id, channel)),
    }
}

fn infer_azure(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    request: &mut stravia_runtime_contract::protocol::ir::AiRequest,
) -> Result<OperationOutput, PluginError> {
    apply_snapshot_model(provider, request)?;
    let protocol = common::endpoint(&provider.protocol)?;
    if protocol != OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1
        && protocol != OPENAI_COMPATIBLE_EMBEDDINGS_V1
    {
        return Err(common::plugin_error(
            ErrorKind::Unsupported,
            "protocol is not supported by the Azure vendor",
        ));
    }
    let protocol = protocol.to_string();
    let encoded = crate::encode_inference_request(&protocol, request)?;
    let endpoint = common::endpoint_url(&provider.base_url, &encoded.path)?;
    let mut url = Url::parse(&endpoint)
        .map_err(|_| common::plugin_error(ErrorKind::Invalid, "Azure base URL is invalid"))?;
    let version = string_value(provider, "apiVersion").unwrap_or_else(|| "v1".into());
    url.query_pairs_mut().append_pair("api-version", &version);
    let api_key = required_string(provider, "apiKey", "Azure API key is required")?;
    let mut headers = common::header_pairs(&encoded.headers)?;
    set_header(&mut headers, "api-key", api_key);
    send_inference(
        host,
        &protocol,
        "POST",
        url.as_str(),
        headers,
        json_body(encoded.body)?,
    )
}

fn infer_bedrock(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    request: &mut stravia_runtime_contract::protocol::ir::AiRequest,
) -> Result<OperationOutput, PluginError> {
    apply_snapshot_model(provider, request)?;
    let encoded = crate::encode_inference_request("bedrock-converse", request)?;
    let url = common::endpoint_url(&provider.base_url, &encoded.path)?;
    let body = json_body(encoded.body)?;
    let mut headers = common::header_pairs(&encoded.headers)?;
    set_header(&mut headers, "content-type", "application/json");
    if let Some(api_key) = string_value(provider, "apiKey") {
        set_header(&mut headers, "authorization", format!("Bearer {api_key}"));
    } else {
        sign_bedrock(provider, &url, &body, &mut headers)?;
    }
    send_inference(host, "bedrock-converse", "POST", &url, headers, body)
}

fn infer_vertex(
    host: &GuestHost,
    channel: &str,
    provider: &ProviderSnapshot,
    request: &mut stravia_runtime_contract::protocol::ir::AiRequest,
) -> Result<OperationOutput, PluginError> {
    apply_snapshot_model(provider, request)?;
    let protocol = if channel == "native" {
        "google-generate-content"
    } else {
        "openai-chat-completions"
    };
    let encoded = crate::encode_inference_request(protocol, request)?;
    let base = vertex_base_url(provider)?;
    let url = if channel == "native" {
        vertex_google_url(&base, &encoded.path)
    } else {
        vertex_openai_url(&base, &encoded.path)
    };
    let token = vertex_access_token(host, provider)?;
    let mut headers = common::header_pairs(&encoded.headers)?;
    set_header(&mut headers, "authorization", format!("Bearer {token}"));
    send_inference(
        host,
        protocol,
        "POST",
        &url,
        headers,
        json_body(encoded.body)?,
    )
}

fn infer_vertex_anthropic(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    request: &mut stravia_runtime_contract::protocol::ir::AiRequest,
) -> Result<OperationOutput, PluginError> {
    apply_snapshot_model(provider, request)?;
    let encoded = crate::encode_inference_request("anthropic-messages", request)?;
    let model = provider
        .model
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            common::plugin_error(ErrorKind::Invalid, "Vertex Anthropic model is required")
        })?;
    let action = if request.stream.enabled {
        "streamRawPredict"
    } else {
        "rawPredict"
    };
    let url = format!(
        "{}/publishers/anthropic/models/{model}:{action}",
        vertex_base_url(provider)?.trim_end_matches('/')
    );
    let token = vertex_access_token(host, provider)?;
    let mut headers = common::header_pairs(&encoded.headers)?;
    set_header(&mut headers, "authorization", format!("Bearer {token}"));
    send_inference(
        host,
        "anthropic-messages",
        "POST",
        &url,
        headers,
        json_body(encoded.body)?,
    )
}

fn infer_sap(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    request: &mut stravia_runtime_contract::protocol::ir::AiRequest,
) -> Result<OperationOutput, PluginError> {
    apply_snapshot_model(provider, request)?;
    let encoded = crate::encode_inference_request("openai-chat-completions", request)?;
    let path = encoded.path.strip_prefix("/v1").unwrap_or(&encoded.path);
    let url = common::endpoint_url(&provider.base_url, path)?;
    let token = sap_access_token(host, provider)?;
    let mut headers = common::header_pairs(&encoded.headers)?;
    set_header(&mut headers, "authorization", format!("Bearer {token}"));
    if let Some(group) = string_value(provider, "resourceGroup") {
        set_header(&mut headers, "AI-Resource-Group", group);
    }
    send_inference(
        host,
        "openai-chat-completions",
        "POST",
        &url,
        headers,
        json_body(encoded.body)?,
    )
}

fn infer_gitlab(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    request: &mut stravia_runtime_contract::protocol::ir::AiRequest,
) -> Result<OperationOutput, PluginError> {
    apply_snapshot_model(provider, request)?;
    let encoded = crate::encode_inference_request("openai-chat-completions", request)?;
    let url = common::endpoint_url(&provider.base_url, &encoded.path)?;
    let body = json_body(encoded.body)?;
    let base_headers = common::header_pairs(&encoded.headers)?;

    let access = gitlab_direct_access(host, provider, false)?;
    host.emit_started()?;
    let response = host.http_start(HttpRequest {
        method: "POST".into(),
        url,
        headers: gitlab_headers(base_headers, &access),
        body,
    })?;
    let status = response.status()?;
    if status == 401 {
        // A model request may already have executed. Return the typed fact to
        // the host; Model Turn owns refresh budgeting and any replay decision.
        return Err(http_error(response, status));
    }
    common::decode_inference(host, "openai-chat-completions", response)
}

fn infer_watsonx(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    request: &mut stravia_runtime_contract::protocol::ir::AiRequest,
) -> Result<OperationOutput, PluginError> {
    apply_snapshot_model(provider, request)?;
    let encoded = crate::encode_inference_request("watsonx-text-chat", request)?;
    let object = encoded.body.as_object().cloned().ok_or_else(|| {
        common::plugin_error(ErrorKind::Invalid, "watsonx request is not an object")
    })?;
    let mut object = object;
    object.insert(
        "project_id".into(),
        Value::String(required_string(
            provider,
            "projectId",
            "watsonx project ID is required",
        )?),
    );
    object.remove("stream");
    if let Some(choice) = object.remove("tool_choice") {
        if matches!(choice.as_str(), Some("auto" | "none" | "required")) {
            object.insert("tool_choice_option".into(), choice);
        } else {
            object.insert("tool_choice".into(), choice);
        }
    }
    let endpoint = if request.stream.enabled {
        "chat_stream"
    } else {
        "chat"
    };
    let version = string_value(provider, "apiVersion").unwrap_or_else(|| "2026-04-20".into());
    let base = provider.base_url.trim_end_matches('/').to_string();
    if base.is_empty() {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "watsonx base URL must be normalized and saved before execution",
        ));
    }
    let mut url = Url::parse(&format!("{base}/ml/v1/text/{endpoint}"))
        .map_err(|_| common::plugin_error(ErrorKind::Invalid, "watsonx base URL is invalid"))?;
    url.query_pairs_mut().append_pair("version", &version);
    let token = ibm_access_token(host, provider)?;
    let mut headers = common::header_pairs(&encoded.headers)?;
    set_header(&mut headers, "authorization", format!("Bearer {token}"));
    send_inference(
        host,
        "watsonx-text-chat",
        "POST",
        url.as_str(),
        headers,
        json_body(Value::Object(object))?,
    )
}

fn send_inference(
    host: &GuestHost,
    protocol: &str,
    method: &str,
    url: &str,
    mut headers: Vec<(String, String)>,
    body: Vec<u8>,
) -> Result<OperationOutput, PluginError> {
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
    {
        headers.push(("content-type".into(), "application/json".into()));
    }
    host.emit_started()?;
    let response = host.http_start(HttpRequest {
        method: method.into(),
        url: url.into(),
        headers,
        body,
    })?;
    crate::decode_inference(host, protocol, response)
}

fn http_error(response: stravia_vendor_sdk::HttpResponse, status: u16) -> PluginError {
    let headers = match response.headers() {
        Ok(headers) => headers,
        Err(error) => return error,
    };
    let body = match read_http_body(&response, 256 * 1024) {
        Ok(body) => body,
        Err(error) => return error,
    };
    common::upstream_error(status, &headers, &body)
}

fn apply_snapshot_model(
    provider: &ProviderSnapshot,
    request: &mut stravia_runtime_contract::protocol::ir::AiRequest,
) -> Result<(), PluginError> {
    let model = provider
        .model
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| common::plugin_error(ErrorKind::Invalid, "an upstream model is required"))?;
    request.model = model.to_string();
    Ok(())
}

fn discover_openai(
    host: &GuestHost,
    provider: &ProviderSnapshot,
) -> Result<OperationOutput, PluginError> {
    let mut headers = Vec::new();
    set_header(
        &mut headers,
        "api-key",
        required_string(provider, "apiKey", "Azure API key is required")?,
    );
    let mut url = Url::parse(&models_url(provider)?)
        .map_err(|_| common::plugin_error(ErrorKind::Invalid, "Azure models URL is invalid"))?;
    let version = string_value(provider, "apiVersion").unwrap_or_else(|| "v1".into());
    url.query_pairs_mut().append_pair("api-version", &version);
    discover_json(host, url.as_str(), headers)
}

fn discover_sap(
    host: &GuestHost,
    provider: &ProviderSnapshot,
) -> Result<OperationOutput, PluginError> {
    let token = sap_access_token(host, provider)?;
    let mut headers = vec![("authorization".into(), format!("Bearer {token}"))];
    if let Some(group) = string_value(provider, "resourceGroup") {
        set_header(&mut headers, "AI-Resource-Group", group);
    }
    discover_json(host, &models_url(provider)?, headers)
}

fn discover_gitlab(
    host: &GuestHost,
    provider: &ProviderSnapshot,
) -> Result<OperationOutput, PluginError> {
    let access = gitlab_direct_access(host, provider, false)?;
    discover_json(
        host,
        &models_url(provider)?,
        gitlab_headers(Vec::new(), &access),
    )
}

fn discover_json(
    host: &GuestHost,
    url: &str,
    headers: Vec<(String, String)>,
) -> Result<OperationOutput, PluginError> {
    let response = host.http_start(HttpRequest {
        method: "GET".into(),
        url: url.into(),
        headers,
        body: Vec::new(),
    })?;
    let status = response.status()?;
    if !(200..300).contains(&status) {
        return Err(http_error(response, status));
    }
    let body = read_http_body(&response, MAX_JSON_BODY)?;
    let value: Value = serde_json::from_slice(&body).map_err(|_| {
        common::plugin_error(ErrorKind::Invalid, "model discovery returned invalid JSON")
    })?;
    let models = extract_models(&value);
    if models.is_empty() {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "model discovery response contained no models",
        ));
    }
    Ok(OperationOutput::Discover(DiscoverResponse {
        models,
        next_cursor: None,
    }))
}

fn extract_models(value: &Value) -> Vec<DiscoveredModel> {
    let from_data = value.get("data").and_then(Value::as_array).is_some();
    let entries = value
        .get("data")
        .or_else(|| value.get("models"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    let mut by_id = BTreeMap::new();
    for entry in entries {
        if !from_data
            && entry
                .get("visibility")
                .and_then(Value::as_str)
                .is_some_and(|visibility| !visibility.eq_ignore_ascii_case("list"))
        {
            continue;
        }
        let Some(id) = entry
            .get("id")
            .or_else(|| entry.get("name"))
            .or_else(|| entry.get("slug"))
            .and_then(Value::as_str)
            .map(|id| {
                if from_data {
                    id.to_string()
                } else {
                    id.rsplit('/').next().unwrap_or(id).to_string()
                }
            })
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        let display_name = entry
            .get("displayName")
            .or_else(|| entry.get("display_name"))
            .and_then(Value::as_str)
            .unwrap_or(&id)
            .to_string();
        let metadata = entry
            .as_object()
            .map(|object| object.clone().into_iter().collect())
            .unwrap_or_default();
        by_id.insert(
            id.clone(),
            DiscoveredModel {
                id,
                display_name,
                family: None,
                selector: None,
                capabilities: Vec::new(),
                metadata,
            },
        );
    }
    by_id.into_values().collect()
}

fn discover_vertex(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    anthropic: bool,
    openai_names: bool,
    cursor: Option<&str>,
) -> Result<OperationOutput, PluginError> {
    if !anthropic {
        if cursor.is_some() {
            return Err(common::plugin_error(
                ErrorKind::Unsupported,
                "Vertex's curated model inventory is not paginated",
            ));
        }
        let ids: &[&str] = if openai_names {
            &[
                "google/gemini-2.5-pro",
                "google/gemini-2.5-flash",
                "google/gemini-2.0-flash-001",
            ]
        } else {
            &[
                "gemini-2.5-pro",
                "gemini-2.5-flash",
                "gemini-2.0-flash-001",
                "gemini-1.5-pro-002",
                "gemini-1.5-flash-002",
            ]
        };
        return Ok(OperationOutput::Discover(DiscoverResponse {
            models: ids
                .iter()
                .map(|id| DiscoveredModel {
                    id: (*id).into(),
                    display_name: (*id).into(),
                    family: None,
                    selector: None,
                    capabilities: Vec::new(),
                    metadata: BTreeMap::new(),
                })
                .collect(),
            next_cursor: None,
        }));
    }
    let token = vertex_access_token(host, provider)?;
    let configured_base = vertex_base_url(provider)?;
    let publisher_base = configured_base
        .strip_suffix("/endpoints/openapi")
        .unwrap_or(configured_base.as_str());
    let raw_url = format!(
        "{}/publishers/anthropic/models",
        publisher_base.trim_end_matches('/')
    );
    let mut parsed_url = Url::parse(&raw_url).map_err(|_| {
        common::plugin_error(ErrorKind::Invalid, "Vertex model discovery URL is invalid")
    })?;
    if let Some(cursor) = cursor.filter(|cursor| !cursor.is_empty()) {
        parsed_url
            .query_pairs_mut()
            .append_pair("pageToken", cursor);
    }
    let url = parsed_url.to_string();
    let response = host.http_start(HttpRequest {
        method: "GET".into(),
        url,
        headers: vec![("authorization".into(), format!("Bearer {token}"))],
        body: Vec::new(),
    })?;
    let status = response.status()?;
    if !(200..300).contains(&status) {
        return Err(http_error(response, status));
    }
    let payload: Value = serde_json::from_slice(&read_http_body(&response, MAX_JSON_BODY)?)
        .map_err(|_| {
            common::plugin_error(
                ErrorKind::Invalid,
                "Vertex model discovery returned invalid JSON",
            )
        })?;
    let mut models = extract_models(&payload);
    if models.is_empty() {
        models = extract_models_from_keys(&payload, &["publisherModels", "foundationModels"]);
    }
    if models.is_empty() {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "Vertex model discovery response contained no models",
        ));
    }
    Ok(OperationOutput::Discover(DiscoverResponse {
        models,
        next_cursor: payload
            .get("nextPageToken")
            .and_then(Value::as_str)
            .map(str::to_owned),
    }))
}

fn extract_models_from_keys(value: &Value, keys: &[&str]) -> Vec<DiscoveredModel> {
    let mut wrapper = Value::Null;
    for key in keys {
        if let Some(entries) = value.get(*key).and_then(Value::as_array) {
            wrapper = json_object_with_models(entries.clone());
            break;
        }
    }
    extract_models(&wrapper)
}

fn json_object_with_models(models: Vec<Value>) -> Value {
    let mut object = serde_json::Map::new();
    object.insert("models".into(), Value::Array(models));
    Value::Object(object)
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PrivateState {
    #[serde(default)]
    tokens: BTreeMap<String, CachedToken>,
    gitlab: Option<CachedGitLabAccess>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedToken {
    value: String,
    expires_at_unix: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedGitLabAccess {
    config_fingerprint: String,
    token: String,
    headers: Vec<(String, String)>,
    expires_at_unix: u64,
}

fn read_state(host: &GuestHost) -> Result<PrivateState, PluginError> {
    let Some(bytes) = host.read_private_state()? else {
        return Ok(PrivateState::default());
    };
    serde_json::from_slice(&bytes)
        .map_err(|_| common::plugin_error(ErrorKind::Invalid, "vendor private state is invalid"))
}

fn write_state(host: &GuestHost, state: &PrivateState) -> Result<(), PluginError> {
    let bytes = serde_json::to_vec(state).map_err(|_| {
        common::plugin_error(ErrorKind::Invalid, "vendor private state cannot be encoded")
    })?;
    host.write_private_state(&bytes)
}

fn cached_token(host: &GuestHost, key: &str) -> Result<Option<String>, PluginError> {
    let state = read_state(host)?;
    Ok(state
        .tokens
        .get(key)
        .filter(|token| token.expires_at_unix > unix_now().saturating_add(300))
        .map(|token| token.value.clone()))
}

fn cache_token(
    host: &GuestHost,
    key: &str,
    value: String,
    expires_in: u64,
) -> Result<String, PluginError> {
    let mut state = read_state(host)?;
    state.tokens.insert(
        key.into(),
        CachedToken {
            value: value.clone(),
            expires_at_unix: unix_now().saturating_add(expires_in),
        },
    );
    write_state(host, &state)?;
    Ok(value)
}

fn vertex_access_token(
    host: &GuestHost,
    provider: &ProviderSnapshot,
) -> Result<String, PluginError> {
    let secret = string_value(provider, "credentials")
        .or_else(|| string_value(provider, "apiKey"))
        .ok_or_else(|| {
            common::plugin_error(
                ErrorKind::Auth,
                "Vertex service-account JSON or access token is required",
            )
        })?;
    if !secret.trim_start().starts_with('{') {
        return Ok(secret);
    }
    let account: GoogleServiceAccount = serde_json::from_str(&secret).map_err(|_| {
        common::plugin_error(ErrorKind::Auth, "Vertex service-account JSON is invalid")
    })?;
    if account.client_email.trim().is_empty() || account.private_key.trim().is_empty() {
        return Err(common::plugin_error(
            ErrorKind::Auth,
            "Vertex service-account JSON is missing client_email or private_key",
        ));
    }
    if account.token_uri.as_deref().unwrap_or(GOOGLE_TOKEN_URL) != GOOGLE_TOKEN_URL {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "Vertex service-account token_uri must use the declared Google OAuth origin",
        ));
    }
    let cache_key = format!(
        "google:{}:{}",
        account.client_email,
        sha256_hex(secret.as_bytes())
    );
    if let Some(token) = cached_token(host, &cache_key)? {
        return Ok(token);
    }
    let now = unix_now();
    let claims = GoogleJwtClaims {
        iss: &account.client_email,
        scope: "https://www.googleapis.com/auth/cloud-platform",
        aud: GOOGLE_TOKEN_URL,
        iat: now,
        exp: now.saturating_add(3600),
    };
    let key = EncodingKey::from_rsa_pem(account.private_key.as_bytes()).map_err(|_| {
        common::plugin_error(
            ErrorKind::Auth,
            "Vertex service-account private key is invalid",
        )
    })?;
    let assertion = encode(&Header::new(Algorithm::RS256), &claims, &key).map_err(|_| {
        common::plugin_error(ErrorKind::Auth, "failed to sign Vertex service-account JWT")
    })?;
    let body = form_body(&[
        ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
        ("assertion", &assertion),
    ]);
    let payload = token_post(host, GOOGLE_TOKEN_URL, body, Vec::new())?;
    let token = payload
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            common::plugin_error(
                ErrorKind::Auth,
                "Google OAuth response is missing access_token",
            )
        })?
        .to_string();
    cache_token(
        host,
        &cache_key,
        token,
        payload
            .get("expires_in")
            .and_then(Value::as_u64)
            .unwrap_or(3600),
    )
}

#[derive(Deserialize)]
struct GoogleServiceAccount {
    project_id: Option<String>,
    client_email: String,
    private_key: String,
    token_uri: Option<String>,
}

#[derive(Serialize)]
struct GoogleJwtClaims<'a> {
    iss: &'a str,
    scope: &'a str,
    aud: &'a str,
    iat: u64,
    exp: u64,
}

fn sap_access_token(host: &GuestHost, provider: &ProviderSnapshot) -> Result<String, PluginError> {
    let token_url = required_string(provider, "tokenUrl", "SAP OAuth token URL is required")?;
    let client_id = required_string(provider, "clientId", "SAP OAuth client ID is required")?;
    let client_secret = required_string(
        provider,
        "clientSecret",
        "SAP OAuth client secret is required",
    )?;
    let cache_key = format!(
        "sap:{}",
        sha256_hex(format!("{token_url}\n{client_id}\n{client_secret}").as_bytes())
    );
    if let Some(token) = cached_token(host, &cache_key)? {
        return Ok(token);
    }
    let payload = token_post(
        host,
        &token_url,
        form_body(&[
            ("grant_type", "client_credentials"),
            ("client_id", &client_id),
            ("client_secret", &client_secret),
        ]),
        Vec::new(),
    )?;
    let token = payload
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            common::plugin_error(
                ErrorKind::Auth,
                "SAP OAuth response is missing access_token",
            )
        })?
        .to_string();
    cache_token(
        host,
        &cache_key,
        token,
        payload
            .get("expires_in")
            .and_then(Value::as_u64)
            .unwrap_or(3600),
    )
}

fn ibm_access_token(host: &GuestHost, provider: &ProviderSnapshot) -> Result<String, PluginError> {
    let api_key = required_string(provider, "apiKey", "IBM Cloud API key is required")?;
    let cache_key = format!("ibm-iam:{}", sha256_hex(api_key.as_bytes()));
    if let Some(token) = cached_token(host, &cache_key)? {
        return Ok(token);
    }
    let payload = token_post(
        host,
        IBM_IAM_URL,
        form_body(&[
            ("grant_type", "urn:ibm:params:oauth:grant-type:apikey"),
            ("apikey", &api_key),
        ]),
        Vec::new(),
    )?;
    let token = payload
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            common::plugin_error(ErrorKind::Auth, "IBM IAM response is missing access_token")
        })?
        .to_string();
    cache_token(
        host,
        &cache_key,
        token,
        payload
            .get("expires_in")
            .and_then(Value::as_u64)
            .unwrap_or(3600),
    )
}

fn token_post(
    host: &GuestHost,
    url: &str,
    body: Vec<u8>,
    mut headers: Vec<(String, String)>,
) -> Result<Value, PluginError> {
    set_header(
        &mut headers,
        "content-type",
        "application/x-www-form-urlencoded",
    );
    let response = host.http_start(HttpRequest {
        method: "POST".into(),
        url: url.into(),
        headers,
        body,
    })?;
    let status = response.status()?;
    if !(200..300).contains(&status) {
        return Err(http_error(response, status));
    }
    serde_json::from_slice(&read_http_body(&response, 1024 * 1024)?)
        .map_err(|_| common::plugin_error(ErrorKind::Auth, "token endpoint returned invalid JSON"))
}

fn refresh_gitlab(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    step: AuthStep,
) -> Result<OperationOutput, PluginError> {
    if !matches!(step, AuthStep::Refresh) {
        return Err(common::plugin_error(
            ErrorKind::Unsupported,
            "GitLab auth supports only refresh",
        ));
    }
    let api_key = required_string(provider, "apiKey", "GitLab access token is required")?;
    let mut state = read_state(host)?;
    state.gitlab = None;
    write_state(host, &state)?;
    // Refresh only the auxiliary direct-access cache. The model request is not
    // replayed here; Model Turn owns the subsequent attempt and its budget.
    gitlab_direct_access(host, provider, true)?;
    Ok(OperationOutput::Auth(AuthResponse::Credentials {
        values: BTreeMap::from([("apiKey".into(), Value::String(api_key))]),
        expires_at_unix_ms: None,
    }))
}

fn gitlab_direct_access(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    force: bool,
) -> Result<CachedGitLabAccess, PluginError> {
    let api_key = required_string(provider, "apiKey", "GitLab access token is required")?;
    let instance = string_value(provider, "instanceUrl")
        .unwrap_or_else(|| "https://gitlab.com".into())
        .trim_end_matches('/')
        .to_string();
    let config_fingerprint = sha256_hex(format!("{instance}\n{api_key}").as_bytes());
    if !force {
        let state = read_state(host)?;
        if let Some(access) = state.gitlab.filter(|access| {
            access.config_fingerprint == config_fingerprint
                && access.expires_at_unix > unix_now().saturating_add(300)
        }) {
            return Ok(access);
        }
    }
    let url = format!("{instance}/api/v4/ai/third_party_agents/direct_access");
    let response = host.http_start(HttpRequest {
        method: "POST".into(),
        url,
        headers: vec![
            ("authorization".into(), format!("Bearer {api_key}")),
            ("content-type".into(), "application/json".into()),
        ],
        body: b"{}".to_vec(),
    })?;
    let status = response.status()?;
    if !(200..300).contains(&status) {
        return Err(http_error(response, status));
    }
    let payload: Value =
        serde_json::from_slice(&read_http_body(&response, 1024 * 1024)?).map_err(|_| {
            common::plugin_error(
                ErrorKind::Auth,
                "GitLab direct-access endpoint returned invalid JSON",
            )
        })?;
    let token = payload
        .get("token")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            common::plugin_error(
                ErrorKind::Auth,
                "GitLab direct-access response is missing token",
            )
        })?
        .to_string();
    let headers = payload
        .get("headers")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(name, value)| {
            let value = value.as_str()?;
            is_safe_header_name(name)
                .then(|| (name.clone(), value.to_string()))
                .filter(|(name, _)| !name.eq_ignore_ascii_case("x-api-key"))
        })
        .collect();
    let access = CachedGitLabAccess {
        config_fingerprint,
        token,
        headers,
        expires_at_unix: unix_now().saturating_add(25 * 60),
    };
    let mut state = read_state(host)?;
    state.gitlab = Some(access.clone());
    write_state(host, &state)?;
    Ok(access)
}

fn gitlab_headers(
    mut base: Vec<(String, String)>,
    access: &CachedGitLabAccess,
) -> Vec<(String, String)> {
    set_header(
        &mut base,
        "authorization",
        format!("Bearer {}", access.token),
    );
    for (name, value) in &access.headers {
        set_header(&mut base, name, value.clone());
    }
    base
}

fn sign_bedrock(
    provider: &ProviderSnapshot,
    url: &str,
    body: &[u8],
    headers: &mut Vec<(String, String)>,
) -> Result<(), PluginError> {
    let region = required_string(provider, "region", "AWS region is required")?;
    let access_key = required_string(provider, "accessKeyId", "AWS access key ID is required")?;
    let secret_key = required_string(
        provider,
        "secretAccessKey",
        "AWS secret access key is required",
    )?;
    let session_token = string_value(provider, "sessionToken");
    let signable_headers = headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect::<Vec<_>>();
    let signable = SignableRequest::new(
        "POST",
        url,
        signable_headers.into_iter(),
        SignableBody::Bytes(body),
    )
    .map_err(|error| {
        common::plugin_error(
            ErrorKind::Invalid,
            format!("invalid Bedrock request: {error}"),
        )
    })?;
    let credentials = Credentials::new(
        access_key,
        secret_key,
        session_token,
        None,
        "stravia-bedrock-guest",
    );
    let identity = credentials.into();
    let params = v4::SigningParams::builder()
        .identity(&identity)
        .region(&region)
        .name("bedrock")
        .time(SystemTime::now())
        .settings(SigningSettings::default())
        .build()
        .map_err(|error| {
            common::plugin_error(
                ErrorKind::Invalid,
                format!("Bedrock signing failed: {error}"),
            )
        })?
        .into();
    let (instructions, _) = sign(signable, &params)
        .map_err(|error| {
            common::plugin_error(
                ErrorKind::Invalid,
                format!("Bedrock signing failed: {error}"),
            )
        })?
        .into_parts();
    for (name, value) in instructions.headers() {
        set_header(headers, name, value);
    }
    Ok(())
}

fn vertex_base_url(provider: &ProviderSnapshot) -> Result<String, PluginError> {
    let base = provider.base_url.trim_end_matches('/');
    if base.is_empty() {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "Vertex base URL must be normalized and saved before execution",
        ));
    }
    if base.contains('{') || base.contains('}') {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "Vertex base URL contains an unresolved placeholder",
        ));
    }
    Ok(base.to_string())
}

fn vertex_google_url(base: &str, path: &str) -> String {
    let (path, query) = path
        .split_once('?')
        .map_or((path, None), |(a, b)| (a, Some(b)));
    let Some(model_action) = path.split_once("/models/").map(|(_, rest)| rest) else {
        return join_url(base, path);
    };
    let mut url = format!(
        "{}/publishers/google/models/{model_action}",
        base.trim_end_matches('/')
    );
    if let Some(query) = query.filter(|query| !query.is_empty()) {
        url.push('?');
        url.push_str(query);
    }
    url
}

fn vertex_openai_url(base: &str, path: &str) -> String {
    let path = path.strip_prefix("/v1/").unwrap_or(path);
    join_url(base, path)
}

fn models_url(provider: &ProviderSnapshot) -> Result<String, PluginError> {
    if let Some(source) = provider
        .operation_metadata
        .get("models_source")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|source| !source.is_empty() && *source != "catalog")
    {
        return Ok(source.to_owned());
    }
    common::model_discovery_url(&provider.base_url)
}

fn validate_config(
    vendor_id: &str,
    provider: &ProviderSnapshot,
    proposed: &BTreeMap<String, Value>,
) -> ConfigValidationResponse {
    let merged = |key: &str| {
        proposed
            .get(key)
            .or_else(|| provider.options.get(key))
            .or_else(|| provider.credentials.get(key))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    let vertex_project = merged("project").map(str::to_owned).or_else(|| {
        merged("credentials").and_then(|credentials| {
            serde_json::from_str::<GoogleServiceAccount>(credentials)
                .ok()
                .and_then(|account| account.project_id)
                .filter(|project| !project.trim().is_empty())
        })
    });
    let mut issues = Vec::new();
    match vendor_id {
        "azure" => {
            require_value(
                &mut issues,
                "resourceName",
                "Azure resource name is required",
                merged("resourceName").is_some(),
            );
            require_value(
                &mut issues,
                "apiKey",
                "Azure API key is required",
                merged("apiKey").is_some(),
            );
        }
        "amazon-bedrock" => {
            require_value(
                &mut issues,
                "region",
                "AWS region is required",
                merged("region").is_some(),
            );
            if merged("apiKey").is_none()
                && (merged("accessKeyId").is_none() || merged("secretAccessKey").is_none())
            {
                issues.push(validation_issue(
                    "apiKey",
                    "auth_required",
                    "configure a Bedrock API key or both AWS access keys",
                ));
            }
        }
        "google-vertex" | "google-vertex-anthropic" => {
            if merged("credentials").is_none() && merged("apiKey").is_none() {
                issues.push(validation_issue(
                    "credentials",
                    "auth_required",
                    "service-account JSON or an access token is required",
                ));
            }
            if let Some(credentials) = merged("credentials")
                && credentials.trim_start().starts_with('{')
                && serde_json::from_str::<GoogleServiceAccount>(credentials)
                    .ok()
                    .is_none_or(|account| {
                        account.client_email.trim().is_empty()
                            || account.private_key.trim().is_empty()
                    })
            {
                issues.push(validation_issue(
                    "credentials",
                    "invalid_service_account",
                    "service-account JSON must contain client_email and private_key",
                ));
            }
            if vertex_project.is_none()
                && (provider.base_url.trim().is_empty()
                    || provider.base_url.contains('{')
                    || provider.base_url.contains('}'))
            {
                issues.push(validation_issue(
                    "project",
                    "required",
                    "Google Cloud project is required when it is not present in service-account JSON",
                ));
            }
        }
        "sap-ai-core" => {
            require_value(
                &mut issues,
                "deploymentUrl",
                "SAP deployment URL is required",
                merged("deploymentUrl").is_some(),
            );
            require_value(
                &mut issues,
                "tokenUrl",
                "SAP OAuth token URL is required",
                merged("tokenUrl").is_some(),
            );
            require_value(
                &mut issues,
                "clientId",
                "SAP OAuth client ID is required",
                merged("clientId").is_some(),
            );
            require_value(
                &mut issues,
                "clientSecret",
                "SAP OAuth client secret is required",
                merged("clientSecret").is_some(),
            );
        }
        "gitlab" => require_value(
            &mut issues,
            "apiKey",
            "GitLab access token is required",
            merged("apiKey").is_some(),
        ),
        "watsonx" => {
            require_value(
                &mut issues,
                "apiKey",
                "IBM Cloud API key is required",
                merged("apiKey").is_some(),
            );
            require_value(
                &mut issues,
                "projectId",
                "watsonx project ID is required",
                merged("projectId").is_some(),
            );
        }
        _ => issues.push(ValidationIssue {
            field: None,
            code: "unsupported_vendor".into(),
            message: "cloud config validator does not own this vendor".into(),
        }),
    }
    let proposed_base_url = if issues.is_empty() && provider.base_url.trim().is_empty() {
        match vendor_id {
            "azure" => merged("resourceName")
                .map(|resource| format!("https://{resource}.openai.azure.com/openai/v1")),
            "amazon-bedrock" => merged("region")
                .map(|region| format!("https://bedrock-runtime.{region}.amazonaws.com")),
            "google-vertex" => vertex_project.as_deref().map(|project| {
                let location = merged("location").unwrap_or("global");
                let suffix = if provider.channel == "openai" {
                    "/endpoints/openapi"
                } else {
                    ""
                };
                format!(
                    "https://aiplatform.googleapis.com/v1/projects/{project}/locations/{location}{suffix}"
                )
            }),
            "google-vertex-anthropic" => vertex_project.as_deref().map(|project| {
                let location = merged("location").unwrap_or("global");
                format!(
                    "https://aiplatform.googleapis.com/v1/projects/{project}/locations/{location}"
                )
            }),
            "sap-ai-core" => merged("deploymentUrl").map(|url| url.trim_end_matches('/').into()),
            "gitlab" => Some(format!(
                "{}/ai/v1/proxy/openai/v1",
                merged("aiGatewayUrl")
                    .unwrap_or("https://cloud.gitlab.com")
                    .trim_end_matches('/')
            )),
            "watsonx" => Some(
                merged("baseUrl")
                    .unwrap_or("https://us-south.ml.cloud.ibm.com")
                    .trim_end_matches('/')
                    .into(),
            ),
            _ => None,
        }
    } else {
        None
    };
    ConfigValidationResponse {
        issues,
        proposed_base_url,
    }
}

fn require_value(issues: &mut Vec<ValidationIssue>, field: &str, message: &str, present: bool) {
    if !present {
        issues.push(validation_issue(field, "required", message));
    }
}

fn validation_issue(field: &str, code: &str, message: &str) -> ValidationIssue {
    ValidationIssue {
        field: Some(field.into()),
        code: code.into(),
        message: message.into(),
    }
}

fn azure_descriptor() -> ProviderDescriptor {
    descriptor_base(
        "azure",
        "Azure OpenAI",
        vec![channel(
            "default",
            "Default",
            "openai-compatible",
            None,
            None,
            &[
                Capability::Infer,
                Capability::ModelDiscovery,
                Capability::ConfigValidation,
            ],
        )],
        vec![
            patterned_text_field(
                "resourceName",
                "Azure resource name",
                true,
                r"^[A-Za-z0-9](?:[A-Za-z0-9-]{0,62}[A-Za-z0-9])?$",
            ),
            text_field("apiKey", "API key", true, true),
            optional_text_field("apiVersion", "API version", Some("v1")),
        ],
        NetworkDeclaration::default(),
    )
}

fn bedrock_descriptor() -> ProviderDescriptor {
    descriptor_base(
        "amazon-bedrock",
        "Amazon Bedrock",
        vec![channel(
            "default",
            "Default",
            "bedrock-converse",
            None,
            Some(DefaultModelsSource::Catalog),
            &[
                Capability::Infer,
                Capability::ModelDiscovery,
                Capability::ConfigValidation,
            ],
        )],
        vec![
            patterned_text_field("region", "AWS region", true, r"^[a-z0-9-]+$"),
            text_field("apiKey", "Bedrock API key", false, true),
            text_field("accessKeyId", "Access key ID", false, true),
            text_field("secretAccessKey", "Secret access key", false, true),
            text_field("sessionToken", "Session token", false, true),
        ],
        NetworkDeclaration::default(),
    )
}

fn vertex_descriptor() -> ProviderDescriptor {
    descriptor_base(
        "google-vertex",
        "Vertex AI",
        vec![
            channel(
                "native",
                "Native Gemini",
                "google-gemini",
                None,
                None,
                &[
                    Capability::Infer,
                    Capability::ModelDiscovery,
                    Capability::ConfigValidation,
                ],
            ),
            channel(
                "openai",
                "OpenAI Compatible",
                "openai-compatible",
                None,
                None,
                &[
                    Capability::Infer,
                    Capability::ModelDiscovery,
                    Capability::ConfigValidation,
                ],
            ),
        ],
        vertex_fields(),
        NetworkDeclaration {
            base_url_field: None,
            extra_origins: vec![
                https_origin("aiplatform.googleapis.com"),
                https_origin(GOOGLE_TOKEN_ORIGIN),
            ],
            field_origins: Vec::new(),
        },
    )
}

fn vertex_anthropic_descriptor() -> ProviderDescriptor {
    descriptor_base(
        "google-vertex-anthropic",
        "Vertex AI Anthropic",
        vec![channel(
            "default",
            "Default",
            "anthropic-messages",
            None,
            None,
            &[
                Capability::Infer,
                Capability::ModelDiscovery,
                Capability::ConfigValidation,
            ],
        )],
        vertex_fields(),
        NetworkDeclaration {
            base_url_field: None,
            extra_origins: vec![
                https_origin("aiplatform.googleapis.com"),
                https_origin(GOOGLE_TOKEN_ORIGIN),
            ],
            field_origins: Vec::new(),
        },
    )
}

fn sap_descriptor() -> ProviderDescriptor {
    descriptor_base(
        "sap-ai-core",
        "SAP AI Core",
        vec![channel(
            "default",
            "Default",
            "openai-compatible",
            None,
            None,
            &[
                Capability::Infer,
                Capability::ModelDiscovery,
                Capability::ConfigValidation,
            ],
        )],
        vec![
            text_field("deploymentUrl", "Deployment URL", true, false),
            text_field("tokenUrl", "OAuth token URL", true, false),
            text_field("clientId", "OAuth client ID", true, true),
            text_field("clientSecret", "OAuth client secret", true, true),
            text_field("resourceGroup", "Resource group", false, false),
        ],
        NetworkDeclaration {
            base_url_field: Some("deploymentUrl".into()),
            extra_origins: Vec::new(),
            field_origins: vec!["tokenUrl".into()],
        },
    )
}

fn gitlab_descriptor() -> ProviderDescriptor {
    descriptor_base(
        "gitlab",
        "GitLab Duo",
        vec![channel(
            "default",
            "Default",
            "openai-compatible",
            Some("https://cloud.gitlab.com/ai/v1/proxy/openai/v1"),
            None,
            &[
                Capability::Infer,
                Capability::AuthOauth,
                Capability::ModelDiscovery,
                Capability::ConfigValidation,
            ],
        )],
        vec![
            text_field("apiKey", "GitLab access token", true, true),
            optional_text_field(
                "instanceUrl",
                "GitLab instance URL",
                Some("https://gitlab.com"),
            ),
            optional_text_field(
                "aiGatewayUrl",
                "GitLab AI Gateway URL",
                Some("https://cloud.gitlab.com"),
            ),
        ],
        NetworkDeclaration {
            base_url_field: None,
            extra_origins: vec![https_origin("gitlab.com"), https_origin("cloud.gitlab.com")],
            field_origins: vec!["instanceUrl".into(), "aiGatewayUrl".into()],
        },
    )
}

fn watsonx_descriptor() -> ProviderDescriptor {
    descriptor_base(
        "watsonx",
        "watsonx.ai",
        vec![channel(
            "default",
            "Default",
            "watsonx-text-chat",
            Some("https://us-south.ml.cloud.ibm.com"),
            Some(DefaultModelsSource::Catalog),
            &[
                Capability::Infer,
                Capability::ModelDiscovery,
                Capability::ConfigValidation,
            ],
        )],
        vec![
            text_field("apiKey", "IBM Cloud API key", true, true),
            text_field("projectId", "Project ID", true, false),
            optional_text_field(
                "baseUrl",
                "Service URL",
                Some("https://us-south.ml.cloud.ibm.com"),
            ),
            optional_text_field("apiVersion", "API version", Some("2026-04-20")),
        ],
        NetworkDeclaration {
            base_url_field: Some("baseUrl".into()),
            extra_origins: vec![https_origin("iam.cloud.ibm.com")],
            field_origins: vec!["baseUrl".into()],
        },
    )
}

fn descriptor_base(
    vendor_id: &str,
    display_name: &str,
    channels: Vec<ChannelDescriptor>,
    config_fields: Vec<ConfigField>,
    network: NetworkDeclaration,
) -> ProviderDescriptor {
    let capabilities = channels
        .iter()
        .flat_map(|channel| channel.capabilities.iter().copied())
        .collect();
    ProviderDescriptor {
        provider_id: vendor_id.into(),
        catalog_id: Some(vendor_id.into()),
        display_name: display_name.into(),
        description: Some(format!("Built-in {display_name} vendor integration")),
        channels,
        capabilities,
        config_fields,
        network,
        data_compat: DataCompatibility::default(),
        website: None,
        implementation: None,
    }
}

fn channel(
    id: &str,
    name: &str,
    protocol: &str,
    default_base_url: Option<&str>,
    default_models_source: Option<DefaultModelsSource>,
    capabilities: &[Capability],
) -> ChannelDescriptor {
    ChannelDescriptor {
        id: id.into(),
        name: name.into(),
        description: None,
        auth: None,
        protocol: Some(protocol.into()),
        protocols: Vec::new(),
        default_base_url: default_base_url.map(str::to_owned),
        default_models_source,
        capabilities: capabilities.iter().copied().collect::<BTreeSet<_>>(),
        model_capabilities: BTreeSet::new(),
        search_model_required: false,
    }
}

fn vertex_fields() -> Vec<ConfigField> {
    vec![
        patterned_text_field(
            "project",
            "Google Cloud project",
            false,
            r"^[a-z][a-z0-9-]{4,28}[a-z0-9]$",
        ),
        patterned_text_field("location", "Google Cloud location", false, r"^[a-z0-9-]+$"),
        multiline_field("credentials", "Service account JSON", false, true),
        text_field("apiKey", "Vertex access token", false, true),
    ]
}

fn text_field(key: &str, label: &str, required: bool, secret: bool) -> ConfigField {
    ConfigField {
        key: key.into(),
        label: label.into(),
        description: None,
        kind: ConfigFieldKind::String { multiline: false },
        required,
        default_json: None,
        group: Some(if secret { "Credentials" } else { "Connection" }.into()),
        secret,
        min: None,
        max: None,
        max_length: Some(if secret { 65_536 } else { 2_048 }),
        pattern: None,
        visible_when: None,
    }
}

fn patterned_text_field(key: &str, label: &str, required: bool, pattern: &str) -> ConfigField {
    let mut field = text_field(key, label, required, false);
    field.pattern = Some(pattern.into());
    field
}

fn multiline_field(key: &str, label: &str, required: bool, secret: bool) -> ConfigField {
    let mut field = text_field(key, label, required, secret);
    field.kind = ConfigFieldKind::String { multiline: true };
    field
}

fn optional_text_field(key: &str, label: &str, default: Option<&str>) -> ConfigField {
    let mut field = text_field(key, label, false, false);
    field.default_json = default.map(|value| Value::String(value.into()));
    field
}

fn https_origin(host: &str) -> OriginDeclaration {
    OriginDeclaration {
        scheme: "https".into(),
        host: host.into(),
        port: None,
    }
}

fn required_string(
    provider: &ProviderSnapshot,
    key: &str,
    message: &str,
) -> Result<String, PluginError> {
    string_value(provider, key).ok_or_else(|| common::plugin_error(ErrorKind::Auth, message))
}

fn string_value(provider: &ProviderSnapshot, key: &str) -> Option<String> {
    provider
        .credentials
        .get(key)
        .or_else(|| provider.options.get(key))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn set_header(headers: &mut Vec<(String, String)>, name: &str, value: impl Into<String>) {
    headers.retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
    headers.push((name.into(), value.into()));
}

fn join_url(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

fn json_body(value: Value) -> Result<Vec<u8>, PluginError> {
    serde_json::to_vec(&value)
        .map_err(|_| common::plugin_error(ErrorKind::Invalid, "request body could not be encoded"))
}

fn form_body(values: &[(&str, &str)]) -> Vec<u8> {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in values {
        serializer.append_pair(key, value);
    }
    serializer.finish().into_bytes()
}

fn sha256_hex(value: &[u8]) -> String {
    let digest = Sha256::digest(value);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

fn is_safe_header_name(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

fn input_mismatch() -> PluginError {
    common::plugin_error(
        ErrorKind::Invalid,
        "operation input does not match operation",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(base_url: &str, channel: &str) -> ProviderSnapshot {
        ProviderSnapshot {
            provider_id: "azure".into(),
            channel: channel.into(),
            base_url: base_url.into(),
            protocol: "openai-compatible".into(),
            options: BTreeMap::new(),
            credentials: BTreeMap::new(),
            model: None,
            model_metadata: None,
            client_headers: Vec::new(),
            operation_metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn cloud_config_derives_only_when_the_admin_left_base_url_blank() {
        let mut azure = provider("  ", "default");
        azure
            .options
            .insert("resourceName".into(), Value::String("tenant-a".into()));
        azure
            .credentials
            .insert("apiKey".into(), Value::String("secret".into()));
        let validation = validate_config("azure", &azure, &BTreeMap::new());
        assert!(validation.issues.is_empty());
        assert_eq!(
            validation.proposed_base_url.as_deref(),
            Some("https://tenant-a.openai.azure.com/openai/v1")
        );

        for explicit in [
            "http://127.0.0.1:8080/openai/v1",
            "https://proxy.example.test/azure/openai/v1",
        ] {
            azure.base_url = explicit.into();
            let validation = validate_config("azure", &azure, &BTreeMap::new());
            assert!(validation.issues.is_empty());
            assert_eq!(validation.proposed_base_url, None, "{explicit}");
        }

        let mut sap = provider("https://proxy.example.test/sap", "default");
        sap.options.extend([
            (
                "deploymentUrl".into(),
                Value::String("https://derived.example.test".into()),
            ),
            (
                "tokenUrl".into(),
                Value::String("https://auth.example.test/token".into()),
            ),
        ]);
        sap.credentials.extend([
            ("clientId".into(), Value::String("client".into())),
            ("clientSecret".into(), Value::String("secret".into())),
        ]);
        let validation = validate_config("sap-ai-core", &sap, &BTreeMap::new());
        assert!(validation.issues.is_empty());
        assert_eq!(validation.proposed_base_url, None);
    }

    #[test]
    fn vertex_google_url_targets_publisher_model_and_preserves_query() {
        assert_eq!(
            vertex_google_url(
                "https://aiplatform.googleapis.com/v1/projects/demo/locations/global",
                "/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse",
            ),
            "https://aiplatform.googleapis.com/v1/projects/demo/locations/global/publishers/google/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn vertex_openai_url_avoids_duplicate_version_segment() {
        assert_eq!(
            vertex_openai_url(
                "https://aiplatform.googleapis.com/v1/projects/demo/locations/global/endpoints/openapi",
                "/v1/chat/completions",
            ),
            "https://aiplatform.googleapis.com/v1/projects/demo/locations/global/endpoints/openapi/chat/completions"
        );
    }
}
