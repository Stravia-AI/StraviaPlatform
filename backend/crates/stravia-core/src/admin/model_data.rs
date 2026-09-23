use super::*;

pub(super) fn normalize_model_balance(balance: Option<&str>) -> anyhow::Result<String> {
    let input = balance
        .unwrap_or("traffic_equalization")
        .trim()
        .to_ascii_lowercase();
    let normalized = match input.as_str() {
        "weighted" | "priority" | "cooldown" => "traffic_equalization",
        "latency" => "latency_preference",
        value => value,
    };
    normalized
        .parse::<RouteSelectionStrategy>()
        .map(|strategy| strategy.as_str().to_string())
}

pub(super) fn ensure_route_targets_valid(backends: &[CreateTarget]) -> anyhow::Result<()> {
    if backends.is_empty() {
        anyhow::bail!("at least one enabled Target is required");
    }
    if !backends.iter().any(|backend| backend.enabled) {
        anyhow::bail!("at least one enabled Target is required");
    }
    let mut targets = std::collections::BTreeSet::new();
    for backend in backends {
        let provider_id = backend.provider_id.trim();
        let provider_model_id = backend.model.as_deref().map(str::trim);
        if provider_id.is_empty() {
            anyhow::bail!("backend provider_id cannot be empty");
        }
        if provider_model_id.is_some_and(str::is_empty) {
            anyhow::bail!("backend model cannot be empty");
        }
        if !targets.insert((provider_id, provider_model_id)) {
            anyhow::bail!(
                "a Route cannot contain the same Provider and Provider Model more than once"
            );
        }
        if backend
            .first_token_timeout_ms
            .is_some_and(|value| value < 0)
        {
            anyhow::bail!("First Token Timeout must be >= 0");
        }
        if backend.target_retry_budget.is_some_and(|value| value < 0) {
            anyhow::bail!("Target Retry Budget must be >= 0");
        }
        if backend.target_cooldown_ms.is_some_and(|value| value < 0) {
            anyhow::bail!("Target Cooldown must be >= 0");
        }
    }
    Ok(())
}
