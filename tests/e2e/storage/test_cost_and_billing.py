from __future__ import annotations

import base64
import json
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from decimal import Decimal
from http.client import HTTPConnection
from pathlib import Path
from typing import Any
from urllib.parse import urlencode

import pytest

from tests.common.helpers import (
    WebSession, find_free_port, http_bytes, http_request, initialize_server,
    start_stravia_server, stop_stravia_server, wait_for_setup_token, wait_until_ready,
)
from tests.e2e.admin.test_observations import (
    _client_errors, _detail, _proxy, _route_interactions, _wait_for,
)
from tests.e2e.storage.test_usage_statistics import (  # noqa: F401
    PNG, _run_sql, clear, stats, usage_env, usage_upstream,
)

pytestmark = [pytest.mark.e2e, pytest.mark.storage]


def admin(env: dict[str, Any], method: str, path: str, payload: Any = None) -> Any:
    status, body = http_request(
        method, f"{env['admin']}/api/v1/{path}", payload=payload, headers=env["auth"],
    )
    assert 200 <= status < 300, (status, body)
    assert "error" not in body, body
    if method == "DELETE":
        assert body == {"ok": True}, body
        return None
    return body["data"]


def test_first_currency_converts_limits_once_and_cannot_be_bypassed(usage_env):
    env = usage_env
    key = admin(env, "POST", "api-keys", {"name": "currency-limit", "spend_limit": "5"})
    currency = admin(env, "PUT", "currency", {"symbol": "¥", "usd_rate": "7.2"})
    assert currency["custom"]["first_rate"] == "7.2"
    first_configured_at = currency["custom"]["configured_at"]
    updated = admin(env, "GET", f"api-keys/{key['id']}")
    assert Decimal(updated["spend_limit"]) == Decimal("36")
    assert updated["spend"] == {"spent": "0", "reset_at": None}

    currency = admin(env, "PUT", "currency", {"symbol": "RMB", "usd_rate": "8"})
    assert currency["custom"]["first_rate"] == "7.2"
    assert currency["custom"]["configured_at"] == first_configured_at
    assert currency["custom"]["usd_rate"] == "8"
    assert Decimal(admin(env, "GET", f"api-keys/{key['id']}")["spend_limit"]) == Decimal("36")

    status, body = http_request(
        "PUT", f"{env['admin']}/api/v1/settings/custom_currency",
        payload={"value": None}, headers=env["auth"],
    )
    assert status >= 400 or "error" in body, body
    status, _ = http_request("DELETE", f"{env['admin']}/api/v1/currency", headers=env["auth"])
    assert status in (404, 405)
    assert admin(env, "GET", "currency") == currency


def priced_route(env, name, *, billing=None, spend_limit=None, rpm_limit=None, retry_budget=0,
                 input_modalities=("text",)):
    provider = admin(env, "POST", "providers", {
        "name": f"{name}-provider",
        "source": {"type": "custom", "vendor": "custom", "channel": "default",
                   "protocol": "openai-compatible", "base_url": env["mock"]},
        "credential": {"type": "api_key", "value": "local-upstream-key"},
        "vendor_options": {},
    })
    admin(env, "POST", f"providers/{provider['id']}/models", {
        "model_id": "gpt-4o-mini", "metadata": {
            "name": name, "modalities": {"input": list(input_modalities), "output": ["text"]},
            "reasoning_efforts": ["medium"],
            "cost": {"input": "1", "output": "2", "cache_read": "0.2"},
        },
    })
    route = admin(env, "POST", "models", {
        "model_id": name, "output_modality": "text", "billing": billing,
        "targets": [{"provider_id": provider["id"], "model": "gpt-4o-mini",
                     "target_retry_budget": retry_budget, "target_cooldown_ms": 0}],
    })
    key = admin(env, "POST", "api-keys", {
        "name": f"{name}-key", "model_ids": [route["id"]], "spend_limit": spend_limit,
        "rpm_limit": rpm_limit, "mcp_access_enabled": True,
    })
    return provider, route, key


