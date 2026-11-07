//! Ordered WebSocket handshake planning and RFC 7692 framing.

use std::{collections::BTreeSet, io};

use super::transport::{
    Head, RequestPlan, Route, TransportError, UpgradeResponse, UpgradedIo, VendorTransport,
};
use bytes::Bytes;
use futures::{SinkExt as _, StreamExt as _};
use stravia_vendor_runtime::HostFailure;
use stravia_vendor_sdk::net::{Field, WsCompression};
use tungstenite_deflate::{
    extensions::compression::deflate::DeflateConfig,
    handshake::{client::generate_key, derive_accept_key},
    protocol::{Role, WebSocketConfig},
};
use url::Url;

/// Limit decompression while it happens, not after allocating the expansion.
pub(super) const MAX_MESSAGE_BYTES: usize = 256 * 1024 * 1024;

pub(super) fn validate_headers(
    fields: &[Field],
    compression: WsCompression,
) -> Result<(), HostFailure> {
    let controlled = [
        "upgrade",
        "connection",
        "sec-websocket-key",
        "sec-websocket-version",
        "sec-websocket-extensions",
        "sec-websocket-protocol",
    ];
    for name in controlled {
        if fields
            .iter()
            .filter(|field| field.name.eq_ignore_ascii_case(name))
            .count()
            > 1
        {
            return Err(super::invalid("duplicate WebSocket handshake header"));
        }
    }
    if let Some(key) = fields
        .iter()
        .find(|field| field.name.eq_ignore_ascii_case("sec-websocket-key"))
        && !key.bytes.is_empty()
    {
        return Err(super::invalid("WebSocket key must be an empty slot"));
    }
    let extension = fields
        .iter()
        .find(|field| field.name.eq_ignore_ascii_case("sec-websocket-extensions"));
    if let Some(field) = extension {
        if compression == WsCompression::Disabled {
            return Err(super::invalid("WebSocket compression is disabled"));
        }
        let offer = std::str::from_utf8(&field.bytes)
            .map_err(|_| super::invalid("invalid WebSocket extension offer"))?;
        validate_offer(offer).map_err(|_| super::invalid("invalid WebSocket extension offer"))?;
    } else if compression == WsCompression::Required {
        return Err(super::invalid(
            "required WebSocket compression needs an offer",
        ));
    }
    Ok(())
}

pub(super) struct Handshake {
    url: Url,
    headers: Vec<Field>,
    protocols: Vec<String>,
    compression: WsCompression,
    offer: Option<String>,
    accept: String,
}

impl Handshake {
    pub(super) fn new(
        url: &Url,
        mut fields: Vec<Field>,
        compression: WsCompression,
    ) -> Result<Self, HostFailure> {
        validate_headers(&fields, compression)?;
        if !matches!(url.scheme(), "ws" | "wss" | "http" | "https") {
            return Err(super::invalid("invalid WebSocket URL scheme"));
        }
        let mut protocols = Vec::new();
        if let Some(field) = fields
            .iter()
            .find(|field| field.name.eq_ignore_ascii_case("sec-websocket-protocol"))
        {
            let text = std::str::from_utf8(&field.bytes)
                .map_err(|_| super::invalid("invalid WebSocket subprotocol"))?;
            let mut seen = BTreeSet::new();
            for protocol in text.split(',').map(str::trim) {
                if protocol.is_empty() || !protocol.bytes().all(is_token) || !seen.insert(protocol)
                {
                    return Err(super::invalid("invalid or duplicate WebSocket subprotocol"));
                }
                protocols.push(protocol.to_owned());
            }
        }
        let offer = fields
            .iter()
            .find(|field| field.name.eq_ignore_ascii_case("sec-websocket-extensions"))
            .map(|field| String::from_utf8(field.bytes.clone()).expect("offer validated"));
        let key = generate_key();
        let accept = derive_accept_key(key.as_bytes());
        for (name, value) in [
            ("Connection", "Upgrade"),
            ("Upgrade", "websocket"),
            ("Sec-WebSocket-Version", "13"),
            ("Sec-WebSocket-Key", key.as_str()),
        ] {
            if let Some(field) = fields
                .iter_mut()
                .find(|field| field.name.eq_ignore_ascii_case(name))
            {
                if !field.bytes.is_empty() && !field.bytes.eq_ignore_ascii_case(value.as_bytes()) {
                    return Err(super::invalid("invalid WebSocket handshake slot value"));
                }
                field.bytes = value.as_bytes().to_vec();
            } else {
                fields.push(Field {
                    name: name.to_owned(),
                    bytes: value.as_bytes().to_vec(),
                });
            }
        }
        Ok(Self {
            url: url.clone(),
            headers: fields,
            protocols,
            compression,
            offer,
            accept,
        })
    }

