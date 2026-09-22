use super::*;

#[derive(Clone)]
pub(super) struct ResponsesWebSocketCall {
    registry: ResponsesWebSocketRegistry,
    namespace: String,
    provider_id: String,
    target_id: String,
    transport_attempt: String,
    full_outbound: OutboundRequest,
    require_affinity: bool,
    session_affinity: Option<String>,
}

pub(super) struct ResponsesWebSocketStream {
    lease: ResponsesWebSocketLease,
    done_marker_sent: bool,
    done: bool,
    event_seen: bool,
    response_event_seen: bool,
    replayed_full_request: bool,
    allow_retries: bool,
    artifact_transfers: Option<ArtifactTransfers>,
    full_request: Value,
    websocket_url: String,
    client: ProxyClient,
    fallback_outbound: OutboundRequest,
    http_fallback: Option<BoxStream<'static, Result<bytes::Bytes, reqwest::Error>>>,
    http_fallback_status: Option<u16>,
    pub(super) fallback_attempt: Option<Arc<AttemptObservation>>,
}

pub(crate) struct ResponsesWebSocketBinding {
    pub(crate) client: ProxyClient,
    pub(crate) outbound: OutboundRequest,
    pub(crate) full_outbound: OutboundRequest,
    pub(crate) registry: ResponsesWebSocketRegistry,
    pub(crate) namespace: String,
    pub(crate) provider_id: String,
    pub(crate) target_id: String,
    pub(crate) transport_attempt: String,
    pub(crate) require_affinity: bool,
    pub(crate) session_affinity: Option<String>,
}

impl ProviderAdapter {
    pub(crate) fn bind_responses_websocket(
        self,
        request: ResponsesWebSocketBinding,
    ) -> ProviderCall {
        let ResponsesWebSocketBinding {
            client,
            outbound,
            full_outbound,
            registry,
            namespace,
            provider_id,
            target_id,
            transport_attempt,
            require_affinity,
            session_affinity,
        } = request;
        let continuation_fallback = outbound
            .body
            .get("previous_response_id")
            .is_some()
            .then(|| full_outbound.clone());
        ProviderCall {
            adapter: self,
            client,
            outbound,
            continuation_fallback,
            allow_retries: true,
            artifact_transfers: None,
            websocket: Some(ResponsesWebSocketCall {
                registry,
                namespace,
                provider_id,
                target_id,
                transport_attempt,
                full_outbound,
                require_affinity,
                session_affinity,
            }),
        }
    }

    fn prepare_responses_websocket_headers(
        &self,
        headers: &mut HeaderMap,
        connection: crate::provider::vendor_ext::ResponsesWebSocketConnectionMetadata<'_>,
    ) -> anyhow::Result<()> {
        crate::provider::common::pipeline::prepare_responses_websocket_headers(
            &self.provider_context(),
            headers,
            connection,
        )
    }

    fn build_responses_websocket_request(
        &self,
        body: &Value,
        connection: crate::provider::vendor_ext::ResponsesWebSocketConnectionMetadata<'_>,
    ) -> anyhow::Result<Value> {
        crate::provider::common::pipeline::build_responses_websocket_request(
            self.vendor.as_ref(),
            &self.provider_context(),
            body,
            connection,
        )
    }

    fn normalize_responses_websocket_event(&self, event: &mut Value) -> anyhow::Result<()> {
        crate::provider::common::pipeline::normalize_responses_websocket_event(
            self.vendor.as_ref(),
            &self.provider_context(),
            event,
        )
    }

    fn retain_responses_websocket_event(&self, event: &Value) -> bool {
        crate::provider::common::pipeline::retain_responses_websocket_event(
            self.vendor.as_ref(),
            &self.provider_context(),
            event,
        )
    }
}

