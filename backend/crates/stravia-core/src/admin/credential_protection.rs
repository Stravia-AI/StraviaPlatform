use super::{AdminService, CredentialDiscoveryPage, CredentialDiscoveryQuery};
use stravia_credential_protection::{
    CredentialMatch, CredentialRuleCatalog, CustomRule, CustomRuleError, CustomRuleInput,
};

impl AdminService {
    /// 返回当前实例内置的本地规则及其有效条件，不执行检测或联网验证。
    pub async fn credential_protection_rules(&self) -> anyhow::Result<CredentialRuleCatalog> {
        Ok(stravia_credential_protection::rule_catalog().await?)
    }

    /// 只检测本次提交的文本；不查询秘密字典，不持久化输入或改变保护状态。
    /// 规则集与实际保护一致：内置规则加已启用的自定义规则。
    /// 位置偏移以 UTF-16 code unit 表示，区间右端不包含在匹配内。
    pub async fn test_credential_protection(
        &self,
        text: String,
    ) -> anyhow::Result<Vec<CredentialMatch>> {
        Ok(self.gw.redaction.test_text(text).await?)
    }

    /// 查询按客户端交互归组的新增凭据摘要，沿用 Observation 的保留与缺失语义。
    pub async fn credential_protection_discoveries(
        &self,
        query: CredentialDiscoveryQuery,
    ) -> anyhow::Result<CredentialDiscoveryPage> {
        self.gw.observation.credential_discoveries(query).await
    }

    pub async fn credential_custom_rules(&self) -> Result<Vec<CustomRule>, CustomRuleError> {
        self.gw.redaction.custom_rules.list().await
    }

    /// 新规则只影响之后的请求；已生成的映射按原保留期继续有效。
    pub async fn create_credential_custom_rule(
        &self,
        input: CustomRuleInput,
    ) -> Result<CustomRule, CustomRuleError> {
        self.gw.redaction.custom_rules.create(input).await
    }

    pub async fn update_credential_custom_rule(
        &self,
        id: &str,
        input: CustomRuleInput,
    ) -> Result<CustomRule, CustomRuleError> {
        self.gw.redaction.custom_rules.update(id, input).await
    }

    /// 删除规则不清除已生成的映射；这些映射在保留期内仍参与替换与还原。
    pub async fn delete_credential_custom_rule(&self, id: &str) -> Result<(), CustomRuleError> {
        self.gw.redaction.custom_rules.delete(id).await
    }
}
