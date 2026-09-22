use std::time::{SystemTime, UNIX_EPOCH};

use stravia_vendor_sdk::{
    AllowanceItem, AllowanceRequest, AllowanceResponse, ErrorKind, GuestHost, HttpRequest,
    PluginError, ProviderSnapshot, read_http_body,
};

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const ALLOWANCE_URL: &str = "https://grok.com/grok_api_v2.GrokBuildBilling/GetGrokCreditsConfig";

pub(crate) fn execute(
    host: &GuestHost,
    provider: ProviderSnapshot,
    _request: AllowanceRequest,
) -> Result<AllowanceResponse, PluginError> {
    let credential = access_token(&provider)?;
    let response = host.http_start(HttpRequest {
        method: "POST".into(),
        url: ALLOWANCE_URL.into(),
        headers: vec![
            ("authorization".into(), format!("Bearer {credential}")),
            ("accept".into(), "*/*".into()),
            ("content-type".into(), "application/grpc-web+proto".into()),
            ("origin".into(), "https://grok.com".into()),
            ("referer".into(), "https://grok.com/?_s=usage".into()),
            ("x-grpc-web".into(), "1".into()),
            ("x-user-agent".into(), "connect-es/2.1.1".into()),
            ("user-agent".into(), "Stravia".into()),
        ],
        body: vec![0, 0, 0, 0, 0],
    })?;
    let status = response.status()?;
    if !(200..300).contains(&status) {
        return Err(status_error(status));
    }
    let headers = response.headers()?;
    check_grpc_status(&headers)?;
    let body = read_http_body(&response, MAX_RESPONSE_BYTES)?;
    parse_xai(&body, current_unix_ms()?).map_err(|()| invalid_response())
}