def call(env, key, route, scenario="known"):
    status, body = _proxy(env, key["key"], route["model_id"], [{"role": "user", "content": scenario}])
    assert status == 200, body
    return body


def wait_spent(env, key, expected):
    def credited():
        current = admin(env, "GET", f"api-keys/{key['id']}")
        return current if Decimal(current["spend"]["spent"]) == Decimal(expected) else None
    return _wait_for(f"charged amount {expected}", credited)


def money(value, amount, known, unknown=0):
    assert Decimal(value["amount"]) == Decimal(amount), value
    assert value["known_count"] == known, value
    assert value["unknown_count"] == unknown, value


def test_frozen_money_survives_rate_changes_reset_and_detail_deletion(usage_env):
    env = usage_env
    provider, route, key = priced_route(
        env, "frozen-money", billing={"mode": "per_call", "amount": "0.02", "currency": "usd"},
    )
    call(env, key, route)
    wait_spent(env, key, "0.02")
    # 15 ordinary inputs + 5 cache reads + 7 outputs => USD 0.00003.
    money(stats(env)["actual_cost"], "0.00003", 1)
    money(stats(env, "models")[0]["billing"], "0.02", 1)
    admin(env, "PUT", "currency", {"symbol": "¥", "usd_rate": "7"})
    wait_spent(env, key, "0.14")
    money(stats(env)["actual_cost"], "0.00021", 1)
    admin(env, "PUT", "currency", {"symbol": "¥", "usd_rate": "8"})
    call(env, key, route, "second")
    wait_spent(env, key, "0.3")
    overview = stats(env)
    assert overview["currency"] == {"kind": "custom", "symbol": "¥"}
    money(overview["actual_cost"], "0.00045", 2)
    for view in ("models", "api-keys"):
        row = stats(env, view)[0]
        money(row["cost"], "0.00045", 2)
        money(row["billing"], "0.3", 2)
    money(stats(env, "providers")[0]["actual_cost"], "0.00045", 2)
    money(stats(env, "series")[0]["actual_cost"], "0.00045", 2)

    reset = admin(env, "POST", f"api-keys/{key['id']}/spend/reset")
    assert reset["spent"] == "0"
    assert reset["reset_at"] is not None
    money(stats(env, "api-keys")[0]["billing"], "0.3", 2)
    clear(env)
    money(stats(env)["actual_cost"], "0.00045", 2)
    admin(env, "DELETE", f"models/{route['model_id']}")
    admin(env, "DELETE", f"providers/{provider['id']}")
    admin(env, "DELETE", f"api-keys/{key['id']}")
    for view in ("models", "api-keys"):
        row = stats(env, view)[0]
        assert row["deleted"] is True
        money(row["cost"], "0.00045", 2)
        money(row["billing"], "0.3", 2)
    assert stats(env, "providers")[0]["deleted"] is True
    money(stats(env)["actual_cost"], "0.00045", 2)


