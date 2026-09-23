use std::collections::BTreeSet;

use serde_json::Value;
use stravia_vendor_sdk::{
    AiErrorKind, AiRequest, AiResponse, AiStreamDelta, CANONICAL_FORMAT_VERSION, Capability,
    ChannelDescriptor, ConfigField, ConfigFieldKind, ConfigValidationResponse, DataCompatibility,
    EnumOption, ErrorKind, GuestHost, HttpRequest, NetworkDeclaration, Operation, OperationInput,
    OperationOutput, PluginError, ProviderDescriptor, ProviderSnapshot, ValidationIssue,
    VendorDescriptor, VendorGuest, VendorKind, read_http_body,
};

struct LifecycleContractVendor;

impl VendorGuest for LifecycleContractVendor {
    fn descriptor() -> VendorDescriptor {
        let capabilities = BTreeSet::from([Capability::Infer, Capability::ConfigValidation]);
        VendorDescriptor {
            vendor_id: build_value("STRAVIA_FIXTURE_VENDOR_ID", "fixture.lifecycle").into(),
            version: semver::Version::parse(build_value("STRAVIA_FIXTURE_VERSION", "1.0.0"))
                .expect("fixture version must be semver"),
            display_name: "Lifecycle Contract Fixture".into(),
            description: Some(
                "Exercises generation pinning, private state, and network authorization".into(),
            ),
            authors: vec![
                build_value("STRAVIA_FIXTURE_AUTHOR", "Unverified fixture author").into(),
            ],
            canonical_format_version: option_env!("STRAVIA_FIXTURE_CANONICAL_FORMAT")
                .map(|value| {
                    value
                        .parse()
                        .expect("fixture canonical format must be an integer")
                })
                .unwrap_or(CANONICAL_FORMAT_VERSION),
            kind: match build_value("STRAVIA_FIXTURE_KIND", "dedicated") {
                "dedicated" => VendorKind::Dedicated,
                "fallback" => VendorKind::Fallback,
                value => panic!("unknown fixture Vendor kind: {value}"),
            },
            providers: vec![ProviderDescriptor {
                provider_id: build_value("STRAVIA_FIXTURE_PROVIDER_ID", "fixture.lifecycle").into(),
                catalog_id: None,
                display_name: "Lifecycle Contract Fixture".into(),
                description: None,
                channels: vec![ChannelDescriptor {
                    id: "default".into(),
                    name: "Default".into(),
                    description: None,
                    auth: None,
                    protocol: Some(
                        option_env!("STRAVIA_FIXTURE_PROTOCOL")
                            .unwrap_or("fixture-lifecycle")
                            .into(),
                    ),
                    default_base_url: None,
                    default_models_source: None,
                    consumes_catalog_models: false,
                    capabilities: capabilities.clone(),
                    model_capabilities: BTreeSet::new(),
                    search_model_required: false,
                }],
                capabilities,
                config_fields: vec![
                    ConfigField {
                        key: "mode".into(),
                        label: "Fixture mode".into(),
                        description: None,
                        kind: ConfigFieldKind::Enum {
                            options: [
                                "base",
                                "aux",
                                "parallel",
                                "target",
                                "redirect",
                                "trap",
                                "fuel",
                                "memory",
                                "oversized-event",
                                "oversized-output",
                                "websocket",
                                "selection-http",
                                "selection-state",
                                "selection-event",
                                "selection-error",
                                "diagnostics",
                                "continuation",
                                "typed-quota",
                            ]
                            .into_iter()
                            .map(|value| EnumOption {
                                value: value.into(),
                                label: value.into(),
                            })
                            .collect(),
                        },
                        required: false,
                        default_json: Some(Value::String("base".into())),
                        group: None,
                        secret: false,
                        min: None,
                        max: None,
                        max_length: None,
                        pattern: None,
                        visible_when: None,
                    },
                    string_field("auxUrl", false),
                    string_field("targetUrl", false),
                    string_field(
                        option_env!("STRAVIA_FIXTURE_CREDENTIAL_KEY").unwrap_or("apiKey"),
                        true,
                    ),
                    scalar_field(
                        "enabled",
                        "Enable diagnostics",
                        ConfigFieldKind::Bool,
                        Value::Bool(false),
                        None,
                        None,
                    ),
                    scalar_field(
                        "minimumSteps",
                        "Minimum steps",
                        ConfigFieldKind::Int,
                        Value::from(1),
                        Some(1.0),
                        Some(8.0),
                    ),
                    scalar_field(
                        "maximumSteps",
                        "Maximum steps",
                        ConfigFieldKind::Int,
                        Value::from(8),
                        Some(1.0),
                        Some(8.0),
                    ),
                    scalar_field(
                        "temperature",
                        "Temperature",
                        ConfigFieldKind::Decimal,
                        Value::from(0.5),
                        Some(0.0),
                        Some(2.0),
                    ),
                ],
                network: NetworkDeclaration {
                    base_url_field: None,
                    extra_origins: Vec::new(),
                    field_origins: vec!["auxUrl".into()],
                },
                data_compat: DataCompatibility {
                    private_state_format: build_value("STRAVIA_FIXTURE_STATE_FORMAT", "1")
                        .parse()
                        .expect("fixture state format must be an integer"),
                    ..DataCompatibility::default()
                },
            }],
        }
    }

