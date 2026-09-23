from __future__ import annotations

import base64
import http.client
import io
import json
import os
import socket
import struct
import threading
import time
import zipfile
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any
from urllib.parse import urlparse

import pytest

from tests.common.helpers import (
    download_observation_bundle,
    http_bytes,
    http_request,
    observation_bundle_events,
)
from tests.e2e.admin.test_observations import (
    _create_route,
    _detail,
    _forest,
    _route_interactions,
    _sse_event,
    _wait_for,
)


class _WireSseProvider(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, _format: str, *_args: object) -> None:
        pass

    def do_POST(self) -> None:  # noqa: N802
        size = int(self.headers.get("content-length", "0"))
        body = self.rfile.read(size)
        self.server.received.append(body)  # type: ignore[attr-defined]
        request = json.loads(body)
        model = request["model"]

        if b"nonstream-error-wire" in body:
            self._respond(502, "application/json", [b'{"error":{"message":"broken'])
            return
        if b"barrier-wire" in body:
            self.server.upstream_ready.set()  # type: ignore[attr-defined]
            if not self.server.prefix_release.wait(timeout=15):  # type: ignore[attr-defined]
                return
            payload = {
                "id": "chatcmpl-barrier",
                "object": "chat.completion.chunk",
                "model": model,
                "choices": [
                    {
                        "index": 0,
                        "delta": {
                            "role": "assistant",
                            "content": "barrier-prefix-and-tail",
                        },
                        "finish_reason": "stop",
                    }
                ],
            }
            event = b"data: " + json.dumps(payload, separators=(",", ":")).encode() + b"\n\n"
            split = event.index(b"barrier-prefix") + len(b"barrier-prefix")
            prefix = event[:split]
            tail = event[split:] + b"data: [DONE]\n\n"
            self.server.sent.append(prefix + tail)  # type: ignore[attr-defined]
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("content-length", str(len(prefix) + len(tail)))
            self.send_header("connection", "close")
            self.end_headers()
            self.wfile.write(prefix)
            self.wfile.flush()
            self.server.barrier_prefix = prefix  # type: ignore[attr-defined]
            self.server.barrier_started.set()  # type: ignore[attr-defined]
            if not self.server.barrier_release.wait(timeout=15):  # type: ignore[attr-defined]
                return
            self.wfile.write(tail)
            self.wfile.flush()
            return
        if b"nonstream-wire" in body:
            response = {
                "id": "chatcmpl-http-wire",
                "object": "chat.completion",
                "model": model,
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": "http-wire-once"},
                        "finish_reason": "stop",
                    }
                ],
                "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4},
            }
            self._respond(
                200,
                "application/json",
                [json.dumps(response, separators=(",", ":")).encode()],
            )
            return
        if b"malformed-wire" in body:
            self._respond(200, "text/event-stream", [b'data: {"id":"truncated"\n\n'])
            return

        payloads = [
            {
                "id": "chatcmpl-wire",
                "object": "chat.completion.chunk",
                "model": model,
                "choices": [
                    {
                        "index": 0,
                        "delta": {"role": "assistant", "content": "wire-one"},
                        "finish_reason": None,
                    }
                ],
            },
            {
                "id": "chatcmpl-wire",
                "object": "chat.completion.chunk",
                "model": model,
                "choices": [
                    {
                        "index": 0,
                        "delta": {"content": "-two"},
                        "finish_reason": None,
                    }
                ],
            },
            {
                "id": "chatcmpl-wire",
                "object": "chat.completion.chunk",
                "model": model,
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5},
            },
        ]
        chunks = [
            b"data: " + json.dumps(payload, separators=(",", ":")).encode() + b"\n\n"
            for payload in payloads
        ] + [b"data: [DONE]\n\n"]
        self._respond(200, "text/event-stream", chunks)

    def _respond(self, status: int, content_type: str, chunks: list[bytes]) -> None:
        self.server.sent.append(b"".join(chunks))  # type: ignore[attr-defined]
        self.send_response(status)
        self.send_header("content-type", content_type)
        self.send_header("content-length", str(sum(map(len, chunks))))
        self.send_header("connection", "close")
        self.end_headers()
        for chunk in chunks:
            self.wfile.write(chunk)
            self.wfile.flush()


