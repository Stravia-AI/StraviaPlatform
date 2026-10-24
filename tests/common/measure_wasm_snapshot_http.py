"""在无外网的 Linux network namespace 内运行真实数据库副本 HTTP 回放。

数据库、原始正文及日志仅写入 --private-root；--output 只包含匿名测量。
该驱动不提供网络隔离降级路径，不使用生产凭据，也不把 HTTP 200 等同于语义等价。
"""
from __future__ import annotations

import argparse
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
import shutil
import socket
import sqlite3
import subprocess
import threading
import time
from urllib.parse import urlsplit

from tests.common.helpers import start_stravia_server, stop_stravia_server, wait_until_ready
from tests.common.measure_protocol_http import Catalog, ProcessReader


def private_json(path: Path, data: object) -> None:
    path.write_text(json.dumps(data, ensure_ascii=False, indent=2), encoding="utf-8")
    path.chmod(0o600)


def interfaces() -> set[str]:
    return {name for _, name in socket.if_nameindex()}


class Replay(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args: object) -> None:
        pass

    def do_POST(self) -> None:  # noqa: N802
        length = int(self.headers.get("content-length", "0"))
        body = self.rfile.read(length)
        with self.server.replay_lock:
            position = self.server.position
            groups = self.server.groups
            recorded = position < len(groups) and urlsplit(self.path).path == groups[position][0]["request_path"]
            self.server.received.append({
                "bytes": len(body), "sha256": hashlib.sha256(body).hexdigest(), "recorded": recorded,
            })
            if recorded:
                self.server.position += 1
        if not recorded:
            self.send_error(502, "offline response unavailable for this request path")
            return
        chunks = groups[position]
        self.send_response(chunks[0]["status"])
        headers = dict(chunks[0]["headers"])
        self.send_header("content-type", headers.get("content-type", "text/event-stream"))
        self.send_header("transfer-encoding", "chunked")
        self.send_header("connection", "close")
        self.end_headers()
        try:
            for chunk in chunks:
                value = Path(chunk["file"]).read_bytes()
                self.wfile.write(f"{len(value):x}\r\n".encode() + value + b"\r\n")
                self.wfile.flush()
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            with self.server.replay_lock:
                self.server.disconnects += 1
        self.close_connection = True


def terminal_response(body: bytes) -> dict | None:
    terminal = None
    for frame in body.decode("utf-8").replace("\r\n", "\n").split("\n\n"):
        lines = [line[6:] for line in frame.splitlines() if line.startswith("data: ")]
        if not lines or lines == ["[DONE]"]:
            continue
        try:
            event = json.loads("\n".join(lines))
        except json.JSONDecodeError:
            continue
        if event.get("type") in {"response.completed", "response.incomplete"}:
            terminal = event.get("response")
    return terminal


def canonical_output(value: dict) -> list:
    units = []
    for item in value["items"]:
        content = item["content"]
        if isinstance(content, str):
            if content:
                units.append(["text", content])
        elif isinstance(content, list):
            for block in content:
                if block.get("type") == "reasoning":
                    if set(block) - {"type", "content", "summary"}:
                        raise ValueError("unsupported_protected_reasoning")
                    reasoning, summary = block.get("content", []), block.get("summary", [])
                    if not all(isinstance(text, str) for text in [*reasoning, *summary]):
                        raise ValueError("unsupported_canonical_reasoning")
                    units.append(["reasoning", reasoning, summary])
                elif block.get("type") == "text":
                    units.append(["text", block["text"]])
                else:
                    raise ValueError("unsupported_canonical_output")
        else:
            raise ValueError("unsupported_canonical_output")
        for call in item.get("tool_calls") or []:
            units.append(["tool_call", call["id"], call["name"], call["arguments"]])
    return units


