use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use stravia_sql::{SqlitePool, sqlite::SqlitePoolOptions};

mod support;
use support::{Captured, Fields};

fn named_spans<'a>(spans: &'a [Fields], name: &str) -> Vec<&'a Fields> {
    spans
        .iter()
        .filter(|span| span.get("span_name").map(String::as_str) == Some(name))
        .collect()
}

fn span_statuses<'a>(spans: &'a [Fields], name: &str) -> Vec<&'a str> {
    named_spans(spans, name)
        .into_iter()
        .filter_map(|span| span.get("status").map(String::as_str))
        .collect()
}

async fn isolated_pool(max_connections: u32) -> SqlitePool {
    SqlitePoolOptions::new()
        .max_connections(max_connections)
        .test_before_acquire(false)
        .connect("sqlite::memory:")
        .await
        .unwrap()
}

async fn wait_started(started: &AtomicBool) {
    while !started.load(Ordering::Acquire) {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_finish_completes_hold_and_finish_without_cleanup() {
    let captured = Captured::default();
    let _subscriber = tracing::subscriber::set_default(captured.subscriber(&["stravia::perf"]));
    let pool = isolated_pool(3).await;

    let mut connection = pool.acquire_write().await.unwrap();
    let mut transaction = connection.begin().await.unwrap();
    stravia_sql::query("CREATE TABLE sample(id INTEGER NOT NULL)")
        .execute(&mut *transaction)
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    connection.finish().await.unwrap();

    let spans = captured.take();
    assert_eq!(
        span_statuses(&spans, "sqlite.write_gate.hold"),
        vec!["completed"]
    );
    assert_eq!(
        span_statuses(&spans, "sqlite.write_gate.finish"),
        vec!["completed"]
    );
    assert!(named_spans(&spans, "sqlite.write_gate.cleanup").is_empty());
    for span in &spans {
        if matches!(
            span.get("span_name").map(String::as_str),
            Some(
                "sqlite.write_gate.hold" | "sqlite.write_gate.finish" | "sqlite.write_gate.cleanup"
            )
        ) {
            assert_eq!(
                span.get("span_target").map(String::as_str),
                Some("stravia::perf")
            );
            assert_eq!(span.get("backend").map(String::as_str), Some("SQLite"));
            assert_eq!(span.get("pool").map(String::as_str), Some("main"));
        }
    }
    pool.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn connection_acquisition_failure_marks_hold_error_without_cleanup() {
    let captured = Captured::default();
    let _subscriber = tracing::subscriber::set_default(captured.subscriber(&["stravia::perf"]));
    let pool = isolated_pool(1).await;
    pool.close().await;

    let error = pool.acquire_write().await.unwrap_err();
    assert!(matches!(error, stravia_sql::Error::PoolClosed));

    let spans = captured.take();
    let holds = named_spans(&spans, "sqlite.write_gate.hold");
    assert_eq!(holds.len(), 1);
    assert_eq!(holds[0].get("status").map(String::as_str), Some("error"));
    assert_eq!(
        holds[0].get("error_kind").map(String::as_str),
        Some("pool_closed")
    );
    assert!(named_spans(&spans, "sqlite.write_gate.finish").is_empty());
    assert!(named_spans(&spans, "sqlite.write_gate.cleanup").is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_connection_acquisition_releases_hold_without_cleanup() {
    let captured = Captured::default();
    let _subscriber = tracing::subscriber::set_default(captured.subscriber(&["stravia::perf"]));
    let pool = isolated_pool(1).await;
    let held = pool.acquire().await.unwrap();

    let started = Arc::new(AtomicBool::new(false));
    let writer = pool.clone();
    let task_started = Arc::clone(&started);
    let task = tokio::spawn(async move {
        task_started.store(true, Ordering::Release);
        let _ = writer.acquire_write().await;
    });
    wait_started(&started).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    drop(held);

    let spans = captured.take();
    let holds = named_spans(&spans, "sqlite.write_gate.hold");
    assert_eq!(holds.len(), 1, "hold starts once the mutex is acquired");
    assert_eq!(
        holds[0].get("status").map(String::as_str),
        Some("cancelled")
    );
    assert!(named_spans(&spans, "sqlite.write_gate.finish").is_empty());
    assert!(named_spans(&spans, "sqlite.write_gate.cleanup").is_empty());
    pool.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn dropped_write_connection_cleanup_finishes_before_hold_closes() {
    let captured = Captured::default();
    let _subscriber = tracing::subscriber::set_default(captured.subscriber(&["stravia::perf"]));
    let pool = isolated_pool(1).await;

    {
        let mut connection = pool.acquire_write().await.unwrap();
        let mut transaction = connection.begin().await.unwrap();
        stravia_sql::query("CREATE TABLE sample(id INTEGER NOT NULL)")
            .execute(&mut *transaction)
            .await
            .unwrap();
        // Drop the transaction and then the write connection without finish.
    }
    // The next lease can only acquire after cleanup released the gate.
    let next = pool.acquire_write().await.unwrap();
    next.finish().await.unwrap();

    let spans = captured.take();
    let holds = named_spans(&spans, "sqlite.write_gate.hold");
    assert_eq!(holds.len(), 2);
    assert!(
        holds
            .iter()
            .any(|span| span.get("status").map(String::as_str) == Some("cancelled"))
    );
    assert!(
        holds
            .iter()
            .any(|span| span.get("status").map(String::as_str) == Some("completed"))
    );
    assert_eq!(
        span_statuses(&spans, "sqlite.write_gate.cleanup"),
        vec!["completed"]
    );
    assert_eq!(
        span_statuses(&spans, "sqlite.write_gate.finish"),
        vec!["completed"]
    );
    pool.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn finish_with_active_transaction_reports_error_and_cleans_up() {
    let captured = Captured::default();
    let _subscriber = tracing::subscriber::set_default(captured.subscriber(&["stravia::perf"]));
    let pool = isolated_pool(1).await;

    let mut connection = pool.acquire_write().await.unwrap();
    let transaction = connection.begin().await.unwrap();
    // Leak the wrapper so the native transaction stays active, exercising the
    // finish-time defensive check without bypassing the public API.
    std::mem::forget(transaction);
    let error = connection.finish().await.unwrap_err();
    assert!(matches!(error, stravia_sql::Error::Protocol(_)));

    // The failed finish hands the gate to the close-on-drop cleanup.
    let next = pool.acquire_write().await.unwrap();
    next.finish().await.unwrap();

    let spans = captured.take();
    assert_eq!(
        span_statuses(&spans, "sqlite.write_gate.finish"),
        vec!["error", "completed"]
    );
    let finishes = named_spans(&spans, "sqlite.write_gate.finish");
    assert_eq!(
        finishes[0].get("error_kind").map(String::as_str),
        Some("protocol")
    );
    assert_eq!(
        span_statuses(&spans, "sqlite.write_gate.cleanup"),
        vec!["completed"]
    );
    let holds = named_spans(&spans, "sqlite.write_gate.hold");
    assert_eq!(holds.len(), 2);
    assert!(
        holds
            .iter()
            .any(|span| span.get("status").map(String::as_str) == Some("error"))
    );
    assert!(
        holds
            .iter()
            .any(|span| span.get("status").map(String::as_str) == Some("completed"))
    );
    pool.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn transaction_phases_use_perf_target_without_extra_query_counts() {
    let captured = Captured::default();
    let _subscriber =
        tracing::subscriber::set_default(captured.subscriber(&["stravia::sql", "stravia::perf"]));
    let pool = isolated_pool(1).await;

    let mut transaction = pool.begin().await.unwrap();
    let nested = transaction.begin().await.unwrap();
    nested.rollback().await.unwrap();
    transaction.commit().await.unwrap();

    let spans = captured.take();
    assert_eq!(
        span_statuses(&spans, "sql.transaction.begin"),
        vec!["completed", "completed"]
    );
    assert_eq!(
        span_statuses(&spans, "sql.transaction.rollback"),
        vec!["completed"]
    );
    assert_eq!(
        span_statuses(&spans, "sql.transaction.commit"),
        vec!["completed"]
    );
    for name in [
        "sql.transaction.begin",
        "sql.transaction.commit",
        "sql.transaction.rollback",
    ] {
        for span in named_spans(&spans, name) {
            assert_eq!(
                span.get("span_target").map(String::as_str),
                Some("stravia::perf")
            );
            assert_eq!(span.get("backend").map(String::as_str), Some("SQLite"));
        }
    }
    assert_eq!(
        named_spans(&spans, "sql.pool.acquire").len(),
        1,
        "Pool::begin keeps its existing single acquisition"
    );
    assert!(
        named_spans(&spans, "sql.query").is_empty(),
        "native transaction control is not an observed query"
    );
    pool.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_pool_transaction_begin_reports_cancelled() {
    let captured = Captured::default();
    let _subscriber =
        tracing::subscriber::set_default(captured.subscriber(&["stravia::sql", "stravia::perf"]));
    let pool = isolated_pool(1).await;
    let held = pool.acquire().await.unwrap();
    captured.take();

    let started = Arc::new(AtomicBool::new(false));
    let begin_pool = pool.clone();
    let task_started = Arc::clone(&started);
    let task = tokio::spawn(async move {
        task_started.store(true, Ordering::Release);
        let _ = begin_pool.begin().await;
    });
    wait_started(&started).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    drop(held);

    let spans = captured.take();
    assert_eq!(
        span_statuses(&spans, "sql.transaction.begin"),
        vec!["cancelled"]
    );
    assert!(named_spans(&spans, "sql.transaction.commit").is_empty());
    assert!(named_spans(&spans, "sql.transaction.rollback").is_empty());
    pool.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn failed_begin_with_reports_error_without_sql_text() {
    let captured = Captured::default();
    let _subscriber =
        tracing::subscriber::set_default(captured.subscriber(&["stravia::sql", "stravia::perf"]));
    let pool = isolated_pool(1).await;
    let mut connection = pool.acquire().await.unwrap();
    captured.take();

    let error = connection
        .begin_with("BEGIN not-a-keyword")
        .await
        .unwrap_err();
    assert!(matches!(error, stravia_sql::Error::Database(_)));

    let spans = captured.take();
    let begins = named_spans(&spans, "sql.transaction.begin");
    assert_eq!(begins.len(), 1);
    assert_eq!(begins[0].get("status").map(String::as_str), Some("error"));
    assert_eq!(
        begins[0].get("error_kind").map(String::as_str),
        Some("database")
    );
    assert_eq!(
        begins[0].get("span_target").map(String::as_str),
        Some("stravia::perf")
    );
    assert!(!format!("{:?}", begins[0]).contains("not-a-keyword"));
    drop(connection);
    pool.close().await;
}
