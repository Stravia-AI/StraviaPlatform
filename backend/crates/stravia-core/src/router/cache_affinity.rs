use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::hook::Principal;
use crate::protocol::ir::{self, AiRequest, Usage};

pub(crate) const CACHE_AFFINITY_MIN_PROMPT_TOKENS: u32 = 20_000;
const DEFAULT_CACHE_AFFINITY_CAPACITY: usize = 1_024;

#[derive(Clone, Default)]
pub(crate) struct CacheAffinity {
    index: Arc<Mutex<CacheAffinityIndex>>,
}

#[derive(Clone, PartialEq, Eq)]
struct CacheAffinityNamespace {
    principal: String,
    route_id: String,
    controls: [u8; 32],
}

struct CacheAffinityRecord {
    namespace: CacheAffinityNamespace,
    item_hashes: Vec<[u8; 32]>,
    target_key: String,
}

struct CacheAffinityIndex {
    capacity: usize,
    records: VecDeque<CacheAffinityRecord>,
}

impl Default for CacheAffinityIndex {
    fn default() -> Self {
        Self {
            capacity: DEFAULT_CACHE_AFFINITY_CAPACITY,
            records: VecDeque::with_capacity(DEFAULT_CACHE_AFFINITY_CAPACITY),
        }
    }
}

impl CacheAffinity {
    #[cfg(test)]
    fn with_capacity(capacity: usize) -> Self {
        Self {
            index: Arc::new(Mutex::new(CacheAffinityIndex {
                capacity,
                records: VecDeque::with_capacity(capacity),
            })),
        }
    }

    pub(crate) fn preferred_target(
        &self,
        principal: &Principal,
        route_id: &str,
        request: &AiRequest,
    ) -> Option<String> {
        let namespace = namespace(principal, route_id, request);
        let item_hashes = ir::canonical::item_hashes(&request.items);
        if item_hashes.is_empty() {
            return None;
        }
        let index = self
            .index
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut best = None;
        for record in index.records.iter().rev() {
            if record.namespace != namespace {
                continue;
            }
            let matched_items = record
                .item_hashes
                .iter()
                .zip(&item_hashes)
                .take_while(|(record, request)| record == request)
                .count();
            if matched_items > 0
                && best
                    .as_ref()
                    .is_none_or(|(best_items, _): &(usize, String)| matched_items > *best_items)
            {
                best = Some((matched_items, record.target_key.clone()));
            }
        }
        best.map(|(_, target_key)| target_key)
    }

    pub(crate) fn record_success(
        &self,
        principal: &Principal,
        route_id: &str,
        request: &AiRequest,
        target_key: &str,
        usage: &Usage,
    ) {
        if !usage.required_components_known
            || usage.prompt_tokens < CACHE_AFFINITY_MIN_PROMPT_TOKENS
        {
            return;
        }
        let item_hashes = ir::canonical::item_hashes(&request.items);
        if item_hashes.is_empty() {
            return;
        }
        let record = CacheAffinityRecord {
            namespace: namespace(principal, route_id, request),
            item_hashes,
            target_key: target_key.to_owned(),
        };
        let mut index = self
            .index
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        index.records.retain(|existing| {
            existing.namespace != record.namespace
                || existing.item_hashes != record.item_hashes
                || existing.target_key != record.target_key
        });
        while index.records.len() >= index.capacity {
            index.records.pop_front();
        }
        if index.capacity > 0 {
            index.records.push_back(record);
        }
    }
}

