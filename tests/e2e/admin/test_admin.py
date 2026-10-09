from __future__ import annotations

import subprocess
import tempfile
import json
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlsplit
from urllib.request import urlopen
from pathlib import Path
from typing import Any
from xml.etree import ElementTree

import pytest

from tests.common.helpers import (
    find_free_port,
    http_request,
    http_bytes,
    start_stravia_server,
    stop_stravia_server,
    wait_until_ready,
)

@contextmanager
def _model_probe_endpoint(*, forward: bool = False):
    received: list[dict[str, Any]] = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args: Any) -> None:
            pass

        def do_GET(self) -> None:
            received.append({"path": self.path, "headers": {key.lower(): value for key, value in self.headers.items()}})
            selected = urlsplit(self.path)
            if forward:
                assert selected.hostname == "127.0.0.1", "proxy must remain local"
                connection = HTTPConnection(selected.hostname, selected.port, timeout=10)
                try:
                    connection.request(
                        "GET", selected.path + ("?" + selected.query if selected.query else ""),
                        headers=dict(self.headers),
                    )
                    upstream = connection.getresponse()
                    status, body = upstream.status, upstream.read()
                finally:
                    connection.close()
            else:
                status = 503 if selected.path == "/failure" else 200
                body = json.dumps({"data": [{"id": "probe-model"}]}).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_POST(self) -> None:
            request = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
            received.append({
                "path": self.path,
                "headers": {key.lower(): value for key, value in self.headers.items()},
                "body": request,
            })
            response = {
                "id": "chatcmpl-auth-probe", "object": "chat.completion", "created": 1,
                "model": "probe-model",
                "choices": [{
                    "index": 0, "message": {"role": "assistant", "content": "local auth probe succeeded"},
                    "finish_reason": "stop",
                }],
                "usage": {"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7},
            }
            if request.get("stream"):
                response["object"] = "chat.completion.chunk"
                response["choices"][0]["delta"] = response["choices"][0].pop("message")
                body = b"data: " + json.dumps(response).encode() + b"\n\ndata: [DONE]\n\n"
                content_type = "text/event-stream"
            else:
                body = json.dumps(response).encode()
                content_type = "application/json"
            self.send_response(200)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}", received
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


def _create_probe_provider(
    env: dict[str, Any], name: str, endpoint: str | None, *,
    credential: dict[str, Any] | None = None, use_proxy: bool = True, **source: Any,
) -> str:
    status, body = http_request(
        "POST", f"{env['admin']}/api/v1/providers",
        payload={
            "name": name,
            "source": {
                "type": "custom", "vendor": "custom", "channel": "default",
                "protocol": "openai-compatible", "base_url": env["mock"],
                "models_source": endpoint, **source,
            },
            "credential": credential if credential is not None else {
                "type": "api_key", "value": "synthetic&key=part+/%?#",
            },
            "vendor_options": {},
            "use_proxy": use_proxy,
        },
        headers=env["auth"],
    )
    assert status == 200, body
    return body["data"]["id"]


@pytest.mark.e2e
@pytest.mark.admin
def test_optional_api_key_provider_discovers_and_infers_without_upstream_auth(
    admin_env: dict[str, Any],
) -> None:
    with _model_probe_endpoint() as (origin, received):
        provider = _create_probe_provider(
            admin_env, "optional-key-provider", f"{origin}/models",
            credential={"type": "none"}, use_proxy=False, base_url=origin,
        )
        provider_url = f"{admin_env['admin']}/api/v1/providers/{provider}"

        def assert_upstream_auth(request: dict[str, Any], upstream_key: str | None) -> None:
            headers = request["headers"]
            assert "x-api-key" not in headers, request
            if upstream_key is None:
                assert "authorization" not in headers, request
            else:
                assert headers.get("authorization") == f"Bearer {upstream_key}", request

        def discover(upstream_key: str | None) -> None:
            for method, path in (("GET", "test-models"), ("POST", "models/sync")):
                before = len(received)
                status, body = http_request(
                    method, f"{provider_url}/{path}",
                    payload={} if method == "POST" else None, headers=admin_env["auth"],
                )
                assert status == 200 and "error" not in body, body
                if method == "GET":
                    assert body["data"] == ["probe-model"], body
                assert len(received) == before + 1, received
                assert received[-1]["path"] == "/models", received[-1]
                assert_upstream_auth(received[-1], upstream_key)
            status, body = http_request("GET", f"{provider_url}/models", headers=admin_env["auth"])
            assert status == 200, body
            assert [model["id"] for model in body["data"]["models"]] == ["probe-model"], body

        discover(None)
        route = _create_model(
            admin_env, provider, "optional-key-route", target_model="probe-model",
        )
        api_key = _create_api_key(admin_env, route, "optional-key-client")

        def infer(upstream_key: str | None) -> None:
            before = len(received)
            status, body = http_request(
                "POST", f"{admin_env['proxy']}/v1/chat/completions",
                payload={
                    "model": "optional-key-route", "stream": False,
                    "messages": [{"role": "user", "content": "verify local upstream authentication"}],
                },
                headers={"authorization": f"Bearer {api_key['key']}"},
            )
            assert status == 200, body
            assert body["choices"][0]["message"]["content"] == "local auth probe succeeded", body
            assert body["choices"][0]["finish_reason"] == "stop", body
            assert len(received) == before + 1, received
            request = received[-1]
            assert request["path"] == "/v1/chat/completions", request
            assert request["body"]["model"] == "probe-model", request
            assert_upstream_auth(request, upstream_key)
            assert all(api_key["key"] not in value for value in request["headers"].values()), request

        infer(None)
        upstream_key = "local-upstream-auth-probe-key"
        status, body = http_request(
            "PUT", provider_url, payload={"api_key": upstream_key}, headers=admin_env["auth"],
        )
        assert status == 200, body
        discover(upstream_key)
        infer(upstream_key)


