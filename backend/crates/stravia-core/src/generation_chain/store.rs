use super::*;

pub(super) const DISCOVERY_WORKSPACE_BYTES: usize = 16 * 1024 * 1024;

pub(super) struct DecodedDiscoveryNode {
    pub(super) id: TurnNodeId,
    pub(super) parent_id: Option<TurnNodeId>,
    pub(super) persisted: PersistedResponseNode<'static>,
}

/// Request-local storage reads and domain decoding share one logical budget.
/// Cached entries are immutable deltas, never accumulated history prefixes.
pub(super) struct DiscoveryWorkspace {
    context: Option<stravia_runtime_contract::turn_chain::TurnReadContext>,
    budget: usize,
}

/// Request-local reuse for preparation and attribution; consumers cannot inspect nodes.
pub(crate) struct GenerationDiscoveryWorkspace(DiscoveryWorkspace);

impl GenerationDiscoveryWorkspace {
    pub(crate) fn new(principal: &Principal) -> Self {
        Self(DiscoveryWorkspace::new(principal, true))
    }

    pub(super) fn workspace(&mut self) -> &mut DiscoveryWorkspace {
        &mut self.0
    }
}

impl DiscoveryWorkspace {
    pub(super) fn new(principal: &Principal, enabled: bool) -> Self {
        Self {
            context: enabled.then(|| {
                stravia_runtime_contract::turn_chain::TurnReadContext::new(
                    principal,
                    TurnNodeKind::Response,
                )
                .with_decoder(Self::decode_shared_node)
            }),
            budget: DISCOVERY_WORKSPACE_BYTES,
        }
    }

    pub(super) fn into_request(self) -> Option<GenerationDiscoveryWorkspace> {
        self.context
            .as_ref()
            .is_some_and(|context| !context.is_empty())
            .then_some(GenerationDiscoveryWorkspace(self))
    }

    fn enable(&mut self, principal: &Principal) {
        if self.context.is_none() {
            self.context = Some(
                stravia_runtime_contract::turn_chain::TurnReadContext::with_budget(
                    principal,
                    TurnNodeKind::Response,
                    self.budget,
                )
                .with_decoder(Self::decode_shared_node),
            );
        }
    }

    fn is_enabled_for(&self, principal: &Principal) -> bool {
        self.budget > 0
            && self
                .context
                .as_ref()
                .is_some_and(|context| context.matches(principal, TurnNodeKind::Response))
    }

    #[cfg(test)]
    pub(super) fn with_budget(principal: &Principal, budget: usize) -> Self {
        Self {
            context: Some(
                stravia_runtime_contract::turn_chain::TurnReadContext::with_budget(
                    principal,
                    TurnNodeKind::Response,
                    budget,
                )
                .with_decoder(Self::decode_shared_node),
            ),
            budget,
        }
    }

    pub(super) fn chain(
        &mut self,
        principal: &Principal,
        id: &TurnNodeId,
        now: std::time::Instant,
    ) -> Option<(Vec<Arc<DecodedDiscoveryNode>>, std::time::Instant)> {
        if !self.is_enabled_for(principal) {
            return None;
        }
        let chain = self.context.as_ref()?.get(id)?;
        if chain.expires_at <= now {
            return None;
        }
        let nodes = chain
            .nodes
            .into_iter()
            .map(|node| node.decoded?.downcast::<DecodedDiscoveryNode>().ok())
            .collect::<Option<Vec<_>>>()?;
        Some((nodes, chain.expires_at))
    }

    #[cfg(test)]
    pub(super) fn decode_chain(
        &mut self,
        _principal: &Principal,
        nodes: Vec<stravia_runtime_contract::turn_chain::TurnNode>,
    ) -> Result<Vec<Arc<DecodedDiscoveryNode>>, String> {
        nodes.into_iter().map(Self::decode_node).collect()
    }

    fn decode_shared_node(
        node: stravia_runtime_contract::turn_chain::TurnNode,
    ) -> Result<Arc<dyn std::any::Any + Send + Sync>, String> {
        Self::decode_node(node).map(|node| node as Arc<dyn std::any::Any + Send + Sync>)
    }

    fn decode_node(
        mut node: stravia_runtime_contract::turn_chain::TurnNode,
    ) -> Result<Arc<DecodedDiscoveryNode>, String> {
        let parent_id = node.parent_id.take();
        let (id, persisted) = decode_response_node(node)?;
        Ok(Arc::new(DecodedDiscoveryNode {
            id,
            parent_id,
            persisted,
        }))
    }

    async fn read(
        &mut self,
        store: &dyn TurnChainStore,
        principal: &Principal,
        id: &TurnNodeId,
    ) -> Result<
        (
            Vec<Arc<DecodedDiscoveryNode>>,
            stravia_runtime_contract::turn_chain::SharedMaterializedTurnChain,
        ),
        TurnUnavailable,
    > {
        use tracing::Instrument as _;
        let context = self.context.as_mut().ok_or(TurnUnavailable::Unavailable)?;
        let span = tracing::info_span!(target: "stravia::perf", "generation_chain.history.load", status = tracing::field::Empty);
        let chain = store
            .materialize_in_context(principal, TurnNodeKind::Response, id, context)
            .instrument(span.clone())
            .await;
        span.record("status", if chain.is_ok() { "completed" } else { "error" });
        let mut chain = chain.inspect_err(|_| {
            context.begin_read();
        })?;
        let mut nodes = Vec::with_capacity(chain.nodes.len());
        for node in &mut chain.nodes {
            let decoded = if let Some(decoded) = node.decoded.as_ref() {
                Arc::clone(decoded)
                    .downcast::<DecodedDiscoveryNode>()
                    .map_err(|_| {
                        context.begin_read();
                        TurnUnavailable::Decode("invalid request history decode".into())
                    })?
            } else {
                let raw = node.raw.take().ok_or_else(|| {
                    context.begin_read();
                    TurnUnavailable::Decode("missing request history payload".into())
                })?;
                Self::decode_node(Arc::try_unwrap(raw).unwrap_or_else(|raw| (*raw).clone()))
                    .map_err(|error| {
                        context.begin_read();
                        TurnUnavailable::Decode(error)
                    })?
            };
            nodes.push(decoded);
        }
        Ok((nodes, chain))
    }

