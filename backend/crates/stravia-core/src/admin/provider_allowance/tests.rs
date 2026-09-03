use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use reqwest::{StatusCode, header::HeaderMap};
use tokio::sync::{Barrier, Notify, Semaphore};

use crate::Gateway;
use crate::config::GatewayConfig;
use crate::db::models::{CreateProviderRecord, UpdateProvider};

use super::{
    AllowanceCondition, AllowanceHttpRequest, AllowanceHttpResponse, AllowanceKind,
    AllowanceTransport, ExhaustionForecastStatus, MonitorKind, ProviderAllowanceErrorCategory,
    ProviderAllowanceStatus, TransportFailure, fetch_monitor,
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
fn zhipu_preserves_each_reported_token_window() {
    let parsed = parse_monitor_response(
        MonitorKind::ZhipuAiCodingPlan,
        br#"{
            "data": {
                "limits": [
                    {
                        "type": "TOKENS_LIMIT",
                        "unit": 3,
                        "number": 5,
                        "percentage": 48,
                        "nextResetTime": 1790000000
                    },
                    {
                        "type": "TOKENS_LIMIT",
                        "unit": 6,
                        "number": 1,
                        "percentage": 12,
                        "nextResetTime": 1790500000
                    },
                    {
                        "type": "TIME_LIMIT",
                        "percentage": 5,
                        "nextResetTime": 1790600000
                    }
                ]
            }
        }"#,
    )
    .expect("Zhipu multi-window response should parse");

    assert_eq!(parsed.allowances.len(), 3);
    assert_eq!(parsed.allowances[0].key, "5h");
    assert_eq!(parsed.allowances[0].window_seconds, Some(18_000));
    assert_eq!(parsed.allowances[0].used_percent, Some(48.0));
    assert_eq!(parsed.allowances[1].key, "weekly");
    assert_eq!(parsed.allowances[1].window_seconds, Some(604_800));
    assert_eq!(parsed.allowances[1].used_percent, Some(12.0));
    assert_eq!(parsed.allowances[1].reset_at, Some(1_790_500_000_000));
    assert_eq!(parsed.allowances[2].key, "mcp_tools");
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
            "5h",
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

struct ConditionFixtureTransport;

#[async_trait]
impl AllowanceTransport for ConditionFixtureTransport {
    async fn execute(
        &self,
        _client: reqwest::Client,
        _use_proxy: bool,
        request: AllowanceHttpRequest,
    ) -> Result<AllowanceHttpResponse, TransportFailure> {
        let authorization = request
            .headers
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .expect("authorization header");
        let snapshot = match authorization {
            "token token-exhausted" => serde_json::json!({
                "entitlement": 100,
                "remaining": 0,
                "unlimited": false
            }),
            "token token-tight" => serde_json::json!({
                "entitlement": 100,
                "remaining": 19,
                "unlimited": false
            }),
            "token token-normal" => serde_json::json!({
                "entitlement": 100,
                "remaining": 20,
                "unlimited": false
            }),
            "token token-unknown" => serde_json::json!({ "unlimited": true }),
            value => panic!("unexpected credential header: {value}"),
        };
        Ok(AllowanceHttpResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: serde_json::to_vec(&serde_json::json!({
                "quota_reset_date": "2026-09-08T00:00:00Z",
                "quota_snapshots": { "premium_interactions": snapshot }
            }))
            .expect("allowance fixture"),
        })
    }
}

#[tokio::test]
async fn admin_service_derives_allowance_condition_from_the_live_snapshot() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    let transport = Arc::new(ConditionFixtureTransport);

    for (name, token, expected) in [
        (
            "Exhausted",
            "token-exhausted",
            Some(AllowanceCondition::Exhausted),
        ),
        ("Tight", "token-tight", Some(AllowanceCondition::Tight)),
        (
            "Normal boundary",
            "token-normal",
            Some(AllowanceCondition::Normal),
        ),
        ("Unknown", "token-unknown", None),
    ] {
        let provider = create_test_provider(&gateway, name, "github-copilot", token).await?;
        let snapshot = refresh_provider_allowance_with_transport(
            &gateway.admin(),
            &provider.id,
            transport.clone(),
        )
        .await?
        .expect("eligible provider snapshot");
        assert_eq!(snapshot.allowances[0].condition, expected);
    }

    Ok(())
}

