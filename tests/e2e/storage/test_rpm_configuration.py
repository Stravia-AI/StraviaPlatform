from __future__ import annotations

import json
import sqlite3
from pathlib import Path

import pytest

from tests.common.helpers import (
    WebSession, find_free_port, http_request, initialize_server,
    minimal_mock_provider, start_stravia_server, stop_stravia_server,
    wait_for_setup_token, wait_until_ready,
)
from tests.e2e.admin.test_observations import _create_route


@pytest.mark.e2e
@pytest.mark.storage
@pytest.mark.parametrize("backend", ["sqlite", "postgres"])
def test_rpm_migrations_preserve_individual_limits(
    storage_runtime: dict[str, object], backend: str,
) -> None:
    if backend == "sqlite":
        directory = Path(__file__).resolve().parents[3] / "backend/crates/stravia-core/migrations/sqlite"
        with sqlite3.connect(":memory:") as connection:
            for migration in sorted(directory.glob("*.sql")):
                if migration.name.startswith("0009"):
                    break
                connection.executescript(migration.read_text(encoding="utf-8"))
            connection.execute("INSERT INTO api_keys (id, token, name, concurrency_limit) VALUES ('old', 'old-token', 'Old key', 5)")
            connection.executescript((directory / "0009_rpm_admission.sql").read_text(encoding="utf-8"))
            assert connection.execute("SELECT rpm_limit FROM api_keys WHERE id = 'old'").fetchone() == (None,)
            assert "concurrency_limit" not in {row[1] for row in connection.execute("PRAGMA table_info(api_keys)")}
            with pytest.raises(sqlite3.IntegrityError):
                connection.execute("UPDATE api_keys SET rpm_limit = 0 WHERE id = 'old'")
            for migration in sorted(directory.glob("*.sql")):
                if "0010" <= migration.name < "0013":
                    connection.executescript(migration.read_text(encoding="utf-8"))
            config = {
                "preferred_wait_ms": 987, "total_wait_ms": 1234, "queue_capacity": 7,
                "pools": [
                    {"id": "limited", "name": "Limited", "rpm_limit": 9},
                    {"id": "unlimited", "name": "Unlimited", "rpm_limit": None},
                ],
                "destinations": [
                    {"provider_id": "p", "model": "a", "rpm_limit": None, "rpm_pool_id": "limited"},
                    {"provider_id": "q", "model": "b", "rpm_limit": None, "rpm_pool_id": "limited"},
                    {"provider_id": "p", "model": "c", "rpm_limit": None, "rpm_pool_id": "unlimited"},
                    {"provider_id": "p", "model": "d", "rpm_limit": 3, "rpm_pool_id": None},
                    {"provider_id": "p", "model": "e", "rpm_limit": 5},
                ],
            }
            connection.execute("INSERT INTO settings (name, value) VALUES ('rpm_admission', ?)", (json.dumps(config),))
            connection.executescript((directory / "0013_remove_shared_rpm_pools.sql").read_text(encoding="utf-8"))
            expected = {key: value for key, value in config.items() if key != "pools"}
            expected["destinations"] = [
                {"provider_id": "p", "model": "a", "rpm_limit": None},
                {"provider_id": "q", "model": "b", "rpm_limit": None},
                {"provider_id": "p", "model": "c", "rpm_limit": None},
                {"provider_id": "p", "model": "d", "rpm_limit": 3},
                {"provider_id": "p", "model": "e", "rpm_limit": 5},
            ]
            assert json.loads(connection.execute("SELECT value FROM settings WHERE name = 'rpm_admission'").fetchone()[0]) == expected
        return
    pg_url = storage_runtime["pg_url"]
    if not pg_url:
        pytest.skip("postgres backend requires DB_URL")
    schema = storage_runtime["make_isolated_schema"]("stravia_rpm_migration")
    action = storage_runtime["run_schema_action"]
    args = {"pg_url": pg_url, "schema": schema}
    action("create", **args)
    try:
        output = action("verify_rpm_migration", **args)
        assert "rpm_migration_clears_old_limit=true" in output
        assert "rpm_migration_preserves_destination_limits=true" in output
    finally:
        action("drop", **args)


