//! Core Web Access 与 Provider 实现之间的传输无关契约。
//!
//! 请求和结果由 core 作为 Internal Web Access wire contract 回导出；适配器只实现本
//! crate 的 trait，不依赖 Gateway、HTTP 路由或桌面运行时。

use serde::{Deserialize, Serialize};

/// 未指定时每次搜索最多返回的结果数。
pub const DEFAULT_SEARCH_RESULTS: usize = 5;
/// 未指定时每次页面访问最多请求的字符数。
pub const DEFAULT_FETCH_CHARACTERS: usize = 8_000;

/// 一次统一 Web Access 搜索请求。
///
/// Core 在调用适配器前负责验证查询、结果上限和域名规则。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SearchRequest {
    pub query: String,
    #[serde(default = "default_search_results")]
    pub max_results: usize,
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    #[serde(default)]
    pub blocked_domains: Vec<String>,
}

fn default_search_results() -> usize {
    DEFAULT_SEARCH_RESULTS
}

/// Provider 返回搜索结果的方式。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    Index,
    Agentic,
}

/// 一个规范化搜索结果。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SearchResult {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

/// Provider 答案引用的来源。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Citation {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// 一次规范化搜索响应。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SearchResponse {
    pub mode: SearchMode,
    pub query: String,
    pub results: Vec<SearchResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub citations: Option<Vec<Citation>>,
}

/// 一次统一 Web Access 页面访问请求。
///
/// Core 在调用适配器前负责 URL 与 SSRF 校验，并在响应后执行公平总量限制。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FetchRequest {
    pub urls: Vec<String>,
    #[serde(default = "default_fetch_characters")]
    pub max_characters: usize,
}

fn default_fetch_characters() -> usize {
    DEFAULT_FETCH_CHARACTERS
}

/// 单个页面访问结果的终态。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FetchStatus {
    Success,
    Error,
}

/// 可安全返回给 Internal Web Access 调用者的错误。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct WebAccessPublicError {
    pub code: WebAccessErrorCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Web Access 跨 Provider 使用的稳定错误分类。
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WebAccessErrorCode {
    InvalidInput,
    Disabled,
    Unsupported,
    Timeout,
    RateLimited,
    Unavailable,
}

/// 单个 URL 的规范化页面访问结果。
///
/// `limitations` 描述弱提取等非致命限制；`error` 仅在 `status=error` 时存在。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct FetchResult {
    pub url: String,
    pub status: FetchStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub limitations: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<WebAccessPublicError>,
}

/// 适配器整体调用失败；Engine 可据此决定是否 failover。
#[derive(Debug, Clone)]
pub struct ProviderFailure {
    pub code: WebAccessErrorCode,
    pub message: String,
}

impl ProviderFailure {
    /// 创建指定稳定错误分类的 Provider 失败。
    pub fn new(code: WebAccessErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// 创建可由后续 Provider 接管的不可用错误。
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(WebAccessErrorCode::Unavailable, message)
    }
}

/// Provider 原生计量信息；仅保留允许的数值字段。
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub credits: Option<f64>,
    pub cost: Option<f64>,
}

impl ProviderUsage {
    /// 从 Provider payload 的 `usage` 对象读取白名单数值字段。
    ///
    /// 缺少全部已知字段时返回 `None`；不会保留查询、URL 或正文。
    pub fn from_payload(payload: &serde_json::Value) -> Option<Self> {
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

/// 适配器成功结果及可选的 Provider 原生计量信息。
pub struct AdapterSuccess<T> {
    pub result: T,
    pub native_usage: Option<ProviderUsage>,
}

impl<T> AdapterSuccess<T> {
    /// 包装一次成功调用。
    pub fn new(result: T, native_usage: Option<ProviderUsage>) -> Self {
        Self {
            result,
            native_usage,
        }
    }
}

#[async_trait::async_trait]
/// Local、Exa 与 Zhipu 共用的单一适配器接口。
///
/// 整体错误通过 [`ProviderFailure`] 返回；Fetch 的逐 URL 错误留在 [`FetchResult`] 中，
/// 供 Engine 仅重试失败 URL。
pub trait WebProviderAdapter: Send + Sync {
    /// 返回日志与诊断使用的稳定 Provider id。
    fn provider_id(&self) -> &str {
        "anonymous"
    }

    /// 该适配器是否可参与搜索列表。
    fn supports_search(&self) -> bool;
    /// 该适配器是否可参与页面访问列表。
    fn supports_fetch(&self) -> bool;

    /// 执行搜索；整体失败允许 Engine 尝试下一 Provider。
    async fn search(
        &self,
        request: &SearchRequest,
    ) -> Result<AdapterSuccess<SearchResponse>, ProviderFailure>;

    /// 访问页面；应为每个输入 URL 返回一个结果，逐 URL 失败不应变成整体失败。
    async fn fetch(
        &self,
        request: &FetchRequest,
    ) -> Result<AdapterSuccess<Vec<FetchResult>>, ProviderFailure>;
}

/// 构造一个不含页面内容的逐 URL 失败结果。
pub fn failed_fetch_result(
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
        limitations: Vec::new(),
        error: Some(WebAccessPublicError { code, message }),
    }
}
