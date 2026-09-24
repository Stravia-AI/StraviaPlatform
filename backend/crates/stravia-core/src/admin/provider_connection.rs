use super::*;
use rust_decimal::prelude::ToPrimitive;

mod capabilities;
mod configuration;
mod interface;

use configuration::{
    NormalizedConfiguration, require_descriptor, resolved_permissions,
    validate_configuration_fields, validate_persisted_configuration_fields,
    validate_provider_base_url,
};
pub use configuration::{
    ProviderConfigurationPreview, ProviderConfigurationPreviewInput, ProviderNetworkPermission,
};
pub(crate) use interface::{
    ProviderConnection, ProviderConnectivityTest, ProviderReconnect, ProviderReconnectCallback,
    ProviderReconnectResult, ProviderReconnectStart, ProviderSave,
};

fn configured_secret_value(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(value) => !value.trim().is_empty(),
        _ => true,
    }
}

/// ADR-0073：凭据健康字段（credential_status/credential_invalid_at/revision）
/// 不属于配置代际——并发 mark/clear 不应中断进行中的 OAuth 绑定、刷新或
/// 配置保存校验，这些写入本身就携带新凭据证据。
pub(in crate::admin) fn same_provider_generation(left: &Provider, right: &Provider) -> bool {
    left.id == right.id
        && left.name == right.name
        && left.vendor == right.vendor
        && left.protocol == right.protocol
        && left.base_url == right.base_url
        && left.preset_key == right.preset_key
        && left.channel == right.channel
        && left.models_source == right.models_source
        && left.static_models == right.static_models
        && left.api_key == right.api_key
        && left.adapter_credentials == right.adapter_credentials
        && left.vendor_options == right.vendor_options
        && left.auth_mode == right.auth_mode
        && left.use_proxy == right.use_proxy
        && left.last_test_success == right.last_test_success
        && left.last_test_at == right.last_test_at
        && left.is_enabled == right.is_enabled
        && left.created_at == right.created_at
        && left.updated_at == right.updated_at
}

fn legacy_credential_field<'a>(
    descriptor: &'a stravia_vendor_sdk::ProviderDescriptor,
    preferred: &str,
) -> anyhow::Result<&'a str> {
    if let Some(field) = descriptor
        .config_fields
        .iter()
        .find(|field| field.secret && field.key == preferred)
    {
        return Ok(&field.key);
    }
    let mut candidates = descriptor
        .config_fields
        .iter()
        .filter(|field| field.secret)
        .filter(|field| {
            matches!(
                &field.kind,
                stravia_vendor_sdk::ConfigFieldKind::String { .. }
            )
        });
    let candidate = candidates
        .next()
        .ok_or_else(|| anyhow::anyhow!("Vendor does not declare a string credential field"))?;
    anyhow::ensure!(
        candidates.next().is_none(),
        "Vendor declares multiple credential fields; submit named credential values"
    );
    Ok(&candidate.key)
}

pub(super) fn ensure_configuration_accepted(
    preview: &ProviderConfigurationPreview,
    saved_base_url: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        preview.issues.is_empty(),
        "vendor configuration validation failed: {}",
        serde_json::to_string(&preview.issues)?
    );
    anyhow::ensure!(
        preview.base_url == saved_base_url,
        "vendor proposed Base URL `{}`; preview and explicitly save that URL before continuing",
        preview.base_url
    );
    Ok(())
}

impl AdminService {
    // ── Providers ──

