use std::{sync::Arc, time::Duration};

use futures::StreamExt;
use reqwest::{StatusCode, Url, header, redirect::Policy};
use serde::Deserialize;

use super::MarketError;
use crate::outbound_proxy::{OutboundProxySettings, update_uses_proxy};
use crate::storage::DynStorage;

const DEFAULT_UPDATER: &str = "https://updater.stravia.cn";
const PAGE_LIMIT: usize = 1024 * 1024;
const MAX_ARTIFACT: usize = 64 * 1024 * 1024;
pub type DownloadProgress = Arc<dyn Fn(u64, Option<u64>) + Send + Sync>;

#[derive(Debug, Clone)]
pub struct Repository {
    pub id: u64,
    pub name: String,
    pub url: String,
    pub mirrored: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub id: u64,
    #[serde(rename = "tag_name", alias = "tag")]
    pub tag: String,
    #[serde(rename = "html_url")]
    pub url: String,
    pub draft: bool,
    pub prerelease: bool,
    #[serde(default)]
    pub assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReleaseAsset {
    pub id: u64,
    pub name: String,
    pub size: u64,
}

#[derive(Deserialize)]
struct RepositoryResponse {
    id: u64,
    #[serde(alias = "repository")]
    full_name: String,
    private: bool,
    #[serde(default)]
    mirrored: Option<bool>,
}

fn error(code: &str, message: impl Into<String>) -> MarketError {
    MarketError {
        code: code.to_owned(),
        message: message.into(),
    }
}

/// Parse only canonical GitHub repository URLs, never arbitrary download URLs.
pub fn parse_repository_url(input: &str) -> Result<String, MarketError> {
    let invalid = || {
        error(
            "network",
            "Expected https://github.com/owner/repository without credentials, port, query, fragment or subpath",
        )
    };
    let url = Url::parse(input).map_err(|_| invalid())?;
    // Check the authority before URL normalization, which removes an explicit :443.
    let rest = input.strip_prefix("https://").ok_or_else(invalid)?;
    let (authority, path) = rest.split_once('/').ok_or_else(invalid)?;
    if !authority.eq_ignore_ascii_case("github.com")
        || url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || path.contains('%')
    {
        return Err(invalid());
    }
    let path = path.strip_suffix('/').unwrap_or(path);
    let (owner, repo) = path.split_once('/').ok_or_else(invalid)?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    if owner.is_empty()
        || repo.is_empty()
        || owner.len() > 39
        || repo.len() > 100
        || !owner
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        || !repo
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
        || owner.starts_with('-')
        || owner.ends_with('-')
        || repo == "."
        || repo == ".."
    {
        return Err(invalid());
    }
    Ok(format!("{owner}/{repo}"))
}

pub struct MarketClient {
    storage: DynStorage,
    updater_base: Option<String>,
    #[cfg(any(test, feature = "test-support"))]
    test_origins: Option<(Url, Url)>,
}

impl MarketClient {
    pub fn new(storage: DynStorage, updater_base: Option<String>) -> Self {
        Self {
            storage,
            updater_base,
            #[cfg(any(test, feature = "test-support"))]
            test_origins: None,
        }
    }

