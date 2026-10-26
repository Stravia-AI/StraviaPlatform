use super::*;
use stravia_vendor_common::connection_check;
use stravia_vendor_sdk::{ConnectionCheckResponse, ConnectionCheckVerification};

/// A real models request at the configured endpoint. Anonymous access is a
/// supported mode and key rejection is not distinguishable from free-tier
/// gating, so only the endpoint is claimed as verified.
pub(super) fn check(
    host: &GuestHost,
    provider: &ProviderSnapshot,
) -> Result<ConnectionCheckResponse, PluginError> {
    let url = format!("{}/models", provider.base_url.trim_end_matches('/'));
    let mut headers = vec![("accept".to_owned(), "application/json".to_owned())];
    native_identity::apply(provider, &mut headers)?;
    apply_auth(provider, &mut headers);
    let value = connection_check::json(
        host,
        HttpRequest {
            method: "GET".to_owned(),
            url,
            headers,
            body: Vec::new(),
        },
    )?;
    if !value.get("data").is_some_and(Value::is_array) {
        return Err(connection_check::invalid_response(
            "model list has no data array",
        ));
    }
    Ok(ConnectionCheckResponse {
        verified: ConnectionCheckVerification::Endpoint,
    })
}
