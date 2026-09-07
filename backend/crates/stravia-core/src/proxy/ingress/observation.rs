use axum::body::Body;
use axum::http::HeaderMap;
use axum::response::Response;
use futures::StreamExt;
use serde_json::Value;
use std::sync::{Arc, Mutex};

use crate::Gateway;
use crate::interaction_observation::{IngressObserver, IngressStart, RejectedOutcome, RunEvent};
use crate::protocol::ids::ProtocolId;
use crate::proxy::context::RequestContext;

pub(super) fn begin(
    gateway: &Gateway,
    context: &RequestContext,
    method: &str,
    path: &str,
    protocol: ProtocolId,
) -> IngressObserver {
    context
        .extensions
        .take::<IngressObserver>()
        .unwrap_or_else(|| {
            gateway.observation.observe_ingress(IngressStart {
                id: context.request_id.clone(),
                method: method.to_owned(),
                path: path.to_owned(),
                protocol: protocol.to_string(),
            })
        })
}

pub(crate) fn reject(
    mut observer: IngressObserver,
    stage: &str,
    code: &str,
    mut response: Response,
) -> Response {
    let status_code = response.status().as_u16();
    observer.reject_pending(RejectedOutcome {
        stage: stage.to_owned(),
        code: code.to_owned(),
        status_code,
    });
    if observer.is_websocket() {
        response
            .extensions_mut()
            .insert(Arc::new(Mutex::new(Some(observer))));
        return response;
    }
    observer.record_debug(|| RunEvent::Wire {
        direction: "platform_to_client".into(),
        transport: "http".into(),
        protocol: "error".into(),
        message_type: "response_head".into(),
        model_turn_id: None,
        attempt_id: None,
        status_code: Some(status_code),
        url: None,
        headers: header_value(response.headers()),
        payload: Value::Null,
    });
    let (parts, body) = response.into_parts();
    // 观察实际发送的错误体；保留 observer 到响应体结束，确保关闭不会越过消息。
    let stream = body.into_data_stream().map(move |result| {
        match &result {
            Ok(bytes) => observer.record_debug(|| RunEvent::Wire {
                direction: "platform_to_client".into(),
                transport: "http".into(),
                protocol: "error".into(),
                message_type: "body_chunk".into(),
                model_turn_id: None,
                attempt_id: None,
                status_code: Some(status_code),
                url: None,
                headers: Value::Null,
                payload: Value::String(String::from_utf8_lossy(bytes).into_owned()),
            }),
            Err(_) => observer.record(RunEvent::ObservationGap {
                reason: "rejection_delivery_failed".into(),
            }),
        }
        result
    });
    Response::from_parts(parts, Body::from_stream(stream))
}

pub(crate) fn take_rejection_observer(response: &mut Response) -> Option<IngressObserver> {
    let pending = response
        .extensions_mut()
        .remove::<Arc<Mutex<Option<IngressObserver>>>>()?;
    let observer = pending.lock().expect("rejection observation").take();
    observer
}

fn header_value(headers: &HeaderMap) -> Value {
    Value::Object(
        headers
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_owned(), Value::String(value.to_owned())))
            })
            .collect(),
    )
}
