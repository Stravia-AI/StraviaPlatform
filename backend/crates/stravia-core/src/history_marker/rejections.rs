//! 受保护推理的进程内拒绝记忆（ADR-0080）：已知来源按上游对记忆，未知来源按载荷摘要记忆。
//! 不存载荷原文；容量达到上限时淘汰最早写入的键。

use super::ThinkingProvenance;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use stravia_runtime_contract::protocol::ir::ContentBlock;
use stravia_runtime_contract::protocol::ir::canonical::hash_bytes;
pub(super) use stravia_runtime_contract::protocol::ir::canonical::hash_hex as digest_hex;

const DEFAULT_CAPACITY: usize = 4_096;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum RejectedReasoning {
    Origin(String),
    Payload([u8; 32]),
}

#[derive(Clone, Default)]
pub(crate) struct ReasoningRejections {
    index: Arc<Mutex<RejectionIndex>>,
}

struct RejectionIndex {
    capacity: usize,
    order: VecDeque<[u8; 32]>,
    keys: HashSet<[u8; 32]>,
}

impl Default for RejectionIndex {
    fn default() -> Self {
        Self {
            capacity: DEFAULT_CAPACITY,
            order: VecDeque::new(),
            keys: HashSet::new(),
        }
    }
}

impl RejectedReasoning {
    pub(crate) fn for_payload(
        provenance: ThinkingProvenance,
        block: &ContentBlock,
    ) -> Option<Self> {
        match provenance {
            ThinkingProvenance::Own => None,
            ThinkingProvenance::Foreign(origin) => Some(Self::Origin(origin)),
            ThinkingProvenance::Unknown => protected_payload_digest(block).map(Self::Payload),
        }
    }
}

impl ReasoningRejections {
    pub(crate) fn rejected(
        &self,
        authority: &str,
        provenance: ThinkingProvenance,
        block: &ContentBlock,
    ) -> bool {
        RejectedReasoning::for_payload(provenance, block)
            .is_some_and(|rejected| self.index.lock().keys.contains(&key(authority, &rejected)))
    }

    pub(crate) fn record(
        &self,
        authority: &str,
        rejected: impl IntoIterator<Item = RejectedReasoning>,
    ) {
        let mut index = self.index.lock();
        for rejected in rejected {
            let key = key(authority, &rejected);
            if !index.keys.insert(key) {
                continue;
            }
            index.order.push_back(key);
            while index.order.len() > index.capacity {
                if let Some(evicted) = index.order.pop_front() {
                    index.keys.remove(&evicted);
                }
            }
        }
    }
}

/// 与来源清单复用同一摘要算法；空字符串没有可回放的受保护载荷。
/// Stravia 签发的 marker 令牌只是指向私有历史的指针，不是上游签发的载荷。
pub(crate) fn protected_payload_digest(block: &ContentBlock) -> Option<[u8; 32]> {
    let payload = match block {
        ContentBlock::Thinking {
            signature: Some(payload),
            ..
        }
        | ContentBlock::Reasoning {
            encrypted_content: Some(payload),
            ..
        }
        | ContentBlock::RedactedThinking { data: payload } => payload,
        _ => return None,
    };
    (!payload.is_empty() && !super::is_history_marker_token(payload))
        .then(|| hash_bytes(payload.as_bytes()))
}

/// 域分隔区分来源键与载荷键；authority 使用长度前缀防止字符串拼接歧义。
fn key(authority: &str, rejected: &RejectedReasoning) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update((authority.len() as u64).to_be_bytes());
    digest.update(authority.as_bytes());
    match rejected {
        RejectedReasoning::Origin(origin) => {
            digest.update([0]);
            digest.update(origin.as_bytes());
        }
        RejectedReasoning::Payload(payload) => {
            digest.update([1]);
            digest.update(payload);
        }
    }
    digest.finalize().into()
}