    /// Explicitly scoped local origins for integration fixtures, never enabled in production builds.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_test_origins(
        storage: DynStorage,
        updater_base: String,
        github_api_base: String,
        github_web_base: String,
    ) -> Result<Self, MarketError> {
        fn local(input: &str) -> Result<Url, MarketError> {
            let mut url = Url::parse(input).map_err(|e| error("network", e.to_string()))?;
            let loopback = url
                .host_str()
                .and_then(|h| h.parse::<std::net::IpAddr>().ok())
                .is_some_and(|ip| ip.is_loopback());
            if url.scheme() != "http"
                || !loopback
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(error("network", "Test origins must be loopback HTTP URLs"));
            }
            if !url.path().ends_with('/') {
                url.path_segments_mut()
                    .map_err(|_| error("network", "Invalid test origin"))?
                    .push("");
            }
            Ok(url)
        }
        let updater = local(&updater_base)?;
        let api = local(&github_api_base)?;
        let web = local(&github_web_base)?;
        Ok(Self {
            storage,
            updater_base: Some(updater.to_string()),
            test_origins: Some((api, web)),
        })
    }

    fn is_test_origin(&self, url: &Url) -> bool {
        #[cfg(any(test, feature = "test-support"))]
        if let Some((api, web)) = &self.test_origins {
            return [api, web].iter().any(|origin| {
                origin.scheme() == url.scheme()
                    && origin.host_str() == url.host_str()
                    && origin.port_or_known_default() == url.port_or_known_default()
            }) || Url::parse(self.updater_base.as_deref().unwrap_or("")).is_ok_and(
                |origin| {
                    origin.scheme() == url.scheme()
                        && origin.host_str() == url.host_str()
                        && origin.port_or_known_default() == url.port_or_known_default()
                },
            );
        }
        let _ = url;
        false
    }

    fn api_base(&self) -> Url {
        #[cfg(any(test, feature = "test-support"))]
        if let Some((api, _)) = &self.test_origins {
            return api.clone();
        }
        Url::parse("https://api.github.com/").expect("constant URL")
    }

    fn web_base(&self) -> Url {
        #[cfg(any(test, feature = "test-support"))]
        if let Some((_, web)) = &self.test_origins {
            return web.clone();
        }
        Url::parse("https://github.com/").expect("constant URL")
    }

    fn updater(&self) -> Result<Url, MarketError> {
        let mut base = Url::parse(
            self.updater_base
                .as_deref()
                .unwrap_or(DEFAULT_UPDATER)
                .trim(),
        )
        .map_err(|e| error("network", format!("Invalid updater URL: {e}")))?;
        if (base.scheme() != "https" && !self.is_test_origin(&base))
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(error(
                "network",
                "Updater URL must be an absolute HTTPS URL without credentials, query or fragment",
            ));
        }
        if !base.path().ends_with('/') {
            base.path_segments_mut()
                .map_err(|_| error("network", "Invalid updater URL"))?
                .push("");
        }
        Ok(base)
    }

    async fn client(&self, updater: &Url) -> Result<reqwest::Client, MarketError> {
        let host = updater.host_str().unwrap_or_default().to_owned();
        let port = updater.port_or_known_default();
        #[cfg(any(test, feature = "test-support"))]
        let fixture_origins = self
            .test_origins
            .as_ref()
            .map(|(api, web)| vec![updater.clone(), api.clone(), web.clone()])
            .unwrap_or_default();
        #[cfg(not(any(test, feature = "test-support")))]
        let fixture_origins: Vec<Url> = Vec::new();
        let policy = Policy::custom(move |attempt| {
            let url = attempt.url();
            let github = matches!(
                url.host_str(),
                Some(
                    "github.com"
                        | "api.github.com"
                        | "objects.githubusercontent.com"
                        | "release-assets.githubusercontent.com"
                        | "github-releases.githubusercontent.com"
                        | "releases.githubusercontent.com"
                )
            );
            let updater_host =
                url.host_str() == Some(host.as_str()) && url.port_or_known_default() == port;
            let fixture = fixture_origins.iter().any(|origin| {
                origin.scheme() == url.scheme()
                    && origin.host_str() == url.host_str()
                    && origin.port_or_known_default() == url.port_or_known_default()
            });
            if attempt.previous().len() >= 10
                || (url.scheme() != "https" && !fixture)
                || !url.username().is_empty()
                || url.password().is_some()
                || (!github && !updater_host && !fixture)
                || (github && url.port().is_some())
            {
                attempt.error("Untrusted redirect")
            } else {
                attempt.follow()
            }
        });
        let settings = self.storage.settings();
        let mut builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .user_agent(format!("Stravia/{}", env!("CARGO_PKG_VERSION")))
            .redirect(policy)
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd();
        if update_uses_proxy(settings)
            .await
            .map_err(|e| error("network", e.to_string()))?
        {
            let proxy = OutboundProxySettings::load(settings)
                .await
                .map_err(|e| error("network", e.to_string()))?
                .reqwest_proxy()
                .map_err(|e| error("network", e.to_string()))?;
            builder = builder.proxy(proxy);
        } else {
            builder = builder.no_proxy();
        }
        builder.build().map_err(|e| error("network", e.to_string()))
    }

    fn updater_path(base: &Url, parts: &[&str]) -> Result<Url, MarketError> {
        let mut url = base.clone();
        url.path_segments_mut()
            .map_err(|_| error("network", "Invalid updater URL"))?
            .pop_if_empty()
            .extend(["api", "v1", "plugin-repositories"])
            .extend(parts.iter().copied());
        Ok(url)
    }

    async fn get(
        &self,
        client: &reqwest::Client,
        url: Url,
        limit: usize,
        progress: Option<&DownloadProgress>,
    ) -> Result<(StatusCode, Vec<u8>), MarketError> {
        let response = client
            .get(url)
            .header(header::ACCEPT_ENCODING, "identity")
            .send()
            .await
            .map_err(|e| error("network", format!("Request failed: {e}")))?;
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS
            || (status == StatusCode::FORBIDDEN
                && response
                    .headers()
                    .get("x-ratelimit-remaining")
                    .is_some_and(|v| v == "0"))
        {
            return Err(error(
                "rate_limited",
                "GitHub or updater rate limit reached",
            ));
        }
        if response
            .headers()
            .get_all(header::CONTENT_ENCODING)
            .iter()
            .any(|v| !v.as_bytes().eq_ignore_ascii_case(b"identity"))
        {
            return Err(error(
                "artifact_corrupt",
                "Non-identity Content-Encoding is not permitted",
            ));
        }
        let total = response.content_length();
        if total.is_some_and(|n| n > limit as u64) {
            return Err(error("resource_limit", "Response exceeds its size limit"));
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        if let Some(callback) = progress {
            callback(0, total);
        }
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|e| error("network", format!("Response interrupted: {e}")))?;
            if bytes.len().saturating_add(chunk.len()) > limit {
                return Err(error("resource_limit", "Response exceeds its size limit"));
            }
            bytes.extend_from_slice(&chunk);
            if let Some(callback) = progress {
                callback(bytes.len() as u64, total);
            }
        }
        Ok((status, bytes))
    }

    async fn json<T: for<'de> Deserialize<'de>>(
        &self,
        client: &reqwest::Client,
        url: Url,
    ) -> Result<T, MarketError> {
        let (status, bytes) = self.get(client, url, PAGE_LIMIT, None).await?;
        if !status.is_success() {
            return Err(error("network", format!("Source returned HTTP {status}")));
        }
        serde_json::from_slice(&bytes)
            .map_err(|e| error("network", format!("Invalid source response: {e}")))
    }

    fn github_repo_url(&self, name: &str) -> Result<Url, MarketError> {
        let mut url = self.api_base();
        let (owner, repo) = name
            .split_once('/')
            .ok_or_else(|| error("network", "Invalid repository name"))?;
        url.path_segments_mut()
            .map_err(|_| error("network", "Invalid GitHub API URL"))?
            .pop_if_empty()
            .extend(["repos", owner, repo]);
        Ok(url)
    }

    fn repository(response: RepositoryResponse, mirrored: bool) -> Result<Repository, MarketError> {
        let name = parse_repository_url(&format!("https://github.com/{}", response.full_name))?;
        if response.private || response.id == 0 {
            return Err(error("network", "Repository must be public"));
        }
        Ok(Repository {
            id: response.id,
            url: format!("https://github.com/{name}"),
            name,
            mirrored,
        })
    }

    pub async fn resolve_repository(&self, url: &str) -> Result<Repository, MarketError> {
        self.resolve_repository_if_present(url)
            .await?
            .ok_or_else(|| {
                error(
                    "source_identity_changed",
                    "GitHub repository was not found or is private",
                )
            })
    }

    async fn resolve_repository_if_present(
        &self,
        url: &str,
    ) -> Result<Option<Repository>, MarketError> {
        let name = parse_repository_url(url)?;
        let base = self.updater()?;
        let client = self.client(&base).await?;
        let (owner, repo) = name.split_once('/').expect("validated name");
        let route = Self::updater_path(&base, &[owner, repo])?;
        let (status, bytes) = self.get(&client, route, PAGE_LIMIT, None).await?;
        if status.is_success() {
            let response: RepositoryResponse = serde_json::from_slice(&bytes).map_err(|e| {
                error(
                    "network",
                    format!("Invalid updater repository response: {e}"),
                )
            })?;
            match response.mirrored {
                Some(true) => return Self::repository(response, true).map(Some),
                Some(false) => {}
                None => return Err(error("network", "Updater did not report mirror capability")),
            }
        } else {
            let body: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|_| error("network", format!("Updater returned HTTP {status}")))?;
            let code = body
                .get("code")
                .and_then(|v| v.as_str())
                .or_else(|| body.get("error").and_then(|v| v.as_str()))
                .or_else(|| body.pointer("/error/code").and_then(|v| v.as_str()));
            if code != Some("not_mirrored") {
                return Err(error("network", format!("Updater returned HTTP {status}")));
            }
        }
        let (status, bytes) = self
            .get(&client, self.github_repo_url(&name)?, PAGE_LIMIT, None)
            .await?;
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(error("network", format!("GitHub returned HTTP {status}")));
        }
        let response = serde_json::from_slice(&bytes).map_err(|e| {
            error(
                "network",
                format!("Invalid GitHub repository response: {e}"),
            )
        })?;
        Self::repository(response, false).map(Some)
    }

    /// Resolve through updater first, checking its identity against the pinned GitHub ID.
    pub async fn resolve_existing_repository(
        &self,
        source_url: &str,
        repository_id: u64,
    ) -> Result<Repository, MarketError> {
        parse_repository_url(source_url)?;
        if repository_id == 0 {
            return Err(error(
                "source_identity_changed",
                "Invalid repository identity",
            ));
        }
        if let Some(resolved) = self.resolve_repository_if_present(source_url).await?
            && resolved.id == repository_id
        {
            return Ok(resolved);
        }
        // An old name may have been reused. Recover the pinned repository by ID,
        // then resolve its new name; never accept the repository occupying the old name.
        let base = self.updater()?;
        let client = self.client(&base).await?;
        let pinned = Repository {
            id: repository_id,
            name: String::new(),
            url: String::new(),
            mirrored: false,
        };
        let current = self.current_repository(&client, &pinned).await?;
        let resolved = self.resolve_repository(&current.url).await?;
        if resolved.id != repository_id {
            return Err(error(
                "source_identity_changed",
                "Repository name now identifies a different repository",
            ));
        }
        Ok(resolved)
    }

    async fn current_repository(
        &self,
        client: &reqwest::Client,
        repository: &Repository,
    ) -> Result<Repository, MarketError> {
        // Numeric lookup follows repository renames without allowing a reused old name to take over.
        let mut url = self.api_base();
        url.path_segments_mut()
            .map_err(|_| error("network", "Invalid GitHub API URL"))?
            .pop_if_empty()
            .extend(["repositories", &repository.id.to_string()]);
        let current = Self::repository(self.json(client, url).await?, repository.mirrored)?;
        if current.id != repository.id {
            return Err(error(
                "source_identity_changed",
                "GitHub repository identity changed",
            ));
        }
        Ok(current)
    }

    pub async fn releases(
        &self,
        repository: &Repository,
        page: usize,
    ) -> Result<Vec<Release>, MarketError> {
        if !(1..=10).contains(&page) {
            return Err(error("resource_limit", "Release page outside scan limit"));
        }
        let base = self.updater()?;
        let client = self.client(&base).await?;
        // The caller resolves and pins identity once for the entire operation.
        let current = repository;
        let mut url = if repository.mirrored {
            let (owner, repo) = current.name.split_once('/').expect("validated name");
            Self::updater_path(&base, &[owner, repo, "releases"])?
        } else {
            let mut url = self.api_base();
            url.path_segments_mut()
                .map_err(|_| error("network", "Invalid GitHub API URL"))?
                .pop_if_empty()
                .extend(["repositories", &repository.id.to_string(), "releases"]);
            url
        };
        url.query_pairs_mut()
            .append_pair("per_page", "100")
            .append_pair("page", &page.to_string());
        let mut releases: Vec<Release> = self.json(&client, url).await?;
        if releases.len() > 100 {
            return Err(error(
                "resource_limit",
                "Release page contains more than 100 entries",
            ));
        }
        let (owner, repo) = current.name.split_once('/').expect("validated name");
        for release in &mut releases {
            if release.tag.is_empty()
                || release.tag == "."
                || release.tag == ".."
                || release.tag.chars().any(char::is_control)
            {
                return Err(error("artifact_corrupt", "Invalid release tag"));
            }
            let mut link = Url::parse("https://github.com/").expect("constant URL");
            link.path_segments_mut()
                .expect("hierarchical URL")
                .pop_if_empty()
                .extend([owner, repo, "releases", "tag", &release.tag]);
            release.url = link.to_string();
        }
        Ok(releases)
    }

    pub async fn asset(
        &self,
        repository: &Repository,
        release: &Release,
        filename: &str,
        limit: usize,
        progress: Option<DownloadProgress>,
    ) -> Result<Vec<u8>, MarketError> {
        if filename.is_empty()
            || filename == "."
            || filename == ".."
            || filename
                .chars()
                .any(|c| c.is_control() || c == '/' || c == '\\')
        {
            return Err(error("artifact_corrupt", "Invalid release asset filename"));
        }
        let mut matching = release.assets.iter().filter(|a| a.name == filename);
        let asset = matching
            .next()
            .ok_or_else(|| error("not_cataloged", "Release asset not found"))?;
        if matching.next().is_some() {
            return Err(error("artifact_corrupt", "Duplicate release asset name"));
        }
        let limit = limit.min(MAX_ARTIFACT);
        if asset.size > limit as u64 {
            return Err(error(
                "resource_limit",
                "Release asset exceeds its size limit",
            ));
        }
        let base = self.updater()?;
        let client = self.client(&base).await?;
        // The caller resolves and pins identity once for the entire operation.
        let current = repository;
        let (owner, repo) = current.name.split_once('/').expect("validated name");
        let url = if repository.mirrored {
            Self::updater_path(
                &base,
                &[owner, repo, "releases", &release.tag, "assets", filename],
            )?
        } else {
            let mut url = self.web_base();
            url.path_segments_mut()
                .expect("hierarchical URL")
                .pop_if_empty()
                .extend([owner, repo, "releases", "download", &release.tag, filename]);
            url
        };
        let (status, bytes) = self.get(&client, url, limit, progress.as_ref()).await?;
        if !status.is_success() {
            return Err(error(
                "network",
                format!("Asset request returned HTTP {status}"),
            ));
        }
        if bytes.len() as u64 != asset.size {
            return Err(error(
                "artifact_corrupt",
                "Release asset size does not match metadata",
            ));
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MemoryStorage;
    use axum::{
        Router,
        body::Body,
        extract::{Request, State},
        http::Response,
        routing::any,
    };
    use std::{collections::HashMap, sync::Mutex};

    type Reply = (u16, Vec<(&'static str, &'static str)>, Vec<u8>);
    #[derive(Default)]
    struct Fixture {
        replies: HashMap<String, Reply>,
        hits: Vec<String>,
    }

    async fn serve(State(state): State<Arc<Mutex<Fixture>>>, request: Request) -> Response<Body> {
        let path = request.uri().path().to_owned();
        let mut state = state.lock().unwrap();
        state.hits.push(path.clone());
        let (status, headers, body) =
            state
                .replies
                .get(&path)
                .cloned()
                .unwrap_or((404, vec![], vec![]));
        let mut response = Response::builder().status(status);
        for (name, value) in headers {
            response = response.header(name, value);
        }
        response.body(Body::from(body)).unwrap()
    }

    async fn fixture(replies: Vec<(&str, Reply)>) -> (MarketClient, Arc<Mutex<Fixture>>) {
        let state = Arc::new(Mutex::new(Fixture {
            replies: replies
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect(),
            hits: vec![],
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = Router::new().fallback(any(serve)).with_state(state.clone());
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let storage: DynStorage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
        (
            MarketClient::with_test_origins(storage, base.clone(), base.clone(), base).unwrap(),
            state,
        )
    }

    fn reply(status: u16, body: &str) -> Reply {
        (status, vec![], body.as_bytes().to_vec())
    }

    #[tokio::test]
    async fn mirror_resolution_falls_back_only_when_explicitly_unlisted() {
        let path = "/api/v1/plugin-repositories/a/b";
        let (client, state) = fixture(vec![(
            path,
            reply(
                200,
                r#"{"id":7,"full_name":"a/b","private":false,"mirrored":true}"#,
            ),
        )])
        .await;
        assert!(
            client
                .resolve_repository("https://github.com/a/b")
                .await
                .unwrap()
                .mirrored
        );
        assert!(
            !state
                .lock()
                .unwrap()
                .hits
                .iter()
                .any(|p| p.starts_with("/repos/"))
        );

        let (client, state) = fixture(vec![
            (path, reply(404, r#"{"code":"not_mirrored"}"#)),
            (
                "/repos/a/b",
                reply(200, r#"{"id":7,"full_name":"a/b","private":false}"#),
            ),
        ])
        .await;
        assert!(
            !client
                .resolve_repository("https://github.com/a/b")
                .await
                .unwrap()
                .mirrored
        );
        assert!(
            state
                .lock()
                .unwrap()
                .hits
                .contains(&"/repos/a/b".to_owned())
        );

        let (client, state) = fixture(vec![
            (path, reply(503, "unavailable")),
            ("/repos/a/b", reply(200, r#"{"id":7,"full_name":"a/b"}"#)),
        ])
        .await;
        assert_eq!(
            client
                .resolve_repository("https://github.com/a/b")
                .await
                .unwrap_err()
                .code,
            "network"
        );
        assert!(
            !state
                .lock()
                .unwrap()
                .hits
                .iter()
                .any(|p| p.starts_with("/repos/"))
        );
    }

    #[tokio::test]
    async fn pinned_repository_survives_rename_and_rejects_takeover() {
        let (client, state) = fixture(vec![(
            "/api/v1/plugin-repositories/old/name",
            reply(
                200,
                r#"{"id":7,"full_name":"new/name","private":false,"mirrored":true}"#,
            ),
        )])
        .await;
        let repository = client
            .resolve_existing_repository("https://github.com/old/name", 7)
            .await
            .unwrap();
        assert_eq!(repository.name, "new/name");
        assert!(
            !state
                .lock()
                .unwrap()
                .hits
                .iter()
                .any(|p| p.starts_with("/repositories/"))
        );
        let (client, _) = fixture(vec![
            (
                "/api/v1/plugin-repositories/old/name",
                reply(
                    200,
                    r#"{"id":8,"full_name":"old/name","private":false,"mirrored":true}"#,
                ),
            ),
            (
                "/repositories/7",
                reply(200, r#"{"id":7,"full_name":"new/name","private":false}"#),
            ),
            (
                "/api/v1/plugin-repositories/new/name",
                reply(
                    200,
                    r#"{"id":8,"full_name":"new/name","private":false,"mirrored":true}"#,
                ),
            ),
        ])
        .await;
        assert_eq!(
            client
                .resolve_existing_repository("https://github.com/old/name", 7)
                .await
                .unwrap_err()
                .code,
            "source_identity_changed"
        );
    }

    #[tokio::test]
    async fn disappeared_old_name_recovers_by_pinned_id() {
        let (client, state) = fixture(vec![
            (
                "/api/v1/plugin-repositories/old/name",
                reply(404, r#"{"code":"not_mirrored"}"#),
            ),
            ("/repos/old/name", reply(404, "not found")),
            (
                "/repositories/7",
                reply(200, r#"{"id":7,"full_name":"new/name","private":false}"#),
            ),
            (
                "/api/v1/plugin-repositories/new/name",
                reply(
                    200,
                    r#"{"id":7,"full_name":"new/name","private":false,"mirrored":true}"#,
                ),
            ),
        ])
        .await;
        assert_eq!(
            client
                .resolve_existing_repository("https://github.com/old/name", 7)
                .await
                .unwrap()
                .name,
            "new/name"
        );
        assert!(
            state
                .lock()
                .unwrap()
                .hits
                .contains(&"/repositories/7".to_owned())
        );

        let (client, state) = fixture(vec![(
            "/api/v1/plugin-repositories/old/name",
            reply(503, "unavailable"),
        )])
        .await;
        assert_eq!(
            client
                .resolve_existing_repository("https://github.com/old/name", 7)
                .await
                .unwrap_err()
                .code,
            "network"
        );
        assert!(
            !state
                .lock()
                .unwrap()
                .hits
                .iter()
                .any(|p| p.starts_with("/repositories/"))
        );
    }

    #[tokio::test]
    async fn repository_visibility_must_be_explicit() {
        let (client, _) = fixture(vec![(
            "/api/v1/plugin-repositories/a/b",
            reply(200, r#"{"id":7,"full_name":"a/b","mirrored":true}"#),
        )])
        .await;
        assert_eq!(
            client
                .resolve_repository("https://github.com/a/b")
                .await
                .unwrap_err()
                .code,
            "network"
        );
    }

    #[tokio::test]
    async fn responses_enforce_size_encoding_and_redirect_policy() {
        let path = "/api/v1/plugin-repositories/a/b";
        let (client, _) = fixture(vec![(path, (200, vec![], vec![b'x'; PAGE_LIMIT + 1]))]).await;
        assert_eq!(
            client
                .resolve_repository("https://github.com/a/b")
                .await
                .unwrap_err()
                .code,
            "resource_limit"
        );
        let (client, _) = fixture(vec![(
            path,
            (
                200,
                vec![("content-encoding", "gzip")],
                b"not compressed".to_vec(),
            ),
        )])
        .await;
        assert_eq!(
            client
                .resolve_repository("https://github.com/a/b")
                .await
                .unwrap_err()
                .code,
            "artifact_corrupt"
        );
        let (client, _) = fixture(vec![(
            path,
            (302, vec![("location", "http://192.0.2.1/unsafe")], vec![]),
        )])
        .await;
        assert_eq!(
            client
                .resolve_repository("https://github.com/a/b")
                .await
                .unwrap_err()
                .code,
            "network"
        );
    }

    #[tokio::test]
    async fn operation_reuses_resolved_identity_for_pages_and_assets() {
        let releases = r#"[{"id":1,"tag_name":"v1","html_url":"","draft":false,"prerelease":false,"assets":[{"id":2,"name":"catalog.json","size":3}]},{"id":3,"tag_name":"v2","html_url":"","draft":false,"prerelease":false,"assets":[{"id":4,"name":"catalog.json","size":3}]}]"#;
        for mirrored in [false, true] {
            let identity = if mirrored {
                reply(
                    200,
                    r#"{"id":7,"full_name":"new/name","private":false,"mirrored":true}"#,
                )
            } else {
                reply(404, r#"{"code":"not_mirrored"}"#)
            };
            let (client, state) = fixture(vec![
                ("/api/v1/plugin-repositories/old/name", identity),
                (
                    "/repos/old/name",
                    reply(200, r#"{"id":7,"full_name":"new/name","private":false}"#),
                ),
                ("/repositories/7/releases", reply(200, releases)),
                (
                    "/api/v1/plugin-repositories/new/name/releases",
                    reply(200, releases),
                ),
                (
                    "/new/name/releases/download/v1/catalog.json",
                    reply(200, "one"),
                ),
                (
                    "/new/name/releases/download/v2/catalog.json",
                    reply(200, "two"),
                ),
                (
                    "/api/v1/plugin-repositories/new/name/releases/v1/assets/catalog.json",
                    reply(200, "one"),
                ),
                (
                    "/api/v1/plugin-repositories/new/name/releases/v2/assets/catalog.json",
                    reply(200, "two"),
                ),
            ])
            .await;
            let repo = client
                .resolve_existing_repository("https://github.com/old/name", 7)
                .await
                .unwrap();
            assert_eq!(repo.name, "new/name");
            assert_eq!(repo.mirrored, mirrored);
            for page in 1..=2 {
                let items = client.releases(&repo, page).await.unwrap();
                assert_eq!(items.len(), 2);
                assert_eq!(items[0].url, "https://github.com/new/name/releases/tag/v1");
                for (item, expected) in items.iter().zip([b"one", b"two"]) {
                    assert_eq!(
                        client
                            .asset(&repo, item, "catalog.json", 10, None)
                            .await
                            .unwrap(),
                        expected.to_vec()
                    );
                }
            }
            let hits = &state.lock().unwrap().hits;
            assert_eq!(
                hits.iter()
                    .filter(|p| *p == "/api/v1/plugin-repositories/old/name")
                    .count(),
                1
            );
            assert_eq!(
                hits.iter().filter(|p| *p == "/repos/old/name").count(),
                usize::from(!mirrored)
            );
            assert!(
                !hits
                    .iter()
                    .any(|p| p == "/repositories/7" || p == "/api/v1/plugin-repositories/new/name")
            );
            assert_eq!(hits.len(), if mirrored { 7 } else { 8 });
        }
    }

    #[tokio::test]
    async fn download_reports_progress_and_rejects_wrong_size() {
        let metadata = r#"{"id":7,"full_name":"a/b","private":false,"mirrored":true}"#;
        let (client, state) = fixture(vec![
            ("/api/v1/plugin-repositories/a/b", reply(200, metadata)),
            (
                "/api/v1/plugin-repositories/a/b/releases/v1/assets/a.zst",
                reply(200, "abc"),
            ),
        ])
        .await;
        let repo = Repository {
            id: 7,
            name: "a/b".into(),
            url: "https://github.com/a/b".into(),
            mirrored: true,
        };
        let mut release = Release {
            id: 1,
            tag: "v1".into(),
            url: "https://github.com/a/b/releases/tag/v1".into(),
            draft: false,
            prerelease: false,
            assets: vec![ReleaseAsset {
                id: 2,
                name: "a.zst".into(),
                size: 3,
            }],
        };
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        let progress: DownloadProgress =
            Arc::new(move |n, total| record.lock().unwrap().push((n, total)));
        assert_eq!(
            client
                .asset(&repo, &release, "a.zst", 10, Some(progress))
                .await
                .unwrap(),
            b"abc"
        );
        assert_eq!(seen.lock().unwrap().last().unwrap().0, 3);
        assert!(
            !state
                .lock()
                .unwrap()
                .hits
                .iter()
                .any(|p| p.starts_with("/repositories/"))
        );
        release.assets[0].size = 4;
        assert_eq!(
            client
                .asset(&repo, &release, "a.zst", 10, None)
                .await
                .unwrap_err()
                .code,
            "artifact_corrupt"
        );
    }

    #[test]
    fn repository_url_rejects_non_repository_urls() {
        assert_eq!(
            parse_repository_url("https://github.com/owner/repo.git/").unwrap(),
            "owner/repo"
        );
        for url in [
            "http://github.com/a/b",
            "https://github.com:443/a/b",
            "https://user@github.com/a/b",
            "https://github.com/a/b?q=1",
            "https://github.com/a/b#x",
            "https://github.com/a/b/c",
            "https://github.com.evil.test/a/b",
            "https://github.com/a/%62",
            "https://github.com/a/..",
        ] {
            assert!(parse_repository_url(url).is_err(), "{url}");
        }
    }
}
