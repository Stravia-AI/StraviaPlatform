use super::*;
use stravia_vendor_common::connection_check;

/// A real models request at the configured endpoint. The endpoint is verified;
/// the models surface does not prove that the bearer key itself was accepted,
/// so credentials are not claimed.
pub(super) fn check(
    host: &GuestHost,
    provider: &ProviderSnapshot,
) -> Result<ConnectionCheckResponse, PluginError> {
    let value = connection_check::json(
        host,
        HttpRequest {
            method: "GET".into(),
            url: format!("{}/models", provider.base_url.trim_end_matches('/')),
            headers: headers(key(provider)?),
            body: Vec::new(),
        },
    )?;
    if !value.get("data").is_some_and(Value::is_array) {
        return Err(connection_check::invalid_response(
            "Muse model list has no data array",
        ));
    }
    Ok(ConnectionCheckResponse {
        verified: ConnectionCheckVerification::Endpoint,
    })
}
