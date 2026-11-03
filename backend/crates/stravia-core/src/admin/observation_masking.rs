//! 管理端读取 Observation 时的凭据脱敏。
//!
//! 观测数据按写入时的原样保存（还原与命中复检需要明文），脱敏只在响应边界做一次：
//! 先用一次索引查询确认实例内存在未过期映射，再按交互 ID 批量解析所属 API Key 主体，
//! 只为主体加载当前有效的可逆映射，并用 `***` 替换正文中的明文，`<!--sr:...-->` 占位符原样保留。
//! 没有有效映射时直接返回原响应：不查主体、不遍历载荷、不克隆。任何查询失败都向上返回，
//! 不降级为未脱敏输出。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::sync::Arc;

use futures::StreamExt;

use stravia_credential_protection::store::MappingStore;
use stravia_runtime_contract::Principal;

use crate::interaction_observation::{
    ClientErrorDetail, ClientErrorPage, ClientErrorSummary, FailureDiagnostic, ForestPage,
    InteractionDetail, InteractionEventsPage, InteractionObservation, InteractionSummary,
    LiveContentBlock, ObservationContent, ObservationEvent, ObservationStream, ObservationUpdate,
    ProtectedSecrets, RootChangesPage, RunDetail, UpstreamErrorDetail, UpstreamErrorPage,
    UpstreamErrorSummary,
};

use super::AdminService;

/// 未认证请求的主体字符串；它无法构成认证身份，也不会存在映射。
const ANONYMOUS_PRINCIPAL: &str = "anonymous";

/// 实时订阅中刷新有效映射的间隔；期间新建的映射要到下一次刷新才参与替换。
const LIVE_MAPPING_REFRESH: std::time::Duration = std::time::Duration::from_secs(1);

fn maskable_principal(principal: &str) -> Option<&str> {
    (!principal.is_empty() && principal != ANONYMOUS_PRINCIPAL).then_some(principal)
}

/// 只含 ID、用量、时序或状态的事件不携带客户端正文，读取时整条跳过。
/// 未知种类按内容事件处理，避免新增事件漏脱敏。
fn carries_content(kind: &str) -> bool {
    !matches!(
        kind,
        "run_admitted"
            | "run_finished"
            | "run_state_changed"
            | "observation_gap"
            | "credential_mappings_created"
            | "credential_mappings_reused"
            | "compaction_operation"
            | "native_compaction_associated"
            | "retained_tail_associated"
            | "generation_associated"
            | "model_turn_started"
            | "model_turn_finished"
            | "target_attempt_started"
            | "model_thinking_finished"
            | "client_output_committed"
            | "delivery_finished"
    )
}

/// 一次响应内按交互解析出的主体与其当前有效映射。
#[derive(Default)]
pub(super) struct CredentialMasker {
    interactions: BTreeMap<String, String>,
    principals: BTreeMap<String, ProtectedSecrets>,
}

impl CredentialMasker {
    /// 先用一次索引查询确认存在未过期映射，再解析主体并加载映射。
    ///
    /// `lookup` 只在存在未过期映射后才被轮询；没有可脱敏内容时它不会发出任何查询。
    /// 每个主体只加载一次当前有效映射；空映射集合的主体整条跳过，
    /// 其交互的载荷与摘要不会被遍历。
    pub(super) async fn load<F>(
        mappings: &dyn MappingStore,
        ids: impl IntoIterator<Item = String>,
        lookup: F,
    ) -> anyhow::Result<Self>
    where
        F: Future<Output = anyhow::Result<HashMap<String, String>>>,
    {
        let ids: BTreeSet<String> = ids.into_iter().collect();
        if ids.is_empty() || !mappings.any_active().await? {
            return Ok(Self::default());
        }
        let principals_by_id = lookup.await?;
        let mut interactions = BTreeMap::new();
        for id in &ids {
            let principal = principals_by_id
                .get(id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("observation principal lookup is incomplete"))?;
            interactions.insert(id.clone(), principal);
        }
        let principals: BTreeSet<&str> = interactions
            .values()
            .filter_map(|principal| maskable_principal(principal))
            .collect();
        let mut loaded = BTreeMap::new();
        for principal in principals {
            let active = mappings.active(&Principal::new(principal)).await?;
            if active.is_empty() {
                continue;
            }
            let secrets = ProtectedSecrets::default();
            secrets.register(active.iter().map(|mapping| mapping.secret.as_str()));
            loaded.insert(principal.to_owned(), secrets);
        }
        Ok(Self {
            interactions,
            principals: loaded,
        })
    }

    fn secrets(&self, interaction: Option<&str>) -> Option<&ProtectedSecrets> {
        let interaction = interaction?;
        let principal = self.interactions.get(interaction)?;
        self.principals.get(principal)
    }

