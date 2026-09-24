use super::*;

#[derive(Clone)]
pub(super) struct PostgresProviderStore {
    pub(super) pool: Pool<Postgres>,
}

#[async_trait]
impl ProviderStore for PostgresProviderStore {
    async fn list(&self) -> anyhow::Result<Vec<Provider>> {
        Ok(
            sqlx::query_as::<_, Provider>(sqlx::AssertSqlSafe(provider_select(None)))
                .fetch_all(&self.pool)
                .await?,
        )
    }

    async fn get(&self, id: &str) -> anyhow::Result<Option<Provider>> {
        Ok(
            sqlx::query_as::<_, Provider>(sqlx::AssertSqlSafe(provider_select(Some(
                "WHERE id = $1",
            ))))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?,
        )
    }

    async fn create(&self, input: CreateProviderRecord) -> anyhow::Result<Provider> {
        let id = stravia_runtime_contract::identifier::new_id();
        let vendor = normalize_provider_vendor(input.vendor.as_deref());
        let models_source = input.effective_models_source().map(ToString::to_string);
        if !is_valid_provider_auth_mode(&input.auth_mode) {
            anyhow::bail!("unsupported provider auth_mode: {}", input.auth_mode);
        }
        sqlx::query(
            "INSERT INTO providers (id, name, vendor, protocol, base_url, preset_key, channel, models_source, static_models, api_key, adapter_credentials, vendor_options, auth_mode, use_proxy) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
        )
        .bind(&id)
        .bind(input.name.trim())
        .bind(vendor)
        .bind(input.protocol.trim())
        .bind(input.base_url.trim())
        .bind(input.preset_key)
        .bind(input.channel)
        .bind(models_source)
        .bind(input.static_models)
        .bind(input.api_key)
        .bind(input.adapter_credentials)
        .bind(input.vendor_options)
        .bind(input.auth_mode)
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
            "UPDATE providers SET name=$1, vendor=$2, protocol=$3, base_url=$4, preset_key=$5, channel=$6, models_source=$7, static_models=$8, api_key=$9, adapter_credentials=$10, vendor_options=$11, auth_mode=$12, use_proxy=$13, is_enabled=$14, updated_at=CURRENT_TIMESTAMP, revision=revision+1, credential_status = CASE WHEN $15 THEN 'ok' ELSE credential_status END, credential_invalid_at = CASE WHEN $16 THEN NULL ELSE credential_invalid_at END WHERE id=$17",
        )
        .bind(name.trim())
        .bind(vendor)
        .bind(protocol.trim())
        .bind(base_url.trim())
        .bind(preset_key)
        .bind(channel)
        .bind(models_source)
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
        sqlx::query_scalar::<_, String>("SELECT id FROM providers WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;

        sqlx::query(
            "DELETE FROM model_backends
             WHERE provider_id = $1",
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

        sqlx::query("DELETE FROM providers WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(())
    }

    async fn exists_by_name(&self, name: &str, exclude_id: Option<&str>) -> anyhow::Result<bool> {
        let row = if let Some(exclude_id) = exclude_id {
            sqlx::query_scalar::<_, String>(
                "SELECT id FROM providers WHERE lower(trim(name)) = lower(trim($1)) AND id != $2 LIMIT 1",
            )
            .bind(name)
            .bind(exclude_id)
            .fetch_optional(&self.pool)
            .await?
        } else {
            sqlx::query_scalar::<_, String>(
                "SELECT id FROM providers WHERE lower(trim(name)) = lower(trim($1)) LIMIT 1",
            )
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
        let _ = result.tested_at;
        // ADR-0073：手动测试成功是恢复路径——上游接受了当前凭据，清除失效。
        sqlx::query(
            "UPDATE providers SET last_test_success = $1, last_test_at = CURRENT_TIMESTAMP, revision = revision + 1, credential_status = CASE WHEN $2 THEN 'ok' ELSE credential_status END, credential_invalid_at = CASE WHEN $3 THEN NULL ELSE credential_invalid_at END WHERE id = $4",
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
                 credential_invalid_at = COALESCE(credential_invalid_at, CURRENT_TIMESTAMP),
                 revision = revision + 1
             WHERE id = $1
               AND revision = $2
               AND (($3::int IS NULL AND NOT EXISTS (
                       SELECT 1 FROM provider_oauth_credentials WHERE provider_id = providers.id))
                    OR ($3::int IS NOT NULL AND EXISTS (
                       SELECT 1 FROM provider_oauth_credentials
                       WHERE provider_id = providers.id AND status_version = $3)))",
        )
        .bind(id)
        .bind(expected.provider_revision)
        .bind(expected.oauth_status_version)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn clear_credential_invalid(&self, id: &str) -> anyhow::Result<()> {
        // 仅在失效时落写：周期性的 OAuth 主动刷新不应空转 revision，
        // 否则会让在途请求锁定的凭据代际无故过期、丢弃真实 401 证据。
        sqlx::query(
            "UPDATE providers SET credential_status = 'ok', credential_invalid_at = NULL, revision = revision + 1 WHERE id = $1 AND credential_status = 'invalid'",
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

#[cfg(test)]
mod tests {
    use sqlx::postgres::PgPoolOptions;

    use super::*;
    use crate::db::models::{ProviderCredentialVersion, UpsertOAuthCredential};
    use crate::storage::postgres::oauth::PostgresOAuthCredentialStore;
    use crate::storage::traits::OAuthCredentialStore;

    async fn store() -> anyhow::Result<Option<(sqlx::PgPool, String, PostgresProviderStore)>> {
        let Ok(url) = std::env::var("DB_URL") else {
            eprintln!("skip PostgreSQL provider credential verification: DB_URL is not set");
            return Ok(None);
        };
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await?;
        let schema = format!("stravia_provider_test_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await?;
        let options: sqlx::postgres::PgConnectOptions = url.parse()?;
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options.options([("search_path", schema.as_str())]))
            .await?;
        Ok(Some((admin, schema, PostgresProviderStore { pool })))
    }

    async fn cleanup(
        admin: sqlx::PgPool,
        schema: String,
        store: &PostgresProviderStore,
    ) -> anyhow::Result<()> {
        store.pool.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&admin)
            .await?;
        admin.close().await;
        Ok(())
    }

    async fn create_provider(
        store: &PostgresProviderStore,
        name: &str,
    ) -> anyhow::Result<Provider> {
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
    }

    #[tokio::test]
    async fn postgres_credential_invalid_lifecycle() -> anyhow::Result<()> {
        let Some((admin, schema, store)) = store().await? else {
            return Ok(());
        };
        let result = async {
            crate::migrations::migrate_postgres(&store.pool).await?;
            let provider = create_provider(&store, "p1").await?;

            // 代际不匹配 → 不标记
            assert!(
                !store
                    .mark_credential_invalid(
                        &provider.id,
                        ProviderCredentialVersion {
                            provider_revision: provider.revision + 1,
                            oauth_status_version: None,
                        },
                    )
                    .await?
            );
            // 代际匹配 → 标记
            assert!(
                store
                    .mark_credential_invalid(
                        &provider.id,
                        ProviderCredentialVersion {
                            provider_revision: provider.revision,
                            oauth_status_version: None,
                        },
                    )
                    .await?
            );
            assert!(
                store
                    .credential_invalid_provider_ids()
                    .await?
                    .contains(&provider.id)
            );

            // 仅意图字段写入不清除；preserve 回填不清除；凭据写入清除
            let updated = store
                .update(
                    &provider.id,
                    UpdateProvider {
                        is_enabled: Some(false),
                        ..Default::default()
                    },
                )
                .await?;
            assert!(updated.credential_invalid());
            let updated = store
                .update(
                    &provider.id,
                    UpdateProvider {
                        api_key: Some("rotated".into()),
                        ..Default::default()
                    },
                )
                .await?;
            assert!(!updated.credential_invalid());

            // OAuth 版本条件写（upsert 顺带覆盖 timestamptz 绑定路径）
            let oauth = PostgresOAuthCredentialStore {
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
                        expires_at: Some("2025-01-02 03:04:05".into()),
                        resource_url: None,
                        subject_id: None,
                        scopes: None,
                        meta: None,
                    },
                )
                .await?;
            let provider = store.get(&provider.id).await?.expect("provider");
            let status_version = oauth
                .get(&provider.id)
                .await?
                .expect("oauth")
                .status_version;
            assert!(
                !store
                    .mark_credential_invalid(
                        &provider.id,
                        ProviderCredentialVersion {
                            provider_revision: provider.revision,
                            oauth_status_version: Some(status_version + 1),
                        },
                    )
                    .await?
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
                    .await?
            );
            Ok::<(), anyhow::Error>(())
        }
        .await;
        let cleanup = cleanup(admin, schema, &store).await;
        result?;
        cleanup?;
        Ok(())
    }
}
