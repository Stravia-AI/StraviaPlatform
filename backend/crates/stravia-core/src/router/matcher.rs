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
