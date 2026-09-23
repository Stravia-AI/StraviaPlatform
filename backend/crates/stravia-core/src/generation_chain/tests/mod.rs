use super::*;
use stravia_runtime_contract::protocol::ids::ANTHROPIC_MESSAGES_2023_06_01;
use stravia_runtime_contract::protocol::ids::GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA;
use stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24;
use stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1;
use stravia_runtime_contract::protocol::ir::AiItemAudience;
use stravia_runtime_contract::protocol::ir::AiItemProvenance;
use stravia_runtime_contract::protocol::ir::AiItemStatus;
use stravia_runtime_contract::protocol::ir::OpenResponsesExt;
use stravia_runtime_contract::protocol::ir::Role;
use stravia_runtime_contract::protocol::ir::ToolCall;

fn principal(id: &str) -> Principal {
    Principal::new(id)
}

fn generation_source() -> GenerationSource {
    GenerationSource::Target {
        namespace: "provider:model".into(),
        protocol: Some(OPEN_RESPONSES_2026_04_24.into()),
        actual_model: "model".into(),
        selected_target_key: "provider:model".into(),
    }
}

async fn generation_store() -> GenerationChainStore {
    GenerationChainStore::from_turn_chain(
        Arc::new(crate::turn_chain::test_store().await),
        DEFAULT_GENERATION_CHAIN_TTL,
    )
}

async fn generation_chain() -> GenerationChain {
    GenerationChain::from_turn_chain(
        Arc::new(crate::turn_chain::test_store().await),
        DEFAULT_GENERATION_CHAIN_TTL,
        None,
    )
}

fn user_message(text: &str) -> AiItem {
    AiItem {
        role: Role::User,
        content: MessageContent::Text(text.into()),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    }
}

fn responses_request(messages: Vec<AiItem>) -> AiRequest {
    let mut request = AiRequest::new("model", messages);
    request.ext = Some(ProtocolExt::OpenResponses(OpenResponsesExt::default()));
    request
}

fn chat_request(messages: serde_json::Value) -> AiRequest {
    stravia_protocol_codec::transform::ProtocolTransform::global()
        .bind(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            OPEN_RESPONSES_2026_04_24,
        )
        .expect("registered protocol pair")
        .decode_request(serde_json::json!({
            "model": "model",
            "messages": messages,
        }))
        .expect("valid Chat Completions request")
}

struct ImmediatelyExpiredTurnChainStore {
    inner: crate::turn_chain::SqlTurnChainStore,
    materializations: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl TurnChainStore for ImmediatelyExpiredTurnChainStore {
    async fn materialize(
        &self,
        principal: &Principal,
        kind: TurnNodeKind,
        id: &TurnNodeId,
    ) -> Result<
        Vec<stravia_runtime_contract::turn_chain::TurnNode>,
        stravia_runtime_contract::turn_chain::TurnUnavailable,
    > {
        self.inner.materialize(principal, kind, id).await
    }

    async fn materialize_with_expiry(
        &self,
        principal: &Principal,
        kind: TurnNodeKind,
        id: &TurnNodeId,
    ) -> Result<
        stravia_runtime_contract::turn_chain::MaterializedTurnChain,
        stravia_runtime_contract::turn_chain::TurnUnavailable,
    > {
        self.materializations
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(
            stravia_runtime_contract::turn_chain::MaterializedTurnChain {
                nodes: self.inner.materialize(principal, kind, id).await?,
                expires_at: std::time::Instant::now(),
            },
        )
    }

    async fn commit(
        &self,
        commit: TurnCommit,
    ) -> Result<TurnNodeId, stravia_runtime_contract::turn_chain::TurnCommitError> {
        self.inner.commit(commit).await
    }

    async fn sweep_expired(
        &self,
    ) -> Result<u64, stravia_runtime_contract::turn_chain::TurnUnavailable> {
        self.inner.sweep_expired().await
    }
}

struct CommitBarrierTurnChainStore {
    inner: crate::turn_chain::SqlTurnChainStore,
    block_next: std::sync::atomic::AtomicBool,
    fail_blocked: std::sync::atomic::AtomicBool,
    force_stale_discovery: std::sync::atomic::AtomicBool,
    entered: tokio::sync::Semaphore,
    release: tokio::sync::Semaphore,
}

impl CommitBarrierTurnChainStore {
    fn new(inner: crate::turn_chain::SqlTurnChainStore) -> Self {
        Self {
            inner,
            block_next: std::sync::atomic::AtomicBool::new(false),
            fail_blocked: std::sync::atomic::AtomicBool::new(false),
            force_stale_discovery: std::sync::atomic::AtomicBool::new(false),
            entered: tokio::sync::Semaphore::new(0),
            release: tokio::sync::Semaphore::new(0),
        }
    }

