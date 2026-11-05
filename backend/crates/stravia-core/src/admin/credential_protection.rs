use super::credential_masking::mask_matches;
use super::{
    AdminService, CredentialDiscoveryError, CredentialDiscoveryField, CredentialDiscoveryHitDetail,
    CredentialDiscoveryPage, CredentialDiscoveryQuery, CredentialDiscoveryReuseQuery,
    CredentialDiscoveryReuses, CredentialVisibility,
};
use crate::interaction_observation::CredentialDiscoveryBody;
use std::collections::BTreeSet;
use stravia_credential_protection::policy::{self, DISABLED_RULES_KEY};
use stravia_credential_protection::{
    CredentialMatch, CredentialRuleCatalog, CustomRule, CustomRuleError, CustomRuleInput,
    DisabledRulesError,
};

impl AdminService {
    /// 返回当前实例内置的本地规则及其有效条件与启用状态，不执行检测或联网验证。
    pub async fn credential_protection_rules(&self) -> anyhow::Result<CredentialRuleCatalog> {
        let mut catalog = stravia_credential_protection::rule_catalog().await?;
        let disabled = self.disabled_credential_rules().await?;
        for rule in &mut catalog.rules {
            rule.enabled = !disabled.contains(&rule.id);
        }
        Ok(catalog)
    }

    /// 整体替换被禁用的内置规则集合，返回去重排序后的结果。
    ///
    /// 禁用只停止新的检出与出站替换：已生成的映射的占位符仍可还原，
    /// 其他启用规则仍可检出同一个秘密。自定义规则用各自的 `enabled`，不在此集合内。
    pub async fn set_credential_disabled_rules(
        &self,
        rule_ids: Vec<String>,
    ) -> Result<Vec<String>, DisabledRulesError> {
        let catalog = stravia_credential_protection::rule_catalog()
            .await
            .map_err(|_| DisabledRulesError::Storage)?;
        let known: BTreeSet<&str> = catalog.rules.iter().map(|rule| rule.id.as_str()).collect();
        if let Some(unknown) = rule_ids.iter().find(|id| !known.contains(id.as_str())) {
            return Err(DisabledRulesError::Unknown(unknown.clone()));
        }
        let rules: BTreeSet<String> = rule_ids.into_iter().collect();
        self.gw
            .storage
            .settings()
            .set(DISABLED_RULES_KEY, &policy::encode_disabled_rules(&rules))
            .await
            .map_err(|_| DisabledRulesError::Storage)?;
        Ok(rules.into_iter().collect())
    }

