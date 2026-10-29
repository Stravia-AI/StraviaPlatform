use super::*;

pub(super) fn decode_response_node(
    node: stravia_runtime_contract::turn_chain::TurnNode,
) -> Result<(TurnNodeId, PersistedResponseNode<'static>), String> {
    if node.payload_version != RESPONSE_PAYLOAD_VERSION {
        return Err("unsupported generation payload version".into());
    }
    let persisted = serde_json::from_value::<PersistedResponseNode<'static>>(node.payload)
        .map_err(|_| "invalid generation payload".to_string())?;
    Ok((node.id, persisted))
}

fn apply_history_input(items: &mut Vec<AiItem>, replace: bool, input: Cow<'_, [AiItem]>) {
    if replace {
        *items = input.into_owned();
    } else {
        match input {
            Cow::Owned(input) => items.extend(input),
            Cow::Borrowed(input) => items.extend_from_slice(input),
        }
    }
}

fn borrowed_mutation<'a>(
    mutation: &'a Option<EffectiveHistoryMutation<'static>>,
) -> Option<(bool, Cow<'a, [AiItem]>)> {
    mutation.as_ref().map(|mutation| match mutation {
        EffectiveHistoryMutation::Append { items } => (false, Cow::Borrowed(items.as_ref())),
        EffectiveHistoryMutation::Replace { items } => (true, Cow::Borrowed(items.as_ref())),
    })
}

fn owned_mutation<'a>(
    mutation: Option<EffectiveHistoryMutation<'static>>,
) -> Option<(bool, Cow<'a, [AiItem]>)> {
    mutation.map(|mutation| match mutation {
        EffectiveHistoryMutation::Append { items } => (false, items),
        EffectiveHistoryMutation::Replace { items } => (true, items),
    })
}

// Owned and borrowed folds select the same persisted client input and output.
fn client_history_parts<'a>(
    persisted: &mut Cow<'a, PersistedResponseNode<'static>>,
) -> (bool, Cow<'a, [AiItem]>, Cow<'a, [AiItem]>) {
    let (mutation, fallback, output) = match persisted {
        Cow::Owned(node) => {
            let mutation = owned_mutation(node.client_history_mutation.take());
            let fallback = Cow::Owned(std::mem::take(&mut node.client_delta.messages).into_owned());
            let output = node
                .client_output
                .take()
                .map(Cow::Owned)
                .unwrap_or_else(|| {
                    Cow::Owned(generic_client_history_output(&node.effective_output))
                });
            (mutation, fallback, output)
        }
        Cow::Borrowed(node) => {
            let node = *node;
            let output = node
                .client_output
                .as_ref()
                .map(|items| Cow::Borrowed(items.as_slice()))
                .unwrap_or_else(|| {
                    Cow::Owned(generic_client_history_output(&node.effective_output))
                });
            (
                borrowed_mutation(&node.client_history_mutation),
                Cow::Borrowed(node.client_delta.messages.as_ref()),
                output,
            )
        }
    };
    let (replace, input) = mutation.unwrap_or((false, fallback));
    (replace, input, output)
}

