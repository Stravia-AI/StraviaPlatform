use super::*;
use crate::db::models::{
    CreateApiKey, CreateRoute, CreateTarget, RoutePurpose, TransparentInjection,
    TransparentInjectionSet,
};
use stravia_runtime_contract::{
    CancellationToken, Principal,
    hook::{PlatformTool, ReadExposureScope, ToolExecutionContext},
    protocol::ir::ContentBlock,
};
use stravia_web_search::{WebSearchBackendDraft, WebSearchConfig, WebSearchResult};

#[tokio::test]
async fn antigravity_discovered_search_route_through_stravia_read() -> anyhow::Result<()> {
    let mut server = local_tls_server(2, |request| {
        match request.path.as_str() {
            "/v1internal:fetchAvailableModels" => LocalResponse::json(json!({
                "defaultAgentModelId": "ordinary-infer",
                "agentModelSorts": [{"groups": [{"modelIds": ["ordinary-infer"]}]}],
                "webSearchModelIds": ["search-first", "selected-search"],
                "models": {
                    "ordinary-infer": {"displayName": "Ordinary inference"},
                    "search-first": {"displayName": "First search"},
                    "selected-search": {"displayName": "Selected search", "apiProvider": 3}
                }
            })),
            "/v1internal:generateContent" => LocalResponse::json(json!({"response": {
                "candidates": [{
                    "finishReason": "STOP",
                    "content": {"role": "model", "parts": [{"text": "Grounded answer"}]},
                    "groundingMetadata": {
                        "groundingChunks": [{"web": {"uri": "https://93.184.216.34/reference", "title": "Reference title"}}],
                        "groundingSupports": [{"segment": {"partIndex": 0, "startIndex": 0, "endIndex": 15, "text": "Grounded answer"}, "groundingChunkIndices": [0]}]
                    }
                }]
            }})),
            path => panic!("unexpected upstream request: {path}"),
        }
    }).await?;
    let directory = tempfile::tempdir()?;
    let pool = crate::db::init_pool(directory.path()).await?;
    crate::migrations::migrate_sqlite(&pool, None).await?;
    let (_gateway_directory, gateway) = gateway_for_storage(
        &server,
        &["antigravity"],
        Arc::new(crate::storage::SqliteStorage::from_pool(pool)),
    )
    .await?;
    let provider = synthetic_oauth_provider(&gateway, "antigravity", false).await?;
    let sync = gateway.admin().sync_provider_models(&provider.id).await?;
    assert_eq!(
        sync.outcome,
        crate::provider_models::ModelChangeOutcome::Success
    );
    let inventory = gateway.admin().list_provider_models(&provider.id).await?;
    for id in ["ordinary-infer", "search-first", "selected-search"] {
        assert!(
            inventory.models.iter().any(|model| model.id == id),
            "missing {id}"
        );
    }
    let route = gateway
        .admin()
        .create_model(CreateRoute {
            model_id: "antigravity-public-search".into(),
            display_name: None,
            purpose: RoutePurpose::WebSearch,
            balance: None,
            targets: vec![CreateTarget {
                provider_id: provider.id.clone(),
                model: "selected-search".into(),
                enabled: true,
                priority: Some(0),
                first_token_timeout_ms: None,
                target_retry_budget: Some(0),
                target_cooldown_ms: Some(0),
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
            billing: None,
        })
        .await?;
    let config = gateway.admin().get_web_search_config().await?.config;
    gateway
        .admin()
        .update_web_search_config(WebSearchConfig {
            revision: config.revision,
            enabled: true,
            backend: Some(WebSearchBackendDraft::External {
                route_id: Some(route.model_id.clone().into()),
            }),
            max_turns: config.max_turns,
            total_time_seconds: config.total_time_seconds,
            updated_at: config.updated_at,
        })
        .await?;
    let key = gateway
        .admin()
        .create_api_key(CreateApiKey {
            key: None,
            name: "search acceptance".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: true,
            transparent_injections: TransparentInjectionSet::from_items([
                TransparentInjection::WebSearch,
            ]),
            model_ids: vec![route.id.into()],
            spend_limit: None,
        })
        .await?;
    let (_, mut tools) = stravia_web_search::builtin_extensions(Arc::new(
        crate::web_search::host::SearchHost(gateway.clone()),
    ));
    let reader = crate::mcp::read::ReadTool::new(&gateway, &mut tools)?;
    let output = reader.execute_result(
        json!({"path": "stravia://search", "body": {"question": "What is the grounded answer?"}}),
        ToolExecutionContext {
            request_id: "search-acceptance".into(), run_id: "search-acceptance".into(),
            interaction_id: None, principal: Principal::new(key.id), read_scope: ReadExposureScope::FULL,
            cancellation: CancellationToken::new(), progress: None,
        },
    ).await?;
    assert!(!output.is_error, "{output:?}");
    let [ContentBlock::Unknown { raw }] = output.content.as_slice() else {
        panic!("unexpected StraviaRead output: {:?}", output.content)
    };
    let result: WebSearchResult = serde_json::from_value(raw.clone())?;
    assert_eq!(
        result.completion,
        stravia_web_search::SearchCompletion::Complete
    );
    assert_eq!(result.report.sources.len(), 1);
    let source = &result.report.sources[0];
    assert_eq!(
        source.path,
        format!("{}/sources/1", result.turn_id.reference())
    );
    assert_eq!(source.url, "https://93.184.216.34/reference");
    assert_eq!(source.title.as_deref(), Some("Reference title"));
    assert_eq!(
        result.report.answer,
        format!("Grounded answer[{}]", source.path)
    );
    assert!(result.report.limitations.is_empty());

    let discovery = server.requests.recv().await.expect("discovery request");
    let search = server.requests.recv().await.expect("search request");
    assert_eq!(discovery.path, "/v1internal:fetchAvailableModels");
    assert_eq!(search.path, "/v1internal:generateContent");
    assert_eq!(search.method, "POST");
    let envelope: Value = serde_json::from_slice(&search.body)?;
    assert_eq!(envelope["model"], "selected-search");
    assert_eq!(envelope["requestType"], "web_search");
    assert_eq!(
        envelope["request"]["contents"][0]["parts"][0]["text"],
        "What is the grounded answer?"
    );
    assert_eq!(
        envelope["request"]["tools"],
        json!([{"enterpriseWebSearch": {}}])
    );
    server.task.await??;
    Ok(())
}
