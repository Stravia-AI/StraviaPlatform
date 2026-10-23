use std::collections::BTreeMap;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use base64::Engine as _;
use rcgen::{CertifiedKey, generate_simple_self_signed};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Notify, mpsc};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};

use super::Gateway;
use crate::admin::provider_allowance::{
    AllowanceCondition, AllowanceKind, ExhaustionForecastStatus, ProviderAllowanceStatus,
};
use crate::auth::types::AuthBindingStatus;
use crate::auth::{
    AuthCompletionInput, AuthCompletionValue, AuthSessionCandidate, AuthSessionStatusData,
    OAuthCallbackMode, OAuthSessionStartOptions,
};
use crate::config::GatewayConfig;
use crate::db::models::{CreateProvider, ProviderCredentialInput, ProviderSourceInput};

const DEEPSEEK_HOST: &str = "api.deepseek.com";
const DEVIN_HOST: &str = "server.codeium.com";
const DEVIN_BASE_URL: &str = "https://server.codeium.com";
const DEVIN_TOKEN_PATH: &str =
    "/exa.seat_management_pb.SeatManagementService/ExchangeDevinCLIPKCECode";
const DEVIN_ALLOWANCE_PATH: &str = "/exa.seat_management_pb.SeatManagementService/GetUserStatus";
const DEVIN_MODELS_PATH: &str = "/exa.api_server_pb.ApiServerService/GetCliModelConfigs";
const DEVIN_MANUAL_CODE: &str = "devin-manual-code";
const DEVIN_CONTRACT_TOKEN: &str = "devin-contract-token";

#[derive(Clone, Debug)]
struct ObservedRequest {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

#[derive(Clone)]
struct LocalResponse {
    status: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
    release: Option<Arc<Notify>>,
}

impl LocalResponse {
    fn json(body: Value) -> Self {
        Self {
            status: "200 OK",
            content_type: "application/json",
            body: serde_json::to_vec(&body).expect("fixture JSON"),
            release: None,
        }
    }

    fn protobuf(body: Vec<u8>) -> Self {
        Self {
            status: "200 OK",
            content_type: "application/proto",
            body,
            release: None,
        }
    }
}

async fn write_local_response(
    socket: &mut (impl AsyncWrite + Unpin),
    response: LocalResponse,
) -> io::Result<()> {
    if let Some(release) = response.release {
        release.notified().await;
    }
    let head = format!(
        "HTTP/1.1 {}\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        response.status,
        response.content_type,
        response.body.len()
    );
    socket.write_all(head.as_bytes()).await?;
    socket.write_all(&response.body).await
}

// 使用真实时间限制测试等待，并持续yield，避免暂停的Tokio时钟自动推进。
async fn with_real_time_limit<F: Future>(future: F) -> anyhow::Result<F::Output> {
    let until = std::time::Instant::now() + Duration::from_secs(10);
    tokio::pin!(future);
    loop {
        anyhow::ensure!(
            std::time::Instant::now() < until,
            "loopback operation exceeded ten seconds of real time"
        );
        tokio::select! {
            biased;
            result = &mut future => return Ok(result),
            _ = tokio::task::yield_now() => {}
        }
    }
}

struct LoopbackOnlyResolver {
    address: SocketAddr,
}

impl reqwest::dns::Resolve for LoopbackOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        if matches!(
            name.as_str(),
            DEEPSEEK_HOST
                | DEVIN_HOST
                | "oauth2.googleapis.com"
                | "daily-cloudcode-pa.googleapis.com"
                | "auth.openai.com"
                | "chatgpt.com"
        ) {
            let addresses: reqwest::dns::Addrs = Box::new(std::iter::once(self.address));
            Box::pin(std::future::ready(Ok(addresses)))
        } else {
            let error = io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("test DNS blocked undeclared host {}", name.as_str()),
            );
            Box::pin(std::future::ready(Err(error.into())))
        }
    }
}

struct LocalTlsServer {
    address: SocketAddr,
    certificate_der: Vec<u8>,
    requests: mpsc::UnboundedReceiver<ObservedRequest>,
    task: tokio::task::JoinHandle<anyhow::Result<Vec<ObservedRequest>>>,
}

async fn local_tls_server(
    request_count: usize,
    response: impl Fn(&ObservedRequest) -> LocalResponse + Send + Sync + 'static,
) -> anyhow::Result<LocalTlsServer> {
    let CertifiedKey { cert, signing_key } = generate_simple_self_signed(
        [
            DEEPSEEK_HOST,
            DEVIN_HOST,
            "oauth2.googleapis.com",
            "daily-cloudcode-pa.googleapis.com",
            "auth.openai.com",
            "chatgpt.com",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>(),
    )?;
    let certificate_der = cert.der().to_vec();
    let private_key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
    let mut tls_config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(vec![cert.der().clone()], private_key)?;
    tls_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(tls_config));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let response = Arc::new(response);
    let (request_tx, requests) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        let mut observed = Vec::with_capacity(request_count);
        for _ in 0..request_count {
            let (socket, _) = listener.accept().await?;
            let mut socket = acceptor.accept(socket).await?;
            let request = read_http_request(&mut socket).await?;
            request_tx
                .send(request.clone())
                .map_err(|_| anyhow::anyhow!("request observer dropped"))?;
            let reply = response(&request);
            observed.push(request);
            // 取消或超时可能在fixture释放响应前关闭客户端连接。
            if let Err(error) = write_local_response(&mut socket, reply).await {
                tracing::debug!(
                    ?error,
                    "local TLS fixture client closed before reply completed"
                );
            }
        }
        Ok(observed)
    });
    Ok(LocalTlsServer {
        address,
        certificate_der,
        requests,
        task,
    })
}

async fn read_http_request(
    socket: &mut (impl AsyncRead + AsyncWrite + Unpin),
) -> anyhow::Result<ObservedRequest> {
    let mut bytes = Vec::new();
    let mut scratch = [0_u8; 4096];
    let (header_end, content_length) = loop {
        let read = socket.read(&mut scratch).await?;
        anyhow::ensure!(
            read != 0,
            "local TLS upstream closed before request headers"
        );
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
            bytes.len() <= 1024 * 1024,
            "local TLS request headers too large"
        );
    };
    while bytes.len() < header_end + content_length {
        let read = socket.read(&mut scratch).await?;
        anyhow::ensure!(read != 0, "local TLS upstream closed before request body");
        bytes.extend_from_slice(&scratch[..read]);
    }

    let header_text = std::str::from_utf8(&bytes[..header_end - 4])?;
    let mut lines = header_text.lines();
    let mut request_line = lines.next().unwrap_or_default().split_ascii_whitespace();
    let method = request_line.next().unwrap_or_default().to_string();
    let path = request_line.next().unwrap_or_default().to_string();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    Ok(ObservedRequest {
        method,
        path,
        headers,
        body: bytes[header_end..header_end + content_length].to_vec(),
    })
}

async fn gateway_for(
    server: &LocalTlsServer,
    vendors: &[&str],
) -> anyhow::Result<(tempfile::TempDir, Gateway)> {
    gateway_for_storage(
        server,
        vendors,
        Arc::new(crate::storage::MemoryStorage::new(
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )),
    )
    .await
}

async fn gateway_for_storage(
    server: &LocalTlsServer,
    vendors: &[&str],
    storage: crate::storage::traits::DynStorage,
) -> anyhow::Result<(tempfile::TempDir, Gateway)> {
    let directory = tempfile::tempdir()?;
    let mut gateway = Gateway::from_storage(
        GatewayConfig {
            data_dir: directory.path().to_path_buf(),
            ..Default::default()
        },
        storage,
    )
    .await?;
    for vendor in vendors {
        crate::plugin::test_support::install_distributed_vendor(&gateway, vendor).await?;
    }
    let root = reqwest::tls::Certificate::from_der(&server.certificate_der)?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .https_only(true)
        .http1_only()
        .tls_certs_only([root])
        .dns_resolver(LoopbackOnlyResolver {
            address: server.address,
        })
        .build()?;
    gateway.vendor_http_client = client.clone();
    gateway.vendor_websocket_client = client;
    Ok((directory, gateway))
}

