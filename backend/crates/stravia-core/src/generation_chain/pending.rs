use super::*;

#[derive(Clone, Default)]
pub(super) struct PendingGenerationCommits {
    entries: Arc<Mutex<Vec<std::sync::Weak<PendingGenerationCommit>>>>,
}

#[derive(Clone)]
pub(crate) struct GenerationCommitFence {
    owner: Arc<GenerationCommitFenceOwner>,
}

struct GenerationCommitFenceOwner {
    pending: Arc<PendingGenerationCommit>,
}

struct PendingGenerationCommit {
    principal: Principal,
    node_id: String,
    controls_fingerprint: String,
    client_items: Vec<AiItem>,
    resolved: tokio::sync::watch::Sender<bool>,
}

impl PendingGenerationCommits {
    pub(super) fn register(
        &self,
        principal: Principal,
        node_id: String,
        projected: ProjectedClientCommit,
    ) -> GenerationCommitFence {
        let (resolved, _) = tokio::sync::watch::channel(false);
        let pending = Arc::new(PendingGenerationCommit {
            principal,
            node_id,
            controls_fingerprint: projected.client_history.controls_fingerprint,
            client_items: projected.client_items,
            resolved,
        });
        let mut entries = self.entries.lock();
        entries.retain(|entry| {
            entry
                .upgrade()
                .is_some_and(|pending| !*pending.resolved.borrow())
        });
        entries.push(Arc::downgrade(&pending));
        GenerationCommitFence {
            owner: Arc::new(GenerationCommitFenceOwner { pending }),
        }
    }

    pub(super) async fn wait_for_relevant(&self, principal: &Principal, request: &AiRequest) {
        let pending = {
            let mut entries = self.entries.lock();
            let mut pending = Vec::new();
            entries.retain(|entry| {
                let Some(entry) = entry.upgrade() else {
                    return false;
                };
                if *entry.resolved.borrow() {
                    return false;
                }
                if entry.principal == *principal {
                    pending.push(entry);
                }
                true
            });
            pending
        };
        if pending.is_empty() {
            return;
        }
        let canonical = canonical_client_history_request(request);
        let controls_fingerprint =
            ClientHistoryState::from_request(&canonical, &canonical.items).controls_fingerprint;
        let explicit_parent = crate::router::parent_id_from_request(request);
        let ingress = ProtocolTransform::inferred_ingress(request);
        let referenced_nodes = ingress
            .map(|ingress| item_reference_node_ids(ingress, &request.items))
            .unwrap_or_default();
        let item_nodes = request
            .items
            .iter()
            .filter_map(AiItem::id_ref)
            .filter_map(
                stravia_protocol_codec::codec::open_responses::formatter::response_id_from_gateway_item_id,
            )
            .collect::<Vec<_>>();
        for pending in pending {
            let id_match = explicit_parent.as_deref() == Some(pending.node_id.as_str())
                || referenced_nodes.iter().any(|id| id == &pending.node_id)
                || item_nodes.iter().any(|id| id == &pending.node_id);
            let prefix_match = pending.controls_fingerprint == controls_fingerprint
                && canonical.items.len() > pending.client_items.len()
                && canonical
                    .items
                    .get(..pending.client_items.len())
                    .is_some_and(|items| items_equal(items, &pending.client_items));
            if id_match || prefix_match {
                pending.wait().await;
            }
        }
    }
}

impl PendingGenerationCommit {
    async fn wait(&self) {
        let mut resolved = self.resolved.subscribe();
        while !*resolved.borrow_and_update() {
            if resolved.changed().await.is_err() {
                return;
            }
        }
    }

    fn resolve(&self) {
        self.resolved.send_replace(true);
    }
}

impl GenerationCommitFence {
    pub(crate) fn resolve(&self) {
        self.owner.pending.resolve();
    }
}

impl Drop for GenerationCommitFenceOwner {
    fn drop(&mut self) {
        self.pending.resolve();
    }
}
