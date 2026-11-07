use bytes::Bytes;
use rama_http_types::{
    body::Full,
    header::{HeaderName, HeaderValue},
    Method, Request, Uri, Version,
};

use super::{Field, RequestPlan, TransportError};

/// Convert the ordered field lines without passing through an unordered iterator.
pub(super) fn request(
    plan: RequestPlan,
    version: Version,
    forward_proxy: bool,
) -> Result<Request<Full<Bytes>>, TransportError> {
    let method = Method::from_bytes(plan.method.as_bytes())
        .map_err(|e| TransportError::Protocol(e.to_string()))?;
    let absolute: Uri = plan.uri.parse()
        .map_err(|e: rama_http_types::uri::InvalidUri| TransportError::Protocol(e.to_string()))?;
    let uri = if version == Version::HTTP_11 && !forward_proxy {
        absolute.path_and_query().map_or("/", |p| p.as_str()).parse()
            .map_err(|e: rama_http_types::uri::InvalidUri| TransportError::Protocol(e.to_string()))?
    } else {
        absolute
    };
    let mut request = Request::builder().method(method).uri(uri).version(version)
        .body(Full::new(plan.body))
        .map_err(|e| TransportError::Protocol(e.to_string()))?;
    for Field { name, bytes } in plan.fields {
        if name.eq_ignore_ascii_case("proxy-authorization") || name.eq_ignore_ascii_case("proxy-connection") {
            return Err(TransportError::Protocol("proxy headers cannot be supplied by the request".into()));
        }
        if version == Version::HTTP_2 && ["host", "connection", "keep-alive", "transfer-encoding", "upgrade"]
            .iter().any(|reserved| name.eq_ignore_ascii_case(reserved)) {
            continue;
        }
        let name = if version == Version::HTTP_2 { name.to_ascii_lowercase() } else { name };
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|e| TransportError::Protocol(e.to_string()))?;
        let value = HeaderValue::from_bytes(&bytes)
            .map_err(|e| TransportError::Protocol(e.to_string()))?;
        request.headers_mut().append(name, value);
    }
    Ok(request)
}
