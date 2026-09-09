use super::AdminService;
use std::sync::Arc;
use stravia_web_search::{
    WebSearchConfig,
    admin::{
        CompatibleCodexProvider, EligibleSearchModel, SearchAdmin, WebSearchConfigError,
        WebSearchConfigView,
    },
};
impl AdminService {
    fn search_admin(&self) -> SearchAdmin {
        SearchAdmin::new(Arc::new(crate::web_search::host::SearchHost(
            self.gw.clone(),
        )))
    }
    pub async fn get_web_search_config(&self) -> Result<WebSearchConfigView, WebSearchConfigError> {
        self.search_admin().get_web_search_config().await
    }
    pub async fn list_eligible_web_search_models(
        &self,
    ) -> Result<Vec<EligibleSearchModel>, WebSearchConfigError> {
        self.search_admin().list_eligible_web_search_models().await
    }
    pub async fn list_compatible_codex_search_providers(
        &self,
    ) -> Result<Vec<CompatibleCodexProvider>, WebSearchConfigError> {
        self.search_admin()
            .list_compatible_codex_search_providers()
            .await
    }
    pub async fn update_web_search_config(
        &self,
        config: WebSearchConfig,
    ) -> Result<WebSearchConfigView, WebSearchConfigError> {
        self.search_admin().update_web_search_config(config).await
    }
}
#[cfg(test)]
mod tests {

    use super::*;
    use stravia_web_search::{WebSearchBackendDraft, admin::WebSearchLimits};

