use serde_json::Value;
use stravia_vendor_sdk::{
    AllowanceAmount, AllowanceItem, AllowanceRequest, AllowanceResponse, ErrorKind, GuestHost,
    PluginError, ProviderSnapshot, read_http_body,
};

use crate::codec::devin_connect::proto::{ProtoField, parse_fields};
use crate::codec::devin_connect::{
    DevinClientPlatform, encode_client_metadata_request_with_platform,
};

const GET_USER_STATUS_PATH: &str = "/exa.seat_management_pb.SeatManagementService/GetUserStatus";
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

pub(crate) fn execute(
    host: &GuestHost,
    provider: ProviderSnapshot,
    _request: AllowanceRequest,
) -> Result<AllowanceResponse, PluginError> {
    let credential = session_token(&provider)?;
    let platform = client_platform(&provider)?;
    let response = host.http_start(stravia_vendor_sdk::wit::types::HttpRequest {
        method: "POST".into(),
        url: endpoint(&provider, GET_USER_STATUS_PATH),
        headers: vec![
            (
                "authorization".into(),
                format!("Basic {credential}-{credential}"),
            ),
            ("accept".into(), "*/*".into()),
            ("content-type".into(), "application/proto".into()),
            ("connect-protocol-version".into(), "1".into()),
        ],
        body: encode_client_metadata_request_with_platform(credential, false, platform),
    })?;
    let status = response.status()?;
    if !(200..300).contains(&status) {
        return Err(PluginError {
            kind: match status {
                401 | 403 => ErrorKind::Auth,
                _ => ErrorKind::upstream_unknown(),
            },
            message: format!("Devin GetUserStatus returned HTTP {status}"),
            upstream_status: Some(status),
        });
    }
    let body = read_http_body(&response, MAX_RESPONSE_BYTES)?;
    parse_response(&body).map_err(|()| {
        error(
            ErrorKind::upstream_unknown(),
            "Devin GetUserStatus returned invalid protobuf",
        )
    })
}

