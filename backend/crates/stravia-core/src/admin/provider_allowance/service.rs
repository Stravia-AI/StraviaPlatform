use std::collections::{HashMap, HashSet};
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures::future::{BoxFuture, FutureExt, Shared};
use futures::stream::{self, StreamExt};
use reqwest::header::{
    ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, ORIGIN, REFERER,
    USER_AGENT,
};
use reqwest::{Method, StatusCode};
use sha2::{Digest, Sha256};
#[cfg(test)]
use tokio::sync::Notify;
use tokio::sync::{Mutex, RwLock};

use crate::admin::AdminService;
use crate::db::models::Provider;

use super::{
    MonitorKind, ParsedAllowance, ProviderAllowanceError, ProviderAllowanceErrorCategory,
    ProviderAllowanceSnapshot, ProviderAllowanceStatus, monitor_for, parse_minimax_fallback,
    parse_monitor_response,
};

const SUCCESS_TTL: Duration = Duration::from_secs(180);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_PARALLEL_REFRESHES: usize = 4;

type SharedFetch =
    Shared<BoxFuture<'static, Result<Option<ProviderAllowanceSnapshot>, Arc<anyhow::Error>>>>;

#[derive(Clone, Default)]
pub(crate) struct ProviderAllowanceState {
    inner: Arc<ProviderAllowanceStateInner>,
}

#[derive(Default)]
struct ProviderAllowanceStateInner {
    cache: RwLock<HashMap<String, CacheEntry>>,
    inflight: Mutex<HashMap<String, SharedFetch>>,
    #[cfg(test)]
    coalesced_fetches: AtomicUsize,
    #[cfg(test)]
    coalesced_fetch: Notify,
}

#[cfg(test)]
impl ProviderAllowanceState {
    pub(super) async fn wait_for_coalesced_fetch(&self) {
        loop {
            let notified = self.inner.coalesced_fetch.notified();
            if self.inner.coalesced_fetches.load(Ordering::SeqCst) > 0 {
                return;
            }
            notified.await;
        }
    }
}

#[derive(Clone)]
struct CacheEntry {
    identity: String,
    snapshot: ProviderAllowanceSnapshot,
    successful_at: Option<Instant>,
}

pub(super) struct AllowanceHttpRequest {
    pub method: Method,
    pub url: &'static str,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

pub(super) struct AllowanceHttpResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TransportFailure {
    Timeout,
    Unavailable,
    InvalidResponse,
}

#[async_trait]
pub(super) trait AllowanceTransport: Send + Sync {
    async fn execute(
        &self,
        client: reqwest::Client,
        use_proxy: bool,
        request: AllowanceHttpRequest,
    ) -> Result<AllowanceHttpResponse, TransportFailure>;
}

struct ReqwestAllowanceTransport;

#[async_trait]
impl AllowanceTransport for ReqwestAllowanceTransport {
    async fn execute(
        &self,
        client: reqwest::Client,
        _use_proxy: bool,
        request: AllowanceHttpRequest,
    ) -> Result<AllowanceHttpResponse, TransportFailure> {
        let mut builder = client
            .request(request.method, request.url)
            .headers(request.headers)
            .timeout(REQUEST_TIMEOUT);
        if !request.body.is_empty() {
            builder = builder.body(request.body);
        }
        let response = builder.send().await.map_err(|error| {
            if error.is_timeout() {
                TransportFailure::Timeout
            } else {
                TransportFailure::Unavailable
            }
        })?;
        let status = response.status();
        let headers = response.headers().clone();
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(TransportFailure::InvalidResponse);
        }
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| {
                if error.is_timeout() {
                    TransportFailure::Timeout
                } else {
                    TransportFailure::Unavailable
                }
            })?;
            if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(TransportFailure::InvalidResponse);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(AllowanceHttpResponse {
            status,
            headers,
            body,
        })
    }
}

impl AdminService {
    pub async fn list_provider_allowances(&self) -> anyhow::Result<Vec<ProviderAllowanceSnapshot>> {
        list_provider_allowances_with_transport(self, false, Arc::new(ReqwestAllowanceTransport))
            .await
    }

