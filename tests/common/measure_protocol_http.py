"""Isolated, auditable long-context, multimodal real-HTTP protocol benchmark.

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
from urllib.parse import parse_qs, urlsplit

from tests.common.helpers import initialize_server, start_stravia_server, stop_stravia_server, wait_for_setup_token
from tests.common.protocol_audit import audit_requests
from tests.common.protocol_history_audit import (
    NORMALIZED_HISTORY, history_storage_schema, read_credential_evidence, read_history_evidence,
)
from tests.common.protocol_workload import (
    Message, Workload, build_request, canonical_digest,
    canonical_input, timed_response_frames,
)

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

    def __init__(self, protocol: str, workload: Workload):
        super().__init__(("127.0.0.1", 0), Provider)
        self.protocol = protocol
        self.lock = threading.Lock()
        self.expected: dict[str, tuple] = {}
        self.arrivals: list[dict] = []
        self.response_histories: dict[str, tuple] = {}
        self.workload = workload
        self.credential_mappings: dict[str, str] = {}
        self.synthetic_secrets: set[str] = set()
        self.gateway_instruction: str | None = None
        self.credential_expectations: dict[str, dict] = {}
        self.bound_plans: dict[str, object] = {}
        threading.Thread(target=self.serve_forever, daemon=True).start()


class Provider(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args: object) -> None:
        pass

    def do_POST(self) -> None:  # noqa: N802
        server = self.server
        assert isinstance(server, Upstream)
        row = {"timestamp": time.time(), "path": self.path.split("?", 1)[0], "nonce": None, "valid": False}
        try:
            length = int(self.headers.get("Content-Length", "0"))
            raw = self.rfile.read(length)
            body = json.loads(raw)
            received_at = time.perf_counter()
            messages = canonical_input(server.protocol, body)
            previous_id = body.get("previous_response_id") if server.protocol == "open-responses" else None
            if previous_id:
                with server.lock:
                    previous = server.response_histories.pop(previous_id, None)
                if previous is None:
                    raise ValueError("unknown native previous_response_id")
                if messages and messages[0].role == "system":
                    if not previous or previous[0] != messages[0]:
                        raise ValueError("native continuation gateway instruction changed")
                    messages = messages[1:]
                messages = previous + messages
            row["native_previous_response"] = bool(previous_id)
            latest_user = next(message for message in reversed(messages) if message.role == "user")
            matches = MARKER.findall(latest_user.text)
            nonce = matches[-1] if matches else None
            digest = canonical_digest(messages)
            row.update(nonce=nonce, wire_bytes=len(raw), content_length=length, input_sha256=digest,
                       request_wire_sha256=hashlib.sha256(raw).hexdigest())
            expected_path = {"openai-chat": "/v1/chat/completions", "open-responses": "/v1/responses", "anthropic-messages": "/v1/messages"}.get(server.protocol)
            valid_path = self.path.split("?", 1)[0] == expected_path if expected_path else self.path.startswith("/v1beta/models/") and (":generateContent" in self.path or ":streamGenerateContent" in self.path)
            with server.lock:
                expected = server.expected.get(nonce)
            actual_model = body.get("model") if server.protocol != "google-content" else self.path.split("/models/", 1)[-1].split(":", 1)[0]
            row["actual_model"] = actual_model
            credential_valid = (
                parse_qs(urlsplit(self.path).query).get("key") == ["benchmark-only"]
                if server.protocol == "google-content"
                else self.headers.get("x-api-key") == "benchmark-only"
                if server.protocol == "anthropic-messages"
                else self.headers.get("Authorization") == "Bearer benchmark-only"
            )
            row["credential_valid"] = credential_valid
            if not (valid_path and credential_valid and len(raw) == length and expected
                    and expected[1] == actual_model):
                raise ValueError("upstream request path, model, complete history or image digest mismatch")
            with server.lock:
                if (not messages or messages[0].role != "system" or not messages[0].text
                        or messages[0].images or MARKER.search(messages[0].text)
                        or "<!--sr:" in messages[0].text
                        or any(secret in messages[0].text for secret in expected[0].credential_secrets)):
                    raise ValueError("gateway credential instruction missing or contaminated")
                if server.gateway_instruction is None:
                    server.gateway_instruction = messages[0].text
                elif messages[0].text != server.gateway_instruction:
                    raise ValueError("gateway credential instruction changed")
                bound = server.workload.bind_protected_plan(
                    expected[0], messages, server.credential_mappings, server.gateway_instruction)
                server.credential_mappings.update(bound.mappings)
                server.bound_plans[nonce] = bound
                server.credential_expectations[nonce] = {
                    "secrets": tuple(bound.mappings),
                    "references": tuple(bound.mappings.values()),
                    "new_references": bound.new_references,
                    "reused_references": bound.reused_references,
                }
            plan = bound.wire
            row.update(valid=True, upstream_input_tokens=plan.input_tokens,
                       gateway_instruction_sha256=hashlib.sha256(server.gateway_instruction.encode()).hexdigest(),
                       gateway_instruction_tokens=server.workload.count(server.gateway_instruction),
                       credential_protection_verified=True,
                       protected_mapping_count=len(bound.mappings),
                       reused_mapping_count=bound.reused_references)
            stream = bool(body.get("stream")) or ":streamGenerateContent" in self.path
            frames = timed_response_frames(server.protocol, actual_model, plan, stream)
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream" if stream else "application/json")
            self.send_header("Connection", "close")
            self.end_headers()
            visible_times = []
            pacing_lag = []
            response_hash = hashlib.sha256()
            for offset, event, value in frames:
                due = received_at + offset
                time.sleep(max(0, due - time.perf_counter()))
                data = value.encode() if isinstance(value, str) else encode(value)
                if stream:
                    data = (b"event: " + event.encode() + b"\n" if event else b"") + b"data: " + data + b"\n\n"
                self.wfile.write(data)
                self.wfile.flush()
                response_hash.update(data)
                emitted = time.perf_counter()
                pacing_lag.append(max(0, emitted - due) * 1000)
                if visible(server.protocol, value, stream):
                    visible_times.append(emitted)
            row.update(
                first_visible_ms=(visible_times[0] - received_at) * 1000 if visible_times else None,
                output_span_seconds=visible_times[-1] - visible_times[0] if len(visible_times) > 1 else None,
                pacing_lag_p95_ms=percentile(pacing_lag, .95),
                pacing_lag_max_ms=max(pacing_lag, default=0),
                output_tokens=plan.output_tokens,
                response_wire_sha256=response_hash.hexdigest(),
            )
            row["finished_at"] = time.time()
            if server.protocol == "open-responses":
                with server.lock:
                    server.response_histories["resp_" + nonce.removeprefix("STRAVIA_BENCH_")] = (
                        plan.messages + (Message("assistant", plan.output_text),))
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


def provisioning(session, upstreams: dict[str, Upstream]) -> tuple[str, dict[str, dict]]:
    def post(path: str, body: dict, expected: int = 200) -> dict:
        status, value = session.request("POST", path, body)
        if status != expected:
            raise RuntimeError(f"provision {path}: HTTP {status}")
        return value["data"]
    routes = {}
    for protocol, upstream in upstreams.items():
        model = "benchmark-" + protocol
        suffix = "/v1" if protocol in PROTOCOLS[:2] else ""
        targets = []
        for name, enabled, priority in (("primary", True, 10), ("secondary", True, 0), ("disabled", False, 20)):
            upstream_model = model + "-" + name
            provider = post("/api/v1/providers", {"name": upstream_model, "source": {"type": "custom", "vendor": "custom", "channel": "default", "protocol": PROVIDERS[protocol], "base_url": f"http://127.0.0.1:{upstream.server_port}{suffix}"}, "credential": {"type": "api_key", "value": "benchmark-only"}, "vendor_options": {}})
            post(f"/api/v1/providers/{provider['id']}/models", {"model_id": upstream_model, "metadata": {"modalities": {"input": ["text", "image"], "output": ["text"]}, "limit": {"context": 1000000, "output": 10000}}}, 201)
            targets.append({"provider_id": provider["id"], "model": upstream_model, "enabled": enabled,
                            "priority": priority, "target_retry_budget": 0, "first_token_timeout_ms": 90000})
        route = post("/api/v1/models", {"model_id": model, "output_modality": "text", "targets": targets})
        primary = next(target for target in route["targets"] if target["provider_id"] == targets[0]["provider_id"])
        routes[protocol] = {"route_id": str(route["id"]), "expected_provider_id": targets[0]["provider_id"],
                            "configured_target_id": primary["id"],
                            "expected_target_id": targets[0]["provider_id"] + ":" + targets[0]["model"],
                            "expected_model": targets[0]["model"]}
    key = post("/api/v1/api-keys", {"name": "isolated-protocol-benchmark", "model_ids": [route["route_id"] for route in routes.values()]})["key"]
    return key, routes


def run_load(args, base: str, key: str, ingress: str, upstream: Upstream, stream: bool,
             mode: str, duration: float, phase: str, route: dict) -> dict:
    model = "benchmark-" + upstream.protocol
    address = urlsplit(base)
    rows: list[dict] = []
    # 固定分配 worker，避免完成顺序改变同一 seed 的多轮上下文。
    tasks = [queue.Queue() for _ in range(args.concurrency)]
    lock = threading.Lock()
    active = peak = 0
    started = time.perf_counter()
    deadline = started + duration
    arrivals_start = len(upstream.arrivals)
    limit = args.requests_per_case if phase == "measure" else None
    identity = f"{ingress}/{upstream.protocol}/{stream}/{mode}/{phase}"
    worker_errors = []

    def send(index: int, scheduled: float, state: dict) -> None:
        nonlocal active, peak
        picked = time.perf_counter()
        if state["turn"] == args.chain_turns:
            state.update(history=(), parent_nonce=None, turn=0, chain=state["chain"] + 1)
        reserve = (args.chain_turns - state["turn"] - 1) * (args.max_tokens_per_second * args.max_duration + 512)
        plan = args.workload_generator.make_plan((identity, index), state["history"], reserve_tokens=reserve)
        nonce = plan.nonce
        session_id = f"bench:{identity}:{state['worker']}:{state['chain']}"
        data = encode(build_request(ingress, model, stream, plan))
        input_sha = canonical_digest(plan.messages)
        with upstream.lock:
            upstream.expected[nonce] = (plan, route["expected_model"])
            if isinstance(upstream, Upstream):
                upstream.synthetic_secrets.update(plan.credential_secrets)
        now = time.perf_counter()
        row = {**plan.metadata(), **route, "index": index, "session_id": session_id,
               "request_id": nonce, "expected_parent": None, "expected_parent_nonce": state["parent_nonce"],
               "chain_turn": state["turn"], "ingress": ingress, "upstream": upstream.protocol, "stream": stream,
               "input_sha256": input_sha,
               "request_wire_sha256": hashlib.sha256(data).hexdigest(),
               "scheduled_at": time.time() - (now - scheduled), "started_at": time.time(),
               "queue_ms": (picked - scheduled) * 1000, "preparation_ms": (now - picked) * 1000,
               "start_lag_ms": (now - scheduled) * 1000,
               "request_bytes": len(data), "status": None, "ttft_ms": None, "error": None}
        with lock:
            active += 1
            peak = max(peak, active)
        connection = http.client.HTTPConnection(address.hostname, address.port, timeout=args.request_timeout)
        try:
            headers = {"Authorization": "Bearer " + key, "Content-Type": "application/json",
                       "x-session-id": session_id, "x-request-id": nonce,
                       "x-stravia-benchmark-request": nonce}
            if ingress == "anthropic-messages":
                headers["anthropic-version"] = "2023-06-01"
            connection.request("POST", ingress_path(ingress, model, stream), data, headers)
            response = connection.getresponse()
            row["status"] = response.status
            row["response_correlation_headers"] = {name: value for name, value in response.getheaders()
                                                   if name.lower() in ("x-request-id", "x-chain-id", "x-stravia-request-id")}
            chunks = []
            frame = bytearray()
            visible_times = []
            if stream:
                while line := response.readline():
                    chunks.append(line)
                    frame.extend(line)
                    if not line.strip():
                        if any(
                            visible(ingress, value, True) for value in sse_values(bytes(frame))
                        ):
                            visible_times.append(time.perf_counter())
                            if row["ttft_ms"] is None:
                                row["ttft_ms"] = (visible_times[-1] - now) * 1000
                        frame.clear()
            else:
                chunks.append(response.read())
            raw = b"".join(chunks)
            row["response_bytes"] = len(raw)
            row["response_wire_sha256"] = hashlib.sha256(raw).hexdigest()
            text = response_text(ingress, raw, stream)
            with upstream.lock:
                bound = upstream.bound_plans.get(nonce) if isinstance(upstream, Upstream) else None
            client_plan = bound.client if bound else plan
            if bound:
                row.update(client_plan.metadata(), output_tokens=bound.wire.output_tokens,
                           client_output_tokens=client_plan.output_tokens,
                           upstream_input_tokens=bound.wire.input_tokens)
            elif plan.credential_secrets:
                raise ValueError("protected upstream plan evidence missing")
            row["actual_output_sha256"] = hashlib.sha256(text.encode()).hexdigest()
            row["actual_output_tokens"] = args.workload_generator.count(text)
            row["content_valid"] = text == client_plan.output_text
            values = sse_values(raw) if stream else [json.loads(raw)]
            if ingress == "open-responses":
                bodies = [value.get("response", value) for value in values if isinstance(value, dict)]
                row["generation_id"] = next((value["id"] for value in reversed(bodies) if value.get("id")), None)
            row["client_output_span_seconds"] = visible_times[-1] - visible_times[0] if len(visible_times) > 1 else None
            row["client_output_tokens_per_second"] = (
                (row["actual_output_tokens"] - 1) / row["client_output_span_seconds"]
                if row["client_output_span_seconds"] else None)
            if response.status != 200 or not row["content_valid"]:
                row["error"] = f"HTTP {response.status}; complete output mismatch={not row['content_valid']}"
            if stream and row["ttft_ms"] is None:
                row["error"] = row["error"] or "no visible text in stream"
            if row["error"] is None:
                state.update(history=plan.messages + (Message("assistant", text),),
                             parent_nonce=nonce, turn=state["turn"] + 1)
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
                if isinstance(upstream, Upstream):
                    upstream.bound_plans.pop(nonce, None)
            if row["error"]:
                state.update(history=(), parent_nonce=None, turn=0, chain=state["chain"] + 1)

    def worker(worker_index: int) -> None:
        state = {"worker": worker_index, "chain": 0, "turn": 0, "history": (), "parent_nonce": None}
        index = worker_index
        try:
            if mode == "fixed-workers":
                while (index < limit if limit is not None else time.perf_counter() < deadline):
                    send(index, time.perf_counter(), state)
                    index += args.concurrency
                return
            while True:
                item = tasks[worker_index].get()
                if item is None:
                    return
                index, scheduled = item
                send(index, scheduled, state)
        except Exception as error:
            with lock:
                worker_errors.append(f"worker {worker_index}, request {index}: {type(error).__name__}: {error}")

    workers = [threading.Thread(target=worker, args=(index,)) for index in range(args.concurrency)]
    for thread in workers:
        thread.start()
    scheduled_count = 0
    max_queue = 0
    if mode == "open-loop":
        scheduled_count = limit if limit is not None else math.ceil(duration * args.qps)
        for index in range(scheduled_count):
            scheduled = started + index / args.qps
            time.sleep(max(0, scheduled - time.perf_counter()))
            tasks[index % args.concurrency].put((index, scheduled))
            max_queue = max(max_queue, sum(task.qsize() for task in tasks))
        for task in tasks:
            task.put(None)
    for thread in workers:
        thread.join()
    elapsed = time.perf_counter() - started
    with upstream.lock:
        arrivals = list(upstream.arrivals[arrivals_start:])
        if isinstance(upstream, Upstream):
            upstream.response_histories.clear()
    counts = Counter(row["nonce"] for row in arrivals)
    by_nonce = {row["nonce"]: row for row in arrivals}
    for row in rows:
        arrival = by_nonce.get(row["nonce"], {})
        row["wire_digests"] = {
            "client_to_platform": row["request_wire_sha256"],
            "platform_to_upstream": arrival.get("request_wire_sha256"),
            "upstream_to_platform": arrival.get("response_wire_sha256"),
            "platform_to_client": row.get("response_wire_sha256"),
        }
    failures = [row for row in rows if row["error"] or counts[row["nonce"]] != 1]
    reasons = list(worker_errors)
    if failures or any(not row["valid"] or row.get("error") for row in arrivals):
        reasons.append("request/upstream validation failed, missing or duplicate arrival")
    if not rows or limit is not None and len(rows) != limit:
        reasons.append("empty or incomplete request cohort")
    if mode == "open-loop" and len(rows) != scheduled_count:
        reasons.append("scheduled requests missing")
    start_lag = percentile([row["start_lag_ms"] for row in rows], .95)
    if mode == "open-loop" and start_lag is not None and start_lag > args.max_start_lag_ms:
        reasons.append("offered request rate not sustained: p95 admission lag exceeds budget")
    if stream and any(row.get("pacing_lag_max_ms", 0) > args.max_pacing_lag_ms for row in arrivals):
        reasons.append("synthetic upstream missed token pacing budget")
    continuations = sum(row["expected_parent_nonce"] is not None for row in rows)
    if phase == "measure" and args.chain_turns > 1 and not continuations:
        reasons.append("no multi-turn continuation exercised; increase duration/request count or reduce concurrency")
    success = [row for row in rows if not row["error"] and counts[row["nonce"]] == 1]
    return {"phase": phase, "mode": mode, "ingress": ingress, "upstream": upstream.protocol,
            "stream": stream, "duration_seconds": duration, "elapsed_including_drain_seconds": elapsed,
            "throughput_scope": "finite cohort including drain; not steady-state completion rate",
            "target_qps": args.qps if mode == "open-loop" else None,
            "actual_qps": len(rows) / elapsed, "successful_qps": len(success) / elapsed,
            "scheduled_requests": scheduled_count if mode == "open-loop" else len(rows),
            "completed_requests": len(rows), "continuation_requests": continuations, "errors": len(failures),
            "status_counts": dict(Counter(str(row["status"]) for row in rows)),
            "peak_inflight": peak, "peak_loadgen_queue": max_queue, "valid": not reasons,
            "failure_reasons": reasons, "latency_ms": {
                name: {"p50": percentile(values, .5), "p95": percentile(values, .95), "p99": percentile(values, .99)}
                for name, field in (("end_to_end", "end_to_end_ms"), ("service", "service_ms"),
                                    ("ttft", "ttft_ms"), ("queue", "queue_ms"), ("preparation", "preparation_ms"),
                                    ("admission", "start_lag_ms"))
                for values in [[row[field] for row in success if row[field] is not None]]},
            "requests": sorted(rows, key=lambda row: row["index"]), "upstream_arrivals": arrivals}


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
    parser.add_argument("--duration", type=float, default=120, help="Admission window; in-flight requests drain afterwards")
    parser.add_argument("--warmup", type=float, default=5)
    parser.add_argument("--qps", type=float, default=.5, help="Open-loop requests/sec, not conversations/sec")
    parser.add_argument("--concurrency", type=int, default=32)
    parser.add_argument("--requests-per-case", type=int, help="Finite measured cohort instead of duration admission limit")
    parser.add_argument("--seed", type=int, default=1)
    parser.add_argument("--chain-turns", type=int, default=2)
    for name, low, high, kind in (
        ("input-tokens", 50_000, 150_000, int), ("images", 0, 10, int),
        ("first-token-delay", 1, 10, float), ("tokens-per-second", 30, 100, int),
        ("duration", 10, 60, int),
    ):
        parser.add_argument("--min-" + name, type=kind, default=low)
        parser.add_argument("--max-" + name, type=kind, default=high)
    parser.add_argument("--image-size", type=int, default=512)
    parser.add_argument("--sample-interval", type=float, default=.25)
    parser.add_argument("--request-timeout", type=float, default=120)
    parser.add_argument("--audit-timeout", type=float, default=180)
    parser.add_argument("--max-start-lag-ms", type=float, default=1000)
    parser.add_argument("--max-pacing-lag-ms", type=float, default=250)
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
    for name in ("duration", "qps", "concurrency", "sample_interval", "request_timeout",
                 "audit_timeout", "max_start_lag_ms", "max_pacing_lag_ms"):
        if not math.isfinite(getattr(args, name)) or getattr(args, name) <= 0:
            parser.error(f"--{name.replace('_', '-')} must be positive and finite")
    if args.warmup < 0 or not math.isfinite(args.warmup) or args.chain_turns < 2:
        parser.error("warmup must be finite/nonnegative; chain-turns must be at least 2")
    if args.requests_per_case is not None and args.requests_per_case < args.chain_turns * args.concurrency:
        parser.error("requests-per-case must cover chain-turns * concurrency, including real continuations")
    for name, lower, upper in (("input_tokens", 50_000, 150_000), ("images", 0, 10),
                               ("first_token_delay", 1, 10), ("tokens_per_second", 30, 100), ("duration", 10, 60)):
        low, high = getattr(args, "min_" + name), getattr(args, "max_" + name)
        if not lower <= low <= high <= upper:
            parser.error(f"{name} bounds must remain within [{lower}, {upper}]")
    reserve = (args.chain_turns - 1) * (args.max_tokens_per_second * args.max_duration + 512)
    if args.min_input_tokens + reserve > args.max_input_tokens:
        parser.error("input token range cannot reserve the requested continuation budget")
    if args.image_size < 2:
        parser.error("--image-size must be at least 2")
    if args.backend == "sqlite" and args.database_url:
        parser.error("--database-url requires --backend postgres")
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
    args.workload_generator = Workload(credential_probes=True, **{key: value for key, value in vars(args).items()
                                         if key == "seed" or key == "image_size" or key.startswith(("min_input_", "max_input_", "min_images", "max_images", "min_first_", "max_first_", "min_tokens_", "max_tokens_", "min_duration", "max_duration"))})
    result = {"schema_version": 5, "response_validation": "full-chain-credentials-token-paced-v3",
              "run_id": run_id, "timestamp": time.time(), "binary_sha256": sha,
              "platform": platform.platform(), "logical_cpus": os.cpu_count(), "workload": metadata,
              "tokenizer": {"package": "tiktoken", "version": "0.14.0", "encoding": "cl100k_base",
                            "input_scope": "full-history client text before credential protection; upstream_input_tokens counts protected model text; both exclude framing and image tokens",
                            "output_scope": "paced model output tokens before credential restoration; client_output_tokens counts restored client text",
                            "sampling": "seeded per request; input/image budgets conditioned on retained history and reserved continuation space"},
              "debug_observation": True, "ordinary_observation": True,
              "credential_protection": {"enabled": True, "new_synthetic_secrets_per_request": 2,
                                        "repeated_and_echoed": True,
                                        "wire_policy": "Authorization header only; ADR-0063"},
              "cpu_units": "core-percent; 100 = one CPU core",
              "memory_units": "bytes; decimal MB = bytes/1000000; MiB = bytes/1048576",
              "cases": [], "samples": [], "fatal_error": None}
    upstreams = {protocol: Upstream(protocol, args.workload_generator) for protocol in args.upstream}
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
            status, _ = session.request("PUT", "/api/v1/settings/reversible_redaction_enabled",
                                        {"value": "true"})
            if status != 200:
                raise RuntimeError(f"credential protection setup HTTP {status}")
            status, protected = session.request("GET", "/api/v1/settings/reversible_redaction_enabled")
            if status != 200 or protected.get("data") != "true":
                raise RuntimeError("credential protection was not enabled")
            status, debug = session.request("GET", "/api/v1/observations/debug")
            if status != 200 or debug["data"]["enabled"]:
                raise RuntimeError("isolated benchmark expected a fresh Observation Debug session")
            key, routes = provisioning(session, upstreams)
            result["route_ids"] = routes
            def performance_snapshot():
                status, snapshot = session.request("GET", "/api/v1/performance/timeline", timeout=args.audit_timeout)
                if status != 200:
                    raise RuntimeError(f"performance timeline HTTP {status}")
                return snapshot
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
                                # 新 Debug epoch 隔离前一批的容量与丢失计数；绝不在请求执行中切换。
                                for enabled in (False, True):
                                    status, _ = session.request("PUT", "/api/v1/observations/debug",
                                                                {"enabled": enabled, "confirmed": True})
                                    if status != 200:
                                        raise RuntimeError(f"Debug session setup HTTP {status}")
                                context.update(case=case_id, phase=phase)
                                case[phase] = run_load(args, base, key, ingress, upstream, delivery == "stream",
                                                       mode, duration, phase, routes[protocol])
                                context.update(phase=phase + "-audit")
                                case[phase]["chain_audit"] = audit_requests(
                                    session, case[phase]["requests"], routes, timeout=args.audit_timeout,
                                    require_debug=True, performance_snapshot=performance_snapshot,
                                    history_evidence=lambda ids: read_history_evidence(
                                        args, directory, ids, allow_legacy=True),
                                    credential_expectations=upstream.credential_expectations,
                                    credential_evidence=lambda expected: read_credential_evidence(args, directory, expected))
                                upstream.credential_expectations.clear()
                                if not case[phase]["chain_audit"]["full_chain_verified"]:
                                    case[phase]["valid"] = False
                                    case[phase]["failure_reasons"].append("per-request full-chain evidence incomplete or inconsistent")
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
            # Inventory is outside measured traffic; ordinary SQL/Observation
            # and Generation Chain storage remain enabled for the entire run.
            if args.backend == "sqlite":
                database_path = Path(directory) / "db" / "gateway.db"
                with closing(sqlite3.connect(f"file:{database_path.as_posix()}?mode=ro", uri=True)) as connection:
                    connection.execute("BEGIN")
                    tables = {row[0] for row in connection.execute("SELECT name FROM sqlite_master WHERE type='table'")}
                    # Table presence selects normalized v3 or the frozen legacy
                    # layout so baseline binaries remain measurable.
                    schema = history_storage_schema(tables)
                    if schema == NORMALIZED_HISTORY:
                        history_tables = sorted(table for table in tables if table.startswith("turn_"))
                        payload_sources = (("turn_node_payloads", "payload"), ("turn_contents", "content"))
                    else:
                        history_tables = sorted(table for table in tables if table.startswith("turn_chain_"))
                        payload_sources = (("turn_chain_nodes", "payload"), ("turn_chain_contents", "content"))
                    result["storage_inventory"] = {
                        table: connection.execute(f'SELECT COUNT(*) FROM "{table}"').fetchone()[0]
                        for table in sorted(tables)
                        if "observation" in table or table in history_tables
                    }
                    result["storage_bytes"] = {
                        "history_schema": schema,
                        "encoded_payloads": {
                            table: connection.execute(f'SELECT COALESCE(SUM(length("{column}")), 0) FROM "{table}"').fetchone()[0]
                            for table, column in (*payload_sources, ("observation_events", "payload"))
                        },
                    }
                    connection.execute("COMMIT")
                sqlite_files = {}
                for name, path in (
                    ("database", database_path),
                    ("wal", database_path.with_name(database_path.name + "-wal")),
                    ("shm", database_path.with_name(database_path.name + "-shm")),
                ):
                    try:
                        sqlite_files[name] = path.stat().st_size
                    except FileNotFoundError:
                        sqlite_files[name] = 0
                result["storage_bytes"]["sqlite_files"] = sqlite_files
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
        instructions = {upstream.gateway_instruction for upstream in upstreams.values()}
        result["gateway_instruction_consistent"] = len(instructions) == 1 and None not in instructions
        if not result["gateway_instruction_consistent"]:
            result["valid"] = False
        def redact(line: str) -> str:
            line = re.sub(r"(Stravia setup token:)\s*\S+", r"\1 [redacted]", line)
            for secret in (args.database_url, args.redis_url):
                if secret:
                    line = line.replace(secret, "[connection URL omitted]")
            return line
        synthetic_secrets = {secret for upstream in upstreams.values()
                             for secret in upstream.synthetic_secrets}
        secret_log_leaks = 0
        safe_logs = []
        for line in logs:
            leaked = False
            for secret in synthetic_secrets:
                if secret in line:
                    leaked = True
                    line = line.replace(secret, "[synthetic credential omitted]")
            secret_log_leaks += leaked
            safe_logs.append(redact(line))
        result["server_logs"] = safe_logs
        result["credential_log_leaks"] = secret_log_leaks
        if secret_log_leaks:
            result["valid"] = False
        if result["fatal_error"]:
            result["fatal_error"] = redact(result["fatal_error"])
        args.output.write_text(json.dumps(result, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps({"output": str(args.output), "valid": result["valid"], "fatal_error": result["fatal_error"], "cases": len(result["cases"])}))
    return 0 if result["valid"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
