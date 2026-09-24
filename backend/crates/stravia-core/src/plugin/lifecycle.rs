use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use stravia_runtime_contract::{CancellationToken, Deadline};
use stravia_vendor_runtime::RuntimeError;
use tokio::sync::{Notify, OwnedRwLockReadGuard, RwLock};

/// 所有供应商操作共用准入与写回屏障，而非仅协调模型推理。
#[derive(Default)]
pub(crate) struct VendorOperationTracker {
    vendors: Mutex<HashMap<String, Arc<VendorActivity>>>,
}

#[derive(Default)]
struct VendorActivity {
    state: Mutex<ActivityState>,
    writes: Arc<RwLock<()>>,
    configuration: Arc<tokio::sync::Mutex<()>>,
    drained: Notify,
}

struct ActivityState {
    accepting: bool,
    epoch: u64,
    next_id: u64,
    operations: HashMap<u64, CancellationToken>,
    publications: CancellationToken,
}

impl Default for ActivityState {
    fn default() -> Self {
        Self {
            accepting: true,
            epoch: 0,
            next_id: 0,
            operations: HashMap::new(),
            publications: CancellationToken::new(),
        }
    }
}

pub(crate) struct VendorOperation {
    activity: Arc<VendorActivity>,
    id: u64,
    epoch: u64,
    cancellation: CancellationToken,
    publications: CancellationToken,
}

/// 已完成操作的结果仍受代际约束，但不再占用等待取消的活跃任务。
#[derive(Clone)]
pub(crate) struct VendorPublicationFence {
    activity: Arc<VendorActivity>,
    epoch: u64,
    publications: CancellationToken,
    caller: CancellationToken,
    deadline: Deadline,
}

/// 只有旧操作与其受控资源全部退出后，才能取得此凭证并重置数据。
pub(crate) struct QuiescentVendor {
    admission: UpdateAdmission,
}

struct UpdateAdmission {
    activity: Arc<VendorActivity>,
}

impl VendorOperationTracker {
    /// 只串行化连接配置与包切换，不等待兼容版本的推理或流式背压。
    pub(in crate::plugin) async fn configuration_guard(
        &self,
        vendor_id: &str,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        let configuration = self
            .vendors
            .lock()
            .entry(vendor_id.to_owned())
            .or_default()
            .configuration
            .clone();
        configuration.lock_owned().await
    }

    pub(in crate::plugin) fn active_count(&self, vendor_id: &str) -> usize {
        self.vendors
            .lock()
            .get(vendor_id)
            .map_or(0, |activity| activity.state.lock().operations.len())
    }

    /// 准入本身不构成写回能力；写协议由 write_fence 的可见性与私有 operations 守住。
    pub(crate) fn begin(&self, vendor_id: &str) -> anyhow::Result<Arc<VendorOperation>> {
        let activity = self
            .vendors
            .lock()
            .entry(vendor_id.to_owned())
            .or_default()
            .clone();
        let mut state = activity.state.lock();
        anyhow::ensure!(state.accepting, "vendor update is in progress");
        let id = state.next_id;
        state.next_id = id
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("vendor operation identifiers exhausted"))?;
        let cancellation = CancellationToken::new();
        state.operations.insert(id, cancellation.clone());
        let epoch = state.epoch;
        let publications = state.publications.clone();
        drop(state);
        Ok(Arc::new(VendorOperation {
            activity,
            id,
            epoch,
            cancellation,
            publications,
        }))
    }

    /// 纯写回入口：一次完成准入与围栏，返回的许可须持有到写回提交。
    pub(crate) async fn write_permit(&self, vendor_id: &str) -> anyhow::Result<WritePermit> {
        self.begin(vendor_id)?.write_permit().await
    }

    /// 调用方必须在持久化重置和运行版本切换完成前持有返回值。
    pub(in crate::plugin) async fn cancel_and_drain(
        &self,
        vendor_id: &str,
    ) -> anyhow::Result<QuiescentVendor> {
        let activity = self
            .vendors
            .lock()
            .entry(vendor_id.to_owned())
            .or_default()
            .clone();
        {
            let mut state = activity.state.lock();
            anyhow::ensure!(state.accepting, "vendor update is already in progress");
            let epoch = state
                .epoch
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("vendor operation epochs exhausted"))?;
            state.accepting = false;
            state.epoch = epoch;
            state.publications.cancel();
            state.publications = CancellationToken::new();
            for cancellation in state.operations.values() {
                cancellation.cancel();
            }
        }
        let admission = UpdateAdmission { activity };
        loop {
            let drained = admission.activity.drained.notified();
            if admission.activity.state.lock().operations.is_empty() {
                break;
            }
            drained.await;
        }
        // 写入许可可以被异步存储调用持有；即使最后一个操作句柄已释放，
        // 仍必须等这些许可结束，才允许管理员确认的数据重置。
        let writes = admission.activity.writes.write().await;
        drop(writes);
        Ok(QuiescentVendor { admission })
    }
}

