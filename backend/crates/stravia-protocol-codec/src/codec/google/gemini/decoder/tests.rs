use super::*;

#[test]
fn explicit_null_controls_reset_inheritance_without_null_output() {
    let request = GoogleDecoder
        .decode_request(serde_json::json!({
            "contents": [], "systemInstruction": null, "tools": null,
            "generationConfig": {
                "temperature": null, "topP": null, "maxOutputTokens": null,
                "presencePenalty": null, "frequencyPenalty": null,
                "responseJsonSchema": null, "responseLogprobs": null,
                "thinkingConfig": {"thinkingLevel": null, "includeThoughts": null}
            }
        }))
        .unwrap();
    for reset in [
        ParameterReset::Instructions,
        ParameterReset::NativeTools,
        ParameterReset::Temperature,
        ParameterReset::TopP,
        ParameterReset::MaxTokens,
        ParameterReset::PresencePenalty,
        ParameterReset::FrequencyPenalty,
        ParameterReset::ResponseFormat,
        ParameterReset::Logprobs,
        ParameterReset::ReasoningLevel,
        ParameterReset::ReasoningSummary,
    ] {
        assert!(request.parameters.resets(reset));
    }
    assert!(request.response_format.is_none());
    let (body, _) = crate::codec::google::gemini::encoder::GoogleEncoder
        .encode_request(request)
        .unwrap();
    assert!(!body.to_string().contains(":null"));
    assert!(body.get("systemInstruction").is_none());
    assert!(body.get("tools").is_none());
    assert!(body["generationConfig"].get("responseJsonSchema").is_none());
}

#[test]
fn allowed_function_names_preserve_the_complete_selection_and_mode() {
    for (mode, required) in [("AUTO", false), ("ANY", true)] {
        for names in [vec!["lookup"], vec!["lookup", "search"], vec![]] {
            let request = GoogleDecoder
                .decode_request(serde_json::json!({
                    "contents": [],
                    "toolConfig": {"functionCallingConfig": {
                        "mode": mode, "allowedFunctionNames": names,
                        "streamFunctionCallArguments": true
                    }}
                }))
                .unwrap();
            assert!(matches!(&request.tool_choice,
                Some(ToolChoice::Allowed { names: actual, required: actual_required })
                if actual.iter().map(String::as_str).collect::<Vec<_>>() == names && *actual_required == required
            ));
            let (body, _) = crate::codec::google::gemini::encoder::GoogleEncoder
                .encode_request(request)
                .unwrap();
            assert_eq!(
                body["toolConfig"],
                serde_json::json!({
                    "functionCallingConfig": {"mode": mode, "allowedFunctionNames": names}
                })
            );
        }
    }
}

#[test]
fn native_tools_normalize_supported_kinds_and_discard_unsupported_tools() {
    let request = GoogleDecoder
        .decode_request(serde_json::json!({
            "contents": [],
            "safetySettings": {"unsupported": "shape"},
            "cachedContent": {"unsupported": "shape"},
            "tools": [
                {"googleSearchRetrieval": {"dynamicRetrievalConfig": {"mode": "MODE_DYNAMIC"}}},
                {"codeExecution": {"private": true}},
                {"urlContext": {}}
            ]
        }))
        .unwrap();
    assert_eq!(request.parameters.native_tools.len(), 2);
    let (body, _) = crate::codec::google::gemini::encoder::GoogleEncoder
        .encode_request(request)
        .unwrap();
    assert_eq!(
        body["tools"],
        serde_json::json!([{"googleSearch": {}}, {"codeExecution": {}}])
    );
    assert!(body.get("safetySettings").is_none());
    assert!(body.get("cachedContent").is_none());
}

