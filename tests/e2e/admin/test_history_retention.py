"""统一保留期（ADR-0089）：保存 ``log_retention_days`` 立即清扫，后端下限为 1 天。

保留期按“根对象的已有时间列 + N”计算：Interaction 看 ``last_active_at``，Turn 节点看
``created_at``，依赖按首次发布时刻（依赖的回收见 ``test_reversible_redaction.py`` 的过期引用用例）。这里不改写任何到期列，而是把这些时间列回拨到 N 天之外，
再保存一次保留天数，观察 HTTP 可见的结果。
"""

from __future__ import annotations

import sqlite3
import tempfile
import time
from contextlib import closing
from pathlib import Path
from typing import Any

import pytest

from tests.common.helpers import (
    download_observation_bundle, find_free_port, http_request, minimal_mock_provider, stop_stravia_server,
)
from tests.e2e.admin.test_observation_failures import _start_initialized
from tests.e2e.admin.test_observations import _create_route, _detail, _proxy, _wait_for

pytestmark = [pytest.mark.e2e, pytest.mark.admin]

DAY_MS = 86_400_000


def save_retention(env: dict[str, Any], days: str) -> tuple[int, Any]:
    return http_request(
        "PUT", f"{env['admin']}/api/v1/settings/log_retention_days",
        payload={"value": days}, headers=env["auth"],
    )


def gateway_sql(env: dict[str, Any], sql: str, *parameters: Any) -> list[tuple[Any, ...]]:
    with closing(sqlite3.connect(env["data_dir"] / "db" / "gateway.db", timeout=10)) as connection:
        rows = connection.execute(sql, parameters).fetchall()
        connection.commit()
        return rows


def interaction_status(env: dict[str, Any], interaction_id: str) -> int:
    status, _ = http_request(
        "GET", f"{env['admin']}/api/v1/observations/interactions/{interaction_id}", headers=env["auth"],
    )
    return status


def settle(env: dict[str, Any], interaction_id: str) -> None:
    """等 Run 收尾并排空 writer 队列：迟到的事件会把回拨的 ``last_active_at`` 刷回当前时间。"""
    detail = _wait_for(
        "interaction finished",
        lambda: (lambda d: d if any(e["kind"] == "run_finished" for r in d["runs"] for e in r["events"]) else None)(
            _detail(env, interaction_id)
        ),
    )
    download_observation_bundle(env, detail)


@pytest.fixture
def isolated(stravia_binary: Path):
    """独立服务端：清扫只作用于本测试的数据。"""
    mock_port = find_free_port()
    mock_server, _ = minimal_mock_provider(mock_port)
    with tempfile.TemporaryDirectory(prefix="stravia-retention-e2e-") as temporary:
        env, process, logs = _start_initialized(stravia_binary, Path(temporary), f"http://127.0.0.1:{mock_port}")
        try:
            yield env
        finally:
            stop_stravia_server(process, logs)
            mock_server.shutdown()
            mock_server.server_close()


def test_retention_days_below_one_is_rejected(isolated: dict[str, Any]) -> None:
    env = isolated
    status, before = http_request("GET", f"{env['admin']}/api/v1/settings/log_retention_days", headers=env["auth"])
    assert status == 200 and before["data"] is None, before  # 未保存时使用默认的 7 天
    for invalid in ("0", "-1", "0.5", "seven", ""):
        # 设置接口沿用管理面约定：校验失败返回带 ``error`` 的响应体，且不落库。
        status, body = save_retention(env, invalid)
        assert status == 200 and "error" in body and "data" not in body, (invalid, status, body)
        if invalid == "0":
            assert body["error"] == "log_retention_days must be at least 1", body
    status, after = http_request("GET", f"{env['admin']}/api/v1/settings/log_retention_days", headers=env["auth"])
    assert after["data"] is None, "a rejected value must not be stored"
    status, body = save_retention(env, "1")
    assert status == 200, body
    status, stored = http_request("GET", f"{env['admin']}/api/v1/settings/log_retention_days", headers=env["auth"])
    assert stored["data"] == "1"