@pytest.mark.e2e
@pytest.mark.admin
def test_model_discovery_uses_saved_proxy_and_preserves_direct_access(admin_env: dict[str, Any]) -> None:
    settings = {}
    for key in ("proxy_enabled", "proxy_url"):
        status, body = http_request(
            "GET", f"{admin_env['admin']}/api/v1/settings/{key}", headers=admin_env["auth"],
        )
        assert status == 200, body
        settings[key] = body["data"] or ""

    def set_setting(key: str, value: str) -> None:
        status, body = http_request(
            "PUT", f"{admin_env['admin']}/api/v1/settings/{key}",
            payload={"value": value}, headers=admin_env["auth"],
        )
        assert status == 200, body

    with _model_probe_endpoint() as (origin, upstream), _model_probe_endpoint(forward=True) as (proxy, forwarded):
        endpoint = f"{origin}/selected/models?region=east%2Bwest&limit=2"
        provider = _create_probe_provider(admin_env, "probe-network-route", endpoint)
        provider_url = f"{admin_env['admin']}/api/v1/providers/{provider}"
        try:
            set_setting("proxy_url", proxy)
            set_setting("proxy_enabled", "true")
            status, body = http_request("GET", f"{provider_url}/test-models", headers=admin_env["auth"])
            assert status == 200 and body["data"] == ["probe-model"], body
            assert forwarded[0]["path"] == endpoint
            assert upstream[0]["path"] == "/selected/models?region=east%2Bwest&limit=2"
            assert upstream[0]["headers"]["authorization"] == "Bearer synthetic&key=part+/%?#"

            status, body = http_request(
                "POST", f"{provider_url}/models/sync", payload={}, headers=admin_env["auth"],
            )
            assert status == 200, body
            assert len(forwarded) == 2 and len(upstream) == 2

            status, body = http_request(
                "PUT", provider_url, payload={"use_proxy": False}, headers=admin_env["auth"],
            )
            assert status == 200, body
            set_setting("proxy_url", "")
            status, body = http_request("GET", f"{provider_url}/test-models", headers=admin_env["auth"])
            assert status == 200 and body["data"] == ["probe-model"], body
            assert len(forwarded) == 2 and len(upstream) == 3

            status, body = http_request(
                "PUT", provider_url, payload={"use_proxy": True}, headers=admin_env["auth"],
            )
            assert status == 200, body
            for method, path in (("GET", "test-models"), ("POST", "models/sync")):
                status, body = http_request(
                    method, f"{provider_url}/{path}",
                    payload={} if method == "POST" else None, headers=admin_env["auth"],
                )
                assert status == (200 if method == "GET" else 400), body
                assert "error" in body and "data" not in body, body
            assert len(upstream) == 3 and len(forwarded) == 2
        finally:
            for key, value in settings.items():
                set_setting(key, value)


@pytest.mark.e2e
@pytest.mark.admin
@pytest.mark.parametrize("protocol", ["gemini", "google-gemini/generate-content/v1beta"])
def test_google_models_uses_declared_models_auth_not_inference_auth(
    admin_env: dict[str, Any], protocol: str,
) -> None:
    with _model_probe_endpoint() as (origin, received):
        endpoint = f"{origin}/custom/inventory?key=endpoint-owned%2Bvalue&region=east"
        provider = _create_probe_provider(
            admin_env, f"google-models-{protocol}", endpoint, vendor="google", protocol=protocol,
        )
        provider_url = f"{admin_env['admin']}/api/v1/providers/{provider}"
        for method, path in (("GET", "test-models"), ("POST", "models/sync")):
            status, body = http_request(
                method, f"{provider_url}/{path}",
                payload={} if method == "POST" else None, headers=admin_env["auth"],
            )
            assert status == 200, body
        assert len(received) == 2
        for request in received:
            assert request["path"] == "/custom/inventory?key=endpoint-owned%2Bvalue&region=east"
            assert request["headers"]["authorization"] == "Bearer synthetic&key=part+/%?#"
            assert "x-goog-api-key" not in request["headers"]


