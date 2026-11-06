//! Codex 独立压缩 V2。
//!
//! 与官方客户端 `compact_remote_v2` 一致：普通流式 `POST /responses` 请求的
//! input 末尾追加 `compaction_trigger`，读取正常 Responses SSE 的唯一
//! compaction item。共享 codec 负责 typed IR 解码与严格的状态校验，宿主从
//! IR 向 V2 客户端输出；本模块只负责体积上限、上游错误分类和错误包装。

use stravia_runtime_contract::protocol::ir::{AiError, AiErrorKind};
use stravia_vendor_common::common::model_error;
use stravia_vendor_sdk::{HttpResponse, PluginError, read_http_body};

/// compaction item 为不透明密文，整段 SSE 上限与宿主 unary 上限一致。
const MAX_COMPACTION_BODY: usize = 32 * 1024 * 1024;

/// 读取终端 Responses SSE，只接受一个完整的原生 compaction item。
/// 非成功状态由调用方的错误保留包装处理。
pub(super) fn decode(
    response: HttpResponse,
) -> Result<stravia_runtime_contract::protocol::ir::NativeCompactionResponse, PluginError> {
    let bytes = read_http_body(&response, MAX_COMPACTION_BODY)?;
    stravia_protocol_codec::codec::compaction::parse_responses_compaction_stream(&bytes).map_err(
        |error| {
            if let Some(upstream) = error.downcast_ref::<AiError>()
                && let Some(raw) = upstream.raw.as_ref()
                && let Some(classified) = classify_upstream_error(raw, &bytes)
            {
                return classified;
            }
            model_error(
                AiErrorKind::ServerError,
                format!("invalid Codex compaction response: {error}"),
            )
        },
    )
}

/// 只在失败路径查找上游原始事件并套用 Codex 既有错误分类；成功流不重复扫描。
fn classify_upstream_error(raw: &serde_json::Value, bytes: &[u8]) -> Option<PluginError> {
    let saw_response_event = std::str::from_utf8(bytes).is_ok_and(|text| {
        text.replace("\r\n", "\n").split("\n\n").any(|block| {
            block.lines().any(|line| {
                line.strip_prefix("event:")
                    .is_some_and(|name| name.trim() == "response.created")
            })
        })
    });
    for candidate in [Some(raw), raw.get("error"), raw.get("response")] {
        let Some(payload) = candidate else { continue };
        if let Some(classified) =
            crate::classify_responses_stream_error(payload, saw_response_event)
        {
            return Some(classified);
        }
    }
    None
}
