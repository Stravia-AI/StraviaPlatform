use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use crate::runtime_cache::RuntimeCache;
use stravia_runtime_contract::Principal;
use stravia_runtime_contract::protocol::ir;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::Usage;

pub(crate) const CACHE_AFFINITY_MIN_PROMPT_TOKENS: u32 = 20_000;
const DEFAULT_CACHE_AFFINITY_CAPACITY: usize = 1_024;
const PUBLICATION_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Clone)]
pub(crate) struct CacheAffinity {
    runtime_cache: RuntimeCache,
    mutation: Arc<Mutex<()>>,
    capacity: usize,
    cache_namespace: Arc<str>,
}

#[derive(serde::Serialize)]
struct CacheAffinityNamespace {
    principal: String,
    route_id: String,
    controls: [u8; 32],
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct CacheAffinityRecord {
    item_hashes: Vec<[u8; 32]>,
    target_key: String,
}

#[cfg(test)]
impl Default for CacheAffinity {
    fn default() -> Self {
        Self::new(RuntimeCache::tinyufo(1024 * 1024))
    }
}

impl CacheAffinity {
    pub(crate) fn new(runtime_cache: RuntimeCache) -> Self {
        Self {
            runtime_cache,
            mutation: Arc::new(Mutex::new(())),
            capacity: DEFAULT_CACHE_AFFINITY_CAPACITY,
            cache_namespace: uuid::Uuid::new_v4().to_string().into(),
        }
    }

