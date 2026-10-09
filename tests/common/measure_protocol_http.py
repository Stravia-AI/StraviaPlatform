"""Isolated, auditable 4 x 4 real HTTP protocol benchmark (stdlib only).

See docs/research/protocol-http-benchmark.md. No production endpoints are used.
"""
from __future__ import annotations

import argparse
import ctypes
import hashlib
import http.client
import json
import math
import os
import platform
import queue
import re
import sqlite3
import subprocess
import tempfile
import threading
import time
import uuid
from collections import Counter
from contextlib import ExitStack, closing
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlsplit

from tests.common.helpers import initialize_server, start_stravia_server, stop_stravia_server, wait_for_setup_token

PROTOCOLS = ("openai-chat", "open-responses", "anthropic-messages", "google-content")
PROVIDERS = dict(zip(PROTOCOLS, ("openai-compatible", "open-responses", "anthropic-messages", "google-gemini")))
MARKER = re.compile(r"STRAVIA_BENCH_[0-9a-f]{32}")


def encode(value: object) -> bytes:
    return json.dumps(value, separators=(",", ":"), ensure_ascii=False).encode()


def sse_values(raw: bytes) -> list[dict | str]:
    values = []
    data = []
    for line in raw.decode("utf-8").splitlines():
        if not line:
            if data:
                text = "\n".join(data)
                values.append(text if text == "[DONE]" else json.loads(text))
                data.clear()
        elif line.startswith("data:"):
            data.append(line[5:].removeprefix(" "))
    if data:
        raise ValueError("stream ended before its SSE event delimiter")
    return values


def visible(protocol: str, value: dict | str, stream: bool) -> str:
    if not isinstance(value, dict):
        return ""
    if protocol == "openai-chat":
        return "".join(choice.get("delta" if stream else "message", {}).get("content") or ""
                       for choice in value.get("choices", []))
    if protocol == "open-responses":
        if stream:
            return value.get("delta", "") if value.get("type") == "response.output_text.delta" else ""
        return "".join(part.get("text", "") for item in value.get("output", [])
                       for part in item.get("content", []) if part.get("type") == "output_text")
    if protocol == "anthropic-messages":
        if stream:
            delta = value.get("delta", {}) if value.get("type") == "content_block_delta" else value.get("content_block", {})
            return delta.get("text", "") if delta.get("type") in ("text_delta", "text") else ""
        return "".join(part.get("text", "") for part in value.get("content", []) if part.get("type") == "text")
    return "".join(part.get("text", "") for candidate in value.get("candidates", [])
                   for part in candidate.get("content", {}).get("parts", []) if not part.get("thought", False))


def response_text(protocol: str, raw: bytes, stream: bool) -> str:
    values = sse_values(raw) if stream else [json.loads(raw)]
    if any(isinstance(value, dict) and (
        value.get("error") is not None or value.get("type") in ("error", "response.failed")
    ) for value in values):
        raise ValueError("protocol error terminal after HTTP response")
    objects = [value for value in values if isinstance(value, dict)]
    if protocol == "openai-chat":
        terminal = any(choice.get("finish_reason") in ("stop", "length")
                       for value in objects for choice in value.get("choices", []))
        terminal = terminal and (not stream or bool(values) and values[-1] == "[DONE]")
    elif protocol == "open-responses":
        if stream:
            terminal = bool(objects) and objects[-1].get("type") in ("response.completed", "response.incomplete")
            response = objects[-1].get("response", {}) if objects else {}
        else:
            response = objects[0] if objects else {}
            terminal = True
        terminal = terminal and response.get("status") in ("completed", "incomplete") and response.get("error") is None
    elif protocol == "anthropic-messages":
        if stream:
            terminal = bool(objects) and objects[-1].get("type") == "message_stop" and any(
                value.get("delta", {}).get("stop_reason") in ("end_turn", "max_tokens", "stop_sequence")
                for value in objects if value.get("type") == "message_delta"
            )
        else:
            terminal = bool(objects) and objects[0].get("stop_reason") in ("end_turn", "max_tokens", "stop_sequence")
    else:
        terminal = bool(objects) and any(candidate.get("finishReason") in ("STOP", "MAX_TOKENS")
                                       for candidate in objects[-1].get("candidates", []))
    if not terminal:
        raise ValueError("missing successful protocol terminal")
    return "".join(visible(protocol, value, stream) for value in values)


def prompt_from(body: dict, protocol: str) -> str:
    if protocol == "open-responses":
        value = body.get("input", [])
        if isinstance(value, str):
            return value
        return "".join(part.get("text", "") for item in value for part in item.get("content", []) if isinstance(part, dict))
    if protocol == "google-content":
        return "".join(part.get("text", "") for item in body.get("contents", []) for part in item.get("parts", []))
    return "".join(item["content"] if isinstance(item.get("content"), str) else "".join(part.get("text", "") for part in item.get("content", []) if isinstance(part, dict)) for item in body.get("messages", []) if item.get("role") == "user")