impl ProviderCall {
    pub(crate) async fn call_stream(&mut self) -> anyhow::Result<ProviderStreamResponse> {
        if let Some(websocket) = &self.websocket {
            let websocket_url = responses_websocket_url(&self.outbound.url)
                .map_err(ProviderRequestPreparationError)?;
            let previous_response_id = self
                .outbound
                .body
                .get("previous_response_id")
                .and_then(Value::as_str);
            // Keep the UUID wire shape Codex uses for external thread/session/window identities.
            let session_id = uuid::Uuid::new_v4().to_string();
            let thread_id = uuid::Uuid::new_v4().to_string();
            let window_id = uuid::Uuid::new_v4().to_string();
            let connection = crate::provider::vendor_ext::ResponsesWebSocketConnectionMetadata {
                session_id: &session_id,
                thread_id: &thread_id,
                window_id: &window_id,
            };
            let mut headers = self.outbound.headers.clone();
            self.adapter
                .prepare_responses_websocket_headers(&mut headers, connection)
                .map_err(ProviderRequestPreparationError)?;
            let handshake_attempt = Arc::new(parking_lot::Mutex::new(None));
            let handshake_attempt_slot = Arc::clone(&handshake_attempt);
            let handshake_adapter = self.adapter.clone();
            let on_connect_start: crate::proxy::client::WebSocketRequestObserver =
                Arc::new(move |request| {
                    let request_url = request.url().to_string();
                    let attempt = handshake_adapter.begin_attempt("websocket", &request_url);
                    attempt.wire(
                        "upstream_request",
                        "handshake_request",
                        None,
                        Some(request.headers()),
                        Value::Null,
                    );
                    *handshake_attempt_slot.lock() = Some(attempt);
                });
            let lease = websocket
                .registry
                .acquire(
                    &self.client.responses_websocket,
                    &websocket.namespace,
                    ResponsesWebSocketTrace {
                        provider_id: &websocket.provider_id,
                        target_id: &websocket.target_id,
                        transport_attempt: &websocket.transport_attempt,
                    },
                    ResponsesWebSocketRequest {
                        url: &websocket_url,
                        headers,
                        on_connect_start: Some(on_connect_start),
                    },
                    ResponsesWebSocketAffinityHint {
                        previous_response_id,
                        session_affinity: websocket.session_affinity.as_deref(),
                        require_affinity: websocket.require_affinity,
                    },
                )
                .await;
            match lease {
                Ok(mut lease) => {
                    // A completed handshake is not a failed model request. Local
                    // body/Hook preparation below must remain outside the deadline
                    // failure window until the request is actually sent.
                    self.adapter.mark_upstream_idle();
                    let request_body =
                        if websocket.require_affinity && lease.previous_response_id().is_none() {
                            &websocket.full_outbound.body
                        } else {
                            &self.outbound.body
                        };
                    let request_body = self
                        .transfer_body(request_body)
                        .await
                        .map_err(ProviderRequestPreparationError)?;
                    let connection = lease.connection_metadata();
                    let request = self
                        .adapter
                        .build_responses_websocket_request(request_body.as_ref(), connection)
                        .map_err(ProviderRequestPreparationError)?;
                    let full_request = self
                        .adapter
                        .build_responses_websocket_request(
                            &websocket.full_outbound.body,
                            connection,
                        )
                        .map_err(ProviderRequestPreparationError)?;
                    let serialized_request = serde_json::to_string(&request)
                        .map_err(|error| ProviderRequestPreparationError(error.into()))?;
                    let attempt = if lease.reused_connection() {
                        self.adapter.begin_attempt("websocket", &websocket_url)
                    } else {
                        let attempt = handshake_attempt
                            .lock()
                            .take()
                            .expect("new WebSocket connection starts an observed handshake");
                        attempt.wire(
                            "upstream_response",
                            "handshake_response",
                            Some(101),
                            Some(lease.handshake_headers()),
                            Value::Null,
                        );
                        attempt
                    };
                    self.adapter.mark_upstream_started();
                    let serialized_payload = serialized_request.clone();
                    if let Err(error) = lease.send_text(serialized_request).await {
                        let diagnostic = crate::proxy::client::TransportDiagnostic::from_error(
                            "websocket_transport",
                            "send",
                            false,
                            Some(101),
                            None,
                            error.as_ref(),
                        );
                        attempt.finish(
                            "failed",
                            Some(101),
                            Some("websocket_send_error".into()),
                            None,
                        );
                        if self.allow_retries
                            && lease.reused_connection()
                            && self.adapter.try_record_recovery_failure()
                        {
                            let trace = lease.trace();
                            tracing::warn!(
                                transport = "responses_websocket",
                                provider_id = trace.provider_id,
                                target_id = trace.target_id,
                                transport_attempt = trace.transport_attempt,
                                failure_stage = "send",
                                fallback_transport = "http_sse",
                                "reused upstream WebSocket failed before a response; retrying silently"
                            );
                            attempt.wire("upstream_request", "close", None, None, Value::Null);
                            lease.terminal();
                            drop(lease);
                            self.adapter.mark_upstream_idle();
                            let mut outbound = websocket.full_outbound.clone();
                            outbound.body["stream"] = Value::Bool(true);
                            return self.http_stream(outbound).await;
                        }
                        return Ok(ProviderStreamResponse::Uncertain {
                            message: diagnostic.to_string(),
                        });
                    }
                    attempt.wire(
                        "upstream_request",
                        "text",
                        None,
                        None,
                        Value::String(serialized_payload),
                    );
                    let mut fallback_outbound = websocket.full_outbound.clone();
                    fallback_outbound.body["stream"] = Value::Bool(true);
                    return self.websocket_stream(
                        lease,
                        full_request,
                        fallback_outbound,
                        websocket_url,
                        attempt,
                    );
                }
                Err(ResponsesWebSocketAcquireError::Unsupported {
                    attempted,
                    status,
                    headers,
                    body,
                }) => {
                    if !self.allow_retries {
                        if let Some(attempt) = handshake_attempt.lock().take() {
                            attempt.wire_lazy(
                                "upstream_response",
                                "handshake_response",
                                status,
                                Some(&headers),
                                || bytes_value(&body),
                            );
                            return Ok(ProviderStreamResponse::Error {
                                status: status.unwrap_or(502),
                                headers: *headers,
                                body: serde_json::from_slice(&body).map_err(anyhow::Error::from),
                                attempt: Box::new(attempt),
                            });
                        }
                        anyhow::bail!("Responses WebSocket is unavailable");
                    }
                    if attempted {
                        let attempt = handshake_attempt
                            .lock()
                            .take()
                            .expect("network handshake starts an observed attempt");
                        attempt.wire_lazy(
                            "upstream_response",
                            "handshake_response",
                            status,
                            Some(&headers),
                            || bytes_value(&body),
                        );
                        if !self.adapter.try_record_recovery_failure() {
                            return Ok(ProviderStreamResponse::Error {
                                status: status.unwrap_or(502),
                                headers: *headers,
                                body: serde_json::from_slice(&body).map_err(anyhow::Error::from),
                                attempt: Box::new(attempt),
                            });
                        }
                        attempt.finish(
                            "failed",
                            status,
                            Some("websocket_unsupported".into()),
                            None,
                        );
                    }
                    self.adapter.mark_upstream_idle();
                    let mut outbound = if websocket.require_affinity {
                        websocket.full_outbound.clone()
                    } else {
                        self.outbound.clone()
                    };
                    outbound.body["stream"] = Value::Bool(true);
                    return self.http_stream(outbound).await;
                }
                Err(ResponsesWebSocketAcquireError::Cooldown) => {
                    if !self.allow_retries {
                        anyhow::bail!("Responses WebSocket is temporarily unavailable");
                    }
                    let mut outbound = if websocket.require_affinity {
                        websocket.full_outbound.clone()
                    } else {
                        self.outbound.clone()
                    };
                    outbound.body["stream"] = Value::Bool(true);
                    return self.http_stream(outbound).await;
                }
                Err(ResponsesWebSocketAcquireError::HandshakeBodyRead {
                    status,
                    headers,
                    diagnostic,
                }) => {
                    let attempt = handshake_attempt
                        .lock()
                        .take()
                        .expect("network handshake starts an observed attempt");
                    attempt.wire(
                        "upstream_response",
                        "handshake_response",
                        Some(status),
                        Some(&headers),
                        Value::Null,
                    );
                    attempt.gap("websocket_handshake_body_read_failed");
                    attempt.finish(
                        "failed",
                        Some(status),
                        Some("websocket_handshake_body_read_failed".into()),
                        None,
                    );
                    if !self.allow_retries || !self.adapter.try_record_recovery_failure() {
                        return Ok(ProviderStreamResponse::Error {
                            status,
                            headers: *headers,
                            body: Err(anyhow::anyhow!(diagnostic.to_string())),
                            attempt: Box::new(attempt),
                        });
                    }
                    self.adapter.mark_upstream_idle();
                    let mut outbound = if websocket.require_affinity {
                        websocket.full_outbound.clone()
                    } else {
                        self.outbound.clone()
                    };
                    outbound.body["stream"] = Value::Bool(true);
                    return self.http_stream(outbound).await;
                }
                Err(ResponsesWebSocketAcquireError::Transport(diagnostic)) => {
                    let attempt = handshake_attempt
                        .lock()
                        .take()
                        .expect("network connect starts an observed attempt");
                    attempt.finish("failed", None, Some("websocket_connect_error".into()), None);
                    if !self.allow_retries || !self.adapter.try_record_recovery_failure() {
                        return Err(anyhow::anyhow!(diagnostic.to_string()));
                    }
                    self.adapter.mark_upstream_idle();
                    let mut outbound = if websocket.require_affinity {
                        websocket.full_outbound.clone()
                    } else {
                        self.outbound.clone()
                    };
                    outbound.body["stream"] = Value::Bool(true);
                    return self.http_stream(outbound).await;
                }
                Err(ResponsesWebSocketAcquireError::Rejected {
                    status,
                    headers,
                    body,
                }) => {
                    let attempt = handshake_attempt
                        .lock()
                        .take()
                        .expect("network handshake starts an observed attempt");
                    attempt.wire_lazy(
                        "upstream_response",
                        "handshake_response",
                        Some(status),
                        Some(&headers),
                        || bytes_value(&body),
                    );
                    tracing::debug!(
                        transport = "responses_websocket",
                        target_namespace = websocket.namespace,
                        provider_id = websocket.provider_id,
                        target_id = websocket.target_id,
                        transport_attempt = websocket.transport_attempt,
                        rejection_status = status,
                        "upstream WebSocket handshake was rejected"
                    );
                    return Ok(ProviderStreamResponse::Error {
                        status,
                        headers: *headers,
                        body: serde_json::from_slice(&body).map_err(|error| {
                            let diagnostic = crate::proxy::client::TransportDiagnostic::from_error(
                                "response_decode",
                                "decode",
                                true,
                                Some(status),
                                None,
                                &error,
                            );
                            anyhow::Error::new(error).context(diagnostic.to_string())
                        }),
                        attempt: Box::new(attempt),
                    });
                }
            }
        }
        self.http_stream(self.outbound.clone()).await
    }

