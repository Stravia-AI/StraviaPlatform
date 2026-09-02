use super::*;

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

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn root_id(&self) -> &str {
        self.parent.root_id.as_deref().unwrap_or(&self.id)
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
        upstream_response_id: Option<String>,
    ) -> bool {
        if !generation_node_is_legal(response)
            || !client_projection_is_valid(
                crate::protocol::transform::ProtocolTransform::inferred_ingress(
                    &self.request_delta,
                ),
                response,
            )
        {
            return false;
        }
        attach_persisted_profile(
            response,
            &mut self.request,
            self.parent.parent_id.as_deref(),
        );
        let target = staged_target(&response);
        response
            .vendor
            .egress
            .remove("__stravia_generation_chain_target");
        let mut effective_state = GenerationChainState::from_request(
            &self.request,
            target
                .as_ref()
                .map_or("", |target| target.namespace.as_str()),
            target
                .as_ref()
                .map_or(crate::protocol::ids::OPEN_RESPONSES_2026_04_24, |target| {
                    target.protocol
                }),
        );
        if let Some(target) = target {
            effective_state = effective_state.with_provider_model(&target.actual_model);
        }
        self.staged = Some(StagedGeneration {
            response: response.clone(),
            upstream_response_id,
            effective_state,
        });
        true
    }

    pub(crate) async fn persist(&mut self) -> Result<(), PersistError> {
        let mut staged = self.staged.clone().ok_or(PersistError::NotStaged)?;
        if let Some(store) = &self.chain.history_markers {
            let mut references =
                crate::history_marker::history_marker_references(&self.parent.parent_client_items);
            let mut untrusted =
                crate::history_marker::history_marker_references(&self.request_delta.items);
            untrusted.extend(crate::history_marker::history_marker_references(
                &staged.response.items,
            ));
            for reference in untrusted {
                let resolved = store
                    .resolve(&self.principal, &reference)
                    .await
                    .map_err(PersistError::HistoryMarker)?;
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
        .and_then(serde_json::Value::as_object)
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
            .and_then(serde_json::Value::as_object)
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
    content.get("report").filter(|report| report.is_object())?;
    content.get("completion")?.as_str()?;
    content
        .get("turn_id")?
        .as_str()
        .filter(|turn_id| turn_id.starts_with("aturn_"))
}

struct StagedTarget {
    namespace: String,
    protocol: ProtocolId,
    actual_model: String,
}

fn staged_target(response: &AiResponse) -> Option<StagedTarget> {
    let target = response
        .vendor
        .egress
        .get("__stravia_generation_chain_target")?
        .as_object()?;
    let protocol = target.get("protocol")?.as_str()?;
    Some(StagedTarget {
        namespace: target.get("namespace")?.as_str()?.to_owned(),
        protocol: crate::protocol::registry::ProtocolRegistry::global().resolve_alias(protocol)?,
        actual_model: target.get("actual_model")?.as_str()?.to_owned(),
    })
}
