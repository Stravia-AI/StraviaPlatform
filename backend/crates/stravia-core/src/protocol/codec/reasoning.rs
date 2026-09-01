use crate::protocol::ir::{AiItem, AiResponse};

pub fn normalize_response_reasoning(resp: &mut AiResponse) {
    if resp.reasoning_items().next().is_some() {
        return;
    }

    let first_text_index = resp
        .items
        .iter()
        .position(|item| item.output_text_ref().is_some());
    let (reasoning, text) = split_think_tags(&resp.output_text());
    if let Some(reasoning) = reasoning {
        resp.replace_output_text(text);
        resp.items.insert(
            first_text_index.unwrap_or(resp.items.len()),
            AiItem::thinking(reasoning, None),
        );
    }
}

pub(crate) fn split_think_tags(content: &str) -> (Option<String>, String) {
    let mut remaining = content;
    let mut reasoning_parts: Vec<String> = Vec::new();
    let mut text_parts: Vec<String> = Vec::new();

    loop {
        let Some(start_idx) = remaining.find("<think>") else {
            if !remaining.is_empty() {
                text_parts.push(remaining.to_string());
            }
            break;
        };

        let before = &remaining[..start_idx];
        if !before.is_empty() {
            text_parts.push(before.to_string());
        }

        let after_start = &remaining[start_idx + "<think>".len()..];
        let Some(end_rel_idx) = after_start.find("</think>") else {
            text_parts.push(remaining[start_idx..].to_string());
            break;
        };

        let thought = after_start[..end_rel_idx].trim();
        if !thought.is_empty() {
            reasoning_parts.push(thought.to_string());
        }
        remaining = &after_start[end_rel_idx + "</think>".len()..];
    }

    let reasoning = if reasoning_parts.is_empty() {
        None
    } else {
        Some(reasoning_parts.join("\n"))
    };
    (reasoning, text_parts.join("").trim().to_string())
}

#[cfg(test)]
mod tests;
