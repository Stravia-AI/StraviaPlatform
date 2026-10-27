//! Project SQL execution boundary. Query builders and drivers remain upstream SQLx.
//! Pool, connection and transaction ownership makes diagnostics automatic without
//! annotations at business query call sites. Driver-internal migration/control SQL
//! and row decoding performed by query_as/query_scalar are deliberately not claimed.
//! `query_id` is SHA-256 of the exact, unnormalised SQL template UTF-8 bytes; bind
//! values are never read for identity. It is neither a business call-site ID nor a
//! Prometheus label. Dynamic query shape/literal changes produce different IDs.
//! Stream timing covers the consumed driver stream lifetime, including consumer
//! backpressure, rather than claiming database-server CPU time.
//! Runtime SQLite writes explicitly acquire a pool-owned write lease; ordinary
//! reads and upstream-owned migrations do not acquire the writer gate.

use futures::{future::BoxFuture, stream::BoxStream};
use sqlx_native::{ConnectOptions as _, Connection as NativeConnection};
pub use sqlx_native::{Database, Either, Error, Execute, Executor};
use std::{
    fmt,
    future::Future,
    ops::{Deref, DerefMut},
    sync::Arc,
    time::Duration,
};

pub use sqlx_native::*;
mod diagnostics;
mod write;
use write::Lease;
pub use write::WriteConnection;

pub type SqlitePool = Pool<Sqlite>;
pub type PgPool = Pool<Postgres>;
pub type SqliteConnection<'c> = ConnectionHandle<'c, Sqlite>;
pub type PgConnection<'c> = ConnectionHandle<'c, Postgres>;

/// Wall-clock phase spans for explicit transaction wrappers.
///
/// Deliberately never entered: entering would reparent the wrapper's pool/query
/// spans and let SQLite workers keep a phase span alive beyond its future.
fn transaction_begin_phase<DB: Database>(pool: &'static str) -> diagnostics::Phase {
    diagnostics::Phase::new(tracing::info_span!(
        target: "stravia::perf",
        "sql.transaction.begin",
        backend = DB::NAME,
        pool = pool,
        status = tracing::field::Empty,
        error_kind = tracing::field::Empty,
    ))
}

fn transaction_commit_phase<DB: Database>(pool: &'static str) -> diagnostics::Phase {
    diagnostics::Phase::new(tracing::info_span!(
        target: "stravia::perf",
        "sql.transaction.commit",
        backend = DB::NAME,
        pool = pool,
        status = tracing::field::Empty,
        error_kind = tracing::field::Empty,
    ))
}

fn transaction_rollback_phase<DB: Database>(pool: &'static str) -> diagnostics::Phase {
    diagnostics::Phase::new(tracing::info_span!(
        target: "stravia::perf",
        "sql.transaction.rollback",
        backend = DB::NAME,
        pool = pool,
        status = tracing::field::Empty,
        error_kind = tracing::field::Empty,
    ))
}

pub mod sqlite {
    pub use sqlx_native::sqlite::*;
    pub type SqlitePoolOptions = crate::PoolOptions<sqlx_native::Sqlite>;
    pub type SqliteConnection<'c> = crate::ConnectionHandle<'c, sqlx_native::Sqlite>;
    pub type SqlitePool = crate::Pool<sqlx_native::Sqlite>;
}
pub mod postgres {
    pub use sqlx_native::postgres::*;
    pub type PgPoolOptions = crate::PoolOptions<sqlx_native::Postgres>;
    pub type PgConnection<'c> = crate::ConnectionHandle<'c, sqlx_native::Postgres>;
    pub type PgPool = crate::Pool<sqlx_native::Postgres>;
}
pub mod pool {
    pub use crate::{Pool, PoolConnection, PoolOptions};
    pub use sqlx_native::pool::{CloseEvent, PoolConnectionMetadata};
}

/// A low-cardinality name identifies the role, never the database URL.
pub struct Pool<DB: Database> {
    inner: sqlx_native::Pool<DB>,
    name: &'static str,
    write_gate: Option<Arc<tokio::sync::Mutex<()>>>,
}
impl<DB: Database> Clone for Pool<DB> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            name: self.name,
            write_gate: self.write_gate.clone(),
        }
    }
}
impl<DB: Database> fmt::Debug for Pool<DB> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pool")
            .field("backend", &DB::NAME)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}
