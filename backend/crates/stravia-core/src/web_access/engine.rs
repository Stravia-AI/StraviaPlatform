use super::*;

fn search_failure_message(code: WebAccessErrorCode) -> &'static str {
    match code {
        WebAccessErrorCode::InvalidInput => "Web Search request is invalid",
        WebAccessErrorCode::Disabled => "Web Search is disabled",
        WebAccessErrorCode::Unsupported => "Web Search is unsupported",
        WebAccessErrorCode::Timeout => "Web Search timed out",
        WebAccessErrorCode::RateLimited => "Web Search is rate limited",
        WebAccessErrorCode::Unavailable => "Web Search is unavailable",
    }
}

pub(super) fn validate_search_request(
    mut request: SearchRequest,
) -> Result<SearchRequest, WebAccessError> {
    request.query = request.query.trim().to_string();
    if request.query.is_empty() {
        return Err(WebAccessError::invalid("query cannot be empty"));
    }
    if request.query.chars().count() > 2_000 {
        return Err(WebAccessError::invalid(
            "query cannot exceed 2,000 characters",
        ));
    }
    if !(1..=20).contains(&request.max_results) {
        return Err(WebAccessError::invalid(
            "max_results must be between 1 and 20",
        ));
    }
    if request.allowed_domains.len() > 20 || request.blocked_domains.len() > 20 {
        return Err(WebAccessError::invalid(
            "domain filters cannot contain more than 20 entries",
        ));
    }

    request.allowed_domains = normalize_domains(request.allowed_domains)?;
    request.blocked_domains = normalize_domains(request.blocked_domains)?;
    let blocked: HashSet<&str> = request.blocked_domains.iter().map(String::as_str).collect();
    if let Some(conflict) = request
        .allowed_domains
        .iter()
        .find(|domain| blocked.contains(domain.as_str()))
    {
        return Err(WebAccessError::invalid(format!(
            "domain appears in allowed_domains and blocked_domains: {conflict}"
        )));
    }
    Ok(request)
}

pub(crate) fn normalize_domains(domains: Vec<String>) -> Result<Vec<String>, WebAccessError> {
    let mut seen = HashSet::new();
    let mut normalized = Vec::with_capacity(domains.len());
    for domain in domains {
        let candidate = domain.trim();
        if candidate.is_empty()
            || candidate.contains('/')
            || candidate.contains('?')
            || candidate.contains('#')
            || candidate.contains('@')
            || candidate.contains(':')
        {
            return Err(WebAccessError::invalid(format!(
                "invalid domain filter: {domain}"
            )));
        }
        let parsed = reqwest::Url::parse(&format!("https://{candidate}/"))
            .map_err(|_| WebAccessError::invalid(format!("invalid domain filter: {domain}")))?;
        let hostname = parsed
            .host_str()
            .ok_or_else(|| WebAccessError::invalid(format!("invalid domain filter: {domain}")))?
            .trim_end_matches('.')
            .to_ascii_lowercase();
        if hostname.is_empty() || !seen.insert(hostname.clone()) {
            continue;
        }
        normalized.push(hostname);
    }
    Ok(normalized)
}

fn apply_domain_filters(request: &SearchRequest, response: &mut SearchResponse) {
    response.results.retain(|result| {
        url_matches_domain_filters(
            &result.url,
            &request.allowed_domains,
            &request.blocked_domains,
        )
    });
    if let Some(citations) = response.citations.as_mut() {
        citations.retain(|citation| {
            url_matches_domain_filters(
                &citation.url,
                &request.allowed_domains,
                &request.blocked_domains,
            )
        });
    }
}

fn url_matches_domain_filters(url: &str, allowed: &[String], blocked: &[String]) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    let Some(hostname) = parsed.host_str() else {
        return false;
    };
    let hostname = hostname.to_ascii_lowercase();
    if blocked
        .iter()
        .any(|domain| hostname == *domain || hostname.ends_with(&format!(".{domain}")))
    {
        return false;
    }
    allowed.is_empty()
        || allowed
            .iter()
            .any(|domain| hostname == *domain || hostname.ends_with(&format!(".{domain}")))
}

