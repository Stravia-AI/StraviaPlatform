use super::*;
use stravia_vendor_common::connection_check;

pub(super) fn check(
    host: &GuestHost,
    provider: &ProviderSnapshot,
) -> Result<ConnectionCheckResponse, PluginError> {
    token(provider)?;
    let response = host.http_start(
        stravia_vendor_sdk::net::Request {
            method: "GET".into(),
            url: format!("{ORIGIN}/api/profile/balance"),
            headers: crate::net_fields(headers(provider)?),
            body: Vec::new(),
        },
        &crate::net_options(),
    )?;
    let value = connection_check::read_json(&response)?;
    if value
        .get("balance")
        .is_none_or(|balance| !balance.is_number())
    {
        return Err(connection_check::invalid_response(
            "Kilo balance response missing numeric balance",
        ));
    }
    Ok(ConnectionCheckResponse {
        verified: ConnectionCheckVerification::Credentials,
    })
}
