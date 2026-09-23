use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use stravia_runtime_contract::protocol::ir::{AiRequest, AiResponse, NativeCompactionResponse};

use crate::bindings::stravia::vendor::types;
use crate::descriptor::{Capability, VendorDescriptor};
use crate::envelope::{CANONICAL_FORMAT_VERSION, decode_payload, error};
use crate::host_api::GuestHost;

pub use crate::bindings::stravia::vendor::types::{
    ErrorKind, ModelErrorKind, PluginError, TransportFailure, UpstreamFailure,
};

impl ErrorKind {
    /// Construct an upstream failure while preserving optional canonical model
    /// semantics and Retry-After. The host, not the guest, decides whether and
    /// when to retry.
    pub fn upstream(
        model_error_kind: Option<stravia_runtime_contract::protocol::ir::AiErrorKind>,
        retry_after: Option<Duration>,
    ) -> Self {
        Self::Upstream(UpstreamFailure {
            model_error_kind: model_error_kind.map(Into::into),
            retry_after_milliseconds: retry_after
                .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)),
            transport_failure: None,
        })
    }

    /// Construct an upstream failure known to be specific to one transport.
    /// This is policy input for the host; guests must not replay the request.
    pub fn upstream_transport(
        model_error_kind: Option<stravia_runtime_contract::protocol::ir::AiErrorKind>,
        retry_after: Option<Duration>,
        transport_failure: TransportFailure,
    ) -> Self {
        let Self::Upstream(mut failure) = Self::upstream(model_error_kind, retry_after) else {
            unreachable!("upstream constructor always returns the upstream variant")
        };
        failure.transport_failure = Some(transport_failure);
        Self::Upstream(failure)
    }

    /// Upstream failure without a known canonical model classification.
    pub fn upstream_unknown() -> Self {
        Self::upstream(None, None)
    }

    pub fn model_error_kind(&self) -> Option<stravia_runtime_contract::protocol::ir::AiErrorKind> {
        match self {
            Self::Upstream(failure) => failure.model_error_kind.map(Into::into),
            _ => None,
        }
    }

    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Upstream(failure) => failure.retry_after_milliseconds.map(Duration::from_millis),
            _ => None,
        }
    }

    pub fn transport_failure(&self) -> Option<TransportFailure> {
        match self {
            Self::Upstream(failure) => failure.transport_failure,
            _ => None,
        }
    }

    pub fn is_upstream_failure(&self) -> bool {
        matches!(self, Self::Upstream(_))
    }
}

impl From<stravia_runtime_contract::protocol::ir::AiErrorKind> for ModelErrorKind {
    fn from(value: stravia_runtime_contract::protocol::ir::AiErrorKind) -> Self {
        use stravia_runtime_contract::protocol::ir::AiErrorKind;
        match value {
            AiErrorKind::AuthenticationError => Self::AuthenticationError,
            AiErrorKind::AuthorizationError => Self::AuthorizationError,
            AiErrorKind::NotFoundError => Self::NotFoundError,
            AiErrorKind::RateLimitError => Self::RateLimitError,
            AiErrorKind::QuotaExceeded => Self::QuotaExceeded,
            AiErrorKind::InvalidRequest => Self::InvalidRequest,
            AiErrorKind::ServerError => Self::ServerError,
            AiErrorKind::ServiceUnavailable => Self::ServiceUnavailable,
            AiErrorKind::Timeout => Self::Timeout,
            AiErrorKind::ContentFiltered => Self::ContentFiltered,
            AiErrorKind::ContextLengthExceeded => Self::ContextLengthExceeded,
            AiErrorKind::ModelNotAvailable => Self::ModelNotAvailable,
            AiErrorKind::StreamMidError => Self::StreamMidError,
            AiErrorKind::UnexpectedEof => Self::UnexpectedEof,
            AiErrorKind::Unknown => Self::Unknown,
        }
    }
}