async fn create_vendor_provider(
    gateway: &Gateway,
    name: &str,
    vendor: &str,
    channel: &str,
    protocol: &str,
    base_url: &str,
    credential: ProviderCredentialInput,
) -> anyhow::Result<crate::db::models::Provider> {
    gateway
        .admin()
        .create_provider(CreateProvider {
            name: Some(name.into()),
            source: ProviderSourceInput::Custom {
                vendor: vendor.into(),
                channel: channel.into(),
                protocol: Some(protocol.into()),
                base_url: base_url.into(),
                models_source: None,
                static_models: None,
            },
            credential,
            vendor_options: Default::default(),
            use_proxy: false,
        })
        .await
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

fn proto_u64(field: u32, value: u64) -> Vec<u8> {
    let mut out = proto_varint(u64::from(field) << 3);
    out.extend(proto_varint(value));
    out
}

fn proto_len(field: u32, value: &[u8]) -> Vec<u8> {
    let mut out = proto_varint((u64::from(field) << 3) | 2);
    out.extend(proto_varint(value.len() as u64));
    out.extend_from_slice(value);
    out
}

fn proto_string(field: u32, value: &str) -> Vec<u8> {
    proto_len(field, value.as_bytes())
}

fn devin_allowance_fixture() -> Vec<u8> {
    let mut plan = Vec::new();
    plan.extend(proto_u64(1, 17));
    plan.extend(proto_string(2, "Devin Max Contract"));
    plan.extend(proto_u64(12, 100));
    plan.extend(proto_u64(13, 10));

    let mut status = Vec::new();
    status.extend(proto_len(1, &plan));
    // Proto3 omits the exhausted daily window's zero remaining percentage.
    status.extend(proto_u64(15, 33));
    status.extend(proto_u64(16, 12_500_000));
    status.extend(proto_u64(17, 2_000_000_000));
    status.extend(proto_u64(18, 2_000_100_000));

    let mut user = Vec::new();
    user.extend(proto_len(13, &status));
    user.extend(proto_u64(28, 25));
    user.extend(proto_u64(29, 4));
    proto_len(1, &user)
}

fn query_parameter(url: &str, key: &str) -> anyhow::Result<String> {
    reqwest::Url::parse(url)?
        .query_pairs()
        .find_map(|(name, value)| (name == key).then(|| value.into_owned()))
        .ok_or_else(|| anyhow::anyhow!("missing `{key}` in authorization URL"))
}

async fn start_devin_session(
    gateway: &Gateway,
) -> anyhow::Result<crate::auth::AuthSessionInitData> {
    gateway
        .admin()
        .init_oauth_session(
            AuthSessionCandidate {
                vendor_id: "devin".into(),
                channel: "devin".into(),
                provider_id: None,
                base_url: DEVIN_BASE_URL.into(),
                protocol: Some("devin-connect".into()),
                options: Default::default(),
                credentials: Default::default(),
                use_proxy: false,
            },
            OAuthSessionStartOptions {
                callback_mode: OAuthCallbackMode::Auto,
                redirect_uri: "http://127.0.0.1:18765/oauth/callback".into(),
                listener_port: Some(18765),
                fallback_reason: None,
            },
        )
        .await
}

/// 手动登录会产生一次真实的授权码兑换；这里应答该兑换，让其余路径复用同一个会话令牌。
fn devin_token_response(request: &ObservedRequest) -> Option<LocalResponse> {
    (request.path == DEVIN_TOKEN_PATH).then(|| {
        LocalResponse::json(json!({
            "sessionToken": DEVIN_CONTRACT_TOKEN,
            "apiServerUrl": DEVIN_BASE_URL
        }))
    })
}

/// 走手动授权码流程登录；`server` 必须通过 `devin_token_response` 应答兑换请求。
async fn sign_in_devin_manually(
    gateway: &Gateway,
    server: &mut LocalTlsServer,
) -> anyhow::Result<crate::auth::AuthSessionInitData> {
    let authorization = gateway
        .admin()
        .init_oauth_session(
            AuthSessionCandidate {
                vendor_id: "devin".into(),
                channel: "devin".into(),
                provider_id: None,
                base_url: DEVIN_BASE_URL.into(),
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
    let auth_url = authorization
        .auth_url
        .as_deref()
        .context("manual Devin authorization URL")?;
    gateway
        .admin()
        .complete_oauth_session(
            &authorization.session_id,
            AuthCompletionInput {
                input: AuthCompletionValue::Manual {
                    value: DEVIN_MANUAL_CODE.into(),
                },
            },
        )
        .await?;
    let exchange = tokio::time::timeout(Duration::from_secs(10), server.requests.recv())
        .await?
        .context("manual Devin code exchange")?;
    assert_eq!(exchange.path, DEVIN_TOKEN_PATH);
    let body: Value = serde_json::from_slice(&exchange.body)?;
    assert_eq!(body["code"], DEVIN_MANUAL_CODE);
    let verifier = body["codeVerifier"].as_str().context("PKCE verifier")?;
    assert_eq!(
        query_parameter(auth_url, "code_challenge")?,
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(verifier.as_bytes()))
    );
    Ok(authorization)
}

fn devin_oauth_provider(name: &str) -> CreateProvider {
    CreateProvider {
        name: Some(name.into()),
        source: ProviderSourceInput::Custom {
            vendor: "devin".into(),
            channel: "devin".into(),
            protocol: Some("devin-connect".into()),
            base_url: DEVIN_BASE_URL.into(),
            models_source: None,
            static_models: None,
        },
        credential: ProviderCredentialInput::None,
        vendor_options: Default::default(),
        use_proxy: false,
    }
}

async fn synthetic_oauth_provider(
    gateway: &Gateway,
    vendor: &str,
    expired: bool,
) -> anyhow::Result<crate::db::models::Provider> {
    let (channel, protocol, base_url) = match vendor {
        "antigravity" => (
            "oauth",
            "google-gemini",
            "https://daily-cloudcode-pa.googleapis.com",
        ),
        "openai-codex" => (
            "codex",
            "open-responses",
            "https://chatgpt.com/backend-api/codex",
        ),
        _ => anyhow::bail!("undeclared synthetic OAuth vendor"),
    };
    let session = gateway
        .admin()
        .init_oauth_session(
            AuthSessionCandidate {
                vendor_id: vendor.into(),
                channel: channel.into(),
                provider_id: None,
                base_url: base_url.into(),
                protocol: Some(protocol.into()),
                options: Default::default(),
                credentials: Default::default(),
                use_proxy: false,
            },
            OAuthSessionStartOptions {
                callback_mode: OAuthCallbackMode::Manual,
                redirect_uri: "http://localhost:1457/auth/callback".into(),
                listener_port: None,
                fallback_reason: None,
            },
        )
        .await?;
    let bundle = crate::auth::CredentialBundle {
        access_token: Some("synthetic-old-access".into()),
        refresh_token: Some("synthetic-refresh".into()),
        expires_at: Some("2099-01-01T00:00:00Z".into()),
        resource_url: Some(base_url.into()),
        raw: json!({"project_id": "synthetic-project"}),
        ..Default::default()
    };
    let runtime = {
        let mut sessions = gateway.auth_sessions.write().await;
        let record = sessions
            .get_mut(&session.session_id)
            .context("synthetic session")?;
        record.status = crate::auth::AuthSessionStatus::Ready.as_str().into();
        record.result_json = Some(serde_json::to_string(&bundle)?);
        record.expires_at = bundle.expires_at.clone();
        record
            .vendor_runtime
            .clone()
            .context("synthetic session runtime")?
    };
    let (_, operation, _) = gateway.vendor_plugins.acquire(vendor)?;
    let publication = operation.publication_fence(
        runtime.cancellation.clone(),
        stravia_runtime_contract::Deadline::fixed(
            std::time::Instant::now() + Duration::from_secs(600),
        ),
    );
    drop(operation);
    *runtime.publication.lock().await = Some(publication);
    let provider = gateway
        .admin()
        .create_provider_with_oauth_session(
            &session.session_id,
            CreateProvider {
                name: Some(format!("synthetic {vendor}")),
                source: ProviderSourceInput::Custom {
                    vendor: vendor.into(),
                    channel: channel.into(),
                    protocol: Some(protocol.into()),
                    base_url: base_url.into(),
                    models_source: None,
                    static_models: None,
                },
                credential: ProviderCredentialInput::None,
                vendor_options: Default::default(),
                use_proxy: false,
            },
        )
        .await?;
    if expired {
        let credential = gateway
            .storage
            .oauth_credentials()
            .get(&provider.id)
            .await?
            .context("synthetic OAuth credential")?;
        gateway
            .storage
            .oauth_credentials()
            .upsert(
                &provider.id,
                crate::db::models::UpsertOAuthCredential {
                    driver_key: vendor.into(),
                    scheme: credential.scheme,
                    access_token: credential.access_token,
                    refresh_token: credential.refresh_token,
                    expires_at: Some("2000-01-01T00:00:00Z".into()),
                    resource_url: credential.resource_url,
                    meta: Some(credential.meta),
                    ..Default::default()
                },
            )
            .await?;
    }
    Ok(provider)
}

async fn antigravity_independent_image_recovery_case(
    retry_budget: i32,
    token_status: &'static str,
    retry_image_failure: bool,
) -> anyhow::Result<()> {
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
    let succeeds = retry_budget > 0 && token_status == "200 OK";
    let request_count = if retry_budget == 0 {
        1
    } else if succeeds {
        3 + usize::from(retry_image_failure)
    } else {
        2
    };
    let refreshed_image_calls = std::sync::atomic::AtomicUsize::new(0);
    let server = local_tls_server(request_count, move |request| {
        if request.path == "/token" {
            return LocalResponse {
                status: token_status,
                ..LocalResponse::json(if token_status == "200 OK" {
                    json!({"access_token": "synthetic-new-access", "expires_in": 3600})
                } else {
                    json!({"error": "temporarily_unavailable"})
                })
            };
        }
        if request.headers.get("authorization").map(String::as_str)
            != Some("Bearer synthetic-new-access")
        {
            return LocalResponse {
                status: "401 Unauthorized",
                ..LocalResponse::json(json!({"error": {
                    "code": 401, "status": "UNAUTHENTICATED",
                    "message": "synthetic expired access token"
                }}))
            };
        }
        if retry_image_failure
            && refreshed_image_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0
        {
            return LocalResponse {
                status: "503 Service Unavailable",
                ..LocalResponse::json(json!({"error": {
                    "code": 503, "status": "UNAVAILABLE",
                    "message": "synthetic transient image failure"
                }}))
            };
        }
        let event = json!({"response": {"candidates": [{
            "content": {"parts": [{"inlineData": {"mimeType": "image/png", "data": PNG}}]},
            "finishReason": "STOP"
        }]}});
        LocalResponse {
            content_type: "text/event-stream",
            body: format!("data: {event}\n\n").into_bytes(),
            ..LocalResponse::json(Value::Null)
        }
    })
    .await?;
    // The real capability send boundary authorizes an API-key principal.
    // Unlike the auxiliary-only fixtures, this consumer needs a SQL-backed
    // API-key store; keep its database isolated from every other fixture.
    let storage_directory = tempfile::tempdir()?;
    let pool = crate::db::init_pool(storage_directory.path()).await?;
    crate::migrations::migrate_sqlite(&pool, None).await?;
    let storage = Arc::new(crate::storage::SqliteStorage::from_pool(pool));
    let (_directory, gateway) = gateway_for_storage(&server, &["antigravity"], storage).await?;
    // Leave time running while SQLx performs real filesystem I/O; the
    // consumer uses a fresh credential and finishes before background refresh.
    tokio::task::yield_now().await;
    let provider =
        with_real_time_limit(synthetic_oauth_provider(&gateway, "antigravity", false)).await??;
    let original = gateway
        .storage
        .oauth_credentials()
        .get(&provider.id)
        .await?
        .context("original image credential")?;
    let model = "gemini-3.1-flash-image";
    gateway
        .admin()
        .create_manual_provider_model(
            &provider.id,
            model,
            crate::provider_models::CreateManualProviderModel {
                metadata: json!({
                    "id": model, "name": model, "capabilities": ["media_image"],
                    "modalities": {"input": ["text"], "output": ["image"]}
                }),
                template_id: None,
            },
        )
        .await?;
    let route = gateway
        .admin()
        .create_model(crate::db::models::CreateRoute {
            model_id: "synthetic-oauth-image".into(),
            display_name: None,
            output_modality: crate::db::models::RouteOutputModality::Image,
            balance: Some("latency_preference".into()),
            targets: vec![crate::db::models::CreateTarget {
                provider_id: provider.id.clone(),
                model: model.into(),
                enabled: true,
                priority: Some(0),
                first_token_timeout_ms: None,
                target_retry_budget: Some(retry_budget),
                target_cooldown_ms: Some(0),
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
        })
        .await?;
    let key = gateway
        .admin()
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "synthetic image owner".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: true,
            transparent_injection_enabled: false,
            inject_web_search: false,
            inject_media_generation: false,
            inject_media_understanding: false,
            model_ids: Vec::new(),
        })
        .await?;
    // Exercise the real independent-capability consumer and installed Wasm
    // component, including HTTP Auth refresh, rather than a refresh-function mock.
    let result = with_real_time_limit(gateway.execute_vendor_route(
        &stravia_runtime_contract::Principal::new(key.id),
        &route,
        crate::plugin::VendorRequest::MediaImage(stravia_vendor_sdk::MediaImageRequest {
            prompt: "synthetic loopback image".into(),
            references: Vec::new(),
            aspect_ratio: None,
            resolution: None,
        }),
        crate::plugin::VendorCallContext::new(
            stravia_runtime_contract::CancellationToken::new(),
            stravia_runtime_contract::Deadline::fixed(
                std::time::Instant::now() + Duration::from_secs(30),
            ),
        ),
    ))
    .await?;
    if succeeds {
        let execution = result?;
        let stravia_vendor_sdk::OperationOutput::MediaImage(output) = execution.output else {
            anyhow::bail!("image consumer returned a different capability");
        };
        assert_eq!(output.artifacts.len(), 1);
        assert_eq!(
            output.artifacts[0].bytes,
            base64::engine::general_purpose::STANDARD.decode(PNG)?
        );
    } else {
        let error = result.err().context("image request must fail")?;
        if retry_budget == 0 {
            assert!(crate::plugin::execution::is_credential_rejection(&error));
        } else {
            assert!(!crate::plugin::execution::is_credential_rejection(&error));
            assert_eq!(
                error
                    .downcast_ref::<stravia_vendor_runtime::RuntimeError>()
                    .and_then(stravia_vendor_runtime::RuntimeError::upstream_status),
                Some(503),
            );
        }
    }
    assert_eq!(
        gateway
            .admin()
            .get_provider(&provider.id)
            .await?
            .credential_invalid(),
        retry_budget == 0,
    );
    let current = gateway
        .storage
        .oauth_credentials()
        .get(&provider.id)
        .await?
        .context("image credential retained")?;
    assert_eq!(current.connection_id, original.connection_id);
    assert_eq!(
        current.access_token,
        if succeeds {
            "synthetic-new-access"
        } else {
            "synthetic-old-access"
        },
    );
    assert_eq!(current.refresh_token.as_deref(), Some("synthetic-refresh"));
    let observed = with_real_time_limit(server.task).await???;
    assert_eq!(observed.len(), request_count);
    let refreshes: Vec<_> = observed
        .iter()
        .filter(|request| request.path == "/token")
        .collect();
    assert_eq!(refreshes.len(), usize::from(retry_budget > 0));
    for request in refreshes {
        assert_eq!(request.method, "POST");
        let form: BTreeMap<_, _> = url::form_urlencoded::parse(&request.body)
            .into_owned()
            .collect();
        assert_eq!(
            form.get("grant_type").map(String::as_str),
            Some("refresh_token")
        );
        assert_eq!(
            form.get("refresh_token").map(String::as_str),
            Some("synthetic-refresh")
        );
    }
    let images: Vec<_> = observed
        .iter()
        .filter(|request| request.path != "/token")
        .collect();
    assert_eq!(
        images.len(),
        if succeeds {
            2 + usize::from(retry_image_failure)
        } else {
            1
        },
    );
    for (index, request) in images.iter().enumerate() {
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/v1internal:streamGenerateContent?alt=sse");
        assert_eq!(
            request.headers.get("authorization").map(String::as_str),
            Some(if index == 0 {
                "Bearer synthetic-old-access"
            } else {
                "Bearer synthetic-new-access"
            }),
        );
        let body: Value = serde_json::from_slice(&request.body)?;
        assert_eq!(body["model"], model);
        assert_eq!(body["project"], "synthetic-project");
    }
    Ok(())
}

#[tokio::test]
async fn antigravity_independent_image_401_refreshes_with_target_budget() -> anyhow::Result<()> {
    antigravity_independent_image_recovery_case(1, "200 OK", false).await
}

#[tokio::test]
async fn antigravity_independent_image_retry_retains_refreshed_credential() -> anyhow::Result<()> {
    antigravity_independent_image_recovery_case(2, "200 OK", true).await
}

#[tokio::test]
async fn antigravity_independent_image_zero_budget_does_not_refresh() -> anyhow::Result<()> {
    antigravity_independent_image_recovery_case(0, "200 OK", false).await
}

#[tokio::test]
async fn antigravity_independent_image_transient_refresh_keeps_provider_valid() -> anyhow::Result<()>
{
    antigravity_independent_image_recovery_case(1, "503 Service Unavailable", false).await
}

#[tokio::test]
async fn antigravity_background_refresh_recovers_after_transient_token_endpoint_failure()
-> anyhow::Result<()> {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let server_calls = calls.clone();
    let server = local_tls_server(2, move |_| {
        if server_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            LocalResponse {
                status: "503 Service Unavailable",
                ..LocalResponse::json(json!({"error": "temporarily_unavailable"}))
            }
        } else {
            LocalResponse::json(json!({"access_token": "synthetic-new-access", "expires_in": 3600}))
        }
    })
    .await?;
    let (_directory, gateway) = gateway_for(&server, &["antigravity"]).await?;
    tokio::time::pause();
    // Drain the immediate background interval tick before publishing an expired
    // credential. The paused clock cannot reach the next 120-second tick while
    // the TLS exchanges are driven by with_real_time_limit.
    tokio::time::advance(Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    let provider =
        with_real_time_limit(synthetic_oauth_provider(&gateway, "antigravity", true)).await??;
    assert_eq!(
        with_real_time_limit(gateway.admin().refresh_oauth_providers()).await??,
        0
    );
    assert!(
        !gateway
            .admin()
            .get_provider(&provider.id)
            .await?
            .credential_invalid()
    );
    let retained = gateway
        .storage
        .oauth_credentials()
        .get(&provider.id)
        .await?
        .context("retained credential")?;
    assert_eq!(retained.refresh_token.as_deref(), Some("synthetic-refresh"));
    assert_eq!(
        with_real_time_limit(gateway.admin().refresh_oauth_providers()).await??,
        1
    );
    let credential = gateway
        .storage
        .oauth_credentials()
        .get(&provider.id)
        .await?
        .context("refreshed credential")?;
    assert_eq!(credential.access_token, "synthetic-new-access");
    assert_eq!(
        credential.refresh_token.as_deref(),
        Some("synthetic-refresh")
    );
    assert!(
        !gateway
            .admin()
            .get_provider(&provider.id)
            .await?
            .credential_invalid()
    );
    let observed = with_real_time_limit(server.task).await???;
    assert_eq!(observed.len(), 2);
    for request in observed {
        assert_eq!(request.path, "/token");
        assert_eq!(request.method, "POST");
        let form: BTreeMap<_, _> = url::form_urlencoded::parse(&request.body)
            .into_owned()
            .collect();
        assert_eq!(
            form.get("grant_type").map(String::as_str),
            Some("refresh_token")
        );
        assert_eq!(
            form.get("refresh_token").map(String::as_str),
            Some("synthetic-refresh")
        );
    }
    Ok(())
}

#[tokio::test]
async fn deepseek_allowance_uses_real_tls_get_and_preserves_multi_currency_unknowns()
-> anyhow::Result<()> {
    let mut server = local_tls_server(1, |_| {
        LocalResponse::json(json!({
            "is_available": true,
            "balance_infos": [
                {"currency": "CNY", "total_balance": "12.50"},
                {"currency": "USD", "total_balance": "3.25"},
                {"total_balance": "4.75"}
            ]
        }))
    })
    .await?;
    let (_directory, gateway) = gateway_for(&server, &[]).await?;
    let provider = create_vendor_provider(
        &gateway,
        "DeepSeek TLS allowance",
        "deepseek",
        "default",
        "openai-compatible",
        "https://api.deepseek.com",
        ProviderCredentialInput::ApiKey {
            value: "deepseek-contract-secret".into(),
        },
    )
    .await?;

    let snapshot = gateway
        .admin()
        .refresh_provider_allowance(&provider.id)
        .await?
        .expect("DeepSeek supports allowance");
    assert_eq!(snapshot.status, ProviderAllowanceStatus::Fresh);
    assert_eq!(snapshot.allowances.len(), 3);
    for (key, value, currency) in [
        ("credits_balance_cny", 12.5, Some("CNY")),
        ("credits_balance_usd", 3.25, Some("USD")),
        ("credits_balance", 4.75, None),
    ] {
        let allowance = snapshot
            .allowances
            .iter()
            .find(|allowance| allowance.key == key)
            .unwrap_or_else(|| panic!("missing allowance {key}"));
        assert_eq!(allowance.kind, AllowanceKind::Balance);
        let remaining = allowance.remaining.as_ref().expect("remaining balance");
        assert_eq!(remaining.value, value);
        assert_eq!(remaining.currency.as_deref(), currency);
        assert_eq!(allowance.condition, None);
        assert_eq!(allowance.forecast.status, ExhaustionForecastStatus::Unknown);
    }

    let request = server.requests.recv().await.expect("DeepSeek request");
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, "/user/balance");
    assert_eq!(
        request.headers.get("host").map(String::as_str),
        Some(DEEPSEEK_HOST)
    );
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer deepseek-contract-secret")
    );
    assert!(request.body.is_empty());
    server.task.await??;
    Ok(())
}

