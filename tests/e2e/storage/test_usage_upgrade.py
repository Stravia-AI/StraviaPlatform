from __future__ import annotations

import json
import os
import sqlite3
import struct
import subprocess
from contextlib import closing
from pathlib import Path
from typing import Any
from urllib.parse import urlencode

import pytest

from tests.common.helpers import (
    WebSession, find_free_port, http_request, initialize_server, start_stravia_server,
    stop_stravia_server, wait_for_setup_token, wait_until_ready,
)
from tests.e2e.storage.conftest import REPO_ROOT, storage_harness_binary
from tests.e2e.storage.test_usage_statistics import clear, stats

pytestmark = [pytest.mark.e2e, pytest.mark.storage]

WINDOW = {"start_at": 900000, "end_at": 3600000}
VIEWS = ("overview", "series", "models", "providers", "api-keys")


def historical_money(view):
    # 升级前没有冻结金额，不能由新价格补算，也不计入已知或未知覆盖率。
    amount = {"amount": "0", "known_count": 0, "unknown_count": 0}
    currency = {"kind": "usd", "symbol": "$"}
    if view in ("models", "api-keys"):
        return {"cost": amount, "billing": amount, "currency": currency}
    if view == "providers":
        # 没有受追踪尝试也没有订阅段：成本不展示。
        return {"cost": None, "subscription": None, "currency": currency}
    return {"actual_cost": amount, "subscription": None, "currency": currency}


def overview(requests=0, terminal=0, failed=0, upstream=0, upstream_errors=0,
             input_tokens=None, output_tokens=None, cache_read=None,
             cache_write=None, reasoning=None, duration=None, tps=None):
    return {
        **historical_money("overview"),
        "total_requests": requests,
        "terminal_request_count": terminal, "failed_request_count": failed,
        "upstream_request_count": upstream, "upstream_error_count": upstream_errors,
        "total_input_tokens": input_tokens, "total_output_tokens": output_tokens,
        "total_cache_read_tokens": cache_read, "total_cache_write_tokens": cache_write,
        "total_reasoning_tokens": reasoning, "avg_duration_ms": duration,
        "avg_first_token_ms": None, "avg_output_tps": tps,
    }


def series_bucket(start, **values):
    result = overview(**values)
    result["request_count"] = result.pop("total_requests")
    # series 没有订阅价值字段；它只保留 actual_cost。
    result.pop("subscription", None)
    return {"bucket_start": start, "bucket_ms": 900000, **result}


def snapshot(env):
    return {view: stats(env, view, **WINDOW, **({"bucket": 900} if view == "series" else {}))
            for view in VIEWS}


def assert_no_legacy_error_count(actual):
    assert "error_count" not in actual["overview"]
    assert all("error_count" not in bucket for bucket in actual["series"])
    assert all("error_count" not in provider for provider in actual["providers"])


def assert_retained_contract(env, actual):
    # One completed client request, two model turns, and one admission failure.
    # Known output zero is not erased by the other wholly unknown attempt.
    assert actual["overview"] == overview(2, 1, 0, 2, 0, 8, 0, 2, duration=200)
    assert actual["series"] == [
        series_bucket(900000, requests=2, terminal=1, upstream=0),
        series_bucket(1800000, upstream=1, input_tokens=8, output_tokens=0, cache_read=2, duration=200, tps=0),
        series_bucket(2700000, upstream=1, duration=200),
    ]
    assert actual["models"] == [{
        **historical_money("models"),
        "model_id": "deleted-model", "deleted": True, "model": "Old Model",
        "request_count": 2, "total_input_tokens": 8, "total_output_tokens": 0,
        "total_reasoning_tokens": None, "avg_duration_ms": 200,
    }]
    assert actual["providers"] == [{
        **historical_money("providers"),
        "provider_id": "deleted-provider", "deleted": True, "provider": "Old Provider",
        "upstream_request_count": 2, "upstream_error_count": 0, "avg_duration_ms": 100,
        "avg_output_tps": None,
    }]
    assert actual["api-keys"] == [{
        **historical_money("api-keys"),
        "api_key_id": "deleted-key", "deleted": True, "api_key_name": "Old Key",
        "request_count": 2, "total_input_tokens": 8, "total_output_tokens": 0,
        "cache_read_tokens": 2, "cache_write_tokens": None, "reasoning_tokens": None,
        "last_used_at": 2700000,
    }]
    assert_no_legacy_error_count(actual)
    # Adjacent absolute windows isolate known zero from unknown usage. Request
    # counts stay in the earlier client-start bucket, not the turn-start buckets.
    assert stats(env, start_at=1800000, end_at=2700000) == overview(
        upstream=1, input_tokens=8, output_tokens=0, cache_read=2, duration=200, tps=0)
    assert stats(env, start_at=2700000, end_at=3600000) == overview(upstream=1, duration=200)
    assert stats(env) == actual["overview"]


