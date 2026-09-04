use super::*;

#[derive(Clone)]
pub(super) struct GenerationChainStore {
    turn_chain: Arc<dyn TurnChainStore>,
    ttl: Duration,
    materializations: Arc<Mutex<GenerationMaterializationCache>>,
}

#[derive(Clone)]
pub(super) struct MaterializedGeneration {
    pub(super) effective_items: Vec<AiItem>,
    pub(super) client_items: Vec<AiItem>,
    pub(super) effective_request: Option<EffectiveRequestConfig>,
    pub(super) effective_system: Option<String>,
    pub(super) upstream_response_id: Option<String>,
    pub(super) effective_state: GenerationChainState,
    pub(super) media_turn_messages: Vec<(usize, Vec<String>)>,
    pub(super) client_history: Option<ClientHistoryState>,
    pub(super) payload_version: u32,
    pub(super) expires_at: std::time::Instant,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct GenerationMaterializationCacheKey {
    principal: String,
    node_id: TurnNodeId,
    payload_version: u32,
}

struct CachedMaterialization {
    materialized: MaterializedGeneration,
    bytes: usize,
    expires_at: std::time::Instant,
}

#[derive(Default)]
struct GenerationMaterializationCache {
    bytes: usize,
    entries: HashMap<GenerationMaterializationCacheKey, CachedMaterialization>,
    head_versions: HashMap<(String, TurnNodeId), u32>,
    lru: VecDeque<GenerationMaterializationCacheKey>,
}

#[derive(Clone)]
pub(super) struct GenerationChainCommit {
    pub(crate) principal: Principal,
    pub(crate) id: String,
    pub(crate) parent: ActiveGenerationChain,
    pub(crate) request_delta: AiRequest,
    pub(crate) effective_request: Option<AiRequest>,
    pub(crate) response: AiResponse,
    pub(crate) upstream_response_id: Option<String>,
    pub(crate) effective_state: GenerationChainState,
}

pub(crate) fn request_has_item_references(request: &AiRequest) -> bool {
    item_reference_ids(&request.items).next().is_some()
}

pub(super) fn item_reference_ids(items: &[AiItem]) -> impl Iterator<Item = &str> {
    items.iter().filter_map(item_reference_id)
}
async fn read_response_artifact_image(
    store: &dyn crate::agent::ArtifactStore,
    principal: &Principal,
    artifact_id: &crate::agent::ArtifactId,
) -> Result<(bytes::Bytes, String), ()> {
    const MAX_RESPONSE_ARTIFACT_IMAGE_BYTES: u64 = 20 * 1024 * 1024;

    let reader = store.open(principal, artifact_id).await.map_err(|_| ())?;
    if reader.artifact.size == 0 || reader.artifact.size > MAX_RESPONSE_ARTIFACT_IMAGE_BYTES {
        return Err(());
    }
    let media_type = reader.artifact.mime_type;
    let crate::agent::ArtifactSource::LocalPath(path) = reader.source else {
        return Err(());
    };
    let file = tokio::fs::File::open(path).await.map_err(|_| ())?;
    let mut bytes = Vec::with_capacity(usize::try_from(reader.artifact.size).unwrap_or(0));
    file.take(MAX_RESPONSE_ARTIFACT_IMAGE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| ())?;
    if bytes.len() as u64 > MAX_RESPONSE_ARTIFACT_IMAGE_BYTES
        || bytes.len() as u64 != reader.artifact.size
    {
        return Err(());
    }
    Ok((bytes::Bytes::from(bytes), media_type))
}

pub(crate) async fn hydrate_response_artifact_references(
    principal: &Principal,
    request: &mut AiRequest,
    artifacts: Option<&dyn crate::agent::ArtifactStore>,
) -> Result<(), String> {
    for message in &mut request.items {
        let MessageContent::Blocks(blocks) = &mut message.content else {
            continue;
        };
        let mut hydrated_artifacts = Vec::new();
        for (block_index, block) in blocks.iter_mut().enumerate() {
            let artifact = match block {
                ContentBlock::Image {
                    source: MediaSource::FileId { file_id, .. },
                    detail,
                    ..
                } => file_id
                    .strip_prefix("stravia-artifact:")
                    .map(|artifact_id| (artifact_id.to_owned(), detail.clone())),
                _ => None,
            };
            let Some((artifact_id, detail)) = artifact else {
                continue;
            };
            let store = artifacts.ok_or_else(|| "item_reference_not_found".to_string())?;
            let (bytes, media_type) = read_response_artifact_image(
                store,
                principal,
                &crate::agent::ArtifactId::new(artifact_id.clone()),
            )
            .await
            .map_err(|_| "item_reference_not_found".to_string())?;
            *block = ContentBlock::Image {
                source: MediaSource::Base64 {
                    media_type,
                    data: base64::engine::general_purpose::STANDARD.encode(bytes),
                },
                detail,
                cache_control: None,
            };
            hydrated_artifacts.push(serde_json::json!({
                "block_index": block_index,
                "artifact_id": artifact_id,
            }));
        }
        if !hydrated_artifacts.is_empty() {
            let mut meta = message
                .meta
                .take()
                .map(|value| match value {
                    serde_json::Value::Object(object) => object,
                    other => serde_json::Map::from_iter([("vendor_meta".into(), other)]),
                })
                .unwrap_or_default();
            meta.insert(
                "__stravia_artifact_references".into(),
                serde_json::Value::Array(hydrated_artifacts),
            );
            message.meta = Some(serde_json::Value::Object(meta));
        }
    }
    Ok(())
}

pub(crate) fn request_preserves_upstream_response(request: &AiRequest) -> bool {
    match request.ext.as_ref() {
        Some(crate::protocol::ir::ProtocolExt::OpenResponses(extension)) => {
            extension.store.unwrap_or(true)
        }
        _ => true,
    }
}

impl GenerationChainStore {
    pub fn from_turn_chain(turn_chain: Arc<dyn TurnChainStore>, ttl: Duration) -> Self {
        Self {
            turn_chain,
            ttl,
            materializations: Arc::new(Mutex::new(GenerationMaterializationCache::default())),
        }
    }

