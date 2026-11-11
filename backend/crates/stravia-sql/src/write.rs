use std::ops::{Deref, DerefMut};

use sqlx_core::transaction::TransactionManager as _;
use sqlx_native::Connection as _;
use tokio::{runtime::Handle, sync::OwnedMutexGuard};

use crate::{
    ConnectionHandle, Database, Error, PoolConnection, SqlSafeStr, Sqlite, WriteMode,
    diagnostics::{self, Phase},
    transaction_begin_phase, transaction_commit_phase, transaction_rollback_phase,
};

/// 持有 SQLite 写协调权及其墙钟计时 span。
///
/// 字段顺序保证先释放 mutex guard，再关闭 hold span，避免自然析构时
/// 记录的协调权释放时间早于实际释放。
#[derive(Debug)]
pub(super) struct Lease {
    _guard: OwnedMutexGuard<()>,
    hold: Phase,
    pool: &'static str,
}

impl Lease {
    pub(super) fn new(_guard: OwnedMutexGuard<()>, hold: Phase, pool: &'static str) -> Self {
        Self { _guard, hold, pool }
    }

    pub(super) fn fail(&mut self, error: &Error) {
        self.hold.fail(error);
    }
}

/// 持有连接及其 SQLite 写协调权。事务必须借用此连接，不能另从池中取连接。
///
/// 仅用于必须在事务外执行的写入（迁移期的 `PRAGMA foreign_keys`、SQLx 迁移器、
/// `sqlite3_deserialize`）；业务写入使用 [`crate::Pool::begin_write`]。
#[derive(Debug)]
pub struct WriteConnection<DB: Database> {
    connection: Option<PoolConnection<DB>>,
    lease: Option<Lease>,
    runtime: Option<Handle>,
    discard_on_drop: bool,
    write_mode: Option<WriteMode<DB>>,
    /// 由 [`WriteTransaction`] 开启且尚未提交或回滚；析构时须先向驱动排入回滚。
    transaction_open: bool,
}

impl<DB: Database> WriteConnection<DB> {
    pub(super) fn new(connection: PoolConnection<DB>, lease: Option<Lease>) -> Self {
        Self {
            connection: Some(connection),
            runtime: lease.as_ref().map(|_| Handle::current()),
            lease,
            discard_on_drop: false,
            write_mode: None,
            transaction_open: false,
        }
    }

    fn native(&mut self) -> &mut DB::Connection {
        self.connection
            .as_mut()
            .expect("live write connection")
            .handle
            .raw()
    }

    fn pool_name(&self) -> &'static str {
        self.connection
            .as_ref()
            .expect("live write connection")
            .handle
            .name
    }

    fn record_failure(&mut self, error: &Error) {
        if let Some(lease) = self.lease.as_mut() {
            lease.hold.fail(error);
        }
    }

    pub(super) fn with_write_mode(mut self, write_mode: Option<WriteMode<DB>>) -> Self {
        self.write_mode = write_mode;
        self
    }

    /// 解除池连接在租约外的只读状态。失败时连接状态不明，析构时关闭而不归还。
    pub(super) async fn enable_writes(&mut self) -> Result<(), Error> {
        let Some(write_mode) = self.write_mode else {
            return Ok(());
        };
        let connection = self.connection.as_mut().expect("live write connection");
        let result = write_mode(connection.handle.raw(), true).await;
        if let Err(error) = result {
            self.discard_on_drop = true;
            self.record_failure(&error);
            return Err(error);
        }
        Ok(())
    }

    /// 等候驱动队列中的语句及回滚完成，再归还连接并释放协调权。
    ///
    /// SQLite 的查询 future 被取消后，worker 仍可能执行语句；仅丢弃 MutexGuard
    /// 不能说明数据库写锁已释放。ping 是同一 worker 队列中的完成屏障。
    /// 屏障失败或事务仍活跃时返回错误，后台关闭任务继续持有协调权直至关闭完成。
    pub async fn finish(mut self) -> Result<(), Error> {
        if self.lease.is_none() {
            drop(self.connection.take());
            return Ok(());
        }
        let span = {
            let lease = self.lease.as_ref().expect("checked write lease");
            tracing::info_span!(
                target: "stravia::perf",
                parent: lease.hold.span(),
                "sqlite.write_gate.finish",
                backend = DB::NAME,
                pool = lease.pool,
                status = tracing::field::Empty,
                error_kind = tracing::field::Empty,
            )
        };
        let mut phase = Phase::new(span);
        let result = {
            let connection = self.connection.as_mut().expect("live write connection");
            settle(connection, self.write_mode).await
        };
        match result {
            Ok(()) => {
                phase.complete();
                // finish 只覆盖驱动屏障及事务状态检查。
                drop(phase);
                let connection = self.connection.take();
                let mut lease = self.lease.take().expect("checked write lease");
                drop(connection);
                lease.hold.complete();
                // 先释放协调权，再关闭 hold span。
                drop(lease);
                Ok(())
            }
            Err(error) => {
                phase.fail(&error);
                drop(phase);
                self.discard_on_drop = true;
                if let Some(lease) = self.lease.as_mut() {
                    lease.hold.fail(&error);
                }
                // self 析构后，cleanup 继续持有协调权直至连接实际关闭，hold span 保持打开。
                Err(error)
            }
        }
    }
}

impl<DB: Database> Deref for WriteConnection<DB> {
    type Target = ConnectionHandle<'static, DB>;

    fn deref(&self) -> &Self::Target {
        self.connection.as_ref().expect("live write connection")
    }
}

impl<DB: Database> DerefMut for WriteConnection<DB> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.connection.as_mut().expect("live write connection")
    }
}