    async fn admin() -> (tempfile::TempDir, AdminService) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let gateway = crate::Gateway::new(crate::config::GatewayConfig {
            data_dir: directory.path().to_path_buf(),
            ..Default::default()
        })
        .await
        .expect("Gateway");
        (directory, gateway.admin())
    }

    #[tokio::test]
    async fn disabled_config_accepts_an_incomplete_binding_as_a_full_replacement() {
        let (_directory, admin) = admin().await;
        let current = admin.get_web_search_config().await.expect("current config");
        assert_eq!(
            current.limits,
            WebSearchLimits {
                min_turns: 2,
                max_turns: 20,
                min_total_time_seconds: 60,
                max_total_time_seconds: 900,
            }
        );

        let updated = admin
            .update_web_search_config(WebSearchConfig {
                revision: current.revision,
                enabled: false,
                backend: Some(WebSearchBackendDraft::Local { model_id: None }),
                max_turns: 6,
                total_time_seconds: 120,
                updated_at: current.updated_at.clone(),
            })
            .await
            .expect("disabled incomplete config");

        assert_eq!(updated.revision, current.revision + 1);
        assert_eq!(updated.max_turns, 6);
        assert_eq!(
            admin.get_web_search_config().await.expect("stored config"),
            updated
        );
    }

    #[tokio::test]
    async fn enabled_config_rejects_incomplete_binding_and_invalid_limits() {
        let (_directory, admin) = admin().await;
        let current = admin.get_web_search_config().await.expect("current config");
        let mut input = WebSearchConfig {
            revision: current.revision,
            enabled: true,
            backend: None,
            max_turns: 12,
            total_time_seconds: 600,
            updated_at: current.updated_at.clone(),
        };

        let error = admin
            .update_web_search_config(input.clone())
            .await
            .expect_err("incomplete binding");
        assert_eq!(error.code, "WEB_SEARCH_INVALID_CONFIG");

        input.enabled = false;
        input.backend = Some(WebSearchBackendDraft::Local { model_id: None });
        input.max_turns = 1;
        let error = admin
            .update_web_search_config(input)
            .await
            .expect_err("invalid limits");
        assert_eq!(error.code, "WEB_SEARCH_INVALID_CONFIG");
    }

    #[tokio::test]
    async fn codex_mode_preserves_local_limits_without_validating_them() {
        let (_directory, admin) = admin().await;
        let current = admin.get_web_search_config().await.expect("current config");

        let codex = admin
            .update_web_search_config(WebSearchConfig {
                revision: current.revision,
                enabled: false,
                backend: Some(WebSearchBackendDraft::Codex {
                    provider_id: None,
                    upstream_model: None,
                }),
                max_turns: 1,
                total_time_seconds: 1,
                updated_at: current.updated_at.clone(),
            })
            .await
            .expect("disabled Codex config");

        assert_eq!(codex.max_turns, 1);
        assert_eq!(codex.total_time_seconds, 1);

        let error = admin
            .update_web_search_config(WebSearchConfig {
                backend: Some(WebSearchBackendDraft::Local { model_id: None }),
                ..codex.config
            })
            .await
            .expect_err("Local mode must validate restored limits");
        assert_eq!(error.code, "WEB_SEARCH_INVALID_CONFIG");
    }

    #[tokio::test]
    async fn local_search_requires_sources_but_ignores_the_legacy_disabled_switch() {
        let (_directory, admin) = admin().await;
        let provider = admin
            .gw
            .storage
            .providers()
            .create(crate::db::models::CreateProviderRecord {
                name: "Tool-capable Provider".into(),
                vendor: None,
                protocol: "openai-compatible".into(),
                base_url: "https://example.com/v1".into(),
                preset_key: None,
                channel: None,
                models_source: None,
                static_models: None,
                api_key: "sk-test".into(),
                adapter_credentials: r#"{"apiKey":"sk-test"}"#.into(),
                auth_mode: "apikey".into(),
                use_proxy: false,
            })
            .await
            .expect("Provider");
        admin
            .create_manual_provider_model(
                &provider.id,
                "tool-model",
                crate::provider_models::CreateManualProviderModel {
                    metadata: serde_json::json!({
                        "id": "tool-model",
                        "tool_call": true,
                    }),
                },
            )
            .await
            .expect("Provider Model");
        let model = admin
            .create_model(crate::db::models::CreateRoute {
                model_id: "Search Model".into(),
                display_name: None,
                balance: Some("traffic_equalization".into()),
                target_provider: provider.id.clone(),
                target_model: "tool-model".into(),
                targets: vec![],
            })
            .await
            .expect("Model route");

        let eligible = admin
            .list_eligible_web_search_models()
            .await
            .expect("eligible Models");

        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0].id, model.id);
        assert_eq!(eligible[0].model_id, "Search Model");
        assert_eq!(eligible[0].display_name, "Search Model");

        admin
            .gw
            .storage
            .settings()
            .set("web_access_enabled", "false")
            .await
            .unwrap();
        let current = admin.get_web_search_config().await.unwrap();
        let config = WebSearchConfig {
            enabled: true,
            backend: Some(WebSearchBackendDraft::Local {
                model_id: Some(model.id),
            }),
            ..current.config
        };
        let sources = admin.gw.storage.web_providers().unwrap();
        sources
            .save_settings(&crate::db::models::WebAccessSettings::default())
            .await
            .unwrap();
        assert_eq!(
            admin
                .update_web_search_config(config.clone())
                .await
                .unwrap_err()
                .code,
            "WEB_SEARCH_SOURCES_UNAVAILABLE"
        );
        let remote = admin
            .create_web_provider(crate::db::models::CreateWebProvider {
                name: "Search sources".into(),
                kind: "exa".into(),
                api_key: Some("secret".into()),
                use_proxy: false,
                local_engines: None,
            })
            .await
            .unwrap();
        admin
            .update_web_access_settings(crate::db::models::WebAccessSettings {
                search_provider_ids: vec![remote.id.clone()],
                fetch_provider_ids: vec![remote.id],
            })
            .await
            .unwrap();
        assert!(!admin.get_web_search_config().await.unwrap().enabled);
        let enabled = admin.update_web_search_config(config).await.unwrap();
        assert!(enabled.enabled);
        assert_eq!(admin.get_web_search_config().await.unwrap(), enabled);
    }

    #[tokio::test]
    async fn missing_browser_excludes_local_sources_without_blocking_remote_search() {
        let (directory, admin) = admin().await;
        admin
            .gw
            .set_browser_path(Some(directory.path().join("missing-chrome.exe")));
        let store = admin.gw.storage.web_providers().unwrap();
        let local = store
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|provider| provider.kind == "local")
            .unwrap();
        let mut settings = crate::db::models::WebAccessSettings {
            search_provider_ids: vec![local.id.clone()],
            fetch_provider_ids: vec![local.id],
        };
        store.save_settings(&settings).await.unwrap();
        assert_eq!(
            admin
                .search_admin()
                .validate_local_sources()
                .await
                .unwrap_err()
                .code,
            "WEB_SEARCH_SOURCES_UNAVAILABLE"
        );
        let remote = admin
            .create_web_provider(crate::db::models::CreateWebProvider {
                name: "Remote".into(),
                kind: "exa".into(),
                api_key: Some("secret".into()),
                use_proxy: false,
                local_engines: None,
            })
            .await
            .unwrap();
        settings.search_provider_ids.push(remote.id.clone());
        store.save_settings(&settings).await.unwrap();
        assert!(admin.search_admin().validate_local_sources().await.is_err());
        settings.fetch_provider_ids.push(remote.id);
        store.save_settings(&settings).await.unwrap();
        admin.search_admin().validate_local_sources().await.unwrap();
    }

    #[tokio::test]
    async fn codex_binding_requires_an_effective_oauth_credential() {
        let (_directory, admin) = admin().await;
        let provider = admin
            .gw
            .storage
            .providers()
            .create(crate::db::models::CreateProviderRecord {
                name: "Codex without OAuth".into(),
                vendor: Some("openai".into()),
                protocol: "open-responses".into(),
                base_url: "https://chatgpt.com/backend-api/codex/responses".into(),
                preset_key: Some("openai".into()),
                channel: Some("codex".into()),
                models_source: None,
                static_models: None,
                api_key: String::new(),
                adapter_credentials: "{}".into(),
                auth_mode: "oauth".into(),
                use_proxy: false,
            })
            .await
            .expect("Codex Provider");

        assert!(
            admin
                .list_compatible_codex_search_providers()
                .await
                .expect("compatible Providers")
                .is_empty()
        );

        let current = admin.get_web_search_config().await.expect("current config");
        let error = admin
            .update_web_search_config(WebSearchConfig {
                revision: current.revision,
                enabled: true,
                backend: Some(WebSearchBackendDraft::Codex {
                    provider_id: Some(provider.id),
                    upstream_model: Some("gpt-5".into()),
                }),
                max_turns: 12,
                total_time_seconds: 600,
                updated_at: current.updated_at.clone(),
            })
            .await
            .expect_err("missing OAuth credential");
        assert_eq!(error.code, "WEB_SEARCH_CODEX_PROVIDER_INVALID");
    }
}
