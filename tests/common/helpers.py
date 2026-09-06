"""Shared test utilities for Stravia E2E test suites."""

from __future__ import annotations

import json
import os
import re
import socket
import subprocess
import sys
import threading
import time
from http.cookiejar import CookieJar
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any
from urllib.error import HTTPError, URLError
from urllib.request import HTTPCookieProcessor, Request, build_opener, urlopen


# ── Port utilities ──────────────────────────────────────────────────────────


def find_free_port() -> int:
    """Bind to port 0 and return the OS-assigned ephemeral port."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return int(s.getsockname()[1])


def is_port_free(port: int) -> bool:
    """Return True if nothing is currently listening on 127.0.0.1:<port>."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.settimeout(0.3)
        try:
            s.connect(("127.0.0.1", port))
            return False
        except (ConnectionRefusedError, OSError):
            return True


# ── HTTP helpers ─────────────────────────────────────────────────────────────


def _decode_body(raw: bytes) -> Any:
    text = raw.decode("utf-8", errors="replace")
    try:
        return json.loads(text)
    except json.JSONDecodeError:
        return text


def http_request(
    method: str,
    url: str,
    payload: Any | None = None,
    headers: dict[str, str] | None = None,
    timeout: float = 15.0,
) -> tuple[int, Any]:
    """Make an HTTP request and return (status_code, decoded_body)."""
    hdrs: dict[str, str] = dict(headers or {})
    data: bytes | None = None
    if payload is not None:
        hdrs.setdefault("content-type", "application/json")
        data = json.dumps(payload).encode("utf-8")

    req = Request(url=url, method=method, data=data, headers=hdrs)
    try:
        with urlopen(req, timeout=timeout) as resp:
            return int(resp.status), _decode_body(resp.read())
    except HTTPError as e:
        return int(e.code), _decode_body(e.read())


class WebSession:
    """Browser-shaped HTTP client with a real cookie jar and CSRF headers."""

    def __init__(self, origin: str) -> None:
        self.origin = origin.rstrip("/")
        self.cookies = CookieJar()
        self._opener = build_opener(HTTPCookieProcessor(self.cookies))

    def request(
        self,
        method: str,
        path: str,
        payload: Any | None = None,
        *,
        headers: dict[str, str] | None = None,
        timeout: float = 15.0,
    ) -> tuple[int, Any]:
        request_headers = dict(headers or {})
        if method.upper() not in ("GET", "HEAD", "OPTIONS"):
            request_headers.setdefault("origin", self.origin)
            request_headers.setdefault("x-stravia-csrf", "1")
        data: bytes | None = None
        if payload is not None:
            request_headers.setdefault("content-type", "application/json")
            data = json.dumps(payload).encode("utf-8")
        url = path if path.startswith(("http://", "https://")) else f"{self.origin}{path}"
        request = Request(url, method=method, data=data, headers=request_headers)
        try:
            with self._opener.open(request, timeout=timeout) as response:
                return int(response.status), _decode_body(response.read())
        except HTTPError as error:
            return int(error.code), _decode_body(error.read())

    def cookie_header(self) -> str:
        return "; ".join(f"{cookie.name}={cookie.value}" for cookie in self.cookies)

    def cookie_value(self, name: str) -> str | None:
        return next((cookie.value for cookie in self.cookies if cookie.name == name), None)

    def auth_headers(self) -> dict[str, str]:
        """Headers for existing helpers that do not accept a session object."""
        return {
            "cookie": self.cookie_header(),
            "origin": self.origin,
            "x-stravia-csrf": "1",
        }


def wait_for_setup_token(
    logs: list[str], proc: subprocess.Popen[str], timeout: float = 30.0
) -> str:
    """Wait for the server's stable one-time setup-token console marker."""
    marker = re.compile(r"Stravia setup token:\s*(\S+)")
    deadline = time.time() + timeout
    while time.time() < deadline:
        for line in logs:
            match = marker.search(line)
            if match:
                return match.group(1)
        if proc.poll() is not None:
            raise RuntimeError(
                f"server exited before setup token (code {proc.returncode}):\n"
                + "\n".join(logs[-80:])
            )
        time.sleep(0.05)
    raise TimeoutError("server did not print a setup token:\n" + "\n".join(logs[-80:]))


