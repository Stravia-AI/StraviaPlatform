use super::*;

impl AdminService {
    // ── API Keys ──

    pub async fn list_api_keys(&self) -> anyhow::Result<Vec<ApiKeyWithBindings>> {
        self.api_keys_store()?.list().await
    }

    pub async fn get_api_key(&self, id: &str) -> anyhow::Result<ApiKeyWithBindings> {
        self.api_keys_store()?
            .get(id)
            .await?
            .context("api key not found")
    }

    pub async fn create_api_key(&self, input: CreateApiKey) -> anyhow::Result<ApiKeyWithBindings> {
        let name = normalize_name(&input.name, "api key name")?;
        self.ensure_api_key_name_unique(None, &name).await?;
        let key = normalize_api_key(input.key)?;
        if let Some(key) = key.as_deref() {
            self.ensure_api_key_unique(None, key).await?;
        }
        let concurrency_limit = validate_concurrency_limit(input.concurrency_limit)?;
        let expires_at = normalize_api_key_expiry(input.expires_at)?;
        let result = self
            .api_keys_store()?
            .create(crate::db::models::CreateApiKey {
                key,
                name,
                concurrency_limit,
                mcp_access_enabled: input.mcp_access_enabled,
                transparent_injection_enabled: input.transparent_injection_enabled,
                inject_media_understanding: input.inject_media_understanding,
                inject_web_search: input.inject_web_search,
                expires_at,
                model_ids: input.model_ids,
            })
            .await?;
        self.gw
            .principal_admission
            .set_limit(&result.id, concurrency_limit);
        self.bump_config_epoch().await?;
        Ok(result)
    }

    pub async fn update_api_key(
        &self,
        id: &str,
        input: UpdateApiKey,
    ) -> anyhow::Result<ApiKeyWithBindings> {
        let current = self
            .api_keys_store()?
            .get(id)
            .await?
            .context("api key not found")?;

        let name = normalize_name(&input.name.unwrap_or(current.name), "api key name")?;
        self.ensure_api_key_name_unique(Some(id), &name).await?;
        let key = match input.key {
            Some(value) => {
                let key = normalize_api_key(Some(value))?.context("API key is required")?;
                self.ensure_api_key_unique(Some(id), &key).await?;
                Some(key)
            }
            None => None,
        };
        let concurrency_limit = match input.concurrency_limit {
            Some(value) => validate_concurrency_limit(value)?,
            None => current.concurrency_limit,
        };
        let is_enabled = input.is_enabled.unwrap_or(current.is_enabled);
        let mcp_access_enabled = input
            .mcp_access_enabled
            .unwrap_or(current.mcp_access_enabled);
        let transparent_injection_enabled = input
            .transparent_injection_enabled
            .unwrap_or(current.transparent_injection_enabled);
        let inject_media_understanding = input
            .inject_media_understanding
            .unwrap_or(current.inject_media_understanding);
        let inject_web_search = input.inject_web_search.unwrap_or(current.inject_web_search);
        let expires_at = match input.expires_at {
            Some(value) => normalize_api_key_expiry(Some(value))?,
            None => current.expires_at,
        };

        let result = self
            .api_keys_store()?
            .update(
                id,
                UpdateApiKey {
                    key,
                    name: Some(name),
                    concurrency_limit: Some(concurrency_limit),
                    is_enabled: Some(is_enabled),
                    mcp_access_enabled: Some(mcp_access_enabled),
                    transparent_injection_enabled: Some(transparent_injection_enabled),
                    inject_media_understanding: Some(inject_media_understanding),
                    inject_web_search: Some(inject_web_search),
                    expires_at,
                    model_ids: input.model_ids,
                },
            )
            .await?;
        self.gw.principal_admission.set_limit(id, concurrency_limit);
        self.bump_config_epoch().await?;
        Ok(result)
    }

    pub async fn delete_api_key(&self, id: &str) -> anyhow::Result<()> {
        self.api_keys_store()?.delete(id).await?;
        self.gw.principal_admission.remove_principal(id);
        self.bump_config_epoch().await?;
        Ok(())
    }
    async fn ensure_api_key_name_unique(
        &self,
        exclude_id: Option<&str>,
        name: &str,
    ) -> anyhow::Result<()> {
        if self
            .api_keys_store()?
            .exists_by_name(name, exclude_id)
            .await?
        {
            return Err(coded_error(
                "API_KEY_NAME_CONFLICT",
                &format!("api key name already exists: {name}"),
                serde_json::json!({ "name": name }),
            ));
        }
        Ok(())
    }