@pytest.mark.parametrize("backend", ["sqlite", "postgres"], ids=["sqlite", "postgres"])
@pytest.mark.parametrize("fixture", ["empty", "retained"], ids=["empty0017", "retained0017"])
def test_supported0017_real_server_upgrade_retains_usage_without_duplicate_backfill(
    stravia_binary: Path, storage_runtime: dict[str, Any], tmp_path: Path,
    backend: str, fixture: str,
):
    """The preparer installs genuine old migrations; only the actual host upgrades.

    Existing Core interrupted-transaction tests complement this HTTP cutover
    acceptance; this test does not claim to inject host migration interruption.
    """
    schema = None
    process = None
    logs = []
    database: dict[str, Any] = {"backend": "sqlite"}
    prepare_env = os.environ.copy()
    prepare_env["STRAVIA_STORAGE_USAGE_UPGRADE_FIXTURE"] = fixture
    prepare_env["STRAVIA_STORAGE_BACKEND"] = backend
    try:
        if backend == "postgres":
            pg_url = storage_runtime["pg_url"]
            if not pg_url:
                pytest.fail("PostgreSQL upgrade HTTP acceptance requires isolated DB_URL")
            schema = storage_runtime["make_isolated_schema"]("stravia_usage_upgrade_http")
            storage_runtime["run_schema_action"]("create", pg_url=pg_url, schema=schema)
            dsn = storage_runtime["postgres_dsn_for_schema"](pg_url, schema)
            database = {"backend": "postgres", "url": dsn}
            prepare_env["STRAVIA_STORAGE_PG_URL"] = dsn
        else:
            sqlite_path = tmp_path / "db" / "gateway.db"
            sqlite_path.parent.mkdir(parents=True)
            prepare_env["STRAVIA_STORAGE_SQLITE_PATH"] = str(sqlite_path)
        prepared = subprocess.run(
            [str(storage_harness_binary())], env=prepare_env, cwd=REPO_ROOT,
            text=True, encoding="utf-8", capture_output=True, check=False, timeout=120,
        )
        assert prepared.returncode == 0, (prepared.stdout, prepared.stderr)
        assert f"usage_upgrade_fixture={fixture}" in prepared.stdout

        base = f"http://127.0.0.1:{find_free_port()}"
        args = ["--data-dir", str(tmp_path), "--host", "127.0.0.1",
                "--port", base.rsplit(":", 1)[1], *storage_runtime["server_args"](backend)]
        process, logs = start_stravia_server(stravia_binary=stravia_binary, args=args)
        wait_until_ready(f"{base}/api/v1/auth/state")
        session = initialize_server(base, wait_for_setup_token(logs, process), database)
        env = {"admin": base, "auth": session.auth_headers()}
        before = snapshot(env)
        if fixture == "retained":
            assert_retained_contract(env, before)
        else:
            assert before["overview"] == overview()
            assert before["models"] == before["providers"] == before["api-keys"] == []
            assert before["series"] == []

        # All identities came from retained observations, not live configuration.
        for resource, identity, error in [
            ("models", "deleted-model", "Route not found: deleted-model"),
            ("providers", "deleted-provider", "provider not found: deleted-provider"),
        ]:
            status, body = http_request("GET", f"{base}/api/v1/{resource}/{identity}", headers=env["auth"])
            assert status == 200, body
            assert body == {"error": error}
        if fixture == "retained":
            status, body = http_request(
                "GET", f"{base}/api/v1/observations/interactions/historical", headers=env["auth"],
            )
            assert status == 200, body
        clear(env)
        status, body = http_request(
            "GET", f"{base}/api/v1/observations/interactions/historical", headers=env["auth"],
        )
        assert status == 404, body
        assert snapshot(env) == before
        stop_stravia_server(process, logs)
        process = None
        process, logs = start_stravia_server(stravia_binary=stravia_binary, args=args)
        wait_until_ready(f"{base}/api/v1/auth/state")
        session = WebSession(base)
        status, body = session.request("POST", "/api/v1/auth/login", {
            "username": "admin", "password": "correct horse battery staple",
        })
        assert status == 200, body
        env["auth"] = session.auth_headers()
        assert snapshot(env) == before
        clear(env)
        assert snapshot(env) == before
    finally:
        if process is not None:
            stop_stravia_server(process, logs)
        if schema is not None:
            storage_runtime["run_schema_action"]("drop", pg_url=storage_runtime["pg_url"], schema=schema)