def _start_provider() -> tuple[ThreadingHTTPServer, threading.Thread]:
    provider = ThreadingHTTPServer(("127.0.0.1", 0), _WireSseProvider)
    provider.received = []  # type: ignore[attr-defined]
    provider.sent = []  # type: ignore[attr-defined]
    provider.barrier_prefix = b""  # type: ignore[attr-defined]
    provider.upstream_ready = threading.Event()  # type: ignore[attr-defined]
    provider.prefix_release = threading.Event()  # type: ignore[attr-defined]
    provider.barrier_started = threading.Event()  # type: ignore[attr-defined]
    provider.barrier_release = threading.Event()  # type: ignore[attr-defined]
    worker = threading.Thread(target=provider.serve_forever, daemon=True)
    worker.start()
    return provider, worker


def _create_media_route(env: dict[str, Any], name: str) -> tuple[str, str]:
    status, body = http_request(
        "POST",
        f"{env['admin']}/api/v1/providers",
        payload={
            "name": f"{name}-provider",
            "source": {
                "type": "custom",
                "vendor": "custom",
                "channel": "default",
                "protocol": "openai-compatible",
                "base_url": env["mock"],
            },
            "credential": {"type": "api_key", "value": "upstream-secret"},
            "vendor_options": {},
        },
        headers=env["auth"],
    )
    assert status == 200, body
    provider_id = body["data"]["id"]
    status, body = http_request(
        "POST",
        f"{env['admin']}/api/v1/providers/{provider_id}/models",
        payload={
            "model_id": "gpt-4o-mini",
            "metadata": {
                "name": name,
                "tool_call": True,
                "modalities": {"input": ["text", "image"], "output": ["text"]},
            },
        },
        headers=env["auth"],
    )
    assert status == 201, body
    status, body = http_request(
        "POST",
        f"{env['admin']}/api/v1/models",
        payload={
            "model_id": name,
            "display_name": f"{name} display",
            "targets": [
                {
                    "provider_id": provider_id,
                    "model": "gpt-4o-mini",
                    "enabled": True,
                    "priority": 0,
                    "target_retry_budget": 0,
                    "target_cooldown_ms": 0,
                }
            ],
        },
        headers=env["auth"],
    )
    assert status == 200, body
    route_id = body["data"]["id"]
    status, body = http_request(
        "POST",
        f"{env['admin']}/api/v1/api-keys",
        payload={"name": f"{name}-key", "model_ids": [route_id]},
        headers=env["auth"],
    )
    assert status == 200, body
    return route_id, str(body["data"]["key"])


def _set_debug(env: dict[str, Any], enabled: bool) -> None:
    status, body = http_request(
        "PUT",
        f"{env['admin']}/api/v1/observations/debug",
        payload={"enabled": enabled, "confirmed": enabled},
        headers=env["auth"],
    )
    assert status == 200, body


def _enable_debug(env: dict[str, Any]) -> None:
    _set_debug(env, True)


def _stream_request(
    env: dict[str, Any], api_key: str, body: bytes, marker: str
) -> tuple[int, bytes]:
    endpoint = urlparse(env["proxy"])
    connection = http.client.HTTPConnection(endpoint.hostname, endpoint.port, timeout=15)
    connection.request(
        "POST",
        "/v1/chat/completions?wire_query=preserved",
        body=body,
        headers={
            "Authorization": f"Bearer {api_key}",
            "Content-Type": "application/json",
            "X-Debug-Risk": marker,
        },
    )
    response = connection.getresponse()
    result = response.status, response.read()
    connection.close()
    return result


def _websocket_connection(env: dict[str, Any], api_key: str) -> socket.socket:
    endpoint = urlparse(env["proxy"])
    connection = socket.create_connection((endpoint.hostname, endpoint.port), timeout=15)
    key = base64.b64encode(os.urandom(16)).decode()
    connection.sendall(
        (
            f"GET /v1/responses HTTP/1.1\r\nHost: {endpoint.netloc}\r\n"
            f"Authorization: Bearer {api_key}\r\nUpgrade: websocket\r\n"
            f"Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n"
            f"Sec-WebSocket-Key: {key}\r\n\r\n"
        ).encode()
    )
    handshake = bytearray()
    while not handshake.endswith(b"\r\n\r\n"):
        chunk = connection.recv(1)
        assert chunk, "WebSocket closed during handshake"
        handshake.extend(chunk)
    assert b" 101 " in handshake, handshake
    return connection