    async fn ensure_api_key_unique(
        &self,
        exclude_id: Option<&str>,
        key: &str,
    ) -> anyhow::Result<()> {
        if self
            .api_keys_store()?
            .exists_by_key(key, exclude_id)
            .await?
        {
            anyhow::bail!("API key already exists");
        }
        Ok(())
    }
    fn api_keys_store(&self) -> anyhow::Result<&dyn crate::storage::traits::ApiKeyStore> {
        self.gw
            .storage
            .api_keys()
            .context("selected storage backend does not support api key management")
    }
}

fn validate_concurrency_limit(value: Option<i32>) -> anyhow::Result<Option<i32>> {
    if value.is_some_and(|limit| limit <= 0) {
        anyhow::bail!("concurrency limit must be a positive integer");
    }
    Ok(value)
}

fn normalize_api_key(value: Option<String>) -> anyhow::Result<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        anyhow::bail!("API key must not be empty");
    }
    Ok(Some(value.to_owned()))
}

fn normalize_api_key_expiry(value: Option<String>) -> anyhow::Result<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let parsed = super::auth_data::parse_datetime_utc(value)
        .context("invalid API key expiration; expected RFC 3339")?;
    Ok(Some(
        parsed.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    ))
}

#[cfg(test)]
mod expiry_tests {
    use super::normalize_api_key_expiry;
    use crate::Gateway;

    #[test]
    fn api_key_expiry_is_validated_and_normalized() {
        assert_eq!(
            normalize_api_key_expiry(Some("2030-01-02T03:04:05+08:00".into()))
                .expect("valid expiration")
                .as_deref(),
            Some("2030-01-01T19:04:05Z")
        );
        assert!(normalize_api_key_expiry(Some("not-a-date".into())).is_err());
        assert_eq!(
            normalize_api_key_expiry(Some("  ".into())).expect("empty clears expiration"),
            None
        );
    }

    #[tokio::test]
    async fn api_key_concurrency_limit_crud_uses_nullable_tri_state() {
        let data_dir = tempfile::tempdir().expect("temporary data dir");
        let (gateway, _logs) = Gateway::new(crate::config::GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..Default::default()
        })
        .await
        .expect("gateway");
        let admin = gateway.admin();
        let created = admin
            .create_api_key(crate::db::models::CreateApiKey {
                key: None,
                name: "Concurrency test".into(),
                concurrency_limit: None,
                expires_at: None,
                mcp_access_enabled: false,
                transparent_injection_enabled: false,
                inject_web_search: false,
                inject_media_understanding: false,
                model_ids: Vec::new(),
            })
            .await
            .expect("API key");
        assert_eq!(created.concurrency_limit, None);

        let set = admin
            .update_api_key(
                &created.id,
                crate::db::models::UpdateApiKey {
                    key: None,
                    name: None,
                    concurrency_limit: Some(Some(3)),
                    is_enabled: None,
                    mcp_access_enabled: None,
                    transparent_injection_enabled: None,
                    inject_web_search: None,
                    inject_media_understanding: None,
                    expires_at: None,
                    model_ids: None,
                },
            )
            .await
            .expect("set concurrency limit");
        assert_eq!(set.concurrency_limit, Some(3));

        let preserved = admin
            .update_api_key(
                &created.id,
                crate::db::models::UpdateApiKey {
                    key: None,
                    name: None,
                    concurrency_limit: None,
                    is_enabled: None,
                    mcp_access_enabled: None,
                    transparent_injection_enabled: None,
                    inject_web_search: None,
                    inject_media_understanding: None,
                    expires_at: None,
                    model_ids: None,
                },
            )
            .await
            .expect("preserve concurrency limit");
        assert_eq!(preserved.concurrency_limit, Some(3));

