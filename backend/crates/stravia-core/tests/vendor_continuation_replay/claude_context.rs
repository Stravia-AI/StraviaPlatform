use super::*;

async fn send(harness: &Harness, messages: Vec<Value>, streaming: bool) -> (StatusCode, String) {
    let body = json!({"model":ROUTE,"max_tokens":256,"stream":streaming,"messages":messages,
        "tools":[{"name":"lookup","description":"fixture lookup","input_schema":{"type":"object","properties":{}}}]});
    let response = create_router(harness.gateway.clone())
        .oneshot(
            Request::post("/v1/messages")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {}", harness.token))
                .header("anthropic-version", "2023-06-01")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .expect("gateway response");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("gateway response body");
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

fn tool_message() -> (u16, Value) {
    (
        200,
        json!({"id":"msg_tool","type":"message","role":"assistant","model":MODEL,
        "content":[{"type":"tool_use","id":"toolu_fixture","name":"_lookup","input":{}}],
        "stop_reason":"tool_use","stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":5}}),
    )
}

fn assert_context(harness: &Harness, index: usize, tools: Value, prompt: u64) {
    let headers = harness.upstream.headers.lock();
    let raw = headers[index]
        .get("x-claude-code-sandy-plover")
        .expect("Claude tool history header")
        .to_str()
        .unwrap();
    assert_eq!(serde_json::from_str::<Value>(raw).unwrap(), tools);
    let bodies = harness.upstream.bodies.lock();
    let billing = bodies[index]["system"][0]["text"]
        .as_str()
        .expect("Claude billing system block");
    assert!(
        billing.contains(&format!("cc_prompt_index={prompt};")),
        "{billing}"
    );
}

#[tokio::test]
async fn delivered_client_tool_handoff_is_inherited_for_buffered_and_live_responses()
-> anyhow::Result<()> {
    for streaming in [false, true] {
        let harness = Harness::start().await?;
        harness.script([
            tool_message(),
            message("msg_result"),
            error(400, None),
            message("msg_after_failure"),
            message("msg_fork"),
        ]);
        let first = json!({"role":"user","content":"Find the fixture."});
        let (status, response) = send(&harness, vec![first.clone()], streaming).await;
        assert_eq!(status, StatusCode::OK, "{response}");
        assert!(response.contains("lookup"), "{response}");
        let assistant = json!({"role":"assistant","content":[{"type":"tool_use","id":"toolu_fixture","name":"lookup","input":{}}]});
        let result = json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_fixture","content":"found"}]});
        let history = vec![first, assistant, result];
        let (status, response) = send(&harness, history.clone(), false).await;
        assert_eq!(status, StatusCode::OK, "{response}");
        let mut failed_history = history.clone();
        failed_history.push(json!({"role":"assistant","content":[{"type":"text","text":"READY"}]}));
        failed_history.push(json!({"role":"user","content":"failed prompt"}));
        let (status, _) = send(&harness, failed_history.clone(), false).await;
        assert!(!status.is_success());
        tokio::time::sleep(Duration::from_millis(COOLDOWN_MS as u64 + 150)).await;
        failed_history.push(json!({"role":"user","content":"next prompt"}));
        let (status, response) = send(&harness, failed_history, false).await;
        assert_eq!(status, StatusCode::OK, "{response}");
        let mut fork = history;
        fork.push(json!({"role":"assistant","content":[{"type":"text","text":"READY"}]}));
        fork.push(json!({"role":"user","content":"fork prompt"}));
        let (status, response) = send(&harness, fork, false).await;
        assert_eq!(status, StatusCode::OK, "{response}");
        assert_context(&harness, 0, json!([]), 1);
        // The ledger records the wire-shaped history name as well as the
        // resolved client handoff name; these are distinct provenance sources.
        assert_context(
            &harness,
            1,
            json!([{"name":"_lookup"},{"name":"lookup"}]),
            1,
        );
        assert_context(
            &harness,
            2,
            json!([{"name":"_lookup"},{"name":"lookup"}]),
            2,
        );
        assert_context(
            &harness,
            3,
            json!([{"name":"_lookup"},{"name":"lookup"}]),
            3,
        );
        assert_context(
            &harness,
            4,
            json!([{"name":"_lookup"},{"name":"lookup"}]),
            2,
        );
        harness.gateway.shutdown().await;
    }
    Ok(())
}

struct ToolRespondHook(Arc<std::sync::atomic::AtomicBool>);
struct ToolRespondSession(Arc<std::sync::atomic::AtomicBool>);

impl stravia_runtime_contract::hook::Hook for ToolRespondHook {
    fn descriptor(&self) -> stravia_runtime_contract::hook::HookDescriptor {
        stravia_runtime_contract::hook::HookDescriptor {
            event_kinds: vec![stravia_runtime_contract::hook::EventKind::Request],
            ..stravia_runtime_contract::hook::HookDescriptor::all("fixture-client-tool-response")
        }
    }
    fn create_session(
        &self,
        _: &stravia_runtime_contract::hook::SessionContext,
    ) -> Box<dyn stravia_runtime_contract::hook::HookSession> {
        Box::new(ToolRespondSession(self.0.clone()))
    }
}

#[async_trait::async_trait]
impl stravia_runtime_contract::hook::HookSession for ToolRespondSession {
    async fn handle(
        &mut self,
        event: stravia_runtime_contract::hook::HookEvent<'_>,
    ) -> Result<stravia_runtime_contract::hook::ActionBatch, String> {
        let stravia_runtime_contract::hook::HookEvent::Request { current, .. } = event else {
            return Ok(stravia_runtime_contract::hook::ActionBatch::default());
        };
        if self.0.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Ok(stravia_runtime_contract::hook::ActionBatch::default());
        }
        let mut response =
            stravia_runtime_contract::protocol::ir::AiResponse::new("hook-tool", &current.model);
        response.extend_tool_calls(vec![stravia_runtime_contract::protocol::ir::ToolCall {
            id: "toolu_fixture".into(),
            name: "lookup".into(),
            arguments: "{}".into(),
        }]);
        response.stop_reason = Some("tool_calls".into());
        Ok(stravia_runtime_contract::hook::ActionBatch::one(
            stravia_runtime_contract::hook::HookAction::Respond(Box::new(response)),
        ))
    }
}

