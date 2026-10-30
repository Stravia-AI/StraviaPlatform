//! Privacy-preserving diagnostics at SQLx's driver execution boundary.
//! No SQL text, bind values, row contents, or formatted errors cross this boundary.
//! Row mapping by `query_as`/`query_scalar`, driver-internal transaction control and
//! migrations are outside this boundary; successful driver execution is not an
//! assertion that later application decoding succeeded.

use futures::{FutureExt, Stream, StreamExt, TryStreamExt, future::BoxFuture, stream::BoxStream};
use sha2::{Digest, Sha256};
use sqlx::{Database, Either, Error, Execute, Executor, Pool};
use sqlx_native as sqlx;
use std::{
    future::Future,
    panic::Location,
    pin::Pin,
    sync::atomic::{AtomicU64, Ordering},
    task::{Context, Poll},
};
use tracing::{Instrument, Span};

struct PoolCounters {
    waiting: AtomicU64,
    failed: AtomicU64,
    cancelled: AtomicU64,
}
impl PoolCounters {
    const fn new() -> Self {
        Self {
            waiting: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            cancelled: AtomicU64::new(0),
        }
    }
}
static POOLS: [PoolCounters; 6] = [const { PoolCounters::new() }; 6];

fn counters<DB: Database>(name: &str) -> &'static PoolCounters {
    let backend = usize::from(DB::NAME == "PostgreSQL") * 3;
    let role = match name {
        "main" => 0,
        "artifact_lock" => 1,
        _ => 2,
    };
    &POOLS[backend + role]
}

#[derive(Clone, Copy, Debug)]
struct Origin {
    location: &'static Location<'static>,
    pool: &'static str,
}

/// Use only static, low-cardinality pool names (for example `artifact_lock`).
#[track_caller]
#[inline]
pub fn observe_named<E>(executor: E, pool: &'static str) -> Observed<E> {
    Observed {
        executor,
        origin: Origin {
            location: Location::caller(),
            pool,
        },
    }
}

#[derive(Debug)]
pub struct Observed<E> {
    executor: E,
    origin: Origin,
}

fn enabled() -> bool {
    tracing::enabled!(target: "stravia::sql", tracing::Level::INFO)
}

struct Completion {
    span: Span,
    finished: bool,
}

impl Completion {
    fn query<DB: Database>(origin: Origin) -> Self {
        Self {
            span: tracing::info_span!(target: "stravia::sql", "sql.query",
            operation = "sql.query", query_id = tracing::field::Empty,
            query_identity = "sha256_template", source_scope = "unavailable",
            backend = DB::NAME, pool = origin.pool,
            execution_scope = "driver", status = "running", error_kind = tracing::field::Empty,
            error_code = tracing::field::Empty, rows_returned = tracing::field::Empty,
            rows_affected = tracing::field::Empty),
            finished: false,
        }
    }

    fn finish(&mut self, error: Option<&Error>) {
        if let Some(error) = error {
            self.span.record("error_kind", error_kind(error));
            if let Error::Database(error) = error
                && let Some(code) = error.code()
            {
                // SQLSTATE is exactly five uppercase ASCII letters/digits. SQLite
                // extended result codes are unsigned 32-bit decimal integers.
                // Only accept the actual driver's format, never arbitrary strings.
                let backend = error
                    .try_downcast_ref::<sqlx::postgres::PgDatabaseError>()
                    .is_some();
                let valid = if backend {
                    code.len() == 5
                        && code
                            .bytes()
                            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
                } else if error
                    .try_downcast_ref::<sqlx::sqlite::SqliteError>()
                    .is_some()
                {
                    !code.is_empty()
                        && code.len() <= 10
                        && code.bytes().all(|c| c.is_ascii_digit())
                        && code.parse::<u32>().is_ok()
                } else {
                    false
                };
                if valid {
                    self.span.record("error_code", code.as_ref());
                }
            }
        }
        self.finished = true;
        // Status is the terminal publication boundary. SQLite workers may retain
        // cloned spans beyond execution, so consumers must not wait for on_close.
        self.span.record(
            "status",
            if error.is_some() {
                "error"
            } else {
                "completed"
            },
        );
    }
}

impl Drop for Completion {
    fn drop(&mut self) {
        if !self.finished {
            self.span.record("status", "cancelled");
        }
    }
}