def _websocket_send(connection: socket.socket, payload: dict[str, Any]) -> None:
    body = json.dumps(payload, separators=(",", ":")).encode()
    mask = os.urandom(4)
    header = bytes([0x81, 0x80 | (len(body) if len(body) < 126 else 126)])
    if len(body) >= 126:
        header += struct.pack("!H", len(body))
    connection.sendall(
        header + mask + bytes(value ^ mask[index % 4] for index, value in enumerate(body))
    )


def _websocket_completed(connection: socket.socket) -> None:
    def read_exact(size: int) -> bytes:
        result = bytearray()
        while len(result) < size:
            chunk = connection.recv(size - len(result))
            assert chunk, "WebSocket closed before response completed"
            result.extend(chunk)
        return bytes(result)

    while True:
        opcode, size = read_exact(2)
        assert not size & 0x80
        size &= 0x7F
        if size == 126:
            size = struct.unpack("!H", read_exact(2))[0]
        elif size == 127:
            size = struct.unpack("!Q", read_exact(8))[0]
        payload = read_exact(size)
        assert opcode & 0xF == 1, payload
        event = json.loads(payload)
        assert event["type"] not in {"error", "response.failed"}, event
        if event["type"] == "response.completed":
            return


def _duplicate_header_request(
    env: dict[str, Any], api_key: str, body: bytes, values: list[bytes]
) -> tuple[int, bytes]:
    endpoint = urlparse(env["proxy"])
    connection = http.client.HTTPConnection(endpoint.hostname, endpoint.port, timeout=15)
    connection.putrequest("POST", "/v1/chat/completions?ingress_capture=duplicate-headers")
    connection.putheader("Authorization", f"Bearer {api_key}")
    connection.putheader("Content-Type", "application/json")
    connection.putheader("Content-Length", str(len(body)))
    for value in values:
        connection.putheader("X-Raw-Duplicate", value)
    connection.endheaders(body)
    response = connection.getresponse()
    result = response.status, response.read()
    connection.close()
    return result


def _raw_ingress_request(
    env: dict[str, Any],
    api_key: str,
    body: bytes,
    marker: str,
    *,
    declared_length: int | None = None,
) -> bytes:
    endpoint = urlparse(env["proxy"])
    request = bytearray(
        (
            f"POST /v1/chat/completions?ingress_capture={marker} HTTP/1.1\r\n"
            f"Host: {endpoint.hostname}:{endpoint.port}\r\n"
            f"Authorization: Bearer {api_key}\r\n"
            "Content-Type: application/json\r\n"
            f"Content-Length: {declared_length if declared_length is not None else len(body)}\r\n"
            "Connection: close\r\n"
        ).encode()
    )
    request.extend(b"\r\n")
    request.extend(body)
    with socket.create_connection((endpoint.hostname, endpoint.port), timeout=10) as connection:
        connection.sendall(request)
        connection.shutdown(socket.SHUT_WR)
        connection.settimeout(10)
        response = bytearray()
        while True:
            try:
                chunk = connection.recv(64 * 1024)
            except (ConnectionResetError, socket.timeout):
                break
            if not chunk:
                break
            response.extend(chunk)
    return bytes(response)


def _rejection_ids(env: dict[str, Any]) -> set[str]:
    status, body = http_request(
        "GET",
        f"{env['admin']}/api/v1/observations/rejections",
        headers=env["auth"],
    )
    assert status == 200, body
    return {item["id"] for item in body["data"]["items"]}


def _new_rejection(
    env: dict[str, Any], before: set[str]
) -> dict[str, Any] | None:
    status, body = http_request(
        "GET",
        f"{env['admin']}/api/v1/observations/rejections",
        headers=env["auth"],
    )
    assert status == 200, body
    for item in body["data"]["items"]:
        if item["id"] in before or not item["debug_enabled"]:
            continue
        status, detail = http_request(
            "GET",
            f"{env['admin']}/api/v1/observations/rejections/{item['id']}",
            headers=env["auth"],
        )
        assert status == 200, detail
        result = detail["data"]
        if (result.get("trace") or {}).get("status") in {"complete", "partial"}:
            return result
    return None


def _terminal_detail(
    env: dict[str, Any], route_id: str, prompt: str
) -> dict[str, Any] | None:
    for interaction in _route_interactions(env, route_id):
        if interaction.get("input_preview") != prompt:
            continue
        detail = _detail(env, interaction["id"])
        runs = detail.get("runs", [])
        return detail if runs and all(run["status"] != "running" for run in runs) else None
    return None