def _encode_event_payload(payload: dict[str, Any]) -> bytes:
    # 与 storage_codec 的 RAW 帧一致：正文 + 编码标记 + 字典槽 + 原始长度 + 版本。
    body = json.dumps(payload, separators=(",", ":")).encode()
    return body + b"\x00" + struct.pack("<I", 0) + struct.pack("<Q", len(body)) + b"\x01"


CLASSIFICATION_FIXTURE_SQL = [
    """INSERT INTO interaction_observations(id,principal,api_key_id,api_key_name,root_id,root_run_id,first_route_id,status,started_at,last_active_at,expires_at) VALUES
       ('retained','owner','key-1','Key One','retained','completed-run','route-x','completed',900000,2700300,4102444800000),
       ('expired','owner','key-1','Key One','expired','expired-run','route-x','completed',4500000,4500300,1)""",
    """INSERT INTO inference_run_observations(id,interaction_id,ingress_protocol,route_id,model_display_name,request_model,status,debug_enabled,started_at,last_active_at,finished_at,expires_at,failure_json,terminal_reason) VALUES
       ('completed-run','retained','responses','route-x','Model X','route-x','completed',FALSE,900000,900100,900100,4102444800000,NULL,NULL),
       ('rejected-run','retained','responses','route-x','Model X','route-x','failed',FALSE,1800000,1800100,1800100,4102444800000,'{"source":"platform","code":"model_not_found","message":"model is unavailable","status_code":404,"upstream_code":null}','model_not_found'),
       ('upstream-run','retained','responses','route-x','Model X','route-x','failed',FALSE,2700000,2700200,2700200,4102444800000,'{"source":"upstream","code":"upstream_error","message":"upstream exploded","status_code":503,"upstream_code":"server_error"}','upstream_error'),
       ('waiting-run','retained','responses','route-x','Model X','route-x','waiting_client',FALSE,3000000,4102444800000,3000000,4102444800000,NULL,NULL),
       ('superseded-run','retained','responses','route-x','Model X','route-x','superseded',FALSE,3150000,3150000,3150000,4102444800000,NULL,'superseded'),
       ('attempted-rejected-run','retained','responses','route-x','Model X','route-x','failed',FALSE,3300000,3300000,3300000,4102444800000,'{"source":"platform","code":"model_not_found","message":"model is unavailable","status_code":404,"upstream_code":null}','model_not_found'),
       ('expired-run','expired','responses','route-x','Model X','route-x','completed',FALSE,4500000,4500300,4500300,1,NULL,NULL)""",
    """INSERT INTO model_turn_observations(id,run_id,interaction_id,route_id,model_display_name,api_key_id,api_key_name,status,started_at,finished_at,last_event_sequence) VALUES
       ('completed-turn','completed-run','retained','route-x','Model X','key-1','Key One','completed',900000,900100,1),
       ('upstream-turn','upstream-run','retained','route-x','Model X','key-1','Key One','failed',2700000,2700200,3),
       ('attempted-turn','attempted-rejected-run','retained','route-x','Model X','key-1','Key One','failed',3300000,3300000,7),
       ('expired-turn','expired-run','expired','route-x','Model X','key-1','Key One','completed',4500000,4500300,5)""",
    """INSERT INTO target_attempt_observations(id,model_turn_id,run_id,interaction_id,target_id,provider_id,provider_name,upstream_model,protocol,status,status_code,error_code,started_at,finished_at,duration_ms,first_token_ms,input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,reasoning_tokens,usage_recorded,last_event_sequence) VALUES
       ('completed-attempt','completed-turn','completed-run','retained','target-1','provider-1','Provider One','upstream-1','responses','completed',200,NULL,900000,900100,100,NULL,10,4,2,1,0,FALSE,1),
       ('upstream-attempt','upstream-turn','upstream-run','retained','target-1','provider-1','Provider One','upstream-1','responses','failed',503,'upstream_error',2700000,2700200,200,NULL,NULL,NULL,NULL,NULL,NULL,FALSE,3),
       ('attempted-local-attempt','attempted-turn','attempted-rejected-run','retained','target-1','provider-1','Provider One','upstream-1','responses','failed',400,'invalid_request',3300000,3300000,0,NULL,NULL,NULL,NULL,NULL,NULL,FALSE,7),
       ('expired-attempt','expired-turn','expired-run','expired','target-1','provider-1','Provider One','upstream-1','responses','completed',200,NULL,4500000,4500300,300,NULL,7,3,1,NULL,NULL,FALSE,5)""",
    """INSERT INTO rejected_request_observations(id,occurred_at,method,path,ingress_protocol,stage,code,status_code,debug_enabled,last_event_sequence,expires_at) VALUES
       ('retained-rejection',2700000,'POST','/v1/responses','responses','admission','invalid_request',400,FALSE,4,4102444800000),
       ('expired-rejection',4500000,'POST','/v1/responses','responses','admission','invalid_request',400,FALSE,6,1)""",
]