#[tokio::test]
async fn hook_synthesized_client_tool_handoff_is_inherited_by_claude() -> anyhow::Result<()> {
    let hook = ToolRespondHook(Arc::new(std::sync::atomic::AtomicBool::new(false)));
    let harness = Harness::start_with_hook(Some(Arc::new(hook))).await?;
    harness.script([message("msg_after_hook")]);
    let first = json!({"role":"user","content":"Use the hook tool."});
    let (status, response) = send(&harness, vec![first.clone()], false).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert!(response.contains("lookup"), "{response}");
    let messages = vec![
        first,
        json!({"role":"assistant","content":[{"type":"tool_use","id":"toolu_fixture","name":"lookup","input":{}}]}),
        json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_fixture","content":"done"}]}),
    ];
    let (status, response) = send(&harness, messages, false).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    // The hook did not invoke the guest; the subsequent client transcript
    // supplies the wire-shaped name, not a decoded guest handoff.
    assert_context(&harness, 0, json!([{"name":"_lookup"}]), 1);
    harness.gateway.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn undeclared_handoff_is_not_recorded_as_a_resolved_client_tool() -> anyhow::Result<()> {
    let harness = Harness::start().await?;
    let mut unknown = tool_message();
    unknown.1["content"][0]["name"] = json!("_not_registered");
    harness.script([unknown, message("msg_after_unknown")]);
    let first = json!({"role":"user","content":"Try an unknown tool."});
    let (status, response) = send(&harness, vec![first.clone()], false).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let messages = vec![
        first,
        json!({"role":"assistant","content":[{"type":"tool_use","id":"toolu_fixture","name":"not_registered","input":{}}]}),
        json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_fixture","content":"done"}]}),
    ];
    let (status, response) = send(&harness, messages, false).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_context(&harness, 0, json!([]), 1);
    // The submitted transcript is still projected under its wire name, but
    // the undeclared response must not add a resolved client-tool name.
    assert_context(&harness, 1, json!([{"name":"_not_registered"}]), 1);
    harness.gateway.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn invalid_tool_name_in_client_history_is_projected_safely() -> anyhow::Result<()> {
    let harness = Harness::start().await?;
    harness.script([message("msg_history")]);
    let messages = vec![
        json!({"role":"user","content":"Search first."}),
        json!({"role":"assistant","content":[
            {"type":"tool_use","id":"toolu_unknown","name":"name with spaces","input":{}}
        ]}),
        json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_unknown","content":"done"}]}),
    ];
    let (status, response) = send(&harness, messages, false).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_context(
        &harness,
        0,
        json!([{"name":"unreadable-tools-used-before-summary"}]),
        1,
    );
    harness.gateway.shutdown().await;
    Ok(())
}
