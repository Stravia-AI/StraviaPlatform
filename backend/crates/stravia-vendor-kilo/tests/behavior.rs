use super::*;
use serde_json::json;
use stravia_runtime_contract::protocol::ir::{ToolChoice, ToolSpec};
use stravia_runtime_contract::thinking::ThinkingLevel;

fn provider() -> ProviderSnapshot {
    ProviderSnapshot {
        provider_id: "kilo".into(),
        channel: "default".into(),
        base_url: "https://attacker.invalid".into(),
        protocol: "openai-compatible".into(),
        options: BTreeMap::new(),
        credentials: BTreeMap::new(),
        model: Some("ignored/model".into()),
        model_metadata: Some(ModelMetadata {
            extensions: BTreeMap::from([
                (
                    "supported_parameters".into(),
                    json!(["reasoning", "tools", "tool_choice", "max_tokens"]),
                ),
                (
                    "opencode".into(),
                    json!({"variants":{"high":{"reasoning":{"effort":"high"}}}}),
                ),
            ]),
            ..Default::default()
        }),
        client_headers: vec![
            ("authorization".into(), "Bearer attacker".into()),
            ("X-KiloCode-OrganizationId".into(), "attacker-org".into()),
            ("X-KiloCode-TaskId".into(), "attacker-task".into()),
            ("X-KiloCode-Parent-TaskId".into(), "attacker-parent".into()),
            ("X-KiloCode-ProjectId".into(), "attacker-project".into()),
            ("X-KiloCode-MachineId".into(), "attacker-machine".into()),
        ],
        operation_metadata: BTreeMap::new(),
    }
}
#[test]
fn only_credentials_supply_auth_and_org_never_comes_from_headers() {
    let mut p = provider();
    p.options.insert("apiKey".into(), json!("not-a-credential"));
    assert!(
        headers(&p)
            .unwrap()
            .iter()
            .all(|(n, _)| !n.eq_ignore_ascii_case("authorization")
                && !n.eq_ignore_ascii_case("X-KiloCode-OrganizationId"))
    );
    p.options.insert("organizationId".into(), json!("org"));
    assert!(headers(&p).is_err());
    p.credentials.insert("apiKey".into(), json!("valid"));
    assert!(headers(&p).is_err());
    p.credentials
        .insert("organizations".into(), json!([{"id":"org"}]));
    assert!(
        headers(&p)
            .unwrap()
            .contains(&("authorization".into(), "Bearer valid".into()))
    );
    assert!(
        headers(&p)
            .unwrap()
            .contains(&("X-KiloCode-OrganizationId".into(), "org".into()))
    );
    p.credentials.insert("expiresAtUnixMs".into(), json!(0));
    assert!(matches!(key(&p).unwrap_err().kind, ErrorKind::Auth));
}

