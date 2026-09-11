from __future__ import annotations

import json
import os
import socket
import sqlite3
import subprocess
import threading
from contextlib import contextmanager
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Iterator
from urllib.parse import urlsplit

import pytest

from tests.common.helpers import (
    WebSession,
    find_free_port,
    initialize_server,
    start_stravia_server,
    stop_stravia_server,
    wait_for_setup_token,
    wait_until_ready,
)


@pytest.fixture(scope="module", autouse=True)
def isolated_external_services() -> Iterator[None]:
    class DenyExternal(BaseHTTPRequestHandler):
        def log_message(self, *_args: Any) -> None:
            pass

        def do_CONNECT(self) -> None:
            self.send_error(403)

        def do_GET(self) -> None:
            self.send_error(403)

    proxy = ThreadingHTTPServer(("127.0.0.1", 0), DenyExternal)
    thread = threading.Thread(target=proxy.serve_forever, daemon=True)
    thread.start()
    try:
        with pytest.MonkeyPatch.context() as environment:
            for name in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"):
                value = f"http://127.0.0.1:{proxy.server_port}"
                environment.setenv(name, value)
                environment.setenv(name.lower(), value)
            environment.setenv("NO_PROXY", "localhost,127.0.0.1,::1")
            environment.setenv("no_proxy", "localhost,127.0.0.1,::1")
            yield
    finally:
        proxy.shutdown()
        proxy.server_close()
        thread.join()


@contextmanager
def _server(
    binary: Path, directory: Path, *security_args: str, host: str = "0.0.0.0",
) -> Iterator[tuple[str, str]]:
    port = find_free_port()
    base = f"http://[::1]:{port}" if host == "::1" else f"http://127.0.0.1:{port}"
    proc, logs = start_stravia_server(
        stravia_binary=binary,
        args=["--host", host, "--port", str(port), "--data-dir", str(directory), *security_args],
        cwd=directory,
    )
    try:
        token = wait_for_setup_token(logs, proc)
        wait_until_ready(f"{base}/healthz")
        yield base, token
    finally:
        stop_stravia_server(proc, logs)


def _request(
    base: str, path: str, *, method: str = "GET",
    headers: list[tuple[str, str]] | None = None, payload: Any = None,
) -> tuple[int, Any]:
    """真实 HTTP 请求保留重复头，用于验证代理边界而非解析器实现。"""
    target = urlsplit(base)
    connection = HTTPConnection(target.hostname, target.port, timeout=15)
    data = json.dumps(payload).encode() if payload is not None else b""
    try:
        connection.putrequest(method, path, skip_host=True)
        pairs = headers or []
        if not any(name.lower() == "host" for name, _ in pairs):
            connection.putheader("Host", base.removeprefix("http://"))
        for name, value in pairs:
            connection.putheader(name, value)
        if payload is not None:
            connection.putheader("Content-Type", "application/json")
        connection.putheader("Content-Length", str(len(data)))
        connection.endheaders(data)
        response = connection.getresponse()
        raw = response.read()
        try:
            body = json.loads(raw)
        except (ValueError, UnicodeDecodeError):
            body = raw
        return response.status, body
    finally:
        connection.close()


@pytest.mark.e2e
@pytest.mark.admin
def test_nonloopback_http_without_entry_configuration_supports_management(
    stravia_binary: Path, tmp_path: Path,
) -> None:
    port = find_free_port()
    base = f"http://127.0.0.1:{port}"
    proc, logs = start_stravia_server(
        stravia_binary=stravia_binary,
        args=["--host", "0.0.0.0", "--port", str(port), "--data-dir", str(tmp_path)],
        cwd=tmp_path,
    )
    try:
        token = wait_for_setup_token(logs, proc)
        wait_until_ready(f"{base}/healthz")
        first = initialize_server(base, token, {"backend": "sqlite", "path": str(tmp_path / "gateway.db")})
        assert first.request("GET", "/api/v1/status")[0] == 200
        assert first.request("PUT", "/api/v1/settings/log_retention_days", {"value": "9"})[0] == 200
        assert first.request("GET", "/api/v1/settings/log_retention_days")[1]["data"] == "9"
        second = WebSession(f"http://localhost:{port}")
        assert second.request("POST", "/api/v1/auth/login", {
            "username": "admin", "password": "correct horse battery staple",
        })[0] == 200
        assert second.request("GET", "/api/v1/status")[0] == 200
        assert first.request("POST", "/api/v1/auth/logout", headers={"origin": second.origin})[0] == 403
        assert first.request("POST", "/api/v1/auth/refresh", {})[0] == 200
        assert first.request("POST", "/api/v1/auth/logout")[0] == 204
        assert first.request("GET", "/api/v1/status")[0] == 401
        assert second.request("GET", "/api/v1/status")[0] == 200
    finally:
        stop_stravia_server(proc, logs)