def _final_detail(env: dict[str, Any], route_id: str, prompt: str) -> dict[str, Any] | None:
    detail = _terminal_detail(env, route_id, prompt)
    if detail is None:
        return None
    trace = detail["runs"][-1].get("trace") or {}
    return detail if trace.get("status") in {"complete", "partial"} else None


def _payload_bytes(records: list[dict[str, Any]], direction: str) -> bytes:
    result = bytearray()
    for record in sorted(records, key=lambda value: value["sequence"]):
        if record.get("direction") != direction:
            continue
        payload = record.get("payload")
        if payload is None:
            continue
        if record.get("payload_encoding") == "base64":
            assert isinstance(payload, str)
            result.extend(base64.b64decode(payload))
        elif isinstance(payload, str):
            result.extend(payload.encode())
        else:
            raise AssertionError(f"wire payload is not recoverable bytes: {record}")
    return bytes(result)


def _assert_wire_only(records: list[dict[str, Any]]) -> None:
    assert records
    assert {record["direction"] for record in records} == {
        "client_to_platform",
        "upstream_request",
        "upstream_response",
        "platform_to_client",
    }
    assert all(record.get("layer") == "wire" for record in records)
    assert all(record.get("stage") is None for record in records)


@pytest.mark.e2e
@pytest.mark.admin
def test_wire_bundle_is_four_direction_raw_only_and_malformed_capture_stays_complete(
    admin_env: dict[str, Any],
) -> None:
    provider, worker = _start_provider()
    env = {**admin_env, "mock": f"http://127.0.0.1:{provider.server_port}"}
    marker = "wire-risk-marker-kept-outside-authorization"
    try:
        _enable_debug(env)
        route_id, api_key = _create_route(env, "wire-only-main-seam", retry_budget=0)

        prompt = f"valid-wire {marker}"
        request_body = json.dumps(
            {
                "model": "wire-only-main-seam",
                "messages": [{"role": "user", "content": prompt}],
                "stream": True,
            },
            separators=(",", ":"),
        ).encode()
        status, client_body = _stream_request(env, api_key, request_body, marker)
        assert status == 200, client_body
        assert b"wire-one" in client_body and b"-two" in client_body
        valid = _wait_for(
            "completed four-direction wire trace",
            lambda: _final_detail(env, route_id, prompt),
        )
        _, _, archive = download_observation_bundle(env, valid)
        records = observation_bundle_events(archive)
        _assert_wire_only(records)
        assert _payload_bytes(records, "client_to_platform") == request_body
        assert _payload_bytes(records, "upstream_request") == provider.received[0]  # type: ignore[attr-defined]
        assert _payload_bytes(records, "upstream_response") == provider.sent[0]  # type: ignore[attr-defined]
        assert _payload_bytes(records, "platform_to_client") == client_body

        client_head = next(
            record
            for record in records
            if record["direction"] == "client_to_platform" and record["headers"]
        )
        assert client_head["headers"]["authorization"] == "***"
        assert client_head["headers"]["x-debug-risk"] == marker
        assert marker in _payload_bytes(records, "client_to_platform").decode()

        malformed_prompt = "malformed-wire"
        malformed_body = json.dumps(
            {
                "model": "wire-only-main-seam",
                "messages": [{"role": "user", "content": malformed_prompt}],
                "stream": True,
            },
            separators=(",", ":"),
        ).encode()
        _stream_request(env, api_key, malformed_body, marker)
        malformed = _wait_for(
            "failed run with complete malformed upstream capture",
            lambda: _final_detail(env, route_id, malformed_prompt),
        )
        run = malformed["runs"][-1]
        assert run["trace"]["status"] == "complete"
        ordinary_kinds = {event["kind"] for event in run["events"]}
        assert {"run_admitted", "target_attempt_started", "target_attempt_finished", "run_finished"} <= ordinary_kinds
        assert "request_failed" in ordinary_kinds

        _, _, malformed_archive = download_observation_bundle(env, malformed)
        malformed_records = observation_bundle_events(malformed_archive)
        _assert_wire_only(malformed_records)
        assert _payload_bytes(malformed_records, "client_to_platform") == malformed_body
        assert _payload_bytes(malformed_records, "upstream_request") == provider.received[1]  # type: ignore[attr-defined]
        assert _payload_bytes(malformed_records, "upstream_response") == provider.sent[1]  # type: ignore[attr-defined]
        with zipfile.ZipFile(io.BytesIO(malformed_archive)) as bundle:
            manifest = json.loads(bundle.read("manifest.json"))
        assert manifest["status"] != "partial"
        assert all(run_manifest["capture_status"] == "complete" for run_manifest in manifest["runs"])
    finally:
        provider.shutdown()
        provider.server_close()
        worker.join(timeout=5)


