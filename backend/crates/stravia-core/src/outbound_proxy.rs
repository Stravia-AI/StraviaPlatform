use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use crate::config::SystemProxySource;
use crate::storage::SettingsStore;
use hyper_util::client::proxy::matcher::Matcher;

pub const SETTINGS_KEY: &str = "outbound_proxy";
pub const UPDATE_PROXY_MODE_KEY: &str = "update_proxy_mode";
pub const PROXY_MODE_NONE: &str = "none";
pub const PROXY_MODE_SYSTEM: &str = "system";

pub const AI_IP_PROBE_SERVICES: [(&str, &str); 10] = [
    ("anthropic.com", "https://anthropic.com/cdn-cgi/trace"),
    ("claude.ai", "https://claude.ai/cdn-cgi/trace"),
    ("chatgpt.com", "https://chatgpt.com/cdn-cgi/trace"),
    ("openai.com", "https://openai.com/cdn-cgi/trace"),
    ("sora.com", "https://sora.com/cdn-cgi/trace"),
    ("grok.com", "https://grok.com/cdn-cgi/trace"),
    ("pixpix.com", "https://pixpix.com/cdn-cgi/trace"),
    ("perplexity.ai", "https://www.perplexity.ai/cdn-cgi/trace"),
    ("midjourney.com", "https://midjourney.com/cdn-cgi/trace"),
    ("mistral.ai", "https://mistral.ai/cdn-cgi/trace"),
];

