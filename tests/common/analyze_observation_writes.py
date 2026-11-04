"""ADR-0089 实测：从 benchmark 产物统计每个 Inference Run 的 Observation 写入量。

输入是 `measure_protocol_http` 的结果 JSON、`--save-performance-snapshots` 目录和
`--data-dir` 保留的 SQLite 数据目录（只读打开）。输出 JSON：

* 计数（与机器负载无关）：每 Run 的 Observation 事件行、写租约次数、显式事务数、
  事务内 SQL 语句数、按 writer 动作的分布；
* 计时（受负载影响，仅供空闲机复测时使用）：写租约等待/持有时长。

归属规则：perf span 的 `parent_id` 链上存在名为 `observation.*` 的祖先即视为
Observation 写入，其余写入归入 `other`。不完整的快照（任何 dropped_* 非零）使该 case
作废，绝不外推。
"""
from __future__ import annotations

import argparse
import json
import math
import sqlite3
import statistics
from collections import Counter, defaultdict
from pathlib import Path


def _spans(snapshot: dict) -> dict:
    return {event["args"]["span_id"]: event for event in snapshot["traceEvents"]
            if event.get("ph") == "X" and "span_id" in event.get("args", {})}


def _chain(spans: dict, span: dict) -> list[str]:
    names, seen = [], set()
    while span is not None and span["args"]["span_id"] not in seen:
        seen.add(span["args"]["span_id"])
        names.append(span["name"])
        span = spans.get(span["args"].get("parent_id"))
    return names


def _group(chain: list[str]) -> str:
    """Observation 写入按最近的 writer 动作分组；其余写入标 other。"""
    for name in chain:
        if name.startswith("observation.writer."):
            return name.removeprefix("observation.writer.")
    if any(name.startswith("observation.") for name in chain):
        return "command_other"
    return "other"


def _split_commits(spans: dict) -> tuple[list[dict], list[dict]]:
    """把 `sql.transaction.commit` 分成真实事务提交与保存点释放。

    sqlx 对嵌套的 `begin()`（保存点）同样产生 begin/commit span。同一父 span 下按时间顺序维护
    深度：深度 1 时的 commit 是事务提交，更深的是保存点释放（不算一个事务）。
    """
    by_parent: dict = defaultdict(list)
    for span in spans.values():
        if span["name"] in ("sql.transaction.begin", "sql.transaction.commit"):
            by_parent[span["args"].get("parent_id")].append(span)
    outer, nested = [], []
    for group in by_parent.values():
        depth = 0
        for span in sorted(group, key=lambda s: (s["ts"], s["name"] == "sql.transaction.commit")):
            if span["name"] == "sql.transaction.begin":
                depth += 1
            else:
                (outer if depth <= 1 else nested).append(span)
                depth = max(depth - 1, 0)
    return outer, nested


def analyze_case(snapshot: dict) -> dict:
    meta = snapshot["metadata"]
    dropped = {key: value for key, value in meta.items() if key.startswith("dropped_") and value}
    spans = _spans(snapshot)
    runs = sum(1 for span in spans.values() if span["name"] == "proxy.inference_run")
    queries = defaultdict(list)
    for span in spans.values():
        if span["name"] == "sql.query":
            queries[span["args"].get("parent_id")].append(span)
    commits = Counter()
    leases, statements, hold_us, wait_us = Counter(), Counter(), Counter(), Counter()
    samples: dict[str, dict[str, list[int]]] = {c: {"hold": [], "wait": []} for c in ("observation", "other")}
    outer_commits, savepoint_commits = _split_commits(spans)
    for span in outer_commits:
        commits[_group(_chain(spans, span))] += 1
    savepoints = Counter(_group(_chain(spans, span)) for span in savepoint_commits)
    for span in spans.values():
        if span["name"] == "sqlite.write_gate.hold":
            group = _group(_chain(spans, span))
            leases[group] += 1
            hold_us[group] += span["dur"]
            samples["other" if group == "other" else "observation"]["hold"].append(span["dur"])
            start, end = span["ts"], span["ts"] + span["dur"]
            statements[group] += sum(1 for q in queries.get(span["args"].get("parent_id"), ())
                                     if start <= q["ts"] <= end)
    for span in (span for span in spans.values() if span["name"] == "sqlite.write_gate.wait"):
        group = _group(_chain(spans, span))
        wait_us[group] += span["dur"]
        samples["other" if group == "other" else "observation"]["wait"].append(span["dur"])
    return {"valid": not dropped and runs > 0, "dropped": dropped, "runs": runs,
            "write_leases": dict(leases), "explicit_transactions": dict(commits),
            "savepoint_commits": dict(savepoints),
            "statements_inside_lease": dict(statements),
            "lease_samples_us": samples,
            "timing_load_dependent": {"hold_us": dict(hold_us), "wait_us": dict(wait_us)}}


