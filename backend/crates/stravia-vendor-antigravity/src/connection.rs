use serde_json::{Value, json};
use stravia_vendor_common::common::plugin_error;
use stravia_vendor_common::connection_check;
use stravia_vendor_sdk::{
    ConnectionCheckResponse, ConnectionCheckVerification, ErrorKind, GuestHost, HttpRequest,
    PluginError, ProviderSnapshot,
};

use crate::client;

/// Read the existing account project's model inventory. This must never start
/// onboarding, refresh credentials, or persist account/session state.
pub(crate) fn execute(
    host: &GuestHost,
    provider: &ProviderSnapshot,
) -> Result<ConnectionCheckResponse, PluginError> {
    let project = existing_project(host, provider)?;
    let body = serde_json::to_vec(&json!({"project": project})).map_err(|_| {
        plugin_error(
            ErrorKind::Invalid,
            "Antigravity request could not be encoded",
        )
    })?;
    let payload = connection_check::json(
        host,
        HttpRequest {
            method: "POST".into(),
            url: client::endpoint(provider, "fetchAvailableModels"),
            headers: client::headers(provider)?,
            body,
        },
    )?;
    payload
        .get("models")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            connection_check::invalid_response(
                "Antigravity model inventory response omitted the models object",
            )
        })?;
    Ok(ConnectionCheckResponse {
        verified: ConnectionCheckVerification::Credentials,
    })
}

/// The check only observes a project already bound by a completed OAuth flow.
/// Discovery/onboarding makes upstream mutations, so it is deliberately not a
/// fallback here.
fn existing_project(host: &GuestHost, provider: &ProviderSnapshot) -> Result<String, PluginError> {
    if let Some(project) = client::credential(provider, "project_id") {
        return Ok(project.into());
    }
    if let Some(session) = crate::auth::optional_session(host, provider)?
        && let Some(project) = session
            .values
            .get("project_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|project| !project.is_empty())
    {
        return Ok(project.into());
    }
    Err(plugin_error(
        ErrorKind::Invalid,
        "Antigravity project is missing; complete account onboarding before testing",
    ))
}
