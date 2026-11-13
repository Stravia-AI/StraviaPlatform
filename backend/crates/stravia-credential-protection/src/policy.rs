//! 凭据保护的运行策略：总开关、被禁用的内置规则，以及由此得出「哪些已记住的凭据仍参与出站替换」。
use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;
use std::time::Duration;

use super::custom::ID_PREFIX;
use super::store::{Mapping, expiry_cutoff};
use stravia_runtime_contract::redaction::RedactionError;

/// 保存被禁用内置规则 ID 的设置键，值为 JSON 字符串数组；缺省表示全部启用。
/// 自定义规则不在其内，它们由各自的 `enabled` 标志控制。
pub const DISABLED_RULES_KEY: &str = "reversible_redaction_disabled_rules";

/// 禁用规则集合的写入错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DisabledRulesError {
    /// 只有内置规则可以禁用；自定义规则用各自的 `enabled`。
    #[error("Credential rule `{0}` is not a built-in rule")]
    Unknown(String),
    #[error("Credential rule settings are unavailable")]
    Storage,
}

/// 每个请求读取一次的运行策略。
#[derive(Clone)]
pub struct RedactionPolicy {
    pub enabled: bool,
    pub disabled_rules: Arc<BTreeSet<String>>,
    /// 映射的保留时长 N（`log_retention_days`）：规则超过它没有再检出的映射停止出站替换。
    pub retention: Duration,
}

impl RedactionPolicy {
    /// 本请求的替换与遮罩范围。`custom_enabled` 是当前存在且已启用的自定义规则 ID；
    /// 过期边界在此刻固定，同一请求内的判断一致。
    pub fn rule_scope(&self, custom_enabled: impl IntoIterator<Item = String>) -> RuleScope {
        RuleScope::new(Arc::clone(&self.disabled_rules), custom_enabled)
            .expiring_after(self.retention)
    }
}

/// 解析设置值；无法解释时失败关闭，由宿主按保护失败处理，不退回「全部启用」以外的任何猜测。
pub fn parse_disabled_rules(value: Option<&str>) -> Result<BTreeSet<String>, RedactionError> {
    match value {
        None => Ok(BTreeSet::new()),
        Some(value) => {
            serde_json::from_str::<BTreeSet<String>>(value).map_err(|_| RedactionError::Storage)
        }
    }
}

/// 与 [`parse_disabled_rules`] 对称：有序、去重，写入同一设置键。
pub fn encode_disabled_rules(rules: &BTreeSet<String>) -> String {
    serde_json::to_string(rules).expect("string set serialization")
}

/// 当前规则启用状态的来源；观测遮罩按需读取，不必持有整个保护能力。
#[async_trait::async_trait]
pub trait RuleScopeSource: Send + Sync {
    async fn rule_scope(&self) -> Result<RuleScope, RedactionError>;
}

/// 某一时刻哪些映射仍参与出站替换与管理端遮罩。
///
/// 内置规则：不在禁用集即启用。自定义规则：必须存在且已启用，
/// 因此被停用或删除的自定义规则同样视为未启用。
/// 另外，规则超过保留期没有再检出的映射视为过期（见 [`Self::expiring_after`]）。
#[derive(Default)]
pub struct RuleScope {
    disabled: Arc<BTreeSet<String>>,
    custom_enabled: HashSet<String>,
    /// 检出时间不晚于它的映射已过期；`None` 表示不按时间过滤。
    expiry_cutoff: Option<i64>,
}

