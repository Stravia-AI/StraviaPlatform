use super::*;

pub const WEB_SEARCH_NAME: &str = "web_search";
pub const WEB_FETCH_NAME: &str = "web_fetch";
pub const DEFAULT_SEARCH_RESULTS: usize = 5;
pub(super) const WEB_ACCESS_DEADLINE: Duration = Duration::from_secs(60);
pub fn search_input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "minLength": 1, "maxLength": 2000 },
            "max_results": { "type": "integer", "minimum": 1, "maximum": 20, "default": 5 },
            "allowed_domains": {
                "type": "array",
                "maxItems": 20,
                "items": { "type": "string" },
                "default": []
            },
            "blocked_domains": {
                "type": "array",
                "maxItems": 20,
                "items": { "type": "string" },
                "default": []
            }
        },
        "required": ["query"],
        "additionalProperties": false
    })
}

pub fn fetch_input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "urls": {
                "type": "array",
                "minItems": 1,
                "maxItems": 20,
                "items": { "type": "string", "format": "uri" }
            },
            "max_characters": {
                "type": "integer",
                "minimum": 1000,
                "maximum": 50000,
                "default": 8000
            }
        },
        "required": ["urls"],
        "additionalProperties": false
    })
}

pub const DEFAULT_FETCH_CHARACTERS: usize = 8_000;
pub const MAX_FETCH_TOTAL_CHARACTERS: usize = 64_000;

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

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    Index,
    Agentic,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SearchResult {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Citation {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

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

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FetchStatus {
    Success,
    Error,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct WebAccessPublicError {
    pub code: WebAccessErrorCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<WebAccessPublicError>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct FetchResponse {
    pub results: Vec<FetchResult>,
}

impl FetchResponse {
    pub(crate) fn is_execution_error(&self) -> bool {
        !self.results.is_empty()
            && self
                .results
                .iter()
                .all(|result| result.status == FetchStatus::Error)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct WebAccessError {
    pub code: WebAccessErrorCode,
    pub message: String,
}

impl WebAccessError {
    pub(super) fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: WebAccessErrorCode::InvalidInput,
            message: message.into(),
        }
    }

    pub(super) fn from_code(code: WebAccessErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}
