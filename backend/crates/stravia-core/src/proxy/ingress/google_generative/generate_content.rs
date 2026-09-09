//! Thin ingress shell: POST /v1beta/models/:model_action

use axum::Json;
use axum::extract::{Path, Query, State, rejection::JsonRejection};
use axum::http::{HeaderMap, HeaderValue};
use axum::response::IntoResponse;
use axum::response::Response;
use serde_json::Value;
use std::collections::HashMap;

use crate::Gateway;
use crate::protocol::codec::google::gemini::decoder::GoogleDecoder;
use crate::proxy::context::RequestContext;
use crate::proxy::dispatcher::dispatch_pipeline;
use crate::proxy::ingress::observation;
use stravia_runtime_contract::protocol::ids::GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA;
use stravia_runtime_contract::protocol::ir::RawEnvelope;

pub async fn handler(
    State(gw): State<Gateway>,
    mut ctx: axum::extract::Extension<RequestContext>,
    headers: HeaderMap,
    Path(model_action): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    ctx.ingress_protocol = GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA;
    let path = format!("/v1beta/models/{model_action}");
    let body = match body {
        Ok(Json(body)) => body,
        Err(rejection) => {
            let observer = observation::begin(
                &gw,
                &ctx,
                "POST",
                &path,
                GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            );
            return observation::reject(
                observer,
                "decode",
                "invalid_json",
                rejection.into_response(),
            );
        }
    };
    let observer = observation::begin(
        &gw,
        &ctx,
        "POST",
        &path,
        GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
    );
    let (model, action) = match model_action.rsplit_once(':') {
        Some((m, a)) => (m.to_string(), a.to_string()),
        None => (model_action.clone(), "generateContent".to_string()),
    };
    let is_stream = action == "streamGenerateContent";
    let flat_headers: std::collections::HashMap<String, String> = headers
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|vs| (k.as_str().to_lowercase(), vs.to_string()))
        })
        .collect();
    let envelope = RawEnvelope::new(Some(body.clone()), flat_headers, "POST", &path);
    let mut auth_headers = headers.clone();
    inject_query_key_for_auth(&mut auth_headers, &query);
    let request = match GoogleDecoder.decode_with_model(body, &model, is_stream) {
        Ok(r) => r,
        Err(error) => {
            return observation::reject(
                observer,
                "decode",
                "invalid_request",
                crate::proxy::dispatcher::decode_error_response(format!(
                    "invalid request: Gemini decode error: {error}"
                )),
            );
        }
    };
    dispatch_pipeline(
        gw,
        observer,
        auth_headers,
        envelope,
        request,
        GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        ctx.0,
    )
    .await
}

fn inject_query_key_for_auth(headers: &mut HeaderMap, query: &HashMap<String, String>) {
    if headers.contains_key("x-goog-api-key") {
        return;
    }

    let Some(key) = query.get("key").map(|v| v.trim()).filter(|v| !v.is_empty()) else {
        return;
    };

    if let Ok(value) = HeaderValue::from_str(key) {
        headers.insert("x-goog-api-key", value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_key_is_available_for_local_auth() {
        let mut headers = HeaderMap::new();
        let query = HashMap::from([("key".to_string(), " sk-client ".to_string())]);

        inject_query_key_for_auth(&mut headers, &query);

        assert_eq!(
            headers
                .get("x-goog-api-key")
                .and_then(|value| value.to_str().ok()),
            Some("sk-client")
        );
    }
}