#[test]
fn mime_formats_and_logprobs_use_unified_controls() {
    for (mime, expected) in [
        ("application/json", Some(ResponseFormat::JsonObject)),
        ("text/plain", Some(ResponseFormat::Text)),
        ("image/png", None),
    ] {
        let request = GoogleDecoder
            .decode_request(serde_json::json!({
                "contents": [], "generationConfig": {
                    "responseMimeType": mime, "responseLogprobs": true, "logprobs": 0,
                    "responseModalities": ["TEXT", "AUDIO", "IMAGE", "UNSUPPORTED"]
                }
            }))
            .unwrap();
        assert_eq!(
            serde_json::to_value(&request.response_format).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        assert_eq!(request.parameters.logprobs, Some(0));
        assert_eq!(
            request.parameters.modalities,
            Some(vec![Modality::Text, Modality::Audio, Modality::Image])
        );
        let (body, _) = crate::codec::google::gemini::encoder::GoogleEncoder
            .encode_request(request)
            .unwrap();
        assert_eq!(body["generationConfig"]["responseLogprobs"], true);
        assert_eq!(body["generationConfig"]["logprobs"], 0);
        if expected.is_some() {
            assert_eq!(body["generationConfig"]["responseMimeType"], mime);
        } else {
            assert!(body["generationConfig"].get("responseMimeType").is_none());
        }
    }
}

#[test]
fn official_native_generation_controls_project_nested_keys() {
    let request = GoogleDecoder.decode_request(serde_json::json!({
        "contents": [], "generationConfig": {
            "speechConfig": {"voiceConfig": {"prebuiltVoiceConfig": {"voiceName": "Kore", "private": true}, "private": true}, "private": true},
            "imageConfig": {"aspectRatio": "1:1", "imageSize": "1K", "private": true},
            "routingConfig": {"manualMode": {"modelName": "gemini", "private": true}, "private": true},
            "modelSelectionConfig": {"featureSelectionPreference": "PRIORITIZE_QUALITY", "private": true}
        },
        "toolConfig": {"functionCallingConfig": {"mode": "AUTO", "private": true}, "private": true}
    })).unwrap();
    assert_eq!(
        request
            .parameters
            .audio
            .as_ref()
            .and_then(|audio| audio.voice.as_deref()),
        Some("Kore")
    );
    let (body, _) = crate::codec::google::gemini::encoder::GoogleEncoder
        .encode_request(request)
        .unwrap();
    assert_eq!(
        body["generationConfig"]["speechConfig"],
        serde_json::json!({"voiceConfig": {"prebuiltVoiceConfig": {"voiceName": "Kore"}}})
    );
    for key in ["imageConfig", "routingConfig", "modelSelectionConfig"] {
        assert!(body["generationConfig"].get(key).is_none());
    }
    assert_eq!(
        body["toolConfig"],
        serde_json::json!({"functionCallingConfig": {"mode": "AUTO"}})
    );
}

#[test]
fn source_effort_roundtrip_does_not_become_dynamic_budget() {
    use crate::codec::google::gemini::encoder::GoogleEncoder;
    let request = GoogleDecoder
        .decode_request(serde_json::json!({
            "contents": [],
            "generationConfig": {"thinkingConfig": {"thinkingLevel": "HIGH"}}
        }))
        .unwrap();
    assert!(request.reasoning.is_enabled());
    let restored: stravia_runtime_contract::protocol::ir::AiRequest =
        serde_json::from_value(serde_json::to_value(request).unwrap()).unwrap();
    let (wire, _) = GoogleEncoder.encode_request(restored).unwrap();
    assert_eq!(
        wire["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "HIGH"
    );
    assert!(
        wire["generationConfig"]["thinkingConfig"]
            .get("thinkingBudget")
            .is_none()
    );
}

#[test]
fn gemini_rejects_non_null_thinking_budget_and_keeps_null_resets() {
    use stravia_runtime_contract::protocol::ir::ParameterReset;

    for budget in [
        serde_json::json!(0),
        serde_json::json!(-1),
        serde_json::json!(2048),
    ] {
        let err = GoogleDecoder
            .decode_request(serde_json::json!({
                "contents": [],
                "generationConfig": {"thinkingConfig": {"thinkingBudget": budget}}
            }))
            .expect_err("thinkingBudget must be rejected");
        assert!(err.to_string().contains("thinkingLevel"));
    }

    let reset = GoogleDecoder
        .decode_request(serde_json::json!({
            "contents": [],
            "generationConfig": {
                "thinkingConfig": {
                    "thinkingBudget": null,
                    "thinkingLevel": null,
                    "includeThoughts": null
                }
            }
        }))
        .expect("null resets remain valid");
    assert!(reset.parameters.resets(ParameterReset::ReasoningLevel));
    assert!(reset.parameters.resets(ParameterReset::ReasoningSummary));
    assert_eq!(reset.reasoning.level, None);
}

#[test]
fn roundtrips_canonical_controls_without_native_request_copies() {
    use crate::codec::google::gemini::encoder::GoogleEncoder;
    let request = GoogleDecoder
        .decode_request(serde_json::json!({
            "contents": [],
            "cachedContent": "",
            "generationConfig": {
                "temperature": 0,
                "topK": 0,
                "candidateCount": 0,
                "stopSequences": [],
                "responseLogprobs": false,
                "logprobs": 0,
                "responseModalities": [],
                "responseMimeType": "",
                "audioTimestamp": false,
                "mediaResolution": "",
                "routingConfig": null,
                "unknownGenerationControl": {"private": true}
            },
            "safetySettings": [],
            "tools": [{"googleSearch": {"excludeDomains": []}}],
            "unknownTopLevelControl": true
        }))
        .expect("canonical controls");
    assert_eq!(request.parameters.top_k, Some(0));
    assert_eq!(request.parameters.candidate_count, Some(0));
    assert_eq!(request.parameters.logprobs, None);
    assert!(request.parameters.resets(ParameterReset::Logprobs));
    assert_eq!(request.parameters.modalities, Some(vec![]));
    assert!(request.response_format.is_none());
    assert_eq!(request.parameters.native_tools.len(), 1);

    assert!(request.meta.vendor.ingress.is_empty());
    let (body, _) = GoogleEncoder
        .encode_request(request)
        .expect("canonical roundtrip");
    let config = &body["generationConfig"];
    assert_eq!(config["temperature"], 0.0);
    assert_eq!(config["topK"], 0);
    assert_eq!(config["candidateCount"], 0);
    assert_eq!(config["stopSequences"], serde_json::json!([]));
    assert!(config.get("responseLogprobs").is_none());
    assert!(config.get("logprobs").is_none());
    assert_eq!(config["responseModalities"], serde_json::json!([]));
    for key in [
        "responseMimeType",
        "audioTimestamp",
        "mediaResolution",
        "routingConfig",
    ] {
        assert!(config.get(key).is_none());
    }
    assert!(config.get("unknownGenerationControl").is_none());
    assert!(body.get("unknownTopLevelControl").is_none());
    assert!(body.get("cachedContent").is_none());
    assert!(body.get("safetySettings").is_none());
    assert!(body["tools"][0].get("googleSearch").is_some());
}

#[test]
fn preserves_json_schema_literals_including_explicit_null() {
    use crate::codec::google::gemini::encoder::GoogleEncoder;
    {
        let schema = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": ["string", "null"], "enum": [null, "", false, 0],
            "default": null, "examples": [null], "x-custom": {"nested": null}
        });
        let request = GoogleDecoder.decode_request(serde_json::json!({
            "contents": [],
            "generationConfig": {"responseJsonSchema": schema},
            "tools": [{"functionDeclarations": [{"name": "lookup", "parametersJsonSchema": schema}]}]
        })).expect("schema literals");
        let Some(ResponseFormat::JsonSchema {
            schema: canonical, ..
        }) = &request.response_format
        else {
            panic!("canonical schema")
        };
        assert_eq!(canonical, &schema);
        assert_eq!(request.tools.as_ref().unwrap()[0].parameters, schema);
        let (body, _) = GoogleEncoder
            .encode_request(request)
            .expect("schema roundtrip");
        assert_eq!(body["generationConfig"]["responseJsonSchema"], schema);
        assert_eq!(
            body["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"],
            schema
        );
    }
}