async fn codex_auxiliary_refresh_case(
    discover: bool,
    expired: bool,
    token_status: &'static str,
    retried_status: &'static str,
) -> anyhow::Result<()> {
    let request_count = if expired || token_status != "200 OK" {
        2
    } else {
        3
    };
    let server = local_tls_server(request_count, move |request| {
        if request.path == "/oauth/token" {
            return LocalResponse {
                status: token_status,
                ..LocalResponse::json(if token_status == "200 OK" {
                    json!({"access_token": "synthetic-new-access",
                        "refresh_token": "synthetic-rotated-refresh", "expires_in": 3600})
                } else {
                    json!({"error": "temporarily_unavailable"})
                })
            };
        }
        let new_token = request.headers.get("authorization").map(String::as_str)
            == Some("Bearer synthetic-new-access");
        if !new_token || retried_status != "200 OK" {
            return LocalResponse {
                status: "401 Unauthorized",
                ..LocalResponse::json(json!({"error": {"message": "synthetic unauthorized"}}))
            };
        }
        LocalResponse::json(if discover {
            json!({"models": [{"slug": "synthetic-codex-model", "display_name": "Synthetic Codex model"}]})
        } else {
            json!({"rate_limit": {"primary_window": {
                "used_percent": 25, "limit_window_seconds": 18000
            }}})
        })
    }).await?;
    let (_directory, gateway) = gateway_for(&server, &["openai-codex"]).await?;
    let provider = synthetic_oauth_provider(&gateway, "openai-codex", expired).await?;
    let succeeds = token_status == "200 OK" && retried_status == "200 OK";
    if discover {
        let result =
            with_real_time_limit(gateway.admin().get_provider_models(&provider.id)).await?;
        if succeeds {
            assert_eq!(result?, vec!["synthetic-codex-model"]);
        } else {
            assert!(result.is_err());
        }
    } else {
        let snapshot =
            with_real_time_limit(gateway.admin().refresh_provider_allowance(&provider.id))
                .await??
                .context("Codex allowance snapshot")?;
        if succeeds {
            assert_eq!(snapshot.status, ProviderAllowanceStatus::Fresh);
            assert_eq!(snapshot.allowances.len(), 1);
        } else {
            assert_ne!(snapshot.status, ProviderAllowanceStatus::Fresh);
        }
    }
    assert_eq!(
        gateway
            .admin()
            .get_provider(&provider.id)
            .await?
            .credential_invalid(),
        token_status == "200 OK" && retried_status == "401 Unauthorized",
    );
    let credential = gateway
        .storage
        .oauth_credentials()
        .get(&provider.id)
        .await?
        .context("Codex credential retained")?;
    assert_eq!(
        credential.refresh_token.as_deref(),
        Some(if token_status == "200 OK" {
            "synthetic-rotated-refresh"
        } else {
            "synthetic-refresh"
        }),
    );
    let observed = with_real_time_limit(server.task).await???;
    let token_requests: Vec<_> = observed
        .iter()
        .filter(|request| request.path == "/oauth/token")
        .collect();
    assert_eq!(
        token_requests.len(),
        1,
        "one token exchange per auxiliary operation"
    );
    let form: BTreeMap<_, _> = url::form_urlencoded::parse(&token_requests[0].body)
        .into_owned()
        .collect();
    assert_eq!(
        form.get("grant_type").map(String::as_str),
        Some("refresh_token")
    );
    assert_eq!(
        form.get("refresh_token").map(String::as_str),
        Some("synthetic-refresh")
    );
    if expired {
        assert_eq!(
            observed[0].path, "/oauth/token",
            "refresh precedes the auxiliary HTTP request"
        );
    }
    let auxiliary: Vec<_> = observed
        .iter()
        .filter(|request| request.path != "/oauth/token")
        .collect();
    for (index, request) in auxiliary.iter().enumerate() {
        if discover {
            assert!(
                request
                    .path
                    .starts_with("/backend-api/codex/models?client_version=")
            );
        } else {
            assert_eq!(request.path, "/backend-api/wham/usage");
        }
        assert_eq!(
            request.headers.get("authorization").map(String::as_str),
            Some(if expired || index > 0 {
                "Bearer synthetic-new-access"
            } else {
                "Bearer synthetic-old-access"
            }),
        );
    }
    Ok(())
}