/// Terminal bookkeeping for a wall-clock `stravia::perf` phase span.
///
/// The span is deliberately never entered: SQLite workers may clone an entered
/// span beyond the future that produced it, which would stretch a phase past its
/// real lifetime. Status is published when the phase ends; a phase dropped
/// before its explicit terminal record is reported as cancelled, never completed.
#[derive(Debug)]
pub(super) struct Phase {
    span: Span,
    finished: bool,
}

impl Phase {
    pub(super) fn new(span: Span) -> Self {
        Self {
            span,
            finished: false,
        }
    }

    pub(super) fn span(&self) -> &Span {
        &self.span
    }

    pub(super) fn complete(&mut self) {
        if !self.finished {
            self.finished = true;
            self.span.record("status", "completed");
        }
    }

    /// Records a failure, including after a drop-path phase was marked cancelled:
    /// the cleanup error is the more specific terminal outcome.
    pub(super) fn fail(&mut self, error: &Error) {
        self.finished = true;
        self.span.record("error_kind", error_kind(error));
        self.span.record("status", "error");
    }

    pub(super) fn finish(&mut self, error: Option<&Error>) {
        match error {
            Some(error) => self.fail(error),
            None => self.complete(),
        }
    }

    pub(super) fn cancel_if_unfinished(&mut self) {
        if !self.finished {
            self.finished = true;
            self.span.record("status", "cancelled");
        }
    }
}

impl Drop for Phase {
    fn drop(&mut self) {
        if !self.finished {
            self.span.record("status", "cancelled");
        }
    }
}

struct Identified<Q> {
    query: Q,
    span: Span,
}

impl<'q, DB: Database, Q: Execute<'q, DB>> Execute<'q, DB> for Identified<Q> {
    fn sql(self) -> sqlx::SqlStr {
        let sql = self.query.sql();
        let digest = Sha256::digest(sql.as_str().as_bytes());
        let mut hex = [0_u8; 64];
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        for (index, byte) in digest.iter().enumerate() {
            hex[index * 2] = DIGITS[usize::from(byte >> 4)];
            hex[index * 2 + 1] = DIGITS[usize::from(byte & 15)];
        }
        // Every byte above comes from the ASCII-only digit table.
        if let Ok(id) = std::str::from_utf8(&hex) {
            self.span.record("query_id", id);
        }
        sql
    }

    fn statement(&self) -> Option<&DB::Statement> {
        self.query.statement()
    }
    fn take_arguments(&mut self) -> Result<Option<DB::Arguments>, sqlx::error::BoxDynError> {
        self.query.take_arguments()
    }
    fn persistent(&self) -> bool {
        self.query.persistent()
    }
}

pub(super) fn error_kind(error: &Error) -> &'static str {
    match error {
        Error::Database(_) => "database",
        Error::PoolTimedOut => "pool_timeout",
        Error::PoolClosed => "pool_closed",
        Error::Io(_) => "io",
        Error::Tls(_) => "tls",
        Error::Protocol(_) => "protocol",
        Error::RowNotFound => "row_not_found",
        Error::Encode(_) => "encode",
        Error::Decode(_) | Error::ColumnDecode { .. } => "decode",
        Error::Configuration(_) | Error::InvalidArgument(_) => "configuration",
        Error::WorkerCrashed => "worker_crashed",
        _ => "other",
    }
}

/// Sample a pool owned by the caller; no global pool clone prolongs its lifetime.
/// Counters aggregate instrumented acquisitions for the same backend and static
/// pool role; size and idle describe this instance, not every same-role instance.
pub fn sample_pool<DB: Database>(pool: &Pool<DB>, pool_name: &'static str) {
    let counters = counters::<DB>(pool_name);
    tracing::info!(target: "stravia::sql", phase = "pool_snapshot", backend = DB::NAME,
        pool = pool_name, pool_size = pool.size(), pool_idle = pool.num_idle() as u64,
        pool_waiting = counters.waiting.load(Ordering::Relaxed),
        pool_acquire_failed = counters.failed.load(Ordering::Relaxed),
        pool_acquire_cancelled = counters.cancelled.load(Ordering::Relaxed),
        waiting_scope = "instrumented_backend_pool_role", failed_scope = "instrumented_backend_pool_role");
}

#[track_caller]
pub fn acquire_named<'p, DB: Database>(
    pool: &'p Pool<DB>,
    pool_name: &'static str,
) -> impl Future<Output = Result<sqlx::pool::PoolConnection<DB>, Error>> + Send + 'p {
    let origin = Origin {
        location: Location::caller(),
        pool: pool_name,
    };
    acquire_origin(pool, origin)
}