#[test]
fn projects_canonical_thinking_level_without_target_control() {
    use crate::codec::google::gemini::encoder::GoogleEncoder;
    for (model, native, expected) in [
        (
            "gemini-3-flash",
            serde_json::json!({"thinkingLevel": "LOW"}),
            serde_json::json!({"thinkingLevel": "LOW", "includeThoughts": true}),
        ),
        (
            "gemini-3-flash",
            serde_json::json!({"thinkingLevel": "HIGH", "includeThoughts": false}),
            serde_json::json!({"thinkingLevel": "HIGH", "includeThoughts": false}),
        ),
        (
            "gemini-3-flash",
            serde_json::json!({"thinkingLevel": "NONE"}),
            serde_json::json!({"thinkingBudget": 0, "includeThoughts": false}),
        ),
        (
            "gemini-2.5-flash",
            serde_json::json!({"thinkingLevel": "LOW"}),
            serde_json::json!({"thinkingBudget": 2048, "includeThoughts": true}),
        ),
    ] {
        let mut request = GoogleDecoder
            .decode_request(serde_json::json!({
                "contents": [], "generationConfig": {"thinkingConfig": native}
            }))
            .expect("thinking controls");
        request.model = model.into();
        assert!(request.reasoning.target_control.is_none());
        let (body, _) = GoogleEncoder
            .encode_request(request)
            .expect("thinking projection");
        assert_eq!(body["generationConfig"]["thinkingConfig"], expected);
    }
}

