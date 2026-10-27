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

mod history;
mod resources;
mod sql;
pub(crate) use resources::{ResourceLease, ResourceUsage};
use sql::{PoolPoint, SpanKind, SqlDetails};

#[derive(Clone)]
struct ResourceSample {
    at: Instant,
    epoch: u64,
    rss_bytes: Option<u64>,
    cpu_percent: Option<f64>,
    resources: Vec<resources::ResourcePoint>,
    pools: Vec<PoolPoint>,
}

static DEBUG_SOURCES: LazyLock<RwLock<Vec<Weak<AtomicBool>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));
static EPOCH: AtomicU64 = AtomicU64::new(0);
static RECORDER: OnceLock<Option<PrometheusHandle>> = OnceLock::new();
const ACTIVE_CAPACITY: usize = 1024;
const COMPLETED_CAPACITY: usize = 10_000;
const RESOURCE_CAPACITY: usize = 720;
const SQL_COMPLETED_CAPACITY: usize = 2048;
const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);
const ACTIVE_CAPACITY_ENV: &str = "STRAVIA_PERF_TRACE_ACTIVE_CAPACITY";
const COMPLETED_CAPACITY_ENV: &str = "STRAVIA_PERF_TRACE_COMPLETED_CAPACITY";
const RESOURCE_CAPACITY_ENV: &str = "STRAVIA_PERF_RESOURCE_CAPACITY";
const SQL_COMPLETED_CAPACITY_ENV: &str = "STRAVIA_PERF_SQL_COMPLETED_CAPACITY";
static NEXT_SPAN_ID: AtomicU64 = AtomicU64::new(1);
static TIMELINE: LazyLock<Mutex<Timeline>> = LazyLock::new(|| Mutex::new(Timeline::default()));

/// 环境变量仅调整内存缓冲；已完成历史溢出到本轮匿名文件，不因条数淘汰。
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
    operation: &'static str,
    owner_id: u64,
}

/// 只复制内部身份，不保留 tracing Span，后台排队不能延长请求生命周期。
#[derive(Clone, Copy, Default)]
pub(crate) struct PerfContext {
    span_id: u64,
    epoch: u64,
}

impl PerfContext {
    pub(crate) fn span_id(self) -> u64 {
        self.span_id
    }
    pub(crate) fn epoch(self) -> u64 {
        self.epoch
    }
}

pub(crate) fn current_context() -> PerfContext {
    tracing::Span::current()
        .with_subscriber(|(id, dispatch)| {
            let registry = dispatch.downcast_ref::<tracing_subscriber::Registry>()?;
            registry.span(id)?.scope().find_map(|span| {
                span.extensions().get::<PerfSpan>().map(|perf| PerfContext {
                    span_id: perf.id,
                    epoch: perf.epoch,
                })
            })
        })
        .flatten()
        .unwrap_or_else(|| PerfContext {
            span_id: 0,
            epoch: EPOCH.load(Ordering::Acquire),
        })
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
    request_id: Option<String>,
    command_kind: Option<&'static str>,
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
    detail: SqlDetails,
    kind: SpanKind,
    operation: &'static str,
    owner_id: u64,
}

#[derive(Clone)]
struct TraceRecord {
    name: &'static str,
    request_id: Option<String>,
    command_kind: Option<&'static str>,
    id: u64,
    parent_id: Option<u64>,
    started: Instant,
    finished: Option<Instant>,
    active_us: Option<u64>,
    status: Option<&'static str>,
    work: [Option<u64>; 4],
    sql: SqlActivity,
    detail: SqlDetails,
    kind: SpanKind,
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
            request_id: self.request_id.clone(),
            command_kind: self.command_kind,
            id: self.id,
            parent_id: self.parent_id,
            started: self.started,
            finished,
            active_us: self.was_entered.then(|| micros(active)),
            status: finished.map(|_| self.status.unwrap_or("closed")),
            work: self.work,
            sql: self.sql,
            detail: self.detail.clone(),
            kind: self.kind,
        }
    }

    fn into_record(mut self, finished: Option<Instant>) -> TraceRecord {
        let detail = std::mem::take(&mut self.detail);
        let request_id = self.request_id.take();
        let mut record = self.record(finished);
        record.detail = detail;
        record.request_id = request_id;
        record
    }
}

struct Timeline {
    active: HashMap<u64, ActiveSpan>,
    history: history::History,
    completed: VecDeque<TraceRecord>,
    sql_completed: VecDeque<TraceRecord>,
    samples: VecDeque<ResourceSample>,
    active_capacity: usize,
    completed_capacity: usize,
    sql_capacity: usize,
    resource_capacity: usize,
    dropped_active: u64,
    current_resources: Vec<resources::ResourcePoint>,
    pools: HashMap<(&'static str, &'static str), PoolPoint>,
}

impl Default for Timeline {
    fn default() -> Self {
        Self {
            active: HashMap::default(),
            history: history::History::new(),
            completed: VecDeque::new(),
            sql_completed: VecDeque::new(),
            samples: VecDeque::new(),
            active_capacity: env_capacity(ACTIVE_CAPACITY_ENV, ACTIVE_CAPACITY),
            completed_capacity: env_capacity(COMPLETED_CAPACITY_ENV, COMPLETED_CAPACITY),
            sql_capacity: env_capacity(SQL_COMPLETED_CAPACITY_ENV, SQL_COMPLETED_CAPACITY),
            resource_capacity: env_capacity(RESOURCE_CAPACITY_ENV, RESOURCE_CAPACITY),
            dropped_active: 0,
            current_resources: Vec::new(),
            pools: HashMap::default(),
        }
    }
}

impl Timeline {
    fn push(&mut self, record: TraceRecord) {
        let (queue, capacity) = if record.kind == SpanKind::Operation {
            (&mut self.completed, self.completed_capacity)
        } else {
            (&mut self.sql_completed, self.sql_capacity)
        };
        queue.push_back(record);
        if queue.len() == capacity {
            // 整批转移所有权；后台追加完成后释放该批分配，不维持满额滑动窗口。
            self.history
                .push(history::Pending::Traces(std::mem::take(queue)));
        }
    }

    fn invalidate(&mut self) {
        self.active = HashMap::new();
        self.completed = VecDeque::new();
        self.sql_completed = VecDeque::new();
        self.samples = VecDeque::new();
        self.current_resources.clear();
        self.pools.clear();
        self.dropped_active = 0;
        self.history.reset();
    }

    fn push_sample(&mut self, mut sample: ResourceSample) {
        // 资源所有者退出后显式给零；不能留下最后一次非零占用误导分析。
        for old in &self.current_resources {
            if !sample
                .resources
                .iter()
                .any(|point| point.resource == old.resource && point.domain == old.domain)
            {
                let mut retired = old.clone();
                retired.items = 0;
                retired.logical_bytes = 0;
                retired.observed_at_unix_ms = None;
                retired.observation_age_ms = None;
                sample.resources.push(retired);
            }
        }
        self.current_resources.clone_from(&sample.resources);
        self.samples.push_back(sample);
        if self.samples.len() == self.resource_capacity {
            self.history
                .push(history::Pending::Samples(std::mem::take(&mut self.samples)));
        }
    }
}