def test_spend_limit_rejects_every_protocol_without_rpm_or_retry_after(usage_env):
    env = usage_env
    _, route, key = priced_route(env, "spend-admission", spend_limit="0.02", rpm_limit=2,
                                billing={"mode": "per_call", "amount": "0.02", "currency": "usd"})
    call(env, key, route)
    wait_spent(env, key, "0.02")
    protocols = [
        ("/v1/chat/completions", {"model": route["model_id"], "messages": [{"role": "user", "content": "blocked"}]},
         {"authorization": f"Bearer {key['key']}"}, "insufficient_quota"),
        ("/v1/responses", {"model": route["model_id"], "input": "blocked"},
         {"authorization": f"Bearer {key['key']}"}, "insufficient_quota"),
        ("/v1/messages", {"model": route["model_id"], "max_tokens": 10, "messages": [{"role": "user", "content": "blocked"}]},
         {"x-api-key": key["key"], "anthropic-version": "2023-06-01"}, "rate_limit_error"),
        (f"/v1beta/models/{route['model_id']}:generateContent", {"contents": [{"role": "user", "parts": [{"text": "blocked"}]}]},
         {"x-goog-api-key": key["key"]}, "RESOURCE_EXHAUSTED"),
    ]
    for path, payload, auth, kind in protocols:
        status, headers, raw = http_bytes("POST", env["proxy"] + path, payload, auth)
        assert status == 429, (path, status, raw)
        assert "retry-after" not in headers
        error = json.loads(raw)["error"]
        assert error.get("type", error.get("status")) == kind, error
        assert error["message"] == "API key spend limit reached."
        if kind == "insufficient_quota":
            assert error["code"] == "api_key_spend_limit_reached"

    status, headers, raw = http_bytes("POST", env["proxy"] + "/mcp", {
        "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
            "name": "StraviaRead", "arguments": {"path": "stravia://artifacts/nonexistent"},
            "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28",
                      "io.modelcontextprotocol/clientCapabilities": {}},
        },
    }, {"authorization": f"Bearer {key['key']}", "accept": "application/json, text/event-stream",
        "MCP-Protocol-Version": "2026-07-28", "Mcp-Method": "tools/call", "Mcp-Name": "StraviaRead"})
    assert status == 429, (status, raw)
    assert "retry-after" not in headers
    assert "spend limit" in raw.decode().lower()
    def rejected():
        rows = _client_errors(env, type="rejected")["items"]
        return rows if len(rows) == 5 else None
    rejections = _wait_for("spend-limit rejected request records", rejected)
    assert all(row["error"]["code"] == "spend_limit" for row in rejections)

    # Four protocol rejections and MCP must not consume the second RPM slot.
    raised = admin(env, "PUT", f"api-keys/{key['id']}", {"spend_limit": "0.04"})
    assert raised["is_enabled"] is True
    call(env, key, route, "after-raise")
    wait_spent(env, key, "0.04")
    admin(env, "PUT", f"api-keys/{key['id']}", {"rpm_limit": None})
    admin(env, "POST", f"api-keys/{key['id']}/spend/reset")
    call(env, key, route, "after-reset")
    assert wait_spent(env, key, "0.02")["is_enabled"] is True


def test_token_billing_unknown_usage_and_peak_rule_coverage(usage_env):
    env = usage_env
    _, route, key = priced_route(env, "token-billing", billing={
        "mode": "per_token", "price": {
            "input": "2", "output": "4", "cache_read": "0.4",
            "peak": {"timezone": "Asia/Shanghai", "weekdays": [1, 2, 3, 4, 5, 6, 7],
                     "ranges": [{"start": "00:00", "end": "24:00"}], "multiplier": "2"},
        },
    })
    call(env, key, route)
    wait_spent(env, key, "0.00012")
    call(env, key, route, "unknown")
    def unknown_accounted():
        row = stats(env, "models")[0]
        return row if row["billing"]["unknown_count"] == 1 else None
    row = _wait_for("unknown token billing coverage", unknown_accounted)
    money(row["cost"], "0.00003", 1, 1)
    money(row["billing"], "0.00012", 1, 1)
    wait_spent(env, key, "0.00012")

    # Per-call charges do not need known token usage.
    admin(env, "PUT", f"models/{route['model_id']}", {
        "billing": {"mode": "per_call", "amount": "0.02", "currency": "usd"},
    })
    call(env, key, route, "unknown")
    wait_spent(env, key, "0.02012")
    money(stats(env, "models")[0]["billing"], "0.02012", 2, 1)


def test_billing_validation_preserves_configuration_and_null_clears_it(usage_env):
    env = usage_env
    _, route, key = priced_route(env, "billing-validation", billing={
        "mode": "per_call", "amount": "0.02", "currency": "usd",
    })
    path = f"models/{route['model_id']}"
    invalid = [
        {"mode": "per_call", "amount": "-1", "currency": "usd"},
        {"mode": "per_call", "amount": "1", "currency": "custom"},
        {"mode": "per_token", "price": {"input": "1", "peak": {
            "timezone": "Asia/Shanghai", "weekdays": [1],
            "ranges": [{"start": "23:00", "end": "01:00"}], "multiplier": "2",
        }}},
    ]
    for billing in invalid:
        status, body = http_request("PUT", f"{env['admin']}/api/v1/{path}",
                                    {"billing": billing}, env["auth"])
        assert status >= 400 or "error" in body, body
        assert admin(env, "GET", path)["billing"] == route["billing"]
    admin(env, "PUT", path, {"display_name": "Updated without billing"})
    call(env, key, route)
    wait_spent(env, key, "0.02")
    assert admin(env, "PUT", path, {"billing": None})["billing"] is None
    call(env, key, route, "after-clear")
    clear(env)
    wait_spent(env, key, "0.02")
    row = stats(env, "models")[0]
    money(row["cost"], "0.00006", 2)
    money(row["billing"], "0.02", 1)