#[derive(Debug, Clone)]
pub(super) struct ProviderFailure {
    pub(super) code: WebAccessErrorCode,
    pub(super) message: String,
}

impl ProviderFailure {
    pub(super) fn new(code: WebAccessErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    #[cfg(test)]
    pub(super) fn unavailable(message: impl Into<String>) -> Self {
        Self::new(WebAccessErrorCode::Unavailable, message)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct ProviderUsage {
    pub(super) input_tokens: Option<u64>,
    pub(super) output_tokens: Option<u64>,
    pub(super) total_tokens: Option<u64>,
    pub(super) credits: Option<f64>,
    pub(super) cost: Option<f64>,
}

impl ProviderUsage {
    pub(super) fn from_payload(payload: &serde_json::Value) -> Option<Self> {
        let usage = payload.get("usage")?.as_object()?;
        let input_tokens = usage
            .get("input_tokens")
            .and_then(serde_json::Value::as_u64)
            .or_else(|| {
                usage
                    .get("prompt_tokens")
                    .and_then(serde_json::Value::as_u64)
            });
        let output_tokens = usage
            .get("output_tokens")
            .and_then(serde_json::Value::as_u64)
            .or_else(|| {
                usage
                    .get("completion_tokens")
                    .and_then(serde_json::Value::as_u64)
            });
        let total_tokens = usage
            .get("total_tokens")
            .and_then(serde_json::Value::as_u64);
        let credits = usage.get("credits").and_then(serde_json::Value::as_f64);
        let cost = usage.get("cost").and_then(serde_json::Value::as_f64);
        if input_tokens.is_none()
            && output_tokens.is_none()
            && total_tokens.is_none()
            && credits.is_none()
            && cost.is_none()
        {
            return None;
        }
        Some(Self {
            input_tokens,
            output_tokens,
            total_tokens,
            credits,
            cost,
        })
    }
}

/// Successful adapter output keeps provider-native usage private to Web Access
/// telemetry; only `result` crosses the engine's public seam.
pub(super) struct AdapterSuccess<T> {
    pub(super) result: T,
    pub(super) native_usage: Option<ProviderUsage>,
}

impl<T> AdapterSuccess<T> {
    pub(super) fn new(result: T, native_usage: Option<ProviderUsage>) -> Self {
        Self {
            result,
            native_usage,
        }
    }
}

#[async_trait::async_trait]
pub(super) trait WebProviderAdapter: Send + Sync {
    fn provider_id(&self) -> &str {
        "anonymous"
    }

    fn supports_search(&self) -> bool;
    fn supports_fetch(&self) -> bool;

    async fn search(
        &self,
        request: &SearchRequest,
    ) -> Result<AdapterSuccess<SearchResponse>, ProviderFailure>;

    async fn fetch(
        &self,
        request: &FetchRequest,
    ) -> Result<AdapterSuccess<Vec<FetchResult>>, ProviderFailure>;
}

#[derive(Clone)]
pub(super) struct WebAccessEngine {
    search_providers: Vec<Arc<dyn WebProviderAdapter>>,
    fetch_providers: Vec<Arc<dyn WebProviderAdapter>>,
    deadline: Duration,
}

impl WebAccessEngine {
    pub(super) fn new(
        search_providers: Vec<Arc<dyn WebProviderAdapter>>,
        fetch_providers: Vec<Arc<dyn WebProviderAdapter>>,
    ) -> Self {
        Self {
            search_providers,
            fetch_providers,
            deadline: WEB_ACCESS_DEADLINE,
        }
    }

    pub(super) async fn search(
        &self,
        request: SearchRequest,
    ) -> Result<SearchResponse, WebAccessError> {
        let request = validate_search_request(request)?;
        let deadline = tokio::time::Instant::now() + self.deadline;
        let mut last_failure = None;
        for (attempt_index, provider) in self.search_providers.iter().enumerate() {
            if !provider.supports_search() {
                continue;
            }
            let started = std::time::Instant::now();
            let attempt = tokio::time::timeout_at(deadline, provider.search(&request)).await;
            match attempt {
                Ok(Ok(AdapterSuccess {
                    result: mut response,
                    native_usage,
                })) => {
                    apply_domain_filters(&request, &mut response);
                    tracing::info!(
                        web_provider_id = provider.provider_id(),
                        attempt_index = attempt_index + 1,
                        attempt_latency_ms = started.elapsed().as_millis() as u64,
                        result_count = response.results.len(),
                        native_usage = ?native_usage,
                        outcome = "success",
                        "Web Search attempt completed"
                    );
                    return Ok(response);
                }
                Ok(Err(failure)) => {
                    tracing::warn!(
                        web_provider_id = provider.provider_id(),
                        attempt_index = attempt_index + 1,
                        attempt_latency_ms = started.elapsed().as_millis() as u64,
                        error_code = ?failure.code,
                        outcome = "failed",
                        "Web Search attempt failed"
                    );
                    last_failure = Some(failure);
                }
                Err(_) => {
                    tracing::warn!(
                        web_provider_id = provider.provider_id(),
                        attempt_index = attempt_index + 1,
                        attempt_latency_ms = started.elapsed().as_millis() as u64,
                        error_code = ?WebAccessErrorCode::Timeout,
                        outcome = "timeout",
                        "Web Search attempt timed out"
                    );
                    return Err(WebAccessError::from_code(
                        WebAccessErrorCode::Timeout,
                        "Web Search deadline exceeded",
                    ));
                }
            }
        }

        match last_failure {
            Some(failure) => Err(WebAccessError::from_code(
                failure.code,
                search_failure_message(failure.code),
            )),
            None => Err(WebAccessError::from_code(
                WebAccessErrorCode::Unsupported,
                "Web Search is unavailable",
            )),
        }
    }

    pub(super) async fn fetch(
        &self,
        request: FetchRequest,
    ) -> Result<FetchResponse, WebAccessError> {
        let deadline = tokio::time::Instant::now() + self.deadline;
        let request = tokio::time::timeout_at(deadline, validate_fetch_request(request))
            .await
            .map_err(|_| {
                WebAccessError::from_code(
                    WebAccessErrorCode::Timeout,
                    "Web Fetch deadline exceeded",
                )
            })??;
        if self.fetch_providers.is_empty() {
            return Err(WebAccessError::from_code(
                WebAccessErrorCode::Unsupported,
                "Web Fetch is unavailable",
            ));
        }

        let effective_limit = request
            .max_characters
            .min(MAX_FETCH_TOTAL_CHARACTERS / request.urls.len());
        let mut pending: Vec<usize> = (0..request.urls.len()).collect();
        let mut completed: Vec<Option<FetchResult>> = vec![None; request.urls.len()];

        for (attempt_index, provider) in self.fetch_providers.iter().enumerate() {
            if pending.is_empty() {
                break;
            }
            if !provider.supports_fetch() {
                continue;
            }
            let attempt_request = FetchRequest {
                urls: pending
                    .iter()
                    .map(|index| request.urls[*index].clone())
                    .collect(),
                max_characters: effective_limit,
            };
            let started = std::time::Instant::now();
            let attempt = tokio::time::timeout_at(deadline, provider.fetch(&attempt_request)).await;
            match attempt {
                Ok(Ok(AdapterSuccess {
                    result: results,
                    native_usage,
                })) if results.len() == pending.len() => {
                    let mut failed = Vec::new();
                    for (index, mut result) in pending.into_iter().zip(results) {
                        result.url = request.urls[index].clone();
                        if result.status == FetchStatus::Success {
                            enforce_content_limit(&mut result, effective_limit);
                            completed[index] = Some(result);
                        } else {
                            completed[index] = Some(result);
                            failed.push(index);
                        }
                    }
                    pending = failed;
                    tracing::info!(
                        web_provider_id = provider.provider_id(),
                        attempt_index = attempt_index + 1,
                        attempt_latency_ms = started.elapsed().as_millis() as u64,
                        remaining_urls = pending.len(),
                        native_usage = ?native_usage,
                        outcome = "completed",
                        "Web Fetch attempt completed"
                    );
                }
                Ok(Ok(_)) => {
                    tracing::warn!(
                        web_provider_id = provider.provider_id(),
                        attempt_index = attempt_index + 1,
                        error_code = ?WebAccessErrorCode::Unavailable,
                        outcome = "invalid_response",
                        "Web Fetch provider returned a mismatched result count"
                    );
                }
                Ok(Err(failure)) => {
                    tracing::warn!(
                        web_provider_id = provider.provider_id(),
                        attempt_index = attempt_index + 1,
                        attempt_latency_ms = started.elapsed().as_millis() as u64,
                        error_code = ?failure.code,
                        outcome = "failed",
                        "Web Fetch attempt failed"
                    );
                    for index in &pending {
                        completed[*index] = Some(failed_fetch_result(
                            request.urls[*index].clone(),
                            failure.code,
                            None,
                        ));
                    }
                }
                Err(_) => {
                    tracing::warn!(
                        web_provider_id = provider.provider_id(),
                        attempt_index = attempt_index + 1,
                        attempt_latency_ms = started.elapsed().as_millis() as u64,
                        error_code = ?WebAccessErrorCode::Timeout,
                        outcome = "timeout",
                        "Web Fetch attempt timed out"
                    );
                    for index in &pending {
                        completed[*index] = Some(failed_fetch_result(
                            request.urls[*index].clone(),
                            WebAccessErrorCode::Timeout,
                            Some("Web Fetch deadline exceeded".into()),
                        ));
                    }
                    break;
                }
            }
        }

        for index in pending {
            if completed[index].is_none() {
                completed[index] = Some(failed_fetch_result(
                    request.urls[index].clone(),
                    WebAccessErrorCode::Unavailable,
                    None,
                ));
            }
        }
        Ok(FetchResponse {
            results: completed
                .into_iter()
                .enumerate()
                .map(|(index, result)| {
                    result.unwrap_or_else(|| {
                        failed_fetch_result(
                            request.urls[index].clone(),
                            WebAccessErrorCode::Unavailable,
                            None,
                        )
                    })
                })
                .collect(),
        })
    }
}

pub(super) async fn validate_fetch_request(
    mut request: FetchRequest,
) -> Result<FetchRequest, WebAccessError> {
    if !(1..=20).contains(&request.urls.len()) {
        return Err(WebAccessError::invalid(
            "urls must contain between 1 and 20 entries",
        ));
    }
    if !(1_000..=50_000).contains(&request.max_characters) {
        return Err(WebAccessError::invalid(
            "max_characters must be between 1,000 and 50,000",
        ));
    }
    for value in &mut request.urls {
        *value = value.trim().to_string();
        let parsed = reqwest::Url::parse(value)
            .map_err(|_| WebAccessError::invalid(format!("invalid URL: {value}")))?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err(WebAccessError::invalid(format!(
                "URL must be public HTTP(S): {value}"
            )));
        }
        let hostname = parsed
            .host_str()
            .expect("host checked above")
            .trim_end_matches('.')
            .to_ascii_lowercase();
        if hostname.is_empty() {
            return Err(WebAccessError::invalid(format!(
                "URL must be public HTTP(S): {value}"
            )));
        }
        if hostname == "localhost"
            || hostname.ends_with(".localhost")
            || hostname.ends_with(".local")
            || hostname == "home.arpa"
            || hostname.ends_with(".home.arpa")
        {
            return Err(WebAccessError::invalid(format!(
                "URL must be public HTTP(S): {value}"
            )));
        }

        if let Ok(address) = hostname.parse::<std::net::IpAddr>() {
            if !is_public_ip(address) {
                return Err(WebAccessError::invalid(format!(
                    "URL must be public HTTP(S): {value}"
                )));
            }
            continue;
        }

        // Tokio's resolver runs through its async runtime rather than blocking
        // the request task. Every A/AAAA answer must be public; accepting any
        // private answer would let a DNS alias reach an internal service.
        let addresses = tokio::net::lookup_host((hostname.as_str(), 0))
            .await
            .map_err(|_| {
                WebAccessError::from_code(
                    WebAccessErrorCode::Unavailable,
                    format!("URL hostname could not be resolved: {hostname}"),
                )
            })?;
        let mut resolved_any = false;
        for address in addresses {
            resolved_any = true;
            if !is_public_ip(address.ip()) {
                return Err(WebAccessError::invalid(format!(
                    "URL must be public HTTP(S): {value}"
                )));
            }
        }
        if !resolved_any {
            return Err(WebAccessError::from_code(
                WebAccessErrorCode::Unavailable,
                format!("URL hostname could not be resolved: {hostname}"),
            ));
        }
    }
    Ok(request)
}

pub(crate) fn is_public_ip(address: std::net::IpAddr) -> bool {
    match address {
        std::net::IpAddr::V4(address) => is_public_ipv4(address),
        std::net::IpAddr::V6(address) => is_public_ipv6(address),
    }
}

fn is_public_ipv4(address: std::net::Ipv4Addr) -> bool {
    let octets = address.octets();
    !address.is_private()
        && !address.is_loopback()
        && !address.is_link_local()
        && !address.is_unspecified()
        && !address.is_multicast()
        && !address.is_broadcast()
        && !address.is_documentation()
        && !(octets[0] == 100 && (64..=127).contains(&octets[1]))
        && !(octets[0] == 192 && octets[1] == 0 && octets[2] == 0 && !matches!(octets[3], 9 | 10))
        && !(octets[0] == 192 && octets[1] == 88 && octets[2] == 99)
        && !(octets[0] == 198 && (octets[1] == 18 || octets[1] == 19))
        && octets[0] < 240
        && octets[0] != 0
}
fn is_public_ipv6(address: std::net::Ipv6Addr) -> bool {
    if let Some(address) = address.to_ipv4() {
        return is_public_ipv4(address);
    }
    let segments = address.segments();
    (0x2000..=0x3fff).contains(&segments[0])
        && !address.is_loopback()
        && !address.is_unspecified()
        && !address.is_multicast()
        && !address.is_unique_local()
        && !address.is_unicast_link_local()
        && !(segments[0] == 0x2001 && segments[1] == 0x0db8)
        && !(segments[0] == 0x2001 && segments[1] == 0)
        && !(segments[0] == 0x2001 && (segments[1] & 0xfff0) == 0x0010)
        && is_global_ipv6_special(&segments)
}

fn is_global_ipv6_special(segments: &[u16; 8]) -> bool {
    // IANA special-purpose ranges that are not globally reachable.
    // Well-known NAT64 64:ff9b::/96 and local-use 64:ff9b:1::/48.
    if (segments[0] == 0x0064
        && segments[1] == 0xff9b
        && segments[2] == 0
        && segments[3] == 0
        && segments[4] == 0
        && segments[5] == 0)
        || (segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2] == 1)
        // 6to4 transition addresses can embed private IPv4 destinations.
        || segments[0] == 0x2002
        // Discard-only 100::/64 and dummy 100:0:0:1::/64.
        || (segments[0] == 0x0100
            && segments[1] == 0
            && segments[2] == 0
            && (segments[3] == 0 || segments[3] == 1))
        || (segments[0] == 0x3fff && (segments[1] & 0xf000) == 0)
        || segments[0] == 0x5f00
        || (segments[0] & 0xffc0) == 0xfec0
    {
        return false;
    }

    // 2001::/23 is reserved for IETF assignments. Permit only the
    // specifically allocated globally reachable entries in that block.
    if segments[0] == 0x2001 && (segments[1] & 0xfe00) == 0 {
        return (segments[1] == 3)
            || (segments[1] == 4 && segments[2] == 0x0112)
            || (segments[1] == 1
                && segments[2..7].iter().all(|segment| *segment == 0)
                && matches!(segments[7], 1..=3))
            || (segments[1] & 0xfff0) == 0x0020
            || (segments[1] & 0xfff0) == 0x0030;
    }
    true
}

pub(super) fn failed_fetch_result(
    url: String,
    code: WebAccessErrorCode,
    message: Option<String>,
) -> FetchResult {
    FetchResult {
        url,
        status: FetchStatus::Error,
        content: None,
        format: None,
        title: None,
        truncated: false,
        error: Some(WebAccessPublicError { code, message }),
    }
}

fn enforce_content_limit(result: &mut FetchResult, limit: usize) {
    let Some(content) = result.content.as_mut() else {
        return;
    };
    if content.chars().count() <= limit {
        return;
    }
    *content = content.chars().take(limit).collect();
    result.truncated = true;
}

#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
pub struct WebAccessAvailability {
    pub search: bool,
    pub fetch: bool,
}

#[derive(Clone)]
pub(super) struct WebAccessRunSnapshot {
    api_key_id: String,
    engine: WebAccessEngine,
}

#[derive(Clone, Default)]
pub(crate) struct WebAccessRunSnapshotStore {
    snapshots: Arc<std::sync::Mutex<HashMap<String, WebAccessRunSnapshot>>>,
}

impl WebAccessRunSnapshotStore {
    fn insert(&self, run_id: String, snapshot: WebAccessRunSnapshot) {
        self.snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(run_id, snapshot);
    }

