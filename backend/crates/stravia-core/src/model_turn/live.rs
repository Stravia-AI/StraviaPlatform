use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::stream;

use super::continuation::{ContinuationLookup, ContinuationTarget};
use super::provider::{
    ProviderAdapter, ProviderBinding, ProviderCall, ProviderStreamError, ProviderStreamResponse,
    ResponsesWebSocketBinding,
};
use super::support::{
    ai_response_to_deltas, is_openai_generation_target, is_retryable, load_model_backends,
    merge_provider_headers, resolve_vendor_adapter, runtime_binding_headers,
};
use super::{
    CanonicalEvent, ModelTurn, ModelTurnAuthorization, ModelTurnError, ModelTurnExecutor,
    StreamResponseAccumulator, TargetIdentity, TurnInput, TurnTransport,
};
use crate::Gateway;
use crate::error::GatewayError;
use crate::hook::RouteContext;
use crate::logging::LogEntry;
use crate::protocol::ProviderProtocols;
use crate::protocol::ids::OPEN_RESPONSES_2026_04_24;
use crate::protocol::ir::request::MediaRoutingMode;
use crate::protocol::ir::{AiRequest, AiStreamDelta, Usage};
use crate::provider::VendorRegistry;
use crate::proxy::client::ProxyClient;
use crate::proxy::context::RequestContext;
use crate::proxy::observability::send_log;
use crate::proxy::planner::{ProtocolMode, ProtocolPlan, negotiate};
use crate::proxy::security::Security;
use crate::router::{
    AttemptFailureDisposition, RouteAttemptPolicy, SelectedTarget, TargetSelector,
    selected_target_key,
};

#[derive(Clone)]
pub struct LiveModelTurnExecutor {
    gateway: Gateway,
    continuation: Arc<dyn ContinuationLookup>,
}

impl LiveModelTurnExecutor {
    pub fn new(gateway: Gateway, continuation: Arc<dyn ContinuationLookup>) -> Self {
        Self {
            gateway,
            continuation,
        }
    }
}

#[async_trait]
impl ModelTurnExecutor for LiveModelTurnExecutor {
    async fn execute(&self, input: TurnInput) -> Result<ModelTurn, ModelTurnError> {
        if input.cancellation.is_cancelled() {
            return Err(ModelTurnError::new("cancelled", "Model Turn cancelled"));
        }
        if Instant::now() >= input.deadline {
            return Err(ModelTurnError::new(
                "deadline_exceeded",
                "Model Turn deadline exceeded",
            ));
        }
        let deadline = tokio::time::Instant::from_std(input.deadline);
        let cancellation = input.cancellation.clone();
        tokio::select! {
            biased;
            result = execute_inner(self.clone(), input) => result,
            _ = cancellation.cancelled() => {
                Err(ModelTurnError::new("cancelled", "Model Turn cancelled"))
            }
            _ = tokio::time::sleep_until(deadline) => {
                Err(ModelTurnError::new("deadline_exceeded", "Model Turn deadline exceeded"))
            }
        }
    }
}