def test_subscription_history_stops_at_delete_and_copy_is_usage(usage_env):
    env = usage_env
    provider, route, key = priced_route(env, "subscription", billing={
        "mode": "per_call", "amount": "0.02", "currency": "usd",
    })
    plan = admin(env, "POST", f"providers/{provider['id']}/cost-plans", {
        "basis": "subscription", "period": "month", "price": {"amount": "2628000", "currency": "usd"},
    })
    assert admin(env, "GET", f"providers/{provider['id']}/cost-plans") == [plan]
    current = admin(env, "GET", f"providers/{provider['id']}")
    assert current["cost_plan"] == plan
    listed = next(row for row in admin(env, "GET", "providers") if row["id"] == provider["id"])
    assert listed["cost_plan"] == plan
    copied = admin(env, "POST", f"providers/{provider['id']}/copy", {})
    assert copied["cost_plan"] is None
    assert admin(env, "GET", f"providers/{copied['id']}/cost-plans") == []
    before_series = time.time_ns() // 1_000_000
    quarter_hour = 900_000
    window = {
        "start_at": plan["effective_from"] // quarter_hour * quarter_hour,
        "end_at": (before_series // quarter_hour + 1) * quarter_hour,
    }
    series = stats(env, "series", **window)
    after_series = time.time_ns() // 1_000_000
    series_amount = sum(Decimal(row["actual_cost"]["amount"]) for row in series)
    assert Decimal(before_series - plan["effective_from"]) / 1000 <= series_amount
    assert series_amount <= Decimal(after_series - plan["effective_from"]) / 1000
    assert all(row["upstream_request_count"] == 0 for row in series)
    call(env, key, route)
    wait_spent(env, key, "0.02")
    row = next(row for row in stats(env, "providers") if row["provider_id"] == provider["id"])
    money(row["usage_equivalent_cost"], "0.00003", 1)
    money(stats(env, "models")[0]["cost"], "0.00003", 1)
    admin(env, "DELETE", f"models/{route['model_id']}")
    before_delete = time.time_ns() // 1_000_000
    admin(env, "DELETE", f"providers/{provider['id']}")
    after_delete = time.time_ns() // 1_000_000
    actual = Decimal(stats(env)["actual_cost"]["amount"])
    # A 365/12-day subscription priced at 2,628,000 USD costs exactly 1 USD/s.
    assert Decimal(before_delete - plan["effective_from"]) / 1000 <= actual
    assert actual <= Decimal(after_delete - plan["effective_from"]) / 1000
    clear(env)
    assert Decimal(stats(env)["actual_cost"]["amount"]) == actual
    deleted = next(row for row in stats(env, "providers") if row["provider_id"] == provider["id"])
    assert deleted["deleted"] is True
    assert Decimal(deleted["actual_cost"]["amount"]) == actual


def test_retry_failure_and_disconnect_never_add_successful_billing(usage_env):
    env = usage_env
    _, route, key = priced_route(env, "terminal-money", retry_budget=1,
                                billing={"mode": "per_call", "amount": "0.02", "currency": "usd"})
    call(env, key, route, "recovered")
    wait_spent(env, key, "0.02")
    status, _ = _proxy(env, key["key"], route["model_id"], [{"role": "user", "content": "failed"}])
    assert status >= 500
    connection = HTTPConnection("127.0.0.1", int(env["proxy"].rsplit(":", 1)[1]), timeout=15)
    try:
        connection.request("POST", "/v1/chat/completions", json.dumps({
            "model": route["model_id"], "messages": [{"role": "user", "content": "cancelled"}], "stream": True,
        }), {"Authorization": f"Bearer {key['key']}", "Content-Type": "application/json"})
        response = connection.getresponse()
        assert response.status == 200
        assert response.readline().startswith(b"data: ")
        assert env["entered"].wait(10)
        response.close()
    finally:
        connection.close()
        env["release"].set()
    def terminal_amounts():
        row = stats(env, "models")[0]
        return row if row["cost"]["unknown_count"] == 4 else None
    row = _wait_for("retry failure and disconnected attempt amounts", terminal_amounts)
    money(row["cost"], "0.00003", 1, 4)
    money(row["billing"], "0.02", 1)
    wait_spent(env, key, "0.02")


def test_deleted_key_does_not_abort_inflight_money_observation(usage_env):
    env = usage_env
    _, route, key = priced_route(env, "deleted-key-money",
                                billing={"mode": "per_call", "amount": "0.02", "currency": "usd"})
    with ThreadPoolExecutor(max_workers=1) as executor:
        pending = executor.submit(call, env, key, route, "blocked")
        assert env["entered"].wait(10)
        try:
            admin(env, "DELETE", f"api-keys/{key['id']}")
        finally:
            env["release"].set()
        pending.result(timeout=15)
    def completed_charge():
        rows = stats(env, "api-keys")
        return next((row for row in rows if row["api_key_id"] == key["id"] and row["billing"]["known_count"] == 1), None)
    row = _wait_for("deleted key retained charge", completed_charge)
    assert row["deleted"] is True
    money(row["billing"], "0.02", 1)
    money(stats(env)["actual_cost"], "0.00003", 1)
    # A failed foreign-key credit would poison observation purge and later writes.
    clear(env)
    fresh = admin(env, "POST", "api-keys", {"name": "following-key", "model_ids": [route["id"]]})
    call(env, fresh, route)
    wait_spent(env, fresh, "0.02")
    money(stats(env)["actual_cost"], "0.00006", 2)


def test_changed_prices_and_billing_do_not_reprice_past_attempts(usage_env):
    env = usage_env
    provider, route, key = priced_route(env, "frozen-prices",
                                      billing={"mode": "per_call", "amount": "0.02", "currency": "usd"})
    call(env, key, route)
    wait_spent(env, key, "0.02")
    detail = admin(env, "GET", f"providers/{provider['id']}/model?" + urlencode({"model": "gpt-4o-mini"}))
    admin(env, "PUT", f"providers/{provider['id']}/model", {
        "model_id": "gpt-4o-mini", "revision": detail["revision"],
        "changes": {"cost.input": {"amount": "10", "currency": "usd"}},
    })
    admin(env, "PUT", f"models/{route['model_id']}", {
        "billing": {"mode": "per_call", "amount": "0.04", "currency": "usd"},
    })
    call(env, key, route, "changed-price")
    wait_spent(env, key, "0.06")
    # Original 0.00003 plus new 0.000165, not two calls at the new price.
    money(stats(env)["actual_cost"], "0.000195", 2)
    money(stats(env, "models")[0]["billing"], "0.06", 2)


def test_spend_read_failure_fails_closed_before_upstream(usage_env):
    env = usage_env
    _, route, key = priced_route(env, "spend-read-error", spend_limit="1",
                                billing={"mode": "per_call", "amount": "0.02", "currency": "usd"})
    _run_sql(env, [("ALTER TABLE api_key_spend RENAME TO unavailable_test_spend", ())])
    try:
        status, _ = _proxy(env, key["key"], route["model_id"], [{"role": "user", "content": "must-not-run"}])
        assert status >= 500
    finally:
        _run_sql(env, [("ALTER TABLE unavailable_test_spend RENAME TO api_key_spend", ())])
    assert stats(env)["upstream_request_count"] == 0
    call(env, key, route)
    wait_spent(env, key, "0.02")


def test_mcp_internal_model_uses_called_route_billing_and_root_key(usage_env):
    env = usage_env
    _, route, key = priced_route(env, "mcp-media-billing", input_modalities=("text", "image"),
                                billing={"mode": "per_call", "amount": "0.03", "currency": "usd"})
    admin(env, "PUT", "media-understanding/image", {
        "enabled": True, "model_id": route["id"], "thinking_level": "medium",
    })
    admin(env, "PUT", f"api-keys/{key['id']}", {
        "transparent_injection_enabled": True, "inject_media_understanding": True,
    })
    auth = {"Authorization": f"Bearer {key['key']}"}
    image = base64.b64decode(PNG)
    status, upload = http_request("POST", env["proxy"] + "/v1/artifacts/uploads",
                                  {"mime_type": "image/png", "size": len(image)}, auth)
    assert status == 201, upload
    connection = HTTPConnection("127.0.0.1", int(env["proxy"].rsplit(":", 1)[1]), timeout=20)
    try:
        connection.request("PUT", f"/v1/artifacts/uploads/{upload['upload_id']}/parts/1", image,
                           {**auth, "x-upload-token": upload["upload_token"]})
        response = connection.getresponse()
        assert response.status == 200
        part = json.loads(response.read())
    finally:
        connection.close()
    status, artifact = http_request("POST", env["proxy"] + f"/v1/artifacts/uploads/{upload['upload_id']}/complete",
                                   {"upload_token": upload["upload_token"], "parts": [part]}, auth)
    assert status == 200, artifact
    status, result = http_request("POST", env["proxy"] + "/mcp", {
        "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
            "name": "StraviaRead", "arguments": {"path": artifact["path"]},
            "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28",
                      "io.modelcontextprotocol/clientCapabilities": {}},
        },
    }, {**auth, "Accept": "application/json, text/event-stream", "MCP-Protocol-Version": "2026-07-28",
        "Mcp-Method": "tools/call", "Mcp-Name": "StraviaRead"}, timeout=60)
    assert status == 200, result
    assert not result["result"]["isError"], result
    wait_spent(env, key, "0.03")
    rows = stats(env, "api-keys")
    assert [row["api_key_id"] for row in rows] == [key["id"]]
    money(rows[0]["billing"], "0.03", 1)
    route_stats = stats(env, "models")[0]
    assert route_stats["model_id"] == route["id"]
    money(route_stats["cost"], "0.00003", 1)
    money(route_stats["billing"], "0.03", 1)