@pytest.mark.e2e
@pytest.mark.admin
def test_native_google_model_discovery_encodes_query_credentials(admin_env: dict[str, Any]) -> None:
    with _model_probe_endpoint() as (origin, received):
        provider = _create_probe_provider(
            admin_env, "native-google-models", None,
            vendor="custom", protocol="google-gemini", base_url=origin,
        )
        provider_url = f"{admin_env['admin']}/api/v1/providers/{provider}"
        for method, path in (("GET", "test-models"), ("POST", "models/sync")):
            status, body = http_request(
                method, f"{provider_url}/{path}",
                payload={} if method == "POST" else None, headers=admin_env["auth"],
            )
            assert status == 200, body
        assert len(received) == 2
        for request in received:
            assert request["path"] == "/v1beta/models?key=synthetic%26key%3Dpart%2B%2F%25%3F%23"
            assert "authorization" not in request["headers"]


@pytest.mark.e2e
@pytest.mark.admin
def test_failed_model_sync_keeps_saved_inventory(admin_env: dict[str, Any]) -> None:
    with _model_probe_endpoint() as (origin, received):
        provider = _create_probe_provider(admin_env, "failed-probe-inventory", f"{origin}/models")
        provider_url = f"{admin_env['admin']}/api/v1/providers/{provider}"
        status, body = http_request(
            "POST", f"{provider_url}/models/sync", payload={}, headers=admin_env["auth"],
        )
        assert status == 200, body
        status, body = http_request(
            "PUT", provider_url, payload={"models_source": f"{origin}/failure"}, headers=admin_env["auth"],
        )
        assert status == 200, body
        status, body = http_request(
            "POST", f"{provider_url}/models/sync", payload={}, headers=admin_env["auth"],
        )
        assert status >= 400, body
        assert received[-1]["path"] == "/failure"
        status, body = http_request("GET", f"{provider_url}/models", headers=admin_env["auth"])
        assert status == 200, body
        assert [model["id"] for model in body["data"]["models"]] == ["probe-model"]


@pytest.mark.e2e
@pytest.mark.admin
@pytest.mark.parametrize(
    ("argument", "value"),
    [
        ("--mode", "removed-mode"),
        ("--admin-token", "removed-token"),
        ("--storage-backend", "sqlite"),
        ("--postgres-dsn", "postgresql://removed"),
        ("--postgres-max-connections", "5"),
        ("--postgres-min-connections", "1"),
        ("--postgres-idle-timeout", "60"),
        ("--migrate-only", None),
        ("--migrate-on-start", "false"),
        ("--webui-dir", "removed"),
        ("--proxy-host", "127.0.0.1"),
        ("--proxy-port", "19530"),
        ("--admin-host", "127.0.0.1"),
        ("--admin-port", "19531"),
    ],
)
def test_server_rejects_removed_options(
    stravia_binary: Path, argument: str, value: str | None
) -> None:
    command = [str(stravia_binary), argument]
    if value is not None:
        command.append(value)
    command.append("--help")

    result = subprocess.run(
        command,
        capture_output=True,
        text=True,
        check=False,
    )

    assert result.returncode != 0
    assert f"unexpected argument '{argument}'" in result.stderr


def _create_provider(env: dict[str, str], name: str) -> str:
    status, resp = http_request(
        "POST",
        f"{env['admin']}/api/v1/providers",
        payload={
            "name": name,
            "source": {
                "type": "custom",
                "vendor": "custom",
                "channel": "default",
                "protocol": "openai-compatible",
                "base_url": env["mock"],
            },
            "credential": {"type": "api_key", "value": "dummy-key"},
            "vendor_options": {},
        },
        headers=env["auth"],
    )
    assert status == 200, f"create provider failed: {status} {resp}"
    provider_id = resp["data"]["id"]
    status, resp = http_request(
        "POST",
        f"{env['admin']}/api/v1/providers/{provider_id}/models",
        payload={
            "model_id": "gpt-4o-mini",
            "metadata": {"id": "gpt-4o-mini", "name": "GPT-4o mini"},
        },
        headers=env["auth"],
    )
    assert status == 201, f"create provider model failed: {status} {resp}"
    return provider_id


