use std::ops::{Deref, DerefMut};

use sqlx_native::Connection as _;
use tokio::{runtime::Handle, sync::OwnedMutexGuard};

use crate::{
    ConnectionHandle, Database, Error, PoolConnection,
    diagnostics::{self, Phase},
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
#[derive(Debug)]
pub struct WriteConnection<DB: Database> {
    connection: Option<PoolConnection<DB>>,
    lease: Option<Lease>,
    runtime: Option<Handle>,
    discard_on_drop: bool,
}

impl<DB: Database> WriteConnection<DB> {
    pub(super) fn new(connection: PoolConnection<DB>, lease: Option<Lease>) -> Self {
        Self {
            connection: Some(connection),
            runtime: lease.as_ref().map(|_| Handle::current()),
            lease,
            discard_on_drop: false,
        }
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
            settle(connection).await
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

impl<DB: Database> Drop for WriteConnection<DB> {
    fn drop(&mut self) {
        let Some(mut connection) = self.connection.take() else {
            return;
        };
        let Some(mut lease) = self.lease.take() else {
            return;
        };
        let runtime = self.runtime.take().expect("SQLite writer runtime");
        let discard = self.discard_on_drop;
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
            if !discard && let Err(error) = settle(&mut connection).await {
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

async fn settle<DB: Database>(connection: &mut PoolConnection<DB>) -> Result<(), Error> {
    connection.ping().await?;
    if connection.handle.raw().is_in_transaction() {
        return Err(Error::Protocol(
            "SQLite write lease ended with an active transaction".into(),
        ));
    }
    Ok(())
}
