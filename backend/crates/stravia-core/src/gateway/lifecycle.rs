use super::*;

pub(crate) struct GatewayLifecycle {
    pub(super) cancellation: stravia_runtime_contract::CancellationToken,
    owners: AtomicUsize,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl GatewayLifecycle {
    pub(super) fn new() -> Self {
        Self {
            cancellation: stravia_runtime_contract::CancellationToken::new(),
            owners: AtomicUsize::new(1),
            tasks: Mutex::new(Vec::new()),
        }
    }

    pub(super) fn add_owner(&self) {
        self.owners.fetch_add(1, Ordering::AcqRel);
    }

    pub(super) fn release_owner(&self) -> bool {
        self.owners.fetch_sub(1, Ordering::AcqRel) == 1
    }

    pub(crate) fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        let handle = tokio::spawn(task);
        let mut tasks = self.tasks.lock();
        tasks.push(handle);
    }

    pub(super) fn abort_tasks(&self) {
        self.cancellation.cancel();
        let tasks = self.tasks.lock();
        for task in tasks.iter() {
            task.abort();
        }
    }

    pub(super) async fn shutdown(&self) {
        self.cancellation.cancel();
        let tasks = {
            let mut tasks = self.tasks.lock();
            std::mem::take(&mut *tasks)
        };
        for task in tasks {
            if let Err(error) = task.await {
                tracing::debug!(%error, "gateway task failed during shutdown");
            }
        }
    }
}

impl Drop for GatewayLifecycle {
    fn drop(&mut self) {
        self.cancellation.cancel();
        let tasks = self.tasks.get_mut();
        for task in tasks.drain(..) {
            task.abort();
        }
    }
}
