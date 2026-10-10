use crate::storage::SettingsStore;

pub const SETTINGS_KEY: &str = "outbound_proxy";
pub const UPDATE_USE_PROXY_KEY: &str = "update_use_proxy";

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct OutboundProxySettings {
    pub url: String,
    pub bypass: String,
    pub force_http1: bool,
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
        self.url = self.url.trim().to_owned();
        self.bypass = canonical_bypass(&self.bypass);
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let raw = self.url.trim();
        if raw.is_empty() {
            return Ok(());
        }
        let url =
            reqwest::Url::parse(raw).map_err(|_| anyhow::anyhow!("invalid outbound proxy URL"))?;
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

    pub fn reqwest_client_config(
        &self,
    ) -> anyhow::Result<
        impl Fn(reqwest::ClientBuilder) -> reqwest::ClientBuilder + Send + Sync + 'static,
    > {
        let proxy = self.reqwest_proxy()?;
        Ok(move |builder: reqwest::ClientBuilder| builder.no_proxy().proxy(proxy.clone()))
    }

    pub fn reqwest_proxy(&self) -> anyhow::Result<reqwest::Proxy> {
        self.validate()?;
        anyhow::ensure!(!self.url.trim().is_empty(), "outbound proxy URL is empty");
        let bypass = canonical_bypass(&self.bypass);
        Ok(reqwest::Proxy::all(self.url.trim())
            .map_err(|_| anyhow::anyhow!("invalid outbound proxy URL"))?
            .no_proxy(reqwest::NoProxy::from_string(&bypass)))
    }
}

fn canonical_bypass(value: &str) -> String {
    let mut canonical = String::with_capacity(value.len());
    for entry in value.split(|ch: char| ch == ',' || ch == ';' || ch.is_ascii_whitespace()) {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        if !canonical.is_empty() {
            canonical.push(',');
        }
        canonical.push_str(entry);
    }
    canonical.make_ascii_lowercase();
    canonical
}

pub async fn update_uses_proxy(settings: &dyn SettingsStore) -> anyhow::Result<bool> {
    match settings.get(UPDATE_USE_PROXY_KEY).await?.as_deref() {
        None | Some("false") => Ok(false),
        Some("true") => Ok(true),
        Some(_) => anyhow::bail!("update_use_proxy must be true or false"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn invalid_urls_fail_without_exposing_credentials() {
        for url in [
            "ftp://user:secret@localhost",
            "not-a-url",
            "http://localhost:0",
            "http://localhost/path",
            "http://localhost?secret",
            "http://localhost#secret",
        ] {
            let config = OutboundProxySettings {
                url: url.into(),
                ..Default::default()
            };
            let error = config.validate().unwrap_err();
            assert!(!error.to_string().contains("secret"));
            assert!(config.reqwest_proxy().is_err());
        }
        for url in [
            "http://localhost:80",
            "https://localhost",
            "socks5://localhost:1080",
            "socks5h://localhost:1080",
        ] {
            OutboundProxySettings {
                url: url.into(),
                ..Default::default()
            }
            .reqwest_proxy()
            .unwrap();
        }
        assert!(OutboundProxySettings::default().validate().is_ok());
        assert!(OutboundProxySettings::default().reqwest_proxy().is_err());
        assert!(OutboundProxySettings::from_setting(Some(r#"{"enabled":true}"#)).is_err());
        assert!(OutboundProxySettings::from_setting(Some(r#"{"url":"ftp://localhost"}"#)).is_ok());
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
        let config = OutboundProxySettings {
            url: format!("socks5h://{address}"),
            ..Default::default()
        };
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let client = reqwest::Client::builder()
                .no_proxy()
                .proxy(config.reqwest_proxy()?)
                .build()?;
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