async fn execute_inner(
    executor: LiveModelTurnExecutor,
    mut input: TurnInput,
) -> Result<ModelTurn, ModelTurnError> {
    let gateway = &executor.gateway;
    let route = {
        let cache = gateway.model_cache.read().await;
        cache
            .match_model(&input.request.model)
            .or_else(|| {
                cache
                    .models
                    .iter()
                    .find(|model| model.id == input.request.model)
            })
            .cloned()
    }
    .ok_or_else(|| ModelTurnError::new("model_not_found", "Model is unavailable"))?;

    if let Some(requested) = input.request.reasoning.level {
        input.request.reasoning.level = requested
            .clamp(&route.supported_thinking_levels)
            .ok_or_else(|| {
                ModelTurnError::new(
                    "thinking_level_unsupported",
                    "Route has no Supported Thinking Level for this request",
                )
            })
            .map(Some)?;
    }

    if input.authorization == ModelTurnAuthorization::CapabilityGrant
        && crate::media::contains_images(&input.request)
        && !crate::media::model_is_image_capable(gateway, &route).await
    {
        return Err(ModelTurnError::new(
            "media_understanding_unavailable",
            "Media Understanding is unavailable",
        ));
    }

    let security = Security::new(gateway.storage.auth());
    let access = match input.authorization {
        ModelTurnAuthorization::RouteBinding => {
            security
                .authorize_principal_model(&input.principal, &route)
                .await
        }
        ModelTurnAuthorization::CapabilityGrant => {
            security
                .authorize_principal_capability(&input.principal)
                .await
        }
    }
    .map_err(model_turn_gateway_error)?;

    let targets = load_model_backends(gateway, &route).await;
    let mut attempts = RouteAttemptPolicy::new(&route.balance, &targets);
    if let Some(plan) = input.request.meta.media_routing.as_ref() {
        attempts.retain(|target| plan.target_keys.contains(&selected_target_key(target)));
        if attempts.is_empty() {
            return Err(ModelTurnError::new(
                "input_modality_unsupported",
                "No eligible Target remains for the fixed Media routing plan",
            ));
        }
    }
    let preferred_target =
        gateway
            .cache_affinity
            .preferred_target(&input.principal, &route.id, &input.request);
    attempts.prefer(preferred_target.as_deref(), &gateway.health_registry);
    if attempts.is_empty() {
        return Err(ModelTurnError::new(
            "model_unavailable",
            "Model has no configured Target",
        ));
    }

    let mut last_error = None;
    while let Some(target) = attempts.next_healthy(&gateway.health_registry) {
        let attempt_started = Instant::now();
        match prepare_attempt(&executor, &route, &target, &input).await {
            Ok(prepared) => match begin_attempt(
                gateway,
                &route,
                &target,
                &input,
                prepared,
                &access,
                attempt_started,
            )
            .await
            {
                Ok(turn) => return Ok(turn),
                Err(failure) => {
                    if !failure.retryable {
                        if failure.record_health {
                            attempts.record_failure(
                                &gateway.health_registry,
                                &target,
                                false,
                                false,
                            );
                        }
                        return Err(failure.error);
                    }
                    if attempts.record_failure(&gateway.health_registry, &target, true, false)
                        == AttemptFailureDisposition::Stop
                    {
                        return Err(failure.error);
                    }
                    last_error = Some(failure.error);
                }
            },
            Err(failure) => {
                if !failure.retryable {
                    if failure.record_health {
                        attempts.record_failure(&gateway.health_registry, &target, false, false);
                    }
                    return Err(failure.error);
                }
                if !failure.record_health {
                    last_error = Some(failure.error);
                    continue;
                }
                if attempts.record_failure(&gateway.health_registry, &target, true, false)
                    == AttemptFailureDisposition::Stop
                {
                    return Err(failure.error);
                }
                last_error = Some(failure.error);
            }
        }
    }

    Err(last_error
        .unwrap_or_else(|| ModelTurnError::new("provider_unavailable", "all Model Targets failed")))
}

struct PreparedAttempt {
    route: RouteContext,
    provider_call: ProviderCall,
    force_stream: bool,
    actual_model: String,
    provider: crate::db::models::Provider,
    namespace: String,
    trace: TurnTransport,
}

struct AttemptFailure {
    error: ModelTurnError,
    retryable: bool,
    record_health: bool,
}

impl AttemptFailure {
    fn retryable(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: ModelTurnError::new(code, message),
            retryable: true,
            record_health: true,
        }
    }

    fn terminal(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: ModelTurnError::new(code, message),
            retryable: false,
            record_health: false,
        }
    }

    fn ineligible(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: ModelTurnError::new(code, message),
            retryable: true,
            record_health: false,
        }
    }
}

