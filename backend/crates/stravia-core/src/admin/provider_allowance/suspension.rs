use super::{AllowanceCondition, ProviderAllowanceSnapshot, ProviderAllowanceStatus};
use crate::admin::AdminService;
use crate::db::models::{AllowanceSuspensionState, ProviderCredentialVersion};

pub(super) async fn submit(
    admin: &AdminService,
    snapshot: &ProviderAllowanceSnapshot,
    expected: ProviderCredentialVersion,
    completed_at: i64,
) {
    if let Err(error) = submit_inner(admin, snapshot, expected, completed_at).await {
        tracing::warn!(provider_id = %snapshot.provider_id, error = ?error, "provider allowance suspension write failed");
    }
}

async fn submit_inner(
    admin: &AdminService,
    snapshot: &ProviderAllowanceSnapshot,
    expected: ProviderCredentialVersion,
    completed_at: i64,
) -> anyhow::Result<()> {
    let store = admin.gw.storage.providers();
    let guards = store.guarded_allowance_keys(&snapshot.provider_id).await?;
    if guards
        .iter()
        .any(|key| !snapshot.allowances.iter().any(|item| &item.key == key))
    {
        return Ok(());
    }
    let previous = store.allowance_suspension(&snapshot.provider_id).await?;
    let exhausted = snapshot
        .allowances
        .iter()
        .filter(|item| {
            guards.contains(&item.key) && item.condition == Some(AllowanceCondition::Exhausted)
        })
        .collect::<Vec<_>>();
    let suspended = !exhausted.is_empty();
    let state = AllowanceSuspensionState {
        suspended,
        suspended_at: if suspended {
            previous
                .as_ref()
                .filter(|state| state.suspended)
                .and_then(|state| state.suspended_at.clone())
                .or_else(|| Some(chrono::Utc::now().to_rfc3339()))
        } else {
            None
        },
        triggered_keys: exhausted.iter().map(|item| item.key.clone()).collect(),
        earliest_reset_at: exhausted.iter().filter_map(|item| item.reset_at).min(),
        evidence_completed_at: completed_at,
    };
    if store
        .write_allowance_suspension(&snapshot.provider_id, expected, &state, &guards)
        .await?
        && previous.as_ref().is_some_and(|state| state.suspended) != suspended
    {
        tracing::info!(provider_id = %snapshot.provider_id, suspended, "provider allowance suspension changed");
    }
    Ok(())
}

pub(super) async fn decorate(
    admin: &AdminService,
    snapshot: &mut ProviderAllowanceSnapshot,
) -> anyhow::Result<()> {
    let store = admin.gw.storage.providers();
    let guards = store.guarded_allowance_keys(&snapshot.provider_id).await?;
    snapshot.missing_guarded_keys = if snapshot.status == ProviderAllowanceStatus::Error {
        Vec::new()
    } else {
        guards
            .iter()
            .filter(|key| !snapshot.allowances.iter().any(|item| &item.key == *key))
            .cloned()
            .collect()
    };
    for item in &mut snapshot.allowances {
        item.guarded = guards.contains(&item.key);
    }
    for model in &mut snapshot.models {
        for item in &mut model.allowances {
            item.guarded = false;
        }
    }
    snapshot.suspension = store
        .allowance_suspension(&snapshot.provider_id)
        .await?
        .and_then(|state| state.suspension());
    Ok(())
}
