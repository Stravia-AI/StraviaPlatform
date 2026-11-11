use std::time::Duration;

use stravia_sql::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};

async fn isolated_pool(directory: &std::path::Path) -> SqlitePool {
    SqlitePoolOptions::new()
        .max_connections(3)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(directory.join("coordination.db"))
                .create_if_missing(true)
                .journal_mode(SqliteJournalMode::Wal)
                .busy_timeout(Duration::from_millis(25)),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn queued_writes_outlive_sqlite_busy_timeout_without_blocking_readers() {
    let directory = tempfile::tempdir().unwrap();
    let pool = isolated_pool(directory.path()).await;
    stravia_sql::query("CREATE TABLE counter (value INTEGER NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    stravia_sql::query("INSERT INTO counter VALUES (0)")
        .execute(&pool)
        .await
        .unwrap();

    let mut first = pool.acquire_write().await.unwrap();
    let mut transaction = first.begin_with("BEGIN IMMEDIATE").await.unwrap();
    stravia_sql::query("UPDATE counter SET value = value + 1")
        .execute(&mut *transaction)
        .await
        .unwrap();

    let second_pool = pool.clone();
    let (started, ready) = tokio::sync::oneshot::channel();
    let mut second = tokio::spawn(async move {
        started.send(()).unwrap();
        let mut connection = second_pool.acquire_write().await?;
        let mut transaction = connection.begin_with("BEGIN IMMEDIATE").await?;
        stravia_sql::query("UPDATE counter SET value = value + 1")
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        connection.finish().await
    });
    ready.await.unwrap();

    let visible: i64 = stravia_sql::query_scalar("SELECT value FROM counter")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        visible, 0,
        "WAL readers must retain their committed snapshot"
    );
    // 故意让首笔写入超过隔离测试库的 busy_timeout；协调不能依赖 SQLite 重试。
    let early = tokio::time::timeout(Duration::from_millis(250), &mut second).await;
    transaction.commit().await.unwrap();
    first.finish().await.unwrap();
    match early {
        Ok(result) => result.unwrap(),
        Err(_) => second.await.unwrap(),
    }
    .expect("both coordinated writes must commit instead of failing SQLITE_BUSY");

    let committed: i64 = stravia_sql::query_scalar("SELECT value FROM counter")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(committed, 2);
    pool.close().await;
}

#[tokio::test]
async fn cancelled_write_holds_admission_until_worker_rollback_finishes() {
    let directory = tempfile::tempdir().unwrap();
    let pool = isolated_pool(directory.path()).await;
    stravia_sql::query("CREATE TABLE counter (value INTEGER NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    stravia_sql::query("INSERT INTO counter VALUES (0)")
        .execute(&pool)
        .await
        .unwrap();

    let entered = std::sync::Arc::new(tokio::sync::Notify::new());
    let waiting = entered.notified();
    let worker_entered = entered.clone();
    let (release, worker_release) = std::sync::mpsc::channel();
    let first_pool = pool.clone();
    let first = tokio::spawn(async move {
        let mut connection = first_pool.acquire_write().await.unwrap();
        let mut transaction = connection.begin_with("BEGIN IMMEDIATE").await.unwrap();
        {
            let mut handle = transaction.lock_handle().await.unwrap();
            let mut release = Some(worker_release);
            handle.set_progress_handler(1, move || {
                if let Some(release) = release.take() {
                    worker_entered.notify_one();
                    // 在真实 SQLite worker 内停住语句；发送端释放或销毁都能解除。
                    let _released = release.recv();
                }
                true
            });
        }
        stravia_sql::query("UPDATE counter SET value = value + 100")
            .execute(&mut *transaction)
            .await
            .unwrap();
        transaction.commit().await.unwrap();
        connection.finish().await.unwrap();
    });
    tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("first write must enter the SQLite worker");
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());

    let next_pool = pool.clone();
    let mut next = tokio::spawn(async move {
        let mut connection = next_pool.acquire_write().await?;
        let mut transaction = connection.begin_with("BEGIN IMMEDIATE").await?;
        stravia_sql::query("UPDATE counter SET value = value + 1")
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        connection.finish().await
    });
    let early = tokio::time::timeout(Duration::from_millis(250), &mut next).await;
    release.send(()).unwrap();
    match early {
        Ok(result) => result.unwrap(),
        Err(_) => next.await.unwrap(),
    }
    .expect("the next write must wait for cancelled worker cleanup, not fail SQLITE_BUSY");

    let committed: i64 = stravia_sql::query_scalar("SELECT value FROM counter")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        committed, 1,
        "cancelled transaction must roll back before its successor"
    );
    pool.close().await;
}

fn assert_readonly<T: std::fmt::Debug>(result: Result<T, stravia_sql::Error>, context: &str) {
    let error = result.expect_err(context);
    assert!(
        error
            .as_database_error()
            .is_some_and(|error| error.message().contains("readonly database")),
        "{context}: {error:?}"
    );
}

#[tokio::test]
async fn lease_required_pool_rejects_every_write_outside_the_lease() {
    let directory = tempfile::tempdir().unwrap();
    // 单连接：每次归还后复用同一连接，验证租约结束后只读状态被恢复。
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .after_connect(|connection, _| {
            Box::pin(async move {
                stravia_sql::query("PRAGMA cache_size = -1234")
                    .execute(connection)
                    .await
                    .map(drop)
            })
        })
        .require_write_lease()
        .connect_with(
            SqliteConnectOptions::new()
                .filename(directory.path().join("lease-required.db"))
                .create_if_missing(true)
                .journal_mode(SqliteJournalMode::Wal),
        )
        .await
        .unwrap();
    let cache_size: i64 = stravia_sql::query_scalar("PRAGMA cache_size")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(cache_size, -1234, "caller connection setup must still run");

    let mut writer = pool.acquire_write().await.unwrap();
    stravia_sql::query("CREATE TABLE counter (value INTEGER NOT NULL)")
        .execute(&mut *writer)
        .await
        .unwrap();
    stravia_sql::query("INSERT INTO counter VALUES (0)")
        .execute(&mut *writer)
        .await
        .unwrap();
    writer.finish().await.unwrap();

    assert_readonly(
        stravia_sql::query("UPDATE counter SET value = value + 100")
            .execute(&pool)
            .await,
        "autocommit write after finish",
    );
    assert_readonly(
        pool.begin_with("BEGIN IMMEDIATE").await,
        "immediate transaction outside the lease",
    );
    let mut transaction = pool.begin().await.unwrap();
    assert_readonly(
        stravia_sql::query("UPDATE counter SET value = value + 100")
            .execute(&mut *transaction)
            .await,
        "deferred transaction write outside the lease",
    );
    transaction.rollback().await.unwrap();

    let mut abandoned = pool.acquire_write().await.unwrap();
    stravia_sql::query("UPDATE counter SET value = value + 1")
        .execute(&mut *abandoned)
        .await
        .unwrap();
    // 未 finish 的错误或取消路径同样必须在归还前恢复只读。
    drop(abandoned);
    assert_readonly(
        stravia_sql::query("UPDATE counter SET value = value + 100")
            .execute(&pool)
            .await,
        "autocommit write after an abandoned lease",
    );

    let mut external = stravia_sql::sqlite::SqliteConnection::connect_with(&pool.connect_options())
        .await
        .unwrap();
    stravia_sql::query("UPDATE counter SET value = value + 10")
        .execute(&mut external)
        .await
        .expect("connections outside the pool keep their own write mode");
    external.close().await.unwrap();

    let committed: i64 = stravia_sql::query_scalar("SELECT value FROM counter")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(committed, 11);
    pool.close().await;
}