    pub async fn refresh_provider_allowances(
        &self,
    ) -> anyhow::Result<Vec<ProviderAllowanceSnapshot>> {
        list_provider_allowances_with_transport(self, true, Arc::new(ReqwestAllowanceTransport))
            .await
    }

    pub async fn refresh_provider_allowance(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<Option<ProviderAllowanceSnapshot>> {
        refresh_provider_allowance_with_transport(
            self,
            provider_id,
            Arc::new(ReqwestAllowanceTransport),
        )
        .await
    }
}

pub(super) async fn refresh_provider_allowance_with_transport(
    admin: &AdminService,
    provider_id: &str,
    transport: Arc<dyn AllowanceTransport>,
) -> anyhow::Result<Option<ProviderAllowanceSnapshot>> {
    let Some(provider) = admin.gw.storage.providers().get(provider_id).await? else {
        return Ok(None);
    };
    if !eligible_monitor()(&provider) {
        admin
            .gw
            .provider_allowance_state
            .inner
            .cache
            .write()
            .await
            .remove(provider_id);
        return Ok(None);
    }
    fetch_provider_allowance(admin, provider, true, transport).await
}

pub(super) async fn list_provider_allowances_with_transport(
    admin: &AdminService,
    force: bool,
    transport: Arc<dyn AllowanceTransport>,
) -> anyhow::Result<Vec<ProviderAllowanceSnapshot>> {
    let mut providers = admin.gw.storage.providers().list().await?;
    providers.retain(eligible_monitor());
    providers.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.id.cmp(&right.id))
    });

    let eligible_ids = providers
        .iter()
        .map(|provider| provider.id.clone())
        .collect::<HashSet<_>>();
    admin
        .gw
        .provider_allowance_state
        .inner
        .cache
        .write()
        .await
        .retain(|provider_id, _| eligible_ids.contains(provider_id));

    let results = stream::iter(providers.into_iter().map(|provider| {
        let admin = admin.clone();
        let transport = Arc::clone(&transport);
        async move {
            let provider_for_error = provider.clone();
            match fetch_provider_allowance(&admin, provider, force, transport).await {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    tracing::error!(
                        provider_id = %provider_for_error.id,
                        error = %error,
                        "Failed to revalidate provider allowance state"
                    );
                    Some(error_snapshot(
                        &provider_for_error,
                        safe_error(ProviderAllowanceErrorCategory::UpstreamUnavailable),
                    ))
                }
            }
        }
    }))
    .buffered(MAX_PARALLEL_REFRESHES)
    .collect::<Vec<_>>()
    .await;

    Ok(results.into_iter().flatten().collect())
}

fn eligible_monitor() -> impl FnMut(&Provider) -> bool {
    |provider| {
        if !provider.is_enabled {
            return false;
        }
        let Some(preset_key) = provider.preset_key.as_deref() else {
            return false;
        };
        let channel = provider.channel.as_deref().unwrap_or("default");
        monitor_for(preset_key, channel).is_some()
    }
}

