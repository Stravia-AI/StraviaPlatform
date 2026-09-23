use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures::future::{BoxFuture, FutureExt, Shared};
use futures::stream::{self, StreamExt};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, RwLock};

use crate::admin::AdminService;
use crate::db::models::Provider;
use crate::plugin::{VendorCallContext, VendorRequest};

use super::samples::{AllowanceSample, AllowanceSampleStore, SAMPLE_RETENTION_MILLIS};
use super::{
    Allowance, AllowanceAmount, AllowanceCondition, AllowanceKind, ExhaustionForecast,
    ExhaustionForecastStatus, ModelAllowance, ProviderAllowanceError,
    ProviderAllowanceErrorCategory, ProviderAllowanceSnapshot, ProviderAllowanceStatus,
    ProviderAllowanceTarget,
};

const SUCCESS_TTL: Duration = Duration::from_secs(180);
pub(crate) const SAMPLE_INTERVAL: Duration = Duration::from_secs(30 * 60);
const MIN_FORECAST_SPAN_MILLIS: i64 = 24 * 60 * 60 * 1000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PARALLEL_REFRESHES: usize = 4;

type SharedFetch =
    Shared<BoxFuture<'static, Result<Option<ProviderAllowanceSnapshot>, Arc<anyhow::Error>>>>;

#[derive(Clone, Default)]
pub(crate) struct ProviderAllowanceState {
    inner: Arc<ProviderAllowanceStateInner>,
}

#[derive(Default)]
struct ProviderAllowanceStateInner {
    cache: RwLock<HashMap<String, CacheEntry>>,
    inflight: Mutex<HashMap<String, SharedFetch>>,
}

#[derive(Clone)]
struct CacheEntry {
    identity: String,
    snapshot: ProviderAllowanceSnapshot,
    successful_at: Option<Instant>,
}

impl AdminService {
    pub async fn list_provider_allowances(&self) -> anyhow::Result<Vec<ProviderAllowanceSnapshot>> {
        list_provider_allowances(self, false).await
    }

    /// Non-blocking list: returns each eligible provider's identity plus its
    /// cached snapshot and starts a coalesced Wasm allowance read when stale.
    pub async fn list_provider_allowance_targets(
        &self,
    ) -> anyhow::Result<Vec<ProviderAllowanceTarget>> {
        let providers = eligible_allowance_providers(self).await?;
        let mut targets = Vec::with_capacity(providers.len());
        for provider in providers {
            let identity = provider_identity(self, &provider).await?;
            let previous = {
                let cache = self.gw.provider_allowance_state.inner.cache.read().await;
                cache
                    .get(&provider.id)
                    .filter(|entry| entry.identity == identity)
                    .cloned()
            };
            let fresh = previous.as_ref().is_some_and(|entry| {
                entry
                    .successful_at
                    .is_some_and(|successful_at| successful_at.elapsed() < SUCCESS_TTL)
            });
            let snapshot = previous.map(|entry| entry.snapshot);
            if fresh {
                targets.push(allowance_target(&provider, snapshot, false));
            } else {
                spawn_allowance_fetch(self.clone(), provider.clone());
                targets.push(allowance_target(&provider, snapshot, true));
            }
        }
        Ok(targets)
    }

    pub async fn get_provider_allowance(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<Option<ProviderAllowanceSnapshot>> {
        provider_allowance(self, provider_id, false).await
    }

    pub async fn refresh_provider_allowance(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<Option<ProviderAllowanceSnapshot>> {
        provider_allowance(self, provider_id, true).await
    }
}

async fn provider_allowance(
    admin: &AdminService,
    provider_id: &str,
    force: bool,
) -> anyhow::Result<Option<ProviderAllowanceSnapshot>> {
    let Some(provider) = admin.gw.storage.providers().get(provider_id).await? else {
        return Ok(None);
    };
    if !eligible_allowance_provider(admin, &provider) {
        admin
            .gw
            .provider_allowance_state
            .inner
            .cache
            .write()
            .await
            .remove(provider_id);
        return Ok(None);
    }
    fetch_provider_allowance(admin, provider, force).await
}

async fn eligible_allowance_providers(admin: &AdminService) -> anyhow::Result<Vec<Provider>> {
    let mut providers = admin.gw.storage.providers().list().await?;
    providers.retain(|provider| eligible_allowance_provider(admin, provider));
    providers.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.id.cmp(&right.id))
    });

    let eligible_ids = providers
        .iter()
        .map(|provider| provider.id.clone())
        .collect::<HashSet<_>>();
    admin
        .gw
        .provider_allowance_state
        .inner
        .cache
        .write()
        .await
        .retain(|provider_id, _| eligible_ids.contains(provider_id));
    Ok(providers)
}

