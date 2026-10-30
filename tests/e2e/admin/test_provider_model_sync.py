from __future__ import annotations

import json
import sqlite3
import threading
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any
from urllib.parse import urlencode, urlsplit

import pytest

from tests.common.helpers import (
    WebSession, find_free_port, http_request, initialize_server, start_stravia_server,
    stop_stravia_server, wait_for_setup_token, wait_until_ready,
)
from tests.e2e.admin.test_admin import (
    _create_api_key, _create_model, _create_probe_provider, _model_probe_endpoint,
)
from tests.e2e.storage.conftest import (
    load_pg_url, make_isolated_schema, postgres_dsn_for_schema, run_schema_action,
    storage_server_args,
)

pytestmark = [pytest.mark.e2e, pytest.mark.admin]


@contextmanager
def _inventory():
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args: Any) -> None:
            pass

        def do_GET(self) -> None:
            state["requests"].append(self.path)
            if urlsplit(self.path).path != "/models":
                # A valid empty directory prevents catalog/background refresh from
                # contacting public services; account discovery remains independent.
                body, status = b"{}", 200
            else:
                entered, release = state["gate"]
                entered.set()
                assert release.wait(15), "test did not release discovery"
                if urlsplit(self.path).query and state.get("fail_page"):
                    body, status = b'{"error":"local subsequent page failed"}', 503
                else:
                    body = json.dumps(state["body"]).encode()
                    status = state["status"]
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_POST(self) -> None:
            state["inference"].append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            body = json.dumps({
                "id": "local-sync-inference", "object": "chat.completion",
                "model": "alpha", "choices": [{
                    "index": 0, "message": {"role": "assistant", "content": "inventory target reached"},
                    "finish_reason": "stop",
                }], "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4},
            }).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    released = threading.Event()
    released.set()
    state: dict[str, Any] = {
        "body": {"data": [{"id": "alpha"}, {"id": "beta"}]}, "status": 200,
        "requests": [], "inference": [], "gate": (threading.Event(), released),
    }
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}", state
    finally:
        state["gate"][1].set()
        server.shutdown()
        server.server_close()
        thread.join()


@pytest.fixture(params=["sqlite", "postgres"], ids=["sqlite", "postgres"])
def sync_env(request: pytest.FixtureRequest, stravia_binary: Path, tmp_path: Path):
    backend = request.param
    database: dict[str, Any] = {"backend": "sqlite"}
    schema = None
    pg_url = load_pg_url()
    if backend == "postgres":
        if not pg_url:
            pytest.skip("PostgreSQL HTTP model-sync contracts require isolated DB_URL")
        schema = make_isolated_schema("stravia_model_sync_http")
        run_schema_action("create", pg_url=pg_url, schema=schema)
        database = {"backend": "postgres", "url": postgres_dsn_for_schema(pg_url, schema)}
    with _inventory() as (origin, upstream):
        port = find_free_port()
        base = f"http://127.0.0.1:{port}"
        args = [
            "--host", "127.0.0.1", "--port", str(port), "--data-dir", str(tmp_path),
            "--test-catalog-base-url", f"{origin}/catalog",
            *storage_server_args(backend),
        ]
        env: dict[str, Any] = {
            "admin": base, "proxy": base, "mock": origin, "upstream": upstream,
            "data_dir": tmp_path,
        }

        def restart() -> None:
            stop_stravia_server(env["process"], env["logs"])
            env["process"], env["logs"] = start_stravia_server(stravia_binary=stravia_binary, args=args)
            wait_until_ready(f"{base}/api/v1/auth/state")
            session = WebSession(base)
            status, body = session.request("POST", "/api/v1/auth/login", {
                "username": "admin", "password": "correct horse battery staple",
            })
            assert status == 200, body
            env["auth"] = session.auth_headers()

        env["restart"] = restart
        try:
            env["process"], env["logs"] = start_stravia_server(stravia_binary=stravia_binary, args=args)
            wait_until_ready(f"{base}/api/v1/auth/state")
            env["auth"] = initialize_server(
                base, wait_for_setup_token(env["logs"], env["process"]), database,
            ).auth_headers()
            yield env
        finally:
            if "process" in env:
                stop_stravia_server(env["process"], env["logs"])
            if schema:
                run_schema_action("drop", pg_url=pg_url, schema=schema)


