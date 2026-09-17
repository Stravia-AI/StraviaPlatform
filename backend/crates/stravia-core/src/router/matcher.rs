use crate::db::models::Route;
use crate::storage::RouteStore;

pub struct RouteCache {
    pub models: Vec<Route>,
}

impl RouteCache {
    pub async fn load(store: &dyn RouteStore) -> anyhow::Result<Self> {
        let mut models = store.list_active().await?;
        for model in &mut models {
            model.refresh_supported_thinking_levels();
        }
        Ok(Self { models })
    }

    pub async fn reload(&mut self, store: &dyn RouteStore) -> anyhow::Result<()> {
        *self = Self::load(store).await?;
        Ok(())
    }
}

pub fn match_model<'a>(models: &'a [Route], model: &str) -> Option<&'a Route> {
    models.iter().find(|m| m.model_id == model)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(id: &str, model_id: &str) -> Route {
        Route {
            id: id.into(),
            model_id: model_id.into(),
            display_name: None,
            balance: "traffic_equalization".into(),
            target_provider: String::new(),
            target_model: String::new(),
            is_enabled: true,
            created_at: String::new(),
            supported_thinking_levels: sqlx::types::Json(Vec::new()),
            context_window: None,
            output_max_tokens: None,
            supports_image_input: false,
            targets: Vec::new(),
        }
    }

    #[test]
    fn resolve_matches_model_id_before_falling_back_to_route_id() {
        let cache = RouteCache {
            models: vec![route("route-a", "logical-a"), route("route-b", "route-a")],
        };

        assert_eq!(
            cache.resolve("logical-a").map(|route| route.id.as_str()),
            Some("route-a")
        );
        assert_eq!(
            cache.resolve("route-b").map(|route| route.id.as_str()),
            Some("route-b")
        );
        // A model_id hit wins over another Route's id.
        assert_eq!(
            cache.resolve("route-a").map(|route| route.id.as_str()),
            Some("route-b")
        );
        assert!(cache.resolve("missing").is_none());
    }
}
