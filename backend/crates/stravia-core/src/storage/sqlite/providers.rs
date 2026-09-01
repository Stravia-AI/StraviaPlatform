use super::*;

#[derive(Clone)]
pub(super) struct SqliteProviderStore {
    pub(super) pool: SqlitePool,
}

#[async_trait]
impl ProviderStore for SqliteProviderStore {
    async fn list(&self) -> anyhow::Result<Vec<Provider>> {
        Ok(sqlx::query_as::<_, Provider>(
            "SELECT id, name, vendor, protocol, base_url, preset_key, channel, models_source, static_models, api_key, COALESCE(adapter_credentials, '{}') AS adapter_credentials, COALESCE(auth_mode, 'apikey') AS auth_mode, COALESCE(use_proxy, 0) AS use_proxy, last_test_success, last_test_at, COALESCE(is_enabled, 1) AS is_enabled, created_at, updated_at FROM providers ORDER BY created_at DESC",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    async fn get(&self, id: &str) -> anyhow::Result<Option<Provider>> {
        Ok(sqlx::query_as::<_, Provider>(
            "SELECT id, name, vendor, protocol, base_url, preset_key, channel, models_source, static_models, api_key, COALESCE(adapter_credentials, '{}') AS adapter_credentials, COALESCE(auth_mode, 'apikey') AS auth_mode, COALESCE(use_proxy, 0) AS use_proxy, last_test_success, last_test_at, COALESCE(is_enabled, 1) AS is_enabled, created_at, updated_at FROM providers WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn create(&self, input: CreateProviderRecord) -> anyhow::Result<Provider> {
        let id = uuid::Uuid::new_v4().to_string();
        let vendor = normalize_provider_vendor(input.vendor.as_deref());
        let models_source = input.effective_models_source().map(ToString::to_string);
        if !is_valid_provider_auth_mode(&input.auth_mode) {
            anyhow::bail!("unsupported provider auth_mode: {}", input.auth_mode);
        }
        sqlx::query(
            "INSERT INTO providers (id, name, vendor, protocol, base_url, preset_key, channel, models_source, static_models, api_key, adapter_credentials, auth_mode, use_proxy) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
        let api_key = input.api_key.unwrap_or_else(|| {
            serde_json::from_str::<std::collections::BTreeMap<String, String>>(&adapter_credentials)
                .ok()
                .and_then(|values| values.get("apiKey").cloned())
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
            "UPDATE providers SET name=?, vendor=?, protocol=?, base_url=?, preset_key=?, channel=?, models_source=?, static_models=?, api_key=?, adapter_credentials=?, auth_mode=?, use_proxy=?, is_enabled=?, updated_at=datetime('now') WHERE id=?",
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
        .bind(auth_mode)
        .bind(use_proxy)
        .bind(is_enabled)
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
        sqlx::query(
            "UPDATE providers SET last_test_success = ?, last_test_at = datetime('now') WHERE id = ?",
        )
        .bind(result.success)
        .bind(provider_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

pub(super) fn normalize_provider_vendor(vendor: Option<&str>) -> Option<String> {
    vendor
        .map(str::trim)
        .filter(|v| !v.is_empty() && *v != "custom")
        .map(|v| v.to_lowercase())
}
