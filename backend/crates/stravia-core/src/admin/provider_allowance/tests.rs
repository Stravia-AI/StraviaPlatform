use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use reqwest::{StatusCode, header::HeaderMap};
use tokio::sync::{Barrier, Notify, Semaphore};

use crate::Gateway;
use crate::config::GatewayConfig;
use crate::db::models::{CreateProviderRecord, UpdateProvider};

use super::{
    AllowanceHttpRequest, AllowanceHttpResponse, AllowanceKind, AllowanceTransport, MonitorKind,
    ProviderAllowanceErrorCategory, ProviderAllowanceStatus, TransportFailure, fetch_monitor,
    list_provider_allowances_with_transport, monitor_for, monitor_requests, parse_monitor_response,
    refresh_provider_allowance_with_transport,
};

#[test]
fn registry_requires_the_exact_catalog_identity() {
    for (preset_key, channel) in [
        ("anthropic", "claude-code"),
        ("openai", "codex"),
        ("github-copilot", "default"),
        ("kimi-for-coding", "default"),
        ("nano-gpt", "default"),
        ("zai-coding-plan", "default"),
        ("zhipuai-coding-plan", "default"),
        ("minimax-coding-plan", "default"),
        ("minimax-cn-coding-plan", "default"),
        ("wafer.ai", "default"),
        ("opencode-go", "default"),
        ("crof", "default"),
        ("deepseek", "default"),
        ("neuralwatt", "default"),
        ("xai", "grok"),
    ] {
        assert!(
            monitor_for(preset_key, channel).is_some(),
            "missing monitor for {preset_key}/{channel}"
        );
    }

    for (preset_key, channel) in [
        ("openai", "default"),
        ("anthropic", "default"),
        ("xai", "default"),
        ("openai", "Codex"),
        ("OpenAI", "codex"),
    ] {
        assert!(
            monitor_for(preset_key, channel).is_none(),
            "unexpected monitor for {preset_key}/{channel}"
        );
    }
}

#[test]
fn copilot_uses_only_premium_interactions() {
    let parsed = parse_monitor_response(
        MonitorKind::GitHubCopilot,
        include_bytes!("fixtures/github-copilot-success.json"),
    )
    .expect("Copilot fixture should parse");

    assert_eq!(parsed.allowances.len(), 1);
    let allowance = &parsed.allowances[0];
    assert_eq!(allowance.key, "premium_interactions");
    assert_eq!(allowance.kind, AllowanceKind::RequestAllowance);
    assert_eq!(
        allowance.used.as_ref().map(|amount| amount.value),
        Some(38.0)
    );
    assert_eq!(
        allowance.remaining.as_ref().map(|amount| amount.value),
        Some(12.0)
    );
    assert_eq!(
        allowance.limit.as_ref().map(|amount| amount.value),
        Some(50.0)
    );
    assert_eq!(allowance.used_percent, Some(76.0));
    assert_eq!(allowance.reset_at, Some(1_788_220_800_000));
    assert!(
        parsed
            .allowances
            .iter()
            .all(|allowance| allowance.key != "chat")
    );
}

