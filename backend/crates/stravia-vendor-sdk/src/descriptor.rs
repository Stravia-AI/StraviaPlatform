//! `VendorDescriptor` — the manifest a plugin reports through the `descriptor`
//! export. The host validates it at load; nothing here grants trust by itself.
//! Self-reported `authors`/`display_name` are never presented as verified.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

/// A vendor capability. Each channel declares its own non-empty set; the
/// provider-level set is the union used for coarse admission checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Chat inference over the canonical contract.
    Infer,
    /// Native compaction of canonical history.
    Compact,
    /// Full web search returning a cited report (single shot, no continuation).
    Search,
    /// Image generation / reference-image editing.
    MediaImage,
    /// OAuth or custom credential exchange.
    AuthOauth,
    /// Model discovery / metadata sync.
    ModelDiscovery,
    /// Upstream allowance / quota read.
    Allowance,
    /// Plugin-side validation of proposed config values.
    ConfigValidation,
}

impl Capability {
    /// Stable snake_case identifier used in bindings, diagnostics, and APIs.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Infer => "infer",
            Self::Compact => "compact",
            Self::Search => "search",
            Self::MediaImage => "media_image",
            Self::AuthOauth => "auth_oauth",
            Self::ModelDiscovery => "model_discovery",
            Self::Allowance => "allowance",
            Self::ConfigValidation => "config_validation",
        }
    }

    /// Parse a stable identifier back into a capability.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "infer" => Self::Infer,
            "compact" => Self::Compact,
            "search" => Self::Search,
            "media_image" => Self::MediaImage,
            "auth_oauth" => Self::AuthOauth,
            "model_discovery" => Self::ModelDiscovery,
            "allowance" => Self::Allowance,
            "config_validation" => Self::ConfigValidation,
            _ => return None,
        })
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Authentication flow a channel exposes to the host-owned session UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthFlow {
    AuthorizationCode,
    DeviceCode,
    Manual,
}

/// Callback listener port policy. Fixed ports are part of a vendor protocol;
/// dynamic ports are chosen by the host and supplied in `AuthStep::Start`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuthCallbackPort {
    Fixed { primary: u16, fallback: Option<u16> },
    Dynamic,
}

/// Host-owned callback listener policy declared by a channel. No vendor id is
/// consulted when binding listeners or constructing callback routes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthCallback {
    pub bind_host: String,
    pub redirect_host: String,
    pub path: String,
    pub port: AuthCallbackPort,
    pub manual_redirect_uri: Option<String>,
    pub cancel_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthManualInputType {
    Text,
    CallbackUrl,
}

/// Host-rendered input requested by a manual flow or offered as a fallback for
/// an authorization-code flow. The guest receives only the submitted value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthManualInput {
    #[serde(rename = "type")]
    pub input_type: AuthManualInputType,
    pub label: String,
    pub description: Option<String>,
    #[serde(default)]
    pub secret: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthDescriptor {
    pub flow: AuthFlow,
    pub callback: Option<AuthCallback>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual_input: Option<AuthManualInput>,
}

/// Canonical declaration that the guest can encode an enabled/disabled
/// thinking control for a model, even when its base codec cannot.
pub const MODEL_CAPABILITY_THINKING_TOGGLE: &str = "thinking_toggle";

/// `models_source` marker selecting the host-owned catalog inventory. Guests
/// must treat it as an opaque source selector, never as a request URL.
pub const MODELS_SOURCE_CATALOG: &str = "catalog";

/// Host-owned model inventory source a channel may select by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefaultModelsSource {
    Catalog,
}

impl DefaultModelsSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Catalog => MODELS_SOURCE_CATALOG,
        }
    }
}