@pytest.mark.e2e
@pytest.mark.storage
@pytest.mark.parametrize("backend", ["sqlite", "postgres"])
def test_destination_rpm_limits_and_restart(
    stravia_binary: Path, storage_runtime: dict[str, object], tmp_path: Path, backend: str,
) -> None:
    pg_url = storage_runtime["pg_url"]
    if backend == "postgres" and not pg_url:
        pytest.skip("postgres backend requires DB_URL")
    schema = None
    database = {"backend": "sqlite"}
    if backend == "postgres":
        schema = storage_runtime["make_isolated_schema"]("stravia_rpm")
        storage_runtime["run_schema_action"]("create", pg_url=pg_url, schema=schema)
        database = {"backend": "postgres", "url": storage_runtime["postgres_dsn_for_schema"](pg_url, schema)}
    mock_port = find_free_port()
    mock, _ = minimal_mock_provider(mock_port)
    port = find_free_port()
    base = f"http://127.0.0.1:{port}"
    args = ["--data-dir", str(tmp_path), "--host", "127.0.0.1", "--port", str(port)]
    process = None
    logs = []
    try:
        process, logs = start_stravia_server(stravia_binary=stravia_binary, args=args)
        wait_until_ready(f"{base}/api/v1/auth/state")
        session = initialize_server(base, wait_for_setup_token(logs, process), database)
        env = {"admin": base, "proxy": base, "auth": session.auth_headers(), "mock": f"http://127.0.0.1:{mock_port}"}
        route_id = f"rpm-storage-{backend}"
        _, token = _create_route(env, route_id)

        def request(method: str, path: str, payload=None):
            return http_request(method, f"{base}/api/v1/{path}", payload=payload, headers=env["auth"])

        _, body = request("GET", "api-keys")
        key = next(item for item in body["data"] if item["name"] == f"{route_id}-key")
        assert key["rpm_limit"] is None
        _, body = request("PUT", f"api-keys/{key['id']}", {"rpm_limit": 1})
        assert body["data"]["rpm_limit"] == 1
        _, body = request("PUT", f"api-keys/{key['id']}", {"name": f"{route_id}-renamed"})
        assert body["data"]["rpm_limit"] == 1

        def infer():
            return http_request("POST", f"{base}/v1/chat/completions",
                                {"model": route_id, "messages": [{"role": "user", "content": "RPM storage smoke"}]},
                                {"Authorization": f"Bearer {token}"})

        status, body = request("GET", "settings/rpm_admission")
        assert status == 200, body
        config = json.loads(body["data"])
        assert (config["preferred_wait_ms"], config["total_wait_ms"], config["queue_capacity"]) == (5000, 30000, 128)
        status, body = request("GET", f"models/{route_id}")
        assert status == 200, body
        target = body["data"]["targets"][0]
        destination = {"provider_id": target["provider_id"], "model": target["model"], "rpm_limit": 3}
        config["destinations"] = [destination]
        status, body = request("PUT", "settings/rpm_admission", {"value": json.dumps(config)})
        assert status == 200, body
        target_id = target["id"]
        for invalid in (
            {**config, "destinations": [{**destination, "rpm_limit": 0}]},
            {**config, "destinations": [{**destination, "rpm_limit": -1}]},
            {**config, "destinations": config["destinations"] * 2},
            {**config, "pools": []},
            {**config, "destinations": [{**destination, "rpm_pool_id": "removed"}]},
            {**config, "preferred_wait_ms": 30001},
            {**config, "queue_capacity": 0},
            {**config, "concurrency_limit": 5},
        ):
            assert "error" in request("PUT", "settings/rpm_admission", {"value": json.dumps(invalid)})[1]

        status, body = infer()
        assert status == 200, body
        assert len(mock.captured_requests) == 1
        assert infer()[0] == 429
        assert len(mock.captured_requests) == 1
        stop_stravia_server(process, logs)
        process = None
        process, logs = start_stravia_server(stravia_binary=stravia_binary, args=args)
        wait_until_ready(f"{base}/api/v1/auth/state")
        session = WebSession(base)
        status, body = session.request("POST", "/api/v1/auth/login", {"username": "admin", "password": "correct horse battery staple"})
        assert status == 200, body
        env["auth"] = session.auth_headers()
        _, body = request("GET", f"api-keys/{key['id']}")
        assert body["data"]["rpm_limit"] == 1
        status, body = infer()
        assert status == 200, body
        assert len(mock.captured_requests) == 2
        _, body = request("PUT", f"api-keys/{key['id']}", {"rpm_limit": None})
        assert body["data"]["rpm_limit"] is None
        for _ in range(2):
            status, body = infer()
            assert status == 200, body
        assert len(mock.captured_requests) == 4
        # 根请求已不限，但目的地发送额度仍限制实际上游请求。
        config["preferred_wait_ms"] = 0
        config["total_wait_ms"] = 0
        status, body = request("PUT", "settings/rpm_admission", {"value": json.dumps(config)})
        assert status == 200, body
        status, body = infer()
        assert status == 429, body
        assert len(mock.captured_requests) == 4
        status, body = request("GET", "settings/rpm_admission")
        assert status == 200, body
        assert json.loads(body["data"]) == config
        status, body = request("GET", f"models/{route_id}")
        assert status == 200, body
        assert body["data"]["targets"][0]["id"] == target_id
        config["destinations"] = []
        status, body = request("PUT", "settings/rpm_admission", {"value": json.dumps(config)})
        assert status == 200, body
        status, body = infer()
        assert status == 200, body
        assert len(mock.captured_requests) == 5
    finally:
        if process is not None:
            stop_stravia_server(process, logs)
        mock.shutdown()
        mock.server_close()
        if schema is not None:
            storage_runtime["run_schema_action"]("drop", pg_url=pg_url, schema=schema)
