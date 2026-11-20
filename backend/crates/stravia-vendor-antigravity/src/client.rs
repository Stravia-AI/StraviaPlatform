use serde_json::{Value, json};
use std::time::Duration;
use stravia_runtime_contract::protocol::ir::AiErrorKind;
use stravia_vendor_common::common::{self, plugin_error};
use stravia_vendor_sdk::{ErrorKind, GuestHost, PluginError, ProviderSnapshot, read_http_body};

use stravia_vendor_sdk::net::{Coding, Fields, HttpVersion, Options, Request};

pub(crate) const MAX_JSON_BODY: usize = 4 * 1024 * 1024;

pub(crate) fn http_start(
    host: &GuestHost,
    request: Request,
) -> Result<stravia_vendor_sdk::HttpResponse, PluginError> {
    let mut options = Options::new();
    options.shape("antigravity").expect("valid shape");
    options
        .http_version(HttpVersion::Http1)
        .expect("valid HTTP version");
    options
        .response_decoding(&[Coding::Gzip])
        .expect("valid decoding");
    host.http_start(request, &options)
}

pub(crate) fn headers(provider: &ProviderSnapshot) -> Result<Fields, PluginError> {
    let token = credential(provider, "access_token")
        .ok_or_else(|| plugin_error(ErrorKind::Auth, "Antigravity access token is missing"))?;
    if token.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(plugin_error(
            ErrorKind::Auth,
            "Antigravity access token is malformed",
        ));
    }
    // Go HTTP/1 writes its transport-owned framing fields in these slots.
    let mut headers = Fields::new();
    headers.slot("Host");
    headers.append("User-Agent", crate::CLI_USER_AGENT.as_bytes());
    headers.slot("Content-Length");
    headers.append("Authorization", format!("Bearer {token}").into_bytes());
    headers.append("Content-Type", b"application/json".as_slice());
    headers.append("Accept-Encoding", b"gzip".as_slice());
    Ok(headers)
}

pub(crate) fn root(provider: &ProviderSnapshot) -> &str {
    if provider.base_url.trim().is_empty() {
        crate::BASE_URL
    } else {
        provider.base_url.trim_end_matches('/')
    }
}

pub(crate) fn endpoint(provider: &ProviderSnapshot, rpc: &str) -> String {
    format!("{}/v1internal:{rpc}", root(provider))
}

pub(crate) fn post_json(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    rpc: &str,
    body: &Value,
) -> Result<Value, PluginError> {
    request_json(
        host,
        "POST",
        endpoint(provider, rpc),
        headers(provider)?,
        serde_json::to_vec(body).map_err(|_| {
            plugin_error(
                ErrorKind::Invalid,
                "Antigravity request could not be encoded",
            )
        })?,
    )
}

pub(crate) fn request_json(
    host: &GuestHost,
    method: &str,
    url: String,
    mut headers: Fields,
    body: Vec<u8>,
) -> Result<Value, PluginError> {
    if method != "POST" {
        headers.remove("content-type");
        headers.remove("content-length");
    }
    let response = http_start(
        host,
        Request {
            method: method.into(),
            url,
            headers,
            body,
        },
    )?;
    let status = response.status()?;
    let headers = response.headers()?;
    let body = read_http_body(&response, MAX_JSON_BODY)?;
    if !(200..300).contains(&status) {
        return Err(upstream_error(status, &headers, &body));
    }
    serde_json::from_slice(&body).map_err(|_| {
        plugin_error(
            ErrorKind::upstream_unknown(),
            "Antigravity response is not valid JSON",
        )
    })
}