impl VendorOperation {
    pub(crate) fn publication_fence(
        &self,
        caller: CancellationToken,
        deadline: Deadline,
    ) -> VendorPublicationFence {
        VendorPublicationFence {
            activity: Arc::clone(&self.activity),
            epoch: self.epoch,
            publications: self.publications.clone(),
            caller,
            deadline,
        }
    }

    pub(crate) fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    pub(crate) fn ensure_current(&self) -> anyhow::Result<()> {
        if self.cancellation.is_cancelled() || self.activity.state.lock().epoch != self.epoch {
            return Err(RuntimeError::Cancelled.into());
        }
        Ok(())
    }

    /// 所有状态、凭据、发现结果和能力结果的最终写回均持有此许可。
    pub(in crate::plugin) async fn write_fence(&self) -> anyhow::Result<OwnedRwLockReadGuard<()>> {
        let guard = tokio::select! {
            biased;
            () = self.cancellation.cancelled() => {
                return Err(RuntimeError::Cancelled.into());
            }
            guard = self.activity.writes.clone().read_owned() => guard,
        };
        self.ensure_current()?;
        Ok(guard)
    }

    /// 写回许可：准入与围栏一次取得，调用方不再组合 begin/write_fence/drop。
    pub(crate) async fn write_permit(self: &Arc<Self>) -> anyhow::Result<WritePermit> {
        Ok(WritePermit {
            _guard: self.write_fence().await?,
            operation: Arc::clone(self),
        })
    }
}

/// Vendor 配置与状态写回持有的复合许可：operation 准入与 writes 围栏一次取得。
/// 字段顺序即释放顺序：先放围栏，再放 operation。
pub(crate) struct WritePermit {
    _guard: OwnedRwLockReadGuard<()>,
    operation: Arc<VendorOperation>,
}

impl WritePermit {
    /// 围栏持有期间插件更新可能已起步（epoch 递增先于 drain 等待）；
    /// 多步写回在提交前复检。
    pub(crate) fn ensure_current(&self) -> anyhow::Result<()> {
        self.operation.ensure_current()
    }
}

impl VendorPublicationFence {
    pub(crate) fn same_activity(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.activity, &other.activity)
    }

    pub(crate) fn activity_order_key(&self) -> usize {
        Arc::as_ptr(&self.activity) as usize
    }

    pub(crate) fn ensure_current(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.publications.is_cancelled()
                && !self.caller.is_cancelled()
                && !self.deadline.is_exceeded()
                && self.activity.state.lock().epoch == self.epoch,
            "vendor result can no longer be published"
        );
        Ok(())
    }

    pub(crate) async fn cancelled(&self) {
        tokio::select! {
            () = self.publications.cancelled() => {}
            () = self.caller.cancelled() => {}
            () = self.deadline.wait() => {}
        }
    }

    pub(crate) async fn write_fence(&self) -> anyhow::Result<OwnedRwLockReadGuard<()>> {
        let guard = tokio::select! {
            biased;
            () = self.cancelled() => {
                anyhow::bail!("vendor result can no longer be published");
            }
            guard = self.activity.writes.clone().read_owned() => guard,
        };
        self.ensure_current()?;
        Ok(guard)
    }

    /// 失败通知不能被导致失败的取消或 deadline 吞掉，但旧代际仍不得继续发布。
    /// 此许可只用于终态错误，不得用于模型内容或持久化写回。
    pub(crate) async fn terminal_write_fence(&self) -> anyhow::Result<OwnedRwLockReadGuard<()>> {
        let guard = tokio::select! {
            biased;
            () = self.publications.cancelled() => {
                anyhow::bail!("vendor generation can no longer publish");
            }
            guard = self.activity.writes.clone().read_owned() => guard,
        };
        anyhow::ensure!(
            !self.publications.is_cancelled() && self.activity.state.lock().epoch == self.epoch,
            "vendor generation can no longer publish"
        );
        Ok(guard)
    }
}

impl Drop for VendorOperation {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.activity.state.lock().operations.remove(&self.id);
        self.activity.drained.notify_waiters();
    }
}

impl QuiescentVendor {
    pub(crate) fn resume(self) {
        drop(self.admission);
    }
}