    pub fn allocate_id(&self) -> String {
        TurnNodeId::response().to_string()
    }

    pub async fn resolve_available_item_references(
        &self,
        principal: &Principal,
        items: &mut [AiItem],
        ingress: ProtocolId,
    ) -> Result<(), String> {
        let response_ids = item_reference_node_ids(ingress, items);
        let mut persisted = Vec::new();
        for response_id in response_ids {
            let Ok(nodes) = self
                .turn_chain
                .materialize(
                    principal,
                    TurnNodeKind::Response,
                    &TurnNodeId::new(response_id),
                )
                .await
            else {
                continue;
            };
            persisted.extend(
                nodes
                    .into_iter()
                    .map(|node| {
                        serde_json::from_value::<PersistedResponseNode>(node.payload)
                            .map(|payload| (node.id, payload))
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| "item_reference_not_found".to_string())?,
            );
        }
        resolve_item_references(items, &persisted, Some(ingress))
    }

    pub async fn materialize_parent(
        &self,
        principal: &Principal,
        request: &mut AiRequest,
    ) -> Result<ActiveGenerationChain, String> {
        let not_found = if request_has_item_references(request) {
            "item_reference_not_found"
        } else {
            "previous_response_not_found"
        };
        let previous_id = match request.ext.as_ref() {
            Some(ProtocolExt::OpenResponses(extension)) => extension.previous_response_id.clone(),
            _ => None,
        };
        let Some(previous_id) = previous_id else {
            if !request_has_item_references(request) {
                return Ok(ActiveGenerationChain::default());
            }
            let ingress = ProtocolTransform::inferred_ingress(request)
                .ok_or_else(|| not_found.to_string())?;
            let response_ids = item_reference_node_ids(ingress, &request.items);
            let mut persisted = Vec::new();
            for response_id in response_ids {
                let nodes = self
                    .turn_chain
                    .materialize(
                        principal,
                        TurnNodeKind::Response,
                        &TurnNodeId::new(response_id),
                    )
                    .await
                    .map_err(|_| not_found.to_string())?;
                persisted.extend(
                    nodes
                        .into_iter()
                        .map(|node| {
                            serde_json::from_value::<PersistedResponseNode>(node.payload)
                                .map(|payload| (node.id, payload))
                        })
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|_| not_found.to_string())?,
                );
            }
            resolve_item_references(&mut request.items, &persisted, Some(ingress))?;
            return Ok(ActiveGenerationChain::default());
        };
        self.materialize_parent_id(principal, &previous_id, request)
            .await
    }

