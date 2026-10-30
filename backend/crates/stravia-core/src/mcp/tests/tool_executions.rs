use super::*;
use crate::mcp::read::ReadTool;
use std::{future::Future, pin::Pin};
use stravia_runtime_contract::{
    CancellationToken,
    hook::{PlatformTool, PlatformToolError, StraviaReadDomain, ToolExecutionContext, ToolId},
};

struct ControlledSearch {
    calls: Arc<AtomicUsize>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    fail: bool,
}

#[async_trait]
impl PlatformTool for ControlledSearch {
    fn id(&self) -> ToolId {
        ToolId::new("controlled-search")
    }
    fn external_name(&self) -> &str {
        "controlled_search"
    }
    fn description(&self) -> Option<&str> {
        Some("Controlled sourced search report")
    }
    fn parameters(&self) -> Value {
        json!({"type":"object"})
    }
    fn read_domain(&self) -> Option<StraviaReadDomain> {
        Some(StraviaReadDomain::Query)
    }
    async fn execute(
        &self,
        arguments: Value,
        _context: ToolExecutionContext,
    ) -> Result<Value, PlatformToolError> {
        assert_eq!(arguments["query"], "controlled evidence");
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        self.release.notified().await;
        if self.fail {
            return Err(PlatformToolError::new(
                "controlled search failed after acceptance",
            ));
        }
        Ok(json!({
            "path":"stravia://turns/abcdefghijklmnopqrstuvwxyzab",
            "completion":"complete",
            "report":{
                "answer":"The controlled evidence confirms the result.",
                "sources":[{"path":"stravia://turns/abcdefghijklmnopqrstuvwxyzab/sources/1","url":"https://example.org/evidence","title":"Controlled evidence"}],
                "limitations":["Only the controlled source was inspected."]
            }
        }))
    }
}

async fn fixture(fail: bool) -> (TestApp, ReadTool, Arc<ControlledSearch>) {
    let app = test_app().await;
    stravia_web_search::SettingsWebSearchConfigStore::new(Arc::new(
        crate::web_search::host::SearchSettings(app.gateway.storage.clone()),
    ))
    .save(&stravia_web_search::WebSearchConfig {
        revision: 1,
        enabled: true,
        backend: None,
        max_turns: 12,
        total_time_seconds: 600,
        updated_at: "2026-10-17T00:00:00Z".into(),
    })
    .await
    .unwrap();
    let upstream = Arc::new(ControlledSearch {
        calls: Arc::new(AtomicUsize::new(0)),
        entered: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
        fail,
    });
    let mut contributions: Vec<Arc<dyn PlatformTool>> = vec![upstream.clone()];
    let reader = ReadTool::new(&app.gateway, &mut contributions).unwrap();
    (app, reader, upstream)
}

async fn read(reader: &ReadTool, owner: &str, path: &str) -> McpToolOutput {
    McpTool::call(reader, json!({"path":path}), &McpContext::new(owner.into()))
        .await
        .unwrap()
}