def _create_model(
    env: dict[str, str],
    provider_id: str,
    model_id: str,
    display_name: str | None = None,
    *,
    target_model: str = "gpt-4o-mini",
) -> str:
    payload: dict[str, Any] = {
        "model_id": model_id,
        "targets": [{"provider_id": provider_id, "model": target_model}],
    }
    if display_name is not None:
        payload["display_name"] = display_name
    status, resp = http_request(
        "POST",
        f"{env['admin']}/api/v1/models",
        payload=payload,
        headers=env["auth"],
    )
    assert status == 200, f"create model failed: {status} {resp}"
    return resp["data"]["id"]


def _create_api_key(env: dict[str, str], model_id: str, name: str) -> dict[str, Any]:
    status, resp = http_request(
        "POST",
        f"{env['admin']}/api/v1/api-keys",
        payload={"name": name, "model_ids": [model_id]},
        headers=env["auth"],
    )
    assert status == 200, f"create api-key failed: {status} {resp}"
    return resp["data"]

@pytest.mark.e2e
@pytest.mark.admin
def test_rpm_burst_rejection_and_shared_destination_limit(admin_env: dict[str, Any]) -> None:
    with _model_probe_endpoint() as (origin, received):
        provider = _create_probe_provider(
            admin_env, "RPM admission", None, base_url=origin, use_proxy=False,
        )
        status, body = http_request(
            "POST", f"{admin_env['admin']}/api/v1/providers/{provider}/models",
            payload={"model_id": "probe-model", "metadata": {"id": "probe-model"}},
            headers=admin_env["auth"],
        )
        assert status == 201, body
        route = _create_model(admin_env, provider, "rpm-http", target_model="probe-model")
        key = _create_api_key(admin_env, route, "RPM burst")
        other = _create_api_key(admin_env, route, "RPM independent")
        key_url = f"{admin_env['admin']}/api/v1/api-keys/{key['id']}"
        status, body = http_request("PUT", key_url, {"rpm_limit": 2}, admin_env["auth"])
        assert status == 200 and body["data"]["rpm_limit"] == 2, body

        def infer(token: str) -> tuple[int, dict[str, str], bytes]:
            return http_bytes(
                "POST", f"{admin_env['proxy']}/v1/chat/completions",
                {"model": "rpm-http", "stream": False, "messages": [{"role": "user", "content": "local RPM probe"}]},
                {"authorization": f"Bearer {token}"},
            )

        with ThreadPoolExecutor(max_workers=3) as executor:
            results = list(executor.map(infer, [key["key"]] * 3))
        assert sorted(result[0] for result in results) == [200, 200, 429], results
        rejection = next(result for result in results if result[0] == 429)
        assert 1 <= int(rejection[1]["retry-after"]) <= 60, rejection
        assert json.loads(rejection[2])["error"]["type"] == "STRAVIA_RPM_LIMIT", rejection
        assert len(received) == 2, received
        assert infer(key["key"])[0] == 429
        assert len(received) == 2, "rejected entrance reached upstream"
        assert infer(other["key"])[0] == 200
        assert len(received) == 3

        # 新启用 Provider/model 目的地限额只记录启用后的发送，与 Principal 根请求限额独立。
        status, body = http_request("PUT", key_url, {"rpm_limit": None}, admin_env["auth"])
        assert status == 200 and body["data"]["rpm_limit"] is None, body
        config_url = f"{admin_env['admin']}/api/v1/settings/rpm_admission"
        status, previous = http_request("GET", config_url, headers=admin_env["auth"])
        assert status == 200, previous
        config = {
            "preferred_wait_ms": 0, "total_wait_ms": 0, "queue_capacity": 128,
            "destinations": [{"provider_id": provider, "model": "probe-model", "rpm_limit": 1}],
        }
        try:
            status, body = http_request("PUT", config_url, {"value": json.dumps(config)}, admin_env["auth"])
            assert status == 200 and body == {"ok": True}, body
            status, body = http_request("GET", config_url, headers=admin_env["auth"])
            assert status == 200 and json.loads(body["data"]) == config, body
            assert infer(key["key"])[0] == 200
            limited = infer(other["key"])
            assert limited[0] == 429, limited
            assert 1 <= int(limited[1]["retry-after"]) <= 60, limited
            assert json.loads(limited[2])["error"]["code"] == "target_rpm_exceeded", limited
            assert len(received) == 4, "Destination RPM rejection reached upstream"
            # 重载同一限额不能返还目的地已发送的窗口额度。
            status, body = http_request("PUT", config_url, {"value": json.dumps(config)}, admin_env["auth"])
            assert status == 200 and body == {"ok": True}, body
            limited = infer(key["key"])
            assert limited[0] == 429, limited
            assert 1 <= int(limited[1]["retry-after"]) <= 60, limited
            assert json.loads(limited[2])["error"]["code"] == "target_rpm_exceeded", limited
            assert len(received) == 4, "reloaded destination limit returned spent allowance"
        finally:
            status, body = http_request("PUT", config_url, {"value": previous["data"]}, admin_env["auth"])
            assert status == 200 and body == {"ok": True}, body
        status, body = http_request("GET", config_url, headers=admin_env["auth"])
        assert status == 200 and json.loads(body["data"]) == json.loads(previous["data"]), body
        assert infer(other["key"])[0] == 200
        assert len(received) == 5, "restored destination configuration did not resume upstream sends"