fn eligible_allowance_provider(admin: &AdminService, provider: &Provider) -> bool {
    if !provider.is_enabled {
        return false;
    }
    let Some(vendor_id) = provider
        .vendor
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return false;
    };
    let Ok(descriptor) = admin.gw.vendor_plugins.descriptor(vendor_id) else {
        return false;
    };
    let channel_id = provider
        .channel
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("default");
    descriptor.channels.iter().any(|channel| {
        channel.id == channel_id
            && channel
                .capabilities
                .contains(&stravia_vendor_sdk::Capability::Allowance)
    })
}

fn spawn_allowance_fetch(admin: AdminService, provider: Provider) {
    let provider_id = provider.id.clone();
    tokio::spawn(async move {
        if let Err(error) = fetch_provider_allowance(&admin, provider, false).await {
            tracing::warn!(
                provider_id = %provider_id,
                error = %error,
                "background provider allowance fetch failed"
            );
        }
    });
}

fn allowance_target(
    provider: &Provider,
    snapshot: Option<ProviderAllowanceSnapshot>,
    refreshing: bool,
) -> ProviderAllowanceTarget {
    ProviderAllowanceTarget {
        provider_id: provider.id.clone(),
        provider_name: provider.name.clone(),
        catalog_provider_id: provider.preset_key.clone().unwrap_or_default(),
        channel: provider.channel.clone().unwrap_or_else(|| "default".into()),
        snapshot,
        refreshing,
    }
}

async fn list_provider_allowances(
    admin: &AdminService,
    force: bool,
) -> anyhow::Result<Vec<ProviderAllowanceSnapshot>> {
    let providers = eligible_allowance_providers(admin).await?;
    let results = stream::iter(providers.into_iter().map(|provider| {
        let admin = admin.clone();
        async move {
            let provider_for_error = provider.clone();
            match fetch_provider_allowance(&admin, provider, force).await {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    tracing::error!(
                        provider_id = %provider_for_error.id,
                        error = %error,
                        "failed to revalidate provider allowance state"
                    );
                    Some(error_snapshot(
                        &provider_for_error,
                        safe_error(error_category(&error)),
                    ))
                }
            }
        }
    }))
    .buffered(MAX_PARALLEL_REFRESHES)
    .collect::<Vec<_>>()
    .await;
    Ok(results.into_iter().flatten().collect())
}

