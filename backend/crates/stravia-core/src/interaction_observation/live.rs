use super::types::LiveContentBlock;
use std::{collections::HashMap, sync::Mutex};

const MAX_LIVE_BYTES: usize = 8 * 1024 * 1024;
#[derive(Default)]
struct Mirror {
    blocks: HashMap<String, (u64, LiveContentBlock)>,
    bytes: usize,
    next_order: u64,
}
#[derive(Default)]
pub(super) struct LiveState(Mutex<Mirror>);
impl LiveState {
    pub(super) fn snapshot(&self) -> Vec<LiveContentBlock> {
        let mirror = self.0.lock().expect("live observation mirror");
        let mut blocks: Vec<_> = mirror.blocks.values().collect();
        blocks.sort_unstable_by_key(|(order, _)| *order);
        blocks.into_iter().map(|(_, block)| block.clone()).collect()
    }
    pub(super) fn replace(&self, block: LiveContentBlock) -> bool {
        let mut mirror = self.0.lock().expect("live observation mirror");
        let previous = mirror
            .blocks
            .get(&block.block_id)
            .map_or(0, |(_, value)| value.text.len());
        let bytes = mirror.bytes - previous + block.text.len();
        if bytes > MAX_LIVE_BYTES {
            return false;
        }
        mirror.bytes = bytes;
        let order = if let Some((order, _)) = mirror.blocks.get(&block.block_id) {
            *order
        } else {
            let order = mirror.next_order;
            mirror.next_order += 1;
            order
        };
        mirror.blocks.insert(block.block_id.clone(), (order, block));
        true
    }
    pub(super) fn remove(&self, id: &str) {
        let mut mirror = self.0.lock().expect("live observation mirror");
        if let Some((_, block)) = mirror.blocks.remove(id) {
            mirror.bytes -= block.text.len();
        }
    }
}
