//! 进程内性能遥测：只使用 Observation Debug 原子开关。
//! 指标仅使用静态操作名，不以 SQL 或请求内容构造标签。

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{
    Arc, LazyLock, OnceLock, Weak,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use metrics::{counter, gauge, histogram};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use parking_lot::{Mutex, RwLock};
use serde_json::{Value, json};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
use tracing::{
    Event, Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id, Record},
};
use tracing_subscriber::{Layer, layer::Context, registry::LookupSpan};

static DEBUG_SOURCES: LazyLock<RwLock<Vec<Weak<AtomicBool>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));
static EPOCH: AtomicU64 = AtomicU64::new(0);
static RECORDER: OnceLock<Option<PrometheusHandle>> = OnceLock::new();
const ACTIVE_CAPACITY: usize = 1024;
const COMPLETED_CAPACITY: usize = 10_000;
const ACTIVE_CAPACITY_ENV: &str = "STRAVIA_PERF_TRACE_ACTIVE_CAPACITY";
const COMPLETED_CAPACITY_ENV: &str = "STRAVIA_PERF_TRACE_COMPLETED_CAPACITY";
static NEXT_SPAN_ID: AtomicU64 = AtomicU64::new(1);
static TIMELINE: LazyLock<Mutex<Timeline>> = LazyLock::new(|| Mutex::new(Timeline::default()));

/// 容量取自环境变量以便现网诊断时免重编译扩大保留窗口；缺失或非法值回退默认。
fn env_capacity(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}
static CLOCK: LazyLock<(Instant, u64)> = LazyLock::new(|| {
    let instant = Instant::now();
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
        .min(u128::from(u64::MAX)) as u64;
    (instant, micros)
});

fn timestamp(instant: Instant) -> u64 {
    CLOCK
        .1
        .saturating_add(instant.saturating_duration_since(CLOCK.0).as_micros() as u64)
}

fn micros(duration: Duration) -> u64 {
    duration.as_micros().min(u128::from(u64::MAX)) as u64
}

struct PerfSpan {
    id: u64,
    epoch: u64,
}

const WORK_FIELDS: [&str; 4] = [
    "node_count",
    "reference_count",
    "candidate_count",
    "event_count",
];

#[derive(Clone, Copy, Default)]
struct SqlActivity {
    query_count: u64,
    query_duration_us: u64,
    pool_acquire_count: u64,
    pool_acquire_duration_us: u64,
}

struct ActiveSpan {
    name: &'static str,
    id: u64,
    parent_id: Option<u64>,
    started: Instant,
    active_started: Option<Instant>,
    active_depth: usize,
    active_elapsed: Duration,
    was_entered: bool,
    status: Option<&'static str>,
    work: [Option<u64>; 4],
    sql: SqlActivity,
}

#[derive(Clone)]
struct TraceRecord {
    name: &'static str,
    id: u64,
    parent_id: Option<u64>,
    started: Instant,
    finished: Option<Instant>,
    active_us: Option<u64>,
    status: Option<&'static str>,
    work: [Option<u64>; 4],
    sql: SqlActivity,
}

impl ActiveSpan {
    fn record(&self, finished: Option<Instant>) -> TraceRecord {
        let end = finished.unwrap_or_else(Instant::now);
        let active = self.active_elapsed
            + self
                .active_started
                .map_or(Duration::ZERO, |start| end.saturating_duration_since(start));
        TraceRecord {
            name: self.name,
            id: self.id,
            parent_id: self.parent_id,
            started: self.started,
            finished,
            active_us: self.was_entered.then(|| micros(active)),
            status: finished.map(|_| self.status.unwrap_or("closed")),
            work: self.work,
            sql: self.sql,
        }
    }
}

struct Timeline {
    active: HashMap<u64, ActiveSpan>,
    completed: VecDeque<TraceRecord>,
    active_capacity: usize,
    completed_capacity: usize,
    dropped_active: u64,
    dropped_completed: u64,
    incomplete: u64,
}

impl Default for Timeline {
    fn default() -> Self {
        Self {
            active: HashMap::default(),
            completed: VecDeque::default(),
            active_capacity: env_capacity(ACTIVE_CAPACITY_ENV, ACTIVE_CAPACITY),
            completed_capacity: env_capacity(COMPLETED_CAPACITY_ENV, COMPLETED_CAPACITY),
            dropped_active: 0,
            dropped_completed: 0,
            incomplete: 0,
        }
    }
}

impl Timeline {
    fn push(&mut self, record: TraceRecord) {
        if self.completed.len() == self.completed_capacity {
            self.completed.pop_front();
            self.dropped_completed += 1;
        }
        self.completed.push_back(record);
    }

    fn invalidate(&mut self) {
        for (_, span) in std::mem::take(&mut self.active) {
            self.incomplete += 1;
            self.push(span.record(None));
        }
    }
}

