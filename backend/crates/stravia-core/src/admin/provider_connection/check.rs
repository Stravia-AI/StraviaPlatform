use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;
use stravia_runtime_contract::protocol::ir::AiErrorKind;
use stravia_vendor_runtime::RuntimeError;
use stravia_vendor_sdk::{
    Capability, ChannelDescriptor, ConnectionCheckRequest, ConnectionCheckVerification, ErrorKind,
    LocalizedText, Operation, OperationOutput, ProviderDescriptor, ProviderSnapshot,
    ValidationIssue,
};

use super::configuration::{
    NormalizedConfiguration, require_descriptor, resolved_permissions,
    validate_configuration_fields, validate_provider_base_url,
};
use super::*;
use crate::admin::auth_data::{is_expired_at, parse_auth_session_bundle};
use crate::auth::types::{AuthSession, AuthSessionStatus, CredentialBundle};
use crate::db::models::{
    ConnectionCheckStage, Provider, ProviderConnectionCheckResult, ProviderCredentialVersion,
};
use crate::plugin::execution::{PreparedVendorExecution, VendorFailureCredentialVersion};
use crate::plugin::{VendorCallContext, VendorRequest};
use crate::storage::traits::ProviderConnectionCheckWrite;

const CHECK_DEADLINE: Duration = Duration::from_secs(30);

/// Request body for `POST /providers/{id}/test`. Without `configuration` the
/// saved connection is checked; with it, the submitted draft is checked in a
/// temporary session and never persisted.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProviderConnectionCheckInput {
    #[serde(default)]
    pub configuration: Option<ProviderConfigurationPreviewInput>,
    #[serde(default)]
    pub use_proxy: Option<bool>,
    #[serde(default)]
    pub authorization_id: Option<String>,
}

struct ValidatedConfiguration {
    descriptor: ProviderDescriptor,
    channel: ChannelDescriptor,
    protocol: String,
    base_url: String,
    options: BTreeMap<String, Value>,
    credentials: BTreeMap<String, Value>,
}

struct StagedOAuth {
    session: AuthSession,
    bundle: CredentialBundle,
}

struct FailureOutcome {
    result: ProviderConnectionCheckResult,
    persist: bool,
}

pub(super) async fn check_provider_connection(
    admin: &AdminService,
    provider_id: &str,
    input: ProviderConnectionCheckInput,
) -> anyhow::Result<ProviderConnectionCheckResult> {
    let started = Instant::now();
    let context = VendorCallContext::new(
        stravia_runtime_contract::CancellationToken::new(),
        stravia_runtime_contract::Deadline::fixed(started + CHECK_DEADLINE),
    );
    match input.configuration {
        Some(configuration) => {
            check_draft(
                admin,
                provider_id,
                configuration,
                input.use_proxy,
                input.authorization_id.as_deref(),
                started,
                context,
            )
            .await
        }
        None => check_saved(admin, provider_id, started, context).await,
    }
}

