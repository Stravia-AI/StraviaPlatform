use super::AdminService;
use crate::platform_llm::{
    self, ConversationTitleInput, ConversationTitleResult, PlatformLlmConfigView, PlatformLlmError,
    UpdatePlatformLlmConfig,
};

impl AdminService {
    pub async fn get_platform_llm_config(&self) -> Result<PlatformLlmConfigView, PlatformLlmError> {
        let config = platform_llm::config::load(&self.gw).await?;
        platform_llm::config::view(&self.gw, config).await
    }

    pub async fn update_platform_llm_config(
        &self,
        update: UpdatePlatformLlmConfig,
    ) -> Result<PlatformLlmConfigView, PlatformLlmError> {
        platform_llm::config::save(&self.gw, update).await
    }

    pub async fn generate_conversation_title(
        &self,
        input: ConversationTitleInput,
    ) -> Result<ConversationTitleResult, PlatformLlmError> {
        platform_llm::service::generate_conversation_title(&self.gw, input).await
    }
}