    pub(super) async fn visit_client_items(
        &mut self,
        store: &dyn TurnChainStore,
        principal: &Principal,
        id: &TurnNodeId,
        visit: impl FnMut(&str, &[Cow<'_, AiItem>], usize),
    ) -> anyhow::Result<bool> {
        let (nodes, chain) = match self.read(store, principal, id).await {
            Ok(read) => read,
            Err(TurnUnavailable::Unavailable) => return Ok(false),
            Err(error) => return Err(anyhow::Error::new(error)),
        };
        tracing::Span::current().record("node_count", nodes.len() as u64);
        visit_client_items_from_nodes(&nodes, visit);
        self.retain(principal, &nodes, &chain);
        Ok(true)
    }

    fn fold(
        &mut self,
        principal: &Principal,
        nodes: &[Arc<DecodedDiscoveryNode>],
        chain: &stravia_runtime_contract::turn_chain::SharedMaterializedTurnChain,
    ) -> Result<MaterializedGeneration, String> {
        match materialize_discovery_nodes(nodes, chain.expires_at) {
            Ok(materialized) => {
                self.retain(principal, nodes, chain);
                Ok(materialized)
            }
            Err(error) => {
                if let Some(context) = self.context.as_mut() {
                    context.begin_read();
                }
                Err(error)
            }
        }
    }

    fn retain(
        &mut self,
        principal: &Principal,
        nodes: &[Arc<DecodedDiscoveryNode>],
        chain: &stravia_runtime_contract::turn_chain::SharedMaterializedTurnChain,
    ) {
        if !self.is_enabled_for(principal)
            || !self
                .context
                .as_ref()
                .is_some_and(|context| context.admission_open())
        {
            return;
        }
        let context = self.context.as_ref().expect("enabled request context");
        let decoded = nodes
            .iter()
            .map(|node| {
                let size = if context.retains_node(&node.id) {
                    0
                } else {
                    serialized_size_bytes(&node.persisted)
                        .saturating_add(node.id.as_str().len())
                        .saturating_add(node.parent_id.as_ref().map_or(0, |id| id.as_str().len()))
                        .saturating_add(128)
                };
                (
                    Arc::clone(node) as Arc<dyn std::any::Any + Send + Sync>,
                    size,
                )
            })
            .collect();
        self.context
            .as_mut()
            .expect("enabled request context")
            .admit(chain, decoded);
    }
}

#[derive(Clone)]
pub(super) struct GenerationChainStore {
    pub(super) turn_chain: Arc<dyn TurnChainStore>,
    ttl: Duration,
    runtime_cache: RuntimeCache,
}

#[derive(Serialize, Deserialize)]
pub(super) struct MaterializedGeneration {
    pub(super) root_id: String,
    pub(super) compaction_record_ids: Vec<String>,
    pub(super) effective_items: Vec<AiItem>,
    pub(super) client_items: Vec<AiItem>,
    pub(super) effective_request: Option<EffectiveRequestConfig>,
    pub(super) effective_system: Option<String>,
    pub(super) upstream_response_id: Option<String>,
    pub(super) effective_state: GenerationChainState,
    pub(super) media_turn_messages: Vec<(usize, Vec<String>)>,
    pub(super) client_history: Option<ClientHistoryState>,
    /// `history_unit_count(client_items)`，物化时与 context fingerprint 同遍算出，
    /// 供候选匹配直接引用，避免每次命中前重复投影整段历史。
    pub(super) client_item_units: usize,
    pub(super) payload_version: u32,
    #[serde(with = "cache_deadline")]
    pub(super) expires_at: std::time::Instant,
}

#[derive(Serialize, Deserialize)]
struct CachedCatalog {
    catalog: Result<Vec<AiItem>, String>,
    #[serde(with = "cache_deadline")]
    expires_at: std::time::Instant,
}

#[derive(Serialize)]
struct BorrowedCachedCatalog<'a> {
    catalog: &'a Result<Vec<AiItem>, String>,
    #[serde(with = "cache_deadline")]
    expires_at: std::time::Instant,
}

// Cache namespaces are process-local. Preserve the exact monotonic deadline
// through Redis instead of reconstructing it from a remaining TTL at read time.
mod cache_deadline {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    fn origin() -> Instant {
        static ORIGIN: OnceLock<Instant> = OnceLock::new();
        *ORIGIN.get_or_init(Instant::now)
    }