UPSTREAM_ATTEMPT_EVENT = {
    "kind": "target_attempt_finished",
    "model_turn_id": "upstream-turn",
    "attempt_id": "upstream-attempt",
    "status": "failed",
    "status_code": 503,
    "error_code": "upstream_error",
    "error": {
        "source": "upstream",
        "code": "upstream_error",
        "message": "upstream exploded",
        "status_code": 503,
        "upstream_code": "server_error",
    },
    "duration_ms": 200,
    "first_token_ms": None,
    "usage": None,
}


LOCAL_ATTEMPT_EVENT = {
    "kind": "target_attempt_finished",
    "model_turn_id": "attempted-turn",
    "attempt_id": "attempted-local-attempt",
    "status": "failed",
    "status_code": 400,
    "error_code": "invalid_request",
    "error": {
        "source": "local",
        "code": "invalid_request",
        "message": "local failure",
        "status_code": 400,
        "upstream_code": None,
    },
    "duration_ms": 0,
    "first_token_ms": None,
    "usage": None,
}

CLASSIFICATION_EVENTS = [
    (2700200, "upstream-run", UPSTREAM_ATTEMPT_EVENT),
    (3300000, "attempted-rejected-run", LOCAL_ATTEMPT_EVENT),
]


def _seed_classification_fixture(backend, *, sqlite_path, pg_url, schema):
    events = [
        (occurred_at, run_id, _encode_event_payload(payload))
        for occurred_at, run_id, payload in CLASSIFICATION_EVENTS
    ]
    if backend == "sqlite":
        with closing(sqlite3.connect(sqlite_path)) as connection, connection:
            for statement in CLASSIFICATION_FIXTURE_SQL:
                connection.execute(statement)
            for occurred_at, run_id, payload in events:
                connection.execute(
                    "INSERT INTO observation_events(occurred_at,interaction_id,run_id,kind,payload,expires_at) VALUES (?,?,?,?,?,?)",
                    (occurred_at, "retained", run_id, "target_attempt_finished", payload, 4102444800000),
                )
        return
    import psycopg
    with psycopg.connect(pg_url) as connection:
        connection.execute(f"SET search_path TO {schema}")
        for statement in CLASSIFICATION_FIXTURE_SQL:
            connection.execute(statement)
        for occurred_at, run_id, payload in events:
            connection.execute(
                "INSERT INTO observation_events(occurred_at,interaction_id,run_id,kind,payload,expires_at) VALUES (%s,%s,%s,%s,%s,%s)",
                (occurred_at, "retained", run_id, "target_attempt_finished", payload, 4102444800000),
            )


