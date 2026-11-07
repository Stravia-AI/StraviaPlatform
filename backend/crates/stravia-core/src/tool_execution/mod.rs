use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};

use futures::FutureExt;
use parking_lot::Mutex;
use serde_json::{Value, json};
use stravia_runtime_contract::{
    CancellationToken, Principal,
    hook::{PlatformTool, PlatformToolError, PlatformToolOutput, ToolExecutionContext},
    protocol::ir::{ContentBlock, ToolCall},
};
use tokio::sync::{Notify, oneshot};

mod marker;
pub(crate) use marker::delivery_processing_error;
pub(crate) use marker::{
    DetachedPlatformExecution, PreparedMarkerExecution, StartedMarkerDelivery,
};

use crate::{
    Gateway,
    history_marker::{
        ClaimOutcome, HiddenHistorySegment, HistoryMarkerError, PlatformExecutionState,
        PlatformMarkerInput, PublicExecution, PublicExecutionInput,
    },
};

const DEFAULT_WAIT: Duration = Duration::from_secs(60);

#[cfg(feature = "test-support")]
async fn enter_handoff_test_barrier(
    address: std::net::SocketAddr,
    id: &str,
) -> Result<tokio::net::TcpStream, PlatformToolError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    if !address.ip().is_loopback() {
        return Err(failure("test handoff barrier must use a loopback address"));
    }
    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .map_err(|error| failure(error.to_string()))?;
    stream
        .write_all(id.as_bytes())
        .await
        .map_err(|error| failure(error.to_string()))?;
    stream
        .write_all(b"\n")
        .await
        .map_err(|error| failure(error.to_string()))?;
    let release = stream
        .read_u8()
        .await
        .map_err(|error| failure(error.to_string()))?;
    if release != 1 {
        return Err(failure("invalid test handoff barrier release"));
    }
    Ok(stream)
}

tokio::task_local! {
    static EXECUTION_DEADLINE: Instant;
    static ACCEPTED_AT: tokio::time::Instant;
}

#[cfg(test)]
tokio::task_local! {
    pub(crate) static QUERY_WAITING: Arc<Notify>;
}

pub(crate) fn current_deadline() -> Option<Instant> {
    EXECUTION_DEADLINE.try_with(|deadline| *deadline).ok()
}

pub(crate) async fn scope_execution<F: Future>(
    accepted: tokio::time::Instant,
    deadline: Instant,
    work: F,
) -> F::Output {
    ACCEPTED_AT
        .scope(accepted, EXECUTION_DEADLINE.scope(deadline, work))
        .await
}

pub(crate) async fn scope_request<F: Future>(deadline: Instant, work: F) -> F::Output {
    let accepted = ACCEPTED_AT
        .try_with(|accepted| *accepted)
        .unwrap_or_else(|_| tokio::time::Instant::now());
    let deadline = current_deadline()
        .map(|current| current.min(deadline))
        .unwrap_or(deadline);
    scope_execution(accepted, deadline, work).await
}

pub(crate) enum ExecutionSource {
    Mcp,
    InferenceRun {
        run_id: String,
        published_cancellation: CancellationToken,
    },
}

pub(crate) struct ToolExecutionOutput {
    pub(crate) output: PlatformToolOutput,
    pub(crate) system_cancelled: bool,
}

impl ToolExecutionOutput {
    fn ordinary(output: PlatformToolOutput) -> Self {
        Self {
            output,
            system_cancelled: false,
        }
    }
    fn cancelled() -> Self {
        Self {
            output: cancelled_output(),
            system_cancelled: true,
        }
    }
}

pub(crate) struct ToolDelivery {
    pub(crate) result: stravia_runtime_contract::hook::PlatformToolResult,
    pub(crate) system_cancelled: bool,
}

struct LiveExecution {
    cancellation: CancellationToken,
    changed: Notify,
    ownership: Mutex<Ownership>,
    persistence_error: Mutex<Option<String>>,
}

enum Ownership {
    Initial,
    Cancelled,
    Committing,
    Background,
}

impl LiveExecution {
    fn cancel_initial(&self) -> bool {
        let mut ownership = self.ownership.lock();
        if matches!(*ownership, Ownership::Initial) {
            *ownership = Ownership::Cancelled;
            self.cancellation.cancel();
        }
        matches!(*ownership, Ownership::Cancelled)
    }

    fn begin_handoff_commit(&self, caller: &CancellationToken) -> bool {
        let mut ownership = self.ownership.lock();
        if !matches!(*ownership, Ownership::Initial) {
            return false;
        }
        if caller.is_cancelled() || self.cancellation.is_cancelled() {
            *ownership = Ownership::Cancelled;
            self.cancellation.cancel();
            return false;
        }
        *ownership = Ownership::Committing;
        true
    }