#[test]
fn optional_client_task_identity_is_not_fabricated_or_forwarded() {
    let mut p = provider();
    p.operation_metadata
        .insert("session_affinity".into(), json!("trusted-root"));
    p.operation_metadata
        .insert("transport_affinity".into(), json!("trusted-session"));
    let outbound = headers(&p).unwrap();
    for name in [
        "X-KiloCode-TaskId",
        "X-KiloCode-Parent-TaskId",
        "X-KiloCode-ProjectId",
        "X-KiloCode-MachineId",
        "x-request-id",
    ] {
        assert!(
            !outbound
                .iter()
                .any(|(key, _)| key.eq_ignore_ascii_case(name))
        );
    }
    let mut request = controlled_request();
    request.parameters.session.request_id = Some("unscoped-client-request".into());
    let (_, body) = request::prepare(&p, request).unwrap();
    for name in [
        "session_id",
        "request_id",
        "task_id",
        "conversation",
        "previous_response_id",
    ] {
        assert!(body.get(name).is_none(), "{name}");
    }
}
#[test]
fn catalog_preserves_ids_and_metadata_and_skips_image_generators() {
    let catalog = json!({"data":[
        {"id":"vendor/paid","pricing":{"prompt":"0","completion":"0"}},
        {"id":"vendor/new:free","name":"Future","pricing":{"prompt":"0","completion":"0"},"context_length":1000,"supported_parameters":["reasoning","tools"],"opencode":{"variants":{"high":{}},"ai_sdk_provider":"openai"},"future":{"field":true}},
        {"id":"vendor/painter","architecture":{"output_modalities":["image"]}}
    ]});
    let result = models::parse(catalog).unwrap();
    assert_eq!(result.models.len(), 2);
    assert_eq!(result.models[1].id, "vendor/new:free");
    assert_eq!(result.models[1].metadata["future"], json!({"field":true}));
    assert_eq!(
        result.models[1].capabilities,
        vec!["reasoning", "tool_call"]
    );
    assert!(models::parse(json!({})).is_err());
}
#[test]
fn catalog_markdown_cannot_invalidate_the_platform_inventory() {
    for description in ["first line\nsecond line".to_owned(), "界".repeat(1500)] {
        let result = models::parse(json!({"data":[{"id":"vendor/model:free","description":description,"pricing":{"prompt":"0","completion":"0"}}]})).unwrap();
        let metadata = &result.models[0].metadata;
        assert!(!metadata.contains_key("description"));
        assert_eq!(metadata["kilo"]["description"], description);
    }
}
#[test]
fn canonical_tools_reasoning_and_model_use_one_wire_source() {
    let p = provider();
    for choice in [
        ToolChoice::Auto,
        ToolChoice::None,
        ToolChoice::Required,
        ToolChoice::Named {
            name: "lookup".into(),
        },
    ] {
        let mut r = AiRequest::new("vendor/model", vec![]);
        r.reasoning.level = Some(ThinkingLevel::High);
        r.tools = Some(vec![ToolSpec {
            name: "lookup".into(),
            description: None,
            parameters: json!({"type":"object"}),
            strict: None,
            defer_loading: None,
        }]);
        r.tool_choice = Some(choice);
        let (protocol, body) = request::prepare(&p, r).unwrap();
        assert_eq!(protocol, CHAT);
        assert_eq!(body["model"], "vendor/model");
        assert_eq!(body["reasoning"]["effort"], "high");
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(body["tools"].as_array().unwrap().len(), 1);
        assert!(body.get("tool_choice").is_some());
    }
}
#[test]
fn metadata_routing_does_not_guess_all_gpt_models_and_undeclared_effort_is_omitted() {
    let mut p = provider();
    assert!(!request::responses(&p, "openai/gpt-4.1"));
    assert!(!request::responses(&p, "openai/gpt-5.5"));
    p.model_metadata
        .as_mut()
        .unwrap()
        .extensions
        .insert("opencode".into(), json!({"ai_sdk_provider":"anthropic"}));
    assert!(!request::responses(&p, "openai/o3"));
    p.model_metadata.as_mut().unwrap().extensions.insert(
        "opencode".into(),
        json!({"ai_sdk_provider":"openai","variants":{"low":{}}}),
    );
    assert!(request::responses(&p, "future/provider-model"));
    let mut r = AiRequest::new(
        "future/provider-model",
        vec![stravia_runtime_contract::protocol::ir::AiItem::output_text(
            "hello",
        )],
    );
    r.reasoning.level = Some(ThinkingLevel::High);
    let (protocol, body) = request::prepare(&p, r).unwrap();
    assert_eq!(protocol, RESPONSES);
    assert!(body.pointer("/reasoning/effort").is_none());
    assert!(body.get("prompt_cache_retention").is_none());
    assert!(body.get("session_id").is_none());
}
#[test]
fn balance_is_a_usd_remaining_amount_without_an_invented_limit() {
    let response = allowance::parse(json!({"balance":12.345678})).unwrap();
    let [item] = response.allowances.as_slice() else {
        panic!("exactly one balance item")
    };
    assert_eq!(item.kind, "balance");
    let remaining = item.remaining.as_ref().unwrap();
    assert_eq!(remaining.value, "12.345678");
    assert_eq!(remaining.currency.as_deref(), Some("USD"));
    assert!(item.used.is_none() && item.limit.is_none());
    assert!(response.models.is_empty());
    assert!(response.billing.is_none());
    assert!(allowance::parse(json!({})).is_err());
    assert!(allowance::parse(json!({"balance":"12"})).is_err());
    assert!(allowance::parse(json!({"usage":[]})).is_err());
}
#[test]
fn pending_denied_expired_and_gateway_failures_are_not_success() {
    assert!(!auth::poll_status(202).unwrap());
    assert!(auth::poll_status(403).is_err());
    assert!(auth::poll_status(410).is_err());
    assert!(stream::failure(&json!({"choices":[{"finish_reason":"error"}]})).is_some());
    let mut failed = json!({"type":"response.failed","response":{"error":{"type":"billing_error","message":"No credits"}}});
    stream::normalize(&mut failed);
    assert_eq!(failed["response"]["error"]["code"], "billing_error");
    let mut frames = stream::Frames::default();
    assert!(frames.push(b"data: {\"text\":\"a\"}\r").unwrap().is_empty());
    assert_eq!(frames.push(b"\n\r\n").unwrap().len(), 1);
}