@pytest.mark.e2e
@pytest.mark.admin
def test_admin_anon_returns_401(admin_env: dict[str, str]) -> None:
    status, _ = http_request("GET", f"{admin_env['admin']}/api/v1/status")
    assert status == 401


@pytest.mark.e2e
@pytest.mark.admin
def test_embedded_webui_is_served(admin_env: dict[str, str]) -> None:
    status, body = http_request("GET", admin_env["admin"])
    assert status == 200
    assert isinstance(body, str)
    assert "<!doctype html>" in body.lower()


@pytest.mark.e2e
@pytest.mark.admin
@pytest.mark.parametrize("path", ["/providers", "/unknown-page", "/apiary", "/v10/unknown"])
def test_webui_routes_fall_back_to_the_embedded_app(
    admin_env: dict[str, str], path: str
) -> None:
    status, body = http_request("GET", f"{admin_env['admin']}{path}")
    assert status == 200
    assert isinstance(body, str)
    assert "<!doctype html>" in body.lower()


@pytest.mark.e2e
@pytest.mark.admin
def test_embedded_static_assets_take_priority_over_webui_fallback(
    admin_env: dict[str, str],
) -> None:
    with urlopen(f"{admin_env['admin']}/stravia-logo.svg") as response:
        assert response.status == 200
        assert response.headers.get_content_type() == "image/svg+xml"
        assert ElementTree.fromstring(response.read()).tag == "{http://www.w3.org/2000/svg}svg"


@pytest.mark.e2e
@pytest.mark.admin
@pytest.mark.parametrize(
    "path",
    ["/api", "/api/unknown", "/api/v1/unknown", "/v1/unknown", "/v1beta/unknown"],
)
def test_reserved_api_namespaces_never_fall_back_to_webui(
    admin_env: dict[str, str], path: str
) -> None:
    status, body = http_request("GET", f"{admin_env['admin']}{path}")
    assert status == 404
    assert not (isinstance(body, str) and "<!doctype html>" in body.lower())


@pytest.mark.e2e
@pytest.mark.admin
def test_health_probes_are_distinct_from_webui_routes(admin_env: dict[str, str]) -> None:
    for path in ("/healthz", "/readyz"):
        status, body = http_request("GET", f"{admin_env['admin']}{path}")
        assert status == 200
        assert body == {"status": "ok"}

    status, body = http_request("GET", f"{admin_env['admin']}/health")
    assert status == 200
    assert isinstance(body, str)
    assert "<!doctype html>" in body.lower()


@pytest.mark.e2e
@pytest.mark.admin
def test_setup_mode_is_live_but_not_ready(
    stravia_binary: Path,
) -> None:
    with tempfile.TemporaryDirectory(prefix="stravia-readyz-e2e-") as data_dir:
        port = find_free_port()
        proc, logs = start_stravia_server(
            stravia_binary=stravia_binary,
            args=[
                "--host",
                "127.0.0.1",
                "--port",
                str(port),
                "--data-dir",
                data_dir,
            ],
        )
        base = f"http://127.0.0.1:{port}"

        try:
            wait_until_ready(f"{base}/healthz")
            status, body = http_request("GET", f"{base}/healthz")
            assert status == 200
            assert body == {"status": "ok"}
            status, _ = http_request("GET", f"{base}/readyz")
            assert status == 503
            status, state = http_request("GET", f"{base}/api/v1/auth/state")
            assert status == 200
            assert state["mode"] == "setup"
            assert not (Path(data_dir) / "db" / "gateway.db").exists()
        finally:
            stop_stravia_server(proc, logs)


@pytest.mark.e2e
@pytest.mark.admin
def test_status_describes_gateway_without_a_listener_port(admin_env: dict[str, str]) -> None:
    status, body = http_request(
        "GET", f"{admin_env['admin']}/api/v1/status", headers=admin_env["auth"]
    )
    assert status == 200
    assert body["status"] == "running"
    assert isinstance(body["version"], str)
    assert body["version"]
    assert "listener_port" not in body


