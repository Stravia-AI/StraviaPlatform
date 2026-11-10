//! Virtual context windows are derived only from validated compaction proofs.
//! They identify Stravia's local lineage, not a physical client session.
use std::io::{self, Write};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use stravia_runtime_contract::Principal;
use stravia_runtime_contract::protocol::ir::canonical;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SourceHistoryProof {
    pub hash: [u8; 32],
    pub units: u64,
    pub items: u64,
}

/// Fingerprint the complete effective source before redaction, excluding only
/// the operation trigger. No source prompt content is persisted in this proof.
pub(crate) fn source_history_proof(
    request: &stravia_runtime_contract::protocol::ir::AiRequest,
) -> SourceHistoryProof {
    let request = super::canonical_client_history_request(request);
    let mut hash = canonical::history_context_hash(&[]);
    let mut units = 0u64;
    let mut items = 0u64;
    for item in request
        .items
        .iter()
        .filter(|item| !item.is_compaction_trigger())
    {
        let (next, count) = canonical::append_history_context_hash_with_count(&hash, item);
        hash = next;
        units += count as u64;
        items += 1;
    }
    SourceHistoryProof { hash, units, items }
}

/// Match only a source anchored before the replacement boundary. Content
/// after that boundary is client delta even when its bytes equal old content.
pub(super) fn source_prefix_before_boundary(
    request: &stravia_runtime_contract::protocol::ir::AiRequest,
    boundary_start: usize,
    proof: SourceHistoryProof,
) -> Option<std::ops::Range<usize>> {
    let canonical = super::canonical_client_history_request(request);
    let leading = request.items.len() - canonical.items.len();
    let end = boundary_start
        .saturating_sub(leading)
        .min(canonical.items.len());
    let mut hash = canonical::history_context_hash(&[]);
    let mut units = 0u64;
    let mut items = 0u64;
    let mut source_end = 0;
    for (index, item) in canonical.items[..end].iter().enumerate() {
        if items == proof.items {
            break;
        }
        if item.is_compaction_trigger() {
            continue;
        }
        let (next, count) = canonical::append_history_context_hash_with_count(&hash, item);
        hash = next;
        units += count as u64;
        items += 1;
        source_end = index + 1;
    }
    (items == proof.items && units == proof.units && hash == proof.hash)
        .then_some(leading..leading + source_end)
}

pub(crate) fn history_boundary_candidate(
    admission_id: uuid::Uuid,
    window: VirtualWindow,
    request: &stravia_runtime_contract::protocol::ir::AiRequest,
    item_gap: usize,
) -> Option<super::HistoryBoundaryCandidate> {
    let prefix = request.items.get(..item_gap)?;
    let (source_prefix_hash, units) = canonical::history_context_hash_with_count(prefix);
    Some(super::HistoryBoundaryCandidate {
        id: uuid::Uuid::new_v4(),
        window_id: window.id,
        source_admission_id: admission_id,
        source_item_gap: u64::try_from(item_gap).ok()?,
        source_unit_gap: u64::try_from(units).ok()?,
        source_prefix_hash,
        source_controls_hash: canonical::history_request_controls_hash(request),
    })
}

impl super::HistoryBoundaryCandidate {
    /// 保留宿主分配的准入身份，只把已证明的位置投影到实际执行视图。
    /// 保存执行后的前缀证明，后续恢复不会依赖重新执行旧 Hook。
    pub(crate) fn at_projected_gap(
        &self,
        request: &stravia_runtime_contract::protocol::ir::AiRequest,
        item_gap: usize,
    ) -> Option<Self> {
        let prefix = request.items.get(..item_gap)?;
        let (source_prefix_hash, units) = canonical::history_context_hash_with_count(prefix);
        Some(Self {
            source_item_gap: u64::try_from(item_gap).ok()?,
            source_unit_gap: u64::try_from(units).ok()?,
            source_prefix_hash,
            source_controls_hash: canonical::history_request_controls_hash(request),
            ..self.clone()
        })
    }
}