    #[cfg(test)]
    pub(super) fn headers(&self) -> &[Field] {
        &self.headers
    }

    pub(super) fn headers_mut(&mut self) -> &mut Vec<Field> {
        &mut self.headers
    }

    pub(super) async fn send(
        &self,
        transport: &VendorTransport,
        route: Route,
        observer: Option<Box<dyn FnOnce(super::transport::RequestHead) + Send>>,
    ) -> Result<UpgradeResponse, TransportError> {
        transport
            .upgrade(
                RequestPlan {
                    method: "GET".into(),
                    uri: self.url.to_string(),
                    fields: self.headers.clone(),
                    body: Bytes::new(),
                    observer,
                },
                route,
            )
            .await
    }

    pub(super) async fn finish(
        self,
        head: &Head,
        io: UpgradedIo,
    ) -> Result<WebSocket, HostFailure> {
        let extension = self
            .validate(head)
            .map_err(super::websocket_transport_failure)?;
        let compressed = extension.is_some();
        let socket = tokio_tungstenite_deflate::WebSocketStream::from_raw_socket(
            io,
            Role::Client,
            Some(options(extension)),
        )
        .await;
        Ok(WebSocket {
            socket,
            compressed,
            ended: false,
        })
    }

    fn validate(&self, head: &Head) -> io::Result<Option<DeflateConfig>> {
        match head.status {
            101 => {
                if !contains_token(&head.fields, "connection", "upgrade")?
                    || !single_header(&head.fields, "upgrade")?
                        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
                {
                    return Err(protocol_error(
                        "invalid WebSocket Connection/Upgrade headers",
                    ));
                }
                if single_header(&head.fields, "sec-websocket-accept")?
                    != Some(self.accept.as_str())
                {
                    return Err(protocol_error("invalid WebSocket accept nonce"));
                }
            }
            200 => {}
            _ => return Err(protocol_error("invalid WebSocket upgrade status")),
        }
        if let Some(version) = single_header(&head.fields, "sec-websocket-version")?
            && version != "13"
        {
            return Err(protocol_error("unsupported WebSocket version"));
        }
        if let Some(selected) = single_header(&head.fields, "sec-websocket-protocol")?
            && !self.protocols.iter().any(|offered| offered == selected)
        {
            return Err(protocol_error(
                "server selected an unoffered WebSocket subprotocol",
            ));
        }
        let extension = single_header(&head.fields, "sec-websocket-extensions")?;
        match (self.compression, self.offer.as_deref(), extension) {
            (_, None, Some(_)) | (WsCompression::Disabled, _, Some(_)) => Err(protocol_error(
                "server selected an unoffered WebSocket extension",
            )),
            (WsCompression::Required, _, None) => Err(protocol_error(
                "server did not accept required permessage-deflate",
            )),
            (_, _, None) => Ok(None),
            (_, Some(offer), Some(value)) => {
                validate_selection(offer, value)?;
                validate_extension(value).map(Some)
            }
        }
    }
}

fn is_token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

fn single_header<'a>(fields: &'a [Field], name: &str) -> io::Result<Option<&'a str>> {
    let mut values = fields
        .iter()
        .filter(|field| field.name.eq_ignore_ascii_case(name));
    let value = values
        .next()
        .map(|field| std::str::from_utf8(&field.bytes).map(str::trim))
        .transpose()
        .map_err(protocol_error)?;
    if values.next().is_some() {
        return Err(protocol_error("duplicate WebSocket handshake header"));
    }
    Ok(value)
}

fn contains_token(fields: &[Field], name: &str, expected: &str) -> io::Result<bool> {
    let mut found = false;
    for field in fields
        .iter()
        .filter(|field| field.name.eq_ignore_ascii_case(name))
    {
        for token in std::str::from_utf8(&field.bytes)
            .map_err(protocol_error)?
            .split(',')
            .map(str::trim)
        {
            if token.is_empty() || !token.bytes().all(is_token) {
                return Err(protocol_error("invalid HTTP upgrade token"));
            }
            found |= token.eq_ignore_ascii_case(expected);
        }
    }
    Ok(found)
}

