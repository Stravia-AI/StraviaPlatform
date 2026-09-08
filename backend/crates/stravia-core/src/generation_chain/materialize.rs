use super::*;

pub(super) fn decode_response_node(
    node: crate::turn_chain::TurnNode,
) -> Result<(TurnNodeId, PersistedResponseNode), String> {
    if !(LEGACY_RESPONSE_PAYLOAD_VERSION..=RESPONSE_PAYLOAD_VERSION).contains(&node.payload_version)
    {
        return Err("unsupported generation payload version".into());
    }
    let mut persisted = serde_json::from_value::<PersistedResponseNode>(node.payload)
        .map_err(|_| "invalid generation payload".to_string())?;
    if node.payload_version < 5 {
        // Visit typed AiItems only: business JSON can legitimately use the same key.
        let mutation_items = match persisted.effective_history_mutation.as_mut() {
            Some(EffectiveHistoryMutation::Append { items })
            | Some(EffectiveHistoryMutation::Replace { items }) => items.as_mut_slice(),
            None => &mut [],
        };
        for item in persisted
            .client_delta
            .messages
            .iter_mut()
            .chain(persisted.client_output.iter_mut().flatten())
            .chain(persisted.effective_input.iter_mut())
            .chain(persisted.effective_output.items.iter_mut())
            .chain(mutation_items.iter_mut())
        {
            if let Some(meta) = item
                .meta
                .as_mut()
                .and_then(serde_json::Value::as_object_mut)
            {
                meta.remove(crate::protocol::ir::TOOL_RESULT_CONTENT_KIND_META);
            }
        }
    }
    Ok((node.id, persisted))
}

pub(super) fn materialize_generation_nodes(
    nodes: Vec<crate::turn_chain::TurnNode>,
    expires_at: std::time::Instant,
) -> Result<MaterializedGeneration, String> {
    let mut effective_items = Vec::new();
    let mut client_items = Vec::new();
    let mut effective_request = None;
    let mut effective_system = None;
    let mut upstream_response_id = None;
    let mut effective_state = GenerationChainState::default();
    let mut media_turn_messages = Vec::new();
    let mut client_history = None;
    let mut payload_version = 0;
    for node in nodes {
        let node_version = node.payload_version;
        let (_, mut persisted) = decode_response_node(node)?;
        match persisted.client_history_mutation {
            Some(EffectiveHistoryMutation::Append { items }) => client_items.extend(items),
            Some(EffectiveHistoryMutation::Replace { items }) => client_items = items,
            None => client_items.extend(persisted.client_delta.messages.clone()),
        }
        match persisted.effective_history_mutation {
            Some(EffectiveHistoryMutation::Append { items }) => effective_items.extend(items),
            Some(EffectiveHistoryMutation::Replace { items }) => effective_items = items,
            None if node_version == LEGACY_RESPONSE_PAYLOAD_VERSION => {
                effective_items = persisted.effective_input;
            }
            None => effective_items.extend(persisted.client_delta.messages.clone()),
        }
        if !persisted.trusted_media_turn_ids.is_empty() {
            media_turn_messages.push((
                effective_items.len(),
                persisted.trusted_media_turn_ids.clone(),
            ));
        }
        effective_items.extend(persisted.effective_output.items.clone());
        if node_version == LEGACY_RESPONSE_PAYLOAD_VERSION {
            persisted.effective_state.context_fingerprint =
                history_context_fingerprint(&effective_items);
            persisted.effective_state.context_messages = effective_items.len();
        }
        client_items.extend(
            persisted
                .client_output
                .take()
                .unwrap_or_else(|| generic_client_history_output(&persisted.effective_output)),
        );
        client_history = persisted.client_history;
        effective_request = persisted.effective_request;
        effective_system = persisted.effective_system.or(persisted.client_delta.system);
        upstream_response_id = persisted.upstream_response_id;
        effective_state = persisted.effective_state;
        payload_version = node_version;
    }
    if payload_version == 0 {
        return Err("generation chain was empty".into());
    }
    Ok(MaterializedGeneration {
        effective_items,
        client_items,
        effective_request,
        effective_system,
        upstream_response_id,
        effective_state,
        media_turn_messages,
        client_history,
        payload_version,
        expires_at,
    })
}

pub(super) fn materialization_size_bytes(materialized: &MaterializedGeneration) -> usize {
    let items = serde_json::to_vec(&materialized.effective_items)
        .map(|value| value.len())
        .unwrap_or(usize::MAX);
    let client_items = serde_json::to_vec(&materialized.client_items)
        .map(|value| value.len())
        .unwrap_or(usize::MAX);
    let profile = serde_json::to_vec(&materialized.effective_request)
        .map(|value| value.len())
        .unwrap_or(usize::MAX);
    items
        .saturating_add(client_items)
        .saturating_add(profile)
        .saturating_add(std::mem::size_of::<MaterializedGeneration>())
}
