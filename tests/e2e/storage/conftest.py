from __future__ import annotations

import json
import os
import secrets
import socket
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from queue import Empty, Queue
from typing import Any
from urllib.parse import urlsplit

import pytest

REPO_ROOT = Path(__file__).resolve().parents[3]


def find_free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def make_isolated_schema(prefix: str = "stravia_storage_e2e", *, max_len: int = 63) -> str:
    suffix = f"{int(time.time())}_{secrets.token_hex(3)}"
    keep = max(1, max_len - len(suffix) - 1)
    return f"{prefix[:keep]}_{suffix}"


def load_pg_url() -> str | None:
    """Select the runner's worker database, or preserve standalone DB_URL behavior."""
    mapping_json = os.environ.get("STRAVIA_STORAGE_TEST_POSTGRES_URLS")
    if mapping_json is None:
        return os.environ.get("DB_URL")
    worker = os.environ.get("PYTEST_XDIST_WORKER", "master")
    try:
        mapping = json.loads(mapping_json)
    except (ValueError, TypeError):
        raise RuntimeError("STRAVIA_STORAGE_TEST_POSTGRES_URLS must be a valid JSON object") from None
    if not isinstance(mapping, dict):
        raise RuntimeError("STRAVIA_STORAGE_TEST_POSTGRES_URLS must be a JSON object")
    url = mapping.get(worker)
    if not isinstance(url, str) or not url:
        raise RuntimeError(f"STRAVIA_STORAGE_TEST_POSTGRES_URLS has no valid URL for worker {worker}")
    try:
        parsed = urlsplit(url)
        valid = (
            parsed.scheme in {"postgres", "postgresql"}
            and bool(parsed.hostname)
            and bool(parsed.path.strip("/"))
            and (parsed.port is None or 0 < parsed.port <= 65535)
        )
    except ValueError:
        valid = False
    if not valid:
        raise RuntimeError(f"STRAVIA_STORAGE_TEST_POSTGRES_URLS has an invalid PostgreSQL URL for worker {worker}")
    return url


class _MockHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt: str, *args: object) -> None:
        return

    def _write_json(self, status: int, payload: dict[str, object]) -> None:
        body = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.send_header("connection", "close")
        self.end_headers()
        self.wfile.write(body)
        self.wfile.flush()

    def do_POST(self) -> None:  # noqa: N802
        length = int(self.headers.get("content-length", "0"))
        raw = self.rfile.read(length) if length else b"{}"
        body = json.loads(raw) if raw else {}
        if self.path.split("?")[0] != "/v1/chat/completions":
            self._write_json(404, {"error": "not found"})
            return
        model = str(body.get("model", "mock"))
        self._write_json(
            200,
            {
                "id": "chatcmpl-storage-e2e",
                "object": "chat.completion",
                "model": model,
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": "ok"},
                        "finish_reason": "stop",
                    }
                ],
                "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4},
            },
        )


def start_mock(port: int) -> ThreadingHTTPServer:
    server = ThreadingHTTPServer(("127.0.0.1", port), _MockHandler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


class LifecycleUpstream(ThreadingHTTPServer):
    """Local upstream that exposes each real vendor-component request to a test."""

    def __init__(self) -> None:
        super().__init__(("127.0.0.1", 0), _LifecycleHandler)
        self.requests: Queue[dict[str, Any]] = Queue()

    @property
    def base_url(self) -> str:
        host, port = self.server_address
        return f"http://{host}:{port}"

    def next_request(self) -> dict[str, Any]:
        try:
            return self.requests.get(timeout=10)
        except Empty as error:
            raise AssertionError("vendor component did not reach the local upstream") from error


class _LifecycleHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt: str, *args: object) -> None:
        return

    def do_POST(self) -> None:  # noqa: N802
        length = int(self.headers.get("content-length", "0"))
        body = self.rfile.read(length) if length else b""
        server = self.server
        assert isinstance(server, LifecycleUpstream)
        server.requests.put(
            {
                "method": "POST",
                "path": self.path,
                "headers": {key.lower(): value for key, value in self.headers.items()},
                "body": body,
            }
        )
        payload = json.dumps(
            {
                "id": "fixture-response",
                "model": "fixture-model",
                "items": [{"role": "assistant", "content": "fixture-ok"}],
                "stop_reason": "stop",
                "usage": {
                    "prompt_tokens": 1,
                    "completion_tokens": 1,
                    "total_tokens": 2,
                    "required_components_known": True,
                },
                "embedding_output": None,
                "error": None,
                "vendor": {"ingress": {}, "egress": {}, "passthrough_safe": {}},
            }
        ).encode("utf-8")
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(payload)))
        self.send_header("connection", "close")
        self.end_headers()
        self.wfile.write(payload)
        self.wfile.flush()