def _api(env: dict[str, Any], path: str, method: str = "GET", payload: Any = None) -> Any:
    status, body = http_request(
        method, f"{env['admin']}/api/v1{path}", payload=payload, headers=env["auth"],
    )
    assert status == 200 and "error" not in body, body
    return body["data"]


def test_model_sync_persists_preferences_and_exact_check_history(sync_env: dict[str, Any]) -> None:
    admin_env = sync_env
    with _model_probe_endpoint() as (origin, received):
        provider = _create_probe_provider(admin_env, "persisted-check-history", f"{origin}/models")
        path = f"/providers/{provider}/models"
        initial = _api(admin_env, f"{path}/sync-state")
        assert initial["automatic_sync"] is True
        assert initial["in_progress"] is False
        assert initial["last_check"] is None
        preference = _api(admin_env, f"{path}/sync-state", "PUT", {"automatic_sync": False})
        assert preference["automatic_sync"] is False
        assert preference["last_check"] is None
        summary = _api(admin_env, f"{path}/sync", "POST", {})
        assert summary["added"] == 1
        assert summary["source"] == "manual_sync"
        assert summary["outcome"] == "success"
        assert summary["record_id"]
        record = _api(admin_env, f"/model-changes/{summary['record_id']}")
        assert record["provider_id"] == provider
        assert record["provider_name"] == "persisted-check-history"
        assert record["initial_sync"] is True
        assert record["checked_at"] == summary["checked_at"]
        assert record["source"] == summary["source"]
        assert record["changes"][0]["model_id"] == "probe-model"
        assert "added" in record["changes"][0]["types"]
        unchanged = _api(admin_env, f"{path}/sync", "POST", {})
        assert unchanged["record_id"] is None
        assert unchanged["added"] == unchanged["missing"] == unchanged["restored"] == 0
        state = _api(admin_env, f"{path}/sync-state")
        assert state["automatic_sync"] is False
        assert state["last_check"] == unchanged
        page = _api(admin_env, f"/model-changes?provider_id={provider}")
        assert [entry["id"] for entry in page["records"]] == [record["id"]]
        assert page["next_cursor"] is None
        assert len(received) == 2


def _provider(env: dict[str, Any], name: str = "Inventory timeline") -> tuple[str, str]:
    provider = _create_probe_provider(env, name, f"{env['mock']}/models", credential={"type": "none"})
    path = f"/providers/{provider}"
    _api(env, f"{path}/models/sync-state", "PUT", {"automatic_sync": False})
    return provider, path


def _history(env: dict[str, Any], provider: str, **filters: Any) -> Any:
    return _api(env, f"/model-changes?{urlencode({'provider_id': provider, **filters})}")


def _model(env: dict[str, Any], path: str, model: str) -> Any:
    return _api(env, f"{path}/model?{urlencode({'model': model})}")