    fn resolve_handoff(&self, committed: bool) {
        let mut ownership = self.ownership.lock();
        if committed {
            *ownership = Ownership::Background;
        } else if matches!(*ownership, Ownership::Initial | Ownership::Cancelled) {
            *ownership = Ownership::Cancelled;
            self.cancellation.cancel();
        }
        // Once COMMIT began, a storage error is not proof of rollback. Leave
        // ownership unresolved and report the error rather than inventing a
        // cancellation fact; the original deadline still bounds local work.
    }
}

// 此表只负责本进程的取消与唤醒；身份、权限和结果始终由持久 Store 决定。
static LIVE: LazyLock<Mutex<HashMap<String, Arc<LiveExecution>>>> = LazyLock::new(Default::default);

struct InitialWait<'a> {
    execution: Arc<LiveExecution>,
    id: &'a str,
    task_started: bool,
    armed: bool,
}

impl Drop for InitialWait<'_> {
    fn drop(&mut self) {
        if !self.task_started {
            LIVE.lock().remove(self.id);
        }
        if self.armed {
            // 等写锁时仍可取消；不可逆 COMMIT 已开始时由跟踪任务完成持久决议。
            self.execution.cancel_initial();
        }
    }
}

fn failure(message: impl Into<String>) -> PlatformToolError {
    PlatformToolError::new(message)
}

fn output(value: Value) -> PlatformToolOutput {
    PlatformToolOutput {
        content: vec![ContentBlock::Unknown { raw: value }],
        is_error: false,
        metadata: Default::default(),
    }
}

fn cancelled_output() -> PlatformToolOutput {
    PlatformToolOutput {
        content: vec![ContentBlock::Unknown {
            raw: json!({"error":{"code":"cancelled","message":"Local tool execution cancelled; this does not guarantee upstream work or billing has stopped."}}),
        }],
        is_error: true,
        metadata: Default::default(),
    }
}

fn snapshot(execution: PublicExecution) -> Result<Value, PlatformToolError> {
    let path = format!("stravia://tool-executions/{}", execution.id);
    let status = match execution.state {
        PlatformExecutionState::Pending => "pending",
        PlatformExecutionState::Running => "running",
        PlatformExecutionState::Completed => "completed",
        PlatformExecutionState::Failed => "failed",
        PlatformExecutionState::Interrupted => "interrupted",
    };
    let mut value = json!({"path":path,"status":status});
    if matches!(
        execution.state,
        PlatformExecutionState::Completed
            | PlatformExecutionState::Failed
            | PlatformExecutionState::Interrupted
    ) {
        let Some(HiddenHistorySegment::Platform {
            result: ContentBlock::ToolResult { content, .. },
            ..
        }) = execution.segment
        else {
            return Err(failure("Tool execution is missing its terminal result"));
        };
        if execution.state == PlatformExecutionState::Completed {
            value["result"] = content;
        } else {
            value["error"] = content.get("error").cloned().unwrap_or(content);
        }
    }
    Ok(value)
}

impl Gateway {
    pub(crate) async fn dispatch_platform_tool(
        &self,
        tool: Arc<dyn PlatformTool>,
        arguments: Value,
        mut context: ToolExecutionContext,
        source: ExecutionSource,
    ) -> Result<ToolExecutionOutput, PlatformToolError> {
        use stravia_runtime_contract::hook::PlatformExecutionPolicy;
        match tool.execution_policy(&arguments) {
            PlatformExecutionPolicy::Query => tool
                .execute_result(arguments, context)
                .await
                .map(ToolExecutionOutput::ordinary),
            PlatformExecutionPolicy::Synchronous => {
                if let ExecutionSource::InferenceRun {
                    published_cancellation,
                    ..
                } = source
                {
                    // 未纳入有界执行的工具仍遵循 Marker 发布后的独立执行契约。
                    context.cancellation = published_cancellation;
                }
                if tool.parallel_safe() {
                    let _permit = self.history_marker_execution_gate.read().await;
                    tool.execute_result(arguments, context)
                        .await
                        .map(ToolExecutionOutput::ordinary)
                } else {
                    let _permit = self.history_marker_execution_gate.write().await;
                    tool.execute_result(arguments, context)
                        .await
                        .map(ToolExecutionOutput::ordinary)
                }
            }
            PlatformExecutionPolicy::Bounded => {
                self.execute_bounded_tool(tool, arguments, context, source)
                    .await
            }
        }
    }