def _stats(values: list[int]) -> dict:
    """微秒样本的 n / 均值 / p50 / p95 / p99 / 最大值（最近秩分位数，不插值）。"""
    if not values:
        return {"n": 0}
    ordered = sorted(values)

    def pick(q: float) -> int:
        return ordered[min(len(ordered) - 1, math.ceil(q * len(ordered)) - 1)]

    return {"n": len(ordered), "mean": round(statistics.fmean(ordered), 1), "p50": pick(0.5),
            "p95": pick(0.95), "p99": pick(0.99), "max": ordered[-1]}


def event_rows(database: Path, run_ids: list[str]) -> dict:
    connection = sqlite3.connect(f"file:{database}?mode=ro", uri=True)
    try:
        columns = {row[1] for row in connection.execute("PRAGMA table_info(observation_events)")}
        blocks = ("observation_blocks" in {row[0] for row in connection.execute(
            "SELECT name FROM sqlite_master WHERE type='table'")})
        out = {}
        for run_id in run_ids:
            events = connection.execute("SELECT COUNT(*) FROM observation_events WHERE run_id=?", (run_id,)).fetchone()[0]
            row = {"events": events, "kinds": dict(connection.execute(
                "SELECT kind, COUNT(*) FROM observation_events WHERE run_id=? GROUP BY kind", (run_id,)).fetchall())}
            if blocks:
                row["blocks"] = connection.execute("SELECT COUNT(*) FROM observation_blocks WHERE run_id=?", (run_id,)).fetchone()[0]
            out[run_id] = row
        return out
    finally:
        connection.close()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--result", type=Path, required=True)
    parser.add_argument("--snapshots", type=Path, required=True)
    parser.add_argument("--data-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    result = json.loads(args.result.read_text(encoding="utf-8"))
    database = args.data_dir / "db" / "gateway.db"
    cases, totals = {}, defaultdict(Counter)
    kind_totals: dict[str, Counter] = defaultdict(Counter)
    pooled: dict[str, dict[str, list[int]]] = defaultdict(lambda: defaultdict(list))
    for case in result["cases"]:
        phase = case.get("measure")
        if phase is None:
            continue
        snapshot_path = args.snapshots / f"{case['id']}--measure-audit.json"
        analysis = analyze_case(json.loads(snapshot_path.read_text(encoding="utf-8")))
        rows = phase["chain_audit"]["requests"]
        run_ids = [row["run_id"] for row in rows]
        per_run = event_rows(database, run_ids)
        analysis["requests"] = len(rows)
        analysis["observation_events_per_run"] = [per_run[run_id]["events"] for run_id in run_ids]
        analysis["observation_blocks_per_run"] = [per_run[run_id].get("blocks") for run_id in run_ids]
        analysis["events_mean"] = statistics.fmean(analysis["observation_events_per_run"])
        kinds: Counter = Counter()
        for run_id in run_ids:
            kinds.update(per_run[run_id]["kinds"])
        analysis["event_rows_by_kind"] = dict(sorted(kinds.items()))
        analysis["case_valid"] = bool(phase["valid"]) and analysis["valid"] and analysis["runs"] == len(rows)
        cases[case["id"]] = analysis
        stream = case["id"].split("--")[2]
        if analysis["case_valid"]:
            bucket = totals[stream]
            bucket["runs"] += analysis["runs"]
            bucket["events"] += sum(analysis["observation_events_per_run"])
            kind_totals[stream].update(analysis["event_rows_by_kind"])
            bucket["observation_leases"] += sum(v for k, v in analysis["write_leases"].items() if k != "other")
            bucket["observation_transactions"] += sum(v for k, v in analysis["explicit_transactions"].items() if k != "other")
            bucket["observation_statements"] += sum(v for k, v in analysis["statements_inside_lease"].items() if k != "other")
            for cls, parts in analysis["lease_samples_us"].items():
                for part, values in parts.items():
                    pooled[stream][f"{cls}.{part}"].extend(values)
            bucket["other_leases"] += analysis["write_leases"].get("other", 0)
            bucket["other_transactions"] += analysis["explicit_transactions"].get("other", 0)
    summary = {}
    for stream, bucket in totals.items():
        runs = bucket["runs"]
        summary[stream] = {"runs": runs, "event_rows_by_kind_per_run": {k: round(v / runs, 3) for k, v in sorted(kind_totals[stream].items())}} | {key: round(value / runs, 3) for key, value in bucket.items() if key != "runs"}
        summary[stream]["lease_timing_us_load_dependent"] = {name: _stats(values) for name, values in sorted(pooled[stream].items())}
    args.output.write_text(json.dumps({"result": str(args.result), "binary_sha256": result.get("binary_sha256"),
                                       "summary_per_run": summary, "cases": cases},
                                      ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