def initialize_server(
    base_url: str,
    setup_token: str,
    database: dict[str, Any],
    *,
    username: str = "admin",
    password: str = "correct horse battery staple",
) -> WebSession:
    """Claim a fresh server, complete setup, and log in through public HTTP APIs."""
    session = WebSession(base_url)
    status, body = session.request(
        "POST", "/api/v1/setup/claim", {"token": setup_token}
    )
    assert status == 204, f"claim setup token failed: {status} {body}"
    status, body = session.request(
        "POST",
        "/api/v1/setup/complete",
        {"database": database, "username": username, "password": password},
        timeout=40.0,
    )
    assert status == 200, f"complete setup failed: {status} {body}"
    status, body = session.request(
        "POST", "/api/v1/auth/login", {"username": username, "password": password}
    )
    assert status == 200, f"admin login failed: {status} {body}"
    return session


def wait_until_ready(
    url: str,
    timeout: float = 30.0,
    headers: dict[str, str] | None = None,
) -> None:
    """Poll <url> until any non-connection-error response arrives (< 500)."""
    deadline = time.time() + timeout
    last_err: str = ""
    while time.time() < deadline:
        try:
            status, _ = http_request("GET", url, headers=headers, timeout=2.0)
            if status < 500:
                return
            last_err = f"status={status}"
        except (URLError, TimeoutError, OSError) as exc:
            last_err = str(exc)
        time.sleep(0.3)
    raise TimeoutError(f"server not ready at {url!r}: {last_err}")


# ── Stravia server process helpers ───────────────────────────────────────────────


def resolve_stravia_binary(repo_root: Path) -> Path:
    """Find the stravia-server binary: $STRAVIA_BINARY env or debug build fallback."""
    env_bin = os.environ.get("STRAVIA_BINARY")
    if env_bin:
        candidate = Path(env_bin)
        if not candidate.is_absolute():
            candidate = repo_root / candidate
        return candidate
    binary_name = "stravia-server.exe" if os.name == "nt" else "stravia-server"
    return repo_root / "target" / "debug" / binary_name


def start_stravia_server(
    *,
    stravia_binary: Path,
    args: list[str],
    env: dict[str, str] | None = None,
) -> tuple[subprocess.Popen[str], list[str]]:
    """Start stravia-server with explicit CLI arguments; return (proc, log_lines)."""
    logs: list[str] = []
    proc = subprocess.Popen(
        [str(stravia_binary), *args],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        env={**os.environ, **(env or {})},
    )

    def _drain() -> None:
        assert proc.stdout is not None
        for line in proc.stdout:
            logs.append(line.rstrip("\n"))

    threading.Thread(target=_drain, name="stravia-server-log", daemon=True).start()
    return proc, logs


def stop_stravia_server(
    proc: subprocess.Popen[str],
    logs: list[str],
    *,
    print_tail: int = 80,
) -> None:
    """Gracefully terminate stravia-server and print tail logs on non-zero exit."""
    if proc.poll() is None:
        proc.terminate()
        try:
            proc.wait(timeout=8)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=3)
    if proc.returncode not in (0, None, -15):
        tail = "\n".join(logs[-print_tail:])
        print("\n--- stravia-server logs (tail) ---", file=sys.stderr)
        print(tail, file=sys.stderr)


# ── Minimal mock provider ─────────────────────────────────────────────────────


class _MinimalMockHandler(BaseHTTPRequestHandler):
    """Single-endpoint OpenAI /v1/chat/completions happy-path mock."""

    protocol_version = "HTTP/1.1"

    def log_message(self, fmt: str, *args: Any) -> None:  # noqa: D401
        return

    def _read_body(self) -> dict[str, Any]:
        length = int(self.headers.get("content-length", "0"))
        raw = self.rfile.read(length) if length else b"{}"
        return json.loads(raw.decode("utf-8")) if raw else {}

    def _write_json(self, status: int, payload: dict[str, Any]) -> None:
        body = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.send_header("connection", "close")
        self.end_headers()
        self.wfile.write(body)
        self.wfile.flush()

    def do_POST(self) -> None:  # noqa: N802
        body = self._read_body()
        path = self.path.split("?", 1)[0]
        if path != "/v1/chat/completions":
            self._write_json(404, {"error": f"unknown path: {path}"})
            return
        model = str(body.get("model", "mock"))
        self._write_json(
            200,
            {
                "id": "chatcmpl-mock",
                "object": "chat.completion",
                "model": model,
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": "mock-ok"},
                        "finish_reason": "stop",
                    }
                ],
                "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5},
            },
        )


def minimal_mock_provider(port: int) -> tuple[ThreadingHTTPServer, threading.Thread]:
    """Start a minimal OpenAI-compatible mock on *port*; return (server, thread)."""
    server = ThreadingHTTPServer(("127.0.0.1", port), _MinimalMockHandler)
    t = threading.Thread(target=server.serve_forever, name="mock-provider", daemon=True)
    t.start()
    return server, t


