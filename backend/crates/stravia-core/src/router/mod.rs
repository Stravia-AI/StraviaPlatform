pub(crate) mod cache_affinity;
pub(crate) mod continuation;
pub mod health;
mod matcher;
pub mod selector;
pub(crate) mod selection;

pub(crate) use continuation::{
    ContinuationLookup, ContinuationTarget, clear_previous_response_id,
    parent_id_from_request, stamp_previous_response_id,
};
pub use matcher::RouteCache;
pub use selector::{
    AttemptFailureDisposition, ConversationIdentity, RouteAttemptContext, RouteAttemptPolicy,
    RouteAttemptReservation, RoutePolicyState, RouteSchedulingSnapshot, SelectedTarget,
    TargetSchedulingSnapshot, conversation_identity, selected_target_key,
};
pub(crate) use selection::{RouteSelector, SelectionError};

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
