use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use parking_lot::Mutex;
use serde_json::{Value, json};
use stravia_core::Gateway;
use stravia_core::auth::types::{
    AuthSessionCandidate, OAuthCallbackMode, OAuthSessionStartOptions,
};
use stravia_core::auth::{AuthCompletionInput, AuthCompletionValue};
use stravia_core::config::GatewayConfig;
use stravia_core::db::models::{
    CreateApiKey, CreateProvider, CreateRoute, ProviderCredentialInput, ProviderSourceInput,
};
use stravia_core::provider_models::CreateManualProviderModel;
use stravia_core::storage::MemoryStorage;
use stravia_vendor_sdk::{Capability, DefaultModelsSource};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceExt;

mod vendor_observation;
mod vendor_plugin_artifacts;
use vendor_observation::{finished_observation, observation_bundle_records, wire_payload_bytes};
use vendor_plugin_artifacts::install_distributed_vendor_plugin;

#[derive(Clone, Debug)]
struct ObservedRequest {
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

#[derive(Clone)]
struct MockResponse {
    status: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
}

impl MockResponse {
    fn json(body: Value) -> Self {
        Self::status_json("200 OK", body)
    }

    fn status_json(status: &'static str, body: Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: serde_json::to_vec(&body).expect("fixture JSON"),
        }
    }

    fn bytes(content_type: &'static str, body: Vec<u8>) -> Self {
        Self {
            status: "200 OK",
            content_type,
            body,
        }
    }
}

async fn read_http_request(socket: &mut TcpStream) -> anyhow::Result<ObservedRequest> {
    let mut bytes = Vec::new();
    let mut scratch = [0_u8; 4096];
    let (header_end, content_length) = loop {
        let read = socket.read(&mut scratch).await?;
        anyhow::ensure!(read != 0, "local upstream closed before request headers");
        bytes.extend_from_slice(&scratch[..read]);
        if let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let header_text = std::str::from_utf8(&bytes[..header_end])?;
            let content_length = header_text
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            break (header_end + 4, content_length);
        }
        anyhow::ensure!(
            bytes.len() <= 4 * 1024 * 1024,
            "local upstream request headers too large"
        );
    };
    while bytes.len() < header_end + content_length {
        let read = socket.read(&mut scratch).await?;
        anyhow::ensure!(read != 0, "local upstream closed before request body");
        bytes.extend_from_slice(&scratch[..read]);
    }

    let header_text = std::str::from_utf8(&bytes[..header_end - 4])?;
    let mut lines = header_text.lines();
    let path = lines
        .next()
        .and_then(|line| line.split_ascii_whitespace().nth(1))
        .unwrap_or_default()
        .to_string();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    Ok(ObservedRequest {
        path,
        headers,
        body: bytes[header_end..header_end + content_length].to_vec(),
    })
}

async fn local_upstream(
    request_count: usize,
    response: impl Fn(&ObservedRequest) -> MockResponse + Send + Sync + 'static,
) -> anyhow::Result<(
    String,
    tokio::task::JoinHandle<anyhow::Result<Vec<ObservedRequest>>>,
)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let response = Arc::new(response);
    let server = tokio::spawn(async move {
        let mut observed = Vec::with_capacity(request_count);
        for _ in 0..request_count {
            let (mut socket, _) = listener.accept().await?;
            let request = read_http_request(&mut socket).await?;
            let reply = response(&request);
            let head = format!(
                "HTTP/1.1 {}\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                reply.status,
                reply.content_type,
                reply.body.len()
            );
            socket.write_all(head.as_bytes()).await?;
            socket.write_all(&reply.body).await?;
            observed.push(request);
        }
        Ok(observed)
    });
    Ok((format!("http://{address}"), server))
}

async fn gateway() -> anyhow::Result<(tempfile::TempDir, Gateway)> {
    let directory = tempfile::tempdir()?;
    let gateway = Gateway::new(GatewayConfig {
        data_dir: directory.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    Ok((directory, gateway))
}

struct CloudCatalogSource;

#[async_trait::async_trait]
impl stravia_core::provider_catalog::CatalogSource for CloudCatalogSource {
    async fn fetch_version(
        &self,
    ) -> anyhow::Result<stravia_core::provider_catalog::CatalogVersion> {
        Ok(stravia_core::provider_catalog::CatalogVersion {
            revision: "bootstrap".into(),
            generated_at: "test".into(),
        })
    }

    async fn fetch_canonical_models(&self) -> anyhow::Result<Vec<u8>> {
        anyhow::bail!("unexpected canonical catalog refresh")
    }

    async fn fetch_logo(&self, _provider_id: &str) -> anyhow::Result<Vec<u8>> {
        anyhow::bail!("unexpected catalog logo request")
    }

    async fn fetch_favicon(&self, _origin: &str) -> anyhow::Result<Vec<u8>> {
        anyhow::bail!("unexpected catalog favicon request")
    }
}

/// Provider scopes arrive through the base plugin's `sync-catalog` export and
/// land in the on-disk cache; tests seed the bootstrap revision directly so
/// cloud catalog enrichment stays offline.
fn seed_cloud_catalog_scopes(data_dir: &std::path::Path) -> anyhow::Result<()> {
    let body = serde_json::to_vec(&json!({
        "catalog-only-model": {
            "id": "catalog-only-model",
            "name": "Catalog-only cloud model",
            "modalities": {"input":["text"],"output":["text"]}
        }
    }))?;
    let directory = stravia_core::data_paths::DataPaths::new(data_dir)
        .catalog_root()
        .join("catalog/scopes/bootstrap");
    for provider_id in ["amazon-bedrock", "watsonx", "google-vertex"] {
        std::fs::create_dir_all(&directory)?;
        std::fs::write(directory.join(format!("{provider_id}.json")), &body)?;
    }
    Ok(())
}

async fn provider_route_and_key(
    gateway: &Gateway,
    name: &str,
    source: ProviderSourceInput,
    upstream_model: &str,
    credential: ProviderCredentialInput,
    vendor_options: serde_json::Map<String, Value>,
    model_metadata: Value,
) -> anyhow::Result<(String, String)> {
    let provider = gateway
        .admin()
        .create_provider(CreateProvider {
            name: Some(format!("{name} provider")),
            source,
            credential,
            vendor_options,
            use_proxy: false,
        })
        .await?;
    gateway
        .admin()
        .create_manual_provider_model(
            &provider.id,
            upstream_model,
            CreateManualProviderModel {
                metadata: model_metadata,
                template_id: None,
            },
        )
        .await?;
    let route = gateway
        .admin()
        .create_model(CreateRoute {
            model_id: format!("{name}-route"),
            display_name: None,
            balance: None,
            targets: vec![stravia_core::db::models::CreateTarget {
                provider_id: provider.id.clone(),
                model: Some(upstream_model.to_string()),
                enabled: true,
                priority: None,
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
        })
        .await?;
    let key = gateway
        .admin()
        .create_api_key(CreateApiKey {
            key: None,
            name: format!("{name} key"),
            concurrency_limit: None,
            expires_at: None,
            mcp_access_enabled: true,
            transparent_injection_enabled: false,
            inject_media_understanding: false,
            inject_web_search: false,
            inject_media_generation: false,
            model_ids: vec![route.id.into()],
        })
        .await?;
    Ok((route.model_id.into(), key.token))
}

async fn chat(gateway: Gateway, token: &str, body: Value) -> anyhow::Result<(StatusCode, Value)> {
    let response = stravia_core::proxy::server::create_router(gateway)
        .oneshot(
            Request::post("/v1/chat/completions")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body)?))?,
        )
        .await?;
    let status = response.status();
    let body = to_bytes(response.into_body(), 16 * 1024 * 1024).await?;
    let value = serde_json::from_slice(&body)
        .unwrap_or_else(|_| json!({"non_json_body": String::from_utf8_lossy(&body).into_owned()}));
    Ok((status, value))
}

async fn embeddings(
    gateway: Gateway,
    token: &str,
    body: Value,
) -> anyhow::Result<(StatusCode, Value)> {
    let response = stravia_core::proxy::server::create_router(gateway)
        .oneshot(
            Request::post("/v1/embeddings")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body)?))?,
        )
        .await?;
    let status = response.status();
    let body = to_bytes(response.into_body(), 16 * 1024 * 1024).await?;
    let value = serde_json::from_slice(&body)
        .unwrap_or_else(|_| json!({"non_json_body": String::from_utf8_lossy(&body).into_owned()}));
    Ok((status, value))
}

fn standard_client_request(model: &str) -> Value {
    json!({
        "model": model,
        "stream": false,
        "messages": [{"role": "user", "content": "Use the lookup tool."}],
        "reasoning_effort": "high",
        "tools": [{
            "type": "function",
            "function": {
                "name": "lookup",
                "description": "look up a value",
                "parameters": {
                    "type": "object",
                    "properties": {"key": {"type": "string"}},
                    "required": ["key"]
                }
            }
        }],
        "tool_choice": "auto"
    })
}