/// One channel = one auth/service entry point under the same vendor identity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelDescriptor {
    /// Stable channel id within the vendor (e.g. `"default"`, `"oauth"`).
    pub id: String,
    /// Human-facing name.
    pub name: String,
    pub description: Option<String>,
    /// Host-owned authentication/callback policy for this channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthDescriptor>,
    /// Initial egress protocol hint when the administrator has not selected
    /// one. A saved connection hint takes precedence; the guest must implement
    /// it or reject it. `None` is valid for a custom vendor wire protocol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    /// Vendor-declared initial base URL presented when creating a connection.
    /// Runtime authority still comes from the saved provider snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_base_url: Option<String>,
    /// Default model discovery source applied when the connection does not
    /// explicitly select one. Currently only the host-owned catalog is valid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_models_source: Option<DefaultModelsSource>,
    /// `true` when catalog-sourced discovery on this channel consumes the
    /// host-injected `catalog_models` scope. The host resolves the catalog
    /// scope only for channels declaring this; channels that resolve
    /// `catalog` into a live account request must leave it `false`.
    #[serde(default)]
    pub consumes_catalog_models: bool,
    /// Capabilities actually usable through this channel. Non-empty.
    pub capabilities: BTreeSet<Capability>,
    /// Positive model capability defaults, supplemented by discovered model
    /// metadata. Explicit negative canonical model facts still take precedence.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub model_capabilities: BTreeSet<String>,
    /// `true` when `search` requires an upstream model selection; `false` for
    /// provider-only research services that pin their own wire model.
    #[serde(default)]
    pub search_model_required: bool,
}

/// Simple conditional display for a config field. No expressions or scripts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldCondition {
    /// Key of another field in the same form.
    pub field: String,
    /// Show this field only when `field` currently equals this value.
    pub equals: serde_json::Value,
}

/// One selectable value of an enum config field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnumOption {
    pub value: String,
    pub label: String,
}

/// Declared type of a config field; drives the host-generated form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConfigFieldKind {
    Bool,
    String {
        /// Long-form text (e.g. service-account JSON); renders a textarea.
        #[serde(default)]
        multiline: bool,
    },
    Int,
    Decimal,
    Enum {
        options: Vec<EnumOption>,
    },
}

/// One admin-configurable field. Secrets are flagged separately so the host
/// never echoes them back into option views.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigField {
    /// Stable key referenced by operations and `visible_when`.
    pub key: String,
    pub label: String,
    pub description: Option<String>,
    pub kind: ConfigFieldKind,
    #[serde(default)]
    pub required: bool,
    /// Default encoded as JSON matching `kind`.
    #[serde(default)]
    pub default_json: Option<serde_json::Value>,
    /// Optional form grouping.
    #[serde(default)]
    pub group: Option<String>,
    /// Secret fields are stored/loaded through the credential channel, never
    /// round-tripped into the admin UI.
    #[serde(default)]
    pub secret: bool,
    /// Numeric bounds for `Int`/`Decimal`.
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    /// Length cap for string fields.
    #[serde(default)]
    pub max_length: Option<u32>,
    /// Regex constraint for string fields.
    #[serde(default)]
    pub pattern: Option<String>,
    #[serde(default)]
    pub visible_when: Option<FieldCondition>,
}

/// One exact additional origin the plugin is allowed to reach. No wildcards:
/// host/port must be concrete.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct OriginDeclaration {
    /// `http`, `https`, `ws`, or `wss`.
    pub scheme: String,
    pub host: String,
    /// `None` = scheme default port.
    pub port: Option<u16>,
}

/// Network footprint declared at install and pinned to the operation snapshot.
/// The plugin cannot extend it at runtime, through private state, or via
/// upstream responses.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct NetworkDeclaration {
    /// Key of the config field whose saved value supplies the base origin.
    /// `None` = use the host-configured provider base URL; the plugin does not
    /// have to re-declare a field for it.
    #[serde(default)]
    pub base_url_field: Option<String>,
    /// Exact extra origins (e.g. a fixed auth endpoint).
    #[serde(default)]
    pub extra_origins: Vec<OriginDeclaration>,
    /// Config field keys whose *saved values* the host resolves into exact
    /// origins (e.g. a `token_url` field). The resolved set is shown to the
    /// admin at save time and frozen into each operation snapshot.
    #[serde(default)]
    pub field_origins: Vec<String>,
}

/// Independent data-format epochs. The host compares each field between the
/// installed and replacement descriptor; only the fields that changed force a
/// data-drop confirmation. A bump in one does not invalidate the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataCompatibility {
    /// Interpretation of saved non-secret config values.
    pub config_fields_format: u32,
    /// Private-state blob format.
    pub private_state_format: u32,
    /// Stored credential payload format.
    pub credentials_format: u32,
    /// Persisted discovered-model metadata format.
    pub model_metadata_format: u32,
}

impl Default for DataCompatibility {
    fn default() -> Self {
        Self {
            config_fields_format: 1,
            private_state_format: 1,
            credentials_format: 1,
            model_metadata_format: 1,
        }
    }
}

