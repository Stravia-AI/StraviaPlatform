//! Thinking Replay 的拒绝记忆（ADR-0075）：记住哪些受保护载荷已被哪个签发作用域拒绝，
//! 此后回放给同一作用域时提前剥离，避免每轮都重复“被拒—恢复”。
//!
//! 这是可丢失的纯性能状态：只存（签发作用域, 载荷 SHA-256）的组合摘要，不存原文；
//! 不按 Principal 隔离——密文不可猜测，被记录的后果只是对该作用域剥离。

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use parking_lot::Mutex;

use stravia_runtime_contract::protocol::ir::canonical::hash_bytes;
use stravia_runtime_contract::protocol::ir::{AiItem, ContentBlock, MessageContent};

/// 需要覆盖“少量签发作用域 × 每段历史少量签名”；满时淘汰最早写入的条目。
const DEFAULT_CAPACITY: usize = 4_096;

type PayloadDigest = [u8; 32];

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

impl ReasoningRejections {
    /// 条目中任一受保护载荷已被 `authority` 拒绝过。
    pub(crate) fn rejected(&self, authority: &str, item: &AiItem) -> bool {
        let mut digests = item_payload_digests(item).peekable();
        if digests.peek().is_none() {
            return false;
        }
        let index = self.index.lock();
        digests.any(|digest| index.keys.contains(&key(authority, &digest)))
    }

    pub(crate) fn record(&self, authority: &str, digests: impl IntoIterator<Item = PayloadDigest>) {
        let mut index = self.index.lock();
        for digest in digests {
            let key = key(authority, &digest);
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

/// 条目中所有受保护载荷（签名、`encrypted_content`、redacted data）的 SHA-256。
pub(crate) fn protected_payload_digests(items: &[AiItem]) -> HashSet<PayloadDigest> {
    items.iter().flat_map(item_payload_digests).collect()
}

fn item_payload_digests(item: &AiItem) -> impl Iterator<Item = PayloadDigest> + '_ {
    let blocks = match &item.content {
        MessageContent::Blocks(blocks) => blocks.as_slice(),
        MessageContent::Text(_) => &[],
    };
    blocks.iter().filter_map(|block| {
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
        Some(hash_bytes(payload.as_bytes()))
    })
}

/// 摘要定长且位于末尾，拼接无歧义。
fn key(authority: &str, digest: &PayloadDigest) -> [u8; 32] {
    let mut bytes = Vec::with_capacity(authority.len() + digest.len());
    bytes.extend_from_slice(authority.as_bytes());
    bytes.extend_from_slice(digest);
    hash_bytes(&bytes)
}
