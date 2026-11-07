from __future__ import annotations

import base64
import http.client
import json
import re
import socket
import threading
from concurrent.futures import ThreadPoolExecutor
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from queue import Queue
from socketserver import StreamRequestHandler, ThreadingTCPServer
from typing import Any

import pytest

from tests.common.helpers import (
    WebSession,
    find_free_port,
    http_bytes,
    http_request,
    initialize_server,
    start_stravia_server,
    stop_stravia_server,
    wait_for_setup_token,
    wait_until_ready,
)
from tests.e2e.storage.test_vendor_plugins import _install_plugin

PNG = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII="


class HandoffController(ThreadingTCPServer):
    daemon_threads = True

    def __init__(self) -> None:
        super().__init__(("127.0.0.1", 0), HandoffHandler)
        self.armed = False
        self.lock = threading.Lock()
        self.entered: Queue[tuple[str, threading.Event, Queue[bytes]]] = Queue()
        self.releases: list[threading.Event] = []


class HandoffHandler(StreamRequestHandler):
    server: HandoffController

    def handle(self) -> None:
        execution_id = self.rfile.readline(64).decode("ascii").strip()
        assert re.fullmatch("[a-z]{28}", execution_id), execution_id
        release = None
        finished: Queue[bytes] = Queue()
        with self.server.lock:
            if self.server.armed:
                self.server.armed = False
                release = threading.Event()
                self.server.releases.append(release)
                self.server.entered.put((execution_id, release, finished))
        if release is not None:
            release.wait()
        self.request.sendall(b"\x01")
        finished.put(self.rfile.read(1))


class ToolUpstream(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self) -> None:
        super().__init__(("127.0.0.1", 0), ToolHandler)
        self.images: Queue[threading.Event] = Queue()
        self.releases: list[threading.Event] = []
        self.image_requests: list[dict[str, Any]] = []
        self.lock = threading.Lock()

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.server_port}"


class ToolHandler(BaseHTTPRequestHandler):
    server: ToolUpstream

    def log_message(self, *_args: Any) -> None:
        pass

    def send_json(self, value: Any) -> None:
        data = json.dumps(value).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        try:
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError):
            # 本地取消或进程中断后，上游仍可能完成；这不代表宿主接受了结果。
            pass

    def do_GET(self) -> None:
        self.send_json({})

    def do_POST(self) -> None:
        request = json.loads(self.rfile.read(int(self.headers["content-length"])))
        if self.path == "/image":
            release = threading.Event()
            with self.server.lock:
                self.server.image_requests.append(request)
                self.server.releases.append(release)
            self.server.images.put(release)
            if not release.wait(240):
                return
            self.send_json({
                "artifacts": [{"media_type": "image/png", "bytes": PNG,
                               "upstream_ref": None, "metadata": {}}],
                "revised_prompt": None, "usage": None,
            })
            return
        assert self.path == "/infer", self.path
        results = [
            block["content"]
            for item in request.get("items", [])
            for block in (item["content"] if isinstance(item.get("content"), list) else [])
            if block.get("type") == "tool_result"
        ]
        if results:
            items = [{"role": "assistant", "content": json.dumps(results[-1])}]
        else:
            name = next(tool["name"] for tool in request["tools"] if "generate" in tool["name"])
            items = [{"role": "assistant", "content": "", "tool_calls": [{
                "id": "fixture_generate", "name": name,
                "arguments": json.dumps({"type": "image", "input": {"prompt": "source-owned square"}}),
            }]}]
        self.send_json({
            "id": "fixture-infer", "model": "fixture-model", "items": items,
            "stop_reason": "stop", "embedding_output": None, "error": None,
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2,
                      "required_components_known": True},
            "vendor": {"ingress": {}, "egress": {}, "passthrough_safe": {}},
        })