@pytest.mark.e2e
@pytest.mark.admin
def test_nonstream_wire_reconstructs_each_response_once_including_malformed_error(
    admin_env: dict[str, Any],
) -> None:
    provider, worker = _start_provider()
    env = {**admin_env, "mock": f"http://127.0.0.1:{provider.server_port}"}
    try:
        _enable_debug(env)
        route_id, api_key = _create_route(env, "http-wire-main-seam", retry_budget=0)

        prompt = "nonstream-wire"
        request_body = json.dumps(
            {
                "model": "http-wire-main-seam",
                "messages": [{"role": "user", "content": prompt}],
                "stream": False,
            },
            separators=(",", ":"),
        ).encode()
        status, client_body = _stream_request(env, api_key, request_body, "nonstream-marker")
        assert status == 200, client_body
        detail = _wait_for(
            "completed non-stream wire trace",
            lambda: _final_detail(env, route_id, prompt),
        )
        _, _, archive = download_observation_bundle(env, detail)
        records = observation_bundle_events(archive)
        _assert_wire_only(records)
        assert _payload_bytes(records, "client_to_platform") == request_body
        assert _payload_bytes(records, "upstream_request") == provider.received[0]  # type: ignore[attr-defined]
        assert _payload_bytes(records, "upstream_response") == provider.sent[0]  # type: ignore[attr-defined]
        assert _payload_bytes(records, "platform_to_client") == client_body

        error_prompt = "nonstream-error-wire"
        error_body = json.dumps(
            {
                "model": "http-wire-main-seam",
                "messages": [{"role": "user", "content": error_prompt}],
                "stream": False,
            },
            separators=(",", ":"),
        ).encode()
        error_status, delivered_error = _stream_request(
            env, api_key, error_body, "nonstream-error-marker"
        )
        assert error_status >= 400, delivered_error
        failed = _wait_for(
            "failed non-stream malformed error trace",
            lambda: _final_detail(env, route_id, error_prompt),
        )
        assert "request_failed" in {
            event["kind"] for event in failed["runs"][-1]["events"]
        }
        _, _, failed_archive = download_observation_bundle(env, failed)
        failed_records = observation_bundle_events(failed_archive)
        _assert_wire_only(failed_records)
        assert _payload_bytes(failed_records, "client_to_platform") == error_body
        assert _payload_bytes(failed_records, "upstream_request") == provider.received[1]  # type: ignore[attr-defined]
        assert _payload_bytes(failed_records, "upstream_response") == provider.sent[1]  # type: ignore[attr-defined]
        assert _payload_bytes(failed_records, "platform_to_client") == delivered_error
    finally:
        provider.shutdown()
        provider.server_close()
        worker.join(timeout=5)


