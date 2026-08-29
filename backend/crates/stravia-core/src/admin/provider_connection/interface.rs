use super::*;

pub(crate) enum ProviderSave {
    Catalog {
        input: CreateProvider,
        authorization_id: Option<String>,
    },
    Custom(CreateProvider),
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
        };
        self.admin.create_provider_from_input(input, false).await
    }

    pub(crate) async fn update(
        &self,
        provider_id: &str,
        input: UpdateProvider,
    ) -> anyhow::Result<Provider> {
        self.admin.update_provider_record(provider_id, input).await
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
    use serde_json::json;

    use super::*;
    use crate::config::GatewayConfig;
    use crate::provider_models::CreateManualProviderModel;

    #[tokio::test]
    async fn saving_a_custom_provider_persists_the_connection_without_testing_it()
    -> anyhow::Result<()> {
        let data_dir = tempfile::tempdir()?;
        let (gateway, _logs) = Gateway::new(GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..GatewayConfig::default()
        })
        .await?;
        let admin = gateway.admin();

        let provider = ProviderConnection::new(&admin)
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
        assert!(admin.get_provider(&provider.id).await.is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn testing_a_candidate_does_not_save_the_provider() -> anyhow::Result<()> {
        let data_dir = tempfile::tempdir()?;
        let (gateway, _logs) = Gateway::new(GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..GatewayConfig::default()
        })
        .await?;
        let admin = gateway.admin();
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

        let result = ProviderConnection::new(&admin)
            .test(ProviderConnectivityTest::Candidate(input))
            .await?;

        assert!(!result.success);
        assert!(admin.list_providers().await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn deleting_a_provider_removes_its_targets_and_routes_left_empty() -> anyhow::Result<()> {
        let data_dir = tempfile::tempdir()?;
        let (gateway, _logs) = Gateway::new(GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..GatewayConfig::default()
        })
        .await?;
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
                weight: None,
                priority: None,
            })
            .await?;

        ProviderConnection::new(&admin).delete(&provider.id).await?;

        assert!(admin.list_providers().await?.is_empty());
        assert!(admin.list_models().await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn deleting_a_primary_provider_keeps_a_route_with_a_fallback_target() -> anyhow::Result<()>
    {
        let data_dir = tempfile::tempdir()?;
        let (gateway, _logs) = Gateway::new(GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..GatewayConfig::default()
        })
        .await?;
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
            .create_model(CreateModel {
                name: "durable-route".into(),
                balance: Some("priority".into()),
                target_provider: primary.id.clone(),
                target_model: "primary-model".into(),
                targets: vec![
                    CreateModelBackend {
                        provider_id: primary.id.clone(),
                        model: "primary-model".into(),
                        weight: Some(100),
                        priority: Some(1),
                        thinking_level_map: Vec::new(),
                    },
                    CreateModelBackend {
                        provider_id: fallback.id.clone(),
                        model: "fallback-model".into(),
                        weight: Some(100),
                        priority: Some(2),
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
        Ok(())
    }
}