def test_crash_during_priced_attempt_records_unknown_cost_without_charge(
    usage_env, stravia_binary: Path,
):
    env = usage_env
    _, route, key = priced_route(
        env, "crash-recovery",
        billing={"mode": "per_call", "amount": "0.02", "currency": "usd"},
    )
    outcome: list[object] = []

    def blocked_request() -> None:
        try:
            outcome.append(_proxy(
                env, key["key"], route["model_id"],
                [{"role": "user", "content": "blocked"}],
            ))
        except Exception as error:  # The real server is deliberately killed mid-request.
            outcome.append(error)

    worker = threading.Thread(target=blocked_request)
    worker.start()
    try:
        assert env["entered"].wait(timeout=10.0), "upstream did not receive the priced attempt"
        interaction = _wait_for(
            "running interaction before crash",
            lambda: next(
                (
                    item for item in _route_interactions(env, route["id"])
                    if item["status"] == "running"
                ),
                None,
            ),
        )

        def started_attempt() -> dict[str, Any] | None:
            detail = _detail(env, interaction["id"])
            if not detail["runs"]:
                return None
            run = detail["runs"][0]
            started = any(
                event["kind"] == "target_attempt_started"
                and event["payload"].get("cost_basis") == "usage"
                for event in run["events"]
            )
            return detail if run["status"] == "running" and started else None

        _wait_for("priced attempt start before crash", started_attempt)
        # The attempt is still running, so it is neither a known nor unknown cost yet.
        money(stats(env)["actual_cost"], "0", 0)
        assert admin(env, "GET", f"api-keys/{key['id']}")["spend"] == {
            "spent": "0", "reset_at": None,
        }
    finally:
        # Kill without a graceful terminal; only restart recovery may close the attempt.
        if env["process"].poll() is None:
            try:
                env["process"].kill()
            except ProcessLookupError:
                pass
            env["process"].wait(timeout=10.0)
        env["release"].set()
    worker.join(timeout=15.0)
    assert outcome, "the blocked request must end when the process crashes"

    restarted = None
    restarted_logs: list[str] = []
    try:
        port = find_free_port()
        base = f"http://127.0.0.1:{port}"
        args = [
            "--data-dir", str(env["data_dir"]),
            "--host", "127.0.0.1", "--port", str(port),
            *env["storage_runtime"]["server_args"](env["backend"]),
        ]
        restarted, restarted_logs = start_stravia_server(
            stravia_binary=stravia_binary, args=args,
        )
        wait_until_ready(f"{base}/readyz")
        session = WebSession(base)
        status, body = session.request(
            "POST", "/api/v1/auth/login",
            {"username": "admin", "password": "correct horse battery staple"},
        )
        assert status == 200, body
        restarted_env = {
            **env, "admin": base, "proxy": base, "auth": session.auth_headers(),
        }
        overview = _wait_for(
            "recovered unknown attempt cost",
            lambda: (lambda view: view if view["actual_cost"]["unknown_count"] == 1 else None)(
                stats(restarted_env)
            ),
        )
        money(overview["actual_cost"], "0", 0, 1)
        model_row = stats(restarted_env, "models")[0]
        money(model_row["cost"], "0", 0, 1)
        # Recovery leaves billing untouched: no charge and no unknown charge.
        money(model_row["billing"], "0", 0)
        api_row = stats(restarted_env, "api-keys")[0]
        money(api_row["billing"], "0", 0)
        assert admin(restarted_env, "GET", f"api-keys/{key['id']}")["spend"] == {
            "spent": "0", "reset_at": None,
        }

        def recovered_interrupted_run() -> dict[str, Any] | None:
            detail = _detail(restarted_env, interaction["id"])
            if not detail["runs"]:
                return None
            return detail if detail["runs"][0]["status"] == "interrupted" else None

        recovered = _wait_for("recovered interrupted run", recovered_interrupted_run)
        assert recovered["runs"][0]["terminal_reason"] == "process_restarted"
    finally:
        if restarted is not None:
            stop_stravia_server(restarted, restarted_logs)