#[test]
fn every_monitor_normalizes_its_response_fixture_and_rejects_schema_drift() {
    let xai = decode_hex(include_str!("fixtures/xai-grok-success.hex"));
    let fixtures: [(MonitorKind, &[u8], &[u8], &str); 15] = [
        (
            MonitorKind::AnthropicClaudeCode,
            include_bytes!("fixtures/anthropic-claude-code-success.json"),
            br#"{"limits":[]}"#,
            "5h",
        ),
        (
            MonitorKind::OpenAiCodex,
            include_bytes!("fixtures/openai-codex-success.json"),
            br#"{"rate_limit":{}}"#,
            "5h",
        ),
        (
            MonitorKind::GitHubCopilot,
            include_bytes!("fixtures/github-copilot-success.json"),
            br#"{"quota_snapshots":{"chat":{"remaining":10}}}"#,
            "premium_interactions",
        ),
        (
            MonitorKind::KimiForCoding,
            include_bytes!("fixtures/kimi-for-coding-success.json"),
            br#"{"usages":[]}"#,
            "weekly",
        ),
        (
            MonitorKind::NanoGpt,
            include_bytes!("fixtures/nano-gpt-success.json"),
            br#"{"period":{}}"#,
            "daily",
        ),
        (
            MonitorKind::ZaiCodingPlan,
            include_bytes!("fixtures/zai-coding-plan-success.json"),
            br#"{"data":{"limits":[]}}"#,
            "5h",
        ),
        (
            MonitorKind::ZhipuAiCodingPlan,
            include_bytes!("fixtures/zhipuai-coding-plan-success.json"),
            br#"{"data":{"limits":[]}}"#,
            "tokens",
        ),
        (
            MonitorKind::MiniMaxCodingPlan,
            include_bytes!("fixtures/minimax-coding-plan-success.json"),
            br#"{"base_resp":{"status_code":0},"model_remains":[]}"#,
            "5h",
        ),
        (
            MonitorKind::MiniMaxCnCodingPlan,
            include_bytes!("fixtures/minimax-cn-coding-plan-success.json"),
            br#"{"base_resp":{"status_code":0},"model_remains":[]}"#,
            "5h",
        ),
        (
            MonitorKind::Wafer,
            include_bytes!("fixtures/wafer-success.json"),
            br#"{"quota":{}}"#,
            "5h",
        ),
        (
            MonitorKind::OpenCodeGo,
            include_bytes!("fixtures/opencode-go-success.json"),
            br#"{"usage":{}}"#,
            "5h",
        ),
        (
            MonitorKind::Crof,
            include_bytes!("fixtures/crof-success.json"),
            br#"{"credits":null}"#,
            "credits",
        ),
        (
            MonitorKind::DeepSeek,
            include_bytes!("fixtures/deepseek-success.json"),
            br#"{"is_available":true,"balance_infos":[]}"#,
            "credits_balance",
        ),
        (
            MonitorKind::NeuralWatt,
            include_bytes!("fixtures/neuralwatt-success.json"),
            br#"{"subscription":{},"key":{}}"#,
            "developer",
        ),
        (MonitorKind::XaiGrok, &xai, &[0x0a], "billing_cycle"),
    ];

    for (monitor, fixture, schema_drift_fixture, expected_key) in fixtures {
        let parsed = parse_monitor_response(monitor, fixture)
            .unwrap_or_else(|_| panic!("{monitor:?} success fixture should parse"));
        assert!(
            parsed
                .allowances
                .iter()
                .any(|allowance| allowance.key == expected_key),
            "{monitor:?} should expose {expected_key}"
        );
        assert!(
            parse_monitor_response(monitor, schema_drift_fixture).is_err(),
            "{monitor:?} must reject schema drift"
        );
    }

    let anthropic = parse_monitor_response(
        MonitorKind::AnthropicClaudeCode,
        include_bytes!("fixtures/anthropic-claude-code-success.json"),
    )
    .unwrap();
    assert_eq!(anthropic.models[0].model, "Claude Sonnet");
    assert_eq!(
        anthropic.allowances[2]
            .used
            .as_ref()
            .and_then(|amount| amount.currency.as_deref()),
        Some("USD")
    );

    let codex = parse_monitor_response(
        MonitorKind::OpenAiCodex,
        include_bytes!("fixtures/openai-codex-success.json"),
    )
    .unwrap();
    assert_eq!(codex.allowances[1].used_percent, Some(111.0));

    let wafer = parse_monitor_response(
        MonitorKind::Wafer,
        include_bytes!("fixtures/wafer-success.json"),
    )
    .unwrap();
    assert_eq!(wafer.allowances[0].used_percent, Some(120.0));
    assert_eq!(
        wafer.allowances[0]
            .remaining
            .as_ref()
            .map(|amount| amount.unit.as_str()),
        Some("requests")
    );

    let deepseek = parse_monitor_response(
        MonitorKind::DeepSeek,
        include_bytes!("fixtures/deepseek-success.json"),
    )
    .unwrap();
    assert_eq!(
        deepseek.allowances[0]
            .remaining
            .as_ref()
            .and_then(|amount| amount.currency.as_deref()),
        Some("USD")
    );
}

fn decode_hex(value: &str) -> Vec<u8> {
    let value = value.trim();
    assert_eq!(value.len() % 2, 0);
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            u8::from_str_radix(std::str::from_utf8(pair).expect("hex fixture"), 16)
                .expect("valid hex fixture")
        })
        .collect()
}

#[derive(Default)]
struct AccountFixtureTransport {
    calls: AtomicUsize,
    proxy_calls: AtomicUsize,
}

