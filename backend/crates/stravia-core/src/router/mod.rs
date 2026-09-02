pub(crate) mod cache_affinity;
pub mod health;
mod matcher;
pub mod selector;

pub use matcher::RouteCache;
pub use selector::{
    AttemptFailureDisposition, ConversationIdentity, RouteAttemptContext, RouteAttemptPolicy,
    RouteAttemptReservation, RoutePolicyState, RouteSchedulingSnapshot, SelectedTarget,
    TargetSchedulingSnapshot, conversation_identity, selected_target_key,
};

use crate::db::models::Route;

impl RouteCache {
    pub fn match_model(&self, model: &str) -> Option<&Route> {
        matcher::match_model(&self.models, model)
    }
}
