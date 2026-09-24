use super::*;

#[derive(Clone)]
pub(super) struct SqliteProviderStore {
    pub(super) pool: SqlitePool,
}

#[async_trait]
impl ProviderStore for SqliteProviderStore {
    async fn list(&self) -> anyhow::Result<Vec<Provider>> {
        Ok(sqlx::query_as::<_, Provider>(
            "SELECT id, name, vendor, protocol, base_url, preset_key, channel, models_source, static_models, api_key, adapter_credentials, vendor_options, auth_mode, use_proxy, last_test_success, last_test_at, is_enabled, credential_status, credential_invalid_at, revision, created_at, updated_at FROM providers ORDER BY created_at DESC",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    async fn get(&self, id: &str) -> anyhow::Result<Option<Provider>> {
        Ok(sqlx::query_as::<_, Provider>(
            "SELECT id, name, vendor, protocol, base_url, preset_key, channel, models_source, static_models, api_key, adapter_credentials, vendor_options, auth_mode, use_proxy, last_test_success, last_test_at, is_enabled, credential_status, credential_invalid_at, revision, created_at, updated_at FROM providers WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn create(&self, input: CreateProviderRecord) -> anyhow::Result<Provider> {
        let id = stravia_runtime_contract::identifier::new_id();
        let vendor = normalize_provider_vendor(input.vendor.as_deref());
        let models_source = input.effective_models_source().map(ToString::to_string);
        if !is_valid_provider_auth_mode(&input.auth_mode) {
            anyhow::bail!("unsupported provider auth_mode: {}", input.auth_mode);
        }
        sqlx::query(
            "INSERT INTO providers (id, name, vendor, protocol, base_url, preset_key, channel, models_source, static_models, api_key, adapter_credentials, vendor_options, auth_mode, use_proxy) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&input.name)
        .bind(&vendor)
        .bind(&input.protocol)
        .bind(&input.base_url)
        .bind(&input.preset_key)
        .bind(&input.channel)
        .bind(&models_source)
        .bind(&input.static_models)
        .bind(&input.api_key)
        .bind(&input.adapter_credentials)
        .bind(&input.vendor_options)
        .bind(&input.auth_mode)
        .bind(input.use_proxy)
        .execute(&self.pool)
        .await?;
        self.get(&id)
            .await?
            .context("provider missing after create")
    }

    async fn update(&self, id: &str, input: UpdateProvider) -> anyhow::Result<Provider> {
        let current = self
            .get(id)
            .await?
            .context("provider not found for update")?;
        // ADR-0073：黑名单字段之外的写入视为新凭据证据，恢复 ok。不能无条件
        // 回写 credential_status——并发 mark 可能在本 UPDATE 之间落库。
        let reset_credential_status =
            !input.preserve_credential_status && input.resets_credential_status();
        let models_source_input = input.models_source.map(|value| value.trim().to_string());
        let name = input.name.unwrap_or(current.name);
        let vendor = if input.vendor.is_some() {
            normalize_provider_vendor(input.vendor.as_deref())
        } else {
            normalize_provider_vendor(current.vendor.as_deref())
        };
        let models_source = models_source_input.or_else(|| current.models_source.clone());
        let protocol = input.protocol.unwrap_or(current.protocol.clone());
        let base_url = input.base_url.unwrap_or(current.base_url);
        let preset_key = input.preset_key.or(current.preset_key);
        let channel = input.channel.or(current.channel);
        let static_models = input.static_models.or(current.static_models);
        let current_api_key = current.api_key;
        let adapter_credentials = input
            .adapter_credentials
            .map(|values| serde_json::to_string(&values))
            .transpose()?
            .unwrap_or(current.adapter_credentials);
        let vendor_options = input
            .vendor_options
            .map(|values| serde_json::to_string(&values))
            .transpose()?
            .unwrap_or(current.vendor_options);
        let api_key = input.api_key.unwrap_or_else(|| {
            serde_json::from_str::<std::collections::BTreeMap<String, serde_json::Value>>(
                &adapter_credentials,
            )
            .ok()
            .and_then(|values| {
                values
                    .get("apiKey")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(current_api_key)
        });
        let auth_mode = input.auth_mode.unwrap_or(current.auth_mode);
        if !is_valid_provider_auth_mode(&auth_mode) {
            anyhow::bail!("unsupported provider auth_mode: {}", auth_mode);
        }
        let use_proxy = input.use_proxy.unwrap_or(current.use_proxy);
        let is_enabled = input.is_enabled.unwrap_or(current.is_enabled);

        sqlx::query(
            "UPDATE providers SET name=?, vendor=?, protocol=?, base_url=?, preset_key=?, channel=?, models_source=?, static_models=?, api_key=?, adapter_credentials=?, vendor_options=?, auth_mode=?, use_proxy=?, is_enabled=?, updated_at=datetime('now'), revision=revision+1, credential_status = CASE WHEN ? THEN 'ok' ELSE credential_status END, credential_invalid_at = CASE WHEN ? THEN NULL ELSE credential_invalid_at END WHERE id=?",
        )
        .bind(name)
        .bind(vendor)
        .bind(&protocol)
        .bind(base_url)
        .bind(preset_key)
        .bind(channel)
        .bind(&models_source)
        .bind(static_models)
        .bind(api_key)
        .bind(adapter_credentials)
        .bind(vendor_options)
        .bind(auth_mode)
        .bind(use_proxy)
        .bind(is_enabled)
        .bind(reset_credential_status)
        .bind(reset_credential_status)
        .bind(id)
        .execute(&self.pool)
        .await?;
        self.get(id).await?.context("provider missing after update")
    }

    async fn delete(&self, id: &str) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "DELETE FROM model_backends
             WHERE provider_id = ?",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "DELETE FROM models
             WHERE NOT EXISTS (
                   SELECT 1 FROM model_backends WHERE model_id = models.id
               )",
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query("DELETE FROM providers WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(())
    }

    async fn exists_by_name(&self, name: &str, exclude_id: Option<&str>) -> anyhow::Result<bool> {
        let sql = if exclude_id.is_some() {
            "SELECT id FROM providers WHERE lower(trim(name)) = lower(trim(?)) AND id != ? LIMIT 1"
        } else {
            "SELECT id FROM providers WHERE lower(trim(name)) = lower(trim(?)) LIMIT 1"
        };
        let row = if let Some(exclude_id) = exclude_id {
            sqlx::query_scalar::<_, String>(sql)
                .bind(name)
                .bind(exclude_id)
                .fetch_optional(&self.pool)
                .await?
        } else {
            sqlx::query_scalar::<_, String>(sql)
                .bind(name)
                .fetch_optional(&self.pool)
                .await?
        };
        Ok(row.is_some())
    }

    async fn record_test_result(
        &self,
        provider_id: &str,
        result: ProviderTestResult,
    ) -> anyhow::Result<()> {
        // ADR-0073：手动测试成功是恢复路径——上游接受了当前凭据，清除失效。
        sqlx::query(
            "UPDATE providers SET last_test_success = ?, last_test_at = datetime('now'), revision = revision + 1, credential_status = CASE WHEN ? THEN 'ok' ELSE credential_status END, credential_invalid_at = CASE WHEN ? THEN NULL ELSE credential_invalid_at END WHERE id = ?",
        )
        .bind(result.success)
        .bind(result.success)
        .bind(result.success)
        .bind(provider_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn mark_credential_invalid(
        &self,
        id: &str,
        expected: ProviderCredentialVersion,
    ) -> anyhow::Result<bool> {
        // 条件写：凭据代际（providers.revision + OAuth status_version）未变
        // 才把拒绝证据归到当前凭据上，否则放弃标记。
        let result = sqlx::query(
            "UPDATE providers
             SET credential_status = 'invalid',
                 credential_invalid_at = COALESCE(credential_invalid_at, datetime('now')),
                 revision = revision + 1
             WHERE id = ?
               AND revision = ?
               AND ((? IS NULL AND NOT EXISTS (
                       SELECT 1 FROM provider_oauth_credentials WHERE provider_id = providers.id))
                    OR (? IS NOT NULL AND EXISTS (
                       SELECT 1 FROM provider_oauth_credentials
                       WHERE provider_id = providers.id AND status_version = ?)))",
        )
        .bind(id)
        .bind(expected.provider_revision)
        .bind(expected.oauth_status_version)
        .bind(expected.oauth_status_version)
        .bind(expected.oauth_status_version.unwrap_or(0))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn clear_credential_invalid(&self, id: &str) -> anyhow::Result<()> {
        // 仅在失效时落写：周期性的 OAuth 主动刷新不应空转 revision，
        // 否则会让在途请求锁定的凭据代际无故过期、丢弃真实 401 证据。
        sqlx::query(
            "UPDATE providers SET credential_status = 'ok', credential_invalid_at = NULL, revision = revision + 1 WHERE id = ? AND credential_status = 'invalid'",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn credential_invalid_provider_ids(
        &self,
    ) -> anyhow::Result<std::collections::HashSet<String>> {
        Ok(sqlx::query_scalar::<_, String>(
            "SELECT id FROM providers WHERE credential_status = 'invalid'",
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .collect())
    }
}

pub(super) fn normalize_provider_vendor(vendor: Option<&str>) -> Option<String> {
    vendor
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| v.to_lowercase())
}

#[cfg(test)]
mod tests {
    use sqlx::sqlite::SqlitePoolOptions;

    use super::*;
    use crate::db::models::{ProviderCredentialVersion, UpsertOAuthCredential};
    use crate::storage::sqlite::oauth::SqliteOAuthCredentialStore;
    use crate::storage::traits::OAuthCredentialStore;

    async fn store() -> SqliteProviderStore {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("SQLite pool");
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await
            .expect("foreign keys");
        crate::migrations::migrate_sqlite(&pool)
            .await
            .expect("migrations");
        SqliteProviderStore { pool }
    }

    async fn create_provider(store: &SqliteProviderStore, name: &str) -> Provider {
        store
            .create(CreateProviderRecord {
                name: name.into(),
                vendor: Some("openai".into()),
                protocol: "openai-compatible".into(),
                base_url: "https://example.com".into(),
                preset_key: None,
                channel: None,
                models_source: None,
                static_models: None,
                api_key: "key".into(),
                adapter_credentials: "{}".into(),
                vendor_options: "{}".into(),
                auth_mode: "apikey".into(),
                use_proxy: false,
            })
            .await
            .expect("create provider")
    }

    #[tokio::test]
    async fn credential_invalid_marking_is_conditional_on_credential_generation() {
        let store = store().await;
        let provider = create_provider(&store, "p1").await;
        assert_eq!(provider.revision, 0);

        assert!(
            !store
                .mark_credential_invalid(
                    &provider.id,
                    ProviderCredentialVersion {
                        provider_revision: provider.revision + 1,
                        oauth_status_version: None,
                    },
                )
                .await
                .expect("mark")
        );
        assert!(
            !store
                .get(&provider.id)
                .await
                .expect("get")
                .expect("provider")
                .credential_invalid()
        );

        assert!(
            store
                .mark_credential_invalid(
                    &provider.id,
                    ProviderCredentialVersion {
                        provider_revision: provider.revision,
                        oauth_status_version: None,
                    },
                )
                .await
                .expect("mark")
        );
        let marked = store
            .get(&provider.id)
            .await
            .expect("get")
            .expect("provider");
        assert!(marked.credential_invalid());
        assert!(marked.credential_invalid_at.is_some());

        assert!(
            store
                .credential_invalid_provider_ids()
                .await
                .expect("ids")
                .contains(&provider.id)
        );
    }

    #[tokio::test]
    async fn credential_invalid_marking_requires_matching_oauth_version() {
        let store = store().await;
        let provider = create_provider(&store, "p1").await;
        let oauth = SqliteOAuthCredentialStore {
            pool: store.pool.clone(),
        };
        oauth
            .upsert(
                &provider.id,
                UpsertOAuthCredential {
                    driver_key: "driver".into(),
                    scheme: "authorization_code".into(),
                    access_token: "token".into(),
                    refresh_token: None,
                    expires_at: None,
                    resource_url: None,
                    subject_id: None,
                    scopes: None,
                    meta: None,
                },
            )
            .await
            .expect("upsert oauth");
        let status_version = oauth
            .get(&provider.id)
            .await
            .expect("get oauth")
            .expect("oauth")
            .status_version;

        // OAuth 行存在但证据声称无 OAuth → 不标记
        assert!(
            !store
                .mark_credential_invalid(
                    &provider.id,
                    ProviderCredentialVersion {
                        provider_revision: provider.revision,
                        oauth_status_version: None,
                    },
                )
                .await
                .expect("mark")
        );
        // OAuth 凭据版本已前进 → 旧证据不落库
        assert!(
            !store
                .mark_credential_invalid(
                    &provider.id,
                    ProviderCredentialVersion {
                        provider_revision: provider.revision,
                        oauth_status_version: Some(status_version + 1),
                    },
                )
                .await
                .expect("mark")
        );
        assert!(
            store
                .mark_credential_invalid(
                    &provider.id,
                    ProviderCredentialVersion {
                        provider_revision: provider.revision,
                        oauth_status_version: Some(status_version),
                    },
                )
                .await
                .expect("mark")
        );
    }

    #[tokio::test]
    async fn credential_invalid_clears_only_on_credential_evidence() {
        let store = store().await;
        let provider = create_provider(&store, "p1").await;
        assert!(
            store
                .mark_credential_invalid(
                    &provider.id,
                    ProviderCredentialVersion {
                        provider_revision: provider.revision,
                        oauth_status_version: None,
                    },
                )
                .await
                .expect("mark")
        );

        // 仅 is_enabled 写入不清除
        let updated = store
            .update(
                &provider.id,
                UpdateProvider {
                    is_enabled: Some(false),
                    ..Default::default()
                },
            )
            .await
            .expect("update");
        assert!(updated.credential_invalid());

        // 管理端全字段回填 + preserve 标志 → 仍保留
        let current = store
            .get(&provider.id)
            .await
            .expect("get")
            .expect("provider");
        let updated = store
            .update(
                &provider.id,
                UpdateProvider {
                    api_key: Some(current.api_key.clone()),
                    preserve_credential_status: true,
                    ..Default::default()
                },
            )
            .await
            .expect("update");
        assert!(updated.credential_invalid());

        // 凭据字段写入 → 恢复 ok
        let updated = store
            .update(
                &provider.id,
                UpdateProvider {
                    api_key: Some("rotated".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("update");
        assert!(!updated.credential_invalid());
        assert!(updated.credential_invalid_at.is_none());
    }

    #[tokio::test]
    async fn successful_test_result_clears_credential_invalid() {
        let store = store().await;
        let provider = create_provider(&store, "p1").await;
        assert!(
            store
                .mark_credential_invalid(
                    &provider.id,
                    ProviderCredentialVersion {
                        provider_revision: provider.revision,
                        oauth_status_version: None,
                    },
                )
                .await
                .expect("mark")
        );

        store
            .record_test_result(
                &provider.id,
                crate::storage::traits::ProviderTestResult {
                    success: true,
                    tested_at: "2026-01-01T00:00:00Z".into(),
                },
            )
            .await
            .expect("record");
        let provider = store
            .get(&provider.id)
            .await
            .expect("get")
            .expect("provider");
        assert!(!provider.credential_invalid());
        assert!(provider.credential_invalid_at.is_none());
    }
}