        let cleared = admin
            .update_api_key(
                &created.id,
                crate::db::models::UpdateApiKey {
                    key: None,
                    name: None,
                    concurrency_limit: Some(None),
                    is_enabled: None,
                    mcp_access_enabled: None,
                    transparent_injection_enabled: None,
                    inject_web_search: None,
                    inject_media_understanding: None,
                    expires_at: None,
                    model_ids: None,
                },
            )
            .await
            .expect("clear concurrency limit");
        assert_eq!(cleared.concurrency_limit, None);

        for (name, concurrency_limit) in [
            ("Invalid zero concurrency", Some(0)),
            ("Invalid negative concurrency", Some(-1)),
        ] {
            assert!(
                admin
                    .create_api_key(crate::db::models::CreateApiKey {
                        key: None,
                        name: name.into(),
                        concurrency_limit,
                        expires_at: None,
                        mcp_access_enabled: false,
                        transparent_injection_enabled: false,
                        inject_media_understanding: false,
                        inject_web_search: false,
                        model_ids: Vec::new(),
                    })
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn api_key_crud_accepts_custom_and_replacement_keys() {
        let data_dir = tempfile::tempdir().expect("temporary data dir");
        let (gateway, _logs) = Gateway::new(crate::config::GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..Default::default()
        })
        .await
        .expect("gateway");
        let admin = gateway.admin();

        let created = admin
            .create_api_key(
                serde_json::from_value(serde_json::json!({
                    "key": " custom-client-key ",
                    "name": "Custom key"
                }))
                .expect("custom API key input"),
            )
            .await
            .expect("create custom API key");
        assert_eq!(created.token, "custom-client-key");

        let updated = admin
            .update_api_key(
                &created.id,
                serde_json::from_value(serde_json::json!({
                    "key": "replacement-client-key"
                }))
                .expect("replacement API key input"),
            )
            .await
            .expect("replace API key");
        assert_eq!(updated.token, "replacement-client-key");

        let duplicate = admin
            .create_api_key(
                serde_json::from_value(serde_json::json!({
                    "key": "replacement-client-key",
                    "name": "Duplicate key"
                }))
                .expect("duplicate API key input"),
            )
            .await
            .expect_err("duplicate API key must be rejected");
        assert!(duplicate.to_string().contains("API key already exists"));
    }

    #[tokio::test]
    async fn api_key_crud_uses_transparent_injection_without_capability_grants() {
        let data_dir = tempfile::tempdir().expect("temporary data dir");
        let (gateway, _logs) = Gateway::new(crate::config::GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..Default::default()
        })
        .await
        .expect("gateway");
        let admin = gateway.admin();

        let create = serde_json::from_value(serde_json::json!({
            "name": "Transparent injection",
            "transparent_injection_enabled": true,
            "inject_media_understanding": true,
            "inject_web_search": false,
            "model_ids": []
        }))
        .expect("new API key contract");
        let created = admin.create_api_key(create).await.expect("API key");
        let created_json = serde_json::to_value(&created).expect("API key JSON");
        assert_eq!(created_json["transparent_injection_enabled"], true);
        assert_eq!(created_json["inject_media_understanding"], true);
        assert_eq!(created_json["inject_web_search"], false);
        assert!(created_json.get("allow_web_research").is_none());
        assert!(created_json.get("allow_media_understanding").is_none());
        assert!(created_json.get("web_search_injection_enabled").is_none());

        let update = serde_json::from_value(serde_json::json!({
            "inject_web_search": true
        }))
        .expect("partial update");
        let updated = admin
            .update_api_key(&created.id, update)
            .await
            .expect("updated API key");
        let updated_json = serde_json::to_value(&updated).expect("updated API key JSON");
        assert_eq!(updated_json["transparent_injection_enabled"], true);
        assert_eq!(updated_json["inject_media_understanding"], true);
        assert_eq!(updated_json["inject_web_search"], true);

        for legacy_field in [
            "allow_web_research",
            "allow_media_understanding",
            "web_search_injection_enabled",
        ] {
            assert!(
                serde_json::from_value::<crate::db::models::CreateApiKey>(
                    serde_json::json!({ "name": "Legacy", legacy_field: true })
                )
                .is_err(),
                "{legacy_field} must not remain in the Admin contract"
            );
        }
    }
}