async fn prepare_attempt(
    executor: &LiveModelTurnExecutor,
    route: &crate::db::models::Model,
    target: &SelectedTarget,
    input: &TurnInput,
) -> Result<PreparedAttempt, AttemptFailure> {
    let gateway = &executor.gateway;
    let target_key = selected_target_key(target);
    let provider = gateway
        .storage
        .providers()
        .get(&target.provider_id)
        .await
        .map_err(|error| {
            AttemptFailure::retryable(
                "provider_unavailable",
                format!("provider unavailable: {error}"),
            )
        })?
        .filter(|provider| provider.is_enabled)
        .ok_or_else(|| {
            AttemptFailure::retryable(
                "provider_unavailable",
                format!("provider unavailable: {}", target.provider_id),
            )
        })?;
    let actual_model = if target.model.is_empty() || target.model == "*" {
        route.name.clone()
    } else {
        target.model.clone()
    };

    let metadata_required = input.request.meta.media_routing.is_some()
        || crate::web_search::native_web_search_requested(&input.request)
        || input
            .request
            .tools
            .as_ref()
            .is_some_and(|tools| !tools.is_empty())
        || request_contains_video(&input.request)
        || crate::media::contains_images(&input.request);
    let provider_model = gateway
        .storage
        .provider_models()
        .get(&provider.id, &actual_model)
        .await
        .map_err(|error| {
            if metadata_required {
                AttemptFailure::terminal(
                    "provider_metadata_unavailable",
                    format!("Provider Model metadata is unavailable: {error}"),
                )
            } else {
                AttemptFailure::retryable("provider_unavailable", error.to_string())
            }
        })?;

    let supports_tools = provider_model
        .as_ref()
        .and_then(|model| model.metadata.tool_call)
        .unwrap_or(false);
    if input
        .request
        .tools
        .as_ref()
        .is_some_and(|tools| !tools.is_empty())
        && !supports_tools
    {
        return Err(AttemptFailure::ineligible(
            if crate::web_search::native_web_search_requested(&input.request) {
                "web_search_unsupported"
            } else {
                "tools_unsupported"
            },
            "selected provider model does not support function tools",
        ));
    }
    if request_contains_video(&input.request)
        && !provider_model
            .as_ref()
            .is_some_and(|model| supports_modality(&model.metadata, "video"))
    {
        return Err(AttemptFailure::ineligible(
            "input_modality_unsupported",
            "selected provider model does not support native video input",
        ));
    }
    if input
        .request
        .meta
        .media_routing
        .as_ref()
        .is_some_and(|plan| plan.mode == MediaRoutingMode::Native)
        && !provider_model
            .as_ref()
            .is_some_and(|model| crate::media::supports_image(&model.metadata))
    {
        return Err(AttemptFailure::ineligible(
            "input_modality_unsupported",
            "selected provider model does not support native image input",
        ));
    }

    let provider_runtime = gateway
        .admin()
        .resolve_provider_runtime(&provider)
        .await
        .map_err(|error| {
            AttemptFailure::retryable("provider_credential_error", error.to_string())
        })?;
    let provider_protocols = ProviderProtocols::from_provider(&provider);
    let ingress = input
        .request
        .meta
        .source_protocol
        .unwrap_or(OPEN_RESPONSES_2026_04_24);
    let openai_generation_target = is_openai_generation_target(
        provider.vendor.as_deref(),
        provider.preset_key.as_deref(),
        input.request.embedding.is_some(),
    );
    let responses_representable = openai_generation_target
        && crate::protocol::transform::ProtocolTransform::global()
            .bind(ingress, OPEN_RESPONSES_2026_04_24)
            .and_then(|pair| pair.encode_request(&input.request))
            .is_ok();
    let mut request_context = RequestContext::new(
        ingress,
        input
            .deadline
            .saturating_duration_since(std::time::Instant::now())
            .max(Duration::from_millis(1)),
    );
    request_context.cancellation = input.cancellation.clone();
    let plan = if responses_representable {
        ProtocolPlan {
            ingress,
            egress: OPEN_RESPONSES_2026_04_24,
            mode: if ingress == OPEN_RESPONSES_2026_04_24 {
                ProtocolMode::Native
            } else {
                ProtocolMode::Transform
            },
            base_url: provider_protocols.base_url.clone(),
            needs_conversion: ingress != OPEN_RESPONSES_2026_04_24,
        }
    } else {
        negotiate(
            ingress,
            None,
            Some(&provider_protocols),
            &mut request_context,
        )
        .map_err(|error| {
            AttemptFailure::terminal("protocol_negotiation_failed", error.to_string())
        })?
    };
    let egress = plan.egress;
    if input
        .request
        .meta
        .vendor
        .ingress
        .get("__stravia_opaque_context_required")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        && !matches!(
            egress,
            crate::protocol::ids::OPEN_RESPONSES_2026_04_24
                | crate::protocol::ids::ANTHROPIC_MESSAGES_2023_06_01
                | crate::protocol::ids::GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA
        )
    {
        return Err(AttemptFailure::ineligible(
            "protected_context_unrepresentable",
            "Target protocol cannot losslessly represent restored protected reasoning",
        ));
    }
    let egress_base_url = provider_runtime
        .binding
        .base_url_override
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            if plan.base_url.is_empty() {
                provider.base_url.clone()
            } else {
                plan.base_url.clone()
            }
        });
    let vendor = resolve_vendor_adapter(&provider, egress.protocol).ok_or_else(|| {
        AttemptFailure::retryable(
            "provider_adapter_unavailable",
            format!(
                "no vendor adapter registered for '{}' or protocol '{}'",
                provider.vendor.as_deref().unwrap_or("custom"),
                egress.protocol
            ),
        )
    })?;
    let adapter = ProviderAdapter::new(
        vendor,
        ProviderBinding {
            provider: provider.clone(),
            protocol: egress,
            egress_base_url,
            api_key: provider_runtime.access_token.clone(),
            actual_model: actual_model.clone(),
            gateway: gateway.clone(),
            disable_default_auth: provider_runtime.binding.disable_default_auth,
            #[cfg(debug_assertions)]
            wire_capture_id: input.wire_capture_id.clone(),
        },
    );

    let target_namespace = target_namespace(
        &provider,
        &provider_runtime,
        &adapter,
        &target_key,
        &actual_model,
    );
    let target_capabilities = VendorRegistry::global()
        .resolve(&provider, egress)
        .map(|adapter| adapter.target_capabilities(egress))
        .unwrap_or_default();
    let websocket_enabled = openai_generation_target && target_capabilities.responses_websocket;
    let mut provider_request = input.request.clone();
    if let Some(level) = provider_request.reasoning.level {
        let Some(control) = crate::thinking::mapping_control(&target.thinking_level_map, level)
        else {
            return Err(AttemptFailure::ineligible(
                "protocol_lossy_rejected",
                format!(
                    "Target has no mapping for Thinking Level {}",
                    level.as_str()
                ),
            ));
        };
        if control.is_hidden() {
            return Err(AttemptFailure::ineligible(
                "protocol_lossy_rejected",
                format!("Target hides Thinking Level {}", level.as_str()),
            ));
        }
        provider_request.reasoning.target_control = Some(control.clone());
    } else {
        provider_request.reasoning.target_control = None;
    }
    provider_request.model.clone_from(&route.name);
    let mut full_provider_request = provider_request.clone();
    crate::model_turn::clear_previous_response_id(&mut full_provider_request);
    let mut full_outbound = adapter
        .build_request(&mut full_provider_request)
        .await
        .map_err(|error| AttemptFailure::terminal(error.stable_code(), error.to_string()))?;
    if egress == OPEN_RESPONSES_2026_04_24
        && let serde_json::Value::Object(profile) =
            crate::protocol::codec::open_responses::encoder::effective_response_profile_from_request(
                &full_provider_request,
            )
    {
        normalize_provider_effective_request(&mut full_provider_request, &profile);
    }
    let require_affinity = provider.channel.as_deref() == Some("codex")
        || full_outbound
            .body
            .get("store")
            .and_then(serde_json::Value::as_bool)
            == Some(false);
    let continued_id = executor
        .continuation
        .prepare(
            &input.principal,
            ContinuationTarget {
                namespace: &target_namespace,
                protocol: egress,
                actual_model: &actual_model,
                logical_model: &input.request.model,
                allow_ephemeral_response: websocket_enabled && require_affinity,
            },
            &mut provider_request,
        )
        .await;
    let mut outbound = if let Some(previous_response_id) = continued_id.as_ref() {
        let mut outbound = adapter
            .build_request(&mut provider_request)
            .await
            .map_err(|error| AttemptFailure::terminal(error.stable_code(), error.to_string()))?;
        outbound.body["previous_response_id"] =
            serde_json::Value::String(previous_response_id.clone());
        outbound
    } else {
        full_outbound.clone()
    };

    let binding_headers = runtime_binding_headers(&provider_runtime.binding)
        .map_err(|error| AttemptFailure::retryable("provider_runtime_error", error.to_string()))?;
    let client_headers = if provider.vendor.as_deref() == Some("openai")
        && provider.channel.as_deref() == Some("codex")
    {
        crate::provider::openai::codex::forwarded_client_headers(&input.extra_headers)
    } else {
        input.extra_headers.clone()
    };
    outbound.headers = merge_provider_headers(
        client_headers.clone(),
        outbound.headers,
        binding_headers.clone(),
    );
    full_outbound.headers =
        merge_provider_headers(client_headers, full_outbound.headers, binding_headers);

    let http_client = gateway
        .http_client_for_provider(provider.use_proxy)
        .await
        .map_err(|error| {
            AttemptFailure::retryable("provider_transport_error", error.to_string())
        })?;
    let client = if websocket_enabled {
        let websocket_client = gateway
            .responses_websocket_client_for_provider(provider.use_proxy)
            .await
            .map_err(|error| {
                AttemptFailure::retryable("provider_transport_error", error.to_string())
            })?;
        ProxyClient::with_responses_websocket(http_client, websocket_client)
    } else {
        ProxyClient::new(http_client)
    };
    let session_affinity = crate::generation_chain::generation_session_fingerprint(&input.request);
    if require_affinity && let Some(prompt_cache_key) = session_affinity.as_ref() {
        insert_default_prompt_cache_key(&mut outbound.body, prompt_cache_key);
        insert_default_prompt_cache_key(&mut full_outbound.body, prompt_cache_key);
    }
    let provider_call = if websocket_enabled {
        adapter.bind_responses_websocket(ResponsesWebSocketBinding {
            client,
            outbound,
            full_outbound,
            registry: gateway.responses_websockets.clone(),
            namespace: target_namespace.clone(),
            provider_id: provider.id.clone(),
            target_id: target_key.clone(),
            transport_attempt: uuid::Uuid::new_v4().to_string(),
            require_affinity,
            session_affinity,
        })
    } else {
        adapter.bind(client, outbound)
    };
    let trace = TurnTransport {
        upstream_url: provider_call.url().to_owned(),
        request_headers: provider_call.request_headers_json(),
        request_body: input
            .request
            .meta
            .media_routing
            .is_none()
            .then(|| provider_call.request_body_string())
            .flatten(),
        ..Default::default()
    };

    Ok(PreparedAttempt {
        route: RouteContext {
            model_id: route.id.clone(),
            provider_id: provider.id.clone(),
            target_id: target_key,
            egress,
        },
        provider_call,
        force_stream: input.request.stream.enabled
            || websocket_enabled
            || target_capabilities.stream_only,
        actual_model,
        provider,
        namespace: target_namespace,
        trace,
    })
}

