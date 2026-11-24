use std::sync::Arc;

use serde_json::json;
use stravia_runtime_contract::protocol::ir::AiResponse;
use stravia_runtime_contract::thinking::ThinkingLevel;

use super::{
    ConversationTitleInput, PlatformLlmStatus, SETTINGS_KEY, UpdatePlatformLlmConfig,
    service::sanitize_title,
};
use crate::Gateway;
use crate::config::GatewayConfig;
use crate::db::models::{
    CreateProviderRecord, CreateRoute, CreateTarget, RouteConfig, RoutePurpose, UpdateRoute,
};
use crate::model_turn::InMemoryModelTurnExecutor;
use crate::provider_models::CreateManualProviderModel;

const TEST_MODEL: &str = "upstream-model";

async fn test_gateway() -> anyhow::Result<(Gateway, tempfile::TempDir, String)> {
    let data_dir = tempfile::tempdir()?;
    let gateway = Gateway::from_storage(
        GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..GatewayConfig::default()
        },
        Arc::new(crate::storage::MemoryStorage::new(
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )),
    )
    .await?;

    let provider = gateway
        .storage
        .providers()
        .create(CreateProviderRecord {
            name: "Local Test Provider".into(),
            vendor: Some("custom".into()),
            protocol: "openai-compatible".into(),
            base_url: "http://127.0.0.1:9".into(),
            preset_key: None,
            channel: Some("default".into()),
            models_source: None,
            static_models: None,
            api_key: "test-key".into(),
            adapter_credentials: "{}".into(),
            vendor_options: "{}".into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
        })
        .await?;

    gateway
        .admin()
        .create_manual_provider_model(
            &provider.id,
            TEST_MODEL,
            CreateManualProviderModel {
                metadata: json!({
                    "id": TEST_MODEL,
                    "name": "Local test model",
                    "capabilities": ["infer", "media_image"],
                    "modalities": {"input": ["text"], "output": ["text", "image"]}
                }),
                template_id: None,
            },
        )
        .await?;

    Ok((gateway, data_dir, provider.id.into()))
}