    /// 运行时忽略目录中已不存在的规则 ID（例如内置规则集升级后被移除的规则）。
    async fn disabled_credential_rules(&self) -> anyhow::Result<BTreeSet<String>> {
        let value = self.gw.storage.settings().get(DISABLED_RULES_KEY).await?;
        Ok(policy::parse_disabled_rules(value.as_deref())?)
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

    /// 查询一行一次新建映射的命中记录；沿用 Observation 的保留与过期语义。
    pub async fn credential_protection_discoveries(
        &self,
        query: CredentialDiscoveryQuery,
    ) -> anyhow::Result<CredentialDiscoveryPage> {
        self.gw.observation.credential_discoveries(query).await
    }

    /// 读取命中字段的正文与匹配；正文只存在于响应，不写入日志。
    /// 默认把匹配区间替换为定长占位符并重算位置，只有 `Revealed` 返回明文与原始偏移。
    pub async fn credential_protection_discovery_hit(
        &self,
        id: &str,
        index: &str,
        visibility: CredentialVisibility,
    ) -> anyhow::Result<CredentialDiscoveryHitDetail> {
        let (sequence, discovery_index) = parse_discovery_id(id)?;
        let hit_index = parse_hit_index(index)?;
        let record = self
            .gw
            .observation
            .credential_discovery_record(sequence, discovery_index)
            .await?
            .ok_or(CredentialDiscoveryError::NotFound)?;
        let hit = record
            .hits
            .get(hit_index)
            .cloned()
            .ok_or(CredentialDiscoveryError::NotFound)?;
        let context = self
            .gw
            .observation
            .credential_discovery_hit_context(&record, &hit)
            .await?;
        let field = CredentialDiscoveryField {
            source_type: hit.source_type.clone(),
            tool_id: hit.tool_id.clone(),
            tool_name: context.tool_name,
        };
        let value = match context.body {
            CredentialDiscoveryBody::Missing => {
                return Ok(CredentialDiscoveryHitDetail::Unavailable {
                    field,
                    reason: "missing".into(),
                });
            }
            CredentialDiscoveryBody::NotRetained => {
                return Ok(CredentialDiscoveryHitDetail::Unavailable {
                    field,
                    reason: "not_retained".into(),
                });
            }
            CredentialDiscoveryBody::Retained(value) => value,
        };
        let text = discovery_text(&value);
        let secret = match record
            .discovery
            .marker
            .as_deref()
            .filter(|marker| !marker.is_empty())
        {
            Some(marker) => self.gw.redaction.mappings.plaintext(marker).await?,
            None => None,
        };
        let hit_basis = if secret.is_some() { "secret" } else { "rule" };
        let mut matches = self
            .gw
            .redaction
            .locate_text(text.clone(), secret.as_deref())
            .await?;
        if secret.is_none() {
            for found in &mut matches {
                found.hit = found.rule_id == hit.rule_id;
            }
        }
        let (text, matches, masked) = match visibility {
            CredentialVisibility::Revealed => (text, matches, false),
            CredentialVisibility::Masked => {
                let (masked_text, masked_matches) = mask_matches(&text, matches);
                (masked_text, masked_matches, true)
            }
        };
        Ok(CredentialDiscoveryHitDetail::Available {
            field,
            text,
            hit_basis: hit_basis.into(),
            masked,
            matches,
        })
    }

    /// 读取写入时登记的复用位置：只按占位符做索引查找，不同主体的行不返回。
    /// 没有占位符的映射不可关联复用，仍按同一契约返回空页与零总数。
    pub async fn credential_protection_discovery_reuses(
        &self,
        id: &str,
        query: CredentialDiscoveryReuseQuery,
    ) -> anyhow::Result<CredentialDiscoveryReuses> {
        let (sequence, discovery_index) = parse_discovery_id(id)?;
        let record = self
            .gw
            .observation
            .credential_discovery_record(sequence, discovery_index)
            .await?
            .ok_or(CredentialDiscoveryError::NotFound)?;
        let marker = record
            .discovery
            .marker
            .as_deref()
            .filter(|marker| !marker.is_empty())
            .unwrap_or("");
        self.gw
            .observation
            .credential_discovery_reuses(&record, marker, query)
            .await
    }

    pub async fn credential_custom_rules(&self) -> Result<Vec<CustomRule>, CustomRuleError> {
        self.gw.redaction.custom_rules.list().await
    }

    /// 新规则只影响之后的请求；已生成的映射按空闲保留期继续有效。
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

    /// 删除规则不清除已生成的映射：占位符仍可还原直到映射过期。
    /// 但只由该规则检出的映射不再参与出站替换，与停用规则一致。
    pub async fn delete_credential_custom_rule(&self, id: &str) -> Result<(), CustomRuleError> {
        self.gw.redaction.custom_rules.delete(id).await
    }
}

fn parse_discovery_id(id: &str) -> Result<(i64, usize), CredentialDiscoveryError> {
    let (sequence, index) = id
        .split_once('.')
        .ok_or(CredentialDiscoveryError::InvalidReference)?;
    if id.len() > 64
        || sequence.is_empty()
        || index.is_empty()
        || !sequence.bytes().all(|byte| byte.is_ascii_digit())
        || !index.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(CredentialDiscoveryError::InvalidReference);
    }
    Ok((
        sequence
            .parse()
            .map_err(|_| CredentialDiscoveryError::InvalidReference)?,
        index
            .parse()
            .map_err(|_| CredentialDiscoveryError::InvalidReference)?,
    ))
}

fn parse_hit_index(index: &str) -> Result<usize, CredentialDiscoveryError> {
    if index.is_empty() || index.len() > 20 || !index.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(CredentialDiscoveryError::InvalidReference);
    }
    index
        .parse()
        .map_err(|_| CredentialDiscoveryError::InvalidReference)
}

fn discovery_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(items) if items.iter().all(text_block) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        other => serde_json::to_string_pretty(other).expect("JSON values always serialize"),
    }
}

fn text_block(value: &serde_json::Value) -> bool {
    value.get("type").and_then(serde_json::Value::as_str) == Some("text")
        && value.get("text").is_some_and(serde_json::Value::is_string)
}