    /// 内容端点：按引用该正文的 Interaction 逐个应用主体映射。
    pub(super) fn mask_contents(
        &self,
        contents: &mut [ObservationContent],
        owners: &HashMap<i64, Vec<String>>,
    ) {
        for content in contents {
            let Some(interactions) = owners.get(&content.content_id) else {
                continue;
            };
            for interaction in interactions {
                if let Some(secrets) = self.secrets(Some(interaction)) {
                    secrets.value(&mut content.body);
                }
            }
        }
    }

    /// 当前已加载映射的稳定版本；无映射时是固定空版本。
    pub(super) fn version(&self) -> String {
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        for (principal, secrets) in &self.principals {
            hasher.update(principal.as_bytes());
            hasher.update([0]);
            hasher.update(secrets.version());
        }
        crate::interaction_observation::hex(&hasher.finalize())
    }

    pub(super) fn mask_summary(&self, summary: &mut InteractionSummary) {
        let Some(secrets) = self.secrets(Some(summary.id.as_str())) else {
            return;
        };
        for event in &mut summary.context_events {
            if !carries_content(&event.kind) {
                continue;
            }
            secrets.value(&mut event.payload);
        }
    }

    fn mask_event(&self, event: &mut ObservationEvent, fallback: Option<&str>) {
        if !carries_content(&event.kind) {
            return;
        }
        let Some(secrets) = self.secrets(event.interaction_id.as_deref().or(fallback)) else {
            return;
        };
        secrets.value(&mut event.payload);
    }

    fn mask_events(&self, events: &mut [ObservationEvent], fallback: Option<&str>) {
        for event in events {
            self.mask_event(event, fallback);
        }
    }

    pub(super) fn mask_failure(&self, interaction: Option<&str>, failure: &mut FailureDiagnostic) {
        let Some(secrets) = self.secrets(interaction) else {
            return;
        };
        if let Some(message) = &mut failure.message {
            secrets.text(message);
        }
    }

    pub(super) fn mask_client_error_summary(&self, item: &mut ClientErrorSummary) {
        self.mask_failure(item.interaction_id.as_deref(), &mut item.error);
    }

    pub(super) fn mask_upstream_error_summary(&self, item: &mut UpstreamErrorSummary) {
        self.mask_failure(item.interaction_id.as_deref(), &mut item.error);
    }

    pub(super) fn mask_client_error_page(&self, page: &mut ClientErrorPage) {
        for item in &mut page.items {
            self.mask_client_error_summary(item);
        }
    }

    pub(super) fn mask_upstream_error_page(&self, page: &mut UpstreamErrorPage) {
        for item in &mut page.items {
            self.mask_upstream_error_summary(item);
        }
    }

    pub(super) fn mask_client_error_detail(&self, detail: &mut ClientErrorDetail) {
        let interaction = detail.request.interaction_id.as_deref();
        self.mask_failure(interaction, &mut detail.request.error);
        self.mask_events(&mut detail.events, interaction);
    }

    pub(super) fn mask_upstream_error_detail(&self, detail: &mut UpstreamErrorDetail) {
        let interaction = detail.error.interaction_id.as_deref();
        self.mask_failure(interaction, &mut detail.error.error);
        self.mask_events(&mut detail.events, interaction);
    }

    fn mask_run(&self, run: &mut RunDetail, fallback: Option<&str>) {
        self.mask_events(&mut run.events, fallback);
    }

    pub(super) fn mask_interaction_detail(&self, detail: &mut InteractionDetail) {
        self.mask_summary(&mut detail.interaction);
        if let Some(secrets) = self.secrets(Some(detail.interaction.id.as_str())) {
            secrets.text(&mut detail.input_text);
            secrets.text(&mut detail.output_text);
        }
        for summary in &mut detail.root.interactions {
            self.mask_summary(summary);
        }
        let interaction = detail.interaction.id.clone();
        for run in &mut detail.runs {
            self.mask_run(run, Some(&interaction));
        }
    }

    pub(super) fn mask_interaction_events_page(
        &self,
        page: &mut InteractionEventsPage,
        interaction: &str,
    ) {
        for run in &mut page.runs {
            self.mask_run(run, Some(interaction));
        }
    }

    pub(super) fn mask_forest(&self, page: &mut ForestPage) {
        for root in &mut page.roots {
            for summary in &mut root.interactions {
                self.mask_summary(summary);
            }
        }
    }

    pub(super) fn mask_root_changes(&self, page: &mut RootChangesPage) {
        for change in &mut page.changes {
            for summary in &mut change.interactions {
                self.mask_summary(summary);
            }
        }
    }
}