@pytest.mark.storage
@pytest.mark.parametrize("backend", ["sqlite", "postgres"], ids=["sqlite", "postgres"])
def test_public_tool_execution_cleanup_and_restart(
    stravia_binary: Path, repo_root: Path, tmp_path: Path,
    storage_runtime: dict[str, Any], backend: str,
) -> None:
    upstream = ToolUpstream()
    upstream_thread = threading.Thread(target=upstream.serve_forever, daemon=True)
    upstream_thread.start()
    handoff = HandoffController()
    handoff_thread = threading.Thread(target=handoff.serve_forever, daemon=True)
    handoff_thread.start()
    pool = ThreadPoolExecutor(max_workers=3)
    process = None
    logs: list[str] = []
    schema: str | None = None
    port = find_free_port()
    origin = f"http://127.0.0.1:{port}"
    args = ["--host", "127.0.0.1", "--port", str(port), "--data-dir", str(tmp_path / backend),
            "--test-handoff-barrier", f"127.0.0.1:{handoff.server_address[1]}",
            "--test-catalog-base-url", upstream.url, *storage_runtime["server_args"](backend)]

    def start() -> None:
        nonlocal process, logs
        process, logs = start_stravia_server(stravia_binary=stravia_binary, args=args)
        wait_until_ready(origin + "/api/v1/auth/state")

    def admin(session: WebSession, method: str, path: str, payload: Any = None) -> Any:
        status, body = session.request(method, path, payload, timeout=60)
        assert status in (200, 201, 204), (method, path, status, body)
        assert not isinstance(body, dict) or "error" not in body, (method, path, body)
        return body.get("data", body) if isinstance(body, dict) else body

    def call(key: str, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
        status, body = http_request("POST", origin + "/mcp", {
            "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
                "name": name, "arguments": arguments, "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
            },
        }, headers={
            "authorization": "Bearer " + key, "accept": "application/json, text/event-stream",
            "MCP-Protocol-Version": "2026-07-28", "Mcp-Method": "tools/call", "Mcp-Name": name,
        }, timeout=150)
        assert status == 200 and "result" in body, (status, body)
        return body["result"]

    def read(key: str, path: str, wait: int | None = None) -> dict[str, Any]:
        arguments: dict[str, Any] = {"path": path}
        if wait is not None:
            arguments["wait"] = wait
        return call(key, "StraviaRead", arguments)

    try:
        database: dict[str, Any] = {"backend": "sqlite"}
        if backend == "postgres":
            schema = storage_runtime["make_isolated_schema"]("stravia_tool_executions")
            storage_runtime["run_schema_action"]("create", pg_url=storage_runtime["pg_url"], schema=schema)
            database = {"backend": "postgres", "url": storage_runtime["postgres_dsn_for_schema"](
                storage_runtime["pg_url"], schema,
            )}
        start()
        assert process is not None
        session = initialize_server(origin, wait_for_setup_token(logs, process), database)
        _install_plugin(session, repo_root / "target" / "vendor-test-fixtures", "capability-multi.wasm")
        provider = admin(session, "POST", "/api/v1/providers", {
            "name": "Isolated tool upstream", "source": {"type": "custom",
                "vendor": "fixture.capability-contract", "channel": "default", "base_url": upstream.url},
            "credential": {"type": "fields", "values": {}}, "vendor_options": {},
        })
        admin(session, "POST", f"/api/v1/providers/{provider['id']}/models", {
            "model_id": "fixture-model", "metadata": {"id": "fixture-model", "name": "Fixture model",
                "tool_call": True, "capabilities": ["infer", "media_image"],
                "modalities": {"input": ["text"], "output": ["text", "image"]}},
        })
        routes = {}
        for modality in ["image", "text"]:
            routes[modality] = admin(session, "POST", "/api/v1/models", {
                "model_id": "fixture-" + modality, "output_modality": modality,
                "targets": [{"provider_id": provider["id"], "model": "fixture-model",
                             "first_token_timeout_ms": 200_000}],
            })
        admin(session, "PUT", "/api/v1/media-generation/config", {
            "enabled": True, "image": {"route_id": routes["image"]["model_id"]},
        })
        key = admin(session, "POST", "/api/v1/api-keys", {
            "name": "Tool execution caller", "model_ids": [routes["text"]["id"]],
            "mcp_access_enabled": True, "transparent_injection_enabled": False,
        })["key"]
        other = admin(session, "POST", "/api/v1/api-keys", {
            "name": "Other Principal", "model_ids": [], "mcp_access_enabled": True,
        })["key"]
        first = pool.submit(call, key, "generate", {"type": "image", "input": {"prompt": "durable square"}})
        release_first = upstream.images.get(timeout=30)

        def inference() -> tuple[dict[str, Any], dict[str, Any]]:
            status, body = http_request("POST", origin + "/v1/chat/completions", {
                "model": routes["text"]["model_id"], "messages": [{"role": "user", "content": "Generate a square"}],
                "tools": [{"type": "function", "function": {"name": "generate", "parameters": {"type": "object"}}}],
                "stream": False,
            }, headers={"authorization": "Bearer " + key}, timeout=150)
            assert status == 200, (status, body)
            message = body["choices"][0]["message"]
            assert not message.get("tool_calls"), message
            return json.loads(message["content"]), message

        source = pool.submit(inference)
        first_path = first.result(timeout=90)["structuredContent"]["path"]
        source_delivery, source_message = source.result(timeout=90)
        source_path = source_delivery["path"]
        # 来源任务已排队并交接后才提交下一项，明确 gate 顺序，不依赖线程调度。
        pending = pool.submit(call, key, "generate", {"type": "image", "input": {"prompt": "interrupted square"}})
        pending_path = pending.result(timeout=90)["structuredContent"]["path"]
        assert read(key, first_path)["structuredContent"]["status"] == "running"
        assert read(key, source_path)["structuredContent"]["status"] == "pending"
        assert read(key, pending_path)["structuredContent"]["status"] == "pending"
        denied = read(other, first_path)
        missing = read(other, "stravia://tool-executions/" + "z" * 28)
        assert denied["isError"] and missing["isError"]
        assert denied["structuredContent"] == missing["structuredContent"]

        release_first.set()
        completed = read(key, first_path, wait=120)["structuredContent"]
        assert completed["status"] == "completed", completed
        assert completed["result"]["media"] == {"width": 1, "height": 1}
        release_source = upstream.images.get(timeout=30)
        assert read(key, source_path)["structuredContent"]["status"] == "running"
        cleared = admin(session, "DELETE", "/api/v1/observations/history")
        assert cleared["deleted_interactions"] > 0 and cleared["skipped_active"] == 0, cleared
        assert read(key, source_path)["structuredContent"]["status"] == "running"
        assert read(key, pending_path)["structuredContent"]["status"] == "pending"
        # 诊断清理不释放执行门、不取消已交接的领域工作。
        release_source.set()
        source_completed = read(key, source_path, wait=120)["structuredContent"]
        assert source_completed["status"] == "completed", source_completed
        assert source_completed["result"]["media"] == {"width": 1, "height": 1}
        release_pending = upstream.images.get(timeout=30)
        status, replayed = http_request("POST", origin + "/v1/chat/completions", {
            "model": routes["text"]["model_id"],
            "messages": [
                {"role": "user", "content": "Generate a square"},
                source_message,
                {"role": "user", "content": "Read the previously delivered tool outcome"},
            ],
            "stream": False,
        }, headers={"authorization": "Bearer " + key}, timeout=30)
        assert status == 200, replayed
        assert json.loads(replayed["choices"][0]["message"]["content"]) == source_delivery
        assert process is not None
        process.kill()
        process.wait(timeout=15)
        start()
        interrupted = read(key, pending_path)
        assert not interrupted["isError"]
        assert interrupted["structuredContent"]["status"] == "interrupted"
        assert interrupted["structuredContent"]["error"]
        assert read(key, source_path)["structuredContent"] == source_completed
        assert read(key, first_path)["structuredContent"] == completed
        grant = read(key, completed["result"]["path"] + ":download")["structuredContent"]
        status, _, image = http_bytes("GET", grant["download_url"])
        assert status == 200 and image == base64.b64decode(PNG)
        assert [request["prompt"] for request in upstream.image_requests] == [
            "durable square", "source-owned square", "interrupted square",
        ], "queries and restart must not repeat an upstream operation"
        release_pending.set()
        connection = http.client.HTTPConnection("127.0.0.1", port, timeout=61)

        def disconnecting_generate() -> None:
            try:
                connection.request("POST", "/mcp", json.dumps({
                    "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
                        "name": "generate", "arguments": {
                            "type": "image", "input": {"prompt": "disconnected square"},
                        }, "_meta": {
                            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                            "io.modelcontextprotocol/clientCapabilities": {},
                        },
                    },
                }), headers={
                    "content-type": "application/json", "authorization": "Bearer " + key,
                    "accept": "application/json, text/event-stream",
                    "MCP-Protocol-Version": "2026-07-28",
                    "Mcp-Method": "tools/call", "Mcp-Name": "generate",
                })
                response = connection.getresponse()
                raise AssertionError(("handoff returned before the controlled disconnect", response.status))
            finally:
                connection.close()

        with handoff.lock:
            handoff.armed = True
        disconnect = pool.submit(disconnecting_generate)
        release_disconnected = upstream.images.get(timeout=30)
        # 等真实默认窗口结束并到达提交点，不用全局写锁阻断无关的租约续期。
        disconnected_id, release_handoff, handoff_finished = handoff.entered.get(timeout=90)
        assert connection.sock is not None
        connection.sock.shutdown(socket.SHUT_RDWR)
        with pytest.raises((http.client.RemoteDisconnected, ConnectionResetError)):
            disconnect.result(timeout=30)
        following = pool.submit(call, key, "generate", {
            "type": "image", "input": {"prompt": "after disconnect"},
        })
        # handoff 和旧上游仍阻塞；新上游受理证明断线已释放本地执行门。
        release_following = upstream.images.get(timeout=30)
        release_handoff.set()
        assert handoff_finished.get(timeout=30) == b"\x00", "cancelled execution must not be handed off"
        cancelled_execution = read(key, f"stravia://tool-executions/{disconnected_id}")
        assert cancelled_execution["structuredContent"]["status"] == "failed", cancelled_execution
        release_disconnected.set()
        release_following.set()
        next_result = following.result(timeout=30)
        assert not next_result["isError"]
        assert next_result["structuredContent"]["media"] == {"width": 1, "height": 1}
        assert [request["prompt"] for request in upstream.image_requests][3:] == [
            "disconnected square", "after disconnect",
        ]

        connection = http.client.HTTPConnection("127.0.0.1", port, timeout=30)
        response = None
        try:
            connection.request("POST", "/v1/chat/completions", json.dumps({
                "model": routes["text"]["model_id"],
                "messages": [{"role": "user", "content": "Generate a cancellable square"}],
                "tools": [{"type": "function", "function": {
                    "name": "generate", "parameters": {"type": "object"},
                }}],
                "stream": True,
            }), headers={"content-type": "application/json", "authorization": "Bearer " + key})
            response = connection.getresponse()
            assert response.status == 200
            projected = ""
            marker = None
            while marker is None:
                line = response.readline()
                assert line, "stream ended before publishing the hidden tool Marker"
                if not line.startswith(b"data: "):
                    continue
                event = json.loads(line[6:])
                delta = event["choices"][0]["delta"] if event.get("choices") else {}
                projected += delta.get("reasoning_content") or ""
                projected += delta.get("content") or ""
                marker = re.search(r"<!--sh:[a-z]{28}-->", projected)
            cancelled_marker = marker.group()
            release_cancelled = upstream.images.get(timeout=30)
            if connection.sock is not None:
                connection.sock.shutdown(socket.SHUT_RDWR)
        finally:
            if response is not None:
                response.close()
            connection.close()

        following = pool.submit(call, key, "generate", {
            "type": "image", "input": {"prompt": "after marker disconnect"},
        })
        # 后续调用被供应商受理，证明断连已释放本地执行门；不要求上游撤销旧请求。
        release_following = upstream.images.get(timeout=30)
        release_cancelled.set()
        release_following.set()
        assert not following.result(timeout=30)["isError"]
        requests_before_restore = len(upstream.image_requests)
        status, restored = http_request("POST", origin + "/v1/chat/completions", {
            "model": routes["text"]["model_id"],
            "messages": [
                {"role": "user", "content": "Generate a cancellable square"},
                {"role": "assistant", "content": None, "reasoning_content": cancelled_marker},
                {"role": "user", "content": "Read the previous tool outcome"},
            ],
            "stream": False,
        }, headers={"authorization": "Bearer " + key}, timeout=30)
        assert status == 200, restored
        cancelled = json.loads(restored["choices"][0]["message"]["content"])
        assert cancelled["error"]["code"] == "cancelled", cancelled
        assert len(upstream.image_requests) == requests_before_restore, "Marker recovery must not replay generation"
    finally:
        with handoff.lock:
            handoff.armed = False
            for release in handoff.releases:
                release.set()
        for release in upstream.releases:
            release.set()
        if process is not None:
            stop_stravia_server(process, logs)
        pool.shutdown(wait=True)
        upstream.shutdown()
        upstream.server_close()
        upstream_thread.join(timeout=10)
        handoff.shutdown()
        handoff.server_close()
        handoff_thread.join(timeout=10)
        if schema is not None:
            storage_runtime["run_schema_action"]("drop", pg_url=storage_runtime["pg_url"], schema=schema)
