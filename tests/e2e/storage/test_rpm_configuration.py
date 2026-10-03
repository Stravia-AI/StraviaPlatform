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
def test_old_concurrency_limit_is_not_reinterpreted_as_rpm(
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
    finally:
        action("drop", **args)


@pytest.mark.e2e
@pytest.mark.storage
@pytest.mark.parametrize("backend", ["sqlite", "postgres"])
def test_rpm_pools_bindings_and_restart(
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
        config["pools"] = [{"id": "shared", "name": "Shared capacity", "rpm_limit": 3}]
        status, body = request("PUT", "settings/rpm_admission", {"value": json.dumps(config)})
        assert status == 200, body
        status, body = request("GET", f"models/{route_id}")
        assert status == 200, body
        target = body["data"]["targets"][0]
        destination = {"provider_id": target["provider_id"], "model": target["model"], "rpm_limit": None, "rpm_pool_id": "shared"}
        config["destinations"] = [destination]
        status, body = request("PUT", "settings/rpm_admission", {"value": json.dumps(config)})
        assert status == 200, body
        target_id = target["id"]
        invalid = {**config, "pools": []}
        assert "error" in request("PUT", "settings/rpm_admission", {"value": json.dumps(invalid)})[1]
        invalid = {**config, "destinations": [{**destination, "rpm_pool_id": "missing"}]}
        assert "error" in request("PUT", "settings/rpm_admission", {"value": json.dumps(invalid)})[1]
        for invalid in (
            {**config, "destinations": [{**destination, "rpm_limit": 7}]},
            {**config, "pools": config["pools"] * 2},
            {**config, "destinations": config["destinations"] * 2},
            {**config, "pools": [{"id": "shared", "name": " ", "rpm_limit": 3}]},
            {**config, "pools": [{"id": "shared", "name": "Shared", "rpm_limit": 0}]},
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
        status, body = request("GET", "settings/rpm_admission")
        assert status == 200, body
        assert json.loads(body["data"]) == config
        status, body = request("GET", f"models/{route_id}")
        assert status == 200, body
        assert body["data"]["targets"][0]["id"] == target_id
        assert "rpm_pool_id" not in body["data"]["targets"][0]
        # 目的地解绑与删池在同一配置写入中完成，Route Target 不保存成员关系。
        config["destinations"] = [{**destination, "rpm_limit": 7, "rpm_pool_id": None}]
        config["pools"] = []
        status, body = request("PUT", "settings/rpm_admission", {"value": json.dumps(config)})
        assert status == 200, body
    finally:
        if process is not None:
            stop_stravia_server(process, logs)
        mock.shutdown()
        mock.server_close()
        if schema is not None:
            storage_runtime["run_schema_action"]("drop", pg_url=pg_url, schema=schema)