def response_frames(protocol: str, model: str, anchor: str, stream: bool) -> list[tuple[str | None, dict | str]]:
    usage = {"input_tokens": 1, "output_tokens": 1}
    if protocol == "openai-chat":
        if not stream:
            return [(None, {"id": "chatcmpl-bench", "object": "chat.completion", "created": 1, "model": model, "choices": [{"index": 0, "message": {"role": "assistant", "content": anchor}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}})]
        return [(None, {"id": "chatcmpl-bench", "object": "chat.completion.chunk", "created": 1, "model": model, "choices": [{"index": 0, "delta": {"role": "assistant", "content": anchor}, "finish_reason": None}]}), (None, {"id": "chatcmpl-bench", "object": "chat.completion.chunk", "model": model, "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}}), (None, "[DONE]")]
    if protocol == "anthropic-messages":
        message = {"id": "msg_bench", "type": "message", "role": "assistant", "model": model, "content": [{"type": "text", "text": anchor}], "stop_reason": "end_turn", "stop_sequence": None, "usage": usage}
        if not stream:
            return [(None, message)]
        return [("message_start", {"type": "message_start", "message": {**message, "content": [], "stop_reason": None}}), ("content_block_start", {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}), ("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": anchor}}), ("content_block_stop", {"type": "content_block_stop", "index": 0}), ("message_delta", {"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": None}, "usage": {"output_tokens": 1}}), ("message_stop", {"type": "message_stop"})]
    if protocol == "google-content":
        return [(None, {"candidates": [{"index": 0, "content": {"role": "model", "parts": [{"text": anchor}]}, "finishReason": "STOP"}], "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1, "totalTokenCount": 2}, "modelVersion": model})]
    item = {"id": "msg_bench", "type": "message", "role": "assistant", "status": "completed", "content": [{"type": "output_text", "text": anchor, "annotations": []}]}
    response = {
        "id": "resp_bench", "object": "response", "created_at": 1, "completed_at": 2,
        "status": "completed", "model": model, "output": [item],
        "incomplete_details": None, "previous_response_id": None,
        "instructions": None, "error": None, "tools": [], "tool_choice": "auto",
        "truncation": "disabled", "parallel_tool_calls": True,
        "text": {"format": {"type": "text"}}, "top_p": None,
        "presence_penalty": None, "frequency_penalty": None, "top_logprobs": None,
        "temperature": None, "reasoning": None, "max_output_tokens": None,
        "max_tool_calls": None, "store": False, "background": False,
        "service_tier": "default", "metadata": {}, "safety_identifier": None,
        "prompt_cache_key": None,
        "usage": {**usage, "total_tokens": 2, "input_tokens_details": {"cached_tokens": 0},
                  "output_tokens_details": {"reasoning_tokens": 0}},
    }
    if not stream:
        return [(None, response)]
    events = [("response.created", {"response": {**response, "status": "in_progress", "output": []}}), ("response.output_item.added", {"output_index": 0, "item": {**item, "status": "in_progress", "content": []}}), ("response.content_part.added", {"item_id": "msg_bench", "output_index": 0, "content_index": 0, "part": {"type": "output_text", "text": "", "annotations": []}}), ("response.output_text.delta", {"item_id": "msg_bench", "output_index": 0, "content_index": 0, "delta": anchor}), ("response.output_text.done", {"item_id": "msg_bench", "output_index": 0, "content_index": 0, "text": anchor}), ("response.content_part.done", {"item_id": "msg_bench", "output_index": 0, "content_index": 0, "part": item["content"][0]}), ("response.output_item.done", {"output_index": 0, "item": item}), ("response.completed", {"response": response})]
    return [(event, {"type": event, "sequence_number": index, **body}) for index, (event, body) in enumerate(events)]


class Upstream(ThreadingHTTPServer):
    request_queue_size = 256
    daemon_threads = True

    def __init__(self, protocol: str, delay: float):
        super().__init__(("127.0.0.1", 0), Provider)
        self.protocol = protocol
        self.delay = delay
        self.lock = threading.Lock()
        self.expected: dict[str, tuple[int, str]] = {}
        self.arrivals: list[dict] = []
        threading.Thread(target=self.serve_forever, daemon=True).start()


class Provider(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args: object) -> None:
        pass

    def do_POST(self) -> None:  # noqa: N802
        server = self.server
        assert isinstance(server, Upstream)
        row = {"timestamp": time.time(), "path": self.path, "nonce": None, "valid": False}
        try:
            length = int(self.headers.get("Content-Length", "0"))
            raw = self.rfile.read(length)
            body = json.loads(raw)
            prompt = prompt_from(body, server.protocol)
            marker = MARKER.search(prompt)
            nonce = marker.group() if marker else None
            row.update(nonce=nonce, wire_bytes=len(raw), content_length=length, prompt_bytes=len(prompt.encode()), prompt_sha256=hashlib.sha256(prompt.encode()).hexdigest())
            expected_path = {"openai-chat": "/v1/chat/completions", "open-responses": "/v1/responses", "anthropic-messages": "/v1/messages"}.get(server.protocol)
            valid_path = self.path.split("?", 1)[0] == expected_path if expected_path else self.path.startswith("/v1beta/models/") and (":generateContent" in self.path or ":streamGenerateContent" in self.path)
            with server.lock:
                expected = server.expected.get(nonce)
            row["valid"] = bool(valid_path and len(raw) == length and expected == (len(prompt.encode()), row["prompt_sha256"]))
            if not row["valid"]:
                raise ValueError("upstream request path, nonce, complete content or digest mismatch")
            stream = bool(body.get("stream")) or ":streamGenerateContent" in self.path
            time.sleep(server.delay)
            frames = response_frames(server.protocol, body.get("model", "bench"), nonce, stream)
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream" if stream else "application/json")
            self.send_header("Connection", "close")
            self.end_headers()
            for event, value in frames:
                data = value.encode() if isinstance(value, str) else encode(value)
                if stream:
                    data = (b"event: " + event.encode() + b"\n" if event else b"") + b"data: " + data + b"\n\n"
                self.wfile.write(data)
                self.wfile.flush()
            row["finished_at"] = time.time()
        except Exception as error:
            row["error"] = str(error)
            if not row["valid"]:
                self.send_error(400, "synthetic upstream validation failed")
        finally:
            with server.lock:
                server.arrivals.append(row)
            self.close_connection = True


class Catalog(BaseHTTPRequestHandler):
    def log_message(self, *_args: object) -> None:
        pass

    def do_GET(self) -> None:  # noqa: N802
        documents = {
            "/version.json": {"revision": "benchmark-v1", "generated_at": "2026-01-01T00:00:00Z"},
            "/providers.json": {"benchmark": {
                "id": "benchmark", "name": "Benchmark", "npm": "@ai-sdk/openai-compatible",
                "api": f"http://127.0.0.1:{self.server.server_port}/v1",
            }},
            "/models.json": {"benchmark/text": {"id": "benchmark/text", "name": "Benchmark Text"}},
        }
        path = self.path.split("?", 1)[0]
        self.server.arrivals.append({"timestamp": time.time(), "path": self.path, "status": 200 if path in documents else 404})
        if path not in documents:
            self.send_error(404)
            return
        data = encode(documents[path])
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


class ProcessReader:
    """Read one PID, never host CPU. CPU time is cumulative kernel + user."""
    def __init__(self, pid: int):
        self.pid = pid
        self.handle = None
        if os.name == "nt":
            from ctypes import wintypes
            self.kernel = ctypes.WinDLL("kernel32", use_last_error=True)
            self.psapi = ctypes.WinDLL("psapi", use_last_error=True)
            self.kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
            self.kernel.OpenProcess.restype = wintypes.HANDLE
            self.kernel.CloseHandle.argtypes = [wintypes.HANDLE]
            self.kernel.GetProcessTimes.argtypes = [wintypes.HANDLE] + [ctypes.POINTER(wintypes.FILETIME)] * 4
            class Memory(ctypes.Structure):
                _fields_ = [("cb", wintypes.DWORD), ("PageFaultCount", wintypes.DWORD)] + [(name, ctypes.c_size_t) for name in ("PeakWorkingSetSize", "WorkingSetSize", "QuotaPeakPagedPoolUsage", "QuotaPagedPoolUsage", "QuotaPeakNonPagedPoolUsage", "QuotaNonPagedPoolUsage", "PagefileUsage", "PeakPagefileUsage", "PrivateUsage")]
            self.Memory = Memory
            self.FileTime = wintypes.FILETIME
            self.psapi.GetProcessMemoryInfo.argtypes = [wintypes.HANDLE, ctypes.POINTER(Memory), wintypes.DWORD]
            self.handle = self.kernel.OpenProcess(0x0400 | 0x0010, False, pid)
            if not self.handle:
                raise ctypes.WinError(ctypes.get_last_error())
        elif platform.system() != "Linux":
            raise RuntimeError("process sampler supports Windows and Linux only")

    def read(self) -> dict:
        if self.handle:
            times = [self.FileTime() for _ in range(4)]
            if not self.kernel.GetProcessTimes(self.handle, *(ctypes.byref(value) for value in times)):
                raise ctypes.WinError(ctypes.get_last_error())
            memory = self.Memory()
            memory.cb = ctypes.sizeof(memory)
            if not self.psapi.GetProcessMemoryInfo(self.handle, ctypes.byref(memory), memory.cb):
                raise ctypes.WinError(ctypes.get_last_error())
            cpu = sum((value.dwHighDateTime << 32) | value.dwLowDateTime for value in times[2:]) / 10_000_000
            return {"cpu_seconds": cpu, "rss_bytes": memory.WorkingSetSize, "private_commit_bytes": memory.PrivateUsage}
        stat = Path(f"/proc/{self.pid}/stat").read_text().rsplit(")", 1)[1].split()
        cpu = (int(stat[11]) + int(stat[12])) / os.sysconf("SC_CLK_TCK")
        rss = int(stat[21]) * os.sysconf("SC_PAGE_SIZE")
        fields = {}
        for line in Path(f"/proc/{self.pid}/status").read_text().splitlines():
            if line.startswith(("RssAnon:", "VmSwap:", "VmSize:")):
                key, value, _unit = line.split()
                fields[key.rstrip(":")] = int(value) * 1024
        return {"cpu_seconds": cpu, "rss_bytes": rss, "private_commit_bytes": None, "anonymous_resident_plus_swap_bytes": fields.get("RssAnon", 0) + fields.get("VmSwap", 0), "virtual_bytes": fields.get("VmSize")}

    def close(self) -> None:
        if self.handle:
            self.kernel.CloseHandle(self.handle)
            self.handle = None


def request_body(protocol: str, model: str, stream: bool, prompt: str) -> dict:
    if protocol == "openai-chat":
        return {"model": model, "stream": stream, "messages": [{"role": "user", "content": prompt}]}
    if protocol == "open-responses":
        return {"model": model, "stream": stream, "input": prompt}
    if protocol == "anthropic-messages":
        return {"model": model, "stream": stream, "max_tokens": 32, "messages": [{"role": "user", "content": prompt}]}
    return {"contents": [{"role": "user", "parts": [{"text": prompt}]}]}


def ingress_path(protocol: str, model: str, stream: bool) -> str:
    if protocol == "google-content":
        return f"/v1beta/models/{model}:" + ("streamGenerateContent?alt=sse" if stream else "generateContent")
    return {"openai-chat": "/v1/chat/completions", "open-responses": "/v1/responses", "anthropic-messages": "/v1/messages"}[protocol]


def percentile(values: list[float], fraction: float) -> float | None:
    return sorted(values)[max(0, math.ceil(len(values) * fraction) - 1)] if values else None


def docker_memory_bytes(value: str) -> float:
    match = re.fullmatch(r"([\d.]+)\s*(B|kB|KB|MB|GB|TB|KiB|MiB|GiB|TiB)", value.strip())
    if not match:
        raise ValueError("unrecognized Docker memory unit")
    units = {"B": 1, "kB": 1000, "KB": 1000, "MB": 1000**2, "GB": 1000**3,
             "TB": 1000**4, "KiB": 1024, "MiB": 1024**2, "GiB": 1024**3, "TiB": 1024**4}
    return float(match[1]) * units[match[2]]


def provisioning(session, upstreams: dict[str, Upstream]) -> tuple[str, dict[str, str]]:
    def post(path: str, body: dict, expected: int = 200) -> dict:
        status, value = session.request("POST", path, body)
        if status != expected:
            raise RuntimeError(f"provision {path}: HTTP {status}: {value}")
        return value["data"]
    routes = {}
    for protocol, upstream in upstreams.items():
        model = "benchmark-" + protocol
        suffix = "/v1" if protocol in PROTOCOLS[:2] else ""
        provider = post("/api/v1/providers", {"name": model, "source": {"type": "custom", "vendor": "custom", "channel": "default", "protocol": PROVIDERS[protocol], "base_url": f"http://127.0.0.1:{upstream.server_port}{suffix}"}, "credential": {"type": "api_key", "value": "benchmark-only"}, "vendor_options": {}})
        post(f"/api/v1/providers/{provider['id']}/models", {"model_id": model, "metadata": {}}, 201)
        route = post("/api/v1/models", {"model_id": model, "targets": [{"provider_id": provider["id"], "model": model}]})
        routes[protocol] = str(route["id"])
    key = post("/api/v1/api-keys", {"name": "isolated-protocol-benchmark", "model_ids": list(routes.values())})["key"]
    return key, routes


def run_load(args, base: str, key: str, ingress: str, upstream: Upstream, stream: bool, mode: str, duration: float, phase: str) -> dict:
    model = "benchmark-" + upstream.protocol
    address = urlsplit(base)
    rows: list[dict] = []
    tasks: queue.Queue = queue.Queue()  # Intentionally no silent bounded-queue drops.
    lock = threading.Lock()
    active = peak = 0
    started = time.perf_counter()
    deadline = started + duration
    arrivals_start = len(upstream.arrivals)

    def send(scheduled: float) -> None:
        nonlocal active, peak
        now = time.perf_counter()
        nonce = "STRAVIA_BENCH_" + uuid.uuid4().hex
        skeleton = encode(request_body(ingress, model, stream, nonce))
        if len(skeleton) > args.payload_bytes:
            raise ValueError("payload-bytes smaller than protocol framing")
        prompt = nonce + args.padding[:args.payload_bytes - len(skeleton)]
        data = encode(request_body(ingress, model, stream, prompt))
        with upstream.lock:
            upstream.expected[nonce] = (len(prompt.encode()), hashlib.sha256(prompt.encode()).hexdigest())
        row = {"nonce": nonce, "scheduled_at": time.time() - (now - scheduled), "started_at": time.time(), "queue_ms": (now - scheduled) * 1000, "request_bytes": len(data), "status": None, "ttft_ms": None, "error": None}
        with lock:
            active += 1
            peak = max(peak, active)
        connection = http.client.HTTPConnection(address.hostname, address.port, timeout=args.request_timeout)
        try:
            headers = {"Authorization": "Bearer " + key, "Content-Type": "application/json"}
            if ingress == "anthropic-messages":
                headers["anthropic-version"] = "2023-06-01"
            connection.request("POST", ingress_path(ingress, model, stream), data, headers)
            response = connection.getresponse()
            row["status"] = response.status
            chunks = []
            frame = bytearray()
            if stream:
                while line := response.readline():
                    chunks.append(line)
                    frame.extend(line)
                    if not line.strip():
                        if row["ttft_ms"] is None and any(
                            visible(ingress, value, True).strip() for value in sse_values(bytes(frame))
                        ):
                            row["ttft_ms"] = (time.perf_counter() - now) * 1000
                        frame.clear()
            else:
                chunks.append(response.read())
            raw = b"".join(chunks)
            row["response_bytes"] = len(raw)
            row["anchor_valid"] = nonce in response_text(ingress, raw, stream)
            if response.status != 200 or not row["anchor_valid"]:
                row["error"] = f"HTTP {response.status}; anchor_valid={row['anchor_valid']}; {raw[:256]!r}"
            if stream and row["ttft_ms"] is None:
                row["error"] = row["error"] or "no visible text in stream"
        except Exception as error:
            row["error"] = f"{type(error).__name__}: {error}"
        finally:
            connection.close()
            row.update(finished_at=time.time(), service_ms=(time.perf_counter() - now) * 1000, end_to_end_ms=(time.perf_counter() - scheduled) * 1000)
            with lock:
                active -= 1
                rows.append(row)
            with upstream.lock:
                upstream.expected.pop(nonce, None)

    def worker() -> None:
        if mode == "fixed-workers":
            while time.perf_counter() < deadline:
                send(time.perf_counter())
            return
        while True:
            scheduled = tasks.get()
            try:
                if scheduled is None:
                    return
                send(scheduled)
            finally:
                tasks.task_done()

    workers = [threading.Thread(target=worker) for _ in range(args.concurrency)]
    for thread in workers:
        thread.start()
    scheduled_count = 0
    max_queue = 0
    if mode == "open-loop":
        scheduled_count = math.ceil(duration * args.qps)
        for index in range(scheduled_count):
            scheduled = started + index / args.qps
            time.sleep(max(0, scheduled - time.perf_counter()))
            tasks.put(scheduled)
            max_queue = max(max_queue, tasks.qsize())
        time.sleep(max(0, deadline - time.perf_counter()))
        for _thread in workers:
            tasks.put(None)
    for thread in workers:
        thread.join()
    elapsed = time.perf_counter() - started
    with upstream.lock:
        arrivals = list(upstream.arrivals[arrivals_start:])
    counts = Counter(row["nonce"] for row in arrivals)
    failures = [row for row in rows if row["error"] or counts[row["nonce"]] != 1]
    reasons = []
    if failures or any(not row["valid"] or row.get("error") for row in arrivals):
        reasons.append("request/upstream validation failed, missing or duplicate arrival")
    if mode == "open-loop" and (len(rows) != scheduled_count or (phase == "measure" and len(rows) / elapsed < args.qps * args.min_qps_ratio)):
        reasons.append("missing request or achieved QPS below required ratio")
    success = [row for row in rows if not row["error"] and counts[row["nonce"]] == 1]
    return {"phase": phase, "mode": mode, "ingress": ingress, "upstream": upstream.protocol, "stream": stream, "duration_seconds": duration, "elapsed_including_drain_seconds": elapsed, "target_qps": args.qps if mode == "open-loop" else None, "actual_qps": len(rows) / elapsed, "successful_qps": len(success) / elapsed, "scheduled_requests": scheduled_count if mode == "open-loop" else len(rows), "completed_requests": len(rows), "errors": len(failures), "status_counts": dict(Counter(str(row["status"]) for row in rows)), "peak_inflight": peak, "peak_loadgen_queue": max_queue, "valid": not reasons, "failure_reasons": reasons, "latency_ms": {name: {"p50": percentile([row[field] for row in success if row[field] is not None], .5), "p95": percentile([row[field] for row in success if row[field] is not None], .95), "p99": percentile([row[field] for row in success if row[field] is not None], .99)} for name, field in (("end_to_end", "end_to_end_ms"), ("service", "service_ms"), ("ttft", "ttft_ms"), ("queue", "queue_ms"))}, "requests": rows, "upstream_arrivals": arrivals}


def database_config(args) -> dict:
    if args.backend == "sqlite":
        return {"backend": "sqlite"}
    url = urlsplit(args.database_url or "")
    if url.scheme not in ("postgres", "postgresql") or url.hostname not in ("127.0.0.1", "localhost", "::1") or not args.confirm_isolated_database:
        raise ValueError("Postgres requires loopback --database-url and --confirm-isolated-database; never supply a production database")
    return {"backend": "postgres", "url": args.database_url}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--label", required=True)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--duration", type=float, default=30)
    parser.add_argument("--warmup", type=float, default=5)
    parser.add_argument("--qps", type=float, default=10)
    parser.add_argument("--concurrency", type=int, default=32)
    parser.add_argument("--payload-bytes", type=int, default=5_000_000)
    parser.add_argument("--payload-pattern", choices=("varied", "repetitive"), default="varied")
    parser.add_argument("--upstream-delay", type=float, default=1)
    parser.add_argument("--sample-interval", type=float, default=.25)
    parser.add_argument("--request-timeout", type=float, default=120)
    parser.add_argument("--min-qps-ratio", type=float, default=.95)
    parser.add_argument("--ingress", nargs="+", choices=PROTOCOLS, default=list(PROTOCOLS))
    parser.add_argument("--upstream", nargs="+", choices=PROTOCOLS, default=list(PROTOCOLS))
    parser.add_argument("--mode", nargs="+", choices=("open-loop", "fixed-workers"), default=["open-loop", "fixed-workers"])
    parser.add_argument("--delivery", nargs="+", choices=("unary", "stream"), default=["unary", "stream"])
    parser.add_argument("--backend", choices=("sqlite", "postgres"), default="sqlite")
    parser.add_argument("--database-url")
    parser.add_argument("--confirm-isolated-database", action="store_true")
    parser.add_argument("--redis-url")
    parser.add_argument("--resource-container", action="append", default=[],
                        help="Dedicated local Docker container to sample separately (repeatable)")
    parser.add_argument("--source-commit", default="unspecified")
    parser.add_argument("--build-condition", default="unspecified")
    parser.add_argument("--config", type=Path, help="Optional server TOML (must not configure production storage)")
    args = parser.parse_args()
    for name in ("duration", "qps", "concurrency", "payload_bytes", "sample_interval", "request_timeout"):
        if getattr(args, name) <= 0:
            parser.error(f"--{name.replace('_', '-')} must be positive")
    if args.warmup < 0 or args.upstream_delay < 0 or not 0 < args.min_qps_ratio <= 1:
        parser.error("warmup/delay must be nonnegative and min-qps-ratio in (0,1]")
    if args.payload_bytes < 512:
        parser.error("--payload-bytes must be at least 512 (protocol framing)")
    if args.redis_url and urlsplit(args.redis_url).hostname not in ("127.0.0.1", "localhost", "::1"):
        parser.error("Redis must be a dedicated loopback benchmark instance")
    if any(not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", name) for name in args.resource_container):
        parser.error("resource-container must be an explicit Docker container name")
    if args.config:
        import tomllib
        config = tomllib.loads(args.config.read_text(encoding="utf-8"))
        if any(key in config for key in ("database", "storage", "cache")):
            parser.error("--config must not contain database/storage/cache; benchmark owns isolated setup")
    binary = args.binary.resolve()
    with binary.open("rb") as source:
        sha = hashlib.file_digest(source, "sha256").hexdigest()
    run_id = uuid.uuid4().hex
    args.output.parent.mkdir(parents=True, exist_ok=True)
    metadata = vars(args).copy()
    for key in ("database_url", "redis_url"):
        metadata[key] = "provided (credentials omitted)" if metadata[key] else None
    metadata = {key: str(value) if isinstance(value, Path) else value for key, value in metadata.items()}
    # 固定内容熵；不让单字符正文的极高压缩率掩盖真实 SQL 编解码成本。
    args.padding = (
        "".join(hashlib.sha256(index.to_bytes(8, "little")).hexdigest()
                for index in range(math.ceil(args.payload_bytes / 64)))[:args.payload_bytes]
        if args.payload_pattern == "varied" else "x" * args.payload_bytes
    )
    result = {"schema_version": 2, "response_validation": "native-terminal-v1", "run_id": run_id, "timestamp": time.time(), "binary_sha256": sha, "platform": platform.platform(), "logical_cpus": os.cpu_count(), "workload": metadata, "debug_observation": False, "cpu_units": "core-percent; 100 = one CPU core", "memory_units": "bytes; decimal MB = bytes/1000000; MiB = bytes/1048576", "cases": [], "samples": [], "fatal_error": None}
    upstreams = {protocol: Upstream(protocol, args.upstream_delay) for protocol in args.upstream}
    catalog = ThreadingHTTPServer(("127.0.0.1", 0), Catalog)
    catalog.arrivals = []
    threading.Thread(target=catalog.serve_forever, daemon=True).start()
    process = None
    logs = []
    sampler = None
    stop = threading.Event()
    context = {"case": "startup", "phase": "setup"}
    readers = []
    docker_stats = None
    docker_sampler = None
    docker_errors = []
    try:
        database = database_config(args)
        with ExitStack() as cleanup:
            directory = cleanup.enter_context(tempfile.TemporaryDirectory(prefix="stravia-protocol-bench-"))
            # Stop the child before TemporaryDirectory removes SQLite files,
            # including on setup/workload errors (important on Windows).
            cleanup.callback(lambda: stop_stravia_server(process, logs, print_tail=0) if process else None)
            command = ["--host", "127.0.0.1", "--port", "0", "--data-dir", directory,
                       "--test-catalog-base-url", f"http://127.0.0.1:{catalog.server_port}"]
            if args.config or args.redis_url:
                config_text = args.config.read_text(encoding="utf-8") if args.config else ""
                if args.redis_url:
                    config_text += "\n[cache]\nredis_url = " + json.dumps(args.redis_url) + "\n"
                config_path = Path(directory) / "benchmark.toml"
                config_path.write_text(config_text, encoding="utf-8")
                command.extend(["--config", str(config_path)])
            environment = {"RUST_LOG": "warn,stravia_server=info"}
            process, logs = start_stravia_server(stravia_binary=binary, args=command, env=environment)
            result["server_pid"] = process.pid
            readers = [("server", ProcessReader(process.pid)), ("harness_loadgen_and_upstream", ProcessReader(os.getpid()))]
            def sample() -> None:
                previous = {}
                while not stop.is_set():
                    now = time.perf_counter()
                    for role, reader in readers:
                        try:
                            row = reader.read()
                            prior = previous.get(role)
                            row["cpu_core_percent"] = ((row["cpu_seconds"] - prior[1]) / (now - prior[0]) * 100) if prior else None
                            previous[role] = (now, row["cpu_seconds"])
                            result["samples"].append({"timestamp": time.time(), "monotonic_seconds": now, "pid": reader.pid, "role": role, **context.copy(), **row})
                        except Exception as error:
                            result["samples"].append({"timestamp": time.time(), "role": role, **context.copy(), "error": str(error)})
                    stop.wait(args.sample_interval)
            sampler = threading.Thread(target=sample, daemon=True)
            sampler.start()
            if args.resource_container:
                docker_stats = subprocess.Popen(
                    ["docker", "stats", "--no-trunc", "--format", "{{json .}}", *args.resource_container],
                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, encoding="utf-8",
                )
                def sample_containers() -> None:
                    try:
                        for line in docker_stats.stdout:
                            # Docker 的流式输出即使重定向也带清屏/行尾 CSI，先去掉显示控制。
                            line = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", line).strip()
                            if not line:
                                continue
                            row = json.loads(line)
                            result["samples"].append({
                                "timestamp": time.time(), **context.copy(),
                                "role": "container:" + row["Name"],
                                "working_set_bytes": docker_memory_bytes(row["MemUsage"].split("/")[0]),
                                "cpu_core_percent": float(row["CPUPerc"].removesuffix("%")),
                            })
                        if not stop.is_set():
                            docker_errors.append("Docker stats stopped before measurement completed")
                    except Exception as error:
                        docker_errors.append(f"Docker stats sampling failed: {type(error).__name__}")
                docker_sampler = threading.Thread(target=sample_containers, daemon=True)
                docker_sampler.start()
            token = wait_for_setup_token(logs, process, timeout=180)
            listener = next(re.search(r"Stravia startup listener opened .*?(127\.0\.0\.1:\d+)", line).group(1) for line in logs if re.search(r"Stravia startup listener opened .*?(127\.0\.0\.1:\d+)", line))
            base = "http://" + listener
            session = initialize_server(base, token, database)
            status, debug = session.request("GET", "/api/v1/observations/debug")
            if status != 200 or debug["data"]["enabled"]:
                raise RuntimeError("benchmark requires Observation Debug=false")
            key, routes = provisioning(session, upstreams)
            result["route_ids"] = routes
            for ingress in args.ingress:
                for protocol, upstream in upstreams.items():
                    for delivery in args.delivery:
                        for mode in args.mode:
                            case_id = f"{ingress}--{protocol}--{delivery}--{mode}"
                            case = {"id": case_id}
                            result["cases"].append(case)
                            for phase, duration in (("warmup", args.warmup), ("measure", args.duration)):
                                if duration == 0:
                                    continue
                                context.update(case=case_id, phase=phase)
                                case[phase] = run_load(args, base, key, ingress, upstream, delivery == "stream", mode, duration, phase)
                            samples = [row for row in result["samples"] if row.get("case") == case_id and row.get("phase") == "measure" and row["role"] == "server"]
                            case["server_resources"] = {field: {"max": max(values) if values else None, "p50": percentile(values, .5), "p95": percentile(values, .95)} for field in ("rss_bytes", "private_commit_bytes", "anonymous_resident_plus_swap_bytes", "cpu_core_percent") if (values := [row[field] for row in samples if row.get(field) is not None]) or field in ("rss_bytes", "private_commit_bytes", "cpu_core_percent")}
                            case["container_resources"] = {}
                            for name in args.resource_container:
                                container_samples = [row for row in result["samples"] if row.get("case") == case_id and row.get("phase") == "measure" and row["role"] == "container:" + name]
                                case["container_resources"][name] = {
                                    field: {"max": max(values) if values else None, "p50": percentile(values, .5), "p95": percentile(values, .95)}
                                    for field in ("working_set_bytes", "cpu_core_percent")
                                    for values in [[row[field] for row in container_samples]]
                                }
                                if not container_samples:
                                    docker_errors.append(f"No container samples for {name} in {case_id}")
                            case["valid"] = all(case[phase]["valid"] for phase in ("warmup", "measure") if phase in case) and bool(samples) and not any("error" in row for row in samples)
                            # Checkpoint all raw evidence; no synthetic request body retained.
                            args.output.write_text(json.dumps(result, ensure_ascii=False, indent=2), encoding="utf-8")
            context.update(case="shutdown", phase="drain")
            time.sleep(args.upstream_delay + args.sample_interval)
            # Inventory is outside measured traffic; ordinary SQL/Observation
            # and Generation Chain storage remain enabled for the entire run.
            if args.backend == "sqlite":
                with closing(sqlite3.connect(f"file:{(Path(directory) / 'db' / 'gateway.db').as_posix()}?mode=ro", uri=True)) as connection:
                    tables = {row[0] for row in connection.execute("SELECT name FROM sqlite_master WHERE type='table'")}
                    result["storage_inventory"] = {table: connection.execute(f'SELECT COUNT(*) FROM "{table}"').fetchone()[0] for table in sorted(tables) if "observation" in table or table.startswith("turn_chain_")}
            stop.set()
            sampler.join()
            stop_stravia_server(process, logs, print_tail=0)
            process = None
    except Exception as error:
        result["fatal_error"] = f"{type(error).__name__}: {error}"
    finally:
        stop.set()
        if docker_stats:
            docker_stats.terminate()
            try:
                docker_stats.wait(timeout=15)
            except subprocess.TimeoutExpired:
                docker_stats.kill()
                docker_stats.wait()
                docker_errors.append("Docker stats did not stop within its cleanup deadline")
        if docker_sampler:
            docker_sampler.join(timeout=15)
            if docker_sampler.is_alive():
                docker_errors.append("Docker stats reader did not stop")
        if docker_stats:
            docker_stats.stdout.close()
            docker_stats.stderr.close()
        result["resource_sampling_errors"] = docker_errors
        if sampler:
            sampler.join()
        for _role, reader in readers:
            reader.close()
        if process:
            stop_stravia_server(process, logs, print_tail=0)
        for upstream in upstreams.values():
            upstream.shutdown()
            upstream.server_close()
        catalog.shutdown()
        catalog.server_close()
        result["catalog_arrivals"] = catalog.arrivals
        result["finished_at"] = time.time()
        result["valid"] = not result["fatal_error"] and not docker_errors and bool(result["cases"]) and all(case.get("valid", False) for case in result["cases"])
        def redact(line: str) -> str:
            line = re.sub(r"(Stravia setup token:)\s*\S+", r"\1 [redacted]", line)
            for secret in (args.database_url, args.redis_url):
                if secret:
                    line = line.replace(secret, "[connection URL omitted]")
            return line
        result["server_logs"] = [redact(line) for line in logs]
        if result["fatal_error"]:
            result["fatal_error"] = redact(result["fatal_error"])
        args.output.write_text(json.dumps(result, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps({"output": str(args.output), "valid": result["valid"], "fatal_error": result["fatal_error"], "cases": len(result["cases"])}))
    return 0 if result["valid"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