    pub fn serialize<S: Serializer>(deadline: &Instant, serializer: S) -> Result<S::Ok, S::Error> {
        let origin = origin();
        let before_origin = *deadline < origin;
        let offset = if before_origin {
            origin.duration_since(*deadline)
        } else {
            deadline.duration_since(origin)
        };
        let nanos = u64::try_from(offset.as_nanos()).map_err(serde::ser::Error::custom)?;
        (before_origin, nanos).serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Instant, D::Error> {
        let (before_origin, nanos) = <(bool, u64)>::deserialize(deserializer)?;
        let offset = Duration::from_nanos(nanos);
        let origin = origin();
        let deadline = if before_origin {
            origin.checked_sub(offset)
        } else {
            origin.checked_add(offset)
        };
        deadline.ok_or_else(|| serde::de::Error::custom("generation cache deadline overflow"))
    }
}

#[derive(Clone)]
pub(super) struct GenerationChainCommit<'a> {
    pub(crate) principal: Principal,
    pub(crate) id: String,
    pub(crate) parent: &'a ActiveGenerationChain,
    pub(crate) request_delta: &'a AiRequest,
    pub(crate) effective_request: Option<&'a AiRequest>,
    pub(crate) response: AiResponse,
    pub(crate) upstream_response_id: Option<String>,
    pub(crate) effective_state: GenerationChainState,
}

pub(super) fn generation_cache_key(
    principal: &str,
    id: &TurnNodeId,
    ingress: Option<ProtocolId>,
) -> String {
    // JSON tuples avoid delimiter collisions in Principal or node IDs.
    let ingress =
        ingress.map(|endpoint| (endpoint.protocol.as_str(), endpoint.name, endpoint.version));
    serde_json::to_string(&(principal, id.as_str(), RESPONSE_PAYLOAD_VERSION, ingress))
        .expect("generation cache key contains only JSON-serializable identifiers")
}

pub(crate) fn request_has_item_references(request: &AiRequest) -> bool {
    item_reference_ids(&request.items).next().is_some()
}

pub(super) fn item_reference_ids(items: &[AiItem]) -> impl Iterator<Item = &str> {
    items.iter().filter_map(item_reference_id)
}
pub(super) fn request_has_response_artifact_references(request: &AiRequest) -> bool {
    request.items.iter().any(|item| match &item.content {
        MessageContent::Blocks(blocks) => blocks.iter().any(|block| {
            matches!(
                block,
                ContentBlock::Image {
                    source: MediaSource::FileId { file_id, .. },
                    ..
                } if file_id.starts_with("stravia://artifacts/")
            )
        }),
        _ => false,
    })
}

pub(crate) async fn hydrate_response_artifact_references(
    principal: &Principal,
    request: &mut AiRequest,
    artifacts: Option<&dyn stravia_runtime_contract::artifact::ArtifactStore>,
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
                } if file_id.starts_with("stravia://artifacts/") => {
                    if file_id.contains(['?', '#']) {
                        return Err("item_reference_not_found".to_string());
                    }
                    Some((
                        stravia_runtime_contract::artifact::ArtifactId::from_reference(file_id)
                            .map_err(|_| "item_reference_not_found".to_string())?,
                        detail.clone(),
                    ))
                }
                _ => None,
            };
            let Some((artifact_id, detail)) = artifact else {
                continue;
            };
            let store = artifacts.ok_or_else(|| "item_reference_not_found".to_string())?;
            let reader = store
                .open(principal, &artifact_id)
                .await
                .map_err(|_| "item_reference_not_found".to_string())?;
            *block = ContentBlock::Image {
                source: MediaSource::Url(reader.artifact.reference()),
                detail,
                cache_control: None,
            };
            hydrated_artifacts.push(serde_json::json!({
                "block_index": block_index,
                "artifact_id": artifact_id,
            }));
        }
        if !hydrated_artifacts.is_empty() {
            message
                .meta
                .get_or_insert_with(Default::default)
                .insert_graph_extension(
                    "__stravia_artifact_references",
                    serde_json::Value::Array(hydrated_artifacts),
                )
                .expect("artifact reference key is not reserved");
        }
    }
    Ok(())
}

pub(crate) fn request_preserves_upstream_response(request: &AiRequest) -> bool {
    request.parameters.store.unwrap_or(true)
}

/// discover_prefix 一次调用内不变的核验输入：借用于调用方已算好的投影与
/// 指纹，避免逐候选重复计算或克隆。
struct PrefixVerifyContext<'a> {
    prefix_units: &'a [u32],
    controls_fingerprint: &'a str,
    client_request: &'a AiRequest,
    leading_control_items: usize,
}

impl GenerationChainStore {
    pub fn from_turn_chain(
        turn_chain: Arc<dyn TurnChainStore>,
        ttl: Duration,
        runtime_cache: RuntimeCache,
    ) -> Self {
        Self {
            turn_chain,
            ttl,
            runtime_cache,
        }
    }

    /// Rebuild derived Generation prefix indexes before accepting requests.
    /// The store visits each ancestor once; the fold retains only its hash and count.
    pub(super) async fn rebuild_prefixes(&self) -> Result<(), TurnUnavailable> {
        let mut rebuild = PrefixRebuild::default();
        self.turn_chain
            .rebuild_prefixes(GENERATION_PREFIX_NAMESPACE, &mut |node, completed_at| {
                rebuild.decode(node, completed_at)
            })
            .await
    }

    pub fn allocate_id(&self) -> String {
        TurnNodeId::response().to_string()
    }

