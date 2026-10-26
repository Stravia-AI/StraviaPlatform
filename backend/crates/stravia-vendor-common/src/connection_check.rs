//! 连接检查只复用请求与解码，不触发模型同步、账号绑定或生成。

use serde_json::Value;
use stravia_vendor_sdk::{ErrorKind, GuestHost, HttpRequest, PluginError, read_http_body};

use crate::common::{plugin_error, upstream_error};

const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// 发送插件选定的非生成请求。调用方还须校验响应结构与认证证据，
/// 不能把任意 JSON 或携带 Authorization 的 2xx 直接解释为检查通过。
pub fn json(host: &GuestHost, request: HttpRequest) -> Result<Value, PluginError> {
    let response = host.http_start(request)?;
    let status = response.status()?;
    let headers = response.headers()?;
    let body = read_http_body(&response, MAX_RESPONSE_BYTES)?;
    if !(200..300).contains(&status) {
        return Err(upstream_error(status, &headers, &body));
    }
    let value: Value = serde_json::from_slice(&body)
        .map_err(|_| invalid_response("connection check response is not valid JSON"))?;
    if value.get("error").is_some_and(|error| !error.is_null()) {
        return Err(upstream_error(status, &headers, &body));
    }
    Ok(value)
}

pub fn invalid_response(message: impl Into<String>) -> PluginError {
    plugin_error(ErrorKind::InvalidResponse, message)
}