    pub async fn discover_parent(
        &self,
        principal: &Principal,
        request: &mut AiRequest,
    ) -> Result<Option<DiscoveredGenerationPrefix>, String> {
        let client_request = canonical_client_history_request(request);
        let leading_control_items = request.items.len() - client_request.items.len();
        if client_request.items.len() < 2 {
            return Ok(None);
        }
        let state = ClientHistoryState::from_request(&client_request, &client_request.items);
        let mut context_fingerprints = Vec::with_capacity(client_request.items.len() - 1);
        let mut context = crate::protocol::ir::canonical::history_context_hash(&[]);
        let mut semantic_units = 0usize;
        for item in &client_request.items[..client_request.items.len() - 1] {
            context = crate::protocol::ir::canonical::append_history_context_hash(&context, item);
            semantic_units +=
                crate::protocol::ir::canonical::history_unit_count(std::slice::from_ref(item));
            context_fingerprints.push((
                crate::protocol::ir::canonical::hash_hex(&context),
                u32::try_from(semantic_units).unwrap_or(u32::MAX),
            ));
        }

        let mut candidates = Vec::new();
        if let Some(session_fingerprint) = state.session_fingerprint.as_ref() {
            let fingerprints = context_fingerprints
                .iter()
                .map(|(_, item_count)| (session_fingerprint.clone(), *item_count))
                .collect();
            candidates.extend(
                self.turn_chain
                    .find_reusable_prefixes(
                        principal,
                        TurnNodeKind::Response,
                        &ReusablePrefixQuery {
                            namespace: state.reusable_namespace(),
                            fingerprints,
                        },
                    )
                    .await
                    .map_err(|error| error.to_string())?,
            );
        }

        candidates.extend(
            self.turn_chain
                .find_reusable_prefixes(
                    principal,
                    TurnNodeKind::Response,
                    &ReusablePrefixQuery {
                        namespace: state.controls_fingerprint.clone(),
                        fingerprints: context_fingerprints,
                    },
                )
                .await
                .map_err(|error| error.to_string())?,
        );
        for candidate in candidates {
            let matched_units = usize::try_from(candidate.item_count).unwrap_or(usize::MAX);
            let Some(matched_items) =
                history_prefix_item_count(&client_request.items, matched_units)
            else {
                continue;
            };
            if matched_items >= client_request.items.len() {
                continue;
            }
            let materialized = self
                .materialize_generation(principal, &candidate.node_id)
                .await?;
            let history_matches =
                crate::protocol::ir::canonical::history_unit_count(&materialized.client_items)
                    == matched_units
                    && items_equal(
                        &materialized.client_items,
                        &client_request.items[..matched_items],
                    )
                    && materialized.client_history.as_ref().is_some_and(|history| {
                        history.controls_fingerprint == state.controls_fingerprint
                    });
            if !history_matches {
                continue;
            }
            let mut delta = request.clone();
            let matched_request_items = leading_control_items + matched_items;
            delta.items = request.items[matched_request_items..].to_vec();
            remap_client_tool_result_ids(
                &mut delta.items,
                &materialized.client_items,
                &materialized.effective_items,
            );
            delta.meta.vendor.ingress.insert(
                VERIFIED_HISTORY_REPLAY_META.into(),
                serde_json::Value::Bool(true),
            );
            let active = self
                .materialize_parent_id(principal, candidate.node_id.as_str(), &mut delta)
                .await?;
            *request = delta;
            return Ok(Some(DiscoveredGenerationPrefix {
                active,
                matched_items: matched_request_items,
            }));
        }
        Ok(None)
    }