def storage_harness_binary() -> Path:
    override = os.environ.get("STRAVIA_STORAGE_HARNESS_BINARY")
    binary = (
        Path(override).resolve()
        if override
        else REPO_ROOT / "target" / "debug" / (
            "stravia-storage-test-harness.exe" if os.name == "nt" else "stravia-storage-test-harness"
        )
    )
    if not binary.is_file():
        raise RuntimeError(
            f"storage test harness binary not found: {binary}; build with "
            "cargo build --locked -p stravia-server -p stravia-devtools "
            "--features stravia-server/test-harness, or set STRAVIA_STORAGE_HARNESS_BINARY"
        )
    return binary


def run_harness(
    backend: str,
    *,
    upstream_port: int,
    work_dir: Path,
    pg_url: str | None = None,
) -> str:
    env = os.environ.copy()
    env["STRAVIA_STORAGE_BACKEND"] = backend
    env["STRAVIA_STORAGE_UPSTREAM"] = f"http://127.0.0.1:{upstream_port}"
    env["STRAVIA_STORAGE_SERVER_PORT"] = str(find_free_port())
    env["STRAVIA_STORAGE_DATA_DIR"] = str(work_dir / f"{backend}-data")

    if backend == "postgres":
        if not pg_url:
            raise RuntimeError("postgres backend requires DB_URL")
        env["STRAVIA_STORAGE_PG_URL"] = pg_url
        env["STRAVIA_STORAGE_PG_SCHEMA"] = make_isolated_schema()

    proc = subprocess.run(
        [str(storage_harness_binary())],
        env=env,
        cwd=str(REPO_ROOT),
        text=True,
        encoding="utf-8",
        capture_output=True,
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            f"backend={backend} harness failed\nstdout:\n{proc.stdout}\nstderr:\n{proc.stderr}"
        )
    return proc.stdout


def postgres_dsn_for_schema(pg_url: str, schema: str) -> str:
    search_path = f"options=-csearch_path%3D{schema}"
    separator = "&" if "?" in pg_url else "?"
    return f"{pg_url}{separator}{search_path}"


def run_schema_action(action: str, *, pg_url: str, schema: str) -> str:
    env = os.environ.copy()
    env["STRAVIA_STORAGE_SCHEMA_ACTION"] = action
    env["STRAVIA_STORAGE_PG_URL"] = pg_url
    env["STRAVIA_STORAGE_PG_SCHEMA"] = schema

    proc = subprocess.run(
        [str(storage_harness_binary())],
        env=env,
        cwd=str(REPO_ROOT),
        text=True,
        encoding="utf-8",
        capture_output=True,
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            f"schema action={action} failed\nstdout:\n{proc.stdout}\nstderr:\n{proc.stderr}"
        )
    return proc.stdout


@pytest.fixture
def lifecycle_upstream() -> LifecycleUpstream:
    server = LifecycleUpstream()
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        yield server
    finally:
        server.shutdown()
        server.server_close()


@pytest.fixture(scope="module")
def storage_runtime() -> dict[str, object]:
    storage_harness_binary()
    upstream_port = find_free_port()
    mock_server = start_mock(upstream_port)

    try:
        with tempfile.TemporaryDirectory(prefix="stravia-storage-e2e-") as tmp:
            tmpdir = Path(tmp)
            yield {
                "upstream_port": upstream_port,
                "work_dir": tmpdir,
                "pg_url": load_pg_url(),
                "make_isolated_schema": make_isolated_schema,
                "run_harness": run_harness,
                "postgres_dsn_for_schema": postgres_dsn_for_schema,
                "run_schema_action": run_schema_action,
            }
    finally:
        mock_server.shutdown()
        mock_server.server_close()
