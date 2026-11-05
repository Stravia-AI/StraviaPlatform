//! 规则开关：禁用内置规则、停用或删除自定义规则对检测、出站替换与目录的影响。
use std::sync::Arc;

use serde_json::json;
use stravia_credential_protection::store::Mapping;
use stravia_credential_protection::{CustomRuleInput, DisabledRulesError, SETTING_KEY};
use stravia_runtime_contract::Principal;
use stravia_runtime_contract::protocol::ir::AiRequest;

use crate::config::GatewayConfig;
use crate::gateway::Gateway;

const SECRET: &str = "ghp_9Er8nQ3wM0tY5bS7uL4oG6xI2kC1dZaVfJpH";

async fn gateway() -> (tempfile::TempDir, Gateway) {
    let directory = tempfile::tempdir().unwrap();
    let gateway = Gateway::from_storage(
        GatewayConfig {
            data_dir: directory.path().to_path_buf(),
            ..Default::default()
        },
        Arc::new(crate::storage::MemoryStorage::new(
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )),
    )
    .await
    .unwrap();
    gateway
        .admin()
        .set_setting(SETTING_KEY, "true")
        .await
        .unwrap();
    (directory, gateway)
}

fn custom(value: serde_json::Value) -> CustomRuleInput {
    serde_json::from_value(value).unwrap()
}

fn literal(text: &str, enabled: bool) -> CustomRuleInput {
    custom(json!({
        "name": "Literal",
        "enabled": enabled,
        "spec": { "mode": "simple", "text": text },
    }))
}

/// 返回发往上游的文本与 `protect` 交给还原环节的全部映射。
async fn protect(gateway: &Gateway, owner: &Principal, text: &str) -> (String, Vec<Mapping>) {
    let mut request = AiRequest::new("model", Vec::new());
    request.instructions = Some(text.into());
    let mut request = Arc::new(request);
    let mappings = gateway
        .redaction
        .protect(owner, &mut request, None)
        .await
        .unwrap();
    (request.instructions.clone().unwrap(), mappings)
}

#[tokio::test]
async fn a_disabled_builtin_rule_stops_detection_and_replacement_but_keeps_restoration() {
    let (_directory, gateway) = gateway().await;
    let admin = gateway.admin();
    let owner = Principal::new("disabled-rule-owner");
    let text = format!("token {SECRET}");

    let (sent, mappings) = protect(&gateway, &owner, &text).await;
    assert!(!sent.contains(SECRET));
    let reference = mappings[0].reference.clone();
    assert!(sent.contains(&reference));
    assert_eq!(mappings[0].rule_ids.as_slice(), ["github-pat"]);

    let saved = admin
        .set_credential_disabled_rules(vec!["github-pat".into(), "github-pat".into()])
        .await
        .unwrap();
    assert_eq!(saved, ["github-pat"]);

    let catalog = admin.credential_protection_rules().await.unwrap();
    assert!(
        !catalog
            .rules
            .iter()
            .find(|rule| rule.id == "github-pat")
            .unwrap()
            .enabled
    );
    assert!(catalog.rules.iter().filter(|rule| rule.enabled).count() > 1);
    let tested = admin
        .test_credential_protection(text.clone())
        .await
        .unwrap();
    assert!(tested.iter().all(|found| found.rule_id != "github-pat"));

    // 新的出站请求不再替换；映射仍交给还原环节，已经发出去的占位符照常还原。
    let (sent, mappings) = protect(&gateway, &owner, &text).await;
    assert!(
        sent.contains(SECRET),
        "disabled rule: secret goes out as is"
    );
    assert_eq!(mappings.len(), 1);
    assert_eq!(mappings[0].reference, reference);
    let (sent, mappings) = protect(&gateway, &owner, &format!("earlier {reference}")).await;
    assert!(sent.contains(&reference));
    assert_eq!(mappings[0].secret, SECRET);

    admin
        .set_credential_disabled_rules(Vec::new())
        .await
        .unwrap();
    let (sent, _) = protect(&gateway, &owner, &text).await;
    assert!(!sent.contains(SECRET));
    assert!(
        sent.contains(&reference),
        "re-enabling reuses the same mapping"
    );
}

