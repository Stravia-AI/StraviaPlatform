use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use stravia_vendor_sdk::{
    Capability, ConfigFieldKind, MODELS_SOURCE_CATALOG, OriginDeclaration, OriginHost,
    ProviderDescriptor,
};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum NetworkOrigin {
    Exact(String),
    HostRegex {
        scheme: String,
        port: u16,
        regex: String,
    },
}

impl std::fmt::Display for NetworkOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exact(origin) => f.write_str(origin),
            Self::HostRegex {
                scheme,
                port,
                regex,
            } => {
                write!(
                    f,
                    "{scheme} host regex /{regex}/ (full host match, port {port})"
                )
            }
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct NetworkPolicy(std::sync::Arc<CompiledNetworkPolicy>);

#[derive(Default)]
struct CompiledNetworkPolicy {
    exact: BTreeSet<String>,
    regex: Vec<(String, u16, regex::Regex)>,
}

impl NetworkPolicy {
    pub(crate) fn compile(
        origins: impl IntoIterator<Item = NetworkOrigin>,
    ) -> anyhow::Result<Self> {
        let mut policy = CompiledNetworkPolicy::default();
        for origin in origins {
            match origin {
                NetworkOrigin::Exact(origin) => {
                    policy.exact.insert(origin);
                }
                NetworkOrigin::HostRegex {
                    scheme,
                    port,
                    regex,
                } => {
                    // 先验证原始语法，避免不平衡括号借外层分组逃逸完整 host 边界。
                    // 无效声明使整个快照失败，不能静默丢弃权限后继续执行。
                    regex::Regex::new(&regex)?;
                    let regex = regex::Regex::new(&format!(r"\A(?:{regex})\z"))?;
                    policy.regex.push((scheme, port, regex));
                }
            }
        }
        Ok(Self(std::sync::Arc::new(policy)))
    }