async fn check_saved(
    admin: &AdminService,
    provider_id: &str,
    started: Instant,
    context: VendorCallContext,
) -> anyhow::Result<ProviderConnectionCheckResult> {
    let Some(provider) = admin.gw.storage.providers().get(provider_id).await? else {
        return Ok(configuration_failure("plugin_unavailable"));
    };
    let Some(vendor_id) = non_empty(provider.vendor.as_deref()) else {
        return Ok(configuration_failure("plugin_unavailable"));
    };
    let Some(channel_id) = non_empty(provider.channel.as_deref()) else {
        return Ok(configuration_failure("plugin_unavailable"));
    };
    let Ok(descriptor) = require_descriptor(&admin.gw.vendor_plugins, vendor_id) else {
        return Ok(configuration_failure("plugin_unavailable"));
    };
    let Some(channel) = descriptor
        .channels
        .iter()
        .find(|channel| channel.id == channel_id)
        .cloned()
    else {
        return Ok(configuration_failure("plugin_unavailable"));
    };
    let options = match parse_object(&provider.vendor_options) {
        Ok(options) => options,
        Err(_) => return Ok(configuration_failure("invalid_configuration")),
    };
    let credentials = match admin
        .provider_configuration_credentials(&provider, &descriptor)
        .await
    {
        Ok(credentials) => declared_secret_credentials(&descriptor, credentials),
        Err(_) => return Ok(configuration_failure("invalid_configuration")),
    };
    let allow_missing_secrets = channel.auth.is_some() || provider.auth_mode.trim() == "oauth";
    let protocol = match resolve_protocol(
        &channel,
        None,
        non_empty(Some(provider.protocol.as_str()))
            .or(channel.protocol.as_deref())
            .unwrap_or_default(),
    ) {
        Ok(protocol) => protocol,
        Err(result) => return Ok(result),
    };
    let validated = match validate_fields(
        &descriptor,
        &channel,
        protocol,
        options,
        credentials,
        allow_missing_secrets,
    ) {
        Ok(validated) => validated,
        Err(result) => return Ok(result),
    };
    let validated = match finalize_configuration(validated, &provider.base_url) {
        Ok(validated) => validated,
        Err(result) => return Ok(result),
    };
    if let Err(result) = validate_guest_configuration(admin, &validated, &context).await {
        return Ok(result);
    }
    if !supports_connection_check(&validated.channel) {
        return Ok(unsupported());
    }

    let prepared = match admin
        .gw
        .prepare_vendor_execution(provider_id, None, Operation::ConnectionCheck, &context)
        .await
    {
        Ok(prepared) => prepared,
        Err(error) => return Ok(classify_error(&error).result),
    };
    if prepared.credential_version().provider_revision != provider.revision {
        let current = admin.gw.storage.providers().get(provider_id).await?;
        // 预刷新可以清除 invalid 并推进行版本，不能把它误当成用户修改了连接。
        if current.as_ref().is_none_or(|current| {
            current.revision != prepared.credential_version().provider_revision
                || !same_connection_configuration(&provider, current)
        }) {
            return Ok(failed(ConnectionCheckStage::Transport, "cancelled"));
        }
    }
    // Keep a clone for the conditional write; the execution consumes its own copy.
    let writer = prepared.clone();
    let execution = match admin
        .gw
        .execute_prepared_vendor(
            prepared,
            VendorRequest::ConnectionCheck(ConnectionCheckRequest {}),
            &context,
        )
        .await
    {
        Ok(execution) => execution,
        Err(error) => {
            let outcome = classify_error(&error);
            let expected = error
                .downcast_ref::<VendorFailureCredentialVersion>()
                .map_or_else(|| writer.credential_version(), |version| version.0);
            if outcome.persist && !write_saved(admin, &writer, expected, false, false).await? {
                return Ok(failed(ConnectionCheckStage::Transport, "cancelled"));
            }
            return Ok(outcome.result);
        }
    };

    let expected = execution
        .credential_version
        .unwrap_or_else(|| writer.credential_version());
    let OperationOutput::ConnectionCheck(response) = execution.output else {
        if !write_saved(admin, &writer, expected, false, false).await? {
            return Ok(failed(ConnectionCheckStage::Transport, "cancelled"));
        }
        return Ok(failed(ConnectionCheckStage::Response, "invalid_response"));
    };
    let verified = response.verified;
    let credentials_verified = matches!(
        verified,
        ConnectionCheckVerification::Credentials
            | ConnectionCheckVerification::EndpointAndCredentials
    );
    if !write_saved(admin, &writer, expected, true, credentials_verified).await? {
        return Ok(failed(ConnectionCheckStage::Transport, "cancelled"));
    }
    Ok(ProviderConnectionCheckResult::Passed {
        verified,
        duration_ms: elapsed_ms(started),
    })
}