    fn get(&self, run_id: &str) -> Option<WebAccessRunSnapshot> {
        self.snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(run_id)
            .cloned()
    }

    fn remove(&self, run_id: &str) {
        self.snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(run_id);
    }
}

#[derive(Clone)]
pub struct WebAccessService {
    gateway: crate::Gateway,
}

impl WebAccessService {
    pub(crate) fn new(gateway: crate::Gateway) -> Self {
        Self { gateway }
    }

    pub async fn settings(&self) -> anyhow::Result<WebAccessSettings> {
        let Some(store) = self.gateway.storage.web_providers() else {
            return Ok(WebAccessSettings::default());
        };
        store.load_settings().await
    }

    pub(crate) async fn capture_run_snapshot(
        &self,
        run_id: &str,
        api_key_id: &str,
    ) -> anyhow::Result<WebAccessAvailability> {
        match tokio::time::timeout(
            WEB_ACCESS_DEADLINE,
            self.capture_run_snapshot_inner(run_id, api_key_id),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                self.release_run_snapshot(run_id);
                Err(anyhow::anyhow!("Web Access snapshot deadline exceeded"))
            }
        }
    }

    async fn capture_run_snapshot_inner(
        &self,
        run_id: &str,
        api_key_id: &str,
    ) -> anyhow::Result<WebAccessAvailability> {
        let (settings, providers, permissions) = self
            .runtime_config(api_key_id)
            .await
            .map_err(anyhow::Error::new)?;
        if !settings.enabled || !permissions.api_key_enabled {
            self.release_run_snapshot(run_id);
            return Ok(WebAccessAvailability::default());
        }
        let engine = self
            .engine(&settings, &providers)
            .await
            .map_err(anyhow::Error::new)?;
        let availability = WebAccessAvailability {
            search: !engine.search_providers.is_empty(),
            fetch: !engine.fetch_providers.is_empty(),
        };
        if availability.search || availability.fetch {
            self.gateway.web_access_run_snapshots.insert(
                run_id.to_string(),
                WebAccessRunSnapshot {
                    api_key_id: api_key_id.to_string(),
                    engine,
                },
            );
        } else {
            self.release_run_snapshot(run_id);
        }
        Ok(availability)
    }

