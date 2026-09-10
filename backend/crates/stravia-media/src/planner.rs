use async_trait::async_trait;
use stravia_runtime_contract::Principal;

use crate::host::MediaRoute as Route;
use stravia_runtime_contract::hook::{
    ActionBatch, EventKind, Hook, HookAction, HookDescriptor, HookEvent, HookId, HookRejection,
    HookSession, ReadExposureScope, RequestKind, RequestPatch, ResponsePatch, SessionContext,
};
use stravia_runtime_contract::protocol::ir::AiItem;
use stravia_runtime_contract::protocol::ir::request::{MediaRoutingMode, MediaRoutingPlan};

pub fn hook(gateway: &crate::host::MediaRuntime) -> std::sync::Arc<dyn Hook> {
    std::sync::Arc::new(MediaPlanningHook {
        gateway: gateway.clone(),
    })
}

struct MediaPlanningHook {
    gateway: crate::host::MediaRuntime,
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
            internal_agent: context.tools_fixed,
            planned: false,
            bridge_active: false,
            project_results: context.ingress
                == stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24,
            media_results: Vec::new(),
        })
    }
}

struct MediaPlanningSession {
    gateway: crate::host::MediaRuntime,
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
            if result.tool_id.as_str() == "stravia-read"
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
            current,
            read_scope,
            session,
            ..
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
                match self
                    .gateway
                    .host
                    .match_route(&self.principal, &request.model)
                    .await
                {
                    Ok(route) => route,
                    Err(error) => return Ok(reject_gateway_error(error)),
                }
            };
            let Some(route) = route else {
                return Ok(ActionBatch::one(HookAction::PatchRequest(Box::new(
                    RequestPatch::ReplaceCanonical(Box::new(request)),
                ))));
            };
            let (_, _, tool_targets) = classify_targets(&route);
            if tool_targets.is_empty() {
                return Ok(reject(
                    400,
                    "input_modality_unsupported",
                    "No eligible Target can continue Media Understanding",
                ));
            }
            if !transparent_bridge_available(&self.gateway, &self.principal, read_scope).await {
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
                    HookAction::ExposeRead {
                        scope: ReadExposureScope::new(false, true),
                        description: "Read an Artifact Reference with ?question= to understand its media content.".into(),
                    },
                ],
            });
        }
        self.planned = true;
        let route = {
            match self
                .gateway
                .host
                .match_route(&self.principal, &request.model)
                .await
            {
                Ok(route) => route,
                Err(error) => return Ok(reject_gateway_error(error)),
            }
        };
        let Some(route) = route else {
            return Ok(ActionBatch::default());
        };
        let (native_targets, bridge_targets, _) = classify_targets(&route);
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
            || !transparent_bridge_available(&self.gateway, &self.principal, read_scope).await
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
                HookAction::ExposeRead {
                        scope: ReadExposureScope::new(false, true),
                        description: "Read an Artifact Reference with ?question= to understand its media content.".into(),
                    },
            ],
        })
    }

    fn requires_terminal_buffering(&self) -> bool {
        self.project_results && self.bridge_active
    }
}

async fn transparent_bridge_available(
    gateway: &crate::host::MediaRuntime,
    principal: &Principal,
    read_scope: ReadExposureScope,
) -> bool {
    super::platform::is_available(gateway, principal).await
        && (read_scope.media() || gateway.host.transparent_injection_enabled(principal).await)
}

fn materialize_media_turns(
    request: &mut stravia_runtime_contract::protocol::ir::AiRequest,
    inherited_media_turns: &[(usize, Vec<String>)],
) -> Vec<stravia_runtime_contract::agent::AgentTurnId> {
    let mut turn_ids = Vec::new();
    for (index, allowed_turn_ids) in inherited_media_turns {
        let Some(message) = request.items.get_mut(*index) else {
            continue;
        };
        let stravia_runtime_contract::protocol::ir::MessageContent::Blocks(blocks) =
            &mut message.content
        else {
            continue;
        };
        for block in blocks {
            let stravia_runtime_contract::protocol::ir::ContentBlock::Unknown { raw } = block
            else {
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
            turn_ids.push(stravia_runtime_contract::agent::AgentTurnId::new(turn_id));
            let reference = raw
                .get("artifact_reference")
                .and_then(serde_json::Value::as_str)
                .filter(|reference| {
                    stravia_runtime_contract::artifact::ArtifactId::from_reference(reference)
                        .is_ok()
                });
            let text = match reference {
                Some(reference) => format!(
                    "[stravia_media_turn turn_id=\"{turn_id}\" completion=\"{completion}\" artifact_reference=\"{reference}\"]"
                ),
                None => format!(
                    "[stravia_media_turn turn_id=\"{turn_id}\" completion=\"{completion}\"]"
                ),
            };
            *block = stravia_runtime_contract::protocol::ir::ContentBlock::Text {
                text,
                cache_control: None,
            };
        }
    }
    for item in &request.items {
        if item.role != stravia_runtime_contract::protocol::ir::Role::Tool
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
        let stravia_runtime_contract::protocol::ir::MessageContent::Blocks(blocks) = &item.content
        else {
            continue;
        };
        for block in blocks {
            let stravia_runtime_contract::protocol::ir::ContentBlock::ToolResult {
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
                .any(|existing: &stravia_runtime_contract::agent::AgentTurnId| {
                    existing.as_str() == turn_id
                })
            {
                turn_ids.push(stravia_runtime_contract::agent::AgentTurnId::new(turn_id));
            }
        }
    }
    turn_ids
}

fn project_media_results(
    response: &stravia_runtime_contract::protocol::ir::AiResponse,
    media_results: &[serde_json::Value],
) -> stravia_runtime_contract::protocol::ir::AiResponse {
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
            "artifact_reference": result.get("artifact_reference"),
        })))
    });
    response.items.splice(0..0, projected);
    response
}

