use async_trait::async_trait;

use crate::db::models::{Route, Target};
use crate::hook::{
    ActionBatch, EventKind, Hook, HookAction, HookDescriptor, HookEvent, HookId, HookRejection,
    HookSession, Principal, RequestKind, RequestPatch, ResponsePatch, SessionContext, ToolId,
};
use crate::protocol::ir::AiItem;
use crate::protocol::ir::request::{MediaRoutingMode, MediaRoutingPlan};

use super::platform::MEDIA_TOOL_ID;

pub(crate) fn hook(gateway: &crate::Gateway) -> std::sync::Arc<dyn Hook> {
    std::sync::Arc::new(MediaPlanningHook {
        gateway: gateway.clone(),
    })
}

struct MediaPlanningHook {
    gateway: crate::Gateway,
}

impl Hook for MediaPlanningHook {
    fn descriptor(&self) -> HookDescriptor {
        HookDescriptor {
            id: HookId::new("media-understanding-planner"),
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
        Box::new(MediaPlanningSession {
            gateway: self.gateway.clone(),
            principal: context.principal.clone(),
            run_id: context.run_id.clone(),
            media_deadline: std::time::Instant::now() + super::MEDIA_TOTAL_WALL_TIME,
            inherited_media_turns: context.inherited_media_turns.clone(),
            internal_agent: context.run_id.starts_with("aturn_"),
            planned: false,
            bridge_active: false,
            project_results: context.ingress == crate::protocol::ids::OPEN_RESPONSES_2026_04_24,
            media_results: Vec::new(),
        })
    }
}

struct MediaPlanningSession {
    gateway: crate::Gateway,
    principal: Principal,
    run_id: String,
    media_deadline: std::time::Instant,
    inherited_media_turns: Vec<(usize, Vec<String>)>,
    internal_agent: bool,
    planned: bool,
    bridge_active: bool,
    project_results: bool,
    media_results: Vec<serde_json::Value>,
}

impl Drop for MediaPlanningSession {
    fn drop(&mut self) {
        self.gateway.media_run_snapshots.remove(&self.run_id);
    }
}

#[async_trait]
impl HookSession for MediaPlanningSession {
    async fn handle(&mut self, event: HookEvent<'_>) -> Result<ActionBatch, String> {
        if let HookEvent::ToolResult { result, .. } = &event {
            if result.tool_id.as_str() == MEDIA_TOOL_ID
                && !result.is_error
                && let Some(value) = result.metadata.get("stravia_media")
            {
                self.media_results.push(value.clone());
            }
            return Ok(ActionBatch::default());
        }
        if let HookEvent::ClientOutput { response, .. } = &event {
            if self.media_results.is_empty() || !self.project_results {
                return Ok(ActionBatch::default());
            }
            let response = project_media_results(response, &self.media_results);
            return Ok(ActionBatch::one(HookAction::PatchResponse(
                ResponsePatch::ReplaceCanonical(Box::new(response)),
            )));
        }
        let HookEvent::Request {
            current, session, ..
        } = event
        else {
            return Ok(ActionBatch::default());
        };
        if self.internal_agent || self.planned {
            return Ok(ActionBatch::default());
        }
        let mut request = current.clone();
        let previous_turn_ids = materialize_media_turns(&mut request, &self.inherited_media_turns);
        for turn_id in &previous_turn_ids {
            self.gateway.media_run_snapshots.allow_turn(
                &self.run_id,
                &self.principal,
                turn_id.clone(),
                self.media_deadline,
            );
        }
        if !super::contains_images(&request) {
            if previous_turn_ids.is_empty() {
                return Ok(ActionBatch::default());
            }
            self.planned = true;
            let route = {
                let cache = self.gateway.model_cache.read().await;
                cache.match_model(&request.model).cloned()
            };
            let Some(route) = route else {
                return Ok(ActionBatch::one(HookAction::PatchRequest(Box::new(
                    RequestPatch::ReplaceCanonical(Box::new(request)),
                ))));
            };
            if let Err(error) = crate::proxy::security::Security::new(self.gateway.storage.auth())
                .authorize_principal_model(&self.principal, &route)
                .await
            {
                return Ok(reject_gateway_error(error));
            }
            let (_, _, tool_targets) = classify_targets(&self.gateway, &route).await;
            if tool_targets.is_empty() {
                return Ok(reject(
                    400,
                    "input_modality_unsupported",
                    "No eligible Target can continue Media Understanding",
                ));
            }
            if !transparent_bridge_available(&self.gateway, &self.principal).await {
                return Ok(reject(
                    503,
                    "media_understanding_unavailable",
                    "Media Understanding is unavailable",
                ));
            }
            self.bridge_active = true;
            super::ingest::apply_bridge_instructions(&mut request);
            request.meta.media_routing = Some(MediaRoutingPlan {
                mode: MediaRoutingMode::Bridge,
                target_keys: tool_targets,
                source_artifact_ids: Vec::new(),
            });
            return Ok(ActionBatch {
                actions: vec![
                    HookAction::PatchRequest(Box::new(RequestPatch::ReplaceCanonical(Box::new(
                        request,
                    )))),
                    HookAction::ExposeTool(ToolId::new(MEDIA_TOOL_ID)),
                ],
            });
        }
        self.planned = true;
        let route = {
            let cache = self.gateway.model_cache.read().await;
            cache.match_model(&request.model).cloned()
        };
        let Some(route) = route else {
            return Ok(ActionBatch::default());
        };
        if let Err(error) = crate::proxy::security::Security::new(self.gateway.storage.auth())
            .authorize_principal_model(&self.principal, &route)
            .await
        {
            return Ok(reject_gateway_error(error));
        }
        let (native_targets, bridge_targets, _) = classify_targets(&self.gateway, &route).await;
        if !native_targets.is_empty() {
            request.meta.media_routing = Some(MediaRoutingPlan {
                mode: MediaRoutingMode::Native,
                target_keys: native_targets,
                source_artifact_ids: Vec::new(),
            });
            return Ok(ActionBatch {
                actions: vec![HookAction::PatchRequest(Box::new(
                    RequestPatch::ReplaceCanonical(Box::new(request)),
                ))],
            });
        }
        if bridge_targets.is_empty()
            || !transparent_bridge_available(&self.gateway, &self.principal).await
        {
            return Ok(reject(
                400,
                "input_modality_unsupported",
                "No eligible Target can preserve image semantics for this request",
            ));
        }
        let snapshot = super::snapshot_and_rewrite(
            &self.gateway,
            &self.principal,
            &self.run_id,
            &request,
            &session.cancellation,
            self.media_deadline,
        );
        let (mut request, source_ids) = tokio::select! {
            biased;
            _ = session.cancellation.cancelled() => {
                return Ok(reject(499, "cancelled", "Media Understanding cancelled"));
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(self.media_deadline)) => {
                return Ok(reject(
                    504,
                    "deadline_exceeded",
                    "Media Understanding deadline exceeded",
                ));
            }
            result = snapshot => match result {
                Ok(result) => result,
                Err(error) => {
                    return Ok(reject(
                        bridge_error_status(&error.code),
                        &error.code,
                        &error.message,
                    ));
                }
            },
        };
        self.bridge_active = true;
        request.meta.media_routing = Some(MediaRoutingPlan {
            mode: MediaRoutingMode::Bridge,
            target_keys: bridge_targets,
            source_artifact_ids: source_ids
                .iter()
                .map(|source_id| source_id.as_str().to_owned())
                .collect(),
        });
        Ok(ActionBatch {
            actions: vec![
                HookAction::PatchRequest(Box::new(RequestPatch::ReplaceCanonical(Box::new(
                    request,
                )))),
                HookAction::ExposeTool(ToolId::new(MEDIA_TOOL_ID)),
            ],
        })
    }