    pub(crate) fn release_run_snapshot(&self, run_id: &str) {
        self.gateway.web_access_run_snapshots.remove(run_id);
    }

    pub(super) async fn search_in_run(
        &self,
        run_id: &str,
        api_key_id: &str,
        request: SearchRequest,
    ) -> Result<SearchResponse, WebAccessError> {
        let snapshot = self.run_snapshot(run_id, api_key_id)?;
        snapshot.engine.search(request).await
    }

    pub(super) async fn fetch_in_run(
        &self,
        run_id: &str,
        api_key_id: &str,
        request: FetchRequest,
    ) -> Result<FetchResponse, WebAccessError> {
        let snapshot = self.run_snapshot(run_id, api_key_id)?;
        snapshot.engine.fetch(request).await
    }

    pub(super) fn run_snapshot(
        &self,
        run_id: &str,
        api_key_id: &str,
    ) -> Result<WebAccessRunSnapshot, WebAccessError> {
        self.gateway
            .web_access_run_snapshots
            .get(run_id)
            .filter(|snapshot| snapshot.api_key_id == api_key_id)
            .ok_or_else(|| {
                WebAccessError::from_code(
                    WebAccessErrorCode::Unavailable,
                    "Web Access runtime snapshot is unavailable",
                )
            })
    }