/// How a vendor package participates in provider resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VendorKind {
    /// The single built-in package used when no dedicated provider package owns
    /// the requested provider identity.
    Fallback,
    /// A package that owns exactly one provider identity in full.
    Dedicated,
}

/// One provider profile implemented by a vendor package.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderDescriptor {
    /// Stable supplier profile identity. This is not a saved connection UUID.
    pub provider_id: String,
    /// Optional catalog identity used to group related provider profiles.
    pub catalog_id: Option<String>,
    pub display_name: String,
    pub description: Option<String>,
    /// Non-empty; each carries its own capability set.
    pub channels: Vec<ChannelDescriptor>,
    /// Union of all channel capabilities, used for coarse admission.
    pub capabilities: BTreeSet<Capability>,
    /// Declared admin-configurable fields (options + secrets).
    #[serde(default)]
    pub config_fields: Vec<ConfigField>,
    #[serde(default)]
    pub network: NetworkDeclaration,
    #[serde(default)]
    pub data_compat: DataCompatibility,
}

/// The manifest a component returns from its `descriptor` export, serialized
/// as JSON. Validated by the host before the plugin is admitted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VendorDescriptor {
    /// Stable package identity. Independent of codec or SDK version.
    pub vendor_id: String,
    pub version: semver::Version,
    pub display_name: String,
    pub description: Option<String>,
    /// Self-reported; never displayed as a verified source.
    #[serde(default)]
    pub authors: Vec<String>,
    /// Canonical envelope schema the guest was built against. The host must
    /// support it; there is no assumption that internal evolution stays
    /// compatible forever.
    pub canonical_format_version: u32,
    pub kind: VendorKind,
    /// Provider profiles owned by this package. Profiles never merge at runtime.
    pub providers: Vec<ProviderDescriptor>,
}

impl VendorDescriptor {
    /// Validate structural invariants the host enforces on load.
    pub fn validate(&self) -> Result<(), DescriptorError> {
        if self.vendor_id.is_empty() {
            return Err(DescriptorError::EmptyVendorId);
        }
        if !valid_id(&self.vendor_id) {
            return Err(DescriptorError::InvalidVendorId);
        }
        if self.display_name.trim().is_empty() {
            return Err(DescriptorError::EmptyDisplayName);
        }
        if self.providers.is_empty() {
            return Err(DescriptorError::NoProviders);
        }
        match self.kind {
            VendorKind::Fallback if self.vendor_id != "base" => {
                return Err(DescriptorError::InvalidFallbackVendorId);
            }
            VendorKind::Dedicated
                if self.vendor_id == "base"
                    || self.providers.len() != 1
                    || self.providers[0].provider_id != self.vendor_id =>
            {
                return Err(DescriptorError::InvalidDedicatedProvider);
            }
            VendorKind::Fallback | VendorKind::Dedicated => {}
        }

        let mut provider_ids = BTreeSet::new();
        for provider in &self.providers {
            if provider.provider_id.is_empty() {
                return Err(DescriptorError::EmptyProviderId);
            }
            if provider.provider_id == "base" || !valid_id(&provider.provider_id) {
                return Err(DescriptorError::InvalidProviderId(
                    provider.provider_id.clone(),
                ));
            }
            if !provider_ids.insert(provider.provider_id.as_str()) {
                return Err(DescriptorError::DuplicateProvider(
                    provider.provider_id.clone(),
                ));
            }
            if provider
                .catalog_id
                .as_deref()
                .is_some_and(|catalog_id| !valid_id(catalog_id))
            {
                return Err(DescriptorError::InvalidCatalogId(
                    provider.catalog_id.clone().unwrap_or_default(),
                ));
            }
            provider.validate()?;
        }
        Ok(())
    }

    /// Find the complete profile owned for `provider_id`.
    pub fn provider(&self, provider_id: &str) -> Option<&ProviderDescriptor> {
        self.providers
            .iter()
            .find(|provider| provider.provider_id == provider_id)
    }
}