/// Perfetto/Chrome Trace JSON：已结束的是 X，仍活动的是 B；关闭 Debug 后本轮历史为空。
/// 每个 span 使用自己的合成 track，tid 不是 CPU 线程；异步并发不会破坏同一 track 的嵌套约束。
pub fn timeline_snapshot() -> Value {
    // barrier 在释放活动锁之后等待，业务线程不等待磁盘 IO。
    let (
        mut disk,
        live,
        retained,
        samples,
        capacities,
        buffered,
        dropped_active,
        epoch,
        debug_enabled,
    ) = loop {
        let (
            barrier,
            live,
            retained,
            samples,
            capacities,
            buffered,
            dropped_active,
            epoch,
            debug_enabled,
        ) = {
            let sources = sources().read();
            let state = TIMELINE.lock();
            (
                state.history.barrier(),
                state
                    .active
                    .values()
                    .map(|span| span.record(None))
                    .collect::<Vec<_>>(),
                state
                    .completed
                    .iter()
                    .chain(&state.sql_completed)
                    .cloned()
                    .collect::<Vec<_>>(),
                state.samples.iter().cloned().collect::<Vec<_>>(),
                [
                    state.active_capacity,
                    state.completed_capacity,
                    state.sql_capacity,
                    state.resource_capacity,
                ],
                [
                    state.completed.len(),
                    state.sql_completed.len(),
                    state.samples.len(),
                ],
                state.dropped_active,
                EPOCH.load(Ordering::Acquire),
                any_enabled(&sources),
            )
        };
        let disk = barrier.wait();
        if !disk.session_ended && epoch == EPOCH.load(Ordering::Acquire) {
            break (
                disk,
                live,
                retained,
                samples,
                capacities,
                buffered,
                dropped_active,
                epoch,
                debug_enabled,
            );
        }
    };
    let active_spans = live.len();
    let frozen_spans = 0;
    let frozen_total = 0_u64;
    let resource_sample_count = disk.samples + samples.len();
    let mut events = std::mem::take(&mut disk.events);
    events.extend(
        retained
            .into_iter()
            .map(|span| trace_event(span, "finished")),
    );
    events.extend(samples.into_iter().flat_map(sample_events));
    events.extend(live.into_iter().map(|span| trace_event(span, "active")));
    events.sort_by_key(|event| event["ts"].as_u64().unwrap_or_default());
    let retained_from = events.first().and_then(|event| event["ts"].as_u64());
    let present = events
        .iter()
        .filter_map(|event| event["args"]["span_id"].as_u64())
        .collect::<HashSet<_>>();
    // 只遍历原始事件，flow 直接追加到同一数组，保持原事件在前、flow 在后的顺序。
    let source_event_count = events.len();
    for index in 0..source_event_count {
        let event = &events[index];
        if let Some(parent) = event["args"]["parent_id"]
            .as_u64()
            .filter(|parent| present.contains(parent))
        {
            let id = &event["args"]["span_id"];
            let start = json!({"name":"parent","cat":"stravia.perf","ph":"s","ts":event["ts"],"pid":1,"tid":parent,"id":id});
            let finish = json!({"name":"parent","cat":"stravia.perf","ph":"f","ts":event["ts"],"pid":1,"tid":id,"id":id,"bp":"s"});
            events.push(start);
            events.push(finish);
        }
    }
    let exported_at = Instant::now();
    let mut snapshot = json!({
        "displayTimeUnit": "ms",
        "metadata": {
            "schema_version": 3,
            "application_version": env!("CARGO_PKG_VERSION"),
            "process_id":std::process::id(),
            "clock_origin_unix_us":CLOCK.1,
            "host": {"os":std::env::consts::OS,"arch":std::env::consts::ARCH,
                "logical_cpus":std::thread::available_parallelism().ok().map(|count|count.get())},
            "exported_at_unix_us":timestamp(exported_at),
            "retained_from_unix_us":retained_from,
            "debug_enabled":debug_enabled,
            "debug_epoch":epoch,
            "sample_interval_ms":SAMPLE_INTERVAL.as_millis(),
            "resource_sample_count":resource_sample_count,
            "capacity": { "active": capacities[0], "completed": capacities[1],
                "sql_completed": capacities[2], "resource_samples": capacities[3],
                "writer_queue": history::QUEUE_CAPACITY },
            "storage": "memory_and_temporary_file",
            "storage_status": disk.status,
            "storage_errors": disk.errors,
            "spilled_completed": disk.spilled[0],
            "spilled_sql_completed": disk.spilled[1],
            "spilled_resource_samples": disk.spilled[2],
            "dropped_active": dropped_active,
            "dropped_completed": disk.dropped[0],
            "dropped_sql_completed":disk.dropped[1],
            "dropped_resource_samples":disk.dropped[2],
            "incomplete": active_spans + frozen_spans,
            "incomplete_total": frozen_total + active_spans as u64,
            "active_spans": active_spans,
            "frozen_spans": frozen_spans,
            "frozen_total": frozen_total,
            "track": "synthetic span tracks; tid is not a CPU thread",
            "parent_relation":"async causal relationship, not timing containment or exclusive request cost",
            "resource_bytes": "logical managed-resource estimates, not allocator measurements",
            "process_scope":"host only; browser/WebView child processes and remote databases excluded",
            "cpu_percent_scale":"100 percent is one fully utilized logical CPU; may exceed 100",
            "sql_scope":"driver outcome; row-mapping errors and driver-internal control statements excluded",
            "sql_duration_scope":"client future/stream lifetime; streaming includes consumption and backpressure, not server time",
            "sql_query_id_scope":"SHA-256 of unnormalized SQL template; no SQL text or bind values",
            "sql_source_scope":"unavailable; no automatic mapping from query fingerprint to application source",
            "pool_waiting_scope":"instrumented acquisitions; driver-internal waits excluded",
            "remote_cache_scope":"last received successful command snapshot; response order and remote TTL are not live gauges",
        }
    });
    // json! 的泛型表达式会通过 to_value(&events) 重建 DOM；这里直接转移所有权。
    snapshot["traceEvents"] = Value::Array(events);
    snapshot["metadata"]["buffered"] = json!({
        "completed": buffered[0],
        "sql_completed": buffered[1],
        "resource_samples": buffered[2],
    });
    snapshot["metadata"]["writer_queue_unit"] = json!("batches");
    snapshot
}