    pub(crate) async fn test_provider(&self, provider: WebProvider) -> Result<(), WebAccessError> {
        tokio::time::timeout(WEB_ACCESS_DEADLINE, self.test_provider_inner(provider))
            .await
            .map_err(|_| {
                WebAccessError::from_code(
                    WebAccessErrorCode::Timeout,
                    "Web Provider connectivity test deadline exceeded",
                )
            })?
    }

    async fn test_provider_inner(&self, provider: WebProvider) -> Result<(), WebAccessError> {
        let adapter = self.adapter(&provider).await?;
        if adapter.supports_search() {
            adapter
                .search(&SearchRequest {
                    query: "Stravia connectivity test".into(),
                    max_results: 1,
                    allowed_domains: vec![],
                    blocked_domains: vec![],
                })
                .await
                .map_err(|failure| WebAccessError::from_code(failure.code, failure.message))?;
        }
        if adapter.supports_fetch() {
            let test_url = "https://example.com/";
            let response = adapter
                .fetch(&FetchRequest {
                    urls: vec![test_url.into()],
                    max_characters: 1_000,
                })
                .await
                .map_err(|failure| WebAccessError::from_code(failure.code, failure.message))?;
            let valid = response.result.len() == 1
                && response.result[0].url == test_url
                && response.result[0].status == FetchStatus::Success;
            if !valid {
                return Err(WebAccessError::from_code(
                    WebAccessErrorCode::Unavailable,
                    "Web Provider Fetch connectivity test returned an invalid result",
                ));
            }
        }
        Ok(())
    }