fn insert_default_prompt_cache_key(body: &mut serde_json::Value, prompt_cache_key: &str) {
    if let Some(body) = body.as_object_mut() {
        body.entry("prompt_cache_key")
            .or_insert_with(|| serde_json::Value::String(prompt_cache_key.to_owned()));
    }
}

#[cfg(test)]
mod tests {
    use super::{handle_terminal_stream_error, insert_default_prompt_cache_key};
    use crate::protocol::ir::{AiError, AiErrorKind, AiStreamDelta};

    #[test]
    fn session_cache_key_fills_only_missing_provider_value() {
        let mut missing = serde_json::json!({"model": "gpt-test"});
        insert_default_prompt_cache_key(&mut missing, "session-cache");
        assert_eq!(missing["prompt_cache_key"], "session-cache");

        let mut explicit = serde_json::json!({"prompt_cache_key": "client-cache"});
        insert_default_prompt_cache_key(&mut explicit, "session-cache");
        assert_eq!(explicit["prompt_cache_key"], "client-cache");
    }

    #[test]
    fn request_scoped_stream_errors_do_not_degrade_target_health() {
        let health = crate::router::health::HealthRegistry::new();
        let deltas = vec![AiStreamDelta::StreamError {
            error: AiError::new(AiErrorKind::StreamMidError, "invalid request").with_status(400),
        }];

        for _ in 0..3 {
            assert!(handle_terminal_stream_error(
                &health,
                "provider:model",
                &deltas
            ));
        }

        assert!(health.is_healthy("provider:model"));
    }