fn trace_event(span: TraceRecord, capture_state: &str) -> Value {
    let mut args = json!({ "span_id": span.id, "capture_state": capture_state });
    if let Some(request_id) = span.request_id {
        args["request_id"] = Value::String(request_id);
    }
    if let Some(parent_id) = span.parent_id {
        args["parent_id"] = json!(parent_id);
    }
    if let Some(command_kind) = span.command_kind {
        args["command_kind"] = json!(command_kind);
    }
    if let Some(status) = span.status {
        args["status"] = json!(status);
    }
    if let Some(active_us) = span.active_us {
        args["active_us"] = json!(active_us);
    }
    if span.kind != SpanKind::Operation
        && let Value::Object(details) = span.detail.export()
    {
        args.as_object_mut()
            .expect("span args object")
            .extend(details);
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
        "cat": if span.kind == SpanKind::Operation { "stravia.perf" } else { "stravia.sql" },
        "ph": if span.finished.is_some() { "X" } else { "B" },
        "ts": timestamp(span.started),
        "pid": 1,
        "tid": span.id,
    });
    event["args"] = args;
    if let Some(finished) = span.finished {
        event["dur"] = json!(micros(finished.saturating_duration_since(span.started)));
    }
    event
}

fn sample_events(sample: ResourceSample) -> Vec<Value> {
    let mut events = Vec::new();
    let ts = timestamp(sample.at);
    let mut process = serde_json::Map::new();
    if let Some(rss) = sample.rss_bytes {
        process.insert("rss_bytes".into(), json!(rss));
    }
    if let Some(cpu) = sample.cpu_percent {
        process.insert("cpu_percent".into(), json!(cpu));
    }
    if !process.is_empty() {
        let mut event = json!({"name":"process","cat":"stravia.resources","ph":"C",
                "pid":1,"tid":0,"ts":ts});
        event["args"] = Value::Object(process);
        events.push(event);
    }
    for point in sample.resources {
        let mut args = json!({"items":point.items,"logical_bytes":point.logical_bytes});
        if let Some(at) = point.observed_at_unix_ms {
            args["observed_at_unix_ms"] = json!(at);
        }
        if let Some(age) = point.observation_age_ms {
            args["observation_age_ms"] = json!(age);
        }
        let mut event = json!({"name":format!("{}.{}",point.resource,point.domain),
                "cat":"stravia.resources","ph":"C","pid":1,"tid":0,"ts":ts});
        event["args"] = args;
        events.push(event);
    }
    for pool in sample.pools {
        let mut args = json!({"observation_age_us":ts.saturating_sub(pool.observed_at_us)});
        if let Some(value) = pool.size {
            args["size"] = json!(value);
        }
        if let Some(value) = pool.idle {
            args["idle"] = json!(value);
        }
        if let Some(value) = pool.waiting {
            args["instrumented_waiting"] = json!(value);
        }
        if let Some(value) = pool.failed {
            args["instrumented_failed"] = json!(value);
        }
        if let Some(value) = pool.cancelled {
            args["instrumented_cancelled"] = json!(value);
        }
        let mut event = json!({"name":format!("sql_pool.{}.{}",pool.backend,pool.pool),
                "cat":"stravia.resources","ph":"C","pid":1,"tid":0,"ts":ts});
        event["args"] = args;
        events.push(event);
    }
    events.push(
        json!({"name":"debug.epoch","cat":"stravia.resources","ph":"C",
            "pid":1,"tid":0,"ts":ts,"args":{"epoch":sample.epoch}}),
    );

    events
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
    // 切换与 span 创建/结束及指标写入串行；旧会话历史立即撤销，
    // 随后重开也绝不把旧 span 算入新会话的时间线或指标。
    let _sources = sources().write();
    if debug.swap(enabled, Ordering::AcqRel) != enabled {
        let mut timeline = TIMELINE.lock();
        timeline.invalidate();
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

pub(crate) fn is_performance_target(target: &str) -> bool {
    matches!(target, "stravia::perf" | "stravia::sql")
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
    // Only enabled for inference root creation while Debug capture is active.
    capture_request_id: bool,
    request_id: Option<String>,
    capture_command_kind: bool,
    command_kind: Option<&'static str>,
    status: Option<&'static str>,
    work: [Option<u64>; 4],
    detail: SqlDetails,
    causal_parent_id: u64,
    causal_epoch: Option<u64>,
    causal_registry_parent_id: u64,
}

impl Visit for StatusVisitor {
    fn record_u64(&mut self, field: &Field, value: u64) {
        let index = match field.name() {
            "node_count" => 0,
            "reference_count" => 1,
            "candidate_count" => 2,
            "event_count" => 3,
            "causal_parent_id" => {
                self.causal_parent_id = value;
                return;
            }
            "causal_registry_parent_id" => {
                self.causal_registry_parent_id = value;
                return;
            }
            "causal_epoch" => {
                self.causal_epoch = Some(value);
                return;
            }
            _ => {
                self.detail.record_u64(field, value);
                return;
            }
        };
        self.work[index] = Some(value);
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "request_id" {
            if self.capture_request_id && stravia_runtime_contract::identifier::valid_id(value) {
                self.request_id = Some(value.to_owned());
            }
        } else if field.name() == "command_kind" {
            if self.capture_command_kind {
                self.command_kind = match value {
                    "clear_tail" => Some("clear_tail"),
                    "clear_debug" => Some("clear_debug"),
                    "client_disconnected" => Some("client_disconnected"),
                    "client_tool_results" => Some("client_tool_results"),
                    "input_preview" => Some("input_preview"),
                    "purge" => Some("purge"),
                    "client_completion" => Some("client_completion"),
                    "admit" => Some("admit"),
                    "event" => Some("event"),
                    "text" => Some("text"),
                    "finish" => Some("finish"),
                    "reject" => Some("reject"),
                    "finalize" => Some("finalize"),
                    "flush_interaction" => Some("flush_interaction"),
                    "barrier" => Some("barrier"),
                    "shutdown" => Some("shutdown"),
                    "closed" => Some("closed"),
                    _ => None,
                };
            }
        } else if field.name() == "status" {
            self.status = match value {
                "completed" => Some("completed"),
                "error" => Some("error"),
                "cancelled" => Some("cancelled"),
                "abandoned" => Some("abandoned"),
                _ => None,
            };
        } else {
            self.detail.record_str(field, value);
        }
    }

    fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
}

/// 静态 span 名称是唯一 operation 标签；根请求的内部 ID 只进入 Debug 时间线，
/// 不进入指标标签；其余字段仅接受状态、静态观测命令类型、工作量数值和 SQL 白名单。
/// 活动 span、有界历史缓冲和短写队列驻留内存；溢出历史写入本轮匿名临时文件。
/// STRAVIA_PERF_TRACE_ACTIVE_CAPACITY 保护活动并发；满额的活动 span 不导出也不记指标。
pub struct PerformanceLayer;

impl<S> Layer<S> for PerformanceLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(kind) = SpanKind::from_target(attrs.metadata().target(), attrs.metadata().name())
        else {
            return;
        };
        let sources = sources().read();
        if !any_enabled(&sources) {
            return;
        }
        let epoch = EPOCH.load(Ordering::Acquire);
        let mut status = StatusVisitor {
            capture_request_id: attrs.metadata().name() == "proxy.inference_run"
                && kind == SpanKind::Operation,
            capture_command_kind: attrs.metadata().name() == "observation.command.process"
                && kind == SpanKind::Operation,
            ..StatusVisitor::default()
        };
        attrs.record(&mut status);
        // SQL 边界不依赖 Core：只在创建租约时借用当前 registry ID 解析因果身份，
        // 不让跨取消清理的 hold span 持有请求父 span。后续生命周期仍使用 epoch。
        let registry_parent = (attrs.metadata().name() == "sqlite.write_gate.hold"
            && status.causal_registry_parent_id != 0)
            .then(|| Id::from_u64(status.causal_registry_parent_id));
        let current = ctx.current_span();
        let parent = attrs
            .parent()
            .or_else(|| attrs.is_contextual().then(|| current.id()).flatten())
            .or(registry_parent.as_ref());
        let parent_context = parent.and_then(|parent| ctx.span(parent)).and_then(|span| {
            // 业务日志 span 可能夹在两级 perf span 之间，沿真实 tracing 父链
            // 找最近的 perf 祖先，而不是把非 perf span 误当成根。
            span.scope().find_map(|ancestor| {
                ancestor
                    .extensions()
                    .get::<PerfSpan>()
                    .map(|perf| (perf.id, perf.operation, perf.owner_id, perf.epoch))
            })
        });
        let Some(span) = ctx.span(id) else { return };
        let rejected_epoch = parent_context
            .filter(|parent| parent.3 != epoch)
            .map(|parent| parent.3)
            .or_else(|| status.causal_epoch.filter(|causal| *causal != epoch));
        if let Some(epoch) = rejected_epoch {
            // 拒收的命令仍会执行业务；保留旧周期标记，防止其后代被误收成新周期根。
            span.extensions_mut().insert(PerfSpan {
                id: 0,
                epoch,
                operation: attrs.metadata().name(),
                owner_id: 0,
            });
            return;
        }
        let parent_id = parent_context.map(|parent| parent.0).or_else(|| {
            (status.causal_parent_id != 0 && status.causal_epoch == Some(epoch))
                .then_some(status.causal_parent_id)
        });
        let mut state = TIMELINE.lock();
        if state.active.len() == state.active_capacity {
            state.dropped_active += 1;
            return;
        }
        let _ = &*CLOCK;
        let span_id = NEXT_SPAN_ID.fetch_add(1, Ordering::Relaxed);
        let (operation, owner_id) = if kind == SpanKind::Operation {
            (attrs.metadata().name(), span_id)
        } else {
            parent_context.map_or(("unattributed", 0), |parent| (parent.1, parent.2))
        };
        let started = Instant::now();
        state.active.insert(
            span_id,
            ActiveSpan {
                name: attrs.metadata().name(),
                request_id: status.request_id,
                command_kind: status.command_kind,
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
                detail: status.detail,
                kind,
                operation,
                owner_id,
            },
        );
        span.extensions_mut().insert(PerfSpan {
            id: span_id,
            epoch,
            operation,
            owner_id,
        });
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
        let mut state = TIMELINE.lock();
        let mut finished = false;
        if let Some(active) = state.active.get_mut(&perf.id) {
            if let Some(status) = status.status {
                active.status = Some(status);
                finished = active.kind != SpanKind::Operation;
            }
            for (current, value) in active.work.iter_mut().zip(status.work) {
                if value.is_some() {
                    *current = value;
                }
            }
            active.detail.merge(status.detail);
        }
        drop(state);
        if finished {
            finish_span(perf.id, Instant::now());
        }
    }

    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        if event.metadata().target() != "stravia::sql" {
            return;
        }
        let mut fields = StatusVisitor::default();
        event.record(&mut fields);
        let guard = sources().read();
        if !any_enabled(&guard) {
            return;
        }
        if let Some(point) = fields.detail.pool_point(timestamp(Instant::now())) {
            record_pool_metrics(&point);
            TIMELINE
                .lock()
                .pools
                .insert((point.backend, point.pool), point);
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
        finish_span(perf.id, Instant::now());
    }
}

