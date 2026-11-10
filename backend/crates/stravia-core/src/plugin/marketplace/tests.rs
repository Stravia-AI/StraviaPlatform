use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use axum::{
    Json, Router,
    extract::State,
    http::{StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::json;

use super::{
    CATALOG, PUBLIC_KEY, PluginMarketplace, PreviewInstallRequest, SIGNATURE, client::MarketClient,
};
use crate::{
    config::GatewayConfig,
    gateway::Gateway,
    storage::{DynStorage, MemoryStorage},
};

// Signed with a disposable Ed25519 test key. The private key is not needed by the tests.
const KEY: &str = "RWR0ZXN0LWtleaGV32rmEz5ZTRx6bSmBrwbb1JJKmr0mbR6eofK4ZXVB";
const CAT_42_V1: &str = r#"{"schema_version":2,"repository_id":42,"release_tag":"v1","generated_at":"2026-01-01T00:00:00Z","releases":[{"vendor_id":"example","version":"1.0.0","display_name":"Example","published_at":"2026-01-01T00:00:00Z","compatibility":{"host_version_req":">=0.0.0","wit_version":"1.0.0","canonical_format_version":1},"artifact":{"filename":"example.wasm.zst","compression":"zstd","media_type":"application/zstd","size":10,"sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},"component":{"size":20,"sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}]}"#;
const CAT_42_V2: &str = r#"{"schema_version":2,"repository_id":42,"release_tag":"v2","generated_at":"2026-01-01T00:00:00Z","releases":[{"vendor_id":"example","version":"1.0.0","display_name":"Example","published_at":"2026-01-01T00:00:00Z","compatibility":{"host_version_req":">=0.0.0","wit_version":"1.0.0","canonical_format_version":1},"artifact":{"filename":"example.wasm.zst","compression":"zstd","media_type":"application/zstd","size":10,"sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},"component":{"size":20,"sha256":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"}}]}"#;
const CAT_43_V1: &str = r#"{"schema_version":2,"repository_id":43,"release_tag":"v1","generated_at":"2026-01-01T00:00:00Z","releases":[{"vendor_id":"example","version":"1.0.0","display_name":"Example","published_at":"2026-01-01T00:00:00Z","compatibility":{"host_version_req":">=0.0.0","wit_version":"1.0.0","canonical_format_version":1},"artifact":{"filename":"example.wasm.zst","compression":"zstd","media_type":"application/zstd","size":10,"sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},"component":{"size":20,"sha256":"dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"}}]}"#;
const SIG_42_V1: &str = "untrusted comment: fixture\nRUR0ZXN0LWtlebWm0tp4nFG2IYjne88klz5mhYNU+BFBbiXOetR99wewDNZEhkb8x955F3QEvpMoIdNI2UOkse4WgcfCHDhfqwQ=\ntrusted comment: test fixture\niQ9R0SvnHDnx6AG3XYOAmwGFF+mF+GRGo2rXFoPC31GeH+J9fRCryNpikNHQQiFweArPT6UBNcxXSioS3h3eDQ==";
const SIG_42_V2: &str = "untrusted comment: fixture\nRUR0ZXN0LWtleVJdxCN6khdOipL3OsbjG8ZuIO835iQzKwC2oCuDKlsjpOmi3tjCepgu4XGFtz6tpDOdEbm074F29nDq+qq5HAM=\ntrusted comment: test fixture\nAg5UgZCYsFeq+sfJ8mFXYmnQ/FfuiaTJELsj+9hy1L45QgAGcsuV53QunGX4w9B+fsWKpHQcbsURW2Fhb8VHDw==";
const SIG_43_V1: &str = "untrusted comment: fixture\nRUR0ZXN0LWtleUMKBEQ9WmakufaORfg/+PxfiMJvbk6beFIqaN6TFQX5PjzJLpSSWKHQcSsxdj4RnxRzyhZQ9CcvPv3ETa7oswc=\ntrusted comment: test fixture\nuTInBOkyGlgKoJrUyH1jb3m9+hvG0amVDqsCpUJiz65KacqNoejSLxRh0bcwrZgZgy075CSUyaWlTsSLXUvbDQ==";

#[derive(Default)]
struct FixtureState {
    fail_one: AtomicBool,
    conflicting_release: AtomicBool,
    block_release_list: AtomicBool,
    release_list_blocked: tokio::sync::Notify,
    resume_release_list: tokio::sync::Notify,
}

fn catalog(repo: u64, tag: &str) -> (&'static str, &'static str) {
    match (repo, tag) {
        (42, "v1") => (CAT_42_V1, SIG_42_V1),
        (42, "v2") => (CAT_42_V2, SIG_42_V2),
        (43, "v1") => (CAT_43_V1, SIG_43_V1),
        _ => unreachable!(),
    }
}

fn release(repo: u64, tag: &str) -> serde_json::Value {
    let (catalog, signature) = catalog(repo, tag);
    json!({"id": if tag == "v1" { 1 } else { 2 }, "tag_name": tag,
    "html_url": format!("https://github.com/acme/{}/releases/tag/{tag}", if repo == 42 { "one" } else { "two" }),
    "draft": false, "prerelease": false,
    "assets": [
        {"id": 1, "name": CATALOG, "size": catalog.len()},
        {"id": 2, "name": SIGNATURE, "size": signature.len()},
        {"id": 3, "name": PUBLIC_KEY, "size": KEY.len()}
    ]})
}

async fn fixture(State(state): State<Arc<FixtureState>>, uri: Uri) -> Response {
    let path = uri.path();
    let repo = if path.contains("/one") || path.contains("/42") {
        42
    } else if path.contains("/two") || path.contains("/43") {
        43
    } else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if repo == 42 && state.fail_one.load(Ordering::SeqCst) {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let name = if repo == 42 { "one" } else { "two" };
    if path.ends_with("/releases") {
        let mut releases = vec![release(repo, "v1")];
        if repo == 42 && state.conflicting_release.load(Ordering::SeqCst) {
            releases.push(release(repo, "v2"));
        }
        if repo == 42 && state.block_release_list.swap(false, Ordering::SeqCst) {
            state.release_list_blocked.notify_one();
            state.resume_release_list.notified().await;
        }
        return Json(releases).into_response();
    }
    if path.contains("/assets/") {
        let tag = if path.contains("/v2/") { "v2" } else { "v1" };
        let (catalog, signature) = catalog(repo, tag);
        let value = if path.ends_with(CATALOG) {
            catalog
        } else if path.ends_with(SIGNATURE) {
            signature
        } else if path.ends_with(PUBLIC_KEY) {
            KEY
        } else {
            return StatusCode::NOT_FOUND.into_response();
        };
        return value.into_response();
    }
    Json(json!({"id": repo, "full_name": format!("acme/{name}"), "private": false, "mirrored": true})).into_response()
}

struct Harness {
    _dir: tempfile::TempDir,
    _server: tokio::task::JoinHandle<()>,
    gateway: Gateway,
    market: PluginMarketplace,
    storage: DynStorage,
    base: String,
    state: Arc<FixtureState>,
}

impl Harness {
    async fn new() -> Self {
        let state = Arc::new(FixtureState::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .fallback(get(fixture))
            .with_state(state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let storage: DynStorage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
        let client = MarketClient::with_test_origins(
            storage.clone(),
            base.clone(),
            base.clone(),
            base.clone(),
        )
        .unwrap();
        let market = PluginMarketplace::new(Arc::new(storage.vendor_plugins().clone()), client);
        let dir = tempfile::tempdir().unwrap();
        let gateway = Gateway::from_storage(
            GatewayConfig {
                data_dir: dir.path().to_owned(),
                ..Default::default()
            },
            storage.clone(),
        )
        .await
        .unwrap();
        for source in market.list_sources().await.unwrap() {
            if source.official && source.enabled {
                market
                    .patch_source(&source.id, Some(false), None)
                    .await
                    .unwrap();
            }
        }
        Self {
            _dir: dir,
            _server: server,
            gateway,
            market,
            storage,
            base,
            state,
        }
    }

    async fn add(&self, name: &str) -> String {
        let preview = self
            .market
            .source_preview("admin", &format!("https://github.com/acme/{name}"), None)
            .await
            .unwrap();
        self.market
            .source_confirm("admin", &preview.preview_id)
            .await
            .unwrap()
            .id
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self._server.abort();
    }
}

#[tokio::test]
async fn source_preview_is_bound_to_the_admin_session() {
    let h = Harness::new().await;
    let preview = h
        .market
        .source_preview("alice", "https://github.com/acme/one", None)
        .await
        .unwrap();
    assert_eq!(preview.repository_id, 42);
    assert_eq!(preview.releases[0].vendor_id, "example");
    let error = h
        .market
        .source_confirm("bob", &preview.preview_id)
        .await
        .unwrap_err();
    assert_eq!(error.code, "preview_expired");
    assert!(
        !h.market
            .list_sources()
            .await
            .unwrap()
            .iter()
            .any(|source| source.repository_id == 42)
    );
    let source = h
        .market
        .source_confirm("alice", &preview.preview_id)
        .await
        .unwrap();
    assert_eq!(source.repository_id, 42);
    assert_eq!(
        h.market
            .source_confirm("alice", &preview.preview_id)
            .await
            .unwrap_err()
            .code,
        "preview_expired"
    );
}

#[tokio::test]
async fn conflicting_signed_releases_are_visible_but_cannot_be_installed() {
    let h = Harness::new().await;
    let id = h.add("one").await;
    h.state.conflicting_release.store(true, Ordering::SeqCst);
    let view = h.market.refresh(&h.gateway, Some(&id)).await.unwrap();
    let entry = view
        .entries
        .iter()
        .find(|e| e.source_id == id && e.vendor_id == "example")
        .unwrap();
    assert!(entry.conflict);
    let release = &entry.releases[0];
    let result = h
        .market
        .preview_install(
            &h.gateway,
            "admin",
            PreviewInstallRequest {
                source_id: id,
                release_id: release.release_id,
                vendor_id: "example".into(),
                version: "1.0.0".into(),
                catalog_sha256: release.catalog_sha256.clone(),
            },
            None,
        )
        .await;
    assert_eq!(result.unwrap_err().code, "version_conflict");
}

#[tokio::test]
async fn failed_refresh_keeps_last_success_and_does_not_block_other_sources() {
    let h = Harness::new().await;
    let one = h.add("one").await;
    let two = h.add("two").await;
    let initial = h.market.refresh(&h.gateway, None).await.unwrap();
    assert!(initial.entries.iter().any(|entry| entry.source_id == one));
    assert!(initial.entries.iter().any(|entry| entry.source_id == two));
    let success = initial
        .sources
        .iter()
        .find(|source| source.id == one)
        .unwrap()
        .succeeded_at;
    assert!(success.is_some());
    h.state.fail_one.store(true, Ordering::SeqCst);
    let refreshed = h.market.refresh(&h.gateway, None).await.unwrap();
    let failed = refreshed
        .sources
        .iter()
        .find(|source| source.id == one)
        .unwrap();
    assert_eq!(failed.succeeded_at, success);
    assert_eq!(failed.error.as_ref().unwrap().code, "network");
    assert!(refreshed.entries.iter().any(|entry| entry.source_id == one));
    assert!(refreshed.entries.iter().any(|entry| entry.source_id == two));
    assert!(
        refreshed
            .sources
            .iter()
            .find(|source| source.id == two)
            .unwrap()
            .error
            .is_none()
    );
}

#[tokio::test]
async fn stale_source_does_not_stop_other_sources_refreshing() {
    let h = Harness::new().await;
    let one = h.add("one").await;
    let two = h.add("two").await;
    h.state.block_release_list.store(true, Ordering::SeqCst);
    let refresh = h.market.refresh(&h.gateway, None);
    let change = async {
        h.state.release_list_blocked.notified().await;
        h.market
            .patch_source(&one, Some(false), None)
            .await
            .unwrap();
        h.state.resume_release_list.notify_one();
    };
    let (view, ()) = tokio::join!(refresh, change);
    let view = view.unwrap();
    assert!(
        !view
            .sources
            .iter()
            .find(|source| source.id == one)
            .unwrap()
            .enabled
    );
    assert!(
        view.sources
            .iter()
            .find(|source| source.id == two)
            .unwrap()
            .succeeded_at
            .is_some()
    );
    assert!(view.entries.iter().any(|entry| entry.source_id == two));
}

#[tokio::test]
async fn slower_older_refresh_cannot_replace_newer_instance_results() {
    let h = Harness::new().await;
    let id = h.add("one").await;
    let client = MarketClient::with_test_origins(
        h.storage.clone(),
        h.base.clone(),
        h.base.clone(),
        h.base.clone(),
    )
    .unwrap();
    let other = PluginMarketplace::new(Arc::new(h.storage.vendor_plugins().clone()), client);
    h.state.block_release_list.store(true, Ordering::SeqCst);

    let older = h.market.refresh(&h.gateway, Some(&id));
    let newer = async {
        h.state.release_list_blocked.notified().await;
        h.state.conflicting_release.store(true, Ordering::SeqCst);
        let result = other.refresh(&h.gateway, Some(&id)).await;
        h.state.resume_release_list.notify_one();
        result
    };
    let (older, newer) = tokio::join!(older, newer);
    let newer = newer.unwrap();
    let new_entry = newer
        .entries
        .iter()
        .find(|entry| entry.source_id == id)
        .unwrap();
    assert!(
        new_entry
            .releases
            .iter()
            .any(|release| release.release_id == 2)
    );
    assert!(new_entry.conflict);
    let new_source = newer.sources.iter().find(|source| source.id == id).unwrap();
    assert!(new_source.error.is_none());
    let succeeded_at = new_source.succeeded_at;
    assert!(succeeded_at.is_some());
    if let Err(error) = older {
        assert_eq!(error.code, "network");
    }
    let final_view = h.market.view(&h.gateway).await.unwrap();
    let final_source = final_view
        .sources
        .iter()
        .find(|source| source.id == id)
        .unwrap();
    assert_eq!(final_source.succeeded_at, succeeded_at);
    assert!(final_source.error.is_none());
    let final_entry = final_view
        .entries
        .iter()
        .find(|entry| entry.source_id == id)
        .unwrap();
    assert!(
        final_entry
            .releases
            .iter()
            .any(|release| release.release_id == 2)
    );
    assert!(final_entry.conflict);
}

// The catalog generator uses this deterministic test-only signing key. This second
// signed release changes the digest of the same lifecycle vendor and version.
const LIFECYCLE_CONFLICT: &str = concat!(
    r#"{"schema_version":2,"repository_id":424242,"release_tag":"conflict","generated_at":"2026-01-01T00:00:00Z","releases":[{"vendor_id":"fixture.lifecycle","version":"2.0.0","display_name":"Fixture","published_at":"2026-01-01T00:00:00Z","compatibility":{"host_version_req":">=0.6.0","wit_version":"0.6.0","canonical_format_version":1},"artifact":{"filename":"conflict.wasm.zst","compression":"zstd","media_type":"application/zstd","size":1,"sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},"component":{"size":1,"sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}]}"#,
    "\n"
);
const LIFECYCLE_CONFLICT_SIG: &str = "untrusted comment: fixture\nRUS9AY4RTdBGBrBj+uh8tnm4K7WbpKnPLiGJgYtqUgMvt3nmenMV7ZKdILWkbq+YjWgOUGw9DIqC0qnYt6EnsyF3bPedEalaYQs=\ntrusted comment: test fixture\n/D3Hq1AAPSCzy+G86GfStveKv9LH3hzV/ITLpU3qI+i+g7McFLV3VmjfEIaJRZMX2FOTLY8Ph3YxKuHTf85FBg==\n";

struct LifecycleFixture {
    catalog: Vec<u8>,
    signature: Vec<u8>,
    key: Vec<u8>,
    rotated_key: Vec<u8>,
    rotated_catalog: Vec<u8>,
    rotated_signature: Vec<u8>,
    v3_catalog: Vec<u8>,
    v3_signature: Vec<u8>,
    artifact: Vec<u8>,
    rotated: AtomicBool,
    conflict: AtomicBool,
    v3: AtomicBool,
    renamed: AtomicBool,
}

impl LifecycleFixture {
    fn load() -> Self {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .join("target/vendor-test-fixtures/marketplace");
        let read = |dir: &str, name: &str| {
            std::fs::read(root.join(dir).join(name))
                .expect("missing signed lifecycle fixtures; run task build:vendor-fixtures")
        };
        Self {
            catalog: read("v2", CATALOG),
            signature: read("v2", SIGNATURE),
            key: read("v2", PUBLIC_KEY),
            rotated_key: read("rotated", PUBLIC_KEY),
            rotated_catalog: read("rotated", CATALOG),
            rotated_signature: read("rotated", SIGNATURE),
            v3_catalog: read("v3", CATALOG),
            v3_signature: read("v3", SIGNATURE),
            artifact: read("v2", "fixture.lifecycle-v2.0.0.wasm.zst"),
            rotated: AtomicBool::new(false),
            conflict: AtomicBool::new(false),
            v3: AtomicBool::new(false),
            renamed: AtomicBool::new(false),
        }
    }

    fn release(&self, conflict: bool) -> serde_json::Value {
        let (id, tag, catalog_size, signature_size, artifact_name, artifact_size) = if conflict {
            (
                2,
                "conflict",
                LIFECYCLE_CONFLICT.len(),
                LIFECYCLE_CONFLICT_SIG.len(),
                "conflict.wasm.zst",
                1,
            )
        } else if self.rotated.load(Ordering::SeqCst) {
            (
                1,
                "v2.0.0",
                self.rotated_catalog.len(),
                self.rotated_signature.len(),
                "fixture.lifecycle-v2.0.0.wasm.zst",
                self.artifact.len(),
            )
        } else {
            (
                1,
                "v2.0.0",
                self.catalog.len(),
                self.signature.len(),
                "fixture.lifecycle-v2.0.0.wasm.zst",
                self.artifact.len(),
            )
        };
        json!({"id": id, "tag_name": tag, "html_url": format!("https://github.com/acme/real/releases/tag/{tag}"),
        "draft": false, "prerelease": false, "assets": [
            {"id": 1, "name": CATALOG, "size": catalog_size},
            {"id": 2, "name": SIGNATURE, "size": signature_size},
            {"id": 3, "name": PUBLIC_KEY, "size": if self.rotated.load(Ordering::SeqCst) { self.rotated_key.len() } else { self.key.len() }},
            {"id": 4, "name": artifact_name, "size": artifact_size}
        ]})
    }
}

async fn lifecycle_server(State(state): State<Arc<LifecycleFixture>>, uri: Uri) -> Response {
    let path = uri.path();
    if path.ends_with("/releases") {
        let mut releases = vec![state.release(false)];
        if state.conflict.load(Ordering::SeqCst) {
            releases.push(state.release(true));
        }
        if state.v3.load(Ordering::SeqCst) {
            releases.push(json!({"id": 3, "tag_name": "v3.0.0", "html_url": "https://github.com/acme/real/releases/tag/v3.0.0",
                "draft": false, "prerelease": false, "assets": [
                    {"id": 1, "name": CATALOG, "size": state.v3_catalog.len()},
                    {"id": 2, "name": SIGNATURE, "size": state.v3_signature.len()},
                    {"id": 3, "name": PUBLIC_KEY, "size": state.key.len()}
                ]}));
        }
        return Json(releases).into_response();
    }
    if path.contains("/assets/") {
        let conflict = path.contains("/conflict/");
        let v3 = path.contains("/v3.0.0/");
        let rotated = state.rotated.load(Ordering::SeqCst);
        let bytes = if path.ends_with(SIGNATURE) {
            if conflict {
                LIFECYCLE_CONFLICT_SIG.as_bytes()
            } else if v3 {
                &state.v3_signature
            } else if rotated {
                &state.rotated_signature
            } else {
                &state.signature
            }
        } else if path.ends_with(CATALOG) {
            if conflict {
                LIFECYCLE_CONFLICT.as_bytes()
            } else if v3 {
                &state.v3_catalog
            } else if rotated {
                &state.rotated_catalog
            } else {
                &state.catalog
            }
        } else if path.ends_with(PUBLIC_KEY) {
            if state.rotated.load(Ordering::SeqCst) {
                &state.rotated_key
            } else {
                &state.key
            }
        } else if path.ends_with("fixture.lifecycle-v2.0.0.wasm.zst") {
            &state.artifact
        } else {
            return StatusCode::NOT_FOUND.into_response();
        };
        return bytes.to_vec().into_response();
    }
    let name = if state.renamed.load(Ordering::SeqCst) {
        "acme/renamed"
    } else {
        "acme/real"
    };
    Json(json!({"id": 424242, "full_name": name, "private": false, "mirrored": true}))
        .into_response()
}

struct LifecycleHarness {
    base: Harness,
    market: PluginMarketplace,
    state: Arc<LifecycleFixture>,
    server: tokio::task::JoinHandle<()>,
}

impl LifecycleHarness {
    async fn new() -> Self {
        let base = Harness::new().await;
        let state = Arc::new(LifecycleFixture::load());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .fallback(get(lifecycle_server))
            .with_state(state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client =
            MarketClient::with_test_origins(base.storage.clone(), url.clone(), url.clone(), url)
                .unwrap();
        let market =
            PluginMarketplace::new(Arc::new(base.storage.vendor_plugins().clone()), client);
        Self {
            base,
            market,
            state,
            server,
        }
    }

    async fn preview(&self) -> (String, crate::plugin::PluginPreview) {
        let source = self
            .market
            .source_preview("alice", "https://github.com/acme/real", None)
            .await
            .unwrap();
        let id = self
            .market
            .source_confirm("alice", &source.preview_id)
            .await
            .unwrap()
            .id;
        let view = self
            .market
            .refresh(&self.base.gateway, Some(&id))
            .await
            .unwrap();
        let preview = self.prepare(&id, &view).await;
        (id, preview)
    }

    async fn prepare(
        &self,
        id: &str,
        view: &super::MarketplaceView,
    ) -> crate::plugin::PluginPreview {
        let entry = view
            .entries
            .iter()
            .find(|entry| entry.source_id == id && entry.vendor_id == "fixture.lifecycle")
            .unwrap();
        let release = entry.releases.first().unwrap();
        let preview = self
            .market
            .preview_install(
                &self.base.gateway,
                "alice",
                PreviewInstallRequest {
                    source_id: id.to_owned(),
                    release_id: release.release_id,
                    vendor_id: "fixture.lifecycle".into(),
                    version: "2.0.0".into(),
                    catalog_sha256: release.catalog_sha256.clone(),
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(preview.vendor_id, "fixture.lifecycle");
        preview
    }

    async fn confirm(
        &self,
        preview: crate::plugin::PluginPreview,
    ) -> anyhow::Result<crate::plugin::PluginSummary> {
        self.market
            .authorize_install_preview("alice", &preview.id)?;
        self.base
            .gateway
            .admin()
            .confirm_vendor_plugin(crate::plugin::ConfirmPluginUpdate {
                preview_id: preview.id,
                allow_data_discard: false,
            })
            .await
    }

    async fn is_installed(&self) -> bool {
        self.base
            .gateway
            .admin()
            .list_vendor_plugins()
            .await
            .unwrap()
            .iter()
            .any(|plugin| plugin.vendor_id == "fixture.lifecycle")
    }
}

impl Drop for LifecycleHarness {
    fn drop(&mut self) {
        self.server.abort();
    }
}

#[tokio::test]
async fn observed_key_change_blocks_existing_real_component_preview() {
    let h = LifecycleHarness::new().await;
    let (id, preview) = h.preview().await;
    let initial_view = h.market.view(&h.base.gateway).await.unwrap();
    let revision = initial_view
        .sources
        .iter()
        .find(|source| source.id == id)
        .unwrap()
        .trust_revision;
    let after_retrust = h.prepare(&id, &initial_view).await;
    let stale_trust = h.market.trust_preview("alice", &id).await.unwrap();
    assert!(!h.is_installed().await);
    h.state.rotated.store(true, Ordering::SeqCst);
    let view = h.market.refresh(&h.base.gateway, Some(&id)).await.unwrap();
    assert_eq!(
        view.sources
            .iter()
            .find(|source| source.id == id)
            .unwrap()
            .error
            .as_ref()
            .unwrap()
            .code,
        "key_changed"
    );

    // Returning to the old key is not an explicit trust decision.
    h.state.rotated.store(false, Ordering::SeqCst);
    let view = h.market.refresh(&h.base.gateway, Some(&id)).await.unwrap();
    assert_eq!(
        view.sources
            .iter()
            .find(|source| source.id == id)
            .unwrap()
            .error
            .as_ref()
            .unwrap()
            .code,
        "key_changed"
    );
    assert!(
        h.market
            .trust_confirm("alice", &id, &stale_trust.preview_id)
            .await
            .is_err()
    );
    assert!(h.confirm(preview).await.is_err());
    assert!(!h.is_installed().await);

    let trust = h.market.trust_preview("alice", &id).await.unwrap();
    let source = h
        .market
        .trust_confirm("alice", &id, &trust.preview_id)
        .await
        .unwrap();
    assert!(source.error.is_none());
    assert_eq!(source.trust_revision, revision + 1);
    assert!(h.confirm(after_retrust).await.is_err());
    assert!(!h.is_installed().await);
}

#[tokio::test]
async fn observed_version_conflict_blocks_existing_real_component_preview() {
    let h = LifecycleHarness::new().await;
    let (id, preview) = h.preview().await;
    assert!(!h.is_installed().await);
    h.state.conflict.store(true, Ordering::SeqCst);
    let view = h.market.refresh(&h.base.gateway, Some(&id)).await.unwrap();
    assert!(
        view.entries
            .iter()
            .find(|entry| entry.source_id == id && entry.vendor_id == "fixture.lifecycle")
            .unwrap()
            .conflict
    );
    assert!(h.confirm(preview).await.is_err());
    assert!(!h.is_installed().await);
}

#[tokio::test]
async fn conflicted_old_version_does_not_block_clean_new_version() {
    let h = LifecycleHarness::new().await;
    let (id, preview) = h.preview().await;
    h.confirm(preview).await.unwrap();
    h.state.conflict.store(true, Ordering::SeqCst);
    h.state.v3.store(true, Ordering::SeqCst);
    let view = h.market.refresh(&h.base.gateway, Some(&id)).await.unwrap();
    let entry = view
        .entries
        .iter()
        .find(|entry| entry.source_id == id && entry.vendor_id == "fixture.lifecycle")
        .unwrap();
    assert!(entry.conflict);
    assert!(
        entry
            .releases
            .iter()
            .filter(|release| release.catalog.version == "2.0.0")
            .all(|release| release.conflict)
    );
    let v3 = entry
        .releases
        .iter()
        .find(|release| release.catalog.version == "3.0.0")
        .unwrap();
    assert!(!v3.conflict);
    assert_eq!(entry.latest.as_ref().unwrap().catalog.version, "3.0.0");
    assert_eq!(entry.update.as_ref().unwrap().catalog.version, "3.0.0");
    assert_eq!(view.update_count, 1);
}

#[tokio::test]
async fn retrust_clears_old_cache_and_accepts_fresh_signed_catalog() {
    let h = LifecycleHarness::new().await;
    let (id, _) = h.preview().await;
    h.state.rotated.store(true, Ordering::SeqCst);
    h.market.refresh(&h.base.gateway, Some(&id)).await.unwrap();
    let trust = h.market.trust_preview("alice", &id).await.unwrap();
    let source = h
        .market
        .trust_confirm("alice", &id, &trust.preview_id)
        .await
        .unwrap();
    assert!(source.error.is_none());
    let fresh = h.market.view(&h.base.gateway).await.unwrap();
    let preview = h.prepare(&id, &fresh).await;
    let installed = h.confirm(preview).await.unwrap();
    assert_eq!(installed.version, "2.0.0");
    assert_eq!(
        installed.origin.unwrap().public_key_fingerprint,
        trust.fingerprint
    );
}

#[tokio::test]
async fn repository_rename_between_real_preview_and_confirmation_is_accepted() {
    let h = LifecycleHarness::new().await;
    let (id, preview) = h.preview().await;
    h.state.renamed.store(true, Ordering::SeqCst);
    let view = h.market.refresh(&h.base.gateway, Some(&id)).await.unwrap();
    assert_eq!(
        view.sources
            .iter()
            .find(|source| source.id == id)
            .unwrap()
            .repository,
        "acme/renamed"
    );
    let installed = h.confirm(preview).await.unwrap();
    assert_eq!(installed.vendor_id, "fixture.lifecycle");
    assert_eq!(installed.version, "2.0.0");
    assert_eq!(installed.origin.unwrap().repository_id, 424242);
    assert!(h.is_installed().await);
}