def test_model_sync_records_detailed_transitions_and_source_owned_specifications(sync_env):
    env = sync_env
    upstream = env["upstream"]
    upstream["body"] = {"data": [
        {"id": "alpha", "display_name": "Alpha", "limit": {"context": 8192},
         "modalities": {"input": ["text"], "output": ["text"]}},
        {"id": "beta"},
    ]}
    provider, path = _provider(env)
    first = _api(env, f"{path}/models/sync", "POST", {})
    first_record = _api(env, f"/model-changes/{first['record_id']}")
    assert {change["model_id"] for change in first_record["changes"]} == {"alpha", "beta"}
    upstream["body"] = {"data": [
        {"id": "alpha", "display_name": "Alpha", "limit": {"context": 16384},
         "modalities": {"input": ["text", "image"], "output": ["text"]}, "status": "deprecated"},
        {"id": "gamma", "status": "deprecated"},
    ]}
    changed = _api(env, f"{path}/models/sync", "POST", {})
    assert (changed["added"], changed["missing"], changed["deprecated"]) == (1, 1, 2)
    assert changed["specification_updated"] == 1
    record = _api(env, f"/model-changes/{changed['record_id']}")
    changes = {change["model_id"]: change for change in record["changes"]}
    assert set(changes) == {"alpha", "beta", "gamma"}
    assert {"deprecated", "specification_updated"} <= set(changes["alpha"]["types"])
    assert set(changes["gamma"]["types"]) == {"added", "deprecated"}
    assert changes["beta"]["before_presence"] == "present"
    assert changes["beta"]["after_presence"] == "missing"
    diffs = {diff["field"]: diff for diff in changes["alpha"]["specification"]}
    assert diffs["context"]["before"] == 8192 and diffs["context"]["after"] == 16384
    assert diffs["input_modalities"]["before"] == ["text"]
    assert diffs["input_modalities"]["after"] == ["text", "image"]
    # Only visible specifications change: it must not be reported as no change.
    upstream["body"]["data"][0]["limit"]["context"] = 32768
    spec_only = _api(env, f"{path}/models/sync", "POST", {})
    assert spec_only["specification_updated"] == 1 and spec_only["record_id"]
    assert spec_only["added"] == spec_only["missing"] == spec_only["deprecated"] == 0
    alpha = _model(env, path, "alpha")
    metadata = {**alpha["metadata"], "name": "Administrator Alpha", "limit": {"context": 4096}}
    _api(env, f"{path}/model", "PUT", {"model_id": "alpha", "revision": alpha["revision"], "changes": {"name": "Administrator Alpha", "limit.context": 4096}})
    before_edit_history = _history(env, provider)["records"]
    upstream["body"] = {"data": [
        {"id": "alpha", "limit": {"context": 65536}}, {"id": "beta"}, {"id": "gamma"},
    ]}
    restored = _api(env, f"{path}/models/sync", "POST", {})
    assert restored["restored"] == 1 and restored["deprecation_cleared"] == 2
    # name/limit are pinned, but the sparse source no longer declares
    # modalities: unpinned fields follow the source and unregister.
    assert restored["specification_updated"] == 1
    alpha = _model(env, path, "alpha")
    assert alpha["metadata"]["name"] == "Administrator Alpha"
    assert alpha["metadata"]["limit"]["context"] == 4096
    assert alpha["metadata"]["modalities"]["input"] == ["text"]
    assert len(_history(env, provider)["records"]) == len(before_edit_history) + 1
    # Historical differences remain the actual old/new values, not today's edit.
    assert _api(env, f"/model-changes/{record['id']}")["changes"] == record["changes"]


def test_model_sync_failure_preserves_inventory_and_returns_saved_secret_safe_record(sync_env):
    env = sync_env
    provider, path = _provider(env)
    _api(env, f"{path}/models/sync", "POST", {})
    inventory = _api(env, f"{path}/models")
    secret = "synthetic&key=part+/%?#"
    _api(env, path, "PUT", {"api_key": secret})
    for status, payload in [
        (503, {"error": {"message": f"upstream echoed {secret}"}}),
        (200, {"data": []}),
        (200, {"data": [{"no_id": True}]}),
        (200, {"data": [{"id": "alpha"}], "next_cursor": "page-2"}),
    ]:
        env["upstream"]["status"], env["upstream"]["body"] = status, payload
        env["upstream"]["fail_page"] = True
        response_status, response = http_request(
            "POST", f"{env['admin']}/api/v1{path}/models/sync", payload={}, headers=env["auth"],
        )
        assert response_status == 400, response
        summary = response["details"]["sync_summary"]
        assert summary["outcome"] == "failure"
        assert summary["source"] == "manual_sync"
        assert summary["record_id"]
        record = _api(env, f"/model-changes/{summary['record_id']}")
        assert record["outcome"] == "failure"
        assert record["checked_at"] == summary["checked_at"]
        assert record["id"] == summary["record_id"]
        assert record["changes"] == []
        assert _api(env, f"{path}/models") == inventory
        assert _api(env, f"{path}/models/sync-state")["last_check"] == summary
        assert secret not in json.dumps({"response": response, "record": record})
    assert len(_history(env, provider, type="sync_failed")["records"]) == 4