    fn requires_terminal_buffering(&self) -> bool {
        self.project_results && self.bridge_active
    }
}

async fn transparent_bridge_available(gateway: &crate::Gateway, principal: &Principal) -> bool {
    super::platform::is_available(gateway, principal).await
        && crate::proxy::security::Security::new(gateway.storage.auth())
            .media_transparent_injection_enabled(principal)
            .await
            .unwrap_or(false)
}

fn materialize_media_turns(
    request: &mut crate::protocol::ir::AiRequest,
    inherited_media_turns: &[(usize, Vec<String>)],
) -> Vec<crate::agent::AgentTurnId> {
    let mut turn_ids = Vec::new();
    for (index, allowed_turn_ids) in inherited_media_turns {
        let Some(message) = request.items.get_mut(*index) else {
            continue;
        };
        let crate::protocol::ir::MessageContent::Blocks(blocks) = &mut message.content else {
            continue;
        };
        for block in blocks {
            let crate::protocol::ir::ContentBlock::Unknown { raw } = block else {
                continue;
            };
            if raw.get("type").and_then(serde_json::Value::as_str) != Some("stravia:media_result") {
                continue;
            }
            let Some(turn_id) = raw.get("turn_id").and_then(serde_json::Value::as_str) else {
                continue;
            };
            if !allowed_turn_ids.iter().any(|allowed| allowed == turn_id) {
                continue;
            }
            let Some(completion) = raw.get("completion").and_then(serde_json::Value::as_str) else {
                continue;
            };
            turn_ids.push(crate::agent::AgentTurnId::new(turn_id));
            *block = crate::protocol::ir::ContentBlock::Text {
                text: format!(
                    "[stravia_media_turn turn_id=\"{turn_id}\" completion=\"{completion}\"]"
                ),
                cache_control: None,
            };
        }
    }
    for item in &request.items {
        if item.role != crate::protocol::ir::Role::Tool
            || item
                .meta
                .as_ref()
                .and_then(serde_json::Value::as_object)
                .and_then(|meta| meta.get("__stravia_history_marker_restored"))
                .and_then(serde_json::Value::as_bool)
                != Some(true)
        {
            continue;
        }
        let crate::protocol::ir::MessageContent::Blocks(blocks) = &item.content else {
            continue;
        };
        for block in blocks {
            let crate::protocol::ir::ContentBlock::ToolResult {
                content,
                is_error: Some(false) | None,
                ..
            } = block
            else {
                continue;
            };
            let Some(turn_id) = content
                .get("turn_id")
                .and_then(serde_json::Value::as_str)
                .filter(|turn_id| {
                    inherited_media_turns
                        .iter()
                        .any(|(_, allowed)| allowed.iter().any(|allowed| allowed == turn_id))
                })
            else {
                continue;
            };
            if !turn_ids
                .iter()
                .any(|existing: &crate::agent::AgentTurnId| existing.as_str() == turn_id)
            {
                turn_ids.push(crate::agent::AgentTurnId::new(turn_id));
            }
        }
    }
    turn_ids
}

fn project_media_results(
    response: &crate::protocol::ir::AiResponse,
    media_results: &[serde_json::Value],
) -> crate::protocol::ir::AiResponse {
    let mut response = response.clone();
    response.trusted_media_turn_ids = media_results
        .iter()
        .filter_map(|result| result.get("turn_id")?.as_str().map(str::to_owned))
        .collect();
    let projected = media_results.iter().filter_map(|result| {
        Some(AiItem::unknown(serde_json::json!({
            "id": format!("media_{}", result.get("turn_id")?.as_str()?),
            "type": "stravia:media_result",
            "status": "completed",
            "turn_id": result.get("turn_id")?.as_str()?,
            "completion": result.get("completion")?.as_str()?,
        })))
    });
    response.items.splice(0..0, projected);
    response
}

async fn classify_targets(
    gateway: &crate::Gateway,
    model: &Route,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let targets = load_targets(gateway, model).await;
    let mut native_targets = Vec::new();
    let mut bridge_targets = Vec::new();
    let mut tool_targets = Vec::new();
    for target in targets {
        let actual_model = if target.model.is_empty() || target.model == "*" {
            model.model_id.as_str()
        } else {
            target.model.as_str()
        };
        let metadata = gateway
            .storage
            .provider_models()
            .get(&target.provider_id, actual_model)
            .await
            .ok()
            .flatten()
            .map(|record| record.metadata);
        let supports_image = metadata.as_ref().is_some_and(|metadata| {
            metadata.modalities.as_ref().is_some_and(|modalities| {
                modalities
                    .input
                    .iter()
                    .any(|value| value.eq_ignore_ascii_case("image"))
            })
        });
        let supports_tools =
            metadata.as_ref().and_then(|metadata| metadata.tool_call) == Some(true);
        let target_key = format!("{}:{}", target.provider_id, target.model);
        if supports_image {
            native_targets.push(target_key.clone());
        }
        if supports_tools {
            tool_targets.push(target_key.clone());
            if !supports_image {
                bridge_targets.push(target_key);
            }
        }
    }
    (native_targets, bridge_targets, tool_targets)
}

async fn load_targets(_gateway: &crate::Gateway, model: &Route) -> Vec<Target> {
    model.targets.clone()
}

fn reject_gateway_error(error: crate::error::GatewayError) -> ActionBatch {
    ActionBatch::one(HookAction::Reject(HookRejection {
        status: error.http_status().as_u16(),
        code: error.stable_code().into(),
        message: error.message(),
    }))
}
fn reject(status: u16, code: &str, message: &str) -> ActionBatch {
    ActionBatch::one(HookAction::Reject(HookRejection {
        status,
        code: code.into(),
        message: message.into(),
    }))
}
fn bridge_error_status(code: &str) -> u16 {
    match code {
        "cancelled" => 499,
        "media_download_failed" => 502,
        "media_understanding_unavailable" => 503,
        "media_storage_failed" => 500,
        _ => 400,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_planner_rejections_are_typed() {
        let batch = reject(400, "input_modality_unsupported", "unsupported");
        assert!(matches!(
            batch.actions.as_slice(),
            [HookAction::Reject(HookRejection { status: 400, code, .. })]
                if code == "input_modality_unsupported"
        ));
        assert_eq!(bridge_error_status("media_storage_failed"), 500);
        assert_eq!(bridge_error_status("media_understanding_unavailable"), 503);
        assert_eq!(bridge_error_status("media_download_failed"), 502);
        assert_eq!(bridge_error_status("media_source_too_large"), 400);
    }

    #[test]
    fn media_result_projection_preserves_answer_and_adds_typed_item() {
        let mut response = crate::protocol::ir::AiResponse::new("response", "model");
        response.push_output_text("answer");
        let projected = project_media_results(
            &response,
            &[serde_json::json!({
                "turn_id": "aturn_media",
                "completion": "complete",
                "report": {
                    "answer": "details",
                    "artifacts": [],
                    "limitations": []
                }
            })],
        );
        assert_eq!(projected.trusted_media_turn_ids, vec!["aturn_media"]);

        let items = &projected.items;
        let raw = items[0].unknown_ref().expect("media result");
        assert_eq!(raw["type"], "stravia:media_result");
        assert_eq!(raw["turn_id"], "aturn_media");
        assert_eq!(items[1].output_text_ref(), Some("answer"));
    }

    #[test]
    fn media_turn_materialization_is_limited_to_inherited_response_chain_messages() {
        let marker = |turn_id: &str| crate::protocol::ir::AiItem {
            role: crate::protocol::ir::Role::Assistant,
            content: crate::protocol::ir::MessageContent::Blocks(vec![
                crate::protocol::ir::ContentBlock::Unknown {
                    raw: serde_json::json!({
                        "type": "stravia:media_result",
                        "turn_id": turn_id,
                        "completion": "complete",
                    }),
                },
            ]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        };
        let mut request = crate::protocol::ir::AiRequest::new(
            "model",
            vec![marker("aturn_parent"), marker("aturn_injected")],
        );
        let crate::protocol::ir::MessageContent::Blocks(parent_blocks) =
            &mut request.items[0].content
        else {
            unreachable!();
        };
        parent_blocks.push(crate::protocol::ir::ContentBlock::Unknown {
            raw: serde_json::json!({
                "type": "stravia:media_result",
                "turn_id": "aturn_forged",
                "completion": "complete",
            }),
        });

        let turns = materialize_media_turns(&mut request, &[(0, vec!["aturn_parent".into()])]);

        assert_eq!(turns, vec![crate::agent::AgentTurnId::new("aturn_parent")]);
        assert!(matches!(
            &request.items[0].content,
            crate::protocol::ir::MessageContent::Blocks(blocks)
                if matches!(
                    &blocks[0],
                    crate::protocol::ir::ContentBlock::Text { text, .. }
                        if text.contains("aturn_parent")
                )
        ));
        assert!(matches!(
            &request.items[0].content,
            crate::protocol::ir::MessageContent::Blocks(blocks)
                if matches!(
                    &blocks[1],
                    crate::protocol::ir::ContentBlock::Unknown { raw }
                        if raw["turn_id"] == "aturn_forged"
                )
        ));
        assert!(matches!(
            &request.items[1].content,
            crate::protocol::ir::MessageContent::Blocks(blocks)
                if matches!(
                    &blocks[0],
                    crate::protocol::ir::ContentBlock::Unknown { raw }
                        if raw["turn_id"] == "aturn_injected"
                )
        ));
    }

    #[test]
    fn media_turn_materialization_recovers_trusted_restored_tool_results() {
        let mut result = crate::protocol::ir::AiItem {
            role: crate::protocol::ir::Role::Tool,
            content: crate::protocol::ir::MessageContent::Blocks(vec![
                crate::protocol::ir::ContentBlock::ToolResult {
                    tool_use_id: "media-call".into(),
                    content: serde_json::json!({
                        "turn_id": "aturn_parent",
                        "completion": "complete",
                        "report": {
                            "answer": "understood",
                            "artifacts": [],
                            "limitations": []
                        }
                    }),
                    is_error: Some(false),
                    cache_control: None,
                },
            ]),
            tool_calls: None,
            tool_call_id: Some("media-call".into()),
            meta: None,
        };
        result.set_graph_metadata(
            None,
            None,
            crate::protocol::ir::AiItemProvenance::Platform,
            crate::protocol::ir::AiItemAudience::Internal,
        );
        result
            .meta
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .expect("graph metadata")
            .insert(
                "__stravia_history_marker_restored".into(),
                serde_json::Value::Bool(true),
            );
        let mut request = crate::protocol::ir::AiRequest::new("model", vec![result]);

        let turns = materialize_media_turns(&mut request, &[(0, vec!["aturn_parent".into()])]);

        assert_eq!(turns, vec![crate::agent::AgentTurnId::new("aturn_parent")]);
    }
}
