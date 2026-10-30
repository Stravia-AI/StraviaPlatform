use super::{
    AdminService, CredentialDiscoveryError, CredentialDiscoveryField, CredentialDiscoveryHitDetail,
    CredentialDiscoveryPage, CredentialDiscoveryQuery, CredentialDiscoveryReuses,
};
use crate::interaction_observation::CredentialDiscoveryBody;
use stravia_credential_protection::{
    CredentialMatch, CredentialRuleCatalog, CustomRule, CustomRuleError, CustomRuleInput,
};

/// 命中正文的字节上限；超出时按字符边界截断并标记 `truncated`。
const MAX_DISCOVERY_TEXT_BYTES: usize = 1024 * 1024;

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

    /// 查询一行一次新建映射的命中记录；沿用 Observation 的保留与过期语义。
    pub async fn credential_protection_discoveries(
        &self,
        query: CredentialDiscoveryQuery,
    ) -> anyhow::Result<CredentialDiscoveryPage> {
        self.gw.observation.credential_discoveries(query).await
    }

    /// 读取命中字段的正文与匹配；正文只存在于响应，不写入日志。
    pub async fn credential_protection_discovery_hit(
        &self,
        id: &str,
        index: &str,
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
        let (text, truncated) = discovery_text(&value);
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
        Ok(CredentialDiscoveryHitDetail::Available {
            field,
            text,
            truncated,
            hit_basis: hit_basis.into(),
            matches,
        })
    }

    /// 读取写入时登记的复用位置：只按占位符做索引查找，不同主体的行不返回。
    pub async fn credential_protection_discovery_reuses(
        &self,
        id: &str,
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
            .filter(|marker| !marker.is_empty());
        match marker {
            Some(marker) => {
                self.gw
                    .observation
                    .credential_discovery_reuses(&record, marker)
                    .await
            }
            None => Ok(CredentialDiscoveryReuses {
                linkable: false,
                items: Vec::new(),
                truncated: false,
            }),
        }
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

fn discovery_text(value: &serde_json::Value) -> (String, bool) {
    let text = match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(items) if items.iter().all(text_block) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        other => serde_json::to_string_pretty(other).expect("JSON values always serialize"),
    };
    if text.len() <= MAX_DISCOVERY_TEXT_BYTES {
        return (text, false);
    }
    let mut end = MAX_DISCOVERY_TEXT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut text = text;
    text.truncate(end);
    (text, true)
}

fn text_block(value: &serde_json::Value) -> bool {
    value.get("type").and_then(serde_json::Value::as_str) == Some("text")
        && value.get("text").is_some_and(serde_json::Value::is_string)
}
