#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Principal {
    api_key_id: String,
}

impl Principal {
    pub fn new(api_key_id: impl Into<String>) -> Self {
        let api_key_id = api_key_id.into();
        assert!(
            !api_key_id.is_empty() && api_key_id != "anonymous",
            "Principal requires an authenticated API key identity"
        );
        Self { api_key_id }
    }

    pub fn api_key_id(&self) -> &str {
        &self.api_key_id
    }

    pub fn continuation_key(&self) -> String {
        format!("api-key:{}", self.api_key_id)
    }
}