#[async_trait]
impl AllowanceTransport for AccountFixtureTransport {
    async fn execute(
        &self,
        _client: reqwest::Client,
        use_proxy: bool,
        request: AllowanceHttpRequest,
    ) -> Result<AllowanceHttpResponse, TransportFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if use_proxy {
            self.proxy_calls.fetch_add(1, Ordering::SeqCst);
        }
        let authorization = request
            .headers
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        let remaining = match authorization {
            "token token-alpha" => 10,
            "token token-beta" => 20,
            value => panic!("unexpected credential header: {value}"),
        };
        Ok(AllowanceHttpResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: serde_json::to_vec(&serde_json::json!({
                "quota_reset_date": "2026-09-01T00:00:00Z",
                "quota_snapshots": {
                    "premium_interactions": {
                        "entitlement": 50,
                        "remaining": remaining,
                        "unlimited": false
                    }
                }
            }))
            .unwrap(),
        })
    }
}

#[tokio::test]
async fn admin_service_filters_sorts_caches_and_keeps_accounts_isolated() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    let alpha =
        create_test_provider(&gateway, "Zulu account", "github-copilot", "token-alpha").await?;
    let beta =
        create_test_provider(&gateway, "Alpha account", "github-copilot", "token-beta").await?;
    let disabled =
        create_test_provider(&gateway, "Disabled", "github-copilot", "token-disabled").await?;
    gateway
        .storage
        .providers()
        .update(
            &disabled.id,
            UpdateProvider {
                is_enabled: Some(false),
                ..Default::default()
            },
        )
        .await?;
    create_test_provider(&gateway, "Unsupported", "openrouter", "token-unsupported").await?;

    let transport = Arc::new(AccountFixtureTransport::default());
    let first =
        list_provider_allowances_with_transport(&gateway.admin(), false, transport.clone()).await?;
    assert_eq!(
        first
            .iter()
            .map(|snapshot| snapshot.provider_name.as_str())
            .collect::<Vec<_>>(),
        ["Alpha account", "Zulu account"]
    );
    assert_eq!(
        first[0].allowances[0]
            .remaining
            .as_ref()
            .map(|amount| amount.value),
        Some(20.0)
    );
    assert_eq!(
        first[1].allowances[0]
            .remaining
            .as_ref()
            .map(|amount| amount.value),
        Some(10.0)
    );
    assert_eq!(transport.calls.load(Ordering::SeqCst), 2);

    let cached =
        list_provider_allowances_with_transport(&gateway.admin(), false, transport.clone()).await?;
    assert_eq!(cached, first);
    assert_eq!(transport.calls.load(Ordering::SeqCst), 2);

    let refreshed =
        list_provider_allowances_with_transport(&gateway.admin(), true, transport.clone()).await?;
    assert_eq!(refreshed.len(), 2);
    assert_eq!(transport.calls.load(Ordering::SeqCst), 4);

    gateway
        .storage
        .providers()
        .update(
            &beta.id,
            UpdateProvider {
                is_enabled: Some(false),
                ..Default::default()
            },
        )
        .await?;
    let after_disable =
        list_provider_allowances_with_transport(&gateway.admin(), false, transport.clone()).await?;
    assert_eq!(
        after_disable
            .iter()
            .map(|snapshot| snapshot.provider_id.as_str())
            .collect::<Vec<_>>(),
        [alpha.id.as_str()]
    );

    gateway
        .storage
        .providers()
        .update(
            &alpha.id,
            UpdateProvider {
                use_proxy: Some(true),
                ..Default::default()
            },
        )
        .await?;
    let with_proxy =
        list_provider_allowances_with_transport(&gateway.admin(), true, transport.clone()).await?;
    assert_eq!(with_proxy.len(), 1);
    assert_eq!(transport.proxy_calls.load(Ordering::SeqCst), 1);

    gateway.storage.providers().delete(&alpha.id).await?;
    let after_delete =
        list_provider_allowances_with_transport(&gateway.admin(), false, transport.clone()).await?;
    assert!(after_delete.is_empty());
    assert_eq!(transport.calls.load(Ordering::SeqCst), 5);
    Ok(())
}