#[test]
fn clears_native_tools_without_emitting_null_or_empty_controls() {
    use crate::codec::google::gemini::encoder::GoogleEncoder;
    for tools in [
        serde_json::json!([]),
        serde_json::json!([{"googleSearch": null}]),
    ] {
        let request = GoogleDecoder
            .decode_request(serde_json::json!({
                "contents": [], "tools": tools, "toolConfig": null
            }))
            .expect("present native options");
        assert!(request.parameters.native_tools.is_empty());
        if tools.as_array().unwrap().is_empty() {
            assert!(request.parameters.resets(ParameterReset::NativeTools));
        }
        let (body, _) = GoogleEncoder
            .encode_request(request)
            .expect("native option presence");
        assert!(body.get("tools").is_none());
        assert!(body.get("toolConfig").is_none());
    }
}

#[test]
fn decodes_both_official_schema_carriers_into_canonical_format() {
    use crate::codec::google::gemini::encoder::GoogleEncoder;
    for (carrier, schema) in [
        (
            "responseSchema",
            serde_json::json!({
                "type": "OBJECT", "properties": {"answer": {"type": "STRING", "nullable": true}, "items": {"type": "ARRAY", "minItems": "1", "items": {"type": "INTEGER"}}}
            }),
        ),
        (
            "responseJsonSchema",
            serde_json::json!({
                "type": "object", "additionalProperties": false,
                "$defs": {"entry": {"type": "string"}},
                "properties": {"answer": {"$ref": "#/$defs/entry"}}
            }),
        ),
    ] {
        let mut config = serde_json::json!({"responseMimeType": "application/json"});
        config[carrier] = schema.clone();
        let request = GoogleDecoder
            .decode_request(serde_json::json!({"contents": [], "generationConfig": config}))
            .expect("native schema");
        let Some(ResponseFormat::JsonSchema {
            schema: normalized, ..
        }) = &request.response_format
        else {
            panic!("canonical response format")
        };
        if carrier == "responseSchema" {
            assert_eq!(normalized["type"], "object");
            assert_eq!(
                normalized["properties"]["answer"]["type"],
                serde_json::json!(["string", "null"])
            );
            assert!(normalized["properties"]["answer"].get("nullable").is_none());
            assert_eq!(normalized["properties"]["items"]["minItems"], 1);
            assert_eq!(
                normalized["properties"]["items"]["items"]["type"],
                "integer"
            );
        } else {
            assert_eq!(normalized, &schema);
        }
        let normalized = normalized.clone();
        let (body, _) = GoogleEncoder
            .encode_request(request)
            .expect("native roundtrip");
        assert_eq!(body["generationConfig"]["responseJsonSchema"], normalized);
        assert!(body["generationConfig"].get("responseSchema").is_none());
    }
}

#[test]
fn decodes_and_roundtrips_native_function_calling_modes() {
    use crate::codec::google::gemini::encoder::GoogleEncoder;
    for config in [
        serde_json::json!({"mode": "AUTO"}),
        serde_json::json!({"mode": "NONE"}),
        serde_json::json!({"mode": "ANY"}),
        serde_json::json!({"mode": "ANY", "allowedFunctionNames": ["lookup"]}),
        serde_json::json!({"mode": "ANY", "allowedFunctionNames": ["lookup", "search"]}),
    ] {
        let request = GoogleDecoder.decode_request(serde_json::json!({"contents": [], "toolConfig": {"functionCallingConfig": config}})).expect("native tool choice");
        match config["mode"].as_str().unwrap() {
            "AUTO" => assert!(matches!(request.tool_choice, Some(ToolChoice::Auto))),
            "NONE" => assert!(matches!(request.tool_choice, Some(ToolChoice::None))),
            "ANY" if config["allowedFunctionNames"].as_array().is_some() => {
                assert!(
                    matches!(&request.tool_choice, Some(ToolChoice::Allowed { names, required: true })
                        if serde_json::json!(names) == config["allowedFunctionNames"])
                )
            }
            _ => assert!(matches!(request.tool_choice, Some(ToolChoice::Required))),
        }
        let (body, _) = GoogleEncoder
            .encode_request(request)
            .expect("native tool choice roundtrip");
        assert_eq!(body["toolConfig"]["functionCallingConfig"], config);
    }
    let request = GoogleDecoder.decode_request(serde_json::json!({"contents": [], "toolConfig": {"functionCallingConfig": {"mode": "UNKNOWN"}}})).expect("unknown mode is not rejected");
    assert!(request.tool_choice.is_none());
}