async fn acquire_origin<DB: Database>(
    pool: &Pool<DB>,
    origin: Origin,
) -> Result<sqlx::pool::PoolConnection<DB>, Error> {
    if !enabled() {
        return pool.acquire().await;
    }
    let location = origin.location;
    let counters = counters::<DB>(origin.pool);
    let span = tracing::info_span!(target: "stravia::sql", "sql.pool.acquire",
            operation = "sql.pool.acquire", source_file = location.file(), source_line = location.line(),
            source_column = location.column(), backend = DB::NAME, pool = origin.pool,
            status = "running", error_kind = tracing::field::Empty, error_code = tracing::field::Empty,
            pool_size = pool.size(), pool_idle = pool.num_idle() as u64,
            pool_waiting = tracing::field::Empty,
            pool_acquire_failed = tracing::field::Empty,
            pool_acquire_cancelled = tracing::field::Empty,
            waiting_scope = "instrumented_backend_pool_role");
    struct Waiting {
        counters: &'static PoolCounters,
        span: Span,
        settled: bool,
    }
    impl Drop for Waiting {
        fn drop(&mut self) {
            self.counters.waiting.fetch_sub(1, Ordering::Relaxed);
            if !self.settled {
                self.counters.cancelled.fetch_add(1, Ordering::Relaxed);
            }
            self.span.record(
                "pool_waiting",
                self.counters.waiting.load(Ordering::Relaxed),
            );
            self.span.record(
                "pool_acquire_cancelled",
                self.counters.cancelled.load(Ordering::Relaxed),
            );
        }
    }
    let mut completion = Completion {
        span,
        finished: false,
    };
    // 动态过滤可能在 enabled() 与 span 创建之间改变；计数不能依赖字段是否求值。
    let waiting_count = counters.waiting.fetch_add(1, Ordering::Relaxed) + 1;
    // Reverse drop order publishes final waiter counts before cancelled status.
    let mut waiting = Waiting {
        counters,
        span: completion.span.clone(),
        settled: false,
    };
    completion.span.record("pool_waiting", waiting_count);
    let result = pool.acquire().instrument(completion.span.clone()).await;
    waiting.settled = true;
    drop(waiting);
    completion.span.record("pool_size", pool.size());
    completion.span.record("pool_idle", pool.num_idle() as u64);
    completion
        .span
        .record("pool_waiting", counters.waiting.load(Ordering::Relaxed));
    if result.is_err() {
        counters.failed.fetch_add(1, Ordering::Relaxed);
    }
    completion.span.record(
        "pool_acquire_failed",
        counters.failed.load(Ordering::Relaxed),
    );
    completion.span.record(
        "pool_acquire_cancelled",
        counters.cancelled.load(Ordering::Relaxed),
    );
    completion.finish(result.as_ref().err());
    result
}

trait Affected: Database {
    fn affected(result: &Self::QueryResult) -> u64;
}
impl Affected for sqlx::Sqlite {
    fn affected(result: &sqlx::sqlite::SqliteQueryResult) -> u64 {
        result.rows_affected()
    }
}
impl Affected for sqlx::Postgres {
    fn affected(result: &sqlx::postgres::PgQueryResult) -> u64 {
        result.rows_affected()
    }
}

type DriverItem<DB> = Result<Either<<DB as Database>::QueryResult, <DB as Database>::Row>, Error>;
type StreamFactory<'a, DB> = Box<dyn FnOnce(Span) -> BoxStream<'a, DriverItem<DB>> + Send + 'a>;

// The factory defers the span and native stream until the first consumer poll.
// Each subsequent poll directly polls SQLx: no task, channel, row buffering or
// extra wakeup is inserted into the driver's per-row path.
struct ObservedStream<'a, DB: Affected> {
    factory: Option<StreamFactory<'a, DB>>,
    stream: Option<BoxStream<'a, DriverItem<DB>>>,
    completion: Option<Completion>,
    origin: Origin,
    rows: u64,
    affected: u64,
    finished: bool,
}

impl<'a, DB: Affected> ObservedStream<'a, DB> {
    fn wrap(
        origin: Origin,
        factory: impl FnOnce(Span) -> BoxStream<'a, DriverItem<DB>> + Send + 'a,
    ) -> BoxStream<'a, DriverItem<DB>> {
        Box::pin(Self {
            factory: Some(Box::new(factory)),
            stream: None,
            completion: None,
            origin,
            rows: 0,
            affected: 0,
            finished: false,
        })
    }
}

