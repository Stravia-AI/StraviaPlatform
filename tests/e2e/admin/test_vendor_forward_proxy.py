from __future__ import annotations

import base64
import hashlib
import json
import select
import socket
import socketserver
import struct
import threading
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Iterator
from urllib.parse import urlsplit

import pytest

from tests.common.helpers import WebSession, http_request
from tests.e2e.storage.test_vendor_plugins import _create_connection, _install_plugin


_RESPONSE = json.dumps({
    "id": "proxy-fixture-response",
    "model": "fixture-model",
    "items": [{"role": "assistant", "content": "forwarded origin response"}],
    "stop_reason": "stop",
    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2, "required_components_known": True},
    "embedding_output": None,
    "error": None,
    "vendor": {"ingress": {}, "egress": {}, "passthrough_safe": {}},
}).encode()


def _read_exact(sock: socket.socket, size: int) -> bytes:
    result = bytearray()
    while len(result) < size:
        part = sock.recv(size - len(result))
        if not part:
            raise EOFError("connection closed before fixture data was complete")
        result.extend(part)
    return bytes(result)


def _read_head(sock: socket.socket) -> bytes:
    head = bytearray()
    while not head.endswith(b"\r\n\r\n"):
        head.extend(_read_exact(sock, 1))
        if len(head) > 65536:
            raise ValueError("fixture HTTP head too large")
    return bytes(head)


def _read_ws_frame(sock: socket.socket) -> tuple[int, bytes]:
    opcode, length = _read_exact(sock, 2)
    masked = bool(length & 0x80)
    length &= 0x7f
    if length == 126:
        length = struct.unpack("!H", _read_exact(sock, 2))[0]
    elif length == 127:
        length = struct.unpack("!Q", _read_exact(sock, 8))[0]
    assert masked and length <= 1024 * 1024
    mask = _read_exact(sock, 4)
    payload = _read_exact(sock, length)
    return opcode & 0x0f, bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))


def _send_ws_frame(sock: socket.socket, opcode: int, payload: bytes) -> None:
    length = len(payload)
    encoded = bytes([length]) if length < 126 else b"\x7e" + struct.pack("!H", length)
    sock.sendall(bytes([0x80 | opcode]) + encoded + payload)