    #[test]
    fn retryable_stream_errors_degrade_target_health() {
        let health = crate::router::health::HealthRegistry::new();
        let deltas = vec![AiStreamDelta::StreamError {
            error: AiError::new(AiErrorKind::StreamMidError, "unavailable").with_status(503),
        }];

        for _ in 0..3 {
            assert!(handle_terminal_stream_error(
                &health,
                "provider:model",
                &deltas
            ));
        }

        assert!(!health.is_healthy("provider:model"));
    }
}

async fn begin_attempt(
    gateway: &Gateway,
    route: &crate::db::models::Model,
    target: &SelectedTarget,
    input: &TurnInput,
    mut prepared: PreparedAttempt,
    access: &crate::proxy::security::ModelAccessGrant,
    attempt_started: Instant,
) -> Result<ModelTurn, AttemptFailure> {
    let mut target_identity = TargetIdentity {
        actual_model: prepared.actual_model.clone(),
        provider_id: prepared.route.provider_id.clone(),
        target_id: prepared.route.target_id.clone(),
        provider_name: prepared.provider.name.clone(),
        route_name: route.name.clone(),
        namespace: prepared.namespace.clone(),
        response_continuation_available: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
            false,
        )),
    };

    if !prepared.force_stream {
        let call = prepared
            .provider_call
            .call_non_stream()
            .await
            .map_err(|error| {
                if let Some(decode) =
                    error.downcast_ref::<crate::proxy::client::UpstreamResponseDecodeError>()
                {
                    AttemptFailure {
                        error: ModelTurnError::new("upstream_error", error.to_string()),
                        retryable: is_retryable(decode.status),
                        record_health: true,
                    }
                } else {
                    AttemptFailure::retryable("upstream_error", error.to_string())
                }
            })?;
        if call.status >= 400 {
            return Err(AttemptFailure {
                error: ModelTurnError::new(
                    "upstream_error",
                    format!("upstream returned HTTP {}", call.status),
                ),
                retryable: is_retryable(call.status),
                record_health: true,
            });
        }
        *prepared
            .trace
            .response_headers
            .lock()
            .expect("response headers") =
            crate::proxy::observability::headers_to_json(&call.headers);
        *prepared.trace.response_body.lock().expect("response body") =
            serde_json::to_vec(&call.raw).unwrap_or_default();
        let response = call
            .canonical
            .map_err(|error| AttemptFailure::terminal(error.stable_code(), error.to_string()))?;
        gateway.cache_affinity.record_success(
            &input.principal,
            &route.id,
            &input.request,
            &prepared.route.target_id,
            &response.usage,
        );
        record_success(
            gateway,
            &route.balance,
            target,
            attempt_started.elapsed().as_secs_f64() * 1000.0,
        );
        emit_internal_model_log(
            gateway,
            route,
            &prepared,
            access,
            input,
            response.usage.clone(),
            attempt_started,
        );
        let mut events = ai_response_to_deltas(&response)
            .into_iter()
            .map(CanonicalEvent::Delta)
            .map(Ok)
            .collect::<Vec<_>>();
        events.push(Ok(CanonicalEvent::Completed(Box::new(response))));
        return Ok(ModelTurn {
            route: prepared.route,
            target: target_identity,
            output: Box::pin(stream::iter(events)),
            streamed: false,
            transport: prepared.trace,
        });
    }

    let stream_started = Instant::now();
    let response = prepared
        .provider_call
        .call_stream()
        .await
        .map_err(|error| AttemptFailure::retryable("upstream_error", error.to_string()))?;
    let mut provider_stream = match response {
        ProviderStreamResponse::Stream(stream) => stream,
        ProviderStreamResponse::Error {
            status,
            headers,
            body,
        } => {
            *prepared
                .trace
                .response_headers
                .lock()
                .expect("response headers") =
                crate::proxy::observability::headers_to_json(&headers);
            *prepared.trace.response_body.lock().expect("response body") = body
                .map(|body| serde_json::to_vec(&body).unwrap_or_default())
                .unwrap_or_default();
            return Err(AttemptFailure {
                error: ModelTurnError::new(
                    "upstream_error",
                    format!("upstream returned HTTP {status}"),
                ),
                retryable: is_retryable(status),
                record_health: true,
            });
        }
        ProviderStreamResponse::Uncertain { message } => {
            return Err(AttemptFailure::terminal(
                "upstream_acceptance_unknown",
                message,
            ));
        }
    };
    target_identity.response_continuation_available =
        provider_stream.response_continuation_available();
    debug_assert!(provider_stream.status < 400);
    *prepared
        .trace
        .response_headers
        .lock()
        .expect("response headers") =
        crate::proxy::observability::headers_to_json(&provider_stream.headers);

    let first_deltas = loop {
        match provider_stream.next().await {
            Ok(Some(chunk)) => {
                prepared
                    .trace
                    .record_stream_chunk(stream_started, &chunk.raw);
                if !chunk.deltas.is_empty() {
                    break chunk.deltas;
                }
            }
            Ok(None) => {
                break provider_stream.finish().await.map_err(stream_failure)?;
            }
            Err(error) => return Err(stream_failure(error)),
        }
    };

    let (tx, rx) = tokio::sync::mpsc::channel(32);
    let internal_log =
        PendingInternalModelLog::new(gateway, route, &prepared, access, input, attempt_started);
    let principal = input.principal.clone();
    let request = input.request.clone();
    let route_id = route.id.clone();
    let route_balance = route.balance.clone();
    let target_key = prepared.route.target_id.clone();
    let health_target_key = selected_target_key(&target);
    let gateway = gateway.clone();
    let target = target.clone();
    let cancellation = input.cancellation.clone();
    let deadline = input.deadline;
    let trace = prepared.trace.clone();
    tokio::spawn(async move {
        let mut internal_log = internal_log;
        let mut accumulator = StreamResponseAccumulator::default();
        let terminal_error = handle_terminal_stream_error(
            &gateway.health_registry,
            &health_target_key,
            &first_deltas,
        );
        if send_deltas(&tx, &mut accumulator, first_deltas)
            .await
            .is_err()
        {
            return;
        }
        if terminal_error {
            return;
        }
        loop {
            let next = tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    let _ = tx.send(Err(ModelTurnError::new("cancelled", "Model Turn cancelled"))).await;
                    return;
                }
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                    let _ = tx.send(Err(ModelTurnError::new("deadline_exceeded", "Model Turn deadline exceeded"))).await;
                    return;
                }
                chunk = provider_stream.next() => chunk,
            };
            match next {
                Ok(Some(chunk)) => {
                    trace.record_stream_chunk(stream_started, &chunk.raw);
                    let terminal_error = handle_terminal_stream_error(
                        &gateway.health_registry,
                        &health_target_key,
                        &chunk.deltas,
                    );
                    if send_deltas(&tx, &mut accumulator, chunk.deltas)
                        .await
                        .is_err()
                    {
                        return;
                    }
                    if terminal_error {
                        return;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    gateway
                        .health_registry
                        .record_failure(&selected_target_key(&target));
                    let _ = tx.send(Err(stream_failure(error).error)).await;
                    return;
                }
            }
        }
        match provider_stream.finish().await {
            Ok(deltas) => {
                let terminal_error = handle_terminal_stream_error(
                    &gateway.health_registry,
                    &health_target_key,
                    &deltas,
                );
                if send_deltas(&tx, &mut accumulator, deltas).await.is_err() {
                    return;
                }
                if terminal_error {
                    return;
                }
            }
            Err(error) => {
                let _ = tx.send(Err(stream_failure(error).error)).await;
                return;
            }
        }
        let response = accumulator.into_ai_response();
        if let Some(log) = internal_log.as_mut() {
            log.set_usage(response.usage.clone());
        }
        gateway.cache_affinity.record_success(
            &principal,
            &route_id,
            &request,
            &target_key,
            &response.usage,
        );
        record_success(
            &gateway,
            &route_balance,
            &target,
            attempt_started.elapsed().as_secs_f64() * 1000.0,
        );
        let _ = tx
            .send(Ok(CanonicalEvent::Completed(Box::new(response))))
            .await;
    });

    Ok(ModelTurn {
        route: prepared.route,
        target: target_identity,
        output: Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)),
        streamed: true,
        transport: prepared.trace,
    })
}