    fn block_next_commit(&self, fail: bool) {
        self.fail_blocked
            .store(fail, std::sync::atomic::Ordering::Release);
        self.block_next
            .store(true, std::sync::atomic::Ordering::Release);
    }

    async fn wait_until_blocked(&self) {
        self.entered
            .acquire()
            .await
            .expect("commit barrier remains open")
            .forget();
    }

    fn release_commit(&self) {
        self.release.add_permits(1);
    }

    fn force_stale_discovery(&self) {
        self.force_stale_discovery
            .store(true, std::sync::atomic::Ordering::Release);
    }

    fn allow_current_discovery(&self) {
        self.force_stale_discovery
            .store(false, std::sync::atomic::Ordering::Release);
    }
}

#[async_trait::async_trait]
impl TurnChainStore for CommitBarrierTurnChainStore {
    async fn materialize(
        &self,
        principal: &Principal,
        kind: TurnNodeKind,
        id: &TurnNodeId,
    ) -> Result<
        Vec<stravia_runtime_contract::turn_chain::TurnNode>,
        stravia_runtime_contract::turn_chain::TurnUnavailable,
    > {
        self.inner.materialize(principal, kind, id).await
    }

    async fn materialize_with_expiry(
        &self,
        principal: &Principal,
        kind: TurnNodeKind,
        id: &TurnNodeId,
    ) -> Result<
        stravia_runtime_contract::turn_chain::MaterializedTurnChain,
        stravia_runtime_contract::turn_chain::TurnUnavailable,
    > {
        self.inner
            .materialize_with_expiry(principal, kind, id)
            .await
    }

    async fn commit(
        &self,
        commit: TurnCommit,
    ) -> Result<TurnNodeId, stravia_runtime_contract::turn_chain::TurnCommitError> {
        if self
            .block_next
            .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            self.entered.add_permits(1);
            self.release
                .acquire()
                .await
                .map_err(|_| {
                    stravia_runtime_contract::turn_chain::TurnCommitError::Storage(
                        "commit barrier closed".into(),
                    )
                })?
                .forget();
            if self
                .fail_blocked
                .swap(false, std::sync::atomic::Ordering::AcqRel)
            {
                return Err(
                    stravia_runtime_contract::turn_chain::TurnCommitError::Storage(
                        "injected commit failure".into(),
                    ),
                );
            }
        }
        self.inner.commit(commit).await
    }

    async fn find_reusable_prefixes(
        &self,
        principal: &Principal,
        kind: TurnNodeKind,
        query: &ReusablePrefixQuery,
    ) -> Result<
        Vec<stravia_runtime_contract::turn_chain::ReusablePrefixCandidate>,
        stravia_runtime_contract::turn_chain::TurnUnavailable,
    > {
        if self
            .force_stale_discovery
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Ok(Vec::new());
        }
        self.inner
            .find_reusable_prefixes(principal, kind, query)
            .await
    }

    async fn rebuild_prefixes(
        &self,
        namespace_prefix: &str,
        decode: &(
             dyn Fn(
            Vec<stravia_runtime_contract::turn_chain::TurnNode>,
            i64,
        ) -> Result<Option<ReusablePrefixMetadata>, String>
                 + Send
                 + Sync
         ),
    ) -> Result<(), stravia_runtime_contract::turn_chain::TurnUnavailable> {
        self.inner.rebuild_prefixes(namespace_prefix, decode).await
    }

    async fn sweep_expired(
        &self,
    ) -> Result<u64, stravia_runtime_contract::turn_chain::TurnUnavailable> {
        self.inner.sweep_expired().await
    }
}

#[cfg(test)]
mod projection;
#[cfg(test)]
mod store_discovery;
#[cfg(test)]
mod write_markers;
