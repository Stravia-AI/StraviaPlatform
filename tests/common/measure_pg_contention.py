"""PostgreSQL 并发争用实测：K 个并发写入客户端 + 后台反复强制历史扫描。

适用于新二进制与 baseline（baseline 不得提供 --redis-url）。强制扫描的机制见
`measure_history_sweep`：`PUT /api/v1/settings/log_retention_days` 在请求内同步执行
`observation.apply_retention_change` 与 `history.sweep`。后台线程在 N 与 N+1 之间交替
保存（两者都不会删除新鲜数据），每次之间隔 `--sweep-interval`（0 = 背靠背），直到写入负载结束。

采集：

* 写入客户端：harness 的 `run_load`（fixed-workers，多轮续写链），每请求延迟 p50/p95/p99、
  HTTP 状态计数、请求级错误；
* 服务日志（及可选的 PostgreSQL 日志文件增量）按 SQLSTATE 23503 / 40001 / 40P01、
  "foreign key"、"could not serialize"、"deadlock detected"、"content_not_found" 等分类计数，
  所有分类始终输出，零是观测到的零；
* `pg_stat_database` 前后差值（deadlocks、conflicts、xact_commit/rollback 等）；
* 负载期间按固定间隔采样 `pg_stat_activity`（非 idle、Lock 等待、被其他会话阻塞的数量及
  wait_event 分布），记录最大值 —— 采样是下界，短于采样间隔的等待可能漏掉；
* `history.sweep` 的 span 耗时（Debug 开启时）。

退出码：0 = 测量有效且无错误；1 = 测量无效；3 = 测量有效但观测到错误（见 `clean`、
`error_summary`）。

PostgreSQL 16 的 `pg_stat_database` 计数在后端空闲时才延迟刷新（最长约 10 秒），所以「之后」
快照在服务进程停止（后端强制刷新）之后读取；「之前」快照取自初始化完成之后，可能含启动阶段
尚未刷新的少量残余，对 deadlocks 这类稀有计数没有实际影响。
"""
from __future__ import annotations

import argparse
import json
import re
import sys
import threading
import time
from collections import Counter
from contextlib import ExitStack
from pathlib import Path

from tests.common.helpers import WebSession
from tests.common.measure_history_sweep import (
    Stack, add_stack_arguments, base_result, current_retention_days,
    fetch_timeline, open_stack, put_retention, reset_debug_epoch, run_cohort, safe_logs, scan_errors,
    start_servers, stop_servers, summarize_sweeps, validate_stack_arguments,
)
from tests.common.measure_protocol_http import percentile
from tests.common.protocol_storage_inventory import read_storage_inventory

PG_STAT_COLUMNS = ("xact_commit", "xact_rollback", "deadlocks", "conflicts", "temp_files", "temp_bytes",
                   "blks_read", "blks_hit", "tup_inserted", "tup_updated", "tup_deleted",
                   "sessions", "sessions_abandoned", "sessions_fatal", "sessions_killed")
ACTIVITY_SQL = """
SELECT count(*) AS backends,
       count(*) FILTER (WHERE state <> 'idle') AS non_idle,
       count(*) FILTER (WHERE state = 'active') AS active,
       count(*) FILTER (WHERE state LIKE 'idle in transaction%') AS idle_in_transaction,
       count(*) FILTER (WHERE wait_event_type = 'Lock') AS lock_waiting,
       count(*) FILTER (WHERE state <> 'idle' AND cardinality(pg_blocking_pids(pid)) > 0) AS blocked_by_other
FROM pg_stat_activity
WHERE datname = current_database() AND pid <> pg_backend_pid()
"""
WAIT_EVENT_SQL = """
SELECT wait_event_type, wait_event, count(*)
FROM pg_stat_activity
WHERE datname = current_database() AND pid <> pg_backend_pid() AND state = 'active' AND wait_event IS NOT NULL
GROUP BY 1, 2
"""
# 这些分类任何一个非零都意味着出现了真实的数据库/服务端错误（warn_level 只是背景噪声）。
ERROR_CATEGORY_NAMES = ("fk_violation_23503", "serialization_40001", "deadlock_40P01", "content_not_found",
                        "unique_violation_23505", "sweep_failed", "database_error", "panic", "error_level")


