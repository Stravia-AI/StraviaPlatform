//! Vendor extensions — three-segment model.
//!
//! Every `AiRequest` and `AiResponse` carries a `VendorExtensions` bag that
//! holds fields which don't have a home in the canonical IR schema.
//!
//! ## Three segments
//!
//! - **`ingress`** — 尚未归入内容或响应元数据的源协议载体，以及旧快照的
//!   私有 wire 字段。请求控制在入口消费到 canonical `RequestParameters`；
//!   保留未知源值不代表允许将其透传到目标。
//!
//! - **`egress`** — private provider annotations and opaque context references.
//!   This metadata does not authorize extra upstream body fields or headers.
//!
//! - **`passthrough_safe`** — historical snapshot metadata. Despite the old
//!   name, it never grants unknown request fields target re-encoding permission.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// Chat 推理字段的显式空字符串或 null；不承载正文，也不表示 Thinking item。
/// 该私有载体可经过响应和流式元数据，但不得出现在公开协议资源中。
pub const CHAT_REASONING_FIELD_META: &str = "__stravia_chat_reasoning_field";

/// 上游实际保存响应的结果；只用于续接资格，不覆盖本地 store，也不进入公开资源。
pub const OPEN_RESPONSES_PROVIDER_STORE_META: &str = "__open_responses_provider_store";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VendorExtensions {
    /// Extra fields from the ingress body (client side).
    pub ingress: HashMap<String, Value>,
    /// Private provider annotations, not an egress request body.
    pub egress: HashMap<String, Value>,
    /// Historical metadata, not an upstream request-parameter authority.
    pub passthrough_safe: HashMap<String, Value>,
}

impl VendorExtensions {
    pub fn is_empty(&self) -> bool {
        self.ingress.is_empty() && self.egress.is_empty() && self.passthrough_safe.is_empty()
    }
}
