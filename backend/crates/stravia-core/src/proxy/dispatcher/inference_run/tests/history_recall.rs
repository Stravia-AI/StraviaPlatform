use super::*;

#[tokio::test]
async fn merged_full_read_scope_does_not_count_as_client_declaration() {
    use stravia_runtime_contract::hook::{
        ContextCompleteness, ContextSnapshot, Hook, HookAction, HookEvent, ReadExposureScope,
        RequestKind, SessionContext, TransportKind,
    };

    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_owned(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let key = gateway
        .admin()
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Merged scope reader".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: true,
            transparent_injections: crate::db::models::TransparentInjectionSet::from_items([
                crate::db::models::TransparentInjection::WebSearch,
                crate::db::models::TransparentInjection::MediaUnderstanding,
                crate::db::models::TransparentInjection::HistoryRecall,
            ]),
            model_ids: vec![],
            spend_limit: None,
        })
        .await
        .expect("API key");
    let hook = crate::mcp::read::ReadPlanningHook {
        tool: crate::mcp::read::ReadTool::new(&gateway, &mut Vec::new()).expect("read tool"),
    };
    let session = SessionContext {
        tools_fixed: false,
        request_id: "request".into(),
        run_id: "run".into(),
        request_kind: RequestKind::Generation,
        ingress: stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        transport: TransportKind::Http,
        principal: stravia_runtime_contract::Principal::new(key.id),
        cancellation: stravia_runtime_contract::CancellationToken::new(),
        inherited_media_turns: Vec::new(),
        response_id: None,
        previous_response_id: None,
    };
    let request = AiRequest::new("model", vec![]);
    let snapshot = ContextSnapshot::from_request(&request, ContextCompleteness::Full);
    let actions = hook
        .create_session(&session)
        .handle(HookEvent::Request {
            session: &session,
            original: &snapshot,
            read_scope: ReadExposureScope::FULL,
            read_declared: false,
            current: &request,
            input_start: 0,
            context: &snapshot,
            route: None,
            round: 0,
        })
        .await
        .expect("read planning");
    let (scope, description) = actions
        .actions
        .iter()
        .find_map(|action| match action {
            HookAction::ExposeRead { scope, description } => Some((scope, description)),
            _ => None,
        })
        .expect("injected reader");
    assert!(scope.history(), "merged scope must retain history");
    assert!(description.contains("History Recall"), "{description}");
    drop(hook);
    close_test_gateway(gateway, data_dir).await;
}

#[tokio::test]
async fn disabled_transparent_injection_does_not_expose_history_reader() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_owned(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let admin = gateway.admin();
    let key = admin
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Disabled history reader".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: false,
            transparent_injections: crate::db::models::TransparentInjectionSet::from_items([
                crate::db::models::TransparentInjection::HistoryRecall,
            ]),
            model_ids: vec![],
            spend_limit: None,
        })
        .await
        .expect("API key");
    let (url, _, requests) =
        serve_openai_sequence_with_requests(vec![openai_response("Done.")]).await;
    let route_id = configure_route_with_id(&gateway, "disabled-history", &[url]).await;
    admin
        .update_api_key(
            &key.id,
            serde_json::from_value(serde_json::json!({"model_ids": [route_id]})).unwrap(),
        )
        .await
        .expect("bind model");
    let mut user = stravia_runtime_contract::protocol::ir::AiItem::output_text("Hello");
    user.role = stravia_runtime_contract::protocol::ir::Role::User;
    let response = execute_non_stream_request_with_headers(
        gateway.clone(),
        bearer_headers(&key.token),
        AiRequest::new("disabled-history", vec![user]),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    {
        let requests = requests.lock();
        let initial: serde_json::Value =
            serde_json::from_str(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap();
        assert!(!initial["tools"].as_array().is_some_and(|tools| {
            tools
                .iter()
                .any(|tool| tool["function"]["name"] == "StraviaRead")
        }));
    }
    close_test_gateway(gateway, data_dir).await;
}

#[tokio::test]
async fn internal_agent_reader_cannot_access_history_for_history_key() {
    use crate::agent::{AgentTool, AgentToolContext, PlatformToolAgentAdapter};
    use stravia_runtime_contract::agent::AgentTurnId;

    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_owned(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let key = gateway
        .admin()
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Agent history key".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: true,
            transparent_injections: crate::db::models::TransparentInjectionSet::from_items([
                crate::db::models::TransparentInjection::HistoryRecall,
            ]),
            model_ids: vec![],
            spend_limit: None,
        })
        .await
        .expect("API key");
    let read = crate::mcp::read::ReadTool::new(&gateway, &mut Vec::new()).expect("read tool");
    let adapter = PlatformToolAgentAdapter::new(std::sync::Arc::new(read), 1);
    let error = adapter
        .execute(
            AgentToolContext {
                principal: stravia_runtime_contract::Principal::new(key.id),
                turn_id: AgentTurnId::agent(),
                cancellation: stravia_runtime_contract::CancellationToken::new(),
                deadline: std::time::Instant::now() + std::time::Duration::from_secs(60),
            },
            serde_json::json!({"path": "stravia://history/"}),
        )
        .await
        .expect_err("agent history read must be rejected");
    assert!(
        error.message.contains(
            "History Recall is only available through this API key's transparent injection."
        ),
        "{error}"
    );
    drop(adapter);
    close_test_gateway(gateway, data_dir).await;
}