async fn create_test_provider(
    gateway: &Gateway,
    name: &str,
    preset_key: &str,
    token: &str,
) -> anyhow::Result<crate::db::models::Provider> {
    gateway
        .storage
        .providers()
        .create(CreateProviderRecord {
            name: name.into(),
            vendor: Some("@ai-sdk/openai-compatible".into()),
            protocol: "openai-compatible".into(),
            base_url: "https://inference.invalid/v1".into(),
            preset_key: Some(preset_key.into()),
            channel: Some("default".into()),
            models_source: None,
            static_models: None,
            api_key: token.into(),
            adapter_credentials: serde_json::json!({ "apiKey": token }).to_string(),
            auth_mode: "apikey".into(),
            use_proxy: false,
        })
        .await
}

struct StaleFixtureTransport {
    fail: AtomicBool,
    calls: AtomicUsize,
}

struct IsolatedFailureTransport;

#[async_trait]
impl AllowanceTransport for IsolatedFailureTransport {
    async fn execute(
        &self,
        _client: reqwest::Client,
        _use_proxy: bool,
        request: AllowanceHttpRequest,
    ) -> Result<AllowanceHttpResponse, TransportFailure> {
        let status = match request
            .headers
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
        {
            Some("token token-alpha") => StatusCode::UNAUTHORIZED,
            Some("token token-beta") => StatusCode::OK,
            value => panic!("unexpected credential header: {value:?}"),
        };
        Ok(AllowanceHttpResponse {
            status,
            headers: HeaderMap::new(),
            body: include_bytes!("fixtures/github-copilot-success.json").to_vec(),
        })
    }
}

#[tokio::test]
async fn one_provider_failure_does_not_fail_the_aggregate_or_change_provider_health()
-> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    let alpha = create_test_provider(&gateway, "Alpha", "github-copilot", "token-alpha").await?;
    let beta = create_test_provider(&gateway, "Beta", "github-copilot", "token-beta").await?;

    let snapshots = list_provider_allowances_with_transport(
        &gateway.admin(),
        true,
        Arc::new(IsolatedFailureTransport),
    )
    .await?;
    assert_eq!(snapshots.len(), 2);
    assert_eq!(snapshots[0].status, ProviderAllowanceStatus::Error);
    assert_eq!(
        snapshots[0].error.as_ref().map(|error| error.category),
        Some(ProviderAllowanceErrorCategory::Authentication)
    );
    assert_eq!(snapshots[1].status, ProviderAllowanceStatus::Fresh);

    for provider in [alpha, beta] {
        let current = gateway
            .storage
            .providers()
            .get(&provider.id)
            .await?
            .expect("provider remains saved");
        assert!(current.is_enabled);
        assert_eq!(current.last_test_success, provider.last_test_success);
        assert_eq!(current.last_test_at, provider.last_test_at);
    }
    Ok(())
}

#[async_trait]
impl AllowanceTransport for StaleFixtureTransport {
    async fn execute(
        &self,
        _client: reqwest::Client,
        _use_proxy: bool,
        _request: AllowanceHttpRequest,
    ) -> Result<AllowanceHttpResponse, TransportFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(AllowanceHttpResponse {
            status: if self.fail.load(Ordering::SeqCst) {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::OK
            },
            headers: HeaderMap::new(),
            body: include_bytes!("fixtures/github-copilot-success.json").to_vec(),
        })
    }
}

#[tokio::test]
async fn refresh_failure_preserves_last_success_without_leaking_credentials() -> anyhow::Result<()>
{
    let data_dir = tempfile::tempdir()?;
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    let provider =
        create_test_provider(&gateway, "Copilot", "github-copilot", "top-secret-token").await?;
    let transport = Arc::new(StaleFixtureTransport {
        fail: AtomicBool::new(false),
        calls: AtomicUsize::new(0),
    });

    let fresh = refresh_provider_allowance_with_transport(
        &gateway.admin(),
        &provider.id,
        transport.clone(),
    )
    .await?
    .expect("eligible provider");
    assert_eq!(fresh.status, ProviderAllowanceStatus::Fresh);
    transport.fail.store(true, Ordering::SeqCst);

    let stale = refresh_provider_allowance_with_transport(
        &gateway.admin(),
        &provider.id,
        transport.clone(),
    )
    .await?
    .expect("eligible provider");
    assert_eq!(stale.status, ProviderAllowanceStatus::Stale);
    assert_eq!(stale.allowances, fresh.allowances);
    assert_eq!(stale.fetched_at, fresh.fetched_at);
    assert_eq!(
        stale.error.as_ref().map(|error| error.category),
        Some(ProviderAllowanceErrorCategory::RateLimited)
    );
    let serialized = serde_json::to_string(&stale)?;
    assert!(!serialized.contains("top-secret-token"));
    assert!(!serialized.contains("inference.invalid"));
    assert_eq!(transport.calls.load(Ordering::SeqCst), 2);
    Ok(())
}