fn namespace(principal: &Principal, route_id: &str, request: &AiRequest) -> CacheAffinityNamespace {
    CacheAffinityNamespace {
        principal: principal.continuation_key(),
        route_id: route_id.to_owned(),
        controls: ir::canonical::cache_controls_hash(request),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ir::{
        AiItem, AnthropicExt, ContentBlock, MessageContent, OpenResponsesExt, ProtocolExt, Role,
        ToolCall, Usage,
    };

    fn request(items: &[&str]) -> AiRequest {
        AiRequest::new(
            "route-model",
            items
                .iter()
                .map(|item| AiItem {
                    role: Role::User,
                    content: MessageContent::Text((*item).into()),
                    tool_calls: None,
                    tool_call_id: None,
                    meta: None,
                })
                .collect(),
        )
    }

    fn usage(prompt_tokens: Option<u32>) -> Usage {
        Usage {
            prompt_tokens: prompt_tokens.unwrap_or_default(),
            required_components_known: prompt_tokens.is_some(),
            ..Default::default()
        }
    }

    #[test]
    fn longest_exact_prefix_prefers_the_target_that_processed_it() {
        let index = CacheAffinity::default();
        let owner = Principal::new("owner");
        index.record_success(
            &owner,
            "route",
            &request(&["a", "b", "older"]),
            "provider:older",
            &usage(Some(CACHE_AFFINITY_MIN_PROMPT_TOKENS)),
        );
        index.record_success(
            &owner,
            "route",
            &request(&["a", "b", "c", "newer"]),
            "provider:newer",
            &usage(Some(CACHE_AFFINITY_MIN_PROMPT_TOKENS)),
        );

        assert_eq!(
            index.preferred_target(&owner, "route", &request(&["a", "b", "c", "next"])),
            Some("provider:newer".into())
        );
    }

    #[test]
    fn anthropic_output_and_responses_replay_share_cache_affinity() {
        let index = CacheAffinity::default();
        let owner = Principal::new("owner");
        let question = AiItem {
            role: Role::User,
            content: MessageContent::Text("question".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        };
        let anthropic_output = AiItem {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![
                ContentBlock::Thinking {
                    thinking: "summaryreasoning".into(),
                    signature: Some("opaque".into()),
                },
                ContentBlock::Text {
                    text: "answer".into(),
                    cache_control: None,
                },
                ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "lookup".into(),
                    input: serde_json::json!({"value": 1}),
                    cache_control: None,
                },
            ]),
            tool_calls: Some(vec![ToolCall {
                id: "call_1".into(),
                name: "lookup".into(),
                arguments: "{\"value\":1}".into(),
            }]),
            tool_call_id: None,
            meta: None,
        };
        let recorded = AiRequest::new(
            "route-model",
            vec![
                question.clone(),
                anthropic_output,
                AiItem {
                    role: Role::User,
                    content: MessageContent::Text("recorded turn".into()),
                    tool_calls: None,
                    tool_call_id: None,
                    meta: None,
                },
            ],
        );
        index.record_success(
            &owner,
            "route",
            &recorded,
            "provider:shared",
            &usage(Some(CACHE_AFFINITY_MIN_PROMPT_TOKENS)),
        );

        let replay = AiRequest::new(
            "route-model",
            vec![
                question,
                AiItem::reasoning(
                    vec!["summary".into()],
                    vec!["reasoning".into()],
                    Some("opaque".into()),
                ),
                AiItem::output_text("answer"),
                AiItem::function_call(ToolCall {
                    id: "call_1".into(),
                    name: "lookup".into(),
                    arguments: "{\"value\":1}".into(),
                }),
                AiItem {
                    role: Role::User,
                    content: MessageContent::Text("next turn".into()),
                    tool_calls: None,
                    tool_call_id: None,
                    meta: None,
                },
            ],
        );

        assert_eq!(
            index.preferred_target(&owner, "route", &replay),
            Some("provider:shared".into())
        );
    }

    #[test]
    fn rejects_unknown_or_short_usage_and_isolates_principal_route_and_cache_controls() {
        let index = CacheAffinity::default();
        let owner = Principal::new("owner");
        let other = Principal::new("other");
        let request = request(&["same", "prefix"]);
        index.record_success(&owner, "route", &request, "provider:eligible", &usage(None));
        index.record_success(
            &owner,
            "route",
            &request,
            "provider:short",
            &usage(Some(CACHE_AFFINITY_MIN_PROMPT_TOKENS - 1)),
        );
        assert_eq!(index.preferred_target(&owner, "route", &request), None);

        index.record_success(
            &owner,
            "route",
            &request,
            "provider:eligible",
            &usage(Some(CACHE_AFFINITY_MIN_PROMPT_TOKENS)),
        );
        assert_eq!(
            index.preferred_target(&owner, "route", &request),
            Some("provider:eligible".into())
        );
        assert_eq!(index.preferred_target(&other, "route", &request), None);
        assert_eq!(
            index.preferred_target(&owner, "other-route", &request),
            None
        );

        let mut default_protocol_extension = request.clone();
        default_protocol_extension.ext =
            Some(ProtocolExt::OpenResponses(OpenResponsesExt::default()));
        assert_eq!(
            index.preferred_target(&owner, "route", &default_protocol_extension),
            Some("provider:eligible".into())
        );

        let mut different_generation_controls = request.clone();
        different_generation_controls.generation.temperature = Some(0.2);
        assert_eq!(
            index.preferred_target(&owner, "route", &different_generation_controls),
            Some("provider:eligible".into())
        );

        let mut different_generation_extension = request.clone();
        different_generation_extension.ext = Some(ProtocolExt::Anthropic(AnthropicExt {
            top_k: Some(8),
            ..Default::default()
        }));
        assert_eq!(
            index.preferred_target(&owner, "route", &different_generation_extension),
            Some("provider:eligible".into())
        );

        let mut different_cache_controls = request.clone();
        different_cache_controls.ext = Some(ProtocolExt::OpenResponses(OpenResponsesExt {
            prompt_cache_key: Some("other-cache-key".into()),
            ..Default::default()
        }));
        assert_eq!(
            index.preferred_target(&owner, "route", &different_cache_controls),
            None
        );
    }

    #[test]
    fn eviction_only_removes_an_affinity_preference() {
        let index = CacheAffinity::with_capacity(1);
        let owner = Principal::new("owner");
        let first = request(&["first"]);
        let second = request(&["second"]);
        index.record_success(
            &owner,
            "route",
            &first,
            "provider:first",
            &usage(Some(CACHE_AFFINITY_MIN_PROMPT_TOKENS)),
        );
        index.record_success(
            &owner,
            "route",
            &second,
            "provider:second",
            &usage(Some(CACHE_AFFINITY_MIN_PROMPT_TOKENS)),
        );

        assert_eq!(index.preferred_target(&owner, "route", &first), None);
        assert_eq!(
            index.preferred_target(&owner, "route", &second),
            Some("provider:second".into())
        );
    }
}