fn finish_span(id: u64, finished: Instant) {
    let mut state = TIMELINE.lock();
    let Some(active) = state.active.remove(&id) else {
        return;
    };
    let status = active.status.unwrap_or("closed");
    let duration = finished.saturating_duration_since(active.started);
    if active.kind != SpanKind::Operation {
        if let Some(owner) = state.active.get_mut(&active.owner_id) {
            if active.kind == SpanKind::Query {
                owner.sql.query_count += 1;
                owner.sql.query_duration_us =
                    owner.sql.query_duration_us.saturating_add(micros(duration));
            } else {
                owner.sql.pool_acquire_count += 1;
                owner.sql.pool_acquire_duration_us = owner
                    .sql
                    .pool_acquire_duration_us
                    .saturating_add(micros(duration));
            }
        }
        if let Some(point) = active.detail.pool_point(timestamp(finished)) {
            record_pool_metrics(&point);
            state.pools.insert((point.backend, point.pool), point);
        }
    }
    let kind = active.kind;
    let name = active.name;
    let operation = active.operation;
    let backend = active.detail.backend.unwrap_or("unknown");
    let pool = active.detail.pool.unwrap_or("unregistered");
    state.push(active.into_record(Some(finished)));
    drop(state);
    match kind {
        SpanKind::Operation => histogram!("stravia_operation_duration_seconds", "operation" => name, "status" => status)
            .record(duration.as_secs_f64()),
        SpanKind::Query => histogram!("stravia_sql_query_duration_seconds", "operation" => operation,
            "backend" => backend, "pool" => pool,
            "status" => status).record(duration.as_secs_f64()),
        SpanKind::Acquire => histogram!("stravia_sql_pool_acquire_duration_seconds", "operation" => operation,
            "backend" => backend, "pool" => pool,
            "status" => status).record(duration.as_secs_f64()),
    }
}

fn record_pool_metrics(point: &PoolPoint) {
    if let Some(value) = point.size {
        gauge!("stravia_sql_pool_size", "backend"=>point.backend, "pool"=>point.pool)
            .set(value as f64);
    }
    if let Some(value) = point.idle {
        gauge!("stravia_sql_pool_idle", "backend"=>point.backend, "pool"=>point.pool)
            .set(value as f64);
    }
    if let Some(value) = point.waiting {
        gauge!("stravia_sql_pool_instrumented_waiting", "backend"=>point.backend, "pool"=>point.pool).set(value as f64);
    }
    if let Some(value) = point.failed {
        counter!("stravia_sql_pool_acquire_failed_total", "backend"=>point.backend, "pool"=>point.pool).absolute(value);
    }
    if let Some(value) = point.cancelled {
        counter!("stravia_sql_pool_acquire_cancelled_total", "backend"=>point.backend, "pool"=>point.pool).absolute(value);
    }
}

