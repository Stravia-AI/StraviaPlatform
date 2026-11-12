//! Open Responses WebSocket 升级握手与 permessage-deflate 协商。
//!
//! axum 内置的 `WebSocketUpgrade` 绑定的 tungstenite 不含 RFC 7692，只能忽略客户端的压缩
//! offer。这里改用出站插件所用的 tungstenite 分支：请求校验与扩展协商交给它的服务端握手
//! 状态机，升级后的帧层也由它承担，避免本地再实现一份 offer 解析。

use std::io::{self, Cursor, Read, Write};

use axum::body::Body;
use axum::extract::FromRequestParts;
use axum::http::header::{
    CONNECTION, SEC_WEBSOCKET_EXTENSIONS, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_VERSION, UPGRADE,
};
use axum::http::{HeaderMap, StatusCode, request::Parts};
use axum::response::{IntoResponse, Response};
use hyper::upgrade::{OnUpgrade, Upgraded};
use hyper_util::rt::TokioIo;
use tokio_tungstenite_deflate::WebSocketStream;
use tungstenite_deflate::{
    extensions::compression::deflate::{DeflateConfig, EXTENSION_NAME},
    handshake::server::{
        ErrorResponse, Request as HandshakeRequest, Response as HandshakeResponse,
    },
    protocol::{Role, WebSocketConfig},
};

pub(super) type Socket = WebSocketStream<TokioIo<Upgraded>>;

/// 通过校验、等待交给 hyper 完成协议切换的 WebSocket 升级请求。
pub struct Upgrade {
    on_upgrade: OnUpgrade,
    response_headers: HeaderMap,
    deflate: Option<DeflateConfig>,
}

impl<S: Send + Sync> FromRequestParts<S> for Upgrade {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let response_headers = accept(&parts.headers).map_err(|error| {
            tracing::debug!(%error, "rejected invalid WebSocket upgrade request");
            (
                StatusCode::BAD_REQUEST,
                "Invalid WebSocket upgrade request.",
            )
                .into_response()
        })?;
        let Some(on_upgrade) = parts.extensions.remove::<OnUpgrade>() else {
            return Err((
                StatusCode::UPGRADE_REQUIRED,
                "WebSocket request couldn't be upgraded since no upgrade state was present.",
            )
                .into_response());
        };
        let deflate = agreed_deflate(&response_headers).map_err(|error| {
            tracing::error!(%error, "WebSocket handshake produced an unusable extension");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "WebSocket extension negotiation failed.",
            )
                .into_response()
        })?;
        Ok(Self {
            on_upgrade,
            response_headers,
            deflate,
        })
    }
}

impl Upgrade {
    /// 返回 101 响应，并在 hyper 交出底层连接后以 `serve` 接管 WebSocket。
    ///
    /// `max_message_bytes` 同时约束解压后的消息大小，防止压缩炸弹。
    pub(super) fn on_upgrade<F, Fut>(self, max_message_bytes: usize, serve: F) -> Response
    where
        F: FnOnce(Socket) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let Self {
            on_upgrade,
            response_headers,
            deflate,
        } = self;
        tokio::spawn(async move {
            let upgraded = match on_upgrade.await {
                Ok(upgraded) => upgraded,
                Err(error) => {
                    tracing::debug!(%error, "WebSocket upgrade did not complete");
                    return;
                }
            };
            let mut config = WebSocketConfig::default().max_message_size(Some(max_message_bytes));
            config.extensions.permessage_deflate = deflate;
            let socket = WebSocketStream::from_raw_socket(
                TokioIo::new(upgraded),
                Role::Server,
                Some(config),
            )
            .await;
            serve(socket).await;
        });
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
        *response.headers_mut() = response_headers;
        response
    }
}

fn server_deflate() -> DeflateConfig {
    let mut config = DeflateConfig::new();
    // 事件小而密集，延迟比压缩率重要；与 HTTP 响应压缩一致取最快档。
    config.compression = flate2::Compression::fast();
    config
}

/// 握手状态机只从中读取一次请求头；它写出的 101 响应由回调截获，写入即丢弃。
struct RequestHead(Cursor<Vec<u8>>);

