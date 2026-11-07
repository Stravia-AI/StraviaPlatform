//! 共享正文存储的跨引用方行为：Turn Chain 与 History Marker 经同一份正文行读写，
//! 删除一个引用方不影响另一个，最后一个引用消失后由回收清走。
//!
//! 只通过领域入口写入与读取；时间只通过 `sweep_history(now)` / `sweep(now, ..)`
//! 的 `now` 参数推进，不修改任何时间戳列。

use std::time::Duration;

use sqlx::{PgPool, SqlitePool};
use stravia_runtime_contract::Principal;
use stravia_runtime_contract::protocol::ir::{AiItem, ContentBlock};
use stravia_runtime_contract::turn_chain::{TurnChainStore, TurnCommit, TurnNodeId, TurnNodeKind};

use crate::history_marker::{
    HiddenHistorySegment, HistoryMarkerStore, SqlHistoryMarkerStore, ThinkingMarkerInput,
    render_text_projection_span,
};
use crate::history_retention::HistoryRetention;
use crate::turn_chain::SqlTurnChainStore;

#[derive(Clone)]
enum Backend {
    Sqlite(SqlitePool),
    Postgres(PgPool),
}

struct Stores {
    backend: Backend,
    turns: SqlTurnChainStore,
    markers: SqlHistoryMarkerStore,
}

/// Turn 节点保留 1 天；Marker 发布后保留 7 天，未发布窗口固定 1 小时。
const TURN_RETENTION_DAYS: u32 = 1;
const MARKER_RETENTION: Duration = Duration::from_secs(7 * 24 * 3600);
const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 24 * HOUR_MS;

impl Stores {
    fn sqlite(pool: SqlitePool) -> Self {
        Self {
            backend: Backend::Sqlite(pool.clone()),
            turns: SqlTurnChainStore::sqlite(
                pool.clone(),
                HistoryRetention::new(TURN_RETENTION_DAYS),
            ),
            markers: SqlHistoryMarkerStore::sqlite(pool),
        }
    }

    fn postgres(pool: PgPool) -> Self {
        Self {
            backend: Backend::Postgres(pool.clone()),
            turns: SqlTurnChainStore::postgres(
                pool.clone(),
                HistoryRetention::new(TURN_RETENTION_DAYS),
            ),
            markers: SqlHistoryMarkerStore::postgres(pool),
        }
    }

    /// 某 Principal 下指定格式、指定字节长度的正文行数；用唯一长度的正文标识一份正文。
    async fn rows(&self, principal: &Principal, format: i64, raw_len: usize) -> i64 {
        let key = principal.continuation_key();
        let raw_len = i64::try_from(raw_len).unwrap();
        match &self.backend {
            Backend::Sqlite(pool) => sqlx::query_scalar(
                "SELECT COUNT(*) FROM contents c JOIN content_scopes s ON s.id = c.scope_id \
                 WHERE s.principal = ? AND c.format = ? AND c.raw_len = ?",
            )
            .bind(key)
            .bind(format)
            .bind(raw_len)
            .fetch_one(pool)
            .await
            .unwrap(),
            Backend::Postgres(pool) => sqlx::query_scalar(
                "SELECT COUNT(*) FROM contents c JOIN content_scopes s ON s.id = c.scope_id \
                 WHERE s.principal = $1 AND c.format = $2 AND c.raw_len = $3",
            )
            .bind(key)
            .bind(format)
            .bind(raw_len)
            .fetch_one(pool)
            .await
            .unwrap(),
        }
    }

    /// 在“现在 + offset”这一时刻跑一轮：Turn 节点、Marker，最后是共享正文。
    async fn sweep_after(&self, offset_ms: i64) {
        let now = chrono::Utc::now().timestamp_millis() + offset_ms;
        self.turns.sweep_history(now).await.expect("sweep turns");
        self.markers
            .sweep(now, MARKER_RETENTION)
            .await
            .expect("sweep markers");
        match &self.backend {
            Backend::Sqlite(pool) => crate::content_store::sweep_sqlite(pool).await,
            Backend::Postgres(pool) => crate::content_store::sweep_postgres(pool).await,
        }
        .expect("sweep content");
    }

    async fn commit_text(&self, owner: &Principal, text: &str) -> TurnNodeId {
        let id = TurnNodeId::response();
        let item = serde_json::to_value(AiItem::output_text(text)).unwrap();
        self.turns
            .commit(TurnCommit {
                atomic_checkpoint_id: None,
                id: id.clone(),
                kind: TurnNodeKind::Response,
                parent_id: None,
                principal: owner.clone(),
                payload_version: 6,
                payload: serde_json::json!({ "items": [item] }),
                marker_references: Vec::new(),
                redaction_references: Vec::new(),
                compaction_record_ids: Vec::new(),
                reusable_prefix: None,
            })
            .await
            .expect("commit turn");
        id
    }