def test_postgres_peer_currency_refresh_uses_snapshot_and_converted_limit(
    stravia_binary: Path, storage_runtime: dict[str, Any], tmp_path: Path,
) -> None:
    pg_url = storage_runtime["pg_url"]
    assert isinstance(pg_url, str) and pg_url, "PostgreSQL peer test requires isolated DB_URL"
    schema = storage_runtime["make_isolated_schema"]("stravia_peer_currency")
    storage_runtime["run_schema_action"]("create", pg_url=pg_url, schema=schema)
    dsn = storage_runtime["postgres_dsn_for_schema"](pg_url, schema)
    node_a = tmp_path / "node-a"
    node_b = tmp_path / "node-b"
    node_a.mkdir()
    node_b.mkdir()
    process_a = None
    process_b = None
    logs_a: list[str] = []
    logs_b: list[str] = []
    try:
        with usage_upstream() as (upstream, _, _):
            port_a = find_free_port()
            base_a = f"http://127.0.0.1:{port_a}"
            process_a, logs_a = start_stravia_server(
                stravia_binary=stravia_binary,
                args=[
                    "--data-dir", str(node_a),
                    "--host", "127.0.0.1", "--port", str(port_a),
                    *storage_runtime["server_args"]("postgres"),
                ],
            )
            wait_until_ready(f"{base_a}/api/v1/auth/state")
            session_a = initialize_server(
                base_a, wait_for_setup_token(logs_a, process_a),
                {"backend": "postgres", "url": dsn},
            )
            env_a = {
                "admin": base_a, "proxy": base_a, "auth": session_a.auth_headers(),
                "mock": f"http://127.0.0.1:{upstream.server_port}",
            }
            _, route, key = priced_route(
                env_a, "peer-currency",
                billing={"mode": "per_call", "amount": "10", "currency": "usd"},
                spend_limit="5",
            )

            port_b = find_free_port()
            base_b = f"http://127.0.0.1:{port_b}"
            process_b, logs_b = start_stravia_server(
                stravia_binary=stravia_binary,
                args=[
                    "--data-dir", str(node_b),
                    "--config", str(node_a / "server.toml"),
                    "--host", "127.0.0.1", "--port", str(port_b),
                    "--config-poll-interval", "1",
                    *storage_runtime["server_args"]("postgres"),
                ],
            )
            wait_until_ready(f"{base_b}/readyz")
            session_b = WebSession(base_b)
            status, body = session_b.request(
                "POST", "/api/v1/auth/login",
                {"username": "admin", "password": "correct horse battery staple"},
            )
            assert status == 200, body
            env_b = {"admin": base_b, "proxy": base_b, "auth": session_b.auth_headers()}
            assert stats(env_b)["currency"] == {"kind": "usd", "symbol": "$"}

            # Prove B consumed an epoch published after startup before changing
            # currency, removing the poller-initialization ordering race.
            admin(env_a, "PUT", f"models/{route['model_id']}", {
                "display_name": "peer-currency refreshed",
            })

            def peer_cache_refreshed() -> dict[str, Any] | None:
                status, body = http_request(
                    "GET", f"{base_b}/v1/models",
                    headers={"authorization": f"Bearer {key['key']}"},
                )
                assert status == 200, body
                model = next(
                    (item for item in body["data"] if item["id"] == route["model_id"]),
                    None,
                )
                return model if model and model["display_name"] == "peer-currency refreshed" else None

            _wait_for("peer model cache epoch refresh", peer_cache_refreshed, timeout=15.0)

            currency = admin(env_a, "PUT", "currency", {"symbol": "¥", "usd_rate": "7.2"})
            assert currency["custom"]["first_rate"] == "7.2"
            # The first currency write converted the stored limit exactly once.
            assert Decimal(
                admin(env_a, "GET", f"api-keys/{key['id']}")["spend_limit"]
            ) == Decimal("36")
            assert Decimal(
                admin(env_b, "GET", f"api-keys/{key['id']}")["spend_limit"]
            ) == Decimal("36")

            def peer_currency_refreshed() -> dict[str, Any] | None:
                view = stats(env_b)
                return view if view["currency"] == {"kind": "custom", "symbol": "¥"} else None

            _wait_for("peer config epoch currency refresh", peer_currency_refreshed, timeout=15.0)

            # B prices this attempt with the currency it observed, not USD.
            status, body = _proxy(
                env_b, key["key"], route["model_id"],
                [{"role": "user", "content": "peer-currency"}],
            )
            assert status == 200, body
            # The call is charged at B's frozen custom rate. Wait on A's
            # already-refreshed snapshot so the 429 below isolates B's compare.
            wait_spent(env_a, key, "72")

            # 72 CNY of spend already exceeds the converted 36 CNY limit; a
            # stale USD-only peer snapshot would still admit this request.
            status, body = _proxy(
                env_b, key["key"], route["model_id"],
                [{"role": "user", "content": "peer-currency-blocked"}],
            )
            assert status == 429, body
            assert body["error"]["type"] == "insufficient_quota", body
            assert body["error"]["code"] == "api_key_spend_limit_reached", body

            # The same snapshot drives B's display of the charged amount.
            wait_spent(env_b, key, "72")
            peer_row = next(
                row for row in stats(env_b, "models") if row["model_id"] == route["id"]
            )
            money(peer_row["billing"], "72", 1)
    finally:
        if process_b is not None:
            stop_stravia_server(process_b, logs_b)
        if process_a is not None:
            stop_stravia_server(process_a, logs_a)
        storage_runtime["run_schema_action"]("drop", pg_url=pg_url, schema=schema)