async fn background(reader: &ReadTool, owner: &str, upstream: &ControlledSearch) -> String {
    let reader = reader.clone();
    let owner = owner.to_owned();
    let call =
        tokio::spawn(async move { read(&reader, &owner, "search://controlled%20evidence").await });
    upstream.entered.notified().await;
    // Acceptance is the barrier: setup and upstream admission run on real time.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(60)).await;
    // Handoff persists through SQL; never await that I/O with auto-advance enabled.
    tokio::time::resume();
    let output = call.await.unwrap();
    assert!(!output.is_error, "{:?}", output.structured_content);
    assert_eq!(output.structured_content["status"], "running");
    let path = output.structured_content["path"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(path.starts_with("stravia://tool-executions/"));
    path
}

async fn assert_pending<F: Future>(future: std::pin::Pin<&mut F>, phase: &str)
where
    F::Output: std::fmt::Debug,
{
    let mut future = future;
    std::future::poll_fn(|cx| {
        let result = future.as_mut().poll(cx);
        assert!(result.is_pending(), "{phase}: {result:?}");
        std::task::Poll::Ready(())
    })
    .await;
}

async fn registered_query<F: Future>(future: F) -> Pin<Box<impl Future<Output = F::Output>>>
where
    F::Output: std::fmt::Debug,
{
    let registered = Arc::new(tokio::sync::Notify::new());
    let mut future =
        Box::pin(crate::tool_execution::QUERY_WAITING.scope(registered.clone(), future));
    tokio::select! {
        output = future.as_mut() => panic!("query completed before waiting: {output:?}"),
        _ = registered.notified() => {}
    }
    future
}

#[tokio::test]
async fn search_queries_wait_independently_and_preserve_the_original_report() {
    let (app, reader, upstream) = fixture(false).await;
    let path = background(&reader, &app.key_id, &upstream).await;
    let immediate = read(&reader, &app.key_id, &path).await;
    assert_eq!(immediate.structured_content["status"], "running");
    let zero = read(&reader, &app.key_id, &format!("{path}?wait=0")).await;
    assert_eq!(zero.structured_content, immediate.structured_content);

    let short_path = format!("{path}?wait=7");
    let short = registered_query(read(&reader, &app.key_id, &short_path)).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(7)).await;
    tokio::time::resume();
    let expired = short.await;
    assert!(!expired.is_error);
    assert_eq!(expired.structured_content, immediate.structured_content);

    let cancellation = CancellationToken::new();
    let cancelled_context = McpContext::new(app.key_id.clone()).for_call(
        cancellation.clone(),
        std::time::Instant::now() + Duration::from_secs(900),
    );
    let cancelled = registered_query(McpTool::call(
        &reader,
        json!({"path":format!("{path}?wait=120")}),
        &cancelled_context,
    ))
    .await;
    let long_path = format!("{path}?wait=120");
    let mut survivor = registered_query(read(&reader, &app.key_id, &long_path)).await;
    cancellation.cancel();
    assert!(cancelled.await.unwrap().is_error);
    // A >60-second query must not itself hand off or reset the original deadline.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(61)).await;
    tokio::time::resume();
    assert_pending(survivor.as_mut(), "surviving query after time advance").await;
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 1);
    upstream.release.notify_one();
    let completed = survivor.await;
    assert!(!completed.is_error, "{:?}", completed.structured_content);
    assert_eq!(completed.structured_content["path"], path);
    assert_eq!(completed.structured_content["status"], "completed");
    let result = &completed.structured_content["result"];
    assert!(result.get("report").is_some());
    assert!(
        result.get("status").is_none(),
        "no nested execution wrapper"
    );
    let repeated = read(&reader, &app.key_id, &path).await;
    assert_eq!(repeated.structured_content, completed.structured_content);
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn failed_background_search_is_a_successful_status_read_and_owner_is_not_disclosed() {
    let (app, reader, upstream) = fixture(true).await;
    let path = background(&reader, &app.key_id, &upstream).await;
    upstream.release.notify_one();
    let completed = read(&reader, &app.key_id, &format!("{path}?wait=120")).await;
    assert!(!completed.is_error);
    assert_eq!(completed.structured_content["status"], "failed");
    assert!(
        completed
            .structured_content
            .get("result")
            .is_none_or(Value::is_null)
    );
    assert!(
        completed.structured_content["error"]
            .to_string()
            .contains("controlled search failed after acceptance")
    );
    let other = app
        .gateway
        .admin()
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "other reader".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: true,
            transparent_injection_enabled: false,
            inject_web_search: false,
            inject_media_generation: false,
            model_ids: vec![],
            inject_media_understanding: false,
            spend_limit: None,
        })
        .await
        .unwrap();
    let denied = read(&reader, &other.id, &path).await;
    let missing = read(
        &reader,
        &other.id,
        "stravia://tool-executions/abcdefghijklmnopqrstuvwxyzab",
    )
    .await;
    assert!(denied.is_error);
    assert!(missing.is_error);
    assert_eq!(denied.structured_content, missing.structured_content);
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 1);
}
