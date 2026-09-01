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
    replayed_full_request: bool,
    full_request: Value,
    client: ProxyClient,
    fallback_outbound: OutboundRequest,
    http_fallback: Option<BoxStream<'static, Result<bytes::Bytes, reqwest::Error>>>,
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
        ProviderCall {
            adapter: self,
            client,
            outbound,
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
            let websocket_url = responses_websocket_url(&self.outbound.url)?;
            let previous_response_id = self
                .outbound
                .body
                .get("previous_response_id")
                .and_then(Value::as_str);
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
                .prepare_responses_websocket_headers(&mut headers, connection)?;
            #[cfg(debug_assertions)]
            let capture_headers = headers.clone();
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
                    },
                    previous_response_id,
                    websocket.session_affinity.as_deref(),
                    websocket.require_affinity,
                )
                .await;
            match lease {
                Ok(mut lease) => {
                    let request_body =
                        if websocket.require_affinity && lease.previous_response_id().is_none() {
                            &websocket.full_outbound.body
                        } else {
                            &self.outbound.body
                        };
                    let connection = lease.connection_metadata();
                    let request = self
                        .adapter
                        .build_responses_websocket_request(request_body, connection)?;
                    let full_request = self.adapter.build_responses_websocket_request(
                        &websocket.full_outbound.body,
                        connection,
                    )?;
                    #[cfg(debug_assertions)]
                    self.adapter.capture_upstream_request(
                        crate::wire_capture::CaptureTransport::WebSocket,
                        &capture_headers,
                        &request,
                    );
                    if let Err(error) = lease.send(&request).await {
                        if lease.reused_connection() {
                            let trace = lease.trace();
                            tracing::warn!(
                                transport = "responses_websocket",
                                provider_id = trace.provider_id,
                                target_id = trace.target_id,
                                transport_attempt = trace.transport_attempt,
                                failure_stage = "send",
                                error = %error,
                                fallback_transport = "http_sse",
                                "reused upstream WebSocket failed before a response; retrying silently"
                            );
                            lease.terminal();
                            drop(lease);
                            let mut outbound = websocket.full_outbound.clone();
                            outbound.body["stream"] = Value::Bool(true);
                            return self.http_stream(outbound).await;
                        }
                        return Ok(ProviderStreamResponse::Uncertain {
                            message: error.to_string(),
                        });
                    }
                    let mut fallback_outbound = websocket.full_outbound.clone();
                    fallback_outbound.body["stream"] = Value::Bool(true);
                    return self.websocket_stream(lease, full_request, fallback_outbound);
                }
                Err(
                    error @ (ResponsesWebSocketAcquireError::Unsupported
                    | ResponsesWebSocketAcquireError::Cooldown
                    | ResponsesWebSocketAcquireError::Transport(_)),
                ) => {
                    let fallback_reason = match &error {
                        ResponsesWebSocketAcquireError::Unsupported => "unsupported",
                        ResponsesWebSocketAcquireError::Cooldown => "cooldown",
                        ResponsesWebSocketAcquireError::Transport(_) => "connect_failure",
                        ResponsesWebSocketAcquireError::Rejected(_) => unreachable!(),
                    };
                    tracing::debug!(
                        transport = "responses_websocket",
                        target_namespace = websocket.namespace,
                        provider_id = websocket.provider_id,
                        target_id = websocket.target_id,
                        transport_attempt = websocket.transport_attempt,
                        fallback_reason,
                        error = %error,
                        "falling back from Responses WebSocket to HTTP/SSE"
                    );
                    let mut outbound = if websocket.require_affinity {
                        websocket.full_outbound.clone()
                    } else {
                        self.outbound.clone()
                    };
                    outbound.body["stream"] = Value::Bool(true);
                    return self.http_stream(outbound).await;
                }
                Err(ResponsesWebSocketAcquireError::Rejected(status)) => {
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
                        headers: HeaderMap::new(),
                        body: Ok(serde_json::json!({
                            "error": {
                                "message": "Responses WebSocket handshake was rejected"
                            }
                        })),
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
                replayed_full_request: false,
                client: self.client.clone(),
                fallback_outbound,
                http_fallback: None,
                full_request,
            })),
            status: 200,
            headers: HeaderMap::new(),
            response_continuation_available,
        })))
    }
}

impl ResponsesWebSocketStream {
    pub(super) fn using_http_fallback(&self) -> bool {
        self.http_fallback.is_some()
    }

