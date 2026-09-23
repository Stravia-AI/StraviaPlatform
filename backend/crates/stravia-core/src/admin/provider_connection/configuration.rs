use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use stravia_vendor_sdk::{
    ConfigField, ConfigFieldKind, LocalizedText, ProviderDescriptor, ValidationIssue,
};

use crate::plugin::permissions::resolve_permissions;

mod messages {
    include!(concat!(env!("OUT_DIR"), "/messages.rs"));
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderNetworkPermission {
    pub origin: String,
    pub configuration_field: Option<String>,
    pub connection_scoped: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfigurationPreviewInput {
    #[serde(default)]
    pub provider_id: Option<String>,
    pub vendor_id: String,
    pub channel: String,
    pub base_url: String,
    #[serde(default)]
    pub options: BTreeMap<String, Value>,
    #[serde(default)]
    pub credentials: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderConfigurationPreview {
    pub base_url: String,
    pub issues: Vec<ValidationIssue>,
    pub network_permissions: Vec<ProviderNetworkPermission>,
}

pub(super) fn require_descriptor(
    plugins: &crate::plugin::manager::VendorPlugins,
    vendor_id: &str,
) -> anyhow::Result<ProviderDescriptor> {
    let vendor_id = vendor_id.trim();
    anyhow::ensure!(!vendor_id.is_empty(), "provider vendor is required");
    plugins
        .descriptor(vendor_id)
        .map_err(|_| anyhow::anyhow!("Vendor `{vendor_id}` is not installed"))
}

pub(super) struct NormalizedConfiguration {
    pub(super) options: BTreeMap<String, Value>,
    pub(super) credentials: BTreeMap<String, Value>,
    pub(super) issues: Vec<ValidationIssue>,
}

pub(super) fn validate_configuration_fields(
    descriptor: &ProviderDescriptor,
    mut options: BTreeMap<String, Value>,
    credentials: BTreeMap<String, Value>,
) -> anyhow::Result<NormalizedConfiguration> {
    for key in options.keys() {
        let field = descriptor
            .config_fields
            .iter()
            .find(|field| field.key == *key)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Vendor `{}` does not declare configuration field `{key}`",
                    descriptor.provider_id
                )
            })?;
        anyhow::ensure!(
            !field.secret,
            "Secret configuration field `{key}` must not be stored in vendor_options"
        );
    }
    for key in credentials.keys() {
        let field = descriptor
            .config_fields
            .iter()
            .find(|field| field.key == *key)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Vendor `{}` does not declare credential field `{key}`",
                    descriptor.provider_id
                )
            })?;
        anyhow::ensure!(
            field.secret,
            "Non-secret configuration field `{key}` must be stored in vendor_options"
        );
    }

    for field in &descriptor.config_fields {
        if !field.secret
            && !options.contains_key(&field.key)
            && let Some(default) = &field.default_json
        {
            options.insert(field.key.clone(), default.clone());
        }
    }

    let merged = descriptor
        .config_fields
        .iter()
        .filter_map(|field| {
            options
                .get(&field.key)
                .cloned()
                .or_else(|| credentials.get(&field.key).cloned())
                .map(|value| (field.key.clone(), value))
        })
        .collect::<BTreeMap<_, _>>();

    let mut issues = Vec::new();
    for field in &descriptor.config_fields {
        let active = field
            .visible_when
            .as_ref()
            .is_none_or(|condition| merged.get(&condition.field) == Some(&condition.equals));
        let value = merged.get(&field.key);
        if active && field.required && value.is_none_or(empty_value) {
            issues.push(ValidationIssue {
                field: Some(field.key.clone()),
                code: "required".into(),
                message: messages::config_required(&field.key),
            });
            continue;
        }
        if let Some(value) = value
            && let Err(error) = validate_field_value(field, value)
        {
            issues.push(ValidationIssue {
                field: Some(field.key.clone()),
                code: "invalid_value".into(),
                message: error,
            });
        }
    }

    Ok(NormalizedConfiguration {
        options,
        credentials,
        issues,
    })
}

