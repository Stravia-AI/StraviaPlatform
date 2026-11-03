//! 接缝 1：Core 历史维护接缝。
//!
//! 写入走领域入口（Generation Chain 提交、History Marker 创建与发布、可逆脱敏映射登记与
//! 发布、原生压缩登记与确认），读取走领域读取 API；时间只通过
//! `HistoryMaintenance::sweep_history(now)` 的 `now` 推进，不修改任何时间戳列，也不断言
//! 队列或 SQL 的内部形态（不变量测试按规格只数各表行数）。
//!
//! 同一组场景在 SQLite 与 PostgreSQL 上各跑一遍；PostgreSQL 场景 `#[ignore]`，
//! 依赖 `STRAVIA_TEST_POSTGRES_URL`（或 `DB_URL` / `DATABASE_URL`）指向隔离的测试库，
//! 每个场景使用独立 schema 并在结束后清理。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use sqlx::{PgPool, SqlitePool};
use stravia_credential_protection::store::{DetectedSecret, MappingStore, SqlMappingStore};
use stravia_runtime_contract::Principal;
use stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24;
use stravia_runtime_contract::protocol::ir::{
    AiItem, AiItemAudience, AiItemProvenance, AiItemStatus, AiRequest, AiResponse, ContentBlock,
    MessageContent, Role, ToolCall, ToolResultContentKind,
};
use stravia_runtime_contract::turn_chain::{
    TurnChainStore, TurnCommitError, TurnNodeId, TurnNodeKind,
};

use super::{ContentGc, HistoryMaintenance, HistoryRetention};
use crate::compaction::{Compaction, CompactionRecord, CompactionRegistration, CompactionTarget};
use crate::generation_chain::{GenerationChain, GenerationSource, PersistError};
use crate::history_marker::{
    ClaimOutcome, HiddenHistorySegment, HistoryMarkerStore, PlatformExecutionState,
    PlatformMarkerInput, PublicExecutionInput, SqlHistoryMarkerStore, ThinkingMarkerInput,
    render_history_marker_reference, render_text_projection_span,
};
use crate::turn_chain::SqlTurnChainStore;

const HOUR: i64 = 3_600_000;
const DAY: i64 = 24 * HOUR;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

// ---------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum Db {
    Sqlite(SqlitePool),
    Postgres(PgPool),
}

/// 一个事务里要引用的依赖。Marker 同时覆盖普通 Marker 语法与跨协议投影语法。
#[derive(Default)]
struct Refs {
    plain_markers: Vec<String>,
    projected_markers: Vec<String>,
    mappings: Vec<String>,
    compaction: Option<AiItem>,
    /// 响应正文；缺省时每次生成唯一文本。
    answer: Option<String>,
}

impl Refs {
    fn marker(reference: &str) -> Self {
        Self {
            plain_markers: vec![reference.to_owned()],
            ..Self::default()
        }
    }

    fn mapping(reference: &str) -> Self {
        Self {
            mappings: vec![reference.to_owned()],
            ..Self::default()
        }
    }

    fn answer(text: &str) -> Self {
        Self {
            answer: Some(text.to_owned()),
            ..Self::default()
        }
    }
}

fn is_dependency_unavailable(error: &PersistError) -> bool {
    matches!(
        error,
        PersistError::Store(TurnCommitError::DependencyUnavailable)
    )
}

struct Compacted {
    record: CompactionRecord,
    state: AiItem,
}

struct Fixture {
    db: Db,
    retention: HistoryRetention,
    maintenance: HistoryMaintenance,
    turns: Arc<dyn TurnChainStore>,
    markers: Arc<dyn HistoryMarkerStore>,
    mappings: Arc<dyn MappingStore>,
    compaction: Compaction,
    chain: GenerationChain,
    owner: Principal,
    counter: AtomicU64,
    _observation_dir: tempfile::TempDir,
}

impl Fixture {
    async fn new(db: Db, days: u32) -> Self {
        let retention = HistoryRetention::new(days);
        let (turns, markers, mappings, compaction, content): (
            Arc<dyn TurnChainStore>,
            Arc<dyn HistoryMarkerStore>,
            Arc<dyn MappingStore>,
            Compaction,
            ContentGc,
        ) = match &db {
            Db::Sqlite(pool) => (
                Arc::new(SqlTurnChainStore::sqlite(pool.clone()).with_retention(retention.clone())),
                Arc::new(SqlHistoryMarkerStore::sqlite(pool.clone())),
                Arc::new(SqlMappingStore::sqlite(pool.clone())),
                Compaction::sqlite(pool.clone()),
                ContentGc::Sqlite(pool.clone()),
            ),
            Db::Postgres(pool) => (
                Arc::new(
                    SqlTurnChainStore::postgres(pool.clone()).with_retention(retention.clone()),
                ),
                Arc::new(SqlHistoryMarkerStore::postgres(pool.clone())),
                Arc::new(SqlMappingStore::postgres(pool.clone())),
                Compaction::postgres(pool.clone()),
                ContentGc::Postgres(pool.clone()),
            ),
        };
        let chain = GenerationChain::from_turn_chain(
            Arc::clone(&turns),
            None,
            crate::runtime_cache::RuntimeCache::tinyufo(16 * 1024 * 1024),
        )
        .with_retention(retention.clone())
        .with_history_markers(Arc::clone(&markers))
        .with_redaction_mappings(Arc::clone(&mappings))
        .with_compaction(compaction.clone());
        let observation_dir = tempfile::tempdir().expect("observation directory");
        let (sqlite, postgres) = match &db {
            Db::Sqlite(pool) => (Some(pool.clone()), None),
            Db::Postgres(pool) => (None, Some(pool.clone())),
        };
        let observation = crate::interaction_observation::InteractionObservation::new(
            sqlite,
            postgres,
            observation_dir.path().to_path_buf(),
            crate::history_retention::HistoryRetention::new(days),
            false,
            chain.clone(),
        )
        .await;
        let maintenance = HistoryMaintenance {
            retention: retention.clone(),
            observation,
            turn_chain: Arc::clone(&turns),
            history_markers: Arc::clone(&markers),
            mappings: Arc::clone(&mappings),
            compaction: compaction.clone(),
            content,
        };
        Self {
            db,
            retention,
            maintenance,
            turns,
            markers,
            mappings,
            compaction,
            chain,
            owner: Principal::new("retention-seam-owner"),
            counter: AtomicU64::new(0),
            _observation_dir: observation_dir,
        }
    }

    fn unique(&self, label: &str) -> String {
        format!("{label}-{}", self.counter.fetch_add(1, Ordering::Relaxed))
    }

    /// 当前 N 对应的毫秒数。
    fn window(&self) -> i64 {
        self.retention.window_millis()
    }

    /// 单一扫描入口；`now` 是唯一的时间来源。
    async fn sweep(&self, now: i64) -> u64 {
        self.maintenance
            .sweep_history(now)
            .await
            .expect("sweep_history")
    }

    /// 扫到不再有进展（下一轮才回收的来源节点、登记来源等）。
    async fn sweep_to_completion(&self, now: i64) {
        for _ in 0..16 {
            if self.sweep(now).await == 0 {
                return;
            }
        }
        panic!("sweep_history did not converge");
    }

