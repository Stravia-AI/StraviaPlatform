pub(crate) mod config;
mod execution;
pub(crate) mod platform;

pub use config::{
    EligibleGenerationRoute, ImageGenerationConfig, MediaGenerationConfig,
    MediaGenerationConfigView, MediaGenerationValidation,
};

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct GenerationError {
    pub code: &'static str,
    pub message: String,
}

impl GenerationError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl From<anyhow::Error> for GenerationError {
    fn from(error: anyhow::Error) -> Self {
        tracing::warn!(error = %error, "Media generation configuration unavailable");
        Self::new(
            "media_generation_unavailable",
            "Media generation is unavailable",
        )
    }
}
