use super::*;

impl ProviderCall {
    async fn call_non_stream_once(
        &self,
        outbound: &OutboundRequest,
    ) -> anyhow::Result<(Value, u16, HeaderMap, AttemptObservation)> {
        let request_body = self.request_body_bytes(outbound).await?;
        let mut request_headers = outbound.headers.clone();
        request_headers
            .entry(reqwest::header::CONTENT_TYPE)
            .or_insert(reqwest::header::HeaderValue::from_static(
                "application/json",
            ));
        let attempt = self.adapter.begin_attempt("http", &outbound.url);
        let result = self
            .client
            .call_non_stream_raw(
                &outbound.url,
                request_headers,
                request_body,
                attempt.http_wire_observer(),
            )
            .await;
        let (raw, status, headers, _response_body) = match result {
            Ok(response) => response,
            Err(error) => {
                let diagnostic = if let Some(decode) =
                    error.downcast_ref::<crate::proxy::client::UpstreamResponseDecodeError>()
                {
                    attempt.finish(
                        "failed",
                        Some(decode.status),
                        Some("response_decode_error".into()),
                        None,
                    );
                    crate::proxy::client::TransportDiagnostic::from_error(
                        "response_decode",
                        "decode",
                        true,
                        Some(decode.status),
                        None,
                        decode,
                    )
                } else if let Some(transport) =
                    error.downcast_ref::<crate::proxy::client::UpstreamTransportError>()
                {
                    let diagnostic = transport.diagnostic().clone();
                    attempt.finish(
                        "failed",
                        diagnostic.http_status,
                        Some("provider_transport_error".into()),
                        None,
                    );
                    diagnostic
                } else {
                    let diagnostic = crate::proxy::client::TransportDiagnostic::from_error(
                        "request",
                        "send",
                        false,
                        None,
                        None,
                        error.as_ref(),
                    );
                    attempt.finish(
                        "failed",
                        None,
                        Some("provider_transport_error".into()),
                        None,
                    );
                    diagnostic
                };
                let safe = diagnostic.to_string();
                return Err(error.context(safe));
            }
        };
        Ok((raw, status, headers, attempt))
    }

    pub(crate) async fn call_compact(
        &mut self,
    ) -> anyhow::Result<(Value, u16, HeaderMap, AttemptObservation)> {
        self.call_non_stream_once(&self.outbound).await
    }

    pub(crate) async fn call_non_stream(&mut self) -> anyhow::Result<ProviderUnaryResponse> {
        loop {
            let (mut raw, mut status, mut headers, mut attempt) =
                self.call_non_stream_once(&self.outbound).await?;
            if self.allow_retries
                && status == 401
                && self
                    .adapter
                    .refresh_auth_on_unauthorized(&mut self.outbound)
                    .await?
                && self.adapter.try_record_recovery_failure()
            {
                attempt.finish("failed", Some(status), Some("unauthorized".into()), None);
                self.adapter.mark_upstream_idle();
                (raw, status, headers, attempt) = self.call_non_stream_once(&self.outbound).await?;
            }
            if self
                .outbound
                .body
                .get("previous_response_id")
                .and_then(Value::as_str)
                .is_some()
                && self.adapter.is_continuation_not_found(status, &raw)
                && self.continuation_fallback.is_some()
                && self.adapter.try_record_recovery_failure()
                && let Some(full_outbound) = self.continuation_fallback.take()
            {
                attempt.finish(
                    "failed",
                    Some(status),
                    Some("previous_response_not_found".into()),
                    None,
                );
                tracing::debug!(
                    transport = "http",
                    provider_id = self.adapter.binding.provider.id,
                    fallback_reason = "previous_response_not_found",
                    "replaying full request after unavailable Target continuation"
                );
                self.adapter.mark_upstream_idle();
                self.outbound = full_outbound;
                continue;
            }
            if status < 400 {
                self.adapter.mark_upstream_idle();
            }
            let canonical = self
                .adapter
                .parse_response(InboundResponse {
                    status,
                    body: raw.clone(),
                })
                .await;
            return Ok(ProviderUnaryResponse {
                raw,
                canonical,
                status,
                headers,
                attempt,
            });
        }
    }

