//! Planning of ordered request fields before the transport selects a protocol.
//!
//! Field names and duplicate order are retained. Only framing slots are filled
//! here; protocol-specific normalization belongs to the negotiated transport.

use http::header::{HeaderName, HeaderValue};
use stravia_vendor_sdk::net::Field;
use url::{Position, Url};

use super::shape::ResolvedShape;

/// Validate and fill the HTTP/1 slots without disturbing the other field lines.
pub(super) fn plan_http(
    fields: Vec<Field>,
    url: &Url,
    body_len: Option<usize>,
    shape: &ResolvedShape,
) -> Result<Vec<Field>, String> {
    plan(fields, url, body_len, true, shape)
}

/// Validate common request fields and fill Host. WebSocket handshake slots are
/// validated and filled by the handshake implementation.
pub(super) fn plan_ws(
    fields: Vec<Field>,
    url: &Url,
    shape: &ResolvedShape,
) -> Result<Vec<Field>, String> {
    plan(fields, url, None, false, shape)
}

fn plan(
    mut fields: Vec<Field>,
    url: &Url,
    body_len: Option<usize>,
    is_http: bool,
    shape: &ResolvedShape,
) -> Result<Vec<Field>, String> {
    if url.host().is_none() {
        return Err("URL has no host".into());
    }
    let authority = &url[Position::BeforeHost..Position::AfterPort];
    let mut host = None;
    let mut length = None;
    let mut transfer = None;
    for (index, field) in fields.iter_mut().enumerate() {
        HeaderName::from_bytes(field.name.as_bytes())
            .map_err(|_| "invalid header name".to_string())?;
        HeaderValue::from_bytes(&field.bytes).map_err(|_| "invalid header value".to_string())?;
        let name = field.name.as_str();
        if name.eq_ignore_ascii_case("proxy-authorization")
            || name.eq_ignore_ascii_case("proxy-connection")
        {
            return Err("proxy headers cannot be supplied by the request".into());
        }
        if name.eq_ignore_ascii_case("host") {
            if host.replace(index).is_some() {
                return Err("duplicate Host header".into());
            }
            if !field.bytes.is_empty() && !field.bytes.eq_ignore_ascii_case(authority.as_bytes()) {
                return Err("Host does not match the authorized URL".into());
            }
            field.bytes = authority.as_bytes().to_vec();
        } else if name.eq_ignore_ascii_case("content-length") {
            if length.replace(index).is_some() {
                return Err("duplicate Content-Length header".into());
            }
            if field.bytes.iter().any(|byte| !byte.is_ascii_digit()) {
                return Err("Content-Length must be a decimal length or an empty slot".into());
            }
            if let Some(body_len) = body_len {
                fill_length(field, body_len)?;
            }
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            if transfer.replace(index).is_some() {
                return Err("Transfer-Encoding requires one chunked value".into());
            }
            if field.bytes.is_empty() {
                field.bytes = b"chunked".to_vec();
            } else if !trim_ascii(&field.bytes).eq_ignore_ascii_case(b"chunked") {
                return Err("Transfer-Encoding requires one chunked value".into());
            }
        } else if name.eq_ignore_ascii_case("te") {
            if !trim_ascii(&field.bytes).eq_ignore_ascii_case(b"trailers") {
                return Err("TE only permits trailers".into());
            }
        } else if is_http && name.eq_ignore_ascii_case("upgrade") {
            return Err("Upgrade is not allowed for ordinary HTTP requests".into());
        } else if is_http && name.eq_ignore_ascii_case("connection") {
            let value = trim_ascii(&field.bytes);
            if !value.eq_ignore_ascii_case(b"keep-alive") && !value.eq_ignore_ascii_case(b"close") {
                return Err("Connection only permits keep-alive or close".into());
            }
        }
    }
    if transfer.is_some() && length.is_some() {
        return Err("Content-Length and Transfer-Encoding cannot be combined".into());
    }
    if host.is_none() {
        insert_slot(
            &mut fields,
            "Host",
            authority.as_bytes().to_vec(),
            shape.http1.slot_positions.host.as_deref(),
            true,
        );
    }
    if let Some(body_len) = body_len
        && body_len != 0
        && length.is_none()
        && transfer.is_none()
    {
        insert_slot(
            &mut fields,
            "Content-Length",
            body_len.to_string().into_bytes(),
            shape.http1.slot_positions.content_length.as_deref(),
            false,
        );
    }
    Ok(fields)
}

/// Complete framing after asynchronous content encoding has produced the wire body.
pub(super) fn finish_http(
    fields: &mut Vec<Field>,
    body_len: usize,
    shape: &ResolvedShape,
) -> Result<(), String> {
    let mut length = None;
    let mut transfer = false;
    for (index, field) in fields.iter_mut().enumerate() {
        if field.name.eq_ignore_ascii_case("content-length") {
            fill_length(field, body_len)?;
            length = Some(index);
        } else if field.name.eq_ignore_ascii_case("transfer-encoding") {
            transfer = true;
        }
    }
    if body_len != 0 && length.is_none() && !transfer {
        insert_slot(
            fields,
            "Content-Length",
            body_len.to_string().into_bytes(),
            shape.http1.slot_positions.content_length.as_deref(),
            false,
        );
    }
    Ok(())
}

fn fill_length(field: &mut Field, body_len: usize) -> Result<(), String> {
    let expected = body_len.to_string();
    if !field.bytes.is_empty() && field.bytes != expected.as_bytes() {
        return Err("Content-Length does not match the request body".into());
    }
    field.bytes = expected.into_bytes();
    Ok(())
}

fn insert_slot(
    fields: &mut Vec<Field>,
    name: &str,
    bytes: Vec<u8>,
    position: Option<&str>,
    first_by_default: bool,
) {
    let field = Field {
        name: name.into(),
        bytes,
    };
    if position == Some("first") || (position.is_none() && first_by_default) {
        fields.insert(0, field);
    } else {
        fields.push(field);
    }
}

fn trim_ascii(mut value: &[u8]) -> &[u8] {
    while matches!(value.first(), Some(b' ' | b'\t')) {
        value = &value[1..];
    }
    while matches!(value.last(), Some(b' ' | b'\t')) {
        value = &value[..value.len() - 1];
    }
    value
}
