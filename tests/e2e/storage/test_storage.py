from __future__ import annotations

import tempfile
from pathlib import Path
from typing import Callable

import pytest

from tests.common.helpers import (
    WebSession,
    find_free_port,
    http_request,
    initialize_server,
    start_stravia_server,
    stop_stravia_server,
    wait_for_setup_token,
    wait_until_ready,
)


@pytest.mark.e2e
@pytest.mark.storage
@pytest.mark.parametrize("backend", ["sqlite", "postgres"], ids=["sqlite", "postgres"])
def test_storage_backend_equivalence(storage_runtime: dict[str, object], backend: str) -> None:
    pg_url = storage_runtime["pg_url"]

    if backend == "postgres" and not pg_url:
        pytest.skip("postgres backend requires DB_URL")

    run_harness: Callable[..., str] = storage_runtime["run_harness"]  # type: ignore[assignment]
    output = run_harness(
        backend,
        upstream_port=storage_runtime["upstream_port"],
        work_dir=storage_runtime["work_dir"],
        pg_url=pg_url,
    )

    assert f"backend={backend}" in output
    assert "logs_total=" in output
    assert "stats_total_requests=" in output
    assert "proxy_status_ok=200" in output
    assert "proxy_status_no_key=401" in output


@pytest.mark.e2e
@pytest.mark.storage
def test_server_boots_postgres_with_migrations(
    stravia_binary: Path, storage_runtime: dict[str, object]
) -> None:
    pg_url = storage_runtime["pg_url"]
    if not isinstance(pg_url, str) or not pg_url:
        pytest.skip("postgres server migration requires DB_URL")

    server_port = find_free_port()
    work_dir = storage_runtime["work_dir"]
    assert isinstance(work_dir, Path)
    make_schema: Callable[..., str] = storage_runtime["make_isolated_schema"]  # type: ignore[assignment]
    run_schema_action: Callable[..., None] = storage_runtime["run_schema_action"]  # type: ignore[assignment]
    postgres_dsn_for_schema: Callable[[str, str], str] = storage_runtime[
        "postgres_dsn_for_schema"
    ]  # type: ignore[assignment]
    schema = make_schema("stravia_server_e2e")
    run_schema_action("create", work_dir=work_dir, pg_url=pg_url, schema=schema)

    try:
        postgres_dsn = postgres_dsn_for_schema(pg_url, schema)
        with tempfile.TemporaryDirectory(prefix="stravia-postgres-server-e2e-") as data_dir:
            proc, logs = start_stravia_server(
                stravia_binary=stravia_binary,
                args=[
                    "--data-dir",
                    data_dir,
                    "--host",
                    "127.0.0.1",
                    "--port",
                    str(server_port),
                ],
            )
            admin_base = f"http://127.0.0.1:{server_port}"
            try:
                wait_until_ready(f"{admin_base}/api/v1/auth/state", timeout=30.0)
                setup_token = wait_for_setup_token(logs, proc)
                session = initialize_server(
                    admin_base,
                    setup_token,
                    {"backend": "postgres", "url": postgres_dsn},
                )
                headers = session.auth_headers()
                status, body = http_request(
                    "GET", f"{admin_base}/api/v1/status", headers=headers
                )
                assert status == 200, f"PostgreSQL server status failed: {body}"

                upstream_port = storage_runtime["upstream_port"]
                assert isinstance(upstream_port, int)
                status, body = http_request(
                    "POST",
                    f"{admin_base}/api/v1/providers",
                    payload={
                        "name": "postgres-server-e2e-provider",
                        "source": {
                            "type": "custom",
                            "vendor": "custom",
                            "protocol": "openai",
                            "base_url": f"http://127.0.0.1:{upstream_port}/v1",
                        },
                        "credential": {
                            "type": "api_key",
                            "value": "dummy-key",
                        },
                    },
                    headers=headers,
                )
                assert status == 200, f"create PostgreSQL provider failed: {body}"
                provider_id = body["data"]["id"]

                status, body = http_request(
                    "POST",
                    f"{admin_base}/api/v1/providers/{provider_id}/models",
                    payload={
                        "model_id": "gpt-4o-mini",
                        "metadata": {
                            "id": "gpt-4o-mini",
                            "name": "GPT-4o mini",
                        },
                    },
                    headers=headers,
                )
                assert status == 201, f"create PostgreSQL provider model failed: {body}"

                status, body = http_request(
                    "POST",
                    f"{admin_base}/api/v1/models",
                    payload={
                        "model_id": "postgres-server-e2e-model",
                        "display_name": "PostgreSQL server E2E model",
                        "target_provider": provider_id,
                        "target_model": "gpt-4o-mini",
                    },
                    headers=headers,
                )
                assert status == 200, f"create PostgreSQL model failed: {body}"
                model_id = body["data"]["id"]

                status, body = http_request(
                    "POST",
                    f"{admin_base}/api/v1/api-keys",
                    payload={
                        "name": "postgres-server-e2e-key",
                        "model_ids": [model_id],
                    },
                    headers=headers,
                )
                assert status == 200, f"create PostgreSQL API key failed: {body}"
                api_key = body["data"]["key"]

                status, body = http_request(
                    "POST",
                    f"{admin_base}/v1/chat/completions",
                    payload={
                        "model": "postgres-server-e2e-model",
                        "messages": [{"role": "user", "content": "hello"}],
                    },
                    headers={"authorization": f"Bearer {api_key}"},
                )
                assert status == 200, f"PostgreSQL proxy request failed: {body}"
                assert body["choices"][0]["message"]["content"] == "ok"
            finally:
                stop_stravia_server(proc, logs)

        reconnect_port = find_free_port()
        reconnect_base = f"http://127.0.0.1:{reconnect_port}"
        with tempfile.TemporaryDirectory(prefix="stravia-postgres-reconnect-e2e-") as reconnect_dir:
            reconnect_proc, reconnect_logs = start_stravia_server(
                stravia_binary=stravia_binary,
                args=[
                    "--data-dir",
                    reconnect_dir,
                    "--host",
                    "127.0.0.1",
                    "--port",
                    str(reconnect_port),
                ],
            )
            try:
                wait_until_ready(f"{reconnect_base}/api/v1/auth/state", timeout=30.0)
                operator = WebSession(reconnect_base)
                status, body = operator.request(
                    "POST",
                    "/api/v1/setup/claim",
                    {"token": wait_for_setup_token(reconnect_logs, reconnect_proc)},
                )
                assert status == 204, body
                status, body = operator.request(
                    "POST",
                    "/api/v1/setup/complete",
                    {
                        "database": {"backend": "postgres", "url": postgres_dsn},
                        "username": "must-not-replace-owner",
                        "password": "must not replace horse battery staple",
                    },
                    timeout=40.0,
                )
                assert status == 200, body

                status, _ = operator.request(
                    "POST",
                    "/api/v1/auth/login",
                    {
                        "username": "must-not-replace-owner",
                        "password": "must not replace horse battery staple",
                    },
                )
                assert status == 401
                owner = WebSession(reconnect_base)
                status, body = owner.request(
                    "POST",
                    "/api/v1/auth/login",
                    {"username": "admin", "password": "correct horse battery staple"},
                )
                assert status == 200, body
                status, body = owner.request("GET", "/api/v1/providers")
                assert status == 200, body
                assert provider_id in {provider["id"] for provider in body["data"]}

                status, body = http_request(
                    "POST",
                    f"{reconnect_base}/v1/chat/completions",
                    payload={
                        "model": "postgres-server-e2e-model",
                        "messages": [{"role": "user", "content": "hello again"}],
                    },
                    headers={"authorization": f"Bearer {api_key}"},
                )
                assert status == 200, body
            finally:
                stop_stravia_server(reconnect_proc, reconnect_logs)
    finally:
        run_schema_action("drop", work_dir=work_dir, pg_url=pg_url, schema=schema)
