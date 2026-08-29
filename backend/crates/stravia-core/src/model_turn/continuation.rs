use async_trait::async_trait;

use crate::hook::Principal;
use crate::protocol::ids::ProtocolId;
use crate::protocol::ir::{AiRequest, ProtocolExt};

pub struct ContinuationTarget<'a> {
    pub namespace: &'a str,
    pub protocol: ProtocolId,
    pub actual_model: &'a str,
    pub logical_model: &'a str,
    pub allow_ephemeral_response: bool,
}

#[async_trait]
pub trait ContinuationLookup: Send + Sync {
    async fn prepare(
        &self,
        principal: &Principal,
        target: ContinuationTarget<'_>,
        request: &mut AiRequest,
    ) -> Option<String>;
}

#[cfg(test)]
use std::sync::Arc;

#[cfg(test)]
#[derive(Clone, Default)]
pub struct ScriptedContinuation {
    previous_response_id: Option<String>,
}

#[cfg(test)]
impl ScriptedContinuation {
    pub fn hit(previous_response_id: impl Into<String>) -> Arc<dyn ContinuationLookup> {
        Arc::new(Self {
            previous_response_id: Some(previous_response_id.into()),
        })
    }

    pub fn miss() -> Arc<dyn ContinuationLookup> {
        Arc::new(Self {
            previous_response_id: None,
        })
    }
}

#[cfg(test)]
#[async_trait]
impl ContinuationLookup for ScriptedContinuation {
    async fn prepare(
        &self,
        _principal: &Principal,
        _target: ContinuationTarget<'_>,
        request: &mut AiRequest,
    ) -> Option<String> {
        match &self.previous_response_id {
            Some(previous_response_id) => {
                stamp_previous_response_id(request, previous_response_id);
                Some(previous_response_id.clone())
            }
            None => {
                clear_previous_response_id(request);
                None
            }
        }
    }
}

pub(crate) fn parent_id_from_request(request: &AiRequest) -> Option<String> {
    request
        .ext
        .as_ref()
        .and_then(|extension| match extension {
            ProtocolExt::OpenResponses(extension) => extension.previous_response_id.clone(),
            _ => None,
        })
        .or_else(|| {
            request
                .meta
                .vendor
                .ingress
                .get("previous_response_id")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
        })
}

pub(crate) fn stamp_previous_response_id(request: &mut AiRequest, parent_id: &str) {
    if let Some(ProtocolExt::OpenResponses(extension)) = request.ext.as_mut() {
        extension.previous_response_id = Some(parent_id.to_owned());
    }
    request.meta.vendor.ingress.insert(
        "previous_response_id".into(),
        serde_json::Value::String(parent_id.to_owned()),
    );
}

pub(crate) fn clear_previous_response_id(request: &mut AiRequest) {
    if let Some(ProtocolExt::OpenResponses(extension)) = request.ext.as_mut() {
        extension.previous_response_id = None;
    }
    request.meta.vendor.ingress.remove("previous_response_id");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ir::OpenResponsesExt;

    #[test]
    fn stamped_parent_survives_protocol_extension_replacement() {
        let mut request = AiRequest::new("model", Vec::new());
        request.ext = Some(ProtocolExt::OpenResponses(OpenResponsesExt::default()));

        stamp_previous_response_id(&mut request, "gateway-parent");
        request.ext = Some(ProtocolExt::OpenResponses(OpenResponsesExt::default()));

        assert_eq!(
            parent_id_from_request(&request).as_deref(),
            Some("gateway-parent")
        );
        clear_previous_response_id(&mut request);
        assert!(parent_id_from_request(&request).is_none());
    }
}
