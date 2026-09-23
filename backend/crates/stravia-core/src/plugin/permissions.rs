use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use stravia_vendor_sdk::{
    Capability, MODELS_SOURCE_CATALOG, OriginDeclaration, ProviderDescriptor,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NetworkGrant {
    pub(crate) origin: String,
    pub(crate) configuration_field: Option<String>,
    pub(crate) connection_scoped: bool,
}

/// Resolves descriptor declarations and administrator-saved connection endpoints.
/// Dynamic origins come from the already-loaded connection snapshot and are never
/// accepted from guest output or private state.
pub(crate) fn resolve_permissions(
    descriptor: &ProviderDescriptor,
    base_url: Option<&str>,
    models_source: Option<&str>,
    static_models: Option<&str>,
    options: &BTreeMap<String, Value>,
    credentials: &BTreeMap<String, Value>,
) -> anyhow::Result<Vec<NetworkGrant>> {
    let mut grants = Vec::new();
    for declaration in &descriptor.network.extra_origins {
        grants.push(NetworkGrant {
            origin: declared_origin(declaration)?,
            configuration_field: None,
            connection_scoped: false,
        });
    }

    if let Some(provider_base_url) = base_url {
        let configured_base = match descriptor.network.base_url_field.as_deref() {
            Some(field) => configured_string(field, options, credentials)
                .map(|value| (value, Some(field.to_owned()))),
            None => (!provider_base_url.trim().is_empty()).then_some((provider_base_url, None)),
        };
        // 未填地址不授予 origin；配置验证仍可先计算供管理员确认的地址。
        if let Some((value, field)) = configured_base {
            grants.push(NetworkGrant {
                origin: exact_origin(value)?,
                configuration_field: field,
                connection_scoped: true,
            });
        }

        for field in &descriptor.network.field_origins {
            if let Some(value) = configured_string(field, options, credentials) {
                grants.push(NetworkGrant {
                    origin: exact_origin(value)?,
                    configuration_field: Some(field.clone()),
                    connection_scoped: true,
                });
            }
        }
    }

    if descriptor
        .capabilities
        .contains(&Capability::ModelDiscovery)
        && static_models.is_none_or(|models| models.trim().is_empty())
        && let Some(source) = models_source
            .map(str::trim)
            .filter(|source| !source.is_empty() && *source != MODELS_SOURCE_CATALOG)
    {
        grants.push(NetworkGrant {
            origin: exact_origin(source)?,
            configuration_field: Some("models_source".to_owned()),
            connection_scoped: true,
        });
    }

    // A descriptor may name the same origin through more than one declaration.
    // Preserve distinct field attribution, but remove exact duplicate grants.
    let mut seen = BTreeSet::new();
    grants.retain(|grant| {
        seen.insert((
            grant.origin.clone(),
            grant.configuration_field.clone(),
            grant.connection_scoped,
        ))
    });
    Ok(grants)
}

fn configured_string<'a>(
    field: &str,
    options: &'a BTreeMap<String, Value>,
    credentials: &'a BTreeMap<String, Value>,
) -> Option<&'a str> {
    options
        .get(field)
        .and_then(Value::as_str)
        .or_else(|| credentials.get(field).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn declared_origin(declaration: &OriginDeclaration) -> anyhow::Result<String> {
    let mut url = url::Url::parse(&format!("{}://localhost", declaration.scheme))?;
    url.set_host(Some(&declaration.host))
        .map_err(|_| anyhow::anyhow!("invalid declared network origin host"))?;
    url.set_port(declaration.port)
        .map_err(|_| anyhow::anyhow!("invalid declared network origin port"))?;
    exact_origin(url.as_str())
}

fn exact_origin(value: &str) -> anyhow::Result<String> {
    let url = url::Url::parse(value.trim())?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https" | "ws" | "wss")
            && url.host().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && !url.host_str().is_some_and(|host| host.contains('*')),
        "network permission requires an exact HTTP or WebSocket origin without credentials"
    );
    Ok(url.origin().ascii_serialization())
}