    async fn runtime_config(&self, api_key_id: &str) -> Result<RuntimeConfig, WebAccessError> {
        let Some(store) = self.gateway.storage.web_providers() else {
            return Ok((
                WebAccessSettings::default(),
                std::collections::HashMap::new(),
                WebAccessApiKeyPermissions::default(),
            ));
        };
        let config = store.load_runtime_config(api_key_id).await.map_err(|_| {
            WebAccessError::from_code(
                WebAccessErrorCode::Unavailable,
                "Web Access configuration is unavailable",
            )
        })?;
        Ok((
            config.settings,
            config
                .web_providers
                .into_iter()
                .map(|provider| (provider.id.clone(), provider))
                .collect(),
            config.api_key_permissions,
        ))
    }
    async fn engine(
        &self,
        settings: &WebAccessSettings,
        records: &std::collections::HashMap<String, WebProvider>,
    ) -> Result<WebAccessEngine, WebAccessError> {
        let search = self
            .ordered_adapters(&settings.search_provider_ids, records)
            .await;
        let fetch = self
            .ordered_adapters(&settings.fetch_provider_ids, records)
            .await;
        Ok(WebAccessEngine::new(search, fetch))
    }
    async fn ordered_adapters(
        &self,
        ids: &[String],
        records: &std::collections::HashMap<String, WebProvider>,
    ) -> Vec<Arc<dyn WebProviderAdapter>> {
        let mut adapters = Vec::with_capacity(ids.len());
        for id in ids {
            let Some(provider) = records.get(id) else {
                continue;
            };
            match self.adapter(provider).await {
                Ok(adapter) => adapters.push(adapter),
                Err(error) => adapters.push(Arc::new(UnavailableAdapter {
                    id: provider.id.clone(),
                    search: provider
                        .capabilities()
                        .is_some_and(|capability| capability.search),
                    fetch: provider
                        .capabilities()
                        .is_some_and(|capability| capability.fetch),
                    error,
                })),
            }
        }
        adapters
    }