    async fn call_stream_once(
        &self,
        outbound: &OutboundRequest,
    ) -> anyhow::Result<(reqwest::Response, u16, AttemptObservation)> {
        let request_body = self.request_body_bytes(outbound).await?;
        let mut request_headers = outbound.headers.clone();
        request_headers
            .entry(reqwest::header::CONTENT_TYPE)
            .or_insert(reqwest::header::HeaderValue::from_static(
                "application/json",
            ));
        let attempt = self.adapter.begin_attempt("sse", &outbound.url);
        match self
            .client
            .call_stream_raw(
                &outbound.url,
                request_headers,
                request_body,
                attempt.http_wire_observer(),
            )
            .await
        {
            Ok((response, status)) => Ok((response, status, attempt)),
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
                attempt.finish(
                    "failed",
                    diagnostic.http_status,
                    Some("provider_transport_error".into()),
                    None,
                );
                let safe = diagnostic.to_string();
                Err(error.context(safe))
            }
        }
    }

    pub(super) async fn http_stream(
        &mut self,
        mut outbound: OutboundRequest,
    ) -> anyhow::Result<ProviderStreamResponse> {
        loop {
            let (mut response, mut status, mut attempt) = self.call_stream_once(&outbound).await?;
            if self.allow_retries
                && status == 401
                && self
                    .adapter
                    .refresh_auth_on_unauthorized(&mut outbound)
                    .await?
                && self.adapter.try_record_recovery_failure()
            {
                attempt.finish("failed", Some(status), Some("unauthorized".into()), None);
                self.adapter.mark_upstream_idle();
                (response, status, attempt) = self.call_stream_once(&outbound).await?;
            }
            let headers = response.headers().clone();
            if status >= 400 {
                let mut body_buffer = bytes::BytesMut::new();
                let mut body_stream = response.bytes_stream();
                let body_bytes = async {
                    while let Some(chunk) = body_stream.next().await {
                        let chunk = chunk.map_err(|error| {
                            let diagnostic =
                                crate::proxy::client::TransportDiagnostic::from_reqwest(
                                    "receive",
                                    !body_buffer.is_empty(),
                                    Some(status),
                                    &error,
                                );
                            anyhow::Error::new(error).context(diagnostic.to_string())
                        })?;
                        attempt.wire_lazy(
                            "upstream_response",
                            "body_chunk",
                            Some(status),
                            None,
                            || bytes_value(&chunk),
                        );
                        body_buffer.extend_from_slice(&chunk);
                    }
                    Ok::<bytes::Bytes, anyhow::Error>(body_buffer.freeze())
                }
                .await;
                let body = body_bytes.and_then(|bytes| {
                    serde_json::from_slice(&bytes).map_err(|error| {
                        let diagnostic = crate::proxy::client::TransportDiagnostic::from_error(
                            "response_decode",
                            "decode",
                            true,
                            Some(status),
                            None,
                            &error,
                        );
                        anyhow::Error::new(error).context(diagnostic.to_string())
                    })
                });
                if outbound
                    .body
                    .get("previous_response_id")
                    .and_then(Value::as_str)
                    .is_some()
                    && body
                        .as_ref()
                        .is_ok_and(|body| self.adapter.is_continuation_not_found(status, body))
                    && self.continuation_fallback.is_some()
                    && self.adapter.try_record_recovery_failure()
                    && let Some(full_outbound) = self.continuation_fallback.take()
                {
                    attempt.finish(
                        "failed",
                        Some(status),
                        Some("previous_response_not_found".into()),
                        None,
                    );
                    tracing::debug!(
                        transport = "http_sse",
                        provider_id = self.adapter.binding.provider.id,
                        fallback_reason = "previous_response_not_found",
                        "replaying full request after unavailable Target continuation"
                    );
                    self.adapter.mark_upstream_idle();
                    outbound = full_outbound;
                    continue;
                }
                self.outbound = outbound;
                return Ok(ProviderStreamResponse::Error {
                    status,
                    headers,
                    body,
                    attempt: Box::new(attempt),
                });
            }
            self.outbound = outbound;
            return Ok(ProviderStreamResponse::Stream(Box::new(ProviderStream {
                adapter: self.adapter.clone(),
                decoder: crate::protocol::transform::ProtocolTransform::global()
                    .decode_stream(self.adapter.binding.protocol)?,
                reasoning: StreamReasoningNormalizer::default(),
                source: ProviderStreamSource::Http(response.bytes_stream().boxed()),
                status,
                attempt,
                response_event_seen: false,
                response_continuation_available: Arc::new(AtomicBool::new(false)),
            })));
        }
    }
}