impl Read for RequestHead {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for RequestHead {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// 把已由 hyper 解析的升级请求头重新编码，交给 tungstenite 做校验与协商，
/// 返回应当原样发给客户端的 101 响应头。
#[expect(
    clippy::result_large_err,
    reason = "tungstenite 的 Callback 固定返回 ErrorResponse"
)]
fn accept(request: &HeaderMap) -> Result<HeaderMap, String> {
    let mut head = b"GET / HTTP/1.1\r\n".to_vec();
    // tungstenite 只检查首个 Connection 字段；合并多行以保持与 axum 一致的宽容度。
    let connection = request
        .get_all(CONNECTION)
        .iter()
        .map(|value| value.as_bytes())
        .collect::<Vec<_>>()
        .join(&b", "[..]);
    if !connection.is_empty() {
        head.extend_from_slice(b"connection: ");
        head.extend_from_slice(&connection);
        head.extend_from_slice(b"\r\n");
    }
    for name in [
        UPGRADE,
        SEC_WEBSOCKET_VERSION,
        SEC_WEBSOCKET_KEY,
        SEC_WEBSOCKET_EXTENSIONS,
    ] {
        for value in request.get_all(&name) {
            head.extend_from_slice(name.as_str().as_bytes());
            head.extend_from_slice(b": ");
            head.extend_from_slice(value.as_bytes());
            head.extend_from_slice(b"\r\n");
        }
    }
    head.extend_from_slice(b"\r\n");

    let mut config = WebSocketConfig::default();
    config.extensions.permessage_deflate = Some(server_deflate());
    let mut response_headers = None;
    tungstenite_deflate::accept_hdr_with_config(
        RequestHead(Cursor::new(head)),
        |_: &HandshakeRequest, response: HandshakeResponse| -> Result<_, ErrorResponse> {
            response_headers = Some(response.headers().clone());
            Ok(response)
        },
        Some(config),
    )
    .map_err(|error| error.to_string())?;
    response_headers.ok_or_else(|| "handshake finished without a response".to_owned())
}

