from __future__ import annotations

import sqlite3
import subprocess
import tempfile
import time
from urllib.error import HTTPError
from urllib.request import Request, urlopen
from pathlib import Path
from typing import Any

import pytest

from tests.common.helpers import (
    find_free_port,
    http_request,
    start_stravia_server,
    stop_stravia_server,
    wait_until_ready,
)


@pytest.mark.e2e
@pytest.mark.admin
@pytest.mark.parametrize(
    ("argument", "value"),
    [
        ("--mode", "removed-mode"),
        ("--config", "removed.yaml"),
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


@pytest.mark.e2e
@pytest.mark.admin
def test_server_rejects_mysql_storage_backend(stravia_binary: Path) -> None:
    result = subprocess.run(
        [str(stravia_binary), "--storage-backend", "mysql"],
        capture_output=True,
        text=True,
        check=False,
    )

    assert result.returncode != 0
    assert "possible values: sqlite, postgres" in result.stderr


@pytest.mark.e2e
@pytest.mark.admin
def test_server_requires_admin_token_for_non_loopback_binding(
    stravia_binary: Path,
) -> None:
    result = subprocess.run(
        [str(stravia_binary), "--host", "0.0.0.0", "--port", "0"],
        capture_output=True,
        text=True,
        check=False,
    )

    assert result.returncode != 0
    assert "--admin-token is required when --host is not loopback" in result.stderr


def _create_provider(env: dict[str, str], name: str) -> str:
    status, resp = http_request(
        "POST",
        f"{env['admin']}/api/v1/providers",
        payload={
            "name": name,
            "source": {
                "type": "custom",
                "vendor": "custom",
                "protocol": "openai",
                "base_url": env["mock"],
            },
            "credential": {"type": "api_key", "value": "dummy-key"},
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
) -> str:
    payload: dict[str, Any] = {
        "model_id": model_id,
        "target_provider": provider_id,
        "target_model": "gpt-4o-mini",
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
    with urlopen(f"{admin_env['admin']}/stravia-logo.png") as response:
        assert response.status == 200
        assert response.headers.get_content_type() == "image/png"
        assert response.read(8) == b"\x89PNG\r\n\x1a\n"


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
def test_readyz_reports_schema_pending(
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
            wait_until_ready(f"{base}/readyz")

            connection = sqlite3.connect(Path(data_dir) / "gateway.db")
            try:
                connection.execute("DROP TABLE models")
                connection.commit()
            finally:
                connection.close()

            status, body = http_request("GET", f"{base}/readyz")
            assert status == 503
            assert body == {"status": "schema_pending"}
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
def test_unified_listener_allows_admin_and_proxy_cors(
    admin_env: dict[str, str],
) -> None:
    origin = admin_env["admin"]
    for path in ("/api/v1/providers", "/v1/chat/completions"):
        request = Request(
            f"{admin_env['admin']}{path}",
            method="OPTIONS",
            headers={
                "Origin": origin,
                "Access-Control-Request-Method": "POST",
            },
        )
        try:
            with urlopen(request) as response:
                status = response.status
                allowed_origin = response.headers.get("Access-Control-Allow-Origin")
        except HTTPError as error:
            status = error.code
            allowed_origin = error.headers.get("Access-Control-Allow-Origin")

        assert status == 200
        assert allowed_origin == origin


@pytest.mark.e2e
@pytest.mark.admin
@pytest.mark.parametrize("path", ["/api/v1/providers", "/v1/chat/completions"])
def test_unified_listener_rejects_untrusted_cors_origin_and_method(
    admin_env: dict[str, str],
    path: str,
) -> None:
    def preflight(origin: str, method: str) -> tuple[int, str | None, str | None]:
        request = Request(
            f"{admin_env['admin']}{path}",
            method="OPTIONS",
            headers={
                "Origin": origin,
                "Access-Control-Request-Method": method,
            },
        )
        try:
            with urlopen(request) as response:
                return (
                    response.status,
                    response.headers.get("Access-Control-Allow-Origin"),
                    response.headers.get("Access-Control-Allow-Methods"),
                )
        except HTTPError as error:
            return (
                error.code,
                error.headers.get("Access-Control-Allow-Origin"),
                error.headers.get("Access-Control-Allow-Methods"),
            )

    # tower-http rejects an origin by omitting Access-Control-Allow-Origin. The
    # network response can still be 200; browsers enforce the denial.
    status, allowed_origin, _ = preflight("https://untrusted.example", "POST")
    assert status == 200
    assert allowed_origin is None

    status, allowed_origin, allowed_methods = preflight(admin_env["admin"], "PATCH")
    assert status == 200
    assert allowed_origin == admin_env["admin"]
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
            "target_provider": provider_id,
            "target_model": "gpt-4o-mini",
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
def test_proxy_request_creates_log(admin_env: dict[str, str]) -> None:
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

    deadline = time.time() + 10.0
    total = 0
    while time.time() < deadline:
        status, logs_resp = http_request(
            "GET",
            f"{admin_env['admin']}/api/v1/logs?limit=20&offset=0",
            headers=admin_env["auth"],
        )
        if status == 200:
            total = int(logs_resp.get("data", {}).get("total", 0))
            if total >= 1:
                break
        time.sleep(0.3)
    assert total >= 1

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
            if attributed_usage is not None:
                break
        time.sleep(0.3)

    assert attributed_usage is not None
    assert attributed_usage["api_key_name"] == "test-key-log"
    assert attributed_usage["request_count"] >= 1
    assert attributed_usage["total_input_tokens"] >= 3
    assert attributed_usage["total_output_tokens"] >= 2
    assert attributed_usage["cache_read_tokens"] == 0
    assert attributed_usage["cache_write_tokens"] == 0


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

    status, resp = http_request(
        "GET",
        f"{admin_env['admin']}/api/v1/stats/overview",
        headers=admin_env["auth"],
    )
    assert status == 200
    data = resp.get("data", {})
    assert data.get("total_requests", 0) >= 1
    assert data.get("total_input_tokens", 0) >= 3
    assert data.get("total_output_tokens", 0) >= 2
    assert data["total_cache_read_tokens"] == 0
    assert data["total_cache_write_tokens"] == 0
    assert data["avg_duration_ms"] >= 0
    assert data["avg_first_token_ms"] is None

    status, resp = http_request(
        "GET",
        f"{admin_env['admin']}/api/v1/stats/hourly",
        headers=admin_env["auth"],
    )
    assert status == 200
    hourly = resp.get("data", [])
    assert hourly
    assert hourly[-1]["total_cache_read_tokens"] == 0
    assert hourly[-1]["total_cache_write_tokens"] == 0
    assert hourly[-1]["avg_duration_ms"] >= 0
    assert hourly[-1]["avg_first_token_ms"] is None