    async fn materialize_parent_id(
        &self,
        principal: &Principal,
        parent_id: &str,
        request: &mut AiRequest,
    ) -> Result<ActiveGenerationChain, String> {
        let not_found = if request_has_item_references(request) {
            "item_reference_not_found"
        } else {
            "previous_response_not_found"
        };
        let materialized = self
            .materialize_generation(principal, &TurnNodeId::new(parent_id))
            .await
            .map_err(|_| not_found.to_string())?;
        let mut new_messages = std::mem::take(&mut request.items);
        let nodes = self
            .turn_chain
            .materialize(
                principal,
                TurnNodeKind::Response,
                &TurnNodeId::new(parent_id),
            )
            .await
            .map_err(|_| not_found.to_string())?;
        let persisted = nodes
            .into_iter()
            .map(|node| {
                serde_json::from_value::<PersistedResponseNode>(node.payload)
                    .map(|payload| (node.id, payload))
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| not_found.to_string())?;
        let root_id = persisted.first().map(|(id, _)| id.to_string());
        resolve_item_references(
            &mut new_messages,
            &persisted,
            ProtocolTransform::inferred_ingress(request),
        )?;
        if let Some(config) = materialized.effective_request.clone() {
            config.apply_missing_to(request);
        }
        request.items = materialized.effective_items.clone();
        request.items.extend(new_messages);
        let instructions_present = matches!(
            request.ext.as_ref(),
            Some(ProtocolExt::OpenResponses(extension)) if extension.instructions_present
        );
        if request.instructions.is_none() && !instructions_present {
            request.instructions = materialized.effective_system.clone();
        }
        if let Some(ProtocolExt::OpenResponses(extension)) = request.ext.as_mut() {
            extension.previous_response_id = None;
            request.meta.vendor.ingress.remove("previous_response_id");
        }
        Ok(ActiveGenerationChain {
            root_id,
            parent_id: Some(parent_id.to_owned()),
            parent_upstream_response_id: materialized.upstream_response_id,
            parent_state: Some(materialized.effective_state),
            media_turn_messages: materialized.media_turn_messages,
            parent_effective_items: materialized.effective_items,
            parent_client_items: materialized.client_items,
            replace_effective_history: false,
        })
    }

    pub(super) async fn materialize_generation(
        &self,
        principal: &Principal,
        id: &TurnNodeId,
    ) -> Result<MaterializedGeneration, String> {
        let principal_key = principal.continuation_key();
        if let Some(materialized) = self.materialization_cache_get(&principal_key, id) {
            return Ok(materialized);
        }
        let chain = self
            .turn_chain
            .materialize_with_expiry(principal, TurnNodeKind::Response, id)
            .await
            .map_err(|error| error.to_string())?;
        let materialized = materialize_generation_nodes(chain.nodes, chain.expires_at)?;
        self.materialization_cache_insert(principal_key, id.clone(), materialized.clone());
        Ok(materialized)
    }

    fn materialization_cache_get(
        &self,
        principal: &str,
        id: &TurnNodeId,
    ) -> Option<MaterializedGeneration> {
        let mut cache = self
            .materializations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let version = *cache
            .head_versions
            .get(&(principal.to_owned(), id.clone()))?;
        let key = GenerationMaterializationCacheKey {
            principal: principal.to_owned(),
            node_id: id.clone(),
            payload_version: version,
        };
        let expired = cache
            .entries
            .get(&key)
            .is_none_or(|entry| entry.expires_at <= std::time::Instant::now());
        if expired {
            if let Some(entry) = cache.entries.remove(&key) {
                cache.bytes = cache.bytes.saturating_sub(entry.bytes);
            }
            cache.lru.retain(|candidate| candidate != &key);
            cache
                .head_versions
                .remove(&(principal.to_owned(), id.clone()));
            return None;
        }
        let materialized = cache.entries.get(&key)?.materialized.clone();
        cache.lru.retain(|candidate| candidate != &key);
        cache.lru.push_back(key);
        Some(materialized)
    }

    fn materialization_cache_insert(
        &self,
        principal: String,
        id: TurnNodeId,
        materialized: MaterializedGeneration,
    ) {
        let bytes = materialization_size_bytes(&materialized);
        if bytes > GENERATION_MATERIALIZATION_CACHE_BYTES {
            return;
        }
        let key = GenerationMaterializationCacheKey {
            principal: principal.clone(),
            node_id: id.clone(),
            payload_version: materialized.payload_version,
        };
        let mut cache = self
            .materializations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(previous_version) = cache
            .head_versions
            .insert((principal, id), materialized.payload_version)
        {
            let previous = GenerationMaterializationCacheKey {
                principal: key.principal.clone(),
                node_id: key.node_id.clone(),
                payload_version: previous_version,
            };
            if let Some(previous) = cache.entries.remove(&previous) {
                cache.bytes = cache.bytes.saturating_sub(previous.bytes);
            }
            cache.lru.retain(|candidate| candidate != &previous);
        }
        while cache.bytes.saturating_add(bytes) > GENERATION_MATERIALIZATION_CACHE_BYTES {
            let Some(evicted) = cache.lru.pop_front() else {
                break;
            };
            if let Some(evicted) = cache.entries.remove(&evicted) {
                cache.bytes = cache.bytes.saturating_sub(evicted.bytes);
            }
            cache
                .head_versions
                .remove(&(evicted.principal, evicted.node_id));
        }
        cache.bytes = cache.bytes.saturating_add(bytes);
        cache.entries.insert(
            key.clone(),
            CachedMaterialization {
                expires_at: materialized.expires_at,
                materialized,
                bytes,
            },
        );
        cache.lru.push_back(key);
    }

    #[cfg(test)]
    pub(super) fn preserves_upstream_response(
        original: &AiResponse,
        candidate: &AiResponse,
    ) -> bool {
        items_equal(&original.items, &candidate.items)
    }

    pub(crate) async fn prepare_target_continuation(
        &self,
        principal: &Principal,
        parent_id: &str,
        request: &mut AiRequest,
        candidate_state: &GenerationChainState,
        allow_ephemeral_response: bool,
    ) -> bool {
        let Ok(materialized) = self
            .materialize_generation(principal, &TurnNodeId::new(parent_id))
            .await
        else {
            return false;
        };
        let active = ActiveGenerationChain {
            parent_id: Some(parent_id.to_owned()),
            parent_upstream_response_id: materialized.upstream_response_id,
            parent_state: Some(materialized.effective_state.clone()),
            ..ActiveGenerationChain::default()
        };
        self.prepare_upstream(&active, request, candidate_state, allow_ephemeral_response)
    }

    pub fn prepare_upstream(
        &self,
        active: &ActiveGenerationChain,
        request: &mut AiRequest,
        candidate_state: &GenerationChainState,
        allow_ephemeral_response: bool,
    ) -> bool {
        if (!request_preserves_upstream_response(request) && !allow_ephemeral_response)
            || !candidate_state.supports_open_responses_continuation()
        {
            return false;
        }
        let (Some(upstream_id), Some(parent_state)) = (
            active.parent_upstream_response_id.as_ref(),
            active.parent_state.as_ref(),
        ) else {
            return false;
        };
        let verified_history_replay = request
            .meta
            .vendor
            .ingress
            .get(VERIFIED_HISTORY_REPLAY_META)
            .and_then(serde_json::Value::as_bool)
            == Some(true);
        let compatible = if verified_history_replay {
            parent_state.same_target(candidate_state)
        } else {
            parent_state.compatible_continuation(candidate_state)
        };
        if !compatible
            || request.items.len() < parent_state.context_messages
            || history_context_fingerprint(&request.items[..parent_state.context_messages])
                != parent_state.context_fingerprint
        {
            return false;
        }
        request.items = request.items.split_off(parent_state.context_messages);
        if let Some(ProtocolExt::OpenResponses(extension)) = request.ext.as_mut() {
            extension.previous_response_id = Some(upstream_id.clone());
        }
        request.meta.vendor.ingress.insert(
            "previous_response_id".into(),
            serde_json::Value::String(upstream_id.clone()),
        );
        true
    }

    #[cfg(test)]
    pub(crate) async fn save(&self, commit: GenerationChainCommit) -> Result<(), TurnCommitError> {
        let GenerationChainCommit {
            principal,
            id,
            parent,
            request_delta,
            response,
            upstream_response_id,
            effective_state,
            ..
        } = commit;
        self.save_with_effective(GenerationChainCommit {
            principal,
            id,
            parent,
            effective_request: None,
            request_delta,
            response,
            upstream_response_id,
            effective_state,
        })
        .await
    }

    pub(crate) async fn save_with_effective(
        &self,
        commit: GenerationChainCommit,
    ) -> Result<(), TurnCommitError> {
        let GenerationChainCommit {
            principal,
            id,
            parent,
            request_delta,
            effective_request,
            response,
            upstream_response_id,
            mut effective_state,
        } = commit;
        let effective_request = effective_request.unwrap_or_else(|| request_delta.clone());
        effective_state.refresh_request_semantics(&effective_request);
        effective_state.append_output(&response);
        let mut client_request_delta = canonical_client_history_request(&request_delta);
        let mut client_items = parent.parent_client_items.clone();
        let parent_items = client_items.len();
        client_items.extend(client_request_delta.items.clone());
        let client_output = project_client_output(
            ProtocolTransform::inferred_ingress(&request_delta),
            &response,
            &mut client_items,
        )?;
        client_request_delta.items = client_items[parent_items..].to_vec();
        client_items.extend(client_output.clone());
        let client_history = ClientHistoryState::from_request(&client_request_delta, &client_items);
        let effective_history_mutation = if parent.replace_effective_history {
            EffectiveHistoryMutation::Replace {
                items: effective_request.items.clone(),
            }
        } else if effective_request
            .items
            .get(..parent.parent_effective_items.len())
            .is_some_and(|prefix| items_equal(prefix, &parent.parent_effective_items))
        {
            EffectiveHistoryMutation::Append {
                items: effective_request.items[parent.parent_effective_items.len()..].to_vec(),
            }
        } else {
            EffectiveHistoryMutation::Replace {
                items: effective_request.items.clone(),
            }
        };
        let item_count = u32::try_from(crate::protocol::ir::canonical::history_unit_count(
            &client_items,
        ))
        .map_err(|error| TurnCommitError::Storage(error.to_string()))?;
        let reusable_prefix = Some(ReusablePrefixMetadata {
            namespace: client_history.reusable_namespace(),
            fingerprint: client_history
                .session_fingerprint
                .clone()
                .unwrap_or_else(|| client_history.context_fingerprint.clone()),
            item_count,
            completed_at: chrono::Utc::now().timestamp_millis(),
        });
        let effective_system = effective_request.instructions.clone();
        let trusted_media_turn_ids = response.trusted_media_turn_ids.clone();
        let payload = serde_json::to_value(PersistedResponseNode {
            client_delta: RequestDelta {
                messages: client_request_delta.items,
                system: client_request_delta.instructions,
            },
            client_output: Some(client_output),
            effective_history_mutation: Some(effective_history_mutation),
            effective_system,
            effective_input: Vec::new(),
            client_history: Some(client_history),
            effective_output: response,
            trusted_media_turn_ids,
            upstream_response_id,
            effective_state,
            effective_request: Some(EffectiveRequestConfig::from_request(&effective_request)),
        })
        .map_err(|error| TurnCommitError::Storage(error.to_string()))?;
        self.turn_chain
            .commit(TurnCommit {
                id: TurnNodeId::new(id),
                kind: TurnNodeKind::Response,
                parent_id: parent.parent_id.map(TurnNodeId::new),
                principal: principal.clone(),
                payload_version: RESPONSE_PAYLOAD_VERSION,
                payload,
                idle_ttl: self.ttl,
                reusable_prefix,
            })
            .await?;
        Ok(())
    }
}