async fn check_draft(
    admin: &AdminService,
    path_provider_id: &str,
    configuration: ProviderConfigurationPreviewInput,
    use_proxy: Option<bool>,
    authorization_id: Option<&str>,
    started: Instant,
    context: VendorCallContext,
) -> anyhow::Result<ProviderConnectionCheckResult> {
    if let Some(provider_id) = configuration.provider_id.as_deref()
        && provider_id != path_provider_id
    {
        return Ok(configuration_failure("invalid_configuration"));
    }
    let saved = match configuration.provider_id.as_deref() {
        Some(provider_id) => match admin.gw.storage.providers().get(provider_id).await? {
            Some(provider) => Some(provider),
            None => return Ok(configuration_failure("invalid_configuration")),
        },
        None => None,
    };
    if saved.is_none()
        && admin
            .gw
            .vendor_plugins
            .is_retired_profile(&configuration.vendor_id)
    {
        return Ok(configuration_failure("plugin_unavailable"));
    }
    let Ok(descriptor) = require_descriptor(&admin.gw.vendor_plugins, &configuration.vendor_id)
    else {
        return Ok(configuration_failure("plugin_unavailable"));
    };
    let Some(channel) = descriptor
        .channels
        .iter()
        .find(|channel| channel.id == configuration.channel)
        .cloned()
    else {
        return Ok(configuration_failure("plugin_unavailable"));
    };
    if let Some(saved) = &saved
        && (saved.vendor.as_deref() != Some(descriptor.provider_id.as_str())
            || saved.channel.as_deref() != Some(configuration.channel.as_str()))
    {
        return Ok(configuration_failure("invalid_configuration"));
    }

    let protocol = match resolve_protocol(
        &channel,
        configuration.protocol.as_deref(),
        saved
            .as_ref()
            .map(|provider| provider.protocol.as_str())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| channel.protocol.as_deref().unwrap_or_default()),
    ) {
        Ok(protocol) => protocol,
        Err(result) => return Ok(result),
    };

    let mut credentials = configuration.credentials;
    let mut saved_oauth_credentials = None;
    if let Some(saved) = &saved {
        let saved_credentials = match admin
            .provider_configuration_credentials(saved, &descriptor)
            .await
        {
            Ok(credentials) => credentials,
            Err(_) => return Ok(configuration_failure("invalid_configuration")),
        };
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
        if saved.auth_mode.trim() == "oauth" {
            saved_oauth_credentials = Some(saved_credentials);
        }
    }

    let staged = match authorization_id {
        Some(authorization_id) => {
            match load_staged_oauth(
                admin,
                authorization_id,
                &configuration.vendor_id,
                &configuration.channel,
                configuration.provider_id.as_deref(),
            )
            .await?
            {
                Ok(staged) => Some(staged),
                Err(result) => return Ok(result),
            }
        }
        None => None,
    };
    let allow_missing_secrets = channel.auth.is_some()
        || staged.is_some()
        || saved
            .as_ref()
            .is_some_and(|p| p.auth_mode.trim() == "oauth");
    let validated = match validate_fields(
        &descriptor,
        &channel,
        protocol,
        configuration.options,
        credentials,
        allow_missing_secrets,
    ) {
        Ok(validated) => validated,
        Err(result) => return Ok(result),
    };
    let effective_use_proxy = use_proxy
        .or_else(|| saved.as_ref().map(|provider| provider.use_proxy))
        .or_else(|| staged.as_ref().map(|staged| staged.session.use_proxy))
        .unwrap_or(false);
    let validated = match finalize_configuration(validated, &configuration.base_url) {
        Ok(validated) => validated,
        Err(result) => return Ok(result),
    };
    if let Err(result) = validate_guest_configuration(admin, &validated, &context).await {
        return Ok(result);
    }
    if !supports_connection_check(&validated.channel) {
        return Ok(unsupported());
    }

    let mut operation_metadata = BTreeMap::new();
    operation_metadata.insert("use_proxy".into(), Value::Bool(effective_use_proxy));
    let snapshot = ProviderSnapshot {
        provider_id: validated.descriptor.provider_id.clone(),
        channel: validated.channel.id.clone(),
        base_url: validated.base_url,
        protocol: validated.protocol,
        options: validated.options,
        credentials: validated.credentials,
        model: None,
        model_metadata: None,
        client_headers: Vec::new(),
        operation_metadata,
    };
    let mut scope = match admin
        .gw
        .create_vendor_session_scope(&validated.descriptor.provider_id, snapshot)
    {
        Ok(scope) => scope,
        Err(_) => return Ok(configuration_failure("invalid_configuration")),
    };
    // 网络授权只来自显式配置。OAuth 返回值不能为本次临时执行新增 origin。
    if let Some(staged) = &staged {
        scope.provider.credentials.clear();
        extend_staged_oauth(&mut scope.provider.credentials, staged);
    } else if let Some(saved_credentials) = saved_oauth_credentials {
        for (key, value) in saved_credentials {
            scope.provider.credentials.entry(key).or_insert(value);
        }
    }
    match admin
        .gw
        .execute_vendor_session(
            &scope,
            VendorRequest::ConnectionCheck(ConnectionCheckRequest {}),
            context,
        )
        .await
    {
        Ok(execution) => {
            let _publication = match execution.publication.write_fence().await {
                Ok(fence) => fence,
                Err(error) => return Ok(classify_error(&error).result),
            };
            let OperationOutput::ConnectionCheck(response) = execution.output else {
                return Ok(failed(ConnectionCheckStage::Response, "invalid_response"));
            };
            Ok(ProviderConnectionCheckResult::Passed {
                verified: response.verified,
                duration_ms: elapsed_ms(started),
            })
        }
        Err(error) => Ok(classify_error(&error).result),
    }
}

