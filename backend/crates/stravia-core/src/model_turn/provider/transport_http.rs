use super::*;

impl ProviderCall {
    async fn call_non_stream_once(
        &self,
        outbound: &OutboundRequest,
    ) -> anyhow::Result<(Value, u16, HeaderMap, AttemptObservation)> {
        let request_body = bytes::Bytes::from(serde_json::to_vec(&outbound.body)?);
        let mut request_headers = outbound.headers.clone();
        request_headers
            .entry(reqwest::header::CONTENT_TYPE)
            .or_insert(reqwest::header::HeaderValue::from_static(
                "application/json",
            ));
        let attempt = self
            .adapter
            .begin_attempt("http", &outbound.url, &request_headers, || {
                bytes_value(&request_body)
            });
        let result = self
            .client
            .call_non_stream_raw(&outbound.url, request_headers, request_body)
            .await;
        let (raw, status, headers, response_body) = match result {
            Ok(response) => response,
            Err(error) => {
                if let Some(decode) =
                    error.downcast_ref::<crate::proxy::client::UpstreamResponseDecodeError>()
                {
                    attempt.wire_lazy(
                        "upstream_response",
                        "http_response",
                        Some(decode.status),
                        Some(&decode.headers),
                        || bytes_value(&decode.body),
                    );
                    attempt.finish(
                        "failed",
                        Some(decode.status),
                        Some("response_decode_error".into()),
                        None,
                    );
                } else {
                    attempt.finish(
                        "failed",
                        None,
                        Some("provider_transport_error".into()),
                        None,
                    );
                }
                return Err(error);
            }
        };
        attempt.wire_lazy(
            "upstream_response",
            "http_response",
            Some(status),
            Some(&headers),
            || bytes_value(&response_body),
        );
        Ok((raw, status, headers, attempt))
    }

    pub(crate) async fn call_compact(
        &mut self,
    ) -> anyhow::Result<(Value, u16, HeaderMap, AttemptObservation)> {
        let (mut raw, mut status, mut headers, mut attempt) =
            self.call_non_stream_once(&self.outbound).await?;
        if status == 401
            && self
                .adapter
                .refresh_auth_on_unauthorized(&mut self.outbound)
                .await?
        {
            attempt.finish("failed", Some(status), Some("unauthorized".into()), None);
            (raw, status, headers, attempt) = self.call_non_stream_once(&self.outbound).await?;
        }
        Ok((raw, status, headers, attempt))
    }

    pub(crate) async fn call_non_stream(&mut self) -> anyhow::Result<ProviderUnaryResponse> {
        loop {
            let (mut raw, mut status, mut headers, mut attempt) =
                self.call_non_stream_once(&self.outbound).await?;
            if status == 401
                && self
                    .adapter
                    .refresh_auth_on_unauthorized(&mut self.outbound)
                    .await?
            {
                attempt.finish("failed", Some(status), Some("unauthorized".into()), None);
                (raw, status, headers, attempt) = self.call_non_stream_once(&self.outbound).await?;
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
                self.outbound = full_outbound;
                continue;
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
        let request_body = bytes::Bytes::from(serde_json::to_vec(&outbound.body)?);
        let mut request_headers = outbound.headers.clone();
        request_headers
            .entry(reqwest::header::CONTENT_TYPE)
            .or_insert(reqwest::header::HeaderValue::from_static(
                "application/json",
            ));
        let attempt = self
            .adapter
            .begin_attempt("sse", &outbound.url, &request_headers, || {
                bytes_value(&request_body)
            });
        match self
            .client
            .call_stream_raw(&outbound.url, request_headers, request_body)
            .await
        {
            Ok((response, status)) => {
                attempt.wire(
                    "upstream_response",
                    "http_headers",
                    Some(status),
                    Some(response.headers()),
                    Value::Null,
                );
                Ok((response, status, attempt))
            }
            Err(error) => {
                attempt.finish(
                    "failed",
                    None,
                    Some("provider_transport_error".into()),
                    None,
                );
                Err(error)
            }
        }
    }

    pub(super) async fn http_stream(
        &mut self,
        mut outbound: OutboundRequest,
    ) -> anyhow::Result<ProviderStreamResponse> {
        loop {
            let (mut response, mut status, mut attempt) = self.call_stream_once(&outbound).await?;
            if status == 401
                && self
                    .adapter
                    .refresh_auth_on_unauthorized(&mut outbound)
                    .await?
            {
                attempt.finish("failed", Some(status), Some("unauthorized".into()), None);
                (response, status, attempt) = self.call_stream_once(&outbound).await?;
            }
            let headers = response.headers().clone();
            if status >= 400 {
                let body_bytes = response.bytes().await.map_err(anyhow::Error::from);
                if let Ok(bytes) = &body_bytes {
                    attempt.wire_lazy("upstream_response", "http_body", Some(status), None, || {
                        bytes_value(bytes)
                    });
                }
                let body = body_bytes
                    .and_then(|bytes| serde_json::from_slice(&bytes).map_err(anyhow::Error::from));
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
                    outbound = full_outbound;
                    continue;
                }
                self.outbound = outbound;
                return Ok(ProviderStreamResponse::Error {
                    status,
                    headers,
                    body,
                    attempt,
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
                response_continuation_available: Arc::new(AtomicBool::new(false)),
            })));
        }
    }
}