    async fn execute_bounded_tool(
        &self,
        tool: Arc<dyn PlatformTool>,
        arguments: Value,
        mut context: ToolExecutionContext,
        source: ExecutionSource,
    ) -> Result<ToolExecutionOutput, PlatformToolError> {
        let accepted = ACCEPTED_AT
            .try_with(|instant| *instant)
            .unwrap_or_else(|_| tokio::time::Instant::now());
        let deadline = current_deadline().unwrap_or_else(|| {
            Instant::now() + tool.execution_limit().unwrap_or(Duration::from_secs(900))
        });
        let remaining = deadline.saturating_duration_since(Instant::now());
        let deadline_unix_ms = chrono::Utc::now()
            .timestamp_millis()
            .saturating_add(remaining.as_millis().min(i64::MAX as u128) as i64);
        let observer = crate::interaction_observation::scope::current();
        let wait_until = accepted + tool.synchronous_wait().unwrap_or(DEFAULT_WAIT);
        let principal = context.principal.clone();
        let caller_cancellation = context.cancellation.clone();
        let cancellation = CancellationToken::new();
        let domain_cancellation = CancellationToken::new();
        context.cancellation = domain_cancellation.clone();
        let retention = self
            .storage
            .settings()
            .get("log_retention_days")
            .await
            .map_err(|error| failure(error.to_string()))?
            .map(|days| days.parse::<u32>())
            .transpose()
            .map_err(|_| failure("Invalid history retention setting"))?
            .unwrap_or(7);
        let retention = Duration::from_secs(u64::from(retention) * 86400);
        let call = ToolCall {
            id: stravia_runtime_contract::identifier::new_id().into(),
            name: tool.external_name().to_owned(),
            arguments: serde_json::to_string(&arguments)
                .map_err(|error| failure(error.to_string()))?,
        };
        let marker = self
            .history_markers
            .create_platform(
                &principal,
                PlatformMarkerInput {
                    tool_id: tool.id().to_string(),
                    call: call.clone(),
                    activity: tool.activity_label().to_owned(),
                    execution_limit: remaining,
                },
            )
            .await
            .map_err(|error| failure(error.to_string()))?;
        let id = self
            .history_markers
            .create_public_execution(
                &principal,
                &marker.reference,
                PublicExecutionInput {
                    source_run_id: match source {
                        ExecutionSource::Mcp => None,
                        // Hook session 的 run_id 不是诊断 Run 身份；缺少诊断时才使用会话 fallback。
                        ExecutionSource::InferenceRun { run_id, .. } => Some(
                            observer
                                .as_ref()
                                .map(|observer| observer.run_id().to_owned())
                                .unwrap_or(run_id),
                        ),
                    },
                    retention,
                    deadline_unix_ms,
                },
            )
            .await
            .map_err(|error| failure(error.to_string()))?;
        let owner = stravia_runtime_contract::identifier::new_id();
        let live = Arc::new(LiveExecution {
            cancellation,
            changed: Notify::new(),
            ownership: Mutex::new(Ownership::Initial),
            persistence_error: Mutex::new(None),
        });
        LIVE.lock().insert(id.clone(), live.clone());
        let mut initial = InitialWait {
            execution: live.clone(),
            id: &id,
            task_started: false,
            armed: true,
        };
        if caller_cancellation.is_cancelled() {
            live.cancel_initial();
        }

        let (tx, mut rx) = oneshot::channel();
        let gateway = self.background_clone();
        let task_live = live.clone();
        let task_id = id.clone();
        let task_principal = principal.clone();
        let parallel_safe = tool.parallel_safe();
        let observer = observer.map(|observer| observer.for_public_execution());
        let root_request = crate::rpm::current_root_request();
        self.lifecycle.spawn(crate::rpm::scope_root_request(root_request, async move {
            let mut claimed = false;
            let operation = async {
                if task_live.cancellation.is_cancelled() {
                    return Ok(ToolExecutionOutput::cancelled());
                }
                let execute = async {
                    if gateway.history_markers.claim_execution(&task_principal, &marker.reference, &owner, remaining).await.map_err(|error| failure(error.to_string()))? != ClaimOutcome::Claimed {
                        return Err(failure("Tool execution could not be claimed"));
                    }
                    claimed = true;
                    tool.execute_result(arguments, context).await.map(ToolExecutionOutput::ordinary)
                };
                if parallel_safe {
                    let _permit = gateway.history_marker_execution_gate.read().await;
                    execute.await
                } else {
                    let _permit = gateway.history_marker_execution_gate.write().await;
                    execute.await
                }
            };
            let result = tokio::select! {
                biased;
                result = std::panic::AssertUnwindSafe(EXECUTION_DEADLINE.scope(deadline, crate::interaction_observation::scope::scope(observer, operation))).catch_unwind() => result.unwrap_or_else(|_| Err(failure("Platform tool panicked"))),
                _ = task_live.cancellation.cancelled() => {
                    domain_cancellation.cancel();
                    Ok(ToolExecutionOutput::cancelled())
                },
                _ = tokio::time::sleep_until(deadline.into()) => {
                    domain_cancellation.cancel();
                    Err(failure("Platform tool execution reached its registered deadline"))
                },
                _ = gateway.lifecycle.cancellation.cancelled() => {
                    domain_cancellation.cancel();
                    Err(failure("Platform tool execution interrupted by shutdown"))
                },
            };
            let interrupted = gateway.lifecycle.cancellation.is_cancelled();
            let result = if interrupted {
                Err(failure("Platform tool execution interrupted by shutdown"))
            } else {
                result
            };
            if !claimed {
                // 排队期间取消也要留下终态，但绝不启动领域工作。
                if let Err(error) = gateway.history_markers.claim_execution(&task_principal, &marker.reference, &owner, remaining).await {
                    *task_live.persistence_error.lock() = Some(error.to_string());
                }
            }
            let encoded = match &result {
                Ok(delivery) => crate::hook::tool::blocks_to_value(delivery.output.content.clone()).map(|(content, kind)| (content, kind, delivery.output.is_error)),
                Err(error) => Ok((Value::String(error.to_string()), stravia_runtime_contract::protocol::ir::ToolResultContentKind::Json, true)),
            };
            let mut authoritative_terminal = false;
            let persisted = match encoded {
                Ok((content, kind, is_error)) => {
                    let persisted = gateway.history_markers.finish_execution(&task_principal, &marker.reference, &owner,
                    if interrupted { PlatformExecutionState::Interrupted } else if is_error { PlatformExecutionState::Failed } else { PlatformExecutionState::Completed },
                    HiddenHistorySegment::Platform { result: ContentBlock::ToolResult { tool_use_id: call.id.clone(), content, content_kind: Some(kind), is_error: Some(is_error), cache_control: None }, call },
                    ).await;
                    if matches!(persisted, Err(HistoryMarkerError::TerminalConflict)) {
                        // Store 可能已在同一事务中收敛到 deadline/lease 终态。
                        // 这是发布竞争，不是无法读取结果的持久故障。
                        match gateway.history_markers.resolve_public_execution(&task_principal, &task_id).await {
                            Ok(Some(execution)) => authoritative_terminal = !matches!(execution.state, PlatformExecutionState::Pending | PlatformExecutionState::Running),
                            Ok(None) => authoritative_terminal = true,
                            Err(error) => {
                                tracing::warn!(execution_id = %task_id, error = ?error, "failed to resolve tool execution after terminal conflict");
                            }
                        }
                    }
                    persisted.map_err(|error| failure(error.to_string()))
                },
                Err(error) => Err(error),
            };
            let result = match persisted {
                Ok(()) => result,
                Err(error) => {
                    if !authoritative_terminal {
                        tracing::warn!(execution_id = %task_id, error = %error, "failed to persist tool execution result");
                        *task_live.persistence_error.lock() = Some(error.to_string());
                    }
                    Err(error)
                }
            };
            task_live.changed.notify_waiters();
            if task_live.persistence_error.lock().is_none() {
                LIVE.lock().remove(&task_id);
            }
            // 受理调用可能已经交接或断开；任务结果以持久记录为准。
            let _ = tx.send(result);
        }));
        initial.task_started = true;
        tokio::select! {
            biased;
            result = &mut rx => {
                initial.armed = false;
                result.map_err(|_| failure("Tool execution interrupted before result delivery"))?
            }
            _ = caller_cancellation.cancelled() => {
                live.cancel_initial();
                // 保留各工具既有取消结果，并等待本地执行退出。
                initial.armed = false;
                rx.await.map_err(|_| failure("Tool execution cancelled"))?
            }
            _ = tokio::time::sleep_until(wait_until) => {
                let store = self.history_markers.clone();
                let task_live = live.clone();
                let task_id = id.clone();
                let task_principal = principal.clone();
                let handoff_cancellation = caller_cancellation.clone();
                let (handoff_tx, mut handoff_rx) = oneshot::channel();
                #[cfg(feature = "test-support")]
                let test_barrier = self.config.test_handoff_barrier;
                self.lifecycle.spawn(async move {
                    #[cfg(feature = "test-support")]
                    let mut probe = if let Some(address) = test_barrier {
                        match enter_handoff_test_barrier(address, &task_id).await {
                            Ok(stream) => Some(stream),
                            Err(error) => {
                                task_live.resolve_handoff(false);
                                let _ = handoff_tx.send(Err(error));
                                return;
                            }
                        }
                    } else {
                        None
                    };
                    let result = if task_live.cancellation.is_cancelled() {
                        Err(failure("Tool execution cancelled before background handoff"))
                    } else {
                        let before_commit = || task_live.begin_handoff_commit(&handoff_cancellation);
                        store.handoff_public_execution(&task_principal, &task_id, &before_commit).await.map_err(|error| {
                            if matches!(error, HistoryMarkerError::TerminalConflict)
                                && (handoff_cancellation.is_cancelled() || task_live.cancellation.is_cancelled())
                            {
                                tracing::debug!(execution_id = %task_id, "tool execution handoff cancelled before commit");
                            } else {
                                tracing::warn!(execution_id = %task_id, error = ?error, "failed to persist tool execution handoff");
                            }
                            failure(error.to_string())
                        })
                    };
                    task_live.resolve_handoff(result.is_ok());
                    #[cfg(feature = "test-support")]
                    if let Some(stream) = probe.as_mut() {
                        use tokio::io::AsyncWriteExt;
                        if let Err(error) = stream.write_all(&[u8::from(result.is_ok())]).await {
                            tracing::warn!(%error, "failed to report test handoff barrier outcome");
                        }
                    }
                    let _ = handoff_tx.send(result);
                });
                tokio::select! {
                    result = &mut handoff_rx => result.map_err(|_| failure("Tool execution handoff interrupted"))??,
                    _ = caller_cancellation.cancelled() => {
                        if live.cancel_initial() {
                            initial.armed = false;
                            return rx.await.map_err(|_| failure("Tool execution cancelled"))?;
                        }
                        handoff_rx.await.map_err(|_| failure("Tool execution handoff interrupted"))??;
                    }
                }
                initial.armed = false;
                let execution = self.history_markers.resolve_public_execution(&principal, &id).await.map_err(|error| failure(error.to_string()))?.ok_or_else(|| failure("Tool execution is unavailable"))?;
                // 到期与完成竞争时仍交付原快速结果，不丢弃已完成的领域输出。
                if let Ok(result) = rx.try_recv() { return result; }
                Ok(ToolExecutionOutput::ordinary(output(snapshot(execution)?)))
            }
        }
    }

