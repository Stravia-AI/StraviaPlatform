//! Client header forwarding for Inference Run extra headers.

use reqwest::header::{
    HeaderMap as ReqwestHeaderMap, HeaderName as ReqwestHeaderName,
    HeaderValue as ReqwestHeaderValue,
};

use crate::protocol::ir::{AiRequest, ProtocolExt};

const SESSION_HEADER_PRIORITY: &[&str] = &[
    "x-session-id",
    "session-id",
    "session_id",
    "conversation_id",
    "x-session-affinity",
    "x-opencode-session",
    "x-conversation-id",
];

/// Resolve an explicit client session without deriving identity from content.
///
/// Stravia's own `x-session-id` remains authoritative for compatibility.
/// Native Codex/OpenCode session headers and Responses `prompt_cache_key`
/// provide deterministic fallbacks. Generation-chain storage is already
/// scoped by authenticated Principal, so equal raw IDs cannot collide across
/// API keys.
pub(super) fn client_session_id(
    headers: &axum::http::HeaderMap,
    request: &AiRequest,
) -> Option<String> {
    for name in SESSION_HEADER_PRIORITY {
        if let Some(value) = headers
            .get(*name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(value.to_owned());
        }
    }
    let ProtocolExt::OpenResponses(extension) = request.ext.as_ref()? else {
        return None;
    };
    extension
        .prompt_cache_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

/// Merge caller hints with provider-generated headers. Runtime bindings are
/// authoritative because they carry OAuth credentials and provider identity.
#[cfg(test)]
pub(super) fn merge_provider_headers(
    mut client_headers: ReqwestHeaderMap,
    adapter_headers: ReqwestHeaderMap,
    binding_headers: ReqwestHeaderMap,
) -> ReqwestHeaderMap {
    client_headers.extend(adapter_headers);
    client_headers.extend(binding_headers);
    client_headers
}

/// Convert client-supplied request headers into the safe subset that may be
/// forwarded upstream.
///
/// Authentication, API-key, cookie, hop-by-hop, proxy, and client network
/// identity headers are intentionally dropped so Stravia's local credentials and
/// caller IP/host metadata never leak to providers. Provider/runtime headers
/// are merged elsewhere after this function, so internal credentials still win.
pub(super) fn forwarded_client_headers(headers: &axum::http::HeaderMap) -> ReqwestHeaderMap {
    let mut forwarded = ReqwestHeaderMap::new();
    for (name, value) in headers {
        if !should_forward_client_header(name.as_str()) {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            ReqwestHeaderName::from_bytes(name.as_str().as_bytes()),
            ReqwestHeaderValue::from_bytes(value.as_bytes()),
        ) {
            forwarded.append(name, value);
        }
    }
    forwarded
}

fn should_forward_client_header(name: &str) -> bool {
    let name = name.trim().to_ascii_lowercase();
    if name.is_empty() {
        return false;
    }

    let denied = matches!(
        name.as_str(),
        // Local/proxy credentials and cookies.
        "authorization"
            | "proxy-authorization"
            | "www-authenticate"
            | "proxy-authenticate"
            | "x-api-key"
            | "x-goog-api-key"
            | "api-key"
            | "x-auth-token"
            | "x-access-token"
            | "x-refresh-token"
            | "access-token"
            | "refresh-token"
            | "cookie"
            | "set-cookie"
            // Hop-by-hop / transport-managed headers.
            | "connection"
            | "keep-alive"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "host"
            | "content-length"
            | "accept-encoding"
            | "content-encoding"
            // Client network identity / local origin metadata.
            | "forwarded"
            | "x-forwarded-for"
            | "x-forwarded-host"
            | "x-forwarded-proto"
            | "x-forwarded-port"
            | "x-forwarded-server"
            | "x-original-forwarded-for"
            | "x-real-ip"
            | "x-client-ip"
            | "x-cluster-client-ip"
            | "x-remote-ip"
            | "x-remote-addr"
            | "remote-host"
            | "remote-addr"
            | "cf-connecting-ip"
            | "true-client-ip"
            | "fastly-client-ip"
            | "via"
            | "origin"
            | "referer"
    ) || name.ends_with("-api-key")
        || name.starts_with("sec-")
        || name.starts_with("proxy-")
        || name.starts_with("cf-");

    !denied
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue};

    use super::*;

    #[test]
    fn client_session_prefers_stravia_then_native_codex_headers() {
        let request = AiRequest::new("gpt", Vec::new());
        let mut headers = HeaderMap::new();
        headers.insert("session-id", HeaderValue::from_static("codex-session"));
        headers.insert("x-session-id", HeaderValue::from_static("stravia-session"));

        assert_eq!(
            client_session_id(&headers, &request).as_deref(),
            Some("stravia-session")
        );

        headers.remove("x-session-id");
        assert_eq!(
            client_session_id(&headers, &request).as_deref(),
            Some("codex-session")
        );
    }

    #[test]
    fn client_session_falls_back_to_responses_prompt_cache_key() {
        let mut request = AiRequest::new("gpt", Vec::new());
        request.ext = Some(ProtocolExt::OpenResponses(
            crate::protocol::ir::OpenResponsesExt {
                prompt_cache_key: Some("cache-session".into()),
                ..Default::default()
            },
        ));

        assert_eq!(
            client_session_id(&HeaderMap::new(), &request).as_deref(),
            Some("cache-session")
        );
    }

    #[test]
    fn forwarded_client_headers_keep_cache_hints() {
        let mut headers = HeaderMap::new();
        headers.insert("anthropic-beta", HeaderValue::from_static("prompt-caching"));
        headers.insert("openai-beta", HeaderValue::from_static("responses=v1"));
        headers.insert("idempotency-key", HeaderValue::from_static("request-123"));

        let forwarded = forwarded_client_headers(&headers);

        assert_eq!(forwarded.get("anthropic-beta").unwrap(), "prompt-caching");
        assert_eq!(forwarded.get("openai-beta").unwrap(), "responses=v1");
        assert_eq!(forwarded.get("idempotency-key").unwrap(), "request-123");
    }

    #[test]
    fn forwarded_client_headers_drop_keys_and_sensitive_network_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer stravia-key"),
        );
        headers.insert("x-api-key", HeaderValue::from_static("stravia-key"));
        headers.insert("x-goog-api-key", HeaderValue::from_static("stravia-key"));
        headers.insert(
            "proxy-authorization",
            HeaderValue::from_static("Basic secret"),
        );
        headers.insert("cookie", HeaderValue::from_static("session=secret"));
        headers.insert("x-forwarded-for", HeaderValue::from_static("10.0.0.1"));
        headers.insert("x-real-ip", HeaderValue::from_static("10.0.0.2"));
        headers.insert("remote-host", HeaderValue::from_static("client.local"));
        headers.insert("connection", HeaderValue::from_static("keep-alive"));
        headers.insert("anthropic-beta", HeaderValue::from_static("prompt-caching"));

        let forwarded = forwarded_client_headers(&headers);

        assert!(forwarded.get("authorization").is_none());
        assert!(forwarded.get("x-api-key").is_none());
        assert!(forwarded.get("x-goog-api-key").is_none());
        assert!(forwarded.get("proxy-authorization").is_none());
        assert!(forwarded.get("cookie").is_none());
        assert!(forwarded.get("x-forwarded-for").is_none());
        assert!(forwarded.get("x-real-ip").is_none());
        assert!(forwarded.get("remote-host").is_none());
        assert!(forwarded.get("connection").is_none());
        assert_eq!(forwarded.get("anthropic-beta").unwrap(), "prompt-caching");
    }

    #[test]
    fn forwarded_client_headers_drop_client_encoding_negotiation() {
        let mut headers = HeaderMap::new();
        headers.insert("accept-encoding", HeaderValue::from_static("gzip"));

        let forwarded = forwarded_client_headers(&headers);

        assert!(
            forwarded.get("accept-encoding").is_none(),
            "reqwest must own upstream response decompression; client encoding hints are only for the Stravia response"
        );
    }
    #[test]
    fn runtime_binding_headers_override_client_identity_hints() {
        let mut client = ReqwestHeaderMap::new();
        client.insert(
            reqwest::header::USER_AGENT,
            ReqwestHeaderValue::from_static("curl/8.21.0"),
        );
        let mut binding = ReqwestHeaderMap::new();
        binding.insert(
            reqwest::header::USER_AGENT,
            ReqwestHeaderValue::from_static("codex_cli_rs/0.145.0"),
        );

        let merged = merge_provider_headers(client, ReqwestHeaderMap::new(), binding);

        assert_eq!(
            merged
                .get(reqwest::header::USER_AGENT)
                .and_then(|value| value.to_str().ok()),
            Some("codex_cli_rs/0.145.0")
        );
    }
}