def read_pg_stat_database(url: str) -> dict:
    """新建连接读取（避免会话内统计快照缓存）；缺列即失败。"""
    import psycopg
    with psycopg.connect(url, autocommit=True) as connection:
        cursor = connection.execute(
            "SELECT " + ", ".join(PG_STAT_COLUMNS) + ", stats_reset, current_database() "
            "FROM pg_stat_database WHERE datname = current_database()")
        row = cursor.fetchone()
        if row is None:
            raise RuntimeError("pg_stat_database has no row for the current database")
        names = [column.name for column in cursor.description]
    snapshot = dict(zip(names, row))
    snapshot["stats_reset"] = snapshot["stats_reset"].isoformat() if snapshot["stats_reset"] else None
    return snapshot


class ActivitySampler(threading.Thread):
    """按固定间隔采样 pg_stat_activity；采样失败记录错误，使运行无效。"""

    def __init__(self, url: str, interval: float) -> None:
        super().__init__(name="pg-activity-sampler", daemon=True)
        self.url, self.interval = url, interval
        self.stop_event = threading.Event()
        self.samples: list[dict] = []
        self.wait_events: Counter = Counter()
        self.error: str | None = None

    def run(self) -> None:
        import psycopg
        try:
            with psycopg.connect(self.url, autocommit=True) as connection:
                while True:
                    cursor = connection.execute(ACTIVITY_SQL)
                    names = [column.name for column in cursor.description]
                    sample = dict(zip(names, cursor.fetchone()))
                    sample["timestamp"] = time.time()
                    self.samples.append(sample)
                    for event_type, event, count in connection.execute(WAIT_EVENT_SQL).fetchall():
                        self.wait_events[f"{event_type}:{event}"] += count
                    if self.stop_event.wait(self.interval):
                        return
        except Exception as error:
            self.error = f"{type(error).__name__}: {error}"

    def summary(self) -> dict:
        fields = ("backends", "non_idle", "active", "idle_in_transaction", "lock_waiting", "blocked_by_other")
        return {"sample_count": len(self.samples), "interval_seconds": self.interval, "error": self.error,
                "max": {field: max((sample[field] for sample in self.samples), default=None) for field in fields},
                "samples_with_lock_waiting": sum(sample["lock_waiting"] > 0 for sample in self.samples),
                "samples_with_blocked_by_other": sum(sample["blocked_by_other"] > 0 for sample in self.samples),
                "wait_event_active_sample_counts": dict(self.wait_events.most_common()),
                "scope": "sampled lower bound; waits shorter than the interval can be missed",
                "series": [[round(sample["timestamp"], 3), sample["non_idle"], sample["lock_waiting"],
                            sample["blocked_by_other"]] for sample in self.samples]}


class SweepForcer(threading.Thread):
    """后台反复强制扫描：在 N 与 N+1 之间交替保存，每次之间隔 --sweep-interval，直到被停止。"""

    def __init__(self, session: WebSession, days: tuple[int, int], timeout: float, interval: float) -> None:
        super().__init__(name="sweep-forcer", daemon=True)
        self.session, self.days, self.timeout, self.interval = session, days, timeout, interval
        self.stop_event = threading.Event()
        self.records: list[dict] = []
        self.error: str | None = None

    def run(self) -> None:
        index = 0
        try:
            while not self.stop_event.is_set():
                # put_retention 在非 200 时抛出，错误由 error 字段承载，不吞。
                self.records.append(put_retention(self.session, self.days[index % 2], self.timeout))
                index += 1
                # 两次强制扫描之间的固定间隔是负载形状参数（0 = 背靠背），不是等待某个条件。
                if self.interval and self.stop_event.wait(self.interval):
                    return
        except Exception as error:
            self.error = f"{type(error).__name__}: {error}"

    def summary(self) -> dict:
        walls = [record["wall_ms"] for record in self.records]
        return {"forced_sweeps": len(self.records), "error": self.error, "days_alternated": list(self.days),
                "interval_seconds": self.interval,
                "wall_ms": {"p50": percentile(walls, .5), "p95": percentile(walls, .95),
                            "p99": percentile(walls, .99), "max": max(walls, default=None)},
                "requests": self.records}