#[tokio::test]
async fn builtin_api_credentials_preserve_existing_connection_field_names() -> anyhow::Result<()> {
    for (vendor, protocol, header) in [
        ("openai", "openai-compatible", "authorization"),
        ("anthropic", "anthropic-messages", "x-api-key"),
    ] {
        let (base_url, server) = local_upstream(1, move |_| {
            MockResponse::json(if vendor == "anthropic" {
                json!({"id":"msg-auth","type":"message","role":"assistant","model":"upstream-model",
                    "content":[{"type":"text","text":"authenticated"}],"stop_reason":"end_turn",
                    "usage":{"input_tokens":1,"output_tokens":1}})
            } else {
                stravia_protocol_codec::codec::open_responses::formatter::response_resource_snapshot(
                    "resp-auth", "upstream-model", "completed",
                    vec![json!({"id":"msg-auth","type":"message","role":"assistant","status":"completed",
                        "content":[{"type":"output_text","text":"authenticated","annotations":[]}]})],
                    Value::Null, Value::Null, Value::Null,
                )
            })
        }).await?;
        let (_directory, gateway) = gateway().await?;
        let (route, token) = provider_route_and_key(
            &gateway,
            vendor,
            ProviderSourceInput::Custom {
                vendor: vendor.into(),
                channel: "default".into(),
                protocol: Some(protocol.into()),
                base_url,
                models_source: None,
                static_models: None,
            },
            "upstream-model",
            ProviderCredentialInput::ApiKey {
                value: "legacy-contract-key".into(),
            },
            Default::default(),
            json!({"id":"upstream-model","name":"upstream-model"}),
        )
        .await?;
        let (status, response) = chat(
            gateway,
            &token,
            json!({"model":route,"messages":[{"role":"user","content":"hello"}]}),
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "{vendor}: {response}");
        assert_eq!(
            response["choices"][0]["message"]["content"],
            "authenticated"
        );
        let requests = server.await??;
        assert_eq!(
            requests[0].headers.get(header).map(String::as_str),
            Some(if vendor == "openai" {
                "Bearer legacy-contract-key"
            } else {
                "legacy-contract-key"
            }),
        );
    }
    Ok(())
}

#[tokio::test]
async fn custom_compatible_connection_can_call_an_explicit_local_upstream_without_credentials()
-> anyhow::Result<()> {
    let (base_url, server) = local_upstream(1, |_| MockResponse::json(json!({
        "id":"chat-local","object":"chat.completion","model":"local-model",
        "choices":[{"index":0,"message":{"role":"assistant","content":"local answer"},"finish_reason":"stop"}]
    }))).await?;
    let (_directory, gateway) = gateway().await?;
    let (route, token) = provider_route_and_key(
        &gateway,
        "local-compatible",
        ProviderSourceInput::Custom {
            vendor: "custom".into(),
            channel: "default".into(),
            protocol: Some("openai-compatible".into()),
            base_url,
            models_source: None,
            static_models: None,
        },
        "local-model",
        ProviderCredentialInput::None,
        Default::default(),
        json!({"id":"local-model","name":"local-model"}),
    )
    .await?;
    let (status, response) = chat(
        gateway,
        &token,
        json!({"model":route,"messages":[{"role":"user","content":"hello"}]}),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["choices"][0]["message"]["content"], "local answer");
    let requests = server.await??;
    assert!(!requests[0].headers.contains_key("authorization"));
    Ok(())
}

#[tokio::test]
async fn custom_and_standard_openai_profiles_use_the_embeddings_codec_over_wasm()
-> anyhow::Result<()> {
    for (name, vendor) in [
        ("custom-embeddings", "custom"),
        ("standard-openai-embeddings", "openai-compatible"),
    ] {
        let (base_url, server) = local_upstream(1, |_| {
            MockResponse::json(json!({
                "object": "list",
                "data": [{"object": "embedding", "index": 0, "embedding": [0.25, -0.75]}],
                "model": "custom-embedding-model",
                "usage": {"prompt_tokens": 2, "total_tokens": 2}
            }))
        })
        .await?;
        let (_directory, gateway) = gateway().await?;
        let (route, token) = provider_route_and_key(
            &gateway,
            name,
            ProviderSourceInput::Custom {
                vendor: vendor.into(),
                channel: "default".into(),
                protocol: Some("openai-compatible".into()),
                base_url,
                models_source: None,
                static_models: None,
            },
            "custom-embedding-model",
            ProviderCredentialInput::ApiKey {
                value: "custom-embedding-key".into(),
            },
            Default::default(),
            json!({"id": "custom-embedding-model", "name": "custom-embedding-model"}),
        )
        .await?;

        let (status, response) = embeddings(
            gateway,
            &token,
            json!({
                "model": route,
                "input": "embed this",
                "dimensions": 2,
                "encoding_format": "float"
            }),
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "{name}: {response}");
        assert_eq!(response["data"][0]["embedding"], json!([0.25, -0.75]));

        let requests = server.await??;
        assert_eq!(requests[0].path, "/v1/embeddings", "{name}");
        assert_eq!(
            requests[0].headers.get("authorization").map(String::as_str),
            Some("Bearer custom-embedding-key")
        );
        let body: Value = serde_json::from_slice(&requests[0].body)?;
        assert_eq!(body["model"], "custom-embedding-model");
        assert_eq!(body["input"], "embed this");
        assert_eq!(body["dimensions"], 2);
    }
    Ok(())
}

struct StandardCase {
    name: &'static str,
    vendor: &'static str,
    protocol: &'static str,
    response: Value,
    request_assertion: fn(&Value),
}

#[tokio::test]
async fn supplemental_discovery_capabilities_preserve_explicit_model_specifications()
-> anyhow::Result<()> {
    let (base_url, server) = local_upstream(1, |_| MockResponse::json(json!({"data":[
        {"id":"minimax-m3","tool_call":true,"reasoning":true,"attachment":true,"structured_output":true},
        {"id":"explicitly-disabled","tool_call":false,"reasoning":false,
            "capabilities":["tools","reasoning"]},
        {"id":"capabilities-only","capabilities":["image_input","structured_output"],"context_window":16384}
    ]}))).await?;
    let (_directory, gateway) = gateway().await?;
    let provider = gateway
        .admin()
        .create_provider(CreateProvider {
            name: Some("Discovery specifications".into()),
            source: ProviderSourceInput::Custom {
                vendor: "custom".into(),
                channel: "default".into(),
                protocol: Some("openai-compatible".into()),
                base_url,
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::ApiKey {
                value: "fixture-key".into(),
            },
            vendor_options: Default::default(),
            use_proxy: false,
        })
        .await?;
    gateway.admin().sync_provider_models(&provider.id).await?;
    let enabled = gateway
        .admin()
        .get_provider_model(&provider.id, "minimax-m3")
        .await?;
    assert_eq!(enabled.metadata.tool_call, Some(true));
    assert_eq!(enabled.metadata.reasoning, Some(true));
    assert_eq!(enabled.metadata.attachment, Some(true));
    assert_eq!(enabled.metadata.structured_output, Some(true));
    let disabled = gateway
        .admin()
        .get_provider_model(&provider.id, "explicitly-disabled")
        .await?;
    assert_eq!(disabled.metadata.tool_call, Some(false));
    assert_eq!(disabled.metadata.reasoning, Some(false));
    let inferred = gateway
        .admin()
        .get_provider_model(&provider.id, "capabilities-only")
        .await?;
    assert_eq!(inferred.metadata.attachment, Some(true));
    assert_eq!(inferred.metadata.structured_output, Some(true));
    assert_eq!(
        inferred
            .metadata
            .limit
            .as_ref()
            .and_then(|limit| limit.context),
        Some(16384)
    );
    assert_eq!(
        inferred
            .metadata
            .modalities
            .as_ref()
            .expect("discovered input")
            .input,
        ["image"]
    );
    server.await??;
    Ok(())
}

#[tokio::test]
async fn discovery_declared_context_window_overrides_catalog_defaults() -> anyhow::Result<()> {
    // Canonical `openai/gpt-5.2-codex` 声明 limit {context:400000,input:272000,
    // output:128000}；上游直接声明的 context_window 覆盖 context，上游未声明
    // 的 input/output 默认配额随旧窗口一并失效。
    let (base_url, server) = local_upstream(1, |_| {
        MockResponse::json(json!({"data":[
            {"id":"gpt-5.2-codex","context_window":272000}
        ]}))
    })
    .await?;
    let (_directory, gateway) = gateway().await?;
    let provider = gateway
        .admin()
        .create_provider(CreateProvider {
            name: Some("Discovery declared specifications".into()),
            source: ProviderSourceInput::Custom {
                vendor: "custom".into(),
                channel: "default".into(),
                protocol: Some("openai-compatible".into()),
                base_url,
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::ApiKey {
                value: "fixture-key".into(),
            },
            vendor_options: Default::default(),
            use_proxy: false,
        })
        .await?;
    gateway.admin().sync_provider_models(&provider.id).await?;

    let overridden = gateway
        .admin()
        .get_provider_model(&provider.id, "gpt-5.2-codex")
        .await?;
    let limit = overridden.metadata.limit.expect("canonical limit");
    assert_eq!(limit.context, Some(272_000));
    assert_eq!(limit.input, None);
    assert_eq!(limit.output, None);
    server.await??;
    Ok(())
}

#[tokio::test]
async fn google_protocol_alias_preserves_query_authentication() -> anyhow::Result<()> {
    let (base_url, server) = local_upstream(1, |_| MockResponse::json(json!({
        "responseId":"google-auth","modelVersion":"gemini-fixture",
        "candidates":[{"content":{"role":"model","parts":[{"text":"authenticated"}]},"finishReason":"STOP"}]
    }))).await?;
    let (_directory, gateway) = gateway().await?;
    let (route, token) = provider_route_and_key(
        &gateway,
        "google-auth",
        ProviderSourceInput::Custom {
            vendor: "google".into(),
            channel: "default".into(),
            protocol: Some("google-gemini".into()),
            base_url,
            models_source: None,
            static_models: None,
        },
        "gemini-fixture",
        ProviderCredentialInput::ApiKey {
            value: "google-fixture-key".into(),
        },
        Default::default(),
        json!({"id":"gemini-fixture","name":"Gemini fixture"}),
    )
    .await?;
    let (status, response) = chat(
        gateway,
        &token,
        json!({"model":route,"messages":[{"role":"user","content":"hello"}]}),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{response}");
    let requests = server.await??;
    let target = url::Url::parse(&format!("http://localhost{}", requests[0].path))?;
    assert!(
        target
            .query_pairs()
            .any(|(key, value)| key == "key" && value == "google-fixture-key")
    );
    assert!(!requests[0].headers.contains_key("authorization"));
    Ok(())
}

#[tokio::test]
async fn new_supplier_installation_does_not_report_an_incompatible_update() -> anyhow::Result<()> {
    let (_directory, gateway) = gateway().await?;
    let component = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../target/vendor-test-fixtures/lifecycle-v1.wasm"),
    )?;
    let preview = gateway.admin().preview_vendor_plugin(component).await?;
    assert!(!preview.cancels_active_operations);
    assert!(preview.discarded_data.is_empty());
    Ok(())
}

#[tokio::test]
async fn removed_base_profile_keeps_its_unavailable_route_visible_after_update_and_restart()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let gateway = Gateway::new(GatewayConfig {
        data_dir: directory.path().to_owned(),
        ..Default::default()
    })
    .await?;
    let mut routes = Vec::new();
    for name in ["removed-deepseek-a", "removed-deepseek-b"] {
        let (route, _token) = provider_route_and_key(
            &gateway,
            name,
            ProviderSourceInput::Custom {
                vendor: "deepseek".into(),
                channel: "default".into(),
                protocol: Some("openai-compatible".into()),
                base_url: "http://127.0.0.1:9".into(),
                models_source: Some("http://127.0.0.1:10/models?region=test".into()),
                static_models: None,
            },
            "deepseek-chat",
            ProviderCredentialInput::ApiKey {
                value: "local-fixture-key".into(),
            },
            Default::default(),
            json!({"id":"deepseek-chat","name":"DeepSeek"}),
        )
        .await?;
        routes.push(route);
    }
    let component = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../target/vendor-test-fixtures/lifecycle-base-local.wasm"),
    )?;
    let preview = gateway.admin().preview_vendor_plugin(component).await?;
    assert_eq!(
        preview
            .removed_network_permissions
            .iter()
            .filter(|origin| origin.as_str() == "http://127.0.0.1:9")
            .count(),
        1
    );
    assert_eq!(
        preview
            .removed_network_permissions
            .iter()
            .filter(|origin| origin.as_str() == "http://127.0.0.1:10")
            .count(),
        1
    );
    assert!(routes.iter().all(|route| {
        preview
            .affected_bindings
            .iter()
            .any(|impact| impact.route_id == *route)
    }));
    let updated = gateway
        .admin()
        .confirm_vendor_plugin(stravia_core::plugin::ConfirmPluginUpdate {
            preview_id: preview.id,
            allow_data_discard: true,
        })
        .await?;
    assert!(routes.iter().all(|route| {
        updated
            .affected_bindings
            .iter()
            .any(|impact| impact.route_id == *route)
    }));
    drop(gateway);
    let restarted = Gateway::new(GatewayConfig {
        data_dir: directory.path().to_owned(),
        ..Default::default()
    })
    .await?;
    let installed = restarted.admin().list_vendor_plugins().await?;
    let base = installed
        .iter()
        .find(|plugin| plugin.vendor_id == "base")
        .unwrap();
    assert!(routes.iter().all(|route| {
        base.affected_bindings
            .iter()
            .any(|impact| impact.route_id == *route)
    }));
    Ok(())
}

