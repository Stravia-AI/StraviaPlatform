"""Shared test utilities for Stravia E2E test suites."""

from __future__ import annotations

import io
import json
import os
import re
import socket
import subprocess
import sys
import threading
import time
import zipfile
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


def http_bytes(
    method: str,
    url: str,
    payload: Any | None = None,
    headers: dict[str, str] | None = None,
    timeout: float = 15.0,
) -> tuple[int, dict[str, str], bytes]:
    """Make an HTTP request without interpreting its response body."""
    hdrs: dict[str, str] = dict(headers or {})
    data: bytes | None = None
    if payload is not None:
        hdrs.setdefault("content-type", "application/json")
        data = json.dumps(payload).encode("utf-8")

    request = Request(url=url, method=method, data=data, headers=hdrs)
    try:
        with urlopen(request, timeout=timeout) as response:
            return (
                int(response.status),
                {key.lower(): value for key, value in response.headers.items()},
                response.read(),
            )
    except HTTPError as error:
        return (
            int(error.code),
            {key.lower(): value for key, value in error.headers.items()},
            error.read(),
        )


def http_request(
    method: str,
    url: str,
    payload: Any | None = None,
    headers: dict[str, str] | None = None,
    timeout: float = 15.0,
) -> tuple[int, Any]:
    """Make an HTTP request and return (status_code, decoded_body)."""
    status, _, body = http_bytes(method, url, payload, headers, timeout)
    return status, _decode_body(body)


def download_observation_bundle(
    env: dict[str, Any], detail: dict[str, Any],
) -> tuple[dict[str, Any], dict[str, str], bytes]:
    """Download a one-use bundle while enforcing the metadata-only detail contract."""
    assert "debug_events" not in detail
    assert all("debug_events" not in run for run in detail.get("runs", []))
    if "interaction" in detail:
        resource = f"interactions/{detail['interaction']['id']}"
    else:
        resource = f"rejections/{detail['rejection']['id']}"
    status, ticket = http_request(
        "POST", f"{env['admin']}/api/v1/observations/{resource}/debug-bundle-tickets",
        payload={"through_sequence": detail["snapshot_sequence"]}, headers=env["auth"],
    )
    assert status == 200, ticket
    data = ticket["data"]
    download = data["download_url"]
    status, headers, archive = http_bytes(
        "GET", f"{env['admin']}{download}" if download.startswith("/") else download,
    )
    assert status == 200
    assert "application/zip" in headers["content-type"]
    return data, headers, archive


