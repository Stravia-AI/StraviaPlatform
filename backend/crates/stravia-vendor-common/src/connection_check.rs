//! 连接检查只复用请求与解码，不触发模型同步、账号绑定或生成。

use serde_json::Value;
use stravia_vendor_sdk::{ErrorKind, HttpResponse, PluginError, read_http_body};

use crate::common::{plugin_error, upstream_error};

const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// 解码已由插件选择传输策略的连接检查响应，保留 HTTP 与业务错误检查。
pub fn read_json(response: &HttpResponse) -> Result<Value, PluginError> {
    let status = response.status()?;
    let headers = response.headers()?;
    let body = read_http_body(response, MAX_RESPONSE_BYTES)?;
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
