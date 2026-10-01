use std::io::{Cursor, Read};

use anyhow::{Context, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::Value;
use zip::ZipArchive;

pub(super) const CONTENT_BLOCK_BYTES: usize = 16 * 1024;
const CODEC: &str = "zip-deflate-v1";
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