impl ProviderDescriptor {
    fn validate(&self) -> Result<(), DescriptorError> {
        if self.display_name.trim().is_empty() {
            return Err(DescriptorError::EmptyProviderDisplayName(
                self.provider_id.clone(),
            ));
        }
        if self.channels.is_empty() {
            return Err(DescriptorError::NoChannels);
        }
        if self.capabilities.is_empty() {
            return Err(DescriptorError::NoCapabilities);
        }
        let mut channel_ids = BTreeSet::new();
        for ch in &self.channels {
            if ch.id.is_empty() {
                return Err(DescriptorError::EmptyChannelId);
            }
            if !channel_ids.insert(ch.id.clone()) {
                return Err(DescriptorError::DuplicateChannel(ch.id.clone()));
            }
            if ch.capabilities.is_empty() {
                return Err(DescriptorError::ChannelWithoutCapabilities(ch.id.clone()));
            }
            if ch.default_models_source == Some(DefaultModelsSource::Catalog)
                && !ch.consumes_catalog_models
            {
                return Err(DescriptorError::CatalogDefaultWithoutConsumption(
                    ch.id.clone(),
                ));
            }
            if let Some(auth) = &ch.auth {
                let callback_expected = matches!(auth.flow, AuthFlow::AuthorizationCode);
                if callback_expected != auth.callback.is_some()
                    || (matches!(auth.flow, AuthFlow::Manual) && auth.manual_input.is_none())
                    || (matches!(auth.flow, AuthFlow::DeviceCode) && auth.manual_input.is_some())
                    || auth.manual_input.as_ref().is_some_and(|input| {
                        input.label.trim().is_empty()
                            || matches!(auth.flow, AuthFlow::Manual)
                                && input.input_type != AuthManualInputType::Text
                    })
                {
                    return Err(DescriptorError::InvalidAuthPolicy(ch.id.clone()));
                }
                if let Some(callback) = &auth.callback {
                    let valid_path = |path: &str| {
                        path.starts_with('/')
                            && path.bytes().all(|byte| byte.is_ascii_graphic())
                            && !path.contains(['?', '#', '{', '}'])
                            && path
                                .split('/')
                                .all(|segment| !segment.starts_with([':', '*']))
                    };
                    let loopback_host = |host: &str| {
                        host.eq_ignore_ascii_case("localhost")
                            || host
                                .parse::<std::net::IpAddr>()
                                .is_ok_and(|ip| ip.is_loopback())
                    };
                    let valid_port = match callback.port {
                        AuthCallbackPort::Fixed { primary, fallback } => {
                            primary != 0
                                && fallback
                                    .is_none_or(|fallback| fallback != 0 && fallback != primary)
                        }
                        AuthCallbackPort::Dynamic => true,
                    };
                    if !loopback_host(&callback.bind_host)
                        || !loopback_host(&callback.redirect_host)
                        || !valid_path(&callback.path)
                        || callback
                            .cancel_path
                            .as_deref()
                            .is_some_and(|path| path == callback.path || !valid_path(path))
                        || callback
                            .manual_redirect_uri
                            .as_deref()
                            .is_some_and(|uri| uri.trim().is_empty())
                        || (auth.manual_input.is_some() && callback.manual_redirect_uri.is_none())
                        || !valid_port
                    {
                        return Err(DescriptorError::InvalidAuthPolicy(ch.id.clone()));
                    }
                }
            }
        }
        let declared: BTreeSet<Capability> = self
            .channels
            .iter()
            .flat_map(|c| c.capabilities.iter().copied())
            .collect();
        if declared != self.capabilities {
            return Err(DescriptorError::CapabilityUnionMismatch);
        }
        let field_keys: BTreeSet<&str> =
            self.config_fields.iter().map(|f| f.key.as_str()).collect();
        if field_keys.len() != self.config_fields.len() {
            return Err(DescriptorError::DuplicateConfigField);
        }
        if let Some(field) = &self.network.base_url_field {
            let Some(config) = self
                .config_fields
                .iter()
                .find(|candidate| candidate.key == *field)
            else {
                return Err(DescriptorError::UnknownNetworkField(field.clone()));
            };
            if !matches!(config.kind, ConfigFieldKind::String { .. }) {
                return Err(DescriptorError::NetworkFieldNotString(field.clone()));
            }
        }
        for field in &self.network.field_origins {
            let Some(config) = self
                .config_fields
                .iter()
                .find(|candidate| candidate.key == *field)
            else {
                return Err(DescriptorError::UnknownNetworkField(field.clone()));
            };
            if !matches!(config.kind, ConfigFieldKind::String { .. }) {
                return Err(DescriptorError::NetworkFieldNotString(field.clone()));
            }
        }
        for origin in &self.network.extra_origins {
            if !matches!(origin.scheme.as_str(), "http" | "https" | "ws" | "wss") {
                return Err(DescriptorError::BadOriginScheme(origin.scheme.clone()));
            }
            if origin.host.is_empty() || origin.host.contains('*') {
                return Err(DescriptorError::BadOriginHost(origin.host.clone()));
            }
        }
        for field in &self.config_fields {
            if field.key.is_empty() {
                return Err(DescriptorError::EmptyConfigFieldKey);
            }
            if field.secret && field.default_json.is_some() {
                return Err(DescriptorError::SecretDefault(field.key.clone()));
            }
            if field.min.is_some_and(|value| !value.is_finite())
                || field.max.is_some_and(|value| !value.is_finite())
                || matches!((field.min, field.max), (Some(min), Some(max)) if min > max)
            {
                return Err(DescriptorError::InvalidBounds(field.key.clone()));
            }
            if let Some(cond) = &field.visible_when
                && !field_keys.contains(cond.field.as_str())
            {
                return Err(DescriptorError::UnknownConditionField(cond.field.clone()));
            }
            if let ConfigFieldKind::Enum { options } = &field.kind {
                if options.is_empty() {
                    return Err(DescriptorError::EmptyEnum(field.key.clone()));
                }
                let values: BTreeSet<&str> =
                    options.iter().map(|option| option.value.as_str()).collect();
                if values.len() != options.len() {
                    return Err(DescriptorError::DuplicateEnumValue(field.key.clone()));
                }
            }
            if let Some(default) = &field.default_json
                && !default_matches_kind(default, &field.kind)
            {
                return Err(DescriptorError::InvalidDefault(field.key.clone()));
            }
        }
        Ok(())
    }
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
}