/// Project trusted, checkpoint-inherited source gaps onto an exact canonical
/// request. Unit offsets select a prefix, not a search for equal message text;
/// the immutable prefix proof validates that the source ancestry survived.
/// Unrepresentable intra-item or ambiguous zero-unit gaps remain unavailable.
pub(crate) fn project_history_boundaries(
    request: &stravia_runtime_contract::protocol::ir::AiRequest,
    current: &super::HistoryBoundaryCandidate,
    retained: &[super::HistoryBoundaryCandidate],
) -> Option<crate::model_turn::HistoryBoundaryMap> {
    let current_gap = usize::try_from(current.source_item_gap).ok()?;
    let mut prefixes = Vec::with_capacity(request.items.len() + 1);
    let mut digest = canonical::history_context_hash(&[]);
    let mut unit_count = 0u64;
    prefixes.push((unit_count, digest));
    for item in &request.items {
        let (next, units) = canonical::append_history_context_hash_with_count(&digest, item);
        digest = next;
        unit_count = unit_count.checked_add(u64::try_from(units).ok()?)?;
        prefixes.push((unit_count, digest));
    }
    if *prefixes.get(current_gap)? != (current.source_unit_gap, current.source_prefix_hash)
        || canonical::history_request_controls_hash(request) != current.source_controls_hash
    {
        return None;
    }
    let mut boundaries = Vec::with_capacity(retained.len() + 1);
    for fact in retained
        .iter()
        .filter(|fact| fact.window_id == current.window_id)
    {
        if fact.id == current.id {
            continue;
        }
        let start = prefixes.partition_point(|(units, _)| *units < fact.source_unit_gap);
        let end = prefixes.partition_point(|(units, _)| *units <= fact.source_unit_gap);
        if end != start + 1 || prefixes.get(start)?.1 != fact.source_prefix_hash {
            return None;
        }
        boundaries.push(crate::model_turn::HistoryBoundary {
            id: fact.id.to_string(),
            item_index: start,
        });
    }
    boundaries.push(crate::model_turn::HistoryBoundary {
        id: current.id.to_string(),
        item_index: current_gap,
    });
    boundaries.sort_by_key(|boundary| boundary.item_index);
    Some(crate::model_turn::HistoryBoundaryMap {
        version: 1,
        window_id: current.window_id.to_string(),
        coverage: "complete".into(),
        boundaries,
        current_boundary_id: current.id.to_string(),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct VirtualWindow {
    pub id: uuid::Uuid,
    pub index: u64,
}

struct HashWriter(Sha256);
impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl VirtualWindow {
    pub(crate) fn root() -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            index: 0,
        }
    }

    /// Derive one virtual replacement identity from the entire verified
    /// boundary proof. Equivalent registrations deliberately yield one ID;
    /// record IDs, arrival order and provider session guesses are not inputs.
    pub(crate) fn install(
        self,
        principal: &Principal,
        boundary: &crate::compaction::ResolvedCompaction,
    ) -> anyhow::Result<Self> {
        if boundary.source_window != Some(self) {
            anyhow::bail!("compaction source window does not match current window");
        }
        if boundary.source_history.is_none() {
            anyhow::bail!("compaction source history proof is unavailable");
        }
        let index = self
            .index
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("context window index overflow"))?;
        let (window_hash, unit_count) =
            canonical::history_context_hash_with_count(&boundary.window);
        let mut writer = HashWriter(Sha256::new());
        writer.0.update(b"stravia-virtual-context-window-v1\0");
        serde_json::to_writer(
            &mut writer,
            &(
                principal.continuation_key(),
                self,
                &boundary.source_generation_id,
                boundary.source_history,
                &boundary.target,
                window_hash,
                unit_count,
            ),
        )?;
        let hash = writer.0.finalize();
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&hash[..16]);
        // RFC 9562 UUIDv8: application-defined SHA-256-based identity.
        bytes[6] = (bytes[6] & 0x0f) | 0x80;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Ok(Self {
            id: uuid::Uuid::from_bytes(bytes),
            index,
        })
    }
}