fn access_token(provider: &ProviderSnapshot) -> Result<&str, PluginError> {
    ["access_token", "api_key", "apiKey", "token"]
        .into_iter()
        .find_map(|key| {
            provider
                .credentials
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .ok_or_else(|| error(ErrorKind::Auth, "provider credential is missing", None))
}

fn parse_xai(body: &[u8], now: i64) -> Result<AllowanceResponse, ()> {
    let payloads = grpc_web_payloads(body)?;
    let mut scan = ProtobufScan::default();
    for payload in payloads {
        scan_protobuf(payload, &[], 0, &mut scan)?;
    }
    let mut percentages = scan
        .fixed32
        .iter()
        .filter(|field| {
            matches!(field.path.as_slice(), [1] | [1, 1])
                && field.value.is_finite()
                && (0.0..=100.0).contains(&field.value)
        })
        .collect::<Vec<_>>();
    percentages.sort_by_key(|field| (field.path.len(), field.order));
    let mut resets = scan
        .varints
        .iter()
        .filter(|field| (1_700_000_000..=2_100_000_000).contains(&field.value))
        .map(|field| {
            (
                field.path.as_slice() == [1, 5, 1],
                field.value as i64 * 1000,
            )
        })
        .filter(|(_, reset)| *reset > now)
        .collect::<Vec<_>>();
    resets.sort_by_key(|(preferred, reset)| (!*preferred, *reset));
    let reset = resets.first().map(|(_, reset)| *reset);
    let has_period = scan.varints.iter().any(|field| {
        (field.path.len() >= 2 && field.path[0] == 1 && field.path[1] == 6)
            || (field.path.as_slice() == [1, 8, 1] && matches!(field.value, 1 | 2))
    });
    let percent = percentages
        .first()
        .map(|field| f64::from(field.value))
        .or_else(|| (scan.fixed32.is_empty() && reset.is_some() && has_period).then_some(0.0))
        .ok_or(())?;
    let mut allowance = AllowanceItem {
        key: "billing_cycle".into(),
        label: "Billing cycle".into(),
        kind: "quota_window".into(),
        used: None,
        remaining: None,
        limit: None,
        used_percent: Some(percent.to_string()),
        window_seconds: None,
        resets_at_unix_ms: reset,
        condition: None,
    };
    allowance.condition = if percent >= 100.0 {
        Some("exhausted".into())
    } else if 100.0 - percent < 20.0 {
        Some("tight".into())
    } else {
        Some("normal".into())
    };
    Ok(AllowanceResponse {
        allowances: vec![allowance],
        models: Vec::new(),
        plan_label: None,
    })
}

#[derive(Default)]
struct ProtobufScan {
    fixed32: Vec<Fixed32Field>,
    varints: Vec<VarintField>,
    order: usize,
}

struct Fixed32Field {
    path: Vec<u32>,
    value: f32,
    order: usize,
}

struct VarintField {
    path: Vec<u32>,
    value: u64,
}

fn grpc_web_payloads(body: &[u8]) -> Result<Vec<&[u8]>, ()> {
    if body.len() < 5 || body[0] & 0x7f != 0 {
        return looks_like_protobuf(body).then_some(vec![body]).ok_or(());
    }
    let (mut payloads, mut index, mut trailers) = (Vec::new(), 0, false);
    while index < body.len() {
        if index + 5 > body.len() {
            return Err(());
        }
        let flags = body[index];
        index += 1;
        if flags & 0x7f != 0 {
            return Err(());
        }
        let trailer = flags & 0x80 != 0;
        if trailers && !trailer {
            return Err(());
        }
        let length =
            u32::from_be_bytes(body[index..index + 4].try_into().map_err(|_| ())?) as usize;
        index += 4;
        let end = index
            .checked_add(length)
            .filter(|end| *end <= body.len())
            .ok_or(())?;
        if trailer {
            trailers = true;
            validate_grpc_trailer(&body[index..end])?;
        } else {
            payloads.push(&body[index..end]);
        }
        index = end;
    }
    (!payloads.is_empty()).then_some(payloads).ok_or(())
}

fn validate_grpc_trailer(body: &[u8]) -> Result<(), ()> {
    let text = std::str::from_utf8(body).map_err(|_| ())?;
    for line in text.lines().filter(|line| !line.is_empty()) {
        let (key, value) = line.split_once(':').ok_or(())?;
        if key.trim().eq_ignore_ascii_case("grpc-status")
            && value.trim().parse::<u32>().map_err(|_| ())? != 0
        {
            return Err(());
        }
    }
    Ok(())
}

fn looks_like_protobuf(body: &[u8]) -> bool {
    body.first().is_some_and(|byte| {
        let field = byte >> 3;
        let wire = byte & 7;
        field > 0 && matches!(wire, 0 | 1 | 2 | 5)
    })
}

fn scan_protobuf(
    body: &[u8],
    path: &[u32],
    depth: usize,
    scan: &mut ProtobufScan,
) -> Result<(), ()> {
    let mut index = 0;
    while index < body.len() {
        let key = read_varint(body, &mut index)?;
        let field = u32::try_from(key >> 3).map_err(|_| ())?;
        let wire = u8::try_from(key & 7).map_err(|_| ())?;
        if field == 0 || field > 0x1fff_ffff {
            return Err(());
        }
        let mut field_path = path.to_vec();
        field_path.push(field);
        match wire {
            0 => {
                let value = read_varint(body, &mut index)?;
                scan.varints.push(VarintField {
                    path: field_path,
                    value,
                });
            }
            1 => {
                index = index
                    .checked_add(8)
                    .filter(|end| *end <= body.len())
                    .ok_or(())?;
            }
            2 => {
                let length = usize::try_from(read_varint(body, &mut index)?).map_err(|_| ())?;
                let end = index
                    .checked_add(length)
                    .filter(|end| *end <= body.len())
                    .ok_or(())?;
                if depth >= 4 && length != 0 {
                    return Err(());
                }
                if depth < 4 && length != 0 {
                    scan_protobuf(&body[index..end], &field_path, depth + 1, scan)?;
                }
                index = end;
            }
            5 => {
                let end = index
                    .checked_add(4)
                    .filter(|end| *end <= body.len())
                    .ok_or(())?;
                let bytes: [u8; 4] = body[index..end].try_into().map_err(|_| ())?;
                scan.fixed32.push(Fixed32Field {
                    path: field_path,
                    value: f32::from_le_bytes(bytes),
                    order: scan.order,
                });
                scan.order += 1;
                index = end;
            }
            _ => return Err(()),
        }
    }
    Ok(())
}

fn read_varint(body: &[u8], index: &mut usize) -> Result<u64, ()> {
    let mut value = 0;
    for shift in (0..64).step_by(7) {
        let byte = *body.get(*index).ok_or(())?;
        *index += 1;
        if shift == 63 && byte & 0x7e != 0 {
            return Err(());
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(())
}

fn check_grpc_status(headers: &[(String, String)]) -> Result<(), PluginError> {
    let Some(status) = headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("grpc-status"))
        .and_then(|(_, value)| value.parse::<u16>().ok())
    else {
        return Ok(());
    };
    if status == 0 {
        return Ok(());
    }
    Err(match status {
        16 => error(ErrorKind::Auth, "upstream authentication failed", None),
        4 => error(
            ErrorKind::upstream(
                Some(stravia_runtime_contract::protocol::ir::AiErrorKind::Timeout),
                None,
            ),
            "upstream allowance request timed out",
            None,
        ),
        13 => error(
            ErrorKind::upstream(
                Some(stravia_runtime_contract::protocol::ir::AiErrorKind::ServerError),
                None,
            ),
            "upstream allowance request failed",
            None,
        ),
        14 => error(
            ErrorKind::upstream(
                Some(stravia_runtime_contract::protocol::ir::AiErrorKind::ServiceUnavailable),
                None,
            ),
            "upstream allowance request failed",
            None,
        ),
        _ => error(
            ErrorKind::upstream_unknown(),
            "upstream allowance request failed",
            None,
        ),
    })
}

fn current_unix_ms() -> Result<i64, PluginError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| error(ErrorKind::Trapped, "system clock precedes Unix epoch", None))?;
    i64::try_from(duration.as_millis()).map_err(|_| {
        error(
            ErrorKind::Trapped,
            "system clock is outside supported range",
            None,
        )
    })
}

fn status_error(status: u16) -> PluginError {
    let kind = match status {
        401 | 403 => ErrorKind::Auth,
        _ => ErrorKind::upstream_unknown(),
    };
    error(kind, "upstream allowance request failed", Some(status))
}

fn invalid_response() -> PluginError {
    error(
        ErrorKind::upstream_unknown(),
        "upstream allowance response has an unsupported shape",
        None,
    )
}

fn error(kind: ErrorKind, message: &str, upstream_status: Option<u16>) -> PluginError {
    PluginError {
        kind,
        message: message.into(),
        upstream_status,
    }
}