def pg_log_offset(path: Path | None) -> int | None:
    return path.stat().st_size if path else None


def pg_log_increment(path: Path | None, offset: int | None) -> dict:
    if path is None:
        return {"scanned": False, "reason": "no --pg-log-file; only pg_stat_database/pg_stat_activity are available"}
    with path.open("rb") as source:
        source.seek(offset)
        text = source.read().decode("utf-8", errors="replace")
    return {"scanned": True, "path": str(path), "bytes": len(text.encode()),
            **scan_errors(text.splitlines())}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    add_stack_arguments(parser)
    parser.add_argument("--concurrency", "--workers", type=int, default=32, dest="concurrency",
                        help="并发写入客户端数")
    parser.add_argument("--duration", type=float, default=120, help="写入准入窗口（秒）；在途请求随后排空")
    parser.add_argument("--requests", type=int,
                        help="改用有限请求数代替 --duration；必须 >= chain-turns * concurrency")
    parser.add_argument("--no-force-sweeps", action="store_true",
                        help="对照运行：不强制扫描（forced_sweeps 记为 0 且不作为无效条件）")
    parser.add_argument("--min-sweeps", type=int, default=1, help="强制扫描至少完成的次数，否则无效")
    parser.add_argument("--sweep-timeout", type=float, default=3600, help="每次 PUT 设置请求的 HTTP 超时")
    parser.add_argument("--sweep-interval", type=float, default=0.5,
                        help="两次强制扫描之间的间隔秒数（0 = 背靠背；背靠背会产生大量 span，可能超出 perf 缓冲）")
    parser.add_argument("--pg-sample-interval", type=float, default=0.25)
    parser.add_argument("--pg-log-file", type=Path,
                        help="可选：PostgreSQL 服务器日志文件；只扫描本次运行期间追加的内容")
    parser.add_argument("--debug-observation", action=argparse.BooleanOptionalAction, default=True,
                        help="负载期间开启 Observation Debug（与 harness 一致，并记录 history.sweep span）")
    parser.add_argument("--span-detail-limit", type=int, default=20,
                        help="带子树明细的 history.sweep 数量上限；汇总表覆盖全部")
    args = parser.parse_args()
    validate_stack_arguments(parser, args)
    if args.backend != "postgres":
        parser.error("--backend postgres is required (contention is measured on PostgreSQL)")
    if args.reuse_data_dir:
        parser.error("--reuse-data-dir is not supported: contention needs a freshly provisioned route")
    for name in ("duration", "sweep_timeout", "pg_sample_interval"):
        if not getattr(args, name) > 0:
            parser.error(f"--{name.replace('_', '-')} must be positive")
    if not args.sweep_interval >= 0:
        parser.error("--sweep-interval must be nonnegative")
    if args.requests is not None and args.requests < args.chain_turns * args.concurrency:
        parser.error("--requests must cover chain-turns * concurrency")
    if args.min_sweeps < 1 and not args.no_force_sweeps:
        parser.error("--min-sweeps must be at least 1")
    if args.pg_log_file is not None and not args.pg_log_file.is_file():
        parser.error("--pg-log-file must be an existing file")

    result = base_result(args, "pg_contention")
    result["forced_sweeps_enabled"] = not args.no_force_sweeps
    args.output.parent.mkdir(parents=True, exist_ok=True)
    catalog, upstreams = start_servers(args, populate=True)
    stack = Stack()
    sampler = forcer = None
    snapshot = None
    try:
        with ExitStack() as cleanup:
            open_stack(stack, args, cleanup, catalog, upstreams)
            result["server_pid"] = stack.process.pid
            session = stack.session
            raw_setting, effective_days = current_retention_days(session)
            result["retention_before"] = {"setting_response": raw_setting, "effective_days": effective_days}
            if args.debug_observation:
                reset_debug_epoch(session)
            result["pg_stat_database_before"] = read_pg_stat_database(args.database_url)
            result["pg_log_offset"] = pg_log_offset(args.pg_log_file)
            sampler = ActivitySampler(args.database_url, args.pg_sample_interval)
            sampler.start()
            if not args.no_force_sweeps:
                forcer = SweepForcer(session, (effective_days, effective_days + 1), args.sweep_timeout,
                                     args.sweep_interval)
                forcer.start()
            result["load_started_at"] = time.time()
            try:
                result["load"] = run_cohort(args, stack, upstreams[args.protocol], args.requests, args.duration)
            finally:
                # 无论负载是否成功，都先停掉后台线程再取证。
                if forcer:
                    forcer.stop_event.set()
                    forcer.join()
                sampler.stop_event.set()
                sampler.join()
            result["load_finished_at"] = time.time()
            if args.debug_observation:
                snapshot = fetch_timeline(session, args.audit_timeout)
            result["inventory_end"] = read_storage_inventory(args, stack.directory)
            # 停止服务让后端退出并强制刷新统计，再读「之后」快照。
            stack.stop()
            result["pg_stat_database_after"] = read_pg_stat_database(args.database_url)
            result["pg_log"] = pg_log_increment(args.pg_log_file, result["pg_log_offset"])
    except Exception as error:
        result["fatal_error"] = f"{type(error).__name__}: {error}"
    finally:
        for thread in (forcer, sampler):
            if thread:
                thread.stop_event.set()
                thread.join()
        stack.stop()
        stop_servers(catalog, upstreams)
        result["finished_at"] = time.time()
        logs, leaks = safe_logs(stack, args, upstreams)
        result["server_logs"] = logs
        result["server_log_scan"] = scan_errors(logs)
        result["credential_log_leaks"] = leaks
        result["server_exit_code_before_stop"] = stack.exit_code_before_stop
        if result["fatal_error"]:
            result["fatal_error"] = re.sub(r"(Stravia setup token:)\s*\S+", r"\1 [redacted]", result["fatal_error"])
        finish(args, result, sampler, forcer, snapshot)
        args.output.write_text(json.dumps(result, ensure_ascii=False, indent=2), encoding="utf-8")
    summary = {"output": str(args.output), "valid": result["valid"], "clean": result["clean"],
               "fatal_error": result["fatal_error"], "failure_reasons": result["failure_reasons"],
               "error_summary": result.get("error_summary"),
               "latency_ms_end_to_end": result.get("load", {}).get("latency_ms", {}).get("end_to_end"),
               "status_counts": result.get("load", {}).get("status_counts"),
               "forced_sweeps": result.get("sweep_forcer", {}).get("forced_sweeps"),
               "pg_deltas": result.get("pg_stat_database_delta"),
               "pg_activity_max": result.get("pg_activity", {}).get("max")}
    print(json.dumps(summary))
    if not result["valid"]:
        return 1
    return 0 if result["clean"] else 3


