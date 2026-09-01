use super::policy::{apply_domain_filters, validate_fetch_request, validate_search_request};
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

    pub(super) fn has_search_providers(&self) -> bool {
        !self.search_providers.is_empty()
    }

    pub(super) fn has_fetch_providers(&self) -> bool {
        !self.fetch_providers.is_empty()
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
