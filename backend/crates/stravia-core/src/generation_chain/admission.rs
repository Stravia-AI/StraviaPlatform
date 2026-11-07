use super::*;
use sha2::{Digest, Sha256};
use stravia_runtime_contract::protocol::ir::Role;
use stravia_runtime_contract::protocol::ir::canonical;

fn same_item_positions(left: &[AiItem], right: &[AiItem]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            canonical::history_items_equal(std::slice::from_ref(left), std::slice::from_ref(right))
        })
}

fn controls_digest(request: &AiRequest) -> String {
    let mut hash = Sha256::new();
    hash.update(b"stravia-prompt-admission-controls-v1\0");
    hash.update(canonical::history_request_controls_hash(request));
    canonical::hash_hex(&hash.finalize().into())
}

fn is_user_prompt(item: &AiItem) -> bool {
    if item.role != Role::User {
        return false;
    }
    match &item.content {
        MessageContent::Text(_) => true,
        MessageContent::Blocks(blocks) => blocks.iter().any(|block| {
            matches!(
                block,
                ContentBlock::Text { .. }
                    | ContentBlock::Image { .. }
                    | ContentBlock::Audio { .. }
                    | ContentBlock::File { .. }
                    | ContentBlock::Video { .. }
            )
        }),
    }
}

fn count_prompts(items: &[AiItem]) -> anyhow::Result<i64> {
    Ok(i64::try_from(
        items.iter().filter(|item| is_user_prompt(item)).count(),
    )?)
}

pub(super) fn compaction_prompt_counts(
    items: &[AiItem],
    window: Option<&std::ops::Range<usize>>,
) -> Option<(u64, u64)> {
    let included = |index: usize| !window.is_some_and(|range| range.contains(&index));
    let prompts = u64::try_from(
        items
            .iter()
            .enumerate()
            .filter(|(index, item)| included(*index) && is_user_prompt(item))
            .count(),
    )
    .ok()?;
    let turns = u64::try_from(
        items
            .iter()
            .enumerate()
            .filter(|(index, item)| {
                included(*index)
                    && is_user_prompt(item)
                    && !matches!(&item.content, MessageContent::Blocks(blocks)
                if blocks.iter().any(|block| matches!(block, ContentBlock::ToolResult { .. })))
            })
            .count(),
    )
    .ok()?;
    Some((prompts, turns))
}

fn count_user_turns(items: &[AiItem]) -> anyhow::Result<i64> {
    Ok(i64::try_from(
        items
            .iter()
            .filter(|item| {
                is_user_prompt(item)
                    && !matches!(&item.content, MessageContent::Blocks(blocks)
            if blocks.iter().any(|block| matches!(block, ContentBlock::ToolResult { .. })))
            })
            .count(),
    )?)
}

type HistoryProof = (i64, String, Vec<(i64, String)>);

// One fold per item, with only temporary prefix digests, never retained text or
// a cumulative per-admission vector. Prefixes end at client item boundaries.
fn history_proof<'a>(
    items: impl IntoIterator<Item = &'a AiItem>,
    collect_prefixes: bool,
) -> anyhow::Result<HistoryProof> {
    let mut hash = canonical::history_context_hash(&[]);
    let mut count = 0i64;
    let mut prefixes = Vec::new();
    for item in items {
        let (next, units) = canonical::append_history_context_hash_with_count(&hash, item);
        count = count
            .checked_add(i64::try_from(units)?)
            .ok_or_else(|| anyhow::anyhow!("history unit count overflow"))?;
        hash = next;
        if collect_prefixes {
            prefixes.push((count, canonical::hash_hex(&hash)));
        }
    }
    Ok((count, canonical::hash_hex(&hash), prefixes))
}

fn metadata_from_admission(entry: &crate::storage::PromptAdmission) -> Option<UserPromptMetadata> {
    let index = u64::try_from(entry.ordinal)
        .ok()
        .filter(|index| *index > 0)?;
    Some(UserPromptMetadata {
        user_prompt_index: index,
        user_turn_index: entry
            .user_turn_index
            .and_then(|index| u64::try_from(index).ok()),
        dialogue_prompt_index: entry
            .dialogue_prompt_index
            .and_then(|index| u64::try_from(index).ok()),
        context_window_id: entry.context_window_id,
        context_window_index: entry.context_window_index,
        user_prompt_id: entry.user_prompt_id.clone()?,
        user_prompt_created_at_ms: entry.user_prompt_created_at_ms?,
        client_conversation_id: entry.conversation_id.clone(),
        client_conversation_created_at_ms: entry.conversation_created_at_ms,
    })
}