async fn fetch_provider_allowance(
    admin: &AdminService,
    provider: Provider,
    force: bool,
    transport: Arc<dyn AllowanceTransport>,
) -> anyhow::Result<Option<ProviderAllowanceSnapshot>> {
    let Some(monitor) = provider.preset_key.as_deref().and_then(|preset_key| {
        monitor_for(preset_key, provider.channel.as_deref().unwrap_or("default"))
    }) else {
        return Ok(None);
    };
    let identity = provider_identity(admin, &provider).await?;
    let previous = {
        let cache = admin.gw.provider_allowance_state.inner.cache.read().await;
        cache
            .get(&provider.id)
            .filter(|entry| entry.identity == identity)
            .cloned()
    };
    if !force
        && let Some(entry) = previous.as_ref()
        && entry
            .successful_at
            .is_some_and(|successful_at| successful_at.elapsed() < SUCCESS_TTL)
    {
        return Ok(Some(entry.snapshot.clone()));
    }

    let inflight_key = format!("{}:{identity}", provider.id);
    let shared_fetch = {
        let mut inflight = admin
            .gw
            .provider_allowance_state
            .inner
            .inflight
            .lock()
            .await;
        if let Some(fetch) = inflight.get(&inflight_key) {
            #[cfg(test)]
            {
                admin
                    .gw
                    .provider_allowance_state
                    .inner
                    .coalesced_fetches
                    .fetch_add(1, Ordering::SeqCst);
                admin
                    .gw
                    .provider_allowance_state
                    .inner
                    .coalesced_fetch
                    .notify_waiters();
            }
            fetch.clone()
        } else {
            let admin = admin.clone();
            let state = admin.gw.provider_allowance_state.clone();
            let identity_for_future = identity.clone();
            let provider_id = provider.id.clone();
            let inflight_key_for_future = inflight_key.clone();
            let fetch = async move {
                let result: anyhow::Result<Option<ProviderAllowanceSnapshot>> = async {
                    let Some(current) = admin.gw.storage.providers().get(&provider_id).await?
                    else {
                        return Ok(None);
                    };
                    if !eligible_monitor()(&current)
                        || provider_identity(&admin, &current).await? != identity_for_future
                    {
                        return Ok(None);
                    }

                    let snapshot = fetch_uncached(
                        &admin,
                        &current,
                        monitor,
                        previous.as_ref().map(|entry| &entry.snapshot),
                        transport,
                    )
                    .await;

                    let Some(latest) = admin.gw.storage.providers().get(&provider_id).await? else {
                        return Ok(None);
                    };
                    if !eligible_monitor()(&latest)
                        || provider_identity(&admin, &latest).await? != identity_for_future
                    {
                        return Ok(None);
                    }

                    let successful_at =
                        (snapshot.status == ProviderAllowanceStatus::Fresh).then(Instant::now);
                    state.inner.cache.write().await.insert(
                        provider_id,
                        CacheEntry {
                            identity: identity_for_future,
                            snapshot: snapshot.clone(),
                            successful_at,
                        },
                    );
                    Ok(Some(snapshot))
                }
                .await;
                state
                    .inner
                    .inflight
                    .lock()
                    .await
                    .remove(&inflight_key_for_future);
                result.map_err(Arc::new)
            }
            .boxed()
            .shared();
            inflight.insert(inflight_key.clone(), fetch.clone());
            fetch
        }
    };

    shared_fetch
        .await
        .map_err(|error| anyhow::anyhow!("provider allowance revalidation failed: {error:#}"))
}

async fn provider_identity(admin: &AdminService, provider: &Provider) -> anyhow::Result<String> {
    let mut digest = Sha256::new();
    for value in [
        provider.id.as_str(),
        provider.name.as_str(),
        provider.preset_key.as_deref().unwrap_or_default(),
        provider.channel.as_deref().unwrap_or("default"),
        provider.api_key.as_str(),
        provider.adapter_credentials.as_str(),
        provider.auth_mode.as_str(),
        if provider.use_proxy {
            "proxy"
        } else {
            "direct"
        },
        provider.updated_at.as_str(),
    ] {
        digest.update(value.as_bytes());
        digest.update([0]);
    }
    if provider.effective_auth_mode().trim() == "oauth"
        && let Some(credential) = admin
            .gw
            .storage
            .oauth_credentials()
            .get(&provider.id)
            .await?
    {
        digest.update(credential.connection_id.as_bytes());
        digest.update([0]);
    }
    Ok(URL_SAFE_NO_PAD.encode(digest.finalize()))
}

