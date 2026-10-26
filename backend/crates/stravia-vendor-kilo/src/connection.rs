use super::*;
use stravia_vendor_common::connection_check;

pub(super) fn check(
    host: &GuestHost,
    provider: &ProviderSnapshot,
) -> Result<ConnectionCheckResponse, PluginError> {
    token(provider)?;
    let value = connection_check::json(
        host,
        HttpRequest {
            method: "GET".into(),
            url: format!("{ORIGIN}/api/profile/balance"),
            headers: headers(provider)?,
            body: Vec::new(),
        },
    )?;
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
