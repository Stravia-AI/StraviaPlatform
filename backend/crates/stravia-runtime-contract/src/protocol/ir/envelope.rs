//! 原始请求的可选上下文；它不提供绕过 canonical pipeline 的透传路径。
//! 普通 HTTP ingress 只保留 headers/method/path，原始正文由 codec 与 Wire
//! capture 各自拥有，不在执行请求中再保存一份完整 JSON。

use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;

/// 原始入口上下文。正文可缺省；审计原始 Wire 必须使用受控的 Debug capture。
#[derive(Debug, Clone, Default, Serialize)]
pub struct RawEnvelope {
    /// 调用方显式保留的原始 JSON；普通代理入口不复制到此字段。
    pub body: Option<Value>,
    /// Flattened request headers (lowercase keys).
    pub headers: HashMap<String, String>,
    /// The HTTP method (e.g. `"POST"`).
    pub method: String,
    /// The request path (e.g. `"/v1/chat/completions"`).
    pub path: String,
}

impl RawEnvelope {
    pub fn new(
        body: Option<Value>,
        headers: HashMap<String, String>,
        method: &str,
        path: &str,
    ) -> Self {
        Self {
            body,
            headers,
            method: method.to_string(),
            path: path.to_string(),
        }
    }
}