impl Drop for UpdateAdmission {
    fn drop(&mut self) {
        self.activity.state.lock().accepting = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::Poll;
    use std::time::{Duration, Instant};

    #[tokio::test]
    async fn revoked_admission_preserves_cancellation_for_preparation_and_writes() {
        let tracker = VendorOperationTracker::default();
        let operation = tracker.begin("vendor").unwrap();
        let mut update = std::pin::pin!(tracker.cancel_and_drain("vendor"));
        assert!(futures::poll!(&mut update).is_pending());
        assert!(matches!(
            operation
                .ensure_current()
                .unwrap_err()
                .downcast_ref::<RuntimeError>(),
            Some(RuntimeError::Cancelled)
        ));
        assert!(matches!(
            operation
                .write_fence()
                .await
                .unwrap_err()
                .downcast_ref::<RuntimeError>(),
            Some(RuntimeError::Cancelled)
        ));
        drop(operation);
        update.await.unwrap().resume();
    }

    #[tokio::test]
    async fn terminal_failures_survive_caller_deadlines_but_not_generation_replacement() {
        let tracker = VendorOperationTracker::default();
        let operation = tracker.begin("vendor").unwrap();
        let caller = CancellationToken::new();
        let publication =
            operation.publication_fence(caller.clone(), Deadline::fixed(Instant::now()));
        assert!(publication.write_fence().await.is_err());
        drop(publication.terminal_write_fence().await.unwrap());

        caller.cancel();
        drop(publication.terminal_write_fence().await.unwrap());
        drop(operation);
        tracker.cancel_and_drain("vendor").await.unwrap().resume();
        assert!(publication.terminal_write_fence().await.is_err());
    }

    #[tokio::test]
    async fn completed_results_are_revoked_without_retaining_active_operations() {
        let tracker = VendorOperationTracker::default();
        let operation = tracker.begin("first").unwrap();
        let other = tracker.begin("second").unwrap();
        let publication = operation.publication_fence(
            CancellationToken::new(),
            Deadline::from_now(Duration::from_secs(60)),
        );
        drop(operation);
        assert_eq!(tracker.active_count("first"), 0);
        let mut writes = crate::model_turn::vendor_write_fences(std::slice::from_ref(&publication))
            .await
            .unwrap();
        let write = writes.pop().expect("one Vendor activity write fence");
        let mut update = std::pin::pin!(tracker.cancel_and_drain("first"));
        assert!(futures::poll!(&mut update).is_pending());
        assert!(publication.ensure_current().is_err());
        other.ensure_current().unwrap();
        drop(write);
        let Poll::Ready(updated) = futures::poll!(&mut update) else {
            panic!("a completed result must not keep an operation alive");
        };
        updated.unwrap().resume();
        assert!(
            crate::model_turn::vendor_write_fences(std::slice::from_ref(&publication))
                .await
                .is_err(),
            "an incompatible update must revoke a queued final write"
        );
        tracker.begin("first").unwrap().ensure_current().unwrap();
    }

    #[tokio::test]
    async fn deduplicated_activity_still_validates_every_result() {
        let tracker = VendorOperationTracker::default();
        let operation = tracker.begin("shared").unwrap();
        let valid = operation.publication_fence(
            CancellationToken::new(),
            Deadline::from_now(Duration::from_secs(60)),
        );
        let cancelled_caller = CancellationToken::new();
        let stale = operation.publication_fence(
            cancelled_caller.clone(),
            Deadline::from_now(Duration::from_secs(60)),
        );
        drop(operation);
        cancelled_caller.cancel();

        assert!(valid.same_activity(&stale));
        assert!(
            crate::model_turn::vendor_write_fences(&[valid, stale])
                .await
                .is_err(),
            "lock deduplication must not skip a result's caller fence"
        );
    }

    #[tokio::test]
    async fn write_permit_blocks_drain_until_released() {
        let tracker = VendorOperationTracker::default();
        let permit = tracker.write_permit("vendor").await.unwrap();
        let mut update = std::pin::pin!(tracker.cancel_and_drain("vendor"));
        assert!(futures::poll!(&mut update).is_pending());
        // epoch 递增先于 drain 等待；陈旧写必须在提交前能被复检拦下。
        assert!(permit.ensure_current().is_err());
        drop(permit);
        update.await.unwrap().resume();
        tracker
            .write_permit("vendor")
            .await
            .unwrap()
            .ensure_current()
            .unwrap();
    }

    #[tokio::test]
    async fn write_permit_rejected_while_update_in_progress() {
        let tracker = VendorOperationTracker::default();
        let operation = tracker.begin("vendor").unwrap();
        let mut update = std::pin::pin!(tracker.cancel_and_drain("vendor"));
        assert!(futures::poll!(&mut update).is_pending());
        assert!(tracker.write_permit("vendor").await.is_err());
        drop(operation);
        update.await.unwrap().resume();
    }

    #[tokio::test]
    async fn write_permit_keeps_operation_registered() {
        let tracker = VendorOperationTracker::default();
        let permit = tracker.write_permit("vendor").await.unwrap();
        assert_eq!(tracker.active_count("vendor"), 1);
        drop(permit);
        assert_eq!(tracker.active_count("vendor"), 0);
    }
}
