use super::types::{LiveContentBlock, ObservationStream, ObservationUpdate};
use parking_lot::Mutex;
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Weak},
};
use tokio::sync::watch;

const MAX_LIVE_BYTES: usize = 8 * 1024 * 1024;
#[derive(Default)]
struct Mirror {
    blocks: HashMap<String, (u64, LiveContentBlock)>,
    bytes: usize,
    next_order: u64,
    subscribers: HashMap<String, Vec<Weak<Mailbox>>>,
    gaps: HashMap<(String, String), String>,
}
struct Mailbox {
    pending: Mutex<VecDeque<ObservationUpdate>>,
    signal: watch::Sender<u64>,
}
impl Mailbox {
    fn push(&self, update: ObservationUpdate) {
        let mut pending = self.pending.lock();
        if let ObservationUpdate::LiveContent(block) = &update {
            let boundary = pending
                .iter()
                .rposition(|value| {
                    matches!(value,
                        ObservationUpdate::LiveFinished { run_id, .. } if run_id == &block.run_id
                    )
                })
                .map_or(0, |index| index + 1);
            if let Some(index) = pending.iter().enumerate().skip(boundary).find_map(|(index, value)|
                matches!(value, ObservationUpdate::LiveContent(old) if old.block_id == block.block_id).then_some(index)
            ) {
                let other_bytes: usize = pending.iter().enumerate().filter(|(at, _)| *at != index)
                    .map(|(_, value)| update_bytes(value)).sum();
                if other_bytes.saturating_add(block.text.len()) <= MAX_LIVE_BYTES {
                    pending[index] = update;
                    self.signal.send_modify(|value| *value += 1);
                    return;
                }
                pending.remove(index);
            }
        }
        let pending_bytes: usize = pending.iter().map(update_bytes).sum();
        let incoming_bytes = update_bytes(&update);
        if pending.len() >= 256 || pending_bytes.saturating_add(incoming_bytes) > MAX_LIVE_BYTES {
            pending.clear();
            let interaction_id = match &update {
                ObservationUpdate::LiveContent(block) => block.interaction_id.clone(),
                ObservationUpdate::LiveGap { interaction_id, .. }
                | ObservationUpdate::LiveFinished { interaction_id, .. } => interaction_id.clone(),
                _ => String::new(),
            };
            pending.push_back(ObservationUpdate::LiveGap {
                interaction_id,
                run_id: String::new(),
                reason: "live_capacity".into(),
            });
        }
        pending.push_back(update);
        self.signal.send_modify(|value| *value += 1);
    }
}
fn update_bytes(update: &ObservationUpdate) -> usize {
    match update {
        ObservationUpdate::LiveContent(block) => block.text.len(),
        ObservationUpdate::LiveSnapshot { blocks } => {
            blocks.iter().map(|block| block.text.len()).sum()
        }
        _ => 0,
    }
}
#[derive(Default)]
pub(super) struct LiveState(Mutex<Mirror>);
impl LiveState {
    pub(super) fn subscribe(&self, interaction: String) -> ObservationStream {
        let (signal, receiver) = watch::channel(0);
        let mailbox = Arc::new(Mailbox {
            pending: Mutex::new(VecDeque::new()),
            signal,
        });
        {
            let mut mirror = self.0.lock();
            mirror.subscribers.retain(|_, subscribers| {
                subscribers.retain(|weak| weak.strong_count() > 0);
                !subscribers.is_empty()
            });
            mailbox
                .pending
                .lock()
                .push_back(ObservationUpdate::LiveSnapshot {
                    blocks: ordered(&mirror, Some(&interaction)),
                });
            let mut gaps: Vec<_> = mirror
                .gaps
                .iter()
                .filter(|((id, _), _)| id == &interaction)
                .collect();
            gaps.sort_unstable_by(|((_, left), _), ((_, right), _)| left.cmp(right));
            for ((interaction_id, run_id), reason) in gaps {
                mailbox
                    .pending
                    .lock()
                    .push_back(ObservationUpdate::LiveGap {
                        interaction_id: interaction_id.clone(),
                        run_id: run_id.clone(),
                        reason: reason.clone(),
                    });
            }
            mirror
                .subscribers
                .entry(interaction)
                .or_default()
                .push(Arc::downgrade(&mailbox));
        }
        Box::pin(futures::stream::unfold(
            (mailbox, receiver),
            |(mailbox, mut receiver)| async move {
                loop {
                    receiver.borrow_and_update();
                    let next = mailbox.pending.lock().pop_front();
                    if let Some(update) = next {
                        return Some((update, (mailbox, receiver)));
                    }
                    if receiver.changed().await.is_err() {
                        return None;
                    }
                }
            },
        ))
    }
    pub(super) fn emit(&self, update: ObservationUpdate) {
        let interaction = match &update {
            ObservationUpdate::LiveContent(block) => &block.interaction_id,
            ObservationUpdate::LiveGap { interaction_id, .. }
            | ObservationUpdate::LiveFinished { interaction_id, .. } => interaction_id,
            ObservationUpdate::Event(_)
            | ObservationUpdate::Change(_)
            | ObservationUpdate::ResetRequired { .. }
            | ObservationUpdate::LiveSnapshot { .. } => return,
        };
        let mut mirror = self.0.lock();
        match &update {
            ObservationUpdate::LiveGap {
                interaction_id,
                run_id,
                reason,
            } => {
                mirror
                    .gaps
                    .insert((interaction_id.clone(), run_id.clone()), reason.clone());
            }
            ObservationUpdate::LiveFinished {
                interaction_id,
                run_id,
            } => {
                mirror
                    .gaps
                    .remove(&(interaction_id.clone(), run_id.clone()));
            }
            ObservationUpdate::LiveContent(_)
            | ObservationUpdate::Event(_)
            | ObservationUpdate::Change(_)
            | ObservationUpdate::ResetRequired { .. }
            | ObservationUpdate::LiveSnapshot { .. } => {}
        }
        if let Some(subscribers) = mirror.subscribers.get_mut(interaction) {
            subscribers.retain(|weak| {
                if let Some(mailbox) = weak.upgrade() {
                    mailbox.push(update.clone());
                    true
                } else {
                    false
                }
            });
        }
    }
    pub(super) fn publish(&self, id: &str) {
        let mirror = self.0.lock();
        let Some((_, block)) = mirror.blocks.get(id) else {
            return;
        };
        if let Some(subscribers) = mirror.subscribers.get(&block.interaction_id) {
            for mailbox in subscribers.iter().filter_map(Weak::upgrade) {
                mailbox.push(ObservationUpdate::LiveContent(block.clone()));
            }
        }
    }
    pub(super) fn invalidate(&self, interactions: &[String]) {
        let mut mirror = self.0.lock();
        mirror
            .blocks
            .retain(|_, (_, block)| !interactions.contains(&block.interaction_id));
        mirror
            .gaps
            .retain(|(interaction, _), _| !interactions.contains(interaction));
        mirror.bytes = mirror
            .blocks
            .values()
            .map(|(_, block)| block.text.len())
            .sum();
        for interaction in interactions {
            if let Some(subscribers) = mirror.subscribers.get(interaction) {
                for mailbox in subscribers.iter().filter_map(Weak::upgrade) {
                    mailbox.pending.lock().clear();
                    mailbox.push(ObservationUpdate::LiveSnapshot { blocks: Vec::new() });
                    mailbox.push(ObservationUpdate::LiveGap {
                        interaction_id: interaction.clone(),
                        run_id: String::new(),
                        reason: "history_invalidated".into(),
                    });
                }
            }
        }
    }
    pub(super) fn replace(&self, block: LiveContentBlock) -> bool {
        let mut mirror = self.0.lock();
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
    pub(super) fn append(&self, id: &str, text: &str, revision: u64) -> bool {
        let mut mirror = self.0.lock();
        if mirror.bytes.saturating_add(text.len()) > MAX_LIVE_BYTES {
            return false;
        }
        let Some((_, block)) = mirror.blocks.get_mut(id) else {
            return false;
        };
        block.text.push_str(text);
        block.revision = revision;
        mirror.bytes += text.len();
        true
    }
    pub(super) fn remove(&self, id: &str) {
        let mut mirror = self.0.lock();
        if let Some((_, block)) = mirror.blocks.remove(id) {
            mirror.bytes -= block.text.len();
        }
    }
}

fn ordered(mirror: &Mirror, interaction: Option<&str>) -> Vec<LiveContentBlock> {
    let mut blocks: Vec<_> = mirror
        .blocks
        .values()
        .filter(|(_, block)| interaction.is_none_or(|id| block.interaction_id == id))
        .collect();
    blocks.sort_unstable_by_key(|(order, _)| *order);
    blocks.into_iter().map(|(_, block)| block.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    fn block(interaction: &str, revision: u64, text: &str) -> LiveContentBlock {
        LiveContentBlock {
            block_id: format!("{interaction}-block"),
            interaction_id: interaction.into(),
            run_id: format!("{interaction}-run"),
            kind: "client_visible_content_delta".into(),
            model_turn_id: None,
            attempt_id: None,
            occurred_at: 1,
            revision,
            text: text.into(),
        }
    }

    #[tokio::test]
    async fn scoped_snapshot_and_slow_consumer_receive_latest_without_unrelated_body() {
        let state = LiveState::default();
        state.replace(block("a", 1, "first"));
        state.replace(block("b", 1, "private-b"));
        let mut a = state.subscribe("a".into());
        let mut b = state.subscribe("b".into());
        assert!(
            matches!(a.next().await, Some(ObservationUpdate::LiveSnapshot { blocks }) if blocks.len() == 1 && blocks[0].text == "first")
        );
        assert!(
            matches!(b.next().await, Some(ObservationUpdate::LiveSnapshot { blocks }) if blocks.len() == 1 && blocks[0].text == "private-b")
        );
        for revision in 2..=300 {
            state.replace(block("a", revision, &format!("latest-{revision}")));
            state.publish("a-block");
        }
        state.replace(block("b", 2, "b-only"));
        state.publish("b-block");
        state.emit(ObservationUpdate::LiveFinished {
            interaction_id: "a".into(),
            run_id: "a-run".into(),
        });
        state.replace(block("a", 301, "late-revision"));
        state.publish("a-block");
        assert!(
            matches!(a.next().await, Some(ObservationUpdate::LiveContent(block)) if block.revision == 300 && block.text == "latest-300")
        );
        assert!(
            matches!(a.next().await, Some(ObservationUpdate::LiveFinished { run_id, .. }) if run_id == "a-run")
        );
        assert!(
            matches!(a.next().await, Some(ObservationUpdate::LiveContent(block)) if block.revision == 301 && block.text == "late-revision")
        );
        assert!(
            matches!(b.next().await, Some(ObservationUpdate::LiveContent(block)) if block.text == "b-only")
        );
    }

    #[tokio::test]
    async fn subscription_sees_current_unpublished_append_and_invalidation_discards_pending_body() {
        let state = LiveState::default();
        state.replace(block("a", 1, "head"));
        assert!(state.append("a-block", " tail", 2));
        let mut stream = state.subscribe("a".into());
        assert!(
            matches!(stream.next().await, Some(ObservationUpdate::LiveSnapshot { blocks }) if blocks[0].text == "head tail" && blocks[0].revision == 2)
        );
        state.publish("a-block");
        state.invalidate(&["a".into()]);
        assert!(
            matches!(stream.next().await, Some(ObservationUpdate::LiveSnapshot { blocks }) if blocks.is_empty())
        );
        assert!(
            matches!(stream.next().await, Some(ObservationUpdate::LiveGap { reason, .. }) if reason == "history_invalidated")
        );
    }
}