fn default_matches_kind(value: &serde_json::Value, kind: &ConfigFieldKind) -> bool {
    match kind {
        ConfigFieldKind::Bool => value.is_boolean(),
        ConfigFieldKind::String { .. } => value.is_string(),
        ConfigFieldKind::Int => value.as_i64().is_some() || value.as_u64().is_some(),
        ConfigFieldKind::Decimal => value.is_number(),
        ConfigFieldKind::Enum { options } => value
            .as_str()
            .is_some_and(|candidate| options.iter().any(|option| option.value == candidate)),
    }
}

/// Structural validation failures for [`VendorDescriptor::validate`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DescriptorError {
    #[error("vendor_id must not be empty")]
    EmptyVendorId,
    #[error("vendor_id must contain only lowercase ASCII letters, digits, '.', '-', or '_'")]
    InvalidVendorId,
    #[error("display_name must not be empty")]
    EmptyDisplayName,
    #[error("fallback vendor_id must be `base`")]
    InvalidFallbackVendorId,
    #[error(
        "dedicated vendor must declare exactly one provider whose provider_id equals vendor_id"
    )]
    InvalidDedicatedProvider,
    #[error("descriptor must declare at least one provider")]
    NoProviders,
    #[error("provider_id must not be empty")]
    EmptyProviderId,
    #[error(
        "provider_id `{0}` must contain only lowercase ASCII letters, digits, '.', '-', or '_'"
    )]
    InvalidProviderId(String),
    #[error("duplicate provider_id `{0}`")]
    DuplicateProvider(String),
    #[error("catalog_id `{0}` must contain only lowercase ASCII letters, digits, '.', '-', or '_'")]
    InvalidCatalogId(String),
    #[error("provider `{0}` display_name must not be empty")]
    EmptyProviderDisplayName(String),
    #[error("provider must declare at least one channel")]
    NoChannels,
    #[error("descriptor must declare at least one capability")]
    NoCapabilities,
    #[error("channel id must not be empty")]
    EmptyChannelId,
    #[error("duplicate channel id `{0}`")]
    DuplicateChannel(String),
    #[error("channel `{0}` declares no capabilities")]
    ChannelWithoutCapabilities(String),
    #[error("channel `{0}` declares an invalid authentication callback policy")]
    InvalidAuthPolicy(String),
    #[error(
        "channel `{0}` defaults to the catalog model source without consuming injected catalog models"
    )]
    CatalogDefaultWithoutConsumption(String),
    #[error("top-level capabilities must equal the union of channel capabilities")]
    CapabilityUnionMismatch,
    #[error("duplicate config field key")]
    DuplicateConfigField,
    #[error("config field key must not be empty")]
    EmptyConfigFieldKey,
    #[error("secret config field `{0}` must not declare a default")]
    SecretDefault(String),
    #[error("config field `{0}` has invalid numeric bounds")]
    InvalidBounds(String),
    #[error("config field `{0}` has a default that does not match its type")]
    InvalidDefault(String),
    #[error("network declaration references unknown config field `{0}`")]
    UnknownNetworkField(String),
    #[error("network address field `{0}` must be a string")]
    NetworkFieldNotString(String),
    #[error("visible_when references unknown config field `{0}`")]
    UnknownConditionField(String),
    #[error("origin scheme `{0}` is not one of http/https/ws/wss")]
    BadOriginScheme(String),
    #[error("origin host `{0}` is empty or contains a wildcard")]
    BadOriginHost(String),
    #[error("enum field `{0}` declares no options")]
    EmptyEnum(String),
    #[error("enum field `{0}` declares a duplicate option value")]
    DuplicateEnumValue(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(provider_id: &str) -> ProviderDescriptor {
        let capabilities = BTreeSet::from([Capability::Infer]);
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
                default_base_url: None,
                default_models_source: None,
                consumes_catalog_models: false,
                capabilities: capabilities.clone(),
                model_capabilities: BTreeSet::new(),
                search_model_required: false,
            }],
            capabilities,
            config_fields: Vec::new(),
            network: NetworkDeclaration::default(),
            data_compat: DataCompatibility::default(),
        }
    }

    fn manifest(
        vendor_id: &str,
        kind: VendorKind,
        providers: Vec<ProviderDescriptor>,
    ) -> VendorDescriptor {
        VendorDescriptor {
            vendor_id: vendor_id.into(),
            version: semver::Version::new(1, 0, 0),
            display_name: "Test vendor".into(),
            description: None,
            authors: Vec::new(),
            canonical_format_version: 1,
            kind,
            providers,
        }
    }

    #[test]
    fn rejects_manifests_that_violate_package_profile_identity() {
        assert_eq!(
            manifest("not-base", VendorKind::Fallback, vec![provider("alpha")]).validate(),
            Err(DescriptorError::InvalidFallbackVendorId)
        );
        assert_eq!(
            manifest("alpha", VendorKind::Dedicated, vec![provider("beta")]).validate(),
            Err(DescriptorError::InvalidDedicatedProvider)
        );
        assert_eq!(
            manifest("base", VendorKind::Dedicated, vec![provider("base")]).validate(),
            Err(DescriptorError::InvalidDedicatedProvider)
        );
        assert_eq!(
            manifest(
                "base",
                VendorKind::Fallback,
                vec![provider("alpha"), provider("alpha")],
            )
            .validate(),
            Err(DescriptorError::DuplicateProvider("alpha".into()))
        );
    }

    #[test]
    fn channel_default_models_source_accepts_only_catalog() {
        let mut valid = provider("alpha");
        valid.channels[0].default_models_source = Some(DefaultModelsSource::Catalog);
        valid.channels[0].consumes_catalog_models = true;
        let descriptor = manifest("base", VendorKind::Fallback, vec![valid]);
        assert!(descriptor.validate().is_ok());

        let mut invalid = serde_json::to_value(descriptor).expect("descriptor JSON");
        invalid["providers"][0]["channels"][0]["default_models_source"] =
            serde_json::json!("https://inventory.test/models");
        assert!(serde_json::from_value::<VendorDescriptor>(invalid).is_err());
    }

    #[test]
    fn catalog_default_requires_consuming_injected_models() {
        let mut invalid = provider("alpha");
        invalid.channels[0].default_models_source = Some(DefaultModelsSource::Catalog);
        assert_eq!(
            manifest("base", VendorKind::Fallback, vec![invalid]).validate(),
            Err(DescriptorError::CatalogDefaultWithoutConsumption(
                "default".into()
            ))
        );
    }

    #[test]
    fn old_single_profile_manifest_shape_is_not_accepted() {
        let old = serde_json::json!({
            "vendor_id": "legacy",
            "version": "1.0.0",
            "display_name": "Legacy",
            "description": null,
            "authors": [],
            "channels": [],
            "capabilities": [],
            "config_fields": [],
            "network": {},
            "canonical_format_version": 1,
            "data_compat": {
                "config_fields_format": 1,
                "private_state_format": 1,
                "credentials_format": 1,
                "model_metadata_format": 1
            }
        });
        assert!(serde_json::from_value::<VendorDescriptor>(old).is_err());
    }
}