struct BlockingTransport {
    calls: AtomicUsize,
    started: Notify,
    release: Semaphore,
}

#[async_trait]
impl AllowanceTransport for BlockingTransport {
    async fn execute(
        &self,
        _client: reqwest::Client,
        _use_proxy: bool,
        _request: AllowanceHttpRequest,
    ) -> Result<AllowanceHttpResponse, TransportFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        self.release
            .acquire()
            .await
            .expect("test release semaphore")
            .forget();
        Ok(AllowanceHttpResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: include_bytes!("fixtures/github-copilot-success.json").to_vec(),
        })
    }
}

#[tokio::test]
async fn concurrent_manual_refreshes_for_one_provider_share_one_request() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    let provider =
        create_test_provider(&gateway, "Copilot", "github-copilot", "token-alpha").await?;
    let transport = Arc::new(BlockingTransport {
        calls: AtomicUsize::new(0),
        started: Notify::new(),
        release: Semaphore::new(0),
    });

    let first_gateway = gateway.clone();
    let first_transport = transport.clone();
    let provider_id = provider.id.clone();
    let first = tokio::spawn(async move {
        refresh_provider_allowance_with_transport(
            &first_gateway.admin(),
            &provider_id,
            first_transport,
        )
        .await
    });
    transport.started.notified().await;

    let second_gateway = gateway.clone();
    let second_transport = transport.clone();
    let provider_id = provider.id.clone();
    let second = tokio::spawn(async move {
        refresh_provider_allowance_with_transport(
            &second_gateway.admin(),
            &provider_id,
            second_transport,
        )
        .await
    });
    gateway
        .provider_allowance_state
        .wait_for_coalesced_fetch()
        .await;
    transport.release.add_permits(10);

    assert!(first.await??.is_some());
    assert!(second.await??.is_some());
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    Ok(())
}

struct ParallelTransport {
    calls: AtomicUsize,
    active: AtomicUsize,
    max_active: AtomicUsize,
    first_wave: Barrier,
    released: AtomicBool,
    release: Notify,
}

#[async_trait]
impl AllowanceTransport for ParallelTransport {
    async fn execute(
        &self,
        _client: reqwest::Client,
        _use_proxy: bool,
        _request: AllowanceHttpRequest,
    ) -> Result<AllowanceHttpResponse, TransportFailure> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        if call <= 4 {
            self.first_wave.wait().await;
        }
        if !self.released.load(Ordering::SeqCst) {
            self.release.notified().await;
        }
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(AllowanceHttpResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: include_bytes!("fixtures/github-copilot-success.json").to_vec(),
        })
    }
}

#[tokio::test]
async fn different_providers_refresh_concurrently_with_a_bound() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    for index in 0..6 {
        create_test_provider(
            &gateway,
            &format!("Account {index}"),
            "github-copilot",
            &format!("token-{index}"),
        )
        .await?;
    }
    let transport = Arc::new(ParallelTransport {
        calls: AtomicUsize::new(0),
        active: AtomicUsize::new(0),
        max_active: AtomicUsize::new(0),
        first_wave: Barrier::new(5),
        released: AtomicBool::new(false),
        release: Notify::new(),
    });

    let list_gateway = gateway.clone();
    let list_transport = transport.clone();
    let refresh = tokio::spawn(async move {
        list_provider_allowances_with_transport(&list_gateway.admin(), true, list_transport).await
    });
    transport.first_wave.wait().await;
    assert_eq!(transport.max_active.load(Ordering::SeqCst), 4);
    transport.released.store(true, Ordering::SeqCst);
    transport.release.notify_waiters();

    assert_eq!(refresh.await??.len(), 6);
    assert_eq!(transport.calls.load(Ordering::SeqCst), 6);
    assert_eq!(transport.max_active.load(Ordering::SeqCst), 4);
    Ok(())
}

#[derive(Clone, Copy)]
enum StaticFailure {
    Status(StatusCode),
    Transport(TransportFailure),
}