// RFC 7692 permits a valueless client_max_window_bits only in the offer.
fn validate_offer(offer: &str) -> io::Result<()> {
    validate_extension_inner(offer, true).map(|_| ())
}

fn validate_selection(offer: &str, selection: &str) -> io::Result<()> {
    validate_offer(offer)?;
    validate_extension(selection)?;
    fn lookup<'a>(text: &'a str, name: &str) -> Option<Option<&'a str>> {
        text.split(';').skip(1).find_map(|part| {
            let part = part.trim();
            let (n, v) = part
                .split_once('=')
                .map_or((part, None), |(n, v)| (n.trim(), Some(v.trim())));
            (n == name).then_some(v)
        })
    }
    fn number(text: &str) -> u8 {
        text.bytes()
            .filter(u8::is_ascii_digit)
            .fold(0, |n, b| n * 10 + b - b'0')
    }
    if lookup(offer, "server_no_context_takeover").is_some()
        && lookup(selection, "server_no_context_takeover").is_none()
    {
        return Err(protocol_error(
            "server ignored required no-context-takeover",
        ));
    }
    if let Some(Some(limit)) = lookup(offer, "server_max_window_bits") {
        let actual = lookup(selection, "server_max_window_bits")
            .flatten()
            .ok_or_else(|| protocol_error("server ignored offered window limit"))?;
        if number(actual) > number(limit) {
            return Err(protocol_error("server exceeded offered window limit"));
        }
    }
    if let Some(Some(limit)) = lookup(offer, "client_max_window_bits")
        && let Some(Some(actual)) = lookup(selection, "client_max_window_bits")
        && number(actual) > number(limit)
    {
        return Err(protocol_error(
            "server exceeded offered client window limit",
        ));
    }
    if lookup(selection, "client_max_window_bits").is_some()
        && lookup(offer, "client_max_window_bits").is_none()
    {
        return Err(protocol_error(
            "server selected unoffered client window parameter",
        ));
    }
    Ok(())
}

/// 从真实协商参数构造引擎上下文，不重复握手，也不把 offer 当作协商成功。
fn validate_extension(value: &str) -> io::Result<DeflateConfig> {
    validate_extension_inner(value, false)
}

fn validate_extension_inner(value: &str, offer: bool) -> io::Result<DeflateConfig> {
    let mut parts = value.split(';');
    if parts.next().map(str::trim) != Some("permessage-deflate") {
        return Err(protocol_error("unsupported WebSocket extension"));
    }
    let mut seen = BTreeSet::new();
    let mut config = DeflateConfig::new();
    for part in parts {
        let (name, value) = match part.trim().split_once('=') {
            Some((name, value)) => (name.trim(), Some(value.trim())),
            None => (part.trim(), None),
        };
        if !seen.insert(name) {
            return Err(protocol_error("duplicate permessage-deflate parameter"));
        }
        match name {
            "server_no_context_takeover" | "client_no_context_takeover" if value.is_none() => {
                if name == "server_no_context_takeover" {
                    config.server_no_context_takeover = true;
                } else {
                    config.client_no_context_takeover = true;
                }
            }
            "server_max_window_bits" | "client_max_window_bits" => {
                let value = match value {
                    Some(value) => value,
                    None if offer && name == "client_max_window_bits" => "15",
                    None => return Err(protocol_error("missing permessage-deflate window bits")),
                };
                let quoted = value.starts_with('"') && value.ends_with('"') && value.len() >= 2;
                let value = if quoted {
                    &value[1..value.len() - 1]
                } else {
                    value
                };
                let mut digits = value.bytes();
                let mut bits = 0_u8;
                let mut count = 0;
                while let Some(digit) = digits.next() {
                    let digit = if quoted && digit == b'\\' {
                        digits.next().ok_or_else(|| {
                            protocol_error("invalid quoted permessage-deflate window bits")
                        })?
                    } else {
                        digit
                    };
                    if !digit.is_ascii_digit() || count == 2 || (count == 0 && digit == b'0') {
                        return Err(protocol_error("invalid permessage-deflate window bits"));
                    }
                    bits = bits * 10 + digit - b'0';
                    count += 1;
                }
                // 服务端 8 位窗口的合法输出可由较大的解码窗口安全读取；
                // zlib 不能表达客户端 8 位压缩窗口，不能假装支持该协商结果。
                if !(8..=15).contains(&bits)
                    || (!offer && name == "client_max_window_bits" && bits == 8)
                {
                    return Err(protocol_error("unsupported permessage-deflate window bits"));
                }
                config = config
                    .set_max_window_bits(
                        if name == "server_max_window_bits" {
                            Role::Server
                        } else {
                            Role::Client
                        },
                        bits.max(9),
                    )
                    .map_err(protocol_error)?;
            }
            _ => {
                return Err(protocol_error(
                    "invalid or unknown permessage-deflate parameter",
                ));
            }
        }
    }
    Ok(config)
}

