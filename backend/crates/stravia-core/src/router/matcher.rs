use crate::db::models::Model;
use crate::storage::{ModelBackendStore, ModelSnapshotStore};

pub struct ModelCache {
    pub models: Vec<Model>,
}

impl ModelCache {
    pub async fn load(
        store: &dyn ModelSnapshotStore,
        backend_store: Option<&dyn ModelBackendStore>,
    ) -> anyhow::Result<Self> {
        let mut models = store.load_active_snapshot().await?;
        for model in &mut models {
            if let Some(backend_store) = backend_store {
                model.targets = backend_store.list_backends_by_model(&model.id).await?;
            }
            model.refresh_supported_thinking_levels();
        }
        Ok(Self { models })
    }

    pub async fn reload(
        &mut self,
        store: &dyn ModelSnapshotStore,
        backend_store: Option<&dyn ModelBackendStore>,
    ) -> anyhow::Result<()> {
        *self = Self::load(store, backend_store).await?;
        Ok(())
    }
}

pub fn match_model<'a>(models: &'a [Model], model: &str) -> Option<&'a Model> {
    models.iter().find(|m| m.name == model)
}
