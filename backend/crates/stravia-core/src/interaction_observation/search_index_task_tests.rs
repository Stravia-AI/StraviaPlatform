//! SQLite 与 PostgreSQL 共用的后台索引行为测试。

use std::{sync::Arc, time::Duration};

use serde_json::json;
use sqlx::{PgPool, SqlitePool};
use stravia_runtime_contract::CancellationToken;
use tokio::sync::{broadcast, mpsc};

use super::{
    content_refs,
    manifest_index::DebugTraceIndex,
    search_index_task,
    store::ObservationStore,
    types::{ObservationEvent, ObservationUpdate},
};

#[derive(Clone)]
enum Db {
    Sqlite(SqlitePool),
    Postgres(PgPool),
}

struct Fixture {
    db: Db,
    directory: tempfile::TempDir,
    admin: Option<(PgPool, String)>,
}

impl Fixture {
    async fn sqlite() -> anyhow::Result<Self> {
        Ok(Self {
            db: Db::Sqlite(crate::test_support::migrated_sqlite_pool().await?),
            directory: tempfile::tempdir()?,
            admin: None,
        })
    }

    async fn postgres() -> anyhow::Result<Self> {
        let url = std::env::var("STRAVIA_TEST_POSTGRES_URL")
            .expect("set STRAVIA_TEST_POSTGRES_URL to an isolated PostgreSQL test database");
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await?;
        let schema = format!("stravia_index_task_test_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await?;
        let options: sqlx::postgres::PgConnectOptions = url.parse()?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect_with(options.options([("search_path", schema.as_str())]))
            .await?;
        crate::migrations::migrate_postgres(&pool, None).await?;
        Ok(Self {
            db: Db::Postgres(pool),
            directory: tempfile::tempdir()?,
            admin: Some((admin, schema)),
        })
    }

    fn store(&self) -> ObservationStore {
        let traces = Arc::new(DebugTraceIndex::empty(self.directory.path()));
        match &self.db {
            Db::Sqlite(pool) => ObservationStore::Sqlite(pool.clone(), traces),
            Db::Postgres(pool) => ObservationStore::Postgres(pool.clone(), traces),
        }
    }

    async fn setup(&self) -> anyhow::Result<()> {
        let sql = "INSERT INTO interaction_observations(id,principal,root_id,root_run_id,first_route_id,status,started_at,last_active_at) VALUES ('i','owner','i','run','route','running',0,0); INSERT INTO inference_run_observations(id,interaction_id,ingress_protocol,route_id,status,debug_enabled,started_at,last_active_at) VALUES ('run','i','responses','route','running',false,0,0)";
        match &self.db {
            Db::Sqlite(pool) => {
                sqlx::raw_sql(sql).execute(pool).await?;
            }
            Db::Postgres(pool) => {
                sqlx::raw_sql(sql).execute(pool).await?;
            }
        }
        Ok(())
    }

    async fn event(&self, sequence: i64) -> anyhow::Result<()> {
        match &self.db {
            Db::Sqlite(pool) => {
                let mut tx = pool.begin().await?;
                let mut content = [content_refs::text_content("background indexed text")];
                let ids = content_refs::intern_sqlite(&mut tx, "owner", &mut content).await?;
                sqlx::query("INSERT INTO observation_events(sequence,occurred_at,interaction_id,kind) VALUES ($1,$1,'i','user_input_recorded')").bind(sequence).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO observation_event_contents(sequence,ordinal,content_id) VALUES ($1,0,$2)").bind(sequence).bind(ids[0]).execute(&mut *tx).await?;
                sqlx::query(
                    "UPDATE interaction_observations SET last_event_sequence=$1 WHERE id='i'",
                )
                .bind(sequence)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
            }
            Db::Postgres(pool) => {
                let mut tx = pool.begin().await?;
                let mut content = [content_refs::text_content("background indexed text")];
                let ids = content_refs::intern_postgres(&mut tx, "owner", &mut content).await?;
                sqlx::query("INSERT INTO observation_events(sequence,occurred_at,interaction_id,kind) VALUES ($1,$1,'i','user_input_recorded')").bind(sequence).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO observation_event_contents(sequence,ordinal,content_id) VALUES ($1,0,$2)").bind(sequence).bind(ids[0]).execute(&mut *tx).await?;
                sqlx::query(
                    "UPDATE interaction_observations SET last_event_sequence=$1 WHERE id='i'",
                )
                .bind(sequence)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
            }
        }
        Ok(())
    }

    async fn execute(&self, sql: &'static str) -> anyhow::Result<()> {
        match &self.db {
            Db::Sqlite(pool) => {
                sqlx::raw_sql(sql).execute(pool).await?;
            }
            Db::Postgres(pool) => {
                sqlx::raw_sql(sql).execute(pool).await?;
            }
        }
        Ok(())
    }

    async fn progress(&self) -> anyhow::Result<i64> {
        let sql =
            "SELECT last_event_sequence FROM history_search_progress WHERE interaction_id='i'";
        Ok(match &self.db {
            Db::Sqlite(pool) => sqlx::query_scalar(sql).fetch_one(pool).await?,
            Db::Postgres(pool) => sqlx::query_scalar(sql).fetch_one(pool).await?,
        })
    }

    async fn wait_for(&self, expected: i64) -> anyhow::Result<()> {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let sql = "SELECT COUNT(*) FROM history_search_segments WHERE interaction_id='i'";
                let count: i64 = match &self.db {
                    Db::Sqlite(pool) => sqlx::query_scalar(sql).fetch_one(pool).await?,
                    Db::Postgres(pool) => sqlx::query_scalar(sql).fetch_one(pool).await?,
                };
                if count == expected {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::task::yield_now().await;
            }
        })
        .await??;
        Ok(())
    }

    async fn close(self) -> anyhow::Result<()> {
        match self.db {
            Db::Sqlite(pool) => pool.close().await,
            Db::Postgres(pool) => pool.close().await,
        }
        if let Some((admin, schema)) = self.admin {
            sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
                .execute(&admin)
                .await?;
            admin.close().await;
        }
        Ok(())
    }
}