    pub(super) fn websocket_stream(
        &self,
        lease: ResponsesWebSocketLease,
        full_request: Value,
        fallback_outbound: OutboundRequest,
        websocket_url: String,
        attempt: AttemptObservation,
    ) -> anyhow::Result<ProviderStreamResponse> {
        let response_continuation_available = Arc::new(AtomicBool::new(true));
        Ok(ProviderStreamResponse::Stream(Box::new(ProviderStream {
            adapter: self.adapter.clone(),
            decoder: crate::protocol::transform::ProtocolTransform::global()
                .decode_stream(self.adapter.binding.protocol)?,
            reasoning: StreamReasoningNormalizer::default(),
            source: ProviderStreamSource::ResponsesWebSocket(Box::new(ResponsesWebSocketStream {
                lease,
                done: false,
                done_marker_sent: false,
                event_seen: false,
                response_event_seen: false,
                replayed_full_request: false,
                allow_retries: self.allow_retries,
                artifact_transfers: self.artifact_transfers.clone(),
                client: self.client.clone(),
                fallback_outbound,
                http_fallback: None,
                http_fallback_status: None,
                fallback_attempt: None,
                full_request,
                websocket_url,
            })),
            status: 200,
            attempt,
            response_event_seen: false,
            response_continuation_available,
        })))
    }
}

