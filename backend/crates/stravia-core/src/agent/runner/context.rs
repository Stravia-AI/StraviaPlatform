use super::*;

#[derive(Default)]
pub(super) struct ParentAgentContext {
    pub(super) transcript: Vec<AiItem>,
    pub(super) processed_input_items: usize,
    pub(super) snapshot: Option<ParentAgentSnapshot>,
    pub(super) root_turn_id: Option<AgentTurnId>,
    pub(super) state_checkpoint_id: Option<uuid::Uuid>,
    pub(super) context_window_id: Option<uuid::Uuid>,
    pub(super) context_window_index: Option<u64>,
    pub(super) user_prompt_index: u64,
    pub(super) user_turn_index: Option<u64>,
    pub(super) client_conversation_id: Option<String>,
    pub(super) client_conversation_created_at_ms: Option<i64>,
}

pub(super) struct ParentAgentSnapshot {
    pub(super) definition_id: AgentDefinitionId,
    pub(super) definition_revision: u32,
    pub(super) model_id: String,
    pub(super) thinking_level: Option<stravia_runtime_contract::thinking::ThinkingLevel>,
}

impl AgentRunner {
    /// 父 Turn 所属的 Definition；调用方据此在没有新 Artifact 时判定续接的模态。
    pub(crate) async fn parent_definition_id(
        &self,
        principal: &Principal,
        parent: &AgentTurnId,
    ) -> Result<AgentDefinitionId, AgentRunError> {
        let context = self.load_parent_context(principal, Some(parent)).await?;
        context
            .snapshot
            .map(|snapshot| snapshot.definition_id)
            .ok_or_else(|| {
                AgentRunError::new("parent_turn_unavailable", "Parent Turn is unavailable")
            })
    }

    pub(crate) async fn parent_artifact_ids(
        &self,
        principal: &Principal,
        parent: &AgentTurnId,
        expected_definition: &AgentDefinitionId,
    ) -> Result<Vec<ArtifactId>, AgentRunError> {
        let context = self.load_parent_context(principal, Some(parent)).await?;
        let Some(snapshot) = context.snapshot else {
            return Err(AgentRunError::new(
                "parent_turn_unavailable",
                "Parent Turn is unavailable",
            ));
        };
        if &snapshot.definition_id != expected_definition {
            return Err(AgentRunError::new(
                "parent_turn_unavailable",
                "Parent Turn is unavailable",
            ));
        }
        let mut seen = HashSet::new();
        let mut artifacts = Vec::new();
        for message in context.transcript {
            let MessageContent::Blocks(blocks) = message.content else {
                continue;
            };
            for block in blocks {
                let (ContentBlock::Image { source, .. } | ContentBlock::Audio { source }) = block
                else {
                    continue;
                };
                let id = match source {
                    MediaSource::Url(reference) => ArtifactId::from_reference(&reference).ok(),
                    MediaSource::FileId { file_id, .. } => {
                        ArtifactId::from_reference(&file_id).ok()
                    }
                    MediaSource::Base64 { .. } => None,
                };
                let Some(id) = id else {
                    continue;
                };
                if seen.insert(id.clone()) {
                    artifacts.push(id);
                }
            }
        }
        Ok(artifacts)
    }