fn controlled_request() -> AiRequest {
    let mut r = AiRequest::new(
        "vendor/model",
        vec![stravia_runtime_contract::protocol::ir::AiItem::output_text(
            "hello",
        )],
    );
    r.generation.temperature = Some(0.0);
    r.generation.top_p = Some(0.0);
    r.generation.max_tokens = Some(32);
    r.generation.stop = Some(vec![]);
    r.parallel_tool_calls = Some(false);
    r.parameters = serde_json::from_value(json!({
        "top_k":0,"min_p":0.0,"repetition_penalty":1.0,"candidate_count":1,
        "logit_bias":{},"logprobs":0,"modalities":[],
        "audio":{"voice":"alloy","format":"wav"},
        "verbosity":"low","metadata":{},"store":false,"background":false,
        "max_tool_calls":0,"truncation":"disabled","end_user_id":"",
        "service_tier":"default",
        "cache":{"key":"","retention":"extended"},
        "continuation":{"previous_response":"foreign"},
        "session":{"id":"foreign"}
    }))
    .unwrap();
    r.response_format = Some(
        stravia_runtime_contract::protocol::ir::ResponseFormat::JsonSchema {
            name: "nullable".into(),
            description: None,
            schema: json!({"type":"object","properties":{"value":{"type":["string","null"]}},"required":["value"]}),
            strict: Some(false),
        },
    );
    r.stream.enabled = true;
    r
}
fn all_parameters(p: &mut ProviderSnapshot) {
    p.model_metadata.as_mut().unwrap().extensions.insert(
        "supported_parameters".into(),
        json!([
            "temperature",
            "top_p",
            "max_tokens",
            "stop",
            "top_k",
            "min_p",
            "repetition_penalty",
            "n",
            "logit_bias",
            "logprobs",
            "top_logprobs",
            "modalities",
            "audio",
            "prediction",
            "response_format",
            "verbosity",
            "parallel_tool_calls",
            "metadata",
            "store",
            "background",
            "max_tool_calls",
            "include",
            "truncation",
            "user",
            "safety_identifier",
            "service_tier",
            "prompt_cache_key",
            "prompt_cache_retention",
            "stream_options",
            "tools",
            "tool_choice",
            "reasoning",
            "session_id",
            "client_metadata",
            "previous_response_id",
            "conversation",
            "context_management",
            "web_search_options"
        ]),
    );
}
#[test]
fn chat_catalog_allowlist_preserves_explicit_controls_and_nullable_schema() {
    let mut p = provider();
    all_parameters(&mut p);
    let (_, body) = request::prepare(&p, controlled_request()).unwrap();
    for (name, expected) in [
        ("temperature", json!(0.0)),
        ("top_p", json!(0.0)),
        ("top_k", json!(0)),
        ("min_p", json!(0.0)),
        ("repetition_penalty", json!(1.0)),
        ("n", json!(1)),
        ("logit_bias", json!({})),
        ("logprobs", json!(true)),
        ("top_logprobs", json!(0)),
        ("modalities", json!([])),
        ("parallel_tool_calls", json!(false)),
        ("stop", json!([])),
        ("metadata", json!({})),
        ("store", json!(false)),
        ("prompt_cache_key", json!("")),
        ("service_tier", json!("default")),
        ("safety_identifier", json!("")),
        ("prompt_cache_retention", json!("24h")),
    ] {
        assert_eq!(body[name], expected, "{name}");
    }
    assert_eq!(
        body["response_format"]["json_schema"]["schema"]["properties"]["value"]["type"],
        json!(["string", "null"])
    );
    assert_eq!(body["response_format"]["json_schema"]["strict"], false);
    for name in [
        "previous_response_id",
        "conversation",
        "client_metadata",
        "context_management",
        "web_search_options",
        "background",
        "include",
        "max_tool_calls",
    ] {
        assert!(body.get(name).is_none(), "{name}");
    }
}
#[test]
fn unknown_or_empty_catalog_does_not_authorize_optional_controls() {
    for supported in [None, Some(json!([]))] {
        let mut p = provider();
        let metadata = &mut p.model_metadata.as_mut().unwrap().extensions;
        metadata.remove("supported_parameters");
        if let Some(supported) = supported {
            metadata.insert("supported_parameters".into(), supported);
        }
        let mut r = controlled_request();
        r.tools = Some(vec![]);
        r.tool_choice = Some(ToolChoice::Auto);
        r.reasoning.mode = Some("enabled".into());
        let (_, body) = request::prepare(&p, r).unwrap();
        let keys: BTreeSet<_> = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            BTreeSet::from(["model", "messages", "stream", "stream_options"])
        );
        assert!(body["stream_options"].get("include_obfuscation").is_none());
    }
}

