"""Proxy/protocol-conversion E2E tests, backed by ``stravia-tools replay``.

Pipeline orchestrated by these fixtures:

  1. Scan ``tests/e2e/fixtures/<protocol>/<vendor>/*.jsonl`` collecting every
     ``replay_model`` string.
  2. Spawn one ``stravia-tools replay`` subprocess per protocol on ports
     25208-25211 (or ephemeral if those are busy).
  3. Boot the unified ``stravia-server`` and configure replay providers and models
     through its Admin API.
  4. Yield the proxy base URL.

If the fixtures tree is empty (typical for a fresh checkout) the suite is
skipped so CI stays green until users contribute recorded fixtures.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import threading
from collections import defaultdict
from pathlib import Path
from typing import Iterator

import pytest

from tests.common.helpers import (
    find_free_port,
    http_request,
    is_port_free,
    start_stravia_server,
    stop_stravia_server,
    wait_until_ready,
)

# Public protocol short-names (kebab-case) — must mirror stravia-tools/protocol.rs
PROTOCOLS: tuple[str, ...] = (
    "openai-chat",
    "open-responses",
    "anthropic-messages",
    "google-content",
)

# Map stravia-tools' kebab-case protocol to the provider protocol accepted by the
# Admin API. Each replay provider receives the exact ingress suite it records.
PROVIDER_PROTOCOL: dict[str, str] = {
    "openai-chat": "openai-compatible",
    "open-responses": "open-responses",
    "anthropic-messages": "anthropic-messages",
    "google-content": "google-gemini",
}

# Path suffix appended to the replay base URL inside `endpoints[*].base_url`.
# OpenAI-shape upstreams expect a `/v1` prefix; Anthropic / Gemini use the
# bare host, since their paths are absolute (`/v1/messages`,
# `/v1beta/models/...`).
STRAVIA_BASE_URL_PATH: dict[str, str] = {
    "openai-chat": "/v1",
    "open-responses": "/v1",
    "anthropic-messages": "",
    "google-content": "",
}

DEFAULT_REPLAY_PORTS: tuple[int, ...] = (25208, 25209, 25210, 25211)
FIXTURES_ROOT = Path(__file__).resolve().parents[2] / "e2e" / "fixtures"


# ---------------------------------------------------------------------------
# fixture discovery
# ---------------------------------------------------------------------------


def _scan_replay_models() -> dict[str, list[str]]:
    """Return ``{protocol: [replay_model, ...]}`` for every recorded fixture."""
    out: dict[str, list[str]] = defaultdict(list)
    for protocol in PROTOCOLS:
        protocol_dir = FIXTURES_ROOT / protocol
        if not protocol_dir.exists():
            continue
        for jsonl in sorted(protocol_dir.rglob("*.jsonl")):
            try:
                first = jsonl.read_text(encoding="utf-8").splitlines()[0]
                doc = json.loads(first)
            except (OSError, IndexError, json.JSONDecodeError) as exc:
                pytest.fail(f"unreadable fixture {jsonl}: {exc}")
            replay_model = doc.get("replay_model")
            recorded_protocol = doc.get("protocol")
            if not replay_model or recorded_protocol != protocol:
                pytest.fail(
                    f"fixture {jsonl} has invalid replay_model/protocol: "
                    f"replay_model={replay_model!r} protocol={recorded_protocol!r}"
                )
            out[protocol].append(replay_model)
    return dict(out)


def _choose_ports() -> list[int]:
    if all(is_port_free(p) for p in DEFAULT_REPLAY_PORTS):
        return list(DEFAULT_REPLAY_PORTS)
    return [find_free_port() for _ in PROTOCOLS]


# ---------------------------------------------------------------------------
# Admin configuration
# ---------------------------------------------------------------------------


def _configure_proxy_routes(
    admin_base: str,
    admin_headers: dict[str, str],
    replay_ports: dict[str, int],
    replay_models: dict[str, list[str]],
) -> str:
    provider_ids: dict[str, str] = {}
    for protocol in PROTOCOLS:
        port = replay_ports[protocol]
        suffix = STRAVIA_BASE_URL_PATH[protocol]
        status, body = http_request(
            "POST",
            f"{admin_base}/api/v1/providers",
            payload={
                "name": f"replay-{protocol}",
                "source": {
                    "type": "custom",
                    "vendor": "custom",
                    "protocol": PROVIDER_PROTOCOL[protocol],
                    "base_url": f"http://127.0.0.1:{port}{suffix}",
                },
                "credential": {"type": "api_key", "value": "replay"},
            },
            headers=admin_headers,
        )
        assert status == 200, f"create replay provider failed: {status} {body}"
        provider_ids[protocol] = body["data"]["id"]

    for protocol in PROTOCOLS:
        for replay_model in replay_models.get(protocol, []):
            metadata = (
                {"reasoning_options": [{"type": "budget_tokens"}]}
                if protocol == "anthropic-messages"
                else {}
            )
            status, body = http_request(
                "POST",
                f"{admin_base}/api/v1/providers/{provider_ids[protocol]}/models",
                payload={"model_id": replay_model, "metadata": metadata},
                headers=admin_headers,
            )
            assert status == 201, f"create replay provider model failed: {status} {body}"

    model_ids: list[str] = []
    for protocol in PROTOCOLS:
        for replay_model in replay_models.get(protocol, []):
            status, body = http_request(
                "POST",
                f"{admin_base}/api/v1/models",
                payload={
                    "name": replay_model,
                    "target_provider": provider_ids[protocol],
                    "target_model": replay_model,
                },
                headers=admin_headers,
            )
            assert (
                status == 200 and "data" in body
            ), f"create replay model failed: {status} {body}"
            model_ids.append(body["data"]["id"])

    status, body = http_request(
        "POST",
        f"{admin_base}/api/v1/api-keys",
        payload={"name": "proxy-e2e", "model_ids": model_ids},
        headers=admin_headers,
    )
    assert status == 200, f"create proxy API key failed: {status} {body}"
    return str(body["data"]["key"])


# ---------------------------------------------------------------------------
# subprocess management
# ---------------------------------------------------------------------------


def _start_replay(
    stravia_devtools: Path, protocol: str, port: int
) -> tuple[subprocess.Popen[str], list[str]]:
    logs: list[str] = []
    proc = subprocess.Popen(
        [
            str(stravia_devtools),
            "replay",
            "-p",
            protocol,
            "-i",
            str(FIXTURES_ROOT),
            "-P",
            str(port),
            "-H",
            "127.0.0.1",
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        env={**os.environ, "RUST_LOG": "info"},
    )

    def _drain() -> None:
        assert proc.stdout is not None
        for line in proc.stdout:
            logs.append(line.rstrip("\n"))

    threading.Thread(
        target=_drain, name=f"replay-log-{protocol}", daemon=True
    ).start()
    return proc, logs


def _stop_proc(proc: subprocess.Popen[str], logs: list[str], label: str) -> None:
    if proc.poll() is None:
        proc.terminate()
        try:
            proc.wait(timeout=8)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=3)
    if proc.returncode not in (0, None, -15):
        tail = "\n".join(logs[-80:])
        print(f"\n--- {label} logs (tail) ---", file=sys.stderr)
        print(tail, file=sys.stderr)


# ---------------------------------------------------------------------------
# fixtures
# ---------------------------------------------------------------------------


@pytest.fixture(scope="session")
def stravia_devtools_binary(repo_root: Path) -> Path:
    env_bin = os.environ.get("STRAVIA_TOOLS_BINARY")
    if env_bin:
        candidate = Path(env_bin)
        if not candidate.is_absolute():
            candidate = repo_root / candidate
    else:
        binary_name = "stravia-tools.exe" if os.name == "nt" else "stravia-tools"
        candidate = repo_root / "target" / "debug" / binary_name
    if not candidate.exists():
        pytest.skip(
            f"stravia-tools binary not found at {candidate}; "
            "run `cargo build -p stravia-devtools` or set STRAVIA_TOOLS_BINARY"
        )
    return candidate


@pytest.fixture(scope="session")
def scenario_metadata(stravia_devtools_binary: Path) -> dict[str, dict]:
    """Return ``{scenario_name: metadata}`` from ``stravia-tools print-scenarios``."""
    out = subprocess.run(
        [str(stravia_devtools_binary), "print-scenarios"],
        capture_output=True,
        text=True,
        check=True,
    )
    doc = json.loads(out.stdout)
    return {entry["name"]: entry for entry in doc["scenarios"]}


@pytest.fixture(scope="session")
def replay_models() -> dict[str, list[str]]:
    return _scan_replay_models()


@pytest.fixture(scope="module")
def replay_cluster(
    stravia_devtools_binary: Path, replay_models: dict[str, list[str]]
) -> Iterator[dict[str, int]]:
    if not any(replay_models.values()):
        pytest.skip(
            "no fixtures recorded under tests/e2e/fixtures/ — "
            "run `stravia-tools record` to populate at least one vendor first"
        )
    ports = _choose_ports()
    port_map = dict(zip(PROTOCOLS, ports))
    procs: list[tuple[str, subprocess.Popen[str], list[str]]] = []
    try:
        for protocol, port in port_map.items():
            proc, logs = _start_replay(stravia_devtools_binary, protocol, port)
            procs.append((protocol, proc, logs))
        for port in ports:
            wait_until_ready(f"http://127.0.0.1:{port}/", timeout=15.0)
        yield port_map
    finally:
        for protocol, proc, logs in procs:
            _stop_proc(proc, logs, f"stravia-tools replay {protocol}")


@pytest.fixture(scope="module")
def stravia_proxy_base(
    stravia_binary: Path,
    replay_cluster: dict[str, int],
    replay_models: dict[str, list[str]],
) -> Iterator[tuple[str, str]]:
    server_port = find_free_port()
    admin_token = "proxy-e2e-token"
    data_dir = tempfile.TemporaryDirectory(prefix="stravia-proxy-e2e-")
    proc, logs = start_stravia_server(
        stravia_binary=stravia_binary,
        args=[
            "--host",
            "127.0.0.1",
            "--port",
            str(server_port),
            "--admin-token",
            admin_token,
            "--data-dir",
            data_dir.name,
        ],
    )
    base = f"http://127.0.0.1:{server_port}"
    admin_base = base
    admin_headers = {"authorization": f"Bearer {admin_token}"}
    try:
        wait_until_ready(
            f"{admin_base}/api/v1/status", timeout=30.0, headers=admin_headers
        )
        api_key = _configure_proxy_routes(
            admin_base, admin_headers, replay_cluster, replay_models
        )
        wait_until_ready(f"{base}/v1/chat/completions", timeout=30.0)
        yield base, api_key
    finally:
        stop_stravia_server(proc, logs)
        data_dir.cleanup()