fn options(compression: Option<DeflateConfig>) -> WebSocketConfig {
    let mut config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES))
        .max_write_buffer_size(MAX_MESSAGE_BYTES + 128 * 1024);
    config.extensions.permessage_deflate = compression;
    config
}

#[derive(Debug)]
pub(super) enum Message {
    Text(String),
    Binary(Bytes),
    Ping(Bytes),
    Pong(Bytes),
    Close { code: u16, reason: String },
}

pub(super) struct WebSocket {
    socket: tokio_tungstenite_deflate::WebSocketStream<UpgradedIo>,
    compressed: bool,
    ended: bool,
}

impl WebSocket {
    pub(super) fn compressed(&self) -> bool {
        self.compressed
    }

    pub(super) async fn send(&mut self, message: Message) -> io::Result<()> {
        use tungstenite_deflate::{
            Message as EngineMessage,
            protocol::frame::{CloseFrame, coding::CloseCode},
        };
        let frame = match message {
            Message::Text(text) => EngineMessage::Text(text.into()),
            Message::Binary(bytes) => EngineMessage::Binary(bytes),
            Message::Ping(bytes) if bytes.len() <= 125 => EngineMessage::Ping(bytes),
            Message::Pong(bytes) if bytes.len() <= 125 => EngineMessage::Pong(bytes),
            Message::Close { code, reason } => {
                if !CloseCode::from(code).is_allowed() || reason.len() > 123 {
                    return Err(protocol_error("invalid outgoing WebSocket close frame"));
                }
                EngineMessage::Close(Some(CloseFrame {
                    code: CloseCode::from(code),
                    reason: reason.into(),
                }))
            }
            _ => {
                return Err(protocol_error(
                    "WebSocket control payload exceeds 125 bytes",
                ));
            }
        };
        self.socket.send(frame).await.map_err(io::Error::other)
    }

    pub(super) async fn next(&mut self) -> Option<io::Result<Message>> {
        if self.ended {
            return None;
        }
        let result = match self.socket.next().await? {
            Ok(frame) => message(frame),
            Err(error) => Err(io::Error::other(error)),
        };
        if result.is_err() || matches!(result, Ok(Message::Close { .. })) {
            self.ended = true;
        }
        Some(result)
    }

    pub(super) async fn close(&mut self, code: u16, reason: Option<String>) -> io::Result<()> {
        self.send(Message::Close {
            code,
            reason: reason.unwrap_or_default(),
        })
        .await
    }
}

fn message(frame: tungstenite_deflate::Message) -> io::Result<Message> {
    use tungstenite_deflate::Message as EngineMessage;
    match frame {
        EngineMessage::Text(text) => Ok(Message::Text(text.to_string())),
        EngineMessage::Binary(bytes) => Ok(Message::Binary(bytes)),
        EngineMessage::Ping(bytes) => Ok(Message::Ping(bytes)),
        EngineMessage::Pong(bytes) => Ok(Message::Pong(bytes)),
        EngineMessage::Close(close) => Ok(Message::Close {
            code: close
                .as_ref()
                .map(|close| close.code.into())
                .unwrap_or(1005),
            reason: close
                .map(|close| close.reason.to_string())
                .unwrap_or_default(),
        }),
        _ => Err(protocol_error(
            "invalid WebSocket control or continuation frame",
        )),
    }
}

fn protocol_error(message: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

#[cfg(test)]
#[path = "websocket/h2_tests.rs"]
mod h2_tests;

#[cfg(test)]
#[path = "websocket_tests.rs"]
mod tests;