def response_output(value: dict) -> list:
    units = []
    for item in value["output"]:
        if item["type"] == "function_call":
            units.append(["tool_call", item["call_id"], item["name"], item["arguments"]])
        elif item["type"] == "message":
            for block in item["content"]:
                if block["type"] != "output_text":
                    raise ValueError("unsupported_responses_output")
                units.append(["text", block["text"]])
        elif item["type"] == "reasoning":
            if set(item) - {"type", "id", "content", "summary"}:
                raise ValueError("unsupported_protected_reasoning")
            content, summary = item.get("content", []), item.get("summary", [])
            if any(part.get("type") != "reasoning_text" for part in content) or any(
                part.get("type") != "summary_text" for part in summary
            ):
                raise ValueError("unsupported_responses_reasoning")
            units.append(["reasoning", [part["text"] for part in content], [part["text"] for part in summary]])
        else:
            raise ValueError("unsupported_responses_output")
    return units


def comparable_output(units: list) -> tuple[list, int]:
    # 每次执行重新分配 History Marker 引用；只重命名身份，保留正文、引用关系、
    # 投影模式、序号和边界。这不是 Marker 授权或隐藏载荷完整性校验。
    marker = re.compile(r"<!--(?P<kind>sh|sp):(?P<id>[A-Za-z]{28})(?P<tail>(?::[trp]:[0-9]+:[se])?)-->")
    references: dict[str, str] = {}

    def normalize(value: object) -> object:
        if isinstance(value, list):
            return [normalize(item) for item in value]
        if not isinstance(value, str):
            return value

        def replace(match: re.Match[str]) -> str:
            identity = match["id"]
            references.setdefault(identity, f"marker_{len(references)}")
            return f"<!--{match['kind']}:{references[identity]}{match['tail']}-->"

        return marker.sub(replace, value)

    return normalize(units), len(references)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--database", type=Path, required=True)
    parser.add_argument("--corpus", type=Path, required=True)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--private-root", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--cases", nargs="+", default=["case-05"])
    parser.add_argument("--rounds", type=int, default=2)
    parser.add_argument("--trusted-cache-from", type=Path)
    parser.add_argument("--artifacts", type=Path, help="already isolated content-addressed artifact directory")
    args = parser.parse_args()
    os.umask(0o077)
    if interfaces() != {"lo"} or os.geteuid() == 0:
        raise RuntimeError("requires unprivileged process in loopback-only network namespace")
    if args.private_root.exists():
        raise RuntimeError("private-root must be a new directory")
    args.private_root.mkdir(mode=0o700, parents=True)
    data = args.private_root / "data"
    (data / "db").mkdir(parents=True, mode=0o700)
    shutil.copyfile(args.database, data / "db" / "gateway.db")
    (data / "server.toml").write_text('[database]\nbackend = "sqlite"\n', encoding="utf-8")
    if args.trusted_cache_from is not None:
        shutil.copytree(args.trusted_cache_from, data / "cache" / "wasmtime")
    if args.artifacts is not None:
        shutil.copytree(args.artifacts, data / "artifacts")
    corpus = json.loads(args.corpus.read_text())
    cases = {case["id"]: case for case in corpus["cases"]}
    selected = [cases[label] for label in args.cases]
    upstream = ThreadingHTTPServer(("127.0.0.1", 0), Replay)
    upstream.replay_lock = threading.Lock()
    upstream.groups, upstream.received, upstream.position, upstream.disconnects = [], [], 0, 0
    catalog = ThreadingHTTPServer(("127.0.0.1", 0), Catalog)
    catalog.arrivals = []
    for server in [upstream, catalog]:
        threading.Thread(target=server.serve_forever, daemon=True).start()
    local_base = f"http://127.0.0.1:{upstream.server_port}"
    db = sqlite3.connect(data / "db" / "gateway.db")
    db.row_factory = sqlite3.Row
    db.execute("UPDATE api_keys SET token='offline-disabled-'||id, rpm_limit=NULL")
    db.execute("UPDATE providers SET is_enabled=0,api_key='offline-token',access_token='offline-token',refresh_token=NULL,expires_at=4102444800,use_proxy=0,adapter_credentials='{}'")
    db.execute("UPDATE provider_oauth_credentials SET access_token='offline-token',refresh_token=NULL,expires_at=4102444800,resource_url=?,subject_id='offline-subject',meta='{}',last_error=NULL", (local_base,))
    db.execute("DELETE FROM admin_sessions")
    db.execute("DELETE FROM provider_allowance_guards")
    db.execute("DELETE FROM provider_allowance_suspensions")
    db.execute("UPDATE settings SET value='null' WHERE name='outbound_proxy'")
    db.commit()
    db.close()
    # Copy only the already isolated active artifact files; never read the production directory.
    artifact_root = args.corpus.parent / "production-plugins"
    artifact_destination = data / "plugins" / "artifacts"
    artifact_destination.mkdir(parents=True, mode=0o700)
    for source in artifact_root.glob("*.wasm"):
        if source.is_symlink() or hashlib.sha256(source.read_bytes()).hexdigest() != source.stem:
            raise RuntimeError("invalid isolated component artifact")
        shutil.copyfile(source, artifact_destination / source.name)
    result = {"kind": "database-copy-http-smoke", "network_interfaces": ["lo"],
              "timing": "unpaced recorded response; not upstream or production latency", "cases": []}
    process, logs = None, []
    try:
        import http.client
        for case in selected:
            directory = Path(case["request"]).parent
            context = json.loads((directory / "http-context-private.json").read_text())
            raw_request = (directory / "captured-client-request.bin").read_bytes()
            chunks = json.loads(Path(case["chunks"]).read_text())
            if not raw_request or not chunks or any(chunk["kind"] != "http" for chunk in chunks):
                result["cases"].append({"id": case["id"], "vendor": case["vendor"], "available": False})
                continue
            provider = json.loads(Path(case["provider"]).read_text())
            groups = {}
            for chunk in chunks:
                if not isinstance(chunk.get("request_path"), str) or not chunk["request_path"].startswith("/"):
                    raise RuntimeError("recorded HTTP request path missing; prepare fixtures again")
                groups.setdefault(chunk["exchange"], []).append(chunk)
            if process is not None:
                stop_stravia_server(process, logs, print_tail=0)
                (args.private_root / (case["id"] + "-server.log")).write_text("\n".join(logs))
                process = None
            db = sqlite3.connect(data / "db" / "gateway.db")
            db.execute("UPDATE providers SET is_enabled=0")
            base_path = urlsplit(provider["base_url"]).path
            db.execute("UPDATE providers SET is_enabled=1,base_url=?,adapter_credentials=? WHERE id=?",
                       (local_base + base_path, json.dumps(provider["credentials"]), context["provider_id"]))
            credentials = provider["credentials"]
            db.execute("UPDATE provider_oauth_credentials SET meta=?,access_token=?,refresh_token=?,resource_url=?,subject_id=?,status='valid' WHERE provider_id=?",
                       (json.dumps(credentials), credentials.get("access_token", "offline-token"),
                        credentials.get("refresh_token"), credentials.get("resource_url"),
                        credentials.get("subject_id"), context["provider_id"]))
            token = "offline-local-evaluation-key"
            db.execute("UPDATE api_keys SET token=?,is_enabled=1,expires_at=NULL WHERE id=?", (token, context["api_key_id"]))
            db.commit()
            db.close()
            process, logs = start_stravia_server(stravia_binary=args.binary.resolve(),
                args=["--host", "127.0.0.1", "--port", "18081", "--data-dir", str(data.resolve()),
                      "--test-catalog-base-url", f"http://127.0.0.1:{catalog.server_port}"],
                env={"RUST_LOG": "warn,stravia_server=info", "HTTP_PROXY": "", "HTTPS_PROXY": "", "ALL_PROXY": ""})
            base = "http://127.0.0.1:18081"
            try:
                wait_until_ready(base + "/readyz", timeout=180)
            except (TimeoutError, OSError):
                result["cases"].append({"id": case["id"], "vendor": case["vendor"], "error": "database_not_ready"})
                continue
            reader = ProcessReader(process.pid)
            for round_index in range(args.rounds):
                with upstream.replay_lock:
                    upstream.groups, upstream.position, upstream.received, upstream.disconnects = list(groups.values()), 0, [], 0
                before = reader.read()
                stop = threading.Event()
                samples = []
                def sample() -> None:
                    while not stop.is_set():
                        try:
                            samples.append(reader.read())
                        except (OSError, ProcessLookupError):
                            return
                        stop.wait(0.01)
                thread = threading.Thread(target=sample, daemon=True)
                thread.start()
                started = time.perf_counter()
                row = {"id": case["id"], "vendor": case["vendor"], "round": round_index,
                       "request_bytes": len(raw_request)}
                try:
                    connection = http.client.HTTPConnection("127.0.0.1", int(urlsplit(base).port), timeout=120)
                    connection.request("POST", context["path"], body=raw_request,
                                       headers={"authorization": "Bearer " + token, "content-type": "application/json"})
                    response = connection.getresponse()
                    row["status"] = response.status
                    body = response.read()
                    row.update(response_bytes=len(body), response_sha256=hashlib.sha256(body).hexdigest())
                    terminal = terminal_response(body)
                    row["output_matches_history"] = False
                    row["comparison_scope"] = "terminal status, ordered text, plaintext reasoning and tool calls including call IDs; fresh History Marker identities consistently renamed; excludes usage, metadata, echoed controls, marker authorization and hidden payload integrity"
                    if terminal is not None:
                        try:
                            recorded = json.loads(Path(case["response"]).read_text())
                            expected = canonical_output(recorded)
                            observed = response_output(terminal)
                            normalized_expected, row["expected_history_references"] = comparable_output(expected)
                            normalized_observed, row["observed_history_references"] = comparable_output(observed)
                            row["terminal_status"] = terminal.get("status")
                            row["expected_terminal_status"] = "incomplete" if recorded.get("stop_reason") == "max_tokens" else "completed"
                            row["raw_public_output_matches_history"] = expected == observed
                            row["output_matches_history"] = normalized_expected == normalized_observed and row["terminal_status"] == row["expected_terminal_status"]
                            row["expected_output_units"], row["observed_output_units"] = len(expected), len(observed)
                        except (KeyError, ValueError):
                            row["comparison_error"] = "unsupported_output_shape"
                    (args.private_root / f"{case['id']}-round-{round_index}-response.bin").write_bytes(body)
                    connection.close()
                except (OSError, TimeoutError, ValueError) as error:
                    row["error"] = type(error).__name__
                finally:
                    row["elapsed_ms"] = (time.perf_counter() - started) * 1000
                    stop.set()
                    thread.join()
                after = reader.read()
                row["cpu_ms"] = (after["cpu_seconds"] - before["cpu_seconds"]) * 1000
                row["resources_before"], row["resources_after"] = before, after
                row["sampled_resources"] = samples
                row["upstream_exchanges"] = sum(request["recorded"] for request in upstream.received)
                row["unrecorded_upstream_requests"] = sum(not request["recorded"] for request in upstream.received)
                row["upstream_disconnects"] = upstream.disconnects
                result["cases"].append(row)
            reader.close()
    finally:
        if process is not None:
            stop_stravia_server(process, logs, print_tail=0)
            (args.private_root / "last-server.log").write_text("\n".join(logs))
        for server in [upstream, catalog]:
            server.shutdown()
        private_json(args.output, result)
    print(json.dumps({"rows": len(result["cases"]), "http_200": sum(row.get("status") == 200 for row in result["cases"]),
                      "output_matches": sum(row.get("output_matches_history") is True for row in result["cases"])}))
    return 0 if result["cases"] and all(
        row.get("status") == 200 and row.get("output_matches_history") is True
        for row in result["cases"]
    ) else 1


if __name__ == "__main__":
    raise SystemExit(main())