    pub async fn preview_provider_configuration(
        &self,
        input: ProviderConfigurationPreviewInput,
    ) -> anyhow::Result<ProviderConfigurationPreview> {
        let descriptor = require_descriptor(&self.gw.vendor_plugins, &input.vendor_id)?;
        anyhow::ensure!(
            input.provider_id.is_some()
                || !self.gw.vendor_plugins.is_retired_profile(&input.vendor_id),
            "Vendor `{}` is no longer available for new providers",
            input.vendor_id
        );
        let channel = descriptor
            .channels
            .iter()
            .find(|channel| channel.id == input.channel)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Vendor `{}` does not declare channel `{}`",
                    descriptor.provider_id,
                    input.channel
                )
            })?;

        let mut credentials = input.credentials;
        let protocol = if let Some(provider_id) = input.provider_id.as_deref() {
            let provider = self.get_provider(provider_id).await?;
            anyhow::ensure!(
                provider.vendor.as_deref() == Some(descriptor.provider_id.as_str())
                    && provider.channel.as_deref() == Some(input.channel.as_str()),
                "provider identity does not match the configuration preview"
            );
            let saved_credentials = self
                .provider_configuration_credentials(&provider, &descriptor)
                .await?;
            for field in descriptor.config_fields.iter().filter(|field| field.secret) {
                if credentials
                    .get(&field.key)
                    .is_none_or(|value| !configured_secret_value(value))
                {
                    credentials.remove(&field.key);
                    if let Some(value) = saved_credentials
                        .get(&field.key)
                        .filter(|value| configured_secret_value(value))
                    {
                        credentials.insert(field.key.clone(), value.clone());
                    }
                }
            }
            provider.protocol
        } else {
            channel.protocol.clone().unwrap_or_default()
        };
        let supports_config_validation = channel
            .capabilities
            .contains(&stravia_vendor_sdk::Capability::ConfigValidation);
        // An empty candidate asks the guest to derive a URL from declared fields. The
        // ConfigValidation session is still scoped from this empty snapshot, so the
        // guest's proposal cannot grant that same execution a new base origin.
        let mut base_url = if input.base_url.trim().is_empty() && supports_config_validation {
            String::new()
        } else {
            validate_provider_base_url(&input.base_url)?
        };
        let mut relaxed_descriptor;
        let validation_descriptor = if channel.auth.is_some() {
            relaxed_descriptor = descriptor.clone();
            for field in &mut relaxed_descriptor.config_fields {
                if field.secret {
                    field.required = false;
                }
            }
            &relaxed_descriptor
        } else {
            &descriptor
        };
        let NormalizedConfiguration {
            options,
            credentials,
            mut issues,
        } = validate_configuration_fields(validation_descriptor, input.options, credentials)?;

        if issues.is_empty() && supports_config_validation {
            let provider = stravia_vendor_sdk::ProviderSnapshot {
                provider_id: descriptor.provider_id.clone(),
                channel: input.channel,
                base_url: base_url.clone(),
                protocol,
                options: options.clone(),
                credentials: credentials.clone(),
                model: None,
                model_metadata: None,
                client_headers: Vec::new(),
                operation_metadata: std::collections::BTreeMap::new(),
            };
            let scope = self
                .gw
                .create_vendor_session_scope(&descriptor.provider_id, provider)?;
            let execution = self
                .gw
                .execute_vendor_session(
                    &scope,
                    crate::plugin::VendorRequest::ConfigValidation(
                        stravia_vendor_sdk::ConfigValidationRequest {
                            options: options.clone(),
                        },
                    ),
                    crate::plugin::VendorCallContext::new(
                        stravia_runtime_contract::CancellationToken::new(),
                        stravia_runtime_contract::Deadline::fixed(
                            std::time::Instant::now() + std::time::Duration::from_secs(30),
                        ),
                    ),
                )
                .await?;
            let _publication = execution.publication.write_fence().await?;
            let stravia_vendor_sdk::OperationOutput::ConfigValidation(validation) =
                execution.output
            else {
                anyhow::bail!("vendor returned the wrong configuration validation result")
            };
            if let Some(proposed) = validation.proposed_base_url {
                base_url = validate_provider_base_url(&proposed)?;
            }
            issues = validation.issues;
        }

        if issues.is_empty() {
            base_url = validate_provider_base_url(&base_url)?;
        }
        let network_permissions =
            resolved_permissions(&descriptor, &base_url, &options, &credentials)?;
        Ok(ProviderConfigurationPreview {
            base_url,
            issues,
            network_permissions,
        })
    }

    pub(in crate::admin) async fn provider_auth_candidate_snapshot(
        &self,
        candidate: &AuthSessionCandidate,
    ) -> anyhow::Result<(
        stravia_vendor_sdk::ProviderSnapshot,
        stravia_vendor_sdk::AuthDescriptor,
    )> {
        let descriptor = require_descriptor(&self.gw.vendor_plugins, &candidate.vendor_id)?;
        anyhow::ensure!(
            candidate.provider_id.is_some()
                || !self
                    .gw
                    .vendor_plugins
                    .is_retired_profile(&candidate.vendor_id),
            "Vendor `{}` is no longer available for new providers",
            candidate.vendor_id
        );
        let channel_id = candidate.channel.as_str();
        let mut credentials = candidate.credentials.clone();
        let channel = descriptor
            .channels
            .iter()
            .find(|candidate| candidate.id == channel_id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Vendor `{}` does not declare channel `{channel_id}`",
                    descriptor.provider_id
                )
            })?;
        anyhow::ensure!(
            channel
                .capabilities
                .contains(&stravia_vendor_sdk::Capability::AuthOauth),
            "Vendor channel does not support authentication"
        );
        let auth = channel
            .auth
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Vendor channel does not declare authentication"))?;

        let existing_protocol = if let Some(provider_id) = candidate.provider_id.as_deref() {
            let provider = self.get_provider(provider_id).await?;
            anyhow::ensure!(
                provider.vendor.as_deref() == Some(descriptor.provider_id.as_str())
                    && provider.channel.as_deref() == Some(channel_id),
                "provider identity does not match the authentication candidate"
            );
            let saved_credentials = self
                .provider_configuration_credentials(&provider, &descriptor)
                .await?;
            for field in descriptor.config_fields.iter().filter(|field| field.secret) {
                if credentials
                    .get(&field.key)
                    .is_none_or(|value| !configured_secret_value(value))
                {
                    credentials.remove(&field.key);
                    if let Some(value) = saved_credentials
                        .get(&field.key)
                        .filter(|value| configured_secret_value(value))
                    {
                        credentials.insert(field.key.clone(), value.clone());
                    }
                }
            }
            Some(provider.protocol)
        } else {
            None
        };
        let protocol = candidate
            .protocol
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .or(existing_protocol)
            .or_else(|| channel.protocol.clone())
            .unwrap_or_default();
        // 登录必须使用管理员明确提交的地址，不能把未展示的 Guest 建议变成认证授权。
        let base_url = validate_provider_base_url(&candidate.base_url)?;
        let mut auth_candidate_descriptor = descriptor.clone();
        for field in &mut auth_candidate_descriptor.config_fields {
            if field.secret {
                field.required = false;
            }
        }
        let NormalizedConfiguration {
            options,
            credentials,
            issues,
        } = validate_configuration_fields(
            &auth_candidate_descriptor,
            candidate.options.clone(),
            credentials,
        )?;
        anyhow::ensure!(
            issues.is_empty(),
            "vendor configuration validation failed: {}",
            serde_json::to_string(&issues)?
        );
        Ok((
            stravia_vendor_sdk::ProviderSnapshot {
                provider_id: descriptor.provider_id.clone(),
                channel: channel_id.to_owned(),
                base_url,
                protocol,
                options,
                credentials,
                model: None,
                model_metadata: None,
                client_headers: Vec::new(),
                operation_metadata: std::collections::BTreeMap::new(),
            },
            auth,
        ))
    }

    pub async fn configured_provider_credential_fields(
        &self,
        provider: &Provider,
    ) -> anyhow::Result<Vec<String>> {
        let Some(descriptor) = provider
            .vendor
            .as_deref()
            .and_then(|vendor_id| self.gw.vendor_plugins.descriptor(vendor_id).ok())
        else {
            // An unavailable package/profile must not hide its saved connection. Do not
            // guess field semantics from another descriptor or expose unclassified keys;
            // restoring the owning profile makes the configured field names visible again.
            return Ok(Vec::new());
        };
        let values = self
            .provider_configuration_credentials(provider, &descriptor)
            .await?;
        let mut configured: Vec<_> = descriptor
            .config_fields
            .iter()
            .filter(|field| field.secret)
            .filter(|field| values.get(&field.key).is_some_and(configured_secret_value))
            .map(|field| field.key.clone())
            .collect();
        configured.sort();
        Ok(configured)
    }

    async fn provider_configuration_credentials(
        &self,
        provider: &Provider,
        descriptor: &stravia_vendor_sdk::ProviderDescriptor,
    ) -> anyhow::Result<std::collections::BTreeMap<String, Value>> {
        let mut values: std::collections::BTreeMap<String, Value> =
            serde_json::from_str(&provider.adapter_credentials)?;
        if !provider.api_key.trim().is_empty()
            && let Ok(key) = legacy_credential_field(descriptor, "apiKey")
        {
            values
                .entry(key.to_owned())
                .or_insert_with(|| Value::String(provider.api_key.clone()));
        }
        if let Some(oauth) = self
            .gw
            .storage
            .oauth_credentials()
            .get(&provider.id)
            .await?
        {
            if let Value::Object(meta) = serde_json::from_str::<Value>(&oauth.meta)? {
                values.extend(meta);
            }
            values.insert("access_token".into(), Value::String(oauth.access_token));
            if let Some(value) = oauth.refresh_token {
                values.insert("refresh_token".into(), Value::String(value));
            }
            if let Some(value) = oauth.resource_url {
                values.insert("resource_url".into(), Value::String(value));
            }
            values.insert(
                "scopes".into(),
                serde_json::from_str(&oauth.scopes).unwrap_or(Value::Array(Vec::new())),
            );
        }
        Ok(values)
    }

    pub async fn list_providers(&self) -> anyhow::Result<Vec<Provider>> {
        ProviderConnection::new(self).list().await
    }

    pub async fn get_provider(&self, id: &str) -> anyhow::Result<Provider> {
        ProviderConnection::new(self).get(id).await
    }
    pub async fn provider_requires_oauth_session(
        &self,
        input: &CreateProvider,
    ) -> anyhow::Result<bool> {
        ProviderConnection::new(self)
            .requires_oauth_session(input)
            .await
    }

    pub async fn create_provider(&self, input: CreateProvider) -> anyhow::Result<Provider> {
        let save = match &input.source {
            ProviderSourceInput::Catalog { .. } => ProviderSave::Catalog {
                input,
                authorization_id: None,
            },
            ProviderSourceInput::Custom { .. } => ProviderSave::Custom(input),
        };
        ProviderConnection::new(self).save(save).await
    }

    pub(super) async fn create_provider_from_input(
        &self,
        input: CreateProvider,
        allow_oauth: bool,
    ) -> anyhow::Result<Provider> {
        let (record, descriptor) = self.resolve_create_provider(input, allow_oauth).await?;
        self.ensure_provider_name_unique(None, &record.name).await?;
        let vendor_id = record
            .vendor
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("provider vendor is required"))?;
        let _configuration = self.gw.vendor_plugins.configuration_guard(vendor_id).await;
        let (loaded, operation, _) = self.gw.vendor_plugins.acquire(vendor_id)?;
        anyhow::ensure!(
            loaded.descriptor().provider(vendor_id) == Some(&descriptor),
            "vendor plugin changed while validating provider configuration"
        );
        let _permit = operation.write_permit().await?;
        let provider = self.gw.storage.providers().create(record).await?;
        Ok(provider)
    }

    async fn resolve_create_provider(
        &self,
        input: CreateProvider,
        allow_oauth: bool,
    ) -> anyhow::Result<(CreateProviderRecord, stravia_vendor_sdk::ProviderDescriptor)> {
        let CreateProvider {
            name,
            source,
            credential,
            vendor_options,
            use_proxy,
        } = input;
        match source {
            ProviderSourceInput::Catalog {
                provider_id,
                channel_id,
                fingerprint,
                base_url_override,
            } => {
                let descriptors = self.gw.vendor_plugins.descriptors();
                let (provider, channel) = self
                    .gw
                    .provider_catalog
                    .resolve_channel(&provider_id, &channel_id, &fingerprint, &descriptors)
                    .await?;
                let descriptor = descriptors
                    .into_iter()
                    .find(|descriptor| descriptor.provider_id == provider.id)
                    .ok_or_else(|| {
                        anyhow::anyhow!("Provider profile `{}` is not installed", provider.id)
                    })?;
                let declared_channel = descriptor
                    .channels
                    .iter()
                    .find(|declared| declared.id == channel.id);
                let declared_models_source = declared_channel
                    .and_then(|declared| declared.default_models_source)
                    .map(|source| source.as_str().to_owned());
                let uses_catalog_scope = match provider.catalog_id.as_deref() {
                    Some(catalog_id) => {
                        self.gw.provider_catalog.contains_provider(catalog_id).await
                    }
                    None => false,
                };
                // Persisting the catalog marker is only meaningful for
                // channels that consume the injected scope; account-discovery
                // channels resolve it back into a live upstream request, so
                // storing it would misreport the model source.
                let models_source = (uses_catalog_scope
                    && declared_channel.is_some_and(|declared| declared.consumes_catalog_models))
                .then(|| stravia_vendor_sdk::MODELS_SOURCE_CATALOG.to_owned())
                .or(declared_models_source);
                let name =
                    normalize_name(name.as_deref().unwrap_or(&provider.name), "provider name")?;
                let (credentials, auth_mode) = match (channel.auth_mode, credential) {
                    (
                        crate::provider_catalog::CatalogAuthMode::OptionalApiKey,
                        ProviderCredentialInput::ApiKey { value },
                    ) => (
                        std::collections::BTreeMap::from([(
                            legacy_credential_field(&descriptor, "apiKey")?.to_owned(),
                            Value::String(value),
                        )]),
                        "apikey".to_string(),
                    ),
                    (
                        crate::provider_catalog::CatalogAuthMode::OptionalApiKey,
                        ProviderCredentialInput::Fields { values },
                    ) => (values, "apikey".to_string()),
                    (
                        crate::provider_catalog::CatalogAuthMode::OptionalApiKey,
                        ProviderCredentialInput::None,
                    ) => (std::collections::BTreeMap::new(), "apikey".to_string()),
                    (
                        crate::provider_catalog::CatalogAuthMode::SetupToken,
                        ProviderCredentialInput::SetupToken { value },
                    ) => (
                        std::collections::BTreeMap::from([(
                            legacy_credential_field(&descriptor, "setup_token")?.to_owned(),
                            Value::String(value),
                        )]),
                        "apikey".to_string(),
                    ),
                    (
                        crate::provider_catalog::CatalogAuthMode::OAuth,
                        ProviderCredentialInput::None,
                    ) if allow_oauth => (std::collections::BTreeMap::new(), "oauth".to_string()),
                    (
                        crate::provider_catalog::CatalogAuthMode::OAuth,
                        ProviderCredentialInput::Fields { values },
                    ) if allow_oauth => (values, "oauth".to_string()),
                    (
                        crate::provider_catalog::CatalogAuthMode::OAuth,
                        ProviderCredentialInput::None,
                    ) => anyhow::bail!(
                        r#"{{"code":"AUTH_SESSION_REQUIRED","message":"OAuth providers must be created from a completed OAuth session"}}"#
                    ),
                    _ => anyhow::bail!(
                        "credential type is not allowed for catalog channel {provider_id}/{channel_id}"
                    ),
                };
                let (vendor_options, credentials) = validate_persisted_configuration_fields(
                    &descriptor,
                    vendor_options,
                    credentials,
                    auth_mode == "oauth",
                )?;
                let adapter_credentials = serde_json::to_string(&credentials)?;
                let api_key = credentials
                    .get("apiKey")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let selected_base_url = base_url_override
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .unwrap_or(&channel.base_url);
                let base_url = validate_provider_base_url(selected_base_url)?;
                if auth_mode != "oauth" {
                    let preview = self
                        .preview_provider_configuration(ProviderConfigurationPreviewInput {
                            provider_id: None,
                            vendor_id: provider.id.clone(),
                            channel: channel.id.clone(),
                            base_url: base_url.clone(),
                            options: vendor_options.clone().into_iter().collect(),
                            credentials: credentials.clone(),
                        })
                        .await?;
                    ensure_configuration_accepted(&preview, &base_url)?;
                }
                Ok((
                    CreateProviderRecord {
                        name,
                        vendor: Some(provider.id),
                        protocol: channel.protocol,
                        base_url,
                        preset_key: provider.catalog_id,
                        channel: Some(channel.id),
                        models_source,
                        static_models: None,
                        api_key,
                        adapter_credentials,
                        vendor_options: serde_json::to_string(&vendor_options)?,
                        auth_mode,
                        use_proxy,
                    },
                    descriptor,
                ))
            }
            ProviderSourceInput::Custom {
                vendor,
                channel,
                protocol,
                base_url,
                models_source,
                static_models,
            } => {
                let name = normalize_name(name.as_deref().unwrap_or_default(), "provider name")?;
                let vendor = vendor.trim().to_owned();
                anyhow::ensure!(!vendor.is_empty(), "provider vendor is required");
                let descriptor = require_descriptor(&self.gw.vendor_plugins, &vendor)?;
                anyhow::ensure!(
                    !self.gw.vendor_plugins.is_retired_profile(&vendor),
                    "Vendor `{vendor}` is no longer available for new providers"
                );
                let channel = descriptor
                    .channels
                    .iter()
                    .find(|candidate| candidate.id == channel.as_str())
                    .ok_or_else(|| {
                        anyhow::anyhow!("Vendor `{vendor}` does not declare channel `{channel}`")
                    })?;
                let models_source = models_source.or_else(|| {
                    channel
                        .default_models_source
                        .map(|source| source.as_str().to_owned())
                });
                let oauth_requested = channel.auth.is_some();
                if oauth_requested && !allow_oauth {
                    anyhow::bail!(
                        r#"{{"code":"AUTH_SESSION_REQUIRED","message":"OAuth providers must be created from a completed authentication session"}}"#
                    );
                }
                let credentials = match credential {
                    ProviderCredentialInput::ApiKey { value } => {
                        std::collections::BTreeMap::from([(
                            legacy_credential_field(&descriptor, "apiKey")?.to_owned(),
                            Value::String(value),
                        )])
                    }
                    ProviderCredentialInput::Fields { values } => values,
                    ProviderCredentialInput::None => std::collections::BTreeMap::new(),
                    ProviderCredentialInput::SetupToken { value } => {
                        std::collections::BTreeMap::from([(
                            legacy_credential_field(&descriptor, "setup_token")?.to_owned(),
                            Value::String(value),
                        )])
                    }
                };
                let (vendor_options, credentials) = validate_persisted_configuration_fields(
                    &descriptor,
                    vendor_options,
                    credentials,
                    oauth_requested,
                )?;
                let adapter_credentials = serde_json::to_string(&credentials)?;
                let api_key = credentials
                    .get("apiKey")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let base_url = validate_provider_base_url(&base_url)?;
                if !oauth_requested {
                    let preview = self
                        .preview_provider_configuration(ProviderConfigurationPreviewInput {
                            provider_id: None,
                            vendor_id: vendor.clone(),
                            channel: channel.id.clone(),
                            base_url: base_url.clone(),
                            options: vendor_options.clone().into_iter().collect(),
                            credentials: credentials.clone(),
                        })
                        .await?;
                    ensure_configuration_accepted(&preview, &base_url)?;
                }
                let protocol = protocol
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .or_else(|| channel.protocol.clone())
                    .unwrap_or_default();
                // Merged profiles (e.g. `custom`) advertise every selectable
                // egress protocol in `protocols`; other vendors keep their
                // stored wire-protocol key without membership checks.
                if !channel.protocols.is_empty() {
                    anyhow::ensure!(
                        channel
                            .protocols
                            .iter()
                            .any(|option| option.value == protocol),
                        "Vendor `{vendor}` channel `{}` does not support protocol `{protocol}`",
                        channel.id
                    );
                }
                Ok((
                    CreateProviderRecord {
                        name,
                        vendor: Some(vendor),
                        protocol,
                        base_url,
                        preset_key: None,
                        channel: Some(channel.id.clone()),
                        models_source,
                        static_models,
                        api_key,
                        adapter_credentials,
                        vendor_options: serde_json::to_string(&vendor_options)?,
                        auth_mode: if oauth_requested { "oauth" } else { "apikey" }.to_string(),
                        use_proxy,
                    },
                    descriptor,
                ))
            }
        }
    }

    pub async fn copy_provider(&self, id: &str) -> anyhow::Result<Provider> {
        self.copy_provider_with_options(id, CopyProviderOptions::default())
            .await
    }

    pub async fn copy_provider_with_options(
        &self,
        id: &str,
        options: CopyProviderOptions,
    ) -> anyhow::Result<Provider> {
        ProviderConnection::new(self).copy(id, options).await
    }

    pub(super) async fn copy_provider_record(
        &self,
        id: &str,
        options: CopyProviderOptions,
    ) -> anyhow::Result<Provider> {
        let original = self.get_provider(id).await?;
        let name = self.next_provider_copy_name(&original.name).await?;
        let credential = if original.effective_auth_mode() == "oauth" {
            ProviderCredentialInput::None
        } else {
            let values: std::collections::BTreeMap<String, Value> =
                serde_json::from_str(&original.adapter_credentials).unwrap_or_default();
            if !values.is_empty() {
                ProviderCredentialInput::Fields { values }
            } else if !original.api_key.is_empty() {
                ProviderCredentialInput::ApiKey {
                    value: original.api_key.clone(),
                }
            } else {
                ProviderCredentialInput::None
            }
        };
        let vendor = original
            .vendor
            .clone()
            .ok_or_else(|| anyhow::anyhow!("provider vendor is missing"))?;
        let channel = original
            .channel
            .clone()
            .ok_or_else(|| anyhow::anyhow!("provider channel is missing"))?;
        let copied = self
            .create_provider_from_input(
                CreateProvider {
                    name: Some(name),
                    source: ProviderSourceInput::Custom {
                        vendor: vendor.clone(),
                        channel,
                        protocol: (!original.protocol.trim().is_empty())
                            .then(|| original.protocol.clone()),
                        base_url: original.base_url.clone(),
                        models_source: original.models_source.clone(),
                        static_models: original.static_models.clone(),
                    },
                    credential,
                    vendor_options: serde_json::from_str(&original.vendor_options)?,
                    use_proxy: original.use_proxy,
                },
                original.effective_auth_mode() == "oauth",
            )
            .await?;
        let copied = self
            .update_provider(
                &copied.id,
                UpdateProvider {
                    is_enabled: Some(false),
                    ..Default::default()
                },
            )
            .await?;

        let copied = if original.effective_auth_mode() == "oauth" {
            match self
                .gw
                .storage
                .oauth_credentials()
                .get(&original.id)
                .await?
            {
                Some(credential) => {
                    let credential_input = upsert_credential_from_oauth(&credential);
                    let provisioned = async {
                        let (_, operation, _) = self.gw.vendor_plugins.acquire(&vendor)?;
                        let publication = operation.publication_fence(
                            stravia_runtime_contract::CancellationToken::new(),
                            stravia_runtime_contract::Deadline::fixed(
                                std::time::Instant::now() + std::time::Duration::from_secs(120),
                            ),
                        );
                        drop(operation);
                        let write_fence = publication.write_fence().await?;
                        publication.ensure_current()?;
                        self.gw
                            .storage
                            .oauth_credentials()
                            .upsert(&copied.id, credential_input)
                            .await?;
                        let driver_key = credential.driver_key.clone();
                        let stored = stored_credential_from_oauth(&credential, &driver_key);
                        let provider = self.sync_provider_runtime_fields(&copied, &stored).await?;
                        publication.ensure_current()?;
                        drop(write_fence);
                        drop(publication);
                        Ok::<_, anyhow::Error>(provider)
                    }
                    .await;

                    match provisioned {
                        Ok(provider) => provider,
                        Err(error) => {
                            if let Err(cleanup_error) = self.delete_provider(&copied.id).await {
                                tracing::warn!(
                                    "failed to rollback copied oauth provider {} after provisioning error: {}",
                                    copied.id,
                                    cleanup_error
                                );
                            }
                            return Err(error.context("copy oauth provider"));
                        }
                    }
                }
                None => copied,
            }
        } else {
            copied
        };

        if options.append_targets {
            super::routes::RouteModule::new(self)
                .copy_provider_targets(&original.id, &copied.id)
                .await?;
        }

        Ok(copied)
    }

    pub async fn update_provider(
        &self,
        id: &str,
        input: UpdateProvider,
    ) -> anyhow::Result<Provider> {
        ProviderConnection::new(self)
            .save(ProviderSave::Update {
                provider_id: id.to_string(),
                input,
            })
            .await
    }

    pub(super) async fn update_provider_record(
        &self,
        id: &str,
        input: UpdateProvider,
    ) -> anyhow::Result<Provider> {
        let current = self.get_provider(id).await?;
        let vendor = current
            .vendor
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("provider does not have a stable vendor identity"))?
            .to_owned();
        let channel = current
            .channel
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("provider does not have a vendor channel"))?
            .to_owned();
        let descriptor = require_descriptor(&self.gw.vendor_plugins, &vendor)?;
        let changes_identity = input
            .vendor
            .as_deref()
            .is_some_and(|value| value.trim() != vendor)
            || input
                .channel
                .as_deref()
                .is_some_and(|value| value.trim() != channel)
            || input
                .protocol
                .as_deref()
                .is_some_and(|value| value.trim() != current.protocol)
            || input
                .auth_mode
                .as_deref()
                .is_some_and(|value| value.trim() != current.auth_mode)
            || input
                .preset_key
                .as_deref()
                .is_some_and(|value| Some(value) != current.preset_key.as_deref());
        anyhow::ensure!(
            !changes_identity,
            "provider vendor, channel, protocol, and authentication cannot be changed after creation"
        );

        // ADR-0073：黑名单按 API 输入判定——下方写入总是全字段重写，
        // 不能把"回填现值"误判成凭据变更。
        let preserve_credential_status = !input.resets_credential_status();
        let changes_options = input.vendor_options.is_some();
        let credential_updates = input.adapter_credentials.clone().unwrap_or_default();
        let changes_credentials = credential_updates.values().any(configured_secret_value)
            || input
                .api_key
                .as_ref()
                .is_some_and(|value| !value.trim().is_empty());
        let name = normalize_name(
            input.name.as_deref().unwrap_or(&current.name),
            "provider name",
        )?;
        self.ensure_provider_name_unique(Some(id), &name).await?;
        let mut credentials = serde_json::from_str::<std::collections::BTreeMap<String, Value>>(
            &current.adapter_credentials,
        )?;
        if !current.api_key.trim().is_empty()
            && let Ok(key) = legacy_credential_field(&descriptor, "apiKey")
        {
            credentials
                .entry(key.to_owned())
                .or_insert_with(|| Value::String(current.api_key.clone()));
        }
        for (key, value) in credential_updates {
            let declared_secret = descriptor
                .config_fields
                .iter()
                .any(|field| field.secret && field.key == key);
            if declared_secret && !configured_secret_value(&value) {
                continue;
            }
            credentials.insert(key, value);
        }
        if let Some(api_key) = input
            .api_key
            .as_ref()
            .filter(|value| !value.trim().is_empty())
        {
            credentials.insert("apiKey".into(), Value::String(api_key.clone()));
        }
        let auth_mode = input
            .auth_mode
            .clone()
            .unwrap_or_else(|| current.auth_mode.clone());
        let options = match input.vendor_options.clone() {
            Some(options) => options,
            None => serde_json::from_str(&current.vendor_options)?,
        };
        let (options, credentials) = validate_persisted_configuration_fields(
            &descriptor,
            options,
            credentials,
            auth_mode == "oauth",
        )?;
        let base_url =
            validate_provider_base_url(input.base_url.as_deref().unwrap_or(&current.base_url))?;
        let preview = self
            .preview_provider_configuration(ProviderConfigurationPreviewInput {
                provider_id: Some(id.to_owned()),
                vendor_id: vendor.clone(),
                channel: channel.clone(),
                base_url: base_url.clone(),
                options: options.clone().into_iter().collect(),
                credentials: credentials.clone(),
            })
            .await?;
        ensure_configuration_accepted(&preview, &base_url)?;

        let _configuration = self.gw.vendor_plugins.configuration_guard(&vendor).await;
        let (loaded, operation, _) = self.gw.vendor_plugins.acquire(&vendor)?;
        anyhow::ensure!(
            loaded.descriptor().provider(&vendor) == Some(&descriptor),
            "vendor plugin changed while validating provider configuration"
        );
        let _permit = operation.write_permit().await?;
        let unchanged = self
            .gw
            .storage
            .providers()
            .get(id)
            .await?
            .is_some_and(|provider| same_provider_generation(&provider, &current));
        anyhow::ensure!(unchanged, "provider changed while validating configuration");
        _permit.ensure_current()?;

        let api_key = if auth_mode == "oauth" {
            String::new()
        } else {
            credentials
                .get("apiKey")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let provider = self
            .gw
            .storage
            .providers()
            .update(
                id,
                UpdateProvider {
                    name: Some(name),
                    vendor: Some(vendor),
                    protocol: Some(current.protocol),
                    base_url: Some(base_url.clone()),
                    preset_key: current.preset_key,
                    channel: Some(channel),
                    models_source: input.models_source.or(current.models_source),
                    static_models: input.static_models.or(current.static_models),
                    api_key: Some(api_key),
                    adapter_credentials: Some(credentials),
                    vendor_options: Some(options),
                    auth_mode: Some(auth_mode),
                    use_proxy: Some(input.use_proxy.unwrap_or(current.use_proxy)),
                    is_enabled: Some(input.is_enabled.unwrap_or(current.is_enabled)),
                    preserve_credential_status,
                },
            )
            .await?;
        if changes_options {
            self.gw
                .vendor_plugins
                .store
                .recovered(id, "options")
                .await?;
        }
        if changes_credentials {
            self.gw
                .vendor_plugins
                .store
                .recovered(id, "credentials")
                .await?;
        }
        self.bump_config_epoch().await?;
        Ok(provider)
    }

    pub async fn delete_provider(&self, id: &str) -> anyhow::Result<()> {
        crate::admin::provider_connection::ProviderConnection::new(self)
            .delete(id)
            .await
    }

    pub(super) async fn delete_provider_record(&self, id: &str) -> anyhow::Result<()> {
        let provider = self.get_provider(id).await?;
        let vendor = provider
            .vendor
            .as_deref()
            .filter(|vendor| !vendor.is_empty());
        // 删除仅清理 Core 自有数据；旧连接的组件即使损坏或无法加载，也必须可清理。
        let _configuration = match vendor {
            Some(vendor) => Some(self.gw.vendor_plugins.configuration_guard(vendor).await),
            None => None,
        };
        let _permit = match vendor {
            Some(vendor) => Some(self.gw.vendor_plugins.write_permit(vendor).await?),
            None => None,
        };
        let unchanged = self
            .gw
            .storage
            .providers()
            .get(id)
            .await?
            .is_some_and(|current| same_provider_generation(&current, &provider));
        anyhow::ensure!(unchanged, "provider changed while preparing deletion");
        if let Some(permit) = &_permit {
            permit.ensure_current()?;
        }

        // ProviderStore owns the backend transaction that removes this
        // Provider, prunes its Targets, and deletes Routes left empty.
        self.gw.storage.providers().delete(id).await?;
        super::routes::RouteModule::new(self).reload_cache().await?;
        self.bump_config_epoch().await?;
        Ok(())
    }

    async fn ensure_provider_name_unique(
        &self,
        exclude_id: Option<&str>,
        name: &str,
    ) -> anyhow::Result<()> {
        if self
            .gw
            .storage
            .providers()
            .exists_by_name(name, exclude_id)
            .await?
        {
            return Err(coded_error(
                "PROVIDER_NAME_CONFLICT",
                &format!("provider name already exists: {name}"),
                serde_json::json!({ "name": name }),
            ));
        }
        Ok(())
    }

    async fn next_provider_copy_name(&self, original_name: &str) -> anyhow::Result<String> {
        let base = format!("{}_Copy", normalize_name(original_name, "provider name")?);
        if !self
            .gw
            .storage
            .providers()
            .exists_by_name(&base, None)
            .await?
        {
            return Ok(base);
        }

        for index in 2.. {
            let candidate = format!("{base}{index}");
            if !self
                .gw
                .storage
                .providers()
                .exists_by_name(&candidate, None)
                .await?
            {
                return Ok(candidate);
            }
        }

        unreachable!("unbounded provider copy name search must return");
    }

    pub async fn test_provider(&self, id: &str) -> anyhow::Result<TestResult> {
        crate::admin::provider_connection::ProviderConnection::new(self)
            .test(
                crate::admin::provider_connection::ProviderConnectivityTest::Existing(
                    id.to_string(),
                ),
            )
            .await
    }

    pub(super) async fn test_provider_record(&self, id: &str) -> anyhow::Result<TestResult> {
        let provider = self.get_provider(id).await?;
        let start = Instant::now();
        let vendor_id = provider
            .vendor
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("provider vendor is missing"))?;
        let channel_id = provider
            .channel
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("provider channel is missing"))?;
        let descriptor = require_descriptor(&self.gw.vendor_plugins, vendor_id)?;
        let supports_validation = descriptor
            .channels
            .iter()
            .find(|channel| channel.id == channel_id)
            .is_some_and(|channel| {
                channel
                    .capabilities
                    .contains(&stravia_vendor_sdk::Capability::ConfigValidation)
            });
        let result = if supports_validation {
            // ADR-0073：预览前先锁定凭据代际，手动测试的上游 401 同样算
            // 失效证据（条件写仍按代际比对，不覆盖更新的凭据）。
            let credential_version = crate::db::models::ProviderCredentialVersion {
                provider_revision: provider.revision,
                oauth_status_version: self
                    .gw
                    .storage
                    .oauth_credentials()
                    .get(&provider.id)
                    .await?
                    .map(|credential| credential.status_version),
            };
            let preview = match self
                .preview_provider_configuration(ProviderConfigurationPreviewInput {
                    provider_id: Some(provider.id.clone()),
                    vendor_id: vendor_id.to_owned(),
                    channel: channel_id.to_owned(),
                    base_url: provider.base_url.clone(),
                    options: serde_json::from_str(&provider.vendor_options)?,
                    credentials: std::collections::BTreeMap::new(),
                })
                .await
            {
                Ok(preview) => preview,
                Err(error) => {
                    if crate::plugin::execution::is_credential_rejection(&error) {
                        self.gw
                            .mark_provider_credential_invalid(&provider.id, credential_version)
                            .await;
                    }
                    return Err(error);
                }
            };
            let error = if preview.issues.is_empty() {
                None
            } else {
                Some(format!(
                    "Vendor configuration validation failed: {}",
                    serde_json::to_string(&preview.issues)?
                ))
            };
            TestResult {
                success: error.is_none(),
                latency_ms: start.elapsed().as_millis() as u64,
                model: None,
                error,
            }
        } else {
            TestResult {
                success: false,
                latency_ms: start.elapsed().as_millis() as u64,
                model: None,
                error: Some(
                    "Vendor channel does not declare configuration validation; no non-consuming connectivity test is available"
                        .into(),
                ),
            }
        };
        self.record_provider_test_result(&provider.id, &result)
            .await?;
        Ok(result)
    }

    pub(super) async fn test_provider_candidate_record(
        &self,
        input: CreateProvider,
    ) -> anyhow::Result<TestResult> {
        let start = Instant::now();
        let (record, descriptor) = self.resolve_create_provider(input, false).await?;
        let channel_id = record
            .channel
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("provider channel is missing"))?;
        let supports_validation = descriptor
            .channels
            .iter()
            .find(|channel| channel.id == channel_id)
            .is_some_and(|channel| {
                channel
                    .capabilities
                    .contains(&stravia_vendor_sdk::Capability::ConfigValidation)
            });
        Ok(TestResult {
            success: supports_validation,
            latency_ms: start.elapsed().as_millis() as u64,
            model: None,
            error: (!supports_validation).then(|| {
                "Vendor channel does not declare configuration validation; no non-consuming connectivity test is available"
                    .into()
            }),
        })
    }

    async fn record_provider_test_result(
        &self,
        provider_id: &str,
        result: &TestResult,
    ) -> anyhow::Result<()> {
        self.gw
            .storage
            .providers()
            .record_test_result(
                provider_id,
                ProviderTestResult {
                    success: result.success,
                    tested_at: String::new(),
                },
            )
            .await
    }
}