@pytest.mark.e2e
@pytest.mark.admin
def test_unified_listener_preserves_proxy_cors_without_cross_origin_management(
    admin_env: dict[str, str],
) -> None:
    origin = admin_env["proxy_cors_origin"]
    for path in ("/api/v1/providers", "/v1/chat/completions"):
        status, headers, _ = http_bytes(
            "OPTIONS",
            f"{admin_env['admin']}{path}",
            headers={
                "Origin": origin,
                "Access-Control-Request-Method": "POST",
            },
        )
        allowed_origin = headers.get("access-control-allow-origin")

        if path.startswith("/api/"):
            assert status == 403
            assert allowed_origin is None
        else:
            assert status == 200
            assert allowed_origin == origin


@pytest.mark.e2e
@pytest.mark.admin
def test_proxy_rejects_untrusted_cors_origin_and_method(
    admin_env: dict[str, str],
) -> None:
    def preflight(origin: str, method: str) -> tuple[int, str | None, str | None]:
        status, headers, _ = http_bytes(
            "OPTIONS",
            f"{admin_env['admin']}/v1/chat/completions",
            headers={
                "Origin": origin,
                "Access-Control-Request-Method": method,
            },
        )
        return (
            status,
            headers.get("access-control-allow-origin"),
            headers.get("access-control-allow-methods"),
        )

    # tower-http rejects an origin by omitting Access-Control-Allow-Origin. The
    # network response can still be 200; browsers enforce the denial.
    status, allowed_origin, _ = preflight("https://untrusted.example", "POST")
    assert status == 200
    assert allowed_origin is None

    status, allowed_origin, allowed_methods = preflight(admin_env["proxy_cors_origin"], "PATCH")
    assert status == 200
    assert allowed_origin == admin_env["proxy_cors_origin"]
    assert allowed_methods is not None
    assert "PATCH" not in {method.strip() for method in allowed_methods.split(",")}


@pytest.mark.e2e
@pytest.mark.admin
def test_provider_crud(admin_env: dict[str, str]) -> None:
    provider_id = _create_provider(admin_env, "test-provider")

    status, resp = http_request("GET", f"{admin_env['admin']}/api/v1/providers", headers=admin_env["auth"])
    assert status == 200
    ids = [item["id"] for item in resp["data"]]
    assert provider_id in ids

    status, resp = http_request(
        "GET",
        f"{admin_env['admin']}/api/v1/providers/{provider_id}",
        headers=admin_env["auth"],
    )
    assert status == 200
    assert resp["data"]["id"] == provider_id


