use super::*;

pub(super) fn normalize_model_balance(balance: Option<&str>) -> anyhow::Result<String> {
    balance
        .unwrap_or("weighted")
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
            weight: Some(100),
            priority: Some(1),
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
                weight: target.weight,
                priority: target.priority,
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
        weight: Some(100),
        priority: Some(1),
        thinking_level_map: Vec::new(),
    }])
}

pub(super) fn ensure_route_targets_valid(backends: &[CreateTarget]) -> anyhow::Result<()> {
    if backends.is_empty() {
        anyhow::bail!("at least one model backend is required");
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
        let weight = backend.weight.unwrap_or(100);
        if weight < 0 {
            anyhow::bail!("backend weight must be >= 0");
        }
        let priority = backend.priority.unwrap_or(1);
        if !(1..=2).contains(&priority) {
            anyhow::bail!("backend priority must be 1 or 2");
        }
    }
    Ok(())
}