async fn fetch_uncached(
    admin: &AdminService,
    provider: &Provider,
    monitor: MonitorKind,
    previous: Option<&ProviderAllowanceSnapshot>,
    transport: Arc<dyn AllowanceTransport>,
) -> ProviderAllowanceSnapshot {
    let result = async {
        let runtime = admin
            .resolve_provider_runtime(provider)
            .await
            .map_err(|_| safe_error(ProviderAllowanceErrorCategory::Authentication))?;
        if runtime.access_token.trim().is_empty() {
            return Err(safe_error(ProviderAllowanceErrorCategory::Authentication));
        }
        let client = admin
            .gw
            .http_client_for_provider(provider.use_proxy)
            .await
            .map_err(|_| safe_error(ProviderAllowanceErrorCategory::UpstreamUnavailable))?;
        fetch_monitor(
            monitor,
            provider.use_proxy,
            runtime.access_token,
            runtime.binding.extra_headers,
            client,
            transport,
        )
        .await
    }
    .await;

    match result {
        Ok(parsed) => ProviderAllowanceSnapshot {
            provider_id: provider.id.clone(),
            provider_name: provider.name.clone(),
            catalog_provider_id: provider.preset_key.clone().unwrap_or_default(),
            channel: provider.channel.clone().unwrap_or_else(|| "default".into()),
            plan_label: parsed.plan_label,
            status: ProviderAllowanceStatus::Fresh,
            fetched_at: Some(chrono::Utc::now().to_rfc3339()),
            allowances: parsed.allowances,
            models: parsed.models,
            error: None,
        },
        Err(error) => stale_or_error_snapshot(provider, previous, error),
    }
}

pub(super) async fn fetch_monitor(
    monitor: MonitorKind,
    use_proxy: bool,
    credential: String,
    extra_headers: HashMap<String, String>,
    client: reqwest::Client,
    transport: Arc<dyn AllowanceTransport>,
) -> Result<ParsedAllowance, ProviderAllowanceError> {
    let requests = monitor_requests(monitor, &credential, &extra_headers)?;
    let request_count = requests.len();
    for (index, request) in requests.into_iter().enumerate() {
        let response = transport
            .execute(client.clone(), use_proxy, request)
            .await
            .map_err(|failure| {
                safe_error(match failure {
                    TransportFailure::Timeout => ProviderAllowanceErrorCategory::Timeout,
                    TransportFailure::Unavailable => {
                        ProviderAllowanceErrorCategory::UpstreamUnavailable
                    }
                    TransportFailure::InvalidResponse => {
                        ProviderAllowanceErrorCategory::InvalidResponse
                    }
                })
            })?;
        if !response.status.is_success() {
            if request_count > 1 && index == 0 && response.status == StatusCode::NOT_FOUND {
                continue;
            }
            return Err(error_for_status(response.status));
        }
        if monitor == MonitorKind::XaiGrok
            && let Some(error) = grpc_status_error(&response.headers)
        {
            return Err(error);
        }
        let parsed = if request_count > 1 && index > 0 {
            parse_minimax_fallback(&response.body)
        } else {
            parse_monitor_response(monitor, &response.body)
        };
        match parsed {
            Ok(parsed) => return Ok(parsed),
            Err(_) if request_count > 1 && index == 0 => continue,
            Err(_) => {
                return Err(safe_error(ProviderAllowanceErrorCategory::InvalidResponse));
            }
        }
    }
    Err(safe_error(ProviderAllowanceErrorCategory::InvalidResponse))
}