#[test]
fn decodes_thought_signature_and_function_ids() {
    let request = GoogleDecoder
        .decode_request(serde_json::json!({
            "contents": [
                {
                    "role": "model",
                    "parts": [
                        {
                            "text": "checked the repository",
                            "thought": true,
                            "thoughtSignature": "opaque-reasoning"
                        },
                        {
                            "functionCall": {
                                "id": "call_read",
                                "name": "read",
                                "args": {"path": "Cargo.toml"}
                            }
                        }
                    ]
                },
                {
                    "role": "user",
                    "parts": [{
                        "functionResponse": {
                            "id": "call_read",
                            "name": "read",
                            "response": {"result": "workspace"}
                        }
                    }]
                }
            ]
        }))
        .expect("Gemini request");

    let MessageContent::Blocks(assistant_blocks) = &request.items[0].content else {
        panic!("assistant blocks");
    };
    assert!(matches!(
        &assistant_blocks[0],
        ContentBlock::Thinking {
            thinking,
            signature: Some(signature)
        } if thinking == "checked the repository" && signature == "opaque-reasoning"
    ));
    assert!(matches!(
        &assistant_blocks[1],
        ContentBlock::ToolUse { id, name, .. }
            if id == "call_read" && name == "read"
    ));

    let MessageContent::Blocks(tool_blocks) = &request.items[1].content else {
        panic!("tool blocks");
    };
    assert!(matches!(
        &tool_blocks[0],
        ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "call_read"
    ));
    assert_eq!(request.items[1].tool_call_id.as_deref(), Some("call_read"));
}

#[test]
fn missing_function_ids_are_compact_deterministic_and_skip_supplied_ids() {
    let body = serde_json::json!({
        "contents": [
            {
                "role": "model",
                "parts": [
                    {"functionCall": {"name": "first", "args": {}}},
                    {"functionCall": {"id": "tc_1", "name": "external", "args": {}}}
                ]
            },
            {
                "role": "model",
                "parts": [
                    {"functionCall": {"name": "second", "args": {}}}
                ]
            }
        ]
    });

    let first = GoogleDecoder
        .decode_request(body.clone())
        .expect("Gemini request");
    let repeated = GoogleDecoder.decode_request(body).expect("Gemini request");
    let first_ids: Vec<&str> = first
        .items
        .iter()
        .flat_map(|item| item.tool_calls.iter().flatten())
        .map(|call| call.id.as_str())
        .collect();
    let repeated_ids: Vec<&str> = repeated
        .items
        .iter()
        .flat_map(|item| item.tool_calls.iter().flatten())
        .map(|call| call.id.as_str())
        .collect();

    assert_eq!(first_ids, ["tc_2", "tc_1", "tc_3"]);
    assert_eq!(repeated_ids, first_ids);
}

#[test]
fn include_thoughts_preserves_summary_without_selecting_intensity() {
    for (include, summary) in [
        (true, ReasoningSummary::Auto),
        (false, ReasoningSummary::None),
    ] {
        let request = GoogleDecoder
            .decode_request(serde_json::json!({
                "contents": [{"role": "user", "parts": [{"text": "reason"}]}],
                "generationConfig": {
                    "thinkingConfig": {"includeThoughts": include}
                }
            }))
            .expect("Gemini request");

        assert_eq!(request.reasoning.summary, Some(summary));
        assert_eq!(request.reasoning.level, None);
        assert!(!request.reasoning.is_enabled());
    }
}

#[test]
fn thinking_level_decodes_case_insensitively() {
    let request = GoogleDecoder
        .decode_request(serde_json::json!({
            "contents": [{"role": "user", "parts": [{"text": "reason"}]}],
            "generationConfig": {
                "thinkingConfig": {"thinkingLevel": "HIGH"}
            }
        }))
        .expect("Gemini request");

    assert_eq!(
        request.reasoning.level,
        Some(stravia_runtime_contract::thinking::ThinkingLevel::High)
    );
}

#[test]
fn unknown_thinking_level_is_rejected() {
    let error = GoogleDecoder
        .decode_request(serde_json::json!({
            "contents": [{"role": "user", "parts": [{"text": "reason"}]}],
            "generationConfig": {
                "thinkingConfig": {"thinkingLevel": "turbo"}
            }
        }))
        .expect_err("unknown thinkingLevel must fail");

    assert!(
        error
            .to_string()
            .contains("unsupported Gemini thinkingLevel")
    );
}