/// Key 选中「历史回看」时注入的 StraviaRead 才带历史范围；只开基础读取时历史路径被拒绝。
#[tokio::test]
async fn history_recall_injection_gates_history_paths_in_transparent_read() {
    for history in [true, false] {
        let data_dir = tempfile::tempdir().expect("temporary data directory");
        let gateway = Gateway::new(crate::config::GatewayConfig {
            data_dir: data_dir.path().to_owned(),
            ..Default::default()
        })
        .await
        .expect("Gateway");
        let admin = gateway.admin();
        let injections = if history {
            crate::db::models::TransparentInjectionSet::from_items([
                crate::db::models::TransparentInjection::HistoryRecall,
            ])
        } else {
            Default::default()
        };
        let key = admin
            .create_api_key(crate::db::models::CreateApiKey {
                key: None,
                name: "History reader".into(),
                rpm_limit: None,
                expires_at: None,
                mcp_access_enabled: false,
                transparent_injection_enabled: true,
                transparent_injections: injections,
                model_ids: vec![],
                spend_limit: None,
            })
            .await
            .expect("API key");
        let mut first = openai_response("");
        first["choices"][0]["message"]["tool_calls"] = serde_json::json!([{
            "id": "history-root",
            "type": "function",
            "function": {
                "name": "StraviaRead",
                "arguments": serde_json::json!({"path": "stravia://history/"}).to_string()
            }
        }]);
        first["choices"][0]["finish_reason"] = serde_json::json!("tool_calls");
        let (url, _, requests) =
            serve_openai_sequence_with_requests(vec![first, openai_response("Done.")]).await;
        let route_id = configure_route_with_id(&gateway, "history-reader", &[url]).await;
        admin
            .update_api_key(
                &key.id,
                serde_json::from_value(serde_json::json!({"model_ids": [route_id]})).unwrap(),
            )
            .await
            .expect("bind model");
        let mut user =
            stravia_runtime_contract::protocol::ir::AiItem::output_text("What did we do before?");
        user.role = stravia_runtime_contract::protocol::ir::Role::User;
        let response = execute_non_stream_request_with_headers(
            gateway.clone(),
            bearer_headers(&key.token),
            AiRequest::new("history-reader", vec![user]),
        )
        .await;
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        {
            let requests = requests.lock();
            let initial: serde_json::Value =
                serde_json::from_str(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap();
            let description = initial["tools"]
                .as_array()
                .expect("injected tools")
                .iter()
                .find(|tool| tool["function"]["name"] == "StraviaRead")
                .expect("basic reader is injected")["function"]["description"]
                .as_str()
                .expect("description")
                .to_owned();
            assert_eq!(
                description.contains("stravia://history/"),
                history,
                "{description}"
            );
            let followup: serde_json::Value =
                serde_json::from_str(requests[1].split_once("\r\n\r\n").unwrap().1).unwrap();
            let result = followup["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|message| message["tool_call_id"] == "history-root")
                .expect("history read result")["content"]
                .as_str()
                .expect("tool result text")
                .to_owned();
            if history {
                let result: serde_json::Value = serde_json::from_str(&result).expect("page JSON");
                assert_eq!(result["read_path"], "stravia://history/");
                assert!(
                    result["content"]
                        .as_str()
                        .expect("listing")
                        .starts_with("# stravia://history/ · "),
                    "{result}"
                );
            } else {
                assert!(
                    result.contains(
                        "History Recall is only available through this API key's transparent injection."
                    ),
                    "{result}"
                );
            }
        }
        close_test_gateway(gateway, data_dir).await;
    }
}
