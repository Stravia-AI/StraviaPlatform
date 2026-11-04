"""ADR-0089 历史保留扫描（`history.sweep`）实测：稳态扫描与调小 N 后的首次扫描。

触发机制（来自 backend/crates/stravia-core 的实现，不是猜测）：

* 周期扫描：`STORE_SWEEP_INTERVAL` = 1 小时；服务启动时第一个 tick 立即触发，之后每小时一次。
  周期扫描不可由 HTTP 触发，也无法在 Debug 关闭时留下 perf span，因此本脚本不依赖它。
* 强制扫描：`PUT /api/v1/settings/log_retention_days`（`AdminService::set_setting`）在保存后
  依次执行 `observation.apply_retention_change` 与 `Gateway::sweep_history`，扫描在该 HTTP
  请求内同步完成。写入与当前相同的 N 即得到「无待处理变更的稳态扫描」，写入更小的 N 即得到
  「调小 N 后的首次扫描」。

测量流程：

1. 新数据目录：启动服务、完成初始化、启用凭据保护、创建 Provider/路由/API key，并用 harness
   的 `run_load` 跑一个小队列填充历史；或 `--reuse-data-dir` 指向已初始化的数据目录（SQLite
   的 `db/gateway.db`，或 PostgreSQL 的 `--database-url` 对应库），跳过填充。
2. 稳态扫描：每次测量前切换一次 Observation Debug（新 epoch，perf 环形缓冲独立），PUT 当前 N，
   记录请求墙钟与该 epoch 内 `history.sweep` 及其子 span。
3. 调小 N：前后各读一次只读库存（`read_storage_inventory`），PUT 更小的 N，记录请求墙钟、
   `history.sweep` 子树，以及各表行数差。
4. 调小后再测一次稳态扫描，确认首次扫描已完成全部工作。

span 通过轮询 `/api/v1/performance/timeline` 等待出现，超时即失败（错误里列出实际出现的 span 名称）；
任何 `dropped_*`、`storage_errors` 非零都使对应测量作废。span 名称取自实际时间线，不做假设：
baseline（ADR-0089 之前）没有 `history.sweep`，保存设置只触发 `observation.maintenance.sweep`
（其余域的扫描不在该请求内），baseline 请加 `--sweep-span observation.maintenance.sweep`。
PostgreSQL 上 baseline 与新二进制一样需要 `--redis-url`（否则 setup 返回 cache_unavailable）。
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import platform
import re
import sys
import tempfile
import threading
import time
import uuid
from collections import Counter, defaultdict
from contextlib import ExitStack
from http.server import ThreadingHTTPServer
from pathlib import Path
from types import SimpleNamespace
from urllib.parse import urlsplit

from tests.common.helpers import (
    WebSession, find_free_port, initialize_server, start_stravia_server,
    stop_stravia_server, wait_for_setup_token, wait_until_ready,
)
from tests.common.measure_protocol_http import (
    PROTOCOLS, Catalog, Upstream, database_config, percentile, provisioning, run_load,
)
from tests.common.protocol_storage_inventory import read_storage_inventory
from tests.common.protocol_workload import Workload

RETENTION_SETTING = "log_retention_days"
# 与 history_retention::DEFAULT_RETENTION_DAYS 一致：设置缺失时服务按 7 天处理。
DEFAULT_RETENTION_DAYS = 7
DEFAULT_SWEEP_SPAN = "history.sweep"
# 与 protocol_audit 的丢失判定字段一致：任何一项不是整数 0，该 epoch 的 span 都不完整。
LOSS_FIELDS = ("dropped_active", "dropped_completed", "dropped_sql_completed",
               "dropped_resource_samples", "frozen_spans", "frozen_total", "storage_errors")

# 服务日志 / PostgreSQL 日志的错误分类；所有分类始终输出（零是观测到的零）。
# SQLSTATE 代码必须前后都不是数字或 `.`，否则会误匹配 `20:56:07.823503Z` 这样的时间戳小数。
ERROR_CATEGORIES = (
    ("fk_violation_23503", re.compile(r"(?<![\d.])23503(?!\d)|foreign key", re.I)),
    ("serialization_40001", re.compile(r"(?<![\d.])40001(?!\d)|could not serialize|serialization failure", re.I)),
    ("deadlock_40P01", re.compile(r"(?<![\d.])40P01(?!\w)|deadlock detected", re.I)),
    ("content_not_found", re.compile(r"content_not_found|content not found", re.I)),
    ("unique_violation_23505", re.compile(r"(?<![\d.])23505(?!\d)|duplicate key", re.I)),
    ("sweep_failed", re.compile(r"history retention sweep failed", re.I)),
    ("database_error", re.compile(r"error returned from database|SQLSTATE", re.I)),
    ("panic", re.compile(r"panicked at", re.I)),
    ("error_level", re.compile(r"\bERROR\b")),
    ("warn_level", re.compile(r"\bWARN(?:ING)?\b")),
)


# ── 命令行 ────────────────────────────────────────────────────────────────────

def add_stack_arguments(parser: argparse.ArgumentParser) -> None:
    """服务、存储与小型负载的共用参数。"""
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--label", required=True)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--backend", choices=("sqlite", "postgres"), default="sqlite")
    parser.add_argument("--database-url")
    parser.add_argument("--confirm-isolated-database", action="store_true")
    parser.add_argument("--redis-url", help="仅新二进制在 PostgreSQL 上使用；baseline 必须留空")
    parser.add_argument("--data-dir", type=Path,
                        help="保留数据目录（默认必须不存在）；与 --reuse-data-dir 合用时必须已初始化")
    parser.add_argument("--reuse-data-dir", action="store_true",
                        help="--data-dir 已由先前的运行初始化：不做初始化、不创建 Provider、不填充历史")
    parser.add_argument("--protocol", choices=PROTOCOLS, default="openai-chat",
                        help="填充/负载使用的入口与上游协议")
    parser.add_argument("--delivery", choices=("unary", "stream"), default="unary")
    parser.add_argument("--seed", type=int, default=1)
    parser.add_argument("--chain-turns", type=int, default=2)
    parser.add_argument("--min-input-tokens", type=int, default=2000)
    parser.add_argument("--max-input-tokens", type=int, default=4000)
    parser.add_argument("--images", type=int, default=0, help="每个请求固定的图片数")
    parser.add_argument("--image-size", type=int, default=64)
    parser.add_argument("--first-token-delay", type=float, default=0.0)
    parser.add_argument("--tokens-per-second", type=int, default=200,
                        help="输出必须能容纳含凭据探针的关联前缀，因此下限偏高")
    parser.add_argument("--output-seconds", type=int, default=1)
    parser.add_argument("--request-timeout", type=float, default=120)
    parser.add_argument("--audit-timeout", type=float, default=180,
                        help="performance timeline 导出的 HTTP 超时")
    parser.add_argument("--startup-timeout", type=float, default=600,
                        help="服务就绪等待上限（升级已有大库时迁移可能很久）")
    parser.add_argument("--sweep-span", default=DEFAULT_SWEEP_SPAN,
                        help="扫描 span 名称。新二进制是 history.sweep；baseline 没有该 span，保存设置只会触发 "
                             "observation.maintenance.sweep（超时错误会列出实际出现的 span 名称）")
    parser.add_argument("--source-commit", default="unspecified")
    parser.add_argument("--build-condition", default="unspecified")
    # run_load 读取的固定字段；fixed-workers 模式下 qps 不被使用。
    parser.set_defaults(qps=1.0, max_start_lag_ms=1000.0, max_pacing_lag_ms=250.0)


def validate_stack_arguments(parser: argparse.ArgumentParser, args: argparse.Namespace) -> None:
    for name in ("request_timeout", "audit_timeout", "startup_timeout", "first_token_delay"):
        if not math.isfinite(getattr(args, name)) or getattr(args, name) < 0:
            parser.error(f"--{name.replace('_', '-')} must be finite and nonnegative")
    for name in ("request_timeout", "audit_timeout", "startup_timeout"):
        if getattr(args, name) <= 0:
            parser.error(f"--{name.replace('_', '-')} must be positive")
    if args.concurrency < 1 or args.tokens_per_second < 1 or args.output_seconds < 1:
        parser.error("--concurrency, --tokens-per-second and --output-seconds must be positive")
    if args.chain_turns < 2:
        parser.error("--chain-turns must be at least 2 (real continuations are required)")
    if args.min_input_tokens < 1 or args.min_input_tokens > args.max_input_tokens:
        parser.error("input token bounds invalid")
    reserve = (args.chain_turns - 1) * (args.tokens_per_second * args.output_seconds + 512)
    if args.min_input_tokens + reserve > args.max_input_tokens:
        parser.error("input token range cannot reserve the requested continuation budget")
    if args.images < 0 or args.image_size < 2:
        parser.error("--images must be nonnegative and --image-size at least 2")
    if args.backend == "sqlite" and (args.database_url or args.redis_url):
        parser.error("--database-url/--redis-url require --backend postgres")
    if args.reuse_data_dir and not args.data_dir:
        parser.error("--reuse-data-dir requires --data-dir")
    if args.redis_url and urlsplit(args.redis_url).hostname not in ("127.0.0.1", "localhost", "::1"):
        parser.error("Redis must be a dedicated loopback instance")
    try:
        database_config(args)
    except ValueError as error:
        parser.error(str(error))
    # run_load 以这两个字段预留续写预算与上游节奏。
    args.max_tokens_per_second = args.tokens_per_second
    args.max_duration = args.output_seconds
    args.workload_generator = Workload(
        seed=args.seed, min_input_tokens=args.min_input_tokens, max_input_tokens=args.max_input_tokens,
        min_first_token_delay=args.first_token_delay, max_first_token_delay=args.first_token_delay,
        min_tokens_per_second=args.tokens_per_second, max_tokens_per_second=args.tokens_per_second,
        min_duration=args.output_seconds, max_duration=args.output_seconds,
        min_images=args.images, max_images=args.images, image_size=args.image_size,
        credential_probes=True)


def redacted_command(args: argparse.Namespace) -> list[str]:
    """命令行记录：连接串不落盘。"""
    secrets = {args.database_url, args.redis_url} - {None}
    return [("[connection URL omitted]" if part in secrets else part) for part in sys.argv]


def base_result(args: argparse.Namespace, kind: str) -> dict:
    binary = args.binary.resolve()
    with binary.open("rb") as source:
        sha = hashlib.file_digest(source, "sha256").hexdigest()
    metadata = {}
    for key, item in vars(args).items():
        if key == "workload_generator":
            continue
        if key in ("database_url", "redis_url"):
            item = "provided (omitted)" if item else None
        metadata[key] = str(item) if isinstance(item, Path) else item
    return {"schema_version": 1, "kind": kind, "run_id": uuid.uuid4().hex, "label": args.label,
            "timestamp": time.time(), "command": redacted_command(args),
            "binary": str(binary), "binary_sha256": sha, "backend": args.backend,
            "redis_configured": bool(args.redis_url), "platform": platform.platform(),
            "arguments": metadata, "valid": False, "failure_reasons": [], "fatal_error": None}


# ── 服务栈 ────────────────────────────────────────────────────────────────────

class Stack:
    """一次运行的服务进程、日志与管理会话。"""

    def __init__(self) -> None:
        self.process = None
        self.logs: list[str] = []
        self.base = ""
        self.session: WebSession | None = None
        self.directory = ""
        self.key: str | None = None
        self.routes: dict | None = None
        self.exit_code_before_stop: int | None = None

    def stop(self) -> None:
        """幂等停止；停止前已退出的进程是崩溃，记录退出码。"""
        if self.process is None:
            return
        self.exit_code_before_stop = self.process.poll()
        stop_stravia_server(self.process, self.logs, print_tail=0)
        self.process = None


def login(base: str) -> WebSession:
    """复用 initialize_server 的默认管理员凭据登录已初始化的服务。"""
    defaults = initialize_server.__kwdefaults__
    session = WebSession(base)
    status, body = session.request("POST", "/api/v1/auth/login", {
        "username": defaults["username"],
        "password": next(item for name, item in defaults.items() if name != "username"),
    })
    if status != 200:
        raise RuntimeError(f"admin login HTTP {status}: {body}")
    return session


def open_stack(stack: Stack, args: argparse.Namespace, cleanup: ExitStack, catalog: ThreadingHTTPServer,
               upstreams: dict[str, Upstream]) -> None:
    """启动服务并取得管理会话；新目录还会完成初始化、启用凭据保护并创建路由。

    `stack` 由调用方创建：即使这里中途失败，日志与进程句柄仍可用于收尾与诊断。
    """
    database = database_config(args)
    if args.reuse_data_dir:
        directory = args.data_dir.resolve()
        if not directory.is_dir():
            raise RuntimeError(f"--reuse-data-dir: {directory} does not exist")
        if args.backend == "sqlite" and not (directory / "db" / "gateway.db").is_file():
            raise RuntimeError(f"--reuse-data-dir: {directory}/db/gateway.db missing")
    elif args.data_dir:
        args.data_dir.mkdir(parents=True, exist_ok=False)
        directory = args.data_dir.resolve()
    else:
        directory = Path(cleanup.enter_context(tempfile.TemporaryDirectory(prefix="adr0089-")))
    stack.directory = str(directory)
    cleanup.callback(stack.stop)
    port = find_free_port()
    command = ["--host", "127.0.0.1", "--port", str(port), "--data-dir", stack.directory,
               "--test-catalog-base-url", f"http://127.0.0.1:{catalog.server_port}"]
    if args.redis_url:
        # 配置文件放在独立临时目录，复用的数据目录保持只读语义。
        config_dir = Path(cleanup.enter_context(tempfile.TemporaryDirectory(prefix="adr0089-config-")))
        config_path = config_dir / "redis.toml"
        config_path.write_text("[cache]\nredis_url = " + json.dumps(args.redis_url) + "\n", encoding="utf-8")
        command.extend(["--config", str(config_path)])
    stack.process, stack.logs = start_stravia_server(
        stravia_binary=args.binary.resolve(), args=command,
        env={"RUST_LOG": "warn,stravia_server=info"})
    stack.base = f"http://127.0.0.1:{port}"
    if args.reuse_data_dir:
        wait_until_ready(f"{stack.base}/api/v1/auth/state", timeout=args.startup_timeout)
        stack.session = login(stack.base)
        return
    token = wait_for_setup_token(stack.logs, stack.process, timeout=args.startup_timeout)
    stack.session = initialize_server(stack.base, token, database)
    status, _ = stack.session.request("PUT", "/api/v1/settings/reversible_redaction_enabled",
                                      {"value": "true"})
    if status != 200:
        raise RuntimeError(f"credential protection setup HTTP {status}")
    status, protected = stack.session.request("GET", "/api/v1/settings/reversible_redaction_enabled")
    if status != 200 or protected.get("data") != "true":
        raise RuntimeError("credential protection was not enabled")
    stack.key, stack.routes = provisioning(stack.session, upstreams)


def start_servers(args: argparse.Namespace, populate: bool) -> tuple[ThreadingHTTPServer, dict[str, Upstream]]:
    """Catalog 总是需要（--test-catalog-base-url）；上游只在会发请求时创建。"""
    catalog = ThreadingHTTPServer(("127.0.0.1", 0), Catalog)
    catalog.arrivals = []
    threading.Thread(target=catalog.serve_forever, daemon=True).start()
    upstreams = {args.protocol: Upstream(args.protocol, args.workload_generator)} if populate else {}
    return catalog, upstreams


def stop_servers(catalog: ThreadingHTTPServer, upstreams: dict[str, Upstream]) -> None:
    for _, upstream in upstreams.items():
        upstream.shutdown()
        upstream.server_close()
    catalog.shutdown()
    catalog.server_close()


def run_cohort(args: argparse.Namespace, stack: Stack, upstream: Upstream, requests: int | None,
               duration: float) -> dict:
    """用 harness 的 fixed-workers 负载发请求；返回去掉逐请求明细的摘要。"""
    args.requests_per_case = requests
    load = run_load(args, stack.base, stack.key, args.protocol, upstream, args.delivery == "stream",
                    "fixed-workers", duration, "measure", stack.routes[args.protocol])
    rows = load.pop("requests")
    load.pop("upstream_arrivals")
    load["request_count"] = len(rows)
    load["request_errors"] = Counter(row["error"] for row in rows if row["error"])
    return load


# ── Debug epoch / timeline ───────────────────────────────────────────────────

def reset_debug_epoch(session: WebSession) -> None:
    """关→开 Observation Debug：新 epoch，perf 缓冲只含此后的 span。绝不在请求执行中调用。"""
    for enabled in (False, True):
        status, body = session.request("PUT", "/api/v1/observations/debug",
                                       {"enabled": enabled, "confirmed": True})
        if status != 200:
            raise RuntimeError(f"Debug toggle enabled={enabled} HTTP {status}: {body}")


def fetch_timeline(session: WebSession, timeout: float) -> dict:
    status, snapshot = session.request("GET", "/api/v1/performance/timeline", timeout=timeout)
    if status != 200:
        raise RuntimeError(f"performance timeline HTTP {status}")
    return snapshot


def capture_loss(metadata: dict) -> dict:
    """返回各丢失字段的实际值；缺失或非整数记为 None，同样视为不完整。"""
    return {field: (metadata[field] if type(metadata.get(field)) is int else None) for field in LOSS_FIELDS}


def completed_spans(snapshot: dict) -> list[dict]:
    return [event for event in snapshot["traceEvents"]
            if event.get("ph") == "X" and "span_id" in event.get("args", {})
            and event["args"].get("capture_state") == "finished"]


def wait_for_sweep_spans(session: WebSession, span_name: str, expected: int, timeout: float,
                         poll_seconds: float, audit_timeout: float) -> dict:
    """轮询 timeline 直到至少 expected 个已完成的扫描 span；超时即失败并列出实际出现的 span 名称。"""
    deadline = time.monotonic() + timeout
    pause = threading.Event()
    while True:
        snapshot = fetch_timeline(session, audit_timeout)
        spans = completed_spans(snapshot)
        found = sum(1 for span in spans if span["name"] == span_name)
        if found >= expected:
            return snapshot
        if time.monotonic() >= deadline:
            raise TimeoutError(f"{span_name}: expected {expected} completed spans, found {found} "
                               f"after {timeout}s; span names present: {sorted({span['name'] for span in spans})}")
        pause.wait(poll_seconds)


def span_stats(durations: list[int]) -> dict:
    return {"count": len(durations), "total_us": sum(durations),
            "p50_us": percentile(durations, .5), "p95_us": percentile(durations, .95),
            "max_us": max(durations, default=None)}


def name_table(spans: list[dict]) -> dict:
    grouped: dict[str, list[int]] = defaultdict(list)
    for span in spans:
        grouped[span["name"]].append(span["dur"])
    return {name: span_stats(durations) for name, durations in sorted(grouped.items())}


def query_table(spans: list[dict], top: int) -> list[dict]:
    """sql.query 按模板指纹（query_id，无 SQL 文本）汇总，按总耗时降序取前 top 个。"""
    grouped: dict[str, list[dict]] = defaultdict(list)
    for span in spans:
        if span["name"] == "sql.query":
            grouped[span["args"].get("query_id", "unavailable")].append(span)
    rows = [{"query_id": query_id, **span_stats([span["dur"] for span in group]),
             "rows_returned_total": sum(span["args"].get("rows_returned", 0) for span in group)}
            for query_id, group in grouped.items()]
    return sorted(rows, key=lambda row: -row["total_us"])[:top]


def summarize_sweeps(snapshot: dict, span_name: str, detail_limit: int | None = None) -> dict:
    """扫描 span（默认 `history.sweep`）的逐次耗时、子树 span 按名称汇总，以及整个 epoch 的 span 名称普查。

    子树按 `parent_id` 因果链归属（异步因果关系，不是时间包含关系）；普查不依赖父子关系，
    用于发现未挂在 sweep 下的相关 span。`detail_limit` 限制带子树明细的 sweep 数量
    （大量强制扫描时避免结果膨胀）；汇总表始终覆盖全部 sweep。
    """
    spans = completed_spans(snapshot)
    by_id = {span["args"]["span_id"]: span for span in spans}
    children: dict[int, list[int]] = defaultdict(list)
    for span in spans:
        parent = span["args"].get("parent_id")
        if parent is not None:
            children[parent].append(span["args"]["span_id"])
    sweeps = sorted((span for span in spans if span["name"] == span_name), key=lambda span: span["ts"])
    subtree_spans: list[dict] = []
    detail = []
    for index, sweep in enumerate(sweeps):
        pending, subtree = [sweep["args"]["span_id"]], []
        while pending:
            for child in children.get(pending.pop(), ()):
                subtree.append(by_id[child])
                pending.append(child)
        subtree_spans.extend(subtree)
        ancestors, parent = [], sweep["args"].get("parent_id")
        while parent is not None and parent in by_id:
            ancestors.append(by_id[parent]["name"])
            parent = by_id[parent]["args"].get("parent_id")
        row = {"span_id": sweep["args"]["span_id"], "ts_unix_us": sweep["ts"], "dur_us": sweep["dur"],
               "args": sweep["args"], "ancestor_names": ancestors}
        if detail_limit is None or index < detail_limit:
            row.update(descendant_names=name_table(subtree), sql_by_query_id=query_table(subtree, 12))
        detail.append(row)
    metadata = snapshot["metadata"]
    loss = capture_loss(metadata)
    return {"debug_epoch": metadata.get("debug_epoch"), "capture_loss": loss,
            "capture_complete": all(item == 0 for item in loss.values()),
            "metadata_incomplete": metadata.get("incomplete"),
            "sweep_count": len(sweeps), "sweep_dur_us": span_stats([sweep["dur"] for sweep in sweeps]),
            "sweeps": detail, "subtree_by_name": name_table(subtree_spans),
            "subtree_sql_by_query_id": query_table(subtree_spans, 20),
            "epoch_span_census": name_table(spans)}


# ── 日志扫描 ──────────────────────────────────────────────────────────────────

def scan_errors(lines: list[str], sample_limit: int = 5) -> dict:
    """每个分类的命中行数与前几条样例；所有分类都出现在结果里，零是观测到的零。"""
    result = {name: {"count": 0, "samples": []} for name, _ in ERROR_CATEGORIES}
    for line in lines:
        for name, pattern in ERROR_CATEGORIES:
            if pattern.search(line):
                entry = result[name]
                entry["count"] += 1
                if len(entry["samples"]) < sample_limit:
                    entry["samples"].append(line[:400])
    return {"lines_scanned": len(lines), "categories": result}


def safe_logs(stack: Stack, args: argparse.Namespace, upstreams: dict[str, Upstream]) -> tuple[list[str], int]:
    """服务日志脱敏（setup token、连接串、合成凭据）；返回脱敏行与泄漏行数。"""
    synthetic = {item for _, upstream in upstreams.items()
                 for item in upstream.synthetic_secrets}
    leaks, safe = 0, []
    for line in list(stack.logs):
        leaked = False
        for item in synthetic:
            if item in line:
                leaked = True
                line = line.replace(item, "[synthetic credential omitted]")
        leaks += leaked
        line = re.sub(r"(Stravia setup token:)\s*\S+", r"\1 [redacted]", line)
        for url in (args.database_url, args.redis_url):
            if url:
                line = line.replace(url, "[connection URL omitted]")
        safe.append(line)
    return safe, leaks


# ── 设置读写 ──────────────────────────────────────────────────────────────────

def current_retention_days(session: WebSession) -> tuple[object, int]:
    """返回 (原始设置响应, 生效 N)。设置缺失时服务按默认 7 天处理。"""
    status, body = session.request("GET", f"/api/v1/settings/{RETENTION_SETTING}")
    if status == 200:
        raw = body.get("data") if isinstance(body, dict) else None
        if raw is None:
            return body, DEFAULT_RETENTION_DAYS
        return body, int(raw)
    if status == 404:
        return body, DEFAULT_RETENTION_DAYS
    raise RuntimeError(f"GET {RETENTION_SETTING} HTTP {status}: {body}")


def put_retention(session: WebSession, days: int, timeout: float) -> dict:
    """保存 N 并计时；扫描在该请求内同步完成，所以墙钟含 apply_retention_change 与扫描。"""
    started = time.time()
    begin = time.perf_counter()
    status, body = session.request("PUT", f"/api/v1/settings/{RETENTION_SETTING}",
                                   {"value": str(days)}, timeout=timeout)
    wall_ms = (time.perf_counter() - begin) * 1000
    if status != 200:
        raise RuntimeError(f"PUT {RETENTION_SETTING}={days} HTTP {status}: {body}")
    return {"days": days, "status": status, "started_at": started, "wall_ms": wall_ms}


def measure_sweep(args: argparse.Namespace, session: WebSession, days: int, label: str) -> dict:
    """新 epoch → PUT → 等 span → 汇总。一个 epoch 内必须恰好一次扫描 span。"""
    reset_debug_epoch(session)
    request = put_retention(session, days, args.sweep_timeout)
    snapshot = wait_for_sweep_spans(session, args.sweep_span, 1, args.span_timeout, args.poll_seconds,
                                    args.audit_timeout)
    summary = summarize_sweeps(snapshot, args.sweep_span)
    reasons = []
    if summary["sweep_count"] != 1:
        reasons.append(f"{label}: expected exactly one {args.sweep_span} span in the epoch, "
                       f"found {summary['sweep_count']}")
    if not summary["capture_complete"]:
        reasons.append(f"{label}: perf capture incomplete or lossy {summary['capture_loss']}")
    return {"label": label, "request": request, "perf": summary, "valid": not reasons,
            "failure_reasons": reasons}


def inventory_delta(before: dict, after: dict) -> dict:
    tables = sorted(set(before["row_counts"]) | set(after["row_counts"]))
    delta = {table: after["row_counts"].get(table, 0) - before["row_counts"].get(table, 0) for table in tables}
    return {"per_table": delta,
            "deleted_rows_total": -sum(change for change in delta.values() if change < 0),
            "inserted_rows_total": sum(change for change in delta.values() if change > 0)}


# ── 主流程 ────────────────────────────────────────────────────────────────────

def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    add_stack_arguments(parser)
    parser.add_argument("--concurrency", type=int, default=4, help="填充负载的 worker 数")
    parser.add_argument("--cohort-requests", type=int, default=16,
                        help="填充历史的请求数（新数据目录）；必须 >= chain-turns * concurrency")
    parser.add_argument("--steady-repeats", type=int, default=3, help="稳态扫描重复次数（各自独立 epoch）")
    parser.add_argument("--warmup-sweeps", type=int, default=1,
                        help="不计入结果的预热扫描（PUT 当前 N），用于避开启动扫描的尾巴")
    parser.add_argument("--lower-to", type=int, default=1,
                        help="调小后的 N；必须小于当前生效 N（新鲜队列没有超过 1 天的数据，因此不会删除行）")
    parser.add_argument("--post-lower-steady", type=int, default=1, help="调小后追加的稳态扫描次数")
    parser.add_argument("--sweep-timeout", type=float, default=3600, help="PUT 设置请求的 HTTP 超时")
    parser.add_argument("--span-timeout", type=float, default=60, help="等待 history.sweep span 出现的上限")
    parser.add_argument("--poll-seconds", type=float, default=0.2)
    args = parser.parse_args()
    validate_stack_arguments(parser, args)
    for name in ("steady_repeats", "warmup_sweeps", "post_lower_steady"):
        if getattr(args, name) < 0:
            parser.error(f"--{name.replace('_', '-')} must be nonnegative")
    if args.steady_repeats < 1:
        parser.error("--steady-repeats must be at least 1")
    if args.lower_to < 1:
        parser.error("--lower-to must be at least 1 (MIN_RETENTION_DAYS)")
    for name in ("sweep_timeout", "span_timeout", "poll_seconds"):
        if getattr(args, name) <= 0:
            parser.error(f"--{name.replace('_', '-')} must be positive")
    populate = not args.reuse_data_dir and args.cohort_requests > 0
    if populate and args.cohort_requests < args.chain_turns * args.concurrency:
        parser.error("--cohort-requests must cover chain-turns * concurrency")

    result = base_result(args, "history_sweep")
    result["sweep_span_name"] = args.sweep_span
    args.output.parent.mkdir(parents=True, exist_ok=True)
    catalog, upstreams = start_servers(args, populate=not args.reuse_data_dir)
    stack = Stack()
    try:
        with ExitStack() as cleanup:
            open_stack(stack, args, cleanup, catalog, upstreams)
            result["server_pid"] = stack.process.pid
            if populate:
                result["cohort"] = run_cohort(args, stack, upstreams[args.protocol], args.cohort_requests, 0)
                if not result["cohort"]["valid"]:
                    result["failure_reasons"].append("cohort load invalid: " + "; ".join(result["cohort"]["failure_reasons"]))
            session = stack.session
            raw_setting, effective_days = current_retention_days(session)
            result["retention_before"] = {"setting_response": raw_setting, "effective_days": effective_days}
            if args.lower_to >= effective_days:
                raise RuntimeError(f"--lower-to {args.lower_to} must be below the current N {effective_days}")
            result["inventory_start"] = read_storage_inventory(args, stack.directory)
            # 预热：把 N 写成当前值，等启动扫描与首次连接/缓存的尾巴落定；不进入统计。
            result["warmup"] = [put_retention(session, effective_days, args.sweep_timeout)
                                for _ in range(args.warmup_sweeps)]
            result["steady"] = [measure_sweep(args, session, effective_days, f"steady-{index}")
                                for index in range(args.steady_repeats)]
            before = read_storage_inventory(args, stack.directory)
            lowered = measure_sweep(args, session, args.lower_to, "lowered-first")
            after = read_storage_inventory(args, stack.directory)
            lowered["inventory_before"] = before
            lowered["inventory_after"] = after
            lowered["inventory_delta"] = inventory_delta(before, after)
            result["lowered"] = lowered
            result["post_lower_steady"] = [measure_sweep(args, session, args.lower_to, f"post-lower-steady-{index}")
                                           for index in range(args.post_lower_steady)]
            _, result["retention_after"] = current_retention_days(session)
            for section in (*result["steady"], lowered, *result["post_lower_steady"]):
                result["failure_reasons"].extend(section["failure_reasons"])
            result["span_names_observed"] = sorted({name for section in (*result["steady"], lowered)
                                                    for name in section["perf"]["epoch_span_census"]})
            stack.stop()
    except Exception as error:
        result["fatal_error"] = f"{type(error).__name__}: {error}"
    finally:
        stack.stop()
        stop_servers(catalog, upstreams)
        result["finished_at"] = time.time()
        logs, leaks = safe_logs(stack, args, upstreams)
        result["server_logs"] = logs
        result["server_log_scan"] = scan_errors(logs)
        result["credential_log_leaks"] = leaks
        result["server_exit_code_before_stop"] = stack.exit_code_before_stop
        if leaks:
            result["failure_reasons"].append("synthetic credential leaked into server logs")
        if stack.exit_code_before_stop is not None:
            result["failure_reasons"].append(f"server exited before shutdown: {stack.exit_code_before_stop}")
        if result["server_log_scan"]["categories"]["sweep_failed"]["count"]:
            result["failure_reasons"].append("server logged 'history retention sweep failed'")
        if result["fatal_error"]:
            result["fatal_error"] = re.sub(r"(Stravia setup token:)\s*\S+", r"\1 [redacted]", result["fatal_error"])
        result["valid"] = not result["fatal_error"] and not result["failure_reasons"] and "lowered" in result
        args.output.write_text(json.dumps(result, ensure_ascii=False, indent=2), encoding="utf-8")
    steady = [item["perf"]["sweep_dur_us"]["max_us"] for item in result.get("steady", [])]
    lowered = result.get("lowered")
    print(json.dumps({
        "output": str(args.output), "valid": result["valid"], "fatal_error": result["fatal_error"],
        "failure_reasons": result["failure_reasons"], "backend": args.backend,
        "steady_sweep_dur_us": steady,
        "lowered_sweep_dur_us": lowered["perf"]["sweep_dur_us"]["max_us"] if lowered else None,
        "lowered_deleted_rows": lowered["inventory_delta"]["deleted_rows_total"] if lowered else None,
        "span_names_observed": result.get("span_names_observed"),
    }))
    return 0 if result["valid"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