impl GenerationChainWrite {
    pub(crate) fn logical_turn(&self) -> Option<LogicalTurn> {
        self.logical_turn
    }

    /// Construct once per ModelTurn before request transforms; retries reuse it.
    pub(crate) fn history_boundary_candidate(&self) -> Option<HistoryBoundaryCandidate> {
        let context = self.atomic_context.as_ref()?;
        history_boundary_candidate(
            context.admission_id,
            self.virtual_window?,
            &self.request,
            self.history_delta_start?,
        )
    }
    pub(crate) fn virtual_window(&self) -> Option<VirtualWindow> {
        self.virtual_window
    }
    pub(crate) fn atomic_conversation_context(
        &self,
    ) -> Option<crate::plugin::execution::AtomicConversationContext> {
        self.atomic_context.clone()
    }

    pub(crate) fn atomic_lease(&self) -> Option<crate::plugin::atomic_lease::AtomicLeaseGuard> {
        self.atomic_guard.clone()
    }

    pub(crate) fn user_prompt_metadata(&self) -> Option<UserPromptMetadata> {
        self.prompt_admission
            .as_ref()
            .and_then(metadata_from_admission)
    }

    #[cfg(test)]
    pub(crate) fn user_prompt_index(&self) -> Option<u64> {
        self.prompt_admission
            .as_ref()
            .and_then(|entry| u64::try_from(entry.ordinal).ok())
            .filter(|index| *index > 0)
    }

