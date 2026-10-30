from __future__ import annotations

import base64
import json
import socket
import sqlite3
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing, contextmanager
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any
from urllib.error import HTTPError
from urllib.parse import urlencode
from urllib.request import Request, urlopen

import pytest

from tests.common.helpers import (
    find_free_port, http_request, initialize_server, minimal_mock_provider,
    start_stravia_server, stop_stravia_server, wait_for_setup_token, wait_until_ready,
)
from tests.e2e.admin.test_observations import (
    _create_route, _detail, _finished_detail, _proxy, _route_interactions, _wait_for,
)

pytestmark = [pytest.mark.e2e, pytest.mark.storage]


@contextmanager
def usage_upstream():
    """Only the external provider is scripted; gateway/storage remain real."""
    entered = threading.Event()
    release = threading.Event()
    attempts: dict[str, int] = {}
    lock = threading.Lock()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args: Any) -> None:
            pass

        def do_POST(self) -> None:
            request = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
            content = request["messages"][-1]["content"]
            # 多模态请求的 content 是数组，不能直接作为 attempts 的键。
            scenario = content if isinstance(content, str) else json.dumps(content, sort_keys=True)
            with lock:
                attempts[scenario] = attempts.get(scenario, 0) + 1
                attempt = attempts[scenario]
            if scenario == "cancelled":
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                self.wfile.write(("data: " + json.dumps({"id": "local-cancel", "object": "chat.completion.chunk", "model": request["model"], "choices": [{"index": 0, "delta": {"content": "started"}, "finish_reason": None}]}) + "\n\n").encode())
                self.wfile.flush()
                entered.set()
                assert release.wait(15), "test did not close cancelled stream"
                return
            if scenario == "blocked":
                entered.set()
                assert release.wait(15), "test did not release upstream barrier"
            if scenario == "stream-error":
                # 200 之后才收到流错误：响应已开始，尝试仍应记为上游错误。
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                self.wfile.write(("data: " + json.dumps({
                    "id": "local-stream-error", "object": "chat.completion.chunk", "model": request["model"],
                    "choices": [{"index": 0, "delta": {"content": "partial"}, "finish_reason": None}],
                }) + "\n\n").encode())
                self.wfile.flush()
                self.wfile.write(("data: " + json.dumps({
                    "error": {"type": "server_error", "code": "stream_failed", "message": "upstream failed after output"},
                }) + "\n\n").encode())
                self.wfile.flush()
                self.close_connection = True
                return
            if scenario == "invalid-parameter":
                body = json.dumps({"error": {"type": "invalid_request_error", "message": "Unsupported parameter"}}).encode()
                self.send_response(400)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
            if scenario == "failed" or scenario in ("recovered", "tool-call-recovered") and attempt == 1:
                body = json.dumps({"error": {"type": "upstream_unavailable", "message": "local fixture failure"}}).encode()
                self.send_response(503)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
            usage = {
                "prompt_tokens": 20, "completion_tokens": 7, "total_tokens": 27,
                "prompt_tokens_details": {"cached_tokens": 5},
                "completion_tokens_details": {"reasoning_tokens": 2},
            }
            if scenario == "unknown":
                usage = None
            elif scenario == "zero":
                usage = {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0,
                         "prompt_tokens_details": {"cached_tokens": 0},
                         "completion_tokens_details": {"reasoning_tokens": 0}}
            answer = "ok"
            if isinstance(content, list):
                sources = []
                for part in content:
                    if part.get("type") != "text":
                        continue
                    try:
                        envelope = json.loads(part["text"])
                    except json.JSONDecodeError:
                        continue
                    if isinstance(envelope, dict):
                        sources.extend(source["path"] for source in envelope.get("media", []))
                if sources:
                    answer = json.dumps({
                        "answer": "A single pixel. " + " ".join(f"[{path}]" for path in sources),
                        "artifacts": [{"path": path} for path in sources], "limitations": [],
                    })
            if scenario in ("tool-call", "tool-call-recovered"):
                # 客户端工具交接：运行以 waiting_client 收口，而不是 completed。
                body = json.dumps({
                    "id": "local-tool-call", "object": "chat.completion", "model": request["model"],
                    "choices": [{
                        "index": 0,
                        "message": {
                            "role": "assistant", "content": None,
                            "tool_calls": [{
                                "id": f"call-usage-tool-{attempt}", "type": "function",
                                "function": {"name": "local_probe", "arguments": json.dumps({"round": 1})},
                            }],
                        },
                        "finish_reason": "tool_calls",
                    }],
                    **({"usage": usage} if usage is not None else {}),
                }).encode()
                content_type = "application/json"
            elif request.get("stream"):
                frames = []
                for report in ([usage, usage, {"completion_tokens": 0}, {"prompt_tokens": None}] if scenario == "revised" else [usage]):
                    frames.append({"id": "local-usage", "object": "chat.completion.chunk", "model": request["model"],
                                   "choices": [{"index": 0, "delta": {"content": answer}, "finish_reason": None}],
                                   **({"usage": report} if report is not None else {})})
                frames.append({"id": "local-usage", "object": "chat.completion.chunk", "model": request["model"],
                               "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]})
                body = ("".join("data: " + json.dumps(frame) + "\n\n" for frame in frames) + "data: [DONE]\n\n").encode()
                content_type = "text/event-stream"
            else:
                body = json.dumps({"id": "local-usage", "object": "chat.completion", "model": request["model"],
                                   "choices": [{"index": 0, "message": {"role": "assistant", "content": answer}, "finish_reason": "stop"}],
                                   **({"usage": usage} if usage is not None else {})}).encode()
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
        yield server, entered, release, attempts
    finally:
        release.set()
        server.shutdown()
        server.server_close()
        thread.join()


@pytest.fixture(params=["sqlite", "postgres"], ids=["sqlite", "postgres"])
def usage_env(request, stravia_binary: Path, storage_runtime: dict[str, Any], tmp_path: Path):
    backend = request.param
    schema = None
    database: dict[str, Any] = {"backend": "sqlite"}
    if backend == "postgres":
        if not storage_runtime["pg_url"]:
            pytest.fail("PostgreSQL stats contracts require isolated DB_URL")
        schema = storage_runtime["make_isolated_schema"]("stravia_usage_contract")
        storage_runtime["run_schema_action"]("create", pg_url=storage_runtime["pg_url"], schema=schema)
        database = {"backend": "postgres", "url": storage_runtime["postgres_dsn_for_schema"](storage_runtime["pg_url"], schema)}
    process = None
    logs = []
    try:
        with usage_upstream() as (upstream, entered, release, attempts):
            base = f"http://127.0.0.1:{find_free_port()}"
            args = ["--data-dir", str(tmp_path), "--host", "127.0.0.1", "--port", base.rsplit(":", 1)[1]]
            process, logs = start_stravia_server(stravia_binary=stravia_binary, args=args + storage_runtime["server_args"](backend))
            wait_until_ready(f"{base}/api/v1/auth/state")
            session = initialize_server(base, wait_for_setup_token(logs, process), database)
            yield {"admin": base, "proxy": base, "auth": session.auth_headers(),
                   "mock": f"http://127.0.0.1:{upstream.server_port}", "entered": entered, "release": release,
                   "attempts": attempts,
                   "backend": backend, "data_dir": tmp_path, "schema": schema,
                   "storage_runtime": storage_runtime, "process": process}
    finally:
        if process is not None:
            stop_stravia_server(process, logs)
        if schema is not None:
            storage_runtime["run_schema_action"]("drop", pg_url=storage_runtime["pg_url"], schema=schema)


@contextmanager
def terminal_attempt_stats_fault(env):
    """Reject only the real summary credit for successful upstream attempts."""
    if env["backend"] == "postgres":
        runtime = env["storage_runtime"]
        arguments = {"pg_url": runtime["pg_url"], "schema": env["schema"]}
        runtime["run_schema_action"]("stats-attempt-fault-install", **arguments)
        try:
            yield
        finally:
            runtime["run_schema_action"]("stats-attempt-fault-remove", **arguments)
    else:
        # usage_env starts --data-dir=tmp_path; DataPaths::database derives db/gateway.db.
        database_path = env["data_dir"] / "db" / "gateway.db"
        assert database_path.is_file(), "initialized isolated SQLite database missing"
        with closing(sqlite3.connect(database_path)) as database:
            database.executescript("""
                CREATE TRIGGER stravia_test_stats_attempt_insert
                BEFORE INSERT ON stats_usage_buckets
                WHEN NEW.successful_attempt_count > 0
                BEGIN SELECT RAISE(ABORT, 'injected terminal attempt stats failure'); END;
                CREATE TRIGGER stravia_test_stats_attempt_update
                BEFORE UPDATE ON stats_usage_buckets
                WHEN NEW.successful_attempt_count > OLD.successful_attempt_count
                BEGIN SELECT RAISE(ABORT, 'injected terminal attempt stats failure'); END;
            """)
        try:
            yield
        finally:
            with closing(sqlite3.connect(database_path)) as database:
                database.executescript("""
                    DROP TRIGGER IF EXISTS stravia_test_stats_attempt_insert;
                    DROP TRIGGER IF EXISTS stravia_test_stats_attempt_update;
                """)


def test_stats_write_failure_rejects_public_purge_and_preserves_history(usage_env):
    env = usage_env
    route_id, key = _create_route(env, "usage-failure-purge", retry_budget=0)
    status, body = _proxy(env, key, "usage-failure-purge", [{"role": "user", "content": "baseline"}])
    assert status == 200 and body["choices"][0]["message"]["content"] == "ok", body
    baseline = _wait_for("baseline usage history", lambda: next((
        item for item in _route_interactions(env, route_id)
        if item["input_preview"] == "baseline" and item["status"] == "completed"
    ), None))
    before = _wait_for("baseline terminal usage", lambda: _finished_detail(env, baseline["id"]))
    assert before["interaction"]["usage"]["output_tokens"] == 7
    with terminal_attempt_stats_fault(env):
        status, body = _proxy(env, key, "usage-failure-purge", [{"role": "user", "content": "stats-failure"}])
        assert status == 200 and body["choices"][0]["message"]["content"] == "ok", body
        # Do not wait for a rolled-back terminal observation. The gap proves
        # the targeted stats-bearing transaction failed before invoking purge.
        failed = _wait_for("stats transaction persistence gap", lambda: next((
            item for item in _route_interactions(env, route_id)
            if item["input_preview"] == "stats-failure" and item["observation_gap"]
        ), None))
        status, error = http_request("DELETE", f"{env['admin']}/api/v1/observations/history", headers=env["auth"])
        assert status == 503, f"unsafe history purge returned HTTP {status}"
        assert error["code"] == "observation_unavailable"
        retained = _detail(env, baseline["id"])
        assert retained["interaction"]["usage"] == before["interaction"]["usage"]
        assert retained["runs"] == before["runs"], "failed purge deleted retained attempt facts"
        assert _detail(env, failed["id"])["interaction"]["observation_gap"]
        assert env["process"].poll() is None, "stats persistence failure stopped the host"
    status, body = _proxy(env, key, "usage-failure-purge", [{"role": "user", "content": "following"}])
    assert status == 200 and body["choices"][0]["message"]["content"] == "ok", body
    following = _wait_for("following successful inference", lambda: next((
        item for item in _route_interactions(env, route_id)
        if item["input_preview"] == "following" and item["status"] == "completed"
    ), None))
    detail = _wait_for("following terminal usage", lambda: _finished_detail(env, following["id"]))
    assert detail["interaction"]["usage"]["output_tokens"] == 7
    assert env["process"].poll() is None
    # Removing the injection does not fabricate repair of lost usage facts.
    status, error = http_request("DELETE", f"{env['admin']}/api/v1/observations/history", headers=env["auth"])
    assert status == 503
    assert error["code"] == "observation_unavailable"
    assert _detail(env, baseline["id"])["interaction"]["usage"] == before["interaction"]["usage"]


def stats(env, view="overview", **query):
    status, body = http_request("GET", f"{env['admin']}/api/v1/stats/{view}?{urlencode(query)}", headers=env["auth"])
    assert status == 200, body
    return body["data"]


def clear(env):
    status, body = http_request("DELETE", f"{env['admin']}/api/v1/observations/history", headers=env["auth"])
    assert status == 200, body


SPLIT_COUNTERS = (
    "terminal_request_count", "failed_request_count",
    "upstream_request_count", "upstream_error_count",
)

PNG = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII="
TOOLS = [{"type": "function", "function": {"name": "local_probe", "parameters": {"type": "object"}}}]


def assert_split(view, *, terminal, failed, upstream, errors):
    assert view["terminal_request_count"] == terminal, view
    assert view["failed_request_count"] == failed, view
    assert view["upstream_request_count"] == upstream, view
    assert view["upstream_error_count"] == errors, view
    assert "error_count" not in view, view


def provider(env, name, base_url, *, input_modalities=("text",), reasoning_efforts=(), protocol="openai-compatible"):
    status, body = http_request(
        "POST", f"{env['admin']}/api/v1/providers",
        payload={
            "name": name,
            "source": {
                "type": "custom", "vendor": "custom", "channel": "default",
                "protocol": protocol, "base_url": base_url,
            },
            "credential": {"type": "api_key", "value": "upstream-key"},
            "vendor_options": {},
        },
        headers=env["auth"],
    )
    assert status == 200, body
    provider_id = body["data"]["id"]
    status, body = http_request(
        "POST", f"{env['admin']}/api/v1/providers/{provider_id}/models",
        payload={
            "model_id": "gpt-4o-mini",
            "metadata": {
                "name": name,
                "modalities": {"input": list(input_modalities), "output": ["text"]},
                **({"reasoning_efforts": list(reasoning_efforts)} if reasoning_efforts else {}),
            },
        },
        headers=env["auth"],
    )
    assert status == 201, body
    return provider_id


def route_with_targets(env, model, targets):
    status, body = http_request(
        "POST", f"{env['admin']}/api/v1/models",
        payload={
            "model_id": model, "output_modality": "text",
            "display_name": f"{model} display", "targets": targets,
        },
        headers=env["auth"],
    )
    assert status == 200, body
    route_id = body["data"]["id"]
    status, body = http_request(
        "POST", f"{env['admin']}/api/v1/api-keys",
        payload={"name": f"{model}-key", "model_ids": [route_id]},
        headers=env["auth"],
    )
    assert status == 200, body
    return route_id, str(body["data"]["key"])


def provider_id(env, name):
    status, body = http_request("GET", f"{env['admin']}/api/v1/providers", headers=env["auth"])
    assert status == 200, body
    return next(item["id"] for item in body["data"] if item["name"] == name)


def _run_sql(env, statements):
    """直接操作隔离数据库，仅用于触发公开 API 无法构造的过期/额度场景。"""
    if env["backend"] == "postgres":
        import psycopg
        runtime = env["storage_runtime"]
        dsn = runtime["postgres_dsn_for_schema"](runtime["pg_url"], env["schema"])
        with psycopg.connect(dsn) as connection, connection.cursor() as cursor:
            for sql, params in statements:
                cursor.execute(sql, params)
            connection.commit()
    else:
        database = env["data_dir"] / "db" / "gateway.db"
        with closing(sqlite3.connect(database)) as connection:
            for sql, params in statements:
                connection.execute(sql, params)
            connection.commit()


def age_observations(env):
    """把观察明细的时间推到保留期外；统计桶按原始起点保留，不受影响。"""
    old = int(time.time() * 1000) - 2 * 86_400_000
    placeholder = "%s" if env["backend"] == "postgres" else "?"
    _run_sql(env, [
        (f"UPDATE interaction_observations SET last_active_at={placeholder}", (old,)),
        (f"UPDATE inference_run_observations SET last_active_at={placeholder}", (old,)),
        (f"UPDATE rejected_request_observations SET occurred_at={placeholder}", (old,)),
        (f"UPDATE observation_events SET occurred_at={placeholder}", (old,)),
    ])


def suspend_provider_allowance(env, provider):
    now = int(time.time() * 1000)
    if env["backend"] == "postgres":
        sql = (
            "INSERT INTO provider_allowance_suspensions(provider_id,suspended,triggered_keys,evidence_completed_at) "
            "VALUES(%s,TRUE,'[]',%s) ON CONFLICT (provider_id) DO UPDATE SET "
            "suspended=TRUE,triggered_keys='[]',evidence_completed_at=EXCLUDED.evidence_completed_at"
        )
    else:
        sql = (
            "INSERT OR REPLACE INTO provider_allowance_suspensions(provider_id,suspended,triggered_keys,evidence_completed_at) "
            "VALUES(?,1,'[]',?)"
        )
    _run_sql(env, [(sql, (provider, now))])


def set_retention_days(env, days):
    status, body = http_request(
        "PUT", f"{env['admin']}/api/v1/settings/log_retention_days",
        payload={"value": str(days)}, headers=env["auth"],
    )
    assert status == 200, body


@pytest.mark.parametrize("scenario,input_tokens,output_tokens,cache_read,reasoning", [
    ("known", 15, 7, 5, 2), ("unknown", None, None, None, None),
    ("zero", 0, 0, 0, 0), ("revised", 15, 0, 5, 2),
])
def test_exact_known_unknown_zero_and_revised_usage(usage_env, scenario, input_tokens, output_tokens, cache_read, reasoning):
    env = usage_env
    _route, key = _create_route(env, "usage-values")
    status, body = _proxy(env, key, "usage-values", [{"role": "user", "content": scenario}],
                          body_extra={"stream": scenario == "revised"})
    assert status == 200, body
    clear(env)
    overview = stats(env)
    assert overview["total_requests"] == 1
    assert_split(overview, terminal=1, failed=0, upstream=1, errors=0)
    assert overview["total_input_tokens"] == input_tokens
    assert overview["total_output_tokens"] == output_tokens
    assert overview["total_cache_read_tokens"] == cache_read
    assert overview["total_reasoning_tokens"] == reasoning
    assert overview["total_cache_write_tokens"] is None
    model = stats(env, "models")[0]
    api_key = stats(env, "api-keys")[0]
    assert model["request_count"] == api_key["request_count"] == 1
    assert model["total_input_tokens"] == api_key["total_input_tokens"] == input_tokens
    assert model["total_output_tokens"] == api_key["total_output_tokens"] == output_tokens
    if scenario == "unknown":
        assert overview["avg_output_tps"] is None
    if scenario == "zero":
        assert overview["avg_output_tps"] == 0


def test_retries_recovery_failure_and_rejection_are_request_facts(usage_env):
    env = usage_env
    _route, key = _create_route(env, "usage-outcomes", retry_budget=1)
    for scenario in ("recovered", "failed"):
        status, body = _proxy(env, key, "usage-outcomes", [{"role": "user", "content": scenario}])
        assert (status == 200 if scenario == "recovered" else status >= 500), body
    status, body = http_request("POST", f"{env['proxy']}/v1/chat/completions", payload={"model": "usage-outcomes", "messages": []})
    assert status == 401, body
    clear(env)
    overview = stats(env)
    assert overview["total_requests"] == 3
    # recovered: 1 次上游错误后被重试吸收；failed: 重试预算耗尽后两次上游错误。
    assert_split(overview, terminal=2, failed=1, upstream=4, errors=3)
    assert overview["total_input_tokens"] == 15
    assert overview["total_output_tokens"] == 7
    provider = stats(env, "providers")[0]
    assert provider["upstream_request_count"] == 4
    assert provider["upstream_error_count"] == 3
    assert "request_count" not in provider and "error_count" not in provider
    assert stats(env, "models")[0]["request_count"] == 2
    assert stats(env, "api-keys")[0]["request_count"] == 2


def test_client_disconnect_does_not_invent_failure_or_tokens(usage_env):
    env = usage_env
    _route, key = _create_route(env, "usage-cancelled")
    connection = HTTPConnection("127.0.0.1", int(env["proxy"].rsplit(":", 1)[1]), timeout=15)
    try:
        connection.request("POST", "/v1/chat/completions", body=json.dumps({
            "model": "usage-cancelled", "messages": [{"role": "user", "content": "cancelled"}], "stream": True,
        }), headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"})
        response = connection.getresponse()
        assert response.status == 200
        assert response.readline().startswith(b"data: ")
        assert env["entered"].wait(10)
        response.close()
    finally:
        connection.close()
        env["release"].set()
    clear(env)
    overview = stats(env)
    assert overview["total_requests"] == 1
    assert_split(overview, terminal=0, failed=0, upstream=0, errors=0)
    assert overview["total_input_tokens"] is None
    assert overview["total_output_tokens"] is None
    assert overview["avg_output_tps"] is None


def test_unknown_sample_does_not_mask_known_tokens_or_fabricate_tps(usage_env):
    env = usage_env
    _route, key = _create_route(env, "usage-partial")
    for scenario in ("known", "unknown"):
        status, body = _proxy(env, key, "usage-partial", [{"role": "user", "content": scenario}])
        assert status == 200, body
    clear(env)
    overview = stats(env)
    assert overview["total_requests"] == 2
    assert overview["total_input_tokens"] == 15
    assert overview["total_output_tokens"] == 7
    assert overview["total_cache_read_tokens"] == 5
    assert overview["avg_output_tps"] is None


def test_clear_during_inflight_request_keeps_both_contributions(usage_env):
    env = usage_env
    _route, key = _create_route(env, "usage-concurrent")
    status, body = _proxy(env, key, "usage-concurrent", [{"role": "user", "content": "known"}])
    assert status == 200, body
    with ThreadPoolExecutor(max_workers=1) as executor:
        pending = executor.submit(_proxy, env, key, "usage-concurrent", [{"role": "user", "content": "blocked"}])
        assert env["entered"].wait(10), "inflight request did not reach upstream"
        try:
            clear(env)
        finally:
            env["release"].set()
        status, body = pending.result(timeout=15)
        assert status == 200, body
    clear(env)
    overview = stats(env)
    assert overview["total_requests"] == 2
    assert_split(overview, terminal=2, failed=0, upstream=2, errors=0)
    assert overview["total_input_tokens"] == 30
    assert overview["total_output_tokens"] == 14


def test_every_stats_view_rejects_invalid_absolute_window(usage_env):
    env = usage_env
    invalid_windows = [
        {"start_at": 900000}, {"end_at": 1800000},
        {"start_at": 1800000, "end_at": 900000},
        {"start_at": 900000, "end_at": 900000},
        {"start_at": 900001, "end_at": 1800000},
        {"start_at": 0, "end_at": 900000},
    ]
    for view in ("overview", "series", "models", "providers", "api-keys"):
        for query in invalid_windows:
            status, body = http_request("GET", f"{env['admin']}/api/v1/stats/{view}?{urlencode(query)}", headers=env["auth"])
            assert status == 400, (view, query, body)
        status, body = http_request("GET", f"{env['admin']}/api/v1/stats/{view}?hours=24", headers=env["auth"])
        assert status == 400, (view, body)
    for bucket in (0, -900, 1, 901):
        status, body = http_request("GET", f"{env['admin']}/api/v1/stats/series?bucket={bucket}", headers=env["auth"])
        assert status == 400, (bucket, body)


def test_deleted_recreated_and_renamed_resources_keep_distinct_history(usage_env):
    env = usage_env
    model_id, key = _create_route(env, "usage-identity")
    status, body = _proxy(env, key, "usage-identity", [{"role": "user", "content": "known"}])
    assert status == 200, body
    clear(env)
    provider_id = stats(env, "providers")[0]["provider_id"]
    key_id = stats(env, "api-keys")[0]["api_key_id"]
    assert stats(env, "models")[0]["model_id"] == model_id
    for resource, identity, update in [
        ("models", model_id, {"display_name": "renamed model"}),
        ("providers", provider_id, {"name": "renamed provider"}),
        ("api-keys", key_id, {"name": "renamed key"}),
    ]:
        status, body = http_request("PUT", f"{env['admin']}/api/v1/{resource}/{identity}", payload=update, headers=env["auth"])
        assert status == 200, body
    status, body = _proxy(env, key, "usage-identity", [{"role": "user", "content": "known"}])
    assert status == 200, body
    clear(env)
    renamed_history = {}
    for view, field, identity in [("models", "model_id", model_id), ("providers", "provider_id", provider_id), ("api-keys", "api_key_id", key_id)]:
        rows = stats(env, view)
        assert len(rows) == 1, rows
        assert rows[0][field] == identity
        if view == "providers":
            assert rows[0]["upstream_request_count"] == 2
            assert rows[0]["upstream_error_count"] == 0
            assert "request_count" not in rows[0] and "error_count" not in rows[0]
        else:
            assert rows[0]["request_count"] == 2
        assert rows[0]["deleted"] is False, rows[0]
        renamed_history[view] = rows[0]
    for resource, identity in [("api-keys", key_id), ("models", model_id), ("providers", provider_id)]:
        status, body = http_request("DELETE", f"{env['admin']}/api/v1/{resource}/{identity}", headers=env["auth"])
        assert status == 200, body
        status, body = http_request("GET", f"{env['admin']}/api/v1/{resource}/{identity}", headers=env["auth"])
        # Management lookups report missing configuration in an error envelope,
        # unlike inference and observation endpoints that use HTTP 404.
        missing_error = {
            "models": f"Route not found: {identity}",
            "providers": f"provider not found: {identity}",
            "api-keys": "api key not found",
        }[resource]
        assert status == 200, body
        assert body == {"error": missing_error}, body
    status, body = _proxy(env, key, "usage-identity", [{"role": "user", "content": "known"}])
    assert status == 401, body
    recreated_model, recreated_key = _create_route(env, "usage-identity")
    assert recreated_model != model_id
    status, body = _proxy(env, recreated_key, "usage-identity", [{"role": "user", "content": "known"}])
    assert status == 200, body
    clear(env)
    for view, field, old_identity in [("models", "model_id", model_id), ("providers", "provider_id", provider_id), ("api-keys", "api_key_id", key_id)]:
        rows = stats(env, view)
        assert len(rows) == 2, rows
        old = next(row for row in rows if row[field] == old_identity)
        new = next(row for row in rows if row[field] != old_identity)
        if view == "providers":
            assert old["upstream_request_count"] == 2
            assert new["upstream_request_count"] == 1
            assert old["upstream_error_count"] == new["upstream_error_count"] == 0
        else:
            assert old["request_count"] == 2
            assert new["request_count"] == 1
        assert old["deleted"] is True, old
        assert new["deleted"] is False, new
        assert new[field] is not None, new
        label = {"models": "model", "providers": "provider", "api-keys": "api_key_name"}[view]
        assert old[label], old
        assert old[label] == renamed_history[view][label], old
        assert new[label] == {
            "models": "usage-identity display",
            "providers": "usage-identity-provider",
            "api-keys": "usage-identity-key",
        }[view], new
    overview = stats(env)
    assert overview["total_requests"] == 4
    # 401 是准入前拒绝，不是失败的请求；三次成功请求各自形成一次已终结请求。
    assert_split(overview, terminal=3, failed=0, upstream=3, errors=0)
    assert overview["total_input_tokens"] == 45
    assert overview["total_output_tokens"] == 21


def test_retention_setting_and_debug_cleanup_do_not_reset_usage(usage_env):
    env = usage_env
    status, body = http_request("PUT", f"{env['admin']}/api/v1/observations/debug", payload={"enabled": True, "confirmed": True}, headers=env["auth"])
    assert status == 200, body
    _route, key = _create_route(env, "usage-debug-lifecycle")
    status, body = _proxy(env, key, "usage-debug-lifecycle", [{"role": "user", "content": "known"}])
    assert status == 200, body
    status, body = http_request("DELETE", f"{env['admin']}/api/v1/observations/debug", headers=env["auth"])
    assert status == 200, body
    # Debug cleanup serializes observation writes without deleting request facts.
    before = {view: stats(env, view) for view in ("overview", "models", "providers", "api-keys")}
    assert before["overview"]["total_requests"] == 1
    assert_split(before["overview"], terminal=1, failed=0, upstream=1, errors=0)
    assert before["overview"]["total_output_tokens"] == 7
    status, body = http_request("GET", f"{env['admin']}/api/v1/observations/debug", headers=env["auth"])
    assert status == 200, body
    assert body["data"]["retained_bytes"] == 0
    for days in (30, 7):
        status, body = http_request("PUT", f"{env['admin']}/api/v1/settings/log_retention_days", payload={"value": str(days)}, headers=env["auth"])
        assert status == 200, body
        assert {view: stats(env, view) for view in before} == before
    clear(env)
    assert {view: stats(env, view) for view in before} == before


def test_long_window_coarsens_without_truncating_current_usage(usage_env):
    env = usage_env
    _route, key = _create_route(env, "usage-long-window")
    status, body = _proxy(env, key, "usage-long-window", [{"role": "user", "content": "known"}])
    assert status == 200, body
    clear(env)
    end = int(time.time() * 1000) // 900000 * 900000 + 900000
    query = {"start_at": end - 900000 * 2048, "end_at": end}
    overview = stats(env, **query)
    series = stats(env, "series", bucket=900, **query)
    assert 1 <= len(series) <= 512
    assert sum(row["request_count"] for row in series) == overview["total_requests"] == 1
    assert_split(overview, terminal=1, failed=0, upstream=1, errors=0)
    for field in SPLIT_COUNTERS:
        assert sum(row[field] for row in series) == overview[field]
        assert all(field in row for row in series)
    assert all("error_count" not in row for row in series)
    assert sum(row["total_output_tokens"] or 0 for row in series) == overview["total_output_tokens"] == 7
    assert sum(row["total_input_tokens"] or 0 for row in series) == overview["total_input_tokens"] == 15
    assert all(query["start_at"] <= row["bucket_start"] < end for row in series)
    assert stats(env)["total_output_tokens"] == 7
    for view in ("models", "api-keys"):
        assert stats(env, view, **query)[0]["request_count"] == 1
    provider = stats(env, "providers", **query)[0]
    assert provider["upstream_request_count"] == 1
    assert provider["upstream_error_count"] == 0
    # Adjacent historical range excludes the request, without drifting to now.
    previous = {"start_at": query["start_at"] - 900000, "end_at": query["start_at"]}
    assert stats(env, **previous)["total_requests"] == 0
    assert stats(env, **previous)["total_requests"] == 0


def test_switch_then_success_counts_absorbed_upstream_error(usage_env):
    env = usage_env
    model = "usage-switch-recovered"
    first_provider = provider(env, f"{model}-first", env["mock"])
    second_provider = provider(env, f"{model}-second", env["mock"])
    _route, key = route_with_targets(env, model, [
        {"provider_id": first_provider, "model": "gpt-4o-mini", "enabled": True,
         "priority": 1, "target_retry_budget": 0, "target_cooldown_ms": 0},
        {"provider_id": second_provider, "model": "gpt-4o-mini", "enabled": True,
         "priority": 0, "target_retry_budget": 0, "target_cooldown_ms": 0},
    ])
    status, body = _proxy(env, key, model, [{"role": "user", "content": "recovered"}])
    assert status == 200, body
    clear(env)
    overview = stats(env)
    assert overview["total_requests"] == 1
    # 第一个 Target 的上游错误被切换吸收：客户端只看到一次成功请求。
    assert_split(overview, terminal=1, failed=0, upstream=2, errors=1)
    rows = {row["provider_id"]: row for row in stats(env, "providers")}
    assert set(rows) == {first_provider, second_provider}, rows
    assert rows[first_provider]["upstream_request_count"] == 1
    assert rows[first_provider]["upstream_error_count"] == 1, rows
    assert rows[second_provider]["upstream_request_count"] == 1
    assert rows[second_provider]["upstream_error_count"] == 0


def test_waiting_client_counts_as_terminal_success(usage_env):
    env = usage_env
    model = "usage-waiting-client"
    _route, key = _create_route(env, model, retry_budget=0)
    status, body = _proxy(
        env, key, model, [{"role": "user", "content": "tool-call"}],
        body_extra={"tools": TOOLS},
    )
    assert status == 200, body
    assistant = body["choices"][0]["message"]
    assert assistant.get("tool_calls"), body

    def waiting_stats():
        current = stats(env)
        return current if current["terminal_request_count"] == 1 else None

    waiting = _wait_for("waiting_client terminal stats", waiting_stats)
    assert waiting["total_requests"] == 1
    # waiting_client 是正常完成的 Agent Turn，不是失败请求。
    assert_split(waiting, terminal=1, failed=0, upstream=1, errors=0)

    status, body = _proxy(
        env, key, model,
        [
            {"role": "user", "content": "tool-call"},
            assistant,
            {"role": "tool", "tool_call_id": assistant["tool_calls"][0]["id"], "content": "tool result"},
        ],
        body_extra={"tools": TOOLS},
    )
    assert status == 200, body

    def continued_stats():
        current = stats(env)
        return current if current["terminal_request_count"] == 2 else None

    after = _wait_for("superseded parent counted once", continued_stats)
    assert after["total_requests"] == 2
    # 父 Run 从 waiting_client 翻为 superseded 后仍只计一次，续接 Run 另计一次。
    assert_split(after, terminal=2, failed=0, upstream=2, errors=0)


def test_switch_then_tool_call_and_supersede_are_terminal_successes(usage_env):
    env = usage_env
    model = "usage-switch-tool-call"
    first_provider = provider(env, f"{model}-first", env["mock"])
    second_provider = provider(env, f"{model}-second", env["mock"])
    _route, key = route_with_targets(env, model, [
        {"provider_id": first_provider, "model": "gpt-4o-mini", "enabled": True,
         "priority": 1, "target_retry_budget": 0, "target_cooldown_ms": 0},
        {"provider_id": second_provider, "model": "gpt-4o-mini", "enabled": True,
         "priority": 0, "target_retry_budget": 0, "target_cooldown_ms": 0},
    ])
    status, body = _proxy(
        env, key, model, [{"role": "user", "content": "tool-call-recovered"}],
        body_extra={"tools": TOOLS},
    )
    assert status == 200, body
    assistant = body["choices"][0]["message"]
    assert assistant.get("tool_calls"), body

    def waiting_stats():
        current = stats(env)
        return current if current["terminal_request_count"] == 1 else None

    waiting = _wait_for("switch to waiting_client", waiting_stats)
    assert waiting["total_requests"] == 1
    # 第一个 Target 的 503 被切换吸收；第二个 Target 的客户端工具交接仍是成功终态。
    assert_split(waiting, terminal=1, failed=0, upstream=2, errors=1)
    rows = {row["provider_id"]: row for row in stats(env, "providers")}
    assert rows[first_provider]["upstream_request_count"] == 1
    assert rows[first_provider]["upstream_error_count"] == 1
    assert rows[second_provider]["upstream_request_count"] == 1
    assert rows[second_provider]["upstream_error_count"] == 0

    status, body = _proxy(
        env, key, model,
        [
            {"role": "user", "content": "tool-call-recovered"},
            assistant,
            {"role": "tool", "tool_call_id": assistant["tool_calls"][0]["id"], "content": "tool result"},
        ],
        body_extra={"tools": TOOLS},
    )
    assert status == 200, body

    def continued_stats():
        current = stats(env)
        return current if current["terminal_request_count"] == 2 else None

    after = _wait_for("superseded switch parent counted once", continued_stats)
    assert after["total_requests"] == 2
    # 续接 Run 再发一次成功上游请求；父 Run 翻为 superseded 不重复计数。
    assert_split(after, terminal=2, failed=0, upstream=3, errors=1)


def test_all_targets_failure_counts_each_upstream_error(usage_env):
    env = usage_env
    model = "usage-all-targets-failed"
    first_provider = provider(env, f"{model}-first", env["mock"])
    second_provider = provider(env, f"{model}-second", env["mock"])
    _route, key = route_with_targets(env, model, [
        {"provider_id": first_provider, "model": "gpt-4o-mini", "enabled": True,
         "priority": 0, "target_retry_budget": 0, "target_cooldown_ms": 0},
        {"provider_id": second_provider, "model": "gpt-4o-mini", "enabled": True,
         "priority": 1, "target_retry_budget": 0, "target_cooldown_ms": 0},
    ])
    status, body = _proxy(env, key, model, [{"role": "user", "content": "failed"}])
    assert status >= 500, body
    clear(env)
    overview = stats(env)
    assert overview["total_requests"] == 1
    assert_split(overview, terminal=1, failed=1, upstream=2, errors=2)
    rows = {row["provider_id"]: row for row in stats(env, "providers")}
    assert set(rows) == {first_provider, second_provider}, rows
    for identity in (first_provider, second_provider):
        assert rows[identity]["upstream_request_count"] == 1
        assert rows[identity]["upstream_error_count"] == 1


def test_rejected_requests_stay_out_of_availability(usage_env):
    env = usage_env
    _route, key = _create_route(env, "usage-rejected")
    _create_route(env, "usage-rejected-other")

    # 非法 JSON 与错误的 Key 在准入前被拒绝。
    invalid = Request(
        f"{env['proxy']}/v1/chat/completions",
        data=b'{"model":"broken",',
        headers={"authorization": f"Bearer {key}", "content-type": "application/json"},
        method="POST",
    )
    with pytest.raises(HTTPError) as invalid_json:
        urlopen(invalid, timeout=15)
    assert invalid_json.value.code == 400
    status, _ = http_request(
        "POST", f"{env['proxy']}/v1/chat/completions",
        payload={"model": "usage-rejected", "messages": [{"role": "user", "content": "bad key"}]},
        headers={"authorization": "Bearer not-a-key"},
    )
    assert status == 401

    # 模型不存在与无权使用模型已准入，但在 Target 选择前终止。
    status, _ = _proxy(env, key, "usage-rejected-missing", [{"role": "user", "content": "missing"}])
    assert status == 404
    status, _ = _proxy(env, key, "usage-rejected-other", [{"role": "user", "content": "forbidden"}])
    assert status == 403

    # 请求 Hook 拒绝的错误码由 Hook 指定，分类依赖显式标记而不是错误码。
    status, hook = http_request(
        "POST", f"{env['proxy']}/v1/responses",
        payload={
            "model": "usage-rejected",
            "input": [{"role": "user", "content": "search the web"}],
            "tools": [{
                "type": "web_search",
                "filters": {"allowed_domains": [f"domain-{index}.example" for index in range(21)]},
            }],
        },
        headers={"authorization": f"Bearer {key}"},
    )
    assert status == 400, hook

    clear(env)
    overview = stats(env)
    assert overview["total_requests"] == 5
    assert_split(overview, terminal=0, failed=0, upstream=0, errors=0)
    series = stats(env, "series")
    assert sum(row["request_count"] for row in series) == 5
    for field in SPLIT_COUNTERS:
        assert sum(row[field] for row in series) == 0
    assert all("error_count" not in row for row in series)
    # 错误的请求没有 Target 尝试，因此不会伪造供应商行。
    assert stats(env, "providers") == []


def test_local_request_validation_does_not_reduce_availability(usage_env):
    env = usage_env
    _route, key = _create_route(env, "usage-invalid-input", retry_budget=0)
    status, body = _proxy(env, key, "usage-invalid-input", [{"role": "user", "content": "known"}])
    assert status == 200, body
    status, body = _proxy(env, key, "usage-invalid-input", [{"role": "user", "content": "invalid-parameter"}])
    assert status == 400, body
    anthropic = provider(env, "usage-local-encoding", env["mock"], protocol="anthropic-messages")
    _, encoding_key = route_with_targets(env, "usage-local-encoding", [{
        "provider_id": anthropic, "model": "gpt-4o-mini",
    }])
    status, body = _proxy(env, encoding_key, "usage-local-encoding", [
        {"role": "user", "content": "invalid tool choice"},
    ], body_extra={"tools": TOOLS, "tool_choice": {"type": "function", "function": {"name": ""}}})
    assert status == 400, body
    assert env["attempts"] == {"known": 1, "invalid-parameter": 1}

    def client_errors():
        status, body = http_request("GET", f"{env['admin']}/api/v1/observations/client-errors", headers=env["auth"])
        assert status == 200, body
        return body["data"]["items"] if body["data"]["total"] == 2 else None

    errors = _wait_for("both rejected and failed request observations", client_errors)
    assert next(row for row in errors if row["error"]["code"] == "vendor_request_invalid")["type"] == "rejected"
    assert sorted(row["type"] for row in errors) == ["failed", "rejected"]
    clear(env)
    overview = stats(env)
    assert overview["total_requests"] == 3
    # 同为 400：上游拒绝计为失败，本地参数拦截不进可用率的分子或分母。
    assert_split(overview, terminal=2, failed=1, upstream=2, errors=1)
    series = stats(env, "series")
    assert sum(row["request_count"] for row in series) == 3
    for field, expected in zip(SPLIT_COUNTERS, (2, 1, 2, 1)):
        assert sum(row[field] for row in series) == expected


def test_no_eligible_target_and_quota_suspension_fail_without_upstream_attempts(usage_env):
    env = usage_env
    no_target_model = "usage-no-target"
    _, no_target_key = _create_route(env, no_target_model, retry_budget=0)
    disabled = provider_id(env, f"{no_target_model}-provider")
    status, body = http_request(
        "PUT", f"{env['admin']}/api/v1/providers/{disabled}",
        payload={"is_enabled": False}, headers=env["auth"],
    )
    assert status == 200, body
    status, body = _proxy(env, no_target_key, no_target_model, [{"role": "user", "content": "no target"}])
    assert status >= 500, body

    quota_model = "usage-quota-suspended"
    _, quota_key = _create_route(env, quota_model, retry_budget=0)
    suspend_provider_allowance(env, provider_id(env, f"{quota_model}-provider"))
    status, body = _proxy(env, quota_key, quota_model, [{"role": "user", "content": "quota"}])
    assert status == 429, body

    clear(env)
    overview = stats(env)
    assert overview["total_requests"] == 2
    # 无可用 Target 与额度暂停都是失败的请求，但从未向上游发送尝试。
    assert_split(overview, terminal=2, failed=2, upstream=0, errors=0)
    assert stats(env, "providers") == []


def test_timeout_connection_refused_and_stream_error_are_upstream_errors(usage_env):
    env = usage_env
    timeout_model = "usage-first-token-timeout"
    _, timeout_key = _create_route(
        env, timeout_model, retry_budget=0, first_token_timeout_ms=1_000,
    )
    try:
        status, body = _proxy(env, timeout_key, timeout_model, [{"role": "user", "content": "blocked"}])
    finally:
        env["release"].set()
    assert status >= 500, body

    refused_model = "usage-connection-refused"
    with socket.socket() as unavailable:
        unavailable.bind(("127.0.0.1", 0))
        refused_env = {**env, "mock": f"http://127.0.0.1:{unavailable.getsockname()[1]}"}
        _, refused_key = _create_route(refused_env, refused_model, retry_budget=0)
        status, body = _proxy(env, refused_key, refused_model, [{"role": "user", "content": "refused"}])
    assert status >= 500, body

    stream_model = "usage-stream-error"
    _, stream_key = _create_route(env, stream_model, retry_budget=0)
    status, body = _proxy(
        env, stream_key, stream_model, [{"role": "user", "content": "stream-error"}],
        body_extra={"stream": True},
    )
    assert status == 200, body
    assert "partial" in body, body

    clear(env)
    overview = stats(env)
    assert overview["total_requests"] == 3
    assert_split(overview, terminal=3, failed=3, upstream=3, errors=3)
    rows = stats(env, "providers")
    assert len(rows) == 3, rows
    for row in rows:
        assert row["upstream_request_count"] == 1, row
        assert row["upstream_error_count"] == 1, row


def test_in_progress_request_stays_out_of_availability(usage_env):
    env = usage_env
    _route, key = _create_route(env, "usage-in-flight")
    with ThreadPoolExecutor(max_workers=1) as executor:
        pending = executor.submit(
            _proxy, env, key, "usage-in-flight", [{"role": "user", "content": "blocked"}],
        )
        assert env["entered"].wait(10), "inflight request did not reach upstream"
        try:
            def in_flight_stats():
                current = stats(env)
                return current if current["total_requests"] == 1 else None

            in_flight = _wait_for("in-flight request counters", in_flight_stats)
            assert_split(in_flight, terminal=0, failed=0, upstream=0, errors=0)
        finally:
            env["release"].set()
        status, body = pending.result(timeout=15)
        assert status == 200, body
    clear(env)
    overview = stats(env)
    assert_split(overview, terminal=1, failed=0, upstream=1, errors=0)


def test_capability_upstream_failure_counts_as_upstream_error(usage_env):
    env = usage_env
    vision_model = "usage-capability-vision"
    vision_name = "usage-capability-vision-provider"
    vision_port = find_free_port()
    vision_mock, _ = minimal_mock_provider(vision_port)

    class FailingVision(vision_mock.RequestHandlerClass):
        def do_POST(self) -> None:  # noqa: N802
            self._read_body()
            self._write_json(503, {"error": {"type": "vision_unavailable", "message": "vision upstream down"}})

    vision_mock.RequestHandlerClass = FailingVision
    try:
        vision_provider = provider(
            env, vision_name, f"http://127.0.0.1:{vision_port}",
            input_modalities=("text", "image"), reasoning_efforts=("medium",),
        )
        status, body = http_request(
            "POST", f"{env['admin']}/api/v1/models",
            payload={
                "model_id": vision_model, "output_modality": "text",
                "targets": [{
                    "provider_id": vision_provider, "model": "gpt-4o-mini",
                    "first_token_timeout_ms": 120_000, "target_retry_budget": 0,
                    "target_cooldown_ms": 0,
                }],
            },
            headers=env["auth"],
        )
        assert status == 200, body
        vision_route = body["data"]["id"]
        status, body = http_request(
            "PUT", f"{env['admin']}/api/v1/media-understanding/image",
            payload={"enabled": True, "model_id": vision_route, "thinking_level": "medium"},
            headers=env["auth"],
        )
        assert status == 200, body
        status, body = http_request(
            "POST", f"{env['admin']}/api/v1/api-keys",
            payload={
                "name": "usage-capability-key", "model_ids": [vision_route],
                "mcp_access_enabled": True, "transparent_injection_enabled": True,
                "inject_media_understanding": True,
            },
            headers=env["auth"],
        )
        assert status == 200, body
        key = body["data"]["key"]
        auth = {"Authorization": f"Bearer {key}"}
        image = base64.b64decode(PNG)
        status, upload = http_request(
            "POST", f"{env['proxy']}/v1/artifacts/uploads",
            {"mime_type": "image/png", "size": len(image)}, auth,
        )
        assert status == 201, upload
        connection = HTTPConnection("127.0.0.1", int(env["proxy"].rsplit(":", 1)[1]), timeout=20)
        try:
            connection.request(
                "PUT", f"/v1/artifacts/uploads/{upload['upload_id']}/parts/1", image,
                {**auth, "x-upload-token": upload["upload_token"]},
            )
            response = connection.getresponse()
            assert response.status == 200
            part = json.loads(response.read())
        finally:
            connection.close()
        status, artifact = http_request(
            "POST", f"{env['proxy']}/v1/artifacts/uploads/{upload['upload_id']}/complete",
            {"upload_token": upload["upload_token"], "parts": [part]}, auth,
        )
        assert status == 200 and artifact["mime_type"] == "image/png", artifact
        # 媒体理解由 MCP 读取同步触发；它经 Provider 发出的尝试属于 capability。
        status, queried = http_request(
            "POST", f"{env['proxy']}/mcp",
            {
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {
                    "name": "StraviaRead", "arguments": {"path": artifact["path"]},
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities": {},
                    },
                },
            },
            {
                **auth, "Accept": "application/json, text/event-stream",
                "MCP-Protocol-Version": "2026-07-28",
                "Mcp-Method": "tools/call", "Mcp-Name": "StraviaRead",
            },
            timeout=60,
        )
        assert status == 200, queried

        clear(env)
        overview = stats(env)
        # MCP 读取是独立运行：失败计入失败的请求，capability 尝试计入上游口径。
        assert overview["total_requests"] == 1
        assert_split(overview, terminal=1, failed=1, upstream=1, errors=1)
        rows = {row["provider_id"]: row for row in stats(env, "providers")}
        assert rows[vision_provider]["upstream_request_count"] == 1
        assert rows[vision_provider]["upstream_error_count"] == 1
    finally:
        vision_mock.shutdown()
        vision_mock.server_close()


def test_rpm_unsent_request_contributes_no_upstream_attempt(usage_env):
    env = usage_env
    model = "usage-rpm-unsent"
    route_id, key = _create_route(env, model, retry_budget=0)
    status, body = http_request("GET", f"{env['admin']}/api/v1/models/{model}", headers=env["auth"])
    assert status == 200, body
    target = body["data"]["targets"][0]
    status, body = http_request("GET", f"{env['admin']}/api/v1/settings/rpm_admission", headers=env["auth"])
    assert status == 200, body
    config = json.loads(body["data"])
    config.update({"preferred_wait_ms": 0, "total_wait_ms": 0})
    config["destinations"] = [{
        "provider_id": target["provider_id"], "model": target["model"], "rpm_limit": 1,
    }]
    status, body = http_request(
        "PUT", f"{env['admin']}/api/v1/settings/rpm_admission",
        payload={"value": json.dumps(config)}, headers=env["auth"],
    )
    assert status == 200, body

    status, body = _proxy(env, key, model, [{"role": "user", "content": "known"}])
    assert status == 200, body
    status, body = _proxy(env, key, model, [{"role": "user", "content": "known"}])
    assert status == 429, body

    clear(env)
    overview = stats(env)
    assert overview["total_requests"] == 2
    # 第二次请求在 RPM 准入处失败，从未发送，因而不进入上游请求或上游错误。
    assert_split(overview, terminal=2, failed=1, upstream=1, errors=0)
    row = stats(env, "providers")[0]
    assert row["upstream_request_count"] == 1
    assert row["upstream_error_count"] == 0


def test_split_counters_survive_retention_expiry_and_clear(usage_env):
    env = usage_env
    model = "usage-retention-split"
    _, key = _create_route(env, model, retry_budget=0)
    status, body = _proxy(env, key, model, [{"role": "user", "content": "known"}])
    assert status == 200, body
    status, body = _proxy(env, key, model, [{"role": "user", "content": "failed"}])
    assert status >= 500, body
    def ready_counters():
        current = stats(env)
        return current if current["terminal_request_count"] == 2 else None

    before = _wait_for("split counters before observation expiry", ready_counters)
    assert before["total_requests"] == 2
    assert_split(before, terminal=2, failed=1, upstream=2, errors=1)

    age_observations(env)
    set_retention_days(env, 1)
    for view in ("client-errors", "upstream-errors"):
        status, body = http_request("GET", f"{env['admin']}/api/v1/observations/{view}", headers=env["auth"])
        assert status == 200, body
        assert body["data"]["total"] == 0, (view, body)
    assert stats(env) == before

    clear(env)
    assert stats(env) == before
    set_retention_days(env, 7)


def test_coarsened_series_sums_split_counters_without_error_count(usage_env):
    env = usage_env
    model = "usage-coarse-split"
    _, key = _create_route(env, model, retry_budget=0)
    status, body = _proxy(env, key, model, [{"role": "user", "content": "known"}])
    assert status == 200, body
    status, body = _proxy(env, key, model, [{"role": "user", "content": "failed"}])
    assert status >= 500, body
    clear(env)

    end = int(time.time() * 1000) // 900000 * 900000 + 900000
    query = {"start_at": end - 900000 * 2048, "end_at": end}
    overview = stats(env, **query)
    series = stats(env, "series", bucket=900, **query)
    assert 1 <= len(series) <= 512
    assert overview["total_requests"] == 2
    assert_split(overview, terminal=2, failed=1, upstream=2, errors=1)
    assert "error_count" not in overview
    for field in SPLIT_COUNTERS:
        assert sum(row[field] for row in series) == overview[field]
        assert all(field in row for row in series)
    assert all("error_count" not in row for row in series)
    rows = stats(env, "providers", **query)
    assert len(rows) == 1
    assert rows[0]["upstream_request_count"] == 2
    assert rows[0]["upstream_error_count"] == 1
    assert "request_count" not in rows[0] and "error_count" not in rows[0]