fn update(sequence: i64) -> ObservationUpdate {
    ObservationUpdate::Event(ObservationEvent {
        sequence,
        occurred_at: sequence,
        interaction_id: Some("i".into()),
        run_id: None,
        rejection_id: None,
        kind: "user_input_recorded".into(),
        payload: json!({}),
    })
}

async fn scenario(f: &Fixture) -> anyhow::Result<()> {
    f.setup().await?;
    // 启动对账无需再次唤醒即可推进多个批次。
    for sequence in 1..=501 {
        f.event(sequence).await?;
    }
    let (updates, receiver) = broadcast::channel(1);
    let (sweeps, sweep_receiver) = mpsc::channel(1);
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(search_index_task::run(
        f.store(),
        receiver,
        sweep_receiver,
        cancellation.clone(),
    ));
    f.wait_for(501).await?;
    tokio::task::yield_now().await;
    assert!(
        !task.is_finished(),
        "worker should remain alive after startup reconciliation"
    );

    // 已提交更新无需手动索引即可检索。
    f.event(502).await?;
    updates.send(update(502))?;
    f.wait_for(502).await?;

    // 广播落后丢失更新后，对账补齐遗漏。
    f.event(503).await?;
    updates.send(update(503))?;
    updates.send(ObservationUpdate::LiveSnapshot { blocks: vec![] })?;
    f.wait_for(503).await?;

    // 回收结束后即使没有通知也执行对账。
    f.event(504).await?;
    sweeps.send(()).await?;
    f.wait_for(504).await?;

    cancellation.cancel();
    tokio::time::timeout(Duration::from_secs(5), task).await??;
    Ok(())
}

async fn convergence(f: &Fixture) -> anyhow::Result<()> {
    f.setup().await?;
    f.event(1).await?;
    // 子节点或非事件更新可使 Interaction 的水位高于其最后一条事件。
    f.execute("UPDATE interaction_observations SET last_event_sequence=10 WHERE id='i'; INSERT INTO interaction_observations(id,principal,root_id,root_run_id,first_route_id,status,started_at,last_active_at,last_event_sequence) VALUES ('bad','owner','bad','bad-run','route','failed',0,0,2); INSERT INTO inference_run_observations(id,interaction_id,ingress_protocol,route_id,status,debug_enabled,started_at,last_active_at,finished_at,last_event_sequence,client_output_committed) VALUES ('bad-run','bad','responses','route','failed',false,0,0,1,2,false)").await?;
    let cancellation = CancellationToken::new();
    // 对账完成证明不合格 Interaction 和水位差不会导致死循环。
    tokio::time::timeout(
        Duration::from_secs(10),
        search_index_task::reconcile(&f.store(), &cancellation),
    )
    .await??;
    assert_eq!(f.progress().await?, 10);
    f.wait_for(1).await?;
    Ok(())
}