def finish(args: argparse.Namespace, result: dict, sampler: ActivitySampler | None,
           forcer: SweepForcer | None, snapshot: dict | None) -> None:
    """汇总各来源证据，判定 valid（测量完整）与 clean（未观测到任何错误）。"""
    reasons = result["failure_reasons"]
    load = result.get("load")
    if load is not None:
        # run_load 把任何请求失败都记为这一条；请求失败是被测现象（clean=False），不是测量作废。
        reasons.extend(f"load: {reason}" for reason in load["failure_reasons"]
                       if not reason.startswith("request/upstream validation failed"))
        floor_ms = (args.first_token_delay + args.output_seconds) * 1000 if args.delivery == "unary" else None
        result["upstream_unary_latency_floor_ms"] = floor_ms
        result["request_error_scan"] = scan_errors(
            [error for error, count in load["request_errors"].items() for _ in range(count)])
    else:
        reasons.append("load did not complete")
    if result["server_exit_code_before_stop"] is not None:
        reasons.append(f"server exited before shutdown: {result['server_exit_code_before_stop']}")
    if result["credential_log_leaks"]:
        reasons.append("synthetic credential leaked into server logs")
    if sampler is not None:
        result["pg_activity"] = sampler.summary()
        if sampler.error or not sampler.samples:
            reasons.append(f"pg_stat_activity sampling failed: {sampler.error or 'no samples'}")
    if forcer is not None:
        result["sweep_forcer"] = forcer.summary()
        if forcer.error:
            reasons.append(f"sweep forcing failed: {forcer.error}")
        if len(forcer.records) < args.min_sweeps:
            reasons.append(f"only {len(forcer.records)} forced sweeps completed (< {args.min_sweeps})")
    before, after = result.get("pg_stat_database_before"), result.get("pg_stat_database_after")
    if before and after:
        delta = {column: after[column] - before[column] for column in PG_STAT_COLUMNS}
        result["pg_stat_database_delta"] = delta
        if delta["xact_commit"] <= 0:
            reasons.append("pg_stat_database xact_commit did not advance; statistics not observed")
    elif not result["fatal_error"]:
        reasons.append("pg_stat_database snapshot missing")
    if args.debug_observation:
        if snapshot is None:
            reasons.append("performance timeline missing")
        else:
            perf = summarize_sweeps(snapshot, args.sweep_span, args.span_detail_limit)
            result["sweep_spans"] = perf
            forced = len(forcer.records) if forcer else 0
            perf["forced_sweep_requests"] = forced
            # span 只用于解释扫描耗时：缺失/丢失标记 sweep_spans_valid，不作废延迟与错误测量。
            result["sweep_spans_valid"] = perf["capture_complete"] and perf["sweep_count"] == forced
            result["sweep_span_name"] = args.sweep_span
    else:
        result["sweep_spans_valid"] = None
    result["valid"] = not result["fatal_error"] and not reasons
    # 错误汇总：服务日志、请求错误串、PostgreSQL 日志增量、pg_stat_database 同口径并列。
    summary = {name: {"server_logs": result["server_log_scan"]["categories"][name]["count"],
                      "request_errors": (result.get("request_error_scan") or {"categories": {}})["categories"]
                      .get(name, {"count": None})["count"],
                      "pg_log": ((result.get("pg_log") or {}).get("categories") or {}).get(name, {"count": None})["count"]}
               for name in ERROR_CATEGORY_NAMES}
    delta = result.get("pg_stat_database_delta") or {}
    summary["pg_stat_database_deadlocks"] = delta.get("deadlocks")
    summary["pg_stat_database_xact_rollback"] = delta.get("xact_rollback")
    summary["pg_stat_database_conflicts"] = delta.get("conflicts")
    summary["non_200_requests"] = (sum(count for status, count in load["status_counts"].items() if status != "200")
                                   if load else None)
    summary["failed_requests"] = load["errors"] if load else None
    result["error_summary"] = summary
    observed = [summary[name][source] for name in ERROR_CATEGORY_NAMES for source in summary[name]]
    counted = [count for count in observed if count is not None]
    result["clean"] = bool(result["valid"]) and not any(counted) and not summary["pg_stat_database_deadlocks"] \
        and not summary["non_200_requests"] and not summary["failed_requests"]


if __name__ == "__main__":
    sys.exit(main())
