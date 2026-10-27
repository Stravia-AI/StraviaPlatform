use futures::StreamExt;
use std::time::Duration;
use stravia_sql::{Pool, Sqlite, sqlite::SqlitePoolOptions};
use tracing_subscriber::{Layer as _, layer::SubscriberExt as _};

mod support;
use support::Captured;

async fn isolated_pool() -> Pool<Sqlite> {
    SqlitePoolOptions::new()
        .max_connections(1)
        .test_before_acquire(false)
        .connect("sqlite::memory:")
        .await
        .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn real_query_results_errors_and_private_values_are_preserved() {
    let captured = Captured::default();
    let _subscriber = tracing::subscriber::set_default(captured.subscriber(&["stravia::sql"]));
    let pool = isolated_pool().await;
    let secret = "private-bind-not-for-diagnostics";
    let value: String = stravia_sql::query_scalar("SELECT ?")
        .bind(secret)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(value, secret);
    let first = captured.take();
    let query = first
        .iter()
        .find(|span| span.get("operation").map(String::as_str) == Some("sql.query"))
        .unwrap();
    assert_eq!(query.get("status").map(String::as_str), Some("completed"));
    assert_eq!(query.get("rows_returned").map(String::as_str), Some("1"));
    let identity = query["query_id"].clone();
    assert_eq!(identity.len(), 64);
    assert!(identity.bytes().all(|byte| byte.is_ascii_hexdigit()));
    let second: String = stravia_sql::query_scalar("SELECT ?")
        .bind("another-private-value")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(second, "another-private-value");
    let none: Option<i64> = stravia_sql::query_scalar("SELECT 1 WHERE 0")
        .fetch_optional(&pool)
        .await
        .unwrap();
    assert_eq!(none, None);
    let error = stravia_sql::query("SELECT * FROM private_missing_table")
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(matches!(error, stravia_sql::Error::Database(_)));
    let remaining = captured.take();
    assert!(
        remaining
            .iter()
            .any(|span| span.get("query_id") == Some(&identity)
                && span.get("status").map(String::as_str) == Some("completed"))
    );
    assert!(
        remaining.iter().any(
            |span| span.get("error_kind").map(String::as_str) == Some("database")
                && span.get("status").map(String::as_str) == Some("error")
        )
    );
    for span in first.iter().chain(&remaining) {
        let serialized = format!("{span:?}");
        for private in [
            secret,
            "another-private-value",
            "private_missing_table",
            "SELECT",
            "db.statement",
            "db.summary",
        ] {
            assert!(
                !serialized.contains(private),
                "diagnostics leaked a private SQL field"
            );
        }
    }
    pool.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn pool_failure_and_cancelled_acquisition_do_not_claim_query_execution() {
    let captured = Captured::default();
    let _subscriber = tracing::subscriber::set_default(captured.subscriber(&["stravia::sql"]));
    let pool = isolated_pool().await;
    let held = pool.acquire().await.unwrap();
    captured.take();
    let blocked = stravia_sql::query_scalar::<_, i64>("SELECT 1").fetch_one(&pool);
    assert!(
        tokio::time::timeout(Duration::from_millis(10), blocked)
            .await
            .is_err()
    );
    let cancelled = captured.take();
    assert!(
        cancelled
            .iter()
            .any(
                |span| span.get("operation").map(String::as_str) == Some("sql.pool.acquire")
                    && span.get("status").map(String::as_str) == Some("cancelled")
            )
    );
    assert!(
        !cancelled
            .iter()
            .any(|span| span.get("operation").map(String::as_str) == Some("sql.query"))
    );
    drop(held);
    pool.close().await;
    assert!(matches!(
        stravia_sql::query("SELECT 1").execute(&pool).await,
        Err(stravia_sql::Error::PoolClosed)
    ));
    let failed = captured.take();
    assert!(
        failed.iter().any(
            |span| span.get("error_kind").map(String::as_str) == Some("pool_closed")
                && span.get("status").map(String::as_str) == Some("error")
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn transactions_query_builders_and_stream_cancellation_keep_native_behavior() {
    let captured = Captured::default();
    let _subscriber = tracing::subscriber::set_default(captured.subscriber(&["stravia::sql"]));
    let pool = isolated_pool().await;
    stravia_sql::query("CREATE TABLE sample(value INTEGER NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    let mut connection = pool.acquire().await.unwrap();
    let mut transaction = connection.begin().await.unwrap();
    stravia_sql::query("INSERT INTO sample VALUES (7)")
        .execute(&mut *transaction)
        .await
        .unwrap();
    let mut nested = transaction.begin().await.unwrap();
    stravia_sql::query("INSERT INTO sample VALUES (8)")
        .execute(&mut *nested)
        .await
        .unwrap();
    nested.rollback().await.unwrap();
    transaction.commit().await.unwrap();
    let mut builder =
        stravia_sql::QueryBuilder::<Sqlite>::new("SELECT value FROM sample WHERE value = ");
    builder.push_bind(7_i64);
    let rows: Vec<(i64,)> = builder
        .build_query_as()
        .fetch_all(&mut *connection)
        .await
        .unwrap();
    assert_eq!(rows, vec![(7,)]);
    captured.take();
    let mut stream = stravia_sql::query("SELECT 1 UNION ALL SELECT 2").fetch(&mut *connection);
    assert!(stream.next().await.unwrap().is_ok());
    drop(stream);
    let cancelled = captured.take();
    assert!(
        cancelled.iter().any(
            |span| span.get("operation").map(String::as_str) == Some("sql.query")
                && span.get("status").map(String::as_str) == Some("cancelled")
        )
    );
    drop(connection);
    pool.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn borrowed_immediate_transaction_commits_on_spawned_worker() {
    let pool = isolated_pool().await;
    let writer = pool.clone();
    tokio::spawn(async move {
        let mut connection = writer.acquire().await.unwrap();
        let mut transaction = connection.begin_with("BEGIN IMMEDIATE").await.unwrap();
        stravia_sql::query("CREATE TABLE committed(value INTEGER NOT NULL)")
            .execute(&mut *transaction)
            .await
            .unwrap();
        stravia_sql::query("INSERT INTO committed VALUES (?)")
            .bind(73_i64)
            .execute(&mut *transaction)
            .await
            .unwrap();
        transaction.commit().await.unwrap();
    })
    .await
    .unwrap();
    let value: i64 = stravia_sql::query_scalar("SELECT value FROM committed")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(value, 73);
    pool.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn filtering_acquire_span_cannot_underflow_pool_waiters() {
    let captured = Captured::default();
    let subscriber = tracing_subscriber::registry().with(captured.clone().with_filter(
        tracing_subscriber::filter::filter_fn(|metadata| {
            metadata.target() == "stravia::sql" && metadata.name() != "sql.pool.acquire"
        }),
    ));
    let _subscriber = tracing::subscriber::set_default(subscriber);
    let pool = isolated_pool().await.named("unregistered");
    drop(pool.acquire().await.unwrap());
    stravia_sql::sample_pool(&pool, "unregistered");
    let events = captured.take();
    let snapshot = events
        .iter()
        .find(|event| event.get("phase").map(String::as_str) == Some("pool_snapshot"))
        .unwrap();
    assert_eq!(snapshot.get("pool_waiting").map(String::as_str), Some("0"));
    pool.close().await;
}
