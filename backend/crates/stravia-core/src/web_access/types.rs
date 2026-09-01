use super::*;

pub const WEB_SEARCH_NAME: &str = "web_search";
pub const WEB_FETCH_NAME: &str = "web_fetch";
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

pub const MAX_FETCH_TOTAL_CHARACTERS: usize = 64_000;

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
