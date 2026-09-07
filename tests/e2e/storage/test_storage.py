from __future__ import annotations

import hashlib
import sqlite3
import tempfile
import time
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
    assert "observation_roots=" in output
    assert "stats_total_requests=" in output
    assert "proxy_status_ok=200" in output
    assert "proxy_status_no_key=401" in output


def _prepare_legacy_sqlite(database: Path, migrations: Path) -> None:
    connection = sqlite3.connect(database)
    try:
        connection.execute(
            """
            CREATE TABLE _sqlx_migrations (
                version BIGINT PRIMARY KEY,
                description TEXT NOT NULL,
                installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
                success BOOLEAN NOT NULL,
                checksum BLOB NOT NULL,
                execution_time BIGINT NOT NULL
            )
            """
        )
        for migration in sorted(migrations.glob("*.sql")):
            version = int(migration.name.split("_", 1)[0])
            if version >= 34:
                continue
            sql = migration.read_text(encoding="utf-8")
            connection.executescript(sql)
            description = migration.stem.split("_", 1)[1].replace("_", " ")
            connection.execute(
                "INSERT INTO _sqlx_migrations "
                "(version, description, success, checksum, execution_time) "
                "VALUES (?, ?, 1, ?, 0)",
                (version, description, hashlib.sha384(sql.encode()).digest()),
            )
        connection.execute(
            "INSERT INTO request_logs (id, created_at, client_request_body) VALUES (?, ?, ?)",
            ("legacy-log-must-not-survive", 1, '{"secret":"legacy"}'),
        )
        connection.commit()
    finally:
        connection.close()


@pytest.mark.e2e
@pytest.mark.storage
def test_sqlite_upgrade_removes_legacy_logs_and_installs_observation_schema(
    stravia_binary: Path, repo_root: Path, tmp_path: Path
) -> None:
    database = tmp_path / "gateway.db"
    _prepare_legacy_sqlite(
        database, repo_root / "backend" / "crates" / "stravia-core" / "migrations" / "sqlite"
    )
    orphan = tmp_path / "observation-debug" / "00000000000040008000000000000001"
    orphan.mkdir(parents=True)
    (orphan / "segment-000001.jsonl").write_text('{"orphan":true}\n', encoding="utf-8")
    server_port = find_free_port()
    proc, logs = start_stravia_server(
        stravia_binary=stravia_binary,
        args=["--data-dir", str(tmp_path), "--host", "127.0.0.1", "--port", str(server_port)],
    )
    base = f"http://127.0.0.1:{server_port}"
    try:
        wait_until_ready(f"{base}/api/v1/auth/state", timeout=30.0)
        session = initialize_server(
            base,
            wait_for_setup_token(logs, proc),
            {"backend": "sqlite", "path": str(database)},
        )
        status, body = http_request(
            "GET", f"{base}/api/v1/observations/interactions", headers=session.auth_headers()
        )
        assert status == 200, body
        assert body["data"]["root_total"] == 0
        deadline = time.time() + 5.0
        while orphan.exists() and time.time() < deadline:
            time.sleep(0.05)
        assert not orphan.exists()
        status, _ = http_request("GET", f"{base}/api/v1/logs", headers=session.auth_headers())
        assert status == 404

        with sqlite3.connect(database) as connection:
            tables = {
                row[0]
                for row in connection.execute(
                    "SELECT name FROM sqlite_master WHERE type = 'table'"
                )
            }
            assert "request_logs" not in tables
            assert {
                "interaction_observations",
                "inference_run_observations",
                "model_turn_observations",
                "target_attempt_observations",
                "observation_events",
                "rejected_request_observations",
                "debug_trace_manifests",
            } <= tables
            indexes = {
                row[0]
                for row in connection.execute(
                    "SELECT name FROM sqlite_master WHERE type = 'index'"
                )
            }
            assert {
                "interaction_observations_window_idx",
                "model_turns_analytics_idx",
                "target_attempts_analytics_idx",
                "observation_events_expiry_idx",
            } <= indexes
    finally:
        stop_stravia_server(proc, logs)


@pytest.mark.e2e
@pytest.mark.storage
def test_postgres_legacy_upgrade_installs_observation_schema_and_reconnects(
    stravia_binary: Path, storage_runtime: dict[str, object]
) -> None:
    pg_url = storage_runtime["pg_url"]
    if not isinstance(pg_url, str) or not pg_url:
        pytest.skip("postgres server migration requires DB_URL")

    server_port = find_free_port()
    work_dir = storage_runtime["work_dir"]
    assert isinstance(work_dir, Path)
    make_schema: Callable[..., str] = storage_runtime["make_isolated_schema"]  # type: ignore[assignment]
    run_schema_action: Callable[..., str] = storage_runtime["run_schema_action"]  # type: ignore[assignment]
    postgres_dsn_for_schema: Callable[[str, str], str] = storage_runtime[
        "postgres_dsn_for_schema"
    ]  # type: ignore[assignment]
    schema = make_schema("stravia_server_e2e")
    run_schema_action("create", work_dir=work_dir, pg_url=pg_url, schema=schema)
    run_schema_action("prepare_legacy", work_dir=work_dir, pg_url=pg_url, schema=schema)

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

                deadline = time.time() + 10.0
                roots = 0
                while time.time() < deadline:
                    status, body = http_request(
                        "GET",
                        f"{admin_base}/api/v1/observations/interactions?limit=10",
                        headers=headers,
                    )
                    assert status == 200, body
                    roots = int(body["data"]["root_total"])
                    if roots:
                        break
                    time.sleep(0.1)
                assert roots == 1
                status, _ = http_request(
                    "GET", f"{admin_base}/api/v1/logs", headers=headers
                )
                assert status == 404
            finally:
                stop_stravia_server(proc, logs)

        schema_report = run_schema_action(
            "inspect_observation", work_dir=work_dir, pg_url=pg_url, schema=schema
        )
        assert "observation_tables=7" in schema_report
        assert "legacy_tables=0" in schema_report
        assert "observation_indexes=4" in schema_report

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