pub(super) fn validate_persisted_configuration_fields(
    descriptor: &ProviderDescriptor,
    options: Map<String, Value>,
    credentials: BTreeMap<String, Value>,
    allow_missing_secrets: bool,
) -> anyhow::Result<(Map<String, Value>, BTreeMap<String, Value>)> {
    let credentials_json = credentials.clone();
    let mut relaxed;
    let descriptor = if allow_missing_secrets {
        relaxed = descriptor.clone();
        for field in &mut relaxed.config_fields {
            if field.secret {
                field.required = false;
            }
        }
        &relaxed
    } else {
        descriptor
    };
    let NormalizedConfiguration {
        options, issues, ..
    } = validate_configuration_fields(descriptor, options.into_iter().collect(), credentials_json)?;
    anyhow::ensure!(
        issues.is_empty(),
        "vendor configuration validation failed: {}",
        serde_json::to_string(&issues)?
    );
    Ok((options.into_iter().collect(), credentials))
}

fn empty_value(value: &Value) -> bool {
    value.as_str().is_some_and(|value| value.trim().is_empty())
}

fn validate_field_value(field: &ConfigField, value: &Value) -> Result<(), LocalizedText> {
    let number = match &field.kind {
        ConfigFieldKind::Bool => {
            if !value.is_boolean() {
                return Err(messages::config_boolean(&field.key));
            }
            None
        }
        ConfigFieldKind::String { .. } => {
            let value = value
                .as_str()
                .ok_or_else(|| messages::config_string(&field.key))?;
            if let Some(max_length) = field.max_length
                && value.chars().count() > max_length as usize
            {
                return Err(messages::config_max_length(
                    &field.key,
                    &max_length.to_string(),
                ));
            }
            if let Some(pattern) = &field.pattern {
                let regex = regex::Regex::new(pattern)
                    .map_err(|_| messages::config_invalid_pattern(&field.key))?;
                if !regex.is_match(value) {
                    return Err(messages::config_pattern(&field.key));
                }
            }
            None
        }
        ConfigFieldKind::Int => {
            if value.as_i64().is_none() && value.as_u64().is_none() {
                return Err(messages::config_integer(&field.key));
            }
            value.as_f64()
        }
        ConfigFieldKind::Decimal => {
            let number = value
                .as_f64()
                .filter(|value| value.is_finite())
                .ok_or_else(|| messages::config_decimal(&field.key))?;
            Some(number)
        }
        ConfigFieldKind::Enum { options } => {
            let value = value
                .as_str()
                .ok_or_else(|| messages::config_enum_type(&field.key))?;
            if !options.iter().any(|option| option.value == value) {
                return Err(messages::config_enum_value(&field.key));
            }
            None
        }
    };
    if let Some(number) = number {
        if let Some(min) = field.min
            && number < min
        {
            return Err(messages::config_minimum(&field.key, &min.to_string()));
        }
        if let Some(max) = field.max
            && number > max
        {
            return Err(messages::config_maximum(&field.key, &max.to_string()));
        }
    }
    Ok(())
}

pub(super) fn validate_provider_base_url(value: &str) -> anyhow::Result<String> {
    let url = reqwest::Url::parse(value.trim())
        .map_err(|_| anyhow::anyhow!("Provider Base URL is invalid"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        anyhow::bail!(
            "Provider Base URL must contain only an HTTP(S) origin and optional base path"
        );
    }
    Ok(url.to_string().trim_end_matches('/').to_owned())
}

pub(super) fn resolved_permissions(
    descriptor: &ProviderDescriptor,
    base_url: &str,
    options: &BTreeMap<String, Value>,
    credentials: &BTreeMap<String, Value>,
) -> anyhow::Result<Vec<ProviderNetworkPermission>> {
    resolve_permissions(descriptor, Some(base_url), None, None, options, credentials).map(
        |grants| {
            grants
                .into_iter()
                .map(|grant| ProviderNetworkPermission {
                    origin: grant.origin,
                    configuration_field: grant.configuration_field,
                    connection_scoped: grant.connection_scoped,
                })
                .collect()
        },
    )
}

#[cfg(test)]
mod image_base_url_tests {
    use super::validate_provider_base_url;

    #[test]
    fn image_provider_base_url_rejects_credential_query_and_fragment_smuggling() {
        for value in [
            "https://user:secret@example.com/v1",
            "https://example.com/v1?api_key=secret",
            "https://example.com/v1#images",
        ] {
            assert!(validate_provider_base_url(value).is_err(), "{value}");
        }
        assert_eq!(
            validate_provider_base_url("https://example.com/base/v1/").unwrap(),
            "https://example.com/base/v1"
        );
    }
}
