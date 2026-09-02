use super::*;

pub(crate) enum ProviderSave {
    Catalog {
        input: CreateProvider,
        authorization_id: Option<String>,
    },
    Custom(CreateProvider),
    Update {
        provider_id: String,
        input: UpdateProvider,
    },
}

pub(crate) enum ProviderConnectivityTest {
    Existing(String),
    Candidate(CreateProvider),
}

pub(crate) enum ProviderReconnect {
    Start(ProviderReconnectStart),
    Callback(ProviderReconnectCallback),
}

pub(crate) enum ProviderReconnectStart {
    Authorization {
        vendor: String,
        use_proxy: bool,
        options: OAuthSessionStartOptions,
    },
    Existing {
        provider_id: String,
    },
}

pub(crate) enum ProviderReconnectCallback {
    Complete {
        authorization_id: String,
        input: auth::AuthExchangeInput,
    },
    Bind {
        provider_id: String,
        authorization_id: String,
    },
}

pub(crate) enum ProviderReconnectResult {
    Redirect(AuthSessionInitData),
    Complete(AuthSessionStatusData),
    Provider(Provider),
    Status(ProviderOAuthStatusData),
}

pub(crate) struct ProviderConnection<'a> {
    admin: &'a AdminService,
}

impl<'a> ProviderConnection<'a> {
    pub(crate) fn new(admin: &'a AdminService) -> Self {
        Self { admin }
    }

    pub(crate) async fn catalog_choices(&self) -> crate::provider_catalog::CatalogProviderList {
        self.admin.gw.provider_catalog.providers().await
    }

    pub(crate) async fn requires_oauth_session(
        &self,
        input: &CreateProvider,
    ) -> anyhow::Result<bool> {
        let ProviderSourceInput::Catalog {
            provider_id,
            channel_id,
            fingerprint,
            ..
        } = &input.source
        else {
            return Ok(false);
        };
        let (_, channel) = self
            .admin
            .gw
            .provider_catalog
            .resolve_channel(provider_id, channel_id, fingerprint)
            .await?;
        Ok(channel.auth_mode == crate::provider_catalog::CatalogAuthMode::OAuth)
    }

    pub(crate) async fn list(&self) -> anyhow::Result<Vec<Provider>> {
        self.admin.gw.storage.providers().list().await
    }

    pub(crate) async fn get(&self, provider_id: &str) -> anyhow::Result<Provider> {
        self.admin
            .gw
            .storage
            .providers()
            .get(provider_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("provider not found: {provider_id}"))
    }

    pub(crate) fn preview_base_url(
        &self,
        vendor_id: &str,
        credentials: std::collections::BTreeMap<String, String>,
        configured_base_url: Option<&str>,
    ) -> anyhow::Result<String> {
        validate_provider_base_url(&assemble_vendor_base_url(
            vendor_id,
            &credentials,
            configured_base_url,
        )?)
    }

    pub(crate) async fn save(&self, input: ProviderSave) -> anyhow::Result<Provider> {
        let input = match input {
            ProviderSave::Catalog {
                input,
                authorization_id,
            } => {
                if !matches!(input.source, ProviderSourceInput::Catalog { .. }) {
                    anyhow::bail!("Catalog Provider save requires a Catalog Entry")
                }
                return match authorization_id {
                    Some(authorization_id) => {
                        self.admin
                            .create_provider_with_oauth_session_record(&authorization_id, input)
                            .await
                    }
                    None => self.admin.create_provider_from_input(input, false).await,
                };
            }
            ProviderSave::Custom(input) => {
                if !matches!(input.source, ProviderSourceInput::Custom { .. }) {
                    anyhow::bail!("custom Provider save requires custom connection input")
                }
                input
            }
            ProviderSave::Update { provider_id, input } => {
                return self.admin.update_provider_record(&provider_id, input).await;
            }
        };
        self.admin.create_provider_from_input(input, false).await
    }

    pub(crate) async fn copy(
        &self,
        provider_id: &str,
        options: CopyProviderOptions,
    ) -> anyhow::Result<Provider> {
        self.admin.copy_provider_record(provider_id, options).await
    }

    pub(crate) async fn test(&self, input: ProviderConnectivityTest) -> anyhow::Result<TestResult> {
        match input {
            ProviderConnectivityTest::Existing(id) => self.admin.test_provider_record(&id).await,
            ProviderConnectivityTest::Candidate(input) => {
                self.admin.test_provider_candidate_record(input).await
            }
        }
    }

    pub(crate) async fn delete(&self, provider_id: &str) -> anyhow::Result<()> {
        self.admin.delete_provider_record(provider_id).await
    }