    pub(crate) fn allows(&self, url: &url::Url) -> bool {
        self.0.exact.contains(&url.origin().ascii_serialization())
            || url.host_str().is_some_and(|host| {
                self.0.regex.iter().any(|(scheme, port, regex)| {
                    url.scheme() == scheme
                        && url.port_or_known_default() == Some(*port)
                        && regex.is_match(host)
                })
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NetworkGrant {
    pub(crate) origin: NetworkOrigin,
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
            // 只有已声明且由管理员开启的开关才能补充对应 WS origin；
            // 不将普通 HTTP 授权隐式扩展到 WebSocket，也不放宽 host/port。
            if options.get("use_websocket").and_then(Value::as_bool) == Some(true)
                && descriptor.config_fields.iter().any(|field| {
                    field.key == "use_websocket"
                        && !field.secret
                        && matches!(field.kind, ConfigFieldKind::Bool)
                })
            {
                let mut url = url::Url::parse(value)?;
                let scheme = match url.scheme() {
                    "https" => "wss",
                    "http" => "ws",
                    _ => anyhow::bail!("WebSocket transport requires an HTTP base URL"),
                };
                url.set_scheme(scheme)
                    .map_err(|_| anyhow::anyhow!("invalid WebSocket URL scheme"))?;
                grants.push(NetworkGrant {
                    origin: NetworkOrigin::Exact(url.origin().ascii_serialization()),
                    configuration_field: Some("use_websocket".into()),
                    connection_scoped: true,
                });
            }
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

fn declared_origin(declaration: &OriginDeclaration) -> anyhow::Result<NetworkOrigin> {
    anyhow::ensure!(
        matches!(declaration.scheme.as_str(), "http" | "https" | "ws" | "wss"),
        "network permission requires an HTTP or WebSocket scheme"
    );
    let host = match &declaration.host {
        OriginHost::Exact(host) => host,
        OriginHost::Regex { regex } => {
            return Ok(NetworkOrigin::HostRegex {
                scheme: declaration.scheme.clone(),
                port: declaration.port.unwrap_or({
                    if matches!(declaration.scheme.as_str(), "https" | "wss") {
                        443
                    } else {
                        80
                    }
                }),
                regex: regex.clone(),
            });
        }
    };
    let mut url = url::Url::parse(&format!("{}://localhost", declaration.scheme))?;
    url.set_host(Some(host))
        .map_err(|_| anyhow::anyhow!("invalid declared network origin host"))?;
    url.set_port(declaration.port)
        .map_err(|_| anyhow::anyhow!("invalid declared network origin port"))?;
    exact_origin(url.as_str())
}

fn exact_origin(value: &str) -> anyhow::Result<NetworkOrigin> {
    let url = url::Url::parse(value.trim())?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https" | "ws" | "wss")
            && url.host().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && !url.host_str().is_some_and(|host| host.contains('*')),
        "network permission requires an exact HTTP or WebSocket origin without credentials"
    );
    Ok(NetworkOrigin::Exact(url.origin().ascii_serialization()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regex_policy(pattern: &str, port: Option<u16>) -> NetworkPolicy {
        NetworkPolicy::compile([declared_origin(&OriginDeclaration {
            scheme: "https".into(),
            host: OriginHost::Regex {
                regex: pattern.into(),
            },
            port,
        })
        .unwrap()])
        .unwrap()
    }

    #[test]
    fn regex_matches_only_full_canonical_host_with_declared_scheme_and_effective_port() {
        let policy = regex_policy(r"cdn[0-9]+\.example\.com", None);
        assert!(policy.allows(&url::Url::parse("https://CDN12.EXAMPLE.COM/image.png").unwrap()));
        assert!(policy.allows(&url::Url::parse("https://cdn12.example.com:443/").unwrap()));
        for value in [
            "https://evilcdn12.example.com/",
            "https://cdn12.example.com.evil.test/",
            "https://evil.test/cdn12.example.com",
            "http://cdn12.example.com/",
            "wss://cdn12.example.com/",
            "https://cdn12.example.com:8443/",
        ] {
            assert!(!policy.allows(&url::Url::parse(value).unwrap()), "{value}");
        }
        let alternate = regex_policy(r"cdn[0-9]+\.example\.com", Some(8443));
        assert!(alternate.allows(&url::Url::parse("https://cdn1.example.com:8443/").unwrap()));
        assert!(!alternate.allows(&url::Url::parse("https://cdn1.example.com/").unwrap()));
    }

    #[test]
    fn malformed_regex_rejects_the_entire_permission_snapshot() {
        let mut rules = vec![
            declared_origin(&OriginDeclaration {
                scheme: "https".into(),
                host: OriginHost::Regex { regex: "[".into() },
                port: None,
            })
            .unwrap(),
        ];
        rules.push(exact_origin("https://saved.example.com/path").unwrap());
        assert!(NetworkPolicy::compile(rules).is_err());
    }

    #[test]
    fn malformed_regex_cannot_escape_host_wrapper() {
        let rule = NetworkOrigin::HostRegex {
            scheme: "https".into(),
            port: 443,
            regex: r"x)\z|(?:.*".into(),
        };
        assert!(NetworkPolicy::compile([rule]).is_err());
    }

    #[test]
    fn administrator_saved_values_remain_exact_origins() {
        let policy =
            NetworkPolicy::compile([exact_origin("https://SAVED.example.com:443/path").unwrap()])
                .unwrap();
        assert!(policy.allows(&url::Url::parse("https://saved.example.com/other").unwrap()));
        for value in [
            "https://sub.saved.example.com/",
            "http://saved.example.com/",
            "https://saved.example.com:8443/",
        ] {
            assert!(!policy.allows(&url::Url::parse(value).unwrap()));
        }
        assert!(exact_origin("https://*.example.com").is_err());
        assert!(exact_origin("https://user:pass@example.com").is_err());
    }

    #[test]
    fn websocket_opt_in_grants_only_the_saved_base_counterpart() {
        let mut descriptor: ProviderDescriptor = serde_json::from_value(serde_json::json!({
            "provider_id": "transport-test",
            "display_name": "Transport test",
            "channels": [],
            "capabilities": [],
            "config_fields": [{
                "key": "use_websocket",
                "label": {"en-US": "Use WebSocket"},
                "kind": {"type": "bool"},
                "required": false,
                "secret": false
            }]
        }))
        .unwrap();
        for (base, paired) in [
            (
                "https://saved.example.com/v1",
                "wss://saved.example.com/responses",
            ),
            (
                "http://saved.example.com:8080/v1",
                "ws://saved.example.com:8080/responses",
            ),
        ] {
            for enabled in [None, Some(false), Some(true)] {
                let options = enabled
                    .map(|enabled| BTreeMap::from([("use_websocket".into(), Value::Bool(enabled))]))
                    .unwrap_or_default();
                let grants = resolve_permissions(
                    &descriptor,
                    Some(base),
                    None,
                    None,
                    &options,
                    &BTreeMap::new(),
                )
                .unwrap();
                let policy =
                    NetworkPolicy::compile(grants.into_iter().map(|grant| grant.origin)).unwrap();
                assert!(policy.allows(&url::Url::parse(base).unwrap()));
                assert_eq!(
                    policy.allows(&url::Url::parse(paired).unwrap()),
                    enabled == Some(true)
                );
                for excluded in [
                    "wss://other.example.com/",
                    "wss://sub.saved.example.com/",
                    "wss://saved.example.com:8443/",
                    "ws://saved.example.com:9090/",
                ] {
                    assert!(
                        !policy.allows(&url::Url::parse(excluded).unwrap()),
                        "{excluded}"
                    );
                }
            }
        }
        descriptor.config_fields.clear();
        let grants = resolve_permissions(
            &descriptor,
            Some("https://saved.example.com/v1"),
            None,
            None,
            &BTreeMap::from([("use_websocket".into(), Value::Bool(true))]),
            &BTreeMap::new(),
        )
        .unwrap();
        let policy = NetworkPolicy::compile(grants.into_iter().map(|grant| grant.origin)).unwrap();
        assert!(!policy.allows(&url::Url::parse("wss://saved.example.com/").unwrap()));
    }
}
