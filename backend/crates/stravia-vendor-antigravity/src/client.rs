use serde_json::{Value, json};
use stravia_vendor_common::common::{plugin_error, upstream_error};
use stravia_vendor_sdk::{ErrorKind, GuestHost, PluginError, ProviderSnapshot, read_http_body};

pub(crate) const MAX_JSON_BODY: usize = 4 * 1024 * 1024;

pub(crate) fn headers(provider: &ProviderSnapshot) -> Result<Vec<(String, String)>, PluginError> {
    let token = credential(provider, "access_token")
        .ok_or_else(|| plugin_error(ErrorKind::Auth, "Antigravity access token is missing"))?;
    if token.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(plugin_error(
            ErrorKind::Auth,
            "Antigravity access token is malformed",
        ));
    }
    // 固定客户端身份，不转发下游请求头。
    Ok(vec![
        ("authorization".into(), format!("Bearer {token}")),
        ("content-type".into(), "application/json".into()),
        ("user-agent".into(), crate::CLI_USER_AGENT.into()),
    ])
}

pub(crate) fn root(provider: &ProviderSnapshot) -> &str {
    if provider.base_url.trim().is_empty() {
        crate::BASE_URL
    } else {
        provider.base_url.trim_end_matches('/')
    }
}

pub(crate) fn endpoint(provider: &ProviderSnapshot, rpc: &str) -> String {
    format!("{}/v1internal:{rpc}", root(provider))
}

pub(crate) fn post_json(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    rpc: &str,
    body: &Value,
) -> Result<Value, PluginError> {
    request_json(
        host,
        "POST",
        endpoint(provider, rpc),
        headers(provider)?,
        serde_json::to_vec(body).map_err(|_| {
            plugin_error(
                ErrorKind::Invalid,
                "Antigravity request could not be encoded",
            )
        })?,
    )
}

pub(crate) fn request_json(
    host: &GuestHost,
    method: &str,
    url: String,
    mut headers: Vec<(String, String)>,
    body: Vec<u8>,
) -> Result<Value, PluginError> {
    if method != "POST" {
        headers.retain(|(name, _)| !name.eq_ignore_ascii_case("content-type"));
    }
    let response = host.http_start(stravia_vendor_sdk::wit::types::HttpRequest {
        method: method.into(),
        url,
        headers,
        body,
    })?;
    let status = response.status()?;
    let headers = response.headers()?;
    let body = read_http_body(&response, MAX_JSON_BODY)?;
    if !(200..300).contains(&status) {
        return Err(upstream_error(status, &headers, &body));
    }
    serde_json::from_slice(&body).map_err(|_| {
        plugin_error(
            ErrorKind::upstream_unknown(),
            "Antigravity response is not valid JSON",
        )
    })
}

pub(crate) fn credential<'a>(provider: &'a ProviderSnapshot, key: &str) -> Option<&'a str> {
    provider
        .credentials
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub(crate) fn project_value(value: &Value) -> Option<&str> {
    value
        .get("cloudaicompanionProject")
        .and_then(|project| {
            project
                .as_str()
                .or_else(|| project.get("id").and_then(Value::as_str))
        })
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub(crate) fn project(
    host: &GuestHost,
    provider: &ProviderSnapshot,
) -> Result<String, PluginError> {
    if let Some(project) = credential(provider, "project_id") {
        return Ok(project.into());
    }
    if let Some(project) = crate::auth::bound_project(host, provider)? {
        return Ok(project);
    }
    Err(plugin_error(
        ErrorKind::Auth,
        "Antigravity project is missing; complete account onboarding",
    ))
}

pub(crate) fn discover_project(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    session: &mut crate::auth::Session,
) -> Result<Option<String>, PluginError> {
    if let Some(operation) = session.operation.as_deref() {
        crate::auth::validate_operation(operation)?;
        let payload = request_json(
            host,
            "GET",
            format!("{}/v1internal/{operation}", root(provider)),
            headers(provider)?,
            Vec::new(),
        )?;
        return operation_result(host, session, &payload);
    }
    let discovery = post_json(
        host,
        provider,
        "loadCodeAssist",
        &json!({"metadata":{"ideType":"ANTIGRAVITY"}}),
    )?;
    if discovery
        .get("projectValidationError")
        .and_then(|error| error.get("code"))
        .and_then(Value::as_i64)
        .is_some_and(|code| code != 0)
    {
        return Err(plugin_error(
            ErrorKind::Auth,
            "Antigravity project validation failed",
        ));
    }
    if let Some(project) = project_value(&discovery) {
        if let Some(tier) = discovery
            .get("currentTier")
            .or_else(|| discovery.get("paidTier"))
        {
            session.values.insert("tier".into(), tier.clone());
        }
        return Ok(Some(project.into()));
    }
    let tier = crate::auth::default_tier(&discovery)?;
    session.values.insert("tier".into(), tier.clone());
    crate::auth::save(host, session)?;
    let payload = post_json(
        host,
        provider,
        "onboardUser",
        &json!({"tierId":tier.get("id"),"metadata":{"ideType":"ANTIGRAVITY"}}),
    )?;
    operation_result(host, session, &payload)
}

fn operation_result(
    host: &GuestHost,
    session: &mut crate::auth::Session,
    payload: &Value,
) -> Result<Option<String>, PluginError> {
    if payload.get("done").and_then(Value::as_bool) == Some(true) {
        if let Some(error) = payload.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Antigravity onboarding operation failed");
            return Err(plugin_error(ErrorKind::upstream_unknown(), message));
        }
        let project = payload
            .get("response")
            .and_then(project_value)
            .ok_or_else(|| {
                plugin_error(
                    ErrorKind::upstream_unknown(),
                    "Antigravity onboarding result omitted project",
                )
            })?;
        session.operation = None;
        return Ok(Some(project.into()));
    }
    if payload.get("done").is_some_and(|value| !value.is_boolean()) {
        return Err(plugin_error(
            ErrorKind::upstream_unknown(),
            "Antigravity onboarding completion flag is malformed",
        ));
    }
    let name = payload.get("name").and_then(Value::as_str).ok_or_else(|| {
        plugin_error(
            ErrorKind::upstream_unknown(),
            "Antigravity onboarding operation name is missing",
        )
    })?;
    crate::auth::validate_operation(name)?;
    if session
        .operation
        .as_deref()
        .is_some_and(|previous| previous != name)
    {
        return Err(plugin_error(
            ErrorKind::upstream_unknown(),
            "Antigravity onboarding operation identity changed",
        ));
    }
    session.operation = Some(name.into());
    crate::auth::save(host, session)?;
    Ok(None)
}
