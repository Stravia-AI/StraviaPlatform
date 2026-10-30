"""Real admin text-detection benchmark, isolated from production services.

Run with ``python -m tests.common.measure_credential_detection --binary PATH
--output result.json --label default``. Optional ``--cache-budget-bytes 262144``
requires an instrumented experimental binary that confirms the applied budget;
the harness rejects unconfirmed budgets instead of silently mislabeling results.
The budget is passed only to the child Server. Default workload: deterministic 50k/100k/
150k cl100k_base tokens, serial and concurrency four, one warmup and three warm
repetitions per input/mode. Cold setup and first detection are separate phases.
"""
from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
from http.server import ThreadingHTTPServer
import json
import os
from pathlib import Path
import random
import re
import sys
import tempfile
import threading
import time

from tests.common.helpers import (
    http_request, initialize_server, start_stravia_server, stop_stravia_server,
    wait_for_setup_token,
)
from tests.common.measure_protocol_http import Catalog, ProcessReader
from tests.common.protocol_workload import Workload

ENDPOINT = "/api/v1/reversible-redaction/test"
FIELDS = ("rule_id", "start", "end", "start_line", "start_column", "end_line", "end_column")
EXPECTED_RULES = {
    "github_unicode_repeat": {"github-pat"},
    "generic_assignment": {"generic-api-key"},
    "composite_positive": {"aikido-client-id", "aikido-client-secret"},
    "multiline_positive": {"private-key"},
    "bittrex_duplicate": {"bittrex-access-key"},
}


def digest(value):
    return hashlib.sha256(json.dumps(value, ensure_ascii=False, separators=(",", ":"),
                                     sort_keys=True).encode()).hexdigest()


def corpus():
    workload = Workload(seed=711, min_images=0, max_images=0, image_pool_size=1, image_size=2)
    rng = random.Random(711)
    pat = "ghp_" + "".join(rng.choices("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789", k=36))
    # Composite, entropy filter, generic assignment and multiline fixtures from
    # test_bundled_rules_apply_composites_filters_and_multiline_secrets.
    client_id = "AIK_CLIENT_e7fb9d1bb335069c097c2a02"
    client_secret = "AIK_SECRET_" + "0123456789abcdef" * 4
    private_key = ("-----BEGIN PRIVATE KEY-----\n"
                   "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4vMDEyMzQ1Njc4OTo7PD0+P0BBQkNERUZH\n"
                   "-----END PRIVATE KEY-----")
    probes = {
        "github_unicode_repeat": f"中文😀\n前缀é {pat}\n重复🧪 {pat}\n",
        "generic_assignment": "api_key=Q8n4Vk7sT2p9X5a3Lc6D0h1R\nimport { x } from 'pkg'\n",
        "composite_positive": f"{client_id}\n{client_secret}\n",
        "multiline_positive": f"before\n{private_key}\nafter\n",
        "composite_orphan_negative": client_secret + "\n",
        "entropy_negative": f"{client_id}\nAIK_SECRET_{'A' * 64}\n",
        "ordinary_negative": "api_key=${SYNTHETIC_KEY}; ordinary prose, not a credential\n",
        "reference_boundaries": "中文😀 <!--sr:abcdefghijklmnopqrstuvwxyzab--> é\n<!--sr:abc--> ordinary\n",
        "bittrex_duplicate": "中文😀 bittrex=a7b2c9d4e0f6g3h8i1j5k9l2m4n6o8p0\nbittrex=a7b2c9d4e0f6g3h8i1j5k9l2m4n6o8p0",
    }
    inputs = [(name, text) for name, text in probes.items()]
    prefix = "".join(probes.values())
    for target in (50_000, 100_000, 150_000):
        # Vocabulary words individually roundtrip as one token; count the complete
        # text, including all Unicode/probe/reference boundaries, not an estimate.
        text = prefix + "".join(rng.choices(workload.vocabulary, k=target - workload.count(prefix)))
        actual = workload.count(text)
        if actual != target:
            raise RuntimeError("deterministic filler token boundary mismatch")
        inputs.append((f"text_{target}", text))
    return [(name, text, {"id": name, "sha256": hashlib.sha256(text.encode()).hexdigest(),
                         "tokens": workload.count(text), "utf8_bytes": len(text.encode())})
            for name, text in inputs]