@pytest.mark.e2e
@pytest.mark.admin
def test_provider_model_specification_preserves_saved_metadata(admin_env: dict[str, str]) -> None:
    provider_id = _create_provider(admin_env, "test-provider-model-specification")
    model_id = "saved-specification-model"
    retained_prices = {"input": 0.125, "output": 2.5, "cache_read": 0.0625, "cache_write": 0.25}
    obsolete_prices = {"reasoning": 7, "input_audio": 8, "output_audio": 9}
    tier_threshold = {"type": "context", "size": 300_000}
    expected_cost = {
        **retained_prices,
        "context_over_200k": retained_prices,
        "tiers": [{"tier": tier_threshold, **retained_prices}],
    }
    metadata = {
        "id": model_id,
        "name": "Saved specification model",
        "reasoning_efforts": ["none", "low", "high", "custom-effort"],
        "modalities": {
            "input": ["text", "image", "pdf"],
            "output": ["text", "audio", "custom-output"],
        },
        "limit": {
            "context": 1_050_000,
        },
        "cost": {
            **retained_prices,
            **obsolete_prices,
            "context_over_200k": {**retained_prices, **obsolete_prices},
            "tiers": [{"tier": tier_threshold, **retained_prices, **obsolete_prices}],
        },
    }

    status, created = http_request(
        "POST",
        f"{admin_env['admin']}/api/v1/providers/{provider_id}/models",
        payload={"model_id": model_id, "metadata": metadata},
        headers=admin_env["auth"],
    )
    assert status == 201, f"create provider model failed: {status} {created}"

    expected_specification = {
        "limit": {
            "context": 1_050_000,
        },
        "modalities": {
            "input": ["text", "image", "pdf"],
            "output": ["text", "audio", "custom-output"],
        },
        "reasoning_efforts": ["none", "low", "high", "custom-effort"],
    }

    status, listed = http_request(
        "GET",
        f"{admin_env['admin']}/api/v1/providers/{provider_id}/models",
        headers=admin_env["auth"],
    )
    assert status == 200
    summary = next(
        model for model in listed["data"]["models"] if model["id"] == model_id
    )
    assert summary["specification"] == expected_specification
    assert "capabilities" not in summary
    unknown_summary = next(
        model for model in listed["data"]["models"] if model["id"] == "gpt-4o-mini"
    )
    assert unknown_summary["specification"] == {
        "limit": None,
        "modalities": {"input": ["text"], "output": ["text"]},
        "reasoning_efforts": None,
    }

    status, detail = http_request(
        "GET",
        f"{admin_env['admin']}/api/v1/providers/{provider_id}/model?model={model_id}",
        headers=admin_env["auth"],
    )
    assert status == 200
    saved = detail["data"]
    assert saved["metadata"]["cost"] == expected_cost
    assert {
        key: saved["metadata"][key] for key in expected_specification
    } == expected_specification

    revised_metadata = {
        **metadata,
        "reasoning_efforts": ["minimal", "medium", "max"],
        "limit": {
            "context": 1_048_576,
        },
    }
    status, updated = http_request(
        "PUT",
        f"{admin_env['admin']}/api/v1/providers/{provider_id}/model",
        payload={
            "model_id": model_id,
            "metadata": revised_metadata,
            "revision": saved["revision"],
        },
        headers=admin_env["auth"],
    )
    assert status == 200, f"update provider model failed: {status} {updated}"
    revised_specification = {
        **expected_specification,
        "limit": revised_metadata["limit"],
        "reasoning_efforts": revised_metadata["reasoning_efforts"],
    }
    assert updated["data"]["revision"] > saved["revision"]
    assert updated["data"]["metadata"]["cost"] == expected_cost
    assert {
        key: updated["data"]["metadata"][key] for key in revised_specification
    } == revised_specification

    status, listed = http_request(
        "GET",
        f"{admin_env['admin']}/api/v1/providers/{provider_id}/models",
        headers=admin_env["auth"],
    )
    assert status == 200
    summary = next(
        model for model in listed["data"]["models"] if model["id"] == model_id
    )
    assert summary["revision"] == updated["data"]["revision"]
    assert summary["specification"] == revised_specification


@pytest.mark.e2e
@pytest.mark.admin
def test_model_crud(admin_env: dict[str, str]) -> None:
    provider_id = _create_provider(admin_env, "test-provider-model")
    route_storage_id = _create_model(admin_env, provider_id, "test-model", "Test model")
    unnamed_storage_id = _create_model(admin_env, provider_id, "test-model-unnamed")

    status, resp = http_request("GET", f"{admin_env['admin']}/api/v1/models", headers=admin_env["auth"])
    assert status == 200
    route = next(item for item in resp.get("data", []) if item["id"] == route_storage_id)
    assert route["model_id"] == "test-model"
    assert route["display_name"] == "Test model"
    assert "name" not in route
    unnamed_route = next(item for item in resp.get("data", []) if item["id"] == unnamed_storage_id)
    assert unnamed_route["model_id"] == "test-model-unnamed"
    assert unnamed_route["display_name"] is None

    status, resp = http_request(
        "GET",
        f"{admin_env['admin']}/api/v1/models/test-model",
        headers=admin_env["auth"],
    )
    assert status == 200
    assert resp["data"]["model_id"] == "test-model"

    status, resp = http_request(
        "PUT",
        f"{admin_env['admin']}/api/v1/models/test-model",
        payload={"model_id": "test-model", "display_name": "   "},
        headers=admin_env["auth"],
    )
    assert status == 200
    assert resp["data"]["display_name"] is None
    assert resp["data"]["model_id"] == "test-model"

    duplicate_id = _create_model(admin_env, provider_id, "test-model-duplicate", "Test model")
    assert duplicate_id != route_storage_id

    status, _ = http_request(
        "POST",
        f"{admin_env['admin']}/api/v1/models",
        payload={
            "name": "legacy-model",
            "targets": [{"provider_id": provider_id, "model": "gpt-4o-mini"}],
        },
        headers=admin_env["auth"],
    )
    assert status >= 400


@pytest.mark.e2e
@pytest.mark.admin
def test_api_key_crud(admin_env: dict[str, str]) -> None:
    provider_id = _create_provider(admin_env, "test-provider-key")
    model_id = _create_model(admin_env, provider_id, "test-model-key")
    api_key = _create_api_key(admin_env, model_id, "test-key")
    assert api_key.get("key"), f"missing api key material: {api_key}"

    status, _ = http_request(
        "GET",
        f"{admin_env['admin']}/api/v1/providers",
        headers={"authorization": f"Bearer {api_key['key']}"},
    )
    assert status == 401
    status, _ = http_request(
        "POST",
        f"{admin_env['proxy']}/v1/chat/completions",
        payload={"model": "test-model-key", "messages": [{"role": "user", "content": "hi"}]},
        headers=admin_env["auth"],
    )
    assert status == 401