async fn load_staged_oauth(
    admin: &AdminService,
    authorization_id: &str,
    vendor_id: &str,
    channel: &str,
    provider_id: Option<&str>,
) -> anyhow::Result<Result<StagedOAuth, ProviderConnectionCheckResult>> {
    let Some(session) = admin.get_auth_session_record(authorization_id).await? else {
        return Ok(Err(configuration_failure("invalid_configuration")));
    };
    if session.status != AuthSessionStatus::Ready.as_str()
        || is_expired_at(session.expires_at.as_deref())
        || session.driver_key != vendor_id
        || session.channel != channel
        || session.provider_id.as_deref() != provider_id
    {
        return Ok(Err(configuration_failure("invalid_configuration")));
    }
    let Ok(bundle) = parse_auth_session_bundle(&session) else {
        return Ok(Err(configuration_failure("invalid_configuration")));
    };
    if non_empty(bundle.access_token.as_deref()).is_none() {
        return Ok(Err(configuration_failure("invalid_configuration")));
    }
    Ok(Ok(StagedOAuth { session, bundle }))
}

fn supports_connection_check(channel: &ChannelDescriptor) -> bool {
    channel.capabilities.contains(&Capability::ConnectionCheck)
}

async fn validate_guest_configuration(
    admin: &AdminService,
    validated: &ValidatedConfiguration,
    context: &VendorCallContext,
) -> Result<(), ProviderConnectionCheckResult> {
    if !validated
        .channel
        .capabilities
        .contains(&Capability::ConfigValidation)
    {
        return Ok(());
    }
    let snapshot = ProviderSnapshot {
        provider_id: validated.descriptor.provider_id.clone(),
        channel: validated.channel.id.clone(),
        base_url: validated.base_url.clone(),
        protocol: validated.protocol.clone(),
        options: validated.options.clone(),
        credentials: validated.credentials.clone(),
        model: None,
        model_metadata: None,
        client_headers: Vec::new(),
        operation_metadata: BTreeMap::new(),
    };
    let validation = admin
        .validate_provider_configuration_snapshot(
            snapshot,
            VendorCallContext::new(context.cancellation.clone(), context.deadline.clone()),
        )
        .await
        .map_err(|error| classify_error(&error).result)?;
    if !validation.issues.is_empty() {
        return Err(failed_with_issues(validation.issues));
    }
    if validation
        .proposed_base_url
        .is_some_and(|url| url != validated.base_url)
    {
        return Err(base_url_failure());
    }
    Ok(())
}