    pub(super) async fn next_raw(
        &mut self,
        adapter: &ProviderAdapter,
        _status: u16,
    ) -> Result<Option<bytes::Bytes>, ProviderStreamError> {
        if let Some(stream) = &mut self.http_fallback {
            return match stream.next().await {
                Some(Ok(bytes)) => Ok(Some(bytes)),
                Some(Err(error)) => Err(ProviderStreamError::Transport(error.to_string())),
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
            let message = match self.lease.next().await {
                Some(Ok(message)) => message,
                Some(Err(error)) => {
                    return self
                        .recover_reused_connection("receive", error.to_string())
                        .await;
                }
                None => {
                    return self
                        .recover_reused_connection(
                            "receive",
                            "Responses WebSocket closed before a terminal event".into(),
                        )
                        .await;
                }
            };
            let text = match message {
                reqwest_websocket::Message::Text(text) => {
                    #[cfg(debug_assertions)]
                    adapter.capture_upstream_response(
                        crate::wire_capture::CaptureTransport::WebSocket,
                        crate::wire_capture::CaptureRepresentation::Wire,
                        _status,
                        None,
                        text.as_bytes(),
                    );
                    text
                }
                reqwest_websocket::Message::Ping(_) | reqwest_websocket::Message::Pong(_) => {
                    continue;
                }
                reqwest_websocket::Message::Close { .. } => {
                    return self
                        .recover_reused_connection(
                            "receive",
                            "Responses WebSocket closed before a terminal event".into(),
                        )
                        .await;
                }
                reqwest_websocket::Message::Binary(_) => {
                    return Err(ProviderStreamError::Uncertain(
                        "Responses WebSocket returned a binary event".into(),
                    ));
                }
            };
            let mut value: Value = serde_json::from_str(&text).map_err(|error| {
                ProviderStreamError::Uncertain(format!(
                    "Responses WebSocket returned invalid JSON: {error}"
                ))
            })?;
            adapter
                .normalize_responses_websocket_event(&mut value)
                .map_err(|error| ProviderStreamError::Uncertain(error.to_string()))?;
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
                if code == Some("websocket_connection_limit_reached") && !self.event_seen {
                    tracing::debug!(
                        transport = "responses_websocket",
                        provider_id = trace.provider_id,
                        target_id = trace.target_id,
                        transport_attempt = trace.transport_attempt,
                        fallback_reason = "connection_limit",
                        "falling back from Responses WebSocket to HTTP/SSE"
                    );
                    self.lease.connection_limit();
                    return self.switch_to_http_fallback().await;
                }
                if code == Some("previous_response_not_found")
                    && self.lease.previous_response_id().is_some()
                    && !self.event_seen
                    && !self.replayed_full_request
                {
                    self.lease.invalidate_previous();
                    self.lease
                        .send(&self.full_request)
                        .await
                        .map_err(|error| ProviderStreamError::Uncertain(error.to_string()))?;
                    self.replayed_full_request = true;
                    continue;
                }
                self.lease.invalidate_previous();
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
        failure_stage: &'static str,
        error: String,
    ) -> Result<Option<bytes::Bytes>, ProviderStreamError> {
        if self.event_seen || !self.lease.reused_connection() {
            return Err(ProviderStreamError::Uncertain(error));
        }
        let trace = self.lease.trace();
        tracing::warn!(
            transport = "responses_websocket",
            provider_id = trace.provider_id,
            target_id = trace.target_id,
            transport_attempt = trace.transport_attempt,
            failure_stage,
            error,
            fallback_transport = "http_sse",
            "reused upstream WebSocket failed before a response; retrying silently"
        );
        self.lease.terminal();
        self.switch_to_http_fallback().await
    }

    async fn switch_to_http_fallback(
        &mut self,
    ) -> Result<Option<bytes::Bytes>, ProviderStreamError> {
        let (response, status) = self
            .client
            .call_stream(
                &self.fallback_outbound.url,
                self.fallback_outbound.headers.clone(),
                self.fallback_outbound.body.clone(),
            )
            .await
            .map_err(|error| ProviderStreamError::Transport(error.to_string()))?;
        if status >= 400 {
            return Err(ProviderStreamError::Transport(format!(
                "HTTP/SSE fallback was rejected with status {status}"
            )));
        }
        self.http_fallback = Some(response.bytes_stream().boxed());
        let stream = self
            .http_fallback
            .as_mut()
            .expect("HTTP fallback stream was just installed");
        match stream.next().await {
            Some(Ok(bytes)) => Ok(Some(bytes)),
            Some(Err(error)) => Err(ProviderStreamError::Transport(error.to_string())),
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