    fn select_protocol(
        _operation: Operation,
        _channel: &str,
        provider: &ProviderSnapshot,
        _request: &AiRequest,
    ) -> Result<String, PluginError> {
        match provider.options.get("mode").and_then(Value::as_str) {
            Some("selection-http") => {
                stravia_vendor_sdk::wit::host::http_start(&HttpRequest {
                    method: "GET".into(),
                    url: format!("{}/selection", provider.base_url.trim_end_matches('/')),
                    headers: Vec::new(),
                    body: Vec::new(),
                })?;
            }
            Some("selection-state") => stravia_vendor_sdk::wit::host::write_private_state(b"999")?,
            Some("selection-event") => stravia_vendor_sdk::wit::host::emit_started()?,
            Some("selection-error") => {
                let secret = provider
                    .credentials
                    .get(option_env!("STRAVIA_FIXTURE_CREDENTIAL_KEY").unwrap_or("apiKey"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                return Err(error(
                    ErrorKind::upstream_unknown(),
                    format!("selection-business-rejected {secret}"),
                ));
            }
            Some("diagnostics") => return Ok("command-code/generate/v1".into()),
            Some("continuation") => return Ok("open-responses/responses/2026-04-24".into()),
            _ => {}
        }
        Ok(provider.protocol.clone())
    }

    fn execute(
        host: &GuestHost,
        operation: Operation,
        _channel: &str,
        input: OperationInput,
    ) -> Result<OperationOutput, PluginError> {
        if let OperationInput::ConfigValidation { provider, .. } = &input {
            let minimum = provider
                .options
                .get("minimumSteps")
                .and_then(Value::as_i64)
                .unwrap_or(1);
            let maximum = provider
                .options
                .get("maximumSteps")
                .and_then(Value::as_i64)
                .unwrap_or(8);
            return Ok(OperationOutput::ConfigValidation(
                ConfigValidationResponse {
                    issues: if minimum > maximum {
                        vec![ValidationIssue {
                            field: None,
                            code: "invalid_step_range".into(),
                            message: "Minimum steps must not exceed maximum steps.".into(),
                        }]
                    } else {
                        Vec::new()
                    },
                    proposed_base_url: None,
                },
            ));
        }
        let OperationInput::Infer { provider, request } = input else {
            return Err(error(
                ErrorKind::Unsupported,
                format!("fixture does not implement {}", operation.as_str()),
            ));
        };
        let mode = provider
            .options
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("base");
        host.emit_started()?;
        match mode {
            "trap" => panic!("intentional lifecycle fixture trap"),
            "fuel" => exhaust_fuel(),
            "memory" => exceed_memory_budget()?,
            "oversized-event" => {
                stravia_vendor_sdk::wit::host::emit_event(&repeated_text_event(2 * 1024 * 1024))?;
            }
            "oversized-output" => exceed_output_budget()?,
            "websocket" => {
                host.ws_connect(stravia_vendor_sdk::WsRequest {
                    url: configured_url(&provider.options, "targetUrl")?.into(),
                    headers: Vec::new(),
                    protocols: Vec::new(),
                })?;
            }
            _ => {}
        }

        let state_before = host
            .read_private_state()?
            .as_deref()
            .map(parse_counter)
            .transpose()?
            .unwrap_or(0);
        let root = match mode {
            "aux" => configured_url(&provider.options, "auxUrl")?,
            "target" => configured_url(&provider.options, "targetUrl")?,
            _ => provider.base_url.trim_end_matches('/'),
        };
        let secret = provider
            .credentials
            .get(option_env!("STRAVIA_FIXTURE_CREDENTIAL_KEY").unwrap_or("apiKey"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let diagnostic_state = "state-4N8P-C6R3";
        if mode == "diagnostics" {
            host.write_private_state(diagnostic_state.as_bytes())?;
        }
        let (method, url, body) = if mode == "redirect" {
            ("GET", format!("{root}/redirect"), Vec::new())
        } else if mode == "diagnostics" {
            (
                "POST",
                format!("{root}/infer"),
                serde_json::to_vec(&serde_json::json!({
                    "ordinary_one": secret,
                    "ordinary_two": diagnostic_state,
                    "business_field": "retained-network-business-field",
                    "request": request,
                }))
                .map_err(invalid)?,
            )
        } else {
            (
                "POST",
                format!("{root}/infer"),
                serde_json::to_vec(&request).map_err(invalid)?,
            )
        };
        let mut headers = vec![
            ("content-type".into(), "application/json".into()),
            (
                "x-fixture-version".into(),
                build_value("STRAVIA_FIXTURE_VERSION", "1.0.0").into(),
            ),
            ("x-fixture-vendor".into(), Self::descriptor().vendor_id),
            ("x-state-before".into(), state_before.to_string()),
        ];
        if !secret.is_empty() {
            headers.push(("authorization".into(), format!("Bearer {secret}")));
        }
        if mode == "diagnostics" {
            headers.push(("x-license".into(), secret.into()));
            headers.push(("x-plugin-state".into(), diagnostic_state.into()));
        }

        let response = host.http_start(HttpRequest {
            method: method.into(),
            url,
            headers,
            body,
        })?;
        let auxiliary = if mode == "parallel" {
            Some(host.http_start(HttpRequest {
                method: "GET".into(),
                url: format!(
                    "{}/aux",
                    configured_url(&provider.options, "auxUrl")?.trim_end_matches('/')
                ),
                headers: Vec::new(),
                body: Vec::new(),
            })?)
        } else {
            None
        };
        let status = response.status()?;
        let body = read_http_body(&response, 1024 * 1024)?;
        if status == 404 && mode == "continuation" {
            return Err(PluginError {
                kind: ErrorKind::ContinuationNotFound,
                message: "fixture continuation is no longer available".into(),
                upstream_status: Some(status),
            });
        }
        if mode == "typed-quota" {
            host.emit_delta(&AiStreamDelta::TextDelta(
                "visible partial before quota".into(),
            ))?;
            let failure = PluginError {
                kind: ErrorKind::upstream(Some(AiErrorKind::QuotaExceeded), None),
                message: "fixture typed quota failure".into(),
                upstream_status: Some(status),
            };
            host.emit_failed(PluginError {
                kind: failure.kind.clone(),
                message: failure.message.clone(),
                upstream_status: failure.upstream_status,
            })?;
            return Err(failure);
        }
        if !(200..300).contains(&status) {
            return Err(PluginError {
                kind: ErrorKind::upstream_unknown(),
                message: format!("fixture upstream returned HTTP {status}"),
                upstream_status: Some(status),
            });
        }
        if let Some(auxiliary) = auxiliary {
            let status = auxiliary.status()?;
            read_http_body(&auxiliary, 1024 * 1024)?;
            if !(200..300).contains(&status) {
                return Err(error(
                    ErrorKind::upstream_unknown(),
                    "auxiliary request failed",
                ));
            }
        }
        let response: AiResponse = if mode == "diagnostics" {
            let mut response = AiResponse::new("fixture-diagnostics", "fixture-model");
            for line in body.split(|byte| *byte == b'\n') {
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                let event: Value = serde_json::from_slice(line).map_err(invalid)?;
                if let Some(text) = event.get("text").and_then(Value::as_str) {
                    response.push_output_text(text);
                }
            }
            response
        } else {
            serde_json::from_slice(&body).map_err(invalid)?
        };
        let next = state_before
            .checked_add(1)
            .ok_or_else(|| error(ErrorKind::ResourceExhausted, "fixture state exhausted"))?;
        host.write_private_state(next.to_string().as_bytes())?;
        host.emit_completed(&response)?;
        Ok(OperationOutput::Infer(Box::new(response)))
    }
}

fn exhaust_fuel() -> ! {
    let mut value = 0x9e37_79b9_7f4a_7c15_u64;
    loop {
        value = std::hint::black_box(
            value
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407)
                .rotate_left(17),
        );
    }
}

fn exceed_memory_budget() -> Result<(), PluginError> {
    const PROBE_BYTES: usize = 65 * 1024 * 1024;

    let mut allocation = Vec::<u8>::new();
    allocation.try_reserve_exact(PROBE_BYTES).map_err(|_| {
        error(
            ErrorKind::ResourceExhausted,
            "guest allocator could not grow beyond the operation memory budget",
        )
    })?;
    allocation.resize(PROBE_BYTES, 0xa5);
    std::hint::black_box(allocation);
    Ok(())
}

fn repeated_text_event(length: usize) -> stravia_vendor_sdk::wit::types::CanonicalEvent {
    // 直接扩展合法 JSON 字符串，避免测试先耗尽序列化 fuel 而未触及事件或输出限额。
    let mut payload = stravia_vendor_sdk::encode_payload(&AiStreamDelta::TextDelta(String::new()));
    let position = payload
        .body
        .windows(2)
        .position(|pair| pair == b"\"\"")
        .expect("text delta contains one empty JSON string")
        + 1;
    let suffix = payload.body.split_off(position);
    payload.body.resize(position + length, b'e');
    payload.body.extend_from_slice(&suffix);
    stravia_vendor_sdk::wit::types::CanonicalEvent::Delta(payload)
}

fn exceed_output_budget() -> Result<(), PluginError> {
    // The encoded event remains below 2 MiB; 32 remain below 64 MiB, while 33 exceed it.
    const EVENT_PAYLOAD_BYTES: usize = 2_040_000;
    const EVENT_COUNT: usize = 33;

    let event = repeated_text_event(EVENT_PAYLOAD_BYTES);
    for _ in 0..EVENT_COUNT {
        stravia_vendor_sdk::wit::host::emit_event(&event)?;
    }
    Ok(())
}

fn string_field(key: &str, secret: bool) -> ConfigField {
    ConfigField {
        key: key.into(),
        label: key.into(),
        description: None,
        kind: ConfigFieldKind::String { multiline: false },
        required: secret,
        default_json: None,
        group: None,
        secret,
        min: None,
        max: None,
        max_length: Some(2048),
        pattern: None,
        visible_when: None,
    }
}

fn scalar_field(
    key: &str,
    label: &str,
    kind: ConfigFieldKind,
    default: Value,
    min: Option<f64>,
    max: Option<f64>,
) -> ConfigField {
    ConfigField {
        label: label.into(),
        kind,
        default_json: Some(default),
        group: Some("Typed settings".into()),
        min,
        max,
        max_length: None,
        ..string_field(key, false)
    }
}

fn configured_url<'a>(
    options: &'a std::collections::BTreeMap<String, Value>,
    key: &str,
) -> Result<&'a str, PluginError> {
    options
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.trim_end_matches('/'))
        .ok_or_else(|| error(ErrorKind::Invalid, format!("missing {key}")))
}