    pub(crate) async fn query_tool_execution(
        &self,
        principal: &Principal,
        id: &str,
        wait: Duration,
        cancellation: &CancellationToken,
    ) -> Result<Value, PlatformToolError> {
        let until = tokio::time::Instant::now()
            .checked_add(wait)
            .ok_or_else(|| failure("Tool execution wait is out of range"))?;
        let until = current_deadline()
            .map(|deadline| until.min(deadline.into()))
            .unwrap_or(until);
        loop {
            let live = LIVE.lock().get(id).cloned();
            let notified = live.as_ref().map(|live| live.changed.notified());
            tokio::pin!(notified);
            if let Some(notified) = notified.as_mut().as_pin_mut() {
                notified.enable();
            }
            let execution = self
                .history_markers
                .resolve_public_execution(principal, id)
                .await
                .map_err(|error| failure(error.to_string()))?
                .ok_or_else(|| failure("Tool execution is unavailable"))?;
            let terminal = !matches!(
                execution.state,
                PlatformExecutionState::Pending | PlatformExecutionState::Running
            );
            if terminal {
                return snapshot(execution);
            }
            if let Some(error) = live
                .as_ref()
                .and_then(|live| live.persistence_error.lock().clone())
            {
                return Err(failure(error));
            }
            let original_until = tokio::time::Instant::now()
                + Duration::from_millis(
                    execution
                        .deadline_unix_ms
                        .saturating_sub(chrono::Utc::now().timestamp_millis())
                        .max(0) as u64,
                );
            let until = until.min(original_until);
            if wait.is_zero() || tokio::time::Instant::now() >= until {
                return snapshot(execution);
            }
            // 测试等 SQL 读取完成后再推进虚拟时钟；普通调用没有安装观察者。
            #[cfg(test)]
            QUERY_WAITING
                .try_with(|registered| registered.notify_one())
                .unwrap_or_default();
            tokio::select! {
                _ = cancellation.cancelled() => return Err(failure("Tool execution query cancelled")),
                _ = tokio::time::sleep_until(until) => {},
                _ = async { match notified.as_mut().as_pin_mut() { Some(notified) => notified.await, None => futures::future::pending().await } } => {},
            }
        }
    }
}