@pytest.mark.e2e
@pytest.mark.admin
def test_explicit_entries_guard_pages_setup_and_all_management(
    stravia_binary: Path, tmp_path: Path,
) -> None:
    allowed = "http://entry.test:23471"
    headers = [("Host", "entry.test:23471")]
    with _server(stravia_binary, tmp_path, "--admin-origin", allowed,
                 "--admin-origin", "http://second.test") as (base, _token):
        assert _request(base, "/api/v1/auth/state", headers=headers)[0] == 200
        assert _request(base, "/api/v1/auth/state", headers=[("Host", "second.test:80")])[0] == 200
        for path in ["/", "/setup", "/login", "/media-understanding", "/_app/immutable/app.js",
                     "/api/v1/auth/state", "/api/v1/status", "/api/v1/setup/state"]:
            assert _request(base, path)[0] == 403, path
        assert _request(base, "/api/v1/setup/claim", method="POST", payload={"token": "invalid"})[0] == 403
        assert _request(base, "/api/v1/auth/login", method="POST", payload={})[0] == 403
        assert _request(base, "/api/v1/auth/state", headers=[("Host", "entry.test")])[0] == 403
        assert _request(base, "/healthz")[0] == 200


@pytest.mark.e2e
@pytest.mark.admin
def test_allowed_origins_do_not_authorize_cross_origin_writes(
    stravia_binary: Path, tmp_path: Path,
) -> None:
    with _server(stravia_binary, tmp_path, "--admin-origin", "http://first.test",
                 "--admin-origin", "http://second.test") as (base, token):
        common = [("Host", "first.test"), ("X-Stravia-CSRF", "1")]
        for origin in ["http://second.test", "null", "http://attacker.invalid"]:
            assert _request(base, "/api/v1/setup/claim", method="POST",
                            headers=[*common, ("Origin", origin)], payload={"token": token})[0] == 403
        assert _request(base, "/api/v1/setup/claim", method="POST",
                        headers=common, payload={"token": token})[0] == 403
        assert _request(base, "/api/v1/setup/claim", method="POST",
                        headers=[("Host", "first.test"), ("Origin", "http://first.test")],
                        payload={"token": token})[0] == 403
        assert _request(base, "/api/v1/setup/claim", method="POST",
                        headers=[*common, ("Origin", "http://first.test")],
                        payload={"token": token})[0] == 204


@pytest.mark.e2e
@pytest.mark.admin
def test_forwarding_metadata_requires_trusted_network_peer(
    stravia_binary: Path, tmp_path: Path,
) -> None:
    forwarded = [("X-Forwarded-Host", "secure.test"), ("X-Forwarded-Proto", "https")]
    with _server(stravia_binary, tmp_path, "--admin-origin", "https://secure.test") as (base, _token):
        assert _request(base, "/api/v1/auth/state", headers=forwarded)[0] == 403
        assert _request(base, "/api/v1/auth/state", headers=[
            *forwarded, ("X-Forwarded-For", "127.0.0.1"), ("Forwarded", "proto=https;host=secure.test"),
        ])[0] == 403


@pytest.mark.e2e
@pytest.mark.admin
def test_trusted_proxy_recovers_origin_but_rejects_ambiguous_metadata(
    stravia_binary: Path, tmp_path: Path,
) -> None:
    forwarded = [("X-Forwarded-Host", "secure.test:443"), ("X-Forwarded-Proto", "https")]
    with _server(stravia_binary, tmp_path, "--admin-origin", "https://secure.test",
                 "--trusted-proxy", "127.0.0.0/8") as (base, token):
        assert _request(base, "/api/v1/auth/state", headers=forwarded)[0] == 200
        assert _request(base, "/api/v1/auth/state")[0] == 403
        invalid = [
            [("X-Forwarded-Proto", "https")],
            [("X-Forwarded-Host", "secure.test")],
            [*forwarded, ("X-Forwarded-Proto", "http")],
            [("X-Forwarded-Host", "secure.test,evil.test"), ("X-Forwarded-Proto", "https")],
            [*forwarded, ("Forwarded", "proto=http;host=evil.test")],
            [("X-Forwarded-Host", "secure.test/path"), ("X-Forwarded-Proto", "https")],
            [("X-Forwarded-Host", "secure.test"), ("X-Forwarded-Proto", "file")],
        ]
        for headers in invalid:
            assert _request(base, "/api/v1/auth/state", headers=headers)[0] in (400, 403), headers
        assert _request(base, "/api/v1/setup/claim", method="POST",
                        headers=[*forwarded, ("Origin", base), ("X-Stravia-CSRF", "1")],
                        payload={"token": token})[0] == 403
        assert _request(base, "/api/v1/setup/claim", method="POST",
                        headers=[*forwarded, ("Origin", "https://secure.test"), ("X-Stravia-CSRF", "1")],
                        payload={"token": token})[0] == 204


