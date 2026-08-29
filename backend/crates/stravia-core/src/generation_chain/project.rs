use super::*;

pub(super) fn project_client_output(
    ingress: Option<ProtocolId>,
    response: &AiResponse,
    prefix: &mut [AiItem],
) -> Result<Vec<AiItem>, TurnCommitError> {
    let Some(ingress) = ingress else {
        return Ok(generic_client_history_output(response));
    };
    project_client_history(ingress, response, prefix).map_err(TurnCommitError::Storage)
}

pub(super) fn project_client_history(
    ingress: ProtocolId,
    response: &AiResponse,
    prefix: &mut [AiItem],
) -> Result<Vec<AiItem>, String> {
    use crate::protocol::ids::{
        ANTHROPIC_MESSAGES_2023_06_01, GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        OPEN_RESPONSES_2026_04_24, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
    };
    if ingress == OPEN_RESPONSES_2026_04_24 {
        let _ = prefix;
        return Ok(
            crate::protocol::codec::open_responses::formatter::stamp_output_graph_ids(response),
        );
    }
    if ingress == OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1 {
        let _ = prefix;
        return Ok(project_chat_history(response));
    }
    if ingress == ANTHROPIC_MESSAGES_2023_06_01 {
        let _ = prefix;
        return Ok(project_anthropic_history(response));
    }
    if ingress == GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA {
        return Ok(project_gemini_history(response, prefix));
    }
    let _ = prefix;
    Ok(generic_client_history_output(response))
}

fn project_chat_history(response: &AiResponse) -> Vec<AiItem> {
    vec![crate::protocol::codec::openai::compatible::stream::client_history_output_item(response)]
}

fn project_anthropic_history(response: &AiResponse) -> Vec<AiItem> {
    if response
        .items
        .iter()
        .any(|item| item.role == Role::Assistant)
    {
        let mut item = response.to_assistant_item();
        crate::protocol::codec::anthropic::messages::stream::normalize_client_history_item(
            &mut item,
        );
        vec![item]
    } else {
        response.items.clone()
    }
}

fn project_gemini_history(response: &AiResponse, prefix: &mut [AiItem]) -> Vec<AiItem> {
    let mut output = generic_client_history_output(response);
    let prefix_len = prefix.len();
    let mut chain = prefix.to_vec();
    chain.extend(output);
    normalize_gemini_client_tool_ids(&mut chain);
    prefix.clone_from_slice(&chain[..prefix_len]);
    output = chain.split_off(prefix_len);
    output
}

