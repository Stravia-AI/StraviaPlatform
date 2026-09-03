use super::*;

impl ProviderCall {
    pub(crate) async fn call_non_stream(&mut self) -> anyhow::Result<ProviderUnaryResponse> {
        loop {
            #[cfg(debug_assertions)]
            self.adapter.capture_upstream_request(
                crate::wire_capture::CaptureTransport::Http,
                &self.outbound.headers,
                &self.outbound.body,
            );
            let (mut raw, mut status, mut headers) = self
                .client
                .call_non_stream(
                    &self.outbound.url,
                    self.outbound.headers.clone(),
                    self.outbound.body.clone(),
                )
                .await?;
            #[cfg(debug_assertions)]
            self.adapter.capture_upstream_response(
                crate::wire_capture::CaptureTransport::Http,
                crate::wire_capture::CaptureRepresentation::Wire,
                status,
                Some(&headers),
                &serde_json::to_vec(&raw).unwrap_or_default(),
            );
            if status == 401
                && self
                    .adapter
                    .refresh_auth_on_unauthorized(&mut self.outbound)
                    .await?
            {
                #[cfg(debug_assertions)]
                self.adapter.capture_upstream_request(
                    crate::wire_capture::CaptureTransport::Http,
                    &self.outbound.headers,
                    &self.outbound.body,
                );
                (raw, status, headers) = self
                    .client
                    .call_non_stream(
                        &self.outbound.url,
                        self.outbound.headers.clone(),
                        self.outbound.body.clone(),
                    )
                    .await?;
                #[cfg(debug_assertions)]
                self.adapter.capture_upstream_response(
                    crate::wire_capture::CaptureTransport::Http,
                    crate::wire_capture::CaptureRepresentation::Wire,
                    status,
                    Some(&headers),
                    &serde_json::to_vec(&raw).unwrap_or_default(),
                );
            }
            if self
                .outbound
                .body
                .get("previous_response_id")
                .and_then(Value::as_str)
                .is_some()
                && self.adapter.is_continuation_not_found(status, &raw)
                && let Some(full_outbound) = self.continuation_fallback.take()
            {
                tracing::debug!(
                    transport = "http",
                    provider_id = self.adapter.binding.provider.id,
                    fallback_reason = "previous_response_not_found",
                    "replaying full request after unavailable Target continuation"
                );
                self.outbound = full_outbound;
                continue;
            }
            let canonical = self.adapter.parse_response(InboundResponse {
                status,
                body: raw.clone(),
            });
            return Ok(ProviderUnaryResponse {
                raw,
                canonical: canonical.await,
                status,
                headers,
            });
        }
    }

    pub(super) async fn http_stream(
        &mut self,
        mut outbound: OutboundRequest,
    ) -> anyhow::Result<ProviderStreamResponse> {
        loop {
            #[cfg(debug_assertions)]
            self.adapter.capture_upstream_request(
                crate::wire_capture::CaptureTransport::Sse,
                &outbound.headers,
                &outbound.body,
            );
            let (mut response, mut status) = self
                .client
                .call_stream(
                    &outbound.url,
                    outbound.headers.clone(),
                    outbound.body.clone(),
                )
                .await?;
            #[cfg(debug_assertions)]
            self.adapter.capture_upstream_response(
                crate::wire_capture::CaptureTransport::Sse,
                crate::wire_capture::CaptureRepresentation::Wire,
                status,
                Some(response.headers()),
                &[],
            );
            if status == 401
                && self
                    .adapter
                    .refresh_auth_on_unauthorized(&mut outbound)
                    .await?
            {
                #[cfg(debug_assertions)]
                self.adapter.capture_upstream_request(
                    crate::wire_capture::CaptureTransport::Sse,
                    &outbound.headers,
                    &outbound.body,
                );
                (response, status) = self
                    .client
                    .call_stream(
                        &outbound.url,
                        outbound.headers.clone(),
                        outbound.body.clone(),
                    )
                    .await?;
                #[cfg(debug_assertions)]
                self.adapter.capture_upstream_response(
                    crate::wire_capture::CaptureTransport::Sse,
                    crate::wire_capture::CaptureRepresentation::Wire,
                    status,
                    Some(response.headers()),
                    &[],
                );
            }
            let headers = response.headers().clone();
            if status >= 400 {
                let body = response.json().await.map_err(anyhow::Error::from);
                #[cfg(debug_assertions)]
                if let Ok(body) = &body {
                    self.adapter.capture_upstream_response(
                        crate::wire_capture::CaptureTransport::Sse,
                        crate::wire_capture::CaptureRepresentation::Wire,
                        status,
                        Some(&headers),
                        &serde_json::to_vec(body).unwrap_or_default(),
                    );
                }
                if outbound
                    .body
                    .get("previous_response_id")
                    .and_then(Value::as_str)
                    .is_some()
                    && body
                        .as_ref()
                        .is_ok_and(|body| self.adapter.is_continuation_not_found(status, body))
                    && let Some(full_outbound) = self.continuation_fallback.take()
                {
                    tracing::debug!(
                        transport = "http_sse",
                        provider_id = self.adapter.binding.provider.id,
                        fallback_reason = "previous_response_not_found",
                        "replaying full request after unavailable Target continuation"
                    );
                    outbound = full_outbound;
                    continue;
                }
                self.outbound = outbound;
                return Ok(ProviderStreamResponse::Error {
                    status,
                    headers,
                    body,
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
                headers,
                response_continuation_available: Arc::new(AtomicBool::new(false)),
            })));
        }
    }
}