#[tokio::test]
async fn codex_expired_discovery_refreshes_before_upstream_request() -> anyhow::Result<()> {
    codex_auxiliary_refresh_case(true, true, "200 OK", "200 OK").await
}

#[tokio::test]
async fn antigravity_invalid_grant_pauses_background_refresh_until_explicit_reconnect()
-> anyhow::Result<()> {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let server_calls = calls.clone();
    let server = local_tls_server(2, move |_| {
        if server_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            LocalResponse {
                status: "400 Bad Request",
                ..LocalResponse::json(json!({"error": "invalid_grant"}))
            }
        } else {
            LocalResponse::json(json!({
                "access_token": "synthetic-reconnected-access",
                "refresh_token": "synthetic-reconnected-refresh",
                "expires_in": 3600
            }))
        }
    })
    .await?;
    let (_directory, gateway) = gateway_for(&server, &["antigravity"]).await?;
    tokio::time::pause();
    with_real_time_limit(async {
        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        let provider = synthetic_oauth_provider(&gateway, "antigravity", true).await?;
        assert!(
            gateway
                .admin()
                .reconnect_provider_oauth(&provider.id)
                .await
                .is_err()
        );
        assert!(
            gateway
                .admin()
                .get_provider(&provider.id)
                .await?
                .credential_invalid()
        );
        assert_eq!(gateway.admin().refresh_oauth_providers().await?, 0);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        gateway
            .admin()
            .reconnect_provider_oauth(&provider.id)
            .await?;
        assert!(
            !gateway
                .admin()
                .get_provider(&provider.id)
                .await?
                .credential_invalid()
        );
        let credential = gateway
            .storage
            .oauth_credentials()
            .get(&provider.id)
            .await?
            .context("explicitly reconnected credential")?;
        assert_eq!(credential.access_token, "synthetic-reconnected-access");
        assert_eq!(
            credential.refresh_token.as_deref(),
            Some("synthetic-reconnected-refresh")
        );
        assert_eq!(server.task.await??.len(), 2);
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn codex_expired_allowance_refreshes_before_upstream_request() -> anyhow::Result<()> {
    codex_auxiliary_refresh_case(false, true, "200 OK", "200 OK").await
}

#[tokio::test]
async fn codex_discovery_401_refreshes_once_and_retries_with_new_token() -> anyhow::Result<()> {
    codex_auxiliary_refresh_case(true, false, "200 OK", "200 OK").await
}

#[tokio::test]
async fn codex_allowance_401_refreshes_once_and_retries_with_new_token() -> anyhow::Result<()> {
    codex_auxiliary_refresh_case(false, false, "200 OK", "200 OK").await
}

#[tokio::test]
async fn codex_allowance_repeated_401_marks_provider_invalid() -> anyhow::Result<()> {
    codex_auxiliary_refresh_case(false, false, "200 OK", "401 Unauthorized").await
}

#[tokio::test]
async fn codex_allowance_transient_refresh_failure_keeps_provider_valid() -> anyhow::Result<()> {
    codex_auxiliary_refresh_case(false, false, "503 Service Unavailable", "200 OK").await
}

#[tokio::test]
async fn codex_discovery_transient_refresh_failure_keeps_provider_valid() -> anyhow::Result<()> {
    codex_auxiliary_refresh_case(true, false, "503 Service Unavailable", "200 OK").await
}

#[tokio::test]
async fn codex_reconnect_recovers_after_transient_refresh_failure() -> anyhow::Result<()> {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let server_calls = calls.clone();
    let server = local_tls_server(2, move |_| {
        if server_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            LocalResponse {
                status: "503 Service Unavailable",
                ..LocalResponse::json(json!({"error": "temporarily_unavailable"}))
            }
        } else {
            LocalResponse::json(json!({
                "access_token": "synthetic-new-access",
                "refresh_token": "synthetic-rotated-refresh", "expires_in": 3600
            }))
        }
    })
    .await?;
    let (_directory, gateway) = gateway_for(&server, &["openai-codex"]).await?;
    let provider = synthetic_oauth_provider(&gateway, "openai-codex", false).await?;
    assert!(
        gateway
            .admin()
            .reconnect_provider_oauth(&provider.id)
            .await
            .is_err()
    );
    assert!(
        !gateway
            .admin()
            .get_provider(&provider.id)
            .await?
            .credential_invalid()
    );
    assert_eq!(
        gateway
            .storage
            .oauth_credentials()
            .get(&provider.id)
            .await?
            .context("retained Codex credential")?
            .refresh_token
            .as_deref(),
        Some("synthetic-refresh"),
    );
    gateway
        .admin()
        .reconnect_provider_oauth(&provider.id)
        .await?;
    let credential = gateway
        .storage
        .oauth_credentials()
        .get(&provider.id)
        .await?
        .context("reconnected Codex credential")?;
    assert_eq!(credential.access_token, "synthetic-new-access");
    assert_eq!(
        credential.refresh_token.as_deref(),
        Some("synthetic-rotated-refresh")
    );
    assert!(
        !gateway
            .admin()
            .get_provider(&provider.id)
            .await?
            .credential_invalid()
    );
    assert_eq!(with_real_time_limit(server.task).await???.len(), 2);
    Ok(())
}

// Only the follower uses this transparent adapter. The notification proves that
// its refresh task observed the peer's active storage lease before release.
struct PeerRefreshObservedStorage {
    inner: crate::storage::traits::DynStorage,
    observed: Arc<Notify>,
}

impl crate::storage::Storage for PeerRefreshObservedStorage {
    fn vendor_plugins(&self) -> &crate::plugin::PluginStore {
        self.inner.vendor_plugins()
    }
    fn providers(&self) -> &dyn crate::storage::ProviderStore {
        self.inner.providers()
    }
    fn web_providers(&self) -> Option<&dyn crate::storage::traits::WebProviderStore> {
        self.inner.web_providers()
    }
    fn routes(&self) -> &dyn crate::storage::RouteStore {
        self.inner.routes()
    }
    fn provider_models(&self) -> &dyn crate::storage::ProviderModelStore {
        self.inner.provider_models()
    }
    fn settings(&self) -> &dyn crate::storage::SettingsStore {
        self.inner.settings()
    }
    fn api_keys(&self) -> Option<&dyn crate::storage::ApiKeyStore> {
        self.inner.api_keys()
    }
    fn auth(&self) -> Option<&dyn crate::storage::AuthAccessStore> {
        self.inner.auth()
    }
    fn admin_identity(&self) -> Option<&dyn crate::storage::AdminIdentityStore> {
        self.inner.admin_identity()
    }
    fn usage_stats(&self) -> &dyn crate::storage::UsageStatsStore {
        self.inner.usage_stats()
    }
    fn oauth_credentials(&self) -> &dyn crate::storage::traits::OAuthCredentialStore {
        self
    }
    fn bootstrap(&self) -> &dyn crate::storage::StorageBootstrap {
        self.inner.bootstrap()
    }
}

#[async_trait::async_trait]
impl crate::storage::traits::OAuthCredentialStore for PeerRefreshObservedStorage {
    async fn get(&self, id: &str) -> anyhow::Result<Option<crate::db::models::OAuthCredential>> {
        self.inner.oauth_credentials().get(id).await
    }
    async fn upsert(
        &self,
        id: &str,
        input: crate::db::models::UpsertOAuthCredential,
    ) -> anyhow::Result<crate::db::models::OAuthCredential> {
        self.inner.oauth_credentials().upsert(id, input).await
    }
    async fn delete(&self, id: &str) -> anyhow::Result<()> {
        self.inner.oauth_credentials().delete(id).await
    }
    async fn try_begin_refresh(
        &self,
        id: &str,
        version: i32,
    ) -> anyhow::Result<Option<crate::db::models::OAuthCredential>> {
        self.inner
            .oauth_credentials()
            .try_begin_refresh(id, version)
            .await
    }
    async fn cancel_refresh(&self, id: &str, version: i32) -> anyhow::Result<bool> {
        self.inner
            .oauth_credentials()
            .cancel_refresh(id, version)
            .await
    }
    async fn complete_refresh(
        &self,
        id: &str,
        version: i32,
        input: crate::db::models::UpsertOAuthCredential,
    ) -> anyhow::Result<crate::db::models::OAuthCredential> {
        self.inner
            .oauth_credentials()
            .complete_refresh(id, version, input)
            .await
    }
    async fn fail_refresh(&self, id: &str, version: i32, message: &str) -> anyhow::Result<bool> {
        self.inner
            .oauth_credentials()
            .fail_refresh(id, version, message)
            .await
    }
    async fn list_expiring(
        &self,
        before: Duration,
    ) -> anyhow::Result<Vec<crate::db::models::OAuthCredential>> {
        self.inner.oauth_credentials().list_expiring(before).await
    }
    async fn recover_stale_refreshing(&self, timeout: Duration) -> anyhow::Result<u64> {
        let recovered = self
            .inner
            .oauth_credentials()
            .recover_stale_refreshing(timeout)
            .await?;
        self.observed.notify_one();
        Ok(recovered)
    }
}

#[tokio::test]
async fn codex_cross_gateway_refresh_failure_preserves_retryable_peer_error_and_reconnects()
-> anyhow::Result<()> {
    let release = Arc::new(Notify::new());
    let response_release = release.clone();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let server_calls = calls.clone();
    let mut server = local_tls_server(2, move |_| {
        if server_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            LocalResponse {
                status: "503 Service Unavailable",
                release: Some(response_release.clone()),
                ..LocalResponse::json(json!({"error": "temporarily_unavailable"}))
            }
        } else {
            LocalResponse::json(json!({
                "access_token": "synthetic-new-access",
                "refresh_token": "synthetic-rotated-refresh", "expires_in": 3600
            }))
        }
    })
    .await?;
    let storage: crate::storage::traits::DynStorage = Arc::new(crate::storage::MemoryStorage::new(
        Vec::new(),
        Vec::new(),
        Vec::new(),
    ));
    let (_leader_directory, leader_gateway) =
        gateway_for_storage(&server, &["openai-codex"], storage.clone()).await?;
    let observed = Arc::new(Notify::new());
    let (_follower_directory, follower_gateway) = gateway_for_storage(
        &server,
        &["openai-codex"],
        Arc::new(PeerRefreshObservedStorage {
            inner: storage.clone(),
            observed: observed.clone(),
        }),
    )
    .await?;
    assert!(!Arc::ptr_eq(
        &leader_gateway.oauth_refresh,
        &follower_gateway.oauth_refresh
    ));
    let provider = synthetic_oauth_provider(&leader_gateway, "openai-codex", false).await?;
    let before = storage
        .oauth_credentials()
        .get(&provider.id)
        .await?
        .context("original credential")?;
    let provider_id = provider.id.clone();
    let leader = tokio::spawn(async move {
        leader_gateway
            .admin()
            .reconnect_provider_oauth(&provider_id)
            .await
    });
    let request = with_real_time_limit(server.requests.recv())
        .await?
        .context("leader token exchange")?;
    assert_eq!(request.path, "/oauth/token");
    let peer_gateway = follower_gateway.clone();
    let provider_id = provider.id.clone();
    let follower = tokio::spawn(async move {
        peer_gateway
            .admin()
            .reconnect_provider_oauth(&provider_id)
            .await
    });
    with_real_time_limit(observed.notified()).await?;
    assert_eq!(
        storage
            .oauth_credentials()
            .get(&provider.id)
            .await?
            .context("active peer lease")?
            .status,
        "refreshing"
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    release.notify_one();
    let (leader, follower) = with_real_time_limit(async { tokio::join!(leader, follower) }).await?;
    let leader_error = leader?.expect_err("leader must propagate the token endpoint failure");
    let follower_error = follower?.expect_err("peer waiter must not accept the old credential");
    for error in [&leader_error, &follower_error] {
        let runtime = error
            .downcast_ref::<stravia_vendor_runtime::RuntimeError>()
            .context("refresh failure must retain its typed runtime classification")?;
        assert!(
            runtime.is_upstream_failure(),
            "transient refresh is not an Auth failure"
        );
        assert_eq!(
            runtime.model_error_kind(),
            Some(stravia_runtime_contract::protocol::ir::AiErrorKind::ServiceUnavailable)
        );
    }
    let peer_runtime = follower_error
        .downcast_ref::<stravia_vendor_runtime::RuntimeError>()
        .unwrap();
    assert_eq!(
        peer_runtime.upstream_status(),
        None,
        "peer cannot infer the token endpoint HTTP status"
    );
    assert_eq!(peer_runtime.retry_after(), None);
    assert!(
        !follower_gateway
            .admin()
            .get_provider(&provider.id)
            .await?
            .credential_invalid()
    );
    let retained = storage
        .oauth_credentials()
        .get(&provider.id)
        .await?
        .context("retained credential")?;
    assert_eq!(retained.access_token, before.access_token);
    assert_eq!(retained.refresh_token, before.refresh_token);
    assert_eq!(retained.expires_at, before.expires_at);
    assert!(
        retained
            .last_error
            .as_deref()
            .is_some_and(|error| !error.is_empty())
    );
    follower_gateway
        .admin()
        .reconnect_provider_oauth(&provider.id)
        .await?;
    let credential = storage
        .oauth_credentials()
        .get(&provider.id)
        .await?
        .context("reconnected credential")?;
    assert_eq!(credential.access_token, "synthetic-new-access");
    assert_eq!(
        credential.refresh_token.as_deref(),
        Some("synthetic-rotated-refresh")
    );
    assert!(
        !follower_gateway
            .admin()
            .get_provider(&provider.id)
            .await?
            .credential_invalid()
    );
    assert_eq!(with_real_time_limit(server.task).await???.len(), 2);
    Ok(())
}

async fn codex_concurrent_reconnect_case(
    cancel_waiter: bool,
    timeout_waiter: bool,
) -> anyhow::Result<()> {
    let release = Arc::new(Notify::new());
    let response_release = release.clone();
    let mut server = local_tls_server(1, move |_| LocalResponse {
        release: Some(response_release.clone()),
        ..LocalResponse::json(json!({
            "access_token": "synthetic-new-access",
            "refresh_token": "synthetic-rotated-refresh",
            "expires_in": 3600
        }))
    })
    .await?;
    let (_directory, gateway) = gateway_for(&server, &["openai-codex"]).await?;
    let provider = synthetic_oauth_provider(&gateway, "openai-codex", false).await?;
    let leader_gateway = gateway.clone();
    let provider_id = provider.id.clone();
    let leader = tokio::spawn(async move {
        leader_gateway
            .admin()
            .reconnect_provider_oauth(&provider_id)
            .await
    });
    let request = with_real_time_limit(server.requests.recv())
        .await?
        .context("leader token exchange observed")?;
    assert_eq!(request.path, "/oauth/token");

    let admin = gateway.admin();
    if timeout_waiter {
        assert!(
            tokio::time::timeout(Duration::ZERO, admin.reconnect_provider_oauth(&provider.id),)
                .await
                .is_err(),
            "a timed-out waiter must not receive the old credential",
        );
    }
    if cancel_waiter {
        let mut cancelled = Box::pin(admin.reconnect_provider_oauth(&provider.id));
        std::future::poll_fn(|cx| {
            assert!(
                cancelled.as_mut().poll(cx).is_pending(),
                "a reconnect waiter must not complete with the old credential"
            );
            std::task::Poll::Ready(())
        })
        .await;
        // Dropping a polled, pending request cancels only this waiter, not the
        // already observed exchange or the remaining reconnect request.
        drop(cancelled);
    }
    let mut survivor = Box::pin(admin.reconnect_provider_oauth(&provider.id));
    std::future::poll_fn(|cx| {
        assert!(
            survivor.as_mut().poll(cx).is_pending(),
            "concurrent reconnect must wait for the observed exchange"
        );
        std::task::Poll::Ready(())
    })
    .await;
    release.notify_one();
    let (leader_status, survivor_status) =
        with_real_time_limit(async { tokio::join!(leader, survivor) }).await?;
    let leader_status = leader_status??;
    let survivor_status = survivor_status?;
    let credential = gateway
        .storage
        .oauth_credentials()
        .get(&provider.id)
        .await?
        .context("published concurrent refresh")?;
    assert_eq!(credential.access_token, "synthetic-new-access");
    assert_eq!(
        credential.refresh_token.as_deref(),
        Some("synthetic-rotated-refresh")
    );
    assert_ne!(
        credential.expires_at.as_deref(),
        Some("2099-01-01T00:00:00Z")
    );
    assert_eq!(leader_status.expires_at, credential.expires_at);
    assert_eq!(survivor_status.expires_at, credential.expires_at);
    assert_eq!(leader_status.status, AuthBindingStatus::Connected.as_str());
    assert_eq!(
        survivor_status.status,
        AuthBindingStatus::Connected.as_str()
    );
    assert!(
        !gateway
            .admin()
            .get_provider(&provider.id)
            .await?
            .credential_invalid()
    );
    assert_eq!(with_real_time_limit(server.task).await???.len(), 1);
    Ok(())
}

#[tokio::test]
async fn codex_concurrent_reconnects_share_one_exchange_and_new_credentials() -> anyhow::Result<()>
{
    codex_concurrent_reconnect_case(false, false).await
}

#[tokio::test]
async fn cancelling_codex_reconnect_waiter_preserves_shared_refresh() -> anyhow::Result<()> {
    codex_concurrent_reconnect_case(true, false).await
}

#[tokio::test]
async fn timing_out_codex_reconnect_waiter_preserves_shared_refresh() -> anyhow::Result<()> {
    codex_concurrent_reconnect_case(false, true).await
}

#[tokio::test]
async fn guard_save_after_suspension_submission_gets_successor_evidence() -> anyhow::Result<()> {
    let release_successor = Arc::new(Notify::new());
    let server_release = release_successor.clone();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let server_calls = calls.clone();
    let mut server = local_tls_server(3, move |request| {
        if let Some(response) = devin_token_response(request) {
            return response;
        }
        let successor = server_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) != 0;
        // Devin retains exhausted daily/weekly windows, unlike DeepSeek's
        // zero-balance currency filtering. Omitted percentages are proto3 zero.
        let mut plan = proto_u64(1, 17);
        plan.extend(proto_string(2, "Guard publication contract"));
        plan.extend(proto_u64(12, 100));
        plan.extend(proto_u64(13, 10));
        let mut status = proto_len(1, &plan);
        if successor {
            status.extend(proto_u64(14, 10));
        }
        status.extend(proto_u64(17, 2_000_000_000));
        status.extend(proto_u64(18, 2_000_100_000));
        let user = proto_len(13, &status);
        let mut response = LocalResponse::protobuf(proto_len(1, &user));
        if successor {
            response.release = Some(server_release.clone());
        }
        response
    })
    .await?;
    // Exercise the production SQLite provider repository while retaining an
    // independently gateable memory sample store for the publication boundary.
    let directory = tempfile::tempdir()?;
    let mut gateway = Gateway::new(GatewayConfig {
        data_dir: directory.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    // Keep the sample tail independently gateable without locking provider DB writes.
    gateway.allowance_samples = crate::admin::provider_allowance::AllowanceSampleStore::memory();
    crate::plugin::test_support::install_distributed_vendor(&gateway, "devin").await?;
    let root = reqwest::tls::Certificate::from_der(&server.certificate_der)?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .https_only(true)
        .http1_only()
        .tls_certs_only([root])
        .dns_resolver(LoopbackOnlyResolver {
            address: server.address,
        })
        .build()?;
    gateway.vendor_http_client = client.clone();
    gateway.vendor_websocket_client = client;
    let authorization = sign_in_devin_manually(&gateway, &mut server).await?;
    let provider = gateway
        .admin()
        .create_provider_with_oauth_session(
            &authorization.session_id,
            devin_oauth_provider("Guard publication barrier"),
        )
        .await?;
    let crate::admin::provider_allowance::AllowanceSampleStore::Memory(rows) =
        &gateway.allowance_samples
    else {
        panic!("memory gateway must use the real memory sample store")
    };
    // This is the real sample-store write lock: hold A after its suspension
    // submission but before forecasts/cache publication, without blocking guards.
    let publication_gate = rows.write().await;
    let first = gateway
        .admin()
        .replace_provider_allowance_guards(&provider.id, vec!["daily".into()])
        .await
        .context("guard successor regression: saving A guard keys")?
        .unwrap();
    assert!(first.target.refreshing);
    let request = tokio::time::timeout(Duration::from_secs(10), server.requests.recv())
        .await
        .context("guard successor regression: waiting for first upstream request")?
        .expect("first allowance request");
    assert_eq!(request.path, DEVIN_ALLOWANCE_PATH);
    // Observe committed consumer-visible evidence, not inflight internals.
    // The lifecycle also consumes state.changed(), so its Notify cannot serve
    // as an exclusive test gate. This bounded yield loop waits only for proof
    // that A submitted; the held real sample lock keeps its publication pending.
    let submitted = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(suspension) = gateway
                .admin()
                .get_provider_allowance_suspension(&provider.id)
                .await?
            {
                break Ok::<_, anyhow::Error>(suspension);
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("guard successor regression: waiting for A persisted suspension")??;
    assert_eq!(submitted.triggered_keys, vec!["daily"]);
    let keys = vec!["daily".into(), "weekly".into()];
    let saved = tokio::time::timeout(
        Duration::from_secs(5),
        gateway
            .admin()
            .replace_provider_allowance_guards(&provider.id, keys),
    )
    .await
    .context("guard successor regression: waiting for B guard save")??
    .unwrap();
    assert_eq!(saved.keys, vec!["daily", "weekly"]);
    assert!(saved.target.refreshing);
    // C must merge with B while B waits for A's complete publication.
    let merged = tokio::time::timeout(
        Duration::from_secs(5),
        gateway
            .admin()
            .replace_provider_allowance_guards(&provider.id, saved.keys.clone()),
    )
    .await
    .context("guard successor regression: waiting for C guard save")??
    .unwrap();
    assert_eq!(merged.keys, saved.keys);
    assert!(server.requests.try_recv().is_err());
    let gw = gateway.clone();
    let id = provider.id.clone();
    let mut pending = tokio::spawn(async move { gw.admin().get_provider_allowance(&id).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut pending)
            .await
            .is_err()
    );
    drop(publication_gate);
    let request = tokio::time::timeout(Duration::from_secs(10), server.requests.recv())
        .await
        .context("guard successor regression: waiting for successor upstream request")?
        .expect("successor allowance request");
    assert_eq!(request.path, DEVIN_ALLOWANCE_PATH);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    // A's cleanup must not remove B: a later GET still joins held B, not A's cache.
    let gw = gateway.clone();
    let id = provider.id.clone();
    let mut later = tokio::spawn(async move { gw.admin().get_provider_allowance(&id).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut later)
            .await
            .is_err()
    );
    release_successor.notify_one();
    let snapshot = tokio::time::timeout(Duration::from_secs(10), pending)
        .await
        .context("guard successor regression: waiting for B snapshot")???
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), later)
            .await
            .context("guard successor regression: waiting for later GET snapshot")???
            .unwrap(),
        snapshot
    );
    assert_eq!(snapshot.status, ProviderAllowanceStatus::Fresh);
    let daily = snapshot
        .allowances
        .iter()
        .find(|item| item.key == "daily")
        .unwrap();
    assert_eq!(daily.used_percent, Some(90.0));
    assert!(
        snapshot
            .allowances
            .iter()
            .filter(|item| matches!(item.key.as_str(), "daily" | "weekly"))
            .all(|item| item.guarded)
    );
    assert_eq!(
        snapshot.suspension.as_ref().unwrap().triggered_keys,
        vec!["weekly"]
    );
    assert_eq!(
        gateway
            .admin()
            .get_provider_allowance_suspension(&provider.id)
            .await?
            .unwrap()
            .triggered_keys,
        vec!["weekly"]
    );
    assert_eq!(
        gateway
            .admin()
            .get_provider_allowance(&provider.id)
            .await?
            .unwrap(),
        snapshot
    );
    // 一次授权码兑换加两次额度请求。
    assert_eq!(server.task.await??.len(), 3);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test]