fn parse_counter(bytes: &[u8]) -> Result<u64, PluginError> {
    std::str::from_utf8(bytes)
        .map_err(invalid)?
        .parse()
        .map_err(invalid)
}

fn invalid(error: impl std::fmt::Display) -> PluginError {
    PluginError {
        kind: ErrorKind::Invalid,
        message: format!("fixture payload error: {error}"),
        upstream_status: None,
    }
}

fn error(kind: ErrorKind, message: impl Into<String>) -> PluginError {
    PluginError {
        kind,
        message: message.into(),
        upstream_status: None,
    }
}

fn build_value(name: &str, fallback: &'static str) -> &'static str {
    match name {
        "STRAVIA_FIXTURE_VENDOR_ID" => option_env!("STRAVIA_FIXTURE_VENDOR_ID").unwrap_or(fallback),
        "STRAVIA_FIXTURE_PROVIDER_ID" => {
            option_env!("STRAVIA_FIXTURE_PROVIDER_ID").unwrap_or(fallback)
        }
        "STRAVIA_FIXTURE_KIND" => option_env!("STRAVIA_FIXTURE_KIND").unwrap_or(fallback),
        "STRAVIA_FIXTURE_VERSION" => option_env!("STRAVIA_FIXTURE_VERSION").unwrap_or(fallback),
        "STRAVIA_FIXTURE_AUTHOR" => option_env!("STRAVIA_FIXTURE_AUTHOR").unwrap_or(fallback),
        "STRAVIA_FIXTURE_STATE_FORMAT" => {
            option_env!("STRAVIA_FIXTURE_STATE_FORMAT").unwrap_or(fallback)
        }
        "STRAVIA_FIXTURE_CANONICAL_FORMAT" => {
            option_env!("STRAVIA_FIXTURE_CANONICAL_FORMAT").unwrap_or(fallback)
        }
        _ => fallback,
    }
}

stravia_vendor_sdk::export_vendor!(LifecycleContractVendor);