    /// Called after principal admission and before the first Model Turn.
    pub(crate) async fn admit_user_prompt(&mut self) -> anyhow::Result<()> {
        if self.prompt_admission.is_some()
            || self.atomic_context.is_some()
            || self.virtual_window.is_some()
        {
            return Ok(());
        }
        let Some(store) = &self.chain.prompt_admissions else {
            return Ok(());
        };
        let _memory_history_guard = match store.memory_history_lock() {
            Some(lock) => Some(lock.read_owned().await),
            None => None,
        };
        let history_epoch = store.history_epoch().await?;
        let principal = self.principal.continuation_key();
        let controls = controls_digest(&self.admission_input);
        let explicit = self
            .admission_input
            .parameters
            .continuation
            .as_ref()
            .and_then(|c| c.previous_response_id())
            .is_some();
        let (history_count, prefix_digest, mut prefixes) = if explicit {
            history_proof(
                self.parent
                    .parent_client_items
                    .iter()
                    .map(|item| item.as_ref())
                    .chain(self.request_delta.items.iter()),
                false,
            )?
        } else {
            history_proof(
                self.admission_input.items.iter(),
                self.parent.parent_id.is_none(),
            )?
        };
        let has_proven_parent = self.parent.parent_id.is_some()
            || self.parent.parent_prompt_metadata.is_some()
            || self.parent.parent_logical_turn.is_some();
        let isolated = has_proven_parent
            && ((self.parent.parent_prompt_metadata.is_none()
                && self.parent.parent_logical_turn.is_none())
                || (self.chain.atomic_state.is_some()
                    && self.parent.parent_atomic_checkpoint_id.is_none())
                || (self.parent.ordinal_delta_unproven
                    && count_prompts(&self.request_delta.items)? != 0))
            || (!has_proven_parent && self.parent.compaction_input_range.is_some());
        let mut prefix_covers_boundary = false;
        let mut failed_prefix_start = None;
        let mut ambiguous_prefix = false;
        let (parent, base_metadata, increment, turn_increment) = if isolated {
            let items = &self.admission_input.items;
            let (before, after) = match self.parent.compaction_input_range.as_ref() {
                Some(range) if range.start <= range.end && range.end <= items.len() => {
                    (&items[..range.start], &items[range.end..])
                }
                Some(_) => anyhow::bail!("invalid compaction source window"),
                None => (items.as_slice(), &[][..]),
            };
            let prompts = count_prompts(before)?
                .checked_add(count_prompts(after)?)
                .ok_or_else(|| anyhow::anyhow!("prompt ordinal overflow"))?;
            let turns = count_user_turns(before)?
                .checked_add(count_user_turns(after)?)
                .ok_or_else(|| anyhow::anyhow!("user turn ordinal overflow"))?;
            (None, None, prompts, turns)
        } else if has_proven_parent {
            let increment = count_prompts(&self.request_delta.items)?;
            let turn_increment = count_user_turns(&self.request_delta.items)?;
            let metadata = self.parent.parent_prompt_metadata.clone();
            let parent = match self.parent.parent_prompt_admission_id.as_deref() {
                Some(id) => store.get(&principal, id).await?,
                None => None,
            };
            if parent
                .as_ref()
                .is_some_and(|entry| metadata_from_admission(entry) != metadata)
            {
                anyhow::bail!("generation parent admission metadata mismatch");
            }
            if let (Some(entry), Some(turn)) = (parent.as_ref(), self.parent.parent_logical_turn)
                && (entry.logical_turn_id != turn.id.to_string()
                    || entry.logical_turn_created_at_ms != turn.created_at_ms)
            {
                anyhow::bail!("generation parent logical turn mismatch");
            }
            // A legitimately swept admission is reanchored by the still-live
            // immutable Generation node, never by a truncated client window.
            (parent, metadata, increment, turn_increment)
        } else {
            // Exact input repeats are independent admissions, not inferred retries.
            prefixes.retain(|(count, _)| *count < history_count);
            let mut candidate: Option<crate::storage::PromptAdmission> = None;
            for entry in store
                .find_prefixes(&principal, &controls, &prefixes)
                .await?
            {
                match &candidate {
                    Some(old) if old.history_count > entry.history_count => continue,
                    Some(old) if old.history_count == entry.history_count => {
                        ambiguous_prefix = true;
                    }
                    _ => {
                        candidate = Some(entry);
                        ambiguous_prefix = false;
                    }
                }
            }
            if ambiguous_prefix {
                // Neither sibling proves ancestry. Do not borrow its state or
                // infer a position in either lineage from the visible history.
                candidate = None;
            }
            if self.chain.atomic_state.is_some()
                && candidate
                    .as_ref()
                    .is_some_and(|entry| !entry.atomic_lineage_created)
            {
                // Pre-atomic admissions cannot prove conversation-state ancestry.
                // Start a new virtual lineage, rather than retaining their identity.
                candidate = None;
            }
            let item_start = if let Some(entry) = candidate.as_ref() {
                prefixes
                    .iter()
                    .position(|(count, digest)| {
                        *count == entry.history_count && *digest == entry.prefix_digest
                    })
                    .map(|index| index + 1)
                    .ok_or_else(|| {
                        anyhow::anyhow!("prompt admission prefix is not an item boundary")
                    })?
            } else {
                0
            };
            failed_prefix_start = Some(item_start);
            prefix_covers_boundary = self
                .parent
                .compaction_input_range
                .as_ref()
                .is_some_and(|range| item_start >= range.end);
            let metadata = candidate.as_ref().and_then(metadata_from_admission);
            if candidate.as_ref().is_some_and(|entry| entry.ordinal > 0) && metadata.is_none() {
                anyhow::bail!("invalid prompt admission ordinal");
            }
            let delta = &self.admission_input.items[item_start..];
            (
                candidate,
                metadata,
                count_prompts(delta)?,
                count_user_turns(delta)?,
            )
        };
        // A zero ordinal with absent turn counters is the durable representation of
        // an unproven ordinal, distinct from a proven history with no prompts.
        // A compacted suffix cannot establish how many prompts preceded it.
        let ordinal_unknown = ambiguous_prefix
            || (isolated
                && (explicit
                    || self.parent.compaction_input_range.is_some()
                    || self.parent.ordinal_delta_unproven))
            || (base_metadata.is_none()
                && !self.parent.parent_prompt_zero_proven
                && parent
                    .as_ref()
                    .is_none_or(|entry| entry.user_turn_index.is_none())
                && self.admission_input.items.iter().any(AiItem::is_compaction))
            || parent
                .as_ref()
                .is_some_and(|entry| entry.ordinal == 0 && entry.user_turn_index.is_none())
            || (has_proven_parent
                && !isolated
                && base_metadata.is_none()
                && parent.is_none()
                && !self.parent.parent_prompt_zero_proven);
        let base_ordinal = base_metadata
            .as_ref()
            .map_or(Ok(0), |metadata| i64::try_from(metadata.user_prompt_index))?;
        let ordinal = if ordinal_unknown {
            0
        } else {
            base_ordinal
                .checked_add(increment)
                .ok_or_else(|| anyhow::anyhow!("prompt ordinal overflow"))?
        };
        let inherited_window = self
            .parent
            .parent_virtual_window
            .or_else(|| {
                parent.as_ref().and_then(|entry| {
                    Some(VirtualWindow {
                        id: entry.context_window_id?,
                        index: entry.context_window_index?,
                    })
                })
            })
            .or_else(|| {
                base_metadata.as_ref().and_then(|metadata| {
                    Some(VirtualWindow {
                        id: metadata.context_window_id?,
                        index: metadata.context_window_index?,
                    })
                })
            });
        let window = if self.window_proof_missing
            || (has_proven_parent
                && self.chain.atomic_state.is_some()
                && self.parent.parent_atomic_checkpoint_id.is_none())
        {
            VirtualWindow::root()
        } else if let Some(boundary) = self
            .pending_window_boundary
            .as_ref()
            .filter(|_| !prefix_covers_boundary)
        {
            match boundary.source_window {
                Some(source) => {
                    if inherited_window.is_some_and(|parent| parent != source) {
                        anyhow::bail!(
                            "compaction source window conflicts with generation ancestry"
                        );
                    }
                    source.install(&self.principal, boundary)?
                }
                None => VirtualWindow::root(),
            }
        } else {
            inherited_window.unwrap_or_else(VirtualWindow::root)
        };
        let admission_id = uuid::Uuid::new_v4();
        let id = admission_id.to_string();
        let created_at_ms = chrono::Utc::now().timestamp_millis();
        let inherited_turn = if isolated {
            None
        } else if let Some(turn) = self.parent.parent_logical_turn {
            Some(turn)
        } else if let Some(entry) = parent.as_ref() {
            Some(LogicalTurn {
                id: uuid::Uuid::parse_str(&entry.logical_turn_id)?,
                created_at_ms: entry.logical_turn_created_at_ms,
            })
        } else if let Some(metadata) = base_metadata.as_ref() {
            Some(LogicalTurn {
                id: uuid::Uuid::parse_str(&metadata.user_prompt_id)?,
                created_at_ms: metadata.user_prompt_created_at_ms,
            })
        } else {
            None
        };
        let logical_turn = if increment != 0 {
            LogicalTurn {
                id: admission_id,
                created_at_ms,
            }
        } else {
            inherited_turn.unwrap_or(LogicalTurn {
                id: admission_id,
                created_at_ms,
            })
        };
        let (conversation_id, conversation_created_at_ms) = base_metadata
            .as_ref()
            .map(|metadata| {
                (
                    metadata.client_conversation_id.clone(),
                    metadata.client_conversation_created_at_ms,
                )
            })
            .or_else(|| {
                parent.as_ref().map(|entry| {
                    (
                        entry.conversation_id.clone(),
                        entry.conversation_created_at_ms,
                    )
                })
            })
            .or_else(|| inherited_turn.map(|turn| (turn.id.to_string(), turn.created_at_ms)))
            .unwrap_or_else(|| (id.clone(), created_at_ms));
        let (user_prompt_id, user_prompt_created_at_ms) = if ordinal == 0 {
            (None, None)
        } else if increment == 0 {
            let metadata = base_metadata
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("missing prior user prompt identity"))?;
            (
                Some(metadata.user_prompt_id.clone()),
                Some(metadata.user_prompt_created_at_ms),
            )
        } else {
            (Some(id.clone()), Some(created_at_ms))
        };
        let (atomic_context, atomic_guard) = if let Some(state) = &self.chain.atomic_state {
            let mut checkpoint = self.parent.parent_atomic_checkpoint_id;
            let mut parent_guard = None;
            if checkpoint.is_none()
                && self.parent.parent_id.is_none()
                && !isolated
                && let Some(parent_admission) = parent.as_ref()
            {
                // Capture failed admitted state under a separate fenced lease.
                let parent_id = uuid::Uuid::parse_str(&parent_admission.id)?;
                let guard = crate::plugin::atomic_lease::AtomicLeaseGuard::acquire_at_epoch(
                    state.clone(),
                    principal.clone(),
                    parent_id,
                    history_epoch,
                )
                .await?;
                let snapshot = uuid::Uuid::new_v4();
                guard.checkpoint(snapshot).await?;
                checkpoint = Some(snapshot);
                parent_guard = Some(guard);
            }
            let guard = crate::plugin::atomic_lease::AtomicLeaseGuard::fork_at_epoch(
                state.clone(),
                principal.clone(),
                admission_id,
                checkpoint,
                history_epoch,
            )
            .await?;
            drop(parent_guard);
            let context = crate::plugin::execution::AtomicConversationContext {
                principal: principal.clone(),
                admission_id,
                atomic_lease_id: guard.lease_id(),
            };
            (Some(context), Some(guard))
        } else {
            (None, None)
        };
        let admission = crate::storage::PromptAdmission {
            context_window_id: Some(window.id),
            context_window_index: Some(window.index),
            atomic_lineage_created: atomic_guard.is_some(),
            logical_turn_id: logical_turn.id.to_string(),
            logical_turn_created_at_ms: logical_turn.created_at_ms,
            id,
            created_at_ms,
            conversation_id,
            conversation_created_at_ms,
            user_prompt_id,
            user_prompt_created_at_ms,
            // This counts host-declared user turns in the virtual lineage,
            // not the external client's native turnPosition.
            user_turn_index: if ordinal_unknown {
                None
            } else {
                match &base_metadata {
                    Some(metadata) => metadata.user_turn_index.map(i64::try_from).transpose()?,
                    None => parent
                        .as_ref()
                        .map_or(Some(0), |entry| entry.user_turn_index),
                }
                .map(|base| {
                    base.checked_add(turn_increment)
                        .ok_or_else(|| anyhow::anyhow!("user turn ordinal overflow"))
                })
                .transpose()?
            },
            dialogue_prompt_index: if ordinal_unknown {
                None
            } else {
                match &base_metadata {
                    Some(metadata) => metadata
                        .dialogue_prompt_index
                        .map(i64::try_from)
                        .transpose()?,
                    None => parent
                        .as_ref()
                        .map_or(Some(0), |entry| entry.dialogue_prompt_index),
                }
                .map(|base| {
                    base.checked_add(turn_increment)
                        .ok_or_else(|| anyhow::anyhow!("dialogue prompt ordinal overflow"))
                })
                .transpose()?
            },
            principal,
            // Equal zero ordinals normally require equal logical turns; unknown
            // ordinals do not prove that two distinct turns have equal positions.
            parent_id: if ordinal_unknown {
                None
            } else {
                parent.map(|entry| entry.id)
            },
            ordinal,
            history_count,
            prefix_digest,
            controls_digest: controls,
        };
        store.insert_at_epoch(&admission, history_epoch).await?;
        self.history_delta_start = if let Some(start) = failed_prefix_start.filter(|_| {
            self.parent.parent_id.is_none()
                && self.parent.compaction_input_range.is_none()
                && self.request.items.len() == self.admission_input.items.len()
                && same_item_positions(&self.request.items, &self.admission_input.items)
        }) {
            Some(start)
        } else {
            self.request
                .items
                .len()
                .checked_sub(self.request_delta.items.len())
                .filter(|start| {
                    same_item_positions(&self.request.items[*start..], &self.request_delta.items)
                })
        };
        self.atomic_context = atomic_context;
        self.atomic_guard = atomic_guard;
        self.prompt_admission = Some(admission);
        self.logical_turn = Some(logical_turn);
        self.virtual_window = Some(window);
        Ok(())
    }
}
