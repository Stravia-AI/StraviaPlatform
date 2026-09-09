use super::{AdminService, CredentialDiscoveryPage, CredentialDiscoveryQuery};
use stravia_credential_protection::{CredentialMatch, CredentialRuleCatalog};

impl AdminService {
    /// 返回当前实例内置的本地规则及其有效条件，不执行检测或联网验证。
    pub async fn credential_protection_rules(&self) -> anyhow::Result<CredentialRuleCatalog> {
        Ok(stravia_credential_protection::rule_catalog().await?)
    }

    /// 只检测本次提交的文本；不查询秘密字典，不持久化输入或改变保护状态。
    /// 位置偏移以 UTF-16 code unit 表示，区间右端不包含在匹配内。
    pub async fn test_credential_protection(
        &self,
        text: String,
    ) -> anyhow::Result<Vec<CredentialMatch>> {
        Ok(stravia_credential_protection::test_text(text).await?)
    }

    /// 查询按客户端交互归组的新增凭据摘要，沿用 Observation 的保留与缺失语义。
    pub async fn credential_protection_discoveries(
        &self,
        query: CredentialDiscoveryQuery,
    ) -> anyhow::Result<CredentialDiscoveryPage> {
        self.gw.observation.credential_discoveries(query).await
    }
}