def observation_bundle_events(archive: bytes, run_id: str | None = None) -> list[dict[str, Any]]:
    """Read capture records from the ZIP's per-run or rejected-request JSONL files."""
    with zipfile.ZipFile(io.BytesIO(archive)) as bundle:
        return [
            json.loads(line)
            for name in bundle.namelist()
            if name.endswith(".jsonl")
            if run_id is None or name.endswith(f"-{run_id}/events.jsonl")
            for line in bundle.read(name).splitlines()
            if line.strip()
        ]


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
        {"database": database, "username": username, "password": password, "client_base_url": base_url},
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
    cwd: Path | None = None,
) -> tuple[subprocess.Popen[str], list[str]]:
    """Start stravia-server with explicit CLI arguments; return (proc, log_lines)."""
    logs: list[str] = []
    proc = subprocess.Popen(
        [str(stravia_binary), *args],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        env={**os.environ, **(env or {})},
        cwd=cwd,
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
        tail = re.sub(r"(Stravia setup token:)\s*\S+", r"\1 [redacted]", tail)
        print("\n--- stravia-server logs (tail) ---", file=sys.stderr)
        print(tail, file=sys.stderr)


# ── Minimal mock provider ─────────────────────────────────────────────────────


class _MinimalMockHandler(BaseHTTPRequestHandler):
    """Deterministic OpenAI upstream with a few externally-triggered scenarios."""

    protocol_version = "HTTP/1.1"
    _attempts: dict[str, int] = {}
    _attempts_lock = threading.Lock()

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

    def _write_sse(self, payloads: list[dict[str, Any]]) -> None:
        body = b"".join(
            b"data: " + json.dumps(payload).encode("utf-8") + b"\n\n"
            for payload in payloads
        ) + b"data: [DONE]\n\n"
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
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
        messages = body.get("messages", [])
        scenario = json.dumps(messages, sort_keys=True, separators=(",", ":"))

        if "observation-visible-credential" in scenario and body.get("stream") is True:
            sentinel = "VISIBLE_RESPONSE_SECRET_7c91"
            fragments = [
                "retain-visible-business-output Authorization: Bear",
                f"er {sentinel} callback=https://user:",
                f"{sentinel}@example.test/cb?signature=",
                f'{sentinel} metadata={{"api_',
                f'key":"{sentinel}"}} form=name=Ada&access_',
                f"token={sentinel}",
            ]
            if "observation-visible-credential-fragmented" not in scenario:
                fragments = ["".join(fragments)]
            payloads = [
                {
                    "id": "chatcmpl-visible-redaction",
                    "object": "chat.completion.chunk",
                    "model": model,
                    "choices": [
                        {
                            "index": 0,
                            "delta": {"content": fragment},
                            "finish_reason": None,
                        }
                    ],
                }
                for fragment in fragments
            ]
            payloads.append(
                {
                    "id": "chatcmpl-visible-redaction",
                    "object": "chat.completion.chunk",
                    "model": model,
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5},
                }
            )
            self._write_sse(payloads)
            return

        if "observation-delay" in scenario:
            time.sleep(3.0)

        if "observation-root-retry" in scenario:
            with self._attempts_lock:
                attempt = self._attempts.get(scenario, 0)
                self._attempts[scenario] = attempt + 1
            if attempt == 0:
                self._write_json(
                    503,
                    {"error": {"type": "upstream_unavailable", "message": "retry"}},
                )
                return

        tool_results = sum(
            1 for message in messages if isinstance(message, dict) and message.get("role") == "tool"
        )
        tool_loop = "observation-tool-loop" in scenario
        branch = "observation-branch" in scenario
        if (tool_loop and tool_results < 3) or (branch and tool_results == 0):
            call_id = f"call-observation-{tool_results + 1}"
            message: dict[str, Any] = {
                "role": "assistant",
                "content": None,
                "tool_calls": [
                    {
                        "id": call_id,
                        "type": "function",
                        "function": {
                            "name": "local_probe",
                            "arguments": json.dumps({"round": tool_results + 1}),
                        },
                    }
                ],
            }
            finish_reason = "tool_calls"
        else:
            content = f"mock-ok-{tool_results}"
            if "observation-visible-credential" in scenario:
                sentinel = "VISIBLE_RESPONSE_SECRET_7c91"
                content = (
                    "retain-visible-business-output "
                    f"Authorization: Bearer {sentinel} "
                    f"callback=https://user:{sentinel}@example.test/cb?signature={sentinel} "
                    f'metadata={{"api_key":"{sentinel}"}} '
                    f"form=name=Ada&access_token={sentinel}"
                )
            message = {"role": "assistant", "content": content}
            finish_reason = "stop"

        self._write_json(
            200,
            {
                "id": f"chatcmpl-mock-{tool_results}",
                "object": "chat.completion",
                "model": model,
                "choices": [
                    {"index": 0, "message": message, "finish_reason": finish_reason}
                ],
                # Deliberately omit cache/reasoning dimensions: they must remain unknown.
                "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5},
            },
        )


def minimal_mock_provider(port: int) -> tuple[ThreadingHTTPServer, threading.Thread]:
    """Start a minimal OpenAI-compatible mock on *port*; return (server, thread)."""
    server = ThreadingHTTPServer(("127.0.0.1", port), _MinimalMockHandler)
    t = threading.Thread(target=server.serve_forever, name="mock-provider", daemon=True)
    t.start()
    return server, t