def test_saving_retention_days_sweeps_aged_history_immediately(isolated: dict[str, Any]) -> None:
    env = isolated
    route_id, key = _create_route(env, "retention-sweep")
    status, aged = _proxy(env, key, "retention-sweep", [{"role": "user", "content": "aged conversation"}])
    assert status == 200, aged
    status, fresh = _proxy(env, key, "retention-sweep", [{"role": "user", "content": "unrelated fresh conversation"}])
    assert status == 200, fresh
    interactions = _wait_for(
        "both interactions persisted",
        lambda: (lambda rows: rows if len(rows) == 2 else None)(
            gateway_sql(env, "SELECT id, last_active_at FROM interaction_observations ORDER BY started_at")
        ),
    )
    aged_id, fresh_id = interactions[0][0], interactions[1][0]
    settle(env, aged_id)
    settle(env, fresh_id)
    # 没有 API Key 的请求在准入阶段被拒绝，留下一条拒绝记录。
    status, rejection = http_request(
        "POST", f"{env['proxy']}/v1/chat/completions",
        payload={"model": "retention-sweep", "messages": [{"role": "user", "content": "no principal"}]},
    )
    assert status == 401, rejection
    rejections = _wait_for(
        "rejected request persisted",
        lambda: gateway_sql(env, "SELECT id FROM rejected_request_observations") or None,
    )

    # 回拨到 N 天之外（默认 N=7），再保存同一个 N：清扫是保存的同步后果，不依赖定时器。
    old = int(time.time() * 1000) - 40 * DAY_MS
    gateway_sql(env, "UPDATE interaction_observations SET last_active_at = ? WHERE id = ?", old, aged_id)
    gateway_sql(env, "UPDATE inference_run_observations SET last_active_at = ? WHERE interaction_id = ?", old, aged_id)
    gateway_sql(env, "UPDATE rejected_request_observations SET occurred_at = ?", old)
    assert interaction_status(env, aged_id) == 200

    status, body = save_retention(env, "7")
    assert status == 200, body
    assert interaction_status(env, aged_id) == 404, "an aged Interaction is removed by the save itself"
    assert interaction_status(env, fresh_id) == 200, "a recent Interaction is kept"
    assert gateway_sql(env, "SELECT COUNT(*) FROM observation_events WHERE interaction_id = ?", aged_id) == [(0,)]
    assert gateway_sql(env, "SELECT COUNT(*) FROM inference_run_observations WHERE interaction_id = ?", aged_id) == [(0,)]
    assert gateway_sql(env, "SELECT COUNT(*) FROM rejected_request_observations") == [(0,)], rejections


def test_longer_retention_keeps_what_the_shorter_one_would_remove(isolated: dict[str, Any]) -> None:
    env = isolated
    _, key = _create_route(env, "retention-longer")
    status, body = _proxy(env, key, "retention-longer", [{"role": "user", "content": "twenty days old"}])
    assert status == 200, body
    (interaction_id, _), = _wait_for(
        "interaction persisted", lambda: gateway_sql(env, "SELECT id, last_active_at FROM interaction_observations") or None,
    )
    settle(env, interaction_id)
    old = int(time.time() * 1000) - 20 * DAY_MS
    gateway_sql(env, "UPDATE interaction_observations SET last_active_at = ? WHERE id = ?", old, interaction_id)
    gateway_sql(env, "UPDATE inference_run_observations SET last_active_at = ? WHERE interaction_id = ?", old, interaction_id)

    status, body = save_retention(env, "30")
    assert status == 200, body
    assert interaction_status(env, interaction_id) == 200, "20 days old is inside a 30-day window"
    status, body = save_retention(env, "10")
    assert status == 200, body
    assert interaction_status(env, interaction_id) == 404, "shrinking N takes effect on the very next sweep"