    #[tracing::instrument(
        target = "stravia::perf",
        name = "generation_chain.references.resolve_available",
        skip_all
    )]
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
                    .map(super::materialize::decode_response_node)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| "item_reference_not_found".to_string())?,
            );
        }
        resolve_item_references(items, &persisted, Some(ingress))
    }

    #[tracing::instrument(
        target = "stravia::perf",
        name = "generation_chain.parent.resolve",
        skip_all
    )]
    pub async fn materialize_parent(
        &self,
        principal: &Principal,
        request: &mut AiRequest,
    ) -> Result<ActiveGenerationChain, String> {
        request.normalize_parameters();
        let not_found = if request_has_item_references(request) {
            "item_reference_not_found"
        } else {
            "previous_response_not_found"
        };
        let previous_id = request
            .parameters
            .continuation
            .as_ref()
            .and_then(|continuation| continuation.previous_response_id())
            .map(str::to_owned);
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
                        .map(super::materialize::decode_response_node)
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
        request: &mut Arc<AiRequest>,
        workspace: &mut DiscoveryWorkspace,
    ) -> Result<Option<DiscoveredGenerationPrefix>, String> {
        self.discover_prefix(principal, request, false, workspace)
            .await
    }

    #[tracing::instrument(
        target = "stravia::perf",
        name = "generation_chain.parent.discover_prefix",
        skip_all,
        fields(candidate_count = 0_u64)
    )]
    pub(super) async fn discover_prefix(
        &self,
        principal: &Principal,
        request: &mut Arc<AiRequest>,
        allow_complete_window: bool,
        workspace: &mut DiscoveryWorkspace,
    ) -> Result<Option<DiscoveredGenerationPrefix>, String> {
        let snapshot = Arc::clone(request);
        let client_request = canonical_client_history_request(&snapshot);
        let leading_control_items = request.items.len() - client_request.items.len();
        let limit = client_request
            .items
            .len()
            .saturating_sub(usize::from(!allow_complete_window));
        if limit == 0 {
            return Ok(None);
        }
        let controls_fingerprint = stravia_runtime_contract::protocol::ir::canonical::hash_hex(
            &stravia_runtime_contract::protocol::ir::canonical::history_request_controls_hash(
                &client_request,
            ),
        );
        let session_fingerprint = generation_session_fingerprint(&client_request);
        let mut context_fingerprints = Vec::with_capacity(limit);
        let mut prefix_units = Vec::with_capacity(limit);
        let mut context =
            stravia_runtime_contract::protocol::ir::canonical::history_context_hash(&[]);
        let mut semantic_units = 0usize;
        for item in &client_request.items[..limit] {
            // 计数与摘要共用借用投影，不为大文本生成临时 Value 副本。
            let (next, units) =
                stravia_runtime_contract::protocol::ir::canonical::append_history_context_hash_with_count(
                    &context, item,
                );
            context = next;
            semantic_units += units;
            let units = u32::try_from(semantic_units).unwrap_or(u32::MAX);
            prefix_units.push(units);
            context_fingerprints.push((
                stravia_runtime_contract::protocol::ir::canonical::hash_hex(&context),
                units,
            ));
        }

        let mut candidate_count = 0_u64;
        let context = PrefixVerifyContext {
            prefix_units: &prefix_units,
            controls_fingerprint: &controls_fingerprint,
            client_request: &client_request,
            leading_control_items,
        };
        // Session 命中时 controls 索引读取是浪费：先核验 session 层，全部
        // 候选不匹配才查 controls。
        if let Some(session_fingerprint) = session_fingerprint.as_ref() {
            let fingerprints = context_fingerprints
                .iter()
                .map(|(_, item_count)| (session_fingerprint.clone(), *item_count))
                .collect();
            let candidates = self
                .turn_chain
                .find_reusable_prefixes(
                    principal,
                    TurnNodeKind::Response,
                    &ReusablePrefixQuery {
                        namespace: format!("{GENERATION_PREFIX_NAMESPACE}session"),
                        fingerprints,
                        projection: Some(stravia_runtime_contract::turn_chain::ReusablePrefixProjectionQuery {
                            version: stravia_runtime_contract::turn_chain::REUSABLE_PREFIX_PROJECTION_VERSION,
                            controls_fingerprint: controls_fingerprint.clone(),
                            context_fingerprints: context_fingerprints.clone(),
                        }),
                    },
                )
                .await
                .map_err(|error| error.to_string())?;
            candidate_count += candidates.len() as u64;
            tracing::Span::current().record("candidate_count", candidate_count);
            if let Some(prefix) = self
                .verify_prefix_candidates(principal, &context, candidates, request, workspace)
                .await?
            {
                return Ok(Some(prefix));
            }
        }

        let candidates = self
            .turn_chain
            .find_reusable_prefixes(
                principal,
                TurnNodeKind::Response,
                &ReusablePrefixQuery {
                    namespace: format!("{}{}", GENERATION_PREFIX_NAMESPACE, controls_fingerprint),
                    projection: Some(stravia_runtime_contract::turn_chain::ReusablePrefixProjectionQuery {
                        version: stravia_runtime_contract::turn_chain::REUSABLE_PREFIX_PROJECTION_VERSION,
                        controls_fingerprint: controls_fingerprint.clone(),
                        context_fingerprints: context_fingerprints.clone(),
                    }),
                    fingerprints: context_fingerprints,
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        candidate_count += candidates.len() as u64;
        tracing::Span::current().record("candidate_count", candidate_count);
        self.verify_prefix_candidates(principal, &context, candidates, request, workspace)
            .await
    }

    /// 索引命中只证明指纹相等：units、controls 与前缀投影逐项复核一致才算
    /// 胜出，候选间保持层内 SQL 顺序。
    async fn verify_prefix_candidates(
        &self,
        principal: &Principal,
        context: &PrefixVerifyContext<'_>,
        candidates: Vec<stravia_runtime_contract::turn_chain::ReusablePrefixCandidate>,
        request: &mut Arc<AiRequest>,
        workspace: &mut DiscoveryWorkspace,
    ) -> Result<Option<DiscoveredGenerationPrefix>, String> {
        for candidate in candidates {
            let matched_units = usize::try_from(candidate.item_count).unwrap_or(usize::MAX);
            // prefix_units 已带累计 units，直接定位前缀边界，不再对
            // client_items 做第二次逐 item 投影。饱和到 u32::MAX 的相等由
            // 下方 items_equal 兜底，语义不变。
            let matched_items = context
                .prefix_units
                .iter()
                .position(|units| *units as usize >= matched_units)
                .filter(|index| context.prefix_units[*index] as usize == matched_units)
                .map(|index| index + 1);
            let Some(matched_items) = matched_items else {
                continue;
            };
            let materialized = self
                .materialize_discovery_generation(principal, &candidate.node_id, workspace)
                .await?;
            // 便宜的标量比较先短路：fingerprint 相等只是索引命中，units 与
            // controls 相同才值得对两侧前缀做完整 canonical 投影比较。
            let history_matches = materialized.client_history.as_ref().is_some_and(|history| {
                history.controls_fingerprint == context.controls_fingerprint
            }) && materialized.client_item_units == matched_units
                && items_equal(
                    &materialized.client_items,
                    &context.client_request.items[..matched_items],
                );
            if !history_matches {
                // Ordinary discovery pays no admission cost when its first actual
                // candidate wins. Native multi-stage discovery is already enabled.
                workspace.enable(principal);
                continue;
            }
            let mut delta = request.as_ref().clone();
            let matched_request_items = context.leading_control_items + matched_items;
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
            // 快路径直接消费已核验的 Arc 只为省掉同节点的第二次 cache_get/
            // load；引用需要全祖先目录（window 不是 catalog）、ingress 语义和
            // item_reference_not_found，过期需保留 TTL 拒绝语义，两者都交给
            // materialize_parent_id 的原始路径。
            let active = if request_has_item_references(&delta)
                || materialized.expires_at <= std::time::Instant::now()
            {
                self.materialize_parent_id(principal, candidate.node_id.as_str(), &mut delta)
                    .await?
            } else {
                Self::adopt_materialized_parent(
                    candidate.node_id.as_str(),
                    materialized.as_ref(),
                    None,
                    None,
                    &mut delta,
                )?
            };
            *request = Arc::new(delta);
            return Ok(Some(DiscoveredGenerationPrefix {
                active,
                matched_items: matched_request_items,
            }));
        }
        Ok(None)
    }

    #[tracing::instrument(
        target = "stravia::perf",
        name = "generation_chain.parent.compaction_source",
        skip_all,
        fields(candidate_count)
    )]
    pub(super) async fn compaction_source_from_items(
        &self,
        principal: &Principal,
        request: &AiRequest,
        workspace: &mut DiscoveryWorkspace,
    ) -> Result<Option<(usize, String)>, BeginError> {
        let request = canonical_client_history_request(request);
        let mut seen = std::collections::HashSet::new();
        let mut source: Option<(usize, String)> = None;
        for (position, item) in request.items.iter().enumerate().rev() {
            let Some(id) = item.id_ref().and_then(
                stravia_protocol_codec::codec::open_responses::formatter::response_id_from_gateway_item_id,
            ) else {
                continue;
            };
            if !seen.insert(id.clone()) {
                continue;
            }
            let node_id = TurnNodeId::new(&id);
            let materialized = if let Some((nodes, expires_at)) =
                workspace.chain(principal, &node_id, std::time::Instant::now())
            {
                materialize_discovery_nodes(&nodes, expires_at)
                    .map_err(|_| BeginError::CompactionUnavailable)?
            } else {
                if workspace.is_enabled_for(principal) {
                    let (nodes, chain) = match workspace
                        .read(self.turn_chain.as_ref(), principal, &node_id)
                        .await
                    {
                        Ok(chain) => chain,
                        Err(TurnUnavailable::Unavailable) => continue,
                        Err(TurnUnavailable::Storage(_)) => {
                            return Err(BeginError::CompactionStorageFailed);
                        }
                        Err(TurnUnavailable::Decode(_)) => {
                            return Err(BeginError::CompactionUnavailable);
                        }
                    };
                    workspace
                        .fold(principal, &nodes, &chain)
                        .map_err(|_| BeginError::CompactionUnavailable)?
                } else {
                    let chain = match self
                        .turn_chain
                        .materialize_with_expiry(principal, TurnNodeKind::Response, &node_id)
                        .await
                    {
                        Ok(chain) => chain,
                        Err(TurnUnavailable::Unavailable) => continue,
                        Err(TurnUnavailable::Storage(_)) => {
                            return Err(BeginError::CompactionStorageFailed);
                        }
                        Err(TurnUnavailable::Decode(_)) => {
                            return Err(BeginError::CompactionUnavailable);
                        }
                    };
                    materialize_generation_nodes(chain.nodes, chain.expires_at)
                        .map_err(|_| BeginError::CompactionUnavailable)?
                }
            };
            let count = materialized.client_items.len();
            // Compact instructions govern a new operation. A gateway-owned output identity
            // plus its complete client prefix proves the source without reusing its controls.
            if position < count
                && count <= request.items.len()
                && materialized.client_items[position].id_ref() == item.id_ref()
                && items_equal(&materialized.client_items, &request.items[..count])
            {
                if source
                    .as_ref()
                    .is_some_and(|(best, other)| count == *best && id != *other)
                {
                    return Err(BeginError::CompactionConflict);
                }
                if source.as_ref().is_none_or(|(best, _)| count > *best) {
                    source = Some((count, id));
                }
            }
        }
        tracing::Span::current().record("candidate_count", seen.len() as u64);
        Ok(source)
    }

    #[tracing::instrument(
        target = "stravia::perf",
        name = "generation_chain.parent.source",
        skip_all
    )]
    pub(super) async fn source_parent(
        &self,
        principal: &Principal,
        parent_id: &str,
        workspace: &mut DiscoveryWorkspace,
    ) -> Result<ActiveGenerationChain, String> {
        let id = TurnNodeId::new(parent_id);
        let (nodes, expires_at) = match workspace.chain(principal, &id, std::time::Instant::now()) {
            Some(chain) => chain,
            None if workspace.is_enabled_for(principal) => {
                let (nodes, chain) = workspace
                    .read(self.turn_chain.as_ref(), principal, &id)
                    .await
                    .map_err(|error| error.to_string())?;
                // Source metadata needs no execution replay. Fold only while a
                // successful result can still admit reusable decoded nodes.
                if workspace
                    .context
                    .as_ref()
                    .is_some_and(|context| context.admission_open())
                {
                    workspace.fold(principal, &nodes, &chain)?;
                }
                (nodes, chain.expires_at)
            }
            None => {
                // Explicit-parent preparation has no discovery cache. Resolve
                // source identity freshly without adopting old execution state.
                let chain = self
                    .turn_chain
                    .materialize_with_expiry(principal, TurnNodeKind::Response, &id)
                    .await
                    .map_err(|error| error.to_string())?;
                let nodes = chain
                    .nodes
                    .into_iter()
                    .map(DiscoveryWorkspace::decode_node)
                    .collect::<Result<Vec<_>, _>>()?;
                (nodes, chain.expires_at)
            }
        };
        let root_id = nodes.first().map(|node| node.id.to_string());
        let mut compaction_record_ids = Vec::new();
        for node in &nodes {
            compaction_record_ids.extend_from_slice(&node.persisted.compaction_record_ids);
        }
        compaction_record_ids.sort();
        compaction_record_ids.dedup();
        let parent_target_key = nodes
            .last()
            .map(|node| &node.persisted.effective_state.selected_target_key)
            .filter(|key| !key.is_empty())
            .cloned();
        Ok(ActiveGenerationChain {
            root_id,
            parent_id: Some(parent_id.to_owned()),
            parent_target_key,
            parent_expires_at: (expires_at > std::time::Instant::now()).then_some(expires_at),
            compaction_record_ids,
            ..ActiveGenerationChain::default()
        })
    }

    #[tracing::instrument(
        target = "stravia::perf",
        name = "generation_chain.parent.materialize",
        skip_all
    )]
    async fn materialize_parent_id(
        &self,
        principal: &Principal,
        parent_id: &str,
        request: &mut AiRequest,
    ) -> Result<ActiveGenerationChain, String> {
        let has_references = request_has_item_references(request);
        let not_found = if has_references {
            "item_reference_not_found"
        } else {
            "previous_response_not_found"
        };
        let ingress = ProtocolTransform::inferred_ingress(request);
        let id = TurnNodeId::new(parent_id);
        let (materialized, catalog) = if has_references {
            let (materialized, catalog) = self
                .materialize_generation_with_catalog(principal, &id, ingress, not_found)
                .await?;
            (materialized, Some(catalog?))
        } else {
            (
                self.materialize_generation(principal, &id)
                    .await
                    .map_err(|_| not_found.to_string())?,
                None,
            )
        };
        Self::adopt_materialized_parent(parent_id, &materialized, catalog, ingress, request)
    }

    /// 供已持有核验结果的调用方（显式 previous_response_id 与
    /// discover_prefix 胜出候选）直接投影父代，避免重复取回同一节点。
    /// `catalog`/`ingress` 仅供 ItemReference 解析，无引用时传 None。
    fn adopt_materialized_parent(
        parent_id: &str,
        materialized: &MaterializedGeneration,
        catalog: Option<Vec<AiItem>>,
        ingress: Option<ProtocolId>,
        request: &mut AiRequest,
    ) -> Result<ActiveGenerationChain, String> {
        let mut new_messages = std::mem::take(&mut request.items);
        if let Some(catalog) = catalog {
            resolve_catalog_references(&mut new_messages, &catalog, ingress)?;
        }
        if let Some(config) = materialized.effective_request.clone() {
            config.apply_missing_to(request);
        }
        request.items = materialized.effective_items.clone();
        request.items.extend(new_messages);
        let instructions_present = request
            .parameters
            .resets(stravia_runtime_contract::protocol::ir::ParameterReset::Instructions);
        if request.instructions.is_none() && !instructions_present {
            request.instructions = materialized.effective_system.clone();
        }
        crate::router::clear_previous_response_id(request);
        Ok(ActiveGenerationChain {
            root_id: Some(materialized.root_id.clone()),
            parent_id: Some(parent_id.to_owned()),
            parent_upstream_response_id: materialized.upstream_response_id.clone(),
            parent_state: Some(materialized.effective_state.clone()),
            parent_target_key: None,
            parent_expires_at: (materialized.expires_at > std::time::Instant::now())
                .then_some(materialized.expires_at),
            media_turn_messages: materialized.media_turn_messages.clone(),
            parent_effective_items: materialized.effective_items.clone(),
            parent_client_items: materialized.client_items.clone(),
            replace_effective_history: false,
            replacement_client_items: None,
            compaction_input_range: None,
            fresh_inline_states: Vec::new(),
            compaction_record_ids: materialized.compaction_record_ids.clone(),
        })
    }

    async fn load_generation_chain(
        &self,
        principal: &Principal,
        id: &TurnNodeId,
    ) -> Result<stravia_runtime_contract::turn_chain::MaterializedTurnChain, String> {
        use tracing::Instrument as _;
        let span = tracing::info_span!(target: "stravia::perf", "generation_chain.history.load", status = tracing::field::Empty);
        let chain = self
            .turn_chain
            .materialize_with_expiry(principal, TurnNodeKind::Response, id)
            .instrument(span.clone())
            .await;
        span.record("status", if chain.is_ok() { "completed" } else { "error" });
        chain.map_err(|error| error.to_string())
    }

    async fn materialize_generation_with_catalog(
        &self,
        principal: &Principal,
        id: &TurnNodeId,
        ingress: Option<ProtocolId>,
        not_found: &str,
    ) -> Result<(Arc<MaterializedGeneration>, Result<Vec<AiItem>, String>), String> {
        let principal_key = principal.continuation_key();
        let cached = self.materialization_cache_get(&principal_key, id).await;
        crate::performance::record_generation_cache_access(cached.is_some());
        if let Some(materialized) = cached {
            if let Some(catalog) = self
                .materialization_catalog_get(&principal_key, id, ingress)
                .await
            {
                return Ok((materialized, catalog));
            }
            let chain = self
                .load_generation_chain(principal, id)
                .await
                .map_err(|_| not_found.to_string())?;
            let expires_at = chain.expires_at.min(materialized.expires_at);
            let mut catalog = Ok(Vec::new());
            for node in chain.nodes {
                let (_, node) = super::materialize::decode_response_node(node)
                    .map_err(|_| not_found.to_string())?;
                if let Ok(items) = catalog.as_mut()
                    && let Err(error) = append_history_catalog_node(items, &node, ingress)
                {
                    catalog = Err(error);
                }
            }
            self.materialization_catalog_insert(&principal_key, id, ingress, &catalog, expires_at)
                .await;
            return Ok((materialized, catalog));
        }
        let chain = self
            .load_generation_chain(principal, id)
            .await
            .map_err(|_| not_found.to_string())?;
        let (materialized, catalog) =
            materialize_generation_nodes_with_catalog(chain.nodes, chain.expires_at, ingress)
                .map_err(|_| not_found.to_string())?;
        let materialized = Arc::new(materialized);
        self.materialization_cache_insert(
            principal_key.clone(),
            id.clone(),
            Arc::clone(&materialized),
        )
        .await;
        self.materialization_catalog_insert(
            &principal_key,
            id,
            ingress,
            &catalog,
            materialized.expires_at,
        )
        .await;
        Ok((materialized, catalog))
    }

    pub(super) async fn materialize_discovery_generation(
        &self,
        principal: &Principal,
        id: &TurnNodeId,
        workspace: &mut DiscoveryWorkspace,
    ) -> Result<Arc<MaterializedGeneration>, String> {
        if !workspace.is_enabled_for(principal) {
            return self.materialize_generation(principal, id).await;
        }
        if let Some((nodes, expires_at)) = workspace.chain(principal, id, std::time::Instant::now())
        {
            return materialize_discovery_nodes(&nodes, expires_at).map(Arc::new);
        }
        let principal_key = principal.continuation_key();
        if let Some(materialized) = self.materialization_cache_get(&principal_key, id).await {
            crate::performance::record_generation_cache_access(true);
            return Ok(materialized);
        }
        crate::performance::record_generation_cache_access(false);
        let (nodes, chain) = workspace
            .read(self.turn_chain.as_ref(), principal, id)
            .await
            .map_err(|error| error.to_string())?;
        let materialized = Arc::new(workspace.fold(principal, &nodes, &chain)?);
        self.materialization_cache_insert(principal_key, id.clone(), Arc::clone(&materialized))
            .await;
        Ok(materialized)
    }

    pub(super) async fn materialize_generation(
        &self,
        principal: &Principal,
        id: &TurnNodeId,
    ) -> Result<Arc<MaterializedGeneration>, String> {
        let principal_key = principal.continuation_key();
        if let Some(materialized) = self.materialization_cache_get(&principal_key, id).await {
            crate::performance::record_generation_cache_access(true);
            return Ok(materialized);
        }
        crate::performance::record_generation_cache_access(false);
        let chain = self.load_generation_chain(principal, id).await?;
        let materialized = Arc::new(materialize_generation_nodes(chain.nodes, chain.expires_at)?);
        self.materialization_cache_insert(principal_key, id.clone(), Arc::clone(&materialized))
            .await;
        Ok(materialized)
    }

    async fn materialization_cache_get(
        &self,
        principal: &str,
        id: &TurnNodeId,
    ) -> Option<Arc<MaterializedGeneration>> {
        let key = generation_cache_key(principal, id, None);
        let materialized = self
            .runtime_cache
            .get::<MaterializedGeneration>("generation.materialized", &key)
            .await?;
        if materialized.expires_at <= std::time::Instant::now()
            || materialized.payload_version != RESPONSE_PAYLOAD_VERSION
        {
            self.runtime_cache
                .remove("generation.materialized", &key)
                .await;
            return None;
        }
        Some(materialized)
    }

    async fn materialization_cache_insert(
        &self,
        principal: String,
        id: TurnNodeId,
        materialized: Arc<MaterializedGeneration>,
    ) {
        let ttl = materialized
            .expires_at
            .saturating_duration_since(std::time::Instant::now());
        if ttl.is_zero() || materialized.payload_version != RESPONSE_PAYLOAD_VERSION {
            return;
        }
        let key = generation_cache_key(&principal, &id, None);
        let bytes = serialized_size_bytes(materialized.as_ref());
        self.runtime_cache
            .put("generation.materialized", &key, materialized, bytes, ttl)
            .await;
    }

    async fn materialization_catalog_get(
        &self,
        principal: &str,
        id: &TurnNodeId,
        ingress: Option<ProtocolId>,
    ) -> Option<Result<Vec<AiItem>, String>> {
        let key = generation_cache_key(principal, id, ingress);
        let entry = self
            .runtime_cache
            .get::<CachedCatalog>("generation.catalog", &key)
            .await?;
        if entry.expires_at <= std::time::Instant::now() {
            self.runtime_cache.remove("generation.catalog", &key).await;
            return None;
        }
        Some(entry.catalog.clone())
    }

    async fn materialization_catalog_insert(
        &self,
        principal: &str,
        id: &TurnNodeId,
        ingress: Option<ProtocolId>,
        catalog: &Result<Vec<AiItem>, String>,
        expires_at: std::time::Instant,
    ) {
        let ttl = expires_at.saturating_duration_since(std::time::Instant::now());
        if ttl.is_zero() {
            return;
        }
        let key = generation_cache_key(principal, id, ingress);
        // 只计数借用表示；超预算历史仍正常返回，不先深拷贝再丢弃。
        let bytes = serialized_size_bytes(&BorrowedCachedCatalog {
            catalog,
            expires_at,
        });
        if !self
            .runtime_cache
            .can_admit::<CachedCatalog>("generation.catalog", &key, bytes)
        {
            return;
        }
        let entry = Arc::new(CachedCatalog {
            catalog: catalog.clone(),
            expires_at,
        });
        self.runtime_cache
            .put("generation.catalog", &key, entry, bytes, ttl)
            .await;
    }

    #[cfg(test)]
    pub(super) fn preserves_upstream_response(
        original: &AiResponse,
        candidate: &AiResponse,
    ) -> bool {
        items_equal(&original.items, &candidate.items)
    }

    #[cfg(test)]
    pub(crate) async fn save(
        &self,
        commit: GenerationChainCommit<'_>,
    ) -> Result<(), TurnCommitError> {
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
        commit: GenerationChainCommit<'_>,
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
        let mut response = response;
        let mut effective_request = Cow::Borrowed(effective_request.unwrap_or(request_delta));
        let projected_client = project_client_commit(parent, request_delta, &response)?;
        let fresh_states = &parent.fresh_inline_states;
        let inline_boundary =
            stravia_protocol_codec::codec::open_responses::inline_compaction_boundary;
        let mut effective_inline = false;
        // Hidden rounds also appear in the projected response. Prefer their real
        // position in the effective request so completed platform work survives.
        if let Some(output_start) = inline_boundary(&response.items, fresh_states) {
            let state = std::slice::from_ref(&response.items[output_start]);
            if let Some(input_start) = inline_boundary(&effective_request.items, state) {
                effective_request.to_mut().items.drain(..input_start);
                response.items.drain(..=output_start);
            } else {
                effective_request.to_mut().items.clear();
                response.items.drain(..output_start);
            }
            effective_inline = true;
        } else if let Some(input_start) = inline_boundary(&effective_request.items, fresh_states) {
            effective_request.to_mut().items.drain(..input_start);
            effective_inline = true;
        }
        effective_state.context_fingerprint = history_context_fingerprint(&effective_request.items);
        effective_state.context_messages = effective_request.items.len();
        effective_state.refresh_request_semantics(&effective_request);
        effective_state.append_output(&response);
        if !effective_inline
            && let Some(proof) =
                effective_request
                    .meta
                    .redaction
                    .provider_proof()
                    .map_err(|_| {
                        TurnCommitError::Storage(
                            "reversible redaction semantic proof unavailable".into(),
                        )
                    })?
        {
            effective_state.context_fingerprint =
                stravia_runtime_contract::protocol::ir::canonical::hash_hex(&proof.context_hash);
            effective_state.context_messages = proof.context_messages;
            effective_state.canonical_controls_fingerprint = proof.controls_fingerprint;
        }
        let ProjectedClientCommit {
            client_request_delta,
            client_items: _,
            client_output,
            client_history_mutation,
            client_history,
            client_item_units,
        } = projected_client;
        let effective_history_mutation = if effective_inline || parent.replace_effective_history {
            EffectiveHistoryMutation::Replace {
                items: Cow::Borrowed(&effective_request.items),
            }
        } else if effective_request
            .items
            .get(..parent.parent_effective_items.len())
            .is_some_and(|prefix| items_equal(prefix, &parent.parent_effective_items))
        {
            EffectiveHistoryMutation::Append {
                items: Cow::Borrowed(
                    &effective_request.items[parent.parent_effective_items.len()..],
                ),
            }
        } else {
            EffectiveHistoryMutation::Replace {
                items: Cow::Borrowed(&effective_request.items),
            }
        };
        let item_count = u32::try_from(client_item_units)
            .map_err(|error| TurnCommitError::Storage(error.to_string()))?;
        let reusable_prefix = Some(ReusablePrefixMetadata {
            namespace: client_history.reusable_namespace(),
            fingerprint: client_history
                .session_fingerprint
                .clone()
                .unwrap_or_else(|| client_history.context_fingerprint.clone()),
            item_count,
            completed_at: chrono::Utc::now().timestamp_millis(),
            projection: Some(client_history.reusable_projection()),
        });
        let effective_system = effective_request.instructions.clone();
        let trusted_media_turn_ids = response.trusted_media_turn_ids.clone();
        let payload = serde_json::to_value(PersistedResponseNode {
            client_delta: RequestDelta {
                messages: Cow::Borrowed(&client_request_delta.items),
                system: client_request_delta.instructions.clone(),
            },
            client_output: Some(client_output),
            client_history_mutation,
            compaction_record_ids: parent.compaction_record_ids.clone(),
            effective_history_mutation: Some(effective_history_mutation),
            effective_system,
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
                parent_id: parent.parent_id.as_deref().map(TurnNodeId::new),
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