def _observation_page(env, path, **query):
    status, body = http_request(
        "GET", f"{env['admin']}/api/v1/{path}?{urlencode(query)}", headers=env["auth"],
    )
    assert status == 200, body
    return body["data"]


def assert_classification_contract(env, actual):
    # 保留期内的运行、尝试与拒绝按新口径回填；过期明细对应的桶保持未知（零）。
    # 等待客户端与接替终态按完成类终结计入，但不计失败；带 Target 尝试的
    # model_not_found 仍是失败请求，只有未尝试的同类错误才算准入前拒绝。
    assert actual["overview"] == overview(
        7, 5, 2, 2, 1, 8, 4, 2, 1, 0, duration=100, tps=40)
    assert actual["series"] == [
        series_bucket(900000, requests=1, terminal=1, upstream=1,
                      input_tokens=8, output_tokens=4, cache_read=2, cache_write=1,
                      reasoning=0, duration=100, tps=40),
        series_bucket(1800000, requests=1),
        series_bucket(2700000, requests=5, terminal=4, failed=2, upstream=1,
                      upstream_errors=1, duration=100),
    ]
    assert actual["models"] == [{
        **historical_money("models"),
        "model_id": "route-x", "deleted": True, "model": "Model X",
        "request_count": 3, "total_input_tokens": 8, "total_output_tokens": 4,
        "total_reasoning_tokens": 0, "avg_duration_ms": 100,
    }]
    assert actual["providers"] == [{
        **historical_money("providers"),
        "provider_id": "provider-1", "deleted": True, "provider": "Provider One",
        "upstream_request_count": 2, "upstream_error_count": 1,
        "avg_duration_ms": 100, "avg_output_tps": 40,
    }]
    assert actual["api-keys"] == [{
        **historical_money("api-keys"),
        "api_key_id": "key-1", "deleted": True, "api_key_name": "Key One",
        "request_count": 3, "total_input_tokens": 8, "total_output_tokens": 4,
        "cache_read_tokens": 2, "cache_write_tokens": 1, "reasoning_tokens": 0,
        "last_used_at": 3300000,
    }]
    assert_no_legacy_error_count(actual)
    assert stats(env, start_at=4500000, end_at=5400000) == overview(
        2, input_tokens=6, output_tokens=3, cache_read=1, duration=300, tps=10)
    assert stats(env, "series", start_at=4500000, end_at=5400000, bucket=900) == [
        series_bucket(4500000, requests=2, input_tokens=6, output_tokens=3,
                      cache_read=1, duration=300, tps=10),
    ]
    assert stats(env, "models", start_at=4500000, end_at=5400000) == [{
        **historical_money("models"),
        "model_id": "route-x", "deleted": True, "model": "Model X",
        "request_count": 1, "total_input_tokens": 6, "total_output_tokens": 3,
        "total_reasoning_tokens": None, "avg_duration_ms": 300,
    }]
    assert stats(env, "providers", start_at=4500000, end_at=5400000) == [{
        **historical_money("providers"),
        "provider_id": "provider-1", "deleted": True, "provider": "Provider One",
        "upstream_request_count": 0, "upstream_error_count": 0,
        "avg_duration_ms": 300, "avg_output_tps": 10,
    }]
    assert stats(env, "api-keys", start_at=4500000, end_at=5400000) == [{
        **historical_money("api-keys"),
        "api_key_id": "key-1", "deleted": True, "api_key_name": "Key One",
        "request_count": 1, "total_input_tokens": 6, "total_output_tokens": 3,
        "cache_read_tokens": 1, "cache_write_tokens": None, "reasoning_tokens": None,
        "last_used_at": 4500000,
    }]

    rejected = _observation_page(env, "observations/client-errors", **WINDOW,
                                 type="rejected", limit=100)
    assert [item["id"] for item in rejected["items"]] == ["retained-rejection", "rejected-run"]
    assert all(item["type"] == "rejected" for item in rejected["items"])
    failed = _observation_page(env, "observations/client-errors", **WINDOW,
                               type="failed", limit=100)
    assert [item["id"] for item in failed["items"]] == ["attempted-rejected-run", "upstream-run"]
    assert all(item["type"] == "failed" for item in failed["items"])

    upstream = _observation_page(env, "observations/upstream-errors", **WINDOW, limit=100)
    assert [item["id"] for item in upstream["items"]] == ["upstream-attempt"]
    item = upstream["items"][0]
    assert item["outcome"] == "caused_failure"
    assert item["error"] == {
        "source": "upstream", "code": "upstream_error", "message": "upstream exploded",
        "status_code": 503, "upstream_code": "server_error",
    }
    assert item["provider_id"] == "provider-1"
    assert item["provider_name"] == "Provider One"
    assert item["upstream_model"] == "upstream-1"
    assert item["model"] == "route-x"
    assert item["api_key_id"] == "key-1"
    assert item["duration_ms"] == 200