pub(crate) fn record_wait(
    operation: &'static str,
    context: PerfContext,
    started: Instant,
    status: &'static str,
) {
    let guard = sources().read();
    if !any_enabled(&guard) || context.epoch != EPOCH.load(Ordering::Acquire) {
        return;
    }
    let finished = Instant::now();
    TIMELINE.lock().push(TraceRecord {
        name: operation,
        request_id: None,
        command_kind: None,
        id: NEXT_SPAN_ID.fetch_add(1, Ordering::Relaxed),
        parent_id: (context.span_id != 0).then_some(context.span_id),
        started,
        finished: Some(finished),
        active_us: None,
        status: Some(status),
        work: [None; 4],
        sql: SqlActivity::default(),
        detail: SqlDetails::default(),
        kind: SpanKind::Operation,
    });
    histogram!("stravia_operation_duration_seconds", "operation" => operation, "status" => status)
        .record(finished.saturating_duration_since(started).as_secs_f64());
}

pub(crate) fn record_observation_queue_depth(depth: usize) {
    let guard = sources().read();
    if any_enabled(&guard) {
        gauge!("stravia_observation_writer_queue_depth").set(depth as f64);
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
pub(crate) fn spawn_upkeep(
    sqlite: Option<sqlx::SqlitePool>,
    postgres: Option<sqlx::PgPool>,
) -> Option<tokio::task::JoinHandle<()>> {
    let handle = RECORDER.get()?.as_ref()?.clone();
    Some(tokio::spawn(async move {
        let mut interval = tokio::time::interval(SAMPLE_INTERVAL);
        loop {
            interval.tick().await;
            if enabled() {
                if let Some(pool) = &sqlite {
                    sqlx::sample_pool(pool, "main");
                }
                if let Some(pool) = &postgres {
                    sqlx::sample_pool(pool, "main");
                }
            }
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
        let mut rss_bytes = None;
        let mut cpu_percent = None;
        if let Some(process) = sampler.system.process(pid) {
            rss_bytes = Some(process.memory());
            gauge!("stravia_process_rss_bytes").set(process.memory() as f64);
            // 首个样本没有可比较的上次 CPU 读数，不能输出伪造的 0。
            if sampler.warmed_epoch == Some(epoch) {
                cpu_percent = Some(f64::from(process.cpu_usage()));
                gauge!("stravia_process_cpu_percent").set(f64::from(process.cpu_usage()));
            }
        }
        sampler.warmed_epoch = Some(epoch);
        let resources = resources::snapshot();
        let mut state = TIMELINE.lock();
        let pools = state.pools.values().cloned().collect();
        state.push_sample(ResourceSample {
            at: Instant::now(),
            epoch,
            rss_bytes,
            cpu_percent,
            resources,
            pools,
        });
        for point in &state.current_resources {
            gauge!("stravia_managed_resource_items", "resource" => point.resource, "domain" => point.domain).set(point.items as f64);
            gauge!("stravia_managed_resource_logical_bytes", "resource" => point.resource, "domain" => point.domain).set(point.logical_bytes as f64);
        }
    }
}

/// Prometheus 0.0.4 文本；关闭 Debug 后旧数据仍可下载。
/// 进程采样仅由定时任务驱动，不因导出请求产生新样本。
pub fn metrics_snapshot() -> Option<String> {
    let handle = RECORDER.get()?.as_ref()?;
    handle.run_upkeep();
    Some(handle.render())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::sync::Barrier;
    use tracing::{Instrument, subscriber::Interest};
    use tracing_subscriber::{filter::dynamic_filter_fn, layer::SubscriberExt};

    struct OtherSpans;
    impl<S: Subscriber> Layer<S> for OtherSpans {}

    fn isolated(name: &str) -> bool {
        isolated_with_env(name, &[])
    }

    fn isolated_with_env(name: &str, env: &[(&str, &str)]) -> bool {
        if std::env::var("STRAVIA_PERFORMANCE_TEST_CHILD")
            .ok()
            .as_deref()
            != Some(name)
        {
            let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
                .arg("--exact")
                .arg(format!("performance::tests::{name}"))
                .env("STRAVIA_PERFORMANCE_TEST_CHILD", name)
                .envs(env.iter().copied())
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
        tracing_subscriber::registry().with(
            PerformanceLayer.with_filter(
                dynamic_filter_fn(|meta, _ctx| is_performance_target(meta.target()) && enabled())
                    .with_callsite_filter(|meta| {
                        if is_performance_target(meta.target()) {
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

    fn sample_value(output: &str, metric: &str, labels: &[(&str, &str)]) -> f64 {
        output
            .lines()
            .find_map(|line| {
                let (key, value) = line.rsplit_once(' ')?;
                let (name, actual) = key.split_once('{')?;
                let actual = actual.strip_suffix('}')?;
                if name != metric || actual.split(',').count() != labels.len() {
                    return None;
                }
                let matches = labels.iter().all(|(expected_key, expected_value)| {
                    actual.split(',').any(|label| {
                        let Some((key, value)) = label.split_once('=') else {
                            return false;
                        };
                        key == *expected_key
                            && value
                                .strip_prefix('"')
                                .and_then(|value| value.strip_suffix('"'))
                                == Some(*expected_value)
                    })
                });
                matches.then(|| value.parse().expect("numeric metric sample"))
            })
            .expect("metric and complete static label set")
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
                emit_sql_span();
                set_debug(&debug, true);
                let stale = tracing::info_span!(target: "stravia::perf", "test.old_session");
                emit_sql_span();
                set_debug(&debug, false);
                emit_sql_span();
                drop(stale);
                set_debug(&debug, true);
                emit_reused_callsite();
                for command_kind in ["admit", "private-command-kind"] {
                    let command = tracing::info_span!(target: "stravia::perf",
                        "observation.command.process", command_kind);
                    // 类型在创建时固定，后续记录不能把正文写进诊断字段。
                    command.record("command_kind", "private-command-revision");
                    drop(command);
                }
                let first_request_id = stravia_runtime_contract::identifier::new_id();
                let second_request_id = stravia_runtime_contract::identifier::new_id();
                let roots: Vec<_> = [
                    first_request_id.as_str(),
                    second_request_id.as_str(),
                    "secret-request-id",
                ]
                .into_iter()
                .map(|request_id| {
                    tracing::info_span!(target: "stravia::perf", "proxy.inference_run",
                        request_id = request_id, password = "secret-password")
                })
                .collect();
                roots[0].record("request_id", second_request_id.as_str());
                roots[0].in_scope(|| {
                    let child = tracing::info_span!(target: "stravia::perf",
                        "test.identity_child", request_id = second_request_id.as_str(),
                        command_kind = "admit");
                    drop(child);
                });
                drop(roots);
                let identity_snapshot = timeline_snapshot();
                let identity_events = identity_snapshot["traceEvents"].as_array().unwrap();
                let roots: Vec<_> = identity_events
                    .iter()
                    .filter(|event| event["name"] == "proxy.inference_run" && event["ph"] == "X")
                    .collect();
                assert_eq!(roots.len(), 3);
                for request_id in [&first_request_id, &second_request_id] {
                    assert_eq!(
                        roots
                            .iter()
                            .filter(|root| root["args"]["request_id"] == request_id.as_str())
                            .count(),
                        1
                    );
                    assert!(!handle.render().contains(request_id));
                }
                assert_eq!(
                    roots
                        .iter()
                        .filter(|root| root["args"].get("request_id").is_none())
                        .count(),
                    1
                );
                let child = identity_events
                    .iter()
                    .find(|event| event["name"] == "test.identity_child")
                    .unwrap();
                assert!(child["args"].get("request_id").is_none());
                assert!(child["args"].get("command_kind").is_none());
                let first_root = roots
                    .iter()
                    .find(|root| root["args"]["request_id"] == first_request_id.as_str())
                    .unwrap();
                assert_eq!(child["args"]["parent_id"], first_root["args"]["span_id"]);
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
            });
        });
        handle.run_upkeep();
        let output = handle.render();
        assert!(output.contains("stravia_sql_query_duration_seconds_bucket"));
        assert_eq!(
            sample_value(
                &output,
                "stravia_sql_query_duration_seconds_count",
                &[
                    ("backend", "sqlite"),
                    ("operation", "unattributed"),
                    ("pool", "main"),
                    ("status", "completed")
                ]
            ),
            1.0
        );
        for (operation, status) in [
            ("test.reused", "completed"),
            ("test.child", "error"),
            ("test.explicit_child", "closed"),
        ] {
            assert_eq!(
                sample_value(
                    &output,
                    "stravia_operation_duration_seconds_count",
                    &[("operation", operation), ("status", status)]
                ),
                1.0
            );
        }
        assert!(!output.contains("test.old_session"));
        assert!(!output.contains("command_kind"));
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
        let commands: Vec<_> = events
            .iter()
            .filter(|event| event["name"] == "observation.command.process" && event["ph"] == "X")
            .collect();
        assert_eq!(commands.len(), 2);
        assert_eq!(
            commands
                .iter()
                .filter(|event| event["args"]["command_kind"] == "admit")
                .count(),
            1
        );
        assert_eq!(
            commands
                .iter()
                .filter(|event| event["args"].get("command_kind").is_none())
                .count(),
            1
        );
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
            "private-command-kind",
            "private-command-revision",
            "secret-request-id",
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
    fn timeline_capture_state_tracks_lifecycle_independently_of_business_status() {
        if !isolated("timeline_capture_state_tracks_lifecycle_independently_of_business_status") {
            return;
        }
        let debug = Arc::new(AtomicBool::new(true));
        bind_debug(&debug);
        let event = |snapshot: &Value, name: &str| {
            snapshot["traceEvents"]
                .as_array()
                .unwrap()
                .iter()
                .find(|event| event["name"] == name && event["args"]["span_id"].is_u64())
                .unwrap()
                .clone()
        };
        let assert_counts = |snapshot: &Value, active: u64, frozen: u64, total: u64| {
            let metadata = &snapshot["metadata"];
            assert_eq!(metadata["active_spans"], active);
            assert_eq!(metadata["frozen_spans"], frozen);
            assert_eq!(metadata["frozen_total"], total);
            assert_eq!(metadata["incomplete"], active + frozen);
            assert_eq!(metadata["incomplete_total"], total + active);
        };
        tracing::subscriber::with_default(subscriber(), || {
            let old = tracing::info_span!(
                target: "stravia::perf", "test.capture.old", status = tracing::field::Empty
            );
            let old_sql = tracing::info_span!(
                target: "stravia::sql", "sql.query", status = tracing::field::Empty,
                backend = "sqlite", pool = "main",
                db.statement = "SELECT private_capture_input", error = "private_capture_error"
            );
            let active = timeline_snapshot();
            let epoch = active["metadata"]["debug_epoch"].clone();
            assert_counts(&active, 2, 0, 0);
            for name in ["test.capture.old", "sql.query"] {
                let span = event(&active, name);
                assert_eq!(span["args"]["capture_state"], "active");
                assert_eq!(span["ph"], "B");
                assert!(span.get("dur").is_none());
            }

            set_debug(&debug, false);
            let closed = timeline_snapshot();
            assert_counts(&closed, 0, 0, 0);
            assert!(closed["traceEvents"].as_array().unwrap().is_empty());
            set_debug(&debug, true);
            old.record("status", "completed");
            old_sql.record("status", "error");
            drop(old_sql);
            drop(old);
            let current = tracing::info_span!(
                target: "stravia::perf", "test.capture.current", status = "completed"
            );
            let reopened = timeline_snapshot();
            assert_ne!(reopened["metadata"]["debug_epoch"], epoch);
            assert_counts(&reopened, 1, 0, 0);
            assert_eq!(
                event(&reopened, "test.capture.current")["args"]["capture_state"],
                "active"
            );
            drop(current);
            drop(tracing::info_span!(
                target: "stravia::perf", "test.capture.error", status = "error"
            ));
            drop(tracing::info_span!(
                target: "stravia::perf", "test.capture.cancelled", status = "cancelled"
            ));
            let finished = timeline_snapshot();
            assert_counts(&finished, 0, 0, 0);
            for name in ["test.capture.old", "sql.query"] {
                assert!(
                    !finished["traceEvents"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|event| event["name"] == name)
                );
            }
            for (name, status) in [
                ("test.capture.current", "completed"),
                ("test.capture.error", "error"),
                ("test.capture.cancelled", "cancelled"),
            ] {
                let span = event(&finished, name);
                assert_eq!(span["args"]["capture_state"], "finished");
                assert_eq!(span["args"]["status"], status);
                assert_eq!(span["ph"], "X");
                assert!(span["dur"].is_u64());
            }
            let output = finished.to_string();
            assert!(!output.contains("private_capture_input"));
            assert!(!output.contains("private_capture_error"));
        });
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
            drop(tracing::info_span!(target: "stravia::perf", "test.overflow_active"));
            let holding = timeline_snapshot();
            assert_eq!(holding["metadata"]["active_spans"], ACTIVE_CAPACITY);
            assert_eq!(holding["metadata"]["dropped_active"], 1);
            set_debug(&debug, false);
            assert!(
                timeline_snapshot()["traceEvents"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            set_debug(&debug, true);
            drop(active);
            for _ in 0..COMPLETED_CAPACITY + 20 {
                drop(
                    tracing::info_span!(target: "stravia::perf", "test.completed_buffer", status = "completed"),
                );
            }
            let snapshot = timeline_snapshot();
            assert_eq!(
                snapshot["metadata"]["capacity"]["completed"],
                COMPLETED_CAPACITY
            );
            assert_eq!(
                snapshot["metadata"]["spilled_completed"],
                COMPLETED_CAPACITY
            );
            assert_eq!(snapshot["metadata"]["dropped_completed"], 0);
            assert_eq!(snapshot["metadata"]["dropped_active"], 0);
            assert_eq!(snapshot["metadata"]["storage_status"], "ready");
            assert_eq!(snapshot["metadata"]["incomplete"], 0);
            assert_eq!(snapshot["metadata"]["frozen_total"], 0);
            assert_eq!(
                snapshot["traceEvents"].as_array().unwrap().len(),
                COMPLETED_CAPACITY + 20
            );
            assert_eq!(TIMELINE.lock().completed.len(), 20);
            set_debug(&debug, false);
            let closed = timeline_snapshot();
            assert!(closed["traceEvents"].as_array().unwrap().is_empty());
            assert_eq!(closed["metadata"]["spilled_completed"], 0);
            assert_eq!(closed["metadata"]["storage_errors"], 0);
            set_debug(&debug, true);
            assert!(
                timeline_snapshot()["traceEvents"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
        });
    }

    fn emit_sql_span() {
        let _span = tracing::info_span!(
            target: "stravia::sql", "sql.query", status = "completed",
            backend = "SQLite", pool = "main", execution_scope = "driver",
            summary = "SELECT password", db.statement = "SELECT password FROM vault",
        );
    }

    #[test]
    fn sql_history_exceeds_memory_capacity_and_keeps_ended_parent_flows() {
        if !isolated_with_env(
            "sql_history_exceeds_memory_capacity_and_keeps_ended_parent_flows",
            &[(COMPLETED_CAPACITY_ENV, "1")],
        ) {
            return;
        }
        let debug = Arc::new(AtomicBool::new(true));
        bind_debug(&debug);
        tracing::subscriber::with_default(subscriber(), || {
            let parent = tracing::info_span!(target: "stravia::perf", "test.disk.parent");
            let context = parent.in_scope(current_context);
            drop(parent);
            drop(tracing::info_span!(target: "stravia::perf", "test.disk.displace"));
            for index in 0..2 * SQL_COMPLETED_CAPACITY + 40 {
                let child = tracing::info_span!(target: "stravia::sql", parent: None, "sql.query",
                    causal_parent_id=context.span_id(), causal_epoch=context.epoch(),
                    backend="SQLite", pool="main", status="completed",
                    db.statement="SELECT private_disk_value");
                drop(child);
                if index + 1 == SQL_COMPLETED_CAPACITY {
                    assert!(TIMELINE.lock().sql_completed.is_empty());
                    let first_batch = timeline_snapshot();
                    assert_eq!(
                        first_batch["metadata"]["spilled_sql_completed"],
                        SQL_COMPLETED_CAPACITY
                    );
                    assert_eq!(
                        first_batch["traceEvents"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .filter(|event| event["name"] == "sql.query" && event["ph"] == "X")
                            .count(),
                        SQL_COMPLETED_CAPACITY
                    );
                }
            }
            let snapshot = timeline_snapshot();
            let events = snapshot["traceEvents"].as_array().unwrap();
            let queries = events
                .iter()
                .filter(|event| event["name"] == "sql.query" && event["ph"] == "X")
                .collect::<Vec<_>>();
            assert_eq!(queries.len(), 2 * SQL_COMPLETED_CAPACITY + 40);
            assert!(
                queries
                    .iter()
                    .all(|event| event["args"]["parent_id"] == context.span_id())
            );
            assert!(
                events
                    .iter()
                    .any(|event| event["ph"] == "s" && event["tid"] == context.span_id())
            );
            assert!(
                events
                    .iter()
                    .any(|event| event["name"] == "test.disk.parent" && event["ph"] == "X")
            );
            assert_eq!(snapshot["metadata"]["spilled_completed"], 2);
            assert_eq!(
                snapshot["metadata"]["spilled_sql_completed"],
                2 * SQL_COMPLETED_CAPACITY
            );
            assert_eq!(TIMELINE.lock().sql_completed.len(), 40);
            assert_eq!(snapshot["metadata"]["dropped_sql_completed"], 0);
            assert!(!snapshot.to_string().contains("private_disk_value"));
            set_debug(&debug, false);
            assert!(
                timeline_snapshot()["traceEvents"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
        });
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
            let mut connection = async {
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
                }
                .instrument(tracing::info_span!(
                    target: "stravia::perf", "test.sqlite.child", node_count = 2_u64
                ))
                .await;
                connection
            }
            .instrument(tracing::info_span!(target: "stravia::perf", "test.sqlite.parent"))
            .await;
            // 同一连接的 FIFO ping 排空先前 Execute 持有的 span；必须在被测
            // span 外提交，因为 worker 会先回复 ping，再释放 ping 自己捕获的 span。
            connection.ping().await.unwrap();
            drop(connection);
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
        let queries = events
            .iter()
            .filter(|event| event["name"] == "sql.query" && event["ph"] == "X")
            .collect::<Vec<_>>();
        assert_eq!(
            queries
                .iter()
                .filter(|event| event["args"]["query_id"]
                    == "66cbb3a40d4bbd150b75825ad291a6545399f3098fc1079e4d8b5bb061a6a481")
                .count(),
            2
        );
        assert!(queries.iter().any(|event| event["args"]["query_id"]
            == "9fb5dfe333b3d38fe20dcecafcfaa65044e2cef57463deb2501603376c71252a"));
        let metrics = metrics_snapshot().unwrap();
        for (operation, count) in [("test.sqlite.parent", 1), ("test.sqlite.child", 2)] {
            assert_eq!(
                sample_value(
                    &metrics,
                    "stravia_sql_query_duration_seconds_count",
                    &[
                        ("backend", "sqlite"),
                        ("operation", operation),
                        ("pool", "main"),
                        ("status", "completed")
                    ]
                ),
                f64::from(count)
            );
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
    fn sqlite_cleanup_keeps_hold_without_extending_cancelled_request() {
        if !isolated("sqlite_cleanup_keeps_hold_without_extending_cancelled_request") {
            return;
        }
        let debug = Arc::new(AtomicBool::new(false));
        bind_debug(&debug);
        tracing::subscriber::set_global_default(subscriber()).expect("isolated subscriber");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .test_before_acquire(false)
                .connect("sqlite::memory:")
                .await
                .unwrap();
            set_debug(&debug, true);
            let request = tracing::info_span!(target: "stravia::perf", "proxy.inference_run",
                status = tracing::field::Empty);
            let connection = pool
                .acquire_write()
                .instrument(request.clone())
                .await
                .unwrap();
            request.record("status", "cancelled");
            drop(request);
            drop(connection);

            // 当前线程尚未让出执行权：清理任务仍未 poll，租约必须继续活动，
            // 但它的因果父边不能把已经取消的请求延长到后台清理完成。
            let snapshot = timeline_snapshot();
            let events = snapshot["traceEvents"].as_array().unwrap();
            let root = events
                .iter()
                .find(|event| event["name"] == "proxy.inference_run")
                .unwrap();
            assert_eq!(root["ph"], "X", "cleanup must not retain the request span");
            assert_eq!(root["args"]["status"], "cancelled");
            let hold = events
                .iter()
                .find(|event| event["name"] == "sqlite.write_gate.hold")
                .unwrap();
            assert_eq!(hold["ph"], "B");
            assert_eq!(hold["args"]["parent_id"], root["args"]["span_id"]);
            let hold_id = hold["args"]["span_id"].clone();
            pool.acquire_write().await.unwrap().finish().await.unwrap();
            pool.close().await;

            let completed = timeline_snapshot();
            let hold = completed["traceEvents"]
                .as_array()
                .unwrap()
                .iter()
                .find(|event| event["args"]["span_id"] == hold_id)
                .unwrap();
            assert_eq!(hold["ph"], "X");
            assert_eq!(hold["args"]["status"], "cancelled");
        });
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
                stale.in_scope(emit_sql_span);
                drop(stale);
                let current = tracing::info_span!(
                    target: "stravia::perf", "test.current_sql",
                    node_count = 3_u64, reference_count = tracing::field::Empty,
                    secret_number = 919191_u64, candidate_count = "private-count"
                );
                current.in_scope(|| {
                    emit_sql_span();
                    let acquire = tracing::info_span!(target: "stravia::sql", "sql.pool.acquire",
                        backend = "SQLite", pool = "main", status = "completed");
                    drop(acquire);
                    current.record("reference_count", 7_u64);
                    current.record("node_count", -1_i64);
                    record_generation_cache_access(true);
                    record_generation_cache_access(false);
                });
                drop(current);
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
        assert_eq!(current["args"]["sql_pool_acquire_count"], 1);
        assert_eq!(current["args"]["node_count"], 3);
        assert_eq!(current["args"]["reference_count"], 7);
        assert!(current["args"].get("candidate_count").is_none());
        assert!(!events.iter().any(|event| event["name"] == "test.stale_sql"));
        let serialized = snapshot.to_string();
        assert!(!serialized.contains("919191"));
        assert!(!serialized.contains("private-count"));
    }

    #[test]
    fn resource_history_spills_transitions_and_clears_with_debug() {
        if !isolated_with_env(
            "resource_history_spills_transitions_and_clears_with_debug",
            &[(RESOURCE_CAPACITY_ENV, "2")],
        ) {
            return;
        }
        let debug = Arc::new(AtomicBool::new(true));
        bind_debug(&debug);
        let usage = ResourceUsage::new("test_resource", "retained");
        let mut lease = usage.lease(2, 4096);
        sample_process();
        lease.resize(1, 1024);
        sample_process();
        drop(lease);
        sample_process();
        let snapshot = timeline_snapshot();
        let samples = snapshot["traceEvents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["name"] == "test_resource.retained")
            .collect::<Vec<_>>();
        assert_eq!(samples.len(), 3);
        assert_eq!(samples[0]["args"]["items"], 2);
        assert_eq!(samples[0]["args"]["logical_bytes"], 4096);
        assert_eq!(samples[1]["args"]["items"], 1);
        assert_eq!(samples[1]["args"]["logical_bytes"], 1024);
        assert_eq!(samples[2]["args"]["items"], 0);
        assert_eq!(samples[2]["args"]["logical_bytes"], 0);
        assert!(samples[0]["ts"].as_u64().unwrap() <= samples[1]["ts"].as_u64().unwrap());
        assert_eq!(snapshot["metadata"]["dropped_resource_samples"], 0);
        assert_eq!(snapshot["metadata"]["spilled_resource_samples"], 2);
        set_debug(&debug, false);
        sample_process();
        assert!(
            timeline_snapshot()["traceEvents"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        set_debug(&debug, true);
        sample_process();
        let reopened = timeline_snapshot();
        let processes = reopened["traceEvents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["name"] == "process")
            .collect::<Vec<_>>();
        assert!(
            processes.last().unwrap()["args"]
                .get("cpu_percent")
                .is_none(),
            "reopened collection must establish a new CPU baseline"
        );
    }

    #[test]
    fn queued_context_does_not_extend_parent_and_rejects_reopened_epoch() {
        if !isolated("queued_context_does_not_extend_parent_and_rejects_reopened_epoch") {
            return;
        }
        let debug = Arc::new(AtomicBool::new(true));
        bind_debug(&debug);
        tracing::subscriber::with_default(subscriber(), || {
            let parent = tracing::info_span!(target: "stravia::perf", "test.queue.origin");
            let context = parent.in_scope(current_context);
            drop(parent);
            let before = timeline_snapshot();
            assert!(
                before["traceEvents"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|event| event["name"] == "test.queue.origin" && event["ph"] == "X")
            );
            record_wait("test.queue.residence", context, Instant::now(), "completed");
            let child = tracing::info_span!(target: "stravia::perf", parent: None, "test.queue.process",
                causal_parent_id=context.span_id(), causal_epoch=context.epoch());
            child.in_scope(emit_sql_span);
            drop(child);
            let snapshot = timeline_snapshot();
            let child = snapshot["traceEvents"]
                .as_array()
                .unwrap()
                .iter()
                .find(|event| event["name"] == "test.queue.process")
                .unwrap();
            assert_eq!(child["args"]["parent_id"], context.span_id());
            let unparented = current_context();
            assert_eq!(unparented.span_id(), 0);
            set_debug(&debug, false);
            set_debug(&debug, true);
            record_wait("test.queue.stale", context, Instant::now(), "completed");
            let stale = tracing::info_span!(target: "stravia::perf", parent: None, "test.queue.stale_process",
                causal_parent_id=context.span_id(), causal_epoch=context.epoch());
            stale.in_scope(|| {
                let nested =
                    tracing::info_span!(target: "stravia::perf", "test.queue.stale_nested");
                nested.in_scope(emit_sql_span);
            });
            drop(stale);
            let stale = tracing::info_span!(target: "stravia::perf", parent: None, "test.queue.stale_unparented",
                causal_parent_id=unparented.span_id(), causal_epoch=unparented.epoch());
            stale.in_scope(emit_sql_span);
            drop(stale);
            let fresh = current_context();
            let fresh = tracing::info_span!(target: "stravia::perf", parent: None, "test.queue.fresh_unparented",
                causal_parent_id=fresh.span_id(), causal_epoch=fresh.epoch());
            drop(fresh);
            let snapshot = timeline_snapshot();
            assert!(!snapshot.to_string().contains("test.queue.stale"));
            let events = snapshot["traceEvents"].as_array().unwrap();
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event["name"] == "sql.query" && event["ph"] == "X")
                    .count(),
                0
            );
            assert!(
                events
                    .iter()
                    .any(|event| event["name"] == "test.queue.fresh_unparented")
            );
        });
    }
}