pub(crate) fn credential<'a>(provider: &'a ProviderSnapshot, key: &str) -> Option<&'a str> {
    provider
        .credentials
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub(crate) fn project_value(value: &Value) -> Option<&str> {
    value
        .get("cloudaicompanionProject")
        .and_then(|project| {
            project
                .as_str()
                .or_else(|| project.get("id").and_then(Value::as_str))
        })
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub(crate) fn project(
    host: &GuestHost,
    provider: &ProviderSnapshot,
) -> Result<String, PluginError> {
    if let Some(project) = credential(provider, "project_id") {
        return Ok(project.into());
    }
    if let Some(project) = crate::auth::bound_project(host, provider)? {
        return Ok(project);
    }
    Err(plugin_error(
        ErrorKind::Auth,
        "Antigravity project is missing; complete account onboarding",
    ))
}

pub(crate) fn discover_project(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    session: &mut crate::auth::Session,
) -> Result<Option<String>, PluginError> {
    if let Some(operation) = session.operation.as_deref() {
        crate::auth::validate_operation(operation)?;
        let payload = request_json(
            host,
            "GET",
            format!("{}/v1internal/{operation}", root(provider)),
            headers(provider)?,
            Vec::new(),
        )?;
        return operation_result(host, session, &payload);
    }
    let discovery = post_json(
        host,
        provider,
        "loadCodeAssist",
        &json!({"metadata":{"ideType":"ANTIGRAVITY"}}),
    )?;
    if discovery
        .get("projectValidationError")
        .and_then(|error| error.get("code"))
        .and_then(Value::as_i64)
        .is_some_and(|code| code != 0)
    {
        return Err(plugin_error(
            ErrorKind::Auth,
            "Antigravity project validation failed",
        ));
    }
    if let Some(project) = project_value(&discovery) {
        if let Some(tier) = discovery
            .get("currentTier")
            .or_else(|| discovery.get("paidTier"))
        {
            session.values.insert("tier".into(), tier.clone());
        }
        return Ok(Some(project.into()));
    }
    let tier = crate::auth::default_tier(&discovery)?;
    session.values.insert("tier".into(), tier.clone());
    crate::auth::save(host, session)?;
    let payload = post_json(
        host,
        provider,
        "onboardUser",
        &json!({"tierId":tier.get("id"),"metadata":{"ideType":"ANTIGRAVITY"}}),
    )?;
    operation_result(host, session, &payload)
}

fn operation_result(
    host: &GuestHost,
    session: &mut crate::auth::Session,
    payload: &Value,
) -> Result<Option<String>, PluginError> {
    if payload.get("done").and_then(Value::as_bool) == Some(true) {
        if let Some(error) = payload.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Antigravity onboarding operation failed");
            return Err(plugin_error(ErrorKind::upstream_unknown(), message));
        }
        let project = payload
            .get("response")
            .and_then(project_value)
            .ok_or_else(|| {
                plugin_error(
                    ErrorKind::upstream_unknown(),
                    "Antigravity onboarding result omitted project",
                )
            })?;
        session.operation = None;
        return Ok(Some(project.into()));
    }
    if payload.get("done").is_some_and(|value| !value.is_boolean()) {
        return Err(plugin_error(
            ErrorKind::upstream_unknown(),
            "Antigravity onboarding completion flag is malformed",
        ));
    }
    let name = payload.get("name").and_then(Value::as_str).ok_or_else(|| {
        plugin_error(
            ErrorKind::upstream_unknown(),
            "Antigravity onboarding operation name is missing",
        )
    })?;
    crate::auth::validate_operation(name)?;
    if session
        .operation
        .as_deref()
        .is_some_and(|previous| previous != name)
    {
        return Err(plugin_error(
            ErrorKind::upstream_unknown(),
            "Antigravity onboarding operation identity changed",
        ));
    }
    session.operation = Some(name.into());
    crate::auth::save(host, session)?;
    Ok(None)
}

const GOOGLE_RPC_RETRY_INFO: &str = "type.googleapis.com/google.rpc.RetryInfo";
const GOOGLE_RPC_ERROR_INFO: &str = "type.googleapis.com/google.rpc.ErrorInfo";

pub(crate) fn upstream_error(status: u16, headers: &Fields, body: &[u8]) -> PluginError {
    let mut error = common::upstream_error(status, headers, body);
    let Some(base_kind) = error.kind.model_error_kind() else {
        return error;
    };
    let details = serde_json::from_slice::<Value>(body)
        .ok()
        .map(|value| parse_google_rpc_details(&value))
        .unwrap_or_default();
    let retry_after = error.kind.retry_after().or(details.retry_delay);
    let kind = if status == 429 {
        let quota_reason = details.reason.as_deref().is_some_and(|reason| {
            reason.eq_ignore_ascii_case("QUOTA_EXHAUSTED")
                || reason.eq_ignore_ascii_case("INSUFFICIENT_G1_CREDITS_BALANCE")
        });
        // 明确配额耗尽，或既无 HTTP Retry-After 也无 Google RetryInfo/quotaResetDelay 的 429：
        // 交由宿主按 QuotaExceeded 直接切目标或返回错误，避免在同一目标上盲目退避重试。
        if quota_reason || retry_after.is_none() {
            AiErrorKind::QuotaExceeded
        } else {
            base_kind
        }
    } else {
        base_kind
    };
    error.kind = ErrorKind::upstream(Some(kind), retry_after);
    error
}

