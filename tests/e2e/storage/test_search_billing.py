from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any

import pytest

from tests.common.helpers import WebSession, http_request
from tests.e2e.storage.test_cost_and_billing import admin, money, wait_spent
from tests.e2e.storage.test_usage_statistics import stats, usage_env  # noqa: F401
from tests.e2e.storage.test_vendor_plugins import _install_plugin

pytestmark = [pytest.mark.e2e, pytest.mark.storage]


class SearchUpstream(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self) -> None:
        super().__init__(("127.0.0.1", 0), SearchHandler)
        self.requests: list[dict[str, Any]] = []
        self.lock = threading.Lock()

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.server_port}"


class SearchHandler(BaseHTTPRequestHandler):
    server: SearchUpstream

    def log_message(self, *_args: Any) -> None:
        pass

    def do_POST(self) -> None:
        assert self.path == "/search", self.path
        length = int(self.headers.get("content-length", "0"))
        request = json.loads(self.rfile.read(length))
        with self.server.lock:
            self.server.requests.append(request)
        body = json.dumps({
            "answer": "Billing fixture evidence [sc:fixture].",
            "sources": [{
                "id": "fixture",
                # A literal public address keeps report validation offline.
                "url": "https://93.184.216.34/search-billing",
                "title": "Search billing fixture",
            }],
            "limitations": [],
            # The external Search contract keeps absent usage unknown; the
            # attempt must record unknown cost instead of inventing zero tokens.
            "usage": None,
        }).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def _admin_session(env: dict[str, Any]) -> WebSession:
    """Re-authenticate so the raw Wasm import helper can use a WebSession."""
    session = WebSession(env["admin"])
    status, body = session.request(
        "POST", "/api/v1/auth/login",
        {"username": "admin", "password": "correct horse battery staple"},
    )
    assert status == 200, body
    env["auth"] = session.auth_headers()
    return session


def _search_route(env: dict[str, Any], base_url: str) -> tuple[dict[str, Any], dict[str, Any]]:
    provider = admin(env, "POST", "providers", {
        "name": "Search billing upstream",
        "source": {"type": "custom", "vendor": "fixture.capability-contract",
                   "channel": "default", "base_url": base_url},
        "credential": {"type": "fields", "values": {}},
        "vendor_options": {},
    })
    admin(env, "POST", f"providers/{provider['id']}/models", {
        "model_id": "fixture-model",
        "metadata": {
            "id": "fixture-model",
            "name": "Fixture search model",
            "capabilities": ["search"],
            "modalities": {"input": ["text"], "output": ["text"]},
        },
    })
    route = admin(env, "POST", "models", {
        "model_id": "search-billing",
        "output_modality": "text",
        "billing": {"mode": "per_call", "amount": "0.01", "currency": "usd"},
        "targets": [{"provider_id": provider["id"], "model": "fixture-model",
                     "target_retry_budget": 0, "target_cooldown_ms": 0}],
    })
    key = admin(env, "POST", "api-keys", {
        "name": "search-billing-key", "model_ids": [], "mcp_access_enabled": True,
    })
    return route, key


def test_external_search_route_bills_attempt_to_root_api_key(usage_env, repo_root: Path):
    env = usage_env
    session = _admin_session(env)
    _install_plugin(session, repo_root / "target" / "vendor-test-fixtures", "capability-multi.wasm")
    upstream = SearchUpstream()
    thread = threading.Thread(target=upstream.serve_forever, daemon=True)
    thread.start()
    try:
        route, key = _search_route(env, upstream.url)
        admin(env, "PUT", "web-search/config", {
            "revision": 0,
            "enabled": True,
            "backend": {"kind": "external", "route_id": route["model_id"]},
            "max_turns": 12,
            "total_time_seconds": 600,
            "updated_at": "",
        })

        auth = {"Authorization": f"Bearer {key['key']}"}
        status, result = http_request("POST", f"{env['proxy']}/mcp", {
            "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
                "name": "StraviaRead",
                "arguments": {"path": "search://billing%20probe"},
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
            },
        }, {
            **auth,
            "Accept": "application/json, text/event-stream",
            "MCP-Protocol-Version": "2026-07-28",
            "Mcp-Method": "tools/call",
            "Mcp-Name": "StraviaRead",
        }, timeout=60)
        assert status == 200, result
        assert not result["result"]["isError"], result
        structured = result["result"]["structuredContent"]
        assert structured["completion"] == "complete", structured
        assert structured["report"]["sources"][0]["url"] == "https://93.184.216.34/search-billing"
        assert "stravia://turns/" in structured["report"]["answer"], structured
        assert len(upstream.requests) == 1, upstream.requests
        assert upstream.requests[0]["query"] == "billing probe", upstream.requests

        # The spend trigger and stats buckets commit with the attempt; the
        # spend read is the public asynchronous barrier used by billing tests.
        wait_spent(env, key, "0.01")

        api_key_stats = stats(env, "api-keys")
        assert [row["api_key_id"] for row in api_key_stats] == [key["id"]]
        money(api_key_stats[0]["billing"], "0.01", 1)
        money(api_key_stats[0]["cost"], "0", 0, 1)
        assert api_key_stats[0]["total_input_tokens"] is None
        assert api_key_stats[0]["total_output_tokens"] is None

        # The standalone MCP read turn and the capability turn share the key;
        # the called Route owns the capability turn and its billed attempt.
        route_stats = [row for row in stats(env, "models") if row["model_id"] == route["id"]]
        assert len(route_stats) == 1
        assert route_stats[0]["request_count"] == 1
        money(route_stats[0]["billing"], "0.01", 1)
        money(route_stats[0]["cost"], "0", 0, 1)
        assert route_stats[0]["total_input_tokens"] is None
        assert route_stats[0]["total_output_tokens"] is None

        overview = stats(env)
        money(overview["actual_cost"], "0", 0, 1)
        assert overview["total_input_tokens"] is None
        assert overview["total_output_tokens"] is None
    finally:
        upstream.shutdown()
        upstream.server_close()
        thread.join(timeout=10)