fn resolve_protocol(
    channel: &ChannelDescriptor,
    requested: Option<&str>,
    fallback: &str,
) -> Result<String, ProviderConnectionCheckResult> {
    let protocol = requested
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(fallback)
        .trim();
    if !channel.protocols.is_empty()
        && !channel
            .protocols
            .iter()
            .any(|option| option.value == protocol)
    {
        return Err(configuration_failure("invalid_configuration"));
    }
    Ok(protocol.to_owned())
}

fn validate_fields(
    descriptor: &ProviderDescriptor,
    channel: &ChannelDescriptor,
    protocol: String,
    options: BTreeMap<String, Value>,
    credentials: BTreeMap<String, Value>,
    allow_missing_secrets: bool,
) -> Result<ValidatedConfiguration, ProviderConnectionCheckResult> {
    let mut relaxed;
    let validation_descriptor = if allow_missing_secrets {
        relaxed = descriptor.clone();
        for field in &mut relaxed.config_fields {
            if field.secret {
                field.required = false;
            }
        }
        &relaxed
    } else {
        descriptor
    };
    let NormalizedConfiguration {
        options,
        credentials,
        issues,
    } = match validate_configuration_fields(validation_descriptor, &protocol, options, credentials)
    {
        Ok(normalized) => normalized,
        Err(_) => return Err(configuration_failure("invalid_configuration")),
    };
    if !issues.is_empty() {
        return Err(failed_with_issues(issues));
    }
    Ok(ValidatedConfiguration {
        descriptor: descriptor.clone(),
        channel: channel.clone(),
        protocol,
        base_url: String::new(),
        options,
        credentials,
    })
}

fn finalize_configuration(
    mut validated: ValidatedConfiguration,
    submitted_base_url: &str,
) -> Result<ValidatedConfiguration, ProviderConnectionCheckResult> {
    let submitted = submitted_base_url.trim();
    let mut base_url = if submitted.is_empty() {
        if validated.descriptor.network.base_url_field.is_none() {
            return Err(base_url_failure());
        }
        String::new()
    } else {
        match validate_provider_base_url(submitted) {
            Ok(base_url) => base_url,
            Err(_) => return Err(base_url_failure()),
        }
    };
    if let Some(field) = validated.descriptor.network.base_url_field.as_deref() {
        base_url = configured_string(field, &validated.options, &validated.credentials)
            .ok_or_else(base_url_failure)?
            .to_owned();
    }
    if base_url.trim().is_empty() {
        return Err(base_url_failure());
    }
    let base_url = match validate_provider_base_url(&base_url) {
        Ok(base_url) => base_url,
        Err(_) => return Err(base_url_failure()),
    };
    if resolved_permissions(
        &validated.descriptor,
        &base_url,
        &validated.options,
        &validated.credentials,
    )
    .is_err()
    {
        return Err(configuration_failure("invalid_configuration"));
    }
    validated.base_url = base_url;
    Ok(validated)
}

fn declared_secret_credentials(
    descriptor: &ProviderDescriptor,
    credentials: BTreeMap<String, Value>,
) -> BTreeMap<String, Value> {
    credentials
        .into_iter()
        .filter(|(key, _)| {
            descriptor
                .config_fields
                .iter()
                .any(|field| field.secret && field.key == key.as_str())
        })
        .collect()
}