async fn send_deltas(
    tx: &tokio::sync::mpsc::Sender<Result<CanonicalEvent, ModelTurnError>>,
    accumulator: &mut StreamResponseAccumulator,
    deltas: Vec<AiStreamDelta>,
) -> Result<(), ()> {
    accumulator.apply_all(&deltas);
    for delta in deltas {
        tx.send(Ok(CanonicalEvent::Delta(delta)))
            .await
            .map_err(|_| ())?;
    }
    Ok(())
}

fn handle_terminal_stream_error(
    health: &crate::router::health::HealthRegistry,
    target_key: &str,
    deltas: &[AiStreamDelta],
) -> bool {
    let Some(error) = deltas.iter().find_map(|delta| match delta {
        AiStreamDelta::StreamError { error } => Some(error),
        _ => None,
    }) else {
        return false;
    };
    let retryable = error
        .status_code
        .map_or_else(|| error.is_retryable(), is_retryable);
    if retryable {
        health.record_failure(target_key);
    }
    true
}

fn stream_failure(error: ProviderStreamError) -> AttemptFailure {
    match error {
        ProviderStreamError::Transport(message) => {
            AttemptFailure::retryable("upstream_stream_error", message)
        }
        ProviderStreamError::Uncertain(message) => {
            AttemptFailure::terminal("upstream_acceptance_unknown", message)
        }
        ProviderStreamError::Decode(error) => {
            AttemptFailure::terminal("protocol_lossy_rejected", error.to_string())
        }
        ProviderStreamError::Normalize(error) => {
            AttemptFailure::terminal(error.stable_code(), error.to_string())
        }
    }
}

