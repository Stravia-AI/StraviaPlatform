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
        continuation_key_for(&self.api_key_id)
    }
}

/// API key id 对应的续接键（`api-key:<id>`）。
///
/// 内容存储 scope、Turn Chain 与 History Marker 都用它作为 Principal 的唯一身份串。
/// 只持有 id 字符串的调用方（如 Observation 的 `interaction_observations.principal`，
/// 可能是不满足 `Principal::new` 断言的 `anonymous`）通过本函数取同一个键，
/// 不再各自拼接前缀。
pub fn continuation_key_for(api_key_id: &str) -> String {
    format!("api-key:{api_key_id}")
}
