use super::*;
use crate::db::models::{
    CreateApiKey, CreateRoute, CreateTarget, RoutePurpose, TransparentInjection,
    TransparentInjectionSet, UpdateRoute,
};
use stravia_runtime_contract::{
    CancellationToken, Principal,
    hook::{PlatformTool, ReadExposureScope, ToolExecutionContext},
    protocol::ir::ContentBlock,
    thinking::ThinkingLevel,
};
use stravia_web_search::{WebSearchBackendDraft, WebSearchConfig, WebSearchResult};

fn deepseek_sse_response() -> LocalResponse {
    let sse = concat!(
        ": keep-alive\n\n",
        "event: response.output_item.done\n",
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"web_search_call\",\"id\":\"srvtoolu_s_00\",\"status\":\"completed\",\"kind\":\"search\",\"action\":{\"type\":\"search\",\"queries\":[\"DeepSeek Flash\"],\"sources\":[{\"type\":\"url\",\"url\":\"https://93.184.216.34/deepseek\",\"title\":\"DeepSeek Reference\",\"snippet\":\"Official DeepSeek reference snippet\",\"published_at\":\"2026-01-02T03:04:05+00:00\"}]}}}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"web_search_call\",\"id\":\"srvtoolu_s_00\",\"status\":\"completed\",\"kind\":\"search\",\"action\":{\"type\":\"search\",\"queries\":[\"DeepSeek Flash\"],\"sources\":[{\"type\":\"url\",\"url\":\"https://93.184.216.34/deepseek\",\"title\":\"DeepSeek Reference\",\"snippet\":\"Official DeepSeek reference snippet\",\"published_at\":\"2026-01-02T03:04:05+00:00\"}]}},{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"DeepSeek grounded answer.\",\"annotations\":[{\"type\":\"url_citation\",\"url\":\"https://93.184.216.34/deepseek\",\"title\":\"\",\"start_index\":0,\"end_index\":24}]}]}],\"usage\":{\"input_tokens\":120,\"input_tokens_details\":{\"cached_tokens\":40},\"output_tokens\":25,\"output_tokens_details\":{\"reasoning_tokens\":8},\"total_tokens\":145,\"server_tool_use\":{\"tool_search\":1,\"tool_fetch\":0,\"tool_image_search\":0}}}}\n\n"
    );
    LocalResponse {
        status: "200 OK",
        content_type: "text/event-stream",
        body: sse.as_bytes().to_vec(),
        release: None,
    }
}