/// Perfetto/Chrome Trace JSON：已结束的是 X，仍活动或因 Debug 关闭中断的是只有 B 的不完整 span。
/// 每个 span 使用自己的合成 track，tid 不是 CPU 线程；异步并发不会破坏同一 track 的嵌套约束。
pub fn timeline_snapshot() -> Value {
    let (records, live, active_capacity, completed_capacity, dropped_active, dropped_completed, incomplete_total) = {
        let state = TIMELINE.lock();
        (
            state.completed.iter().cloned().collect::<Vec<_>>(),
            state
                .active
                .values()
                .map(|span| span.record(None))
                .collect::<Vec<_>>(),
            state.active_capacity,
            state.completed_capacity,
            state.dropped_active,
            state.dropped_completed,
            state.incomplete + state.active.len() as u64,
        )
    };
    let incomplete = records
        .iter()
        .filter(|record| record.finished.is_none())
        .count()
        + live.len();
    let mut records = records.into_iter().chain(live).collect::<Vec<_>>();
    records.sort_unstable_by_key(|record| record.started);
    let present = records
        .iter()
        .map(|record| record.id)
        .collect::<HashSet<_>>();
    let mut events = Vec::with_capacity(records.len() * 3);
    for span in records {
        if let Some(parent_id) = span.parent_id.filter(|parent| present.contains(parent)) {
            // Chrome flow 箭头关联合成 track，不暗示异步子任务运行在父任务的 CPU 线程。
            let flow = json!({"name":"parent","cat":"stravia.perf","ph":"s","ts":timestamp(span.started),
                "pid":1,"tid":parent_id,"id":span.id});
            events.push(flow);
            events.push(
                json!({"name":"parent","cat":"stravia.perf","ph":"f","ts":timestamp(span.started),
                "pid":1,"tid":span.id,"id":span.id,"bp":"s"}),
            );
        }
        let mut args = json!({ "span_id": span.id });
        if let Some(parent_id) = span.parent_id {
            args["parent_id"] = json!(parent_id);
        }
        if let Some(status) = span.status {
            args["status"] = json!(status);
        }
        if let Some(active_us) = span.active_us {
            args["active_us"] = json!(active_us);
        }
        for (name, value) in WORK_FIELDS.into_iter().zip(span.work) {
            if let Some(value) = value {
                args[name] = json!(value);
            }
        }
        if span.sql.query_count > 0 {
            args["sql_query_count"] = json!(span.sql.query_count);
            args["sql_query_duration_us"] = json!(span.sql.query_duration_us);
        }
        if span.sql.pool_acquire_count > 0 {
            args["sql_pool_acquire_count"] = json!(span.sql.pool_acquire_count);
            args["sql_pool_acquire_duration_us"] = json!(span.sql.pool_acquire_duration_us);
        }
        let mut event = json!({
            "name": span.name,
            "cat": "stravia.perf",
            "ph": if span.finished.is_some() { "X" } else { "B" },
            "ts": timestamp(span.started),
            "pid": 1,
            "tid": span.id,
            "args": args,
        });
        if let Some(finished) = span.finished {
            event["dur"] = json!(micros(finished.saturating_duration_since(span.started)));
        }
        events.push(event);
    }
    json!({
        "traceEvents": events,
        "displayTimeUnit": "ms",
        "metadata": {
            "capacity": { "active": active_capacity, "completed": completed_capacity },
            "dropped_active": dropped_active,
            "dropped_completed": dropped_completed,
            "incomplete": incomplete,
            "incomplete_total": incomplete_total,
            "track": "synthetic span tracks; tid is not a CPU thread",
        }
    })
}
struct ProcessSampler {
    system: System,
    warmed_epoch: Option<u64>,
}

static PROCESS: LazyLock<Mutex<ProcessSampler>> = LazyLock::new(|| {
    Mutex::new(ProcessSampler {
        system: System::new(),
        warmed_epoch: None,
    })
});

fn sources() -> &'static RwLock<Vec<Weak<AtomicBool>>> {
    &DEBUG_SOURCES
}

/// 直接绑定 Observation 的原子开关，不复制状态；弱引用不延长 Gateway 生命周期。
pub(crate) fn bind_debug(debug: &Arc<AtomicBool>) {
    let mut sources = sources().write();
    sources.retain(|source| source.strong_count() > 0);
    sources.push(Arc::downgrade(debug));
}

pub(crate) fn set_debug(debug: &AtomicBool, enabled: bool) {
    // 切换与 span 创建/结束及指标写入串行；旧会话仅保留不完整的时间线，
    // 即使随后重开，也绝不把旧 span 算入新会话的指标。
    let _sources = sources().write();
    if debug.swap(enabled, Ordering::AcqRel) != enabled {
        TIMELINE.lock().invalidate();
        EPOCH.fetch_add(1, Ordering::AcqRel);
    }
}

fn any_enabled(sources: &[Weak<AtomicBool>]) -> bool {
    sources.iter().any(|source| {
        source
            .upgrade()
            .is_some_and(|debug| debug.load(Ordering::Acquire))
    })
}

pub fn enabled() -> bool {
    any_enabled(&sources().read())
}

