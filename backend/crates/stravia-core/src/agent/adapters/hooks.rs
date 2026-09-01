use super::*;

pub(crate) struct AgentDefinitionHook {
    definitions: AgentDefinitionRegistry,
}

impl AgentDefinitionHook {
    pub(crate) fn new(definitions: AgentDefinitionRegistry) -> Self {
        Self { definitions }
    }
}

impl Hook for AgentDefinitionHook {
    fn descriptor(&self) -> HookDescriptor {
        HookDescriptor {
            id: crate::hook::HookId::new("agent-definitions"),
            request_kinds: vec![RequestKind::Generation],
            event_kinds: vec![
                EventKind::Request,
                EventKind::ToolResult,
                EventKind::ClientOutput,
            ],
            requires_full_context: false,
            max_buffered_bytes: 0,
            max_delayed_events: 0,
        }
    }

    fn create_session(&self, context: &SessionContext) -> Box<dyn HookSession> {
        Box::new(AgentDefinitionHookSession {
            definitions: self.definitions.clone(),
            expose_turn_ids: context.ingress == crate::protocol::ids::OPEN_RESPONSES_2026_04_24,
            turn_ids: Vec::new(),
        })
    }
}

struct AgentDefinitionHookSession {
    definitions: AgentDefinitionRegistry,
    expose_turn_ids: bool,
    turn_ids: Vec<String>,
}

#[async_trait]
impl HookSession for AgentDefinitionHookSession {
    async fn handle(&mut self, event: HookEvent<'_>) -> Result<ActionBatch, String> {
        match event {
            HookEvent::Request { .. } => {
                let actions = self
                    .definitions
                    .list_public()
                    .await
                    .into_iter()
                    .filter(|record| record.config.enabled && record.config.model_id.is_some())
                    .map(|record| {
                        HookAction::ExposeTool(ToolId::new(format!(
                            "agent-definition:{}",
                            record.spec.id.as_str()
                        )))
                    })
                    .collect();
                Ok(ActionBatch { actions })
            }
            HookEvent::ToolResult { result, .. }
                if self.expose_turn_ids
                    && result.tool_id.as_str().starts_with("agent-definition:") =>
            {
                if let Some(turn_id) = result.content.get("turn_id").and_then(Value::as_str) {
                    self.turn_ids.push(turn_id.to_owned());
                }
                Ok(ActionBatch::default())
            }
            HookEvent::ClientOutput { response, .. } if !self.turn_ids.is_empty() => {
                let mut response = response.clone();
                response
                    .items
                    .extend(self.turn_ids.drain(..).map(|turn_id| {
                        crate::protocol::ir::AiItem::unknown(serde_json::json!({
                            "id": format!("agent_{turn_id}"),
                            "type": "stravia:agent_result",
                            "status": "completed",
                            "turn_id": turn_id,
                        }))
                    }));
                Ok(ActionBatch::one(HookAction::PatchResponse(
                    ResponsePatch::ReplaceCanonical(Box::new(response)),
                )))
            }
            _ => Ok(ActionBatch::default()),
        }
    }
}
