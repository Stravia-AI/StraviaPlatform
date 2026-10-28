use super::*;

#[derive(Clone)]
pub(super) struct SqliteProviderStore {
    pub(super) pool: SqlitePool,
}

#[async_trait]
impl ProviderStore for SqliteProviderStore {
    async fn list(&self) -> anyhow::Result<Vec<Provider>> {
        let mut providers = sqlx::query_as::<_, Provider>(
    "SELECT id, name, vendor, protocol, base_url, preset_key, channel, models_source, static_models, api_key, adapter_credentials, vendor_options, auth_mode, use_proxy, last_test_success, last_test_at, is_enabled, credential_status, credential_invalid_at, revision, created_at, updated_at FROM providers ORDER BY created_at DESC",
            )
            .fetch_all(&self.pool)
            .await?;
        let bases = load_cost_bases(&self.pool).await?;
        for provider in &mut providers {
            provider.cost_basis = bases.get(&provider.id).copied().unwrap_or_default();
        }
        Ok(providers)
    }

    async fn get(&self, id: &str) -> anyhow::Result<Option<Provider>> {
        let mut provider = sqlx::query_as::<_, Provider>(
    "SELECT id, name, vendor, protocol, base_url, preset_key, channel, models_source, static_models, api_key, adapter_credentials, vendor_options, auth_mode, use_proxy, last_test_success, last_test_at, is_enabled, credential_status, credential_invalid_at, revision, created_at, updated_at FROM providers WHERE id = ?",
            )
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        if let Some(provider) = provider.as_mut() {
            load_cost_basis(&self.pool, provider).await?;
        }
        Ok(provider)
    }

    async fn create(&self, input: CreateProviderRecord) -> anyhow::Result<Provider> {
        let id = stravia_runtime_contract::identifier::new_id();
        let vendor = normalize_provider_vendor(input.vendor.as_deref());
        let models_source = input.effective_models_source().map(ToString::to_string);
        if !is_valid_provider_auth_mode(&input.auth_mode) {
            anyhow::bail!("unsupported provider auth_mode: {}", input.auth_mode);
        }
        let mut connection = self.pool.acquire_write().await?;
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
            .execute(&mut *connection)
            .await?;
        connection.finish().await?;
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

        let mut connection = self.pool.acquire_write().await?;
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
            .execute(&mut *connection)
            .await?;
        connection.finish().await?;
        self.get(id).await?.context("provider missing after update")
    }

