use super::AdminService;
use crate::media_generation::{
    self, EligibleGenerationRoute, GenerationError, MediaGenerationConfig,
    MediaGenerationConfigView,
};

impl AdminService {
    pub async fn get_media_generation_config(
        &self,
    ) -> Result<MediaGenerationConfigView, GenerationError> {
        let config = media_generation::config::load(&self.gw).await?;
        media_generation::config::view(&self.gw, config).await
    }

    pub async fn update_media_generation_config(
        &self,
        config: MediaGenerationConfig,
    ) -> Result<MediaGenerationConfigView, GenerationError> {
        media_generation::config::save(&self.gw, config).await
    }

    pub async fn list_eligible_media_generation_routes(
        &self,
    ) -> Result<Vec<EligibleGenerationRoute>, GenerationError> {
        media_generation::config::eligible_routes(&self.gw).await
    }
}