fn assert_openai_standard_request(body: &Value) {
    assert_eq!(body["reasoning_effort"], "high");
    assert_eq!(body["tools"][0]["function"]["name"], "lookup");
}

fn assert_responses_standard_request(body: &Value) {
    assert_eq!(body["reasoning"]["effort"], "high");
    assert_eq!(body["tools"][0]["name"], "lookup");
}

fn assert_anthropic_standard_request(body: &Value) {
    assert!(body.get("thinking").is_some() || body.pointer("/output_config/effort").is_some());
    assert_eq!(body["tools"][0]["name"], "lookup");
}

fn assert_gemini_standard_request(body: &Value) {
    assert_eq!(
        body.pointer("/generationConfig/thinkingConfig/thinkingLevel"),
        Some(&json!("HIGH"))
    );
    assert_eq!(
        body["tools"][0]["functionDeclarations"][0]["name"],
        "lookup"
    );
}

#[tokio::test]
async fn fresh_inventory_is_exactly_base_and_persists_no_wasm_artifact() -> anyhow::Result<()> {
    let (directory, gateway) = gateway().await?;
    let packages = gateway.admin().list_vendor_plugins().await?;
    assert_eq!(packages.len(), 1);
    assert_eq!(packages[0].vendor_id, "base");
    assert_eq!(packages[0].status, "ready");
    let artifact_directory = stravia_core::data_paths::DataPaths::new(directory.path())
        .plugins()
        .join("artifacts");
    if artifact_directory.is_dir() {
        for entry in std::fs::read_dir(&artifact_directory)? {
            assert_ne!(
                entry?
                    .path()
                    .extension()
                    .and_then(|extension| extension.to_str()),
                Some("wasm"),
                "fresh Gateway must load the bundled base component from memory without persisting Wasm"
            );
        }
    }
    let profiles = gateway.admin().list_vendor_metadata().await?;
    for provider_id in ["command-code", "devin", "openai-codex", "xai-grok"] {
        assert!(
            profiles
                .iter()
                .all(|profile| profile.provider_id != provider_id),
            "fresh Gateway unexpectedly exposes dedicated provider profile {provider_id}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn manually_installed_dedicated_packages_keep_complete_profiles() -> anyhow::Result<()> {
    let (_directory, gateway) = gateway().await?;
    for vendor_id in ["command-code", "devin", "openai-codex", "xai-grok"] {
        let installed = install_distributed_vendor_plugin(&gateway, vendor_id).await?;
        assert_eq!(installed.vendor_id, vendor_id);
        assert_eq!(installed.source, stravia_core::plugin::PluginSource::Local);
        assert_eq!(installed.status, "ready");
    }

    let profiles = gateway.admin().list_vendor_metadata().await?;
    let profile = |provider_id: &str| {
        profiles
            .iter()
            .find(|profile| profile.provider_id == provider_id)
            .unwrap_or_else(|| panic!("missing effective provider profile {provider_id}"))
    };
    let openai = profile("openai");
    assert_eq!(openai.catalog_id.as_deref(), Some("openai"));
    assert!(
        openai
            .channels
            .iter()
            .any(|channel| channel.id == "default")
    );
    assert!(openai.channels.iter().all(|channel| channel.id != "codex"));
    let xai = profile("xai");
    assert_eq!(xai.catalog_id.as_deref(), Some("xai"));
    assert!(xai.channels.iter().any(|channel| channel.id == "default"));
    assert!(xai.channels.iter().all(|channel| channel.id != "grok"));
    for (provider_id, catalog_id, channel_id) in [
        ("openai-codex", "openai", "codex"),
        ("xai-grok", "xai", "grok"),
        ("command-code", "command-code", "default"),
        ("devin", "devin", "devin"),
    ] {
        let dedicated = profile(provider_id);
        assert_eq!(dedicated.catalog_id.as_deref(), Some(catalog_id));
        assert_eq!(
            dedicated
                .channels
                .iter()
                .map(|channel| channel.id.as_str())
                .collect::<Vec<_>>(),
            [channel_id]
        );
    }

    let observed_capabilities = profiles
        .iter()
        .flat_map(|profile| profile.capabilities.iter().copied())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        observed_capabilities,
        std::collections::BTreeSet::from([
            Capability::Infer,
            Capability::Compact,
            Capability::Search,
            Capability::MediaImage,
            Capability::AuthOauth,
            Capability::ModelDiscovery,
            Capability::Allowance,
            Capability::ConfigValidation,
        ]),
        "the manually installed dedicated packages must keep every previously public capability observable"
    );

    Ok(())
}

#[tokio::test]
async fn custom_profile_standard_protocols_preserve_tools_thinking_usage_and_terminal_state()
-> anyhow::Result<()> {
    let cases = [
        StandardCase {
            name: "openai-chat",
            vendor: "custom",
            protocol: "openai-compatible",
            response: json!({
                "id": "chatcmpl-contract", "model": "upstream-model",
                "choices": [{"index": 0, "message": {
                    "role": "assistant", "content": "", "reasoning_content": "because",
                    "tool_calls": [{"id": "call-1", "type": "function", "function": {"name": "lookup", "arguments": "{\"key\":\"answer\"}"}}]
                }, "finish_reason": "tool_calls"}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7, "completion_tokens_details": {"reasoning_tokens": 2}}
            }),
            request_assertion: assert_openai_standard_request,
        },
        StandardCase {
            name: "responses",
            vendor: "custom",
            protocol: "open-responses",
            response:
                stravia_protocol_codec::codec::open_responses::formatter::response_resource_snapshot(
                    "resp-contract",
                    "upstream-model",
                    "completed",
                    vec![
                        json!({"id": "reason-1", "type": "reasoning", "status": "completed", "summary": [{"type": "summary_text", "text": "because"}], "content": null, "encrypted_content": null}),
                        json!({"id": "fc-1", "type": "function_call", "status": "completed", "call_id": "call-1", "name": "lookup", "arguments": "{\"key\":\"answer\"}"}),
                    ],
                    Value::Null,
                    Value::Null,
                    json!({"input_tokens": 3, "output_tokens": 4, "total_tokens": 7, "input_tokens_details": {"cached_tokens": 0}, "output_tokens_details": {"reasoning_tokens": 2}}),
                ),
            request_assertion: assert_responses_standard_request,
        },
        StandardCase {
            name: "anthropic",
            vendor: "custom",
            protocol: "anthropic-messages",
            response: json!({
                "id": "msg-contract", "model": "upstream-model", "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "because", "signature": "test-signature"},
                    {"type": "tool_use", "id": "call-1", "name": "lookup", "input": {"key": "answer"}}
                ],
                "stop_reason": "tool_use",
                "usage": {"input_tokens": 3, "output_tokens": 4}
            }),
            request_assertion: assert_anthropic_standard_request,
        },
        StandardCase {
            name: "gemini",
            vendor: "custom",
            protocol: "google-gemini",
            response: json!({
                "responseId": "gemini-contract", "modelVersion": "upstream-model",
                "candidates": [{"content": {"role": "model", "parts": [
                    {"text": "because", "thought": true},
                    {"functionCall": {"name": "lookup", "args": {"key": "answer"}}}
                ]}, "finishReason": "STOP"}],
                "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 4, "thoughtsTokenCount": 2, "totalTokenCount": 7}
            }),
            request_assertion: assert_gemini_standard_request,
        },
    ];

    for case in cases {
        let response_fixture = case.response.clone();
        let (base_url, server) =
            local_upstream(1, move |_| MockResponse::json(response_fixture.clone())).await?;
        let (_directory, gateway) = gateway().await?;
        let profile = gateway.admin().vendor_metadata(case.vendor)?;
        assert!(
            profile.channels[0]
                .protocols
                .iter()
                .any(|option| option.value == case.protocol),
            "{} must offer {} through the merged Custom profile",
            case.vendor,
            case.protocol
        );
        let (route, token) = provider_route_and_key(&gateway, case.name, ProviderSourceInput::Custom { vendor: case.vendor.to_string(), channel: "default".to_string(), protocol: Some(case.protocol.to_string()), base_url, models_source: None, static_models: None }, "upstream-model", ProviderCredentialInput::ApiKey {
            value: "test-standard-key".into(),
        }, Default::default(), json!({"id": "upstream-model", "name": "upstream-model", "reasoning": true, "tool_call": true}))
        .await?;

        let (status, response) = chat(gateway, &token, standard_client_request(&route)).await?;
        assert_eq!(status, StatusCode::OK, "{}: {response}", case.name);
        assert_eq!(
            response["choices"][0]["finish_reason"], "tool_calls",
            "{}",
            case.name
        );
        assert!(
            response["choices"][0]["message"]["reasoning_content"]
                .as_str()
                .is_some_and(|text| text.contains("because")),
            "{}: {response}",
            case.name
        );
        assert_eq!(
            response["choices"][0]["message"]["tool_calls"][0]["function"]["name"], "lookup",
            "{}",
            case.name
        );
        assert_eq!(response["usage"]["prompt_tokens"], 3, "{}", case.name);
        assert_eq!(response["usage"]["completion_tokens"], 4, "{}", case.name);

        let requests = server.await??;
        assert_eq!(requests.len(), 1);
        let upstream_body: Value = serde_json::from_slice(&requests[0].body)?;
        (case.request_assertion)(&upstream_body);
    }
    Ok(())
}