@contextmanager
def _origin() -> Iterator[tuple[str, list[dict[str, Any]]]]:
    received: list[dict[str, Any]] = []

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_args: Any) -> None:
            return

        def do_POST(self) -> None:  # noqa: N802
            payload = self.rfile.read(int(self.headers["content-length"]))
            received.append({"method": "POST", "path": self.path, "authorization": self.headers.get("authorization"), "payload": payload})
            self.send_response(200)
            self.send_header("Content-Length", str(len(_RESPONSE)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(_RESPONSE)
            self.close_connection = True

        def do_GET(self) -> None:  # noqa: N802
            key = self.headers["Sec-WebSocket-Key"]
            accept = base64.b64encode(hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
            self.send_response(101)
            self.send_header("Upgrade", "websocket")
            self.send_header("Connection", "Upgrade")
            self.send_header("Sec-WebSocket-Accept", accept)
            self.send_header("Sec-WebSocket-Protocol", "manual-v1")
            self.end_headers()
            opcode, payload = _read_ws_frame(self.connection)
            assert opcode == 1
            received.append({"method": "GET", "path": self.path, "authorization": self.headers.get("authorization"), "payload": payload})
            _send_ws_frame(self.connection, 1, _RESPONSE)
            opcode, payload = _read_ws_frame(self.connection)
            if opcode == 8:
                _send_ws_frame(self.connection, 8, payload)
            self.close_connection = True

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}", received
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


@contextmanager
def _forward_proxy(scheme: str, origin_port: int) -> Iterator[tuple[str, list[dict[str, Any]], list[str]]]:
    forwarded: list[dict[str, Any]] = []
    errors: list[str] = []

    class Handler(socketserver.BaseRequestHandler):
        def handle(self) -> None:
            client = self.request
            client.settimeout(20)
            try:
                initial = b""
                if scheme == "socks5":
                    version, count = _read_exact(client, 2)
                    assert version == 5 and 0 in _read_exact(client, count)
                    client.sendall(b"\x05\x00")
                    version, command, reserved, address_type = _read_exact(client, 4)
                    assert (version, command, reserved) == (5, 1, 0)
                    if address_type == 1:
                        host = socket.inet_ntop(socket.AF_INET, _read_exact(client, 4))
                    elif address_type == 4:
                        host = socket.inet_ntop(socket.AF_INET6, _read_exact(client, 16))
                    elif address_type == 3:
                        host = _read_exact(client, _read_exact(client, 1)[0]).decode("ascii")
                    else:
                        raise ValueError(f"unknown SOCKS address type {address_type}")
                    port = struct.unpack("!H", _read_exact(client, 2))[0]
                    assert host == "127.0.0.1" and port == origin_port
                    upstream = socket.create_connection((host, port), timeout=20)
                    client.sendall(b"\x05\x00\x00\x01\x7f\x00\x00\x01\x00\x00")
                    forwarded.append({"host": host, "port": port})
                else:
                    head = _read_head(client)
                    first, rest = head.split(b"\r\n", 1)
                    method, target, version = first.split(b" ", 2)
                    parsed = urlsplit(target.decode("ascii"))
                    assert parsed.scheme == "http" and parsed.hostname == "127.0.0.1" and parsed.port == origin_port
                    upstream = socket.create_connection((parsed.hostname, parsed.port), timeout=20)
                    path = (parsed.path or "/") + ("?" + parsed.query if parsed.query else "")
                    initial = method + b" " + path.encode("ascii") + b" " + version + b"\r\n" + rest
                    forwarded.append({"method": method.decode(), "target": target.decode()})
                with upstream:
                    if initial:
                        upstream.sendall(initial)
                    while True:
                        readable, _, _ = select.select([client, upstream], [], [], 20)
                        if not readable:
                            raise TimeoutError("proxy relay timed out")
                        for source in readable:
                            data = source.recv(65536)
                            if not data:
                                return
                            (upstream if source is client else client).sendall(data)
            except Exception as error:
                errors.append(repr(error))

    class Server(socketserver.ThreadingTCPServer):
        allow_reuse_address = True
        daemon_threads = True

    server = Server(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"{scheme}://127.0.0.1:{server.server_address[1]}", forwarded, errors
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


@pytest.fixture(scope="module")
def manual_session(admin_env: dict[str, Any], repo_root: Path) -> WebSession:
    session = WebSession(admin_env["admin"])
    status, body = session.request("POST", "/api/v1/auth/login", {
        "username": admin_env["username"], "password": admin_env["password"],
    })
    assert status == 200, body
    _install_plugin(session, repo_root / "target" / "vendor-test-fixtures", "manual-entrypoints.wasm")
    return session


@pytest.mark.e2e
@pytest.mark.admin
@pytest.mark.parametrize("transport", ["http", "websocket"])
@pytest.mark.parametrize("proxy_scheme", ["http", "socks5"])
def test_vendor_transport_through_forward_proxy(
    admin_env: dict[str, Any], manual_session: WebSession, transport: str, proxy_scheme: str,
) -> None:
    settings = f"{admin_env['admin']}/api/v1/settings/outbound_proxy"
    status, body = http_request("GET", settings, headers=admin_env["auth"])
    assert status == 200, body
    original = body["data"] or json.dumps({"url": "", "bypass": "", "force_http1": False})
    with _origin() as (origin, received), _forward_proxy(proxy_scheme, urlsplit(origin).port) as (proxy, forwarded, errors):
        try:
            status, body = http_request("PUT", settings, headers=admin_env["auth"], payload={
                "value": json.dumps({"url": proxy, "bypass": "", "force_http1": True}),
            })
            assert status == 200 and not body.get("error"), body
            model = f"proxy-forward-{proxy_scheme}-{transport}"
            connection = _create_connection(
                manual_session, vendor="fixture.manual-entrypoints", name=model,
                model_id=model, base_url=origin, secret="local-proxy-origin-key",
            )
            options = {"mode": "base"}
            if transport == "websocket":
                options = {"mode": "websocket", "auxUrl": origin.replace("http://", "ws://", 1) + "/socket"}
            status, body = manual_session.request("PUT", f"/api/v1/providers/{connection['provider_id']}", {
                "use_proxy": True, "vendor_options": options,
            })
            assert status == 200, body
            status, body = manual_session.request("POST", "/api/v1/api-keys", {
                "name": model, "model_ids": [connection["route_id"]],
            })
            assert status == 200 and not body.get("error"), body
            key = body["data"]["key"]
            status, body = http_request("POST", f"{admin_env['proxy']}/v1/responses", headers={
                "authorization": f"Bearer {key}",
            }, payload={"model": model, "input": "through a real forwarding proxy"}, timeout=30)
            assert status == 200, (body, errors)
            assert "forwarded origin response" in json.dumps(body)
            assert len(forwarded) == 1, (forwarded, errors)
            assert not errors
            assert len(received) == 1
            request = received[0]
            assert (request["method"], request["path"]) == (("POST", "/infer") if transport == "http" else ("GET", "/socket"))
            assert request["authorization"] == "Bearer local-proxy-origin-key"
            assert b"through a real forwarding proxy" in request["payload"]
            if proxy_scheme == "http":
                assert forwarded[0] == {"method": request["method"], "target": origin + request["path"]}
        finally:
            status, body = http_request("PUT", settings, headers=admin_env["auth"], payload={"value": original})
            assert status == 200 and not body.get("error"), body