@pytest.mark.parametrize("sync_env", ["sqlite"], indirect=True)
def test_model_sync_postcommit_failure_reports_saved_inventory_and_record(sync_env):
    env = sync_env
    provider, path = _provider(env)
    _api(env, f"{path}/models/sync", "POST", {})
    database = env["data_dir"] / "db" / "gateway.db"
    # 故障只落在库存提交后的插件恢复写入；断言仍经过公开管理接口。
    with sqlite3.connect(database) as connection:
        connection.execute(
            "INSERT INTO vendor_data_recovery(provider_id, data_kind) VALUES (?, 'models')",
            (provider,),
        )
        connection.execute("""
            CREATE TRIGGER fail_model_recovery
            BEFORE DELETE ON vendor_data_recovery
            WHEN OLD.data_kind = 'models'
            BEGIN
                SELECT RAISE(ABORT, 'synthetic postcommit recovery failure');
            END
        """)
    env["upstream"]["body"] = {
        "data": [{"id": "alpha"}, {"id": "beta"}, {"id": "saved-after-recovery-failure"}],
    }
    summary = _api(env, f"{path}/models/sync", "POST", {})
    assert summary["outcome"] == "partial_success"
    assert summary["added"] == 1
    assert summary["record_id"]
    assert summary["error"]
    assert {model["id"] for model in _api(env, f"{path}/models")["models"]} == {
        "alpha", "beta", "saved-after-recovery-failure",
    }
    record = _api(env, f"/model-changes/{summary['record_id']}")
    assert record["outcome"] == "partial_success"
    assert record["checked_at"] == summary["checked_at"]
    assert [change["model_id"] for change in record["changes"]] == [
        "saved-after-recovery-failure",
    ]
    assert _api(env, f"{path}/models/sync-state")["last_check"] == summary
    assert len(_history(env, provider)["records"]) == 2
    with sqlite3.connect(database) as connection:
        connection.execute("DROP TRIGGER fail_model_recovery")
    recovered = _api(env, f"{path}/models/sync", "POST", {})
    assert recovered["outcome"] == "success"
    assert recovered["record_id"] is None
    assert len(_history(env, provider)["records"]) == 2


def test_manual_model_actions_history_filters_and_keyset_keep_complete_batches(sync_env):
    env = sync_env
    provider, path = _provider(env)
    _api(env, f"{path}/models/sync", "POST", {})
    status, body = http_request(
        "POST", f"{env['admin']}/api/v1{path}/models",
        payload={"model_id": "manual_%_needle", "metadata": {"name": "Local Needle"}},
        headers=env["auth"],
    )
    assert status == 201, body
    manual = _model(env, path, "manual_%_needle")
    for policy in ["force_disabled", "force_enabled", "auto"]:
        before = manual["selection_policy"]
        manual = _api(env, f"{path}/model/selection", "PUT", {
            "model_id": manual["id"], "revision": manual["revision"], "policy": policy,
        })
        record = _history(env, provider, type="selection_policy_changed", limit=1)["records"][0]
        assert record["source"] == "manual_change"
        assert record["changes"][0]["before_policy"] == before
        assert record["changes"][0]["after_policy"] == policy
    prior = _history(env, provider)["records"]
    _api(env, f"{path}/model/selection", "PUT", {
        "model_id": manual["id"], "revision": manual["revision"], "policy": "auto",
    })
    _api(env, f"{path}/model", "PUT", {
        "model_id": manual["id"], "revision": manual["revision"],
        "changes": {"name": "Edited Local Needle"},
    })
    assert _history(env, provider)["records"] == prior
    _api(env, f"{path}/models/sync", "POST", {})
    assert _model(env, path, manual["id"])["available"] is True
    found = _history(env, provider, q="NEEDLE", source="manual_change")["records"]
    assert {record["id"] for record in found} == {record["id"] for record in prior if record["source"] == "manual_change"}
    assert len(_history(env, provider, q="manual_%_needle", type="manual_added")["records"]) == 1
    assert _history(env, provider, q="absent literal %")["records"] == []
    batch = _history(env, provider, q="alpha", source="manual_sync")["records"]
    assert len(batch) == 1
    assert {change["model_id"] for change in batch[0]["changes"]} == {"alpha", "beta"}
    page = _history(env, provider, limit=2)
    loaded = page["records"]
    env["upstream"]["body"] = {"data": [{"id": "alpha"}, {"id": "beta"}, {"id": "new-after-page"}]}
    newer = _api(env, f"{path}/models/sync", "POST", {})
    while page["next_cursor"]:
        page = _history(env, provider, limit=2, cursor=page["next_cursor"])
        loaded.extend(page["records"])
    assert [record["id"] for record in loaded] == [record["id"] for record in prior]
    assert newer["record_id"] not in {record["id"] for record in loaded}


