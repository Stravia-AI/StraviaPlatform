use super::*;
use crate::memory::json_retained_bytes;
use stravia_runtime_contract::protocol::ir::{
    AudioOutput, CacheParameters, CodeExecutionTool, CompactionControl, Continuation, CustomTool,
    GenerationConfig, ImageGenerationTool, NamespaceMember, NativeTool, ReasoningConfig,
    RequestParameters, ResponseFormat, SessionParameters, ToolChoice, ToolNamespace,
    ToolSearchTool, ToolSpec, UserLocation, WebSearchTool,
};
use stravia_runtime_contract::thinking::TargetThinkingControl;

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

fn apply_history_input(items: &mut Vec<FrozenItem>, replace: bool, input: Cow<'_, [FrozenItem]>) {
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
) -> Option<(bool, Cow<'a, [FrozenItem]>)> {
    mutation.as_ref().map(|mutation| match mutation {
        EffectiveHistoryMutation::Append { items } => (false, items.as_shared()),
        EffectiveHistoryMutation::Replace { items } => (true, items.as_shared()),
    })
}

fn owned_mutation<'a>(
    mutation: Option<EffectiveHistoryMutation<'static>>,
) -> Option<(bool, Cow<'a, [FrozenItem]>)> {
    mutation.map(|mutation| match mutation {
        EffectiveHistoryMutation::Append { items } => (false, items.into_shared()),
        EffectiveHistoryMutation::Replace { items } => (true, items.into_shared()),
    })
}

/// Legacy nodes without a stored client output still need the projected
/// fallback; that cold path materializes one response at the boundary.
fn client_output_fallback(response: &FrozenResponse) -> Vec<FrozenItem> {
    freeze_items(generic_client_history_output(&response.to_owned_response()))
}