async fn fetch_provider_allowance(
    admin: &AdminService,
    provider: Provider,
    force: bool,
) -> anyhow::Result<Option<ProviderAllowanceSnapshot>> {
    if !eligible_allowance_provider(admin, &provider) {
        return Ok(None);
    }
    let identity = provider_identity(admin, &provider).await?;
    let previous = {
        let cache = admin.gw.provider_allowance_state.inner.cache.read().await;
        cache
            .get(&provider.id)
            .filter(|entry| entry.identity == identity)
            .cloned()
    };
    if !force
        && let Some(entry) = previous.as_ref()
        && entry
            .successful_at
            .is_some_and(|successful_at| successful_at.elapsed() < SUCCESS_TTL)
    {
        return Ok(Some(entry.snapshot.clone()));
    }

    let inflight_key = format!("{}:{identity}", provider.id);
    let shared_fetch = {
        let mut inflight = admin
            .gw
            .provider_allowance_state
            .inner
            .inflight
            .lock()
            .await;
        if let Some(fetch) = inflight.get(&inflight_key) {
            fetch.clone()
        } else {
            let admin = admin.clone();
            let state = admin.gw.provider_allowance_state.clone();
            let identity_for_future = identity.clone();
            let provider_id = provider.id.clone();
            let inflight_key_for_future = inflight_key.clone();
            let fetch = async move {
                let result: anyhow::Result<Option<ProviderAllowanceSnapshot>> = async {
                    let Some(current) = admin.gw.storage.providers().get(&provider_id).await?
                    else {
                        return Ok(None);
                    };
                    if !eligible_allowance_provider(&admin, &current)
                        || provider_identity(&admin, &current).await? != identity_for_future
                    {
                        return Ok(None);
                    }

                    let context = VendorCallContext::new(
                        stravia_runtime_contract::CancellationToken::new(),
                        Instant::now() + REQUEST_TIMEOUT,
                    );
                    let prepared = admin
                        .gw
                        .prepare_vendor_execution(
                            &provider_id,
                            None,
                            stravia_vendor_sdk::Operation::Allowance,
                            &context,
                        )
                        .await;
                    let execution = match prepared {
                        Ok(prepared) => {
                            let credential_version = prepared.credential_version();
                            let result = admin
                                .gw
                                .execute_prepared_vendor(
                                    prepared,
                                    VendorRequest::Allowance(
                                        stravia_vendor_sdk::AllowanceRequest::default(),
                                    ),
                                    context,
                                )
                                .await;
                            // ADR-0073：Allowance 与 Infer 携带同一份凭据，
                            // 上游拒绝同样算失效证据。
                            if let Err(error) = &result
                                && crate::plugin::execution::is_credential_rejection(error)
                            {
                                admin
                                    .gw
                                    .mark_provider_credential_invalid(
                                        &provider_id,
                                        credential_version,
                                    )
                                    .await;
                            }
                            result
                        }
                        Err(error) => Err(error),
                    };

                    let (mut snapshot, publication) = match execution {
                        Ok(execution) => {
                            let response = match execution.output {
                                stravia_vendor_sdk::OperationOutput::Allowance(response) => {
                                    response
                                }
                                _ => anyhow::bail!("vendor returned a non-allowance result"),
                            };
                            let mapped = map_allowance_response(&current, response).unwrap_or_else(
                                |error| {
                                    tracing::warn!(
                                        provider_id = %provider_id,
                                        error = %error,
                                        "vendor allowance result was invalid"
                                    );
                                    stale_or_error_snapshot(
                                        &current,
                                        previous.as_ref().map(|entry| &entry.snapshot),
                                        safe_error(ProviderAllowanceErrorCategory::InvalidResponse),
                                    )
                                },
                            );
                            (mapped, Some(execution.publication))
                        }
                        Err(error) => {
                            if matches!(
                                error.downcast_ref::<stravia_vendor_runtime::RuntimeError>(),
                                Some(stravia_vendor_runtime::RuntimeError::Cancelled)
                            ) || error.to_string().contains("vendor operation was cancelled")
                            {
                                return Err(error);
                            }
                            (
                                stale_or_error_snapshot(
                                    &current,
                                    previous.as_ref().map(|entry| &entry.snapshot),
                                    safe_error(error_category(&error)),
                                ),
                                None,
                            )
                        }
                    };

                    let Some(latest) = admin.gw.storage.providers().get(&provider_id).await? else {
                        return Ok(None);
                    };
                    if !eligible_allowance_provider(&admin, &latest)
                        || provider_identity(&admin, &latest).await? != identity_for_future
                    {
                        return Ok(None);
                    }

                    // Successful guest results remain fenced until the final
                    // sample, forecast and cache publication has completed.
                    let _publication_guard = match publication {
                        Some(publication) => Some(publication.write_fence().await?),
                        None => None,
                    };

                    if snapshot.status == ProviderAllowanceStatus::Fresh {
                        if let Err(error) = admin
                            .gw
                            .allowance_samples
                            .record_snapshot_at(&snapshot, chrono::Utc::now().timestamp_millis())
                            .await
                        {
                            tracing::warn!(
                                provider_id = %provider_id,
                                error = ?error,
                                "provider allowance sample write failed"
                            );
                        }
                        if let Err(error) = apply_forecasts(
                            &mut snapshot,
                            &admin.gw.allowance_samples,
                            chrono::Utc::now().timestamp_millis(),
                        )
                        .await
                        {
                            tracing::warn!(
                                provider_id = %provider_id,
                                error = ?error,
                                "provider allowance forecast load failed"
                            );
                        }
                    }

                    let successful_at =
                        (snapshot.status == ProviderAllowanceStatus::Fresh).then(Instant::now);
                    state.inner.cache.write().await.insert(
                        provider_id,
                        CacheEntry {
                            identity: identity_for_future,
                            snapshot: snapshot.clone(),
                            successful_at,
                        },
                    );
                    Ok(Some(snapshot))
                }
                .await;
                state
                    .inner
                    .inflight
                    .lock()
                    .await
                    .remove(&inflight_key_for_future);
                result.map_err(Arc::new)
            }
            .boxed()
            .shared();
            inflight.insert(inflight_key.clone(), fetch.clone());
            fetch
        }
    };

    shared_fetch
        .await
        .map_err(|error| anyhow::anyhow!("provider allowance revalidation failed: {error:#}"))
}