#[tokio::test]
async fn deepseek_search_follows_bound_flash_thinking_level_through_stravia_read()
-> anyhow::Result<()> {
    let mut server = local_tls_server(4, |request| match request.path.as_str() {
        "/models" => LocalResponse::json(json!({
            "data": [
                {"id": "deepseek-flash"},
                {"id": "deepseek-chat"}
            ]
        })),
        "/responses" => deepseek_sse_response(),
        path => panic!("unexpected upstream request: {path}"),
    })
    .await?;

    let directory = tempfile::tempdir()?;
    let pool = crate::db::init_pool(directory.path()).await?;
    crate::migrations::migrate_sqlite(&pool, None).await?;
    let (_gateway_directory, gateway) = gateway_for_storage(
        &server,
        &[],
        Arc::new(crate::storage::SqliteStorage::from_pool(pool)),
    )
    .await?;

    let provider = gateway
        .admin()
        .create_provider(CreateProvider {
            name: Some("DeepSeek TLS Search".into()),
            source: ProviderSourceInput::Custom {
                vendor: "deepseek".into(),
                channel: "default".into(),
                protocol: Some("openai-compatible".into()),
                base_url: "https://api.deepseek.com".into(),
                models_source: Some("https://api.deepseek.com/models".into()),
                static_models: None,
            },
            credential: ProviderCredentialInput::ApiKey {
                value: "deepseek-search-test-secret".into(),
            },
            vendor_options: Default::default(),
            use_proxy: false,
        })
        .await?;

    let sync = gateway.admin().sync_provider_models(&provider.id).await?;
    assert_eq!(
        sync.outcome,
        crate::provider_models::ModelChangeOutcome::Success
    );

    // 非 flash 模型不能绑定为 WebSearch 路由目标。
    let non_flash_err = gateway
        .admin()
        .create_model(CreateRoute {
            model_id: "deepseek-chat-search".into(),
            display_name: None,
            purpose: RoutePurpose::WebSearch,
            balance: None,
            targets: vec![CreateTarget {
                provider_id: provider.id.clone(),
                model: "deepseek-chat".into(),
                enabled: true,
                priority: Some(0),
                first_token_timeout_ms: None,
                target_retry_budget: Some(0),
                target_cooldown_ms: Some(0),
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: Some(ThinkingLevel::Off),
            billing: None,
        })
        .await
        .expect_err("deepseek-chat must not be eligible for WebSearch");
    assert!(
        non_flash_err.to_string().contains("search")
            || non_flash_err.to_string().contains("purpose"),
        "unexpected error: {non_flash_err}"
    );

    let route = gateway
        .admin()
        .create_model(CreateRoute {
            model_id: "deepseek-flash-search".into(),
            display_name: None,
            purpose: RoutePurpose::WebSearch,
            balance: None,
            targets: vec![CreateTarget {
                provider_id: provider.id.clone(),
                model: "deepseek-flash".into(),
                enabled: true,
                priority: Some(0),
                first_token_timeout_ms: None,
                target_retry_budget: Some(0),
                target_cooldown_ms: Some(0),
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: Some(ThinkingLevel::Off),
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
            name: "deepseek search acceptance".into(),
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

    let reader = crate::mcp::read::ReadTool::new(&gateway);

    let run_search = |request_id: &str| {
        reader.execute(
            json!({"path": "stravia://search", "body": {"question": "What is DeepSeek Flash?"}}),
            ToolExecutionContext {
                request_id: request_id.into(),
                run_id: request_id.into(),
                interaction_id: None,
                principal: Principal::new(key.id.clone()),
                read_scope: ReadExposureScope::FULL,
                cancellation: CancellationToken::new(),
                progress: None,
            },
        )
    };

    // 1. default_thinking_level = off -> 关闭思考 (reasoning.effort = "none")
    let output_off = run_search("deepseek-search-off").await?;
    assert!(!output_off.is_error, "{output_off:?}");
    let [ContentBlock::Unknown { raw }] = output_off.content.as_slice() else {
        panic!("unexpected StraviaRead output: {:?}", output_off.content)
    };
    let result: WebSearchResult = serde_json::from_value(raw.clone())?;
    assert_eq!(
        result.completion,
        stravia_web_search::SearchCompletion::Complete
    );
    assert_eq!(result.report.sources.len(), 1);
    let source = &result.report.sources[0];
    assert_eq!(source.url, "https://93.184.216.34/deepseek");
    assert_eq!(source.title.as_deref(), Some("DeepSeek Reference"));
    assert_eq!(
        result.report.answer,
        format!("DeepSeek grounded answer [{}].", source.path)
    );

    // 2. default_thinking_level = high -> 开启思考 (reasoning.effort = "high")
    gateway
        .admin()
        .update_model(
            &route.model_id,
            UpdateRoute {
                model_id: None,
                display_name: None,
                purpose: None,
                balance: None,
                is_enabled: None,
                targets: None,
                default_thinking_level: Some(Some(ThinkingLevel::High)),
                billing: None,
            },
        )
        .await?;
    let output_high = run_search("deepseek-search-high").await?;
    assert!(!output_high.is_error, "{output_high:?}");

    // 3. default_thinking_level = None -> 省略 reasoning（供应商默认）
    gateway
        .admin()
        .update_model(
            &route.model_id,
            UpdateRoute {
                model_id: None,
                display_name: None,
                purpose: None,
                balance: None,
                is_enabled: None,
                targets: None,
                default_thinking_level: Some(None),
                billing: None,
            },
        )
        .await?;
    let output_default = run_search("deepseek-search-default").await?;
    assert!(!output_default.is_error, "{output_default:?}");

    let discovery = server.requests.recv().await.expect("discovery request");
    assert_eq!(discovery.path, "/models");

    let req_off = server.requests.recv().await.expect("search off request");
    assert_eq!(req_off.method, "POST");
    assert_eq!(req_off.path, "/responses");
    assert_eq!(
        req_off.headers.get("authorization").map(String::as_str),
        Some("Bearer deepseek-search-test-secret")
    );
    let body_off: Value = serde_json::from_slice(&req_off.body)?;
    assert_eq!(body_off["model"], "deepseek-flash");
    assert_eq!(body_off["tools"], json!([{"type": "deepseek_web_search"}]));
    assert_eq!(body_off["reasoning"], json!({"effort": "none"}));

    let req_high = server.requests.recv().await.expect("search high request");
    let body_high: Value = serde_json::from_slice(&req_high.body)?;
    assert_eq!(body_high["reasoning"], json!({"effort": "high"}));

    let req_default = server
        .requests
        .recv()
        .await
        .expect("search default request");
    let body_default: Value = serde_json::from_slice(&req_default.body)?;
    assert!(body_default.get("reasoning").is_none());

    server.task.await??;
    Ok(())
}

#[tokio::test]
#[ignore = "live upstream test; requires DEEPSEEK_API_KEY"]
async fn deepseek_search_live_upstream_smoke() -> anyhow::Result<()> {
    let api_key = std::env::var("DEEPSEEK_API_KEY")
        .map_err(|_| anyhow::anyhow!("DEEPSEEK_API_KEY must be set"))?;
    let directory = tempfile::tempdir()?;
    let gateway = Gateway::new(GatewayConfig {
        data_dir: directory.path().to_owned(),
        ..GatewayConfig::default()
    })
    .await?;

    let provider = gateway
        .admin()
        .create_provider(CreateProvider {
            name: Some("DeepSeek Live Search".into()),
            source: ProviderSourceInput::Custom {
                vendor: "deepseek".into(),
                channel: "default".into(),
                protocol: Some("openai-compatible".into()),
                base_url: "https://api.deepseek.com".into(),
                models_source: Some("https://api.deepseek.com/models".into()),
                static_models: None,
            },
            credential: ProviderCredentialInput::ApiKey { value: api_key },
            vendor_options: Default::default(),
            use_proxy: false,
        })
        .await?;

    let sync = gateway.admin().sync_provider_models(&provider.id).await?;
    assert_eq!(
        sync.outcome,
        crate::provider_models::ModelChangeOutcome::Success
    );

    let route = gateway
        .admin()
        .create_model(CreateRoute {
            model_id: "deepseek-live-search".into(),
            display_name: None,
            purpose: RoutePurpose::WebSearch,
            balance: None,
            targets: vec![CreateTarget {
                provider_id: provider.id.clone(),
                model: "deepseek-flash".into(),
                enabled: true,
                priority: Some(0),
                first_token_timeout_ms: None,
                target_retry_budget: Some(0),
                target_cooldown_ms: Some(0),
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: Some(ThinkingLevel::Off),
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
            name: "deepseek live search".into(),
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

    let reader = crate::mcp::read::ReadTool::new(&gateway);

    let run = |id: &str, question: &str| {
        reader.execute(
            json!({"path": "stravia://search", "body": {"question": question}}),
            ToolExecutionContext {
                request_id: id.into(),
                run_id: id.into(),
                interaction_id: None,
                principal: Principal::new(key.id.clone()),
                read_scope: ReadExposureScope::FULL,
                cancellation: CancellationToken::new(),
                progress: None,
            },
        )
    };

    // 1. thinking = off (联网搜索)
    let out_off = run("live-off", "Rust 最新稳定版本是多少？").await?;
    assert!(!out_off.is_error, "{out_off:?}");
    let [ContentBlock::Unknown { raw: raw_off }] = out_off.content.as_slice() else {
        panic!("unexpected output: {:?}", out_off.content);
    };
    let res_off: WebSearchResult = serde_json::from_value(raw_off.clone())?;
    assert_eq!(
        res_off.completion,
        stravia_web_search::SearchCompletion::Complete
    );
    assert!(!res_off.report.sources.is_empty());
    println!(
        "[live off] sources={} answer_len={} first_url={}",
        res_off.report.sources.len(),
        res_off.report.answer.len(),
        res_off.report.sources[0].url
    );

    // 2. thinking = low (深度思考搜索)
    gateway
        .admin()
        .update_model(
            &route.model_id,
            UpdateRoute {
                model_id: None,
                display_name: None,
                purpose: None,
                balance: None,
                is_enabled: None,
                targets: None,
                default_thinking_level: Some(Some(ThinkingLevel::Low)),
                billing: None,
            },
        )
        .await?;
    let out_low = run("live-low", "What is the latest population of Iceland?").await?;
    assert!(!out_low.is_error, "{out_low:?}");
    let [ContentBlock::Unknown { raw: raw_low }] = out_low.content.as_slice() else {
        panic!("unexpected output: {:?}", out_low.content);
    };
    let res_low: WebSearchResult = serde_json::from_value(raw_low.clone())?;
    assert_eq!(
        res_low.completion,
        stravia_web_search::SearchCompletion::Complete
    );
    assert!(!res_low.report.sources.is_empty());
    println!(
        "[live low] sources={} answer_len={} first_url={}",
        res_low.report.sources.len(),
        res_low.report.answer.len(),
        res_low.report.sources[0].url
    );

    Ok(())
}