impl From<ModelErrorKind> for stravia_runtime_contract::protocol::ir::AiErrorKind {
    fn from(value: ModelErrorKind) -> Self {
        match value {
            ModelErrorKind::AuthenticationError => Self::AuthenticationError,
            ModelErrorKind::AuthorizationError => Self::AuthorizationError,
            ModelErrorKind::NotFoundError => Self::NotFoundError,
            ModelErrorKind::RateLimitError => Self::RateLimitError,
            ModelErrorKind::QuotaExceeded => Self::QuotaExceeded,
            ModelErrorKind::InvalidRequest => Self::InvalidRequest,
            ModelErrorKind::ServerError => Self::ServerError,
            ModelErrorKind::ServiceUnavailable => Self::ServiceUnavailable,
            ModelErrorKind::Timeout => Self::Timeout,
            ModelErrorKind::ContentFiltered => Self::ContentFiltered,
            ModelErrorKind::ContextLengthExceeded => Self::ContextLengthExceeded,
            ModelErrorKind::ModelNotAvailable => Self::ModelNotAvailable,
            ModelErrorKind::StreamMidError => Self::StreamMidError,
            ModelErrorKind::UnexpectedEof => Self::UnexpectedEof,
            ModelErrorKind::Unknown => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Infer,
    Compact,
    Search,
    MediaImage,
    Auth,
    Discover,
    Allowance,
    ConfigValidation,
}

impl Operation {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Infer => "infer",
            Self::Compact => "compact",
            Self::Search => "search",
            Self::MediaImage => "media_image",
            Self::Auth => "auth",
            Self::Discover => "discover",
            Self::Allowance => "allowance",
            Self::ConfigValidation => "config_validation",
        }
    }

    pub const fn required_capability(self) -> Capability {
        match self {
            Self::Infer => Capability::Infer,
            Self::Compact => Capability::Compact,
            Self::Search => Capability::Search,
            Self::MediaImage => Capability::MediaImage,
            Self::Auth => Capability::AuthOauth,
            Self::Discover => Capability::ModelDiscovery,
            Self::Allowance => Capability::Allowance,
            Self::ConfigValidation => Capability::ConfigValidation,
        }
    }
}

impl From<types::OperationKind> for Operation {
    fn from(value: types::OperationKind) -> Self {
        match value {
            types::OperationKind::Infer => Self::Infer,
            types::OperationKind::Compact => Self::Compact,
            types::OperationKind::Search => Self::Search,
            types::OperationKind::MediaImage => Self::MediaImage,
            types::OperationKind::Auth => Self::Auth,
            types::OperationKind::Discover => Self::Discover,
            types::OperationKind::Allowance => Self::Allowance,
            types::OperationKind::ConfigValidation => Self::ConfigValidation,
        }
    }
}

impl From<Operation> for types::OperationKind {
    fn from(value: Operation) -> Self {
        match value {
            Operation::Infer => Self::Infer,
            Operation::Compact => Self::Compact,
            Operation::Search => Self::Search,
            Operation::MediaImage => Self::MediaImage,
            Operation::Auth => Self::Auth,
            Operation::Discover => Self::Discover,
            Operation::Allowance => Self::Allowance,
            Operation::ConfigValidation => Self::ConfigValidation,
        }
    }
}

pub const TRANSPORT_PREFERENCE_METADATA_KEY: &str = "transport_preference";

/// Host-selected transport for the current attempt. The guest obeys this
/// choice but never changes it or performs transport fallback on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransportPreference {
    #[default]
    Automatic,
    HttpOnly,
}

impl TransportPreference {
    pub const fn as_metadata_value(self) -> &'static str {
        match self {
            Self::Automatic => "automatic",
            Self::HttpOnly => "http-only",
        }
    }
}

/// Immutable, operation-scoped view of one provider connection. The host has
/// already applied credential and origin authorization before constructing it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderSnapshot {
    /// Stable supplier profile identity selected by the host.
    pub provider_id: String,
    pub channel: String,
    pub base_url: String,
    /// Canonical selected egress protocol endpoint or host-defined alias.
    pub protocol: String,
    #[serde(default)]
    pub options: BTreeMap<String, Value>,
    #[serde(default)]
    pub credentials: BTreeMap<String, Value>,
    pub model: Option<String>,
    pub model_metadata: Option<ModelMetadata>,
    /// Host-filtered inbound headers for this operation only.
    #[serde(default)]
    pub client_headers: Vec<(String, String)>,
    /// Host-owned non-secret request/session metadata.
    #[serde(default)]
    pub operation_metadata: BTreeMap<String, Value>,
}

impl ProviderSnapshot {
    pub fn transport_preference(&self) -> TransportPreference {
        match self
            .operation_metadata
            .get(TRANSPORT_PREFERENCE_METADATA_KEY)
            .and_then(Value::as_str)
        {
            Some("http-only") => TransportPreference::HttpOnly,
            _ => TransportPreference::Automatic,
        }
    }