fn parse_response(body: &[u8]) -> Result<AllowanceResponse, ()> {
    fn message<'a>(fields: &'a [ProtoField<'a>], number: u32) -> Option<Vec<ProtoField<'a>>> {
        let field = fields
            .iter()
            .find(|field| field.number == number && field.wire_type == 2)?;
        parse_fields(field.bytes).ok()
    }
    fn varint(fields: &[ProtoField<'_>], number: u32) -> Option<u64> {
        fields
            .iter()
            .find(|field| field.number == number && field.wire_type == 0)
            .map(|field| field.scalar)
    }
    fn string(fields: &[ProtoField<'_>], number: u32) -> Option<String> {
        let field = fields
            .iter()
            .find(|field| field.number == number && field.wire_type == 2)?;
        let value = std::str::from_utf8(field.bytes).ok()?.trim();
        (!value.is_empty()).then(|| value.to_owned())
    }

    let top = parse_fields(body).map_err(|_| ())?;
    let mut plan_label = message(&top, 2).and_then(|inner| string(&inner, 2));
    let user = message(&top, 1).unwrap_or_else(|| top.clone());
    let used_prompt = varint(&user, 28);
    let used_flow = varint(&user, 29);
    let mut allowances = Vec::new();
    if let Some(status) = message(&user, 13) {
        for (key, percent_field, reset_field, seconds) in
            [("daily", 14, 17, 86_400), ("weekly", 15, 18, 604_800)]
        {
            if let Some(remaining) = varint(&status, percent_field) {
                let mut allowance = item(key, window_label(key), "quota_window");
                set_percent(&mut allowance, Some((100.0 - remaining as f64).max(0.0)));
                allowance.window_seconds = Some(seconds);
                allowance.resets_at_unix_ms = varint(&status, reset_field)
                    .and_then(|value| value.checked_mul(1000))
                    .and_then(|value| i64::try_from(value).ok());
                allowances.push(allowance);
            }
        }
        if let Some(info) = message(&status, 1) {
            if let Some(name) = string(&info, 2) {
                plan_label = Some(name);
            } else if let Some(tier) = varint(&info, 1) {
                plan_label = Some(tier_label(tier).into());
            }
            let finite = |value: Option<u64>| value.filter(|value| *value != u64::MAX);
            if let Some(allowance) = credit_window(
                "prompt_credits",
                "Prompt credits",
                used_prompt,
                finite(varint(&info, 12)),
            ) {
                allowances.push(allowance);
            }
            if let Some(allowance) = credit_window(
                "flow_credits",
                "Flow credits",
                used_flow,
                finite(varint(&info, 13)),
            ) {
                allowances.push(allowance);
            }
        }
        if let Some(balance) = varint(&status, 16) {
            let mut allowance = item("balance_usd", "Balance", "balance");
            allowance.remaining = Some(amount(balance as f64 / 1e6, "currency", Some("USD")));
            allowances.push(allowance);
        }
    } else {
        for (key, label, used) in [
            ("prompt_credits", "Prompt credits", used_prompt),
            ("flow_credits", "Flow credits", used_flow),
        ] {
            if let Some(used) = used {
                let mut allowance = item(key, label, "quota_window");
                allowance.used = Some(amount(used as f64, "credits", None));
                allowances.push(allowance);
            }
        }
    }
    if plan_label.is_none() && allowances.is_empty() {
        return Err(());
    }
    for allowance in &mut allowances {
        allowance.condition = condition(allowance);
    }
    Ok(AllowanceResponse {
        allowances,
        models: Vec::new(),
        plan_label,
    })
}

fn credit_window(
    key: &str,
    label: &str,
    used: Option<u64>,
    limit: Option<u64>,
) -> Option<AllowanceItem> {
    if used.is_none() && limit.is_none() {
        return None;
    }
    let mut allowance = item(key, label, "quota_window");
    allowance.used = used.map(|value| amount(value as f64, "credits", None));
    allowance.limit = limit.map(|value| amount(value as f64, "credits", None));
    if let (Some(used), Some(limit)) = (used, limit) {
        allowance.remaining = Some(amount(limit.saturating_sub(used) as f64, "credits", None));
        if limit > 0 {
            set_percent(&mut allowance, Some(used as f64 / limit as f64 * 100.0));
        }
    }
    Some(allowance)
}

fn item(key: impl Into<String>, label: impl Into<String>, kind: &str) -> AllowanceItem {
    AllowanceItem {
        key: key.into(),
        label: label.into(),
        kind: kind.into(),
        used: None,
        remaining: None,
        limit: None,
        used_percent: None,
        window_seconds: None,
        resets_at_unix_ms: None,
        condition: None,
    }
}

fn amount(value: f64, unit: &str, currency: Option<&str>) -> AllowanceAmount {
    AllowanceAmount {
        value: value.to_string(),
        unit: unit.into(),
        currency: currency.map(str::to_owned),
    }
}

fn set_percent(item: &mut AllowanceItem, value: Option<f64>) {
    item.used_percent = value.map(|value| value.to_string());
}

fn condition(item: &AllowanceItem) -> Option<String> {
    let used = item
        .used_percent
        .as_deref()
        .and_then(|value| value.parse::<f64>().ok());
    let remaining = item
        .remaining
        .as_ref()
        .and_then(|value| value.value.parse::<f64>().ok());
    if used.is_some_and(|value| value.is_finite() && value >= 100.0)
        || remaining.is_some_and(|value| value.is_finite() && value <= 0.0)
    {
        return Some("exhausted".into());
    }
    let remaining_percent = used
        .filter(|value| value.is_finite())
        .map(|value| 100.0 - value)
        .or_else(|| {
            let limit = item.limit.as_ref()?.value.parse::<f64>().ok()?;
            let remaining = remaining?;
            (limit.is_finite() && remaining.is_finite() && limit > 0.0)
                .then_some(remaining / limit * 100.0)
        })?;
    Some(
        if remaining_percent < 20.0 {
            "tight"
        } else {
            "normal"
        }
        .into(),
    )
}

fn window_label(key: &str) -> &'static str {
    match key {
        "daily" => "Daily window",
        "weekly" => "Weekly window",
        _ => "Quota window",
    }
}

fn tier_label(tier: u64) -> &'static str {
    match tier {
        1 => "Teams",
        2 => "Pro",
        3 => "Enterprise (SaaS)",
        4 => "Hybrid",
        5 => "Enterprise (Self-Hosted)",
        7 => "Teams Ultimate",
        8 => "Pro Ultimate",
        9 => "Trial",
        10 => "Enterprise (Self-Serve)",
        11 => "Enterprise (SaaS Pooled)",
        12 => "Devin Enterprise",
        14 => "Devin Teams",
        15 => "Devin Teams V2",
        16 => "Devin Pro",
        17 => "Devin Max",
        18 => "Max",
        19 => "Devin Free",
        20 => "Devin Trial",
        _ => "Unknown",
    }
}

fn client_platform(provider: &ProviderSnapshot) -> Result<DevinClientPlatform, PluginError> {
    match provider
        .operation_metadata
        .get("host_platform")
        .and_then(Value::as_str)
    {
        Some("windows") => Ok(DevinClientPlatform::Windows),
        Some("macos") => Ok(DevinClientPlatform::Mac),
        Some("linux") => Ok(DevinClientPlatform::Linux),
        Some(_) => Err(error(
            ErrorKind::Invalid,
            "operation metadata contains an unsupported host_platform",
        )),
        None => Err(error(
            ErrorKind::Invalid,
            "operation metadata is missing host_platform",
        )),
    }
}

fn session_token(provider: &ProviderSnapshot) -> Result<&str, PluginError> {
    [
        "session_token",
        "sessionToken",
        "access_token",
        "accessToken",
        "api_key",
        "apiKey",
        "windsurf_api_key",
        "windsurfApiKey",
        "token",
    ]
    .into_iter()
    .find_map(|key| provider.credentials.get(key).and_then(Value::as_str))
    .map(str::trim)
    .filter(|value| !value.is_empty())
    .ok_or_else(|| error(ErrorKind::Auth, "Devin session token is missing"))
}

fn endpoint(provider: &ProviderSnapshot, path: &str) -> String {
    let base = provider.base_url.trim();
    let base = if base.is_empty() {
        "https://server.codeium.com"
    } else {
        base
    };
    format!("{}{path}", base.trim_end_matches('/'))
}

fn error(kind: ErrorKind, message: impl Into<String>) -> PluginError {
    PluginError {
        kind,
        message: message.into(),
        upstream_status: None,
    }
}