    async fn adapter(
        &self,
        provider: &WebProvider,
    ) -> Result<Arc<dyn WebProviderAdapter>, WebAccessError> {
        use providers::AdapterConfig;
        let config = match provider.kind.as_str() {
            "exa" => AdapterConfig::Exa {
                id: provider.id.clone(),
                api_key: required_secret(provider)?,
            },
            "brave" => AdapterConfig::Brave {
                id: provider.id.clone(),
                api_key: required_secret(provider)?,
            },
            "tavily" => AdapterConfig::Tavily {
                id: provider.id.clone(),
                api_key: required_secret(provider)?,
            },
            "zhipu" => AdapterConfig::Zhipu {
                id: provider.id.clone(),
                api_key: required_secret(provider)?,
            },
            _ => {
                return Err(WebAccessError::from_code(
                    WebAccessErrorCode::Unsupported,
                    "unsupported Web Provider kind",
                ));
            }
        };
        Ok(providers::build_adapter(
            self.gateway.http_client.clone(),
            config,
        ))
    }
}

fn required_secret(provider: &WebProvider) -> Result<String, WebAccessError> {
    provider
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .ok_or_else(unavailable_configuration)
}

fn unavailable_configuration() -> WebAccessError {
    WebAccessError::from_code(
        WebAccessErrorCode::Unavailable,
        "Web Provider configuration is unavailable",
    )
}

struct UnavailableAdapter {
    id: String,
    search: bool,
    fetch: bool,
    error: WebAccessError,
}

#[async_trait::async_trait]
impl WebProviderAdapter for UnavailableAdapter {
    fn provider_id(&self) -> &str {
        &self.id
    }

    fn supports_search(&self) -> bool {
        self.search
    }

    fn supports_fetch(&self) -> bool {
        self.fetch
    }

    async fn search(
        &self,
        _request: &SearchRequest,
    ) -> Result<AdapterSuccess<SearchResponse>, ProviderFailure> {
        Err(ProviderFailure::new(
            self.error.code,
            self.error.message.clone(),
        ))
    }

    async fn fetch(
        &self,
        _request: &FetchRequest,
    ) -> Result<AdapterSuccess<Vec<FetchResult>>, ProviderFailure> {
        Err(ProviderFailure::new(
            self.error.code,
            self.error.message.clone(),
        ))
    }
}
