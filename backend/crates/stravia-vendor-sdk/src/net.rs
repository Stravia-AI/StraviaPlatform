pub mod shape;
pub use shape::EgressShape;

use std::time::Duration;

use crate::bindings::stravia::net::{http, types, websocket};
use crate::guest::{ErrorKind, ModelErrorKind, PluginError, UpstreamFailure};

/// Stable names for the open transport option namespace.
pub mod keys {
    pub const SHAPE: &str = "shape";
    pub const HTTP_VERSION: &str = "http.version";
    pub const REQUEST_ENCODING: &str = "http.request-encoding";
    pub const RESPONSE_DECODING: &str = "http.response-decoding";
    pub const WS_COMPRESSION: &str = "ws.compression";
    pub const WS_TRANSPORT: &str = "ws.transport";
    pub const REUSE_MAX_AGE: &str = "ws.reuse-max-age-millis";
    pub const REUSE_ONLY: &str = "ws.reuse-only";
    pub const CONTINUATION: &str = "ws.continuation-id";
    pub const COOKIES: &str = "cookies";
}

/// Opt-in host cookie selection. Domains match hostnames exactly. Names and
/// prefixes match cookie names case-sensitively. This does not grant network access.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CookiePolicy {
    pub domains: Vec<String>,
    pub names: Vec<String>,
    pub prefixes: Vec<String>,
}

impl CookiePolicy {
    /// Parse and validate a bounded JSON policy, as used by the host option setter.
    pub fn parse(value: &str) -> Result<Self, OptionError> {
        const MAX_POLICY_BYTES: usize = 16 * 1024;
        const MAX_ENTRIES: usize = 64;
        let invalid = || OptionError {
            key: keys::COOKIES.into(),
            value: value.into(),
        };
        if value.len() > MAX_POLICY_BYTES || !value.trim_start().starts_with('{') {
            return Err(invalid());
        }
        let policy: Self = serde_json::from_str(value).map_err(|_| invalid())?;
        if policy.domains.len() > MAX_ENTRIES
            || policy.names.len() > MAX_ENTRIES
            || policy.prefixes.len() > MAX_ENTRIES
            || !policy
                .domains
                .iter()
                .all(|domain| valid_cookie_domain(domain))
            || !policy
                .names
                .iter()
                .chain(&policy.prefixes)
                .all(|name| valid_cookie_token(name))
        {
            return Err(invalid());
        }
        Ok(policy)
    }
}

fn valid_cookie_domain(domain: &str) -> bool {
    !domain.is_empty()
        && domain.len() <= 253
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label.as_bytes()[0].is_ascii_alphanumeric()
                && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
                && label
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        })
}

fn valid_cookie_token(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && name.bytes().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub bytes: Vec<u8>,
}

/// Ordered HTTP fields. Names are case-insensitive for lookup, but spelling and
/// duplicate order are preserved on the wire where the protocol permits.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fields(Vec<Field>);

