use super::*;

fn result_ids(item: &AiItem) -> impl Iterator<Item = &str> {
    let top_level = (item.role == stravia_runtime_contract::protocol::ir::Role::Tool)
        .then_some(item.tool_call_id.as_deref())
        .flatten();
    let blocks = match &item.content {
        MessageContent::Blocks(blocks) => blocks.as_slice(),
        _ => &[],
    };
    top_level
        .into_iter()
        .chain(blocks.iter().filter_map(|block| match block {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        }))
}

impl GenerationChainWrite {
    pub(crate) fn request(&self) -> &AiRequest {
        &self.request
    }

    pub(crate) fn request_mut(&mut self) -> &mut AiRequest {
        &mut self.request
    }

    pub(crate) fn request_delta(&self) -> &AiRequest {
        &self.request_delta
    }

    /// Observation evidence only; never changes history, execution lineage or input.
    pub(crate) fn has_matching_pending_tool_result(&self) -> bool {
        // 历史编辑可使父节点退回早期工具调用；后续 Assistant 之前的结果是历史，
        // 不能把它当成本次工具续接，从而吞掉独立的新用户输入。
        let tail_start = self
            .request_delta
            .items
            .iter()
            .rposition(|item| item.role == stravia_runtime_contract::protocol::ir::Role::Assistant)
            .map_or(0, |index| index + 1);
        let tail = &self.request_delta.items[tail_start..];
        if self.parent.parent_id.is_none()
            || !tail.iter().any(|item| result_ids(item).next().is_some())
        {
            return false;
        }
        // begin retains the original client request_delta; only the execution
        // request is remapped. Never use hook/platform effective tool history here.
        let mut pending = std::collections::HashSet::new();
        for item in &self.parent.parent_client_items {
            if item.role == stravia_runtime_contract::protocol::ir::Role::Assistant {
                for call in item.tool_calls.iter().flatten() {
                    if !call.id.is_empty() {
                        pending.insert(call.id.as_str());
                    }
                }
                if let MessageContent::Blocks(blocks) = &item.content {
                    for block in blocks {
                        if let ContentBlock::ToolUse { id, .. } = block
                            && !id.is_empty()
                        {
                            pending.insert(id.as_str());
                        }
                    }
                }
            }
            for id in result_ids(item) {
                pending.remove(id);
            }
        }
        // Gemini may return a function-name alias instead of the normalized
        // client call ID. Use the execution path's same last-call alias rules.
        let aliases = tool_result_id_mapping(
            &self.parent.parent_client_items,
            &self.parent.parent_client_items,
        );
        tail.iter()
            .flat_map(result_ids)
            .any(|id| pending.contains(aliases.get(id).copied().unwrap_or(id)))
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn root_id(&self) -> &str {
        self.parent.root_id.as_deref().unwrap_or(&self.id)
    }

    pub(crate) fn parent_id(&self) -> Option<&str> {
        self.parent.parent_id.as_deref()
    }

    pub(crate) fn crosses_compaction_boundary(&self) -> bool {
        self.parent.replacement_client_items.is_some()
    }

    pub(crate) fn record_inline_publications(
        &mut self,
        publications: &[crate::model_turn::CompactionPublication],
    ) {
        for publication in publications.iter().filter(|publication| {
            matches!(
                publication.mode,
                crate::interaction_observation::CompactionMode::Inline
            )
        }) {
            self.parent
                .fresh_inline_states
                .push(publication.state.clone());
            self.parent
                .compaction_record_ids
                .push(publication.record_id.clone());
        }
        self.parent.compaction_record_ids.sort();
        self.parent.compaction_record_ids.dedup();
    }

    pub(crate) fn inherited_media_turns(&self) -> &[(usize, Vec<String>)] {
        &self.parent.media_turn_messages
    }

    pub(crate) fn observe_effective(&mut self, request: AiRequest) {
        let marker_references = self
            .request
            .items
            .iter()
            .enumerate()
            .flat_map(|(source_index, item)| {
                crate::history_marker::history_marker_references(std::slice::from_ref(item))
                    .into_iter()
                    .map(move |reference| (source_index, reference))
            })
            .fold(
                Vec::<(usize, String)>::new(),
                |mut references, occurrence| {
                    if !references
                        .iter()
                        .any(|(_, existing)| existing == &occurrence.1)
                    {
                        references.push(occurrence);
                    }
                    references
                },
            );
        // resolve_request_markers has already expanded each marker in atom order;
        // its restored-item sentinels carry the exact reference to anchor here.
        let marker_anchors = history_marker_anchor_indices(&request.items);
        let mut marker_insertions = marker_references
            .into_iter()
            .enumerate()
            .map(|(ordinal, (source_index, reference))| {
                let index = marker_anchors
                    .iter()
                    .find(|(_, anchor_reference)| anchor_reference == &reference)
                    .map(|(anchor, _)| {
                        request.items[..*anchor]
                            .iter()
                            .filter(|item| !history_marker_restored(item))
                            .count()
                    })
                    .unwrap_or(source_index);
                (
                    index,
                    ordinal,
                    AiItem::thinking(
                        crate::history_marker::render_history_marker_reference(&reference),
                        None,
                    ),
                )
            })
            .collect::<Vec<_>>();

        self.request = request;
        self.request
            .items
            .retain(|item| !history_marker_restored(item));
        marker_insertions.sort_by_key(|(index, ordinal, _)| (*index, *ordinal));
        for (offset, (index, _, marker)) in marker_insertions.into_iter().enumerate() {
            self.request.items.insert(
                index.saturating_add(offset).min(self.request.items.len()),
                marker,
            );
        }
    }

    pub(crate) fn stage(
        &mut self,
        response: &mut AiResponse,
        source: &GenerationSource,
        upstream_response_id: Option<String>,
    ) -> bool {
        if !generation_node_is_legal(response)
            || !client_projection_is_valid(
                stravia_protocol_codec::transform::ProtocolTransform::inferred_ingress(
                    &self.request_delta,
                ),
                response,
            )
        {
            return false;
        }
        if let Some(source) = source.thinking_source() {
            source.stamp_response(response);
        }
        attach_persisted_profile(
            response,
            &mut self.request,
            self.parent.parent_id.as_deref(),
        );
        let (effective_state, upstream_response_id) = match source {
            GenerationSource::Target {
                namespace,
                protocol,
                actual_model,
                selected_target_key,
            } => (
                GenerationChainState::from_request(
                    &self.request,
                    namespace,
                    Option::<ProtocolId>::None,
                )
                .with_protocol_identity(protocol.as_ref())
                .with_provider_model(actual_model)
                .with_selected_target_key(selected_target_key),
                upstream_response_id,
            ),
            GenerationSource::Hook { protocol } => (
                GenerationChainState::from_request(&self.request, "hook", Some(*protocol)),
                None,
            ),
        };
        let projected = match project_client_commit(&self.parent, &self.request_delta, response) {
            Ok(projected) => projected,
            Err(error) => {
                tracing::warn!(%error, "failed to project client generation history");
                return false;
            }
        };
        self.commit_fence = Some(self.chain.pending_commits.register(
            self.principal.clone(),
            self.id.clone(),
            projected,
        ));
        self.staged = Some(StagedGeneration {
            response: response.clone(),
            upstream_response_id,
            effective_state,
        });
        true
    }

    #[cfg(test)]
    pub(crate) async fn persist(&mut self) -> Result<(), PersistError> {
        let result = self.persist_holding_fence().await;
        self.resolve_commit_fence();
        result
    }

    pub(crate) async fn persist_holding_fence(&mut self) -> Result<(), PersistError> {
        let result = self.persist_inner().await;
        if result.is_err() {
            self.resolve_commit_fence();
        }
        result
    }

    fn resolve_commit_fence(&mut self) {
        if let Some(fence) = self.commit_fence.take() {
            fence.resolve();
        }
    }

    pub(crate) fn take_commit_fence(&mut self) -> Option<GenerationCommitFence> {
        self.commit_fence.take()
    }

    async fn persist_inner(&mut self) -> Result<(), PersistError> {
        let mut staged = self.staged.clone().ok_or(PersistError::NotStaged)?;
        if let Some(compaction) = &self.chain.compaction {
            compaction
                .extend_retention(&self.principal, &self.parent.compaction_record_ids)
                .await
                .map_err(PersistError::Compaction)?;
        }
        if let Some(store) = &self.chain.redaction_mappings {
            let references = self
                .request
                .meta
                .redaction
                .references()
                .map_err(PersistError::Redaction)?;
            if !references.is_empty() {
                store
                    .extend_retention(&self.principal, &references, self.chain.ttl)
                    .await
                    .map_err(PersistError::Redaction)?;
            }
        }
        if let Some(store) = &self.chain.history_markers {
            let mut parent_references =
                crate::history_marker::history_marker_references(&self.parent.parent_client_items);
            parent_references.sort();
            parent_references.dedup();
            let parent_resolved = store
                .resolve_many(&self.principal, &parent_references)
                .await
                .map_err(PersistError::HistoryMarker)?;
            let mut references = Vec::with_capacity(parent_references.len());
            for (reference, resolved) in parent_references.into_iter().zip(parent_resolved) {
                if resolved.as_ref().is_some_and(|marker| marker.published) {
                    references.push(reference);
                }
            }
            let mut untrusted =
                crate::history_marker::history_marker_references(&self.request_delta.items);
            untrusted.extend(crate::history_marker::history_marker_references(
                &staged.response.items,
            ));
            let untrusted_resolved = store
                .resolve_many(&self.principal, &untrusted)
                .await
                .map_err(PersistError::HistoryMarker)?;
            for (reference, resolved) in untrusted.into_iter().zip(untrusted_resolved) {
                if resolved.as_ref().is_some_and(|marker| marker.published) {
                    references.push(reference);
                }
                if let Some(turn_id) = resolved
                    .as_ref()
                    .and_then(|marker| marker.segment.as_ref())
                    .and_then(media_turn_id)
                    && !staged
                        .response
                        .trusted_media_turn_ids
                        .iter()
                        .any(|existing| existing == turn_id)
                {
                    staged
                        .response
                        .trusted_media_turn_ids
                        .push(turn_id.to_owned());
                }
            }
            references.sort();
            references.dedup();
            store
                .extend_retention(&self.principal, &references, self.chain.ttl)
                .await
                .map_err(PersistError::HistoryMarker)?;
        }
        self.chain
            .store
            .save_with_effective(GenerationChainCommit {
                principal: self.principal.clone(),
                id: self.id.clone(),
                parent: self.parent.clone(),
                request_delta: self.request_delta.clone(),
                effective_request: Some(self.request.clone()),
                response: staged.response,
                upstream_response_id: staged.upstream_response_id,
                effective_state: staged.effective_state,
            })
            .await
            .map_err(PersistError::Store)
    }
}

fn history_marker_restored(item: &AiItem) -> bool {
    item.meta
        .as_ref()
        .and_then(|meta| meta.get("__stravia_history_marker_restored"))
        .and_then(serde_json::Value::as_bool)
        == Some(true)
}

fn history_marker_anchor_indices(items: &[AiItem]) -> Vec<(usize, String)> {
    let mut anchors = Vec::new();
    for (index, item) in items.iter().enumerate() {
        if !history_marker_restored(item) {
            continue;
        }
        let Some(reference) = item
            .meta
            .as_ref()
            .and_then(|meta| meta.get("__stravia_history_marker_reference"))
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        if !anchors.iter().any(|(_, existing)| existing == reference) {
            anchors.push((index, reference.to_owned()));
        }
    }
    anchors
}

fn media_turn_id(segment: &crate::history_marker::HiddenHistorySegment) -> Option<&str> {
    let crate::history_marker::HiddenHistorySegment::Platform {
        result:
            ContentBlock::ToolResult {
                content,
                is_error: Some(false) | None,
                ..
            },
        ..
    } = segment
    else {
        return None;
    };
    content.get("artifacts")?.as_array()?;
    let report = content.get("report")?.as_object()?;
    report.get("answer")?.as_str()?;
    report.get("artifacts")?.as_array()?;
    report.get("limitations")?.as_array()?;
    content.get("completion")?.as_str()?;
    content
        .get("path")?
        .as_str()?
        .strip_prefix("stravia://turns/")
        .filter(|turn_id| stravia_runtime_contract::identifier::valid_id(turn_id))
}

#[cfg(test)]
mod media_turn_tests {
    use super::*;

    fn segment(report: serde_json::Value) -> crate::history_marker::HiddenHistorySegment {
        crate::history_marker::HiddenHistorySegment::Platform {
            call: stravia_runtime_contract::protocol::ir::ToolCall {
                id: "external-call".into(),
                name: "StraviaRead".into(),
                arguments: "{}".into(),
            },
            result: ContentBlock::ToolResult {
                tool_use_id: "external-call".into(),
                content: serde_json::json!({
                    "path": "stravia://turns/aaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "completion": "complete",
                    "artifacts": [],
                    "report": report,
                }),
                content_kind: Some(
                    stravia_runtime_contract::protocol::ir::ToolResultContentKind::Json,
                ),
                is_error: None,
                cache_control: None,
            },
        }
    }

    #[test]
    fn media_turn_identity_uses_report_schema_not_shared_tool_name() {
        let media = segment(serde_json::json!({
            "answer": "description",
            "artifacts": [],
            "limitations": [],
        }));
        assert_eq!(media_turn_id(&media), Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaa"));

        let search = segment(serde_json::json!({
            "answer": "result",
            "sources": [],
        }));
        assert_eq!(media_turn_id(&search), None);
    }
}
