"""Isolated real HTTP/SSE/SQL measurement; no production or user data.

python -m tests.common.measure_observation_http --binary PATH --label baseline \
  --source-commit SHA --build-condition 'cargo build --locked -p stravia-server' \
  --web-dist ORIGINAL_WEBUI_DIST --browser-script BROWSER_MEASUREMENT_SCRIPT \
  --output PATH
Run exactly the same command/workload for the optimized binary, changing only
binary/label/source-commit/output. Measurements are not benchmark assertions.
"""
from __future__ import annotations

import argparse
import hashlib
import http.client
import json
import mimetypes
import re
import sqlite3
import subprocess
import tempfile
import threading
import time
from collections import Counter
from contextlib import closing
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlencode
from urllib.parse import urlsplit, unquote
from urllib.request import Request, urlopen

from tests.common.helpers import initialize_server, start_stravia_server, wait_for_setup_token
from tests.e2e.admin.test_observations import _create_route


class Provider(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args: object) -> None:
        pass

    def do_POST(self) -> None:  # noqa: N802
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        messages = body.get("messages", [])
        text = json.dumps(messages)
        if "synthetic-failure" in text:
            payload = b'{"error":{"message":"synthetic fixture failure"}}'
            self.send_response(502)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return
        if not body.get("stream"):
            payload = json.dumps({"id": "synthetic", "object": "chat.completion",
                                 "model": body["model"], "choices": [{"index": 0,
                                 "message": {"role": "assistant", "content": "synthetic reply"},
                                 "finish_reason": "stop"}],
                                 "usage": {"prompt_tokens": 10000, "completion_tokens": 12000}}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Connection", "close")
        self.end_headers()
        marker = "B" if "synthetic-B" in text else "A"
        emission = {"marker": marker, "first_chunk_at": None, "terminal_chunk_at": None}
        self.server.emissions.append(emission)
        try:
            for index in range(80):
                chunk = {"id": "synthetic", "object": "chat.completion.chunk", "model": body["model"],
                         "choices": [{"index": 0, "delta": {"content": marker + "文🙂" * 40},
                                      "finish_reason": None}]}
                self.wfile.write(b"data: " + json.dumps(chunk).encode() + b"\n\n")
                self.wfile.flush()
                if emission["first_chunk_at"] is None:
                    emission["first_chunk_at"] = time.time() * 1000
                # Identical provider pacing on both binaries. The first gap
                # allows four actual viewers to select the admitted scopes
                # before the steady-state revision burst begins.
                time.sleep(2.0 if index == 0 else 0.01)
            self.wfile.write(b'data: {"id":"synthetic","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10000,"completion_tokens":12000}}\n\ndata: [DONE]\n\n')
            self.wfile.flush()
            emission["terminal_chunk_at"] = time.time() * 1000
        except (BrokenPipeError, ConnectionResetError):
            pass
        self.close_connection = True

def web_proxy(base: str, directory: Path) -> ThreadingHTTPServer:
    """Static original build plus byte-forwarding reverse proxy; no API fixtures."""
    upstream = urlsplit(base)
    class Web(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"
        def log_message(self, *_args: object) -> None:
            pass
        def do_GET(self) -> None:  # noqa: N802
            self.dispatch()
        def do_POST(self) -> None:  # noqa: N802
            self.dispatch()
        def do_PUT(self) -> None:  # noqa: N802
            self.dispatch()
        def do_DELETE(self) -> None:  # noqa: N802
            self.dispatch()
        def dispatch(self) -> None:
            if self.path.startswith(("/api/", "/v1/")):
                connection = http.client.HTTPConnection(upstream.hostname, upstream.port, timeout=30)
                try:
                    body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
                    connection.request(self.command, self.path, body=body or None,
                                       headers={key: value for key, value in self.headers.items()
                                                if key.lower() not in ("connection", "transfer-encoding")})
                    response = connection.getresponse()
                    self.send_response(response.status)
                    for key, value in response.getheaders():
                        if key.lower() not in ("connection", "transfer-encoding"):
                            self.send_header(key, value)
                    self.send_header("Connection", "close")
                    self.end_headers()
                    while chunk := response.read1(65536):
                        self.wfile.write(chunk)
                        self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError, TimeoutError):
                    pass
                finally:
                    connection.close()
                    self.close_connection = True
                return
            requested = unquote(urlsplit(self.path).path).lstrip("/")
            asset = (directory / requested).resolve()
            if not asset.is_relative_to(directory.resolve()):
                self.send_error(404)
                return
            if not asset.is_file():
                asset = directory / "index.html"
            data = asset.read_bytes()
            self.send_response(200)
            self.send_header("Content-Type", mimetypes.guess_type(asset)[0] or "application/octet-stream")
            self.send_header("Content-Length", str(len(data)))
            self.send_header("Connection", "close")
            self.end_headers()
            try:
                self.wfile.write(data)
            except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
                # Browser navigation legitimately cancels superseded static assets.
                pass
            self.close_connection = True
    class StaticServer(ThreadingHTTPServer):
        # Four cold Chromium viewers request fonts/modules concurrently. The
        # stdlib's five-connection accept backlog is not a product limit.
        request_queue_size = 128
    server = StaticServer(("127.0.0.1", 0), Web)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


class Meter:
    def __init__(self, env: dict):
        self.env = env
        self.rows: list[dict] = []

    def request(self, path: str, payload: dict | None = None, *, proxy: bool = False) -> dict:
        data = None if payload is None else json.dumps(payload).encode()
        request = Request(self.env["admin"] + path, data=data,
                          headers=({"Authorization": "Bearer " + self.env["key"]} if proxy else self.env["auth"]) |
                          ({"Content-Type": "application/json"} if data else {}))
        start = time.perf_counter()
        started_at = time.time() * 1000
        try:
            with urlopen(request, timeout=30) as response:
                raw = response.read()
                status = response.status
        except Exception as error:
            if not hasattr(error, "read"):
                raise
            raw = error.read()
            status = error.code
        self.rows.append({"endpoint": path.split("?")[0], "method": request.get_method(),
                          "status": status, "response_bytes": len(raw), "request_bytes": len(data or b""),
                          "started_at": started_at, "finished_at": time.time() * 1000,
                          "service_ms": (time.perf_counter() - start) * 1000})
        if raw.startswith((b"{", b"[")):
            return json.loads(raw)
        return {"raw_bytes": len(raw), "status": status}

def members(page: dict) -> list[dict]:
    return [item for root in page["roots"] for item in root["interactions"]]

def max_depth(page: dict) -> int:
    parents = {item["id"]: item["parent_interaction_id"] for item in members(page)}
    deepest = 0
    for interaction_id in parents:
        seen = set()
        parent = parents[interaction_id]
        while parent is not None:
            if parent in seen:
                raise RuntimeError("interaction cycle in actual forest")
            seen.add(parent)
            parent = parents.get(parent)
        deepest = max(deepest, len(seen))
    return deepest


class Stream:
    def __init__(self, env: dict, path: str, *, slow: bool = False):
        self.rows: list[dict] = []
        self.bytes = 0
        self.error: str | None = None
        self.response = urlopen(Request(env["admin"] + path, headers=env["auth"]), timeout=3)
        self.start = time.perf_counter()
        self.thread = threading.Thread(target=self.read, args=(slow,), daemon=True)
        self.thread.start()

    def read(self, slow: bool) -> None:
        event, data, event_id = "", [], ""
        try:
            while True:
                raw = self.response.readline()
                if not raw:
                    return
                self.bytes += len(raw)
                line = raw.decode().rstrip("\r\n")
                if not line and data:
                    value = json.loads("\n".join(data))
                    self.rows.append({"event": event, "id": event_id, "data": value,
                                      "received_ms": (time.perf_counter() - self.start) * 1000})
                    event, data, event_id = "", [], ""
                    if slow:
                        time.sleep(0.05)  # Controlled consumer cadence, same on both binaries.
                elif line.startswith("event:"):
                    event = line[6:].strip()
                elif line.startswith("id:"):
                    event_id = line[3:].strip()
                elif line.startswith("data:"):
                    data.append(line[5:].strip())
        except Exception as error:
            self.error = type(error).__name__

    def close(self) -> dict:
        self.thread.join(timeout=4)
        self.response.close()
        return {"sse_bytes": self.bytes, "event_counts": dict(Counter(row["event"] for row in self.rows)),
                "events": self.rows, "read_end": self.error}


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--label", required=True)
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--build-condition", required=True)
    parser.add_argument("--optimized", action="store_true")
    parser.add_argument("--web-dist", type=Path, required=True)
    parser.add_argument("--browser-script", type=Path, required=True)
    parser.add_argument("--scenarios", nargs="+", choices=("no-detail", "long-body", "failure-page", "deep-root", "many-roots", "many-viewers", "slow-consumer", "reconnect"),
                        default=("no-detail", "long-body", "failure-page", "deep-root", "many-roots", "many-viewers", "slow-consumer", "reconnect"))
    args = parser.parse_args()
    provider = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    provider.emissions = []
    threading.Thread(target=provider.serve_forever, daemon=True).start()
    scenarios = []
    try:
        for scenario in args.scenarios:
            provider.emissions = []
            with tempfile.TemporaryDirectory(prefix="stravia-observation-measure-") as temporary:
                process, logs = start_stravia_server(stravia_binary=args.binary.resolve(), args=["--host", "127.0.0.1", "--port", "0", "--data-dir", temporary])
                web = None
                browser_process = None
                try:
                    token = wait_for_setup_token(logs, process)
                    address = next(re.search(r"Stravia startup listener opened .*?(127\.0\.0\.1:\d+)", line).group(1)
                                   for line in logs if re.search(r"Stravia startup listener opened .*?(127\.0\.0\.1:\d+)", line))
                    base = "http://" + address
                    session = initialize_server(base, token, {"backend": "sqlite"})
                    env = {"admin": base, "proxy": base, "auth": session.auth_headers(),
                           "mock": f"http://127.0.0.1:{provider.server_port}"}
                    route, key = _create_route(env, "measurement")
                    env["key"] = key
                    meter = Meter(env)
                    # Debug setting uses PUT and enables the existing real SQL recorder.
                    with urlopen(Request(base + "/api/v1/observations/debug", data=b'{"enabled":true,"confirmed":true}',
                                         headers=env["auth"] | {"Content-Type": "application/json"}, method="PUT")) as response:
                        response.read()
                    filters = {"anchor_at": int(time.time() * 1000) + 60000, "window_index": 0, "min_tokens": 0}
                    forest_path = "/api/v1/observations/interactions?" + urlencode(filters)
                    history = None
                    if scenario == "deep-root":
                        history = [{"role": "user", "content": "synthetic-deep-0 " + "fixture context " * 4000}]
                        for index in range(12):
                            result = meter.request("/v1/chat/completions", {"model": "measurement", "messages": history}, proxy=True)
                            history += [result["choices"][0]["message"], {"role": "user", "content": f"synthetic-deep-{index + 1}"}]
                            # A real distinct user Interaction must be outside the
                            # existing rapid-continuation window, on both versions.
                            time.sleep(2.1)
                    initial = meter.request(forest_path)["data"]
                    if scenario == "deep-root" and max_depth(initial) < 11:
                        raise RuntimeError("fixture did not form a genuine twelve-node interaction root")
                    count = 4 if scenario == "many-viewers" else 1
                    browser_output = args.output.resolve().with_suffix(f".{scenario}.browser.json")
                    if args.web_dist and args.browser_script:
                        web = web_proxy(base, args.web_dist.resolve())
                        browser_process = subprocess.Popen(["bun", str(args.browser_script.resolve())],
                            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, encoding="utf-8")
                        browser_process.stdin.write(json.dumps({"origin": f"http://127.0.0.1:{web.server_port}",
                            "password": "correct horse battery staple", "viewers": count, "scenario": scenario,
                            "output": str(browser_output)}) + "\n")
                        browser_process.stdin.flush()
                        if browser_process.stdout.readline().strip() != "ready":
                            raise RuntimeError("Chromium startup failed: " + browser_process.stderr.read())
                    streams = [Stream(env, "/api/v1/observations/events?after=" + str(initial["snapshot_sequence"]), slow=scenario == "slow-consumer") for _ in range(count)]
                    jobs: list[threading.Thread] = []
                    outcomes: list[dict] = []
                    def invoke(index: int, messages: list | None = None, stream: bool = True) -> None:
                        outcomes.append(meter.request("/v1/chat/completions", {"model": "measurement", "stream": stream,
                            "messages": messages or [{"role": "user", "content": ("synthetic-failure" if scenario == "failure-page" else f"synthetic-{'B' if index % 2 else 'A'}-{index}") + " fixture context " * 4000}]}, proxy=True))
                    for index in range(8 if scenario == "many-roots" else 2):
                        job = threading.Thread(target=invoke, args=(index, history if index == 0 else None))
                        job.start()
                        jobs.append(job)
                    # Discover actual admitted interactions using the ordinary product forest.
                    deadline = time.monotonic() + 10
                    forest = {"roots": []}
                    selected = []
                    while scenario != "failure-page" and len(selected) < 2 and time.monotonic() < deadline:
                        forest = meter.request(forest_path)["data"]
                        selected = [item for item in members(forest)
                                    if item["last_event_sequence"] > initial["snapshot_sequence"] and item.get("input_preview")]
                        if len(selected) < 2:
                            time.sleep(0.01)  # Same explicit discovery cadence for both binaries.
                    if len(selected) < 2 and scenario != "failure-page":
                        raise RuntimeError("two admitted interactions with current input identity were not discovered")
                    if browser_process:
                        browser_process.stdin.write(json.dumps({"interactions": [item["id"] for item in selected],
                            "markers": ["B" if "synthetic-B" in (item.get("input_preview") or "") else "A" for item in selected]}) + "\n")
                        browser_process.stdin.flush()
                    scoped = []
                    if args.optimized and scenario not in ("no-detail", "failure-page"):
                        for index in range(count):
                            interaction = selected[index % len(selected)]
                            scoped.append(Stream(env, f"/api/v1/observations/interactions/{interaction['id']}/live", slow=scenario == "slow-consumer"))
                    for job in jobs:
                        job.join(timeout=30)
                        if job.is_alive():
                            raise RuntimeError("inference did not finish")
                    if browser_process:
                        browser_process.stdin.write('{"done":true}\n')
                        browser_process.stdin.flush()
                        stdout, stderr = browser_process.communicate(timeout=60)
                        if browser_process.returncode != 0:
                            raise RuntimeError("Chromium measurement failed: " + stderr)
                    final = meter.request(forest_path)["data"]
                    terminal_deadline = time.monotonic() + 10
                    while any(item["status"] == "running" for item in members(final)):
                        if time.monotonic() >= terminal_deadline:
                            raise RuntimeError("persisted terminal observation did not become visible")
                        time.sleep(0.01)
                        final = meter.request(forest_path)["data"]
                    if scenario == "failure-page":
                        meter.request("/api/v1/observations/failed-requests?" + urlencode(filters))
                    if args.optimized:
                        known = members(initial)
                        roots = [{"root_id": root, "after_sequence": initial["snapshot_sequence"],
                                  "known_interactions": [{"id": item["id"], "last_event_sequence": item["last_event_sequence"],
                                                          "matched": item["matched"], "debug_status": item["debug_status"]}
                                                         for item in known if item["root_id"] == root]}
                                 for root in sorted({item["root_id"] for item in members(final)})]
                        meter.request("/api/v1/observations/interactions/changes", {"filters": filters, "roots": roots})
                    else:
                        for item in members(final):
                            if item["last_event_sequence"] <= initial["snapshot_sequence"]:
                                continue
                            meter.request(f"/api/v1/observations/interactions/{item['id']}/summary?" + urlencode(filters))
                    if scenario == "reconnect":
                        streams.append(Stream(env, "/api/v1/observations/events?after=" + str(initial["snapshot_sequence"])))
                    stream_results = [stream.close() for stream in streams + scoped]
                    timeline = meter.request("/api/v1/performance/timeline")
                    with urlopen(Request(base + "/api/v1/performance/metrics", headers=env["auth"])) as response:
                        metrics = response.read().decode()
                    database = Path(temporary) / "db" / "gateway.db"
                    with closing(sqlite3.connect(f"file:{database.as_posix()}?mode=ro", uri=True)) as connection:
                        stored_events = dict(connection.execute(
                            "SELECT kind, COUNT(*) FROM observation_events GROUP BY kind").fetchall())
                        stored_runs = connection.execute("SELECT COUNT(*) FROM inference_run_observations").fetchone()[0]
                        stored_interactions = connection.execute("SELECT COUNT(*) FROM interaction_observations").fetchone()[0]
                    scenarios.append({"scenario": scenario, "http": meter.rows, "streams": stream_results,
                                      "actual_interactions": len(members(final)), "root_total": final.get("root_total"),
                                      "actual_max_depth": max_depth(final),
                                      "fixture_inventory": {"stored_events_by_kind": stored_events,
                                          "stored_runs": stored_runs, "stored_interactions": stored_interactions,
                                          "inspection": "three external read-only SQLite inventory queries after workload; excluded from Gateway recorder"},
                                      "upstream_emissions": provider.emissions,
                                      "timeline": timeline, "metrics": metrics,
                                      "browser": json.loads(browser_output.read_text(encoding="utf-8")) if browser_process else None})
                    partial = args.output.with_suffix(f".{scenario}.json")
                    partial.parent.mkdir(parents=True, exist_ok=True)
                    serialized = json.dumps({"source_commit": args.source_commit, "build_condition": args.build_condition,
                        "binary_sha256": hashlib.sha256(args.binary.read_bytes()).hexdigest(), **scenarios[-1]},
                        ensure_ascii=False, indent=2)
                    private_values = [token, key, "upstream-secret", "correct horse battery staple"]
                    for name, value in env["auth"].items():
                        if len(value) >= 16:
                            private_values.append(value)
                        if name.lower() == "cookie":
                            private_values.extend(part.partition("=")[2].strip() for part in value.split(";")
                                                  if len(part.partition("=")[2].strip()) >= 16)
                    if any(value in serialized for value in private_values if value):
                        raise RuntimeError("measurement attempted to export a private credential/session value")
                    partial.write_text(serialized, encoding="utf-8")
                finally:
                    if browser_process and browser_process.poll() is None:
                        browser_process.kill()
                        browser_process.wait(timeout=10)
                    if web:
                        web.shutdown()
                        web.server_close()
                    process.terminate()
                    process.wait(timeout=10)
    finally:
        provider.shutdown()
        provider.server_close()
    result = {"label": args.label, "source_commit": args.source_commit, "build_condition": args.build_condition,
              "binary_sha256": hashlib.sha256(args.binary.read_bytes()).hexdigest(),
              "workload": {"stream_chunks": 80, "chunk_text": "marker + 文🙂*40", "upstream_cadence_ms": 10,
                           "upstream_first_gap_ms": 2000,
                           "slow_consumer_ms": 50, "deep_followups": 12, "many_roots_requests": 8, "viewers": 4,
                           "input_context_repetitions": 4000, "provider_reported_prompt_tokens": 10000, "provider_reported_completion_tokens": 12000,
                           "deep_followup_cadence_ms": 2100, "static_proxy_connection_backlog": 128},
              "limitations": ["Explicit HTTP client and actual Chromium request counts are reported separately; do not combine harness reads with WebUI refresh amplification",
                               "HTTP bytes are decoded body/SSE bytes, not TCP/TLS overhead", "SQLite only; isolated PostgreSQL not configured",
                               "Process CPU/RSS/SQL are only existing recorder samples; no allocation instrumentation",
                               "Snapshots/summary/changes client is explicit, not a substitute for actual WebUI orchestration",
                               "SSE receive delay includes real service and transport; active scheduling components cannot be isolated here"],
              "scenarios": scenarios}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps({"output": str(args.output), "scenarios": len(scenarios), "binary_sha256": result["binary_sha256"]}))


if __name__ == "__main__":
    main()