impl ResponsesWebSocketStream {
    pub(super) fn using_http_fallback(&self) -> bool {
        self.http_fallback.is_some()
    }

    pub(super) fn diagnostic_http_status(&self) -> u16 {
        self.http_fallback_status.unwrap_or(101)
    }

    pub(super) async fn next_raw(
        &mut self,
        adapter: &ProviderAdapter,
        base_attempt: &AttemptObservation,
        status: u16,
    ) -> Result<Option<bytes::Bytes>, ProviderStreamError> {
        if let Some(stream) = &mut self.http_fallback {
            let fallback_status = self.http_fallback_status.unwrap_or(status);
            return match stream.next().await {
                Some(Ok(bytes)) => {
                    if let Some(fallback_attempt) = &self.fallback_attempt {
                        fallback_attempt.wire_lazy(
                            "upstream_response",
                            "sse_chunk",
                            Some(fallback_status),
                            None,
                            || bytes_value(&bytes),
                        );
                    }
                    self.response_event_seen = true;
                    Ok(Some(bytes))
                }
                Some(Err(error)) => {
                    let diagnostic = crate::proxy::client::TransportDiagnostic::from_reqwest(
                        "receive",
                        self.response_event_seen,
                        Some(fallback_status),
                        &error,
                    );
                    if let Some(fallback_attempt) = &self.fallback_attempt {
                        fallback_attempt.finish(
                            "failed",
                            Some(fallback_status),
                            Some("provider_transport_error".into()),
                            None,
                        );
                    }
                    Err(ProviderStreamError::Transport(diagnostic.to_string()))
                }
                None => Ok(None),
            };
        }
        if self.done {
            if self.done_marker_sent {
                return Ok(None);
            }
            self.done_marker_sent = true;
            return Ok(Some(bytes::Bytes::from_static(b"data: [DONE]\n\n")));
        }
        loop {
            adapter.mark_upstream_started();
            let active_attempt = self.fallback_attempt.clone();
            let attempt = active_attempt.as_deref().unwrap_or(base_attempt);
            let message = match self.lease.next().await {
                Some(Ok(message)) => message,
                Some(Err(error)) => {
                    let diagnostic = crate::proxy::client::TransportDiagnostic::from_error(
                        "websocket_transport",
                        "receive",
                        self.response_event_seen,
                        Some(101),
                        None,
                        &error,
                    );
                    return self
                        .recover_reused_connection(adapter, attempt, diagnostic)
                        .await;
                }
                None => {
                    let diagnostic = crate::proxy::client::TransportDiagnostic::from_message(
                        "websocket_eof",
                        "receive",
                        self.response_event_seen,
                        Some(101),
                        None,
                        "Responses WebSocket closed before a terminal event",
                    );
                    return self
                        .recover_reused_connection(adapter, attempt, diagnostic)
                        .await;
                }
            };
            adapter.mark_upstream_idle();
            let text = match message {
                reqwest_websocket::Message::Text(text) => {
                    self.response_event_seen = true;
                    attempt.wire_lazy("upstream_response", "text", Some(status), None, || {
                        Value::String(text.to_string())
                    });
                    text
                }
                reqwest_websocket::Message::Ping(bytes) => {
                    attempt.wire_lazy("upstream_response", "ping", Some(status), None, || {
                        bytes_value(&bytes)
                    });
                    continue;
                }
                reqwest_websocket::Message::Pong(bytes) => {
                    attempt.wire_lazy("upstream_response", "pong", Some(status), None, || {
                        bytes_value(&bytes)
                    });
                    continue;
                }
                reqwest_websocket::Message::Close { code, reason } => {
                    let close_code = format!("{code:?}");
                    attempt.wire_lazy(
                        "upstream_response",
                        "close",
                        Some(status),
                        None,
                        || serde_json::json!({"code": close_code, "reason": reason.to_string()}),
                    );
                    let diagnostic =
                        websocket_close_diagnostic(code, &reason, self.response_event_seen);
                    return self
                        .recover_reused_connection(adapter, attempt, diagnostic)
                        .await;
                }
                reqwest_websocket::Message::Binary(bytes) => {
                    self.response_event_seen = true;
                    attempt.wire_lazy("upstream_response", "binary", Some(status), None, || {
                        bytes_value(&bytes)
                    });
                    let diagnostic = crate::proxy::client::TransportDiagnostic::from_message(
                        "protocol_frame",
                        "decode",
                        true,
                        Some(101),
                        None,
                        "Responses WebSocket returned a binary event",
                    );
                    return Err(ProviderStreamError::Uncertain(diagnostic.to_string()));
                }
            };
            let mut value: Value = serde_json::from_str(&text).map_err(|error| {
                let diagnostic = crate::proxy::client::TransportDiagnostic::from_error(
                    "protocol_decode",
                    "decode",
                    true,
                    Some(101),
                    None,
                    &error,
                );
                ProviderStreamError::Uncertain(diagnostic.to_string())
            })?;
            adapter
                .normalize_responses_websocket_event(&mut value)
                .map_err(|error| ProviderStreamError::Local(error.to_string()))?;
            if !adapter.retain_responses_websocket_event(&value) {
                continue;
            }
            let event_type = value
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if event_type == "error" {
                let code = value
                    .pointer("/error/code")
                    .or_else(|| value.get("code"))
                    .and_then(Value::as_str);
                let trace = self.lease.trace();
                tracing::debug!(
                    transport = "responses_websocket",
                    provider_id = trace.provider_id,
                    target_id = trace.target_id,
                    transport_attempt = trace.transport_attempt,
                    upstream_event = "error",
                    upstream_error_code = code.unwrap_or("unknown"),
                    "received upstream WebSocket error event"
                );
                if self.allow_retries
                    && code == Some("websocket_connection_limit_reached")
                    && !self.event_seen
                    && adapter.try_record_recovery_failure()
                {
                    tracing::debug!(
                        transport = "responses_websocket",
                        provider_id = trace.provider_id,
                        target_id = trace.target_id,
                        transport_attempt = trace.transport_attempt,
                        fallback_reason = "connection_limit",
                        "falling back from Responses WebSocket to HTTP/SSE"
                    );
                    attempt.wire("upstream_request", "close", None, None, Value::Null);
                    self.lease.connection_limit();
                    return self.switch_to_http_fallback(adapter, attempt).await;
                }
                if self.allow_retries
                    && code == Some("previous_response_not_found")
                    && self.lease.previous_response_id().is_some()
                    && !self.event_seen
                    && !self.replayed_full_request
                    && adapter.try_record_recovery_failure()
                {
                    self.lease.invalidate_previous();
                    attempt.finish(
                        "failed",
                        Some(status),
                        Some("previous_response_not_found".into()),
                        None,
                    );
                    adapter.mark_upstream_idle();
                    let body = match &self.artifact_transfers {
                        Some(transfers) => std::borrow::Cow::Owned(
                            transfers
                                .materialize(&adapter.binding.gateway, &self.full_request)
                                .await
                                .map_err(|error| ProviderStreamError::Local(error.to_string()))?,
                        ),
                        None => std::borrow::Cow::Borrowed(&self.full_request),
                    };
                    let replay_text = serde_json::to_string(body.as_ref())
                        .map_err(|error| ProviderStreamError::Local(error.to_string()))?;
                    let replay_attempt =
                        Arc::new(adapter.begin_attempt("websocket", &self.websocket_url));
                    let replay_payload = replay_text.clone();
                    if let Err(error) = self.lease.send_text(replay_text).await {
                        let diagnostic = crate::proxy::client::TransportDiagnostic::from_error(
                            "websocket_transport",
                            "send",
                            self.response_event_seen,
                            Some(101),
                            None,
                            error.as_ref(),
                        );
                        replay_attempt.finish(
                            "failed",
                            Some(101),
                            Some("websocket_send_error".into()),
                            None,
                        );
                        return Err(ProviderStreamError::Uncertain(diagnostic.to_string()));
                    }
                    replay_attempt.wire(
                        "upstream_request",
                        "text",
                        None,
                        None,
                        Value::String(replay_payload),
                    );
                    self.fallback_attempt = Some(replay_attempt);
                    self.replayed_full_request = true;
                    continue;
                }
                self.lease.invalidate_previous();
                attempt.wire("upstream_request", "close", None, None, Value::Null);
                self.lease.terminal();
                self.done = true;
            } else {
                self.event_seen = true;
                if responses_websocket_terminal(&event_type, &value) {
                    if response_completed(&event_type, &value) {
                        if let Some(response_id) = value
                            .pointer("/response/id")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                        {
                            self.lease.completed(response_id);
                        } else {
                            attempt.wire("upstream_request", "close", None, None, Value::Null);
                            self.lease.terminal();
                        }
                    } else {
                        let code = value
                            .pointer("/response/error/code")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown");
                        tracing::debug!(
                            transport = "responses_websocket",
                            upstream_event = event_type,
                            upstream_error_code = code,
                            "received unsuccessful upstream WebSocket terminal event"
                        );
                        self.lease.invalidate_previous();
                        attempt.wire("upstream_request", "close", None, None, Value::Null);
                        self.lease.terminal();
                    }
                    self.done = true;
                }
            }
            let text = serde_json::to_string(&value)
                .map_err(|error| ProviderStreamError::Uncertain(error.to_string()))?;
            return Ok(Some(bytes::Bytes::from(format!(
                "event: {event_type}\ndata: {text}\n\n"
            ))));
        }
    }