#[tokio::test]
async fn base_owned_private_protocols_select_and_round_trip_inside_real_wasm() -> anyhow::Result<()>
{
    struct Case {
        vendor: &'static str,
        protocol: &'static str,
        expected_path: &'static str,
        expected_text: &'static str,
        response: Value,
        options: serde_json::Map<String, Value>,
    }
    let cases = [
        Case {
            vendor: "amazon-bedrock",
            protocol: "bedrock-converse",
            expected_path: "/model/private-model/converse",
            expected_text: "bedrock private codec",
            response: json!({
                "output":{"message":{"role":"assistant","content":[{"text":"bedrock private codec"}]}},
                "stopReason":"end_turn",
                "usage":{"inputTokens":2,"outputTokens":3,"totalTokens":5}
            }),
            options: serde_json::Map::from_iter([("region".into(), json!("us-east-1"))]),
        },
        Case {
            vendor: "cohere",
            protocol: "cohere-chat",
            expected_path: "/chat",
            expected_text: "cohere private codec",
            response: json!({
                "id":"cohere-contract",
                "message":{"role":"assistant","content":[{"type":"text","text":"cohere private codec"}]},
                "finish_reason":"COMPLETE",
                "usage":{"tokens":{"input_tokens":2,"output_tokens":3}}
            }),
            options: Default::default(),
        },
        Case {
            vendor: "gateway",
            protocol: "gateway-language-model",
            expected_path: "/language-model",
            expected_text: "gateway private codec",
            response: json!({
                "content":[{"type":"text","text":"gateway private codec"}],
                "finishReason":{"unified":"stop"},
                "usage":{"inputTokens":{"total":2},"outputTokens":{"total":3}},
                "response":{"id":"gateway-contract","modelId":"private-model"}
            }),
            options: Default::default(),
        },
    ];

    for case in cases {
        let fixture = case.response.clone();
        let (base_url, server) =
            local_upstream(1, move |_| MockResponse::json(fixture.clone())).await?;
        let (_directory, gateway) = gateway().await?;
        let profile = gateway.admin().vendor_metadata(case.vendor)?;
        assert!(profile.capabilities.contains(&Capability::Infer));
        assert_eq!(
            profile.channels[0].protocol.as_deref(),
            Some(case.protocol),
            "{} must advertise its opaque guest-selected protocol",
            case.vendor
        );
        let (route, token) = provider_route_and_key(
            &gateway,
            case.vendor,
            ProviderSourceInput::Custom {
                vendor: case.vendor.into(),
                channel: "default".into(),
                protocol: Some(case.protocol.into()),
                base_url,
                models_source: None,
                static_models: None,
            },
            "private-model",
            ProviderCredentialInput::ApiKey {
                value: "private-codec-test-key".into(),
            },
            case.options,
            json!({"id":"private-model","name":"private-model","tool_call":true}),
        )
        .await?;
        let (status, body) = chat(
            gateway,
            &token,
            json!({
                "model":route,
                "stream":false,
                "messages":[{"role":"user","content":"exercise the guest-owned wire"}]
            }),
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "{}: {body}", case.vendor);
        assert_eq!(
            body["choices"][0]["message"]["content"], case.expected_text,
            "{}",
            case.vendor
        );
        let requests = server.await??;
        assert_eq!(requests[0].path, case.expected_path, "{}", case.vendor);
        let wire: Value = serde_json::from_slice(&requests[0].body)?;
        assert_ne!(wire, Value::Null, "{} request body", case.vendor);
    }

    let (_directory, gateway) = gateway().await?;
    let watsonx = gateway.admin().vendor_metadata("watsonx")?;
    assert!(watsonx.capabilities.contains(&Capability::Infer));
    assert_eq!(
        watsonx.channels[0].protocol.as_deref(),
        Some("watsonx-text-chat")
    );
    Ok(())
}