@pytest.mark.e2e
@pytest.mark.admin
def test_running_bundle_fixes_incomplete_sse_prefix_and_preserves_inline_media(
    admin_env: dict[str, Any],
) -> None:
    provider, worker = _start_provider()
    env = {**admin_env, "mock": f"http://127.0.0.1:{provider.server_port}"}
    request_thread: threading.Thread | None = None
    try:
        _enable_debug(env)
        _route_id, api_key = _create_media_route(env, "barrier-wire-main-seam")
        prompt = "barrier-wire"
        media = b"\x89PNG\r\n\x1a\n\x00wire-media\xff"
        media_url = "data:image/png;base64," + base64.b64encode(media).decode()
        request_body = json.dumps(
            {
                "model": "barrier-wire-main-seam",
                "messages": [
                    {
                        "role": "user",
                        "content": [
                            {"type": "text", "text": prompt},
                            {"type": "image_url", "image_url": {"url": media_url}},
                        ],
                    }
                ],
                "stream": True,
            },
            separators=(",", ":"),
        ).encode()
        request_result: dict[str, Any] = {}

        def send_request() -> None:
            try:
                request_result["response"] = _stream_request(
                    env, api_key, request_body, "barrier-marker"
                )
            except BaseException as error:  # surfaced on the test thread below
                request_result["error"] = error

        before = _forest(env)["snapshot_sequence"]
        request_thread = threading.Thread(target=send_request, daemon=True)
        request_thread.start()
        assert provider.upstream_ready.wait(timeout=10)  # type: ignore[attr-defined]

        admitted_event = _sse_event(
            env,
            before,
            lambda event: event["event"] == "observation"
            and event["data"]["kind"] == "run_admitted",
        )
        run_id = admitted_event["data"]["run_id"]
        interaction_id = admitted_event["data"]["interaction_id"]
        baseline_event = _sse_event(
            env,
            int(admitted_event["id"]),
            lambda event: event["event"] == "observation"
            and event["data"]["kind"] == "trace_manifest_updated"
            and event["data"]["run_id"] == run_id,
        )
        baseline_bytes = baseline_event["data"]["payload"]["bytes_written"]

        provider.prefix_release.set()  # type: ignore[attr-defined]
        assert provider.barrier_started.wait(timeout=10)  # type: ignore[attr-defined]
        captured_event = _sse_event(
            env,
            int(baseline_event["id"]),
            lambda event: event["event"] == "observation"
            and event["data"]["kind"] == "trace_manifest_updated"
            and event["data"]["run_id"] == run_id
            and event["data"]["payload"]["bytes_written"] > baseline_bytes,
        )
        running = _detail(env, interaction_id)
        assert running["runs"][-1]["status"] == "running"
        ticket_status, ticket_body = http_request(
            "POST",
            f"{env['admin']}/api/v1/observations/interactions/{interaction_id}/debug-bundle-tickets",
            payload={"through_sequence": running["snapshot_sequence"]},
            headers=env["auth"],
        )
        assert ticket_status == 200, ticket_body
        delayed_ticket = ticket_body["data"]
        _, _, running_archive = download_observation_bundle(env, running)
        running_records = observation_bundle_events(running_archive)
        assert all(record.get("layer") == "wire" for record in running_records)
        assert all(record.get("stage") is None for record in running_records)
        prefix = provider.barrier_prefix  # type: ignore[attr-defined]
        assert _payload_bytes(running_records, "upstream_response") == prefix
        assert b"\n\n" not in prefix
        with pytest.raises(json.JSONDecodeError):
            json.loads(prefix.removeprefix(b"data: "))
        assert _payload_bytes(running_records, "client_to_platform") == request_body
        captured_client = json.loads(
            _payload_bytes(running_records, "client_to_platform")
        )
        captured_url = captured_client["messages"][0]["content"][1]["image_url"]["url"]
        assert captured_url == media_url
        assert base64.b64decode(captured_url.split(",", 1)[1]) == media
        assert b'"sa:' not in _payload_bytes(running_records, "client_to_platform")
        assert _payload_bytes(running_records, "upstream_request") == provider.received[0]  # type: ignore[attr-defined]
        assert media_url.encode() in provider.received[0]  # type: ignore[attr-defined]
        with zipfile.ZipFile(io.BytesIO(running_archive)) as bundle:
            running_manifest = json.loads(bundle.read("manifest.json"))
        assert running_manifest["status"] == "running"
        assert running_manifest["completeness"] == "partial"
        assert running_manifest["runs"][-1]["capture_status"] == "captured"

        provider.barrier_release.set()  # type: ignore[attr-defined]
        request_thread.join(timeout=10)
        assert not request_thread.is_alive()
        assert "error" not in request_result, request_result.get("error")
        status, client_body = request_result["response"]
        assert status == 200, client_body
        assert b"barrier-prefix-and-tail" in client_body

        _sse_event(
            env,
            int(captured_event["id"]),
            lambda event: event["event"] == "observation"
            and event["data"]["kind"] == "run_finished"
            and event["data"]["run_id"] == run_id,
        )
        finished = _detail(env, interaction_id)
        assert finished["runs"][-1]["status"] != "running"
        _, _, finished_archive = download_observation_bundle(env, finished)
        finished_records = observation_bundle_events(finished_archive)
        _assert_wire_only(finished_records)
        assert _payload_bytes(finished_records, "upstream_response") == provider.sent[0]  # type: ignore[attr-defined]
        assert _payload_bytes(finished_records, "platform_to_client") == client_body
        delayed_url = delayed_ticket["download_url"]
        delayed_status, delayed_headers, delayed_archive = http_bytes(
            "GET",
            f"{env['admin']}{delayed_url}" if delayed_url.startswith("/") else delayed_url,
        )
        assert delayed_status == 200
        assert "application/zip" in delayed_headers["content-type"]
        assert _payload_bytes(
            observation_bundle_events(delayed_archive), "upstream_response"
        ) == prefix
        assert provider.sent[0].startswith(prefix)  # type: ignore[attr-defined]
        assert provider.sent[0] != prefix  # type: ignore[attr-defined]
    finally:
        provider.prefix_release.set()  # type: ignore[attr-defined]
        provider.barrier_release.set()  # type: ignore[attr-defined]
        if request_thread is not None:
            request_thread.join(timeout=5)
        provider.shutdown()
        provider.server_close()
        worker.join(timeout=5)


