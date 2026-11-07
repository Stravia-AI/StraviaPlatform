use rama_http_types::{
    Method, Request, Version,
    header::{HeaderName, HeaderValue},
};
use rama_net::uri::Uri;

use super::{Field, RequestPlan, TransportError, body::RequestBody};

/// Convert the ordered field lines without passing through an unordered iterator.
pub(super) fn request(
    plan: RequestPlan,
    version: Version,
    omit_length: bool,
) -> Result<Request<RequestBody>, TransportError> {
    let method = Method::from_bytes(plan.method.as_bytes())
        .map_err(|e| TransportError::Protocol(e.to_string()))?;
    let uri: Uri = plan
        .uri
        .parse()
        .map_err(|e: rama_net::uri::ParseError| TransportError::Protocol(e.to_string()))?;
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .version(version)
        .body(RequestBody::new(
            plan.body,
            version == Version::HTTP_2 && omit_length,
        ))
        .map_err(|e| TransportError::Protocol(e.to_string()))?;
    if version == Version::HTTP_11
        && plan
            .fields
            .iter()
            .any(|field| field.name.eq_ignore_ascii_case("transfer-encoding"))
    {
        request.body_mut().require_empty_chunk();
    }
    for Field { name, bytes } in plan.fields {
        if name.eq_ignore_ascii_case("proxy-authorization")
            || name.eq_ignore_ascii_case("proxy-connection")
        {
            return Err(TransportError::Protocol(
                "proxy headers cannot be supplied by the request".into(),
            ));
        }
        if version == Version::HTTP_2 && omit_length && name.eq_ignore_ascii_case("content-length")
        {
            continue;
        }
        if version == Version::HTTP_2
            && [
                "host",
                "connection",
                "keep-alive",
                "transfer-encoding",
                "upgrade",
            ]
            .iter()
            .any(|reserved| name.eq_ignore_ascii_case(reserved))
        {
            continue;
        }
        let name = if version == Version::HTTP_2 {
            name.to_ascii_lowercase()
        } else {
            name
        };
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|e| TransportError::Protocol(e.to_string()))?;
        let value =
            HeaderValue::from_bytes(&bytes).map_err(|e| TransportError::Protocol(e.to_string()))?;
        request.headers_mut().append(name, value);
    }
    Ok(request)
}