def test_model_history_identity_and_preferences_survive_restart_rename_delete_recreate(sync_env):
    env = sync_env
    provider, path = _provider(env, "Historical identity")
    summary = _api(env, f"{path}/models/sync", "POST", {})
    record = _api(env, f"/model-changes/{summary['record_id']}")
    _api(env, path, "PUT", {"name": "Renamed identity"})
    env["restart"]()
    state = _api(env, f"{path}/models/sync-state")
    assert state["automatic_sync"] is False and state["in_progress"] is False
    assert state["last_check"] == summary
    assert _api(env, f"/model-changes/{record['id']}") == record
    status, body = http_request("DELETE", f"{env['admin']}/api/v1{path}", headers=env["auth"])
    assert status == 200, body
    deleted = _api(env, f"/model-changes/{record['id']}")
    assert deleted["provider_deleted"] is True
    assert deleted["provider_name"] == "Historical identity"
    recreated, new_path = _provider(env, "Historical identity")
    assert recreated != provider
    assert _history(env, recreated)["records"] == []
    assert [entry["id"] for entry in _history(env, provider)["records"]] == [record["id"]]
    env["restart"]()
    assert _api(env, f"/model-changes/{record['id']}") == deleted
    assert _api(env, f"{new_path}/models/sync-state")["last_check"] is None


def test_existing_target_follows_synced_availability_without_losing_binding_or_overrides(sync_env):
    env = sync_env
    alpha_metadata = {"id": "alpha", "modalities": {"input": ["text"], "output": ["text"]}}
    env["upstream"]["body"] = {"data": [alpha_metadata, {"id": "beta"}]}
    provider, path = _provider(env)
    _api(env, f"{path}/models/sync", "POST", {})
    route = _create_model(env, provider, "availability-route", target_model="alpha")
    key = _create_api_key(env, route, "availability-client")

    def send(allowed: bool) -> None:
        before = len(env["upstream"]["inference"])
        status, body = http_request("POST", f"{env['proxy']}/v1/chat/completions", payload={
            "model": "availability-route", "stream": False,
            "messages": [{"role": "user", "content": "exercise saved target"}],
        }, headers={"authorization": f"Bearer {key['key']}"})
        if allowed:
            assert status == 200, body
            assert body["choices"][0]["message"]["content"] == "inventory target reached"
            assert len(env["upstream"]["inference"]) == before + 1
            assert env["upstream"]["inference"][-1]["model"] == "alpha"
        else:
            assert status >= 400, body
            assert len(env["upstream"]["inference"]) == before

    route_path = "/models/availability-route"
    saved_route = _api(env, route_path)
    send(True)
    env["upstream"]["body"] = {"data": [{"id": "beta"}]}
    _api(env, f"{path}/models/sync", "POST", {})
    send(False)
    assert _api(env, route_path) == saved_route
    alpha = _model(env, path, "alpha")
    alpha = _api(env, f"{path}/model/selection", "PUT", {
        "model_id": "alpha", "revision": alpha["revision"], "policy": "force_enabled",
    })
    send(True)
    env["upstream"]["body"] = {"data": [{**alpha_metadata, "status": "deprecated"}, {"id": "beta"}]}
    _api(env, f"{path}/models/sync", "POST", {})
    assert _model(env, path, "alpha")["selection_policy"] == "force_enabled"
    send(True)
    alpha = _model(env, path, "alpha")
    _api(env, f"{path}/model/selection", "PUT", {
        "model_id": "alpha", "revision": alpha["revision"], "policy": "auto",
    })
    send(False)
    env["upstream"]["body"] = {"data": [alpha_metadata, {"id": "beta"}]}
    _api(env, f"{path}/models/sync", "POST", {})
    send(True)
    alpha = _model(env, path, "alpha")
    _api(env, f"{path}/model/selection", "PUT", {
        "model_id": "alpha", "revision": alpha["revision"], "policy": "force_disabled",
    })
    _api(env, f"{path}/models/sync", "POST", {})
    send(False)
    assert _model(env, path, "alpha")["selection_policy"] == "force_disabled"
    assert _api(env, route_path) == saved_route


