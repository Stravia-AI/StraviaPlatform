"""Align complete protocol benchmarks and export measured before/after costs."""
from __future__ import annotations

import argparse
import csv
import json
import math
from pathlib import Path

WORKLOAD_KEYS = (
    "duration", "warmup", "qps", "concurrency", "requests_per_case", "seed", "chain_turns",
    "min_input_tokens", "max_input_tokens", "min_images", "max_images", "image_size",
    "min_first_token_delay", "max_first_token_delay", "min_tokens_per_second",
    "max_tokens_per_second", "min_duration", "max_duration",
    "sample_interval", "request_timeout", "audit_timeout", "max_start_lag_ms", "max_pacing_lag_ms",
    "ingress", "upstream", "mode", "delivery", "backend",
)


def cases_by_id(result: dict) -> dict:
    if "finished_at" not in result or "valid" not in result:
        raise ValueError("benchmark has not finished; do not compare a checkpoint")
    cases = {case["id"]: case for case in result["cases"]}
    if len(cases) != len(result["cases"]) or not cases or result["fatal_error"]:
        raise ValueError("benchmark is empty, interrupted or contains duplicate case IDs")
    if any("measure" not in case or "server_resources" not in case
           or "chain_audit" not in case["measure"] for case in cases.values()):
        raise ValueError("benchmark contains unfinished cases")
    return cases


def number(value: float | None, divisor: float = 1) -> str:
    return "" if value is None else f"{value / divisor:.3f}"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("candidate", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--budget-mb", type=float, help="Optional explicit per-case server memory ceiling")
    args = parser.parse_args()
    if args.budget_mb is not None and (not math.isfinite(args.budget_mb) or args.budget_mb <= 0):
        parser.error("budget-mb must be positive and finite")
    baseline = json.loads(args.baseline.read_text(encoding="utf-8"))
    candidate = json.loads(args.candidate.read_text(encoding="utf-8"))
    if any(result.get("schema_version") != 5
           or result.get("response_validation") != "full-chain-credentials-token-paced-v3"
           or type(result.get("gateway_instruction_consistent")) is not bool
           or result.get("credential_protection", {}).get("enabled") is not True
           for result in (baseline, candidate)):
        parser.error("comparison requires authorization-only full-chain token-paced schema 5; older workload contracts are incomparable")
    for key in ("schema_version", "response_validation", "platform", "logical_cpus", "cpu_units", "memory_units",
                "tokenizer", "debug_observation", "ordinary_observation", "credential_protection"):
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
        fields.extend(f"{label}_{name}" for name in (*metrics, "successful_qps", "e2e_p95_ms", "ttft_p95_ms",
                                                     "queue_p95_ms", "errors", "requests", "full_chain_verified",
                                                     "verified_requests", "credential_verified_requests",
                                                     "input_tokens_mean", "images_mean", "output_tokens_mean"))
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
            row[f"{label}_requests"] = measurement["completed_requests"]
            audit = measurement["chain_audit"]
            row[f"{label}_full_chain_verified"] = audit["full_chain_verified"]
            row[f"{label}_verified_requests"] = audit["verified"]
            row[f"{label}_credential_verified_requests"] = sum(
                item.get("credential_protection", {}).get("verified") is True
                for item in audit["requests"])
            for name, key in (("input_tokens_mean", "input_tokens"), ("images_mean", "image_count"),
                              ("output_tokens_mean", "output_tokens")):
                requests = measurement["requests"]
                row[f"{label}_{name}"] = number(sum(request[key] for request in requests) / len(requests) if requests else None)
        rows.append(row)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("w", encoding="utf-8", newline="") as output:
        writer = csv.DictWriter(output, fieldnames=fields)
        writer.writeheader()
        writer.writerows(rows)
    budget_bytes = args.budget_mb * 1_000_000 if args.budget_mb is not None else None
    meets_memory = None if budget_bytes is None else all(
        case["server_resources"]["rss_bytes"]["max"] is not None
        and case["server_resources"]["rss_bytes"]["max"] < budget_bytes
        and (case["server_resources"]["private_commit_bytes"]["max"] is None
             or case["server_resources"]["private_commit_bytes"]["max"] < budget_bytes)
        for case in after.values()
    )
    meets_target = candidate["valid"] and all(case["measure"]["chain_audit"]["full_chain_verified"]
                                             for case in after.values()) and meets_memory is not False
    print(json.dumps({"output": str(args.output), "cases": len(rows), "baseline_sha256": baseline["binary_sha256"],
                      "candidate_sha256": candidate["binary_sha256"], "candidate_valid": candidate["valid"],
                      "memory_budget_mb": args.budget_mb, "memory_below_budget": meets_memory,
                      "meets_target": meets_target}))
    return 0 if meets_target else 1


if __name__ == "__main__":
    raise SystemExit(main())