impl<DB: Database> Pool<DB> {
    fn wrap(inner: sqlx_native::Pool<DB>) -> Self {
        Self {
            inner,
            name: "main",
            write_gate: (DB::NAME == <Sqlite as Database>::NAME)
                .then(|| Arc::new(tokio::sync::Mutex::new(()))),
        }
    }
    pub fn named(mut self, name: &'static str) -> Self {
        self.name = name;
        self
    }
    pub async fn connect(url: &str) -> Result<Self, Error> {
        sqlx_native::Pool::<DB>::connect(url).await.map(Self::wrap)
    }
    pub async fn connect_with(
        options: <DB::Connection as NativeConnection>::Options,
    ) -> Result<Self, Error> {
        sqlx_native::Pool::<DB>::connect_with(options)
            .await
            .map(Self::wrap)
    }
    pub fn connect_lazy(url: &str) -> Result<Self, Error> {
        sqlx_native::Pool::<DB>::connect_lazy(url).map(Self::wrap)
    }
    pub fn connect_lazy_with(options: <DB::Connection as NativeConnection>::Options) -> Self {
        Self::wrap(sqlx_native::Pool::<DB>::connect_lazy_with(options))
    }
    #[track_caller]
    pub fn acquire(&self) -> impl Future<Output = Result<PoolConnection<DB>, Error>> + Send + '_ {
        let acquisition = diagnostics::acquire_named(&self.inner, self.name);
        let name = self.name;
        async move {
            acquisition.await.map(|connection| PoolConnection {
                handle: ConnectionHandle {
                    inner: ConnectionInner::Pooled(connection),
                    name,
                },
            })
        }
    }
    /// 在取得连接前协调同池 SQLite 写入；PostgreSQL 不加全局锁。
    ///
    /// 租约只覆盖数据库阶段，不能跨文件、网络 I/O 或再次申请同池写租约。
    /// 正常路径应调用 `WriteConnection::finish`，取消路径自动等候驱动清理。
    /// SQLite 租约须在 Tokio runtime 中获取，取消后的清理继续由该 runtime 执行。
    pub async fn acquire_write(&self) -> Result<WriteConnection<DB>, Error> {
        let guard = match &self.write_gate {
            Some(gate) => Some(
                tracing::Instrument::instrument(
                    Arc::clone(gate).lock_owned(),
                    tracing::info_span!(target: "stravia::perf", "sqlite.write_gate.wait"),
                )
                .await,
            ),
            None => None,
        };
        let Some(guard) = guard else {
            let connection = self.acquire().await?;
            return Ok(WriteConnection::new(connection, None));
        };
        // 只有取得 mutex 后才开始 hold 计时。租约可以跨越请求的 poll 生命周期，
        // 因此不进入此 span，并让它严格随协调权释放而关闭。
        let hold = diagnostics::Phase::new(tracing::info_span!(
            target: "stravia::perf",
            parent: None,
            "sqlite.write_gate.hold",
            causal_registry_parent_id = tracing::Span::current().id().map_or(0, |id| id.into_u64()),
            backend = DB::NAME,
            pool = self.name,
            status = tracing::field::Empty,
            error_kind = tracing::field::Empty,
        ));
        let mut lease = Lease::new(guard, hold, self.name);
        match self.acquire().await {
            Ok(connection) => Ok(WriteConnection::new(connection, Some(lease))),
            Err(error) => {
                lease.fail(&error);
                // 先释放协调权，再关闭 hold span。
                drop(lease);
                Err(error)
            }
        }
    }
    #[track_caller]
    pub fn begin(
        &self,
    ) -> impl Future<Output = Result<Transaction<'static, DB>, Error>> + Send + '_ {
        self.begin_inner(None)
    }
    #[track_caller]
    pub fn begin_with(
        &self,
        statement: impl SqlSafeStr,
    ) -> impl Future<Output = Result<Transaction<'static, DB>, Error>> + Send + '_ {
        self.begin_inner(Some(statement.into_sql_str()))
    }
    #[track_caller]
    fn begin_inner(
        &self,
        statement: Option<SqlStr>,
    ) -> impl Future<Output = Result<Transaction<'static, DB>, Error>> + Send + '_ {
        let acquisition = diagnostics::acquire_named(&self.inner, self.name);
        let name = self.name;
        async move {
            let mut phase = transaction_begin_phase::<DB>(name);
            let result = match acquisition.await {
                Ok(connection) => match sqlx_native::Transaction::begin(
                    sqlx_native::pool::MaybePoolConnection::PoolConnection(connection),
                    statement,
                )
                .await
                {
                    Ok(transaction) => Ok(Transaction {
                        handle: ConnectionHandle {
                            inner: ConnectionInner::Transaction(transaction),
                            name,
                        },
                    }),
                    Err(error) => Err(error),
                },
                Err(error) => Err(error),
            };
            phase.finish(result.as_ref().err());
            result
        }
    }
    pub fn close(&self) -> impl Future<Output = ()> + '_ {
        self.inner.close()
    }
    pub fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }
    pub fn size(&self) -> u32 {
        self.inner.size()
    }
    pub fn num_idle(&self) -> usize {
        self.inner.num_idle()
    }
    pub fn connect_options(&self) -> Arc<<DB::Connection as NativeConnection>::Options> {
        self.inner.connect_options()
    }
    /// Native migration entry point only: SQLx's internal migration queries bypass
    /// this executor and are not exported as observed application queries.
    pub fn migration_pool(&self) -> &sqlx_native::Pool<DB> {
        &self.inner
    }
}

