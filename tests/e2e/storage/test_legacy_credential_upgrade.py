"""真实服务端从 0054 升级：删除仍被引用的旧映射，保留现行规则的还原能力。"""
from __future__ import annotations

import json
import os
import subprocess
import time
from pathlib import Path
from typing import Any

import pytest

from tests.common.helpers import (
    find_free_port, http_request, initialize_server, start_stravia_server,
    stop_stravia_server, wait_for_setup_token, wait_until_ready,
)
from tests.e2e.admin.test_observations import _create_route, _proxy
from tests.e2e.admin.test_reversible_redaction import echo_provider, set_enabled
from tests.e2e.storage.conftest import REPO_ROOT, storage_harness_binary
from tests.e2e.storage.test_cost_billing_upgrade import fixture_database

pytestmark = [pytest.mark.e2e, pytest.mark.storage]

LEGACY_REFERENCE = "<!--sr:" + "j" * 28 + "-->"
RETIRED_REFERENCE = "<!--sr:" + "l" * 28 + "-->"
CURRENT_REFERENCE = "<!--sr:" + "k" * 28 + "-->"
CURRENT_VALUE = "X9!ValidCleanupValue"


@pytest.mark.parametrize("backend", ["sqlite", "postgres"])
def test_upgrade_removes_referenced_legacy_mappings_without_breaking_current_restore(
    stravia_binary: Path, storage_runtime: dict[str, Any], tmp_path: Path, backend: str,
) -> None:
    schema = None
    dsn = None
    process = None
    logs: list[str] = []
    sqlite_path = tmp_path / "db" / "gateway.db"
    sqlite_path.parent.mkdir(parents=True)
    prepare_env = {
        **os.environ,
        "STRAVIA_STORAGE_USAGE_UPGRADE_FIXTURE": "legacy-credential-mappings",
        "STRAVIA_STORAGE_BACKEND": backend,
    }
    database: dict[str, Any] = {"backend": backend}
    placeholder = "%s" if backend == "postgres" else "?"
    try:
        if backend == "postgres":
            schema = storage_runtime["make_isolated_schema"]("stravia_legacy_mapping_upgrade")
            storage_runtime["run_schema_action"]("create", pg_url=storage_runtime["pg_url"], schema=schema)
            dsn = storage_runtime["postgres_dsn_for_schema"](storage_runtime["pg_url"], schema)
            prepare_env["STRAVIA_STORAGE_PG_URL"] = dsn
            database["url"] = dsn
        else:
            prepare_env["STRAVIA_STORAGE_SQLITE_PATH"] = str(sqlite_path)
        prepared = subprocess.run(
            [str(storage_harness_binary())], env=prepare_env, cwd=REPO_ROOT,
            capture_output=True, text=True, timeout=120, check=False,
        )
        assert prepared.returncode == 0, (prepared.stdout, prepared.stderr)
        assert "usage_upgrade_fixture=legacy-credential-mappings" in prepared.stdout
        now = int(time.time() * 1000)
        with fixture_database(backend, sqlite_path, dsn) as connection:
            scope = connection.execute(
                "INSERT INTO content_scopes(principal) VALUES('api-key:legacy-fixture') RETURNING id"
            ).fetchone()[0]
            node = connection.execute(
                f"INSERT INTO turn_nodes(id,scope_id,kind,payload_version,created_at) "
                f"VALUES('legacy-node',{placeholder},'response',6,{placeholder}) RETURNING node_pk",
                (scope, now),
            ).fetchone()[0]
            for reference, value, rules in [
                (LEGACY_REFERENCE, "LegacyWord", []),
                (RETIRED_REFERENCE, "RetiredWord", ["kingfisher.old", "removed-builtin"]),
                (CURRENT_REFERENCE, CURRENT_VALUE, ["generic-api-key"]),
            ]:
                connection.execute(
                    "INSERT INTO reversible_redaction_mappings"
                    "(reference,principal,secret,published_at,created_at,last_detected_at,rule_ids) "
                    f"VALUES({placeholder},'api-key:legacy-fixture',{placeholder},"
                    f"{placeholder},{placeholder},{placeholder},{placeholder})",
                    (reference, value, now, now, now, json.dumps(rules)),
                )
                connection.execute(
                    f"INSERT INTO turn_node_redaction_mappings(node_pk,reference) VALUES({placeholder},{placeholder})",
                    (node, reference),
                )
        base = f"http://127.0.0.1:{find_free_port()}"
        args = ["--data-dir", str(tmp_path), "--host", "127.0.0.1", "--port", base.rsplit(":", 1)[1],
                *storage_runtime["server_args"](backend)]
        process, logs = start_stravia_server(stravia_binary=stravia_binary, args=args)
        wait_until_ready(f"{base}/api/v1/auth/state")
        session = initialize_server(base, wait_for_setup_token(logs, process), database)
        for path in ["/healthz", "/readyz"]:
            status, body = http_request("GET", base + path)
            assert status == 200 and body["status"] == "ok", (path, status, body)
        with fixture_database(backend, sqlite_path, dsn) as connection:
            assert connection.execute(
                "SELECT reference FROM reversible_redaction_mappings ORDER BY reference"
            ).fetchall() == [(CURRENT_REFERENCE,)]
            assert connection.execute(
                "SELECT reference FROM turn_node_redaction_mappings ORDER BY reference"
            ).fetchall() == [(CURRENT_REFERENCE,)]
            assert connection.execute(
                "SELECT success FROM _sqlx_migrations WHERE version=55"
            ).fetchone()[0]
        with echo_provider() as (url, received):
            env = {"admin": base, "proxy": base, "auth": session.auth_headers(), "mock": url}
            model = "legacy-cleanup-http"
            _, key = _create_route(env, model)
            set_enabled(env, True)
            # 将保留下来的控制映射归到本次测试刚通过 API 创建的 Key，验证其仍能还原。
            with fixture_database(backend, sqlite_path, dsn) as connection:
                key_id = connection.execute(
                    f"SELECT id FROM api_keys WHERE name={placeholder}", (f"{model}-key",),
                ).fetchone()[0]
                connection.execute(
                    f"UPDATE reversible_redaction_mappings SET principal={placeholder} WHERE reference={placeholder}",
                    (f"api-key:{key_id}", CURRENT_REFERENCE),
                )
            text = f"LegacyWord {LEGACY_REFERENCE}; RetiredWord {RETIRED_REFERENCE}; current {CURRENT_VALUE}"
            status, body = _proxy(env, key, model, [{"role": "user", "content": text}])
            assert status == 200, body
            expected_upstream = text.replace(CURRENT_VALUE, CURRENT_REFERENCE)
            assert received[-1]["body"]["messages"][-1]["content"] == expected_upstream
            assert body["choices"][0]["message"]["content"] == text
    finally:
        if process is not None:
            stop_stravia_server(process, logs)
        if schema is not None:
            storage_runtime["run_schema_action"]("drop", pg_url=storage_runtime["pg_url"], schema=schema)