fn target_namespace(
    provider: &crate::db::models::Provider,
    runtime: &crate::admin::ResolvedProviderRuntime,
    adapter: &ProviderAdapter,
    target_key: &str,
    actual_model: &str,
) -> String {
    let account_identity = runtime
        .binding
        .extra_headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("chatgpt-account-id"))
        .map(|(_, value)| value.as_str())
        .unwrap_or(provider.id.as_str());
    let credential_identity = if provider.auth_mode.eq_ignore_ascii_case("oauth") {
        account_identity.to_owned()
    } else {
        namespace_fingerprint(&runtime.access_token)
    };
    let stable_headers = runtime
        .binding
        .extra_headers
        .iter()
        .filter(|(name, _)| {
            let name = name.to_ascii_lowercase();
            !matches!(
                name.as_str(),
                "authorization" | "proxy-authorization" | "x-api-key" | "cookie"
            ) && !name.contains("token")
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let stable_model_aliases = runtime
        .binding
        .model_aliases
        .iter()
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut stable_models = runtime.binding.static_models_override.clone();
    if let Some(models) = &mut stable_models {
        models.sort();
    }
    namespace_fingerprint(&(
        target_key,
        provider.id.as_str(),
        provider.vendor.as_deref().unwrap_or("custom"),
        provider.channel.as_deref().unwrap_or("default"),
        provider.protocol.as_str(),
        provider.use_proxy,
        adapter.binding().egress_base_url.as_str(),
        actual_model,
        account_identity,
        credential_identity,
        (
            runtime.binding.base_url_override.as_deref(),
            stable_headers,
            stable_model_aliases,
            runtime.binding.models_source_override.as_deref(),
            runtime.binding.disable_default_auth,
            stable_models,
        ),
    ))
}

fn namespace_fingerprint<T: serde::Serialize>(value: &T) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    crate::protocol::ir::canonical::hash_hex(&crate::protocol::ir::canonical::hash_bytes(&bytes))
}

fn request_contains_video(request: &AiRequest) -> bool {
    request.items.iter().any(|message| {
        let crate::protocol::ir::MessageContent::Blocks(blocks) = &message.content else {
            return false;
        };
        blocks
            .iter()
            .any(|block| matches!(block, crate::protocol::ir::ContentBlock::Video { .. }))
    })
}

fn supports_modality(
    metadata: &crate::provider_models::ProviderModelMetadata,
    modality: &str,
) -> bool {
    metadata.modalities.as_ref().is_some_and(|modalities| {
        modalities
            .input
            .iter()
            .any(|value| value.eq_ignore_ascii_case(modality))
    })
}

fn model_turn_gateway_error(error: GatewayError) -> ModelTurnError {
    ModelTurnError::new(error.stable_code(), error.message())
}