struct StaticFailureTransport(StaticFailure);

#[async_trait]
impl AllowanceTransport for StaticFailureTransport {
    async fn execute(
        &self,
        _client: reqwest::Client,
        _use_proxy: bool,
        _request: AllowanceHttpRequest,
    ) -> Result<AllowanceHttpResponse, TransportFailure> {
        match self.0 {
            StaticFailure::Status(status) => Ok(AllowanceHttpResponse {
                status,
                headers: HeaderMap::new(),
                body: br#"{"unexpected":"schema"}"#.to_vec(),
            }),
            StaticFailure::Transport(failure) => Err(failure),
        }
    }
}

#[tokio::test]
async fn every_monitor_uses_safe_error_categories() {
    for monitor in all_monitors() {
        for (failure, expected) in [
            (
                StaticFailure::Status(StatusCode::UNAUTHORIZED),
                ProviderAllowanceErrorCategory::Authentication,
            ),
            (
                StaticFailure::Status(StatusCode::TOO_MANY_REQUESTS),
                ProviderAllowanceErrorCategory::RateLimited,
            ),
            (
                StaticFailure::Transport(TransportFailure::Timeout),
                ProviderAllowanceErrorCategory::Timeout,
            ),
            (
                StaticFailure::Transport(TransportFailure::Unavailable),
                ProviderAllowanceErrorCategory::UpstreamUnavailable,
            ),
            (
                StaticFailure::Transport(TransportFailure::InvalidResponse),
                ProviderAllowanceErrorCategory::InvalidResponse,
            ),
        ] {
            let error = fetch_monitor(
                monitor,
                true,
                "never-expose-this-secret".into(),
                Default::default(),
                reqwest::Client::new(),
                Arc::new(StaticFailureTransport(failure)),
            )
            .await
            .expect_err("failure must be normalized");
            assert_eq!(error.category, expected, "{monitor:?}");
            assert!(!error.message.contains("never-expose-this-secret"));
        }
    }
}

#[test]
fn every_monitor_uses_fixed_official_endpoints_and_proxy_policy() {
    let expected_urls = [
        "https://api.anthropic.com/api/oauth/usage",
        "https://chatgpt.com/backend-api/wham/usage",
        "https://api.github.com/copilot_internal/user",
        "https://api.kimi.com/coding/v1/usages",
        "https://nano-gpt.com/api/subscription/v1/usage",
        "https://api.z.ai/api/monitor/usage/quota/limit",
        "https://open.bigmodel.cn/api/monitor/usage/quota/limit",
        "https://api.minimax.io/v1/token_plan/remains",
        "https://api.minimax.io/v1/api/openplatform/coding_plan/remains",
        "https://api.minimaxi.com/v1/token_plan/remains",
        "https://www.minimaxi.com/v1/api/openplatform/coding_plan/remains",
        "https://pass.wafer.ai/v1/inference/quota",
        "https://opencode.ai/zen/go/v1/usage",
        "https://crof.ai/usage_api/",
        "https://api.deepseek.com/user/balance",
        "https://api.neuralwatt.com/v1/quota",
        "https://grok.com/grok_api_v2.GrokBuildBilling/GetGrokCreditsConfig",
    ];
    let urls = all_monitors()
        .into_iter()
        .flat_map(|monitor| {
            monitor_requests(monitor, "secret", &Default::default()).expect("valid request")
        })
        .map(|request| {
            assert!(!request.url.contains("inference.invalid"));
            request.url
        })
        .collect::<Vec<_>>();
    assert_eq!(urls, expected_urls);
}

fn all_monitors() -> [MonitorKind; 15] {
    [
        MonitorKind::AnthropicClaudeCode,
        MonitorKind::OpenAiCodex,
        MonitorKind::GitHubCopilot,
        MonitorKind::KimiForCoding,
        MonitorKind::NanoGpt,
        MonitorKind::ZaiCodingPlan,
        MonitorKind::ZhipuAiCodingPlan,
        MonitorKind::MiniMaxCodingPlan,
        MonitorKind::MiniMaxCnCodingPlan,
        MonitorKind::Wafer,
        MonitorKind::OpenCodeGo,
        MonitorKind::Crof,
        MonitorKind::DeepSeek,
        MonitorKind::NeuralWatt,
        MonitorKind::XaiGrok,
    ]
}