    async fn read_turn_item(
        &self,
        owner: &Principal,
        id: &TurnNodeId,
    ) -> Option<serde_json::Value> {
        self.turns
            .materialize(owner, TurnNodeKind::Response, id)
            .await
            .ok()
            .map(|nodes| nodes[0].payload["items"][0].clone())
    }

    async fn create_marker(&self, owner: &Principal, text: &str) -> String {
        self.markers
            .create_thinking(
                owner,
                ThinkingMarkerInput {
                    block: ContentBlock::Thinking {
                        thinking: text.to_owned(),
                        signature: Some("signature".into()),
                    },
                    source: None,
                    activity: "Preserving protected reasoning".into(),
                },
            )
            .await
            .expect("create marker")
            .reference
    }

    /// 发布 Marker：此后下限变为首次发布时刻 + N。
    async fn publish_marker(&self, owner: &Principal, reference: &str) {
        self.markers
            .publish(owner, std::slice::from_ref(&reference.to_owned()))
            .await
            .expect("publish marker");
    }

    async fn read_marker_text(&self, owner: &Principal, reference: &str) -> Option<String> {
        let resolved = self.markers.resolve(owner, reference).await.unwrap()?;
        match resolved.segment? {
            HiddenHistorySegment::Thinking {
                block:
                    ContentBlock::Thinking {
                        thinking,
                        signature,
                    },
                ..
            } => {
                assert_eq!(signature.as_deref(), Some("signature"));
                Some(thinking)
            }
            other => panic!("unexpected segment {other:?}"),
        }
    }
}

const TEXT: i64 = 0;

async fn shared_body_lives_until_the_last_referrer_is_gone(stores: Stores, label: &str) {
    let owner = Principal::new(format!("shared-body-{label}"));

    // 同一份正文被 Turn Chain 与已发布 Marker 共用：只有一行。
    let shared = "shared body A ".repeat(40);
    let turn_a = stores.commit_text(&owner, &shared).await;
    let marker_a = stores.create_marker(&owner, &shared).await;
    stores.publish_marker(&owner, &marker_a).await;
    assert_eq!(stores.rows(&owner, TEXT, shared.len()).await, 1);

    // 未发布 Marker 越过 1 小时窗口被回收后，Turn Chain 仍可完整读取同一份正文。
    let marker_gone = stores.create_marker(&owner, &shared).await;
    // 只留 Marker：稍后 Turn 节点到期被回收，已发布 Marker 仍可读。
    let only_marker = "marker survivor body ".repeat(31);
    let expired_turn = stores.commit_text(&owner, &only_marker).await;
    let survivor = stores.create_marker(&owner, &only_marker).await;
    stores.publish_marker(&owner, &survivor).await;
    // 最后一个引用方消失：正文由回收清走。
    let last = "last referrer body ".repeat(23);
    let last_turn = stores.commit_text(&owner, &last).await;
    let last_marker = stores.create_marker(&owner, &last).await;
    assert_eq!(stores.rows(&owner, TEXT, last.len()).await, 1);

    stores.sweep_after(2 * HOUR_MS).await;
    assert!(
        stores
            .read_marker_text(&owner, &marker_gone)
            .await
            .is_none()
    );
    assert!(
        stores
            .read_marker_text(&owner, &last_marker)
            .await
            .is_none()
    );
    assert_eq!(stores.rows(&owner, TEXT, shared.len()).await, 1);
    let item = stores
        .read_turn_item(&owner, &turn_a)
        .await
        .expect("turn A");
    assert_eq!(
        item,
        serde_json::to_value(AiItem::output_text(&shared)).unwrap()
    );
    assert_eq!(
        stores.read_marker_text(&owner, &marker_a).await.as_deref(),
        Some(shared.as_str())
    );
    assert!(stores.read_turn_item(&owner, &last_turn).await.is_some());

    stores.sweep_after(2 * DAY_MS).await;
    assert!(stores.read_turn_item(&owner, &expired_turn).await.is_none());
    assert_eq!(
        stores.read_marker_text(&owner, &survivor).await.as_deref(),
        Some(only_marker.as_str())
    );
    assert_eq!(stores.rows(&owner, TEXT, only_marker.len()).await, 1);
    assert!(stores.read_turn_item(&owner, &last_turn).await.is_none());
    assert_eq!(stores.rows(&owner, TEXT, last.len()).await, 0);
}

async fn projection_text_round_trips_byte_for_byte(stores: Stores, label: &str) {
    let owner = Principal::new(format!("projection-{label}"));
    let reference = crate::history_marker::syntax::new_reference();
    let body = "projected body ".repeat(17);
    let projected = format!(
        "lead\n{}\ntail",
        render_text_projection_span(&reference, 0, &body)
    );

    let projected_turn = stores.commit_text(&owner, &projected).await;
    // 同一正文以普通文本出现在另一个节点：共享同一行，投影字面量不进入正文行。
    let plain_turn = stores.commit_text(&owner, &body).await;

    assert_eq!(
        stores
            .read_turn_item(&owner, &projected_turn)
            .await
            .unwrap(),
        serde_json::to_value(AiItem::output_text(&projected)).unwrap()
    );
    assert_eq!(
        stores.read_turn_item(&owner, &plain_turn).await.unwrap(),
        serde_json::to_value(AiItem::output_text(&body)).unwrap()
    );
    assert_eq!(stores.rows(&owner, TEXT, body.len()).await, 1);
    assert_eq!(stores.rows(&owner, TEXT, projected.len()).await, 0);
}