#[derive(Default)]
struct GoogleRpcErrorDetails {
    reason: Option<String>,
    retry_delay: Option<Duration>,
}

fn parse_google_rpc_details(value: &Value) -> GoogleRpcErrorDetails {
    let mut out = GoogleRpcErrorDetails::default();
    let mut quota_reset_delay = None;
    let Some(details) = value.pointer("/error/details").and_then(Value::as_array) else {
        return out;
    };
    for detail in details {
        let Some(kind) = detail.get("@type").and_then(Value::as_str) else {
            continue;
        };
        if kind == GOOGLE_RPC_RETRY_INFO {
            if out.retry_delay.is_none()
                && let Some(delay) = detail
                    .get("retryDelay")
                    .and_then(Value::as_str)
                    .and_then(parse_go_duration)
            {
                out.retry_delay = Some(delay);
            }
        } else if kind == GOOGLE_RPC_ERROR_INFO {
            if out.reason.is_none()
                && let Some(reason) = detail
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
            {
                out.reason = Some(reason.to_owned());
            }
            if quota_reset_delay.is_none()
                && let Some(delay) = detail
                    .pointer("/metadata/quotaResetDelay")
                    .and_then(Value::as_str)
                    .and_then(parse_go_duration)
            {
                quota_reset_delay = Some(delay);
            }
        }
    }
    if out.retry_delay.is_none() {
        out.retry_delay = quota_reset_delay;
    }
    out
}