    /// 等墙钟推进到下一毫秒并返回边界 `t`：此前写入的 `created_at` / 发布时刻都 `<= t`，
    /// 此后写入的都 `> t`。用于把 `now` 精确放在两次写入之间；不是用 sleep 掩盖竞态。
    async fn tick(&self) -> i64 {
        let boundary = now_ms();
        while now_ms() <= boundary {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        boundary
    }

    // --- 写入 ---

    async fn try_commit(&self, parent: Option<&str>, refs: &Refs) -> Result<String, PersistError> {
        let mut items = Vec::new();
        if let Some(state) = &refs.compaction {
            items.push(state.clone());
        }
        items.push(AiItem {
            role: Role::User,
            content: MessageContent::Text(Arc::new(self.unique("question"))),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        });
        let mut request = AiRequest::new("model", items);
        request.meta.source_protocol = Some(OPEN_RESPONSES_2026_04_24);
        if let Some(parent) = parent {
            crate::router::stamp_previous_response_id(&mut request, parent);
        }
        request
            .meta
            .redaction
            .record(refs.mappings.iter().cloned())
            .expect("record redaction references");
        let mut write = self
            .chain
            .begin(self.owner.clone(), request)
            .await
            .expect("generation chain begin");
        let mut answer = String::new();
        for reference in &refs.plain_markers {
            answer.push_str(&render_history_marker_reference(reference));
        }
        for reference in &refs.projected_markers {
            answer.push_str(&render_text_projection_span(
                reference,
                0,
                "projected visible",
            ));
        }
        answer.push_str(&refs.answer.clone().unwrap_or_else(|| self.unique("answer")));
        let mut response = AiResponse::new("retention-response", "model");
        response.items = vec![AiItem::output_text(answer)];
        response.items[0].set_graph_metadata(
            Some(
                stravia_protocol_codec::codec::open_responses::formatter::gateway_item_id(
                    "msg",
                    write.id(),
                    0,
                ),
            ),
            Some(AiItemStatus::Completed),
            AiItemProvenance::Provider,
            AiItemAudience::Client,
        );
        let source = GenerationSource::Target(crate::history_marker::ThinkingSource {
            namespace: "provider:model".into(),
            protocol: Some(OPEN_RESPONSES_2026_04_24.into()),
            actual_model: "model".into(),
            target_id: "provider:model".into(),
            authority: Some("provider:model".into()),
            issued: Vec::new(),
        });
        assert!(write.stage(&mut response, &source, None), "stage response");
        write.persist().await?;
        Ok(write.id().to_owned())
    }

    async fn commit(&self, parent: Option<&str>, refs: &Refs) -> String {
        self.try_commit(parent, refs)
            .await
            .expect("generation chain commit")
    }

    /// 不写入，只确认 Generation Chain 仍能以该节点为父续接。
    async fn can_continue(&self, parent: &str) -> bool {
        let mut request = AiRequest::new(
            "model",
            vec![AiItem {
                role: Role::User,
                content: MessageContent::Text(Arc::new("continue".into())),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            }],
        );
        request.meta.source_protocol = Some(OPEN_RESPONSES_2026_04_24);
        crate::router::stamp_previous_response_id(&mut request, parent);
        self.chain.begin(self.owner.clone(), request).await.is_ok()
    }

    async fn thinking_marker(&self, body: &str, publish: bool) -> String {
        let reference = self
            .markers
            .create_thinking(
                &self.owner,
                ThinkingMarkerInput {
                    block: ContentBlock::Thinking {
                        thinking: body.to_owned(),
                        signature: Some("signature".into()),
                    },
                    source: None,
                    activity: "Preserving protected reasoning".into(),
                },
            )
            .await
            .expect("create marker")
            .reference;
        if publish {
            self.publish_marker(&reference).await;
        }
        reference
    }

    async fn publish_marker(&self, reference: &str) {
        self.markers
            .publish(&self.owner, &[reference.to_owned()])
            .await
            .expect("publish marker");
    }

    async fn mapping(&self, publish: bool) -> String {
        let secret = self.unique("retention-secret");
        let reference = self
            .mappings
            .intern(&self.owner, &[DetectedSecret::from(secret.as_str())])
            .await
            .expect("intern mapping")
            .mappings
            .remove(0)
            .reference;
        if publish {
            self.publish_mapping(&reference).await;
        }
        reference
    }

    async fn publish_mapping(&self, reference: &str) {
        self.mappings
            .publish(&self.owner, &[reference.to_owned()])
            .await
            .expect("publish mapping");
    }

    async fn native_compaction(
        &self,
        source_generation_id: Option<&str>,
        source_record_ids: &[&str],
        confirm: bool,
    ) -> Compacted {
        let state = stravia_protocol_codec::codec::open_responses::decoder::decode_input_item(
            &serde_json::json!({
                "type": "compaction",
                "id": self.unique("native-state"),
                "encrypted_content": self.unique("opaque-state"),
            }),
        )
        .expect("decode compaction state")
        .expect("compaction item");
        let record = self
            .compaction
            .register(
                &self.owner,
                CompactionRegistration {
                    source_generation_id: source_generation_id.map(str::to_owned),
                    source_record_ids: source_record_ids
                        .iter()
                        .map(|id| (*id).to_owned())
                        .collect(),
                    operation_id: self.unique("operation"),
                    target: CompactionTarget {
                        target_key: "local-target".into(),
                        namespace: "local-account-generation".into(),
                        model: "local-model".into(),
                        protocol: "open-responses/responses/2026-04-24".into(),
                    },
                    window: vec![state.clone()],
                    state_items: vec![state.clone()],
                },
            )
            .await
            .expect("register compaction");
        if confirm {
            self.confirm_compaction(&record).await;
        }
        Compacted { record, state }
    }

    async fn confirm_compaction(&self, compacted: &CompactionRecord) {
        self.compaction
            .confirm_delivery(&self.owner, std::slice::from_ref(&compacted.id))
            .await
            .expect("confirm compaction");
    }

    // --- 读取 ---

    async fn turn_readable(&self, id: &str) -> bool {
        self.turns
            .materialize(&self.owner, TurnNodeKind::Response, &TurnNodeId::new(id))
            .await
            .is_ok()
    }

    /// 读回整条链的节点数；节点不可用时为 0。
    async fn chain_len(&self, id: &str) -> usize {
        self.turns
            .materialize(&self.owner, TurnNodeKind::Response, &TurnNodeId::new(id))
            .await
            .map_or(0, |nodes| nodes.len())
    }

    async fn marker_readable(&self, reference: &str) -> bool {
        self.markers
            .resolve(&self.owner, reference)
            .await
            .expect("resolve marker")
            .is_some()
    }

    async fn mapping_readable(&self, reference: &str) -> bool {
        self.mappings
            .plaintext(reference)
            .await
            .expect("mapping plaintext")
            .is_some()
    }

    async fn compaction_readable(&self, state: &AiItem) -> bool {
        matches!(
            self.compaction
                .resolve(&self.owner, std::slice::from_ref(state))
                .await,
            Ok(Some(_))
        )
    }

    /// 平台工具 Marker：创建公开执行记录，经 claim / finish 走到终态（SQLite 靠触发器
    /// `retain_terminal_tool_execution` 写出执行记录的到期时刻），再发布。
    async fn executed_platform_marker(&self, execution_retention: Duration) -> (String, String) {
        let call = ToolCall {
            id: self.unique("call").into(),
            name: "web_search".into(),
            arguments: r#"{"query":"rust"}"#.into(),
        };
        let marker = self
            .markers
            .create_platform(
                &self.owner,
                PlatformMarkerInput {
                    tool_id: "web-search".into(),
                    call: call.clone(),
                    activity: "Searching the web".into(),
                    execution_limit: Duration::from_secs(30),
                },
            )
            .await
            .expect("create platform marker");
        let execution = self
            .markers
            .create_public_execution(
                &self.owner,
                &marker.reference,
                PublicExecutionInput {
                    source_run_id: None,
                    retention: execution_retention,
                    deadline_unix_ms: now_ms() + 60_000,
                },
            )
            .await
            .expect("create public execution");
        assert_eq!(
            self.markers
                .claim_execution(
                    &self.owner,
                    &marker.reference,
                    "worker",
                    Duration::from_secs(30)
                )
                .await
                .expect("claim execution"),
            ClaimOutcome::Claimed
        );
        self.markers
            .finish_execution(
                &self.owner,
                &marker.reference,
                "worker",
                PlatformExecutionState::Completed,
                HiddenHistorySegment::Platform {
                    call: call.clone(),
                    result: ContentBlock::ToolResult {
                        content_kind: Some(ToolResultContentKind::Json),
                        tool_use_id: call.id,
                        content: serde_json::json!({"answer": "stable"}),
                        is_error: Some(false),
                        cache_control: None,
                    },
                },
            )
            .await
            .expect("finish execution");
        (marker.reference, execution)
    }

    async fn execution_readable(&self, execution: &str) -> bool {
        self.markers
            .resolve_public_execution(&self.owner, execution)
            .await
            .expect("resolve execution")
            .is_some()
    }

    /// 思考型 Marker 的正文；不可读时为 `None`。
    async fn marker_text(&self, reference: &str) -> Option<String> {
        match self
            .markers
            .resolve(&self.owner, reference)
            .await
            .expect("resolve marker")?
            .segment?
        {
            HiddenHistorySegment::Thinking {
                block: ContentBlock::Thinking { thinking, .. },
                ..
            } => Some(thinking),
            other => panic!("unexpected segment {other:?}"),
        }
    }

    /// 节点链的载荷是否含有某段正文。
    async fn turn_mentions(&self, id: &str, needle: &str) -> bool {
        self.turns
            .materialize(&self.owner, TurnNodeKind::Response, &TurnNodeId::new(id))
            .await
            .is_ok_and(|nodes| {
                nodes
                    .iter()
                    .any(|node| node.payload.to_string().contains(needle))
            })
    }

    // --- Observation 写入 ---

    /// 经 Observation 领域入口写一段交互：来源 Run 的用户输入与可见输出块，
    /// 续接 Run 的客户端工具结果（均引用正文）。返回续接 Run 的 id。
    async fn observe(&self, input: &str, visible: &str, tool_output: &str) -> String {
        use crate::interaction_observation::{
            AdmissionFacts, IngressStart, RunEvent, RunOutcome, RunStart,
        };

        let observation = &self.maintenance.observation;
        let user = |text: &str| AiItem {
            role: Role::User,
            content: MessageContent::Text(Arc::new(text.to_owned())),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        };
        let admit = |id: &str, items: Vec<AiItem>, parent: Option<&str>, new_user: bool| {
            observation
                .observe_ingress(IngressStart {
                    id: id.into(),
                    method: "POST".into(),
                    path: "/v1/responses".into(),
                    protocol: "responses".into(),
                })
                .admit(
                    RunStart {
                        id: id.into(),
                        principal: self.owner.api_key_id().into(),
                        api_key_id: None,
                        api_key_name: None,
                        route_id: "route".into(),
                        model_display_name: None,
                        ingress_protocol: "responses".into(),
                    },
                    AdmissionFacts {
                        client_request: Arc::new(AiRequest::new("model", items)),
                        has_new_user: new_user,
                        has_matching_pending_tool_result: parent.is_some(),
                        generation_root_id: parent.map(str::to_owned),
                        generation_parent_id: parent.map(str::to_owned),
                    },
                )
        };
        let finish = |run: &crate::interaction_observation::RunObserver,
                      node: &str,
                      status: &str| {
            run.finish(RunOutcome {
                client_output_committed: true,
                delivery: None,
                delivery_completed_at: Some(now_ms()),
                status: status.into(),
                terminal_reason: None,
                generation_node_id: Some(node.to_owned()),
                generation_root_id: Some(node.to_owned()),
            });
        };

        let tool_id = self.unique("observed-call");
        let source_node = self.commit(None, &Refs::default()).await;
        let question = user(input);
        let call = AiItem::function_call(ToolCall {
            id: tool_id.clone().into(),
            name: "probe".into(),
            arguments: "{}".into(),
        });
        let source = admit(
            &self.unique("observed-source"),
            vec![question.clone()],
            None,
            true,
        );
        // 用户输入随 Turn 开始阶段一起落盘，所以来源 Run 要有完整的 Turn 与 attempt。
        source.record(RunEvent::ModelTurnStarted {
            retention_detached: false,
            model_turn_id: "observed-turn".into(),
            route_id: "route".into(),
            model_display_name: None,
            estimated_input_tokens: None,
        });
        source.capture_input_preview(std::slice::from_ref(&question));
        source.publish_input_preview();
        source.record(RunEvent::TargetAttemptStarted {
            model_turn_id: "observed-turn".into(),
            attempt_id: "observed-attempt".into(),
            target_id: "target".into(),
            provider_id: "provider".into(),
            provider_name: "Provider".into(),
            upstream_model: "model".into(),
            protocol: "responses".into(),
            upstream_url: "http://127.0.0.1".into(),
            cost_basis: None,
        });
        let block_id = self.unique("observed-block");
        source.record(RunEvent::ClientVisibleContent {
            text: visible.to_owned(),
            parts: vec![serde_json::json!({"type": "text", "text": visible})],
            block_id: block_id.clone(),
            item: serde_json::json!({"id": block_id, "role": "assistant", "content": visible}),
            complete: true,
        });
        source.record(RunEvent::TargetAttemptFinished {
            model_turn_id: "observed-turn".into(),
            attempt_id: "observed-attempt".into(),
            status: "completed".into(),
            status_code: Some(200),
            error_code: None,
            error: None,
            duration_ms: 10,
            first_token_ms: None,
            usage: None,
            amounts: None,
        });
        source.record(RunEvent::ModelTurnFinished {
            retention_detached: false,
            model_turn_id: "observed-turn".into(),
            status: "completed".into(),
        });
        source.record(RunEvent::ClientToolHandoff {
            tool_id: tool_id.clone(),
            name: "probe".into(),
            input: None,
        });
        source.observe_client_completion(
            std::slice::from_ref(&question),
            std::slice::from_ref(&call),
            now_ms(),
            true,
        );
        finish(&source, &source_node, "waiting_client");

        let items = vec![
            question,
            call,
            AiItem::function_call_output(tool_id.as_str(), serde_json::json!(tool_output)),
        ];
        let followup_id = self.unique("observed-followup");
        let followup = admit(&followup_id, items.clone(), Some(&source_node), false);
        followup.capture_client_tool_results(&items);
        followup.publish_input_preview();
        finish(&followup, &source_node, "completed");
        observation.flush().await.expect("observation flush");
        followup_id
    }

    /// 经公共查询读取：森林里是否还有交互。
    async fn observed_interactions(&self) -> usize {
        self.maintenance
            .observation
            .query_forest(crate::interaction_observation::ForestQuery::default())
            .await
            .expect("observation forest")
            .roots
            .iter()
            .map(|root| root.interactions.len())
            .sum()
    }

    // --- 不变量用的行数 ---

    async fn count(&self, table: &str) -> i64 {
        let sql = format!("SELECT COUNT(*) FROM {table}");
        match &self.db {
            Db::Sqlite(pool) => sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
                .fetch_one(pool)
                .await
                .expect("count rows"),
            Db::Postgres(pool) => sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
                .fetch_one(pool)
                .await
                .expect("count rows"),
        }
    }

    /// 全部回收队列表：按命名约定发现，新增队列自动纳入不变量。
    async fn queue_tables(&self) -> Vec<String> {
        match &self.db {
            Db::Sqlite(pool) => sqlx::query_scalar(
                "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE '%\\_gc' ESCAPE '\\' ORDER BY name",
            )
            .fetch_all(pool)
            .await
            .expect("list queues"),
            Db::Postgres(pool) => sqlx::query_scalar(
                "SELECT table_name::text FROM information_schema.tables \
                 WHERE table_schema = current_schema() AND table_name LIKE '%\\_gc' ORDER BY table_name",
            )
            .fetch_all(pool)
            .await
            .expect("list queues"),
        }
    }

    /// 某格式、某字节长度的正文行数；用唯一长度标识一份正文（沿用共享正文测试的做法）。
    async fn text_body_rows(&self, raw_len: usize) -> i64 {
        let raw_len = i64::try_from(raw_len).unwrap();
        let key = self.owner.continuation_key();
        match &self.db {
            Db::Sqlite(pool) => sqlx::query_scalar(
                "SELECT COUNT(*) FROM contents c JOIN content_scopes s ON s.id = c.scope_id \
                 WHERE s.principal = ? AND c.format = 0 AND c.raw_len = ?",
            )
            .bind(key)
            .bind(raw_len)
            .fetch_one(pool)
            .await
            .expect("content rows"),
            Db::Postgres(pool) => sqlx::query_scalar(
                "SELECT COUNT(*) FROM contents c JOIN content_scopes s ON s.id = c.scope_id \
                 WHERE s.principal = $1 AND c.format = 0 AND c.raw_len = $2",
            )
            .bind(key)
            .bind(raw_len)
            .fetch_one(pool)
            .await
            .expect("content rows"),
        }
    }
}

// ---------------------------------------------------------------------------
// 双后端运行器
// ---------------------------------------------------------------------------

async fn run_sqlite<F, Fut>(days: u32, scenario: F)
where
    F: FnOnce(Fixture) -> Fut,
    Fut: Future<Output = ()>,
{
    let pool = crate::test_support::migrated_sqlite_pool()
        .await
        .expect("SQLite test database");
    scenario(Fixture::new(Db::Sqlite(pool), days).await).await;
}

/// 在隔离 schema 中运行 PostgreSQL 场景，结束后无论成败都清理。
async fn run_postgres<F, Fut>(days: u32, scenario: F)
where
    F: FnOnce(Fixture) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let url = std::env::var("STRAVIA_TEST_POSTGRES_URL")
        .or_else(|_| std::env::var("DB_URL"))
        .or_else(|_| std::env::var("DATABASE_URL"))
        .expect("set STRAVIA_TEST_POSTGRES_URL to run PostgreSQL history retention scenarios");
    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("PostgreSQL admin pool");
    let schema = format!("history_retention_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .expect("create isolated schema");
    let options: sqlx::postgres::PgConnectOptions =
        url.parse().expect("PostgreSQL connection options");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect_with(options.options([("search_path", schema.as_str())]))
        .await
        .expect("isolated PostgreSQL pool");

    let scenario_pool = pool.clone();
    let result = tokio::spawn(async move {
        crate::migrations::migrate_postgres(&scenario_pool, None)
            .await
            .expect("PostgreSQL migrations");
        scenario(Fixture::new(Db::Postgres(scenario_pool), days).await).await;
    })
    .await;

    pool.close().await;
    let cleanup = sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await;
    admin.close().await;
    cleanup.expect("drop isolated schema");
    if let Err(error) = result {
        if error.is_panic() {
            std::panic::resume_unwind(error.into_panic());
        }
        panic!("PostgreSQL scenario task failed: {error}");
    }
}

/// 为一个场景生成 `sqlite_*` 与 `postgres_*`（`#[ignore]`）两个测试。
macro_rules! both_backends {
    ($sqlite:ident, $postgres:ident, $days:expr, $scenario:path) => {
        #[tokio::test(flavor = "multi_thread")]
        async fn $sqlite() {
            run_sqlite($days, $scenario).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "requires STRAVIA_TEST_POSTGRES_URL pointing to an isolated test PostgreSQL"]
        async fn $postgres() {
            run_postgres($days, $scenario).await;
        }
    };
}

// ---------------------------------------------------------------------------
// 保留期设置
// ---------------------------------------------------------------------------

#[test]
fn retention_days_are_clamped_to_the_documented_minimum() {
    use super::{DAY_MILLIS, DEFAULT_RETENTION_DAYS, MIN_RETENTION_DAYS};
    let retention = HistoryRetention::new(0);
    assert_eq!(retention.days(), MIN_RETENTION_DAYS);
    assert_eq!(retention.window_millis(), DAY_MILLIS);
    assert_eq!(retention.set_days(0), MIN_RETENTION_DAYS);
    assert_eq!(
        HistoryRetention::from_setting(None).days(),
        DEFAULT_RETENTION_DAYS
    );
    assert_eq!(
        HistoryRetention::from_setting(Some("not-a-number")).days(),
        DEFAULT_RETENTION_DAYS
    );
    assert_eq!(HistoryRetention::from_setting(Some("3")).days(), 3);
    assert_eq!(retention.deadline(10), 10 + DAY_MILLIS);
}

// ---------------------------------------------------------------------------
// 场景
// ---------------------------------------------------------------------------

/// 子树：最晚的 `created_at + N` 之前，节点及其祖先都可读、可续接；整棵子树到期后，
/// 一轮扫描自下而上删除。
async fn subtree_lives_until_its_latest_node_expires(fx: Fixture) {
    let n = fx.window();
    // 树 A：root2 下有较早的 x 与较晚的 y。
    let root2 = fx.commit(None, &Refs::default()).await;
    let x = fx.commit(Some(&root2), &Refs::default()).await;
    let t_x = fx.tick().await;
    let y = fx.commit(Some(&root2), &Refs::default()).await;
    let t_y = fx.tick().await;
    // 树 B：链 root -> a -> b，均晚于树 A。
    let root = fx.commit(None, &Refs::default()).await;
    let a = fx.commit(Some(&root), &Refs::default()).await;
    let t_a = fx.tick().await;
    let b = fx.commit(Some(&a), &Refs::default()).await;
    let t_b = fx.tick().await;
    assert!(
        t_x < t_y && t_y < t_a && t_a < t_b,
        "ticks order the writes"
    );

    // x 单独到期被回收；root2 仍被较晚的 y 保护，整条链 B 也都在期内。
    assert!(fx.sweep(t_x + n).await > 0);
    assert!(!fx.turn_readable(&x).await, "an expired leaf is reclaimed");
    assert!(fx.turn_readable(&y).await);
    assert!(
        fx.turn_readable(&root2).await,
        "an ancestor outlives its older child"
    );
    assert_eq!(fx.chain_len(&y).await, 2);
    assert_eq!(fx.chain_len(&b).await, 3);

    // 树 A 整棵到期而 b 未到期：root、a 自己的期限已过，仍随 b 可读、可续接。
    fx.sweep(t_a + n).await;
    assert!(!fx.turn_readable(&y).await);
    assert!(
        !fx.turn_readable(&root2).await,
        "the whole expired subtree is reclaimed"
    );
    assert_eq!(fx.chain_len(&b).await, 3);
    assert!(fx.can_continue(&b).await);
    assert!(fx.can_continue(&a).await);
    assert!(fx.turn_readable(&root).await);

    // 最晚节点 b 到期：一轮扫描自下而上删除整条链。
    fx.sweep(t_b + n).await;
    for id in [&b, &a, &root] {
        assert!(
            !fx.turn_readable(id).await,
            "{id} must be reclaimed bottom-up in one sweep"
        );
    }
}
both_backends!(
    sqlite_subtree_lives_until_its_latest_node_expires,
    postgres_subtree_lives_until_its_latest_node_expires,
    1,
    subtree_lives_until_its_latest_node_expires
);

/// 依赖：节点可读期间依赖都可读；节点删除后，各依赖在自己的下限窗口过去后的那一轮删除。
/// 下限早于节点期限的依赖随节点同一轮删除，下限更晚的依赖在后续一轮删除。
async fn dependencies_are_reclaimed_after_their_floor_window(fx: Fixture) {
    let n = fx.window();
    // 早依赖：先于节点发布，下限比节点期限更早。
    let early_marker = fx.thinking_marker("early dependency body", true).await;
    let early_projected = fx.thinking_marker("early projected body", true).await;
    let early_mapping = fx.mapping(true).await;
    let early_compaction = fx.native_compaction(None, &[], true).await;
    let t_dep = fx.tick().await;
    let early_node = fx
        .commit(
            None,
            &Refs {
                plain_markers: vec![early_marker.clone()],
                projected_markers: vec![early_projected.clone()],
                mappings: vec![early_mapping.clone()],
                compaction: Some(early_compaction.state.clone()),
                ..Refs::default()
            },
        )
        .await;
    let t_early = fx.tick().await;
    // 晚依赖：提交时未发布，节点之后才发布 / 确认，下限比节点期限更晚。
    let late_marker = fx.thinking_marker("late dependency body", false).await;
    let late_mapping = fx.mapping(false).await;
    let late_compaction = fx.native_compaction(None, &[], false).await;
    let late_node = fx
        .commit(
            None,
            &Refs {
                plain_markers: vec![late_marker.clone()],
                mappings: vec![late_mapping.clone()],
                compaction: Some(late_compaction.state.clone()),
                ..Refs::default()
            },
        )
        .await;
    let t_late = fx.tick().await;
    fx.publish_marker(&late_marker).await;
    fx.publish_mapping(&late_mapping).await;
    fx.confirm_compaction(&late_compaction.record).await;
    let t_published = fx.tick().await;

    // 节点仍可读：所有依赖下限都已过（含未发布的 1 小时窗口），但仍被引用，全部可读。
    fx.sweep(t_dep + n).await;
    assert!(fx.turn_readable(&early_node).await);
    assert!(fx.turn_readable(&late_node).await);
    for marker in [&early_marker, &early_projected, &late_marker] {
        assert!(fx.marker_readable(marker).await);
    }
    for mapping in [&early_mapping, &late_mapping] {
        assert!(fx.mapping_readable(mapping).await);
    }
    assert!(fx.compaction_readable(&early_compaction.state).await);
    assert!(fx.compaction_readable(&late_compaction.state).await);

    // 早节点到期：它和下限已过的早依赖同一轮删除；晚依赖尚未到下限，保留。
    fx.sweep(t_early + n).await;
    assert!(!fx.turn_readable(&early_node).await);
    for marker in [&early_marker, &early_projected] {
        assert!(
            !fx.marker_readable(marker).await,
            "{marker} is reclaimed with its node"
        );
    }
    assert!(!fx.mapping_readable(&early_mapping).await);
    assert!(!fx.compaction_readable(&early_compaction.state).await);
    assert!(fx.turn_readable(&late_node).await);
    assert!(fx.marker_readable(&late_marker).await);
    assert!(fx.mapping_readable(&late_mapping).await);
    assert!(fx.compaction_readable(&late_compaction.state).await);

    // 晚节点到期：依赖已不被引用，但下限（发布时刻 + N）未到，仍可读。
    fx.sweep(t_late + n).await;
    assert!(!fx.turn_readable(&late_node).await);
    assert!(fx.marker_readable(&late_marker).await);
    assert!(fx.mapping_readable(&late_mapping).await);
    assert!(fx.compaction_readable(&late_compaction.state).await);

    // 下限窗口过去后的那一轮删除。
    fx.sweep(t_published + n).await;
    assert!(!fx.marker_readable(&late_marker).await);
    assert!(!fx.mapping_readable(&late_mapping).await);
    assert!(!fx.compaction_readable(&late_compaction.state).await);
}
both_backends!(
    sqlite_dependencies_are_reclaimed_after_their_floor_window,
    postgres_dependencies_are_reclaimed_after_their_floor_window,
    1,
    dependencies_are_reclaimed_after_their_floor_window
);

/// 下限窗口：未发布依赖按 `pending_until` 回收，与 N 无关；已发布但无人引用的依赖
/// 在发布后 N 天内被新提交引用即保留，之后随引用方删除。
async fn floor_windows_are_independent_of_the_retention_days(fx: Fixture) {
    let n = fx.window();
    assert_eq!(n, 7 * DAY);

    // 未发布：1 小时窗口，N = 7 天时同样在 2 小时后回收；已发布对照仍保留。
    let unpublished_marker = fx.thinking_marker("unpublished marker body", false).await;
    let unpublished_mapping = fx.mapping(false).await;
    let unpublished_compaction = fx.native_compaction(None, &[], false).await;
    let published_marker = fx.thinking_marker("published control body", true).await;
    let t_created = fx.tick().await;
    fx.sweep(t_created + HOUR / 2).await;
    assert!(
        fx.marker_readable(&unpublished_marker).await,
        "inside the pending window"
    );
    assert!(fx.mapping_readable(&unpublished_mapping).await);
    assert!(fx.compaction_readable(&unpublished_compaction.state).await);
    fx.sweep(t_created + 2 * HOUR).await;
    assert!(!fx.marker_readable(&unpublished_marker).await);
    assert!(!fx.mapping_readable(&unpublished_mapping).await);
    assert!(!fx.compaction_readable(&unpublished_compaction.state).await);
    assert!(
        fx.marker_readable(&published_marker).await,
        "published floor is publish time + N"
    );
    fx.sweep(t_created + n).await;
    assert!(!fx.marker_readable(&published_marker).await);

    // 已发布但无人引用：窗口内被新提交引用即保留，其余的到点删除。
    let rescued_marker = fx.thinking_marker("rescued marker body", true).await;
    let rescued_mapping = fx.mapping(true).await;
    let lost_marker = fx.thinking_marker("lost marker body", true).await;
    let lost_mapping = fx.mapping(true).await;
    let t_published = fx.tick().await;
    fx.sweep(t_published + n - HOUR).await;
    for marker in [&rescued_marker, &lost_marker] {
        assert!(
            fx.marker_readable(marker).await,
            "before the floor nothing is reclaimed"
        );
    }
    let node = fx
        .commit(
            None,
            &Refs {
                plain_markers: vec![rescued_marker.clone()],
                mappings: vec![rescued_mapping.clone()],
                ..Refs::default()
            },
        )
        .await;
    let t_node = fx.tick().await;
    fx.sweep(t_published + n).await;
    assert!(fx.turn_readable(&node).await);
    assert!(
        fx.marker_readable(&rescued_marker).await,
        "a new commit brought it back"
    );
    assert!(fx.mapping_readable(&rescued_mapping).await);
    assert!(!fx.marker_readable(&lost_marker).await);
    assert!(!fx.mapping_readable(&lost_mapping).await);

    // 引用方到期：下限早已过，依赖随它同一轮删除。
    fx.sweep(t_node + n).await;
    assert!(!fx.turn_readable(&node).await);
    assert!(!fx.marker_readable(&rescued_marker).await);
    assert!(!fx.mapping_readable(&rescued_mapping).await);
}
both_backends!(
    sqlite_floor_windows_are_independent_of_the_retention_days,
    postgres_floor_windows_are_independent_of_the_retention_days,
    7,
    floor_windows_are_independent_of_the_retention_days
);

/// 已过期但尚未被扫描删除的节点与依赖：被续接或被新节点引用即保留。
async fn expired_but_unswept_objects_are_kept_when_used_again(fx: Fixture) {
    let n = fx.window();
    let parent = fx.commit(None, &Refs::default()).await;
    let dependency = fx
        .thinking_marker("expired but unswept dependency", true)
        .await;
    let unused = fx.thinking_marker("expired and unused", true).await;
    let t_parent = fx.tick().await;
    // 虚拟时间早已越过期限，但还没有扫描：仍可续接，并可被新节点引用。
    assert!(fx.can_continue(&parent).await);
    let child = fx.commit(Some(&parent), &Refs::default()).await;
    let referrer = fx.commit(None, &Refs::marker(&dependency)).await;
    let t_used = fx.tick().await;

    fx.sweep(t_parent + n).await;
    assert!(
        fx.turn_readable(&parent).await,
        "an expired parent is kept by its new child"
    );
    assert_eq!(fx.chain_len(&child).await, 2);
    assert!(fx.turn_readable(&referrer).await);
    assert!(
        fx.marker_readable(&dependency).await,
        "referenced by a new node"
    );
    assert!(
        !fx.marker_readable(&unused).await,
        "control: unreferenced and past its floor"
    );

    fx.sweep(t_used + n).await;
    for id in [&child, &parent, &referrer] {
        assert!(!fx.turn_readable(id).await);
    }
    assert!(!fx.marker_readable(&dependency).await);
}
both_backends!(
    sqlite_expired_but_unswept_objects_are_kept_when_used_again,
    postgres_expired_but_unswept_objects_are_kept_when_used_again,
    1,
    expired_but_unswept_objects_are_kept_when_used_again
);

/// N 缩小一轮删除全部新到期对象；N 变大或 `now` 回退再前进，既不漏删也不多删。
async fn retention_changes_and_clock_moves_neither_miss_nor_over_delete(fx: Fixture) {
    // 缩小：N = 7 天时都未到期，改为 1 天后同一轮全部删除（含各自已发布的 Marker）。
    let mut nodes = Vec::new();
    let mut markers = Vec::new();
    for index in 0..3 {
        let marker = fx
            .thinking_marker(&format!("shrink body {index}"), true)
            .await;
        nodes.push(fx.commit(None, &Refs::marker(&marker)).await);
        markers.push(marker);
        fx.tick().await;
    }
    let t_all = fx.tick().await;
    fx.sweep(t_all + 3 * DAY).await;
    for id in &nodes {
        assert!(
            fx.turn_readable(id).await,
            "N = 7 days: nothing is eligible yet"
        );
    }
    fx.retention.set_days(1);
    assert!(fx.sweep(t_all + 3 * DAY).await > 0);
    for id in &nodes {
        assert!(
            !fx.turn_readable(id).await,
            "shrinking N reclaims everything in one sweep"
        );
    }
    for marker in &markers {
        assert!(!fx.marker_readable(marker).await);
    }

    // 变大：已删除的不会回来，尚在窗口内的对象不会被误删，到新期限才删除。
    // 上一轮把 `now` 推到了未来；新写入的对象发生在真实“现在”，先让 `now` 回到现在。
    fx.sweep(now_ms()).await;
    let first_marker = fx.thinking_marker("grow body first", true).await;
    let first = fx.commit(None, &Refs::marker(&first_marker)).await;
    let t_first = fx.tick().await;
    let second_marker = fx.thinking_marker("grow body second", true).await;
    let second = fx.commit(None, &Refs::marker(&second_marker)).await;
    let t_second = fx.tick().await;
    fx.sweep(t_first + DAY).await;
    assert!(!fx.turn_readable(&first).await);
    assert!(!fx.marker_readable(&first_marker).await);
    assert!(fx.turn_readable(&second).await);
    assert!(fx.marker_readable(&second_marker).await);
    fx.retention.set_days(3);
    fx.sweep(t_second + DAY).await;
    assert!(
        fx.turn_readable(&second).await,
        "growing N must not over-delete"
    );
    assert!(fx.marker_readable(&second_marker).await);
    fx.sweep(t_second + 3 * DAY).await;
    assert!(!fx.turn_readable(&second).await);
    assert!(!fx.marker_readable(&second_marker).await);

    // `now` 先跳到远未来，再回到当前，之后新写入的对象仍会在自己的期限被删除。
    fx.retention.set_days(1);
    fx.sweep(now_ms() + 30 * DAY).await;
    fx.sweep(now_ms()).await;
    let late_mapping = fx.mapping(true).await;
    let late_node = fx.commit(None, &Refs::mapping(&late_mapping)).await;
    let t_late = fx.tick().await;
    fx.sweep(t_late + DAY).await;
    assert!(
        !fx.turn_readable(&late_node).await,
        "no node is missed after the clock went back"
    );
    assert!(!fx.mapping_readable(&late_mapping).await);
}
both_backends!(
    sqlite_retention_changes_and_clock_moves_neither_miss_nor_over_delete,
    postgres_retention_changes_and_clock_moves_neither_miss_nor_over_delete,
    7,
    retention_changes_and_clock_moves_neither_miss_nor_over_delete
);

/// 两个节点引用同一依赖：只有最后一个引用消失后依赖才可回收。
async fn only_the_last_reference_removal_releases_a_dependency(fx: Fixture) {
    let n = fx.window();
    let marker = fx.thinking_marker("shared dependency body", true).await;
    let mapping = fx.mapping(true).await;
    let both = Refs {
        plain_markers: vec![marker.clone()],
        mappings: vec![mapping.clone()],
        ..Refs::default()
    };
    let first = fx.commit(None, &both).await;
    let t_first = fx.tick().await;
    let second = fx.commit(None, &both).await;
    let t_second = fx.tick().await;

    fx.sweep(t_first + n).await;
    assert!(!fx.turn_readable(&first).await);
    assert!(fx.turn_readable(&second).await);
    assert!(
        fx.marker_readable(&marker).await,
        "still referenced by the second node"
    );
    assert!(fx.mapping_readable(&mapping).await);

    fx.sweep(t_second + n).await;
    assert!(!fx.turn_readable(&second).await);
    assert!(
        !fx.marker_readable(&marker).await,
        "released with the last reference"
    );
    assert!(!fx.mapping_readable(&mapping).await);
}
both_backends!(
    sqlite_only_the_last_reference_removal_releases_a_dependency,
    postgres_only_the_last_reference_removal_releases_a_dependency,
    1,
    only_the_last_reference_removal_releases_a_dependency
);

/// 带平台工具执行记录的 Marker 在执行记录到期前不回收；执行记录到期后读取被拒绝。
async fn marker_with_a_tool_execution_outlives_its_publication_floor(fx: Fixture) {
    let n = fx.window();
    let execution_retention = Duration::from_millis(u64::try_from(3 * DAY).unwrap());
    let (marker, execution) = fx.executed_platform_marker(execution_retention).await;
    let control = fx.thinking_marker("control without execution", true).await;
    fx.publish_marker(&marker).await;
    let t_published = fx.tick().await;

    // 发布下限（published_at + N）已过：没有执行记录的对照被回收，带执行记录的不回收。
    fx.sweep(t_published + n).await;
    assert!(!fx.marker_readable(&control).await);
    assert!(fx.marker_readable(&marker).await);
    assert!(fx.execution_readable(&execution).await);
    fx.sweep(t_published + 2 * DAY).await;
    assert!(
        fx.marker_readable(&marker).await,
        "execution retention is independent of N"
    );

    // 执行记录到期：同一轮删除执行记录与失去它的 Marker，读取被拒绝。
    fx.sweep(t_published + 3 * DAY).await;
    assert!(!fx.execution_readable(&execution).await);
    assert!(!fx.marker_readable(&marker).await);
}
both_backends!(
    sqlite_marker_with_a_tool_execution_outlives_its_publication_floor,
    postgres_marker_with_a_tool_execution_outlives_its_publication_floor,
    1,
    marker_with_a_tool_execution_outlives_its_publication_floor
);

/// 原生压缩：登记被引用期间，来源节点与来源登记可解析；登记删除后，来源在随后一轮回收。
async fn native_compaction_sources_follow_their_record(fx: Fixture) {
    let n = fx.window();
    let source = fx.commit(None, &Refs::default()).await;
    let first = fx.native_compaction(None, &[], true).await;
    let second = fx
        .native_compaction(Some(&source), &[first.record.id.as_str()], true)
        .await;
    let t_registered = fx.tick().await;
    let referrer = fx
        .commit(
            None,
            &Refs {
                compaction: Some(second.state.clone()),
                ..Refs::default()
            },
        )
        .await;
    let t_referrer = fx.tick().await;

    // 引用方在期内：来源节点、来源登记都可解析。
    fx.sweep(t_registered + n).await;
    assert!(fx.turn_readable(&referrer).await);
    assert!(
        fx.turn_readable(&source).await,
        "the source node is resolvable"
    );
    assert!(fx.compaction_readable(&second.state).await);
    assert!(
        fx.compaction_readable(&first.state).await,
        "the source record is resolvable"
    );

    // 引用方到期：登记随之删除，来源在随后一轮回收。
    fx.sweep(t_referrer + n).await;
    assert!(!fx.turn_readable(&referrer).await);
    assert!(!fx.compaction_readable(&second.state).await);
    fx.sweep(t_referrer + n).await;
    assert!(
        !fx.turn_readable(&source).await,
        "sources are reclaimed in the next sweep"
    );
    assert!(!fx.compaction_readable(&first.state).await);
}
both_backends!(
    sqlite_native_compaction_sources_follow_their_record,
    postgres_native_compaction_sources_follow_their_record,
    1,
    native_compaction_sources_follow_their_record
);

/// Turn 与 Marker 共用一份正文：删除其中一方，另一方仍可读；最后一个引用方消失后，
/// 正文由唯一扫描入口回收。
async fn shared_content_lives_until_its_last_referrer_is_gone(fx: Fixture) {
    let n = fx.window();
    // Turn 先到期：Marker 在其后发布，下限更晚。
    let turn_first = "turn expires first body ".repeat(30);
    let node = fx.commit(None, &Refs::answer(&turn_first)).await;
    let t_node = fx.tick().await;
    let marker = fx.thinking_marker(&turn_first, true).await;
    let t_marker = fx.tick().await;
    assert_eq!(
        fx.text_body_rows(turn_first.len()).await,
        1,
        "one shared body row"
    );
    fx.sweep(t_node + n).await;
    assert!(!fx.turn_readable(&node).await);
    assert_eq!(
        fx.marker_text(&marker).await.as_deref(),
        Some(turn_first.as_str())
    );
    assert_eq!(fx.text_body_rows(turn_first.len()).await, 1);
    fx.sweep(t_marker + n).await;
    assert!(fx.marker_text(&marker).await.is_none());
    assert_eq!(
        fx.text_body_rows(turn_first.len()).await,
        0,
        "last referrer removed"
    );

    // Marker 先到期：Turn 仍能读到完整正文。
    let marker_first = "marker expires first body ".repeat(29);
    let marker = fx.thinking_marker(&marker_first, true).await;
    let t_marker = fx.tick().await;
    let node = fx.commit(None, &Refs::answer(&marker_first)).await;
    let t_node = fx.tick().await;
    assert_eq!(fx.text_body_rows(marker_first.len()).await, 1);
    fx.sweep(t_marker + n).await;
    assert!(fx.marker_text(&marker).await.is_none());
    assert!(fx.turn_mentions(&node, &marker_first).await);
    assert_eq!(fx.text_body_rows(marker_first.len()).await, 1);
    fx.sweep(t_node + n).await;
    assert!(!fx.turn_readable(&node).await);
    assert_eq!(fx.text_body_rows(marker_first.len()).await, 0);
}
both_backends!(
    sqlite_shared_content_lives_until_its_last_referrer_is_gone,
    postgres_shared_content_lives_until_its_last_referrer_is_gone,
    1,
    shared_content_lives_until_its_last_referrer_is_gone
);

/// Observation 引用的正文与 Turn 共享：Turn 先过期时正文留给仍在期内的交互，
/// 交互也被清理后正文随之回收；可见输出与工具结果正文随交互一并回收。
async fn observation_content_lives_until_its_interaction_is_swept(fx: Fixture) {
    let n = fx.window();
    let shared = "observation shared body ".repeat(30);
    let visible = "observation visible body ".repeat(31);
    let tool_output = "observation tool output ".repeat(27);
    let node = fx.commit(None, &Refs::answer(&shared)).await;
    let t_node = fx.tick().await;
    fx.observe(&shared, &visible, &tool_output).await;
    let t_observed = fx.tick().await;
    assert!(fx.observed_interactions().await > 0, "interaction is readable");
    assert_eq!(fx.text_body_rows(shared.len()).await, 1, "one shared body row");
    assert_eq!(fx.text_body_rows(visible.len()).await, 1);
    assert_eq!(fx.text_body_rows(tool_output.len()).await, 1);

    fx.sweep_to_completion(t_node + n).await;
    assert!(!fx.turn_readable(&node).await);
    assert!(
        fx.observed_interactions().await > 0,
        "the interaction was active after the node was written"
    );
    assert_eq!(
        fx.text_body_rows(shared.len()).await,
        1,
        "the observed input still references the shared body"
    );

    fx.sweep_to_completion(t_observed + n).await;
    assert_eq!(fx.observed_interactions().await, 0);
    for body in [&shared, &visible, &tool_output] {
        assert_eq!(fx.text_body_rows(body.len()).await, 0, "last referrer removed");
    }
}
both_backends!(
    sqlite_observation_content_lives_until_its_interaction_is_swept,
    postgres_observation_content_lives_until_its_interaction_is_swept,
    1,
    observation_content_lives_until_its_interaction_is_swept
);

/// 不变量：`now` 越过所有下限并扫完后，节点、Marker、映射、压缩登记、正文、正文范围
/// 与所有回收队列都为空。任何一种引用关系缺少释放触发器，都会在这里留下残余。
async fn nothing_is_left_behind_once_every_floor_has_passed(fx: Fixture) {
    let shared = "invariant shared body ".repeat(28);
    let shared_marker = fx.thinking_marker(&shared, true).await;
    let plain_marker = fx
        .thinking_marker("invariant plain marker body", true)
        .await;
    let projected_marker = fx
        .thinking_marker("invariant projected marker body", true)
        .await;
    let published_mapping = fx.mapping(true).await;
    let pending_mapping = fx.mapping(false).await;

    // 原生压缩链：来源节点 -> 登记 1 -> 登记 2 -> 引用登记 2 的节点。
    let source = fx.commit(None, &Refs::answer(&shared)).await;
    let first = fx.native_compaction(None, &[], true).await;
    let second = fx
        .native_compaction(Some(&source), &[first.record.id.as_str()], true)
        .await;
    fx.native_compaction(None, &[], false).await; // 无人引用且未确认。

    // 带执行记录的平台 Marker（已发布）与从未发布的 Marker。必须在提前扫描之前建好：
    // 扫描会把未发布窗口的水位推进到“未来”，之后才创建的未发布对象会落在水位之后。
    let (executed, _) = fx
        .executed_platform_marker(Duration::from_millis(u64::try_from(2 * DAY).unwrap()))
        .await;
    fx.publish_marker(&executed).await;
    fx.thinking_marker("invariant unpublished marker body", false)
        .await;
    let t_deps = fx.tick().await;

    // 节点树：两个子节点引用同一批依赖，另有只引用压缩登记的节点。
    let root = fx
        .commit(
            None,
            &Refs {
                plain_markers: vec![plain_marker.clone()],
                mappings: vec![published_mapping.clone()],
                ..Refs::default()
            },
        )
        .await;
    fx.commit(
        Some(&root),
        &Refs {
            projected_markers: vec![projected_marker.clone()],
            mappings: vec![published_mapping.clone(), pending_mapping.clone()],
            ..Refs::default()
        },
    )
    .await;
    fx.commit(
        Some(&root),
        &Refs {
            plain_markers: vec![plain_marker.clone(), shared_marker.clone()],
            ..Refs::default()
        },
    )
    .await;
    fx.commit(
        None,
        &Refs {
            compaction: Some(second.state.clone()),
            ..Refs::default()
        },
    )
    .await;

    // 依赖已越过自己的下限而节点仍在期内：先扫一轮，让依赖的水位越过它们；
    // 之后它们只能靠引用释放触发器重新入队，缺任何一条都会在最后留下残余。
    fx.sweep(t_deps + fx.window()).await;

    // Observation 的交互与正文：用户输入与 Turn/Marker 共用一份正文，另有可见输出与工具结果。
    // 在提前扫描之后才写入，保证它们在最终扫描前一直在期内。
    fx.observe(
        &shared,
        "invariant observed visible body",
        "invariant observed tool output",
    )
    .await;

    let t_end = fx.tick().await;

    for table in [
        "turn_nodes",
        "history_markers",
        "reversible_redaction_mappings",
        "native_compactions",
        "contents",
        "interaction_observations",
        "inference_run_observations",
        "observation_events",
        "observation_blocks",
        "observation_event_contents",
        "observation_tool_results",
    ] {
        assert!(
            fx.count(table).await > 0,
            "{table} is populated before the sweep"
        );
    }
    fx.sweep_to_completion(t_end + 400 * DAY).await;

    let mut tables: Vec<String> = [
        "turn_nodes",
        "turn_node_payloads",
        "turn_node_contents",
        "turn_node_retention",
        "turn_node_markers",
        "turn_node_redaction_mappings",
        "turn_node_compactions",
        "history_markers",
        "history_marker_contents",
        "reversible_redaction_mappings",
        "native_compactions",
        "native_compaction_sources",
        "native_compaction_states",
        "public_tool_executions",
        "interaction_observations",
        "inference_run_observations",
        "rejected_request_observations",
        "observation_events",
        "observation_blocks",
        "observation_event_contents",
        "observation_tool_results",
        "contents",
        "content_scopes",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    tables.extend(fx.queue_tables().await);
    assert!(
        tables.iter().any(|table| table == "turn_node_gc"),
        "queue discovery finds the reclaim queues: {tables:?}"
    );
    for table in tables {
        assert_eq!(fx.count(&table).await, 0, "{table} must be empty");
    }
}
both_backends!(
    sqlite_nothing_is_left_behind_once_every_floor_has_passed,
    postgres_nothing_is_left_behind_once_every_floor_has_passed,
    1,
    nothing_is_left_behind_once_every_floor_has_passed
);

/// 并发：提交引用一个已过下限的依赖，同时回收该依赖。结果只能是“提交成功且依赖保留”
/// 或“提交以 DependencyUnavailable 失败”，回收本身不得出错。
async fn commit_and_reclaim_of_the_same_dependency_race_safely(fx: Fixture) {
    let n = fx.window();
    let fx = Arc::new(fx);
    let mut committed = 0;
    let mut rejected = 0;
    for round in 0..24 {
        let mapping = fx.mapping(true).await;
        let t_published = fx.tick().await;
        let refs = Refs::mapping(&mapping);
        let (commit, sweep) = tokio::join!(
            fx.try_commit(None, &refs),
            fx.maintenance.sweep_history(t_published + n)
        );
        sweep.unwrap_or_else(|error| panic!("round {round}: reclaim must not fail: {error:#}"));
        match commit {
            Ok(node) => {
                committed += 1;
                assert!(fx.turn_readable(&node).await, "round {round}");
                assert!(
                    fx.mapping_readable(&mapping).await,
                    "round {round}: a committed node keeps its dependency"
                );
            }
            Err(failure) => {
                rejected += 1;
                assert!(
                    is_dependency_unavailable(&failure),
                    "round {round}: unexpected failure {failure:?}"
                );
                assert!(!fx.mapping_readable(&mapping).await, "round {round}");
            }
        }
    }
    assert_eq!(committed + rejected, 24);
}
both_backends!(
    sqlite_commit_and_reclaim_of_the_same_dependency_race_safely,
    postgres_commit_and_reclaim_of_the_same_dependency_race_safely,
    1,
    commit_and_reclaim_of_the_same_dependency_race_safely
);

/// 引用已被回收的受信依赖时提交被拒绝（`DependencyUnavailable`），不留下节点。
async fn commit_rejects_a_dependency_that_was_already_reclaimed(fx: Fixture) {
    let mapping = fx.mapping(true).await;
    let t_published = fx.tick().await;
    fx.sweep(t_published + fx.window()).await;
    assert!(!fx.mapping_readable(&mapping).await);
    let failure = fx
        .try_commit(None, &Refs::mapping(&mapping))
        .await
        .expect_err("the reclaimed mapping cannot be referenced");
    assert!(is_dependency_unavailable(&failure), "{failure:?}");
}
both_backends!(
    sqlite_commit_rejects_a_dependency_that_was_already_reclaimed,
    postgres_commit_rejects_a_dependency_that_was_already_reclaimed,
    1,
    commit_rejects_a_dependency_that_was_already_reclaimed
);

mod observation_sharing;