async fn provider_identity(admin: &AdminService, provider: &Provider) -> anyhow::Result<String> {
    let mut digest = Sha256::new();
    for value in [
        provider.id.as_str(),
        provider.name.as_str(),
        provider.vendor.as_deref().unwrap_or_default(),
        provider.preset_key.as_deref().unwrap_or_default(),
        provider.channel.as_deref().unwrap_or("default"),
        provider.base_url.as_str(),
        provider.protocol.as_str(),
        provider.api_key.as_str(),
        provider.adapter_credentials.as_str(),
        provider.vendor_options.as_str(),
        provider.auth_mode.as_str(),
        if provider.use_proxy {
            "proxy"
        } else {
            "direct"
        },
        provider.updated_at.as_str(),
    ] {
        digest.update(value.as_bytes());
        digest.update([0]);
    }
    if let Some(vendor_id) = provider.vendor.as_deref()
        && let Ok(descriptor) = admin.gw.vendor_plugins.descriptor(vendor_id)
    {
        digest.update(serde_json::to_vec(&descriptor)?);
        digest.update([0]);
    }
    if provider.auth_mode.trim() == "oauth"
        && let Some(credential) = admin
            .gw
            .storage
            .oauth_credentials()
            .get(&provider.id)
            .await?
    {
        for value in [
            credential.connection_id.as_str(),
            credential.access_token.as_str(),
            credential.refresh_token.as_deref().unwrap_or_default(),
            credential.expires_at.as_deref().unwrap_or_default(),
            credential.resource_url.as_deref().unwrap_or_default(),
            credential.subject_id.as_deref().unwrap_or_default(),
            credential.meta.as_str(),
        ] {
            digest.update(value.as_bytes());
            digest.update([0]);
        }
    }
    Ok(URL_SAFE_NO_PAD.encode(digest.finalize()))
}