    async fn recover_reused_connection(
        &mut self,
        adapter: &ProviderAdapter,
        attempt: &AttemptObservation,
        diagnostic: crate::proxy::client::TransportDiagnostic,
    ) -> Result<Option<bytes::Bytes>, ProviderStreamError> {
        if !self.allow_retries
            || self.event_seen
            || !self.lease.reused_connection()
            || !adapter.try_record_recovery_failure()
        {
            return Err(ProviderStreamError::Uncertain(diagnostic.to_string()));
        }
        let trace = self.lease.trace();
        tracing::warn!(
            transport = "responses_websocket",
            provider_id = trace.provider_id,
            target_id = trace.target_id,
            transport_attempt = trace.transport_attempt,
            failure_stage = diagnostic.stage.as_str(),
            fallback_transport = "http_sse",
            "reused upstream WebSocket failed before a response; retrying silently"
        );
        attempt.wire("upstream_request", "close", None, None, Value::Null);
        attempt.finish(
            "failed",
            diagnostic.http_status,
            Some("websocket_connection_error".into()),
            None,
        );
        self.lease.terminal();
        self.switch_to_http_fallback(adapter, attempt).await
    }

    async fn switch_to_http_fallback(
        &mut self,
        adapter: &ProviderAdapter,
        attempt: &AttemptObservation,
    ) -> Result<Option<bytes::Bytes>, ProviderStreamError> {
        adapter.mark_upstream_idle();
        attempt.finish(
            "failed",
            None,
            Some("responses_websocket_fallback".into()),
            None,
        );
        let body = match &self.artifact_transfers {
            Some(transfers) => std::borrow::Cow::Owned(
                transfers
                    .materialize(&adapter.binding.gateway, &self.fallback_outbound.body)
                    .await
                    .map_err(|error| ProviderStreamError::Local(error.to_string()))?,
            ),
            None => std::borrow::Cow::Borrowed(&self.fallback_outbound.body),
        };
        let request_body = bytes::Bytes::from(
            serde_json::to_vec(body.as_ref())
                .map_err(|error| ProviderStreamError::Local(error.to_string()))?,
        );
        let fallback_attempt = Arc::new(adapter.begin_attempt("sse", &self.fallback_outbound.url));
        let result = self
            .client
            .call_stream_raw(
                &self.fallback_outbound.url,
                self.fallback_outbound.headers.clone(),
                request_body,
                fallback_attempt.http_wire_observer(),
            )
            .await;
        let (response, status) = match result {
            Ok(response) => response,
            Err(error) => {
                let diagnostic = error
                    .downcast_ref::<crate::proxy::client::UpstreamTransportError>()
                    .map(|transport| transport.diagnostic().clone())
                    .unwrap_or_else(|| {
                        crate::proxy::client::TransportDiagnostic::from_error(
                            "request",
                            "send",
                            false,
                            None,
                            None,
                            error.as_ref(),
                        )
                    });
                fallback_attempt.finish(
                    "failed",
                    diagnostic.http_status,
                    Some("provider_transport_error".into()),
                    None,
                );
                return Err(ProviderStreamError::Transport(diagnostic.to_string()));
            }
        };
        if status >= 400 {
            fallback_attempt.finish("failed", Some(status), Some("upstream_error".into()), None);
            return Err(ProviderStreamError::Transport(format!(
                "HTTP/SSE fallback was rejected with status {status}"
            )));
        }
        self.fallback_attempt = Some(fallback_attempt);
        self.http_fallback_status = Some(status);
        self.response_event_seen = false;
        self.http_fallback = Some(response.bytes_stream().boxed());
        let stream = self
            .http_fallback
            .as_mut()
            .expect("HTTP fallback stream was just installed");
        match stream.next().await {
            Some(Ok(bytes)) => {
                if let Some(attempt) = &self.fallback_attempt {
                    attempt.wire_lazy("upstream_response", "sse_chunk", Some(status), None, || {
                        bytes_value(&bytes)
                    });
                }
                self.response_event_seen = true;
                Ok(Some(bytes))
            }
            Some(Err(error)) => {
                let diagnostic = crate::proxy::client::TransportDiagnostic::from_reqwest(
                    "receive",
                    false,
                    Some(status),
                    &error,
                );
                if let Some(attempt) = &self.fallback_attempt {
                    attempt.finish(
                        "failed",
                        Some(status),
                        Some("provider_transport_error".into()),
                        None,
                    );
                }
                Err(ProviderStreamError::Transport(diagnostic.to_string()))
            }
            None => Ok(None),
        }
    }
}

