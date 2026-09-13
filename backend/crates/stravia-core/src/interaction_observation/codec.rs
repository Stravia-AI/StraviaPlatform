use std::io::{Cursor, Read, Write};

use anyhow::{Context, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::Value;
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

pub(super) const CONTENT_BLOCK_BYTES: usize = 16 * 1024;
const CODEC: &str = "zip-deflate-v1";
const MIN_COMPRESS_BYTES: usize = 1024;

/// 只编码已封口文本，不改变调用身份、作用域或其他可索引字段。
pub(super) fn encode_payload(payload: &Value) -> anyhow::Result<Option<Value>> {
    if !matches!(
        payload["kind"].as_str(),
        Some("client_visible_content_delta" | "model_thinking_delta")
    ) {
        return Ok(None);
    }
    let Some(text) = payload["text"].as_str() else {
        return Ok(None);
    };
    if !(MIN_COMPRESS_BYTES..=CONTENT_BLOCK_BYTES).contains(&text.len()) {
        return Ok(None);
    }
    let mut archive = ZipWriter::new(Cursor::new(Vec::new()));
    archive.start_file(
        "content",
        SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
    )?;
    archive.write_all(text.as_bytes())?;
    let encoded = STANDARD.encode(archive.finish()?.into_inner());
    // Base64 与容器元数据也计入收益，不以压缩后的裸字节数冒充净节省。
    if encoded.len().saturating_add(128) >= text.len() {
        return Ok(None);
    }
    let mut stored = payload.clone();
    let object = stored
        .as_object_mut()
        .context("invalid observation payload")?;
    object.remove("text");
    object.insert(
        "text_storage".into(),
        serde_json::json!({"codec":CODEC,"bytes":text.len(),"data":encoded}),
    );
    Ok(Some(stored))
}

/// 旧记录保持原样；压缩损坏或超限不能伪装成空正文。
pub(super) fn decode_payload(mut payload: Value) -> anyhow::Result<Value> {
    let Some(storage) = payload.get("text_storage") else {
        return Ok(payload);
    };
    ensure!(
        matches!(
            payload["kind"].as_str(),
            Some("client_visible_content_delta" | "model_thinking_delta")
        ) && payload.get("text").is_none()
            && storage["codec"].as_str() == Some(CODEC),
        "invalid observation content encoding"
    );
    let length = storage["bytes"]
        .as_u64()
        .context("invalid observation content length")?;
    ensure!(
        length <= CONTENT_BLOCK_BYTES as u64,
        "observation content exceeds block limit"
    );
    let encoded = storage["data"]
        .as_str()
        .context("invalid observation content data")?;
    ensure!(
        encoded.len() <= CONTENT_BLOCK_BYTES * 2,
        "encoded observation content exceeds block limit"
    );
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| anyhow::anyhow!("invalid observation content base64"))?;
    let mut archive = ZipArchive::new(Cursor::new(bytes))
        .map_err(|_| anyhow::anyhow!("invalid observation content archive"))?;
    ensure!(archive.len() == 1, "invalid observation content archive");
    let mut entry = archive
        .by_name("content")
        .map_err(|_| anyhow::anyhow!("missing observation content entry"))?;
    ensure!(entry.size() == length, "invalid observation content length");
    let mut decoded = Vec::with_capacity(length as usize);
    (&mut entry)
        .take(length + 1)
        .read_to_end(&mut decoded)
        .map_err(|_| anyhow::anyhow!("damaged observation content"))?;
    ensure!(
        decoded.len() as u64 == length,
        "invalid observation content length"
    );
    let text = String::from_utf8(decoded)
        .map_err(|_| anyhow::anyhow!("invalid observation content UTF-8"))?;
    let object = payload
        .as_object_mut()
        .context("invalid observation payload")?;
    object.remove("text_storage");
    object.insert("text".into(), Value::String(text));
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compressed_content_preserves_unicode_and_scope_with_net_savings() -> anyhow::Result<()> {
        let payload = serde_json::json!({
            "kind":"model_thinking_delta","model_turn_id":"turn","attempt_id":"attempt",
            "block_id":"block","text":"解释 e\u{301} 👩🏽‍💻\n```rust\nlet x = 1;\n```\n".repeat(180),
        });
        let stored = encode_payload(&payload)?.expect("repetitive text benefits from compression");
        assert!(serde_json::to_vec(&stored)?.len() < serde_json::to_vec(&payload)?.len() / 2);
        assert_eq!(decode_payload(stored)?, payload);
        Ok(())
    }

    #[test]
    fn plain_history_and_nontext_results_keep_their_contents() -> anyhow::Result<()> {
        let plain = serde_json::json!({"kind":"client_visible_content_delta","text":"old text"});
        assert_eq!(decode_payload(plain.clone())?, plain);
        let tool = serde_json::json!({"kind":"client_tool_result","tool_id":"call","content":"result".repeat(1000)});
        assert!(encode_payload(&tool)?.is_none());
        assert_eq!(decode_payload(tool.clone())?, tool);
        Ok(())
    }

    #[test]
    fn damaged_and_oversized_content_is_not_silently_recovered() -> anyhow::Result<()> {
        let payload = serde_json::json!({
            "kind":"client_visible_content_delta","text":"observation content ".repeat(200),
        });
        let stored = encode_payload(&payload)?.expect("compressible content");
        let mut damaged = stored.clone();
        damaged["text_storage"]["data"] = Value::String("not base64".into());
        assert!(decode_payload(damaged).is_err());
        let mut oversized = stored.clone();
        oversized["text_storage"]["bytes"] = serde_json::json!(CONTENT_BLOCK_BYTES + 1);
        assert!(decode_payload(oversized).is_err());
        let mut wrong_length = stored;
        wrong_length["text_storage"]["bytes"] = serde_json::json!(1);
        assert!(decode_payload(wrong_length).is_err());
        Ok(())
    }
}