/// 从握手状态机自己生成的 101 响应头还原协商结果。
///
/// 该响应是 offer 与服务端配置的交集，逐项列出了两个方向的上下文接管与窗口位数，
/// 因此可直接作为帧层压缩器的参数；未知参数一律报错，避免两端压缩上下文失配。
fn agreed_deflate(response: &HeaderMap) -> Result<Option<DeflateConfig>, String> {
    let Some(extensions) = response.get(SEC_WEBSOCKET_EXTENSIONS) else {
        return Ok(None);
    };
    let extensions = extensions.to_str().map_err(|error| error.to_string())?;
    let mut parameters = extensions.split(';').map(str::trim);
    if parameters.next() != Some(EXTENSION_NAME) {
        return Err(format!("unexpected WebSocket extension `{extensions}`"));
    }
    let mut config = server_deflate();
    for parameter in parameters {
        let (name, value) = parameter
            .split_once('=')
            .map_or((parameter, None), |(name, value)| (name, Some(value)));
        let role = match name {
            "server_no_context_takeover" => {
                config.server_no_context_takeover = true;
                continue;
            }
            "client_no_context_takeover" => {
                config.client_no_context_takeover = true;
                continue;
            }
            "server_max_window_bits" => Role::Server,
            "client_max_window_bits" => Role::Client,
            _ => return Err(format!("unexpected permessage-deflate parameter `{name}`")),
        };
        let bits = value
            .and_then(|bits| bits.parse().ok())
            .ok_or_else(|| format!("invalid value for permessage-deflate `{name}`"))?;
        config = config
            .set_max_window_bits(role, bits)
            .map_err(|error| error.to_string())?;
    }
    Ok(Some(config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn upgrade_request(extensions: &[&'static str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(CONNECTION, HeaderValue::from_static("Upgrade"));
        headers.insert(UPGRADE, HeaderValue::from_static("websocket"));
        headers.insert(SEC_WEBSOCKET_VERSION, HeaderValue::from_static("13"));
        headers.insert(
            SEC_WEBSOCKET_KEY,
            HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="),
        );
        for extension in extensions {
            headers.append(
                SEC_WEBSOCKET_EXTENSIONS,
                HeaderValue::from_static(extension),
            );
        }
        headers
    }

    fn negotiate(extensions: &[&'static str]) -> (HeaderMap, Option<DeflateConfig>) {
        let response = accept(&upgrade_request(extensions)).expect("valid upgrade request");
        let deflate = agreed_deflate(&response).expect("negotiated extension");
        (response, deflate)
    }

    #[test]
    fn accept_key_and_upgrade_headers_follow_rfc_6455() {
        let (response, _) = negotiate(&[]);
        assert_eq!(
            response["sec-websocket-accept"],
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
        assert_eq!(response["upgrade"], "websocket");
    }

    #[test]
    fn compression_is_negotiated_only_when_offered() {
        let (response, deflate) = negotiate(&[]);
        assert!(!response.contains_key(SEC_WEBSOCKET_EXTENSIONS));
        assert!(deflate.is_none());

        let (response, deflate) = negotiate(&["permessage-deflate; client_max_window_bits"]);
        assert_eq!(response[SEC_WEBSOCKET_EXTENSIONS], "permessage-deflate");
        let deflate = deflate.expect("permessage-deflate");
        assert!(!deflate.server_no_context_takeover);
        assert_eq!(deflate.server_max_window_bits().get(), 15);
    }

    #[test]
    fn offered_context_takeover_and_window_limits_are_honored() {
        let (response, deflate) = negotiate(&[
            "permessage-deflate; server_no_context_takeover; client_no_context_takeover; \
             server_max_window_bits=10; client_max_window_bits=12",
        ]);
        let response = response[SEC_WEBSOCKET_EXTENSIONS].to_str().unwrap();
        for expected in [
            "server_no_context_takeover",
            "client_no_context_takeover",
            "server_max_window_bits=10",
            "client_max_window_bits=12",
        ] {
            assert!(response.contains(expected), "{response}");
        }
        let deflate = deflate.expect("permessage-deflate");
        assert!(deflate.server_no_context_takeover);
        assert!(deflate.client_no_context_takeover);
        assert_eq!(deflate.server_max_window_bits().get(), 10);
        assert_eq!(deflate.client_max_window_bits().get(), 12);
    }

    #[test]
    fn first_acceptable_offer_wins_across_header_lines() {
        let (response, deflate) = negotiate(&[
            "permessage-deflate; server_max_window_bits=8",
            "permessage-deflate; server_max_window_bits=11, permessage-deflate",
        ]);
        assert_eq!(
            response[SEC_WEBSOCKET_EXTENSIONS],
            "permessage-deflate; server_max_window_bits=11"
        );
        assert_eq!(deflate.unwrap().server_max_window_bits().get(), 11);
    }

    #[test]
    fn unsupported_offers_upgrade_without_compression() {
        for offer in [
            "permessage-deflate; server_max_window_bits=8",
            "permessage-deflate; unknown_parameter",
            "permessage-deflate; server_no_context_takeover=1",
            "x-webkit-deflate-frame",
        ] {
            let (response, deflate) = negotiate(&[offer]);
            assert!(!response.contains_key(SEC_WEBSOCKET_EXTENSIONS), "{offer}");
            assert!(deflate.is_none(), "{offer}");
        }
    }

    #[test]
    fn multiple_connection_lines_are_merged() {
        let mut headers = upgrade_request(&[]);
        headers.insert(CONNECTION, HeaderValue::from_static("keep-alive"));
        headers.append(CONNECTION, HeaderValue::from_static("Upgrade"));
        accept(&headers).expect("Upgrade token on a later Connection line");
    }

    #[test]
    fn invalid_upgrade_requests_are_rejected() {
        for header in [
            CONNECTION,
            UPGRADE,
            SEC_WEBSOCKET_VERSION,
            SEC_WEBSOCKET_KEY,
        ] {
            let mut headers = upgrade_request(&[]);
            headers.remove(&header);
            accept(&headers).expect_err(header.as_str());
        }
        let mut headers = upgrade_request(&[]);
        headers.insert(SEC_WEBSOCKET_VERSION, HeaderValue::from_static("8"));
        accept(&headers).expect_err("unsupported WebSocket version");
    }
}