/// 实时订阅的主体掩码器：有效映射最多每秒刷新一次，刷新失败时结束订阅而不是输出明文。
pub(super) struct LiveCredentialMasker {
    mappings: Arc<dyn MappingStore>,
    observation: InteractionObservation,
    interaction_id: String,
    principal: Option<String>,
    secrets: Option<ProtectedSecrets>,
    refreshed_at: tokio::time::Instant,
}

impl LiveCredentialMasker {
    pub(super) async fn load(
        mappings: Arc<dyn MappingStore>,
        observation: InteractionObservation,
        interaction_id: String,
    ) -> anyhow::Result<Self> {
        let mut masker = Self {
            mappings,
            observation,
            interaction_id,
            principal: None,
            secrets: None,
            refreshed_at: tokio::time::Instant::now(),
        };
        if masker.mappings.any_active().await? {
            masker.reload().await?;
        }
        Ok(masker)
    }

    /// 只在存在未过期映射时解析主体并加载映射；主体只解析一次。
    async fn reload(&mut self) -> anyhow::Result<()> {
        if self.principal.is_none() {
            let principals = self
                .observation
                .interaction_principals(std::slice::from_ref(&self.interaction_id))
                .await?;
            self.principal = Some(
                principals
                    .get(&self.interaction_id)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("observation interaction has no principal"))?,
            );
        }
        let mut secrets = None;
        if let Some(principal) = self.principal.as_deref().and_then(maskable_principal) {
            let active = self.mappings.active(&Principal::new(principal)).await?;
            if !active.is_empty() {
                let protected = ProtectedSecrets::default();
                protected.register(active.iter().map(|mapping| mapping.secret.as_str()));
                secrets = Some(protected);
            }
        }
        self.secrets = secrets;
        self.refreshed_at = tokio::time::Instant::now();
        Ok(())
    }

    async fn refresh_if_due(&mut self) -> anyhow::Result<()> {
        if self.refreshed_at.elapsed() < LIVE_MAPPING_REFRESH {
            return Ok(());
        }
        if self.mappings.any_active().await? {
            self.reload().await?;
        } else {
            self.secrets = None;
            self.refreshed_at = tokio::time::Instant::now();
        }
        Ok(())
    }

    fn mask_block(&self, block: &mut LiveContentBlock) {
        if let Some(secrets) = &self.secrets {
            secrets.text(&mut block.text);
        }
    }
}

/// 在 SSE 输出边界脱敏实时内容。
///
/// 刷新映射失败时发送 `live_gap` 并结束订阅：继续输出会让刷新前新建的明文泄漏。
pub(super) fn masked_live_stream(
    stream: ObservationStream,
    masker: LiveCredentialMasker,
    interaction_id: String,
) -> ObservationStream {
    Box::pin(futures::stream::unfold(
        (stream, masker, interaction_id, false),
        |(mut stream, mut masker, interaction_id, finished)| async move {
            if finished {
                return None;
            }
            let update = stream.next().await?;
            let masked = match update {
                ObservationUpdate::LiveContent(mut block) => {
                    if let Err(error) = masker.refresh_if_due().await {
                        tracing::warn!(cause = %error, "live observation masking unavailable");
                        return Some((
                            ObservationUpdate::LiveGap {
                                interaction_id,
                                run_id: block.run_id,
                                reason: "masking_unavailable".into(),
                            },
                            (stream, masker, String::new(), true),
                        ));
                    }
                    masker.mask_block(&mut block);
                    ObservationUpdate::LiveContent(block)
                }
                ObservationUpdate::LiveSnapshot { mut blocks } => {
                    if let Err(error) = masker.refresh_if_due().await {
                        tracing::warn!(cause = %error, "live observation masking unavailable");
                        return Some((
                            ObservationUpdate::LiveGap {
                                interaction_id,
                                run_id: String::new(),
                                reason: "masking_unavailable".into(),
                            },
                            (stream, masker, String::new(), true),
                        ));
                    }
                    for block in &mut blocks {
                        masker.mask_block(block);
                    }
                    ObservationUpdate::LiveSnapshot { blocks }
                }
                update => update,
            };
            Some((masked, (stream, masker, interaction_id, finished)))
        },
    ))
}

impl AdminService {
    /// 一次响应只做一次存在性检查、一次主体解析与每个主体一次映射加载。
    pub(super) async fn observation_masker(
        &self,
        interactions: Vec<String>,
    ) -> anyhow::Result<CredentialMasker> {
        let ids: Vec<String> = interactions
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let lookup_ids = ids.clone();
        let observation = self.gw.observation.clone();
        CredentialMasker::load(&*self.gw.redaction.mappings, ids, async move {
            observation.interaction_principals(&lookup_ids).await
        })
        .await
    }