/// Samples only borrowed pools; no registry prolongs pool or connection lifetime.
pub fn sample_pool<DB: Database>(pool: &Pool<DB>, pool_name: &'static str) {
    diagnostics::sample_pool(&pool.inner, pool_name);
}

pub struct PoolOptions<DB: Database>(sqlx_native::pool::PoolOptions<DB>);
impl<DB: Database> Default for PoolOptions<DB> {
    fn default() -> Self {
        Self::new()
    }
}
impl<DB: Database> PoolOptions<DB> {
    pub fn new() -> Self {
        Self(sqlx_native::pool::PoolOptions::new())
    }
    pub fn max_connections(self, value: u32) -> Self {
        Self(self.0.max_connections(value))
    }
    pub fn min_connections(self, value: u32) -> Self {
        Self(self.0.min_connections(value))
    }
    pub fn acquire_timeout(self, value: Duration) -> Self {
        Self(self.0.acquire_timeout(value))
    }
    pub fn idle_timeout(self, value: impl Into<Option<Duration>>) -> Self {
        Self(self.0.idle_timeout(value))
    }
    pub fn max_lifetime(self, value: impl Into<Option<Duration>>) -> Self {
        Self(self.0.max_lifetime(value))
    }
    pub fn test_before_acquire(self, value: bool) -> Self {
        Self(self.0.test_before_acquire(value))
    }
    pub fn acquire_time_level(self, value: log::LevelFilter) -> Self {
        Self(self.0.acquire_time_level(value))
    }
    /// SQLx connection initialization callbacks use native driver connections.
    /// Their driver-internal setup statements are outside application diagnostics.
    pub fn after_connect<F>(self, callback: F) -> Self
    where
        F: for<'c> Fn(
                &'c mut DB::Connection,
                sqlx_native::pool::PoolConnectionMetadata,
            ) -> BoxFuture<'c, Result<(), Error>>
            + 'static
            + Send
            + Sync,
    {
        Self(self.0.after_connect(callback))
    }
    pub async fn connect(self, url: &str) -> Result<Pool<DB>, Error> {
        self.0.connect(url).await.map(Pool::wrap)
    }
    pub async fn connect_with(
        self,
        options: <DB::Connection as NativeConnection>::Options,
    ) -> Result<Pool<DB>, Error> {
        self.0.connect_with(options).await.map(Pool::wrap)
    }
    pub fn connect_lazy(self, url: &str) -> Result<Pool<DB>, Error> {
        self.0.connect_lazy(url).map(Pool::wrap)
    }
    pub fn connect_lazy_with(
        self,
        options: <DB::Connection as NativeConnection>::Options,
    ) -> Pool<DB> {
        Pool::wrap(self.0.connect_lazy_with(options))
    }
}

