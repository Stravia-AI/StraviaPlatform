from __future__ import annotations

import json
import os
import sqlite3
import subprocess
from contextlib import contextmanager
from pathlib import Path
from typing import Any
from urllib.parse import urlencode

import pytest

from tests.common.helpers import (
    find_free_port, initialize_server, start_stravia_server, stop_stravia_server,
    wait_for_setup_token, wait_until_ready,
)
from tests.e2e.admin.test_observations import _wait_for
from tests.e2e.storage.conftest import REPO_ROOT, storage_harness_binary
from tests.e2e.storage.test_cost_and_billing import admin

pytestmark = [pytest.mark.e2e, pytest.mark.storage]


@contextmanager
def fixture_database(backend, sqlite_path, dsn):
    if backend == "postgres":
        import psycopg
        with psycopg.connect(dsn, autocommit=True) as connection:
            yield connection
    else:
        with sqlite3.connect(sqlite_path) as connection:
            yield connection


def install_failure(connection, backend, model, name, message):
    # Faults belong only to isolated legacy fixtures. Startup and recovery use
    # the real migrator; a second guard proves committed batches are not replayed.
    if backend == "sqlite":
        connection.execute(f"""CREATE TRIGGER {name} BEFORE UPDATE ON provider_models
            WHEN OLD.model_id = '{model}' AND json_extract(OLD.snapshot_state,'$.type') = 'edited'
            BEGIN SELECT RAISE(ABORT, '{message}'); END""")
    else:
        connection.execute(f"""CREATE FUNCTION {name}() RETURNS trigger LANGUAGE plpgsql AS $$
            BEGIN IF OLD.model_id = '{model}' AND OLD.snapshot_state->>'type' = 'edited'
            THEN RAISE EXCEPTION '{message}'; END IF; RETURN NEW; END $$""")
        connection.execute(f"""CREATE TRIGGER {name} BEFORE UPDATE ON provider_models
            FOR EACH ROW EXECUTE FUNCTION {name}()""")


def remove_failure(connection, backend, name):
    connection.execute(f"DROP TRIGGER {name}" + (" ON provider_models" if backend == "postgres" else ""))
    if backend == "postgres":
        connection.execute(f"DROP FUNCTION {name}()")


@pytest.mark.parametrize("backend", ["sqlite", "postgres"])
@pytest.mark.parametrize("interrupted", [False, True], ids=["upgrade", "resume"])
def test_legacy_edited_models_keep_pins_and_resume_committed_batches(
    stravia_binary: Path, storage_runtime: dict[str, Any], tmp_path: Path,
    backend: str, interrupted: bool,
):
    schema = None
    dsn = None
    process = None
    logs: list[str] = []
    sqlite_path = tmp_path / "db" / "gateway.db"
    sqlite_path.parent.mkdir(parents=True)
    prepare_env = {
        **os.environ, "STRAVIA_STORAGE_USAGE_UPGRADE_FIXTURE": "cost-and-billing",
        "STRAVIA_STORAGE_BACKEND": backend,
    }
    database: dict[str, Any] = {"backend": backend}
    try:
        if backend == "postgres":
            assert storage_runtime["pg_url"], "PostgreSQL upgrade requires isolated DB_URL"
            schema = storage_runtime["make_isolated_schema"]("stravia_cost_upgrade")
            storage_runtime["run_schema_action"]("create", pg_url=storage_runtime["pg_url"], schema=schema)
            dsn = storage_runtime["postgres_dsn_for_schema"](storage_runtime["pg_url"], schema)
            prepare_env["STRAVIA_STORAGE_PG_URL"] = dsn
            database["url"] = dsn
        else:
            prepare_env["STRAVIA_STORAGE_SQLITE_PATH"] = str(sqlite_path)
        result = subprocess.run([str(storage_harness_binary())], env=prepare_env, cwd=REPO_ROOT,
                                capture_output=True, text=True, timeout=120, check=False)
        assert result.returncode == 0, (result.stdout, result.stderr)
        assert "usage_upgrade_fixture=cost-and-billing" in result.stdout
        with fixture_database(backend, sqlite_path, dsn) as connection:
            connection.execute("""INSERT INTO providers(id,name,vendor,protocol,base_url,api_key,is_enabled)
                VALUES ('legacy-provider','Legacy Provider','custom','openai-compatible','http://127.0.0.1:1','local-test-key',FALSE)""")
            for index in range(201):
                model = f"legacy-{index:03}"
                connection.execute(f"""INSERT INTO provider_models
                    (provider_id,model_id,source_kind,presence,name,metadata_json,snapshot_state)
                    VALUES ('legacy-provider','{model}','discovered','present','Administrator model',
                    '{{"name":"Administrator model","cost":{{"input":"1","output":"2"}}}}',
                    '{{"type":"edited","source":{{"type":"discovery"}}}}')""")
            connection.execute("""INSERT INTO provider_models
                (provider_id,model_id,source_kind,presence,name,metadata_json,snapshot_state)
                VALUES ('legacy-provider','local','manual','present','Local model',
                '{"name":"Local model"}','{"type":"edited","source":null}')""")
            if interrupted:
                install_failure(connection, backend, "legacy-100", "cost_upgrade_fault", "injected billing upgrade failure")

        config = f'[database]\nbackend = "{backend}"\n'
        if dsn:
            config += f"url = {json.dumps(dsn)}\n"
        (tmp_path / "server.toml").write_text(config, encoding="utf-8")
        base = f"http://127.0.0.1:{find_free_port()}"
        args = ["--data-dir", str(tmp_path), "--host", "127.0.0.1", "--port", base.rsplit(":", 1)[1],
                *storage_runtime["server_args"](backend)]
        if interrupted:
            process, logs = start_stravia_server(stravia_binary=stravia_binary, args=args)
            _wait_for("second migration batch failure", lambda: any("injected billing upgrade failure" in line for line in logs), timeout=30)
            stop_stravia_server(process, logs)
            process = None
            with fixture_database(backend, sqlite_path, dsn) as connection:
                remove_failure(connection, backend, "cost_upgrade_fault")
                install_failure(connection, backend, "legacy-000", "cost_upgrade_replay_guard", "committed batch was retried")

        process, logs = start_stravia_server(stravia_binary=stravia_binary, args=args)
        wait_until_ready(base + "/api/v1/auth/state")
        session = initialize_server(base, wait_for_setup_token(logs, process), database)
        env = {"admin": base, "auth": session.auth_headers()}
        for model in ("legacy-000", "legacy-100", "legacy-200"):
            detail = admin(env, "GET", "providers/legacy-provider/model?" + urlencode({"model": model}))
            assert detail["metadata"]["name"] == "Administrator model"
            assert {"name", "reasoning_efforts", "cost.input", "cost.output", "cost.tiers"}.issubset(detail["pinned_fields"])
            assert detail["restorable_fields"] == []
        local = admin(env, "GET", "providers/legacy-provider/model?model=local")
        assert local["metadata"]["name"] == "Local model"
        assert local["pinned_fields"] == []
        assert local["restorable_fields"] == []
        assert all("committed batch was retried" not in line for line in logs)
    finally:
        if process is not None:
            stop_stravia_server(process, logs)
        if schema is not None:
            storage_runtime["run_schema_action"]("drop", pg_url=storage_runtime["pg_url"], schema=schema)