fn emit_internal_model_log(
    gateway: &Gateway,
    route: &crate::db::models::Model,
    prepared: &PreparedAttempt,
    access: &crate::proxy::security::ModelAccessGrant,
    input: &TurnInput,
    usage: Usage,
    started_at: Instant,
) {
    let Some(entry) = internal_model_log_entry(route, prepared, access, input, usage, started_at)
    else {
        return;
    };
    send_log(gateway, entry);
}

fn internal_model_log_entry(
    route: &crate::db::models::Model,
    prepared: &PreparedAttempt,
    access: &crate::proxy::security::ModelAccessGrant,
    input: &TurnInput,
    usage: Usage,
    started_at: Instant,
) -> Option<LogEntry> {
    if input.authorization == ModelTurnAuthorization::RouteBinding {
        return None;
    }
    let protocol = input
        .request
        .meta
        .source_protocol
        .unwrap_or(OPEN_RESPONSES_2026_04_24)
        .to_string();
    Some(LogEntry {
        api_key_id: access.api_key_id.clone(),
        api_key_name: access.api_key_name.clone(),
        created_at: chrono::Utc::now().timestamp_millis(),
        client_protocol: protocol.clone(),
        upstream_protocol: prepared.route.egress.to_string(),
        provider_id: prepared.provider.id.clone(),
        provider_name: prepared.provider.name.clone(),
        model_id: Some(route.id.clone()),
        model_name: Some(route.name.clone()),
        upstream_url: Some(prepared.trace.upstream_url.clone()),
        client_model: route.name.clone(),
        upstream_model: prepared.actual_model.clone(),
        method: None,
        path: None,
        client_request_headers: None,
        client_request_body: None,
        client_response_headers: None,
        client_response_body: None,
        upstream_request_headers: prepared.trace.request_headers.clone(),
        upstream_request_body: None,
        upstream_response_headers: prepared
            .trace
            .response_headers
            .lock()
            .expect("response headers")
            .clone(),
        upstream_response_body: None,
        upstream_status_code: Some(200),
        client_status_code: 200,
        latency_total_ms: started_at.elapsed().as_millis() as i64,
        latency_upstream_ms: None,
        usage,
        thinking_level: input.request.reasoning.level,
        is_stream: prepared.force_stream,
        stream_chunks_count: 0,
        stream_first_chunk_ms: None,
    })
}

struct PendingInternalModelLog {
    gateway: Gateway,
    entry: Option<LogEntry>,
    trace: crate::model_turn::TurnTransport,
    started_at: Instant,
}

impl PendingInternalModelLog {
    fn new(
        gateway: &Gateway,
        route: &crate::db::models::Model,
        prepared: &PreparedAttempt,
        access: &crate::proxy::security::ModelAccessGrant,
        input: &TurnInput,
        started_at: Instant,
    ) -> Option<Self> {
        Some(Self {
            gateway: gateway.clone(),
            entry: Some(internal_model_log_entry(
                route,
                prepared,
                access,
                input,
                Usage::default(),
                started_at,
            )?),
            trace: prepared.trace.clone(),
            started_at,
        })
    }

    fn set_usage(&mut self, usage: Usage) {
        if let Some(entry) = self.entry.as_mut() {
            entry.usage = usage;
        }
    }

    fn emit(&mut self) {
        let Some(mut entry) = self.entry.take() else {
            return;
        };
        let metrics = self.trace.stream_metrics();
        entry.created_at = chrono::Utc::now().timestamp_millis();
        entry.latency_total_ms = self.started_at.elapsed().as_millis() as i64;
        entry.upstream_response_headers = self
            .trace
            .response_headers
            .lock()
            .expect("response headers")
            .clone();
        entry.stream_chunks_count = metrics.chunks_count;
        entry.stream_first_chunk_ms = metrics.first_chunk_ms;
        send_log(&self.gateway, entry);
    }
}

impl Drop for PendingInternalModelLog {
    fn drop(&mut self) {
        self.emit();
    }
}

fn normalize_provider_effective_request(
    request: &mut AiRequest,
    profile: &serde_json::Map<String, serde_json::Value>,
) {
    let Ok(effective) =
        crate::protocol::codec::open_responses::decoder::decode_effective_response_profile(
            &request.model,
            profile,
        )
    else {
        return;
    };
    request.generation = effective.generation;
    request.tools = effective.tools;
    request.tool_choice = effective.tool_choice;
    request.parallel_tool_calls = effective.parallel_tool_calls;
    request.disable_parallel_tool_calls = effective.disable_parallel_tool_calls;
    request.reasoning = effective.reasoning;
    request.response_format = effective.response_format;
    request.safety_settings = effective.safety_settings;
    request.ext = effective.ext;
}

fn record_success(gateway: &Gateway, balance: &str, target: &SelectedTarget, latency_ms: f64) {
    let target_key = selected_target_key(target);
    gateway.health_registry.record_success(&target_key);
    TargetSelector::record_selected(balance, &target_key);
    TargetSelector::record_latency(balance, &target_key, latency_ms);
}
