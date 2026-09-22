pub(crate) mod cache_affinity;
pub(crate) mod continuation;
mod matcher;
pub(crate) mod selection;
pub mod selector;

pub(crate) use continuation::{
    ContinuationLookup, ContinuationTarget, clear_previous_response_id, parent_id_from_request,
    stamp_previous_response_id,
};
pub use matcher::RouteCache;
pub(crate) use selection::{RouteSelector, SelectionError};
pub(crate) use selector::target_key;
pub use selector::{
    AttemptFailureDisposition, ConversationIdentity, RouteAttemptContext, RouteAttemptPolicy,
    RouteAttemptReservation, RoutePolicyState, RouteSchedulingSnapshot, SelectedTarget,
    TargetRuntimeState, TargetRuntimeStatus, TargetSchedulingSnapshot, conversation_identity,
    selected_target_key,
};

use crate::db::models::Route;

impl RouteCache {
    pub fn match_model(&self, model: &str) -> Option<&Route> {
        matcher::match_model(&self.models, model)
    }

    /// A request's `model` names a logical Model ID, falling back to a Route ID.
    pub fn resolve(&self, model: &str) -> Option<&Route> {
        self.match_model(model)
            .or_else(|| self.models.iter().find(|route| route.id == model))
    }
}