@pytest.mark.e2e
@pytest.mark.admin
def test_access_control_rejects_anonymous(admin_env: dict[str, str]) -> None:
    provider_id = _create_provider(admin_env, "test-provider-access")
    _create_model(admin_env, provider_id, "test-model-access")

    status, _ = http_request(
        "POST",
        f"{admin_env['proxy']}/v1/chat/completions",
        payload={"model": "test-model-access", "messages": [{"role": "user", "content": "hi"}]},
    )
    assert status == 401


@pytest.mark.e2e
@pytest.mark.admin
def test_proxy_request_updates_usage_analytics(admin_env: dict[str, str]) -> None:
    provider_id = _create_provider(admin_env, "test-provider-log")
    model_id = _create_model(admin_env, provider_id, "test-model-log")
    api_key = _create_api_key(admin_env, model_id, "test-key-log")

    status, resp = http_request(
        "POST",
        f"{admin_env['proxy']}/v1/chat/completions",
        payload={
            "model": "test-model-log",
            "messages": [{"role": "user", "content": "log-trigger"}],
        },
        headers={"authorization": f"Bearer {api_key['key']}"},
    )
    assert status == 200, f"proxy request failed: {status} {resp}"

    attributed_usage: dict[str, Any] | None = None
    deadline = time.time() + 10.0
    while time.time() < deadline:
        status, stats_resp = http_request(
            "GET",
            f"{admin_env['admin']}/api/v1/stats/api-keys",
            headers=admin_env["auth"],
        )
        if status == 200:
            attributed_usage = next(
                (
                    item
                    for item in stats_resp.get("data", [])
                    if item.get("api_key_id") == api_key["id"]
                ),
                None,
            )
            if (
                attributed_usage is not None
                and attributed_usage.get("request_count")
                and attributed_usage.get("total_output_tokens") is not None
            ):
                break
        time.sleep(0.3)

    assert attributed_usage is not None
    assert attributed_usage["api_key_name"] == "test-key-log"
    assert attributed_usage["request_count"] >= 1
    # 未报告缓存读取时无法计算净输入；不能把原始总输入当成管理输入。
    assert attributed_usage["total_input_tokens"] is None
    assert attributed_usage["total_output_tokens"] is not None
    assert attributed_usage["total_output_tokens"] >= 2
    assert attributed_usage["cache_read_tokens"] is None
    assert attributed_usage["cache_write_tokens"] is None


@pytest.mark.e2e
@pytest.mark.admin
def test_stats_overview_incremented(admin_env: dict[str, str]) -> None:
    provider_id = _create_provider(admin_env, "test-provider-stats")
    model_id = _create_model(admin_env, provider_id, "test-model-stats")
    api_key = _create_api_key(admin_env, model_id, "test-key-stats")

    status, _ = http_request(
        "POST",
        f"{admin_env['proxy']}/v1/chat/completions",
        payload={
            "model": "test-model-stats",
            "messages": [{"role": "user", "content": "stats-trigger"}],
        },
        headers={"authorization": f"Bearer {api_key['key']}"},
    )
    assert status == 200

    # Request admission is persisted before usage and completion observations.
    data: dict[str, Any] = {}
    deadline = time.time() + 10.0
    while time.time() < deadline:
        status, resp = http_request(
            "GET",
            f"{admin_env['admin']}/api/v1/stats/overview",
            headers=admin_env["auth"],
        )
        assert status == 200, resp
        data = resp["data"]
        if all(data.get(field) is not None for field in (
            "total_output_tokens", "avg_duration_ms",
        )):
            break
        time.sleep(0.3)

    assert data["total_input_tokens"] is None, data
    assert data.get("total_output_tokens") is not None, data
    assert data.get("avg_duration_ms") is not None, data
    assert data.get("total_requests", 0) >= 1
    assert data.get("total_output_tokens", 0) >= 2
    assert data["total_cache_read_tokens"] is None
    assert data["total_cache_write_tokens"] is None
    assert data["avg_duration_ms"] >= 0

    status, resp = http_request(
        "GET",
        f"{admin_env['admin']}/api/v1/stats/series?hours=24&bucket=3600",
        headers=admin_env["auth"],
    )
    assert status == 200
    series = resp.get("data", [])
    assert series
    assert series[-1]["total_cache_read_tokens"] is None
    assert series[-1]["total_cache_write_tokens"] is None
    assert series[-1]["avg_duration_ms"] >= 0
