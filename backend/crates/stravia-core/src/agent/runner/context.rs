use super::*;

#[derive(Default)]
pub(super) struct ParentAgentContext {
    pub(super) transcript: Vec<AiItem>,
    pub(super) snapshot: Option<ParentAgentSnapshot>,
    pub(super) root_turn_id: Option<AgentTurnId>,
}

pub(super) struct ParentAgentSnapshot {
    pub(super) definition_id: AgentDefinitionId,
    pub(super) definition_revision: u32,
    pub(super) model_id: String,
    pub(super) thinking_level: Option<crate::thinking::ThinkingLevel>,
}

impl AgentRunner {
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
                let ContentBlock::Image {
                    source: MediaSource::FileId { file_id, .. },
                    ..
                } = block
                else {
                    continue;
                };
                let Some(id) = file_id.strip_prefix("stravia-artifact:") else {
                    continue;
                };
                let id = ArtifactId::new(id);
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
            let source = MediaSource::FileId {
                file_id: format!("stravia-artifact:{}", id.as_str()),
                detail: None,
            };
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
                let Some(artifact_id) = file_id.strip_prefix("stravia-artifact:") else {
                    continue;
                };
                let store = self.artifacts.as_ref().ok_or_else(|| {
                    AgentRunError::new(
                        "artifact_store_unavailable",
                        "Agent Turn references Artifacts but no ArtifactStore is configured",
                    )
                })?;
                let reader = store
                    .open(principal, &ArtifactId::new(artifact_id))
                    .await
                    .map_err(|error| {
                        AgentRunError::new("artifact_unavailable", error.to_string())
                    })?;
                *source = match reader.source {
                    ArtifactSource::HttpsUrl(url) => MediaSource::Url(url),
                    ArtifactSource::LocalPath(path) => {
                        let bytes = tokio::fs::read(path).await.map_err(|error| {
                            AgentRunError::new("artifact_read_failed", error.to_string())
                        })?;
                        MediaSource::Base64 {
                            media_type: reader.artifact.mime_type,
                            data: base64::engine::general_purpose::STANDARD.encode(bytes),
                        }
                    }
                };
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
        let root_turn_id = chain
            .first()
            .map(|node| node.id.clone())
            .ok_or_else(|| AgentRunError::new("parent_turn_unavailable", "Parent Turn is empty"))?;
        let payload = chain
            .last()
            .ok_or_else(|| AgentRunError::new("parent_turn_unavailable", "Parent Turn is empty"))?;
        let transcript = serde_json::from_value(
            payload.payload.get("transcript").cloned().ok_or_else(|| {
                AgentRunError::new(
                    "parent_turn_invalid",
                    "Parent Turn has no canonical transcript",
                )
            })?,
        )
        .map_err(|error| AgentRunError::new("parent_turn_invalid", error.to_string()))?;
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
                serde_json::from_value::<Option<crate::thinking::ThinkingLevel>>(value.clone())
                    .map_err(|error| AgentRunError::new("parent_turn_invalid", error.to_string()))
            })
            .transpose()?
            .flatten();
        Ok(ParentAgentContext {
            transcript,
            snapshot: Some(ParentAgentSnapshot {
                definition_id,
                definition_revision: revision,
                model_id,
                thinking_level,
            }),
            root_turn_id: Some(root_turn_id),
        })
    }
}