fn normalize_gemini_client_tool_ids(items: &mut [AiItem]) {
    let mut ids = HashMap::<String, String>::new();
    let mut names = HashMap::<String, String>::new();

    for item in items {
        if let MessageContent::Blocks(blocks) = &mut item.content {
            blocks.retain(|block| {
                !matches!(
                    block,
                    ContentBlock::Thinking { .. } | ContentBlock::Reasoning { .. }
                )
            });
            for block in blocks {
                match block {
                    ContentBlock::ToolUse {
                        id, name, input, ..
                    } => {
                        let stable = stable_gemini_tool_id(name, input);
                        ids.insert(id.clone(), stable.clone());
                        names.insert(name.clone(), stable.clone());
                        *id = stable;
                    }
                    ContentBlock::ToolResult { tool_use_id, .. } => {
                        if let Some(stable) = ids
                            .get(tool_use_id)
                            .or_else(|| names.get(tool_use_id))
                            .cloned()
                        {
                            *tool_use_id = stable;
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Some(tool_calls) = item.tool_calls.as_mut() {
            for call in tool_calls {
                let arguments = serde_json::from_str(&call.arguments)
                    .unwrap_or_else(|_| serde_json::Value::String(call.arguments.clone()));
                let stable = stable_gemini_tool_id(&call.name, &arguments);
                ids.insert(call.id.clone(), stable.clone());
                names.insert(call.name.clone(), stable.clone());
                call.id = stable;
            }
        }
        if let Some(tool_call_id) = item.tool_call_id.as_mut()
            && let Some(stable) = ids
                .get(tool_call_id)
                .or_else(|| names.get(tool_call_id))
                .cloned()
        {
            *tool_call_id = stable;
        }
    }
}

fn stable_gemini_tool_id(name: &str, arguments: &serde_json::Value) -> String {
    let identity = serde_json::to_vec(&serde_json::json!({
        "name": name,
        "arguments": arguments,
    }))
    .expect("Gemini tool identity is JSON serializable");
    format!(
        "gemini_call_{}",
        crate::protocol::ir::canonical::hash_hex(&crate::protocol::ir::canonical::hash_bytes(
            &identity
        ))
    )
}

pub(super) fn generic_client_history_output(response: &AiResponse) -> Vec<AiItem> {
    if response
        .items
        .iter()
        .any(|item| item.role == Role::Assistant)
    {
        vec![response.to_assistant_item()]
    } else {
        response.items.clone()
    }
}

pub(super) fn item_reference_id(item: &AiItem) -> Option<&str> {
    item.meta
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .and_then(|meta| meta.get("__open_responses_item_reference"))
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
}

pub(super) fn item_reference_node_ids(ingress: ProtocolId, items: &[AiItem]) -> Vec<String> {
    if ingress != crate::protocol::ids::OPEN_RESPONSES_2026_04_24 {
        return Vec::new();
    }
    items
        .iter()
        .filter_map(item_reference_id)
        .filter_map(
            crate::protocol::codec::open_responses::formatter::response_id_from_gateway_item_id,
        )
        .collect()
}

pub(super) fn resolve_protocol_item_references(
    ingress: ProtocolId,
    items: &mut [AiItem],
    catalog: &[AiItem],
) -> Result<(), String> {
    if ingress == crate::protocol::ids::OPEN_RESPONSES_2026_04_24 {
        let mut index = std::collections::HashMap::<String, AiItem>::new();
        for item in catalog {
            if let Some(id) = item.id_ref() {
                index.insert(id.to_owned(), item.clone());
            }
        }
        for item in items {
            let Some(id) = item_reference_id(item) else {
                continue;
            };
            if let Some(resolved) = index.get(id) {
                *item = resolved.clone();
            }
        }
        return Ok(());
    }
    if items.iter().any(|item| item_reference_id(item).is_some()) {
        return Err("item_reference_not_found".into());
    }
    Ok(())
}

pub(super) fn generation_node_is_legal(response: &AiResponse) -> bool {
    let status = response
        .vendor
        .egress
        .get("__open_responses_terminal")
        .and_then(serde_json::Value::as_object)
        .and_then(|terminal| terminal.get("status"))
        .and_then(serde_json::Value::as_str);
    status.map_or_else(
        || response.error.is_none(),
        |status| matches!(status, "completed" | "incomplete"),
    )
}

pub(crate) fn generation_node_is_completed(response: &AiResponse) -> bool {
    response
        .vendor
        .egress
        .get("__open_responses_terminal")
        .and_then(serde_json::Value::as_object)
        .and_then(|terminal| terminal.get("status"))
        .and_then(serde_json::Value::as_str)
        .map_or_else(|| response.error.is_none(), |status| status == "completed")
}

pub(crate) fn mark_generation_target(
    response: &mut AiResponse,
    namespace: &str,
    protocol: ProtocolId,
    actual_model: &str,
) {
    response.vendor.egress.insert(
        "__stravia_generation_chain_target".into(),
        serde_json::json!({
            "namespace": namespace,
            "protocol": protocol.to_string(),
            "actual_model": actual_model,
        }),
    );
}

fn provider_effective_profile(
    response: &AiResponse,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    response
        .vendor
        .egress
        .get("__open_responses_provider_effective")
        .or_else(|| {
            response
                .vendor
                .ingress
                .get("__open_responses_response_profile")
        })
        .and_then(serde_json::Value::as_object)
        .cloned()
}

fn apply_provider_effective_request(
    request: &mut AiRequest,
    profile: &serde_json::Map<String, serde_json::Value>,
) {
    let Ok(effective) =
        crate::protocol::codec::open_responses::decoder::decode_effective_response_profile(
            &request.model,
            profile,
        )
    else {
        return;
    };
    request.generation = effective.generation;
    request.tools = effective.tools;
    request.tool_choice = effective.tool_choice;
    request.parallel_tool_calls = effective.parallel_tool_calls;
    request.reasoning = effective.reasoning;
    let Some(crate::protocol::ir::ProtocolExt::OpenResponses(source)) = effective.ext else {
        return;
    };
    let Some(crate::protocol::ir::ProtocolExt::OpenResponses(target)) = request.ext.as_mut() else {
        return;
    };
    target.max_tool_calls = source.max_tool_calls;
    target.top_logprobs = source.top_logprobs;
    target.truncation = source.truncation;
    target.text = source.text;
    target.service_tier = source.service_tier;
    target.tool_choice_ext = source.tool_choice_ext;
}

pub(super) fn attach_persisted_profile(
    response: &mut AiResponse,
    request: &mut AiRequest,
    previous_response_id: Option<&str>,
) {
    let mut profile =
        crate::protocol::codec::open_responses::encoder::response_profile_from_request(request);
    let provider_effective = provider_effective_profile(response);
    if let Some(profile) = profile.as_object_mut() {
        if let Some(provider_effective) = provider_effective {
            apply_provider_effective_request(request, &provider_effective);
            profile.extend(provider_effective);
        }
        profile.insert(
            "model".into(),
            serde_json::Value::String(request.model.clone()),
        );
        profile.insert(
            "previous_response_id".into(),
            previous_response_id
                .filter(|id| !id.is_empty())
                .map(|id| serde_json::Value::String(id.to_owned()))
                .unwrap_or(serde_json::Value::Null),
        );
    }
    response
        .vendor
        .ingress
        .insert("__open_responses_response_profile".into(), profile.clone());
    response
        .vendor
        .ingress
        .insert("__open_responses_effective_request".into(), profile);
}

fn history_catalog(
    persisted: &[(TurnNodeId, PersistedResponseNode)],
    ingress: Option<ProtocolId>,
) -> Result<Vec<AiItem>, String> {
    let mut catalog = Vec::new();
    for (_, node) in persisted {
        catalog.extend(node.client_delta.messages.clone());
        if let Some(output) = &node.client_output {
            catalog.extend(output.clone());
            continue;
        }
        if let Some(ingress) = ingress {
            catalog.extend(project_client_history(
                ingress,
                &node.effective_output,
                &mut [],
            )?);
        } else {
            catalog.extend(generic_client_history_output(&node.effective_output));
        }
    }
    Ok(catalog)
}

pub(super) fn resolve_item_references(
    messages: &mut [AiItem],
    persisted: &[(TurnNodeId, PersistedResponseNode)],
    ingress: Option<ProtocolId>,
) -> Result<(), String> {
    let catalog = history_catalog(persisted, ingress)?;
    if let Some(ingress) = ingress {
        resolve_protocol_item_references(ingress, messages, &catalog)?;
    } else if messages
        .iter()
        .any(|item| item_reference_id(item).is_some())
    {
        return Err("item_reference_not_found".into());
    }
    if item_reference_ids(messages).next().is_some() {
        return Err("item_reference_not_found".into());
    }
    Ok(())
}

pub(super) fn items_equal(left: &[AiItem], right: &[AiItem]) -> bool {
    crate::protocol::ir::canonical::history_items_equal(left, right)
}

pub(super) fn canonical_client_history_request(request: &AiRequest) -> AiRequest {
    let mut canonical = request.clone();
    if canonical.instructions.is_none() {
        let leading_developer_items = canonical
            .items
            .iter()
            .take_while(|item| item.role == crate::protocol::ir::Role::Developer)
            .count();
        let instructions = canonical.items[..leading_developer_items]
            .iter()
            .map(|item| item.content.to_text())
            .collect::<Vec<_>>()
            .join("\n");
        if !instructions.is_empty() {
            canonical.instructions = Some(instructions);
            canonical.items.drain(..leading_developer_items);
        }
    }

    if let Some(ingress) = ProtocolTransform::inferred_ingress(&canonical) {
        let mut items = std::mem::take(&mut canonical.items);
        if project_client_history(
            ingress,
            &AiResponse::new(String::new(), String::new()),
            &mut items,
        )
        .is_ok()
        {
            canonical.items = items;
        }
    }
    canonical
}

pub(super) fn remap_client_tool_result_ids(
    delta: &mut [AiItem],
    client_history: &[AiItem],
    effective_history: &[AiItem],
) {
    let client_calls = history_tool_calls(client_history);
    let effective_calls = history_tool_calls(effective_history);
    let mut ids = HashMap::new();
    for ((client_id, client_name), (effective_id, effective_name)) in
        client_calls.into_iter().zip(effective_calls)
    {
        if client_name.eq_ignore_ascii_case(&effective_name) {
            ids.insert(client_id, effective_id.clone());
            ids.insert(client_name, effective_id);
        }
    }

    for item in delta {
        if let Some(tool_call_id) = item.tool_call_id.as_mut()
            && let Some(effective_id) = ids.get(tool_call_id)
        {
            *tool_call_id = effective_id.clone();
        }
        if let MessageContent::Blocks(blocks) = &mut item.content {
            for block in blocks {
                if let ContentBlock::ToolResult { tool_use_id, .. } = block
                    && let Some(effective_id) = ids.get(tool_use_id)
                {
                    *tool_use_id = effective_id.clone();
                }
            }
        }
    }
}

fn history_tool_calls(items: &[AiItem]) -> Vec<(String, String)> {
    let mut calls = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for item in items {
        if let Some(tool_calls) = item.tool_calls.as_ref() {
            for call in tool_calls {
                if seen.insert((call.id.clone(), call.name.to_ascii_lowercase())) {
                    calls.push((call.id.clone(), call.name.clone()));
                }
            }
        } else if let MessageContent::Blocks(blocks) = &item.content {
            for (id, name) in blocks.iter().filter_map(|block| {
                if let ContentBlock::ToolUse { id, name, .. } = block {
                    Some((id.clone(), name.clone()))
                } else {
                    None
                }
            }) {
                if seen.insert((id.clone(), name.to_ascii_lowercase())) {
                    calls.push((id, name));
                }
            }
        }
    }
    calls
}

pub(super) fn history_prefix_item_count(items: &[AiItem], expected_units: usize) -> Option<usize> {
    let mut semantic_units = 0usize;
    for (index, item) in items.iter().enumerate() {
        semantic_units +=
            crate::protocol::ir::canonical::history_unit_count(std::slice::from_ref(item));
        match semantic_units.cmp(&expected_units) {
            std::cmp::Ordering::Less => {}
            std::cmp::Ordering::Equal => return Some(index + 1),
            std::cmp::Ordering::Greater => return None,
        }
    }
    None
}

pub(super) fn history_context_fingerprint(messages: &[AiItem]) -> String {
    crate::protocol::ir::canonical::hash_hex(&crate::protocol::ir::canonical::history_context_hash(
        messages,
    ))
}

pub(crate) fn generation_session_fingerprint(request: &AiRequest) -> Option<String> {
    let session_id = request
        .meta
        .vendor
        .ingress
        .get(GENERATION_SESSION_ID_META)?
        .as_str()?;
    let mut bytes = b"stravia-generation-session-v1\0".to_vec();
    bytes.extend_from_slice(session_id.as_bytes());
    Some(crate::protocol::ir::canonical::hash_hex(
        &crate::protocol::ir::canonical::hash_bytes(&bytes),
    ))
}

pub(super) fn append_history_context_fingerprint(previous: &str, message: &AiItem) -> String {
    let hash = context_hash_from_hex(previous)
        .unwrap_or_else(|| crate::protocol::ir::canonical::history_context_hash(&[]));
    crate::protocol::ir::canonical::hash_hex(
        &crate::protocol::ir::canonical::append_history_context_hash(&hash, message),
    )
}

fn context_hash_from_hex(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 {
        return None;
    }
    let mut hash = [0; 32];
    for (index, byte) in hash.iter_mut().enumerate() {
        let offset = index * 2;
        *byte = u8::from_str_radix(&value[offset..offset + 2], 16).ok()?;
    }
    Some(hash)
}

pub(super) fn legacy_payload_fingerprint<T: Serialize>(value: &T) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    crate::protocol::ir::canonical::hash_hex(&crate::protocol::ir::canonical::hash_bytes(&bytes))
}