    pub(super) async fn load_artifact_blocks(
        &self,
        principal: &Principal,
        artifact_ids: &[ArtifactId],
        policy: &ArtifactPolicy,
    ) -> Result<Vec<ContentBlock>, AgentRunError> {
        if artifact_ids.is_empty() {
            return Ok(Vec::new());
        }
        if artifact_ids.len() > policy.max_artifacts as usize {
            return Err(AgentRunError::new(
                "artifact_limit",
                "Agent input exceeds the Artifact count limit",
            ));
        }
        let store = self.artifacts.as_ref().ok_or_else(|| {
            AgentRunError::new(
                "artifact_store_unavailable",
                "Agent input references Artifacts but no ArtifactStore is configured",
            )
        })?;
        let mut blocks = Vec::with_capacity(artifact_ids.len());
        let mut total_bytes = 0_u64;
        for id in artifact_ids {
            let reader = store
                .open(principal, id)
                .await
                .map_err(|error| AgentRunError::new("artifact_unavailable", error.to_string()))?;
            total_bytes = total_bytes.saturating_add(reader.artifact.size);
            if total_bytes > policy.max_bytes {
                return Err(AgentRunError::new(
                    "artifact_bytes_limit",
                    "Agent input exceeds the Artifact byte limit",
                ));
            }
            if !policy
                .allowed_mime_types
                .iter()
                .any(|allowed| mime_matches(allowed, &reader.artifact.mime_type))
            {
                return Err(AgentRunError::new(
                    "artifact_mime_type_denied",
                    format!(
                        "Artifact MIME type is not allowed: {}",
                        reader.artifact.mime_type
                    ),
                ));
            }
            let media_type = reader.artifact.mime_type.clone();
            let source = MediaSource::Url(reader.artifact.reference());
            let block = if media_type.starts_with("image/") {
                ContentBlock::Image {
                    source,
                    detail: None,
                    cache_control: None,
                }
            } else if media_type.starts_with("video/") {
                ContentBlock::Video {
                    source,
                    media_type: Some(media_type),
                }
            } else if media_type.starts_with("audio/") {
                ContentBlock::Audio { source }
            } else {
                ContentBlock::File {
                    source,
                    media_type: Some(media_type),
                }
            };
            blocks.push(block);
        }
        Ok(blocks)
    }

    pub(super) async fn hydrate_transcript(
        &self,
        principal: &Principal,
        transcript: &[AiItem],
    ) -> Result<Vec<AiItem>, AgentRunError> {
        let mut hydrated = transcript.to_vec();
        for message in &mut hydrated {
            let MessageContent::Blocks(blocks) = &mut message.content else {
                continue;
            };
            for block in blocks {
                let source = match block {
                    ContentBlock::Image { source, .. }
                    | ContentBlock::Video { source, .. }
                    | ContentBlock::Audio { source }
                    | ContentBlock::File { source, .. } => source,
                    _ => continue,
                };

                let MediaSource::FileId { file_id, .. } = source else {
                    continue;
                };
                let Ok(artifact_id) = ArtifactId::from_reference(file_id) else {
                    continue;
                };
                let store = self.artifacts.as_ref().ok_or_else(|| {
                    AgentRunError::new(
                        "artifact_store_unavailable",
                        "Agent Turn references Artifacts but no ArtifactStore is configured",
                    )
                })?;
                let reader = store.open(principal, &artifact_id).await.map_err(|error| {
                    AgentRunError::new("artifact_unavailable", error.to_string())
                })?;
                *source = MediaSource::Url(reader.artifact.reference());
            }
        }
        Ok(hydrated)
    }

