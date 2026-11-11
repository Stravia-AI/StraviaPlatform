//! SQLite 与显式配置的 PostgreSQL 共用索引生命周期不变量。

use std::time::Duration;

use serde_json::json;
use sqlx::{PgPool, SqlitePool};

use super::{InteractionObservation, content_refs, store};
use crate::history_recall::index::{index_postgres, index_sqlite};

#[derive(Clone)]
enum Db {
    Sqlite(SqlitePool),
    Postgres(PgPool),
}

struct Fixture {
    db: Db,
    observation: InteractionObservation,
    _directory: tempfile::TempDir,
    admin: Option<(PgPool, String)>,
}

impl Fixture {
    async fn sqlite() -> anyhow::Result<Self> {
        let pool = crate::test_support::migrated_sqlite_pool().await?;
        Self::new(Db::Sqlite(pool), None).await
    }

    async fn postgres() -> anyhow::Result<Self> {
        let url = std::env::var("STRAVIA_TEST_POSTGRES_URL")
            .expect("set STRAVIA_TEST_POSTGRES_URL to an isolated PostgreSQL test database");
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await?;
        let schema = format!("stravia_index_test_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await?;
        let options: sqlx::postgres::PgConnectOptions = url.parse()?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect_with(options.options([("search_path", schema.as_str())]))
            .await?;
        crate::migrations::migrate_postgres(&pool, None).await?;
        Self::new(Db::Postgres(pool), Some((admin, schema))).await
    }

    async fn new(db: Db, admin: Option<(PgPool, String)>) -> anyhow::Result<Self> {
        let directory = tempfile::tempdir()?;
        let (sqlite, postgres) = match &db {
            Db::Sqlite(pool) => (Some(pool.clone()), None),
            Db::Postgres(pool) => (None, Some(pool.clone())),
        };
        let observation = InteractionObservation::new(
            sqlite,
            postgres,
            directory.path().to_path_buf(),
            crate::history_retention::HistoryRetention::new(1),
            true,
            crate::generation_chain::test_chain().await,
        )
        .await;
        Ok(Self {
            db,
            observation,
            _directory: directory,
            admin,
        })
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

    async fn index(&self) -> anyhow::Result<()> {
        match &self.db {
            Db::Sqlite(pool) => index_sqlite(pool, "i").await,
            Db::Postgres(pool) => index_postgres(pool, "i").await,
        }
    }

    async fn user_event(&self, sequence: i64, text: &str) -> anyhow::Result<()> {
        match &self.db {
            Db::Sqlite(pool) => {
                let mut tx = pool.begin().await?;
                let mut content = [content_refs::text_content(text)];
                let ids = content_refs::intern_sqlite(&mut tx, "owner", &mut content).await?;
                sqlx::query("INSERT INTO observation_events(sequence,occurred_at,interaction_id,kind) VALUES ($1,$1,'i','user_input_recorded')")
                    .bind(sequence).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO observation_event_contents(sequence,ordinal,content_id) VALUES ($1,0,$2)")
                    .bind(sequence).bind(ids[0]).execute(&mut *tx).await?;
                tx.commit().await?;
            }
            Db::Postgres(pool) => {
                let mut tx = pool.begin().await?;
                let mut content = [content_refs::text_content(text)];
                let ids = content_refs::intern_postgres(&mut tx, "owner", &mut content).await?;
                sqlx::query("INSERT INTO observation_events(sequence,occurred_at,interaction_id,kind) VALUES ($1,$1,'i','user_input_recorded')")
                    .bind(sequence).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO observation_event_contents(sequence,ordinal,content_id) VALUES ($1,0,$2)")
                    .bind(sequence).bind(ids[0]).execute(&mut *tx).await?;
                tx.commit().await?;
            }
        }
        Ok(())
    }

    async fn tool_event(
        &self,
        sequence: i64,
        kind: &str,
        input: serde_json::Value,
    ) -> anyhow::Result<()> {
        let attrs =
            store::encode_attrs(&json!({"name":"lookup", "input":input, "tool_id":"tool"}))?;
        let sql = "INSERT INTO observation_events(sequence,occurred_at,interaction_id,kind,attrs) VALUES ($1,$1,'i',$2,$3)";
        match &self.db {
            Db::Sqlite(pool) => {
                sqlx::query(sql)
                    .bind(sequence)
                    .bind(kind)
                    .bind(attrs)
                    .execute(pool)
                    .await?;
            }
            Db::Postgres(pool) => {
                sqlx::query(sql)
                    .bind(sequence)
                    .bind(kind)
                    .bind(attrs)
                    .execute(pool)
                    .await?;
            }
        }
        Ok(())
    }

    async fn segments(&self) -> anyhow::Result<Vec<(i64, i64, i64, i64, i64)>> {
        let sql = "SELECT event_sequence,segment_ordinal,CAST(role_weight AS BIGINT),byte_start,byte_end FROM history_search_segments WHERE interaction_id='i' ORDER BY event_sequence,segment_ordinal";
        Ok(match &self.db {
            Db::Sqlite(pool) => sqlx::query_as(sql).fetch_all(pool).await?,
            Db::Postgres(pool) => sqlx::query_as(sql).fetch_all(pool).await?,
        })
    }

    async fn counts(&self) -> anyhow::Result<(i64, i64)> {
        let sql = "SELECT (SELECT COUNT(*) FROM history_search_segments),(SELECT COUNT(*) FROM history_search_progress)";
        Ok(match &self.db {
            Db::Sqlite(pool) => {
                let counts: (i64, i64) = sqlx::query_as(sql).fetch_one(pool).await?;
                let fts: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM history_search_segments_fts")
                        .fetch_one(pool)
                        .await?;
                assert_eq!(fts, counts.0, "contentless FTS rows track segment rows");
                counts
            }
            Db::Postgres(pool) => sqlx::query_as(sql).fetch_one(pool).await?,
        })
    }

    async fn close(self) -> anyhow::Result<()> {
        self.observation.shutdown().await;
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

async fn lifecycle(f: &Fixture) -> anyhow::Result<()> {
    f.execute("INSERT INTO interaction_observations(id,principal,root_id,root_run_id,first_route_id,status,started_at,last_active_at) VALUES ('i','owner','i','run','route','running',0,0); INSERT INTO inference_run_observations(id,interaction_id,ingress_protocol,route_id,status,debug_enabled,started_at,last_active_at) VALUES ('run','i','responses','route','running',false,0,0)").await?;
    // 单个事务最多推进 500 个已提交事件。
    for sequence in 1..=501 {
        f.user_event(sequence, "alpha beta").await?;
    }
    f.index().await?;
    assert_eq!(f.segments().await?.len(), 500);
    f.index().await?;
    let incremental = f.segments().await?;
    assert_eq!(incremental.len(), 501);
    assert!(
        incremental
            .iter()
            .enumerate()
            .all(|(i, row)| *row == ((i + 1) as i64, 0, 3, 0, 10))
    );
    f.index().await?;
    assert_eq!(f.segments().await?, incremental);
    f.counts().await?;

    // 回看历史的调用不索引工具名或参数。
    f.tool_event(
        502,
        "client_tool_handoff",
        json!({"path":"stravia://history/i"}),
    )
    .await?;
    f.tool_event(
        503,
        "platform_tool_started",
        json!({"path":"stravia://history/i"}),
    )
    .await?;
    f.tool_event(504, "client_tool_handoff", json!({"query":"needle"}))
        .await?;
    f.execute("UPDATE interaction_observations SET status='completed' WHERE id='i'; UPDATE inference_run_observations SET status='completed', client_output_committed=true WHERE id='run'").await?;
    let (left, right) = tokio::join!(f.index(), f.index());
    left?;
    right?;
    let terminal = f.segments().await?;
    assert_eq!(&terminal[..501], incremental.as_slice());
    assert!(terminal.iter().all(|row| row.0 != 502 && row.0 != 503));
    assert_eq!(
        terminal
            .iter()
            .filter(|row| row.0 == 504 && row.2 == 1)
            .count(),
        1
    );
    f.counts().await?;

    // 终结后续写并重建的结果，应与从空索引重建全部已提交事件一致。
    f.user_event(505, "later input").await?;
    f.user_event(506, &"word ".repeat(260)).await?;
    f.index().await?;
    let extended = f.segments().await?;
    assert!(extended.contains(&(505, 0, 3, 0, 11)));
    assert!(extended.contains(&(506, 0, 3, 0, 1279)));
    assert!(extended.contains(&(506, 1, 3, 1205, 1299)));
    f.execute("DELETE FROM history_search_segments; DELETE FROM history_search_progress;")
        .await?;
    f.index().await?;
    assert_eq!(f.segments().await?, extended);
    f.counts().await?;

    f.observation
        .sweep_history(200_000_000, Duration::from_secs(86_400))
        .await?;
    assert_eq!(f.counts().await?, (0, 0));
    f.execute("INSERT INTO interaction_observations(id,principal,root_id,root_run_id,first_route_id,status,started_at,last_active_at) VALUES ('i','owner','i','run','route','completed',0,0); INSERT INTO inference_run_observations(id,interaction_id,ingress_protocol,route_id,status,debug_enabled,started_at,last_active_at,client_output_committed) VALUES ('run','i','responses','route','completed',false,0,0,true)").await?;
    f.user_event(1, "clear this").await?;
    f.index().await?;
    assert_eq!(f.counts().await?, (1, 1));
    f.observation.clear_history().await?;
    assert_eq!(f.counts().await?, (0, 0));
    Ok(())
}

#[tokio::test]
async fn sqlite_index_lifecycle() -> anyhow::Result<()> {
    let fixture = Fixture::sqlite().await?;
    lifecycle(&fixture).await?;
    fixture.close().await
}

#[tokio::test]
#[ignore = "requires STRAVIA_TEST_POSTGRES_URL pointing to an isolated test database"]
async fn postgres_index_lifecycle() -> anyhow::Result<()> {
    let fixture = Fixture::postgres().await?;
    lifecycle(&fixture).await?;
    fixture.close().await
}
