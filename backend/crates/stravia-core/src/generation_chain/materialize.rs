use super::*;

pub(super) fn decode_response_node(
    node: stravia_runtime_contract::turn_chain::TurnNode,
) -> Result<(TurnNodeId, PersistedResponseNode), String> {
    if node.payload_version != RESPONSE_PAYLOAD_VERSION {
        return Err("unsupported generation payload version".into());
    }
    let persisted = serde_json::from_value::<PersistedResponseNode>(node.payload)
        .map_err(|_| "invalid generation payload".to_string())?;
    Ok((node.id, persisted))
}

fn fold_client_history(
    client_items: &mut Vec<AiItem>,
    persisted: &mut PersistedResponseNode,
) -> usize {
    match persisted.client_history_mutation.take() {
        Some(EffectiveHistoryMutation::Append { items }) => client_items.extend(items),
        Some(EffectiveHistoryMutation::Replace { items }) => *client_items = items,
        None => client_items.append(&mut persisted.client_delta.messages),
    }
    let input_end = client_items.len();
    client_items.extend(
        persisted
            .client_output
            .take()
            .unwrap_or_else(|| generic_client_history_output(&persisted.effective_output)),
    );
    input_end
}

pub(super) fn materialize_generation_nodes(
    nodes: Vec<stravia_runtime_contract::turn_chain::TurnNode>,
    expires_at: std::time::Instant,
) -> Result<MaterializedGeneration, String> {
    materialize_generation_nodes_inner(nodes, expires_at, |_| {})
}

pub(super) fn materialize_generation_nodes_with_catalog(
    nodes: Vec<stravia_runtime_contract::turn_chain::TurnNode>,
    expires_at: std::time::Instant,
    ingress: Option<ProtocolId>,
) -> Result<(MaterializedGeneration, Result<Vec<AiItem>, String>), String> {
    let mut catalog = Ok(Vec::new());
    // 保留原有错误优先级：完整验证历史后再报告引用目录的协议投影错误。
    // 目录按 (materialized, ingress) 缓存；此处仅负责本次构建，缓存归调用方。
    let materialized = materialize_generation_nodes_inner(nodes, expires_at, |node| {
        if let Ok(items) = catalog.as_mut()
            && let Err(error) = append_history_catalog_node(items, node, ingress)
        {
            catalog = Err(error);
        }
    })?;
    Ok((materialized, catalog))
}

fn materialize_generation_nodes_inner(
    nodes: Vec<stravia_runtime_contract::turn_chain::TurnNode>,
    expires_at: std::time::Instant,
    mut visit_node: impl FnMut(&PersistedResponseNode),
) -> Result<MaterializedGeneration, String> {
    let mut root_id = None;
    let mut compaction_record_ids = Vec::new();
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
        let (id, mut persisted) = decode_response_node(node)?;
        if root_id.is_none() {
            root_id = Some(id.to_string());
        }
        compaction_record_ids.append(&mut persisted.compaction_record_ids);
        visit_node(&persisted);
        match persisted.effective_history_mutation.take() {
            Some(EffectiveHistoryMutation::Append { items }) => effective_items.extend(items),
            Some(EffectiveHistoryMutation::Replace { items }) => effective_items = items,
            None => effective_items.extend_from_slice(&persisted.client_delta.messages),
        }
        if !persisted.trusted_media_turn_ids.is_empty() {
            media_turn_messages.push((
                effective_items.len(),
                std::mem::take(&mut persisted.trusted_media_turn_ids),
            ));
        }
        fold_client_history(&mut client_items, &mut persisted);
        let response = &mut persisted.effective_output;
        crate::model_turn::support::restore_chat_reasoning_field(
            &mut response.items,
            0,
            response
                .vendor
                .ingress
                .get(stravia_runtime_contract::protocol::ir::vendor_ext::CHAT_REASONING_FIELD_META),
        );
        effective_items.append(&mut persisted.effective_output.items);
        client_history = persisted.client_history;
        effective_request = persisted.effective_request;
        effective_system = persisted.effective_system.or(persisted.client_delta.system);
        upstream_response_id = persisted.upstream_response_id;
        effective_state = persisted.effective_state;
        payload_version = node_version;
    }
    let root_id = root_id.ok_or_else(|| "generation chain was empty".to_string())?;
    compaction_record_ids.sort();
    compaction_record_ids.dedup();
    // Client identity is derived from retained client history. Do not replace
    // upstream proofs: they may describe a reversible-redaction Provider view,
    // not these retained items. An old incompatible proof safely declines
    // Target Continuation while automatic parent discovery remains available.
    let mut client_item_units = 0usize;
    if let Some(history) = client_history.as_mut() {
        let mut context =
            stravia_runtime_contract::protocol::ir::canonical::history_context_hash(&[]);
        for item in &client_items {
            let values =
                stravia_runtime_contract::protocol::ir::canonical::history_item_values(item);
            client_item_units += values.len();
            for value in values {
                context =
                    stravia_runtime_contract::protocol::ir::canonical::append_history_value_hash(
                        context, value,
                    );
            }
        }
        history.context_fingerprint =
            stravia_runtime_contract::protocol::ir::canonical::hash_hex(&context);
        history.context_messages = client_items.len();
    }
    Ok(MaterializedGeneration {
        root_id,
        compaction_record_ids,
        effective_items,
        client_items,
        client_item_units,
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

pub(crate) fn rebuilt_prefix(
    nodes: Vec<stravia_runtime_contract::turn_chain::TurnNode>,
    completed_at: i64,
) -> Result<Option<ReusablePrefixMetadata>, String> {
    let materialized = materialize_generation_nodes(nodes, std::time::Instant::now())?;
    let Some(history) = materialized.client_history else {
        return Ok(None);
    };
    Ok(Some(ReusablePrefixMetadata {
        namespace: history.reusable_namespace(),
        fingerprint: history
            .session_fingerprint
            .clone()
            .unwrap_or(history.context_fingerprint.clone()),
        item_count: u32::try_from(materialized.client_item_units)
            .map_err(|error| error.to_string())?,
        completed_at,
    }))
}

pub(super) fn serialized_size_bytes(value: &impl serde::Serialize) -> usize {
    #[derive(Default)]
    struct ByteCounter(usize);

    impl std::io::Write for ByteCounter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut counter = ByteCounter::default();
    match serde_json::to_writer(&mut counter, value) {
        Ok(()) => counter.0,
        Err(_) => usize::MAX,
    }
}

pub(super) fn visit_client_items_from_nodes(
    nodes: Vec<stravia_runtime_contract::turn_chain::TurnNode>,
    mut visit: impl FnMut(&str, &[AiItem], usize),
) -> Result<(), String> {
    let mut decoded = Vec::with_capacity(nodes.len());
    for node in nodes {
        decoded.push(decode_response_node(node)?);
    }

    let mut client_items = Vec::new();
    for (node_id, mut persisted) in decoded {
        let input_end = fold_client_history(&mut client_items, &mut persisted);
        visit(node_id.as_str(), &client_items, input_end);
    }
    Ok(())
}