/// 每个进程仅安装一次 recorder，不随 Gateway 或 HTTP listener 重装。
/// 其他代码先安装全局 recorder 时返回 false。
pub fn init_metrics() -> bool {
    RECORDER
        .get_or_init(|| {
            // SQL 和较长的 Run/工具操作使用固定且不同的 Prometheus 直方图桶。
            const OPERATIONS: &[f64] = &[
                0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
                120.0,
            ];
            const SQL: &[f64] = &[
                0.0001, 0.0005, 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 5.0,
            ];
            PrometheusBuilder::new()
                .set_buckets(OPERATIONS)
                .expect("fixed nonempty operation buckets")
                .set_buckets_for_metric(
                    Matcher::Full("stravia_sql_query_duration_seconds".into()),
                    SQL,
                )
                .expect("fixed nonempty SQL buckets")
                .set_buckets_for_metric(
                    Matcher::Full("stravia_sql_pool_acquire_duration_seconds".into()),
                    SQL,
                )
                .expect("fixed nonempty pool buckets")
                .install_recorder()
                .map_err(|error| {
                    eprintln!("Stravia performance recorder unavailable: {error}");
                    error
                })
                .ok()
        })
        .is_some()
}

#[derive(Default)]
struct StatusVisitor {
    status: Option<&'static str>,
    work: [Option<u64>; 4],
}

impl Visit for StatusVisitor {
    fn record_u64(&mut self, field: &Field, value: u64) {
        let index = match field.name() {
            "node_count" => 0,
            "reference_count" => 1,
            "candidate_count" => 2,
            "event_count" => 3,
            _ => return,
        };
        self.work[index] = Some(value);
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "status" {
            self.status = match value {
                "completed" => Some("completed"),
                "error" => Some("error"),
                "cancelled" => Some("cancelled"),
                "abandoned" => Some("abandoned"),
                _ => None,
            };
        }
    }

    fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
}

/// 静态 span 名称是唯一 operation 标签；仅接受状态和工作量数值白名单。
/// 内存上限默认活动 1024 + 已结束 10000 条，可用 STRAVIA_PERF_TRACE_ACTIVE_CAPACITY
/// 与 STRAVIA_PERF_TRACE_COMPLETED_CAPACITY 在进程启动时调整；满额的活动 span 不导出也不记指标。
pub struct PerformanceLayer;

impl<S> Layer<S> for PerformanceLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        if attrs.metadata().target() != "stravia::perf" {
            return;
        }
        let sources = sources().read();
        if !any_enabled(&sources) {
            return;
        }
        let epoch = EPOCH.load(Ordering::Acquire);
        let current = ctx.current_span();
        let parent = attrs.parent().or_else(|| {
            if attrs.is_contextual() {
                current.id()
            } else {
                None
            }
        });
        let parent_id = parent.and_then(|parent| ctx.span(parent)).and_then(|span| {
            // 业务日志 span 可能夹在两级 perf span 之间，沿真实 tracing 父链
            // 找最近的 perf 祖先，而不是把非 perf span 误当成根。
            span.scope().find_map(|ancestor| {
                ancestor
                    .extensions()
                    .get::<PerfSpan>()
                    .filter(|perf| perf.epoch == epoch)
                    .map(|perf| perf.id)
            })
        });
        let mut status = StatusVisitor::default();
        attrs.record(&mut status);
        let Some(span) = ctx.span(id) else { return };
        let mut state = TIMELINE.lock();
        if state.active.len() == state.active_capacity {
            state.dropped_active += 1;
            return;
        }
        let _ = &*CLOCK;
        let span_id = NEXT_SPAN_ID.fetch_add(1, Ordering::Relaxed);
        let started = Instant::now();
        state.active.insert(
            span_id,
            ActiveSpan {
                name: attrs.metadata().name(),
                id: span_id,
                parent_id,
                started,
                active_started: None,
                active_depth: 0,
                active_elapsed: Duration::ZERO,
                was_entered: false,
                status: status.status,
                work: status.work,
                sql: SqlActivity::default(),
            },
        );
        span.extensions_mut()
            .insert(PerfSpan { id: span_id, epoch });
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let extensions = span.extensions();
        let Some(perf) = extensions.get::<PerfSpan>() else {
            return;
        };
        let sources = sources().read();
        if !any_enabled(&sources) || perf.epoch != EPOCH.load(Ordering::Acquire) {
            return;
        }
        let mut status = StatusVisitor::default();
        values.record(&mut status);
        if let Some(active) = TIMELINE.lock().active.get_mut(&perf.id) {
            if let Some(status) = status.status {
                active.status = Some(status);
            }
            for (current, value) in active.work.iter_mut().zip(status.work) {
                if value.is_some() {
                    *current = value;
                }
            }
        }
    }

    fn on_enter(&self, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let extensions = span.extensions();
        let Some(perf) = extensions.get::<PerfSpan>() else {
            return;
        };
        let sources = sources().read();
        if !any_enabled(&sources) || perf.epoch != EPOCH.load(Ordering::Acquire) {
            return;
        }
        if let Some(active) = TIMELINE.lock().active.get_mut(&perf.id) {
            if active.active_depth == 0 {
                active.active_started = Some(Instant::now());
            }
            active.active_depth += 1;
            active.was_entered = true;
        }
    }

    fn on_exit(&self, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let extensions = span.extensions();
        let Some(perf) = extensions.get::<PerfSpan>() else {
            return;
        };
        let sources = sources().read();
        if !any_enabled(&sources) || perf.epoch != EPOCH.load(Ordering::Acquire) {
            return;
        }
        if let Some(active) = TIMELINE.lock().active.get_mut(&perf.id)
            && active.active_depth > 0
        {
            active.active_depth -= 1;
            if active.active_depth == 0
                && let Some(start) = active.active_started.take()
            {
                active.active_elapsed += start.elapsed();
            }
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else { return };
        let Some(perf) = span.extensions_mut().remove::<PerfSpan>() else {
            return;
        };
        let sources = sources().read();
        if !any_enabled(&sources) || perf.epoch != EPOCH.load(Ordering::Acquire) {
            return;
        }
        let mut state = TIMELINE.lock();
        let Some(active) = state.active.remove(&perf.id) else {
            return;
        };
        let finished = Instant::now();
        let record = active.record(Some(finished));
        let status = record.status.expect("completed trace has status");
        state.push(record);
        drop(state);
        histogram!("stravia_operation_duration_seconds", "operation" => active.name, "status" => status)
            .record(finished.saturating_duration_since(active.started).as_secs_f64());
    }
}