/// 写事务。SQLite 持有同池写租约并以 `BEGIN IMMEDIATE` 开启；PostgreSQL 为普通事务。
///
/// 以 [`commit`](Self::commit) 或 [`rollback`](Self::rollback) 结束，二者在事务结束后
/// 恢复连接只读并释放租约。未结束即析构（错误或取消）时排入回滚，后台清理确认回滚
/// 完成后才放行下一个写者。事务内不能做文件或网络 I/O，也不能再开启同池写事务。
#[derive(Debug)]
pub struct WriteTransaction<DB: Database> {
    connection: WriteConnection<DB>,
}

impl<DB: Database> WriteTransaction<DB> {
    pub(super) async fn begin(mut connection: WriteConnection<DB>) -> Result<Self, Error> {
        let mut phase = transaction_begin_phase::<DB>(connection.pool_name());
        // 先标记：BEGIN 失败或被取消时析构照常排入回滚，驱动在无事务时忽略回滚。
        connection.transaction_open = true;
        // 立即取得数据库写锁：延迟事务升级时遇到外部写者会绕过 busy handler 直接失败。
        let statement = (DB::NAME == <Sqlite as Database>::NAME)
            .then(|| SqlSafeStr::into_sql_str("BEGIN IMMEDIATE"));
        let result = DB::TransactionManager::begin(connection.native(), statement).await;
        phase.finish(result.as_ref().err());
        match result {
            Ok(()) => Ok(Self { connection }),
            Err(error) => {
                connection.record_failure(&error);
                Err(error)
            }
        }
    }

    pub async fn commit(mut self) -> Result<(), Error> {
        let mut phase = transaction_commit_phase::<DB>(self.connection.pool_name());
        let result = DB::TransactionManager::commit(self.connection.native()).await;
        phase.finish(result.as_ref().err());
        self.end(result).await
    }

    pub async fn rollback(mut self) -> Result<(), Error> {
        let mut phase = transaction_rollback_phase::<DB>(self.connection.pool_name());
        let result = DB::TransactionManager::rollback(self.connection.native()).await;
        phase.finish(result.as_ref().err());
        self.end(result).await
    }

    async fn end(mut self, result: Result<(), Error>) -> Result<(), Error> {
        if let Err(error) = result {
            // 事务仍活跃：析构排入回滚，并由后台清理持有协调权直至回滚完成。
            self.connection.record_failure(&error);
            return Err(error);
        }
        self.connection.transaction_open = false;
        self.connection.finish().await
    }
}

impl<DB: Database> Deref for WriteTransaction<DB> {
    type Target = ConnectionHandle<'static, DB>;

    fn deref(&self) -> &Self::Target {
        &self.connection
    }
}

impl<DB: Database> DerefMut for WriteTransaction<DB> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.connection
    }
}

impl<DB: Database> Drop for WriteConnection<DB> {
    fn drop(&mut self) {
        let Some(mut connection) = self.connection.take() else {
            return;
        };
        if self.transaction_open {
            // 与 SQLx 事务析构相同：只向驱动排入回滚，后续屏障或下次使用等候其完成。
            DB::TransactionManager::start_rollback(connection.handle.raw());
        }
        let Some(mut lease) = self.lease.take() else {
            return;
        };
        let runtime = self.runtime.take().expect("SQLite writer runtime");
        let discard = self.discard_on_drop;
        let write_mode = self.write_mode;
        lease.hold.cancel_if_unfinished();
        // 从进入析构路径开始计时，包含分离任务首次被 poll 前的等待。
        // 不进入此 span，避免 SQLite worker 克隆它并延长到清理任务结束之后。
        let cleanup_span = tracing::info_span!(
            target: "stravia::perf",
            parent: lease.hold.span(),
            "sqlite.write_gate.cleanup",
            backend = DB::NAME,
            pool = lease.pool,
            status = tracing::field::Empty,
            error_kind = tracing::field::Empty,
        );
        let mut phase = Phase::new(cleanup_span);
        // 只在取消或错误退出时分离清理任务；正常 finish 不分配额外任务。
        // 事务的 Drop 已先向同一 worker 排入回滚，屏障完成前不能放行后续写入。
        let _cleanup = runtime.spawn(async move {
            let mut failed = false;
            if !discard && let Err(error) = settle(&mut connection, write_mode).await {
                phase.fail(&error);
                lease.hold.fail(&error);
                tracing::warn!(
                    error_kind = diagnostics::error_kind(&error),
                    "SQLite writer cleanup failed"
                );
                failed = true;
            }
            if discard || failed {
                // SQLx 不向已丢弃的事务报告后台 ROLLBACK 错误，必须确认已离开事务。
                // 无法确认时关闭连接，并在 worker 确认关闭后才释放协调权。
                if let Err(error) = connection.handle.close().await {
                    phase.fail(&error);
                    lease.hold.fail(&error);
                    tracing::warn!(
                        error_kind = diagnostics::error_kind(&error),
                        "SQLite writer connection close failed"
                    );
                    failed = true;
                }
            } else {
                drop(connection);
            }
            if !failed {
                phase.complete();
            }
            drop(phase);
            // 先释放协调权，再关闭 hold span。
            drop(lease);
        });
    }
}

async fn settle<DB: Database>(
    connection: &mut PoolConnection<DB>,
    write_mode: Option<WriteMode<DB>>,
) -> Result<(), Error> {
    match write_mode {
        // 恢复只读与 ping 同样排在该 worker 队列末尾，兼作完成屏障。
        Some(write_mode) => write_mode(connection.handle.raw(), false).await?,
        None => connection.ping().await?,
    }
    if connection.handle.raw().is_in_transaction() {
        return Err(Error::Protocol(
            "SQLite write lease ended with an active transaction".into(),
        ));
    }
    Ok(())
}