@pytest.mark.parametrize("edit", ["connection", "model"])
def test_inflight_discovery_rejects_stale_connection_or_manual_model_edit(sync_env, edit):
    env = sync_env
    provider, path = _provider(env)
    _api(env, f"{path}/models/sync", "POST", {})
    initial_history = _history(env, provider)["records"]
    entered, release = threading.Event(), threading.Event()
    env["upstream"]["gate"] = entered, release
    env["upstream"]["body"] = {"data": [{"id": "alpha"}, {"id": "new-stale"}]}
    with ThreadPoolExecutor(max_workers=1) as executor:
        pending = executor.submit(
            http_request, "POST", f"{env['admin']}/api/v1{path}/models/sync",
            payload={}, headers=env["auth"],
        )
        try:
            assert entered.wait(15), "discovery did not reach the controlled upstream"
            assert _api(env, f"{path}/models/sync-state")["in_progress"] is True
            if edit == "connection":
                _api(env, path, "PUT", {"base_url": f"{env['mock']}/changed"})
            else:
                alpha = _model(env, path, "alpha")
                _api(env, f"{path}/model", "PUT", {
                    "model_id": "alpha", "revision": alpha["revision"],
                    "changes": {"name": "Concurrent administrator value"},
                })
        finally:
            release.set()
        status, body = pending.result(timeout=15)
    assert status == 409, body
    summary = body["details"]["sync_summary"]
    assert summary["outcome"] == "failure"
    assert {model["id"] for model in _api(env, f"{path}/models")["models"]} == {"alpha", "beta"}
    if edit == "model":
        assert _model(env, path, "alpha")["metadata"]["name"] == "Concurrent administrator value"
    history = _history(env, provider)["records"]
    assert [entry["id"] for entry in history if entry["outcome"] == "success"] == [
        entry["id"] for entry in initial_history
    ]
    assert _api(env, f"{path}/models/sync-state")["in_progress"] is False


def test_model_history_and_sync_preferences_require_management_authentication(sync_env):
    env = sync_env
    provider, path = _provider(env)
    summary = _api(env, f"{path}/models/sync", "POST", {})
    for method, endpoint, payload in [
        ("GET", "/model-changes", None),
        ("GET", f"/model-changes/{summary['record_id']}", None),
        ("GET", f"{path}/models/sync-state", None),
        ("PUT", f"{path}/models/sync-state", {"automatic_sync": True}),
    ]:
        status, _body = http_request(
            method, f"{env['admin']}/api/v1{endpoint}", payload=payload,
            headers={"x-stravia-csrf": "1", "origin": env["admin"]},
        )
        assert status == 401
    assert _api(env, f"{path}/models/sync-state")["automatic_sync"] is False
    assert len(_history(env, provider)["records"]) == 1