impl RuleScope {
    /// `custom_enabled` 是当前存在且已启用的自定义规则 ID。不按时间过滤，需要时接 [`Self::expiring_after`]。
    pub fn new(
        disabled: Arc<BTreeSet<String>>,
        custom_enabled: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            disabled,
            custom_enabled: custom_enabled.into_iter().collect(),
            expiry_cutoff: None,
        }
    }

    /// 以当前时刻与保留期 `retention` 固定过期边界，与回收共用 [`expiry_cutoff`]。
    pub fn expiring_after(mut self, retention: Duration) -> Self {
        self.expiry_cutoff = Some(expiry_cutoff(
            chrono::Utc::now().timestamp_millis(),
            retention,
        ));
        self
    }

    pub fn rule_enabled(&self, rule_id: &str) -> bool {
        if rule_id.starts_with(ID_PREFIX) {
            self.custom_enabled.contains(rule_id)
        } else {
            !self.disabled.contains(rule_id)
        }
    }

    /// 规则超过保留期没有再检出该秘密。过期映射只用于还原旧占位符。
    pub fn expired(&self, mapping: &Mapping) -> bool {
        self.expiry_cutoff
            .is_some_and(|cutoff| mapping.last_detected_at <= cutoff)
    }

    /// 映射是否仍参与出站替换与管理端遮罩：未过期，且创建它的规则至少有一条仍启用。
    /// 另一条启用的规则本来就会重新检出同一个秘密，所以用「至少一条」而不是「没有被禁用的」。
    /// 没有归因的映射（归因记录出现之前创建）无法判断规则，保持启用，但仍受过期约束。
    pub fn allows(&self, mapping: &Mapping) -> bool {
        let rule_ids = mapping.rule_ids.as_slice();
        !self.expired(mapping)
            && (rule_ids.is_empty() || rule_ids.iter().any(|rule_id| self.rule_enabled(rule_id)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::RuleIds;

    fn scope(disabled: &[&str], custom: &[&str]) -> RuleScope {
        RuleScope::new(
            Arc::new(disabled.iter().map(|id| (*id).to_owned()).collect()),
            custom.iter().map(|id| (*id).to_owned()),
        )
    }

    fn mapping(rule_ids: &[&str]) -> Mapping {
        Mapping {
            reference: "ref".into(),
            secret: "secret".into(),
            rule_ids: RuleIds::from(
                rule_ids
                    .iter()
                    .map(|id| (*id).to_owned())
                    .collect::<Vec<_>>(),
            ),
            last_detected_at: 0,
        }
    }

    #[test]
    fn a_mapping_stays_active_while_any_attributed_rule_is_enabled() {
        let scope = scope(&["github-pat"], &["custom.live"]);
        assert!(!scope.allows(&mapping(&["github-pat"])));
        assert!(scope.allows(&mapping(&["github-pat", "generic-api-key"])));
        assert!(scope.allows(&mapping(&["github-pat", "custom.live"])));
    }

    #[test]
    fn custom_rules_count_only_while_they_exist_and_are_enabled() {
        let scope = scope(&[], &["custom.live"]);
        assert!(scope.allows(&mapping(&["custom.live"])));
        // 已停用或已删除的自定义规则不在启用集合里。
        assert!(!scope.allows(&mapping(&["custom.gone"])));
    }

    #[test]
    fn unattributed_mappings_cannot_be_judged_and_stay_active() {
        assert!(scope(&["github-pat"], &[]).allows(&mapping(&[])));
    }

    fn detected(rule_ids: &[&str], last_detected_at: i64) -> Mapping {
        Mapping {
            last_detected_at,
            ..mapping(rule_ids)
        }
    }

    #[test]
    fn a_mapping_stops_replacing_once_no_rule_has_detected_it_within_the_retention_window() {
        let cutoff = expiry_cutoff(10 * 86_400_000, Duration::from_secs(7 * 86_400));
        let mut scope = scope(&[], &[]);
        scope.expiry_cutoff = Some(cutoff);
        assert!(scope.allows(&detected(&["generic-api-key"], cutoff + 1)));
        let stale = detected(&["generic-api-key"], cutoff);
        assert!(scope.expired(&stale));
        assert!(!scope.allows(&stale));
        // 没有归因的存量映射同样受过期约束：这是旧映射误杀的出口。
        assert!(!scope.allows(&detected(&[], cutoff)));
        assert!(scope.allows(&detected(&[], cutoff + 1)));
    }

    #[test]
    fn disabled_rules_round_trip_and_reject_malformed_values() {
        let rules = BTreeSet::from(["b".to_owned(), "a".to_owned()]);
        let encoded = encode_disabled_rules(&rules);
        assert_eq!(encoded, r#"["a","b"]"#);
        assert_eq!(parse_disabled_rules(Some(&encoded)).unwrap(), rules);
        assert!(parse_disabled_rules(None).unwrap().is_empty());
        assert!(parse_disabled_rules(Some("not json")).is_err());
        assert!(parse_disabled_rules(Some(r#"{"a":1}"#)).is_err());
    }
}