class Measurement:
    """Cumulative CPU endpoints are independent of periodic RSS samples."""
    def __init__(self, reader, interval):
        self.reader, self.interval = reader, interval

    def run(self, name, action, rows):
        before = self.reader.read()
        samples = [before["rss_bytes"]]
        errors = []
        stop = threading.Event()
        def sample():
            while not stop.wait(self.interval):
                try:
                    samples.append(self.reader.read()["rss_bytes"])
                except Exception as error:
                    errors.append(type(error).__name__)
                    return
        thread = threading.Thread(target=sample, daemon=True)
        thread.start()
        started = time.perf_counter()
        row = {"phase": name, "cpu_before_seconds": before["cpu_seconds"]}
        rows.append(row)
        try:
            return action()
        finally:
            ended = time.perf_counter()
            stop.set()
            thread.join()
            after = self.reader.read()
            samples.append(after["rss_bytes"])
            row.update(wall_seconds=ended-started, cpu_after_seconds=after["cpu_seconds"],
                       cpu_seconds=after["cpu_seconds"]-before["cpu_seconds"],
                       rss_peak_sampled_bytes=max(samples), rss_end_bytes=after["rss_bytes"],
                       rss_samples=len(samples), sampling_errors=errors)
            if errors:
                raise RuntimeError("Server RSS sampling failed")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--label", required=True)
    parser.add_argument("--cache-budget-bytes", type=int, default=None)
    parser.add_argument("--concurrency", type=int, nargs="+", choices=(1, 4), default=[1, 4])
    parser.add_argument("--warmup", type=int, default=1)
    parser.add_argument("--repeat", type=int, default=3)
    parser.add_argument("--sample-interval", type=float, default=0.02)
    parser.add_argument("--timeout", type=float, default=300)
    args = parser.parse_args()
    if args.repeat < 2 or args.warmup < 0 or args.sample_interval <= 0 or args.timeout <= 0:
        parser.error("repeat >= 2, warmup >= 0, sample interval and timeout > 0 required")
    if args.cache_budget_bytes is not None and args.cache_budget_bytes <= 0:
        parser.error("cache budget must be positive; omit it for the default")
    if "STRAVIA_BENCH_DFA_CACHE_BYTES" in os.environ:
        parser.error("unset STRAVIA_BENCH_DFA_CACHE_BYTES in the harness environment; use --cache-budget-bytes")
    result = {"schema_version": 1, "label": args.label, "budget_bytes": args.cache_budget_bytes,
              "server_pid": None, "binary_sha256": None, "inputs": [], "phases": [],
              "requests": [], "fatal_error": None, "success": False,
              "configuration": {"concurrency": args.concurrency, "warmup": args.warmup,
                                "repeat": args.repeat, "sample_interval": args.sample_interval},
              "tokenizer": "tiktoken/cl100k_base", "cpu_scope": "Server PID cumulative user+kernel seconds",
              "memory_scope": "Server PID RSS only; peak is sampled, not an exact high-water mark",
              "history_and_observation": "unchanged; no history/observation settings disabled"}
    process = reader = catalog = None
    logs = []
    try:
        binary = args.binary.resolve(strict=True)
        with binary.open("rb") as source:
            result["binary_sha256"] = hashlib.file_digest(source, "sha256").hexdigest()
        inputs = corpus()
        result["inputs"] = [metadata for _, _, metadata in inputs]
        catalog = ThreadingHTTPServer(("127.0.0.1", 0), Catalog)
        catalog.arrivals = []
        threading.Thread(target=catalog.serve_forever, daemon=True).start()
        with tempfile.TemporaryDirectory(prefix="stravia-detector-bench-") as directory:
            try:
                environment = {"RUST_LOG": "warn,stravia_server=info"}
                if args.cache_budget_bytes is not None:
                    environment["STRAVIA_BENCH_DFA_CACHE_BYTES"] = str(args.cache_budget_bytes)
                launch = time.perf_counter()
                process, logs = start_stravia_server(stravia_binary=binary, args=[
                    "--host", "127.0.0.1", "--port", "0", "--data-dir", directory,
                    "--test-catalog-base-url", f"http://127.0.0.1:{catalog.server_port}"], env=environment)
                result["server_pid"] = process.pid
                reader = ProcessReader(process.pid)
                measurement = Measurement(reader, args.sample_interval)
                def setup():
                    token = wait_for_setup_token(logs, process, timeout=args.timeout)
                    listener = next(re.search(r"Stravia startup listener opened .*?(127\.0\.0\.1:\d+)", line).group(1)
                                    for line in logs if re.search(r"Stravia startup listener opened .*?(127\.0\.0\.1:\d+)", line))
                    return initialize_server("http://" + listener, token, {"backend": "sqlite"})
                session = measurement.run("cold_startup_setup", setup, result["phases"])
                result["launch_to_setup_wall_seconds"] = time.perf_counter() - launch
                result["startup_cpu_before_first_sample_seconds"] = result["phases"][0]["cpu_before_seconds"]
                result["startup_cpu_since_process_creation_seconds"] = result["phases"][0]["cpu_after_seconds"]
                auth = session.auth_headers()
                canonical = {}
                def request(item, phase, concurrency):
                    name, text, _metadata = item
                    started = time.perf_counter()
                    row = {"input": name, "phase": phase, "concurrency": concurrency,
                           "http_status": None}
                    result["requests"].append(row)
                    try:
                        status, body = http_request("POST", session.origin + ENDPOINT,
                                                    payload={"text": text}, headers=auth, timeout=args.timeout)
                    except Exception as error:
                        row["failure"] = type(error).__name__
                        raise
                    finally:
                        row["wall_seconds"] = time.perf_counter() - started
                    row["http_status"] = status
                    if status != 200:
                        raise RuntimeError(f"diagnostic HTTP {status}")
                    matches = body["data"]["matches"]
                    normalized = sorted([[match[field] for field in FIELDS] for match in matches])
                    if not EXPECTED_RULES.get(name, set()).issubset({match[0] for match in normalized}):
                        raise RuntimeError("positive probe was not detected")
                    if (name.endswith("_negative") or name == "reference_boundaries") and normalized:
                        raise RuntimeError("negative probe produced a detection")
                    row["matches_sha256"] = digest(normalized)
                    row["match_count"] = len(normalized)
                    return name, normalized
                def batch(items, phase, concurrency):
                    with ThreadPoolExecutor(max_workers=concurrency) as pool:
                        values = list(pool.map(lambda item: request(item, phase, concurrency), items))
                    for name, normalized in values:
                        if name in canonical and canonical[name] != normalized:
                            raise RuntimeError("diagnostic result changed across repetitions")
                        canonical[name] = normalized
                # No diagnostic request precedes this phase: lazy rules compilation
                # and first detection are deliberately excluded from warm runs.
                measurement.run("cold_first_detection", lambda: batch([inputs[-1]], "cold_first_detection", 1), result["phases"])
                applied = {
                    int(match.group(1))
                    for line in logs
                    if (match := re.search(r"STRAVIA_BENCH_DFA_CACHE_APPLIED=(\d+)", line))
                }
                result["confirmed_experiment_budget_bytes"] = next(iter(applied)) if len(applied) == 1 else None
                if args.cache_budget_bytes is not None and applied != {args.cache_budget_bytes}:
                    result["failure_reason"] = "cache_budget_not_confirmed"
                    raise RuntimeError("binary did not confirm the experimental cache budget")
                measurement.run("cold_remaining_inputs", lambda: batch(inputs, "cold_remaining_inputs", 1), result["phases"])
                for concurrency in args.concurrency:
                    for phase, repeats in (("warmup", args.warmup), ("warm", args.repeat)):
                        for repeat in range(repeats):
                            name = f"{phase}_c{concurrency}_r{repeat+1}"
                            # Exercise same-input concurrent pressure, rather
                            # than only scheduling unrelated small probes together.
                            items = [item for item in inputs for _ in range(concurrency)]
                            measurement.run(name, lambda: batch(items, name, concurrency), result["phases"])
                for metadata in result["inputs"]:
                    matches = canonical[metadata["id"]]
                    metadata.update(match_fields=list(FIELDS), matches=matches, matches_sha256=digest(matches))
                result["corpus_sha256"] = digest([(m["id"], m["sha256"]) for m in result["inputs"]])
                result["detection_sha256"] = digest([(m["id"], m["matches"]) for m in result["inputs"]])
                status, body = http_request("GET", session.origin + "/api/v1/reversible-redaction/rules",
                                            headers=auth, timeout=args.timeout)
                if status != 200:
                    raise RuntimeError(f"catalog HTTP {status}")
                result["catalog_rule_ids"] = [rule["id"] for rule in body["data"]["rules"]]
                if len(set(result["catalog_rule_ids"])) != len(result["catalog_rule_ids"]):
                    raise RuntimeError("catalog contains duplicate identities")
                result["success"] = True
            finally:
                if reader:
                    reader.close()
                if process:
                    stop_stravia_server(process, logs, print_tail=0)
    except Exception as error:
        # Never serialize response bodies, exceptions containing request text,
        # Server logs, or synthetic fixture values.
        result["fatal_error"] = {"type": type(error).__name__, "message": "benchmark failed; inspect phase/status metadata"}
    finally:
        if catalog:
            catalog.shutdown()
            catalog.server_close()
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"output": str(args.output), "success": result["success"],
                      "server_pid": result["server_pid"]}))
    return 0 if result["success"] else 1


if __name__ == "__main__":
    sys.exit(main())