// The lifetime belongs to a native SQLx transaction, not to a stored fabricated
// reference. Every variant safely borrows its original driver's connection.
enum ConnectionInner<'c, DB: Database> {
    Direct(DB::Connection),
    Pooled(sqlx_native::pool::PoolConnection<DB>),
    Transaction(sqlx_native::Transaction<'c, DB>),
}
pub struct ConnectionHandle<'c, DB: Database> {
    inner: ConnectionInner<'c, DB>,
    name: &'static str,
}
impl<DB: Database> fmt::Debug for ConnectionHandle<'_, DB> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectionHandle")
            .field("backend", &DB::NAME)
            .field("pool", &self.name)
            .finish_non_exhaustive()
    }
}
impl<DB: Database> ConnectionHandle<'_, DB> {
    fn raw(&mut self) -> &mut DB::Connection {
        match &mut self.inner {
            ConnectionInner::Direct(connection) => connection,
            ConnectionInner::Pooled(connection) => connection,
            ConnectionInner::Transaction(transaction) => transaction,
        }
    }
    pub async fn begin(&mut self) -> Result<Transaction<'_, DB>, Error> {
        let name = self.name;
        let mut phase = transaction_begin_phase::<DB>(name);
        let result = match self.raw().begin().await {
            Ok(transaction) => Ok(Transaction {
                handle: ConnectionHandle {
                    inner: ConnectionInner::Transaction(transaction),
                    name,
                },
            }),
            Err(error) => Err(error),
        };
        phase.finish(result.as_ref().err());
        result
    }
    pub fn begin_with(
        &mut self,
        statement: impl SqlSafeStr,
    ) -> impl Future<Output = Result<Transaction<'_, DB>, Error>> + Send + '_ {
        let name = self.name;
        let future = self.raw().begin_with(statement.into_sql_str());
        async move {
            let mut phase = transaction_begin_phase::<DB>(name);
            let result = match future.await {
                Ok(transaction) => Ok(Transaction {
                    handle: ConnectionHandle {
                        inner: ConnectionInner::Transaction(transaction),
                        name,
                    },
                }),
                Err(error) => Err(error),
            };
            phase.finish(result.as_ref().err());
            result
        }
    }
    pub fn close_on_drop(&mut self) {
        if let ConnectionInner::Pooled(connection) = &mut self.inner {
            connection.close_on_drop();
        }
    }
    pub async fn ping(&mut self) -> Result<(), Error> {
        self.raw().ping().await
    }
    pub async fn close(self) -> Result<(), Error> {
        let name = self.name;
        match self.inner {
            ConnectionInner::Direct(connection) => connection.close().await,
            ConnectionInner::Pooled(connection) => connection.close().await,
            ConnectionInner::Transaction(transaction) => {
                let mut phase = transaction_rollback_phase::<DB>(name);
                let result = transaction.rollback().await;
                phase.finish(result.as_ref().err());
                result
            }
        }
    }
}
impl<DB: Database> ConnectionHandle<'static, DB> {
    pub async fn connect(url: &str) -> Result<Self, Error> {
        DB::Connection::connect(url).await.map(|connection| Self {
            inner: ConnectionInner::Direct(connection),
            name: "direct",
        })
    }
    pub async fn connect_with(
        options: &<DB::Connection as NativeConnection>::Options,
    ) -> Result<Self, Error> {
        options.connect().await.map(|connection| Self {
            inner: ConnectionInner::Direct(connection),
            name: "direct",
        })
    }
}
impl ConnectionHandle<'_, Sqlite> {
    pub async fn lock_handle(
        &mut self,
    ) -> Result<sqlx_native::sqlite::LockedSqliteHandle<'_>, Error> {
        self.raw().lock_handle().await
    }
    #[cfg(feature = "sqlite-deserialize")]
    pub async fn serialize(
        &mut self,
        schema: Option<&str>,
    ) -> Result<sqlx_native::sqlite::SqliteOwnedBuf, Error> {
        self.raw().serialize(schema).await
    }
    #[cfg(feature = "sqlite-deserialize")]
    pub async fn deserialize(
        &mut self,
        schema: Option<&str>,
        data: sqlx_native::sqlite::SqliteOwnedBuf,
        read_only: bool,
    ) -> Result<(), Error> {
        self.raw().deserialize(schema, data, read_only).await
    }
}