@pytest.mark.e2e
@pytest.mark.admin
def test_rejected_ingress_preserves_invalid_utf8_body_bytes(
    admin_env: dict[str, Any],
) -> None:
    _enable_debug(admin_env)
    _route_id, api_key = _create_route(admin_env, "invalid-utf8-ingress", retry_budget=0)
    before = _rejection_ids(admin_env)
    body = b'{"model":"invalid-utf8-ingress","messages":[],"raw":"\xff\x00"}'
    status, _response = _stream_request(admin_env, api_key, body, "invalid-utf8")
    assert status == 400

    detail = _wait_for(
        "invalid UTF-8 rejected ingress trace",
        lambda: _new_rejection(admin_env, before),
    )
    _, _, archive = download_observation_bundle(admin_env, detail)
    records = observation_bundle_events(archive)
    assert _payload_bytes(records, "client_to_platform") == body


@pytest.mark.e2e
@pytest.mark.admin
def test_rejected_ingress_preserves_duplicate_headers(
    admin_env: dict[str, Any],
) -> None:
    _enable_debug(admin_env)
    _route_id, api_key = _create_route(admin_env, "opaque-header-ingress", retry_budget=0)
    before = _rejection_ids(admin_env)
    body = b'{"model":"opaque-header-ingress","messages":[],"raw":"\xff"}'
    duplicates = [b"first", b"second"]
    status, _response = _duplicate_header_request(
        admin_env, api_key, body, duplicates
    )
    assert status == 400

    detail = _wait_for(
        "opaque-header rejected ingress trace",
        lambda: _new_rejection(admin_env, before),
    )
    _, _, archive = download_observation_bundle(admin_env, detail)
    records = observation_bundle_events(archive)
    head = next(
        record
        for record in records
        if record.get("direction") == "client_to_platform"
        and record.get("message_type") == "request_head"
    )
    assert head["headers"]["authorization"] == "***"
    assert head["headers"]["x-raw-duplicate"] == [
        value.decode() for value in duplicates
    ]


@pytest.mark.e2e
@pytest.mark.admin
def test_incomplete_ingress_keeps_received_body_prefix(
    admin_env: dict[str, Any],
) -> None:
    _enable_debug(admin_env)
    _route_id, api_key = _create_route(admin_env, "partial-ingress", retry_budget=0)
    before = _rejection_ids(admin_env)
    prefix = b'{"model":"partial-ingress","messages":[{"role":"user","content":"prefix'
    _raw_ingress_request(
        admin_env,
        api_key,
        prefix,
        "partial-prefix",
        declared_length=len(prefix) + 64,
    )

    detail = _wait_for(
        "partial rejected ingress trace",
        lambda: _new_rejection(admin_env, before),
    )
    assert detail["trace"]["status"] == "partial"
    _, _, archive = download_observation_bundle(admin_env, detail)
    records = observation_bundle_events(archive)
    assert _payload_bytes(records, "client_to_platform") == prefix