    #[cfg(test)]
    fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            ..Self::default()
        }
    }

    pub(crate) async fn preferred_target(
        &self,
        principal: &Principal,
        route_id: &str,
        request: &AiRequest,
    ) -> Option<String> {
        if request.items.is_empty() {
            return None;
        }
        let key = self.namespace_key(principal, route_id, request);
        let records = self
            .runtime_cache
            .get::<VecDeque<CacheAffinityRecord>>("affinity", &key)
            .await?;
        if records.is_empty() {
            return None;
        }
        let item_hashes = ir::canonical::item_hashes(&request.items);
        let mut best = None;
        for record in records.iter().rev() {
            let matched_items = record
                .item_hashes
                .iter()
                .zip(&item_hashes)
                .take_while(|(record, request)| record == request)
                .count();
            if matched_items > 0
                && best
                    .as_ref()
                    .is_none_or(|(best_items, _): &(usize, &str)| matched_items > *best_items)
            {
                best = Some((matched_items, record.target_key.as_str()));
            }
        }
        best.map(|(_, target_key)| target_key.to_owned())
    }

    pub(crate) async fn record_success(
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
        if self.capacity == 0 {
            return;
        }
        let key = self.namespace_key(principal, route_id, request);
        let record = CacheAffinityRecord {
            item_hashes,
            target_key: target_key.to_owned(),
        };
        // 所有 clone 共用 RMW 门；等待锁和 Redis 读写共用可选发布的总 deadline。
        // 超时直接放弃，不让已完成推理的 reservation/Observation 无限排队。
        let publication = async {
            let _guard = self.mutation.lock().await;
            let mut records = self
                .runtime_cache
                .get::<VecDeque<CacheAffinityRecord>>("affinity", &key)
                .await
                .map(|records| records.as_ref().clone())
                .unwrap_or_default();
            records.retain(|existing| {
                existing.item_hashes != record.item_hashes
                    || existing.target_key != record.target_key
            });
            while records.len() >= self.capacity {
                records.pop_front();
            }
            records.push_back(record);
            let estimated_bytes = std::mem::size_of::<VecDeque<CacheAffinityRecord>>()
                + records.capacity() * std::mem::size_of::<CacheAffinityRecord>()
                + records
                    .iter()
                    .map(|record| {
                        record.item_hashes.capacity() * std::mem::size_of::<[u8; 32]>()
                            + record.target_key.capacity()
                    })
                    .sum::<usize>();
            self.runtime_cache
                .put(
                    "affinity",
                    &key,
                    Arc::new(records),
                    estimated_bytes,
                    Duration::from_secs(86_400),
                )
                .await;
        };
        if tokio::time::timeout(PUBLICATION_TIMEOUT, publication)
            .await
            .is_err()
        {
            tracing::warn!("Cache affinity publication timed out; optional preference skipped");
        }
    }

    fn namespace_key(&self, principal: &Principal, route_id: &str, request: &AiRequest) -> String {
        // Fixed field types cannot fail serialization. Only identities and
        // canonical control hashes are retained, never prompt text.
        format!(
            "{}:{}",
            self.cache_namespace,
            serde_json::to_string(&namespace(principal, route_id, request))
                .expect("cache affinity namespace serialization")
        )
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
    use stravia_runtime_contract::protocol::ir::AiItem;
    use stravia_runtime_contract::protocol::ir::AnthropicExt;
    use stravia_runtime_contract::protocol::ir::ContentBlock;
    use stravia_runtime_contract::protocol::ir::MessageContent;
    use stravia_runtime_contract::protocol::ir::OpenResponsesExt;
    use stravia_runtime_contract::protocol::ir::ProtocolExt;
    use stravia_runtime_contract::protocol::ir::Role;
    use stravia_runtime_contract::protocol::ir::ToolCall;
    use stravia_runtime_contract::protocol::ir::Usage;

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

    #[tokio::test]
    async fn longest_exact_prefix_prefers_the_target_that_processed_it() {
        let index = CacheAffinity::default();
        let owner = Principal::new("owner");
        index
            .record_success(
                &owner,
                "route",
                &request(&["a", "b", "older"]),
                "provider:older",
                &usage(Some(CACHE_AFFINITY_MIN_PROMPT_TOKENS)),
            )
            .await;
        index
            .record_success(
                &owner,
                "route",
                &request(&["a", "b", "c", "newer"]),
                "provider:newer",
                &usage(Some(CACHE_AFFINITY_MIN_PROMPT_TOKENS)),
            )
            .await;

        assert_eq!(
            index
                .preferred_target(&owner, "route", &request(&["a", "b", "c", "next"]))
                .await,
            Some("provider:newer".into())
        );
    }

    #[tokio::test]
    async fn anthropic_output_and_responses_replay_share_cache_affinity() {
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
        index
            .record_success(
                &owner,
                "route",
                &recorded,
                "provider:shared",
                &usage(Some(CACHE_AFFINITY_MIN_PROMPT_TOKENS)),
            )
            .await;

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
            index.preferred_target(&owner, "route", &replay).await,
            Some("provider:shared".into())
        );
    }

    #[tokio::test]
    async fn rejects_unknown_or_short_usage_and_isolates_principal_route_and_cache_controls() {
        let index = CacheAffinity::default();
        let owner = Principal::new("owner");
        let other = Principal::new("other");
        let request = request(&["same", "prefix"]);
        index
            .record_success(&owner, "route", &request, "provider:eligible", &usage(None))
            .await;
        index
            .record_success(
                &owner,
                "route",
                &request,
                "provider:short",
                &usage(Some(CACHE_AFFINITY_MIN_PROMPT_TOKENS - 1)),
            )
            .await;
        assert_eq!(
            index.preferred_target(&owner, "route", &request).await,
            None
        );

        index
            .record_success(
                &owner,
                "route",
                &request,
                "provider:eligible",
                &usage(Some(CACHE_AFFINITY_MIN_PROMPT_TOKENS)),
            )
            .await;
        assert_eq!(
            index.preferred_target(&owner, "route", &request).await,
            Some("provider:eligible".into())
        );
        assert_eq!(
            index.preferred_target(&other, "route", &request).await,
            None
        );
        assert_eq!(
            index
                .preferred_target(&owner, "other-route", &request)
                .await,
            None
        );

        let mut default_protocol_extension = request.clone();
        default_protocol_extension.ext =
            Some(ProtocolExt::OpenResponses(OpenResponsesExt::default()));
        assert_eq!(
            index
                .preferred_target(&owner, "route", &default_protocol_extension)
                .await,
            Some("provider:eligible".into())
        );

        let mut different_generation_controls = request.clone();
        different_generation_controls.generation.temperature = Some(0.2);
        assert_eq!(
            index
                .preferred_target(&owner, "route", &different_generation_controls)
                .await,
            Some("provider:eligible".into())
        );

        let mut different_generation_extension = request.clone();
        different_generation_extension.ext = Some(ProtocolExt::Anthropic(AnthropicExt {
            top_k: Some(8),
            ..Default::default()
        }));
        assert_eq!(
            index
                .preferred_target(&owner, "route", &different_generation_extension)
                .await,
            Some("provider:eligible".into())
        );

        let mut different_cache_controls = request.clone();
        different_cache_controls.ext = Some(ProtocolExt::OpenResponses(OpenResponsesExt {
            prompt_cache_key: Some("other-cache-key".into()),
            ..Default::default()
        }));
        assert_eq!(
            index
                .preferred_target(&owner, "route", &different_cache_controls)
                .await,
            None
        );
    }

    #[tokio::test]
    async fn clones_share_recent_success_and_runtime_cache_removal_is_a_miss() {
        let runtime_cache = RuntimeCache::tinyufo(1024 * 1024);
        let index = CacheAffinity::new(runtime_cache.clone());
        let cloned = index.clone();
        let owner = Principal::new("owner");
        let history = request(&["shared", "history"]);
        let eligible_usage = usage(Some(CACHE_AFFINITY_MIN_PROMPT_TOKENS));
        index
            .record_success(&owner, "route", &history, "provider:older", &eligible_usage)
            .await;
        cloned
            .record_success(&owner, "route", &history, "provider:newer", &eligible_usage)
            .await;
        assert_eq!(
            index.preferred_target(&owner, "route", &history).await,
            Some("provider:newer".into()),
        );

        // Removing the shared entry must not expose a private retained copy.
        runtime_cache
            .remove("affinity", &index.namespace_key(&owner, "route", &history))
            .await;
        assert_eq!(
            cloned.preferred_target(&owner, "route", &history).await,
            None
        );
        assert_eq!(
            index.preferred_target(&owner, "route", &history).await,
            None
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_mutation_gate_skips_optional_preference_within_publication_deadline() {
        let index = CacheAffinity::default();
        let guard = index.mutation.lock().await;
        let publisher = index.clone();
        let publication = tokio::spawn(async move {
            publisher
                .record_success(
                    &Principal::new("owner"),
                    "route",
                    &request(&["history"]),
                    "provider:eligible",
                    &usage(Some(CACHE_AFFINITY_MIN_PROMPT_TOKENS)),
                )
                .await;
        });
        tokio::task::yield_now().await;
        tokio::time::advance(PUBLICATION_TIMEOUT - Duration::from_millis(1)).await;
        assert!(!publication.is_finished());
        tokio::time::advance(Duration::from_millis(1)).await;
        publication.await.unwrap();
        drop(guard);
        assert_eq!(
            index
                .preferred_target(&Principal::new("owner"), "route", &request(&["history"]))
                .await,
            None
        );
    }

    #[tokio::test]
    async fn eviction_only_removes_an_affinity_preference() {
        let index = CacheAffinity::with_capacity(1);
        let owner = Principal::new("owner");
        let first = request(&["first"]);
        let second = request(&["second"]);
        index
            .record_success(
                &owner,
                "route",
                &first,
                "provider:first",
                &usage(Some(CACHE_AFFINITY_MIN_PROMPT_TOKENS)),
            )
            .await;
        index
            .record_success(
                &owner,
                "route",
                &second,
                "provider:second",
                &usage(Some(CACHE_AFFINITY_MIN_PROMPT_TOKENS)),
            )
            .await;

        assert_eq!(index.preferred_target(&owner, "route", &first).await, None);
        assert_eq!(
            index.preferred_target(&owner, "route", &second).await,
            Some("provider:second".into())
        );
    }
}