#[derive(Debug)]
pub struct PoolConnection<DB: Database> {
    handle: ConnectionHandle<'static, DB>,
}
impl<DB: Database> Deref for PoolConnection<DB> {
    type Target = ConnectionHandle<'static, DB>;
    fn deref(&self) -> &Self::Target {
        &self.handle
    }
}
impl<DB: Database> DerefMut for PoolConnection<DB> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.handle
    }
}
#[derive(Debug)]
pub struct Transaction<'c, DB: Database> {
    handle: ConnectionHandle<'c, DB>,
}
impl<'c, DB: Database> Deref for Transaction<'c, DB> {
    type Target = ConnectionHandle<'c, DB>;
    fn deref(&self) -> &Self::Target {
        &self.handle
    }
}
impl<DB: Database> DerefMut for Transaction<'_, DB> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.handle
    }
}
impl<DB: Database> Transaction<'_, DB> {
    pub async fn commit(self) -> Result<(), Error> {
        let name = self.handle.name;
        let mut phase = transaction_commit_phase::<DB>(name);
        let result = match self.handle.inner {
            ConnectionInner::Transaction(transaction) => transaction.commit().await,
            _ => unreachable!("transaction handle invariant"),
        };
        phase.finish(result.as_ref().err());
        result
    }
    pub async fn rollback(self) -> Result<(), Error> {
        let name = self.handle.name;
        let mut phase = transaction_rollback_phase::<DB>(name);
        let result = match self.handle.inner {
            ConnectionInner::Transaction(transaction) => transaction.rollback().await,
            _ => unreachable!("transaction handle invariant"),
        };
        phase.finish(result.as_ref().err());
        result
    }
}

macro_rules! executor {
    ($db:ty) => {
        impl<'c> Executor<'c> for &'c Pool<$db> {
            type Database = $db;
            fn fetch_many<'e, 'q: 'e, Q>(
                self,
                query: Q,
            ) -> BoxStream<
                'e,
                Result<Either<<$db as Database>::QueryResult, <$db as Database>::Row>, Error>,
            >
            where
                'c: 'e,
                Q: 'q + Execute<'q, $db>,
            {
                diagnostics::observe_named(&self.inner, self.name).fetch_many(query)
            }
            fn fetch_optional<'e, 'q: 'e, Q>(
                self,
                query: Q,
            ) -> BoxFuture<'e, Result<Option<<$db as Database>::Row>, Error>>
            where
                'c: 'e,
                Q: 'q + Execute<'q, $db>,
            {
                diagnostics::observe_named(&self.inner, self.name).fetch_optional(query)
            }
            fn prepare_with<'e>(
                self,
                sql: SqlStr,
                parameters: &'e [<$db as Database>::TypeInfo],
            ) -> BoxFuture<'e, Result<<$db as Database>::Statement, Error>>
            where
                'c: 'e,
            {
                (&self.inner).prepare_with(sql, parameters)
            }
            fn describe<'e>(self, sql: SqlStr) -> BoxFuture<'e, Result<Describe<$db>, Error>>
            where
                'c: 'e,
            {
                (&self.inner).describe(sql)
            }
            delegate_executor_methods!($db, pool);
        }
        impl<'c, 'owned> Executor<'c> for &'c mut ConnectionHandle<'owned, $db> {
            type Database = $db;
            fn fetch_many<'e, 'q: 'e, Q>(
                self,
                query: Q,
            ) -> BoxStream<
                'e,
                Result<Either<<$db as Database>::QueryResult, <$db as Database>::Row>, Error>,
            >
            where
                'c: 'e,
                Q: 'q + Execute<'q, $db>,
            {
                let name = self.name;
                diagnostics::observe_named(self.raw(), name).fetch_many(query)
            }
            fn fetch_optional<'e, 'q: 'e, Q>(
                self,
                query: Q,
            ) -> BoxFuture<'e, Result<Option<<$db as Database>::Row>, Error>>
            where
                'c: 'e,
                Q: 'q + Execute<'q, $db>,
            {
                let name = self.name;
                diagnostics::observe_named(self.raw(), name).fetch_optional(query)
            }
            fn prepare_with<'e>(
                self,
                sql: SqlStr,
                parameters: &'e [<$db as Database>::TypeInfo],
            ) -> BoxFuture<'e, Result<<$db as Database>::Statement, Error>>
            where
                'c: 'e,
            {
                self.raw().prepare_with(sql, parameters)
            }
            fn describe<'e>(self, sql: SqlStr) -> BoxFuture<'e, Result<Describe<$db>, Error>>
            where
                'c: 'e,
            {
                self.raw().describe(sql)
            }
            delegate_executor_methods!($db, connection);
        }
    };
}