    #[doc(hidden)]
    pub fn encode_for_host(&self) -> Result<types::ProviderSnapshot, PluginError> {
        Ok(types::ProviderSnapshot {
            provider_id: self.provider_id.clone(),
            channel: self.channel.clone(),
            base_url: self.base_url.clone(),
            protocol: self.protocol.clone(),
            options: encode_body(&self.options)?,
            secrets: encode_body(&self.credentials)?,
            model: self.model.clone(),
            model_metadata: self.model_metadata.as_ref().map(encode_body).transpose()?,
            client_headers: self.client_headers.clone(),
            operation_metadata: encode_body(&self.operation_metadata)?,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelMetadata {
    pub id: Option<String>,
    pub family: Option<String>,
    pub selector: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    /// External full-search plugins are single-shot. The SDK rejects a
    /// non-empty continuation before invoking guest code.
    pub continuation: Option<String>,
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    pub language: Option<String>,
    pub max_sources: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchSource {
    /// Stable source id used by report citation markers such as `[sc:{id}]`.
    pub id: String,
    pub url: String,
    pub title: Option<String>,
    pub snippet: Option<String>,
    pub published_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub answer: String,
    pub sources: Vec<SearchSource>,
    #[serde(default)]
    pub limitations: Vec<String>,
    /// Canonical Usage JSON when the upstream reported it; absent remains
    /// unknown and must never be replaced with fabricated zeros.
    pub usage: Option<stravia_runtime_contract::protocol::ir::Usage>,
}

mod base64_bytes {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde::{Deserializer, Serializer, de::Error as _};

    pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = <String as serde::Deserialize>::deserialize(deserializer)?;
        STANDARD
            .decode(encoded)
            .map_err(|_| D::Error::custom("media bytes must be canonical base64"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaReference {
    pub media_type: String,
    /// Decoded media bytes. The canonical JSON wire uses one padded base64 string.
    #[serde(with = "base64_bytes")]
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaImageAspectRatio {
    #[serde(rename = "1:1")]
    Square,
    #[serde(rename = "3:4")]
    Portrait,
    #[serde(rename = "4:3")]
    Landscape,
    #[serde(rename = "9:16")]
    Tall,
    #[serde(rename = "16:9")]
    Wide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaImageResolution {
    #[serde(rename = "1K")]
    OneK,
    #[serde(rename = "2K")]
    TwoK,
    #[serde(rename = "4K")]
    FourK,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaImageRequest {
    pub prompt: String,
    #[serde(default)]
    pub references: Vec<MediaReference>,
    pub aspect_ratio: Option<MediaImageAspectRatio>,
    pub resolution: Option<MediaImageResolution>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaArtifact {
    pub media_type: String,
    /// Decoded media bytes. The canonical JSON wire uses one padded base64 string.
    #[serde(with = "base64_bytes")]
    pub bytes: Vec<u8>,
    pub upstream_ref: Option<String>,
    /// Real upstream/decoded dimensions, format, and other non-secret facts.
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaImageResponse {
    pub artifacts: Vec<MediaArtifact>,
    pub revised_prompt: Option<String>,
    /// Confirmed canonical upstream usage when reported; absence remains
    /// unknown and must not be converted to zero.
    pub usage: Option<stravia_runtime_contract::protocol::ir::Usage>,
}

#[cfg(test)]
mod media_bytes_tests {
    use super::{MediaArtifact, MediaReference};
    use std::collections::BTreeMap;

    #[test]
    fn media_bytes_use_one_strict_base64_wire_shape() {
        let reference = MediaReference {
            media_type: "image/png".into(),
            bytes: vec![0, 1, 2, 255],
        };
        let encoded = serde_json::to_value(&reference).expect("encode media reference");
        assert_eq!(encoded["bytes"], "AAEC/w==");
        let decoded: MediaReference =
            serde_json::from_value(encoded).expect("decode media reference");
        assert_eq!(decoded.bytes, reference.bytes);
        assert!(
            serde_json::from_value::<MediaReference>(serde_json::json!({
                "media_type": "image/png",
                "bytes": [0, 1, 2, 255]
            }))
            .is_err()
        );

        let artifact = MediaArtifact {
            media_type: "image/png".into(),
            bytes: vec![255, 0],
            upstream_ref: None,
            metadata: BTreeMap::new(),
        };
        let encoded = serde_json::to_value(artifact).expect("encode media artifact");
        assert_eq!(encoded["bytes"], "/wA=");
        assert!(
            serde_json::from_value::<MediaArtifact>(serde_json::json!({
                "media_type": "image/png",
                "bytes": [255, 0],
                "upstream_ref": null,
                "metadata": {}
            }))
            .is_err()
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuthStep {
    Start { redirect_uri: String, state: String },
    Exchange { callback_url: String },
    ManualInput { value: String },
    Poll,
    Refresh,
    Revoke,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthRequest {
    pub step: AuthStep,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AuthResponse {
    Authorization {
        url: String,
        user_code: Option<String>,
        verification_uri: Option<String>,
        interval_seconds: Option<u32>,
    },
    Pending {
        retry_after_seconds: Option<u32>,
    },
    Credentials {
        values: BTreeMap<String, Value>,
        expires_at_unix_ms: Option<i64>,
    },
    Revoked,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiscoverRequest {
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredModel {
    pub id: String,
    pub display_name: String,
    pub family: Option<String>,
    pub selector: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoverResponse {
    pub models: Vec<DiscoveredModel>,
    pub next_cursor: Option<String>,
}

/// Input of the `sync-catalog` export, serialized as the canonical payload
/// body. The host owns persistence; the guest owns fetching, validation, and
/// profile derivation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogSyncRequest {
    /// Host-configured catalog service base URL (e.g. `https://models.stravia.cn`).
    pub catalog_base_url: String,
    /// `true` = fetch a fresh remote snapshot; `false` = derive profiles from
    /// `snapshot_body`, falling back to the guest's embedded bootstrap list.
    #[serde(default)]
    pub refresh_remote: bool,
    /// Last-good `providers.json` body persisted by the host, when any.
    #[serde(default)]
    pub snapshot_body: Option<String>,
    /// Revision of `snapshot_body`, echoed in the outcome when present.
    #[serde(default)]
    pub snapshot_revision: Option<String>,
    /// `generated_at` of `snapshot_body`, echoed in the outcome when present.
    #[serde(default)]
    pub snapshot_generated_at: Option<String>,
    /// When set, also fetch `providers/{id}/models.json` and return it as
    /// `scope_body`. A missing scope is `provider-not-found`, not an empty
    /// success.
    #[serde(default)]
    pub scope_provider_id: Option<String>,
}

/// Result of `sync-catalog`: the snapshot revision plus the complete
/// replacement Provider Profile set owned by this vendor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogSyncOutcome {
    /// Revision of the snapshot the profiles were derived from (`"bootstrap"`
    /// for the embedded bootstrap set).
    pub revision: String,
    /// Upstream `generated_at` timestamp of the snapshot, when known.
    #[serde(default)]
    pub generated_at: Option<String>,
    /// Freshly fetched `providers.json` body for the host to persist as
    /// last-good. Absent when profiles came from the supplied/embedded data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_body: Option<String>,
    /// Fetched `providers/{scope_provider_id}/models.json` body for the host
    /// to cache. Absent when no scope was requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_body: Option<String>,
    /// Complete replacement Provider Profile set owned by this vendor.
    pub providers: Vec<crate::descriptor::ProviderDescriptor>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AllowanceRequest {
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AllowanceAmount {
    /// Decimal string, preserving upstream precision and avoiding currency
    /// conversion or accidental binary-float merging.
    pub value: String,
    pub unit: String,
    /// ISO currency code when the amount is monetary. Unknown currencies stay
    /// distinct and are never converted or merged by the plugin boundary.
    pub currency: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AllowanceItem {
    pub key: String,
    pub label: String,
    pub kind: String,
    pub used: Option<AllowanceAmount>,
    pub remaining: Option<AllowanceAmount>,
    pub limit: Option<AllowanceAmount>,
    /// Decimal percentage string when the upstream reports one.
    pub used_percent: Option<String>,
    pub window_seconds: Option<u64>,
    pub resets_at_unix_ms: Option<i64>,
    pub condition: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelAllowance {
    pub model: String,
    pub allowances: Vec<AllowanceItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AllowanceResponse {
    #[serde(default)]
    pub allowances: Vec<AllowanceItem>,
    #[serde(default)]
    pub models: Vec<ModelAllowance>,
    pub plan_label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigValidationRequest {
    pub options: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationIssue {
    pub field: Option<String>,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigValidationResponse {
    #[serde(default)]
    pub issues: Vec<ValidationIssue>,
    /// Normalized host base URL derived from validated config fields. Core may
    /// present and persist it through the generic provider configuration flow.
    pub proposed_base_url: Option<String>,
}

/// Fully decoded operation input. Every operation has a fixed concrete type;
/// no plugin-facing arbitrary byte escape hatch exists.
#[derive(Debug, Clone)]
pub enum OperationInput {
    Infer {
        provider: ProviderSnapshot,
        request: AiRequest,
    },
    Compact {
        provider: ProviderSnapshot,
        request: AiRequest,
    },
    Search {
        provider: ProviderSnapshot,
        request: SearchRequest,
    },
    MediaImage {
        provider: ProviderSnapshot,
        request: MediaImageRequest,
    },
    Auth {
        provider: ProviderSnapshot,
        request: AuthRequest,
    },
    Discover {
        provider: ProviderSnapshot,
        request: DiscoverRequest,
    },
    Allowance {
        provider: ProviderSnapshot,
        request: AllowanceRequest,
    },
    ConfigValidation {
        provider: ProviderSnapshot,
        request: ConfigValidationRequest,
    },
}

impl OperationInput {
    pub fn operation(&self) -> Operation {
        match self {
            Self::Infer { .. } => Operation::Infer,
            Self::Compact { .. } => Operation::Compact,
            Self::Search { .. } => Operation::Search,
            Self::MediaImage { .. } => Operation::MediaImage,
            Self::Auth { .. } => Operation::Auth,
            Self::Discover { .. } => Operation::Discover,
            Self::Allowance { .. } => Operation::Allowance,
            Self::ConfigValidation { .. } => Operation::ConfigValidation,
        }
    }

    pub fn provider(&self) -> &ProviderSnapshot {
        match self {
            Self::Infer { provider, .. }
            | Self::Compact { provider, .. }
            | Self::Search { provider, .. }
            | Self::MediaImage { provider, .. }
            | Self::Auth { provider, .. }
            | Self::Discover { provider, .. }
            | Self::Allowance { provider, .. }
            | Self::ConfigValidation { provider, .. } => provider,
        }
    }

    #[doc(hidden)]
    pub fn encode_for_host(
        &self,
    ) -> Result<(types::OperationKind, types::OperationInput), PluginError> {
        let (provider, payload) = match self {
            Self::Infer { provider, request } | Self::Compact { provider, request } => {
                (provider, encode_canonical(request)?)
            }
            Self::Search { provider, request } => (provider, encode_canonical(request)?),
            Self::MediaImage { provider, request } => (provider, encode_canonical(request)?),
            Self::Auth { provider, request } => (provider, encode_canonical(request)?),
            Self::Discover { provider, request } => (provider, encode_canonical(request)?),
            Self::Allowance { provider, request } => (provider, encode_canonical(request)?),
            Self::ConfigValidation { provider, request } => (provider, encode_canonical(request)?),
        };
        let operation = self.operation().into();
        Ok((
            operation,
            types::OperationInput {
                provider: provider.encode_for_host()?,
                input: payload,
            },
        ))
    }

    fn decode(
        operation: Operation,
        channel: &str,
        input: types::OperationInput,
    ) -> Result<Self, PluginError> {
        let provider = decode_provider(&input.provider)?;
        if provider.channel != channel {
            return Err(error(
                ErrorKind::Invalid,
                "provider snapshot channel does not match execute channel",
            ));
        }
        Ok(match operation {
            Operation::Infer => Self::Infer {
                provider,
                request: decode_body(&input.input)?,
            },
            Operation::Compact => Self::Compact {
                provider,
                request: decode_body(&input.input)?,
            },
            Operation::Search => {
                let request: SearchRequest = decode_body(&input.input)?;
                if request.continuation.is_some() {
                    return Err(error(
                        ErrorKind::Invalid,
                        "external vendor search does not support continuation",
                    ));
                }
                Self::Search { provider, request }
            }
            Operation::MediaImage => Self::MediaImage {
                provider,
                request: decode_body(&input.input)?,
            },
            Operation::Auth => Self::Auth {
                provider,
                request: decode_body(&input.input)?,
            },
            Operation::Discover => Self::Discover {
                provider,
                request: decode_body(&input.input)?,
            },
            Operation::Allowance => Self::Allowance {
                provider,
                request: decode_body(&input.input)?,
            },
            Operation::ConfigValidation => Self::ConfigValidation {
                provider,
                request: decode_body(&input.input)?,
            },
        })
    }
}

#[derive(Debug, Clone)]
pub enum OperationOutput {
    Infer(Box<AiResponse>),
    Compact(NativeCompactionResponse),
    Search(SearchResponse),
    MediaImage(MediaImageResponse),
    Auth(AuthResponse),
    Discover(DiscoverResponse),
    Allowance(AllowanceResponse),
    ConfigValidation(ConfigValidationResponse),
}

impl OperationOutput {
    #[doc(hidden)]
    pub fn decode_for_host(operation: Operation, bytes: &[u8]) -> Result<Self, PluginError> {
        Ok(match operation {
            Operation::Infer => Self::Infer(decode_json(bytes, "infer result")?),
            Operation::Compact => Self::Compact(decode_json(bytes, "compact result")?),
            Operation::Search => Self::Search(decode_json(bytes, "search result")?),
            Operation::MediaImage => Self::MediaImage(decode_json(bytes, "media image result")?),
            Operation::Auth => Self::Auth(decode_json(bytes, "auth result")?),
            Operation::Discover => Self::Discover(decode_json(bytes, "discover result")?),
            Operation::Allowance => Self::Allowance(decode_json(bytes, "allowance result")?),
            Operation::ConfigValidation => {
                Self::ConfigValidation(decode_json(bytes, "config validation result")?)
            }
        })
    }

    fn encode_for(&self, operation: Operation) -> Result<Vec<u8>, PluginError> {
        match (operation, self) {
            (Operation::Infer, Self::Infer(value)) => encode_body(value),
            (Operation::Compact, Self::Compact(value)) => encode_body(value),
            (Operation::Search, Self::Search(value)) => encode_body(value),
            (Operation::MediaImage, Self::MediaImage(value)) => encode_body(value),
            (Operation::Auth, Self::Auth(value)) => encode_body(value),
            (Operation::Discover, Self::Discover(value)) => encode_body(value),
            (Operation::Allowance, Self::Allowance(value)) => encode_body(value),
            (Operation::ConfigValidation, Self::ConfigValidation(value)) => encode_body(value),
            _ => Err(error(
                ErrorKind::Invalid,
                "guest returned an output for a different operation",
            )),
        }
    }
}

pub trait VendorGuest {
    fn descriptor() -> VendorDescriptor;

    /// Select the egress protocol for a canonical model request. This hook is
    /// pure: host transport, private state, and event imports are unavailable
    /// while it runs. Single-protocol guests inherit the configured protocol.
    fn select_protocol(
        _operation: Operation,
        _channel: &str,
        provider: &ProviderSnapshot,
        _request: &AiRequest,
    ) -> Result<String, PluginError> {
        Ok(provider.protocol.clone())
    }

    fn execute(
        host: &GuestHost,
        operation: Operation,
        channel: &str,
        input: OperationInput,
    ) -> Result<OperationOutput, PluginError>;

    /// Synchronize the vendor-owned runtime catalog. Vendor-scoped rather
    /// than connection-scoped: `http-start` is admitted only for the
    /// configured catalog origin. The default implementation reports
    /// `unsupported` for vendors without a runtime catalog.
    fn sync_catalog(
        _host: &GuestHost,
        _request: CatalogSyncRequest,
    ) -> Result<CatalogSyncOutcome, PluginError> {
        Err(error(
            ErrorKind::Unsupported,
            "vendor does not implement a runtime catalog",
        ))
    }
}

fn decode_provider(value: &types::ProviderSnapshot) -> Result<ProviderSnapshot, PluginError> {
    Ok(ProviderSnapshot {
        provider_id: value.provider_id.clone(),
        channel: value.channel.clone(),
        base_url: value.base_url.clone(),
        protocol: value.protocol.clone(),
        options: decode_json_map(&value.options, "provider options")?,
        credentials: decode_json_map(&value.secrets, "provider credentials")?,
        model: value.model.clone(),
        model_metadata: value
            .model_metadata
            .as_deref()
            .map(|bytes| decode_json(bytes, "model metadata"))
            .transpose()?,
        client_headers: value.client_headers.clone(),
        operation_metadata: decode_json_map(&value.operation_metadata, "operation metadata")?,
    })
}

fn decode_body<T: DeserializeOwned>(payload: &types::CanonicalPayload) -> Result<T, PluginError> {
    Ok(decode_payload(payload)?.body)
}

fn encode_canonical<T: Serialize>(value: &T) -> Result<types::CanonicalPayload, PluginError> {
    Ok(types::CanonicalPayload {
        format: CANONICAL_FORMAT_VERSION,
        body: encode_body(value)?,
    })
}

fn decode_json<T: DeserializeOwned>(bytes: &[u8], label: &str) -> Result<T, PluginError> {
    serde_json::from_slice(bytes).map_err(|_| error(ErrorKind::Invalid, format!("invalid {label}")))
}

fn decode_json_map(bytes: &[u8], label: &str) -> Result<BTreeMap<String, Value>, PluginError> {
    if bytes.is_empty() {
        Ok(BTreeMap::new())
    } else {
        decode_json(bytes, label)
    }
}

fn encode_body<T: Serialize>(value: &T) -> Result<Vec<u8>, PluginError> {
    serde_json::to_vec(value)
        .map_err(|_| error(ErrorKind::Invalid, "operation result could not be encoded"))
}

#[doc(hidden)]
pub fn descriptor_json<G: VendorGuest>() -> String {
    let descriptor = G::descriptor();
    if descriptor.canonical_format_version != CANONICAL_FORMAT_VERSION {
        panic!("descriptor canonical_format_version does not match SDK");
    }
    descriptor.validate().expect("guest descriptor is invalid");
    serde_json::to_string(&descriptor).expect("guest descriptor must serialize")
}

#[doc(hidden)]
pub fn encode_protocol_selection(
    provider: &ProviderSnapshot,
    request: &AiRequest,
) -> Result<(types::ProviderSnapshot, types::CanonicalPayload), PluginError> {
    Ok((provider.encode_for_host()?, encode_canonical(request)?))
}

fn admit_provider<G: VendorGuest>(
    operation: Operation,
    provider: &ProviderSnapshot,
) -> Result<(), PluginError> {
    let descriptor = G::descriptor();
    let runtime_profile;
    let profile = match descriptor.provider(&provider.provider_id) {
        Some(profile) => profile,
        None => {
            // Profiles registered through `sync-catalog` are echoed back by
            // the host so the guest can admit them on the pure select-protocol
            // path where catalog state is unavailable.
            runtime_profile = provider
                .operation_metadata
                .get("vendor_profile")
                .and_then(|value| {
                    serde_json::from_value::<crate::descriptor::ProviderDescriptor>(value.clone())
                        .ok()
                })
                .filter(|profile| profile.provider_id == provider.provider_id);
            match runtime_profile.as_ref() {
                Some(profile) => profile,
                None => {
                    return Err(error(
                        ErrorKind::Unsupported,
                        "vendor does not own the provider profile",
                    ));
                }
            }
        }
    };
    let Some(channel) = profile
        .channels
        .iter()
        .find(|channel| channel.id == provider.channel)
    else {
        return Err(error(
            ErrorKind::Unsupported,
            "provider profile does not implement the selected channel",
        ));
    };
    if !channel
        .capabilities
        .contains(&operation.required_capability())
    {
        return Err(error(
            ErrorKind::Unsupported,
            "provider profile channel does not implement the operation",
        ));
    }
    Ok(())
}

#[doc(hidden)]
pub fn dispatch_select_protocol<G: VendorGuest>(
    operation: types::OperationKind,
    channel: String,
    provider: types::ProviderSnapshot,
    request: types::CanonicalPayload,
) -> Result<String, PluginError> {
    let operation = Operation::from(operation);
    if !matches!(operation, Operation::Infer | Operation::Compact) {
        return Err(error(
            ErrorKind::Invalid,
            "protocol selection is only valid for infer and compact",
        ));
    }
    let provider = decode_provider(&provider)?;
    if provider.channel != channel {
        return Err(error(
            ErrorKind::Invalid,
            "provider snapshot channel does not match select-protocol channel",
        ));
    }
    admit_provider::<G>(operation, &provider)?;
    let request = decode_body(&request)?;
    G::select_protocol(operation, &channel, &provider, &request)
}

#[doc(hidden)]
pub fn dispatch_sync_catalog<G: VendorGuest>(
    input: types::CanonicalPayload,
) -> Result<types::CanonicalPayload, PluginError> {
    let request: CatalogSyncRequest = decode_body(&input)?;
    let host = GuestHost::new();
    G::sync_catalog(&host, request).and_then(|outcome| encode_canonical(&outcome))
}

#[doc(hidden)]
pub fn dispatch<G: VendorGuest>(
    operation: types::OperationKind,
    channel: String,
    input: types::OperationInput,
) -> Result<Vec<u8>, PluginError> {
    let operation = Operation::from(operation);
    let input = OperationInput::decode(operation, &channel, input)?;
    admit_provider::<G>(operation, input.provider())?;
    let host = GuestHost::new();
    G::execute(&host, operation, &channel, input)?.encode_for(operation)
}

/// Register one [`VendorGuest`] implementation as the component export.
#[macro_export]
macro_rules! export_vendor {
    ($guest:ty) => {
        struct __StraviaVendorExport;

        impl $crate::wit::Guest for __StraviaVendorExport {
            fn descriptor() -> String {
                $crate::guest::descriptor_json::<$guest>()
            }

            fn select_protocol(
                operation: $crate::wit::types::OperationKind,
                channel: String,
                provider: $crate::wit::types::ProviderSnapshot,
                request: $crate::wit::types::CanonicalPayload,
            ) -> Result<String, $crate::wit::types::PluginError> {
                $crate::guest::dispatch_select_protocol::<$guest>(
                    operation,
                    channel,
                    provider,
                    request,
                )
            }

            fn execute(
                operation: $crate::wit::types::OperationKind,
                channel: String,
                input: $crate::wit::types::OperationInput,
            ) -> Result<Vec<u8>, $crate::wit::types::PluginError> {
                $crate::guest::dispatch::<$guest>(operation, channel, input)
            }

            fn sync_catalog(
                input: $crate::wit::types::CanonicalPayload,
            ) -> Result<$crate::wit::types::CanonicalPayload, $crate::wit::types::PluginError>
            {
                $crate::guest::dispatch_sync_catalog::<$guest>(input)
            }
        }

        $crate::bindings::export!(__StraviaVendorExport with_types_in $crate::bindings);
    };
}

#[cfg(test)]
mod profile_admission_tests {
    use super::*;
    use crate::descriptor::{
        ChannelDescriptor, DataCompatibility, NetworkDeclaration, ProviderDescriptor, VendorKind,
    };
    use std::collections::BTreeSet;

    struct MultiProfileGuest;

    impl VendorGuest for MultiProfileGuest {
        fn descriptor() -> VendorDescriptor {
            VendorDescriptor {
                vendor_id: "base".into(),
                version: semver::Version::new(1, 0, 0),
                display_name: "Base".into(),
                description: None,
                authors: Vec::new(),
                canonical_format_version: CANONICAL_FORMAT_VERSION,
                kind: VendorKind::Fallback,
                providers: vec![
                    profile("alpha", Capability::Infer),
                    profile("beta", Capability::Search),
                ],
            }
        }

        fn execute(
            _host: &GuestHost,
            _operation: Operation,
            _channel: &str,
            _input: OperationInput,
        ) -> Result<OperationOutput, PluginError> {
            unreachable!("admission test never executes the guest")
        }
    }

    fn profile(provider_id: &str, capability: Capability) -> ProviderDescriptor {
        let capabilities = BTreeSet::from([capability]);
        ProviderDescriptor {
            provider_id: provider_id.into(),
            catalog_id: None,
            display_name: provider_id.into(),
            description: None,
            channels: vec![ChannelDescriptor {
                id: "default".into(),
                name: "Default".into(),
                description: None,
                auth: None,
                protocol: Some("test".into()),
                protocols: Vec::new(),
                default_base_url: None,
                default_models_source: None,
                consumes_catalog_models: false,
                capabilities: capabilities.clone(),
                model_capabilities: BTreeSet::new(),
                search_model_required: false,
            }],
            capabilities,
            website: None,
            implementation: None,
            config_fields: Vec::new(),
            network: NetworkDeclaration::default(),
            data_compat: DataCompatibility::default(),
        }
    }

    fn snapshot(provider_id: &str) -> ProviderSnapshot {
        ProviderSnapshot {
            provider_id: provider_id.into(),
            channel: "default".into(),
            base_url: "https://example.invalid".into(),
            protocol: "test".into(),
            options: BTreeMap::new(),
            credentials: BTreeMap::new(),
            model: None,
            model_metadata: None,
            client_headers: Vec::new(),
            operation_metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn does_not_union_capabilities_across_provider_profiles() {
        let error = admit_provider::<MultiProfileGuest>(Operation::Search, &snapshot("alpha"))
            .expect_err("alpha must not inherit beta search support");
        assert!(matches!(error.kind, ErrorKind::Unsupported));
    }

    #[test]
    fn rejects_provider_profiles_the_vendor_does_not_own() {
        let error = admit_provider::<MultiProfileGuest>(Operation::Infer, &snapshot("gamma"))
            .expect_err("unknown profile must be rejected");
        assert!(matches!(error.kind, ErrorKind::Unsupported));
    }
}