#[tokio::test]
async fn only_known_builtin_rules_can_be_disabled_and_the_setting_is_not_writable_directly() {
    let (_directory, gateway) = gateway().await;
    let admin = gateway.admin();
    for rejected in ["no-such-rule", "custom.anything"] {
        assert_eq!(
            admin
                .set_credential_disabled_rules(vec!["github-pat".into(), rejected.into()])
                .await,
            Err(DisabledRulesError::Unknown(rejected.into()))
        );
    }
    assert!(
        admin
            .set_setting("reversible_redaction_disabled_rules", "[]")
            .await
            .is_err()
    );

    gateway
        .storage
        .settings()
        .set("reversible_redaction_disabled_rules", "not json")
        .await
        .unwrap();
    let mut request = Arc::new(AiRequest::new("model", Vec::new()));
    assert!(
        gateway
            .redaction
            .protect(&Principal::new("malformed"), &mut request, None)
            .await
            .is_err(),
        "an unreadable rule switch fails closed instead of guessing"
    );
    assert!(admin.credential_protection_rules().await.is_err());
}

#[tokio::test]
async fn an_enabled_rule_still_replaces_a_secret_first_found_by_a_now_disabled_rule() {
    let (_directory, gateway) = gateway().await;
    let admin = gateway.admin();
    let owner = Principal::new("redetected-owner");
    let text = format!("token {SECRET}");
    let (_, mappings) = protect(&gateway, &owner, &text).await;
    let reference = mappings[0].reference.clone();

    admin
        .set_credential_disabled_rules(vec!["github-pat".into()])
        .await
        .unwrap();
    let (sent, _) = protect(&gateway, &owner, &text).await;
    assert!(sent.contains(SECRET));

    let rule = admin
        .create_credential_custom_rule(literal(SECRET, true))
        .await
        .unwrap();
    let (sent, mappings) = protect(&gateway, &owner, &text).await;
    assert!(!sent.contains(SECRET));
    assert!(sent.contains(&reference), "reuses the existing mapping");
    assert_eq!(mappings.len(), 1);

    admin
        .update_credential_custom_rule(&rule.id, literal(SECRET, false))
        .await
        .unwrap();
    let (sent, _) = protect(&gateway, &owner, &text).await;
    assert!(
        sent.contains(SECRET),
        "no enabled rule left for this secret"
    );
}

#[tokio::test]
async fn stopping_or_deleting_a_custom_rule_stops_replacing_what_only_it_found() {
    let (_directory, gateway) = gateway().await;
    let admin = gateway.admin();
    let owner = Principal::new("custom-cascade-owner");
    let rule = admin
        .create_credential_custom_rule(literal("A B C", true))
        .await
        .unwrap();
    let text = "pass A B C, rest";

    let (sent, mappings) = protect(&gateway, &owner, text).await;
    assert!(!sent.contains("A B C"));
    let reference = mappings[0].reference.clone();
    // 弱秘密只在词边界处替换：紧贴单词字符的出现不是同一个词。
    let (sent, _) = protect(&gateway, &owner, "xA B Cx").await;
    assert!(sent.contains("xA B Cx"));

    admin
        .update_credential_custom_rule(&rule.id, literal("A B C", false))
        .await
        .unwrap();
    let (sent, mappings) = protect(&gateway, &owner, text).await;
    assert!(sent.contains("A B C"), "stopped rule: not replaced");
    assert!(
        mappings
            .iter()
            .any(|mapping| mapping.reference == reference)
    );

    admin
        .update_credential_custom_rule(&rule.id, literal("A B C", true))
        .await
        .unwrap();
    let (sent, _) = protect(&gateway, &owner, text).await;
    assert!(
        sent.contains(&reference),
        "restarting the rule resumes replacement"
    );

    admin.delete_credential_custom_rule(&rule.id).await.unwrap();
    let (sent, mappings) = protect(&gateway, &owner, text).await;
    assert!(sent.contains("A B C"), "deleted rule: not replaced");
    assert!(
        mappings
            .iter()
            .any(|mapping| mapping.reference == reference)
    );
}
