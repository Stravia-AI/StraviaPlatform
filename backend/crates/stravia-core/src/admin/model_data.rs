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

pub(super) fn normalize_create_route_targets(
    input: &CreateRoute,
) -> anyhow::Result<Vec<CreateTarget>> {
    if !input.targets.is_empty() {
        return Ok(input.targets.clone());
    }
    if !input.target_provider.trim().is_empty() && !input.target_model.trim().is_empty() {
        return Ok(vec![CreateTarget {
            provider_id: input.target_provider.clone(),
            model: input.target_model.clone(),
            enabled: true,
            priority: None,
            first_token_timeout_ms: None,
            target_retry_budget: None,
            target_cooldown_ms: None,
            thinking_level_map: Vec::new(),
        }]);
    }
    anyhow::bail!("at least one model backend is required")
}

pub(super) fn normalize_update_route_targets(
    current: &Route,
    input: &UpdateRoute,
) -> anyhow::Result<Vec<CreateTarget>> {
    if let Some(targets) = &input.targets {
        let mapped = targets
            .iter()
            .map(|target| CreateTarget {
                provider_id: target.provider_id.clone(),
                model: target.model.clone(),
                enabled: target.enabled,
                priority: target.priority,
                first_token_timeout_ms: target.first_token_timeout_ms,
                target_retry_budget: target.target_retry_budget,
                target_cooldown_ms: target.target_cooldown_ms,
                thinking_level_map: target.thinking_level_map.clone(),
            })
            .collect();
        return Ok(mapped);
    }

    let provider = input
        .target_provider
        .clone()
        .unwrap_or_else(|| current.target_provider.clone());
    let model = input
        .target_model
        .clone()
        .unwrap_or_else(|| current.target_model.clone());
    if provider.trim().is_empty() || model.trim().is_empty() {
        anyhow::bail!("model backend cannot be empty");
    }
    Ok(vec![CreateTarget {
        provider_id: provider,
        model,
        enabled: true,
        priority: None,
        first_token_timeout_ms: None,
        target_retry_budget: None,
        target_cooldown_ms: None,
        thinking_level_map: Vec::new(),
    }])
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
        let provider_model_id = backend.model.trim();
        if provider_id.is_empty() {
            anyhow::bail!("backend provider_id cannot be empty");
        }
        if provider_model_id.is_empty() {
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