async fn create_route(
    gateway: &Gateway,
    provider_id: &str,
    model_id: &str,
    purpose: RoutePurpose,
    is_enabled: bool,
) -> anyhow::Result<RouteConfig> {
    let route = gateway
        .admin()
        .create_model(CreateRoute {
            purpose,
            model_id: model_id.into(),
            display_name: Some(format!("Display {model_id}")),
            balance: None,
            targets: vec![CreateTarget {
                provider_id: provider_id.into(),
                model: TEST_MODEL.into(),
                enabled: true,
                priority: Some(0),
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
            billing: None,
        })
        .await?;
    if !is_enabled {
        return gateway
            .admin()
            .update_model(
                route.model_id.as_str(),
                UpdateRoute {
                    is_enabled: Some(false),
                    ..Default::default()
                },
            )
            .await;
    }
    Ok(route)
}

#[tokio::test]
async fn config_lifecycle_validation_rename_and_stale_binding() -> anyhow::Result<()> {
    let (gateway, _dir, provider_id) = test_gateway().await?;
    let admin = gateway.admin();

    let enabled_inference = create_route(
        &gateway,
        &provider_id,
        "fast-chat",
        RoutePurpose::Inference,
        true,
    )
    .await?;
    let backup_inference = create_route(
        &gateway,
        &provider_id,
        "backup-chat",
        RoutePurpose::Inference,
        true,
    )
    .await?;
    let disabled_inference = create_route(
        &gateway,
        &provider_id,
        "disabled-chat",
        RoutePurpose::Inference,
        false,
    )
    .await?;
    let image_route = create_route(
        &gateway,
        &provider_id,
        "image-gen",
        RoutePurpose::ImageGeneration,
        true,
    )
    .await?;

    // 1. Default unset + eligible filtering.
    let initial = admin.get_platform_llm_config().await?;
    assert_eq!(initial.status, PlatformLlmStatus::Unset);
    assert!(initial.route_id.is_none());
    assert!(initial.route.is_none());
    let eligible_ids: Vec<&str> = initial
        .eligible_routes
        .iter()
        .map(|item| item.id.as_str())
        .collect();
    assert_eq!(eligible_ids.len(), 2);
    assert!(eligible_ids.contains(&enabled_inference.id.as_str()));
    assert!(eligible_ids.contains(&backup_inference.id.as_str()));

    // 2. Save valid route.
    let saved = admin
        .update_platform_llm_config(UpdatePlatformLlmConfig {
            route_id: Some(enabled_inference.id.clone().into()),
        })
        .await?;
    assert_eq!(saved.status, PlatformLlmStatus::Available);
    assert_eq!(
        saved.route_id.as_deref(),
        Some(enabled_inference.id.as_str())
    );
    assert_eq!(
        saved.route.as_ref().map(|r| r.model_id.as_str()),
        Some("fast-chat")
    );

    // 3. Invalid saves fail and preserve previous valid binding.
    for invalid_id in [
        disabled_inference.id.as_str(),
        image_route.id.as_str(),
        "nonexistent-route-id",
    ] {
        let err = admin
            .update_platform_llm_config(UpdatePlatformLlmConfig {
                route_id: Some(invalid_id.to_owned()),
            })
            .await
            .unwrap_err();
        assert_eq!(err.code, "platform_llm_route_invalid");
    }
    let after_invalid = admin.get_platform_llm_config().await?;
    assert_eq!(after_invalid.status, PlatformLlmStatus::Available);
    assert_eq!(
        after_invalid.route_id.as_deref(),
        Some(enabled_inference.id.as_str())
    );

    // 4. Renaming public model_id preserves binding by stable record ID.
    admin
        .update_model(
            enabled_inference.model_id.as_str(),
            UpdateRoute {
                model_id: Some("fast-chat-renamed".into()),
                ..Default::default()
            },
        )
        .await?;
    let after_rename = admin.get_platform_llm_config().await?;
    assert_eq!(after_rename.status, PlatformLlmStatus::Available);
    assert_eq!(
        after_rename.route.as_ref().map(|r| r.model_id.as_str()),
        Some("fast-chat-renamed")
    );

    // 5. Disabling the bound route is allowed and surfaces as Unavailable without fallback.
    admin
        .update_model(
            "fast-chat-renamed",
            UpdateRoute {
                is_enabled: Some(false),
                ..Default::default()
            },
        )
        .await?;
    let after_disable = admin.get_platform_llm_config().await?;
    assert_eq!(after_disable.status, PlatformLlmStatus::Unavailable);
    assert_eq!(
        after_disable.route_id.as_deref(),
        Some(enabled_inference.id.as_str())
    );
    assert!(after_disable.route.is_none());
    let resolve_err = super::config::resolve_active_route(&gateway)
        .await
        .unwrap_err();
    assert_eq!(resolve_err.code, "platform_llm_route_unavailable");

    // 6. Clearing returns to Unset.
    let cleared = admin
        .update_platform_llm_config(UpdatePlatformLlmConfig { route_id: None })
        .await?;
    assert_eq!(cleared.status, PlatformLlmStatus::Unset);
    assert!(cleared.route_id.is_none());
    let unset_err = super::config::resolve_active_route(&gateway)
        .await
        .unwrap_err();
    assert_eq!(unset_err.code, "platform_llm_not_configured");

    // 7. Generic settings API rejects raw reads/writes of platform_llm_config.
    assert!(admin.get_setting(SETTINGS_KEY).await.is_err());
    assert!(admin.set_setting(SETTINGS_KEY, "{}").await.is_err());

    Ok(())
}

#[tokio::test]
async fn conversation_title_respects_thinking_level_inheritance_and_override() -> anyhow::Result<()>
{
    let (mut gateway, _dir, provider_id) = test_gateway().await?;
    let route = create_route(
        &gateway,
        &provider_id,
        "title-model",
        RoutePurpose::Inference,
        true,
    )
    .await?;

    let mut resp_follow = AiResponse::new("r1", "title-model");
    resp_follow.push_output_text("  # \"Rust async error handling\"!  \nExtra line");
    let mut resp_override = AiResponse::new("r2", "title-model");
    resp_override.push_output_text("「排查 SQLite 锁表问题。」");

    let executor = Arc::new(InMemoryModelTurnExecutor::scripted([
        resp_follow,
        resp_override,
    ]));
    gateway.model_turn = executor.clone();

    gateway
        .admin()
        .update_platform_llm_config(UpdatePlatformLlmConfig {
            route_id: Some(route.id.clone().into()),
        })
        .await?;

    // 1. No per-task thinking_level -> follows bound Route default (request.reasoning.level is None).
    let first = gateway
        .admin()
        .generate_conversation_title(ConversationTitleInput {
            api_key_id: "key-1".into(),
            user_message: "How do I handle async errors in Rust?".into(),
            assistant_message: Some("Use Result and the ? operator...".into()),
            locale: Some("en-US".into()),
            thinking_level: None,
        })
        .await?;
    assert_eq!(first.title.as_deref(), Some("Rust async error handling"));

    // 2. Explicit per-task thinking_level -> overrides request.reasoning.level.
    let second = gateway
        .admin()
        .generate_conversation_title(ConversationTitleInput {
            api_key_id: "key-1".into(),
            user_message: "为什么 SQLite 会出现 database is locked？".into(),
            assistant_message: None,
            locale: Some("zh-CN".into()),
            thinking_level: Some(ThinkingLevel::Off),
        })
        .await?;
    assert_eq!(second.title.as_deref(), Some("排查 SQLite 锁表问题"));

    let recorded = executor.requests();
    assert_eq!(recorded.len(), 2);
    assert_eq!(recorded[0].model, route.id.as_str());
    assert_eq!(recorded[0].reasoning.level, None);
    assert!(recorded[0].tools.is_none());
    assert!(
        recorded[0]
            .instructions
            .as_deref()
            .is_some_and(|text| text.contains("Write the title in English."))
    );

    assert_eq!(recorded[1].model, route.id.as_str());
    assert_eq!(recorded[1].reasoning.level, Some(ThinkingLevel::Off));
    assert!(
        recorded[1]
            .instructions
            .as_deref()
            .is_some_and(|text| text.contains("Write the title in Simplified Chinese."))
    );

    Ok(())
}

#[test]
fn sanitize_title_handles_tags_deferral_json_and_limits() {
    assert_eq!(
        sanitize_title("Sure:\n<title>Rust async error handling</title>").as_deref(),
        Some("Rust async error handling")
    );
    assert_eq!(
        sanitize_title("<title>Unclosed truncated tag").as_deref(),
        Some("Unclosed truncated tag")
    );
    assert_eq!(
        sanitize_title(r#"{"title": "JSON fallback title."}"#).as_deref(),
        Some("JSON fallback title")
    );
    assert_eq!(
        sanitize_title("  Title: \"Quick Start Guide.\" \nSecond line").as_deref(),
        Some("Quick Start Guide")
    );
    assert_eq!(
        sanitize_title("标题：《架构设计评审》！").as_deref(),
        Some("架构设计评审")
    );
    // Deferral markers and overlong chatter are rejected rather than truncated.
    assert_eq!(sanitize_title("<title/>"), None);
    assert_eq!(sanitize_title("<title>none</title>"), None);
    assert_eq!(sanitize_title("None"), None);
    assert_eq!(sanitize_title("   \n  \"\"  "), None);
    assert_eq!(
        sanitize_title("one two three four five six seven eight nine ten eleven twelve thirteen"),
        None
    );
    assert_eq!(sanitize_title(&"a".repeat(81)), None);
}
