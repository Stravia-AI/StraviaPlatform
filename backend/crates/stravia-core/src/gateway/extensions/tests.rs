use super::*;
use crate::db::models::{CreateApiKey, CreateRoute, CreateTarget, UpdateRoute};
use stravia_runtime_contract::Principal;
use stravia_web_search::{ResolvedWebSearchBackend, SearchRunAuthorizer};

#[tokio::test]
async fn local_search_binding_survives_rename_and_cannot_use_a_public_name_collision() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let gateway = Gateway::new(crate::config::GatewayConfig {
        data_dir: directory.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let admin = gateway.admin();
    let provider = gateway
        .storage
        .providers()
        .create(crate::db::models::CreateProviderRecord {
            name: "Search binding fixture".into(),
            vendor: Some("custom".into()),
            protocol: "openai-compatible".into(),
            base_url: "https://example.com/v1".into(),
            preset_key: None,
            channel: Some("default".into()),
            models_source: None,
            static_models: None,
            api_key: "fixture-key".into(),
            adapter_credentials: r#"{"apiKey":"fixture-key"}"#.into(),
            vendor_options: "{}".into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
        })
        .await
        .expect("Provider");
    admin
        .create_manual_provider_model(
            &provider.id,
            "fixture-model",
            crate::provider_models::CreateManualProviderModel {
                metadata: serde_json::json!({
                    "id": "fixture-model",
                    "modalities": {"input": ["text"], "output": ["text"]},
                }),
                template_id: None,
            },
        )
        .await
        .expect("Provider Model");
    let draft = |model_id: String| CreateRoute {
        purpose: crate::db::models::RoutePurpose::Inference,
        model_id,
        display_name: None,
        balance: None,
        targets: vec![CreateTarget {
            provider_id: provider.id.clone(),
            model: "fixture-model".into(),
            enabled: true,
            priority: None,
            first_token_timeout_ms: None,
            target_retry_budget: None,
            target_cooldown_ms: None,
            thinking_level_map: Vec::new(),
        }],
        default_thinking_level: None,
        billing: None,
    };
    let model = admin
        .create_model(draft("search-original".into()))
        .await
        .expect("Model");
    let key = admin
        .create_api_key(CreateApiKey {
            key: None,
            name: "Search binding fixture".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: true,
            transparent_injection_enabled: false,
            transparent_injections: Default::default(),
            model_ids: Vec::new(),
            spend_limit: None,
        })
        .await
        .expect("API key without inference model bindings");
    let principal = Principal::new(key.id);
    let authorizer = GatewayWebSearchAuthorizer { gateway };
    let local = ResolvedWebSearchBackend::Local {
        model_id: model.id.as_str().into(),
    };
    authorizer
        .authorize(&principal, &local)
        .await
        .expect("Local binding uses the internal Model ID");
    authorizer
        .authorize(
            &principal,
            &ResolvedWebSearchBackend::External {
                route_id: "search-original".into(),
            },
        )
        .await
        .expect("External binding keeps the public Model name");
    assert!(
        authorizer
            .authorize(
                &principal,
                &ResolvedWebSearchBackend::External {
                    route_id: model.id.as_str().into(),
                },
            )
            .await
            .is_err()
    );

    admin
        .update_model(
            "search-original",
            UpdateRoute {
                model_id: Some("search-renamed".into()),
                ..Default::default()
            },
        )
        .await
        .expect("rename the public Model");
    authorizer
        .authorize(&principal, &local)
        .await
        .expect("renaming does not change the Local binding");
    admin
        .create_model(draft(model.id.as_str().into()))
        .await
        .expect("a different Model with a colliding public name");
    admin
        .update_model(
            "search-renamed",
            UpdateRoute {
                is_enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .expect("disable the bound Model");
    assert_eq!(
        authorizer
            .authorize(&principal, &local)
            .await
            .unwrap_err()
            .code,
        "authorization_failed"
    );
    authorizer
        .authorize(
            &principal,
            &ResolvedWebSearchBackend::External {
                route_id: model.id.as_str().into(),
            },
        )
        .await
        .expect("the unrelated public-name Route remains enabled");
}