    pub(crate) async fn reconnect(
        &self,
        input: ProviderReconnect,
    ) -> anyhow::Result<ProviderReconnectResult> {
        match input {
            ProviderReconnect::Start(ProviderReconnectStart::Authorization {
                vendor,
                use_proxy,
                options,
            }) => self
                .admin
                .init_oauth_session_record(&vendor, use_proxy, options)
                .await
                .map(ProviderReconnectResult::Redirect),
            ProviderReconnect::Start(ProviderReconnectStart::Existing { provider_id }) => self
                .admin
                .reconnect_provider_oauth_record(&provider_id)
                .await
                .map(ProviderReconnectResult::Status),
            ProviderReconnect::Callback(ProviderReconnectCallback::Complete {
                authorization_id,
                input,
            }) => self
                .admin
                .complete_oauth_session_record(&authorization_id, input)
                .await
                .map(ProviderReconnectResult::Complete),
            ProviderReconnect::Callback(ProviderReconnectCallback::Bind {
                provider_id,
                authorization_id,
            }) => self
                .admin
                .bind_provider_with_oauth_session_record(&provider_id, &authorization_id)
                .await
                .map(ProviderReconnectResult::Provider),
        }
    }
}

impl AdminService {
    pub async fn catalog_choices(&self) -> crate::provider_catalog::CatalogProviderList {
        ProviderConnection::new(self).catalog_choices().await
    }

