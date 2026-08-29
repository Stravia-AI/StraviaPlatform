pub(crate) mod cache_affinity;
pub mod health;
mod matcher;
pub mod selector;

pub use matcher::ModelCache;
pub use selector::{
    AttemptFailureDisposition, CooldownStrategy, LatencyStrategy, PriorityStrategy,
    RouteAttemptPolicy, RoutingStrategy, SelectedTarget, TargetSelector, WeightedStrategy,
    selected_target_key,
};

use crate::db::models::Model;

impl ModelCache {
    pub fn match_model(&self, model: &str) -> Option<&Model> {
        matcher::match_model(&self.models, model)
    }
}
