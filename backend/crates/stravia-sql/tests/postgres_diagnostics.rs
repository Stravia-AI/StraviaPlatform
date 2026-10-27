//! Optional PostgreSQL contract follows the workspace's isolated-test URL convention.
//! All setup is transactional; rollback/drop removes the isolated schema.
use futures::StreamExt;
use std::time::{SystemTime, UNIX_EPOCH};
use stravia_sql::{PgPool, Row};

mod support;
use support::Captured;

#[tokio::test(flavor = "current_thread")]
async fn postgres_write_leases_preserve_database_lock_concurrency()
-> Result<(), Box<dyn std::error::Error>> {
    let Ok(url) = std::env::var("STRAVIA_TEST_POSTGRES_URL") else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let key = i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_micros())?;
    let mut first = pool.acquire_write().await?;
    let mut transaction = first.begin().await?;
    stravia_sql::query("SELECT pg_advisory_xact_lock($1)")
        .bind(key)
        .execute(&mut *transaction)
        .await?;

    let second = async {
        let mut connection = pool.acquire_write().await?;
        let mut transaction = connection.begin().await?;
        let acquired: bool = stravia_sql::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
            .bind(key)
            .fetch_one(&mut *transaction)
            .await?;
        transaction.rollback().await?;
        connection.finish().await?;
        Ok::<_, stravia_sql::Error>(acquired)
    };
    let acquired = tokio::time::timeout(std::time::Duration::from_secs(5), second).await??;
    assert!(
        !acquired,
        "independent writer must reach PostgreSQL while the first holds its lock"
    );
    transaction.commit().await?;
    first.finish().await?;

    let mut connection = pool.acquire_write().await?;
    let mut transaction = connection.begin().await?;
    let acquired: bool = stravia_sql::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
        .bind(key)
        .fetch_one(&mut *transaction)
        .await?;
    assert!(
        acquired,
        "committing the first transaction must release its database lock"
    );
    transaction.rollback().await?;
    connection.finish().await?;
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn postgres_queries_transactions_and_database_errors_keep_their_results()
-> Result<(), Box<dyn std::error::Error>> {
    let Ok(url) = std::env::var("STRAVIA_TEST_POSTGRES_URL") else {
        return Ok(());
    };
    let captured = Captured::default();
    let _subscriber = tracing::subscriber::set_default(captured.subscriber(&["stravia::sql"]));
    let pool = PgPool::connect(&url).await?;
    let mut transaction = pool.begin().await?;
    let schema = format!(
        "stravia_sql_diagnostics_{}_{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    stravia_sql::raw_sql(stravia_sql::AssertSqlSafe(format!(
        "CREATE SCHEMA {schema}; SET LOCAL search_path TO {schema}"
    )))
    .execute(&mut *transaction)
    .await?;
    stravia_sql::query("CREATE TABLE sample(value BIGINT NOT NULL)")
        .execute(&mut *transaction)
        .await?;
    stravia_sql::query("INSERT INTO sample VALUES ($1)")
        .bind(7_i64)
        .execute(&mut *transaction)
        .await?;
    let value: i64 = stravia_sql::query_scalar("SELECT value FROM sample")
        .fetch_one(&mut *transaction)
        .await?;
    assert_eq!(value, 7);
    let secret = "postgres-private-bind-value";
    let obtained: String = stravia_sql::query_scalar("SELECT $1::text")
        .bind(secret)
        .fetch_one(&mut *transaction)
        .await?;
    assert_eq!(obtained, secret);
    let completed = captured.take();
    assert!(
        completed.iter().any(
            |span| span.get("status").map(String::as_str) == Some("completed")
                && span.get("backend").map(String::as_str) == Some("PostgreSQL")
                && span.get("rows_returned").map(String::as_str) == Some("1")
        )
    );
    let mut rows = stravia_sql::query("SELECT generate_series(1, 100)").fetch(&mut *transaction);
    assert_eq!(rows.next().await.unwrap()?.get::<i32, _>(0), 1);
    drop(rows);
    let cancelled = captured.take();
    assert!(
        cancelled.iter().any(
            |span| span.get("status").map(String::as_str) == Some("cancelled")
                && span.get("rows_returned").map(String::as_str) == Some("1")
        )
    );
    let error = stravia_sql::query("SELECT * FROM private_missing_table")
        .execute(&mut *transaction)
        .await
        .unwrap_err();
    assert!(matches!(error, stravia_sql::Error::Database(_)));
    let state = error.as_database_error().unwrap().code().unwrap();
    let failed = captured.take();
    assert!(failed.iter().any(
        |span| span.get("status").map(String::as_str) == Some("error")
            && span.get("error_kind").map(String::as_str) == Some("database")
            && span.get("error_code").map(String::as_str) == Some(state.as_ref())
    ));
    for span in completed.iter().chain(&cancelled).chain(&failed) {
        let serialized = format!("{span:?}");
        for private in [secret, "private_missing_table", "SELECT", schema.as_str()] {
            assert!(
                !serialized.contains(private),
                "diagnostics leaked private SQL data"
            );
        }
    }
    transaction.rollback().await?;
    pool.close().await;
    Ok(())
}