fn map_allowance_response(
    provider: &Provider,
    response: stravia_vendor_sdk::AllowanceResponse,
) -> anyhow::Result<ProviderAllowanceSnapshot> {
    let allowances = response
        .allowances
        .into_iter()
        .map(map_allowance)
        .collect::<anyhow::Result<Vec<_>>>()?;
    let models = response
        .models
        .into_iter()
        .map(|model| {
            let model_id = model.model.trim();
            anyhow::ensure!(
                !model_id.is_empty(),
                "model allowance has an empty model id"
            );
            Ok(ModelAllowance {
                model: model_id.to_owned(),
                allowances: model
                    .allowances
                    .into_iter()
                    .map(map_allowance)
                    .collect::<anyhow::Result<Vec<_>>>()?,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    anyhow::ensure!(
        !allowances.is_empty() || models.iter().any(|model| !model.allowances.is_empty()),
        "allowance result is empty"
    );
    Ok(ProviderAllowanceSnapshot {
        provider_id: provider.id.clone(),
        provider_name: provider.name.clone(),
        catalog_provider_id: provider.preset_key.clone().unwrap_or_default(),
        channel: provider.channel.clone().unwrap_or_else(|| "default".into()),
        plan_label: response
            .plan_label
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()),
        status: ProviderAllowanceStatus::Fresh,
        allowances,
        models,
        fetched_at: Some(chrono::Utc::now().to_rfc3339()),
        error: None,
    })
}

fn map_allowance(item: stravia_vendor_sdk::AllowanceItem) -> anyhow::Result<Allowance> {
    let key = item.key.trim();
    let label = item.label.trim();
    anyhow::ensure!(!key.is_empty(), "allowance key is empty");
    anyhow::ensure!(!label.is_empty(), "allowance label is empty");
    let kind = match item.kind.as_str() {
        "quota_window" => AllowanceKind::QuotaWindow,
        "request_allowance" => AllowanceKind::RequestAllowance,
        "balance" => AllowanceKind::Balance,
        other => anyhow::bail!("unsupported allowance kind `{other}`"),
    };
    let used = item.used.map(map_amount).transpose()?;
    let remaining = item.remaining.map(map_amount).transpose()?;
    let limit = item.limit.map(map_amount).transpose()?;
    ensure_compatible_amounts([used.as_ref(), remaining.as_ref(), limit.as_ref()])?;
    let used_percent = item
        .used_percent
        .map(|value| parse_decimal(&value, "used percent"))
        .transpose()?;
    let derive_condition = item.condition.is_none();
    let condition = match item.condition.as_deref() {
        None | Some("unknown") => None,
        Some("normal") => Some(AllowanceCondition::Normal),
        Some("tight") => Some(AllowanceCondition::Tight),
        Some("exhausted") => Some(AllowanceCondition::Exhausted),
        Some(other) => anyhow::bail!("unsupported allowance condition `{other}`"),
    };
    let mut allowance = Allowance {
        key: key.to_owned(),
        label: label.to_owned(),
        kind,
        used,
        remaining,
        limit,
        used_percent,
        window_seconds: item.window_seconds,
        reset_at: item.resets_at_unix_ms,
        condition,
        forecast: ExhaustionForecast::default(),
    };
    if derive_condition {
        allowance.condition = allowance_condition(&allowance);
    }
    Ok(allowance)
}

fn map_amount(amount: stravia_vendor_sdk::AllowanceAmount) -> anyhow::Result<AllowanceAmount> {
    let unit = amount.unit.trim();
    anyhow::ensure!(!unit.is_empty(), "allowance amount unit is empty");
    let currency = amount
        .currency
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    Ok(AllowanceAmount {
        value: parse_decimal(&amount.value, "allowance amount")?,
        unit: unit.to_owned(),
        currency,
    })
}

fn parse_decimal(value: &str, label: &str) -> anyhow::Result<f64> {
    let parsed = value
        .parse::<f64>()
        .map_err(|_| anyhow::anyhow!("{label} is not a decimal"))?;
    anyhow::ensure!(parsed.is_finite(), "{label} must be finite");
    Ok(parsed)
}

fn ensure_compatible_amounts(amounts: [Option<&AllowanceAmount>; 3]) -> anyhow::Result<()> {
    let mut identity: Option<(&str, Option<&str>)> = None;
    for amount in amounts.into_iter().flatten() {
        let current = (amount.unit.as_str(), amount.currency.as_deref());
        if let Some(identity) = identity {
            anyhow::ensure!(
                identity == current,
                "allowance amounts use incompatible units"
            );
        } else {
            identity = Some(current);
        }
    }
    Ok(())
}

fn error_category(error: &anyhow::Error) -> ProviderAllowanceErrorCategory {
    match error.downcast_ref::<stravia_vendor_runtime::RuntimeError>() {
        Some(stravia_vendor_runtime::RuntimeError::Plugin {
            kind: stravia_vendor_sdk::ErrorKind::Auth,
            ..
        }) => ProviderAllowanceErrorCategory::Authentication,
        Some(stravia_vendor_runtime::RuntimeError::Plugin {
            upstream_status: Some(429),
            ..
        }) => ProviderAllowanceErrorCategory::RateLimited,
        Some(stravia_vendor_runtime::RuntimeError::DeadlineExceeded) => {
            ProviderAllowanceErrorCategory::Timeout
        }
        Some(stravia_vendor_runtime::RuntimeError::Plugin {
            kind:
                stravia_vendor_sdk::ErrorKind::Invalid | stravia_vendor_sdk::ErrorKind::Unsupported,
            ..
        })
        | Some(stravia_vendor_runtime::RuntimeError::InvalidOutput) => {
            ProviderAllowanceErrorCategory::InvalidResponse
        }
        _ => ProviderAllowanceErrorCategory::UpstreamUnavailable,
    }
}

fn allowance_condition(allowance: &Allowance) -> Option<AllowanceCondition> {
    if allowance
        .used_percent
        .is_some_and(|used| used.is_finite() && used >= 100.0)
        || allowance
            .remaining
            .as_ref()
            .is_some_and(|remaining| remaining.value.is_finite() && remaining.value <= 0.0)
    {
        return Some(AllowanceCondition::Exhausted);
    }
    let remaining_percent = allowance
        .used_percent
        .filter(|used| used.is_finite())
        .map(|used| 100.0 - used)
        .or_else(|| {
            allowance
                .remaining
                .as_ref()
                .zip(allowance.limit.as_ref())
                .filter(|(remaining, limit)| {
                    remaining.value.is_finite() && limit.value.is_finite() && limit.value > 0.0
                })
                .map(|(remaining, limit)| remaining.value / limit.value * 100.0)
        })?;
    Some(if remaining_percent < 20.0 {
        AllowanceCondition::Tight
    } else {
        AllowanceCondition::Normal
    })
}

async fn apply_forecasts(
    snapshot: &mut ProviderAllowanceSnapshot,
    store: &AllowanceSampleStore,
    now: i64,
) -> anyhow::Result<()> {
    for allowance in &mut snapshot.allowances {
        let since = allowance
            .reset_at
            .zip(allowance.window_seconds)
            .map(|(reset_at, window_seconds)| {
                reset_at.saturating_sub(
                    i64::try_from(window_seconds)
                        .unwrap_or(i64::MAX)
                        .saturating_mul(1000),
                )
            })
            .unwrap_or_else(|| now.saturating_sub(SAMPLE_RETENTION_MILLIS));
        let samples = store
            .list_for_item(&snapshot.provider_id, &allowance.key, since)
            .await?;
        allowance.forecast = forecast_allowance(allowance, &samples);
    }
    Ok(())
}

fn forecast_allowance(allowance: &Allowance, samples: &[AllowanceSample]) -> ExhaustionForecast {
    if let Some(reset_at) = allowance.reset_at {
        let Some(window_seconds) = allowance.window_seconds else {
            return ExhaustionForecast::default();
        };
        let window_start = reset_at.saturating_sub(
            i64::try_from(window_seconds)
                .unwrap_or(i64::MAX)
                .saturating_mul(1000),
        );
        let points = samples
            .iter()
            .filter(|sample| sample.sampled_at >= window_start)
            .filter_map(|sample| {
                sample_remaining_percent(sample).map(|remaining| (sample.sampled_at, remaining))
            })
            .collect::<Vec<_>>();
        let Some(line) = LinearTrend::from_points(&points) else {
            return ExhaustionForecast::default();
        };
        let projected = line.value_at(reset_at);
        if line.slope < 0.0 && projected <= 0.0 {
            return ExhaustionForecast {
                status: ExhaustionForecastStatus::WillExhaust,
                projected_remaining_percent: Some(0.0),
                exhausts_at: line.zero_at().map(|value| value.round() as i64),
            };
        }
        return ExhaustionForecast {
            status: ExhaustionForecastStatus::NoRisk,
            projected_remaining_percent: Some(projected.clamp(0.0, 100.0)),
            exhausts_at: None,
        };
    }

    let Some(current_amount) = allowance.remaining.as_ref() else {
        return ExhaustionForecast::default();
    };
    let points = samples
        .iter()
        .filter(|sample| {
            sample.amount_unit.as_deref() == Some(current_amount.unit.as_str())
                && sample.currency.as_deref() == current_amount.currency.as_deref()
        })
        .filter_map(|sample| {
            sample
                .remaining_value
                .filter(|value| value.is_finite())
                .map(|remaining| (sample.sampled_at, remaining))
        })
        .collect::<Vec<_>>();
    let Some(line) = LinearTrend::from_points(&points) else {
        return ExhaustionForecast::default();
    };
    if current_amount.value.is_finite() && current_amount.value <= 0.0 {
        return ExhaustionForecast {
            status: ExhaustionForecastStatus::WillExhaust,
            projected_remaining_percent: None,
            exhausts_at: line
                .zero_at()
                .map(|value| value.round() as i64)
                .or_else(|| {
                    points
                        .iter()
                        .filter_map(|(sampled_at, remaining)| {
                            (*remaining <= 0.0).then_some(*sampled_at)
                        })
                        .min()
                }),
        };
    }
    if line.slope < 0.0
        && let Some(exhausts_at) = line.zero_at().map(|value| value.round() as i64)
    {
        return ExhaustionForecast {
            status: ExhaustionForecastStatus::WillExhaust,
            projected_remaining_percent: None,
            exhausts_at: Some(exhausts_at),
        };
    }
    ExhaustionForecast {
        status: ExhaustionForecastStatus::NoRisk,
        projected_remaining_percent: None,
        exhausts_at: None,
    }
}

fn sample_remaining_percent(sample: &AllowanceSample) -> Option<f64> {
    sample
        .used_percent
        .filter(|value| value.is_finite())
        .map(|used| 100.0 - used)
        .or_else(|| {
            sample
                .remaining_value
                .zip(sample.limit_value)
                .filter(|(remaining, limit)| {
                    remaining.is_finite() && limit.is_finite() && *limit > 0.0
                })
                .map(|(remaining, limit)| remaining / limit * 100.0)
        })
}

struct LinearTrend {
    origin: f64,
    intercept: f64,
    slope: f64,
}

impl LinearTrend {
    fn from_points(points: &[(i64, f64)]) -> Option<Self> {
        if points.len() < 2 {
            return None;
        }
        let first = points.iter().map(|(time, _)| *time).min()?;
        let last = points.iter().map(|(time, _)| *time).max()?;
        if last.saturating_sub(first) < MIN_FORECAST_SPAN_MILLIS {
            return None;
        }
        let origin = first as f64;
        let count = points.len() as f64;
        let mean_x = points
            .iter()
            .map(|(time, _)| *time as f64 - origin)
            .sum::<f64>()
            / count;
        let mean_y = points.iter().map(|(_, value)| *value).sum::<f64>() / count;
        let (numerator, denominator) =
            points
                .iter()
                .fold((0.0, 0.0), |(numerator, denominator), (time, value)| {
                    let centered_x = (*time as f64 - origin) - mean_x;
                    (
                        numerator + centered_x * (*value - mean_y),
                        denominator + centered_x * centered_x,
                    )
                });
        if denominator <= f64::EPSILON {
            return None;
        }
        let slope = numerator / denominator;
        Some(Self {
            origin,
            intercept: mean_y - slope * mean_x,
            slope,
        })
    }

    fn value_at(&self, timestamp: i64) -> f64 {
        self.intercept + self.slope * (timestamp as f64 - self.origin)
    }

    fn zero_at(&self) -> Option<f64> {
        (self.slope < 0.0).then_some(self.origin - self.intercept / self.slope)
    }
}

fn safe_error(category: ProviderAllowanceErrorCategory) -> ProviderAllowanceError {
    let message = match category {
        ProviderAllowanceErrorCategory::Authentication => {
            "Authentication failed. Reconnect this provider or update its credential."
        }
        ProviderAllowanceErrorCategory::RateLimited => {
            "The allowance service is rate limited. Try again later."
        }
        ProviderAllowanceErrorCategory::Timeout => {
            "The allowance service timed out. Try again later."
        }
        ProviderAllowanceErrorCategory::UpstreamUnavailable => {
            "The allowance service is unavailable. Try again later."
        }
        ProviderAllowanceErrorCategory::InvalidResponse => {
            "The allowance service returned an unsupported response."
        }
    };
    ProviderAllowanceError {
        category,
        message: message.into(),
    }
}

fn error_snapshot(provider: &Provider, error: ProviderAllowanceError) -> ProviderAllowanceSnapshot {
    ProviderAllowanceSnapshot {
        provider_id: provider.id.clone(),
        provider_name: provider.name.clone(),
        catalog_provider_id: provider.preset_key.clone().unwrap_or_default(),
        channel: provider.channel.clone().unwrap_or_else(|| "default".into()),
        plan_label: None,
        status: ProviderAllowanceStatus::Error,
        fetched_at: None,
        allowances: Vec::new(),
        models: Vec::new(),
        error: Some(error),
    }
}

fn stale_or_error_snapshot(
    provider: &Provider,
    previous: Option<&ProviderAllowanceSnapshot>,
    error: ProviderAllowanceError,
) -> ProviderAllowanceSnapshot {
    let Some(previous) = previous.filter(|snapshot| snapshot.fetched_at.is_some()) else {
        return error_snapshot(provider, error);
    };
    ProviderAllowanceSnapshot {
        provider_id: provider.id.clone(),
        provider_name: provider.name.clone(),
        catalog_provider_id: provider.preset_key.clone().unwrap_or_default(),
        channel: provider.channel.clone().unwrap_or_else(|| "default".into()),
        plan_label: previous.plan_label.clone(),
        status: ProviderAllowanceStatus::Stale,
        fetched_at: previous.fetched_at.clone(),
        allowances: previous.allowances.clone(),
        models: previous.models.clone(),
        error: Some(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sdk_amount(value: &str, currency: Option<&str>) -> stravia_vendor_sdk::AllowanceAmount {
        stravia_vendor_sdk::AllowanceAmount {
            value: value.into(),
            unit: "currency".into(),
            currency: currency.map(str::to_owned),
        }
    }

    fn sdk_item(currency: &str) -> stravia_vendor_sdk::AllowanceItem {
        stravia_vendor_sdk::AllowanceItem {
            key: format!("balance_{}", currency.to_ascii_lowercase()),
            label: "Balance".into(),
            kind: "balance".into(),
            used: None,
            remaining: Some(sdk_amount("12.25", Some(currency))),
            limit: None,
            used_percent: None,
            window_seconds: None,
            resets_at_unix_ms: None,
            condition: Some("unknown".into()),
        }
    }

    #[test]
    fn sdk_allowances_keep_currency_and_unknown_state_distinct() {
        let cny = map_allowance(sdk_item("CNY")).expect("CNY allowance");
        let usd = map_allowance(sdk_item("USD")).expect("USD allowance");
        assert_eq!(
            cny.remaining
                .as_ref()
                .and_then(|amount| amount.currency.as_deref()),
            Some("CNY")
        );
        assert_eq!(
            usd.remaining
                .as_ref()
                .and_then(|amount| amount.currency.as_deref()),
            Some("USD")
        );
        assert_eq!(cny.condition, None);
        assert_eq!(usd.condition, None);
    }

    #[test]
    fn one_allowance_cannot_merge_different_currencies() {
        let mut item = sdk_item("USD");
        item.used = Some(sdk_amount("1", Some("CNY")));
        assert!(map_allowance(item).is_err());
    }
}