fn parse_go_duration(raw: &str) -> Option<Duration> {
    let mut rest = raw.trim();
    if rest.is_empty() || rest.starts_with('-') {
        return None;
    }
    if let Some(stripped) = rest.strip_prefix('+') {
        rest = stripped;
    }
    if rest.is_empty() {
        return None;
    }
    let mut total_nanos: u128 = 0;
    while !rest.is_empty() {
        let int_len = rest.bytes().take_while(u8::is_ascii_digit).count();
        let (int_str, after_int) = rest.split_at(int_len);
        let (frac_str, after_num) = if let Some(after_dot) = after_int.strip_prefix('.') {
            let frac_len = after_dot.bytes().take_while(u8::is_ascii_digit).count();
            after_dot.split_at(frac_len)
        } else {
            ("", after_int)
        };
        if int_str.is_empty() && frac_str.is_empty() {
            return None;
        }
        let int_val: u128 = if int_str.is_empty() {
            0
        } else {
            int_str.parse().ok()?
        };
        let (unit_ns, next) = if let Some(r) = after_num.strip_prefix("ns") {
            (1_u128, r)
        } else if let Some(r) = after_num
            .strip_prefix("us")
            .or_else(|| after_num.strip_prefix("µs"))
            .or_else(|| after_num.strip_prefix("μs"))
        {
            (1_000_u128, r)
        } else if let Some(r) = after_num.strip_prefix("ms") {
            (1_000_000_u128, r)
        } else if let Some(r) = after_num.strip_prefix('s') {
            (1_000_000_000_u128, r)
        } else if let Some(r) = after_num.strip_prefix('m') {
            (60_u128 * 1_000_000_000, r)
        } else if let Some(r) = after_num.strip_prefix('h') {
            (3_600_u128 * 1_000_000_000, r)
        } else {
            return None;
        };
        let mut part_nanos = int_val.saturating_mul(unit_ns);
        if !frac_str.is_empty() {
            // 截断到最多 12 位小数避免 10^n 溢出，同时保留完整纳秒精度。
            let trimmed = &frac_str[..frac_str.len().min(12)];
            let frac_val: u128 = trimmed.parse().ok()?;
            let scale = 10_u128.pow(trimmed.len() as u32);
            part_nanos = part_nanos.saturating_add(frac_val.saturating_mul(unit_ns) / scale);
        }
        total_nanos = total_nanos.saturating_add(part_nanos);
        rest = next;
    }
    if total_nanos == 0 {
        return None;
    }
    let secs = (total_nanos / 1_000_000_000).min(u64::MAX as u128) as u64;
    let nanos = (total_nanos % 1_000_000_000) as u32;
    Some(Duration::new(secs, nanos))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_go_duration_handles_google_rpc_formats() {
        assert_eq!(parse_go_duration("0.5s"), Some(Duration::from_millis(500)));
        assert_eq!(
            parse_go_duration("0.201506475s"),
            Some(Duration::from_nanos(201_506_475))
        );
        assert_eq!(parse_go_duration("2m30s"), Some(Duration::from_secs(150)));
        assert_eq!(
            parse_go_duration("1h30m"),
            Some(Duration::from_secs(5_400))
        );
        assert_eq!(parse_go_duration("200ms"), Some(Duration::from_millis(200)));
        assert_eq!(parse_go_duration("0s"), None);
        assert_eq!(parse_go_duration("-5s"), None);
        assert_eq!(parse_go_duration("invalid"), None);
    }

    #[test]
    fn upstream_error_429_without_retry_info_is_quota_exceeded() {
        let headers = Fields::new();
        let body = serde_json::to_vec(&json!({
            "error": {
                "code": 429,
                "status": "RESOURCE_EXHAUSTED",
                "message": "rate limited",
                "details": [{
                    "@type": "type.googleapis.com/google.rpc.ErrorInfo",
                    "reason": "RATE_LIMIT_EXCEEDED"
                }]
            }
        }))
        .unwrap();
        let error = upstream_error(429, &headers, &body);
        assert_eq!(error.kind.model_error_kind(), Some(AiErrorKind::QuotaExceeded));
        assert_eq!(error.kind.retry_after(), None);
    }

    #[test]
    fn upstream_error_429_with_google_retry_info_preserves_rate_limit_and_delay() {
        let headers = Fields::new();
        let body = serde_json::to_vec(&json!({
            "error": {
                "code": 429,
                "status": "RESOURCE_EXHAUSTED",
                "message": "slow down",
                "details": [
                    {
                        "@type": "type.googleapis.com/google.rpc.ErrorInfo",
                        "reason": "RATE_LIMIT_EXCEEDED"
                    },
                    {
                        "@type": "type.googleapis.com/google.rpc.RetryInfo",
                        "retryDelay": "4.5s"
                    }
                ]
            }
        }))
        .unwrap();
        let error = upstream_error(429, &headers, &body);
        assert_eq!(error.kind.model_error_kind(), Some(AiErrorKind::RateLimitError));
        assert_eq!(error.kind.retry_after(), Some(Duration::from_millis(4_500)));
    }

    #[test]
    fn upstream_error_429_quota_exhausted_stays_quota_exceeded_with_reset_delay() {
        let headers = Fields::new();
        let body = serde_json::to_vec(&json!({
            "error": {
                "code": 429,
                "status": "RESOURCE_EXHAUSTED",
                "message": "quota exhausted",
                "details": [{
                    "@type": "type.googleapis.com/google.rpc.ErrorInfo",
                    "reason": "QUOTA_EXHAUSTED",
                    "metadata": {
                        "quotaResetDelay": "2h15m"
                    }
                }]
            }
        }))
        .unwrap();
        let error = upstream_error(429, &headers, &body);
        assert_eq!(error.kind.model_error_kind(), Some(AiErrorKind::QuotaExceeded));
        assert_eq!(error.kind.retry_after(), Some(Duration::from_secs(8_100)));
    }

    #[test]
    fn upstream_error_503_extracts_retry_delay_without_reclassifying() {
        let headers = Fields::new();
        let body = serde_json::to_vec(&json!({
            "error": {
                "code": 503,
                "status": "UNAVAILABLE",
                "message": "capacity exhausted",
                "details": [{
                    "@type": "type.googleapis.com/google.rpc.RetryInfo",
                    "retryDelay": "1.5s"
                }]
            }
        }))
        .unwrap();
        let error = upstream_error(503, &headers, &body);
        assert_eq!(
            error.kind.model_error_kind(),
            Some(AiErrorKind::ServiceUnavailable)
        );
        assert_eq!(error.kind.retry_after(), Some(Duration::from_millis(1_500)));
    }
}