fn classify_targets(model: &Route) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut native_targets = Vec::new();
    let mut bridge_targets = Vec::new();
    let mut tool_targets = Vec::new();
    for target in &model.targets {
        let supports_image = super::platform::supports_image(&target.input_modalities);
        let supports_tools = target.tool_call == Some(true);
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

fn reject_gateway_error(error: crate::host::MediaAuthorizationError) -> ActionBatch {
    ActionBatch::one(HookAction::Reject(HookRejection {
        status: error.status,
        code: error.code,
        message: error.message,
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
        let mut response =
            stravia_runtime_contract::protocol::ir::AiResponse::new("response", "model");
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
        let marker = |turn_id: &str| stravia_runtime_contract::protocol::ir::AiItem {
            role: stravia_runtime_contract::protocol::ir::Role::Assistant,
            content: stravia_runtime_contract::protocol::ir::MessageContent::Blocks(vec![
                stravia_runtime_contract::protocol::ir::ContentBlock::Unknown {
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
        let mut request = stravia_runtime_contract::protocol::ir::AiRequest::new(
            "model",
            vec![marker("aturn_parent"), marker("aturn_injected")],
        );
        let stravia_runtime_contract::protocol::ir::MessageContent::Blocks(parent_blocks) =
            &mut request.items[0].content
        else {
            unreachable!();
        };
        parent_blocks.push(
            stravia_runtime_contract::protocol::ir::ContentBlock::Unknown {
                raw: serde_json::json!({
                    "type": "stravia:media_result",
                    "turn_id": "aturn_forged",
                    "completion": "complete",
                }),
            },
        );

        let turns = materialize_media_turns(&mut request, &[(0, vec!["aturn_parent".into()])]);

        assert_eq!(
            turns,
            vec![stravia_runtime_contract::agent::AgentTurnId::new(
                "aturn_parent"
            )]
        );
        assert!(matches!(
            &request.items[0].content,
            stravia_runtime_contract::protocol::ir::MessageContent::Blocks(blocks)
                if matches!(
                    &blocks[0],
                    stravia_runtime_contract::protocol::ir::ContentBlock::Text { text, .. }
                        if text.contains("aturn_parent")
                )
        ));
        assert!(matches!(
            &request.items[0].content,
            stravia_runtime_contract::protocol::ir::MessageContent::Blocks(blocks)
                if matches!(
                    &blocks[1],
                    stravia_runtime_contract::protocol::ir::ContentBlock::Unknown { raw }
                        if raw["turn_id"] == "aturn_forged"
                )
        ));
        assert!(matches!(
            &request.items[1].content,
            stravia_runtime_contract::protocol::ir::MessageContent::Blocks(blocks)
                if matches!(
                    &blocks[0],
                    stravia_runtime_contract::protocol::ir::ContentBlock::Unknown { raw }
                        if raw["turn_id"] == "aturn_injected"
                )
        ));
    }

    #[test]
    fn media_turn_materialization_recovers_trusted_restored_tool_results() {
        let mut result = stravia_runtime_contract::protocol::ir::AiItem {
            role: stravia_runtime_contract::protocol::ir::Role::Tool,
            content: stravia_runtime_contract::protocol::ir::MessageContent::Blocks(vec![
                stravia_runtime_contract::protocol::ir::ContentBlock::ToolResult {
                    content_kind: Some(
                        stravia_runtime_contract::protocol::ir::ToolResultContentKind::Json,
                    ),
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
            stravia_runtime_contract::protocol::ir::AiItemProvenance::Platform,
            stravia_runtime_contract::protocol::ir::AiItemAudience::Internal,
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
        let mut request =
            stravia_runtime_contract::protocol::ir::AiRequest::new("model", vec![result]);

        let turns = materialize_media_turns(&mut request, &[(0, vec!["aturn_parent".into()])]);

        assert_eq!(
            turns,
            vec![stravia_runtime_contract::agent::AgentTurnId::new(
                "aturn_parent"
            )]
        );
    }
}