// Override defaults so the disabled path returns the upstream future/stream
// directly, rather than layering an additional collect/filter allocation.
macro_rules! delegated {
    (pool, $self:ident, $method:ident, $query:ident) => {
        diagnostics::observe_named(&$self.inner, $self.name).$method($query)
    };
    (connection, $self:ident, $method:ident, $query:ident) => {{
        let name = $self.name;
        diagnostics::observe_named($self.raw(), name).$method($query)
    }};
}
macro_rules! delegate_executor_methods {
    ($db:ty, $kind:ident) => {
        fn execute<'e, 'q: 'e, Q>(
            self,
            query: Q,
        ) -> BoxFuture<'e, Result<<$db as Database>::QueryResult, Error>>
        where
            'c: 'e,
            Q: 'q + Execute<'q, $db>,
        {
            delegated!($kind, self, execute, query)
        }
        fn execute_many<'e, 'q: 'e, Q>(
            self,
            query: Q,
        ) -> BoxStream<'e, Result<<$db as Database>::QueryResult, Error>>
        where
            'c: 'e,
            Q: 'q + Execute<'q, $db>,
        {
            delegated!($kind, self, execute_many, query)
        }
        fn fetch<'e, 'q: 'e, Q>(
            self,
            query: Q,
        ) -> BoxStream<'e, Result<<$db as Database>::Row, Error>>
        where
            'c: 'e,
            Q: 'q + Execute<'q, $db>,
        {
            delegated!($kind, self, fetch, query)
        }
        fn fetch_all<'e, 'q: 'e, Q>(
            self,
            query: Q,
        ) -> BoxFuture<'e, Result<Vec<<$db as Database>::Row>, Error>>
        where
            'c: 'e,
            Q: 'q + Execute<'q, $db>,
        {
            delegated!($kind, self, fetch_all, query)
        }
        fn fetch_one<'e, 'q: 'e, Q>(
            self,
            query: Q,
        ) -> BoxFuture<'e, Result<<$db as Database>::Row, Error>>
        where
            'c: 'e,
            Q: 'q + Execute<'q, $db>,
        {
            delegated!($kind, self, fetch_one, query)
        }
    };
}
executor!(Sqlite);
executor!(Postgres);

// Only migration control delegates directly to the native driver. No Acquire or
// Connection implementation exposes an accidental unobserved application path.
impl<DB: Database> sqlx_native::migrate::Migrate for ConnectionHandle<'_, DB>
where
    DB::Connection: sqlx_native::migrate::Migrate,
{
    fn create_schema_if_not_exists<'e>(
        &'e mut self,
        name: &'e str,
    ) -> BoxFuture<'e, Result<(), sqlx_native::migrate::MigrateError>> {
        self.raw().create_schema_if_not_exists(name)
    }
    fn ensure_migrations_table<'e>(
        &'e mut self,
        name: &'e str,
    ) -> BoxFuture<'e, Result<(), sqlx_native::migrate::MigrateError>> {
        self.raw().ensure_migrations_table(name)
    }
    fn dirty_version<'e>(
        &'e mut self,
        name: &'e str,
    ) -> BoxFuture<'e, Result<Option<i64>, sqlx_native::migrate::MigrateError>> {
        self.raw().dirty_version(name)
    }
    fn list_applied_migrations<'e>(
        &'e mut self,
        name: &'e str,
    ) -> BoxFuture<
        'e,
        Result<Vec<sqlx_native::migrate::AppliedMigration>, sqlx_native::migrate::MigrateError>,
    > {
        self.raw().list_applied_migrations(name)
    }
    fn lock(&mut self) -> BoxFuture<'_, Result<(), sqlx_native::migrate::MigrateError>> {
        self.raw().lock()
    }
    fn unlock(&mut self) -> BoxFuture<'_, Result<(), sqlx_native::migrate::MigrateError>> {
        self.raw().unlock()
    }
    fn apply<'e>(
        &'e mut self,
        name: &'e str,
        migration: &'e sqlx_native::migrate::Migration,
    ) -> BoxFuture<'e, Result<Duration, sqlx_native::migrate::MigrateError>> {
        self.raw().apply(name, migration)
    }
    fn revert<'e>(
        &'e mut self,
        name: &'e str,
        migration: &'e sqlx_native::migrate::Migration,
    ) -> BoxFuture<'e, Result<Duration, sqlx_native::migrate::MigrateError>> {
        self.raw().revert(name, migration)
    }
    fn skip<'e>(
        &'e mut self,
        name: &'e str,
        migration: &'e sqlx_native::migrate::Migration,
    ) -> BoxFuture<'e, Result<(), sqlx_native::migrate::MigrateError>> {
        self.raw().skip(name, migration)
    }
}