#[tokio::test]
async fn sqlite_shared_body_lives_until_the_last_referrer_is_gone() {
    let pool = crate::test_support::migrated_sqlite_pool().await.unwrap();
    shared_body_lives_until_the_last_referrer_is_gone(Stores::sqlite(pool), "sqlite").await;
}

#[tokio::test]
async fn sqlite_projection_text_round_trips_byte_for_byte() {
    let pool = crate::test_support::migrated_sqlite_pool().await.unwrap();
    projection_text_round_trips_byte_for_byte(Stores::sqlite(pool), "sqlite").await;
}

/// 在隔离 schema 中运行 PostgreSQL 场景，结束后无论成败都清理。
async fn with_postgres<F, Fut>(scenario: F)
where
    F: FnOnce(PgPool) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let url = std::env::var("STRAVIA_TEST_POSTGRES_URL")
        .expect("set STRAVIA_TEST_POSTGRES_URL to run PostgreSQL content store scenarios");
    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("PostgreSQL admin pool");
    let schema = format!("content_store_test_{}", uuid::Uuid::new_v4().simple());
    let options: sqlx::postgres::PgConnectOptions =
        url.parse().expect("PostgreSQL connection options");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect_with(options.options([("search_path", schema.as_str())]))
        .await
        .expect("isolated PostgreSQL pool");
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .expect("create isolated PostgreSQL schema");

    let scenario_pool = pool.clone();
    let result = tokio::spawn(async move {
        crate::migrations::migrate_postgres(&scenario_pool, None)
            .await
            .expect("PostgreSQL migrations");
        scenario(scenario_pool).await;
    })
    .await;

    pool.close().await;
    let cleanup = sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await;
    admin.close().await;
    cleanup.expect("drop isolated PostgreSQL schema");
    if let Err(error) = result {
        if error.is_panic() {
            std::panic::resume_unwind(error.into_panic());
        }
        panic!("PostgreSQL content store task failed: {error}");
    }
}

#[tokio::test]
#[ignore = "requires STRAVIA_TEST_POSTGRES_URL pointing to an isolated test PostgreSQL"]
async fn postgres_shared_body_lives_until_the_last_referrer_is_gone() {
    with_postgres(|pool| async move {
        shared_body_lives_until_the_last_referrer_is_gone(Stores::postgres(pool), "postgres").await;
    })
    .await;
}

#[tokio::test]
#[ignore = "requires STRAVIA_TEST_POSTGRES_URL pointing to an isolated test PostgreSQL"]
async fn postgres_projection_text_round_trips_byte_for_byte() {
    with_postgres(|pool| async move {
        projection_text_round_trips_byte_for_byte(Stores::postgres(pool), "postgres").await;
    })
    .await;
}

/// 写入与回收竞争同一份正文：每个长期 Marker 最终都能读回正文，过程中不出现存储错误。
#[tokio::test]
#[ignore = "requires STRAVIA_TEST_POSTGRES_URL pointing to an isolated test PostgreSQL"]
async fn postgres_writers_and_gc_race_on_the_same_digest() {
    with_postgres(|pool| async move {
        let stores = std::sync::Arc::new(Stores::postgres(pool.clone()));
        let owner = Principal::new("postgres-write-vs-gc");
        let body = "contended body ".repeat(29);

        let writer = {
            let stores = std::sync::Arc::clone(&stores);
            let owner = owner.clone();
            let body = body.clone();
            tokio::spawn(async move {
                let mut references = Vec::new();
                for _ in 0..40 {
                    let reference = stores.create_marker(&owner, &body).await;
                    stores.publish_marker(&owner, &reference).await;
                    references.push(reference);
                    // 同一正文的未发布 Marker 会在 pending 窗口后失去引用，制造回收候选。
                    stores.create_marker(&owner, &body).await;
                }
                references
            })
        };
        let collector = {
            let stores = std::sync::Arc::clone(&stores);
            tokio::spawn(async move {
                for _ in 0..40 {
                    stores.sweep_after(2 * HOUR_MS).await;
                }
            })
        };
        let references = writer.await.expect("writer");
        collector.await.expect("collector");

        for reference in &references {
            assert_eq!(
                stores.read_marker_text(&owner, reference).await.as_deref(),
                Some(body.as_str())
            );
        }
        assert_eq!(stores.rows(&owner, TEXT, body.len()).await, 1);
    })
    .await;
}
