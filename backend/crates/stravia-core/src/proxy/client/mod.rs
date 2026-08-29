//! Adapter-agnostic upstream HTTP and Responses WebSocket transports.
//!
//! URL construction and authentication remain at the provider call site via
//! `VendorRegistry::resolve` and `VendorExtension`. `ProxyClient` receives
//! fully built URLs, headers, and canonical request bodies; this module owns
//! the network call plus process-local Responses WebSocket capability,
//! connection-affinity, and lifetime state.

use anyhow::Result;
use reqwest::header::HeaderMap;
use serde_json::Value;

#[derive(Clone)]
pub struct ProxyClient {
    pub http: reqwest::Client,
    pub responses_websocket: reqwest::Client,
}

#[derive(Debug, thiserror::Error)]
#[error("error decoding response body: {source}")]
pub struct UpstreamResponseDecodeError {
    pub source: serde_json::Error,
    pub status: u16,
    pub headers: HeaderMap,
    pub body: bytes::Bytes,
}

impl UpstreamResponseDecodeError {
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

impl ProxyClient {
    pub fn new(http: reqwest::Client) -> Self {
        Self {
            responses_websocket: http.clone(),
            http,
        }
    }

    pub fn with_responses_websocket(
        http: reqwest::Client,
        responses_websocket: reqwest::Client,
    ) -> Self {
        Self {
            http,
            responses_websocket,
        }
    }

    pub async fn call_non_stream(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Value,
    ) -> Result<(Value, u16, HeaderMap)> {
        let resp = self
            .http
            .post(url)
            .headers(headers)
            .json(&body)
            .send()
            .await?;
        let status = resp.status().as_u16();
        let resp_headers = resp.headers().clone();
        let bytes = resp.bytes().await?;
        let json: Value =
            serde_json::from_slice(&bytes).map_err(|source| UpstreamResponseDecodeError {
                source,
                status,
                headers: resp_headers.clone(),
                body: bytes,
            })?;
        Ok((json, status, resp_headers))
    }

    pub async fn call_stream(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Value,
    ) -> Result<(reqwest::Response, u16)> {
        let resp = self
            .http
            .post(url)
            .headers(headers)
            .json(&body)
            .send()
            .await?;
        let status = resp.status().as_u16();
        Ok((resp, status))
    }
}

mod websocket;
pub(crate) use websocket::*;
