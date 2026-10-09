"""Align complete protocol benchmarks and export measured before/after costs."""
from __future__ import annotations

import argparse
import csv
import json
import math
from pathlib import Path

WORKLOAD_KEYS = (
    "duration", "warmup", "qps", "concurrency", "payload_bytes", "payload_pattern",
    "upstream_delay", "sample_interval", "request_timeout", "min_qps_ratio",
    "ingress", "upstream", "mode", "delivery", "backend",
)


def cases_by_id(result: dict) -> dict:
    if "finished_at" not in result or "valid" not in result:
        raise ValueError("benchmark has not finished; do not compare a checkpoint")
    cases = {case["id"]: case for case in result["cases"]}
    if len(cases) != len(result["cases"]) or not cases or result["fatal_error"]:
        raise ValueError("benchmark is empty, interrupted or contains duplicate case IDs")
    if any("measure" not in case or "server_resources" not in case for case in cases.values()):
        raise ValueError("benchmark contains unfinished cases")
    return cases


def number(value: float | None, divisor: float = 1) -> str:
    return "" if value is None else f"{value / divisor:.3f}"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("candidate", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--budget-mb", type=float, default=250)
    args = parser.parse_args()
    if not math.isfinite(args.budget_mb) or args.budget_mb <= 0:
        parser.error("budget-mb must be positive and finite")
    baseline = json.loads(args.baseline.read_text(encoding="utf-8"))
    candidate = json.loads(args.candidate.read_text(encoding="utf-8"))
    if any(result.get("schema_version") != 2 or result.get("response_validation") != "native-terminal-v1"
           for result in (baseline, candidate)):
        parser.error("comparison requires native-terminal benchmark schema 2; rerun older benchmarks")
    for key in ("schema_version", "response_validation", "platform", "logical_cpus", "cpu_units", "memory_units"):
        if baseline[key] != candidate[key]:
            parser.error(f"incomparable metadata: {key}")
    for key in WORKLOAD_KEYS:
        if baseline["workload"][key] != candidate["workload"][key]:
            parser.error(f"incomparable workload: {key}")
    try:
        before, after = cases_by_id(baseline), cases_by_id(candidate)
    except ValueError as error:
        parser.error(str(error))
    if before.keys() != after.keys():
        parser.error("case sets differ; do not compare a narrowed matrix")
    fields = ["case", "baseline_valid", "candidate_valid"]
    metrics = {
        "rss_peak_mb": ("rss_bytes", "max", 1_000_000),
        "private_commit_peak_mb": ("private_commit_bytes", "max", 1_000_000),
        "cpu_p50_cores": ("cpu_core_percent", "p50", 100),
        "cpu_p95_cores": ("cpu_core_percent", "p95", 100),
    }
    for label in ("baseline", "candidate"):
        fields.extend(f"{label}_{name}" for name in (*metrics, "successful_qps", "e2e_p95_ms", "ttft_p95_ms", "queue_p95_ms", "errors"))
    rows = []
    for identity, original in before.items():
        changed = after[identity]
        row = {"case": identity, "baseline_valid": original["valid"], "candidate_valid": changed["valid"]}
        for label, case in (("baseline", original), ("candidate", changed)):
            for name, (metric, aggregate, divisor) in metrics.items():
                row[f"{label}_{name}"] = number(case["server_resources"].get(metric, {}).get(aggregate), divisor)
            measurement = case["measure"]
            row[f"{label}_successful_qps"] = number(measurement["successful_qps"])
            for name, latency in (("e2e", "end_to_end"), ("ttft", "ttft"), ("queue", "queue")):
                row[f"{label}_{name}_p95_ms"] = number(measurement["latency_ms"][latency]["p95"])
            row[f"{label}_errors"] = measurement["errors"]
        rows.append(row)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("w", encoding="utf-8", newline="") as output:
        writer = csv.DictWriter(output, fieldnames=fields)
        writer.writeheader()
        writer.writerows(rows)
    normal = [case for case in after.values() if case["measure"]["mode"] == "open-loop"]
    budget_bytes = args.budget_mb * 1_000_000
    meets_memory = bool(normal) and all(
        case["server_resources"]["rss_bytes"]["max"] is not None
        and case["server_resources"]["rss_bytes"]["max"] < budget_bytes
        and (case["server_resources"]["private_commit_bytes"]["max"] is None
             or case["server_resources"]["private_commit_bytes"]["max"] < budget_bytes)
        for case in normal
    )
    meets_target = candidate["valid"] and meets_memory
    print(json.dumps({"output": str(args.output), "cases": len(rows), "baseline_sha256": baseline["binary_sha256"],
                      "candidate_sha256": candidate["binary_sha256"], "candidate_valid": candidate["valid"],
                      "normal_memory_budget_mb": args.budget_mb, "normal_memory_below_budget": meets_memory,
                      "meets_target": meets_target}))
    return 0 if meets_target else 1


if __name__ == "__main__":
    raise SystemExit(main())
