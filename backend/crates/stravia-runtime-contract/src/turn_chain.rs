use std::{
    any::Any,
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Principal;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TurnNodeId(String);

impl TurnNodeId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn response() -> Self {
        Self(crate::identifier::new_id())
    }

    pub fn agent() -> Self {
        Self(crate::identifier::new_id())
    }

    pub fn web_search() -> Self {
        Self(crate::identifier::new_id())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn from_reference(reference: &str) -> Result<Self, TurnReferenceError> {
        let id = reference
            .strip_prefix("stravia://turns/")
            .filter(|id| crate::identifier::valid_id(id))
            .ok_or(TurnReferenceError)?;
        Ok(Self::new(id))
    }

    pub fn reference(&self) -> String {
        format!("stravia://turns/{}", self.as_str())
    }
}

#[derive(Debug, Clone, Copy, thiserror::Error, PartialEq, Eq)]
#[error("invalid Turn Reference")]
pub struct TurnReferenceError;

pub mod serde_path {
    use serde::{Deserialize, Deserializer, Serializer};

    use super::TurnNodeId;

    pub fn serialize<S>(id: &TurnNodeId, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&id.reference())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<TurnNodeId, D::Error>
    where
        D: Deserializer<'de>,
    {
        let path = String::deserialize(deserializer)?;
        TurnNodeId::from_reference(&path).map_err(serde::de::Error::custom)
    }
}

pub mod serde_option_path {
    use serde::{Deserialize, Deserializer, Serializer};

    use super::TurnNodeId;

    pub fn serialize<S>(id: &Option<TurnNodeId>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match id {
            Some(id) => serializer.serialize_some(&id.reference()),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<TurnNodeId>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let path = Option::<String>::deserialize(deserializer)?;
        path.map(|path| TurnNodeId::from_reference(&path).map_err(serde::de::Error::custom))
            .transpose()
    }
}

impl std::fmt::Display for TurnNodeId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnNodeKind {
    Response,
    Agent,
    WebSearch,
}

impl TurnNodeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Response => "response",
            Self::Agent => "agent",
            Self::WebSearch => "web_search",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnNode {
    pub id: TurnNodeId,
    pub kind: TurnNodeKind,
    pub parent_id: Option<TurnNodeId>,
    pub payload_version: u32,
    pub payload: Value,
}

#[derive(Debug, Clone)]
pub struct MaterializedTurnChain {
    pub nodes: Vec<TurnNode>,
    pub expires_at: std::time::Instant,
}

/// One request's validated, decoded history workspace. Raw node payloads are
/// never retained. Decoded nodes and restored external contents share one byte
/// budget; only a successful domain fold followed by `admit` retains either.
/// Budget refusal closes admission without discarding existing cache hits.
pub struct TurnReadContext {
    principal: Principal,
    kind: TurnNodeKind,
    decoder: Option<TurnNodeDecoder>,
    nodes: HashMap<TurnNodeId, SharedTurnNode>,
    bytes: usize,
    budget: usize,
    admission_open: bool,
    validated_until: HashMap<TurnNodeId, Instant>,
    contents: HashMap<String, (Arc<Value>, usize)>,
    pending_contents: HashMap<String, (Arc<Value>, usize)>,
    pending_bytes: usize,
}

pub type TurnNodeDecoder = fn(TurnNode) -> Result<Arc<dyn Any + Send + Sync>, String>;

#[derive(Clone)]
pub struct SharedTurnNode {
    pub id: TurnNodeId,
    pub kind: TurnNodeKind,
    pub parent_id: Option<TurnNodeId>,
    pub payload_version: u32,
    pub expires_at: Instant,
    pub raw: Option<Arc<TurnNode>>,
    pub decoded: Option<Arc<dyn Any + Send + Sync>>,
}

pub struct SharedMaterializedTurnChain {
    pub nodes: Vec<SharedTurnNode>,
    pub expires_at: Instant,
}

impl TurnReadContext {
    pub const BYTE_LIMIT: usize = 16 * 1024 * 1024;

    pub fn new(principal: &Principal, kind: TurnNodeKind) -> Self {
        Self::with_budget(principal, kind, Self::BYTE_LIMIT)
    }

    pub fn with_budget(principal: &Principal, kind: TurnNodeKind, budget: usize) -> Self {
        Self {
            principal: principal.clone(),
            kind,
            decoder: None,
            nodes: HashMap::new(),
            bytes: 0,
            budget: budget.min(Self::BYTE_LIMIT),
            admission_open: true,
            validated_until: HashMap::new(),
            contents: HashMap::new(),
            pending_contents: HashMap::new(),
            pending_bytes: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// 在各节点恢复后立即消费 JSON，避免整条链的原始树与领域对象同时驻留。
    /// 领域折叠成功之前仍不得 admit；解码失败不保留本次暂存内容。
    pub fn with_decoder(mut self, decoder: TurnNodeDecoder) -> Self {
        self.decoder = Some(decoder);
        self
    }

    pub fn retains_node(&self, id: &TurnNodeId) -> bool {
        self.nodes.contains_key(id)
    }

    pub fn prepare_node(
        &self,
        node: TurnNode,
        expires_at: Instant,
    ) -> Result<SharedTurnNode, TurnUnavailable> {
        let mut shared = SharedTurnNode {
            id: node.id.clone(),
            kind: node.kind,
            parent_id: node.parent_id.clone(),
            payload_version: node.payload_version,
            expires_at,
            raw: None,
            decoded: None,
        };
        if let Some(decode) = self.decoder {
            shared.decoded = Some(decode(node).map_err(TurnUnavailable::Decode)?);
        } else {
            shared.raw = Some(Arc::new(node));
        }
        Ok(shared)
    }

    pub fn admission_open(&self) -> bool {
        self.admission_open
    }

    /// Adapters clear unadmitted content before starting every candidate read.
    pub fn begin_read(&mut self) {
        self.pending_contents.clear();
        self.pending_bytes = 0;
    }

    pub fn cached_content(&self, key: &str) -> Option<Arc<Value>> {
        self.contents.get(key).map(|(value, _)| value.clone())
    }

    pub fn stage_content(&mut self, key: String, value: Arc<Value>, bytes: usize) {
        if !self.admission_open
            || self.contents.contains_key(&key)
            || self.pending_contents.contains_key(&key)
        {
            return;
        }
        let remaining = self
            .budget
            .saturating_sub(self.bytes)
            .saturating_sub(self.pending_bytes);
        if bytes > remaining {
            self.admission_open = false;
            self.begin_read();
            return;
        }
        self.pending_bytes += bytes;
        self.pending_contents.insert(key, (value, bytes));
    }

    pub fn matches(&self, principal: &Principal, kind: TurnNodeKind) -> bool {
        &self.principal == principal && self.kind == kind
    }

    /// Only call after the entire chain has passed domain decoding and folding.
    /// Sizes include decoded payloads and their owned allocations.
    pub fn admit(
        &mut self,
        chain: &SharedMaterializedTurnChain,
        decoded: Vec<(Arc<dyn Any + Send + Sync>, usize)>,
    ) {
        let pending = std::mem::take(&mut self.pending_contents);
        let content_bytes = std::mem::take(&mut self.pending_bytes);
        if !self.admission_open
            || decoded.len() != chain.nodes.len()
            || chain.expires_at <= Instant::now()
            || chain.nodes.iter().any(|node| node.kind != self.kind)
        {
            return;
        }
        let additional = chain
            .nodes
            .iter()
            .zip(&decoded)
            .filter(|(node, _)| !self.nodes.contains_key(&node.id))
            .try_fold(0usize, |sum, (_, (_, size))| sum.checked_add(*size));
        let Some(additional) = additional else {
            self.admission_open = false;
            return;
        };
        let Some(additional) = additional.checked_add(content_bytes) else {
            self.admission_open = false;
            return;
        };
        if additional > self.budget.saturating_sub(self.bytes) {
            self.admission_open = false;
            return;
        }
        let mut prefix_expiry: Option<Instant> = None;
        for (node, (decoded, _)) in chain.nodes.iter().zip(decoded) {
            let retained = self
                .nodes
                .entry(node.id.clone())
                .and_modify(|retained| {
                    retained.expires_at = retained.expires_at.max(node.expires_at);
                })
                .or_insert_with(|| SharedTurnNode {
                    raw: None,
                    decoded: Some(decoded),
                    ..node.clone()
                });
            let expiry = prefix_expiry.map_or(retained.expires_at, |expiry| {
                expiry.min(retained.expires_at)
            });
            prefix_expiry = Some(expiry);
            self.validated_until.insert(node.id.clone(), expiry);
        }
        self.contents.extend(pending);
        self.bytes += additional;
    }

    /// Returns a complete validated ancestor chain, checking each node's own TTL.
    pub fn get(&self, id: &TurnNodeId) -> Option<SharedMaterializedTurnChain> {
        self.get_at(id, Instant::now())
    }

    /// Checks a complete proof at the read's monotonic completion boundary.
    pub fn get_at(&self, id: &TurnNodeId, now: Instant) -> Option<SharedMaterializedTurnChain> {
        let mut nodes = Vec::new();
        let mut next = Some(id);
        let mut expires_at = None;
        while let Some(id) = next {
            let node = self.nodes.get(id)?;
            if node.expires_at <= now || nodes.len() >= self.nodes.len() {
                return None;
            }
            expires_at = Some(expires_at.map_or(node.expires_at, |expiry: Instant| {
                expiry.min(node.expires_at)
            }));
            nodes.push(node.clone());
            next = node.parent_id.as_ref();
        }
        nodes.reverse();
        Some(SharedMaterializedTurnChain {
            nodes,
            expires_at: expires_at?,
        })
    }

    pub fn validated_ids(&self) -> Vec<String> {
        let now = Instant::now();
        self.validated_until
            .iter()
            .filter(|(_, expiry)| **expiry > now)
            .map(|(id, _)| id.as_str().to_owned())
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct TurnCommit {
    pub id: TurnNodeId,
    pub kind: TurnNodeKind,
    pub parent_id: Option<TurnNodeId>,
    pub principal: Principal,
    pub payload_version: u32,
    pub payload: Value,
    pub idle_ttl: Duration,
    pub reusable_prefix: Option<ReusablePrefixMetadata>,
}
/// Version of the optional controls/client-context exclusion proof. Unknown
/// versions must remain eligible for the authoritative history matcher.
pub const REUSABLE_PREFIX_PROJECTION_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReusablePrefixProjection {
    pub version: u32,
    pub controls_fingerprint: String,
    pub context_fingerprint: String,
}

/// Context fingerprints pair each request prefix with its canonical unit count,
/// not its item count. This proof can exclude candidates, never establish a hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReusablePrefixProjectionQuery {
    pub version: u32,
    pub controls_fingerprint: String,
    pub context_fingerprints: Vec<(String, u32)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReusablePrefixMetadata {
    pub namespace: String,
    pub fingerprint: String,
    pub item_count: u32,
    pub completed_at: i64,
    pub projection: Option<ReusablePrefixProjection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReusablePrefixQuery {
    pub namespace: String,
    pub fingerprints: Vec<(String, u32)>,
    /// Absent or unknown proofs leave all candidates eligible. Even a known
    /// proof only prefilters; callers must still perform exact history matching.
    pub projection: Option<ReusablePrefixProjectionQuery>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReusablePrefixCandidate {
    pub node_id: TurnNodeId,
    pub item_count: u32,
    pub completed_at: i64,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TurnUnavailable {
    #[error("turn unavailable")]
    Unavailable,
    #[error("turn storage failed: {0}")]
    Storage(String),
    #[error("{0}")]
    Decode(String),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TurnCommitError {
    #[error("turn parent unavailable")]
    ParentUnavailable,
    #[error("turn already exists")]
    AlreadyExists,
    #[error("turn storage failed: {0}")]
    Storage(String),
}

#[async_trait]
pub trait TurnChainStore: Send + Sync {
    async fn materialize(
        &self,
        principal: &Principal,
        kind: TurnNodeKind,
        id: &TurnNodeId,
    ) -> Result<Vec<TurnNode>, TurnUnavailable>;

    async fn materialize_with_expiry(
        &self,
        principal: &Principal,
        kind: TurnNodeKind,
        id: &TurnNodeId,
    ) -> Result<MaterializedTurnChain, TurnUnavailable> {
        Ok(MaterializedTurnChain {
            nodes: self.materialize(principal, kind, id).await?,
            // External adapters that have not adopted the richer contract
            // remain correct by declining to retain a materialization.
            expires_at: std::time::Instant::now(),
        })
    }

    async fn materialize_in_context(
        &self,
        principal: &Principal,
        kind: TurnNodeKind,
        id: &TurnNodeId,
        context: &mut TurnReadContext,
    ) -> Result<SharedMaterializedTurnChain, TurnUnavailable> {
        context.begin_read();
        if !context.matches(principal, kind) {
            return Err(TurnUnavailable::Unavailable);
        }
        if let Some(chain) = context.get(id) {
            return Ok(chain);
        }
        let chain = self.materialize_with_expiry(principal, kind, id).await?;
        Ok(SharedMaterializedTurnChain {
            expires_at: chain.expires_at,
            nodes: chain
                .nodes
                .into_iter()
                .map(|node| context.prepare_node(node, chain.expires_at))
                .collect::<Result<_, _>>()?,
        })
    }

    async fn commit(&self, commit: TurnCommit) -> Result<TurnNodeId, TurnCommitError>;

    async fn find_reusable_prefixes(
        &self,
        _principal: &Principal,
        _kind: TurnNodeKind,
        _query: &ReusablePrefixQuery,
    ) -> Result<Vec<ReusablePrefixCandidate>, TurnUnavailable> {
        Ok(Vec::new())
    }

    /// Rebuild derived prefix indexes for stored nodes whose namespace falls
    /// outside `namespace_prefix`. A change to projection semantics must bump
    /// that namespace generation so retained client history rebuilds the proof.
    /// Visit their complete, unexpired, principal-scoped ancestor closure once,
    /// parents before children. `decode` can retain derived parent state for
    /// incremental folding; payloads must not be retained by the store.
    /// Ancestors without an index have no completion time. Only stale indexes
    /// are changed: `None` clears the prefix and its optional projection.
    /// Unavailable ancestry clears its stale indexes without calling `decode`.
    /// A decode/storage failure rolls back all index changes in this invocation.
    /// Stores without derived prefix indexes keep the default no-op.
    async fn rebuild_prefixes(
        &self,
        namespace_prefix: &str,
        decode: &mut (
                 dyn FnMut(TurnNode, Option<i64>) -> Result<Option<ReusablePrefixMetadata>, String>
                     + Send
             ),
    ) -> Result<(), TurnUnavailable> {
        let _ = (namespace_prefix, decode);
        Ok(())
    }

    async fn sweep_expired(&self) -> Result<u64, TurnUnavailable>;
}

#[cfg(test)]
mod reference_tests {
    use super::*;

    #[test]
    fn turn_reference_uses_exact_stravia_uri() {
        let id = "a".repeat(crate::identifier::ID_LEN);
        let path = format!("stravia://turns/{id}");
        assert_eq!(TurnNodeId::from_reference(&path).unwrap().as_str(), id);
        assert_eq!(TurnNodeId::new(&id).reference(), path);

        for invalid in [
            id.clone(),
            format!("stravia://artifacts/{id}"),
            format!("stravia://turns/{id}/sources/1"),
            format!("stravia://turns/{id}/extra"),
            format!("stravia://turns/{id}?x=1"),
            format!("stravia://turns/{id}#fragment"),
            format!("stravia://turns/{}", "A".repeat(crate::identifier::ID_LEN)),
        ] {
            assert!(
                TurnNodeId::from_reference(&invalid).is_err(),
                "accepted {invalid}"
            );
        }
    }
}