@pytest.mark.e2e
@pytest.mark.admin
def test_ipv6_peer_and_external_origin_are_matched_without_ipv4_assumptions(
    stravia_binary: Path, tmp_path: Path,
) -> None:
    try:
        with socket.socket(socket.AF_INET6, socket.SOCK_STREAM) as probe:
            probe.bind(("::1", 0))
    except OSError:
        pytest.skip("IPv6 loopback is unavailable")
    with _server(stravia_binary, tmp_path, "--admin-origin", "https://[2001:db8::10]",
                 "--trusted-proxy", "::1/128", host="::1") as (base, _token):
        forwarded = [("X-Forwarded-Host", "[2001:db8::10]:443"), ("X-Forwarded-Proto", "https")]
        assert _request(base, "/api/v1/auth/state", headers=forwarded)[0] == 200
        assert _request(base, "/api/v1/auth/state")[0] == 403
        assert _request(base, "/api/v1/auth/state", headers=[
            ("X-Forwarded-Host", "[2001:db8::11]"), ("X-Forwarded-Proto", "https"),
        ])[0] == 403


@pytest.mark.e2e
@pytest.mark.admin
@pytest.mark.parametrize("args", [
    ["--admin-origin", ""],
    ["--admin-origin", "http://user:password@entry.test"],
    ["--admin-origin", "http://entry.test/settings"],
    ["--admin-origin", "http://entry.test:99999"],
    ["--admin-origin", "https://*.example.com"],
    ["--admin-origin", "http://entry.test,http://second.test,"],
    ["--trusted-proxy", ""],
    ["--trusted-proxy", "127.0.0.1/33"],
    ["--trusted-proxy", "::1/129"],
    ["--trusted-proxy", "localhost"],
])
def test_invalid_explicit_security_configuration_never_opens_management(
    stravia_binary: Path, tmp_path: Path, args: list[str],
) -> None:
    result = subprocess.run(
        [str(stravia_binary), "--host", "0.0.0.0", "--data-dir", str(tmp_path), *args],
        cwd=tmp_path, capture_output=True, text=True, timeout=15,
    )
    assert result.returncode != 0
    assert "Stravia setup token:" not in result.stdout


@pytest.mark.e2e
@pytest.mark.admin
def test_removed_origin_environment_cannot_silently_remove_entry_restrictions(
    stravia_binary: Path, tmp_path: Path,
) -> None:
    try:
        result = subprocess.run(
            [str(stravia_binary), "--port", "0", "--data-dir", str(tmp_path)],
            cwd=tmp_path, capture_output=True, text=True, timeout=5,
            env={**os.environ, "STRAVIA_PUBLIC_ORIGIN": "https://old-entry.test"},
        )
    except subprocess.TimeoutExpired:
        pytest.fail("Removed origin configuration silently started an unrestricted Server", pytrace=False)
    assert result.returncode != 0
    assert "Stravia setup token:" not in result.stdout