pub(super) fn monitor_requests(
    monitor: MonitorKind,
    credential: &str,
    extra_headers: &HashMap<String, String>,
) -> Result<Vec<AllowanceHttpRequest>, ProviderAllowanceError> {
    let (method, urls, body) = match monitor {
        MonitorKind::AnthropicClaudeCode => (
            Method::GET,
            vec!["https://api.anthropic.com/api/oauth/usage"],
            Vec::new(),
        ),
        MonitorKind::OpenAiCodex => (
            Method::GET,
            vec!["https://chatgpt.com/backend-api/wham/usage"],
            Vec::new(),
        ),
        MonitorKind::GitHubCopilot => (
            Method::GET,
            vec!["https://api.github.com/copilot_internal/user"],
            Vec::new(),
        ),
        MonitorKind::KimiForCoding => (
            Method::GET,
            vec!["https://api.kimi.com/coding/v1/usages"],
            Vec::new(),
        ),
        MonitorKind::NanoGpt => (
            Method::GET,
            vec!["https://nano-gpt.com/api/subscription/v1/usage"],
            Vec::new(),
        ),
        MonitorKind::ZaiCodingPlan => (
            Method::GET,
            vec!["https://api.z.ai/api/monitor/usage/quota/limit"],
            Vec::new(),
        ),
        MonitorKind::ZhipuAiCodingPlan => (
            Method::GET,
            vec!["https://open.bigmodel.cn/api/monitor/usage/quota/limit"],
            Vec::new(),
        ),
        MonitorKind::MiniMaxCodingPlan => (
            Method::GET,
            vec![
                "https://api.minimax.io/v1/token_plan/remains",
                "https://api.minimax.io/v1/api/openplatform/coding_plan/remains",
            ],
            Vec::new(),
        ),
        MonitorKind::MiniMaxCnCodingPlan => (
            Method::GET,
            vec![
                "https://api.minimaxi.com/v1/token_plan/remains",
                "https://www.minimaxi.com/v1/api/openplatform/coding_plan/remains",
            ],
            Vec::new(),
        ),
        MonitorKind::Wafer => (
            Method::GET,
            vec!["https://pass.wafer.ai/v1/inference/quota"],
            Vec::new(),
        ),
        MonitorKind::OpenCodeGo => (
            Method::GET,
            vec!["https://opencode.ai/zen/go/v1/usage"],
            Vec::new(),
        ),
        MonitorKind::Crof => (Method::GET, vec!["https://crof.ai/usage_api/"], Vec::new()),
        MonitorKind::DeepSeek => (
            Method::GET,
            vec!["https://api.deepseek.com/user/balance"],
            Vec::new(),
        ),
        MonitorKind::NeuralWatt => (
            Method::GET,
            vec!["https://api.neuralwatt.com/v1/quota"],
            Vec::new(),
        ),
        MonitorKind::XaiGrok => (
            Method::POST,
            vec!["https://grok.com/grok_api_v2.GrokBuildBilling/GetGrokCreditsConfig"],
            vec![0, 0, 0, 0, 0],
        ),
    };

    let mut requests = Vec::with_capacity(urls.len());
    for url in urls {
        let mut headers = HeaderMap::new();
        let authorization = if monitor == MonitorKind::GitHubCopilot {
            format!("token {credential}")
        } else {
            format!("Bearer {credential}")
        };
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&authorization)
                .map_err(|_| safe_error(ProviderAllowanceErrorCategory::Authentication))?,
        );
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));

        match monitor {
            MonitorKind::AnthropicClaudeCode => {
                headers.insert(
                    HeaderName::from_static("anthropic-beta"),
                    HeaderValue::from_static("oauth-2025-04-20"),
                );
            }
            MonitorKind::OpenAiCodex => {
                if let Some(account_id) = extra_headers.get("chatgpt-account-id") {
                    headers.insert(
                        HeaderName::from_static("chatgpt-account-id"),
                        HeaderValue::from_str(account_id).map_err(|_| {
                            safe_error(ProviderAllowanceErrorCategory::Authentication)
                        })?,
                    );
                }
            }
            MonitorKind::GitHubCopilot => {
                headers.insert(
                    HeaderName::from_static("editor-version"),
                    HeaderValue::from_static("vscode/1.96.2"),
                );
                headers.insert(
                    HeaderName::from_static("x-github-api-version"),
                    HeaderValue::from_static("2025-04-01"),
                );
            }
            MonitorKind::OpenCodeGo => {
                headers.insert(USER_AGENT, HeaderValue::from_static("Stravia"));
            }
            MonitorKind::XaiGrok => {
                headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
                headers.insert(
                    CONTENT_TYPE,
                    HeaderValue::from_static("application/grpc-web+proto"),
                );
                headers.insert(ORIGIN, HeaderValue::from_static("https://grok.com"));
                headers.insert(
                    REFERER,
                    HeaderValue::from_static("https://grok.com/?_s=usage"),
                );
                headers.insert(
                    HeaderName::from_static("x-grpc-web"),
                    HeaderValue::from_static("1"),
                );
                headers.insert(
                    HeaderName::from_static("x-user-agent"),
                    HeaderValue::from_static("connect-es/2.1.1"),
                );
                headers.insert(USER_AGENT, HeaderValue::from_static("Stravia"));
            }
            _ => {}
        }
        requests.push(AllowanceHttpRequest {
            method: method.clone(),
            url,
            headers,
            body: body.clone(),
        });
    }
    Ok(requests)
}

