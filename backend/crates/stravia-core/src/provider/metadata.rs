//! Vendor metadata types.
//!
//! Runtime defaults for provider protocol resolution. Each vendor module owns
//! a `const METADATA: VendorMetadata` and registers itself via
//! `inventory::submit!`.

use serde::Serialize;

/// Where to fetch model capability data for a provider channel.
///
/// This is a compile-time preset concern — not user-configurable. The admin
/// layer reads this from the vendor preset to decide how to resolve capabilities
/// without consulting a DB column.
#[derive(Debug, Clone, Copy)]
pub enum CapabilitiesSource {
    /// Resolve exact Provider Catalog Entries for the given catalog Provider ID.
    Catalog(&'static str),
    /// Query an HTTP endpoint (e.g. Ollama `/api/show`, OpenRouter `/v1/models`).
    Http(&'static str),
    /// No catalog or provider-specific capability source is available.
    Auto,
}

/// Bilingual label.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Label {
    pub zh: &'static str,
    pub en: &'static str,
}

/// Control used to collect one Adapter Credential value.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialInputKind {
    Text,
    Password,
    Textarea,
}

/// A credential field declared by a Vendor from its npm provider settings.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialFieldDef {
    pub key: &'static str,
    /// Canonical English fallback for clients that do not localize this field.
    pub label: &'static str,
    pub secret: bool,
    pub required: bool,
    pub input: CredentialInputKind,
}

pub const API_KEY_CREDENTIAL_FIELDS: &[CredentialFieldDef] = &[CredentialFieldDef {
    key: "apiKey",
    label: "API key",
    secret: true,
    required: true,
    input: CredentialInputKind::Password,
}];

/// Authentication mode advertised to the WebUI.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    ApiKey,
    OAuth,
    SetupToken,
}

/// (protocol_alias, base_url) pair.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ProtocolBaseUrl {
    pub protocol: &'static str,
    pub base_url: &'static str,
}

/// OAuth configuration for a channel.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthConfig {
    pub auth_base_url: &'static str,
    pub authorize_url: &'static str,
    pub token_url: &'static str,
    pub client_id: &'static str,
    pub redirect_uri: &'static str,
    pub scope: &'static str,
}

/// Runtime hints used by OAuth drivers (currently only Codex).
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeConfig {
    pub api_base_url: &'static str,
    pub models_url: &'static str,
    pub models_client_version: &'static str,
}

/// One channel under a vendor (e.g. `openai/default`, `openai/codex`).
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelDef {
    pub id: &'static str,
    pub label: Label,
    #[serde(
        serialize_with = "serialize_base_urls",
        skip_serializing_if = "<[ProtocolBaseUrl]>::is_empty"
    )]
    pub base_urls: &'static [ProtocolBaseUrl],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models_source: Option<&'static str>,
    /// Internal preset capability strategy — never serialized to the wire API.
    #[serde(skip)]
    pub capabilities_source: CapabilitiesSource,
    #[serde(skip_serializing_if = "<[&str]>::is_empty")]
    pub static_models: &'static [&'static str],
    pub auth_mode: AuthMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeConfig>,
}

/// Top-level vendor entry. One `VendorMetadata` per vendor.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VendorMetadata {
    pub id: &'static str,
    pub label: Label,
    pub icon: &'static str,
    pub default_protocol: &'static str,
    #[serde(skip_serializing_if = "<[CredentialFieldDef]>::is_empty")]
    pub credential_fields: &'static [CredentialFieldDef],
    pub channels: &'static [ChannelDef],
}

fn serialize_base_urls<S>(base_urls: &&[ProtocolBaseUrl], serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeMap;
    let mut map = serializer.serialize_map(Some(base_urls.len()))?;
    for entry in base_urls.iter() {
        map.serialize_entry(entry.protocol, entry.base_url)?;
    }
    map.end()
}