fn configured_string<'a>(
    field: &str,
    options: &'a BTreeMap<String, Value>,
    credentials: &'a BTreeMap<String, Value>,
) -> Option<&'a str> {
    options
        .get(field)
        .and_then(Value::as_str)
        .or_else(|| credentials.get(field).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn parse_object(value: &str) -> anyhow::Result<BTreeMap<String, Value>> {
    let parsed: Value = serde_json::from_str(value)?;
    let Value::Object(values) = parsed else {
        anyhow::bail!("provider configuration must be a JSON object");
    };
    Ok(values.into_iter().collect())
}

fn extend_staged_oauth(credentials: &mut BTreeMap<String, Value>, staged: &StagedOAuth) {
    if let Value::Object(values) = &staged.bundle.raw {
        for (key, value) in values {
            credentials.insert(key.clone(), value.clone());
        }
    }
    credentials.insert("raw".into(), staged.bundle.raw.clone());
    insert_optional(
        credentials,
        "access_token",
        staged.bundle.access_token.as_deref(),
    );
    insert_optional(
        credentials,
        "refresh_token",
        staged.bundle.refresh_token.as_deref(),
    );
    insert_optional(
        credentials,
        "expires_at",
        staged.bundle.expires_at.as_deref(),
    );
    insert_optional(
        credentials,
        "resource_url",
        staged.bundle.resource_url.as_deref(),
    );
    insert_optional(
        credentials,
        "subject_id",
        staged.bundle.subject_id.as_deref(),
    );
    credentials.insert(
        "driver_key".into(),
        Value::String(staged.session.driver_key.clone()),
    );
    credentials.insert(
        "scheme".into(),
        Value::String(staged.session.scheme.clone()),
    );
    credentials.insert(
        "scopes".into(),
        Value::Array(
            staged
                .bundle
                .scopes
                .iter()
                .cloned()
                .map(Value::String)
                .collect(),
        ),
    );
}

fn insert_optional(map: &mut BTreeMap<String, Value>, key: &str, value: Option<&str>) {
    if let Some(value) = non_empty(value) {
        map.insert(key.to_owned(), Value::String(value.to_owned()));
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn same_connection_configuration(first: &Provider, second: &Provider) -> bool {
    first.vendor == second.vendor
        && first.protocol == second.protocol
        && first.base_url == second.base_url
        && first.preset_key == second.preset_key
        && first.channel == second.channel
        && first.models_source == second.models_source
        && first.static_models == second.static_models
        && first.api_key == second.api_key
        && first.adapter_credentials == second.adapter_credentials
        && first.vendor_options == second.vendor_options
        && first.auth_mode == second.auth_mode
        && first.use_proxy == second.use_proxy
}

async fn write_saved(
    admin: &AdminService,
    prepared: &PreparedVendorExecution,
    expected: ProviderCredentialVersion,
    success: bool,
    credentials_verified: bool,
) -> anyhow::Result<bool> {
    let Ok(_fence) = prepared.write_fence().await else {
        return Ok(false);
    };
    admin
        .gw
        .storage
        .providers()
        .record_test_result(
            prepared.provider_id(),
            expected,
            ProviderConnectionCheckWrite {
                success,
                credentials_verified,
            },
        )
        .await
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn failed(stage: ConnectionCheckStage, code: &str) -> ProviderConnectionCheckResult {
    ProviderConnectionCheckResult::Failed {
        stage,
        code: code.to_owned(),
        field: None,
        upstream_status: None,
        issues: Vec::new(),
    }
}

fn failed_with_status(
    stage: ConnectionCheckStage,
    code: &str,
    upstream_status: Option<u16>,
) -> ProviderConnectionCheckResult {
    ProviderConnectionCheckResult::Failed {
        stage,
        code: code.to_owned(),
        field: None,
        upstream_status,
        issues: Vec::new(),
    }
}

fn failed_with_issues(issues: Vec<ValidationIssue>) -> ProviderConnectionCheckResult {
    let field = issues.iter().find_map(|issue| issue.field.clone());
    ProviderConnectionCheckResult::Failed {
        stage: ConnectionCheckStage::Configuration,
        code: "invalid_configuration".into(),
        field,
        upstream_status: None,
        issues,
    }
}

fn configuration_failure(code: &str) -> ProviderConnectionCheckResult {
    failed(ConnectionCheckStage::Configuration, code)
}

fn unsupported() -> ProviderConnectionCheckResult {
    ProviderConnectionCheckResult::Unsupported {
        code: "connection_check_unsupported".into(),
    }
}

fn base_url_failure() -> ProviderConnectionCheckResult {
    failed_with_issues(vec![ValidationIssue {
        field: Some("base_url".into()),
        code: "invalid_base_url".into(),
        message: LocalizedText::from_translations([
            ("en-US", "The Base URL is invalid."),
            ("zh-CN", "Base URL 无效。"),
        ]),
    }])
}

fn classify_error(error: &anyhow::Error) -> FailureOutcome {
    let Some(runtime) = error.downcast_ref::<RuntimeError>() else {
        return FailureOutcome {
            result: configuration_failure("plugin_unavailable"),
            persist: false,
        };
    };
    match runtime {
        RuntimeError::Plugin {
            kind,
            upstream_status,
            ..
        } => classify_plugin(kind, *upstream_status),
        RuntimeError::Cancelled => FailureOutcome {
            result: failed(ConnectionCheckStage::Transport, "cancelled"),
            persist: false,
        },
        RuntimeError::DeadlineExceeded => FailureOutcome {
            result: failed(ConnectionCheckStage::Transport, "timeout"),
            persist: true,
        },
        RuntimeError::ResourceExhausted | RuntimeError::Trapped => FailureOutcome {
            result: failed(ConnectionCheckStage::Response, "plugin_unavailable"),
            persist: true,
        },
        RuntimeError::InvalidOutput => FailureOutcome {
            result: failed(ConnectionCheckStage::Response, "invalid_response"),
            persist: true,
        },
    }
}

fn classify_plugin(kind: &ErrorKind, upstream_status: Option<u16>) -> FailureOutcome {
    if matches!(kind, ErrorKind::Unsupported) {
        return FailureOutcome {
            result: unsupported(),
            persist: false,
        };
    }
    if matches!(kind, ErrorKind::InvalidResponse) {
        return FailureOutcome {
            result: failed(ConnectionCheckStage::Response, "invalid_response"),
            persist: true,
        };
    }
    if let Some(status) = upstream_status {
        let code = match status {
            401 => "authentication_failed",
            403 => "permission_denied",
            429 => "rate_limited",
            500..=599 => "upstream_error",
            _ if matches!(kind, ErrorKind::Auth) => "authentication_failed",
            _ => "upstream_error",
        };
        return FailureOutcome {
            result: failed_with_status(ConnectionCheckStage::Upstream, code, Some(status)),
            persist: true,
        };
    }
    if matches!(kind, ErrorKind::Invalid) {
        return FailureOutcome {
            result: configuration_failure("invalid_configuration"),
            persist: false,
        };
    }
    if matches!(kind, ErrorKind::ProviderNotFound) {
        return FailureOutcome {
            result: configuration_failure("plugin_unavailable"),
            persist: false,
        };
    }
    match kind {
        ErrorKind::InvalidResponse => FailureOutcome {
            result: failed(ConnectionCheckStage::Response, "invalid_response"),
            persist: true,
        },
        ErrorKind::Auth => FailureOutcome {
            result: failed(ConnectionCheckStage::Upstream, "authentication_failed"),
            persist: true,
        },
        ErrorKind::DeadlineExceeded => FailureOutcome {
            result: failed(ConnectionCheckStage::Transport, "timeout"),
            persist: true,
        },
        ErrorKind::Cancelled => FailureOutcome {
            result: failed(ConnectionCheckStage::Transport, "cancelled"),
            persist: false,
        },
        ErrorKind::Trapped | ErrorKind::ResourceExhausted => FailureOutcome {
            result: failed(ConnectionCheckStage::Response, "plugin_unavailable"),
            persist: true,
        },
        ErrorKind::Upstream(_) => {
            if kind.model_error_kind() == Some(AiErrorKind::Timeout) {
                FailureOutcome {
                    result: failed(ConnectionCheckStage::Transport, "timeout"),
                    persist: true,
                }
            } else {
                FailureOutcome {
                    result: failed(ConnectionCheckStage::Transport, "connection_failed"),
                    persist: true,
                }
            }
        }
        _ => FailureOutcome {
            result: failed(ConnectionCheckStage::Response, "invalid_response"),
            persist: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime_failure(kind: ErrorKind, status: Option<u16>) -> anyhow::Error {
        anyhow::Error::new(RuntimeError::from_guest(
            kind,
            "synthetic guest diagnostic".into(),
            status,
        ))
    }

    #[test]
    fn upstream_status_classification_uses_typed_status() {
        for (status, stage, code) in [
            (401, ConnectionCheckStage::Upstream, "authentication_failed"),
            (403, ConnectionCheckStage::Upstream, "permission_denied"),
            (429, ConnectionCheckStage::Upstream, "rate_limited"),
            (500, ConnectionCheckStage::Upstream, "upstream_error"),
            (400, ConnectionCheckStage::Upstream, "upstream_error"),
            (200, ConnectionCheckStage::Upstream, "upstream_error"),
        ] {
            let outcome = classify_error(&runtime_failure(
                ErrorKind::upstream_unknown(),
                Some(status),
            ));
            let ProviderConnectionCheckResult::Failed {
                stage: actual_stage,
                code: actual_code,
                ..
            } = outcome.result
            else {
                panic!("expected failed result for status {status}");
            };
            assert_eq!(actual_stage, stage, "status {status}");
            assert_eq!(actual_code, code, "status {status}");
        }
    }

    #[test]
    fn connection_refused_and_timeout_are_distinct_transport_failures() {
        let refused = classify_error(&runtime_failure(ErrorKind::upstream_unknown(), None));
        let ProviderConnectionCheckResult::Failed { stage, code, .. } = refused.result else {
            panic!("expected failed result");
        };
        assert_eq!(stage, ConnectionCheckStage::Transport);
        assert_eq!(code, "connection_failed");

        let timeout = classify_error(&runtime_failure(
            ErrorKind::upstream(Some(AiErrorKind::Timeout), None),
            None,
        ));
        let ProviderConnectionCheckResult::Failed { stage, code, .. } = timeout.result else {
            panic!("expected failed result");
        };
        assert_eq!(stage, ConnectionCheckStage::Transport);
        assert_eq!(code, "timeout");
    }

    #[test]
    fn unsupported_and_cancelled_are_never_persisted() {
        let unsupported = classify_error(&runtime_failure(ErrorKind::Unsupported, None));
        assert!(!unsupported.persist);
        assert!(matches!(
            unsupported.result,
            ProviderConnectionCheckResult::Unsupported { .. }
        ));

        let cancelled = classify_error(&runtime_failure(ErrorKind::Cancelled, None));
        assert!(!cancelled.persist);
        let ProviderConnectionCheckResult::Failed { stage, code, .. } = cancelled.result else {
            panic!("expected failed result");
        };
        assert_eq!(stage, ConnectionCheckStage::Transport);
        assert_eq!(code, "cancelled");
    }

    #[test]
    fn invalid_response_and_plugin_traps_stay_in_the_response_stage() {
        for kind in [
            ErrorKind::InvalidResponse,
            ErrorKind::Trapped,
            ErrorKind::ResourceExhausted,
        ] {
            let outcome = classify_error(&runtime_failure(kind, None));
            let ProviderConnectionCheckResult::Failed { stage, .. } = outcome.result else {
                panic!("expected failed result");
            };
            assert_eq!(stage, ConnectionCheckStage::Response);
            assert!(outcome.persist);
        }
    }
}