pub(crate) fn record_observation_queue_depth(depth: usize) {
    let guard = sources().read();
    if any_enabled(&guard) {
        gauge!("stravia_observation_writer_queue_depth").set(depth as f64);
    }
}

pub(crate) fn record_generation_cache_bytes(bytes: usize) {
    let guard = sources().read();
    if any_enabled(&guard) {
        gauge!("stravia_generation_materialization_cache_bytes").set(bytes as f64);
    }
}

pub(crate) fn record_generation_cache_access(hit: bool) {
    let guard = sources().read();
    if any_enabled(&guard) {
        counter!(
            "stravia_generation_materialization_cache_access_total",
            "result" => if hit { "hit" } else { "miss" }
        )
        .increment(1);
    }
}

/// 每五秒清理直方图并采样进程。每个 Gateway 持有自己的任务，在关停时中止；
/// 正常运行时只有一个 Gateway，测试中多个 Gateway 仍共享唯一 recorder。
pub(crate) fn spawn_upkeep() -> Option<tokio::task::JoinHandle<()>> {
    let handle = RECORDER.get()?.as_ref()?.clone();
    Some(tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            interval.tick().await;
            sample_process();
            handle.run_upkeep();
        }
    }))
}

fn sample_process() {
    if !enabled() {
        return;
    }
    let epoch = EPOCH.load(Ordering::Acquire);
    let pid = match sysinfo::get_current_pid() {
        Ok(pid) => pid,
        Err(error) => {
            tracing::warn!(%error, "performance process sampling unavailable");
            return;
        }
    };
    let mut sampler = PROCESS.lock();
    sampler.system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        false,
        ProcessRefreshKind::nothing().with_memory().with_cpu(),
    );
    let guard = sources().read();
    if any_enabled(&guard) && epoch == EPOCH.load(Ordering::Acquire) {
        if let Some(process) = sampler.system.process(pid) {
            gauge!("stravia_process_rss_bytes").set(process.memory() as f64);
            // 首个样本没有可比较的上次 CPU 读数，不能输出伪造的 0。
            if sampler.warmed_epoch == Some(epoch) {
                gauge!("stravia_process_cpu_percent").set(f64::from(process.cpu_usage()));
            }
        }
        sampler.warmed_epoch = Some(epoch);
    }
}

/// Prometheus 0.0.4 文本；关闭 Debug 后旧数据仍可下载。
/// 进程采样仅由定时任务驱动，不因导出请求产生新样本。
pub fn metrics_snapshot() -> Option<String> {
    let handle = RECORDER.get()?.as_ref()?;
    handle.run_upkeep();
    Some(handle.render())
}

/// SQLx 事件只读取数值；绝不访问 `summary`、`db.statement` 或其他 SQL 字段。
#[derive(Default)]
struct ElapsedVisitor {
    elapsed_secs: Option<f64>,
    acquired_after_secs: Option<f64>,
}

impl Visit for ElapsedVisitor {
    fn record_f64(&mut self, field: &Field, value: f64) {
        match field.name() {
            "elapsed_secs" => self.elapsed_secs = Some(value),
            "acquired_after_secs" => self.acquired_after_secs = Some(value),
            _ => {}
        }
    }

    fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
}

pub fn is_sqlx_metric_target(target: &str) -> bool {
    matches!(target, "sqlx::query" | "sqlx::pool::acquire")
}

/// 在日志初始化时安装目标过滤；SQL 不进入 fmt layers。
pub struct SqlxMetricsLayer;