    /// 实时订阅在建立连接前只做一次存在性检查；主体与映射按需解析并每秒刷新。
    pub(super) async fn masked_live_observation(
        &self,
        interaction_id: String,
    ) -> anyhow::Result<ObservationStream> {
        let masker = LiveCredentialMasker::load(
            Arc::clone(&self.gw.redaction.mappings),
            self.gw.observation.clone(),
            interaction_id.clone(),
        )
        .await?;
        Ok(masked_live_stream(
            self.gw.observation.subscribe_live(interaction_id.clone()),
            masker,
            interaction_id,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use parking_lot::Mutex;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use stravia_credential_protection::store::{DetectedSecret, InternedMappings, Mapping};
    use stravia_runtime_contract::redaction::RedactionError;

    const PLAINTEXT: &str = "sk-live-abcdefghijklmnopqrstuvwxyz";

    fn marker() -> String {
        format!(
            "{}abcdefghijklmnopqrstuvwxyzab{}",
            stravia_credential_protection::marker::PREFIX,
            stravia_credential_protection::marker::SUFFIX
        )
    }

    #[derive(Default)]
    struct CountingMappings {
        plaintexts: Mutex<BTreeMap<String, Vec<String>>>,
        active_calls: Mutex<Vec<String>>,
        existence_calls: AtomicUsize,
        any_active: bool,
        fail: bool,
    }

    impl CountingMappings {
        fn with(secrets: &[&str]) -> Self {
            Self {
                plaintexts: Mutex::new(BTreeMap::from([(
                    "owner".to_owned(),
                    secrets.iter().map(|secret| (*secret).to_owned()).collect(),
                )])),
                any_active: true,
                ..Default::default()
            }
        }

        fn add(&self, secret: &str) {
            self.plaintexts
                .lock()
                .entry("owner".to_owned())
                .or_default()
                .push(secret.to_owned());
        }

        fn active_calls(&self) -> Vec<String> {
            self.active_calls.lock().clone()
        }

        fn existence_calls(&self) -> usize {
            self.existence_calls.load(Ordering::Acquire)
        }
    }

    #[async_trait]
    impl MappingStore for CountingMappings {
        async fn active(&self, principal: &Principal) -> Result<Vec<Mapping>, RedactionError> {
            self.active_calls
                .lock()
                .push(principal.api_key_id().to_owned());
            if self.fail {
                return Err(RedactionError::Storage);
            }
            let plaintexts = self.plaintexts.lock();
            Ok(plaintexts
                .get(principal.api_key_id())
                .into_iter()
                .flatten()
                .map(|secret| Mapping {
                    reference: format!("ref-{secret}"),
                    secret: secret.clone(),
                    rule_ids: Vec::new().into(),
                })
                .collect())
        }

        async fn any_active(&self) -> Result<bool, RedactionError> {
            self.existence_calls.fetch_add(1, Ordering::AcqRel);
            if self.fail {
                return Err(RedactionError::Storage);
            }
            Ok(self.any_active)
        }

        async fn intern(
            &self,
            _principal: &Principal,
            _secrets: &[DetectedSecret],
        ) -> Result<InternedMappings, RedactionError> {
            unimplemented!("masker never creates mappings")
        }

        async fn publish(
            &self,
            _principal: &Principal,
            _references: &[String],
        ) -> Result<(), RedactionError> {
            unimplemented!("masker never publishes mappings")
        }

        async fn sweep(
            &self,
            _now: i64,
            _retention: std::time::Duration,
        ) -> Result<u64, RedactionError> {
            unimplemented!("masker never sweeps mappings")
        }

        async fn plaintext(&self, _reference: &str) -> Result<Option<String>, RedactionError> {
            unimplemented!("masker never restores plaintext")
        }
    }

    fn summary(id: &str, text: &str) -> InteractionSummary {
        InteractionSummary {
            context_events: vec![
                ObservationEvent {
                    sequence: 1,
                    occurred_at: 0,
                    interaction_id: Some(id.into()),
                    run_id: Some("run".into()),
                    rejection_id: None,
                    kind: "target_attempt_started".into(),
                    payload: json!({"attempt_id": format!("{text}-attempt")}),
                },
                ObservationEvent {
                    sequence: 2,
                    occurred_at: 0,
                    interaction_id: Some(id.into()),
                    run_id: Some("run".into()),
                    rejection_id: None,
                    kind: "client_tool_result".into(),
                    payload: json!({
                        "content": serde_json::to_string(text).unwrap(),
                        "arguments": format!(r#"{{"api_key":"{text}"}}"#),
                        "marker": marker(),
                    }),
                },
            ],
            id: id.into(),
            root_id: "root".into(),
            parent_interaction_id: None,
            generation_root_id: None,
            first_route_id: "route".into(),
            first_model_display_name: None,
            status: "completed".into(),
            started_at: 0,
            last_active_at: 1,
            input: Default::default(),
            output: Default::default(),
            failed_request: false,
            client_output_delivered: true,
            usage: Default::default(),
            debug_status: "none".into(),
            observation_gap: false,
            matched: true,
            last_event_sequence: 2,
        }
    }

    fn event(interaction: Option<&str>, text: &str) -> ObservationEvent {
        ObservationEvent {
            sequence: 2,
            occurred_at: 0,
            interaction_id: interaction.map(str::to_owned),
            run_id: Some("run".into()),
            rejection_id: None,
            kind: "target_attempt_finished".into(),
            payload: json!({"error": {"message": text}, "nested": serde_json::to_string(text).unwrap()}),
        }
    }

    #[tokio::test]
    async fn no_active_mappings_skip_principal_lookup_and_mapping_load() -> anyhow::Result<()> {
        let mappings = CountingMappings::default();
        let lookups = Arc::new(AtomicUsize::new(0));
        let lookup_calls = Arc::clone(&lookups);
        let masker = CredentialMasker::load(
            &mappings,
            ["first".to_owned(), "second".to_owned()],
            async move {
                lookup_calls.fetch_add(1, Ordering::AcqRel);
                Ok(HashMap::new())
            },
        )
        .await?;
        assert_eq!(mappings.existence_calls(), 1);
        assert_eq!(lookups.load(Ordering::Acquire), 0);
        assert!(mappings.active_calls().is_empty());
        let mut summary = summary("first", PLAINTEXT);
        masker.mask_summary(&mut summary);
        assert_eq!(
            summary.context_events[1].payload["content"],
            json!(serde_json::to_string(PLAINTEXT).unwrap())
        );
        Ok(())
    }

    #[tokio::test]
    async fn empty_interaction_set_skips_even_the_existence_check() -> anyhow::Result<()> {
        let mappings = CountingMappings::default();
        let lookups = Arc::new(AtomicUsize::new(0));
        let lookup_calls = Arc::clone(&lookups);
        CredentialMasker::load(&mappings, Vec::new(), async move {
            lookup_calls.fetch_add(1, Ordering::AcqRel);
            Ok(HashMap::new())
        })
        .await?;
        assert_eq!(mappings.existence_calls(), 0);
        assert_eq!(lookups.load(Ordering::Acquire), 0);
        Ok(())
    }

    #[tokio::test]
    async fn masker_loads_each_distinct_principal_once_and_masks_content_kinds()
    -> anyhow::Result<()> {
        let mappings = CountingMappings::with(&[PLAINTEXT]);
        let masker = CredentialMasker::load(
            &mappings,
            ["first".to_owned(), "second".to_owned(), "third".to_owned()],
            async {
                Ok(HashMap::from([
                    ("first".to_owned(), "owner".to_owned()),
                    ("second".to_owned(), "owner".to_owned()),
                    ("third".to_owned(), "other".to_owned()),
                ]))
            },
        )
        .await?;
        assert_eq!(mappings.existence_calls(), 1);
        assert_eq!(
            mappings.active_calls(),
            ["other".to_owned(), "owner".to_owned()]
        );

        let marker = marker();
        let mut owned = summary("first", PLAINTEXT);
        masker.mask_summary(&mut owned);
        assert_eq!(
            owned.context_events[0].payload,
            json!({"attempt_id": format!("{PLAINTEXT}-attempt")}),
            "metadata-only kinds keep their payload untouched"
        );
        assert_eq!(
            owned.context_events[1].payload["content"],
            json!(r#""***""#)
        );
        assert_eq!(
            owned.context_events[1].payload["arguments"],
            json!(r#"{"api_key":"***"}"#)
        );
        assert_eq!(owned.context_events[1].payload["marker"], json!(marker));
        assert_eq!(owned.id, "first");
        assert!(owned.usage.input_tokens.is_none());
        assert_eq!(owned.last_event_sequence, 2);

        let mut untouched = summary("third", PLAINTEXT);
        masker.mask_summary(&mut untouched);
        assert_eq!(
            untouched.context_events[1].payload["content"],
            json!(serde_json::to_string(PLAINTEXT).unwrap())
        );
        Ok(())
    }

    #[tokio::test]
    async fn principal_without_mappings_is_skipped_entirely() -> anyhow::Result<()> {
        let mappings = CountingMappings::with(&[PLAINTEXT]);
        let masker = CredentialMasker::load(
            &mappings,
            ["owned".to_owned(), "unprotected".to_owned()],
            async {
                Ok(HashMap::from([
                    ("owned".to_owned(), "owner".to_owned()),
                    ("unprotected".to_owned(), "no-mappings".to_owned()),
                ]))
            },
        )
        .await?;
        assert_eq!(
            mappings.active_calls(),
            ["no-mappings".to_owned(), "owner".to_owned()]
        );
        assert!(masker.secrets(Some("unprotected")).is_none());
        assert!(masker.secrets(Some("owned")).is_some());
        let mut unprotected = summary("unprotected", PLAINTEXT);
        masker.mask_summary(&mut unprotected);
        assert_eq!(
            unprotected.context_events[1].payload["content"],
            json!(serde_json::to_string(PLAINTEXT).unwrap())
        );
        Ok(())
    }

    #[tokio::test]
    async fn mapping_lookup_failure_fails_instead_of_returning_plaintext() {
        let mappings = CountingMappings {
            fail: true,
            ..Default::default()
        };
        let result = CredentialMasker::load(&mappings, ["first".to_owned()], async {
            Ok(HashMap::from([("first".to_owned(), "owner".to_owned())]))
        })
        .await;
        assert!(result.is_err(), "existence check failure must surface");
    }

    #[tokio::test]
    async fn incomplete_principal_lookup_fails_instead_of_returning_plaintext() {
        let mappings = CountingMappings::with(&[PLAINTEXT]);
        let result = CredentialMasker::load(
            &mappings,
            ["first".to_owned(), "missing".to_owned()],
            async { Ok(HashMap::from([("first".to_owned(), "owner".to_owned())])) },
        )
        .await;
        assert!(
            result.is_err(),
            "missing principal must not degrade to plaintext"
        );
    }

    #[tokio::test]
    async fn detail_events_and_error_summaries_use_the_owning_interaction() -> anyhow::Result<()> {
        let mappings = CountingMappings::with(&[PLAINTEXT]);
        let masker = CredentialMasker::load(
            &mappings,
            [
                "owner-interaction".to_owned(),
                "other-interaction".to_owned(),
            ],
            async {
                Ok(HashMap::from([
                    ("owner-interaction".to_owned(), "owner".to_owned()),
                    ("other-interaction".to_owned(), "other".to_owned()),
                ]))
            },
        )
        .await?;

        let mut events = vec![
            event(Some("owner-interaction"), PLAINTEXT),
            event(None, PLAINTEXT),
            event(Some("other-interaction"), PLAINTEXT),
        ];
        masker.mask_events(&mut events, Some("owner-interaction"));
        assert_eq!(events[0].payload["error"]["message"], json!("***"));
        assert_eq!(
            events[1].payload["error"]["message"],
            json!("***"),
            "events without their own interaction fall back to the response interaction"
        );
        assert_eq!(
            events[2].payload["error"]["message"],
            json!(PLAINTEXT),
            "another principal's event stays untouched"
        );
        assert_eq!(events[0].kind, "target_attempt_finished");
        assert_eq!(events[0].sequence, 2);
        Ok(())
    }

    #[tokio::test]
    async fn client_and_upstream_error_pages_mask_messages_only_for_owned_interactions()
    -> anyhow::Result<()> {
        let mappings = CountingMappings::with(&[PLAINTEXT]);
        let masker = CredentialMasker::load(&mappings, ["owner-interaction".to_owned()], async {
            Ok(HashMap::from([(
                "owner-interaction".to_owned(),
                "owner".to_owned(),
            )]))
        })
        .await?;
        let failure = |message: &str| FailureDiagnostic {
            source: Some("upstream".into()),
            code: Some("upstream_status".into()),
            message: Some(message.into()),
            status_code: Some(502),
            upstream_code: Some("bad".into()),
        };

        let mut page = ClientErrorPage {
            items: vec![
                ClientErrorSummary {
                    request_id: "owned".into(),
                    id: "owned".into(),
                    kind: "run".into(),
                    request_type: "failed".into(),
                    started_at: 0,
                    duration_ms: Some(1),
                    api_key_id: Some("owner".into()),
                    api_key_name: None,
                    client: None,
                    model: None,
                    model_display_name: None,
                    services: Vec::new(),
                    error: failure(&format!("upstream said {PLAINTEXT}")),
                    interaction_id: Some("owner-interaction".into()),
                    root_id: Some("root".into()),
                    run_id: Some("run".into()),
                    debug_status: "none".into(),
                    observation_gap: false,
                },
                ClientErrorSummary {
                    request_id: "rejected".into(),
                    id: "rejected".into(),
                    kind: "rejection".into(),
                    request_type: "rejected".into(),
                    started_at: 0,
                    duration_ms: Some(1),
                    api_key_id: None,
                    api_key_name: None,
                    client: None,
                    model: None,
                    model_display_name: None,
                    services: Vec::new(),
                    error: failure(&format!("rejected {PLAINTEXT}")),
                    interaction_id: None,
                    root_id: None,
                    run_id: None,
                    debug_status: "none".into(),
                    observation_gap: false,
                },
            ],
            total: 2,
            next_cursor: None,
            snapshot_sequence: 7,
        };
        masker.mask_client_error_page(&mut page);
        assert_eq!(
            page.items[0].error.message.as_deref(),
            Some("upstream said ***")
        );
        assert_eq!(page.items[0].error.status_code, Some(502));
        assert_eq!(page.items[0].error.upstream_code.as_deref(), Some("bad"));
        assert_eq!(
            page.items[1].error.message.as_deref(),
            Some(format!("rejected {PLAINTEXT}").as_str()),
            "rejections have no interaction principal"
        );
        assert_eq!(page.snapshot_sequence, 7);

        let mut upstream = UpstreamErrorPage {
            items: vec![UpstreamErrorSummary {
                id: "attempt".into(),
                run_id: "run".into(),
                interaction_id: Some("owner-interaction".into()),
                root_id: Some("root".into()),
                request_id: "owned".into(),
                started_at: 0,
                duration_ms: Some(1),
                provider_id: Some("provider".into()),
                provider_name: "Provider".into(),
                upstream_model: Some("model".into()),
                model: None,
                model_display_name: None,
                api_key_id: Some("owner".into()),
                api_key_name: None,
                client: None,
                error: failure(&format!("body {PLAINTEXT}")),
                outcome: "caused_failure".into(),
                debug_status: "none".into(),
                observation_gap: false,
            }],
            total: 1,
            next_cursor: None,
            snapshot_sequence: 8,
        };
        masker.mask_upstream_error_page(&mut upstream);
        assert_eq!(upstream.items[0].error.message.as_deref(), Some("body ***"));
        Ok(())
    }

    fn live_block(text: &str) -> LiveContentBlock {
        LiveContentBlock {
            block_id: "block".into(),
            interaction_id: "interaction".into(),
            run_id: "run".into(),
            kind: "text".into(),
            model_turn_id: None,
            attempt_id: None,
            occurred_at: 0,
            receive_order: None,
            revision: 1,
            text: text.into(),
        }
    }

    #[tokio::test]
    async fn live_masker_without_active_mappings_passes_blocks_through() -> anyhow::Result<()> {
        let mappings = Arc::new(CountingMappings::default());
        let (_directory, _pool, observation) = test_observation().await?;
        let masker = LiveCredentialMasker::load(
            Arc::clone(&mappings) as Arc<dyn MappingStore>,
            observation,
            "interaction".into(),
        )
        .await?;
        assert_eq!(mappings.existence_calls(), 1);
        assert!(mappings.active_calls().is_empty());
        let mut block = live_block(&format!("accumulated {PLAINTEXT}"));
        masker.mask_block(&mut block);
        assert_eq!(block.text, format!("accumulated {PLAINTEXT}"));
        Ok(())
    }

    #[tokio::test]
    async fn live_masker_refreshes_mappings_at_most_once_per_second() -> anyhow::Result<()> {
        let mappings = Arc::new(CountingMappings::with(&[PLAINTEXT]));
        let (_directory, _pool, observation) = test_observation().await?;
        let mut masker = LiveCredentialMasker::load(
            Arc::clone(&mappings) as Arc<dyn MappingStore>,
            observation,
            "interaction".into(),
        )
        .await?;
        tokio::time::pause();
        assert_eq!(mappings.existence_calls(), 1);
        assert_eq!(mappings.active_calls().len(), 1);

        mappings.add("sk-live-second");
        let mut block = live_block("accumulated sk-live-second");
        masker.mask_block(&mut block);
        assert_eq!(block.text, "accumulated sk-live-second");
        masker.refresh_if_due().await?;
        assert_eq!(
            mappings.active_calls().len(),
            1,
            "no refresh within one second"
        );

        tokio::time::advance(LIVE_MAPPING_REFRESH).await;
        masker.refresh_if_due().await?;
        assert_eq!(mappings.active_calls().len(), 2);
        masker.mask_block(&mut block);
        assert_eq!(block.text, "accumulated ***");
        tokio::time::resume();
        Ok(())
    }

    #[tokio::test]
    async fn masked_live_stream_masks_content_and_snapshot_blocks() -> anyhow::Result<()> {
        let mappings = CountingMappings::with(&[PLAINTEXT]);
        let (_directory, _pool, observation) = test_observation().await?;
        let masker = LiveCredentialMasker::load(
            Arc::new(mappings) as Arc<dyn MappingStore>,
            observation,
            "interaction".into(),
        )
        .await?;
        let updates = vec![
            ObservationUpdate::LiveSnapshot {
                blocks: vec![live_block(&format!("snapshot {PLAINTEXT}"))],
            },
            ObservationUpdate::LiveContent(live_block(&format!("delta {PLAINTEXT}"))),
            ObservationUpdate::LiveFinished {
                interaction_id: "interaction".into(),
                run_id: "run".into(),
            },
        ];
        let mut stream = masked_live_stream(
            Box::pin(futures::stream::iter(updates)),
            masker,
            "interaction".into(),
        );
        match stream.next().await {
            Some(ObservationUpdate::LiveSnapshot { blocks }) => {
                assert_eq!(blocks[0].text, "snapshot ***");
            }
            _ => panic!("expected masked snapshot"),
        }
        match stream.next().await {
            Some(ObservationUpdate::LiveContent(block)) => assert_eq!(block.text, "delta ***"),
            _ => panic!("expected masked live content"),
        }
        assert!(matches!(
            stream.next().await,
            Some(ObservationUpdate::LiveFinished { .. })
        ));
        assert!(stream.next().await.is_none());
        Ok(())
    }

    /// 真实 SQLite：批量主体解析、存在性检查与过期映射不参与脱敏。
    #[tokio::test]
    async fn real_sqlite_batches_principals_and_masks_only_active_mappings() -> anyhow::Result<()> {
        use stravia_credential_protection::store::SqlMappingStore;

        let (_directory, pool, observation) = test_observation().await?;
        let store = SqlMappingStore::sqlite(pool.clone());
        assert!(!store.any_active().await?);

        let now = chrono::Utc::now().timestamp_millis();
        sqlx::query(
            "INSERT INTO reversible_redaction_mappings (reference,principal,secret,created_at,updated_at,pending_until) VALUES (?,?,?,?,?,?)",
        )
        .bind("ref-owner")
        .bind("api-key:owner")
        .bind(PLAINTEXT)
        .bind(now)
        .bind(now)
        .bind(now + 60_000)
        .execute(&pool)
        .await?;
        assert!(store.any_active().await?);

        let ids = vec!["interaction".to_owned(), "other".to_owned()];
        let principals = observation.interaction_principals(&ids).await?;
        assert_eq!(principals.len(), 2);
        assert_eq!(principals["interaction"], "owner");
        assert_eq!(principals["other"], "other");

        let lookup_ids = ids.clone();
        let lookup = observation.clone();
        let masker = CredentialMasker::load(&store, ids, async move {
            lookup.interaction_principals(&lookup_ids).await
        })
        .await?;
        let content = |content_id: i64| ObservationContent {
            content_id,
            format: "text".into(),
            raw_len: PLAINTEXT.len() as i64,
            body: json!(PLAINTEXT),
            digest: [0; 32],
        };
        let owners = HashMap::from([
            (1, vec!["interaction".to_owned()]),
            (2, vec!["other".to_owned()]),
        ]);
        let mut contents = vec![content(1), content(2)];
        masker.mask_contents(&mut contents, &owners);
        assert_eq!(contents[0].body, json!("***"));
        assert_eq!(
            contents[1].body,
            json!(PLAINTEXT),
            "没有映射的主体不应被遮蔽"
        );
        Ok(())
    }

    /// 真实 SQLite 观察存储：主体解析只在存在未过期映射时才会用到。
    async fn test_observation()
    -> anyhow::Result<(tempfile::TempDir, sqlx::SqlitePool, InteractionObservation)> {
        let pool = crate::test_support::migrated_sqlite_pool().await?;
        let directory = tempfile::tempdir()?;
        let observation = InteractionObservation::new(
            Some(pool.clone()),
            None,
            directory.path().to_path_buf(),
            crate::history_retention::HistoryRetention::new(1),
            false,
            crate::generation_chain::test_chain().await,
        )
        .await;
        sqlx::query(
            "INSERT INTO interaction_observations (id,principal,root_id,root_run_id,first_route_id,status,started_at,last_active_at) VALUES ('interaction','owner','root','run','route','completed',0,0)",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO interaction_observations (id,principal,root_id,root_run_id,first_route_id,status,started_at,last_active_at) VALUES ('other','other','other-root','other-run','route','completed',0,0)",
        )
        .execute(&pool)
        .await?;
        Ok((directory, pool, observation))
    }
}
