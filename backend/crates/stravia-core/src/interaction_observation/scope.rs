use super::RunObserver;

tokio::task_local! {
    static OBSERVER: Option<RunObserver>;
}

/// 仅传播诊断归属；离开平台工具执行作用域后不保留环境状态。
pub(crate) async fn scope<F: Future>(observer: Option<RunObserver>, future: F) -> F::Output {
    OBSERVER.scope(observer, future).await
}

pub(crate) fn current() -> Option<RunObserver> {
    OBSERVER.try_with(Clone::clone).ok().flatten()
}