    async fn delete(&self, id: &str) -> anyhow::Result<()> {
        let mut connection = self.pool.acquire_write().await?;
        let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;

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

        // A deleted subscription Provider keeps its history; close the current
        // segment with a usage record in the same transaction.
        let latest_basis: Option<String> = sqlx::query_scalar(
            "SELECT basis FROM provider_cost_plans WHERE provider_id = ? \
             ORDER BY effective_from DESC, id DESC LIMIT 1",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        if latest_basis.as_deref() == Some("subscription") {
            let now = chrono::Utc::now();
            let previous: i64 = sqlx::query_scalar(
                "SELECT COALESCE(MAX(effective_from), 0) FROM provider_cost_plans \
                 WHERE provider_id = ?",
            )
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            let effective_from = now.timestamp_millis().max(
                previous
                    .checked_add(1)
                    .context("cost plan timestamp overflow")?,
            );
            sqlx::query(
                "INSERT INTO provider_cost_plans (id, provider_id, effective_from, basis, period, \
                 price_usd_nanos, price_custom_nanos, price_currency, created_at) \
                 VALUES (?, ?, ?, 'usage', NULL, NULL, NULL, NULL, ?)",
            )
            .bind(stravia_runtime_contract::identifier::new_id())
            .bind(id)
            .bind(effective_from)
            .bind(now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
            .execute(&mut *tx)
            .await?;
        }

        sqlx::query("DELETE FROM providers WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        connection.finish().await?;
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
        expected: ProviderCredentialVersion,
        result: ProviderConnectionCheckWrite,
    ) -> anyhow::Result<bool> {
        // ADR-0073：只有本次检查实际验证了凭据，成功才构成恢复证据；
        // 条件写保证迟到的检查不能覆盖更新的凭据/配置代际。
        let mut connection = self.pool.acquire_write().await?;
        let updated = sqlx::query(
            "UPDATE providers
     SET last_test_success = ?,
         last_test_at = datetime('now'),
         credential_status = CASE WHEN ? AND ? THEN 'ok' ELSE credential_status END,
         credential_invalid_at = CASE WHEN ? AND ? THEN NULL ELSE credential_invalid_at END,
         revision = revision + 1
     WHERE id = ?
       AND revision = ?
       AND ((? IS NULL AND NOT EXISTS (
               SELECT 1 FROM provider_oauth_credentials WHERE provider_id = providers.id))
            OR (? IS NOT NULL AND EXISTS (
               SELECT 1 FROM provider_oauth_credentials
               WHERE provider_id = providers.id AND status_version = ?)))",
        )
        .bind(result.success)
        .bind(result.success)
        .bind(result.credentials_verified)
        .bind(result.success)
        .bind(result.credentials_verified)
        .bind(provider_id)
        .bind(expected.provider_revision)
        .bind(expected.oauth_status_version)
        .bind(expected.oauth_status_version)
        .bind(expected.oauth_status_version.unwrap_or(0))
        .execute(&mut *connection)
        .await?;
        connection.finish().await?;
        Ok(updated.rows_affected() == 1)
    }

    async fn guarded_allowance_keys(&self, id: &str) -> anyhow::Result<Vec<String>> {
        Ok(sqlx::query_scalar("SELECT allowance_key FROM provider_allowance_guards WHERE provider_id = ? ORDER BY allowance_key").bind(id).fetch_all(&self.pool).await?)
    }

    async fn replace_guarded_allowance_keys(
        &self,
        id: &str,
        keys: &[String],
    ) -> anyhow::Result<()> {
        let keys_json = serde_json::to_string(keys)?;
        let mut connection = self.pool.acquire_write().await?;
        let mut tx = connection.begin().await?;
        // Acquire the SQLite writer lock before reading the suspension.
        sqlx::query("DELETE FROM provider_allowance_guards WHERE provider_id = ? AND allowance_key NOT IN (SELECT value FROM json_each(?))").bind(id).bind(keys_json).execute(&mut *tx).await?;
        let provider = sqlx::query_scalar::<_, String>("SELECT id FROM providers WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        anyhow::ensure!(provider.is_some(), "provider not found");
        for key in keys {
            sqlx::query("INSERT INTO provider_allowance_guards (provider_id, allowance_key) VALUES (?, ?) ON CONFLICT DO NOTHING").bind(id).bind(key).execute(&mut *tx).await?;
        }
        // Removed triggers may own the aggregate reset time; a fresh read must
        // establish the remaining triggers' earliest reset rather than guessing.
        sqlx::query("UPDATE provider_allowance_suspensions SET earliest_reset_at = CASE WHEN EXISTS (SELECT value FROM json_each(triggered_keys) WHERE value NOT IN (SELECT allowance_key FROM provider_allowance_guards WHERE provider_id = ?)) THEN NULL ELSE earliest_reset_at END, triggered_keys = (SELECT json_group_array(value) FROM json_each(triggered_keys) WHERE value IN (SELECT allowance_key FROM provider_allowance_guards WHERE provider_id = ?)) WHERE provider_id = ?").bind(id).bind(id).bind(id).execute(&mut *tx).await?;
        sqlx::query("UPDATE provider_allowance_suspensions SET suspended = FALSE, suspended_at = NULL, earliest_reset_at = NULL WHERE provider_id = ? AND json_array_length(triggered_keys) = 0").bind(id).execute(&mut *tx).await?;
        tx.commit().await?;
        connection.finish().await?;
        Ok(())
    }

    async fn write_allowance_suspension(
        &self,
        id: &str,
        expected: ProviderCredentialVersion,
        state: &crate::db::models::AllowanceSuspensionState,
        expected_guarded_keys: &[String],
    ) -> anyhow::Result<bool> {
        let triggered_keys_json = serde_json::to_string(&state.triggered_keys)?;
        let guarded_keys_json = serde_json::to_string(expected_guarded_keys)?;
        let mut connection = self.pool.acquire_write().await?;
        let result = sqlx::query(
            "INSERT INTO provider_allowance_suspensions (provider_id, suspended, suspended_at, triggered_keys, earliest_reset_at, evidence_completed_at)
             SELECT ?, ?, ?, ?, ?, ? FROM providers WHERE id = ? AND revision = ?
             AND ((? IS NULL AND NOT EXISTS (SELECT 1 FROM provider_oauth_credentials WHERE provider_id = providers.id))
               OR (? IS NOT NULL AND EXISTS (SELECT 1 FROM provider_oauth_credentials WHERE provider_id = providers.id AND status_version = ?)))
             AND NOT EXISTS (SELECT allowance_key FROM provider_allowance_guards WHERE provider_id = providers.id EXCEPT SELECT value FROM json_each(?))
             AND NOT EXISTS (SELECT value FROM json_each(?) EXCEPT SELECT allowance_key FROM provider_allowance_guards WHERE provider_id = providers.id)
             ON CONFLICT (provider_id) DO UPDATE SET suspended = excluded.suspended,
             suspended_at = CASE WHEN provider_allowance_suspensions.suspended AND excluded.suspended THEN provider_allowance_suspensions.suspended_at ELSE excluded.suspended_at END,
             triggered_keys = excluded.triggered_keys, earliest_reset_at = excluded.earliest_reset_at, evidence_completed_at = excluded.evidence_completed_at
             WHERE excluded.evidence_completed_at > provider_allowance_suspensions.evidence_completed_at")
            .bind(id).bind(state.suspended).bind(&state.suspended_at).bind(triggered_keys_json).bind(state.earliest_reset_at).bind(state.evidence_completed_at)
            .bind(id).bind(expected.provider_revision).bind(expected.oauth_status_version).bind(expected.oauth_status_version).bind(expected.oauth_status_version.unwrap_or(0))
            .bind(&guarded_keys_json).bind(&guarded_keys_json).execute(&mut *connection).await?;
        connection.finish().await?;
        Ok(result.rows_affected() == 1)
    }

    async fn allowance_suspension(
        &self,
        id: &str,
    ) -> anyhow::Result<Option<crate::db::models::AllowanceSuspensionState>> {
        let row = sqlx::query_as::<_, (bool, Option<String>, String, Option<i64>, i64)>("SELECT suspended, suspended_at, triggered_keys, earliest_reset_at, evidence_completed_at FROM provider_allowance_suspensions WHERE provider_id = ?").bind(id).fetch_optional(&self.pool).await?;
        row.map(
            |(suspended, suspended_at, json, earliest_reset_at, evidence_completed_at)| {
                Ok(crate::db::models::AllowanceSuspensionState {
                    suspended,
                    suspended_at,
                    triggered_keys: serde_json::from_str(&json)?,
                    earliest_reset_at,
                    evidence_completed_at,
                })
            },
        )
        .transpose()
    }

    async fn allowance_suspended_provider_ids(
        &self,
    ) -> anyhow::Result<std::collections::HashSet<String>> {
        Ok(sqlx::query_scalar::<_, String>(
            "SELECT provider_id FROM provider_allowance_suspensions WHERE suspended = TRUE",
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .collect())
    }

    async fn mark_credential_invalid(
        &self,
        id: &str,
        expected: ProviderCredentialVersion,
    ) -> anyhow::Result<bool> {
        // 条件写：凭据代际（providers.revision + OAuth status_version）未变
        // 才把拒绝证据归到当前凭据上，否则放弃标记。
        let mut connection = self.pool.acquire_write().await?;
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
        .execute(&mut *connection)
        .await?;
        connection.finish().await?;
        Ok(result.rows_affected() == 1)
    }

    async fn clear_credential_invalid(&self, id: &str) -> anyhow::Result<()> {
        // 仅在失效时落写：周期性的 OAuth 主动刷新不应空转 revision，
        // 否则会让在途请求锁定的凭据代际无故过期、丢弃真实 401 证据。
        let mut connection = self.pool.acquire_write().await?;
        sqlx::query(
    "UPDATE providers SET credential_status = 'ok', credential_invalid_at = NULL, revision = revision + 1 WHERE id = ? AND credential_status = 'invalid'",
            )
            .bind(id)
            .execute(&mut *connection)
            .await?;
        connection.finish().await?;
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

async fn load_cost_basis(pool: &SqlitePool, provider: &mut Provider) -> anyhow::Result<()> {
    let basis: Option<String> = sqlx::query_scalar(
        "SELECT basis FROM provider_cost_plans WHERE provider_id = ? \
         ORDER BY effective_from DESC, id DESC LIMIT 1",
    )
    .bind(&provider.id)
    .fetch_optional(pool)
    .await?;
    provider.cost_basis = basis
        .as_deref()
        .map(crate::storage::money::parse_cost_basis)
        .transpose()?
        .unwrap_or_default();
    Ok(())
}

async fn load_cost_bases(
    pool: &SqlitePool,
) -> anyhow::Result<std::collections::HashMap<String, crate::pricing::CostBasis>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT provider_id, basis FROM (\
           SELECT provider_id, basis, \
                  ROW_NUMBER() OVER (\
                    PARTITION BY provider_id ORDER BY effective_from DESC, id DESC\
                  ) AS row_number \
           FROM provider_cost_plans\
         ) WHERE row_number = 1",
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|(provider_id, basis)| {
            Ok((
                provider_id,
                crate::storage::money::parse_cost_basis(&basis)?,
            ))
        })
        .collect()
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
        crate::migrations::migrate_sqlite(&pool, None)
            .await
            .expect("migrations");
        SqliteProviderStore { pool }
    }

    #[tokio::test]
    async fn oauth_refresh_recovery_storage_contract() -> anyhow::Result<()> {
        let store = store().await;
        let provider = create_provider(&store, "oauth-recovery").await;
        let oauth = SqliteOAuthCredentialStore {
            pool: store.pool.clone(),
        };
        crate::storage::traits::verify_oauth_refresh_recovery_storage_contract(&oauth, &provider.id)
            .await
    }

    #[tokio::test]
    async fn allowance_suspension_storage_contract() -> anyhow::Result<()> {
        let store = store().await;
        let provider = create_provider(&store, "allowance").await;
        let oauth = SqliteOAuthCredentialStore {
            pool: store.pool.clone(),
        };
        crate::storage::traits::verify_allowance_storage_contract(&store, &oauth, provider).await
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
    async fn successful_test_result_clears_credential_invalid_only_with_credentials_verified() {
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
        let invalid = store
            .get(&provider.id)
            .await
            .expect("get")
            .expect("provider");

        // Endpoint-only success records the probe but cannot clear credential evidence.
        assert!(
            store
                .record_test_result(
                    &provider.id,
                    ProviderCredentialVersion {
                        provider_revision: invalid.revision,
                        oauth_status_version: None,
                    },
                    ProviderConnectionCheckWrite {
                        success: true,
                        credentials_verified: false,
                    },
                )
                .await
                .expect("record endpoint-only")
        );
        let endpoint_only = store
            .get(&provider.id)
            .await
            .expect("get")
            .expect("provider");
        assert!(endpoint_only.credential_invalid());
        assert!(endpoint_only.credential_invalid_at.is_some());
        assert_eq!(endpoint_only.last_test_success, Some(true));

        assert!(
            store
                .record_test_result(
                    &provider.id,
                    ProviderCredentialVersion {
                        provider_revision: endpoint_only.revision,
                        oauth_status_version: None,
                    },
                    ProviderConnectionCheckWrite {
                        success: true,
                        credentials_verified: true,
                    },
                )
                .await
                .expect("record verified")
        );
        let recovered = store
            .get(&provider.id)
            .await
            .expect("get")
            .expect("provider");
        assert!(!recovered.credential_invalid());
        assert!(recovered.credential_invalid_at.is_none());
    }
}