#[tokio::test]
async fn explicit_model_sources_keep_vendor_auth_through_real_wasm() -> anyhow::Result<()> {
    let (_directory, gateway) = gateway().await?;
    for vendor in ["azure", "sap-ai-core", "gitlab", "custom"] {
        let expected_calls = if matches!(vendor, "sap-ai-core" | "gitlab") {
            2
        } else {
            1
        };
        let (origin, server) = local_upstream(expected_calls, |request| {
            if request.path == "/oauth/token" {
                MockResponse::json(json!({"access_token":"sap-directory-token","expires_in":3600}))
            } else if request.path == "/api/v4/ai/third_party_agents/direct_access" {
                MockResponse::json(json!({
                    "token":"gitlab-directory-token",
                    "headers":{"x-gitlab-directory":"enabled"}
                }))
            } else if request.path.starts_with("/directory/models?") {
                MockResponse::json(json!({"data":[{"id":"model-from-directory"}]}))
            } else {
                MockResponse::status_json(
                    "404 Not Found",
                    json!({"error":"unexpected directory path"}),
                )
            }
        })
        .await?;
        let base_url = "http://127.0.0.1:9/inference";
        let mut options = serde_json::Map::new();
        let credential = match vendor {
            "sap-ai-core" => {
                options.insert("deploymentUrl".into(), json!(base_url));
                options.insert("tokenUrl".into(), json!(format!("{origin}/oauth/token")));
                options.insert("resourceGroup".into(), json!("directory-group"));
                ProviderCredentialInput::Fields {
                    values: BTreeMap::from_iter([
                        ("clientId".into(), json!("directory-client")),
                        ("clientSecret".into(), json!("directory-secret")),
                    ]),
                }
            }
            "gitlab" => {
                options.insert("instanceUrl".into(), json!(origin));
                options.insert("aiGatewayUrl".into(), json!("http://127.0.0.1:9"));
                ProviderCredentialInput::ApiKey {
                    value: "directory-api-key".into(),
                }
            }
            "azure" => {
                options.insert("resourceName".into(), json!("unused-proxy-resource"));
                options.insert("apiVersion".into(), json!("2025-04-01-preview"));
                ProviderCredentialInput::ApiKey {
                    value: "directory-api-key".into(),
                }
            }
            _ => ProviderCredentialInput::ApiKey {
                value: "directory-api-key".into(),
            },
        };
        let provider = gateway
            .admin()
            .create_provider(CreateProvider {
                name: Some(format!("{vendor} explicit directory")),
                source: ProviderSourceInput::Custom {
                    vendor: vendor.into(),
                    channel: "default".into(),
                    protocol: Some(
                        if vendor == "custom" {
                            "anthropic-messages"
                        } else {
                            "openai-compatible"
                        }
                        .into(),
                    ),
                    base_url: base_url.into(),
                    models_source: Some(format!("{origin}/directory/models?tenant=local")),
                    static_models: None,
                },
                credential,
                vendor_options: options,
                use_proxy: false,
            })
            .await?;
        assert_eq!(
            gateway.admin().get_provider_models(&provider.id).await?,
            ["model-from-directory"],
        );
        let requests = server.await??;
        let models = requests.last().expect("directory request");
        let url = reqwest::Url::parse(&format!("http://local{}", models.path))?;
        assert_eq!(url.path(), "/directory/models");
        assert!(
            url.query_pairs()
                .any(|(name, value)| name == "tenant" && value == "local")
        );
        match vendor {
            "azure" => {
                assert_eq!(
                    models.headers.get("api-key").map(String::as_str),
                    Some("directory-api-key")
                );
                assert!(url.query_pairs().any(|(name, value)| name == "api-version" && value == "2025-04-01-preview"));
            }
            "sap-ai-core" => {
                assert_eq!(
                    models.headers.get("authorization").map(String::as_str),
                    Some("Bearer sap-directory-token")
                );
                assert_eq!(
                    models.headers.get("ai-resource-group").map(String::as_str),
                    Some("directory-group")
                );
            }
            "gitlab" => {
                assert_eq!(
                    models.headers.get("authorization").map(String::as_str),
                    Some("Bearer gitlab-directory-token")
                );
                assert_eq!(
                    models.headers.get("x-gitlab-directory").map(String::as_str),
                    Some("enabled")
                );
            }
            _ => {
                assert_eq!(
                    models.headers.get("x-api-key").map(String::as_str),
                    Some("directory-api-key")
                );
                assert_eq!(
                    models.headers.get("anthropic-version").map(String::as_str),
                    Some("2023-06-01")
                );
                assert!(!models.headers.contains_key("authorization"));
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn vertex_curated_inventory_preserves_channel_model_ids_without_network() -> anyhow::Result<()>
{
    let (directory, mut gateway) = gateway().await?;
    gateway.provider_catalog = gateway
        .provider_catalog
        .with_source_override(Arc::new(CloudCatalogSource));
    seed_cloud_catalog_scopes(directory.path())?;
    for (channel, protocol, expected) in [
        (
            "native",
            "google-gemini",
            vec![
                "gemini-1.5-flash-002",
                "gemini-1.5-pro-002",
                "gemini-2.0-flash-001",
                "gemini-2.5-flash",
                "gemini-2.5-pro",
            ],
        ),
        (
            "openai",
            "openai-compatible",
            vec![
                "google/gemini-2.0-flash-001",
                "google/gemini-2.5-flash",
                "google/gemini-2.5-pro",
            ],
        ),
    ] {
        let provider = gateway
            .admin()
            .create_provider(CreateProvider {
                name: Some(format!("Vertex {channel} curated inventory")),
                source: ProviderSourceInput::Custom {
                    vendor: "google-vertex".into(),
                    channel: channel.into(),
                    protocol: Some(protocol.into()),
                    base_url: "http://127.0.0.1:9".into(),
                    models_source: Some("catalog".into()),
                    static_models: None,
                },
                credential: ProviderCredentialInput::ApiKey {
                    value: "local-contract-token".into(),
                },
                vendor_options: serde_json::Map::new(),
                use_proxy: false,
            })
            .await?;
        let mut models = gateway.admin().get_provider_models(&provider.id).await?;
        models.sort();
        assert_eq!(models, expected);
    }
    Ok(())
}

#[tokio::test]
async fn bedrock_and_watsonx_explicit_and_default_catalog_models_discover_through_real_wasm()
-> anyhow::Result<()> {
    struct Case {
        vendor: &'static str,
        protocol: &'static str,
        model: &'static str,
        options: serde_json::Map<String, Value>,
    }

    let cases = [
        Case {
            vendor: "amazon-bedrock",
            protocol: "bedrock-converse",
            model: "anthropic.claude-explicit",
            options: serde_json::Map::from_iter([("region".into(), json!("us-east-1"))]),
        },
        Case {
            vendor: "watsonx",
            protocol: "watsonx-text-chat",
            model: "ibm/granite-explicit",
            options: serde_json::Map::from_iter([
                ("projectId".into(), json!("local-project")),
                ("baseUrl".into(), json!("http://127.0.0.1:9")),
            ]),
        },
    ];

    let (directory, mut gateway) = gateway().await?;
    gateway.provider_catalog = gateway
        .provider_catalog
        .with_source_override(Arc::new(CloudCatalogSource));
    seed_cloud_catalog_scopes(directory.path())?;
    for case in cases {
        let profile = gateway.admin().vendor_metadata(case.vendor)?;
        assert!(profile.capabilities.contains(&Capability::Infer));
        assert!(profile.capabilities.contains(&Capability::ModelDiscovery));
        assert_eq!(
            profile.channels[0].default_models_source,
            Some(DefaultModelsSource::Catalog)
        );

        let provider = gateway
            .admin()
            .create_provider(CreateProvider {
                name: Some(format!("{} explicit models", case.vendor)),
                source: ProviderSourceInput::Custom {
                    vendor: case.vendor.into(),
                    channel: "default".into(),
                    protocol: Some(case.protocol.into()),
                    base_url: "http://127.0.0.1:9".into(),
                    models_source: None,
                    static_models: Some(format!("{},{}", case.model, case.model)),
                },
                credential: ProviderCredentialInput::ApiKey {
                    value: "local-contract-key".into(),
                },
                vendor_options: case.options.clone(),
                use_proxy: false,
            })
            .await?;

        assert_eq!(
            gateway.admin().get_provider_models(&provider.id).await?,
            [case.model]
        );
        let summary = gateway.admin().sync_provider_models(&provider.id).await?;
        assert_eq!(summary.added, 1);
        let persisted = gateway.admin().list_provider_models(&provider.id).await?;
        assert_eq!(
            persisted
                .models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            [case.model]
        );
        let capabilities = gateway
            .admin()
            .get_model_capabilities(&provider.id, case.model)
            .await?;
        assert_eq!(capabilities.provider, case.vendor);
        assert_eq!(capabilities.model_id, case.model);

        let catalog_provider = gateway
            .admin()
            .create_provider(CreateProvider {
                name: Some(format!("{} catalog models", case.vendor)),
                source: ProviderSourceInput::Custom {
                    vendor: case.vendor.into(),
                    channel: "default".into(),
                    protocol: Some(case.protocol.into()),
                    base_url: "http://127.0.0.1:9".into(),
                    models_source: None,
                    static_models: None,
                },
                credential: ProviderCredentialInput::ApiKey {
                    value: "local-contract-key".into(),
                },
                vendor_options: case.options,
                use_proxy: false,
            })
            .await?;
        assert_eq!(catalog_provider.models_source.as_deref(), Some("catalog"));
        assert_eq!(
            gateway
                .admin()
                .get_provider_models(&catalog_provider.id)
                .await?,
            ["catalog-only-model"],
            "{} default discovery must use the catalog rather than the live upstream",
            case.vendor
        );
    }
    Ok(())
}

#[tokio::test]
async fn gitlab_inference_keeps_the_normalized_proxy_prefix_inside_real_wasm() -> anyhow::Result<()>
{
    let (origin, server) = local_upstream(2, |request| match request.path.as_str() {
        "/api/v4/ai/third_party_agents/direct_access" => MockResponse::json(json!({
            "token": "gitlab-direct-token",
            "headers": {"x-gitlab-test": "enabled"}
        })),
        "/ai/v1/proxy/openai/v1/chat/completions" => MockResponse::json(json!({
            "id": "gitlab-contract",
            "model": "duo-model",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "gitlab local answer"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 2, "completion_tokens": 3, "total_tokens": 5}
        })),
        _ => MockResponse {
            status: "404 Not Found",
            content_type: "application/json",
            body: br#"{"error":"unexpected local path"}"#.to_vec(),
        },
    })
    .await?;
    let (_directory, gateway) = gateway().await?;
    let (route, token) = provider_route_and_key(
        &gateway,
        "gitlab-proxy-prefix",
        ProviderSourceInput::Custom {
            vendor: "gitlab".into(),
            channel: "default".into(),
            protocol: Some("openai-compatible".into()),
            base_url: format!("{origin}/ai/v1/proxy/openai/v1"),
            models_source: None,
            static_models: None,
        },
        "duo-model",
        ProviderCredentialInput::ApiKey {
            value: "gitlab-personal-token".into(),
        },
        serde_json::Map::from_iter([
            ("instanceUrl".into(), json!(origin)),
            ("aiGatewayUrl".into(), json!(origin)),
        ]),
        json!({"id":"duo-model","name":"duo-model"}),
    )
    .await?;

    let (status, response) = chat(
        gateway,
        &token,
        json!({
            "model": route,
            "stream": false,
            "messages": [{"role": "user", "content": "exercise GitLab proxy routing"}]
        }),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(
        response["choices"][0]["message"]["content"],
        "gitlab local answer"
    );

    let requests = server.await??;
    assert_eq!(
        requests
            .iter()
            .map(|request| request.path.as_str())
            .collect::<Vec<_>>(),
        [
            "/api/v4/ai/third_party_agents/direct_access",
            "/ai/v1/proxy/openai/v1/chat/completions"
        ]
    );
    assert_eq!(
        requests[0].headers.get("authorization").map(String::as_str),
        Some("Bearer gitlab-personal-token")
    );
    assert_eq!(
        requests[1].headers.get("authorization").map(String::as_str),
        Some("Bearer gitlab-direct-token")
    );
    assert_eq!(
        requests[1].headers.get("x-gitlab-test").map(String::as_str),
        Some("enabled")
    );
    Ok(())
}

#[tokio::test]
async fn gitlab_401_refreshes_direct_access_token_and_retries_once() -> anyhow::Result<()> {
    let direct_access_calls = Arc::new(AtomicUsize::new(0));
    let observed_direct_access_calls = direct_access_calls.clone();
    let (origin, server) = local_upstream(4, move |request| match request.path.as_str() {
        "/api/v4/ai/third_party_agents/direct_access" => {
            let call = direct_access_calls.fetch_add(1, Ordering::SeqCst);
            let token = if call == 0 {
                "expired-gitlab-direct-token"
            } else {
                "refreshed-gitlab-direct-token"
            };
            MockResponse::json(json!({
                "token": token,
                "headers": {"x-gitlab-refresh": call.to_string()}
            }))
        }
        "/ai/v1/proxy/openai/v1/chat/completions"
            if request.headers.get("authorization").map(String::as_str)
                == Some("Bearer expired-gitlab-direct-token") =>
        {
            MockResponse::status_json(
                "401 Unauthorized",
                json!({"error":{"type":"authentication_error","message":"expired direct access token"}}),
            )
        }
        "/ai/v1/proxy/openai/v1/chat/completions" => MockResponse::json(json!({
            "id": "gitlab-refreshed-contract",
            "model": "duo-model",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "gitlab refreshed answer"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 2, "completion_tokens": 3, "total_tokens": 5}
        })),
        _ => MockResponse::status_json(
            "404 Not Found",
            json!({"error":"unexpected local path"}),
        ),
    })
    .await?;
    let (_directory, gateway) = gateway().await?;
    let (route, token) = provider_route_and_key(
        &gateway,
        "gitlab-auth-recovery",
        ProviderSourceInput::Custom {
            vendor: "gitlab".into(),
            channel: "default".into(),
            protocol: Some("openai-compatible".into()),
            base_url: format!("{origin}/ai/v1/proxy/openai/v1"),
            models_source: None,
            static_models: None,
        },
        "duo-model",
        ProviderCredentialInput::ApiKey {
            value: "gitlab-personal-token".into(),
        },
        serde_json::Map::from_iter([
            ("instanceUrl".into(), json!(origin)),
            ("aiGatewayUrl".into(), json!(origin)),
        ]),
        json!({"id":"duo-model","name":"duo-model"}),
    )
    .await?;

    let (status, response) = chat(
        gateway,
        &token,
        json!({
            "model": route,
            "stream": false,
            "messages": [{"role": "user", "content": "recover GitLab authentication"}]
        }),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(
        response["choices"][0]["message"]["content"],
        "gitlab refreshed answer"
    );

    let requests = server.await??;
    assert_eq!(
        requests
            .iter()
            .map(|request| request.path.as_str())
            .collect::<Vec<_>>(),
        [
            "/api/v4/ai/third_party_agents/direct_access",
            "/ai/v1/proxy/openai/v1/chat/completions",
            "/api/v4/ai/third_party_agents/direct_access",
            "/ai/v1/proxy/openai/v1/chat/completions",
        ]
    );
    assert_eq!(observed_direct_access_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        requests[1].headers.get("authorization").map(String::as_str),
        Some("Bearer expired-gitlab-direct-token")
    );
    assert_eq!(
        requests[3].headers.get("authorization").map(String::as_str),
        Some("Bearer refreshed-gitlab-direct-token")
    );
    Ok(())
}

#[tokio::test]
async fn gitlab_403_does_not_refresh_or_replay() -> anyhow::Result<()> {
    let (origin, server) = local_upstream(2, |request| match request.path.as_str() {
        "/api/v4/ai/third_party_agents/direct_access" => MockResponse::json(json!({
            "token": "permission-scoped-direct-token",
            "headers": {}
        })),
        "/ai/v1/proxy/openai/v1/chat/completions" => MockResponse::status_json(
            "403 Forbidden",
            json!({"error":{"type":"permission_denied","message":"model access denied"}}),
        ),
        _ => MockResponse::status_json("404 Not Found", json!({"error":"unexpected local path"})),
    })
    .await?;
    let (_directory, gateway) = gateway().await?;
    let (route, token) = provider_route_and_key(
        &gateway,
        "gitlab-permission-rejection",
        ProviderSourceInput::Custom {
            vendor: "gitlab".into(),
            channel: "default".into(),
            protocol: Some("openai-compatible".into()),
            base_url: format!("{origin}/ai/v1/proxy/openai/v1"),
            models_source: None,
            static_models: None,
        },
        "duo-model",
        ProviderCredentialInput::ApiKey {
            value: "gitlab-personal-token".into(),
        },
        serde_json::Map::from_iter([
            ("instanceUrl".into(), json!(origin)),
            ("aiGatewayUrl".into(), json!(origin)),
        ]),
        json!({"id":"duo-model","name":"duo-model"}),
    )
    .await?;

    let (status, response) = chat(
        gateway,
        &token,
        json!({
            "model": route,
            "stream": false,
            "messages": [{"role": "user", "content": "do not replay a forbidden request"}]
        }),
    )
    .await?;
    assert_eq!(status, StatusCode::FORBIDDEN, "{response}");

    let requests = server.await??;
    assert_eq!(
        requests
            .iter()
            .map(|request| request.path.as_str())
            .collect::<Vec<_>>(),
        [
            "/api/v4/ai/third_party_agents/direct_access",
            "/ai/v1/proxy/openai/v1/chat/completions",
        ]
    );
    Ok(())
}

#[tokio::test]
async fn azure_builtin_embeddings_use_the_native_endpoint_and_codec_over_wasm() -> anyhow::Result<()>
{
    let (base_url, server) = local_upstream(1, |_| {
        MockResponse::json(json!({
            "object": "list",
            "data": [
                {"object": "embedding", "index": 0, "embedding": [0.125, -0.5, 0.75]},
                {"object": "embedding", "index": 1, "embedding": [1.0, 0.0, -1.0]}
            ],
            "model": "embedding-deployment",
            "usage": {"prompt_tokens": 4, "total_tokens": 4}
        }))
    })
    .await?;
    let (_directory, gateway) = gateway().await?;
    let (route, token) = provider_route_and_key(
        &gateway,
        "azure-embeddings",
        ProviderSourceInput::Custom {
            vendor: "azure".to_string(),
            channel: "default".to_string(),
            protocol: Some("openai-compatible".to_string()),
            base_url: format!("{base_url}/openai/v1"),
            models_source: None,
            static_models: None,
        },
        "embedding-deployment",
        ProviderCredentialInput::ApiKey {
            value: "azure-contract-key".into(),
        },
        serde_json::Map::from_iter([
            ("resourceName".into(), json!("unused-explicit-proxy")),
            ("apiVersion".into(), json!("2024-02-01")),
        ]),
        json!({"id": "embedding-deployment", "name": "embedding-deployment"}),
    )
    .await?;

    let (status, response) = embeddings(
        gateway,
        &token,
        json!({
            "model": route,
            "input": ["first input", "second input"],
            "dimensions": 3,
            "encoding_format": "float",
            "user": "azure-contract"
        }),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["model"], "embedding-deployment");
    assert_eq!(response["data"][0]["embedding"], json!([0.125, -0.5, 0.75]));
    assert_eq!(response["data"][1]["embedding"], json!([1.0, 0.0, -1.0]));
    assert_eq!(
        response["usage"],
        json!({"prompt_tokens": 4, "total_tokens": 4})
    );

    let requests = server.await??;
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.path, "/openai/v1/embeddings?api-version=2024-02-01");
    assert_eq!(
        request.headers.get("api-key").map(String::as_str),
        Some("azure-contract-key")
    );
    let body: Value = serde_json::from_slice(&request.body)?;
    assert_eq!(body["model"], "embedding-deployment");
    assert_eq!(body["input"], json!(["first input", "second input"]));
    assert_eq!(body["dimensions"], 3);
    assert_eq!(body["encoding_format"], "float");
    assert_eq!(body["user"], "azure-contract");
    Ok(())
}

#[tokio::test]
async fn standard_plugin_rejects_an_unrepresentable_hard_requirement_before_network()
-> anyhow::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let base_url = format!("http://{}", listener.local_addr()?);
    let (_directory, gateway) = gateway().await?;
    let (route, token) = provider_route_and_key(&gateway, "gemini-lossy", ProviderSourceInput::Custom { vendor: "custom".to_string(), channel: "default".to_string(), protocol: Some("google-gemini".to_string()), base_url, models_source: None, static_models: None }, "upstream-model", ProviderCredentialInput::ApiKey { value: "test-key".into() }, Default::default(), json!({"id": "upstream-model", "name": "upstream-model", "reasoning": true, "tool_call": true}))
    .await?;
    let mut request = standard_client_request(&route);
    request["tools"][0]["function"]["strict"] = json!(true);

    let (status, body) = chat(gateway, &token, request).await?;
    assert_ne!(
        status,
        StatusCode::OK,
        "strict tool schema was silently weakened: {body}"
    );
    assert_eq!(body["error"]["code"], "vendor_request_invalid");
    assert_eq!(
        listener.into_std()?.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "rejected request reached the upstream"
    );
    Ok(())
}

async fn assert_native_compaction_rejected(
    gateway: &Gateway,
    token: &str,
    model: &str,
) -> anyhow::Result<()> {
    for control in [
        json!({
            "input": "hello",
            "context_management": [{"type": "compaction", "compact_threshold": 2000}]
        }),
        json!({"input": [
            {"role": "user", "content": "hello"},
            {"type": "compaction_trigger"}
        ]}),
    ] {
        let mut body = control;
        body["model"] = json!(model);
        body["stream"] = json!(false);
        let response = stravia_core::proxy::server::create_router(gateway.clone())
            .oneshot(
                Request::post("/v1/responses")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body)?))?,
            )
            .await?;
        let status = response.status();
        let body = to_bytes(response.into_body(), 16 * 1024 * 1024).await?;
        let body: Value = serde_json::from_slice(&body)?;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{model}: {body}");
        assert_eq!(
            body["error"]["code"], "compaction_unsupported",
            "{model}: {body}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn native_compaction_is_rejected_before_network_by_non_responses_plugins()
-> anyhow::Result<()> {
    let (_directory, gateway) = gateway().await?;
    install_distributed_vendor_plugin(&gateway, "command-code").await?;
    for (vendor, protocol) in [
        ("custom", "openai-compatible"),
        ("cohere", "cohere-chat"),
        ("gateway", "gateway-language-model"),
        ("command-code", "command-code"),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let (route, token) = provider_route_and_key(
            &gateway,
            &format!("native-rejection-{vendor}"),
            ProviderSourceInput::Custom {
                vendor: vendor.into(),
                channel: "default".into(),
                protocol: Some(protocol.into()),
                base_url: format!("http://{}", listener.local_addr()?),
                models_source: None,
                static_models: None,
            },
            "upstream-model",
            ProviderCredentialInput::ApiKey {
                value: "test-key".into(),
            },
            Default::default(),
            json!({"id": "upstream-model", "name": "upstream-model"}),
        )
        .await?;
        tokio::select! {
            result = assert_native_compaction_rejected(&gateway, &token, &route) => result?,
            request = listener.accept() => {
                drop(request?);
                anyhow::bail!("{vendor}: rejected request reached the upstream");
            }
        }
        assert_eq!(
            listener.into_std()?.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "{vendor}: rejected request reached the upstream"
        );
    }
    Ok(())
}

#[tokio::test]
async fn deepseek_builtin_applies_thinking_and_tool_history_on_the_real_wire() -> anyhow::Result<()>
{
    let captured = Arc::new(Mutex::new(None));
    let observed = Arc::clone(&captured);
    let (base_url, server) = local_upstream(1, move |request| {
        *observed.lock() = Some(request.clone());
        MockResponse::json(json!({
            "id": "deepseek-contract", "model": "deepseek-v4",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "answer", "reasoning_content": "new reasoning"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 11, "completion_tokens": 5, "total_tokens": 16, "completion_tokens_details": {"reasoning_tokens": 3}}
        }))
    })
    .await?;
    let (_directory, gateway) = gateway().await?;
    let (route, token) = provider_route_and_key(
        &gateway,
        "deepseek",
        ProviderSourceInput::Custom {
            vendor: "deepseek".to_string(),
            channel: "default".to_string(),
            protocol: Some("openai-compatible".to_string()),
            base_url,
            models_source: None,
            static_models: None,
        },
        "deepseek-v4",
        ProviderCredentialInput::ApiKey {
            value: "deepseek-test-key".into(),
        },
        Default::default(),
        json!({
            "id": "deepseek-v4",
            "name": "deepseek-v4",
            "reasoning": true,
            "reasoning_options": [{"type": "toggle"}],
            "tool_call": true
        }),
    )
    .await?;
    let request = json!({
        "model": route,
        "stream": false,
        "reasoning_effort": "medium",
        "messages": [
            {"role": "user", "content": "first"},
            {"role": "assistant", "content": "foreign answer"},
            {"role": "user", "content": "second"},
            {"role": "assistant", "content": "native answer", "reasoning_content": "native reasoning"},
            {"role": "user", "content": "continue"}
        ],
        "tools": [{"type": "function", "function": {"name": "lookup", "parameters": {"type": "object"}}}]
    });
    let (status, response) = chat(gateway, &token, request).await?;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert!(
        response["choices"][0]["message"]["reasoning_content"]
            .as_str()
            .is_some_and(|text| text.contains("new reasoning"))
    );
    assert_eq!(
        response["usage"]["completion_tokens_details"]["reasoning_tokens"],
        3
    );
    server.await??;

    let request = captured.lock().clone().expect("wire request");
    let wire: Value = serde_json::from_slice(&request.body)?;
    assert_eq!(wire["thinking"]["type"], "enabled");
    let assistants = wire["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter(|message| message["role"] == "assistant")
        .collect::<Vec<_>>();
    assert_eq!(assistants.len(), 2);
    assert_eq!(assistants[0]["reasoning_content"], "");
    assert_eq!(assistants[1]["reasoning_content"], "native reasoning");
    Ok(())
}

#[tokio::test]
async fn command_code_legacy_catalog_marker_still_discovers_from_the_connection_origin()
-> anyhow::Result<()> {
    let (base_url, server) = local_upstream(1, |_| {
        MockResponse::json(json!({"data":[{"id":"command-local-model"}]}))
    })
    .await?;
    let (_directory, gateway) = gateway().await?;
    install_distributed_vendor_plugin(&gateway, "command-code").await?;
    let provider = gateway
        .admin()
        .create_provider(CreateProvider {
            name: Some("Local Command Code discovery".into()),
            source: ProviderSourceInput::Custom {
                vendor: "command-code".into(),
                channel: "default".into(),
                protocol: Some("command-code".into()),
                base_url,
                models_source: Some("catalog".into()),
                static_models: None,
            },
            credential: ProviderCredentialInput::Fields {
                values: BTreeMap::from([("apiKey".into(), json!("local-command-key"))]),
            },
            vendor_options: Default::default(),
            use_proxy: false,
        })
        .await?;
    assert_eq!(provider.models_source.as_deref(), Some("catalog"));
    assert_eq!(
        gateway.admin().get_provider_models(&provider.id).await?,
        ["command-local-model"]
    );
    let requests = server.await??;
    assert_eq!(requests[0].path, "/provider/v1/models");
    assert_eq!(
        requests[0].headers.get("authorization").map(String::as_str),
        Some("Bearer local-command-key")
    );
    Ok(())
}

#[tokio::test]
async fn command_code_admin_option_reaches_initialization_and_inference_headers()
-> anyhow::Result<()> {
    let (base_url, server) = local_upstream(3, |request| {
        if request.path.contains("generate") {
            // The real upstream labels its NDJSON stream `text/event-stream`.
            MockResponse::bytes(
                "text/event-stream",
                b"{\"type\":\"reasoning-delta\",\"text\":\"because\"}\n{\"type\":\"text-delta\",\"text\":\"done\"}\n{\"type\":\"finish\",\"finishReason\":\"stop\",\"totalUsage\":{\"inputTokens\":7,\"outputTokens\":3}}\n".to_vec(),
            )
        } else {
            MockResponse::bytes(
                "application/json; charset=utf-8",
                b"{\n  \"ok\": true\n}\n".to_vec(),
            )
        }
    })
    .await?;
    let (_directory, gateway) = gateway().await?;
    install_distributed_vendor_plugin(&gateway, "command-code").await?;
    let (route, token) = provider_route_and_key(&gateway, "command-code", ProviderSourceInput::Custom { vendor: "command-code".to_string(), channel: "default".to_string(), protocol: Some("command-code".to_string()), base_url, models_source: None, static_models: None }, "command-r-plus", ProviderCredentialInput::Fields {
        values: BTreeMap::from([("apiKey".into(), json!("command-test-key"))]),
    }, serde_json::Map::from_iter([("zdr".into(), json!(true))]), json!({"id": "command-r-plus", "name": "command-r-plus", "reasoning": true, "tool_call": true}))
    .await?;
    let provider = gateway
        .admin()
        .list_providers()
        .await?
        .into_iter()
        .find(|provider| provider.name == "command-code provider")
        .expect("Command Code provider");
    assert_eq!(provider.vendor_options, r#"{"zdr":true}"#);

    gateway.admin().set_observation_debug(true);
    let mut observations = gateway.admin().observation_subscribe(0);
    let (status, response) = chat(
        gateway.clone(),
        &token,
        json!({"model": route, "stream": false, "messages": [{"role": "user", "content": "work"}]}),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["choices"][0]["message"]["content"], "done");
    assert!(
        response["choices"][0]["message"]["reasoning_content"]
            .as_str()
            .is_some_and(|text| text.contains("because"))
    );

    let requests = server.await??;
    assert_eq!(requests.len(), 3);
    assert!(
        requests
            .iter()
            .all(|request| request.headers.get("x-cmd-zdr").map(String::as_str) == Some("1"))
    );
    assert!(
        requests
            .iter()
            .any(|request| request.path == "/alpha/fingerprint/record")
    );
    assert!(
        requests
            .iter()
            .any(|request| request.path == "/alpha/lifecycle-events")
    );
    assert!(
        requests
            .iter()
            .any(|request| request.path.contains("generate"))
    );
    let (interaction_id, _) = finished_observation(&mut observations).await?;
    let records = observation_bundle_records(&gateway, &interaction_id).await?;
    let generated_records = records
        .iter()
        .filter(|record| {
            record["direction"] == "upstream_response"
                && record["message_type"] == "body_chunk"
                && record["url"]
                    .as_str()
                    .is_some_and(|url| url.contains("generate"))
        })
        .collect::<Vec<_>>();
    assert!(
        !generated_records.is_empty(),
        "builtin Wasm response Wire is missing"
    );
    assert!(generated_records.iter().all(|record| {
        record["layer"] == "wire"
            && record["stage"].is_null()
            && record["model_turn_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
            && record["attempt_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
    }));
    let generated = generated_records
        .iter()
        .try_fold(Vec::new(), |mut body, record| {
            body.extend(wire_payload_bytes(record)?);
            Ok::<_, anyhow::Error>(body)
        })?;
    assert_eq!(
        generated.as_slice(),
        b"{\"type\":\"reasoning-delta\",\"text\":\"because\"}\n{\"type\":\"text-delta\",\"text\":\"done\"}\n{\"type\":\"finish\",\"finishReason\":\"stop\",\"totalUsage\":{\"inputTokens\":7,\"outputTokens\":3}}\n",
        "the actual builtin Wasm transport must retain raw NDJSON response bytes"
    );
    for path in ["/alpha/fingerprint/record", "/alpha/lifecycle-events"] {
        let response = records
            .iter()
            .filter(|record| {
                record["direction"] == "upstream_response"
                    && record["message_type"] == "body_chunk"
                    && record["url"]
                        .as_str()
                        .is_some_and(|url| url.ends_with(path))
            })
            .try_fold(Vec::new(), |mut body, record| {
                body.extend(wire_payload_bytes(record)?);
                Ok::<_, anyhow::Error>(body)
            })?;
        let payload: Value = serde_json::from_slice(&response)?;
        assert_eq!(payload, json!({"ok": true}), "{path}");
    }
    let (status, response) = chat(
        gateway.clone(),
        &token,
        json!({"model": route, "messages": [{"role": "user", "content": "work"}],
            "response_format": {"type":"json_object"}}),
    )
    .await?;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
    assert_eq!(response["error"]["code"], "vendor_request_invalid");
    Ok(())
}

fn proto_varint(mut value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            return out;
        }
    }
}

fn proto_key(field: u32, wire: u8) -> Vec<u8> {
    proto_varint(((field as u64) << 3) | u64::from(wire))
}

fn proto_len(field: u32, bytes: &[u8]) -> Vec<u8> {
    let mut out = proto_key(field, 2);
    out.extend(proto_varint(bytes.len() as u64));
    out.extend_from_slice(bytes);
    out
}

fn proto_string(field: u32, value: &str) -> Vec<u8> {
    proto_len(field, value.as_bytes())
}

fn proto_u64(field: u32, value: u64) -> Vec<u8> {
    let mut out = proto_key(field, 0);
    out.extend(proto_varint(value));
    out
}

fn devin_model_entry(selector: &str, label: &str, alias: &str, router: bool) -> Vec<u8> {
    let mut info = Vec::new();
    info.extend(proto_string(23, alias));
    if selector.ends_with("medium") {
        info.extend(proto_string(20, "opus48"));
    }
    if router {
        info.extend(proto_u64(25, 1));
    }
    let mut entry = Vec::new();
    entry.extend(proto_string(1, label));
    entry.extend(proto_u64(5, 1));
    entry.extend(proto_u64(10, 3));
    entry.extend(proto_u64(18, 200_000));
    entry.extend(proto_string(22, selector));
    entry.extend(proto_len(23, &info));
    entry
}

fn devin_catalog_fixture() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend(proto_len(
        1,
        &devin_model_entry(
            "claude-opus-4.8-medium",
            "Claude Opus 4.8 Medium",
            "claude-opus-4.8",
            true,
        ),
    ));
    out.extend(proto_len(
        1,
        &devin_model_entry(
            "claude-opus-4.8-high",
            "Claude Opus 4.8 High",
            "claude-opus-4.8",
            false,
        ),
    ));
    out
}

fn devin_assignment_fixture() -> Vec<u8> {
    let mut assignment = Vec::new();
    assignment.extend(proto_string(1, "assignment-jwt"));
    assignment.extend(proto_string(2, "resolved-model-uid"));
    proto_len(1, &assignment)
}

fn connect_frame(flags: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![flags];
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

fn devin_chat_fixture() -> Vec<u8> {
    let mut metadata = Vec::new();
    metadata.extend(proto_u64(2, 13));
    metadata.extend(proto_u64(3, 5));
    metadata.extend(proto_string(9, "resolved-model-uid"));
    let mut payload = Vec::new();
    payload.extend(proto_string(3, "devin answer"));
    payload.extend(proto_len(7, &metadata));
    payload.extend(proto_u64(5, 2));
    let mut out = connect_frame(0, &payload);
    out.extend(connect_frame(2, b"{}"));
    out
}

#[tokio::test]
async fn manually_installed_devin_discovers_families_assigns_a_router_and_streams_real_protobuf()
-> anyhow::Result<()> {
    let (base_url, server) = local_upstream(3, |request| {
        if request.path.ends_with("/GetCliModelConfigs") {
            MockResponse::bytes("application/proto", devin_catalog_fixture())
        } else if request.path.ends_with("/AssignModel") {
            MockResponse::bytes("application/proto", devin_assignment_fixture())
        } else if request.path.ends_with("/GetChatMessage") {
            MockResponse::bytes("application/connect+proto", devin_chat_fixture())
        } else {
            MockResponse {
                status: "404 Not Found",
                content_type: "application/json",
                body: br#"{"error":"unexpected Devin path"}"#.to_vec(),
            }
        }
    })
    .await?;
    let (_directory, gateway) = gateway().await?;
    install_distributed_vendor_plugin(&gateway, "devin").await?;
    let authorization = gateway
        .admin()
        .init_oauth_session(
            AuthSessionCandidate {
                vendor_id: "devin".into(),
                channel: "devin".into(),
                provider_id: None,
                base_url: base_url.clone(),
                protocol: Some("devin-connect".into()),
                options: Default::default(),
                credentials: Default::default(),
                use_proxy: false,
            },
            OAuthSessionStartOptions {
                callback_mode: OAuthCallbackMode::Manual,
                redirect_uri: "chisel-show-auth-token".into(),
                listener_port: None,
                fallback_reason: None,
            },
        )
        .await?;
    gateway
        .admin()
        .complete_oauth_session(
            &authorization.session_id,
            AuthCompletionInput {
                input: AuthCompletionValue::Manual {
                    value: "session_token=devin-test-token".into(),
                },
            },
        )
        .await?;
    let provider = gateway
        .admin()
        .create_provider_with_oauth_session(
            &authorization.session_id,
            CreateProvider {
                name: Some("Devin contract provider".into()),
                source: ProviderSourceInput::Custom {
                    vendor: "devin".into(),
                    channel: "devin".into(),
                    protocol: Some("devin-connect".into()),
                    base_url,
                    models_source: None,
                    static_models: None,
                },
                credential: ProviderCredentialInput::None,
                vendor_options: Default::default(),
                use_proxy: false,
            },
        )
        .await?;
    let summary = gateway.admin().sync_provider_models(&provider.id).await?;
    assert!(summary.added >= 1);
    let models = gateway.admin().list_provider_models(&provider.id).await?;
    assert!(
        models
            .models
            .iter()
            .any(|model| model.id == "claude-opus-4.8")
    );
    let family = gateway
        .admin()
        .get_provider_model(&provider.id, "claude-opus-4.8")
        .await?;
    assert_eq!(family.metadata.family.as_deref(), Some("claude-opus-4.8"));
    assert_eq!(
        family.metadata.extensions["selector"],
        "claude-opus-4.8-medium"
    );
    assert_eq!(
        family.metadata.extensions["devin"]["routers"],
        json!(["claude-opus-4.8-medium"])
    );
    assert_eq!(family.metadata.extensions["upstream_provider"], "anthropic");
    assert_eq!(family.metadata.extensions["context_window"], 200_000);

    let route = gateway
        .admin()
        .create_model(CreateRoute {
            model_id: "devin-contract-route".into(),
            display_name: None,
            balance: None,
            targets: vec![stravia_core::db::models::CreateTarget {
                provider_id: provider.id,
                model: Some("claude-opus-4.8".into()),
                enabled: true,
                priority: None,
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
        })
        .await?;
    let key = gateway
        .admin()
        .create_api_key(CreateApiKey {
            key: None,
            name: "Devin contract key".into(),
            concurrency_limit: None,
            expires_at: None,
            mcp_access_enabled: true,
            transparent_injection_enabled: false,
            inject_media_understanding: false,
            inject_web_search: false,
            inject_media_generation: false,
            model_ids: vec![route.id.into()],
        })
        .await?;
    let request = json!({
        "model": route.model_id,
        "stream": false,
        "messages": [{"role": "user", "content": "hello"}],
        "tools": [{"type": "function", "function": {"name": "lookup", "parameters": {"type": "object"}}}]
    });
    let mut constrained = request.clone();
    constrained["tool_choice"] = json!("required");
    let (status, response) = chat(gateway.clone(), &key.token, constrained).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
    assert_eq!(response["error"]["code"], "vendor_request_invalid");
    assert_native_compaction_rejected(&gateway, &key.token, &route.model_id).await?;
    let (status, response) = chat(gateway, &key.token, request).await?;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["choices"][0]["message"]["content"], "devin answer");
    assert_eq!(response["usage"]["prompt_tokens"], 13);
    assert_eq!(response["usage"]["completion_tokens"], 5);

    let requests = server.await??;
    let catalog = requests
        .iter()
        .find(|request| request.path.ends_with("/GetCliModelConfigs"))
        .expect("catalog request");
    let assign = requests
        .iter()
        .find(|request| request.path.ends_with("/AssignModel"))
        .expect("AssignModel request");
    let chat = requests
        .iter()
        .find(|request| request.path.ends_with("/GetChatMessage"))
        .expect("GetChatMessage request");
    assert_eq!(
        catalog.headers.get("content-type").map(String::as_str),
        Some("application/proto")
    );
    assert!(
        catalog
            .body
            .windows("devin-test-token".len())
            .any(|window| window == b"devin-test-token")
    );
    assert!(
        assign
            .body
            .windows("claude-opus-4.8-medium".len())
            .any(|window| window == b"claude-opus-4.8-medium")
    );
    assert!(
        chat.body
            .windows("resolved-model-uid".len())
            .any(|window| window == b"resolved-model-uid")
    );
    assert!(
        chat.body
            .windows("assignment-jwt".len())
            .any(|window| window == b"assignment-jwt")
    );
    Ok(())
}

#[tokio::test]
async fn devin_oauth_start_is_owned_by_the_manually_installed_component_without_contacting_production()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let gateway = Gateway::from_storage(
        GatewayConfig {
            data_dir: directory.path().to_path_buf(),
            ..Default::default()
        },
        Arc::new(MemoryStorage::new(Vec::new(), Vec::new(), Vec::new())),
    )
    .await?;
    install_distributed_vendor_plugin(&gateway, "devin").await?;
    let started = gateway
        .admin()
        .init_oauth_session(
            AuthSessionCandidate {
                vendor_id: "devin".into(),
                channel: "devin".into(),
                provider_id: None,
                base_url: "http://127.0.0.1:9".into(),
                protocol: Some("devin-connect".into()),
                options: Default::default(),
                credentials: Default::default(),
                use_proxy: false,
            },
            OAuthSessionStartOptions {
                callback_mode: OAuthCallbackMode::Manual,
                redirect_uri: "http://127.0.0.1/callback".into(),
                listener_port: None,
                fallback_reason: Some("contract test uses manual callback capture".into()),
            },
        )
        .await?;
    assert_eq!(started.vendor_id, "devin");
    assert_eq!(started.channel, "devin");
    assert_eq!(started.redirect_uri, "http://127.0.0.1/callback");
    let auth_url = started.auth_url.expect("Devin authorization URL");
    assert!(auth_url.starts_with("https://app.devin.ai/auth/cli/continue?"));
    assert!(auth_url.contains("state="));
    assert!(auth_url.contains("code_challenge="));
    gateway
        .admin()
        .cancel_oauth_session(&started.session_id)
        .await?;
    Ok(())
}