#[test]
fn unknown_controls_and_catalog_names_do_not_gain_passthrough_authority() {
    let mut p = provider();
    p.model_metadata.as_mut().unwrap().extensions.insert(
        "supported_parameters".into(),
        json!(["arbitrary_control", "provider", "extra_body"]),
    );
    p.options.insert(
        "extra_body".into(),
        json!({"provider":{"allow_fallbacks":true}}),
    );
    let plain = AiRequest::new("vendor/model", Vec::new());
    let mut injected = plain.clone();
    injected.parameters = serde_json::from_value(json!({
        "extra_body":{"provider":{"allow_fallbacks":true}},
        "extensions":{"openai-chat":{"arbitrary_control":true}},
        "arbitrary_control":true
    }))
    .unwrap();
    assert_eq!(
        request::prepare(&p, plain).unwrap(),
        request::prepare(&p, injected).unwrap()
    );
}

#[test]
fn responses_family_filters_chat_sampling_and_foreign_native_state() {
    let mut p = provider();
    all_parameters(&mut p);
    p.model_metadata
        .as_mut()
        .unwrap()
        .extensions
        .insert("opencode".into(), json!({"ai_sdk_provider":"openai"}));
    let mut r = controlled_request();
    r.items
        .push(stravia_runtime_contract::protocol::ir::AiItem::reasoning(
            vec!["summary".into()],
            vec!["plain".into()],
            Some("foreign-opaque".into()),
        ));
    let (protocol, body) = request::prepare(&p, r).unwrap();
    assert_eq!(protocol, RESPONSES);
    for name in [
        "top_k",
        "min_p",
        "repetition_penalty",
        "n",
        "logit_bias",
        "logprobs",
        "modalities",
        "audio",
        "prediction",
        "stop",
        "user",
        "session_id",
        "prompt_cache_retention",
        "prompt_cache_options",
        "client_metadata",
        "previous_response_id",
        "conversation",
        "context_management",
    ] {
        assert!(body.get(name).is_none(), "{name}");
    }
    assert!(!body.to_string().contains("foreign"));
    for (name, value) in [
        ("background", json!(false)),
        ("store", json!(false)),
        ("max_tool_calls", json!(0)),
        ("include", json!(["message.output_text.logprobs"])),
        ("parallel_tool_calls", json!(false)),
        ("prompt_cache_key", json!("")),
        ("max_output_tokens", json!(32)),
    ] {
        assert_eq!(body[name], value, "{name}");
    }
    assert_eq!(
        body["text"]["format"]["schema"]["properties"]["value"]["type"],
        json!(["string", "null"])
    );
    assert_eq!(body["text"]["verbosity"], "low");
    p.model_metadata
        .as_mut()
        .unwrap()
        .extensions
        .insert("supported_parameters".into(), json!(["verbosity"]));
    let (_, body) = request::prepare(&p, controlled_request()).unwrap();
    assert_eq!(body["text"], json!({"verbosity":"low"}));
}
#[test]
fn chat_reasoning_mode_and_budget_have_canonical_ownership() {
    let p = provider();
    let mut r = AiRequest::new("vendor/model", vec![]);
    r.reasoning.mode = Some("disabled".into());
    let (_, body) = request::prepare(&p, r).unwrap();
    assert_eq!(body["reasoning"], json!({"enabled":false}));
    let mut r = AiRequest::new("vendor/model", vec![]);
    r.reasoning.mode = Some("enabled".into());
    r.reasoning.budget_tokens = Some(0);
    let (_, body) = request::prepare(&p, r).unwrap();
    assert_eq!(body["reasoning"], json!({"enabled":true,"max_tokens":0}));
}
#[test]
fn unknown_parameters_and_native_tool_choices_never_gain_wire_rights() {
    let mut p = provider();
    all_parameters(&mut p);
    let r = controlled_request();
    let mut value = serde_json::to_value(&r).unwrap();
    value["parameters"]["future_control"] = json!({"client_metadata":{"session_id":"foreign"}});
    let r = serde_json::from_value(value).unwrap();
    let (_, body) = request::prepare(&p, r).unwrap();
    assert!(body.get("future_control").is_none());
    assert!(body.get("client_metadata").is_none());
    let mut r = controlled_request();
    r.tool_choice = Some(ToolChoice::Native(
        stravia_runtime_contract::protocol::ir::NativeToolKind::WebSearch,
    ));
    r.parameters
        .native_tools
        .push(stravia_runtime_contract::protocol::ir::NativeTool::WebSearch(Default::default()));
    let (_, body) = request::prepare(&p, r).unwrap();
    assert!(body.get("tool_choice").is_none());
    assert!(body.get("tools").is_none());
}
#[test]
fn sparse_catalog_filters_nested_response_format_independently_from_verbosity() {
    let mut p = provider();
    p.model_metadata
        .as_mut()
        .unwrap()
        .extensions
        .insert("opencode".into(), json!({"ai_sdk_provider":"openai"}));
    p.model_metadata.as_mut().unwrap().extensions.insert(
        "supported_parameters".into(),
        json!(["response_format", "max_tokens"]),
    );
    let (_, body) = request::prepare(&p, controlled_request()).unwrap();
    assert!(body["text"].get("format").is_some());
    assert!(body["text"].get("verbosity").is_none());
    assert_eq!(body["max_output_tokens"], 32);
    assert!(body.get("parallel_tool_calls").is_none());
    assert!(body.get("store").is_none());
    p.model_metadata
        .as_mut()
        .unwrap()
        .extensions
        .insert("supported_parameters".into(), json!(["store"]));
    let r = AiRequest::new(
        "vendor/model",
        vec![stravia_runtime_contract::protocol::ir::AiItem::output_text(
            "hello",
        )],
    );
    let (_, body) = request::prepare(&p, r).unwrap();
    assert!(body.get("store").is_none());
}