@pytest.mark.parametrize("backend", ["sqlite", "postgres"], ids=["sqlite", "postgres"])
def test_supported0025_upgrade_backfills_request_classification_counters(
    stravia_binary: Path, storage_runtime: dict[str, Any], tmp_path: Path, backend: str,
):
    """0017 明细升级后：保留期内回填分类与上游计数，过期桶保持零且旧贡献不变。"""
    schema = None
    process = None
    logs = []
    pg_url = None
    database: dict[str, Any] = {"backend": "sqlite"}
    prepare_env = os.environ.copy()
    prepare_env["STRAVIA_STORAGE_USAGE_UPGRADE_FIXTURE"] = "empty"
    prepare_env["STRAVIA_STORAGE_BACKEND"] = backend
    try:
        if backend == "postgres":
            pg_url = storage_runtime["pg_url"]
            if not pg_url:
                pytest.fail("PostgreSQL upgrade HTTP acceptance requires isolated DB_URL")
            schema = storage_runtime["make_isolated_schema"]("stravia_usage_classification")
            storage_runtime["run_schema_action"]("create", pg_url=pg_url, schema=schema)
            dsn = storage_runtime["postgres_dsn_for_schema"](pg_url, schema)
            database = {"backend": "postgres", "url": dsn}
            prepare_env["STRAVIA_STORAGE_PG_URL"] = dsn
            sqlite_path = None
        else:
            sqlite_path = tmp_path / "db" / "gateway.db"
            sqlite_path.parent.mkdir(parents=True)
            prepare_env["STRAVIA_STORAGE_SQLITE_PATH"] = str(sqlite_path)
        prepared = subprocess.run(
            [str(storage_harness_binary())], env=prepare_env, cwd=REPO_ROOT,
            text=True, encoding="utf-8", capture_output=True, check=False, timeout=120,
        )
        assert prepared.returncode == 0, (prepared.stdout, prepared.stderr)
        assert "usage_upgrade_fixture=empty" in prepared.stdout
        _seed_classification_fixture(
            backend, sqlite_path=sqlite_path, pg_url=pg_url, schema=schema,
        )

        base = f"http://127.0.0.1:{find_free_port()}"
        args = ["--data-dir", str(tmp_path), "--host", "127.0.0.1",
                "--port", base.rsplit(":", 1)[1], *storage_runtime["server_args"](backend)]
        process, logs = start_stravia_server(stravia_binary=stravia_binary, args=args)
        wait_until_ready(f"{base}/api/v1/auth/state")
        session = initialize_server(base, wait_for_setup_token(logs, process), database)
        env = {"admin": base, "auth": session.auth_headers()}
        before = snapshot(env)
        assert_classification_contract(env, before)

        # 明细清除只回收检查点；永久统计桶（含新计数）保持不变。
        clear(env)
        assert snapshot(env) == before
        stop_stravia_server(process, logs)
        process = None
        process, logs = start_stravia_server(stravia_binary=stravia_binary, args=args)
        wait_until_ready(f"{base}/api/v1/auth/state")
        session = WebSession(base)
        status, body = session.request("POST", "/api/v1/auth/login", {
            "username": "admin", "password": "correct horse battery staple",
        })
        assert status == 200, body
        env["auth"] = session.auth_headers()
        assert snapshot(env) == before
    finally:
        if process is not None:
            stop_stravia_server(process, logs)
        if schema is not None:
            storage_runtime["run_schema_action"]("drop", pg_url=storage_runtime["pg_url"], schema=schema)