#[tokio::test]
async fn fresh_monitor_reads_persist_account_samples_and_start_with_unknown_forecasts()
-> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    let provider =
        create_test_provider(&gateway, "Sampled", "github-copilot", "token-normal").await?;

    let snapshot = refresh_provider_allowance_with_transport(
        &gateway.admin(),
        &provider.id,
        Arc::new(ConditionFixtureTransport),
    )
    .await?
    .expect("eligible provider snapshot");

    assert_eq!(
        snapshot.allowances[0].forecast.status,
        ExhaustionForecastStatus::Unknown
    );
    let samples = gateway
        .allowance_samples
        .list_for_item(&provider.id, "premium_interactions", i64::MIN)
        .await?;
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].remaining_value, Some(20.0));

    Ok(())
}

#[tokio::test]
async fn fresh_monitor_reads_prune_samples_older_than_fourteen_days() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    let provider =
        create_test_provider(&gateway, "Retained", "github-copilot", "token-normal").await?;
    let transport = Arc::new(ConditionFixtureTransport);
    let snapshot = refresh_provider_allowance_with_transport(
        &gateway.admin(),
        &provider.id,
        transport.clone(),
    )
    .await?
    .expect("initial allowance");
    gateway
        .allowance_samples
        .record_snapshot_at(
            &snapshot,
            chrono::Utc::now().timestamp_millis() - 15 * 24 * 60 * 60 * 1000,
        )
        .await?;

    refresh_provider_allowance_with_transport(&gateway.admin(), &provider.id, transport)
        .await?
        .expect("refresh allowance");
    let samples = gateway
        .allowance_samples
        .list_for_item(&provider.id, "premium_interactions", i64::MIN)
        .await?;
    assert_eq!(samples.len(), 2);

    Ok(())
}

struct ForecastFixtureTransport {
    reset_at: i64,
}

#[async_trait]
impl AllowanceTransport for ForecastFixtureTransport {
    async fn execute(
        &self,
        _client: reqwest::Client,
        _use_proxy: bool,
        _request: AllowanceHttpRequest,
    ) -> Result<AllowanceHttpResponse, TransportFailure> {
        Ok(AllowanceHttpResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: serde_json::to_vec(&serde_json::json!({
                "data": {
                    "level": "Pro",
                    "limits": [{
                        "type": "CREDIT_LIMIT",
                        "unit": 6,
                        "number": 1,
                        "percentage": 80,
                        "currentValue": 800,
                        "usage": 1000,
                        "remaining": 200,
                        "nextResetTime": self.reset_at
                    }]
                }
            }))
            .expect("forecast fixture"),
        })
    }
}

#[tokio::test]
async fn current_window_samples_forecast_exhaustion_before_reset() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    let now = chrono::Utc::now().timestamp_millis();
    let reset_at = now + 48 * 60 * 60 * 1000;
    let provider =
        create_test_provider(&gateway, "Forecasted", "zai-coding-plan", "forecast-token").await?;
    let transport = Arc::new(ForecastFixtureTransport { reset_at });

    let current = refresh_provider_allowance_with_transport(
        &gateway.admin(),
        &provider.id,
        transport.clone(),
    )
    .await?
    .expect("current allowance");
    let mut historical = current.clone();
    historical.allowances[0].used_percent = Some(50.0);
    gateway
        .allowance_samples
        .record_snapshot_at(&historical, now - 25 * 60 * 60 * 1000)
        .await?;

    let forecasted =
        refresh_provider_allowance_with_transport(&gateway.admin(), &provider.id, transport)
            .await?
            .expect("forecasted allowance");
    let forecast = &forecasted.allowances[0].forecast;
    assert_eq!(forecast.status, ExhaustionForecastStatus::WillExhaust);
    assert_eq!(forecast.projected_remaining_percent, Some(0.0));
    assert!(
        forecast
            .exhausts_at
            .is_some_and(|at| at > now && at < reset_at)
    );

    Ok(())
}

