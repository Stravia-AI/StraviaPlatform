use super::*;
use stravia_vendor_common::connection_check;

/// A real models request at the configured endpoint. The endpoint is verified;
/// the models surface does not prove that the bearer key itself was accepted,
/// so credentials are not claimed.
pub(super) fn check(
    host: &GuestHost,
    provider: &ProviderSnapshot,
) -> Result<ConnectionCheckResponse, PluginError> {
    let response = host.http_start(
        net::Request {
            method: "GET".into(),
            url: format!("{}/models", provider.base_url.trim_end_matches('/')),
            headers: headers(key(provider)?),
            body: Vec::new(),
        },
        &bun_options(),
    )?;
    let value = connection_check::read_json(&response)?;
    if !value.get("data").is_some_and(Value::is_array) {
        return Err(connection_check::invalid_response(
            "Muse model list has no data array",
        ));
    }
    Ok(ConnectionCheckResponse {
        verified: ConnectionCheckVerification::Endpoint,
    })
}