// Owned and borrowed folds select the same persisted client input and output.
fn client_history_parts<'a>(
    persisted: &mut Cow<'a, PersistedResponseNode<'static>>,
) -> (bool, Cow<'a, [FrozenItem]>, Cow<'a, [FrozenItem]>) {
    let (mutation, fallback, output) = match persisted {
        Cow::Owned(node) => {
            let mutation = owned_mutation(node.client_history_mutation.take());
            let fallback = Cow::Owned(std::mem::take(&mut node.client_delta.messages).into_owned());
            let output = node
                .client_output
                .take()
                .map(Cow::Owned)
                .unwrap_or_else(|| Cow::Owned(client_output_fallback(&node.effective_output)));
            (mutation, fallback, output)
        }
        Cow::Borrowed(node) => {
            let node = *node;
            let output = node
                .client_output
                .as_ref()
                .map(|items| Cow::Borrowed(items.as_slice()))
                .unwrap_or_else(|| Cow::Owned(client_output_fallback(&node.effective_output)));
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
    client_items: &mut Vec<FrozenItem>,
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
) -> Result<(MaterializedGeneration, Result<Vec<FrozenItem>, String>), String> {
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
            Cow::Owned(node) => effective_items.extend(node.effective_output.take_items()),
            Cow::Borrowed(node) => {
                effective_items.extend(node.effective_output.items().iter().cloned());
            }
        }
        crate::model_turn::support::restore_chat_reasoning_field(
            &mut effective_items,
            output_start,
            persisted
                .effective_output
                .vendor()
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
        let (context, units) = history_context_hash_with_count(&client_items);
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
            .unwrap_or_else(|| (false, std::mem::take(&mut persisted.client_delta.messages)));
        if replace {
            context = self.empty;
        }
        let output = persisted
            .client_output
            .take()
            .unwrap_or_else(|| client_output_fallback(&persisted.effective_output));
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

/// Logical size retained by shared history windows: one handle slot per item
/// plus each distinct item allocation, deduplicated across the windows of the
/// same cache entry. Bodies are shared, so they are counted once per identity
/// instead of once per reference. This is a cache admission estimate, neither
/// a serialized byte count nor an allocator/RSS measurement.
pub(super) fn shared_window_bytes(windows: &[&[FrozenItem]]) -> usize {
    let mut seen = std::collections::HashSet::new();
    let mut bytes = 0usize;
    for window in windows {
        bytes = bytes
            .saturating_add(std::mem::size_of::<Vec<FrozenItem>>())
            .saturating_add(
                window
                    .len()
                    .saturating_mul(std::mem::size_of::<FrozenItem>()),
            );
        for item in *window {
            if seen.insert(item.allocation()) {
                bytes = bytes.saturating_add(item_retained_bytes(item.as_ref()));
            }
        }
    }
    bytes
}

fn item_retained_bytes(item: &AiItem) -> usize {
    let mut bytes = std::mem::size_of::<AiItem>()
        .saturating_add(2 * std::mem::size_of::<usize>())
        .saturating_add(content_retained_bytes(&item.content))
        .saturating_add(item.tool_call_id.as_ref().map_or(0, |id| id.as_str().len()));
    if let Some(calls) = item.tool_calls.as_ref() {
        bytes = bytes
            .saturating_add(std::mem::size_of_val(calls.as_slice()))
            .saturating_add(
                calls
                    .iter()
                    .map(|call| call.id.as_str().len() + call.name.len() + call.arguments.len())
                    .sum::<usize>(),
            );
    }
    if let Some(meta) = item.meta.as_ref() {
        bytes = bytes.saturating_add(metadata_retained_bytes(meta.as_ref()));
    }
    bytes
}

/// Metadata weight includes the JSON actually retained by extensions/opaque
/// values, so a large `reasoning_content` or unknown extension affects cache
/// admission instead of looking like a fixed-size box.
fn metadata_retained_bytes(meta: &stravia_runtime_contract::protocol::ir::AiItemMetadata) -> usize {
    let fixed = std::mem::size_of_val(meta);
    let Some(extensions) = meta.object_extensions() else {
        return fixed.saturating_add(meta.opaque_value().map_or(0, json_retained_bytes));
    };
    let bytes = fixed
        .saturating_add(meta.id().map_or(0, |id| id.as_str().len()))
        .saturating_add(
            meta.reference()
                .map_or(0, |reference| reference.as_str().len()),
        );
    bytes.saturating_add(crate::memory::json_object_heap_bytes(extensions.iter()))
}

fn content_retained_bytes(content: &MessageContent) -> usize {
    match content {
        MessageContent::Text(text) => text.len(),
        MessageContent::Blocks(blocks) => blocks.iter().map(block_retained_bytes).fold(
            std::mem::size_of_val(blocks.as_slice()),
            usize::saturating_add,
        ),
    }
}

fn media_source_retained_bytes(source: &MediaSource) -> usize {
    match source {
        MediaSource::Base64 { media_type, data } => media_type.len() + data.len(),
        MediaSource::Url(url) => url.len(),
        MediaSource::FileId { file_id, detail } => {
            file_id.len() + detail.as_ref().map_or(0, String::len)
        }
    }
}

fn document_source_retained_bytes(
    source: &stravia_runtime_contract::protocol::ir::DocumentSource,
) -> usize {
    use stravia_runtime_contract::protocol::ir::DocumentSource;
    match source {
        DocumentSource::Base64Pdf { data } | DocumentSource::PlainText { data } => data.len(),
        DocumentSource::Url(url) => url.len(),
        DocumentSource::Blocks { content } => content.iter().map(block_retained_bytes).fold(
            std::mem::size_of_val(content.as_slice()),
            usize::saturating_add,
        ),
    }
}

fn block_retained_bytes(block: &ContentBlock) -> usize {
    let bytes = std::mem::size_of::<ContentBlock>();
    match block {
        ContentBlock::Text { text, .. } => bytes.saturating_add(text.len()),
        ContentBlock::Image { source, detail, .. } => bytes
            .saturating_add(media_source_retained_bytes(source))
            .saturating_add(detail.as_ref().map_or(0, String::len)),
        ContentBlock::Audio { source } => bytes.saturating_add(media_source_retained_bytes(source)),
        ContentBlock::File {
            source, media_type, ..
        }
        | ContentBlock::Video {
            source, media_type, ..
        } => bytes
            .saturating_add(media_source_retained_bytes(source))
            .saturating_add(media_type.as_ref().map_or(0, String::len)),
        ContentBlock::Thinking {
            thinking,
            signature,
        } => bytes
            .saturating_add(thinking.len())
            .saturating_add(signature.as_ref().map_or(0, String::len)),
        ContentBlock::Reasoning {
            summary,
            content,
            encrypted_content,
        } => bytes
            .saturating_add(
                summary
                    .iter()
                    .chain(content)
                    .map(String::len)
                    .sum::<usize>(),
            )
            .saturating_add(encrypted_content.as_ref().map_or(0, String::len)),
        ContentBlock::Compaction { encrypted_content } => {
            bytes.saturating_add(encrypted_content.len())
        }
        ContentBlock::CompactionTrigger {} => bytes,
        ContentBlock::RedactedThinking { data } => bytes.saturating_add(data.len()),
        ContentBlock::ToolUse {
            id, name, input, ..
        } => bytes
            .saturating_add(id.as_str().len())
            .saturating_add(name.len())
            .saturating_add(json_retained_bytes(input)),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } => bytes
            .saturating_add(tool_use_id.as_str().len())
            .saturating_add(json_retained_bytes(content)),
        ContentBlock::ServerToolUse {
            id,
            name,
            input,
            server_type,
            ..
        } => bytes
            .saturating_add(id.as_str().len())
            .saturating_add(name.len())
            .saturating_add(server_type.as_ref().map_or(0, String::len))
            .saturating_add(json_retained_bytes(input)),
        ContentBlock::ServerToolResult {
            tool_use_id,
            content,
            server_type,
            ..
        } => bytes
            .saturating_add(tool_use_id.as_str().len())
            .saturating_add(server_type.as_ref().map_or(0, String::len))
            .saturating_add(json_retained_bytes(content)),
        ContentBlock::Document {
            source,
            title,
            context,
            ..
        } => bytes
            .saturating_add(document_source_retained_bytes(source))
            .saturating_add(title.as_ref().map_or(0, String::len))
            .saturating_add(context.as_ref().map_or(0, String::len)),
        ContentBlock::SearchResult {
            content,
            source,
            title,
            ..
        } => bytes
            .saturating_add(content.iter().map(block_retained_bytes).fold(
                std::mem::size_of_val(content.as_slice()),
                usize::saturating_add,
            ))
            .saturating_add(source.len())
            .saturating_add(title.len()),
        ContentBlock::Citation { cited_text, source } => bytes
            .saturating_add(cited_text.len())
            .saturating_add(json_retained_bytes(source)),
        ContentBlock::ExecutableCode { code, language, id } => bytes
            .saturating_add(code.len())
            .saturating_add(language.as_ref().map_or(0, String::len))
            .saturating_add(id.as_ref().map_or(0, String::len)),
        ContentBlock::CodeExecutionResult {
            stdout, stderr, id, ..
        } => bytes
            .saturating_add(stdout.len())
            .saturating_add(stderr.len())
            .saturating_add(id.as_ref().map_or(0, String::len)),
        ContentBlock::ContainerUpload { file_id, .. } => bytes.saturating_add(file_id.len()),
        ContentBlock::Refusal { refusal } => bytes.saturating_add(refusal.len()),
        ContentBlock::Unknown { raw } => bytes.saturating_add(json_retained_bytes(raw)),
    }
}

fn option_string_bytes(value: &Option<String>) -> usize {
    value.as_ref().map_or(0, String::len)
}

pub(super) fn string_vec_bytes(values: &[String]) -> usize {
    values
        .iter()
        .fold(std::mem::size_of_val(values), |bytes, value| {
            bytes.saturating_add(value.len())
        })
}

fn mutation_window<'a>(
    mutation: &'a Option<EffectiveHistoryMutation<'_>>,
) -> (&'a [FrozenItem], usize) {
    match mutation {
        Some(
            EffectiveHistoryMutation::Append { items }
            | EffectiveHistoryMutation::Replace { items },
        ) => match items {
            HistoryItems::Frozen(items) => (items.as_ref(), 0),
            HistoryItems::Borrowed(items) => (
                &[],
                items.iter().fold(0usize, |bytes, item| {
                    bytes.saturating_add(item_retained_bytes(item))
                }),
            ),
        },
        None => (&[], 0),
    }
}

/// Decoded discovery deltas retain response metadata as well as the windows
/// used by the domain fold. Count borrowed fields directly, without serializing
/// every candidate again just to decide whether to keep it in the workspace.
pub(super) fn persisted_node_retained_bytes(node: &PersistedResponseNode<'_>) -> usize {
    let PersistedResponseNode {
        client_delta,
        client_output,
        client_history_mutation,
        compaction_record_ids,
        effective_history_mutation,
        effective_system,
        effective_output,
        client_history,
        trusted_media_turn_ids,
        upstream_response_id,
        effective_state,
        effective_request,
    } = node;
    let (client_mutation, client_borrowed) = mutation_window(client_history_mutation);
    let (effective_mutation, effective_borrowed) = mutation_window(effective_history_mutation);
    std::mem::size_of_val(node)
        .saturating_add(shared_window_bytes(&[
            client_delta.messages.as_ref(),
            client_output.as_deref().unwrap_or_default(),
            client_mutation,
            effective_mutation,
            effective_output.items(),
        ]))
        .saturating_add(client_borrowed)
        .saturating_add(effective_borrowed)
        .saturating_add(option_string_bytes(&client_delta.system))
        .saturating_add(option_string_bytes(effective_system))
        .saturating_add(option_string_bytes(upstream_response_id))
        .saturating_add(string_vec_bytes(compaction_record_ids))
        .saturating_add(string_vec_bytes(trusted_media_turn_ids))
        .saturating_add(
            client_history
                .as_ref()
                .map_or(0, ClientHistoryState::heap_bytes),
        )
        .saturating_add(effective_state.heap_bytes())
        .saturating_add(
            effective_request
                .as_ref()
                .map_or(0, EffectiveRequestConfig::retained_bytes),
        )
        .saturating_add(response_metadata_heap_bytes(&effective_output.response))
}

fn response_metadata_heap_bytes(response: &AiResponse) -> usize {
    use crate::memory::{json_heap_bytes, json_object_heap_bytes};
    use stravia_runtime_contract::protocol::ir::{
        error::AiError,
        response::{EmbeddingData, EmbeddingOutput, EmbeddingVector},
        vendor_ext::VendorExtensions,
    };
    let AiResponse {
        id,
        model,
        items: _,
        stop_reason,
        usage: _,
        embedding_output,
        error,
        vendor,
        trusted_media_turn_ids,
    } = response;
    let VendorExtensions {
        ingress,
        egress,
        passthrough_safe,
    } = vendor;
    let mut bytes = id
        .capacity()
        .saturating_add(model.capacity())
        .saturating_add(option_string_bytes(stop_reason))
        .saturating_add(string_vec_bytes(trusted_media_turn_ids));
    for fields in [ingress, egress, passthrough_safe] {
        bytes = bytes.saturating_add(json_object_heap_bytes(fields.iter()));
    }
    if let Some(AiError {
        kind: _,
        message,
        status_code: _,
        raw,
    }) = error
    {
        bytes = bytes
            .saturating_add(message.capacity())
            .saturating_add(raw.as_ref().map_or(0, json_heap_bytes));
    }
    if let Some(EmbeddingOutput {
        object,
        data,
        extensions,
    }) = embedding_output
    {
        bytes = bytes
            .saturating_add(option_string_bytes(object))
            .saturating_add(
                data.capacity()
                    .saturating_mul(std::mem::size_of::<EmbeddingData>()),
            )
            .saturating_add(json_object_heap_bytes(extensions.iter()));
        for EmbeddingData {
            object,
            index: _,
            embedding,
            extensions,
        } in data
        {
            bytes = bytes
                .saturating_add(option_string_bytes(object))
                .saturating_add(json_object_heap_bytes(extensions.iter()))
                .saturating_add(match embedding {
                    EmbeddingVector::Floats(values) => {
                        values.capacity().saturating_mul(std::mem::size_of::<f64>())
                    }
                    EmbeddingVector::Base64(value) => value.capacity(),
                });
        }
    }
    bytes
}

pub(super) fn tool_specs_retained_bytes(tools: &[ToolSpec]) -> usize {
    tools
        .iter()
        .fold(std::mem::size_of_val(tools), |bytes, tool| {
            bytes.saturating_add(tool_spec_retained_bytes(tool))
        })
}

fn tool_spec_retained_bytes(tool: &ToolSpec) -> usize {
    let ToolSpec {
        name,
        description,
        parameters,
        strict: _,
        defer_loading: _,
    } = tool;
    std::mem::size_of::<ToolSpec>()
        .saturating_add(name.len())
        .saturating_add(option_string_bytes(description))
        .saturating_add(json_retained_bytes(parameters))
}

pub(super) fn tool_choice_retained_bytes(choice: &ToolChoice) -> usize {
    let fixed = std::mem::size_of::<ToolChoice>();
    match choice {
        ToolChoice::Auto | ToolChoice::None | ToolChoice::Required | ToolChoice::Native(_) => fixed,
        ToolChoice::Named { name } => fixed.saturating_add(name.len()),
        ToolChoice::Allowed { names, required: _ } => fixed.saturating_add(string_vec_bytes(names)),
    }
}

pub(super) fn generation_config_retained_bytes(config: &GenerationConfig) -> usize {
    let GenerationConfig {
        temperature: _,
        max_tokens: _,
        top_p: _,
        seed: _,
        stop,
        presence_penalty: _,
        frequency_penalty: _,
    } = config;
    std::mem::size_of::<GenerationConfig>()
        .saturating_add(stop.as_ref().map_or(0, |stop| string_vec_bytes(stop)))
}

pub(super) fn reasoning_config_retained_bytes(config: &ReasoningConfig) -> usize {
    let ReasoningConfig {
        mode,
        budget_tokens: _,
        summary: _,
        level: _,
        target_control,
    } = config;
    std::mem::size_of::<ReasoningConfig>()
        .saturating_add(option_string_bytes(mode))
        .saturating_add(target_control.as_ref().map_or(0, |control| match control {
            TargetThinkingControl::Effort { value } => value.len(),
            TargetThinkingControl::Budget { value: _ }
            | TargetThinkingControl::Enabled
            | TargetThinkingControl::Disabled
            | TargetThinkingControl::Hidden => 0,
        }))
}

pub(super) fn response_format_retained_bytes(format: &ResponseFormat) -> usize {
    let fixed = std::mem::size_of::<ResponseFormat>();
    match format {
        ResponseFormat::Text | ResponseFormat::JsonObject => fixed,
        ResponseFormat::JsonSchema {
            name,
            description,
            schema,
            strict: _,
        } => fixed
            .saturating_add(name.len())
            .saturating_add(option_string_bytes(description))
            .saturating_add(json_retained_bytes(schema)),
    }
}

fn custom_tool_retained_bytes(tool: &CustomTool) -> usize {
    let CustomTool {
        name,
        description,
        defer_loading: _,
        grammar,
    } = tool;
    std::mem::size_of::<CustomTool>()
        .saturating_add(name.len())
        .saturating_add(option_string_bytes(description))
        .saturating_add(grammar.as_ref().map_or(0, |grammar| {
            grammar
                .syntax
                .len()
                .saturating_add(grammar.definition.len())
        }))
}

fn namespace_member_retained_bytes(member: &NamespaceMember) -> usize {
    let fixed = std::mem::size_of::<NamespaceMember>();
    match member {
        NamespaceMember::Function(tool) => fixed.saturating_add(tool_spec_retained_bytes(tool)),
        NamespaceMember::Custom(tool) => fixed.saturating_add(custom_tool_retained_bytes(tool)),
    }
}

fn native_tool_retained_bytes(tool: &NativeTool) -> usize {
    let fixed = std::mem::size_of::<NativeTool>();
    match tool {
        NativeTool::WebSearch(tool) => {
            let WebSearchTool {
                max_uses: _,
                allowed_domains,
                blocked_domains,
                user_location,
                context_size: _,
            } = tool;
            fixed
                .saturating_add(std::mem::size_of::<WebSearchTool>())
                .saturating_add(string_vec_bytes(allowed_domains))
                .saturating_add(string_vec_bytes(blocked_domains))
                .saturating_add(user_location.as_ref().map_or(0, |location| {
                    let UserLocation {
                        country,
                        region,
                        city,
                        timezone,
                    } = location;
                    std::mem::size_of::<UserLocation>()
                        .saturating_add(option_string_bytes(country))
                        .saturating_add(option_string_bytes(region))
                        .saturating_add(option_string_bytes(city))
                        .saturating_add(option_string_bytes(timezone))
                }))
        }
        NativeTool::CodeExecution(tool) => {
            let CodeExecutionTool { container } = tool;
            fixed
                .saturating_add(std::mem::size_of::<CodeExecutionTool>())
                .saturating_add(option_string_bytes(container))
        }
        NativeTool::ImageGeneration(tool) => {
            let ImageGenerationTool {
                action,
                output_format,
                size,
                quality,
            } = tool;
            fixed
                .saturating_add(std::mem::size_of::<ImageGenerationTool>())
                .saturating_add(option_string_bytes(action))
                .saturating_add(option_string_bytes(output_format))
                .saturating_add(option_string_bytes(size))
                .saturating_add(option_string_bytes(quality))
        }
        NativeTool::ToolSearch(tool) => {
            let ToolSearchTool {
                execution,
                description,
                parameters,
            } = tool;
            fixed
                .saturating_add(std::mem::size_of::<ToolSearchTool>())
                .saturating_add(option_string_bytes(execution))
                .saturating_add(option_string_bytes(description))
                .saturating_add(parameters.as_ref().map_or(0, json_retained_bytes))
        }
        NativeTool::Custom(tool) => fixed.saturating_add(custom_tool_retained_bytes(tool)),
        NativeTool::Namespace(tool) => {
            let ToolNamespace {
                name,
                description,
                tools,
            } = tool;
            fixed
                .saturating_add(std::mem::size_of::<ToolNamespace>())
                .saturating_add(name.len())
                .saturating_add(option_string_bytes(description))
                .saturating_add(
                    tools
                        .iter()
                        .fold(std::mem::size_of_val(tools.as_slice()), |bytes, member| {
                            bytes.saturating_add(namespace_member_retained_bytes(member))
                        }),
                )
        }
    }
}

/// `RequestParameters::legacy` is `#[serde(skip)]`, so every cached config was
/// rebuilt from the persisted payload with an empty legacy bag; unknown
/// historical controls are normalized into these public fields by
/// `normalize_parameters`. Counting the public fields is therefore complete.
pub(super) fn request_parameters_retained_bytes(parameters: &RequestParameters) -> usize {
    let mut bytes = std::mem::size_of::<RequestParameters>()
        .saturating_add(option_string_bytes(&parameters.end_user_id));
    if let Some(bias) = parameters.logit_bias.as_ref() {
        bytes = bytes.saturating_add(bias.iter().fold(0usize, |bytes, (key, _)| {
            bytes
                .saturating_add(std::mem::size_of::<(String, f64)>())
                .saturating_add(key.len())
        }));
    }
    if let Some(modalities) = parameters.modalities.as_ref() {
        bytes = bytes.saturating_add(std::mem::size_of_val(modalities.as_slice()));
    }
    if let Some(audio) = parameters.audio.as_ref() {
        let AudioOutput { voice, format } = audio;
        bytes = bytes
            .saturating_add(std::mem::size_of::<AudioOutput>())
            .saturating_add(option_string_bytes(voice))
            .saturating_add(option_string_bytes(format));
    }
    if let Some(compaction) = parameters.compaction.as_ref() {
        let CompactionControl {
            threshold_tokens: _,
        } = compaction;
        bytes = bytes.saturating_add(std::mem::size_of::<CompactionControl>());
    }
    if let Some(metadata) = parameters.metadata.as_ref() {
        bytes = bytes.saturating_add(metadata.iter().fold(0usize, |bytes, (key, value)| {
            bytes
                .saturating_add(2 * std::mem::size_of::<String>())
                .saturating_add(key.len())
                .saturating_add(value.len())
        }));
    }
    if let Some(continuation) = parameters.continuation.as_ref() {
        bytes = bytes.saturating_add(match continuation {
            Continuation::PreviousResponse(id) | Continuation::Conversation(id) => id.len(),
        });
    }
    let CacheParameters { key, retention: _ } = &parameters.cache;
    let SessionParameters {
        id,
        request_id,
        parent_id,
        forked_from_id,
        context_window_id,
        turn_id,
        parent_turn_id,
        root_turn_id,
    } = &parameters.session;
    bytes
        .saturating_add(parameters.native_tools.iter().fold(
            std::mem::size_of_val(parameters.native_tools.as_slice()),
            |bytes, tool| bytes.saturating_add(native_tool_retained_bytes(tool)),
        ))
        .saturating_add(std::mem::size_of::<CacheParameters>())
        .saturating_add(option_string_bytes(key))
        .saturating_add(std::mem::size_of::<SessionParameters>())
        .saturating_add(option_string_bytes(id))
        .saturating_add(option_string_bytes(request_id))
        .saturating_add(option_string_bytes(parent_id))
        .saturating_add(option_string_bytes(forked_from_id))
        .saturating_add(option_string_bytes(context_window_id))
        .saturating_add(option_string_bytes(turn_id))
        .saturating_add(option_string_bytes(parent_turn_id))
        .saturating_add(option_string_bytes(root_turn_id))
        .saturating_add(std::mem::size_of_val(parameters.resets.as_slice()))
}

pub(super) fn visit_client_items_from_nodes(
    nodes: &[Arc<DecodedDiscoveryNode>],
    mut visit: impl FnMut(&str, &[Cow<'_, AiItem>], usize),
) {
    fn extend<'a>(view: &mut Vec<Cow<'a, AiItem>>, items: Cow<'a, [FrozenItem]>) {
        match items {
            Cow::Borrowed(items) => {
                view.extend(items.iter().map(|item| Cow::Borrowed(item.as_ref())))
            }
            Cow::Owned(items) => {
                view.extend(items.into_iter().map(|item| Cow::Owned(item.into_owned())))
            }
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