impl Fields {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn append(&mut self, name: impl Into<String>, bytes: impl Into<Vec<u8>>) {
        self.0.push(Field {
            name: name.into(),
            bytes: bytes.into(),
        });
    }
    pub fn set(&mut self, name: impl Into<String>, bytes: impl Into<Vec<u8>>) {
        let name = name.into();
        let bytes = bytes.into();
        if let Some(index) = self
            .0
            .iter()
            .position(|field| field.name.eq_ignore_ascii_case(&name))
        {
            self.0[index] = Field {
                name: name.clone(),
                bytes,
            };
            let mut seen = false;
            self.0.retain(|field| {
                if !field.name.eq_ignore_ascii_case(&name) {
                    return true;
                }
                if !seen {
                    seen = true;
                    true
                } else {
                    false
                }
            });
        } else {
            self.append(name, bytes);
        }
    }
    pub fn slot(&mut self, name: impl Into<String>) {
        self.append(name, Vec::new());
    }
    pub fn remove(&mut self, name: &str) {
        self.0
            .retain(|field| !field.name.eq_ignore_ascii_case(name));
    }
    pub fn get(&self, name: &str) -> Option<&[u8]> {
        self.0
            .iter()
            .find(|field| field.name.eq_ignore_ascii_case(name))
            .map(|field| field.bytes.as_slice())
    }
    pub fn iter(&self) -> impl Iterator<Item = &Field> {
        self.0.iter()
    }
    pub fn into_vec(self) -> Vec<Field> {
        self.0
    }
    fn into_wit(self) -> Vec<types::Field> {
        self.0
            .into_iter()
            .map(|f| types::Field {
                name: f.name,
                bytes: f.bytes,
            })
            .collect()
    }
    fn from_wit(fields: Vec<types::Field>) -> Self {
        Self(
            fields
                .into_iter()
                .map(|f| Field {
                    name: f.name,
                    bytes: f.bytes,
                })
                .collect(),
        )
    }
}

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub url: String,
    pub headers: Fields,
    pub body: Vec<u8>,
}
impl Request {
    pub fn new(method: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            method: method.into(),
            url: url.into(),
            headers: Fields::new(),
            body: Vec::new(),
        }
    }
    pub fn fields(mut self, fields: Fields) -> Self {
        self.headers = fields;
        self
    }
    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }
}
#[derive(Debug, Clone)]
pub struct ConnectRequest {
    pub url: String,
    pub headers: Fields,
}
impl ConnectRequest {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            headers: Fields::new(),
        }
    }
    pub fn fields(mut self, fields: Fields) -> Self {
        self.headers = fields;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpVersion {
    Auto,
    Http1,
    Http2,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coding {
    Identity,
    Gzip,
    Deflate,
    Br,
    Zstd,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsCompression {
    Disabled,
    Preferred,
    Required,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsTransport {
    Auto,
    Http1,
    H2,
}
impl Coding {
    fn as_str(self) -> &'static str {
        match self {
            Self::Identity => "identity",
            Self::Gzip => "gzip",
            Self::Deflate => "deflate",
            Self::Br => "br",
            Self::Zstd => "zstd",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionError {
    pub key: String,
    pub value: String,
}
impl std::fmt::Display for OptionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unsupported option {}={}", self.key, self.value)
    }
}
impl std::error::Error for OptionError {}

fn option_valid(key: &str, value: &str) -> Option<bool> {
    Some(match key {
        keys::SHAPE => !value.is_empty(),
        keys::CONTINUATION => true,
        keys::COOKIES => CookiePolicy::parse(value).is_ok(),
        keys::HTTP_VERSION => matches!(value, "auto" | "http1" | "http2"),
        keys::REQUEST_ENCODING => matches!(value, "identity" | "gzip" | "deflate" | "br" | "zstd"),
        keys::RESPONSE_DECODING => {
            value == "none"
                || (!value.is_empty()
                    && value
                        .split(',')
                        .all(|v| matches!(v, "identity" | "gzip" | "deflate" | "br" | "zstd")))
        }
        keys::WS_COMPRESSION => matches!(value, "disabled" | "preferred" | "required"),
        keys::WS_TRANSPORT => matches!(value, "auto" | "http1" | "h2"),
        keys::REUSE_ONLY => matches!(value, "true" | "false"),
        keys::REUSE_MAX_AGE => {
            !value.is_empty()
                && value.bytes().all(|c| c.is_ascii_digit())
                && value.parse::<u64>().is_ok()
        }
        _ => return None,
    })
}

/// Strict host-side validation; shape registry membership is checked separately.
pub fn validate_host_option(key: &str, value: &str) -> Result<(), OptionError> {
    if option_valid(key, value) == Some(true) {
        Ok(())
    } else {
        Err(OptionError {
            key: key.into(),
            value: value.into(),
        })
    }
}

/// Pure option values; the host independently validates every setter and
/// snapshots the resulting resource at start/connect. Unknown keys are sent to
/// the host setter before any request, enabling forward-compatible probing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options(Vec<(String, String)>);
impl Options {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), OptionError> {
        if option_valid(key, value) == Some(false) {
            return Err(OptionError {
                key: key.into(),
                value: value.into(),
            });
        }
        if let Some((_, existing)) = self.0.iter_mut().find(|(k, _)| k == key) {
            *existing = value.into();
        } else {
            self.0.push((key.into(), value.into()));
        }
        Ok(())
    }
    pub fn shape(&mut self, value: &str) -> Result<(), OptionError> {
        self.set(keys::SHAPE, value)
    }
    pub fn http_version(&mut self, value: HttpVersion) -> Result<(), OptionError> {
        self.set(
            keys::HTTP_VERSION,
            match value {
                HttpVersion::Auto => "auto",
                HttpVersion::Http1 => "http1",
                HttpVersion::Http2 => "http2",
            },
        )
    }
    pub fn request_encoding(&mut self, value: Coding) -> Result<(), OptionError> {
        self.set(keys::REQUEST_ENCODING, value.as_str())
    }
    pub fn response_decoding(&mut self, values: &[Coding]) -> Result<(), OptionError> {
        let value = if values.is_empty() {
            "none".into()
        } else {
            values
                .iter()
                .map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(",")
        };
        self.set(keys::RESPONSE_DECODING, &value)
    }
    pub fn ws_compression(&mut self, value: WsCompression) -> Result<(), OptionError> {
        self.set(
            keys::WS_COMPRESSION,
            match value {
                WsCompression::Disabled => "disabled",
                WsCompression::Preferred => "preferred",
                WsCompression::Required => "required",
            },
        )
    }
    pub fn ws_transport(&mut self, value: WsTransport) -> Result<(), OptionError> {
        self.set(
            keys::WS_TRANSPORT,
            match value {
                WsTransport::Auto => "auto",
                WsTransport::Http1 => "http1",
                WsTransport::H2 => "h2",
            },
        )
    }
    pub fn reuse_max_age(&mut self, value: Duration) -> Result<(), OptionError> {
        let millis = u64::try_from(value.as_millis()).map_err(|_| OptionError {
            key: keys::REUSE_MAX_AGE.into(),
            value: value.as_millis().to_string(),
        })?;
        self.set(keys::REUSE_MAX_AGE, &millis.to_string())
    }
    pub fn continuation(&mut self, value: &str) -> Result<(), OptionError> {
        self.set(keys::CONTINUATION, value)
    }
    /// Only check out an existing socket. A miss returns continuation-unavailable
    /// without sending a handshake, allowing fresh request metadata on reconnect.
    pub fn reuse_only(&mut self, enabled: bool) -> Result<(), OptionError> {
        self.set(keys::REUSE_ONLY, if enabled { "true" } else { "false" })
    }
    fn host(&self) -> Result<types::Options, PluginError> {
        let opts = types::Options::new();
        for (key, value) in self.iter() {
            opts.set(key, value).map_err(net_error)?;
        }
        Ok(opts)
    }
}

fn net_error(error: types::NetError) -> PluginError {
    let kind = match error.code.as_str() {
        "unsupported" | "unsupported-option" => ErrorKind::Unsupported,
        "invalid" | "invalid-request" => ErrorKind::Invalid,
        "invalid-response" => ErrorKind::InvalidResponse,
        "auth" => ErrorKind::Auth,
        "continuation-not-found" => ErrorKind::ContinuationNotFound,
        "continuation-unavailable" => ErrorKind::ContinuationUnavailable,
        "protected-reasoning-rejected" => ErrorKind::ProtectedReasoningRejected,
        "trapped" => ErrorKind::Trapped,
        "cancelled" => ErrorKind::Cancelled,
        "deadline-exceeded" => ErrorKind::DeadlineExceeded,
        "resource-exhausted" => ErrorKind::ResourceExhausted,
        "provider-not-found" => ErrorKind::ProviderNotFound,
        "upstream" => ErrorKind::Upstream(UpstreamFailure {
            model_error_kind: error.model_error_kind.map(|s| model_kind(&s)),
            retry_after_milliseconds: error.retry_after_millis,
            transport_failure: error.transport_failure,
        }),
        _ => ErrorKind::InvalidResponse,
    };
    PluginError {
        kind,
        message: error.message,
        upstream_status: error.status,
    }
}
fn model_kind(s: &str) -> ModelErrorKind {
    match s {
        "authentication-error" => ModelErrorKind::AuthenticationError,
        "authorization-error" => ModelErrorKind::AuthorizationError,
        "not-found-error" => ModelErrorKind::NotFoundError,
        "rate-limit-error" => ModelErrorKind::RateLimitError,
        "quota-exceeded" => ModelErrorKind::QuotaExceeded,
        "invalid-request" => ModelErrorKind::InvalidRequest,
        "server-error" => ModelErrorKind::ServerError,
        "service-unavailable" => ModelErrorKind::ServiceUnavailable,
        "timeout" => ModelErrorKind::Timeout,
        "content-filtered" => ModelErrorKind::ContentFiltered,
        "context-length-exceeded" => ModelErrorKind::ContextLengthExceeded,
        "model-not-available" => ModelErrorKind::ModelNotAvailable,
        "stream-mid-error" => ModelErrorKind::StreamMidError,
        "unexpected-eof" => ModelErrorKind::UnexpectedEof,
        "unknown" => ModelErrorKind::Unknown,
        _ => ModelErrorKind::Other(s.into()),
    }
}

pub struct HttpResponse(http::Response);
impl HttpResponse {
    pub fn status(&self) -> Result<u16, PluginError> {
        self.0.status().map_err(net_error)
    }
    pub fn headers(&self) -> Result<Fields, PluginError> {
        self.0.headers().map(Fields::from_wit).map_err(net_error)
    }
    pub fn read_body(&self) -> Result<Option<Vec<u8>>, PluginError> {
        self.0.read_body().map_err(net_error)
    }
}
pub use websocket::Message as WsMessage;
pub struct WebSocket(websocket::Connection);
impl WebSocket {
    pub fn handshake_status(&self) -> u16 {
        self.0.handshake_status()
    }
    pub fn handshake_headers(&self) -> Fields {
        Fields::from_wit(self.0.handshake_headers())
    }
    /// Whether this checkout established a new handshake rather than reusing
    /// a pooled connection. The answer is fixed for this resource's lifetime.
    pub fn is_fresh_handshake(&self) -> Result<bool, PluginError> {
        self.0.is_fresh_handshake().map_err(net_error)
    }
    pub fn send(&self, msg: &WsMessage) -> Result<(), PluginError> {
        self.0.send(msg).map_err(net_error)
    }
    pub fn next(&self) -> Result<Option<WsMessage>, PluginError> {
        self.0.next().map_err(net_error)
    }
    pub fn close(&self, continuation_id: Option<&str>) -> Result<(), PluginError> {
        self.0.close(continuation_id).map_err(net_error)
    }
}
pub(crate) fn start(request: Request, options: &Options) -> Result<HttpResponse, PluginError> {
    let opts = options.host()?;
    http::start(
        &http::Request {
            method: request.method,
            url: request.url,
            headers: request.headers.into_wit(),
            body: request.body,
        },
        &opts,
    )
    .map(HttpResponse)
    .map_err(net_error)
}
pub(crate) fn connect(
    request: ConnectRequest,
    options: &Options,
) -> Result<WebSocket, PluginError> {
    let opts = options.host()?;
    websocket::connect(
        &websocket::ConnectRequest {
            url: request.url,
            headers: request.headers.into_wit(),
        },
        &opts,
    )
    .map(WebSocket)
    .map_err(net_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cookie_policy_is_validated_by_guest_and_host_setters() {
        let valid = r#"{"domains":["chatgpt.com","xn--bcher-kva.example"],"names":["Session","a!#$%&'*+-.^_`|~"],"prefixes":["__Secure-"]}"#;
        let policy = CookiePolicy::parse(valid).unwrap();
        assert_eq!(policy.domains, ["chatgpt.com", "xn--bcher-kva.example"]);
        assert_eq!(policy.names, ["Session", "a!#$%&'*+-.^_`|~"]);
        assert_eq!(policy.prefixes, ["__Secure-"]);
        assert!(validate_host_option(keys::COOKIES, valid).is_ok());
        let mut options = Options::new();
        options.set(keys::COOKIES, valid).unwrap();
        for invalid in [
            "null",
            "[[],[],[]]",
            "{",
            r#"{"unknown":[]}"#,
            r#"{"domains":null}"#,
            r#"{"domains":[1]}"#,
            r#"{"domains":[],"domains":[]}"#,
            r#"{"domains":["Example.com"]}"#,
            r#"{"domains":["*.example.com"]}"#,
            r#"{"domains":["example.com."]}"#,
            r#"{"domains":["-a.example"]}"#,
            r#"{"domains":["bücher.example"]}"#,
            r#"{"domains":["a..example"]}"#,
            r#"{"domain_suffixes":["example.com"]}"#,
            r#"{"domain_suffixes":[".example.com"]}"#,
            r#"{"domain_suffixes":["EXAMPLE.com"]}"#,
            r#"{"domain_suffixes":null}"#,
            r#"{"names":[""]}"#,
            r#"{"names":["bad name"]}"#,
            r#"{"names":["a=b"]}"#,
            r#"{"prefixes":[""]}"#,
            r#"{"prefixes":["a;"]}"#,
        ] {
            assert!(
                validate_host_option(keys::COOKIES, invalid).is_err(),
                "{invalid}"
            );
            assert!(options.set(keys::COOKIES, invalid).is_err(), "{invalid}");
        }
        assert_eq!(
            options.iter().collect::<Vec<_>>(),
            vec![(keys::COOKIES, valid)]
        );
        assert_eq!(CookiePolicy::parse("{}").unwrap().domains.len(), 0);
    }

    #[test]
    fn cookie_policy_enforces_resource_bounds() {
        let policy =
            |key: &str, entries: Vec<String>| serde_json::json!({(key): entries}).to_string();
        for key in ["domains", "names", "prefixes"] {
            assert!(CookiePolicy::parse(&policy(key, vec!["a".into(); 64])).is_ok());
            assert!(CookiePolicy::parse(&policy(key, vec!["a".into(); 65])).is_err());
        }
        assert!(CookiePolicy::parse(&policy("names", vec!["a".repeat(256)])).is_ok());
        assert!(CookiePolicy::parse(&policy("names", vec!["a".repeat(257)])).is_err());
        assert!(CookiePolicy::parse(&policy("prefixes", vec!["a".repeat(257)])).is_err());
        assert!(CookiePolicy::parse(&policy("domains", vec!["a".repeat(64)])).is_err());
        let long_host = format!(
            "{}.{}.{}.{}",
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(61)
        );
        assert!(CookiePolicy::parse(&policy("domains", vec![long_host.clone()])).is_ok());
        assert!(CookiePolicy::parse(&policy("domains", vec![format!("{long_host}d")])).is_err());
        assert!(CookiePolicy::parse(&format!("{{}}{}", " ".repeat(16 * 1024))).is_err());
    }

    #[test]
    fn documented_invalid_request_decodes_to_native_invalid() {
        let error = net_error(types::NetError {
            code: "invalid-request".into(),
            message: "invalid header".into(),
            status: None,
            retry_after_millis: None,
            model_error_kind: None,
            transport_failure: None,
        });
        assert!(matches!(error.kind, ErrorKind::Invalid));
        assert_eq!(error.message, "invalid header");
    }
    #[test]
    fn fields_replace_first_and_remove_later_duplicates_without_reordering() {
        let mut f = Fields::new();
        f.append("X-A", b"old".to_vec());
        f.append("X-B", b"middle".to_vec());
        f.append("x-a", b"later".to_vec());
        f.set("x-A", b"new".to_vec());
        assert_eq!(
            f.into_vec(),
            vec![
                Field {
                    name: "x-A".into(),
                    bytes: b"new".to_vec()
                },
                Field {
                    name: "X-B".into(),
                    bytes: b"middle".to_vec()
                }
            ]
        );
    }
    #[test]
    fn host_option_validation_rejects_unknown_and_malformed_values() {
        assert!(validate_host_option("future.option", "x").is_err());
        assert!(validate_host_option(keys::REUSE_MAX_AGE, "-1").is_err());
        assert!(validate_host_option(keys::REUSE_MAX_AGE, "1200").is_ok());
        let mut options = Options::new();
        options.set("future.option", "x").unwrap();
        assert_eq!(
            options.iter().collect::<Vec<_>>(),
            vec![("future.option", "x")]
        );
    }
    #[test]
    fn options_reject_bad_values_without_changing_previous_setting() {
        let mut o = Options::new();
        o.http_version(HttpVersion::Http2).unwrap();
        assert!(o.set(keys::HTTP_VERSION, "http3").is_err());
        assert_eq!(
            o.iter().collect::<Vec<_>>(),
            vec![(keys::HTTP_VERSION, "http2")]
        );
    }
}