@pytest.mark.e2e
@pytest.mark.admin
def test_restarting_with_entry_list_gates_existing_admin_without_gating_model_api(
    stravia_binary: Path, tmp_path: Path,
) -> None:
    with _server(stravia_binary, tmp_path) as (base, token):
        operator = initialize_server(base, token, {"backend": "sqlite", "path": str(tmp_path / "gateway.db")})
        status, key = operator.request("POST", "/api/v1/api-keys", {
            "name": "protocol-client", "model_ids": [], "mcp_access_enabled": True,
        })
        assert status == 200
        protocol_headers = [("Authorization", f"Bearer {key['data']['key']}")]
    port = find_free_port()
    base = f"http://127.0.0.1:{port}"
    proc, logs = start_stravia_server(
        stravia_binary=stravia_binary,
        args=["--port", str(port), "--data-dir", str(tmp_path)],
        env={"STRAVIA_ADMIN_ORIGINS": "http://entry.test,http://second.test"},
        cwd=tmp_path,
    )
    try:
        wait_until_ready(f"{base}/healthz")
        for path in ["/", "/login", "/media-understanding", "/api/v1/auth/state", "/api/v1/status"]:
            assert _request(base, path)[0] == 403, path
        allowed = [("Host", "entry.test"), ("Origin", "http://entry.test"), ("X-Stravia-CSRF", "1")]
        assert _request(base, "/api/v1/auth/login", method="POST", headers=allowed, payload={
            "username": "admin", "password": "correct horse battery staple",
        })[0] == 200
        assert _request(base, "/api/v1/status", headers=allowed)[0] == 401
        assert _request(base, "/api/v1/auth/state", headers=[("Host", "second.test")])[0] == 200
        assert _request(base, "/v1/models")[0] == 401
        assert _request(base, "/v1/models", headers=protocol_headers)[0] == 200
        initialize = {
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                       "clientInfo": {"name": "entry-regression", "version": "1"}},
        }
        accept = [("Accept", "application/json, text/event-stream")]
        assert _request(base, "/mcp", method="POST", headers=accept, payload=initialize)[0] == 401
        status, initialized = _request(base, "/mcp", method="POST",
                                       headers=[*accept, *protocol_headers], payload=initialize)
        assert status == 200
        if isinstance(initialized, bytes):
            for event in initialized.replace(b"\r\n", b"\n").split(b"\n\n"):
                data = b"\n".join(line[5:].lstrip() for line in event.splitlines() if line.startswith(b"data:"))
                if data:
                    message = json.loads(data)
                    if message.get("id") == 1:
                        initialized = message
                        break
        assert isinstance(initialized, dict)
        assert initialized["result"]["protocolVersion"] == "2025-06-18"
    finally:
        stop_stravia_server(proc, logs)
    proc, logs = start_stravia_server(
        stravia_binary=stravia_binary,
        args=["--port", str(port), "--data-dir", str(tmp_path)],
        cwd=tmp_path,
    )
    try:
        wait_until_ready(f"{base}/healthz")
        session = WebSession(base)
        assert session.request("POST", "/api/v1/auth/login", {
            "username": "admin", "password": "correct horse battery staple",
        })[0] == 200
        assert session.request("GET", "/api/v1/status")[0] == 200
    finally:
        stop_stravia_server(proc, logs)


@pytest.mark.e2e
@pytest.mark.admin
def test_unavailable_transition_keeps_the_entire_management_entry_guard(
    stravia_binary: Path, tmp_path: Path,
) -> None:
    with _server(stravia_binary, tmp_path, "--admin-origin", "http://entry.test") as (base, token):
        operator = WebSession(base)
        headers = {"host": "entry.test", "origin": "http://entry.test"}
        assert operator.request("POST", "/api/v1/setup/claim", {"token": token}, headers=headers)[0] == 204
        database_path = tmp_path / "gateway.db"
        setup = {
            "database": {"backend": "sqlite", "path": str(database_path)},
            "username": "admin", "password": "", "client_base_url": "http://entry.test",
        }
        assert operator.request("POST", "/api/v1/setup/complete", setup, headers=headers)[0] == 400
        # 注入管理员创建后才显现的存储损坏；结果仅通过真实 HTTP 表面断言。
        with sqlite3.connect(database_path) as database:
            for operation in ("INSERT", "UPDATE OF username"):
                name = "insert" if operation == "INSERT" else "update"
                database.execute(f"""
                    CREATE TRIGGER corrupt_artifact_settings_{name} AFTER {operation} ON admin_identity
                    WHEN NEW.username IS NOT NULL
                    BEGIN UPDATE settings SET value = 'invalid-json' WHERE name = 'artifact_settings'; END
                """)
        database.close()
        setup["password"] = "correct horse battery staple"
        assert operator.request("POST", "/api/v1/setup/complete", setup, headers=headers)[0] == 503
        status, state = operator.request("GET", "/api/v1/auth/state", headers=headers)
        assert status == 200
        assert state["mode"] == "unavailable"
        assert state["setup_authorized"] is False
        for path in ["/", "/setup", "/login", "/api/v1/auth/state", "/api/v1/status"]:
            assert _request(base, path)[0] == 403, path
        assert _request(base, "/healthz")[0] == 200
        assert _request(base, "/readyz")[0] == 503