async fn gateway_background_discovery_and_allowance_keep_distinct_phases_and_stop_on_shutdown()
-> anyhow::Result<()> {
    let first_sample = Arc::new(Notify::new());
    let first_release = Arc::clone(&first_sample);
    // 后台任务在构造时捕获HTTP客户端；使用字面量loopback地址而非替换前台客户端。
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let base_url = format!("http://{}", listener.local_addr()?);
    let (request_tx, mut requests) = mpsc::unbounded_channel();
    let server_task = tokio::spawn(async move {
        let mut first_allowance = true;
        for _ in 0..5 {
            let (mut socket, _) = listener.accept().await?;
            let request = read_http_request(&mut socket).await?;
            if request.path != "/oauth/exchange" {
                request_tx
                    .send(request.clone())
                    .map_err(|_| anyhow::anyhow!("request observer dropped"))?;
            }
            let response = match request.path.as_str() {
                "/oauth/exchange" => LocalResponse::json(json!({
                    "access_token": "schedule-token",
                    "refresh_token": "schedule-refresh-token",
                    "expires_at_unix_ms": 4_102_444_800_000_i64
                })),
                "/allowance" => {
                    if first_allowance {
                        first_allowance = false;
                        let mut response =
                            LocalResponse::json(json!({"error": "temporarily unavailable"}));
                        response.status = "503 Service Unavailable";
                        response.release = Some(Arc::clone(&first_release));
                        response
                    } else {
                        LocalResponse::json(json!({"remaining": "7.25"}))
                    }
                }
                "/models" => LocalResponse::json(json!({
                    "models": [{"id": "scheduled-model", "display_name": "Scheduled Model", "family": "management", "selector": "management-selector"}],
                    "next_cursor": null
                })),
                path => panic!("unexpected scheduled upstream request: {path}"),
            };
            write_local_response(&mut socket, response).await?;
        }
        Ok::<(), anyhow::Error>(())
    });
    tokio::time::pause();
    let clock_guard = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    let directory = tempfile::tempdir()?;
    let gateway = Gateway::from_storage(
        GatewayConfig {
            data_dir: directory.path().to_path_buf(),
            ..Default::default()
        },
        Arc::new(crate::storage::MemoryStorage::new(
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )),
    )
    .await?;
    let artifact = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("target/vendor-test-fixtures/management-v2.wasm");
    let preview = gateway
        .admin()
        .preview_vendor_plugin(std::fs::read(&artifact)?)
        .await?;
    gateway
        .admin()
        .confirm_vendor_plugin(crate::plugin::ConfirmPluginUpdate {
            preview_id: preview.id,
            allow_data_discard: false,
        })
        .await?;
    let authorization = gateway
        .admin()
        .init_oauth_session(
            AuthSessionCandidate {
                vendor_id: "fixture.management".into(),
                channel: "default".into(),
                provider_id: None,
                base_url: base_url.clone(),
                protocol: Some("fixture-management".into()),
                options: BTreeMap::from([("workspace".into(), json!("schedule-workspace"))]),
                credentials: BTreeMap::new(),
                use_proxy: false,
            },
            OAuthSessionStartOptions {
                callback_mode: OAuthCallbackMode::Manual,
                redirect_uri: "http://127.0.0.1:18765/callback".into(),
                listener_port: None,
                fallback_reason: None,
            },
        )
        .await?;
    let state = query_parameter(
        authorization
            .auth_url
            .as_deref()
            .context("fixture authorization URL")?,
        "state",
    )?;
    gateway
        .admin()
        .complete_oauth_session(
            &authorization.session_id,
            AuthCompletionInput {
                input: AuthCompletionValue::CallbackUrl {
                    value: format!(
                        "{}?code=fixture-code&state={state}",
                        authorization.redirect_uri
                    ),
                },
            },
        )
        .await?;
    let provider = gateway
        .admin()
        .create_provider_with_oauth_session(
            &authorization.session_id,
            CreateProvider {
                name: Some("Scheduled management maintenance".into()),
                source: ProviderSourceInput::Custom {
                    vendor: "fixture.management".into(),
                    channel: "default".into(),
                    protocol: Some("fixture-management".into()),
                    base_url,
                    models_source: None,
                    static_models: None,
                },
                credential: ProviderCredentialInput::None,
                vendor_options: BTreeMap::from([("workspace".into(), json!("schedule-workspace"))])
                    .into_iter()
                    .collect(),
                use_proxy: false,
            },
        )
        .await?;
    // 构造前固定调度时钟，guard只阻止自动跳时，不改变产品std::Instant预算。
    // 先推进首个毫秒，唤醒timer wheel中启动即到期的维护任务。
    tokio::time::advance(Duration::from_millis(1)).await;
    let result = async {
        // 无请求窗口仅为有界佐证；主要以60分钟模型往返、库存发布后的请求顺序验证错峰。
        let settle = || async {
            let until = std::time::Instant::now() + Duration::from_millis(100);
            while std::time::Instant::now() < until {
                tokio::task::yield_now().await;
            }
        };
        async fn next_request(
            requests: &mut mpsc::UnboundedReceiver<ObservedRequest>,
        ) -> anyhow::Result<ObservedRequest> {
            with_real_time_limit(requests.recv())
                .await?
                .context("scheduled loopback request observer closed")
        }
        settle().await;
        tokio::time::advance(Duration::from_secs(29 * 60)).await;
        settle().await;
        assert!(
            requests.try_recv().is_err(),
            "no sampling before 30 minutes"
        );
        tokio::time::advance(Duration::from_secs(60)).await;
        settle().await;
        assert!(
            requests.try_recv().is_err(),
            "allowance sampling must wait until the 35-minute phase"
        );
        tokio::time::advance(Duration::from_secs(5 * 60)).await;
        assert_eq!(
            next_request(&mut requests)
                .await
                .context("35-minute allowance phase must issue its first request")?
                .path,
            "/allowance"
        );
        let admin = gateway.admin();
        let pending = admin.get_provider_allowance(&provider.id);
        tokio::pin!(pending);
        // 先经公开API加入被fixture阻塞的采样，再释放503，避免错误后另起刷新。
        tokio::select! {
            biased;
            _ = &mut pending => panic!("withheld first sample must still be pending"),
            _ = std::future::ready(()) => {}
        }
        first_sample.notify_one();
        let failed = with_real_time_limit(&mut pending)
            .await
            .context("failed scheduled sample was not published")??
            .expect("failed allowance sample");
        assert_eq!(failed.status, ProviderAllowanceStatus::Error);
        assert!(failed.error.is_some());
        assert!(failed.allowances.is_empty());
        tokio::time::advance(Duration::from_secs(25 * 60)).await;
        assert_eq!(
            next_request(&mut requests)
                .await
                .context("60-minute model phase must discover inventory")?
                .path,
            "/models"
        );
        with_real_time_limit(async {
            loop {
                let inventory = gateway.admin().list_provider_models(&provider.id).await?;
                if inventory
                    .models
                    .iter()
                    .any(|model| model.id == "scheduled-model")
                {
                    return Ok::<(), anyhow::Error>(());
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .context("scheduled discovery result was not published")??;
        settle().await;
        assert!(
            requests.try_recv().is_err(),
            "60-minute model discovery must not trigger allowance sampling"
        );
        tokio::time::advance(Duration::from_secs(5 * 60)).await;
        assert_eq!(
            next_request(&mut requests)
                .await
                .context("65-minute allowance phase must issue its second request")?
                .path,
            "/allowance"
        );
        let admin = gateway.admin();
        let snapshot = with_real_time_limit(admin.get_provider_allowance(&provider.id))
            .await
            .context("scheduled allowance result was not published")??
            .expect("scheduled allowance result");
        assert_eq!(snapshot.status, ProviderAllowanceStatus::Fresh);
        let remaining = snapshot.allowances[0]
            .remaining
            .as_ref()
            .expect("sampled balance");
        assert_eq!(remaining.value, 7.25);
        assert_eq!(remaining.unit, "requests");
        assert_eq!(remaining.currency, None);
        settle().await;
        gateway.shutdown().await;
        tokio::time::advance(Duration::from_secs(2 * 60 * 60)).await;
        settle().await;
        assert!(
            requests.try_recv().is_err(),
            "shutdown must cancel future maintenance"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    gateway.shutdown().await;
    server_task.abort();
    clock_guard.abort();
    tokio::time::resume();
    result
}

#[tokio::test]
async fn paginated_discovery_exhausts_one_minute_without_replacing_inventory() -> anyhow::Result<()>
{
    let first_page = Arc::new(Notify::new());
    let second_page = Arc::new(Notify::new());
    let first_release = Arc::clone(&first_page);
    let second_release = Arc::clone(&second_page);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let server_calls = Arc::clone(&calls);
    let mut server = local_tls_server(3, move |request| {
        let call = server_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(request.method, "GET");
        match call {
            0 => {
                assert_eq!(request.path, "/v1/models");
                LocalResponse::json(json!({"data": [{"id": "confirmed-model"}]}))
            }
            1 => {
                assert_eq!(request.path, "/v1/models");
                let mut response = LocalResponse::json(json!({
                    "data": [{"id": "unconfirmed-first-page"}],
                    "next_cursor": "second-page"
                }));
                response.release = Some(Arc::clone(&first_release));
                response
            }
            2 => {
                assert_eq!(request.path, "/v1/models?after=second-page");
                let mut response = LocalResponse::json(json!({
                    "data": [{"id": "unconfirmed-second-page"}]
                }));
                response.release = Some(Arc::clone(&second_release));
                response
            }
            _ => unreachable!("unexpected discovery request"),
        }
    })
    .await?;
    let (_directory, gateway) = gateway_for(&server, &[]).await?;
    let provider = create_vendor_provider(
        &gateway,
        "Custom shared pagination budget",
        "custom",
        "default",
        "openai-compatible",
        "https://api.deepseek.com",
        ProviderCredentialInput::ApiKey {
            value: "deepseek-pagination-secret".into(),
        },
    )
    .await?;
    let confirmed = gateway.admin().sync_provider_models(&provider.id).await?;
    assert_eq!(
        confirmed.outcome,
        crate::provider_models::ModelChangeOutcome::Success
    );
    let before = gateway.admin().list_provider_models(&provider.id).await?;
    assert_eq!(before.models.len(), 1);
    assert_eq!(before.models[0].id, "confirmed-model");
    server
        .requests
        .recv()
        .await
        .expect("confirmed discovery request");

    let sync_gateway = gateway.clone();
    let provider_id = provider.id.clone();
    let started_at = std::time::Instant::now();
    let mut sync = tokio::spawn(async move {
        sync_gateway
            .admin()
            .sync_provider_models(&provider_id)
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), server.requests.recv())
        .await?
        .expect("first discovery page request");
    // 首页消耗真实31秒；第二页只能使用同一次60秒预算的剩余部分，不能重计时。
    tokio::time::sleep(Duration::from_secs(31)).await;
    first_page.notify_one();
    tokio::time::timeout(Duration::from_secs(10), server.requests.recv())
        .await?
        .expect("second discovery page request");
    let completed = tokio::time::timeout(Duration::from_secs(35), &mut sync).await;
    let elapsed = started_at.elapsed();
    // 无论同步完成或超时，都释放本地fixture，避免响应任务挂起。
    second_page.notify_one();
    server.task.await??;
    let error = completed
        .context("paginated discovery must fail within its single 60-second budget")??
        .expect_err("an unfinished final page must be an explicit discovery failure");
    let failure = error
        .downcast_ref::<crate::provider_models::ProviderModelSyncFailure>()
        .context("admin API must expose a persisted sync failure summary")?;
    assert_eq!(
        failure.summary.outcome,
        crate::provider_models::ModelChangeOutcome::Failure
    );
    assert!(
        elapsed >= Duration::from_secs(60),
        "discovery must retain its full one-minute budget"
    );
    let state = gateway
        .admin()
        .provider_model_sync_state(&provider.id)
        .await?;
    assert_eq!(state.last_check.as_ref(), Some(&failure.summary));
    let after = gateway.admin().list_provider_models(&provider.id).await?;
    assert_eq!(
        serde_json::to_value(&after.models)?,
        serde_json::to_value(&before.models)?
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    gateway.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn devin_slow_model_discovery_publishes_the_confirmed_inventory() -> anyhow::Result<()> {
    let release = Arc::new(Notify::new());
    let server_release = Arc::clone(&release);
    let mut config = proto_string(1, "Delayed catalog model");
    config.extend(proto_string(22, "contract-delayed-model"));
    config.extend(proto_len(23, &proto_string(23, "contract-delayed-model")));
    let body = proto_len(1, &config);
    let mut server = local_tls_server(2, move |request| {
        if let Some(response) = devin_token_response(request) {
            return response;
        }
        let mut response = LocalResponse::protobuf(body.clone());
        response.release = Some(Arc::clone(&server_release));
        response
    })
    .await?;
    let (_directory, gateway) = gateway_for(&server, &["devin"]).await?;
    let authorization = sign_in_devin_manually(&gateway, &mut server).await?;
    let provider = gateway
        .admin()
        .create_provider_with_oauth_session(
            &authorization.session_id,
            devin_oauth_provider("Devin slow discovery"),
        )
        .await?;
    gateway
        .start_provider_model_sync(tokio::time::Instant::now())
        .await?;
    let request = tokio::time::timeout(Duration::from_secs(10), server.requests.recv())
        .await?
        .expect("Devin discovery request");
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, DEVIN_MODELS_PATH);
    let sync_gateway = gateway.clone();
    let provider_id = provider.id.clone();
    let sync = tokio::spawn(async move {
        sync_gateway
            .admin()
            .sync_provider_models(&provider_id)
            .await
    });
    // 真实上游延迟跨过旧的 30 秒发现上限；不缩短产品 deadline 来模拟。
    tokio::time::sleep(Duration::from_secs(31)).await;
    release.notify_one();
    let summary = tokio::time::timeout(Duration::from_secs(10), sync).await???;
    assert_eq!(
        summary.outcome,
        crate::provider_models::ModelChangeOutcome::Success
    );
    assert_eq!(
        summary.source,
        crate::provider_models::ModelChangeSource::AutomaticSync,
        "manual checks must join the scheduled discovery instead of issuing another request"
    );
    let state = gateway
        .admin()
        .provider_model_sync_state(&provider.id)
        .await?;
    assert_eq!(state.last_check.as_ref(), Some(&summary));
    let inventory = gateway.admin().list_provider_models(&provider.id).await?;
    assert!(
        inventory
            .models
            .iter()
            .any(|model| model.id == "contract-delayed-model"),
        "the delayed upstream model must be saved, not replaced by static fallback"
    );
    server.task.await??;
    Ok(())
}

#[tokio::test]
async fn devin_allowance_uses_real_tls_protobuf_request_and_decodes_credit_windows()
-> anyhow::Result<()> {
    let mut server = local_tls_server(2, |request| {
        devin_token_response(request)
            .unwrap_or_else(|| LocalResponse::protobuf(devin_allowance_fixture()))
    })
    .await?;
    let (_directory, gateway) = gateway_for(&server, &["devin"]).await?;
    let authorization = sign_in_devin_manually(&gateway, &mut server).await?;
    let provider = gateway
        .admin()
        .create_provider_with_oauth_session(
            &authorization.session_id,
            devin_oauth_provider("Devin TLS allowance"),
        )
        .await?;

    let snapshot = gateway
        .admin()
        .refresh_provider_allowance(&provider.id)
        .await?
        .expect("Devin supports allowance");
    assert_eq!(snapshot.status, ProviderAllowanceStatus::Fresh);
    assert_eq!(snapshot.plan_label.as_deref(), Some("Devin Max Contract"));
    let daily = snapshot
        .allowances
        .iter()
        .find(|allowance| allowance.key == "daily")
        .expect("exhausted daily window");
    assert_eq!(daily.used_percent, Some(100.0));
    assert_eq!(daily.condition, Some(AllowanceCondition::Exhausted));
    assert_eq!(daily.reset_at, Some(2_000_000_000_000));
    let weekly = snapshot
        .allowances
        .iter()
        .find(|allowance| allowance.key == "weekly")
        .expect("weekly window");
    assert_eq!(weekly.used_percent, Some(67.0));
    assert_eq!(weekly.reset_at, Some(2_000_100_000_000));
    let prompt = snapshot
        .allowances
        .iter()
        .find(|allowance| allowance.key == "prompt_credits")
        .expect("prompt credit allowance");
    assert_eq!(prompt.used.as_ref().map(|amount| amount.value), Some(25.0));
    assert_eq!(
        prompt.limit.as_ref().map(|amount| amount.value),
        Some(100.0)
    );
    assert_eq!(
        prompt.remaining.as_ref().map(|amount| amount.value),
        Some(75.0)
    );
    assert_eq!(prompt.used_percent, Some(25.0));
    let flow = snapshot
        .allowances
        .iter()
        .find(|allowance| allowance.key == "flow_credits")
        .expect("flow credit allowance");
    assert_eq!(
        flow.remaining.as_ref().map(|amount| amount.value),
        Some(6.0)
    );
    let balance = snapshot
        .allowances
        .iter()
        .find(|allowance| allowance.key == "balance_usd")
        .expect("USD balance");
    assert_eq!(
        balance.remaining.as_ref().map(|amount| amount.value),
        Some(12.5)
    );

    let request = server.requests.recv().await.expect("Devin request");
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, DEVIN_ALLOWANCE_PATH);
    assert_eq!(
        request.headers.get("host").map(String::as_str),
        Some(DEVIN_HOST)
    );
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Basic devin-contract-token-devin-contract-token")
    );
    assert_eq!(
        request.headers.get("content-type").map(String::as_str),
        Some("application/proto")
    );
    assert_eq!(
        request
            .headers
            .get("connect-protocol-version")
            .map(String::as_str),
        Some("1")
    );
    assert!(
        request
            .body
            .windows("devin-contract-token".len())
            .any(|window| window == b"devin-contract-token")
    );
    server.task.await??;
    Ok(())
}

#[tokio::test]
async fn devin_manual_flow_opens_the_cli_code_page_and_exchanges_the_pasted_code()
-> anyhow::Result<()> {
    let mut server = local_tls_server(1, |request| {
        devin_token_response(request).expect("only the code exchange is expected")
    })
    .await?;
    let (_directory, gateway) = gateway_for(&server, &["devin"]).await?;
    let authorization = sign_in_devin_manually(&gateway, &mut server).await?;

    // 与 `devin auth login --force-manual-token-flow` 相同：app.devin.ai 授权页，
    // 不带 redirect_uri，页面直接展示可粘贴的授权码。
    let auth_url = reqwest::Url::parse(authorization.auth_url.as_deref().expect("auth URL"))?;
    assert_eq!(
        auth_url.origin().ascii_serialization(),
        "https://app.devin.ai"
    );
    assert_eq!(auth_url.path(), "/auth/cli/continue");
    assert_eq!(
        query_parameter(auth_url.as_str(), "code_challenge_method")?,
        "S256"
    );
    assert_eq!(query_parameter(auth_url.as_str(), "cli_pkce_marker")?, "1");
    assert!(auth_url.query_pairs().all(|(key, _)| key != "redirect_uri"));

    let provider = gateway
        .admin()
        .create_provider_with_oauth_session(
            &authorization.session_id,
            devin_oauth_provider("Devin manual code"),
        )
        .await?;
    let credential = gateway
        .storage
        .oauth_credentials()
        .get(&provider.id)
        .await?
        .expect("manual OAuth credential");
    assert_eq!(credential.access_token, DEVIN_CONTRACT_TOKEN);
    server.task.await??;
    Ok(())
}

#[tokio::test]
async fn devin_manual_input_without_a_code_is_rejected_before_any_exchange() -> anyhow::Result<()> {
    let mut server = local_tls_server(1, |request| {
        devin_token_response(request).expect("only the code exchange is expected")
    })
    .await?;
    let (_directory, gateway) = gateway_for(&server, &["devin"]).await?;
    let session = gateway
        .admin()
        .init_oauth_session(
            AuthSessionCandidate {
                vendor_id: "devin".into(),
                channel: "devin".into(),
                provider_id: None,
                base_url: DEVIN_BASE_URL.into(),
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
            &session.session_id,
            AuthCompletionInput {
                input: AuthCompletionValue::Manual {
                    value: "https://app.devin.ai/somewhere?state=abc".into(),
                },
            },
        )
        .await
        .expect_err("a URL without a code is not an authorization code");
    assert!(server.requests.try_recv().is_err());
    gateway
        .admin()
        .cancel_oauth_session(&session.session_id)
        .await?;
    gateway.shutdown().await;
    server.task.abort();
    Ok(())
}

#[tokio::test]
async fn devin_pkce_exchanges_are_session_isolated_and_bind_their_own_credentials()
-> anyhow::Result<()> {
    let mut server = local_tls_server(2, |request| {
        let body: Value = serde_json::from_slice(&request.body).expect("OAuth JSON body");
        let code = body["code"].as_str().expect("authorization code");
        LocalResponse::json(json!({
            "sessionToken": format!("token-for-{code}"),
            "refreshToken": format!("refresh-for-{code}"),
            "expiresIn": 3600,
            "scope": "openid offline_access",
            "apiServerUrl": DEVIN_BASE_URL
        }))
    })
    .await?;
    let (_directory, gateway) = gateway_for(&server, &["devin"]).await?;
    let first = start_devin_session(&gateway).await?;
    let second = start_devin_session(&gateway).await?;
    let first_url = first.auth_url.as_deref().expect("first authorization URL");
    let second_url = second
        .auth_url
        .as_deref()
        .expect("second authorization URL");
    let first_state = query_parameter(first_url, "state")?;
    let second_state = query_parameter(second_url, "state")?;
    assert_ne!(first_state, second_state);
    assert_ne!(
        query_parameter(first_url, "code_challenge")?,
        query_parameter(second_url, "code_challenge")?
    );

    let crossed = gateway
        .admin()
        .complete_oauth_session(
            &first.session_id,
            AuthCompletionInput {
                input: AuthCompletionValue::CallbackUrl {
                    value: format!(
                        "http://127.0.0.1:18765/oauth/callback?code=crossed&state={second_state}"
                    ),
                },
            },
        )
        .await
        .expect_err("a different session state must not exchange");
    assert!(crossed.to_string().contains("STATE_MISMATCH"));
    assert!(server.requests.try_recv().is_err());

    for (session, state, code) in [
        (&first, first_state.as_str(), "first-code"),
        (&second, second_state.as_str(), "second-code"),
    ] {
        let status = gateway
            .admin()
            .complete_oauth_session(
                &session.session_id,
                AuthCompletionInput {
                    input: AuthCompletionValue::CallbackUrl {
                        value: format!(
                            "http://127.0.0.1:18765/oauth/callback?code={code}&state={state}"
                        ),
                    },
                },
            )
            .await?;
        assert!(matches!(status, AuthSessionStatusData::Ready { .. }));
    }

    let first_request = server.requests.recv().await.expect("first exchange");
    let second_request = server.requests.recv().await.expect("second exchange");
    for (request, auth_url, expected_code) in [
        (&first_request, first_url, "first-code"),
        (&second_request, second_url, "second-code"),
    ] {
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, DEVIN_TOKEN_PATH);
        assert_eq!(
            request.headers.get("host").map(String::as_str),
            Some(DEVIN_HOST)
        );
        assert_eq!(
            request
                .headers
                .get("connect-protocol-version")
                .map(String::as_str),
            Some("1")
        );
        let body: Value = serde_json::from_slice(&request.body)?;
        assert_eq!(body["code"], expected_code);
        let verifier = body["codeVerifier"].as_str().expect("PKCE verifier");
        let expected_challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(
            query_parameter(auth_url, "code_challenge")?,
            expected_challenge
        );
    }

    let first_provider = gateway
        .admin()
        .create_provider_with_oauth_session(
            &first.session_id,
            devin_oauth_provider("first isolated Devin"),
        )
        .await?;
    let second_provider = gateway
        .admin()
        .create_provider_with_oauth_session(
            &second.session_id,
            devin_oauth_provider("second isolated Devin"),
        )
        .await?;
    let first_credential = gateway
        .storage
        .oauth_credentials()
        .get(&first_provider.id)
        .await?
        .expect("first OAuth credential");
    let second_credential = gateway
        .storage
        .oauth_credentials()
        .get(&second_provider.id)
        .await?
        .expect("second OAuth credential");
    assert_eq!(first_credential.access_token, "token-for-first-code");
    assert_eq!(second_credential.access_token, "token-for-second-code");
    assert_ne!(
        first_credential.access_token,
        second_credential.access_token
    );
    for provider in [&first_provider, &second_provider] {
        let status = gateway
            .admin()
            .get_provider_oauth_status(&provider.id)
            .await?;
        assert_eq!(status.status, AuthBindingStatus::Connected.as_str());
        assert_eq!(status.resource_url.as_deref(), Some(DEVIN_BASE_URL));
    }
    server.task.await??;
    Ok(())
}

#[tokio::test]
async fn cancelling_a_devin_exchange_prevents_a_late_tls_callback_from_restoring_it()
-> anyhow::Result<()> {
    let release = Arc::new(Notify::new());
    let response_release = Arc::clone(&release);
    let mut server = local_tls_server(1, move |_| LocalResponse {
        status: "200 OK",
        content_type: "application/json",
        body: serde_json::to_vec(&json!({
            "sessionToken": "late-session-token",
            "apiServerUrl": DEVIN_BASE_URL
        }))
        .expect("fixture JSON"),
        release: Some(Arc::clone(&response_release)),
    })
    .await?;
    let (_directory, gateway) = gateway_for(&server, &["devin"]).await?;
    let session = start_devin_session(&gateway).await?;
    let auth_url = session.auth_url.as_deref().expect("authorization URL");
    let state = query_parameter(auth_url, "state")?;
    let session_id = session.session_id.clone();
    let completing_gateway = gateway.clone();
    let completion = tokio::spawn(async move {
        completing_gateway
            .admin()
            .complete_oauth_session(
                &session_id,
                AuthCompletionInput {
                    input: AuthCompletionValue::CallbackUrl {
                        value: format!(
                            "http://127.0.0.1:18765/oauth/callback?code=late-code&state={state}"
                        ),
                    },
                },
            )
            .await
    });

    let request = server.requests.recv().await.expect("in-flight exchange");
    assert_eq!(request.path, DEVIN_TOKEN_PATH);
    gateway
        .admin()
        .cancel_oauth_session(&session.session_id)
        .await?;
    release.notify_one();
    let completion_error = completion
        .await?
        .expect_err("a cancelled session must reject its late exchange result");
    assert!(
        completion_error.to_string().contains("not found")
            || completion_error.to_string().contains("cancel")
    );
    assert!(
        !gateway
            .auth_sessions
            .read()
            .await
            .contains_key(&session.session_id)
    );
    assert!(
        gateway
            .admin()
            .get_oauth_session_status(&session.session_id)
            .await
            .is_err()
    );
    server.task.await??;
    Ok(())
}
