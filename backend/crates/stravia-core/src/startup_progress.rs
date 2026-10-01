//! 与传输无关的启动及迁移进度；计数只表示当前阶段，不表示整体就绪。

use std::future::Future;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct StartupProgress {
    pub phase: &'static str,
    pub label: &'static str,
    pub completed: u64,
    pub total: Option<u64>,
}

tokio::task_local! {
    static OBSERVER: Box<dyn Fn(StartupProgress) + Send + Sync>;
}

/// 报告已提交或已落盘的当前阶段工作；未知总量使用 `None`，不得模拟百分比。
/// phase 是稳定机器代码，label 是不含路径、SQL 或凭据的静态英文回退文案。
pub fn report(phase: &'static str, label: &'static str, completed: u64, total: Option<u64>) {
    let progress = StartupProgress {
        phase,
        label,
        completed,
        total,
    };
    tracing::info!(
        target: "stravia::startup",
        phase,
        label,
        completed,
        total,
        "Startup progress"
    );
    // 未安装观察者是正常的日志-only 路径，不需要创建全局观察者。
    OBSERVER
        .try_with(|observer| observer(progress))
        .unwrap_or(());
}

/// 在当前异步任务内观察进度，互不共享全局 sink，嵌套 scope 结束后恢复外层观察者。
///
/// Tokio task-local 不传播到 `spawn` / `spawn_blocking` 子任务。批次工作必须
/// 在调用任务 await/join 成功并提交事务或完成文件落盘后调用 [`report`]。
/// 观察者应快速同步返回；整体 ready/failed 由外壳生命周期决定。
pub async fn observe_startup<F: Future>(
    observer: impl Fn(StartupProgress) + Send + Sync + 'static,
    future: F,
) -> F::Output {
    OBSERVER.scope(Box::new(observer), future).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn concurrent_scopes_isolate_progress_and_do_not_inherit_spawn() {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let mut tasks = Vec::new();
        for phase in ["first", "second"] {
            let events = Arc::new(Mutex::new(Vec::new()));
            let observed = events.clone();
            let barrier = barrier.clone();
            tasks.push(tokio::spawn(async move {
                observe_startup(move |event| observed.lock().unwrap().push(event), async {
                    report(phase, "Working", 0, Some(1));
                    barrier.wait().await;
                    tokio::spawn(async { report("child", "Working", 0, None) })
                        .await
                        .unwrap();
                    tokio::task::spawn_blocking(|| report("blocking_child", "Working", 0, None))
                        .await
                        .unwrap();
                    report(phase, "Working", 1, Some(1));
                })
                .await;
                let events = events.lock().unwrap();
                assert_eq!(
                    events
                        .iter()
                        .map(|event| (event.phase, event.completed))
                        .collect::<Vec<_>>(),
                    vec![(phase, 0), (phase, 1)]
                );
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
    }
}
