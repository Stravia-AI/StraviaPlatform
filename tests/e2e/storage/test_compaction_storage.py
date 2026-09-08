from __future__ import annotations

import json
from pathlib import Path
from urllib.request import Request, urlopen

import pytest

from tests.common.helpers import (
    WebSession, find_free_port, http_request, initialize_server,
    start_stravia_server, stop_stravia_server, wait_for_setup_token, wait_until_ready,
)
from tests.e2e.admin.test_compaction import (
    _native_route, _run, _success, compaction_provider,
)
from tests.e2e.admin.test_observations import _detail, _route_interactions


@pytest.mark.e2e
@pytest.mark.storage
@pytest.mark.parametrize("backend", ["sqlite", "postgres"], ids=["sqlite", "postgres"])
def test_native_client_and_effective_windows_survive_restart_and_observation_clear(
    stravia_binary: Path, storage_runtime, tmp_path: Path, backend, compaction_provider,
):
    pg_url = storage_runtime["pg_url"]
    if backend == "postgres" and not pg_url:
        pytest.skip("postgres backend requires DB_URL")
    schema = None
    database = {"backend": "sqlite", "path": str(tmp_path / "gateway.db")}
    if backend == "postgres":
        schema = storage_runtime["make_isolated_schema"]("stravia_compaction_restart")
        storage_runtime["run_schema_action"]("create", work_dir=storage_runtime["work_dir"], pg_url=pg_url, schema=schema)
        database = {"backend": "postgres", "url": storage_runtime["postgres_dsn_for_schema"](pg_url, schema)}
    process = None
    logs = []
    port = find_free_port()
    base = f"http://127.0.0.1:{port}"
    args = ["--data-dir", str(tmp_path), "--host", "127.0.0.1", "--port", str(port)]
    try:
        process, logs = start_stravia_server(stravia_binary=stravia_binary, args=args)
        wait_until_ready(f"{base}/api/v1/auth/state", timeout=30)
        session = initialize_server(base, wait_for_setup_token(logs, process), database)
        env = {"admin": base, "proxy": base, "auth": session.auth_headers()}
        model = f"native-cold-{backend}"
        route, key = _native_route(env, compaction_provider[0], model)
        original = [{"role": "user", "content": "removed-history-sentinel"}]
        source = _success(env, key, model, original)
        _run(env, route, source["id"])
        request = Request(f"{base}/v1/responses", data=json.dumps({
            "model": model, "input": original + source["output"],
            "context_management": [{"type": "compaction", "compact_threshold": 2000}],
            "instructions": "pause-after-state", "stream": True,
        }).encode(), headers={"authorization": f"Bearer {key}", "content-type": "application/json"})
        with urlopen(request, timeout=15) as stream:
            for line in stream:
                if not line.startswith(b"data: "):
                    continue
                event = json.loads(line[6:])
                if event["type"] == "response.output_item.done" and event["item"]["type"] == "compaction":
                    window = [event["item"]]
                    # Kill with the upstream body still open: no generation terminal
                    # or graceful shutdown may turn the interrupted response into a node.
                    process.kill()
                    process.wait(timeout=10)
                    break
            else:
                pytest.fail("native state was not published")
        stop_stravia_server(process, logs)
        process = None
        process, logs = start_stravia_server(stravia_binary=stravia_binary, args=args)
        wait_until_ready(f"{base}/api/v1/auth/state", timeout=30)
        session = WebSession(base)
        status, body = session.request("POST", "/api/v1/auth/login", {"username": "admin", "password": "correct horse battery staple"})
        assert status == 200, body
        env["auth"] = session.auth_headers()
        history = window + [{"role": "user", "content": "first compacted turn"}]
        first = _success(env, key, model, history)
        assert _run(env, route, first["id"])["generation_parent_id"] == source["id"]
        generations = {
            run["generation_node_id"]
            for item in _route_interactions(env, route)
            for run in _detail(env, item["id"])["runs"]
            if run.get("generation_node_id")
        }
        assert generations == {source["id"], first["id"]}
        history += first["output"]
        stop_stravia_server(process, logs)
        process = None
        process, logs = start_stravia_server(stravia_binary=stravia_binary, args=args)
        wait_until_ready(f"{base}/api/v1/auth/state", timeout=30)
        session = WebSession(base)
        status, body = session.request("POST", "/api/v1/auth/login", {"username": "admin", "password": "correct horse battery staple"})
        assert status == 200, body
        env["auth"] = session.auth_headers()
        history += [{"role": "user", "content": "cold client shaped replay"}]
        second = _success(env, key, model, history)
        assert _run(env, route, second["id"])["generation_parent_id"] == first["id"]
        history += second["output"]
        old_interactions = {item["id"] for item in _route_interactions(env, route)}
        status, body = http_request("DELETE", f"{base}/api/v1/observations/history", headers=env["auth"])
        assert status == 200, body
        assert _route_interactions(env, route) == []
        history += [{"role": "user", "content": "continuation after diagnostic deletion"}]
        third = _success(env, key, model, history)
        assert _run(env, route, third["id"])["generation_parent_id"] == second["id"]
        assert old_interactions.isdisjoint(item["id"] for item in _route_interactions(env, route))
    finally:
        if process is not None:
            stop_stravia_server(process, logs)
        if schema is not None:
            storage_runtime["run_schema_action"]("drop", work_dir=storage_runtime["work_dir"], pg_url=pg_url, schema=schema)