    pub async fn test_provider_candidate(
        &self,
        input: CreateProvider,
    ) -> anyhow::Result<TestResult> {
        ProviderConnection::new(self)
            .test(ProviderConnectivityTest::Candidate(input))
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::config::GatewayConfig;
    use crate::provider_models::CreateManualProviderModel;
    use crate::storage::{DynStorage, MemoryStorage};

    async fn memory_gateway() -> anyhow::Result<(tempfile::TempDir, Gateway)> {
        let data_dir = tempfile::tempdir()?;
        let storage: DynStorage = Arc::new(MemoryStorage::new(Vec::new(), Vec::new(), Vec::new()));
        let (gateway, _logs) = Gateway::from_storage(
            GatewayConfig {
                data_dir: data_dir.path().to_path_buf(),
                ..GatewayConfig::default()
            },
            storage,
        )
        .await?;
        Ok((data_dir, gateway))
    }

    #[tokio::test]
    async fn saving_a_custom_provider_persists_the_connection_without_testing_it()
    -> anyhow::Result<()> {
        let (_data_dir, gateway) = memory_gateway().await?;
        let admin = gateway.admin();
        let providers = ProviderConnection::new(&admin);

        let provider = providers
            .save(ProviderSave::Custom(CreateProvider {
                name: Some("Custom Connection".into()),
                source: ProviderSourceInput::Custom {
                    vendor: None,
                    protocol: "openai".into(),
                    base_url: "http://127.0.0.1:9/v1/".into(),
                    models_source: None,
                    static_models: None,
                },
                credential: ProviderCredentialInput::None,
                use_proxy: false,
            }))
            .await?;

        assert_eq!(provider.name, "Custom Connection");
        assert_eq!(provider.base_url, "http://127.0.0.1:9/v1");
        assert_eq!(provider.last_test_success, None);
        assert_eq!(providers.list().await?.len(), 1);
        assert_eq!(providers.get(&provider.id).await?.id, provider.id);
        Ok(())
    }

    #[tokio::test]
    async fn catalog_save_uses_the_selected_snapshot_and_preserves_an_override()
    -> anyhow::Result<()> {
        let (_data_dir, gateway) = memory_gateway().await?;
        let admin = gateway.admin();
        let providers = ProviderConnection::new(&admin);
        let catalog = providers.catalog_choices().await;
        let openai = catalog
            .providers
            .iter()
            .find(|provider| provider.id == "openai")
            .expect("OpenAI Catalog Entry");
        let channel = openai
            .channels
            .iter()
            .find(|channel| channel.id == "default")
            .expect("OpenAI default channel");

        let provider = providers
            .save(ProviderSave::Catalog {
                input: CreateProvider {
                    name: None,
                    source: ProviderSourceInput::Catalog {
                        provider_id: openai.id.clone(),
                        channel_id: channel.id.clone(),
                        fingerprint: channel.fingerprint.clone(),
                        base_url_override: Some("https://proxy.example/v1/".into()),
                    },
                    credential: ProviderCredentialInput::None,
                    use_proxy: false,
                },
                authorization_id: None,
            })
            .await?;

        assert_eq!(provider.vendor.as_deref(), Some("openai"));
        assert_eq!(provider.protocol, "open-responses");
        assert_eq!(provider.base_url, "https://proxy.example/v1");
        assert_eq!(provider.models_source.as_deref(), Some("catalog"));
        Ok(())
    }

    #[tokio::test]
    async fn existing_test_and_copy_stay_inside_the_provider_interface() -> anyhow::Result<()> {
        let (_data_dir, gateway) = memory_gateway().await?;
        let admin = gateway.admin();
        let providers = ProviderConnection::new(&admin);
        let original = providers
            .save(ProviderSave::Custom(CreateProvider {
                name: Some("Original".into()),
                source: ProviderSourceInput::Custom {
                    vendor: None,
                    protocol: "openai".into(),
                    base_url: "http://127.0.0.1:9".into(),
                    models_source: None,
                    static_models: None,
                },
                credential: ProviderCredentialInput::None,
                use_proxy: false,
            }))
            .await?;

        let result = providers
            .test(ProviderConnectivityTest::Existing(original.id.clone()))
            .await?;
        assert!(!result.success);
        assert_eq!(
            providers.get(&original.id).await?.last_test_success,
            Some(false)
        );

        let copied = providers
            .copy(&original.id, CopyProviderOptions::default())
            .await?;
        assert_ne!(copied.id, original.id);
        assert_eq!(copied.base_url, original.base_url);
        assert_eq!(providers.list().await?.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn oauth_authorization_start_is_one_provider_operation() -> anyhow::Result<()> {
        let (_data_dir, gateway) = memory_gateway().await?;
        let admin = gateway.admin();
        let result = ProviderConnection::new(&admin)
            .reconnect(ProviderReconnect::Start(
                ProviderReconnectStart::Authorization {
                    vendor: "codex".into(),
                    use_proxy: false,
                    options: OAuthSessionStartOptions {
                        callback_mode: OAuthCallbackMode::Manual,
                        redirect_uri: "http://localhost:1457/auth/callback".into(),
                        listener_port: None,
                        fallback_reason: None,
                    },
                },
            ))
            .await?;

        let ProviderReconnectResult::Redirect(started) = result else {
            panic!("authorization start must return a redirect contract");
        };
        assert_eq!(started.vendor, "codex");
        assert!(!started.session_id.is_empty());
        assert!(!started.auth_url.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn credential_validation_and_base_url_snapshot_apply_to_save_and_update()
    -> anyhow::Result<()> {
        let (_data_dir, gateway) = memory_gateway().await?;
        let admin = gateway.admin();
        let providers = ProviderConnection::new(&admin);
        let invalid = providers
            .save(ProviderSave::Custom(CreateProvider {
                name: Some("Invalid".into()),
                source: ProviderSourceInput::Custom {
                    vendor: Some("openai".into()),
                    protocol: "openai".into(),
                    base_url: "https://snapshot.example/v1".into(),
                    models_source: None,
                    static_models: None,
                },
                credential: ProviderCredentialInput::Fields {
                    values: BTreeMap::from([("unknown".into(), "secret".into())]),
                },
                use_proxy: false,
            }))
            .await
            .expect_err("undeclared Vendor credential must be rejected");
        assert!(invalid.to_string().contains("credential"));

        let provider = providers
            .save(ProviderSave::Custom(CreateProvider {
                name: Some("Snapshot".into()),
                source: ProviderSourceInput::Custom {
                    vendor: Some("openai".into()),
                    protocol: "openai".into(),
                    base_url: "https://snapshot.example/v1/".into(),
                    models_source: None,
                    static_models: None,
                },
                credential: ProviderCredentialInput::Fields {
                    values: BTreeMap::from([("apiKey".into(), "first".into())]),
                },
                use_proxy: false,
            }))
            .await?;
        let updated = providers
            .save(ProviderSave::Update {
                provider_id: provider.id,
                input: UpdateProvider {
                    adapter_credentials: Some(BTreeMap::from([("apiKey".into(), "second".into())])),
                    ..UpdateProvider::default()
                },
            })
            .await?;
        assert_eq!(updated.base_url, "https://snapshot.example/v1");
        Ok(())
    }

    #[tokio::test]
    async fn testing_a_candidate_does_not_save_the_provider() -> anyhow::Result<()> {
        let (_data_dir, gateway) = memory_gateway().await?;
        let admin = gateway.admin();
        let providers = ProviderConnection::new(&admin);
        let input = CreateProvider {
            name: Some("Unsaved Candidate".into()),
            source: ProviderSourceInput::Custom {
                vendor: None,
                protocol: "openai".into(),
                base_url: "http://127.0.0.1:9".into(),
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::None,
            use_proxy: false,
        };

        let result = providers
            .test(ProviderConnectivityTest::Candidate(input))
            .await?;

        assert!(!result.success);
        assert!(providers.list().await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn updating_a_provider_uses_the_save_contract() -> anyhow::Result<()> {
        let (_data_dir, gateway) = memory_gateway().await?;
        let admin = gateway.admin();
        let providers = ProviderConnection::new(&admin);
        let provider = providers
            .save(ProviderSave::Custom(CreateProvider {
                name: Some("Before".into()),
                source: ProviderSourceInput::Custom {
                    vendor: None,
                    protocol: "openai".into(),
                    base_url: "http://127.0.0.1:9/v1".into(),
                    models_source: None,
                    static_models: None,
                },
                credential: ProviderCredentialInput::None,
                use_proxy: false,
            }))
            .await?;

        let updated = providers
            .save(ProviderSave::Update {
                provider_id: provider.id.clone(),
                input: UpdateProvider {
                    name: Some("After".into()),
                    ..UpdateProvider::default()
                },
            })
            .await?;

        assert_eq!(updated.id, provider.id);
        assert_eq!(updated.name, "After");
        assert_eq!(providers.get(&provider.id).await?.name, "After");
        Ok(())
    }

    #[tokio::test]
    async fn deleting_a_provider_removes_its_targets_and_routes_left_empty() -> anyhow::Result<()> {
        let (_data_dir, gateway) = memory_gateway().await?;
        let admin = gateway.admin();
        let provider = ProviderConnection::new(&admin)
            .save(ProviderSave::Custom(CreateProvider {
                name: Some("Disposable Connection".into()),
                source: ProviderSourceInput::Custom {
                    vendor: None,
                    protocol: "openai".into(),
                    base_url: "http://127.0.0.1:9".into(),
                    models_source: None,
                    static_models: None,
                },
                credential: ProviderCredentialInput::None,
                use_proxy: false,
            }))
            .await?;
        admin
            .create_manual_provider_model(
                &provider.id,
                "disposable-model",
                CreateManualProviderModel {
                    metadata: json!({
                        "id": "disposable-model",
                        "name": "Disposable Model"
                    }),
                },
            )
            .await?;
        admin
            .bind_route(BindRouteInput {
                route_id: None,
                provider_id: provider.id.clone(),
                provider_model_id: "disposable-model".into(),
                priority: None,
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
            })
            .await?;

        ProviderConnection::new(&admin).delete(&provider.id).await?;

        assert!(admin.list_providers().await?.is_empty());
        assert!(admin.list_models().await?.is_empty());
        assert!(
            gateway
                .model_cache
                .read()
                .await
                .match_model("disposable-model")
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn deleting_a_primary_provider_keeps_a_route_with_a_fallback_target() -> anyhow::Result<()>
    {
        let (_data_dir, gateway) = memory_gateway().await?;
        let admin = gateway.admin();
        let save = |name: &str| CreateProvider {
            name: Some(name.into()),
            source: ProviderSourceInput::Custom {
                vendor: None,
                protocol: "openai".into(),
                base_url: "http://127.0.0.1:9".into(),
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::None,
            use_proxy: false,
        };
        let primary = admin.create_provider(save("Primary Connection")).await?;
        let fallback = admin.create_provider(save("Fallback Connection")).await?;
        admin
            .create_manual_provider_model(
                &primary.id,
                "primary-model",
                CreateManualProviderModel {
                    metadata: json!({"id": "primary-model", "name": "Primary Model"}),
                },
            )
            .await?;
        admin
            .create_manual_provider_model(
                &fallback.id,
                "fallback-model",
                CreateManualProviderModel {
                    metadata: json!({"id": "fallback-model", "name": "Fallback Model"}),
                },
            )
            .await?;
        admin
            .create_model(CreateRoute {
                model_id: "durable-route".into(),
                display_name: None,
                balance: Some("traffic_equalization".into()),
                target_provider: primary.id.clone(),
                target_model: "primary-model".into(),
                targets: vec![
                    CreateTarget {
                        provider_id: primary.id.clone(),
                        model: "primary-model".into(),
                        priority: Some(1),
                        first_token_timeout_ms: None,
                        target_retry_budget: None,
                        target_cooldown_ms: None,
                        thinking_level_map: Vec::new(),
                    },
                    CreateTarget {
                        provider_id: fallback.id.clone(),
                        model: "fallback-model".into(),
                        priority: Some(2),
                        first_token_timeout_ms: None,
                        target_retry_budget: None,
                        target_cooldown_ms: None,
                        thinking_level_map: Vec::new(),
                    },
                ],
            })
            .await?;

        ProviderConnection::new(&admin).delete(&primary.id).await?;

        let routes = admin.list_models().await?;
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].target_provider, fallback.id);
        assert_eq!(routes[0].target_model, "fallback-model");
        assert_eq!(routes[0].targets.len(), 1);
        assert_eq!(routes[0].targets[0].provider_id, fallback.id);
        assert!(
            gateway
                .model_cache
                .read()
                .await
                .match_model("durable-route")
                .is_some()
        );
        Ok(())
    }
}