fn grpc_status_error(headers: &HeaderMap) -> Option<ProviderAllowanceError> {
    let status = headers
        .get("grpc-status")?
        .to_str()
        .ok()?
        .parse::<u16>()
        .ok()?;
    if status == 0 {
        return None;
    }
    Some(safe_error(match status {
        16 => ProviderAllowanceErrorCategory::Authentication,
        8 => ProviderAllowanceErrorCategory::RateLimited,
        4 => ProviderAllowanceErrorCategory::Timeout,
        _ => ProviderAllowanceErrorCategory::UpstreamUnavailable,
    }))
}

fn error_for_status(status: StatusCode) -> ProviderAllowanceError {
    safe_error(match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            ProviderAllowanceErrorCategory::Authentication
        }
        StatusCode::TOO_MANY_REQUESTS => ProviderAllowanceErrorCategory::RateLimited,
        StatusCode::REQUEST_TIMEOUT => ProviderAllowanceErrorCategory::Timeout,
        _ => ProviderAllowanceErrorCategory::UpstreamUnavailable,
    })
}

fn safe_error(category: ProviderAllowanceErrorCategory) -> ProviderAllowanceError {
    let message = match category {
        ProviderAllowanceErrorCategory::Authentication => {
            "Authentication failed. Reconnect this provider or update its credential."
        }
        ProviderAllowanceErrorCategory::RateLimited => {
            "The allowance service is rate limited. Try again later."
        }
        ProviderAllowanceErrorCategory::Timeout => {
            "The allowance service timed out. Try again later."
        }
        ProviderAllowanceErrorCategory::UpstreamUnavailable => {
            "The allowance service is unavailable. Try again later."
        }
        ProviderAllowanceErrorCategory::InvalidResponse => {
            "The allowance service returned an unsupported response."
        }
    };
    ProviderAllowanceError {
        category,
        message: message.into(),
    }
}

fn error_snapshot(provider: &Provider, error: ProviderAllowanceError) -> ProviderAllowanceSnapshot {
    ProviderAllowanceSnapshot {
        provider_id: provider.id.clone(),
        provider_name: provider.name.clone(),
        catalog_provider_id: provider.preset_key.clone().unwrap_or_default(),
        channel: provider.channel.clone().unwrap_or_else(|| "default".into()),
        plan_label: None,
        status: ProviderAllowanceStatus::Error,
        fetched_at: None,
        allowances: Vec::new(),
        models: Vec::new(),
        error: Some(error),
    }
}

fn stale_or_error_snapshot(
    provider: &Provider,
    previous: Option<&ProviderAllowanceSnapshot>,
    error: ProviderAllowanceError,
) -> ProviderAllowanceSnapshot {
    let Some(previous) = previous.filter(|snapshot| snapshot.fetched_at.is_some()) else {
        return error_snapshot(provider, error);
    };
    ProviderAllowanceSnapshot {
        provider_id: provider.id.clone(),
        provider_name: provider.name.clone(),
        catalog_provider_id: provider.preset_key.clone().unwrap_or_default(),
        channel: provider.channel.clone().unwrap_or_else(|| "default".into()),
        plan_label: previous.plan_label.clone(),
        status: ProviderAllowanceStatus::Stale,
        fetched_at: previous.fetched_at.clone(),
        allowances: previous.allowances.clone(),
        models: previous.models.clone(),
        error: Some(error),
    }
}