    pub(super) async fn load_parent_context(
        &self,
        principal: &Principal,
        parent: Option<&AgentTurnId>,
    ) -> Result<ParentAgentContext, AgentRunError> {
        let Some(parent) = parent else {
            return Ok(ParentAgentContext::default());
        };
        let chain = self
            .turns
            .materialize(principal, TurnNodeKind::Agent, parent)
            .await
            .map_err(|error| AgentRunError::new("parent_turn_unavailable", error.to_string()))?;
        if chain.first().is_none_or(|node| node.parent_id.is_some())
            || chain.last().is_none_or(|node| &node.id != parent)
            || chain
                .windows(2)
                .any(|nodes| nodes[1].parent_id.as_ref() != Some(&nodes[0].id))
        {
            return Err(AgentRunError::new(
                "parent_turn_invalid",
                "Parent Turn ancestry is incomplete",
            ));
        }
        let user_prompt_index = u64::try_from(chain.len()).map_err(|_| {
            AgentRunError::new("parent_turn_invalid", "Agent Turn ancestry is too long")
        })?;
        let root_turn_id = chain
            .first()
            .map(|node| node.id.clone())
            .ok_or_else(|| AgentRunError::new("parent_turn_unavailable", "Parent Turn is empty"))?;
        let root = &chain[0].payload;
        let client_conversation_id = root
            .get("user_prompt_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let client_conversation_created_at_ms = root
            .get("user_prompt_created_at_ms")
            .and_then(Value::as_i64);
        let payload = chain
            .last()
            .ok_or_else(|| AgentRunError::new("parent_turn_unavailable", "Parent Turn is empty"))?;
        let state_checkpoint_id = payload
            .payload
            .get("state_checkpoint_id")
            .map(|value| serde_json::from_value::<Option<uuid::Uuid>>(value.clone()))
            .transpose()
            .map_err(|error| AgentRunError::new("parent_turn_invalid", error.to_string()))?
            .flatten();
        let context_window_id = payload
            .payload
            .get("context_window_id")
            .map(|value| serde_json::from_value::<Option<uuid::Uuid>>(value.clone()))
            .transpose()
            .map_err(|error| AgentRunError::new("parent_turn_invalid", error.to_string()))?
            .flatten();
        let context_window_index = payload
            .payload
            .get("context_window_index")
            .and_then(Value::as_u64);
        if context_window_id.is_some() != context_window_index.is_some() {
            return Err(AgentRunError::new(
                "parent_turn_invalid",
                "Incomplete Agent context window",
            ));
        }
        let user_turn_index = payload
            .payload
            .get("user_turn_index")
            .and_then(Value::as_u64);
        let transcript: Vec<AiItem> = serde_json::from_value(
            payload.payload.get("transcript").cloned().ok_or_else(|| {
                AgentRunError::new(
                    "parent_turn_invalid",
                    "Parent Turn has no canonical transcript",
                )
            })?,
        )
        .map_err(|error| AgentRunError::new("parent_turn_invalid", error.to_string()))?;
        // 旧 Turn 没有保存 Request Hook 的执行视图，不能从响应归属推断已处理。
        let processed_input_items = payload
            .payload
            .get("processed_input_items")
            .map(|value| serde_json::from_value::<usize>(value.clone()))
            .transpose()
            .map_err(|error| AgentRunError::new("parent_turn_invalid", error.to_string()))?
            .unwrap_or(0);
        if processed_input_items > transcript.len() {
            return Err(AgentRunError::new(
                "parent_turn_invalid",
                "Processed input boundary is outside the Agent transcript",
            ));
        }
        let definition_id = payload
            .payload
            .get("definition_id")
            .and_then(Value::as_str)
            .map(AgentDefinitionId::new)
            .ok_or_else(|| {
                AgentRunError::new("parent_turn_invalid", "Parent Turn has no Definition ID")
            })?;
        let revision = payload
            .payload
            .get("definition_revision")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| {
                AgentRunError::new(
                    "parent_turn_invalid",
                    "Parent Turn has no Definition Revision",
                )
            })?;
        let model_id = payload
            .payload
            .get("model_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                AgentRunError::new("parent_turn_invalid", "Parent Turn has no Model snapshot")
            })?;
        let thinking_level = payload
            .payload
            .get("thinking_level")
            .map(|value| {
                serde_json::from_value::<Option<stravia_runtime_contract::thinking::ThinkingLevel>>(
                    value.clone(),
                )
                .map_err(|error| AgentRunError::new("parent_turn_invalid", error.to_string()))
            })
            .transpose()?
            .flatten();
        Ok(ParentAgentContext {
            transcript,
            processed_input_items,
            snapshot: Some(ParentAgentSnapshot {
                definition_id,
                definition_revision: revision,
                model_id,
                thinking_level,
            }),
            root_turn_id: Some(root_turn_id),
            state_checkpoint_id,
            context_window_id,
            context_window_index,
            user_prompt_index,
            user_turn_index,
            client_conversation_id,
            client_conversation_created_at_ms,
        })
    }
}