impl<DB: Affected> Stream for ObservedStream<'_, DB> {
    type Item = DriverItem<DB>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.finished {
            return Poll::Ready(None);
        }
        if let Some(factory) = this.factory.take() {
            let completion = Completion::query::<DB>(this.origin);
            this.stream = Some(factory(completion.span.clone()));
            this.completion = Some(completion);
        }
        let completion = this.completion.as_mut().expect("initialized on first poll");
        let entered = completion.span.enter();
        let item = this
            .stream
            .as_mut()
            .expect("initialized on first poll")
            .as_mut()
            .poll_next(cx);
        drop(entered);
        match &item {
            Poll::Ready(Some(Ok(Either::Left(result)))) => this.affected += DB::affected(result),
            Poll::Ready(Some(Ok(Either::Right(_)))) => this.rows += 1,
            _ => {}
        }
        if item.is_ready() {
            completion.span.record("rows_returned", this.rows);
            completion.span.record("rows_affected", this.affected);
        }
        match &item {
            Poll::Ready(Some(Err(error))) => {
                completion.finish(Some(error));
                this.finished = true;
            }
            Poll::Ready(None) => {
                completion.finish(None);
                this.finished = true;
            }
            _ => {}
        }
        item
    }
}

#[doc(hidden)]
pub trait Source<'c>: Executor<'c> {
    fn observed_many<'e, 'q: 'e, Q>(
        self,
        query: Q,
        origin: ObservedOrigin,
    ) -> BoxStream<'e, DriverItem<Self::Database>>
    where
        'c: 'e,
        Q: 'q + Execute<'q, Self::Database>;
    fn observed_optional<'e, 'q: 'e, Q>(
        self,
        query: Q,
        origin: ObservedOrigin,
    ) -> BoxFuture<'e, Result<Option<<Self::Database as Database>::Row>, Error>>
    where
        'c: 'e,
        Q: 'q + Execute<'q, Self::Database>;
}

#[doc(hidden)]
#[derive(Clone, Copy)]
pub struct ObservedOrigin(Origin);

macro_rules! source {
    ($db:ty, $connection:ty) => {
        impl<'c> Source<'c> for &'c mut $connection {
            fn observed_many<'e, 'q: 'e, Q>(
                self,
                query: Q,
                origin: ObservedOrigin,
            ) -> BoxStream<
                'e,
                Result<Either<<$db as Database>::QueryResult, <$db as Database>::Row>, Error>,
            >
            where
                'c: 'e,
                Q: 'q + Execute<'q, $db>,
            {
                ObservedStream::<$db>::wrap(origin.0, move |span| {
                    self.fetch_many(Identified { query, span })
                })
            }
            fn observed_optional<'e, 'q: 'e, Q>(
                self,
                query: Q,
                origin: ObservedOrigin,
            ) -> BoxFuture<'e, Result<Option<<$db as Database>::Row>, Error>>
            where
                'c: 'e,
                Q: 'q + Execute<'q, $db>,
            {
                Box::pin(async move {
                    let mut completion = Completion::query::<$db>(origin.0);
                    let query = Identified {
                        query,
                        span: completion.span.clone(),
                    };
                    let result = self
                        .fetch_optional(query)
                        .instrument(completion.span.clone())
                        .await;
                    if let Ok(row) = &result {
                        completion
                            .span
                            .record("rows_returned", u64::from(row.is_some()));
                    }
                    completion.finish(result.as_ref().err());
                    result
                })
            }
        }
        impl<'c> Source<'c> for &'c Pool<$db> {
            fn observed_many<'e, 'q: 'e, Q>(
                self,
                query: Q,
                origin: ObservedOrigin,
            ) -> BoxStream<
                'e,
                Result<Either<<$db as Database>::QueryResult, <$db as Database>::Row>, Error>,
            >
            where
                'c: 'e,
                Q: 'q + Execute<'q, $db>,
            {
                Box::pin(sqlx_core::try_stream! {
                    let mut connection = acquire_at(self, origin.0).await?;
                    let mut stream = (&mut *connection).observed_many(query, origin);
                    while let Some(item) = stream.try_next().await? {
                        r#yield!(item);
                    }
                    Ok(())
                })
            }
            fn observed_optional<'e, 'q: 'e, Q>(
                self,
                query: Q,
                origin: ObservedOrigin,
            ) -> BoxFuture<'e, Result<Option<<$db as Database>::Row>, Error>>
            where
                'c: 'e,
                Q: 'q + Execute<'q, $db>,
            {
                Box::pin(async move {
                    let mut connection = acquire_at(self, origin.0).await?;
                    (&mut *connection).observed_optional(query, origin).await
                })
            }
        }
    };
}
source!(sqlx::Sqlite, sqlx::SqliteConnection);
source!(sqlx::Postgres, sqlx::PgConnection);