@pytest.mark.e2e
@pytest.mark.admin
def test_client_websocket_handshake_is_recorded_once_for_first_debug_run(
    admin_env: dict[str, Any],
) -> None:
    _enable_debug(admin_env)
    model = "ws-handshake-once"
    route_id, api_key = _create_route(admin_env, model, retry_budget=0)
    connection = _websocket_connection(admin_env, api_key)
    handshake_completed_at = int(time.time() * 1000)
    time.sleep(0.02)
    prompts = [
        "observation-websocket handshake-first",
        "observation-websocket handshake-debug-off",
        "observation-websocket handshake-debug-on-again",
    ]
    details: list[dict[str, Any]] = []
    try:
        for enabled, prompt in zip([True, False, True], prompts, strict=True):
            _set_debug(admin_env, enabled)
            _websocket_send(
                connection,
                {
                    "type": "response.create",
                    "model": model,
                    "input": [{"role": "user", "content": prompt}],
                },
            )
            _websocket_completed(connection)
            details.append(
                _wait_for(
                    f"terminal WebSocket run for {prompt}",
                    lambda prompt=prompt: _terminal_detail(admin_env, route_id, prompt),
                )
            )
    finally:
        connection.close()
        _enable_debug(admin_env)

    assert details[0]["runs"][-1]["debug_enabled"] is True
    assert details[1]["runs"][-1]["debug_enabled"] is False
    assert details[1]["runs"][-1]["trace"] is None
    assert details[2]["runs"][-1]["debug_enabled"] is True

    _, _, first_archive = download_observation_bundle(admin_env, details[0])
    first_records = observation_bundle_events(first_archive)
    first_handshake = [
        record
        for record in first_records
        if record.get("message_type") in {"handshake_request", "handshake_response"}
    ]
    assert [record["message_type"] for record in first_handshake] == [
        "handshake_request",
        "handshake_response",
    ]
    first_text = next(
        record
        for record in first_records
        if record.get("direction") == "client_to_platform"
        and record.get("message_type") == "text"
    )
    assert max(record["recorded_at"] for record in first_handshake) <= handshake_completed_at
    assert max(record["recorded_at"] for record in first_handshake) <= first_text["recorded_at"]

    _, _, third_archive = download_observation_bundle(admin_env, details[2])
    third_records = observation_bundle_events(third_archive)
    assert not any(
        record.get("message_type") in {"handshake_request", "handshake_response"}
        for record in third_records
    )


@pytest.mark.e2e
@pytest.mark.admin
def test_client_websocket_handshake_respects_boundary_debug_admission(
    admin_env: dict[str, Any],
) -> None:
    model = "ws-handshake-admission"
    route_id, api_key = _create_route(admin_env, model, retry_budget=0)

    _set_debug(admin_env, False)
    initially_off = _websocket_connection(admin_env, api_key)
    off_then_on_prompt = "observation-websocket handshake-off-then-on"
    try:
        _set_debug(admin_env, True)
        _websocket_send(
            initially_off,
            {
                "type": "response.create",
                "model": model,
                "input": [{"role": "user", "content": off_then_on_prompt}],
            },
        )
        _websocket_completed(initially_off)
    finally:
        initially_off.close()
    off_then_on = _wait_for(
        "terminal first run after Debug-off handshake",
        lambda: _final_detail(admin_env, route_id, off_then_on_prompt),
    )
    _, _, off_then_on_archive = download_observation_bundle(admin_env, off_then_on)
    assert not any(
        record.get("message_type") in {"handshake_request", "handshake_response"}
        for record in observation_bundle_events(off_then_on_archive)
    )

    initially_on = _websocket_connection(admin_env, api_key)
    on_then_off_prompt = "observation-websocket handshake-on-then-off"
    on_again_prompt = "observation-websocket handshake-after-first-off"
    try:
        _set_debug(admin_env, False)
        _websocket_send(
            initially_on,
            {
                "type": "response.create",
                "model": model,
                "input": [{"role": "user", "content": on_then_off_prompt}],
            },
        )
        _websocket_completed(initially_on)
        on_then_off = _wait_for(
            "terminal first run after Debug-on handshake",
            lambda: _terminal_detail(admin_env, route_id, on_then_off_prompt),
        )
        assert on_then_off["runs"][-1]["debug_enabled"] is False
        assert on_then_off["runs"][-1]["trace"] is None

        _set_debug(admin_env, True)
        _websocket_send(
            initially_on,
            {
                "type": "response.create",
                "model": model,
                "input": [{"role": "user", "content": on_again_prompt}],
            },
        )
        _websocket_completed(initially_on)
    finally:
        initially_on.close()
        _enable_debug(admin_env)
    on_again = _wait_for(
        "terminal later run after first Debug-off run",
        lambda: _final_detail(admin_env, route_id, on_again_prompt),
    )
    _, _, on_again_archive = download_observation_bundle(admin_env, on_again)
    assert not any(
        record.get("message_type") in {"handshake_request", "handshake_response"}
        for record in observation_bundle_events(on_again_archive)
    )
