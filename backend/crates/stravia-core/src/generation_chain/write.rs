use super::*;

impl GenerationChainWrite {
    pub(crate) fn request(&self) -> &AiRequest {
        &self.request
    }

    pub(crate) fn request_mut(&mut self) -> &mut AiRequest {
        &mut self.request
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
        let marker_items = self
            .request
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                let references =
                    crate::history_marker::history_marker_references(std::slice::from_ref(item));
                if references.is_empty() {
                    return None;
                }
                let content = references
                    .iter()
                    .map(|reference| {
                        crate::history_marker::render_history_marker_reference(reference)
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Some((
                    index,
                    AiItem {
                        role: item.role,
                        content: MessageContent::Text(content),
                        tool_calls: None,
                        tool_call_id: None,
                        meta: None,
                    },
                ))
            })
            .collect::<Vec<_>>();
        self.request = request;
        self.request.items.retain(|item| {
            item.meta
                .as_ref()
                .and_then(serde_json::Value::as_object)
                .and_then(|meta| meta.get("__stravia_history_marker_restored"))
                .and_then(serde_json::Value::as_bool)
                != Some(true)
        });
        for (index, marker) in marker_items {
            self.request
                .items
                .insert(index.min(self.request.items.len()), marker);
        }
    }

    pub(crate) fn stage(
        &mut self,
        response: &mut AiResponse,
        upstream_response_id: Option<String>,
    ) -> bool {
        if !generation_node_is_legal(response) {
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
        let staged = self.staged.clone().ok_or(PersistError::NotStaged)?;
        if let Some(store) = &self.chain.history_markers {
            let mut references =
                crate::history_marker::history_marker_references(&self.parent.parent_client_items);
            let mut untrusted =
                crate::history_marker::history_marker_references(&self.request_delta.items);
            untrusted.extend(crate::history_marker::history_marker_references(
                &staged.response.items,
            ));
            for reference in untrusted {
                if store
                    .resolve(&self.principal, &reference)
                    .await
                    .map_err(PersistError::HistoryMarker)?
                    .is_some_and(|marker| marker.published)
                {
                    references.push(reference);
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