fn acquire_at<DB: Database>(
    pool: &Pool<DB>,
    origin: Origin,
) -> impl Future<Output = Result<sqlx::pool::PoolConnection<DB>, Error>> + Send + '_ {
    acquire_origin(pool, origin)
}

impl<'c, E: Source<'c>> Executor<'c> for Observed<E> {
    type Database = E::Database;
    fn fetch_many<'e, 'q: 'e, Q>(
        self,
        query: Q,
    ) -> BoxStream<
        'e,
        Result<
            Either<<E::Database as Database>::QueryResult, <E::Database as Database>::Row>,
            Error,
        >,
    >
    where
        'c: 'e,
        Q: 'q + Execute<'q, E::Database>,
    {
        if !enabled() {
            return self.executor.fetch_many(query);
        }
        self.executor
            .observed_many(query, ObservedOrigin(self.origin))
    }
    fn fetch_optional<'e, 'q: 'e, Q>(
        self,
        query: Q,
    ) -> BoxFuture<'e, Result<Option<<E::Database as Database>::Row>, Error>>
    where
        'c: 'e,
        Q: 'q + Execute<'q, E::Database>,
    {
        if !enabled() {
            return self.executor.fetch_optional(query);
        }
        self.executor
            .observed_optional(query, ObservedOrigin(self.origin))
    }
    fn execute<'e, 'q: 'e, Q>(
        self,
        query: Q,
    ) -> BoxFuture<'e, Result<<E::Database as Database>::QueryResult, Error>>
    where
        'c: 'e,
        Q: 'q + Execute<'q, E::Database>,
    {
        if !enabled() {
            return self.executor.execute(query);
        }
        self.execute_many(query).try_collect().boxed()
    }
    fn execute_many<'e, 'q: 'e, Q>(
        self,
        query: Q,
    ) -> BoxStream<'e, Result<<E::Database as Database>::QueryResult, Error>>
    where
        'c: 'e,
        Q: 'q + Execute<'q, E::Database>,
    {
        if !enabled() {
            return self.executor.execute_many(query);
        }
        self.fetch_many(query)
            .try_filter_map(|item| async move {
                Ok(match item {
                    Either::Left(result) => Some(result),
                    Either::Right(_) => None,
                })
            })
            .boxed()
    }
    fn fetch<'e, 'q: 'e, Q>(
        self,
        query: Q,
    ) -> BoxStream<'e, Result<<E::Database as Database>::Row, Error>>
    where
        'c: 'e,
        Q: 'q + Execute<'q, E::Database>,
    {
        if !enabled() {
            return self.executor.fetch(query);
        }
        self.fetch_many(query)
            .try_filter_map(|item| async move {
                Ok(match item {
                    Either::Left(_) => None,
                    Either::Right(row) => Some(row),
                })
            })
            .boxed()
    }
    fn fetch_all<'e, 'q: 'e, Q>(
        self,
        query: Q,
    ) -> BoxFuture<'e, Result<Vec<<E::Database as Database>::Row>, Error>>
    where
        'c: 'e,
        Q: 'q + Execute<'q, E::Database>,
    {
        if !enabled() {
            return self.executor.fetch_all(query);
        }
        self.fetch(query).try_collect().boxed()
    }
    fn fetch_one<'e, 'q: 'e, Q>(
        self,
        query: Q,
    ) -> BoxFuture<'e, Result<<E::Database as Database>::Row, Error>>
    where
        'c: 'e,
        Q: 'q + Execute<'q, E::Database>,
    {
        if !enabled() {
            return self.executor.fetch_one(query);
        }
        let future = self.fetch_optional(query);
        Box::pin(async move { future.await?.ok_or(Error::RowNotFound) })
    }
    fn prepare_with<'e>(
        self,
        sql: sqlx::SqlStr,
        parameters: &'e [<E::Database as Database>::TypeInfo],
    ) -> BoxFuture<'e, Result<<E::Database as Database>::Statement, Error>>
    where
        'c: 'e,
    {
        self.executor.prepare_with(sql, parameters)
    }

    fn describe<'e>(
        self,
        sql: sqlx::SqlStr,
    ) -> BoxFuture<'e, Result<sqlx::Describe<Self::Database>, Error>>
    where
        'c: 'e,
    {
        self.executor.describe(sql)
    }
}