fn fold_client_history_view(
    client_items: &mut Vec<AiItem>,
    persisted: &mut Cow<'_, PersistedResponseNode<'static>>,
) -> usize {
    let (replace, input, output) = client_history_parts(persisted);
    apply_history_input(client_items, replace, input);
    let input_end = client_items.len();
    apply_history_input(client_items, false, output);
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
    visit_node: impl FnMut(&PersistedResponseNode<'static>),
) -> Result<MaterializedGeneration, String> {
    materialize_generation_replay(
        nodes.into_iter().map(|node| {
            decode_response_node(node).map(|(id, node)| (Cow::Owned(id), Cow::Owned(node)))
        }),
        expires_at,
        visit_node,
    )
}

fn materialize_generation_replay<'a>(
    nodes: impl IntoIterator<
        Item = Result<(Cow<'a, TurnNodeId>, Cow<'a, PersistedResponseNode<'static>>), String>,
    >,
    expires_at: std::time::Instant,
    mut visit_node: impl FnMut(&PersistedResponseNode<'static>),
) -> Result<MaterializedGeneration, String> {
    let mut root_id = None;
    let mut compaction_record_ids = Vec::new();
    let mut effective_items = Vec::new();
    let mut client_items = Vec::new();
    let mut media_turn_messages = Vec::new();
    let mut last = None;
    for node in nodes {
        let (id, mut persisted) = node?;
        if root_id.is_none() {
            root_id = Some(id.to_string());
        }
        visit_node(&persisted);
        let (mutation, fallback) = match &mut persisted {
            Cow::Owned(node) => (
                owned_mutation(node.effective_history_mutation.take()),
                Cow::Borrowed(node.client_delta.messages.as_ref()),
            ),
            Cow::Borrowed(node) => (
                borrowed_mutation(&node.effective_history_mutation),
                Cow::Borrowed(node.client_delta.messages.as_ref()),
            ),
        };
        let (replace, input) = mutation.unwrap_or((false, fallback));
        apply_history_input(&mut effective_items, replace, input);
        match &mut persisted {
            Cow::Owned(node) => {
                compaction_record_ids.append(&mut node.compaction_record_ids);
                if !node.trusted_media_turn_ids.is_empty() {
                    media_turn_messages.push((
                        effective_items.len(),
                        std::mem::take(&mut node.trusted_media_turn_ids),
                    ));
                }
            }
            Cow::Borrowed(node) => {
                compaction_record_ids.extend_from_slice(&node.compaction_record_ids);
                if !node.trusted_media_turn_ids.is_empty() {
                    media_turn_messages
                        .push((effective_items.len(), node.trusted_media_turn_ids.clone()));
                }
            }
        }
        fold_client_history_view(&mut client_items, &mut persisted);
        let output_start = effective_items.len();
        match &mut persisted {
            Cow::Owned(node) => effective_items.append(&mut node.effective_output.items),
            Cow::Borrowed(node) => effective_items.extend_from_slice(&node.effective_output.items),
        }
        crate::model_turn::support::restore_response_history(
            &mut effective_items,
            output_start,
            &id.to_string(),
            persisted
                .effective_output
                .vendor
                .ingress
                .get(stravia_runtime_contract::protocol::ir::vendor_ext::CHAT_REASONING_FIELD_META),
        );
        last = Some(persisted);
    }
    let root_id = root_id.ok_or_else(|| "generation chain was empty".to_string())?;
    let last = last.expect("a nonempty chain has a final node");
    let (
        mut client_history,
        effective_request,
        effective_system,
        upstream_response_id,
        effective_state,
    ) = match last {
        Cow::Owned(node) => (
            node.client_history,
            node.effective_request,
            node.effective_system.or(node.client_delta.system),
            node.upstream_response_id,
            node.effective_state,
        ),
        Cow::Borrowed(node) => (
            node.client_history.clone(),
            node.effective_request.clone(),
            node.effective_system
                .as_ref()
                .or(node.client_delta.system.as_ref())
                .cloned(),
            node.upstream_response_id.clone(),
            node.effective_state.clone(),
        ),
    };
    compaction_record_ids.sort();
    compaction_record_ids.dedup();
    // Retained client history determines local discovery identity, not the
    // upstream proof: that proof may describe a reversible-redaction view.
    let mut client_item_units = 0usize;
    if let Some(history) = client_history.as_mut() {
        let (context, units) =
            stravia_runtime_contract::protocol::ir::canonical::history_context_hash_with_count(
                &client_items,
            );
        client_item_units = units;
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
        payload_version: RESPONSE_PAYLOAD_VERSION,
        expires_at,
    })
}

pub(super) fn materialize_discovery_nodes(
    nodes: &[Arc<DecodedDiscoveryNode>],
    expires_at: std::time::Instant,
) -> Result<MaterializedGeneration, String> {
    materialize_generation_replay(
        nodes
            .iter()
            .map(|node| Ok((Cow::Borrowed(&node.id), Cow::Borrowed(&node.persisted)))),
        expires_at,
        |_| {},
    )
}

#[derive(Clone, Copy)]
struct RebuiltClientContext {
    hash: [u8; 32],
    units: u32,
}

pub(crate) struct PrefixRebuild {
    contexts: HashMap<TurnNodeId, RebuiltClientContext>,
    empty: RebuiltClientContext,
}

impl Default for PrefixRebuild {
    fn default() -> Self {
        Self {
            contexts: HashMap::new(),
            empty: RebuiltClientContext {
                hash: stravia_runtime_contract::protocol::ir::canonical::history_context_hash(&[]),
                units: 0,
            },
        }
    }
}

impl PrefixRebuild {
    pub(crate) fn decode(
        &mut self,
        node: stravia_runtime_contract::turn_chain::TurnNode,
        completed_at: Option<i64>,
    ) -> Result<Option<ReusablePrefixMetadata>, String> {
        let mut context = match node.parent_id.as_ref() {
            Some(parent) => *self
                .contexts
                .get(parent)
                .ok_or_else(|| "generation parent was not rebuilt".to_string())?,
            None => self.empty,
        };
        let (id, mut persisted) = decode_response_node(node)?;
        let (replace, input) = owned_mutation(persisted.client_history_mutation.take())
            .unwrap_or((false, persisted.client_delta.messages));
        if replace {
            context = self.empty;
        }
        let output = persisted
            .client_output
            .take()
            .unwrap_or_else(|| generic_client_history_output(&persisted.effective_output));
        // 只折叠此节点的客户端增量；分支复制固定大小的父摘要，不物化或保留祖先正文。
        for item in input.iter().chain(&output) {
            let (hash, units) =
                stravia_runtime_contract::protocol::ir::canonical::append_history_context_hash_with_count(
                    &context.hash, item,
                );
            context.units = context
                .units
                .checked_add(u32::try_from(units).map_err(|error| error.to_string())?)
                .ok_or_else(|| "generation history exceeds prefix item count".to_string())?;
            context.hash = hash;
        }
        self.contexts.insert(id, context);
        let (Some(mut history), Some(completed_at)) = (persisted.client_history, completed_at)
        else {
            return Ok(None);
        };
        history.context_fingerprint =
            stravia_runtime_contract::protocol::ir::canonical::hash_hex(&context.hash);
        Ok(Some(ReusablePrefixMetadata {
            namespace: history.reusable_namespace(),
            fingerprint: history
                .session_fingerprint
                .clone()
                .unwrap_or_else(|| history.context_fingerprint.clone()),
            item_count: context.units,
            completed_at,
            projection: Some(history.reusable_projection()),
        }))
    }
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
    nodes: &[Arc<DecodedDiscoveryNode>],
    mut visit: impl FnMut(&str, &[Cow<'_, AiItem>], usize),
) {
    fn extend<'a>(view: &mut Vec<Cow<'a, AiItem>>, items: Cow<'a, [AiItem]>) {
        match items {
            Cow::Borrowed(items) => view.extend(items.iter().map(Cow::Borrowed)),
            Cow::Owned(items) => view.extend(items.into_iter().map(Cow::Owned)),
        }
    }

    let mut client_items = Vec::new();
    for node in nodes {
        let mut persisted = Cow::Borrowed(&node.persisted);
        let (replace, input, output) = client_history_parts(&mut persisted);
        if replace {
            client_items.clear();
        }
        extend(&mut client_items, input);
        let input_end = client_items.len();
        extend(&mut client_items, output);
        visit(node.id.as_str(), &client_items, input_end);
    }
}