impl<S: Subscriber + for<'lookup> LookupSpan<'lookup>> Layer<S> for SqlxMetricsLayer {
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let target = event.metadata().target();
        if !is_sqlx_metric_target(target) {
            return;
        }
        let epoch = EPOCH.load(Ordering::Acquire);
        let mut values = ElapsedVisitor::default();
        event.record(&mut values);
        let guard = sources().read();
        if !any_enabled(&guard) || EPOCH.load(Ordering::Acquire) != epoch {
            return;
        }
        let owner = ctx.event_scope(event).and_then(|mut scope| {
            scope.find_map(|span| {
                span.extensions()
                    .get::<PerfSpan>()
                    .map(|perf| (perf.id, perf.epoch, span.metadata().name()))
            })
        });
        // SQLite worker 会跨线程持有提交查询时的 span；重开 Debug 后不能
        // 把旧周期尚未完成的查询计入新周期，也不能回退为 unattributed。
        if owner.is_some_and(|(_, span_epoch, _)| span_epoch != epoch) {
            return;
        }
        let operation = owner.map_or("unattributed", |(_, _, name)| name);
        if target == "sqlx::query" {
            if let Some(value) = values
                .elapsed_secs
                .filter(|value| value.is_finite() && *value >= 0.0)
            {
                histogram!("stravia_sql_query_duration_seconds", "operation" => operation)
                    .record(value);
                if let Some((id, _, _)) = owner
                    && let Some(active) = TIMELINE.lock().active.get_mut(&id)
                {
                    active.sql.query_count += 1;
                    active.sql.query_duration_us = active
                        .sql
                        .query_duration_us
                        .saturating_add((value * 1_000_000.0) as u64);
                }
            }
        } else if let Some(value) = values
            .acquired_after_secs
            .filter(|value| value.is_finite() && *value >= 0.0)
        {
            histogram!("stravia_sql_pool_acquire_duration_seconds", "operation" => operation)
                .record(value);
            if let Some((id, _, _)) = owner
                && let Some(active) = TIMELINE.lock().active.get_mut(&id)
            {
                active.sql.pool_acquire_count += 1;
                active.sql.pool_acquire_duration_us = active
                    .sql
                    .pool_acquire_duration_us
                    .saturating_add((value * 1_000_000.0) as u64);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::sync::Barrier;
    use tracing::{Instrument, Level, subscriber::Interest};
    use tracing_subscriber::{filter::dynamic_filter_fn, layer::SubscriberExt};

    struct OtherSpans;
    impl<S: Subscriber> Layer<S> for OtherSpans {}

    fn isolated(name: &str) -> bool {
        if std::env::var("STRAVIA_PERFORMANCE_TEST_CHILD")
            .ok()
            .as_deref()
            != Some(name)
        {
            let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
                .arg("--exact")
                .arg(format!("performance::tests::{name}"))
                .env("STRAVIA_PERFORMANCE_TEST_CHILD", name)
                .output()
                .expect("isolated performance regression");
            assert!(
                output.status.success(),
                "isolated performance regression failed: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            return false;
        }
        true
    }

    fn subscriber() -> impl Subscriber + Send + Sync {
        tracing_subscriber::registry()
            .with(
                PerformanceLayer.with_filter(
                    dynamic_filter_fn(|meta, _ctx| meta.target() == "stravia::perf" && enabled())
                        .with_callsite_filter(|meta| {
                            if meta.target() == "stravia::perf" {
                                Interest::sometimes()
                            } else {
                                Interest::never()
                            }
                        }),
                ),
            )
            .with(
                SqlxMetricsLayer.with_filter(
                    dynamic_filter_fn(|meta, _ctx| {
                        (is_sqlx_metric_target(meta.target()) || meta.target() == "stravia::perf")
                            && enabled()
                    })
                    .with_callsite_filter(|meta| {
                        if is_sqlx_metric_target(meta.target()) || meta.target() == "stravia::perf"
                        {
                            Interest::sometimes()
                        } else {
                            Interest::never()
                        }
                    }),
                ),
            )
    }

    fn emit_reused_callsite() {
        let span = tracing::info_span!(target: "stravia::perf", "test.reused", status = tracing::field::Empty);
        span.record("status", "completed");
    }

    #[test]
    fn spans_and_sql_share_debug_epoch_and_only_export_safe_fields() {
        if !isolated("spans_and_sql_share_debug_epoch_and_only_export_safe_fields") {
            return;
        }
        let recorder = PrometheusBuilder::new()
            .set_buckets(&[0.001, 0.01, 0.1, 1.0])
            .expect("nonempty buckets")
            .build_recorder();
        let handle = recorder.handle();
        let debug = Arc::new(AtomicBool::new(false));
        bind_debug(&debug);
        metrics::with_local_recorder(&recorder, || {
            tracing::subscriber::with_default(subscriber(), || {
                emit_reused_callsite();
                emit_sql_event();
                set_debug(&debug, true);
                let stale = tracing::info_span!(target: "stravia::perf", "test.old_session");
                emit_sql_event();
                set_debug(&debug, false);
                emit_sql_event();
                drop(stale);
                set_debug(&debug, true);
                emit_reused_callsite();
                std::thread::sleep(Duration::from_millis(2));
                let parent = tracing::info_span!(target: "stravia::perf", "test.parent",
                    status = tracing::field::Empty, password = "secret-password",
                    url = "https://secret.example", error = "secret-error");
                {
                    let _entered = parent.enter();
                    let child = tracing::info_span!(target: "stravia::perf",
                        "test.child", status = tracing::field::Empty,
                        db_statement = "SELECT secret FROM vault");
                    child.record("status", "error");
                    drop(child);
                }
                let explicit = tracing::info_span!(target: "stravia::perf", parent: &parent,
                    "test.explicit_child", status = tracing::field::Empty);
                explicit.record("status", "not-a-valid-status");
                drop(explicit);
                parent.record("status", "completed");
                drop(parent);
                set_debug(&debug, false);
            });
        });
        handle.run_upkeep();
        let output = handle.render();
        assert!(output.contains("stravia_sql_query_duration_seconds_bucket"));
        assert!(
            output
                .contains("stravia_sql_query_duration_seconds_count{operation=\"unattributed\"} 1")
        );
        assert!(output.contains("operation=\"test.reused\",status=\"completed\""));
        assert!(output.contains("operation=\"test.child\",status=\"error\""));
        assert!(output.contains("operation=\"test.explicit_child\",status=\"closed\""));
        assert!(!output.contains("test.old_session"));
        assert!(!output.contains("SELECT"));
        assert!(!output.contains("password"));
        let snapshot = timeline_snapshot();
        let events = snapshot["traceEvents"]
            .as_array()
            .expect("Chrome trace events");
        let get = |name| {
            events
                .iter()
                .find(|event| event["name"] == name && event["ph"] == "X")
                .expect("completed trace")
        };
        let parent = get("test.parent");
        assert!(
            parent["ts"].as_u64().unwrap() > get("test.reused")["ts"].as_u64().unwrap(),
            "two spans before the first snapshot must retain distinct start times"
        );
        for name in ["test.child", "test.explicit_child"] {
            let child = get(name);
            assert_eq!(child["args"]["parent_id"], parent["args"]["span_id"]);
            assert!(child["dur"].as_u64().is_some());
            assert!(
                events
                    .iter()
                    .any(|event| event["ph"] == "s" && event["id"] == child["tid"])
            );
            assert!(
                events
                    .iter()
                    .any(|event| event["ph"] == "f" && event["id"] == child["tid"])
            );
        }
        assert_eq!(get("test.explicit_child")["args"]["status"], "closed");
        assert!(
            !events
                .iter()
                .any(|event| event["name"] == "test.old_session" && event["ph"] == "X")
        );
        let serialized = snapshot.to_string();
        for secret in [
            "secret-password",
            "secret.example",
            "secret-error",
            "SELECT",
            "not-a-valid-status",
        ] {
            assert!(!serialized.contains(secret), "sensitive field leaked");
        }
    }

    #[test]
    fn concurrent_enters_and_async_polls_have_union_not_sum() {
        if !isolated("concurrent_enters_and_async_polls_have_union_not_sum") {
            return;
        }
        let debug = Arc::new(AtomicBool::new(true));
        bind_debug(&debug);
        let dispatch = tracing::Dispatch::new(subscriber());
        tracing::dispatcher::with_default(&dispatch, || {
            let span = tracing::info_span!(target: "stravia::perf", "test.concurrent");
            let barrier = Arc::new(Barrier::new(2));
            let thread_span = span.clone();
            let thread_barrier = barrier.clone();
            let child_dispatch = dispatch.clone();
            let thread = std::thread::spawn(move || {
                tracing::dispatcher::with_default(&child_dispatch, || {
                    let _entered = thread_span.enter();
                    thread_barrier.wait();
                    thread_barrier.wait();
                });
            });
            {
                let _first = span.enter();
                let _again = span.enter();
                barrier.wait();
                std::thread::sleep(Duration::from_millis(2));
                barrier.wait();
            }
            thread.join().expect("concurrent enter");
            drop(span);

            // future 两次 poll 在不同线程，instrument 的 guard 每次 poll 后退出。
            let future_span = tracing::info_span!(target: "stravia::perf", "test.async_polls");
            let mut polls = 0;
            let future = futures::future::poll_fn(move |_cx| {
                polls += 1;
                if polls == 1 {
                    std::task::Poll::Pending
                } else {
                    std::task::Poll::Ready(())
                }
            })
            .instrument(future_span.clone());
            let mut future = Box::pin(future);
            let waker = futures::task::noop_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            assert!(future.as_mut().poll(&mut cx).is_pending());
            let thread_dispatch = dispatch.clone();
            future = std::thread::spawn(move || {
                tracing::dispatcher::with_default(&thread_dispatch, || {
                    let waker = futures::task::noop_waker();
                    let mut cx = std::task::Context::from_waker(&waker);
                    assert!(future.as_mut().poll(&mut cx).is_ready());
                    future
                })
            })
            .join()
            .expect("second poll");
            drop(future);
            drop(future_span);
            set_debug(&debug, false);
        });
        let snapshot = timeline_snapshot();
        for name in ["test.concurrent", "test.async_polls"] {
            let record = snapshot["traceEvents"]
                .as_array()
                .unwrap()
                .iter()
                .find(|event| event["name"] == name)
                .expect("recorded span");
            assert_eq!(record["ph"], "X");
            assert!(
                record["args"]["active_us"].as_u64().unwrap() <= record["dur"].as_u64().unwrap()
            );
        }
    }

    #[test]
    fn non_perf_spans_keep_nearest_perf_ancestor() {
        if !isolated("non_perf_spans_keep_nearest_perf_ancestor") {
            return;
        }
        let debug = Arc::new(AtomicBool::new(true));
        bind_debug(&debug);
        tracing::subscriber::with_default(
            tracing_subscriber::registry()
                .with(PerformanceLayer)
                .with(OtherSpans),
            || {
                let parent = tracing::info_span!(target: "stravia::perf", "test.outer");
                {
                    let _parent = parent.enter();
                    let other = tracing::info_span!(target: "application::log", "ignored.log_span");
                    let _other = other.enter();
                    let child =
                        tracing::info_span!(target: "stravia::perf", "test.nested_after_log");
                    drop(child);
                }
                drop(parent);
                set_debug(&debug, false);
            },
        );
        let trace = timeline_snapshot();
        let events = trace["traceEvents"].as_array().unwrap();
        let parent = events
            .iter()
            .find(|event| event["name"] == "test.outer")
            .unwrap();
        let child = events
            .iter()
            .find(|event| event["name"] == "test.nested_after_log")
            .unwrap();
        assert_eq!(child["args"]["parent_id"], parent["args"]["span_id"]);
        assert!(
            !events
                .iter()
                .any(|event| event["name"] == "ignored.log_span")
        );
    }

    #[test]
    fn bounded_timeline_preserves_incomplete_spans_and_reports_overflow() {
        if !isolated("bounded_timeline_preserves_incomplete_spans_and_reports_overflow") {
            return;
        }
        let debug = Arc::new(AtomicBool::new(true));
        bind_debug(&debug);
        tracing::subscriber::with_default(subscriber(), || {
            let active = (0..ACTIVE_CAPACITY)
                .map(|_| tracing::info_span!(target: "stravia::perf", "test.holding"))
                .collect::<Vec<_>>();
            let over = tracing::info_span!(target: "stravia::perf", "test.overflow_active");
            drop(over);
            set_debug(&debug, false);
            let frozen = timeline_snapshot();
            assert_eq!(
                frozen["metadata"]["incomplete"].as_u64(),
                Some(ACTIVE_CAPACITY as u64)
            );
            assert_eq!(frozen["metadata"]["dropped_active"], 1);
            assert_eq!(
                frozen["traceEvents"].as_array().unwrap().len(),
                ACTIVE_CAPACITY
            );
            assert!(
                frozen["traceEvents"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|event| event["ph"] == "B" && event.get("dur").is_none())
            );
            drop(active);
            assert_eq!(
                timeline_snapshot(),
                frozen,
                "disabled snapshot must stay frozen"
            );
            set_debug(&debug, true);
            for _ in 0..COMPLETED_CAPACITY + 20 {
                let span = tracing::info_span!(target: "stravia::perf", "test.completed_buffer",
                    status = "completed");
                drop(span);
            }
            set_debug(&debug, false);
        });
        let snapshot = timeline_snapshot();
        assert_eq!(
            snapshot["metadata"]["capacity"]["active"].as_u64(),
            Some(ACTIVE_CAPACITY as u64)
        );
        assert_eq!(
            snapshot["metadata"]["capacity"]["completed"].as_u64(),
            Some(COMPLETED_CAPACITY as u64)
        );
        assert_eq!(
            snapshot["metadata"]["dropped_completed"].as_u64(),
            Some((ACTIVE_CAPACITY + 20) as u64)
        );
        assert_eq!(snapshot["metadata"]["incomplete"], 0);
        assert_eq!(
            snapshot["metadata"]["incomplete_total"].as_u64(),
            Some(ACTIVE_CAPACITY as u64)
        );
        assert_eq!(
            snapshot["traceEvents"].as_array().unwrap().len(),
            COMPLETED_CAPACITY
        );
        assert_eq!(snapshot["displayTimeUnit"], "ms");
        assert!(
            snapshot["traceEvents"]
                .as_array()
                .unwrap()
                .iter()
                .all(|event| event["ph"] == "X" && event["dur"].as_u64().is_some())
        );
    }

    fn emit_sql_event() {
        tracing::event!(
            target: "sqlx::query", Level::DEBUG,
            summary = "SELECT password", db.statement = "SELECT password FROM vault",
            elapsed_secs = 0.025_f64,
        );
    }

    #[test]
    fn sqlite_worker_queries_keep_exclusive_operation_attribution() {
        if !isolated("sqlite_worker_queries_keep_exclusive_operation_attribution") {
            return;
        }
        let debug = Arc::new(AtomicBool::new(false));
        bind_debug(&debug);
        assert!(init_metrics());
        tracing::subscriber::set_global_default(subscriber()).expect("isolated subscriber");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .test_before_acquire(false)
                .acquire_time_level(log::LevelFilter::Debug)
                .connect("sqlite::memory:")
                .await
                .unwrap();
            set_debug(&debug, true);
            async {
                let mut connection = pool.acquire().await.unwrap();
                let value: i64 = sqlx::query_scalar("SELECT 41 + 1")
                    .fetch_one(&mut *connection)
                    .await
                    .unwrap();
                assert_eq!(value, 42);
                async {
                    for secret in ["private-query-value-one", "private-query-value-two"] {
                        let value: String = sqlx::query_scalar("SELECT ?")
                            .bind(secret)
                            .fetch_one(&mut *connection)
                            .await
                            .unwrap();
                        assert_eq!(value, secret);
                    }
                    // fetch_one 可先于 worker 的 QueryLogger 析构返回；ping 是
                    // 同一连接的 FIFO 屏障，不产生额外 SQL，也不依赖时间等待。
                    sqlx::Connection::ping(&mut *connection).await.unwrap();
                }
                .instrument(tracing::info_span!(
                    target: "stravia::perf", "test.sqlite.child", node_count = 2_u64
                ))
                .await;
            }
            .instrument(tracing::info_span!(target: "stravia::perf", "test.sqlite.parent"))
            .await;
            set_debug(&debug, false);
            pool.close().await;
        });
        let trace = timeline_snapshot();
        let events = trace["traceEvents"].as_array().unwrap();
        let get = |name| {
            events
                .iter()
                .find(|event| event["name"] == name && event["ph"] == "X")
                .unwrap()
        };
        let parent = get("test.sqlite.parent");
        let child = get("test.sqlite.child");
        assert_eq!(parent["args"]["sql_query_count"], 1);
        assert_eq!(parent["args"]["sql_pool_acquire_count"], 1);
        assert_eq!(child["args"]["sql_query_count"], 2);
        assert_eq!(child["args"]["node_count"], 2);
        assert_eq!(child["args"]["parent_id"], parent["args"]["span_id"]);
        let metrics = metrics_snapshot().unwrap();
        for (operation, count) in [("test.sqlite.parent", 1), ("test.sqlite.child", 2)] {
            assert!(metrics.contains(&format!(
                "stravia_sql_query_duration_seconds_count{{operation=\"{operation}\"}} {count}\n"
            )));
        }
        assert!(!metrics.contains("operation=\"unattributed\""));
        for output in [metrics, trace.to_string()] {
            for secret in [
                "SELECT",
                "private-query-value-one",
                "private-query-value-two",
            ] {
                assert!(!output.contains(secret), "query data leaked into telemetry");
            }
        }
    }

    #[test]
    fn sql_attribution_rejects_stale_epochs_and_unapproved_work_fields() {
        if !isolated("sql_attribution_rejects_stale_epochs_and_unapproved_work_fields") {
            return;
        }
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let debug = Arc::new(AtomicBool::new(false));
        bind_debug(&debug);
        metrics::with_local_recorder(&recorder, || {
            tracing::subscriber::with_default(subscriber(), || {
                record_generation_cache_access(true);
                set_debug(&debug, true);
                let stale = tracing::info_span!(target: "stravia::perf", "test.stale_sql");
                set_debug(&debug, false);
                set_debug(&debug, true);
                stale.in_scope(emit_sql_event);
                drop(stale);
                let current = tracing::info_span!(
                    target: "stravia::perf", "test.current_sql",
                    node_count = 3_u64, reference_count = tracing::field::Empty,
                    secret_number = 919191_u64, candidate_count = "private-count"
                );
                current.in_scope(|| {
                    emit_sql_event();
                    tracing::event!(target: "sqlx::pool::acquire", Level::DEBUG,
                        acquired_after_secs = 0.002_f64);
                    current.record("reference_count", 7_u64);
                    current.record("node_count", -1_i64);
                    record_generation_cache_access(true);
                    record_generation_cache_access(false);
                });
                drop(current);
                set_debug(&debug, false);
                record_generation_cache_access(false);
            });
        });
        handle.run_upkeep();
        let metrics = handle.render();
        assert!(!metrics.contains("test.stale_sql"));
        assert!(!metrics.contains("operation=\"unattributed\""));
        for result in ["hit", "miss"] {
            assert!(metrics.contains(&format!(
                "stravia_generation_materialization_cache_access_total{{result=\"{result}\"}} 1\n"
            )));
        }
        let snapshot = timeline_snapshot();
        let events = snapshot["traceEvents"].as_array().unwrap();
        let current = events
            .iter()
            .find(|event| event["name"] == "test.current_sql")
            .unwrap();
        assert_eq!(current["args"]["sql_query_count"], 1);
        assert_eq!(current["args"]["sql_query_duration_us"], 25_000);
        assert_eq!(current["args"]["sql_pool_acquire_count"], 1);
        assert_eq!(current["args"]["sql_pool_acquire_duration_us"], 2_000);
        assert_eq!(current["args"]["node_count"], 3);
        assert_eq!(current["args"]["reference_count"], 7);
        assert!(current["args"].get("candidate_count").is_none());
        let stale = events
            .iter()
            .find(|event| event["name"] == "test.stale_sql")
            .unwrap();
        assert!(stale["args"].get("sql_query_count").is_none());
        let serialized = snapshot.to_string();
        assert!(!serialized.contains("919191"));
        assert!(!serialized.contains("private-count"));
    }
}