pub fn default_proxy_mode() -> String {
    PROXY_MODE_SYSTEM.to_owned()
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum ProxySelection {
    None,
    #[default]
    System,
    Proxy(String),
}

impl ProxySelection {
    pub fn parse(raw: &str) -> anyhow::Result<Self> {
        let trimmed = raw.trim();
        anyhow::ensure!(!trimmed.is_empty(), "proxy_mode must not be empty");
        match trimmed {
            PROXY_MODE_NONE => Ok(Self::None),
            PROXY_MODE_SYSTEM => Ok(Self::System),
            id => Ok(Self::Proxy(id.to_owned())),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::None => PROXY_MODE_NONE,
            Self::System => PROXY_MODE_SYSTEM,
            Self::Proxy(id) => id.as_str(),
        }
    }

    pub fn proxy_id(&self) -> Option<&str> {
        match self {
            Self::Proxy(id) => Some(id.as_str()),
            Self::None | Self::System => None,
        }
    }
}

impl serde::Serialize for ProxySelection {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for ProxySelection {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct OutboundProxyEntry {
    pub id: String,
    pub name: String,
    pub url: String,
}

impl OutboundProxyEntry {
    pub fn normalize(&mut self) {
        self.id = self.id.trim().to_owned();
        if self.id.is_empty() {
            self.id = uuid::Uuid::new_v4().to_string();
        }
        self.name = self.name.trim().to_owned();
        self.url = self.url.trim().to_owned();
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let id = self.id.trim();
        anyhow::ensure!(!id.is_empty(), "outbound proxy id must not be empty");
        anyhow::ensure!(
            !id.eq_ignore_ascii_case(PROXY_MODE_NONE)
                && !id.eq_ignore_ascii_case(PROXY_MODE_SYSTEM),
            "outbound proxy id uses a reserved name"
        );
        validate_proxy_url(&self.url)
    }

    pub fn display_name(&self) -> String {
        let name = self.name.trim();
        if !name.is_empty() {
            return name.to_owned();
        }
        redacted_host_port(&self.url).unwrap_or_else(|| self.id.clone())
    }
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct OutboundProxySettings {
    pub proxies: Vec<OutboundProxyEntry>,
}

impl OutboundProxySettings {
    pub fn from_setting(value: Option<&str>) -> anyhow::Result<Self> {
        Ok(value
            .map(serde_json::from_str)
            .transpose()?
            .unwrap_or_default())
    }

    pub async fn load(settings: &dyn SettingsStore) -> anyhow::Result<Self> {
        Self::from_setting(settings.get(SETTINGS_KEY).await?.as_deref())
    }

    pub fn normalize(&mut self) {
        for entry in &mut self.proxies {
            entry.normalize();
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let mut seen_ids = HashSet::with_capacity(self.proxies.len());
        for entry in &self.proxies {
            entry.validate()?;
            anyhow::ensure!(
                seen_ids.insert(entry.id.trim().to_owned()),
                "duplicate outbound proxy id"
            );
        }
        Ok(())
    }

    pub fn find(&self, id: &str) -> Option<&OutboundProxyEntry> {
        self.proxies.iter().find(|entry| entry.id == id)
    }
}

pub fn validate_proxy_url(raw: &str) -> anyhow::Result<()> {
    let raw = raw.trim();
    anyhow::ensure!(!raw.is_empty(), "outbound proxy URL is empty");
    let url = reqwest::Url::parse(raw).map_err(|_| anyhow::anyhow!("invalid outbound proxy URL"))?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https" | "socks5" | "socks5h"),
        "unsupported outbound proxy URL scheme"
    );
    anyhow::ensure!(
        url.host_str().is_some(),
        "outbound proxy URL requires a host"
    );
    anyhow::ensure!(
        url.port() != Some(0),
        "outbound proxy URL requires a nonzero port"
    );
    anyhow::ensure!(
        url.path().is_empty() || url.path() == "/",
        "outbound proxy URL must not contain a path"
    );
    anyhow::ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "outbound proxy URL must not contain a query or fragment"
    );
    reqwest::Proxy::all(raw).map_err(|_| anyhow::anyhow!("invalid outbound proxy URL"))?;
    Ok(())
}

pub fn redacted_proxy_url(raw: &str) -> Option<String> {
    let mut url = reqwest::Url::parse(raw.trim()).ok()?;
    let _ = url.set_username("");
    let _ = url.set_password(None);
    Some(url.to_string())
}

fn redacted_host_port(raw: &str) -> Option<String> {
    let url = reqwest::Url::parse(raw.trim()).ok()?;
    let host = url.host_str()?;
    Some(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    })
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ResolvedOutboundProxy {
    #[default]
    Direct,
    System(SystemProxySource),
    Explicit {
        url: String,
        force_http1: bool,
    },
}

impl ResolvedOutboundProxy {
    pub fn resolve(
        selection: &ProxySelection,
        settings: &OutboundProxySettings,
        system_source: SystemProxySource,
    ) -> anyhow::Result<Self> {
        match selection {
            ProxySelection::None => Ok(Self::Direct),
            ProxySelection::System => Ok(Self::System(system_source)),
            ProxySelection::Proxy(id) => {
                let entry = settings
                    .find(id)
                    .ok_or_else(|| anyhow::anyhow!("configured outbound proxy was not found"))?;
                validate_proxy_url(&entry.url)?;
                Ok(Self::Explicit {
                    url: entry.url.trim().to_owned(),
                    force_http1: false,
                })
            }
        }
    }

    pub async fn resolve_from_store(
        selection: &ProxySelection,
        settings: &dyn SettingsStore,
        system_source: SystemProxySource,
    ) -> anyhow::Result<Self> {
        match selection {
            ProxySelection::None => Ok(Self::Direct),
            ProxySelection::System => Ok(Self::System(system_source)),
            ProxySelection::Proxy(_) => {
                let config = OutboundProxySettings::load(settings).await?;
                Self::resolve(selection, &config, system_source)
            }
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        match self {
            Self::Direct | Self::System(_) => Ok(()),
            Self::Explicit { url, .. } => validate_proxy_url(url),
        }
    }

    pub fn force_http1(&self) -> bool {
        match self {
            Self::Explicit { force_http1, .. } => *force_http1,
            Self::Direct | Self::System(_) => false,
        }
    }

    pub fn reqwest_client_config(
        &self,
    ) -> anyhow::Result<
        impl Fn(reqwest::ClientBuilder) -> reqwest::ClientBuilder + Send + Sync + 'static,
    > {
        match self {
            Self::Direct => Ok(Box::new(|builder: reqwest::ClientBuilder| builder.no_proxy())
                as Box<dyn Fn(reqwest::ClientBuilder) -> reqwest::ClientBuilder + Send + Sync>),
            Self::Explicit { url, .. } => {
                validate_proxy_url(url)?;
                let proxy = reqwest::Proxy::all(url.trim())
                    .map_err(|_| anyhow::anyhow!("invalid outbound proxy URL"))?;
                Ok(Box::new(move |builder: reqwest::ClientBuilder| {
                    builder.no_proxy().proxy(proxy.clone())
                }))
            }
            Self::System(source) => {
                let matcher = Arc::new(system_proxy_matcher(*source));
                let proxy = reqwest::Proxy::custom(move |url| {
                    intercept_matcher_for_url(&matcher, url)
                });
                Ok(Box::new(move |builder: reqwest::ClientBuilder| {
                    builder.no_proxy().proxy(proxy.clone())
                }))
            }
        }
    }

    pub fn web_access_mode(&self) -> stravia_web_access::OutboundProxyMode {
        match self {
            Self::Direct => stravia_web_access::OutboundProxyMode::Direct,
            Self::System(source) => {
                // 动态解析当前系统代理（供外部 Web Provider 适配器使用）。
                let matcher = system_proxy_matcher(*source);
                let probe_uri: http::Uri = http::Uri::from_static("https://example.com/");
                if let Some(intercept) = matcher.intercept(&probe_uri)
                    && let Some(url) = intercept_to_url(&intercept)
                {
                    stravia_web_access::OutboundProxyMode::Explicit {
                        url: url.to_string(),
                        bypass: String::new(),
                    }
                } else {
                    stravia_web_access::OutboundProxyMode::Direct
                }
            }
            Self::Explicit { url, .. } => stravia_web_access::OutboundProxyMode::Explicit {
                url: url.clone(),
                bypass: String::new(),
            },
        }
    }

    pub fn reuse_identity(&self) -> anyhow::Result<String> {
        let encoded = match self {
            Self::Direct => serde_json::to_vec(&"direct")?,
            Self::System(source) => {
                let tag = match source {
                    SystemProxySource::Environment => "system_env",
                    SystemProxySource::OperatingSystem => "system_os",
                };
                // 纳入当前解析到的系统代理快照，避免系统切换代理后误复用旧连接。
                let detected = describe_system_proxy(*source).detected_url;
                serde_json::to_vec(&(tag, detected))?
            }
            Self::Explicit { url, force_http1 } => {
                serde_json::to_vec(&("explicit", url, force_http1))?
            }
        };
        Ok(stravia_runtime_contract::protocol::ir::canonical::hash_hex(
            &stravia_runtime_contract::protocol::ir::canonical::hash_bytes(&encoded),
        ))
    }
}

pub fn system_proxy_matcher(source: SystemProxySource) -> Matcher {
    match source {
        SystemProxySource::Environment => Matcher::from_env(),
        SystemProxySource::OperatingSystem => {
            #[cfg(target_os = "linux")]
            if let Some(gnome) = linux_gnome_system_proxy_matcher() {
                return gnome;
            }
            Matcher::from_system()
        }
    }
}

#[cfg(target_os = "linux")]
fn linux_gnome_system_proxy_matcher() -> Option<Matcher> {
    use std::process::Command;

    fn gsettings_get(schema: &str, key: &str) -> Option<String> {
        let output = Command::new("gsettings")
            .args(["get", schema, key])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let value = String::from_utf8(output.stdout).ok()?;
        Some(value.trim().trim_matches('\'').to_owned())
    }

    if gsettings_get("org.gnome.system.proxy", "mode")?.as_str() != "manual" {
        return None;
    }

    let read_endpoint = |subschema: &str, scheme: &str| -> Option<String> {
        let host = gsettings_get(subschema, "host")?;
        if host.is_empty() {
            return None;
        }
        let port: u16 = gsettings_get(subschema, "port")?.parse().ok()?;
        if port == 0 {
            return None;
        }
        Some(format!("{scheme}://{host}:{port}"))
    };

    let http_proxy = read_endpoint("org.gnome.system.proxy.http", "http");
    let https_proxy = read_endpoint("org.gnome.system.proxy.https", "http");
    let socks_proxy = read_endpoint("org.gnome.system.proxy.socks", "socks5h");

    if http_proxy.is_none() && https_proxy.is_none() && socks_proxy.is_none() {
        return None;
    }

    let mut builder = Matcher::builder();
    if let Some(socks) = socks_proxy {
        builder = builder.all(socks);
    }
    if let Some(http) = http_proxy {
        builder = builder.http(http);
    }
    if let Some(https) = https_proxy {
        builder = builder.https(https);
    }
    if let Some(raw_ignore) = gsettings_get("org.gnome.system.proxy", "ignore-hosts") {
        let no_proxy = raw_ignore
            .trim_matches(|c| c == '[' || c == ']')
            .split(',')
            .map(|entry| entry.trim().trim_matches('\'').trim_matches('"'))
            .filter(|entry| !entry.is_empty())
            .collect::<Vec<_>>()
            .join(",");
        if !no_proxy.is_empty() {
            builder = builder.no(no_proxy);
        }
    }
    Some(builder.build())
}

pub fn intercept_to_url(
    intercept: &hyper_util::client::proxy::matcher::Intercept,
) -> Option<reqwest::Url> {
    let mut url = reqwest::Url::parse(&intercept.uri().to_string()).ok()?;
    if let Some(auth_header) = intercept.basic_auth()
        && let Ok(auth_str) = auth_header.to_str()
        && let Some(encoded) = auth_str.strip_prefix("Basic ")
        && let Ok(decoded) =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded)
        && let Ok(credentials) = String::from_utf8(decoded)
        && let Some((user, pass)) = credentials.split_once(':')
    {
        let _ = url.set_username(user);
        let _ = url.set_password(Some(pass));
    }
    Some(url)
}

fn intercept_matcher_for_url(matcher: &Matcher, target: &reqwest::Url) -> Option<reqwest::Url> {
    let uri: http::Uri = target.as_str().parse().ok()?;
    let intercept = matcher.intercept(&uri)?;
    intercept_to_url(&intercept)
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SystemProxySummary {
    pub source: String,
    pub detected_url: Option<String>,
}

pub fn describe_system_proxy(source: SystemProxySource) -> SystemProxySummary {
    let matcher = system_proxy_matcher(source);
    let https_uri = http::Uri::from_static("https://anthropic.com/");
    let http_uri = http::Uri::from_static("http://anthropic.com/");
    let detected_url = matcher
        .intercept(&https_uri)
        .or_else(|| matcher.intercept(&http_uri))
        .and_then(|intercept| redacted_proxy_url(&intercept.uri().to_string()));
    SystemProxySummary {
        source: match source {
            SystemProxySource::Environment => "env".to_owned(),
            SystemProxySource::OperatingSystem => "os".to_owned(),
        },
        detected_url,
    }
}

pub async fn update_proxy_mode(settings: &dyn SettingsStore) -> anyhow::Result<ProxySelection> {
    match settings.get(UPDATE_PROXY_MODE_KEY).await?.as_deref() {
        None => Ok(ProxySelection::System),
        Some(raw) => ProxySelection::parse(raw),
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ProxyIpProbeGroup {
    pub ip: String,
    pub country_code: Option<String>,
    pub services: Vec<String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ProxyIpProbeResult {
    pub groups: Vec<ProxyIpProbeGroup>,
    pub failed_services: Vec<String>,
    pub total_services: usize,
    pub probed_at: String,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct ProxyIpProbeRequest {
    #[serde(default)]
    pub proxy_mode: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

pub fn parse_cf_trace(body: &str) -> (Option<String>, Option<String>) {
    let mut ip = None;
    let mut loc = None;
    for line in body.lines() {
        if let Some((key, value)) = line.split_once('=') {
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            match key.trim() {
                "ip" => ip = Some(value.to_owned()),
                "loc" => {
                    let lower = value.to_ascii_lowercase();
                    if lower.len() == 2
                        && lower.bytes().all(|b| b.is_ascii_alphabetic())
                        && lower != "xx"
                        && lower != "t1"
                    {
                        loc = Some(lower);
                    }
                }
                _ => {}
            }
        }
    }
    (ip, loc)
}

pub fn aggregate_probe_observations(
    observations: Vec<(usize, String, Option<(String, Option<String>)>)>,
    probed_at: String,
) -> ProxyIpProbeResult {
    let total_services = observations.len();
    let mut failed_services = Vec::new();
    let mut grouped: Vec<(usize, ProxyIpProbeGroup)> = Vec::new();

    for (index, service, result) in observations {
        match result {
            Some((ip, country_code)) => {
                if let Some((_, existing)) = grouped.iter_mut().find(|(_, g)| g.ip == ip) {
                    existing.services.push(service);
                    if existing.country_code.is_none() && country_code.is_some() {
                        existing.country_code = country_code;
                    }
                } else {
                    grouped.push((
                        index,
                        ProxyIpProbeGroup {
                            ip,
                            country_code,
                            services: vec![service],
                        },
                    ));
                }
            }
            None => failed_services.push(service),
        }
    }

    // 相同 IP 聚合后按服务数量降序排序；数量相同按原始服务顺序排。
    grouped.sort_by(|(a_idx, a), (b_idx, b)| {
        b.services
            .len()
            .cmp(&a.services.len())
            .then_with(|| a_idx.cmp(b_idx))
    });

    ProxyIpProbeResult {
        groups: grouped.into_iter().map(|(_, group)| group).collect(),
        failed_services,
        total_services,
        probed_at,
    }
}

pub async fn probe_proxy_ai_ips(
    resolved: &ResolvedOutboundProxy,
) -> anyhow::Result<ProxyIpProbeResult> {
    let configure = resolved.reqwest_client_config()?;
    let client = configure(
        reqwest::Client::builder()
            .timeout(Duration::from_secs(6))
            .user_agent(format!("Stravia/{}", env!("CARGO_PKG_VERSION"))),
    )
    .build()
    .map_err(|_| anyhow::anyhow!("failed to build proxy probe HTTP client"))?;

    let futures = AI_IP_PROBE_SERVICES
        .iter()
        .enumerate()
        .map(|(index, (service, url))| {
            let client = client.clone();
            let service = (*service).to_owned();
            let url = *url;
            async move {
                let result = match client.get(url).send().await {
                    Ok(resp) if resp.status().is_success() => match resp.text().await {
                        Ok(body) => {
                            let (ip, loc) = parse_cf_trace(&body);
                            ip.map(|ip| (ip, loc))
                        }
                        Err(_) => None,
                    },
                    _ => None,
                };
                (index, service, result)
            }
        });

    let observations = futures::future::join_all(futures).await;
    Ok(aggregate_probe_observations(
        observations,
        chrono::Utc::now().to_rfc3339(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn invalid_urls_fail_without_exposing_credentials() {
        for url in [
            "",
            "ftp://user:secret@localhost",
            "not-a-url",
            "http://localhost:0",
            "http://localhost/path",
            "http://localhost?secret",
            "http://localhost#secret",
        ] {
            let entry = OutboundProxyEntry {
                id: "p1".into(),
                name: "Test".into(),
                url: url.into(),
            };
            let error = entry.validate().unwrap_err();
            assert!(!error.to_string().contains("secret"));
        }
        for url in [
            "http://localhost:80",
            "https://localhost",
            "socks5://localhost:1080",
            "socks5h://localhost:1080",
        ] {
            OutboundProxyEntry {
                id: "p1".into(),
                name: "Test".into(),
                url: url.into(),
            }
            .validate()
            .unwrap();
        }
        assert!(OutboundProxySettings::default().validate().is_ok());
        assert!(OutboundProxySettings::from_setting(Some(r#"{"enabled":true}"#)).is_err());
    }

    #[test]
    fn reserved_ids_and_duplicates_are_rejected() {
        for reserved in ["none", "system", "NONE", "System", ""] {
            let entry = OutboundProxyEntry {
                id: reserved.into(),
                name: "Bad".into(),
                url: "http://127.0.0.1:7890".into(),
            };
            assert!(entry.validate().is_err());
        }
        let settings = OutboundProxySettings {
            proxies: vec![
                OutboundProxyEntry {
                    id: "dup".into(),
                    name: "A".into(),
                    url: "http://127.0.0.1:7890".into(),
                },
                OutboundProxyEntry {
                    id: "dup".into(),
                    name: "B".into(),
                    url: "http://127.0.0.1:7891".into(),
                },
            ],
        };
        assert!(settings.validate().is_err());
    }

    #[test]
    fn cf_trace_parsing_and_group_sorting_put_most_common_first() {
        let (ip, loc) = parse_cf_trace("fl=123\nip=23.236.75.171\nloc=US\n");
        assert_eq!(ip.as_deref(), Some("23.236.75.171"));
        assert_eq!(loc.as_deref(), Some("us"));

        let result = aggregate_probe_observations(
            vec![
                (0, "anthropic.com".into(), Some(("1.1.1.1".into(), Some("us".into())))),
                (1, "claude.ai".into(), Some(("2.2.2.2".into(), Some("jp".into())))),
                (2, "chatgpt.com".into(), Some(("2.2.2.2".into(), Some("jp".into())))),
                (3, "openai.com".into(), Some(("2.2.2.2".into(), Some("jp".into())))),
                (4, "sora.com".into(), None),
                (5, "grok.com".into(), Some(("1.1.1.1".into(), Some("us".into())))),
            ],
            "2026-01-01T00:00:00Z".into(),
        );
        assert_eq!(result.total_services, 6);
        assert_eq!(result.failed_services, vec!["sora.com"]);
        assert_eq!(result.groups.len(), 2);
        assert_eq!(result.groups[0].ip, "2.2.2.2");
        assert_eq!(result.groups[0].country_code.as_deref(), Some("jp"));
        assert_eq!(
            result.groups[0].services,
            vec!["claude.ai", "chatgpt.com", "openai.com"]
        );
        assert_eq!(result.groups[1].ip, "1.1.1.1");
        assert_eq!(result.groups[1].services, vec!["anthropic.com", "grok.com"]);
    }

    #[tokio::test]
    async fn socks5h_sends_unresolved_domain_and_transports_http() -> anyhow::Result<()> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await?;
            let mut greeting = [0; 2];
            socket.read_exact(&mut greeting).await?;
            assert_eq!(greeting[0], 5);
            let mut methods = vec![0; usize::from(greeting[1])];
            socket.read_exact(&mut methods).await?;
            assert!(methods.contains(&0));
            socket.write_all(&[5, 0]).await?;
            let mut header = [0; 4];
            socket.read_exact(&mut header).await?;
            assert_eq!(header, [5, 1, 0, 3]);
            let length = socket.read_u8().await?;
            let mut host = vec![0; usize::from(length)];
            socket.read_exact(&mut host).await?;
            assert_eq!(host, b"unresolvable-proxy-test.invalid");
            assert_eq!(socket.read_u16().await?, 80);
            socket.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80]).await?;
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(socket.read_u8().await?);
                anyhow::ensure!(request.len() < 8192, "request header exceeded limit");
            }
            assert!(request.starts_with(b"GET /probe HTTP/1.1\r\n"));
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\nConnection: close\r\n\r\nsocks reached").await?;
            anyhow::Ok(())
        });
        let resolved = ResolvedOutboundProxy::Explicit {
            url: format!("socks5h://{address}"),
            force_http1: false,
        };
        let configure = resolved.reqwest_client_config()?;
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            let client = configure(reqwest::Client::builder()).build()?;
            assert_eq!(
                client
                    .get("http://unresolvable-proxy-test.invalid/probe")
                    .send()
                    .await?
                    .text()
                    .await?,
                "socks reached"
            );
            server.await??;
            anyhow::Ok(())
        })
        .await;
        result??;
        Ok(())
    }
}
