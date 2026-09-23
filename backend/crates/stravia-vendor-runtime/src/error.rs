use std::time::Duration;

use stravia_runtime_contract::protocol::ir::AiErrorKind;
use stravia_vendor_sdk::ErrorKind;

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("plugin is not a valid WebAssembly Component")]
    InvalidComponent(String),
    #[error("plugin imports forbidden interface `{0}`")]
    ForbiddenImport(String),
    #[error("plugin descriptor could not be read")]
    DescriptorExecution(String),
    #[error("plugin descriptor is not valid JSON")]
    DescriptorJson(#[source] serde_json::Error),
    #[error("plugin descriptor is invalid: {0}")]
    DescriptorInvalid(String),
    #[error("plugin canonical format {actual} is not supported (expected {expected})")]
    CanonicalFormat { actual: u32, expected: u32 },
}

/// Execution failures expose only host-owned summaries. Guest-provided error
/// messages and trap text remain available solely as redacted diagnostics in
/// the host implementation; they are never used as `safe_summary`.
#[derive(thiserror::Error)]
pub enum RuntimeError {
    #[error("{safe_summary}")]
    Plugin {
        kind: ErrorKind,
        safe_summary: &'static str,
        diagnostic_message: String,
        upstream_status: Option<u16>,
    },
    #[error("vendor operation was cancelled")]
    Cancelled,
    #[error("vendor operation exceeded its deadline")]
    DeadlineExceeded,
    #[error("vendor plugin exceeded a resource limit")]
    ResourceExhausted,
    #[error("vendor plugin execution failed")]
    Trapped,
    #[error("vendor plugin returned an invalid typed result")]
    InvalidOutput,
}

impl std::fmt::Debug for RuntimeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("RuntimeError")
            .field(&format_args!("{self}"))
            .finish()
    }
}

impl RuntimeError {
    pub fn from_guest(
        kind: ErrorKind,
        diagnostic_message: String,
        upstream_status: Option<u16>,
    ) -> Self {
        let safe_summary = match &kind {
            ErrorKind::Unsupported => "vendor operation is not supported",
            ErrorKind::Invalid => "vendor operation input was rejected",
            ErrorKind::Auth => "vendor authentication failed",
            ErrorKind::ContinuationNotFound => "vendor continuation is no longer available",
            ErrorKind::ProtectedReasoningRejected => "vendor rejected protected reasoning replay",
            ErrorKind::Upstream(_) => "vendor upstream request failed",
            ErrorKind::Trapped => "vendor plugin execution failed",
            ErrorKind::Cancelled => "vendor operation was cancelled",
            ErrorKind::DeadlineExceeded => "vendor operation exceeded its deadline",
            ErrorKind::ResourceExhausted => "vendor plugin exceeded a resource limit",
            ErrorKind::ProviderNotFound => "vendor provider profile is no longer available",
        };
        Self::Plugin {
            kind,
            safe_summary,
            diagnostic_message,
            upstream_status,
        }
    }

    /// Canonical model classification supplied by an upstream failure. Local
    /// lifecycle and validation failures always return `None`.
    pub fn model_error_kind(&self) -> Option<AiErrorKind> {
        match self {
            Self::Plugin { kind, .. } => kind.model_error_kind(),
            _ => None,
        }
    }

    /// Retry-After observed by the guest. This is policy input only; neither
    /// the SDK nor runtime sleeps or retries on its own.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Plugin { kind, .. } => kind.retry_after(),
            _ => None,
        }
    }

    pub fn upstream_status(&self) -> Option<u16> {
        match self {
            Self::Plugin {
                upstream_status, ..
            } => *upstream_status,
            _ => None,
        }
    }

    /// Untrusted guest text retained only for redacted diagnostics.
    pub fn diagnostic_message(&self) -> Option<&str> {
        match self {
            Self::Plugin {
                diagnostic_message, ..
            } => Some(diagnostic_message),
            _ => None,
        }
    }

    pub fn transport_failure(&self) -> Option<stravia_vendor_sdk::TransportFailure> {
        match self {
            Self::Plugin { kind, .. } => kind.transport_failure(),
            _ => None,
        }
    }

    pub fn is_upstream_failure(&self) -> bool {
        matches!(self, Self::Plugin { kind, .. } if kind.is_upstream_failure())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_diagnostics_are_not_exposed_by_error_formatting() {
        let secret = "untrusted-upstream-credential";
        let error = RuntimeError::from_guest(
            ErrorKind::upstream_unknown(),
            format!("invalid token: {secret}"),
            Some(403),
        );
        assert!(!error.to_string().contains(secret));
        assert!(!format!("{error:?}").contains(secret));
    }

    #[test]
    fn exposes_only_typed_upstream_retry_facts() {
        let error = RuntimeError::from_guest(
            ErrorKind::upstream_transport(
                Some(AiErrorKind::RateLimitError),
                Some(Duration::from_secs(11)),
                stravia_vendor_sdk::TransportFailure::Websocket,
            ),
            "rate limited".into(),
            Some(429),
        );
        assert!(error.is_upstream_failure());
        assert_eq!(error.model_error_kind(), Some(AiErrorKind::RateLimitError));
        assert_eq!(error.retry_after(), Some(Duration::from_secs(11)));
        assert_eq!(error.upstream_status(), Some(429));
        assert!(matches!(
            error.transport_failure(),
            Some(stravia_vendor_sdk::TransportFailure::Websocket)
        ));

        let trapped = RuntimeError::from_guest(
            ErrorKind::Trapped,
            "untrusted trap detail".into(),
            Some(503),
        );
        assert!(!trapped.is_upstream_failure());
        assert_eq!(trapped.model_error_kind(), None);
        assert_eq!(trapped.retry_after(), None);
    }
}
