use super::*;
use stravia_core::admin::provider_allowance::{
    ProviderAllowanceResetOutcome, ProviderAllowanceResetRequest,
};

const REQUEST_ID: &str = "11111111-1111-4111-8111-111111111111";

fn request(retry: bool) -> ProviderAllowanceResetRequest {
    ProviderAllowanceResetRequest {
        grant_id: "grant-1".into(),
        request_id: REQUEST_ID.into(),
        retry,
    }
}

fn allowance(remaining: &str, resets: Option<u32>) -> UpstreamReply {
    let resets = resets.map(|remaining| {
        serde_json::json!({
            "remaining": remaining,
            "grants": [{"id": "grant-1", "label": "Free reset", "remaining": remaining,
                "expires_at_unix_ms": null, "usable": remaining > 0,
                "unavailable_reason": if remaining > 0 { None } else { Some("exhausted") }}]
        })
    });
    UpstreamReply::json(serde_json::json!({"remaining": remaining, "resets": resets}))
}

fn reset_reply(outcome: &str) -> UpstreamReply {
    UpstreamReply::json(serde_json::json!({"reset_result": {
        "request_id": REQUEST_ID, "outcome": outcome
    }}))
}

async fn setup() -> anyhow::Result<(tempfile::TempDir, Gateway, TestUpstream, String)> {
    let directory = tempfile::tempdir()?;
    let gateway = new_gateway(directory.path().to_owned()).await?;
    install(&gateway, "management-v2.wasm", false).await?;
    let mut upstream = TestUpstream::start().await;
    let provider = create_oauth_provider(
        &gateway,
        &mut upstream,
        MANAGEMENT_VENDOR,
        "Reset fixture",
        "reset-fixture-token",
    )
    .await?;
    Ok((directory, gateway, upstream, provider.id))
}

fn spawn_reset(
    gateway: &Gateway,
    provider_id: &str,
    retry: bool,
) -> tokio::task::JoinHandle<
    anyhow::Result<stravia_core::admin::provider_allowance::ProviderAllowanceResetResult>,
> {
    let gateway = gateway.clone();
    let provider_id = provider_id.to_owned();
    tokio::spawn(async move {
        gateway
            .admin()
            .reset_provider_allowance(&provider_id, request(retry))
            .await
    })
}

async fn respond_read(upstream: &mut TestUpstream, reply: UpstreamReply) {
    let read = tokio::time::timeout(std::time::Duration::from_secs(10), upstream.next())
        .await
        .unwrap();
    assert_eq!(read.uri.path(), "/allowance");
    read.reply(reply);
}

async fn respond_reset(upstream: &mut TestUpstream, reply: UpstreamReply) {
    let reset = tokio::time::timeout(std::time::Duration::from_secs(10), upstream.next())
        .await
        .unwrap();
    assert_eq!(reset.uri.path(), "/reset");
    let body: serde_json::Value = serde_json::from_slice(&reset.body).unwrap();
    assert_eq!(body["grant_id"], "grant-1");
    assert_eq!(body["request_id"], REQUEST_ID);
    reset.reply(reply);
}

#[tokio::test]
async fn reset_rejects_provider_without_advertised_capability() -> anyhow::Result<()> {
    let (_directory, gateway, mut upstream, id) = setup().await?;
    let reset = spawn_reset(&gateway, &id, false);
    respond_read(&mut upstream, allowance("0", None)).await;
    assert!(reset.await?.is_err());
    upstream.assert_no_request();
    Ok(())
}

#[tokio::test]
async fn reset_reports_business_failure_and_invalidates_cached_allowance() -> anyhow::Result<()> {
    let (_directory, gateway, mut upstream, id) = setup().await?;
    refresh_allowance(&gateway, &mut upstream, &id, allowance("0", Some(1))).await?;
    let reset = spawn_reset(&gateway, &id, false);
    respond_read(&mut upstream, allowance("0", Some(1))).await;
    respond_reset(&mut upstream, reset_reply("cooldown")).await;
    assert_eq!(
        reset.await??.outcome,
        ProviderAllowanceResetOutcome::Cooldown
    );
    let gw = gateway.clone();
    let provider_id = id.clone();
    let read = tokio::spawn(async move { gw.admin().get_provider_allowance(&provider_id).await });
    respond_read(&mut upstream, allowance("0", Some(1))).await;
    let snapshot = read.await??.expect("fresh allowance after reset");
    assert_eq!(snapshot.resets.unwrap().remaining, 1);
    Ok(())
}

#[tokio::test]
async fn successful_reset_keeps_suspension_until_fresh_recovery_evidence() -> anyhow::Result<()> {
    let (_directory, gateway, mut upstream, id) = setup().await?;
    let snapshot = set_guards(
        &gateway,
        &mut upstream,
        &id,
        &["requests"],
        allowance("0", Some(1)),
    )
    .await?;
    assert!(snapshot.suspension.is_some());
    let reset = spawn_reset(&gateway, &id, false);
    respond_read(&mut upstream, allowance("0", Some(1))).await;
    respond_reset(&mut upstream, reset_reply("reset")).await;
    assert_eq!(reset.await??.outcome, ProviderAllowanceResetOutcome::Reset);
    assert!(
        gateway
            .admin()
            .get_provider_allowance_suspension(&id)
            .await?
            .is_some()
    );
    let snapshot =
        refresh_allowance(&gateway, &mut upstream, &id, allowance("12", Some(0))).await?;
    assert!(snapshot.suspension.is_none());
    assert!(
        gateway
            .admin()
            .get_provider_allowance_suspension(&id)
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn reset_rejects_retry_without_pending_record() -> anyhow::Result<()> {
    let (_directory, gateway, mut upstream, id) = setup().await?;
    let retry = spawn_reset(&gateway, &id, true);
    assert!(retry.await?.is_err());
    upstream.assert_no_request();
    Ok(())
}

#[tokio::test]
async fn ambiguous_reset_reuses_request_id_after_grant_is_depleted() -> anyhow::Result<()> {
    let (_directory, gateway, mut upstream, id) = setup().await?;
    let first = spawn_reset(&gateway, &id, false);
    respond_read(&mut upstream, allowance("0", Some(1))).await;
    let mut failure = UpstreamReply::json(serde_json::json!({"error": "reply lost"}));
    failure.status = StatusCode::SERVICE_UNAVAILABLE;
    respond_reset(&mut upstream, failure).await;
    assert!(first.await?.is_err());
    let retry = spawn_reset(&gateway, &id, true);
    respond_read(&mut upstream, allowance("12", Some(0))).await;
    respond_reset(&mut upstream, reset_reply("reset")).await;
    assert_eq!(retry.await??.outcome, ProviderAllowanceResetOutcome::Reset);
    let repeated = spawn_reset(&gateway, &id, true);
    assert_eq!(
        repeated.await??.outcome,
        ProviderAllowanceResetOutcome::Reset
    );
    upstream.assert_no_request();
    Ok(())
}