#[tokio::test]
async fn forecast_excludes_prior_windows_and_projects_remaining_at_reset() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    let now = chrono::Utc::now().timestamp_millis();
    let reset_at = now + 48 * 60 * 60 * 1000;
    let provider =
        create_test_provider(&gateway, "No risk", "zai-coding-plan", "forecast-token").await?;
    let transport = Arc::new(ForecastFixtureTransport { reset_at });
    let current = refresh_provider_allowance_with_transport(
        &gateway.admin(),
        &provider.id,
        transport.clone(),
    )
    .await?
    .expect("current allowance");

    let mut within_window = current.clone();
    within_window.allowances[0].used_percent = Some(70.0);
    gateway
        .allowance_samples
        .record_snapshot_at(&within_window, now - 25 * 60 * 60 * 1000)
        .await?;
    let mut prior_window = current.clone();
    prior_window.allowances[0].used_percent = Some(0.0);
    gateway
        .allowance_samples
        .record_snapshot_at(&prior_window, now - 6 * 24 * 60 * 60 * 1000)
        .await?;

    let forecasted =
        refresh_provider_allowance_with_transport(&gateway.admin(), &provider.id, transport)
            .await?
            .expect("forecasted allowance");
    let forecast = &forecasted.allowances[0].forecast;
    assert_eq!(forecast.status, ExhaustionForecastStatus::NoRisk);
    assert!(
        forecast
            .projected_remaining_percent
            .is_some_and(|value| value > 0.0 && value < 5.0)
    );
    assert!(forecast.exhausts_at.is_none());

    Ok(())
}

struct BalanceFixtureTransport {
    balance: f64,
}

#[async_trait]
impl AllowanceTransport for BalanceFixtureTransport {
    async fn execute(
        &self,
        _client: reqwest::Client,
        _use_proxy: bool,
        _request: AllowanceHttpRequest,
    ) -> Result<AllowanceHttpResponse, TransportFailure> {
        Ok(AllowanceHttpResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: serde_json::to_vec(&serde_json::json!({
                "is_available": true,
                "balance_infos": [{
                    "currency": "USD",
                    "total_balance": self.balance.to_string()
                }]
            }))
            .expect("balance fixture"),
        })
    }
}

#[tokio::test]
async fn balance_samples_without_reset_forecast_when_the_balance_reaches_zero() -> anyhow::Result<()>
{
    let data_dir = tempfile::tempdir()?;
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    let provider = create_test_provider(&gateway, "Balance", "deepseek", "balance-token").await?;
    let transport = Arc::new(BalanceFixtureTransport { balance: 10.0 });
    let now = chrono::Utc::now().timestamp_millis();
    let current = refresh_provider_allowance_with_transport(
        &gateway.admin(),
        &provider.id,
        transport.clone(),
    )
    .await?
    .expect("current balance");
    let mut historical = current.clone();
    historical.allowances[0]
        .remaining
        .as_mut()
        .expect("balance amount")
        .value = 40.0;
    gateway
        .allowance_samples
        .record_snapshot_at(&historical, now - 25 * 60 * 60 * 1000)
        .await?;

    let forecasted =
        refresh_provider_allowance_with_transport(&gateway.admin(), &provider.id, transport)
            .await?
            .expect("forecasted balance");
    let forecast = &forecasted.allowances[0].forecast;
    assert_eq!(forecast.status, ExhaustionForecastStatus::WillExhaust);
    assert!(forecast.exhausts_at.is_some_and(|at| at > now));
    assert!(forecast.projected_remaining_percent.is_none());

    Ok(())
}

#[tokio::test]
async fn exhausted_balance_is_not_forecast_as_no_risk() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let (gateway, _logs) = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    let transport = Arc::new(BalanceFixtureTransport { balance: 0.0 });
    for (index, historical_balance) in [40.0, 0.0].into_iter().enumerate() {
        let provider = create_test_provider(
            &gateway,
            &format!("Exhausted balance {index}"),
            "deepseek",
            "balance-token",
        )
        .await?;
        let now = chrono::Utc::now().timestamp_millis();
        let current = refresh_provider_allowance_with_transport(
            &gateway.admin(),
            &provider.id,
            transport.clone(),
        )
        .await?
        .expect("current balance");
        let mut historical = current.clone();
        historical.allowances[0]
            .remaining
            .as_mut()
            .expect("balance amount")
            .value = historical_balance;
        gateway
            .allowance_samples
            .record_snapshot_at(&historical, now - 25 * 60 * 60 * 1000)
            .await?;

        let forecasted = refresh_provider_allowance_with_transport(
            &gateway.admin(),
            &provider.id,
            transport.clone(),
        )
        .await?
        .expect("forecasted balance");
        let forecast = &forecasted.allowances[0].forecast;
        assert_eq!(forecast.status, ExhaustionForecastStatus::WillExhaust);
        let observed_at = chrono::Utc::now().timestamp_millis();
        assert!(forecast.exhausts_at.is_some_and(|at| at <= observed_at));
    }
    Ok(())
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
