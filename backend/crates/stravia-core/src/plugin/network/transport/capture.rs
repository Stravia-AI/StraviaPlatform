//! Snapshot the request head as Rama's client serializers normalize it.

use rama_http_types::{
    Method, Request, Version,
    header::{self, HeaderMap, HeaderValue},
    proto::h2::{PseudoHeader, PseudoHeaderOrder, ext::Protocol, frame::Pseudo},
};

use super::{Field, RequestHead, body::RequestBody};

pub(super) fn request_head(
    request: &Request<RequestBody>,
    pseudo_order: Option<&PseudoHeaderOrder>,
    is_upgrade: bool,
) -> RequestHead {
    let is_h2 = request.version() == Version::HTTP_2;
    let len = request
        .body()
        .actual_size_hint()
        .exact()
        .expect("Full has an exact size");
    let mut headers = request.headers().clone();
    let mut fields = Vec::new();

    if is_h2 {
        strip_h2_connection_headers(&mut headers);
        if !request.body().omit_length()
            && (len != 0
                || !matches!(
                    *request.method(),
                    Method::GET | Method::HEAD | Method::DELETE | Method::CONNECT
                ))
            && !headers.contains_key(header::CONTENT_LENGTH)
        {
            headers.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
        }
        let protocol = is_upgrade.then(|| Protocol::from_static("websocket"));
        let pseudo = Pseudo::request(request.method().clone(), request.uri(), protocol);
        append_pseudo(&mut fields, pseudo, pseudo_order);
    } else if len == 0 && !headers.contains_key(header::TRANSFER_ENCODING) {
        headers.remove(header::TRANSFER_ENCODING);
    } else {
        let valid_length = content_length(&headers).is_some();
        if request.version() == Version::HTTP_10 {
            headers.remove(header::TRANSFER_ENCODING);
            if !valid_length {
                headers.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
            }
        } else if let header::Entry::Occupied(mut te) = headers.entry(header::TRANSFER_ENCODING) {
            let chunked = te.iter().next_back().is_some_and(|line| {
                line.to_str()
                    .ok()
                    .and_then(|s| s.rsplit(',').next())
                    .is_some_and(|s| s.trim().eq_ignore_ascii_case("chunked"))
            });
            if !chunked {
                if let Some(line) = te.iter_mut().next_back() {
                    let mut value = line.as_bytes().to_vec();
                    value.extend_from_slice(b", chunked");
                    if let Ok(value) = HeaderValue::from_bytes(&value) {
                        *line = value;
                    } else {
                        te.append(HeaderValue::from_static("chunked"));
                    }
                } else {
                    te.insert(HeaderValue::from_static("chunked"));
                }
            }
            if valid_length {
                headers.remove(header::CONTENT_LENGTH);
            }
        } else if !valid_length {
            if cfg!(debug_assertions) {
                match headers.entry(header::CONTENT_LENGTH) {
                    header::Entry::Occupied(mut entry) => {
                        entry.insert(HeaderValue::from(len));
                    }
                    header::Entry::Vacant(entry) => {
                        entry.insert(HeaderValue::from(len));
                    }
                }
            } else {
                headers.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
            }
        }
    }

    fields.extend(headers.ordered_iter().map(|(name, value)| Field {
        name: if is_h2 {
            name.as_str().to_ascii_lowercase()
        } else {
            name.to_string()
        },
        bytes: value.as_bytes().to_vec(),
    }));
    RequestHead { fields }
}

fn content_length(headers: &HeaderMap) -> Option<u64> {
    let mut length = None;
    for value in headers.get_all(header::CONTENT_LENGTH).iter() {
        for part in value.to_str().ok()?.split(',') {
            let digits = part.trim().as_bytes();
            if digits.is_empty() {
                return None;
            }
            let mut n = 0u64;
            for &digit in digits {
                if !digit.is_ascii_digit() {
                    return None;
                }
                n = n.checked_mul(10)?.checked_add(u64::from(digit - b'0'))?;
            }
            if length.is_some_and(|previous| previous != n) {
                return None;
            }
            length = Some(n);
        }
    }
    length
}

fn strip_h2_connection_headers(headers: &mut HeaderMap) {
    for name in [
        header::KEEP_ALIVE,
        header::PROXY_CONNECTION,
        header::TRANSFER_ENCODING,
        header::UPGRADE,
    ] {
        headers.remove(name);
    }
    if headers
        .get(header::TE)
        .is_some_and(|value| value != "trailers")
    {
        headers.remove(header::TE);
    }
    if let Some(connection) = headers.remove(header::CONNECTION) {
        for name in connection.as_bytes().split(|byte| *byte == b',') {
            if let Ok(name) = std::str::from_utf8(name.trim_ascii()) {
                headers.remove(name);
            }
        }
    }
}

fn append_pseudo(fields: &mut Vec<Field>, pseudo: Pseudo, order: Option<&PseudoHeaderOrder>) {
    let mut values = [
        pseudo.method.map(|v| v.to_string().into_bytes()),
        pseudo.scheme.map(|v| v.as_bytes().to_vec()),
        pseudo.authority.map(|v| v.as_bytes().to_vec()),
        pseudo.path.map(|v| v.as_bytes().to_vec()),
        pseudo.protocol.map(|v| v.as_str().as_bytes().to_vec()),
        pseudo.status.map(|v| v.to_string().into_bytes()),
    ];
    let names = [
        ":method",
        ":scheme",
        ":authority",
        ":path",
        ":protocol",
        ":status",
    ];
    let mut order = order.map(PseudoHeaderOrder::iter).into_iter().flatten();
    loop {
        // Rama consumes one configured entry per emitted header. If it is
        // absent, the same iteration falls back to the first remaining field.
        let requested = order.next().map(|header| match header {
            PseudoHeader::Method => 0,
            PseudoHeader::Scheme => 1,
            PseudoHeader::Authority => 2,
            PseudoHeader::Path => 3,
            PseudoHeader::Protocol => 4,
            PseudoHeader::Status => 5,
        });
        let index = requested
            .filter(|&i| values[i].is_some())
            .or_else(|| values.iter().position(Option::is_some));
        let Some(index) = index else { break };
        fields.push(Field {
            name: names[index].into(),
            bytes: values[index].take().unwrap(),
        });
    }
}