#[tokio::test]
async fn sqlite_background_indexing_reconciles_and_stops() -> anyhow::Result<()> {
    let fixture = Fixture::sqlite().await?;
    scenario(&fixture).await?;
    fixture.close().await?;
    let fixture = Fixture::sqlite().await?;
    convergence(&fixture).await?;
    fixture.close().await
}

#[tokio::test]
#[ignore = "requires STRAVIA_TEST_POSTGRES_URL pointing to an isolated test database"]
async fn postgres_background_indexing_reconciles_and_stops() -> anyhow::Result<()> {
    let fixture = Fixture::postgres().await?;
    scenario(&fixture).await?;
    fixture.close().await?;
    let fixture = Fixture::postgres().await?;
    convergence(&fixture).await?;
    fixture.close().await
}

async fn real_writer(f: &Fixture) -> anyhow::Result<()> {
    use super::{AdmissionFacts, IngressStart, InteractionObservation, RunStart};
    use stravia_runtime_contract::protocol::ir::{AiItem, AiRequest, MessageContent, Role};

    let (sqlite, postgres) = match &f.db {
        Db::Sqlite(pool) => (Some(pool.clone()), None),
        Db::Postgres(pool) => (None, Some(pool.clone())),
    };
    let observation = InteractionObservation::new(
        sqlite,
        postgres,
        f.directory.path().to_path_buf(),
        crate::history_retention::HistoryRetention::new(1),
        true,
        crate::generation_chain::test_chain().await,
    )
    .await;
    let receiver = observation.inner.updates.subscribe();
    let (_sweeps, sweep_receiver) = mpsc::channel(1);
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(search_index_task::run(
        observation.inner.store.clone(),
        receiver,
        sweep_receiver,
        cancellation.clone(),
    ));

    // 通过真实入口写入用户消息，索引任务只接收 Observation 自己发布的通知。
    let run = observation
        .observe_ingress(IngressStart {
            id: "writer-run".into(),
            method: "POST".into(),
            path: "/v1/responses".into(),
            protocol: "responses".into(),
        })
        .admit(
            RunStart {
                id: "writer-run".into(),
                principal: "owner".into(),
                api_key_id: None,
                api_key_name: None,
                route_id: "route".into(),
                model_display_name: None,
                ingress_protocol: "responses".into(),
            },
            AdmissionFacts {
                client_request: Arc::new(AiRequest::new(
                    "model",
                    vec![AiItem {
                        role: Role::User,
                        content: MessageContent::Text(Arc::new(
                            "real writer searchable text".into(),
                        )),
                        tool_calls: None,
                        tool_call_id: None,
                        meta: None,
                    }],
                )),
                has_new_user: true,
                has_matching_pending_tool_result: false,
                generation_root_id: None,
                generation_parent_id: None,
            },
        );
    run.capture_user_input(&[AiItem {
        role: Role::User,
        content: MessageContent::Text(Arc::new("real writer searchable text".into())),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    }]);
    run.publish_user_input();
    observation.flush().await?;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let sql = "SELECT COUNT(*) FROM history_search_segments s JOIN interaction_observations i ON i.id=s.interaction_id WHERE i.root_run_id='writer-run'";
            let count: i64 = match &f.db {
                Db::Sqlite(pool) => sqlx::query_scalar(sql).fetch_one(pool).await?,
                Db::Postgres(pool) => sqlx::query_scalar(sql).fetch_one(pool).await?,
            };
            if count > 0 { return Ok::<_, anyhow::Error>(()); }
            tokio::task::yield_now().await;
        }
    }).await??;
    drop(run);
    cancellation.cancel();
    tokio::time::timeout(Duration::from_secs(5), task).await??;
    observation.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn sqlite_real_observation_is_indexed_automatically() -> anyhow::Result<()> {
    let fixture = Fixture::sqlite().await?;
    real_writer(&fixture).await?;
    fixture.close().await
}

#[tokio::test]
#[ignore = "requires STRAVIA_TEST_POSTGRES_URL pointing to an isolated test database"]
async fn postgres_real_observation_is_indexed_automatically() -> anyhow::Result<()> {
    let fixture = Fixture::postgres().await?;
    real_writer(&fixture).await?;
    fixture.close().await
}

#[tokio::test]
async fn gateway_shutdown_joins_background_indexer() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let gateway = crate::gateway::Gateway::new(crate::config::GatewayConfig {
        data_dir: directory.path().to_path_buf(),
        catalog_base_url: None,
        ..Default::default()
    })
    .await?;
    tokio::time::timeout(Duration::from_secs(10), gateway.shutdown()).await?;
    Ok(())
}