fn responses_websocket_url(http_url: &str) -> anyhow::Result<String> {
    let mut url = reqwest::Url::parse(http_url)?;
    match url.scheme() {
        "http" => {
            url.set_scheme("ws")
                .map_err(|_| anyhow::anyhow!("invalid Responses WebSocket URL"))?;
        }
        "https" => {
            url.set_scheme("wss")
                .map_err(|_| anyhow::anyhow!("invalid Responses WebSocket URL"))?;
        }
        scheme => anyhow::bail!("unsupported Responses WebSocket URL scheme: {scheme}"),
    }
    Ok(url.to_string())
}

fn websocket_close_diagnostic(
    code: reqwest_websocket::CloseCode,
    reason: &str,
    has_received_response_event: bool,
) -> crate::proxy::client::TransportDiagnostic {
    crate::proxy::client::TransportDiagnostic::from_message(
        "websocket_close",
        "receive",
        has_received_response_event,
        Some(101),
        Some(code.to_string()),
        format!("Responses WebSocket closed before a terminal event: {reason}"),
    )
}

fn responses_websocket_terminal(event_type: &str, value: &Value) -> bool {
    matches!(
        event_type,
        "response.completed" | "response.failed" | "response.incomplete"
    ) || (event_type == "response.done"
        && value
            .pointer("/response/status")
            .and_then(Value::as_str)
            .is_some_and(|status| matches!(status, "completed" | "failed" | "incomplete")))
}

fn response_completed(event_type: &str, value: &Value) -> bool {
    event_type == "response.completed"
        || (event_type == "response.done"
            && value.pointer("/response/status").and_then(Value::as_str) == Some("completed"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn early_websocket_close_preserves_code_response_state_and_redacts_reason_url() {
        let diagnostic = websocket_close_diagnostic(
            reqwest_websocket::CloseCode::Policy,
            "denied by https://close-user:close-password@example.test/path?token=close-secret",
            false,
        );
        let rendered = diagnostic.to_string();

        assert_eq!(diagnostic.stage, "receive");
        assert!(!diagnostic.has_received_response_event);
        assert_eq!(diagnostic.http_status, Some(101));
        assert_eq!(diagnostic.websocket_close_code.as_deref(), Some("1008"));
        assert!(rendered.contains("websocket_close_code=1008"));
        for secret in ["close-user", "close-password", "close-secret"] {
            assert!(!rendered.contains(secret), "leaked {secret}: {rendered}");
        }
    }
}
