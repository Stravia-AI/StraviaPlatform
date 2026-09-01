use super::*;
use crate::protocol::ids::{
    ANTHROPIC_MESSAGES_2023_06_01, GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
    OPEN_RESPONSES_2026_04_24, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
};
use crate::protocol::ir::{
    AiItemAudience, AiItemProvenance, AiItemStatus, OpenResponsesExt, Role, ToolCall,
};

fn principal(id: &str) -> Principal {
    Principal::new(id)
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

fn legacy_context_fingerprint(messages: &[AiItem]) -> String {
    use sha2::{Digest, Sha256};

    messages.iter().fold(
        "stravia-response-chain-context-v2".to_owned(),
        |previous, message| {
            let message = serde_json::to_vec(message).expect("legacy test item must serialize");
            let mut hasher = Sha256::new();
            hasher.update(b"stravia-response-chain-context-v2\0");
            hasher.update(previous.as_bytes());
            hasher.update(message.len().to_be_bytes());
            hasher.update(message);
            crate::protocol::ir::canonical::hash_hex(&hasher.finalize().into())
        },
    )
}

fn responses_request(messages: Vec<AiItem>) -> AiRequest {
    let mut request = AiRequest::new("model", messages);
    request.ext = Some(ProtocolExt::OpenResponses(OpenResponsesExt::default()));
    request
}

fn chat_request(messages: serde_json::Value) -> AiRequest {
    crate::protocol::transform::ProtocolTransform::global()
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
    ) -> Result<Vec<crate::turn_chain::TurnNode>, crate::turn_chain::TurnUnavailable> {
        self.inner.materialize(principal, kind, id).await
    }

    async fn materialize_with_expiry(
        &self,
        principal: &Principal,
        kind: TurnNodeKind,
        id: &TurnNodeId,
    ) -> Result<crate::turn_chain::MaterializedTurnChain, crate::turn_chain::TurnUnavailable> {
        self.materializations
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(crate::turn_chain::MaterializedTurnChain {
            nodes: self.inner.materialize(principal, kind, id).await?,
            expires_at: std::time::Instant::now(),
        })
    }

    async fn commit(
        &self,
        commit: TurnCommit,
    ) -> Result<TurnNodeId, crate::turn_chain::TurnCommitError> {
        self.inner.commit(commit).await
    }

    async fn sweep_expired(&self) -> Result<u64, crate::turn_chain::TurnUnavailable> {
        self.inner.sweep_expired().await
    }
}

#[cfg(test)]
mod projection;
#[cfg(test)]
mod store_discovery;
#[cfg(test)]
mod write_markers;
