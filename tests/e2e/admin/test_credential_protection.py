from __future__ import annotations

import json
import sqlite3
import time
from concurrent.futures import ThreadPoolExecutor, TimeoutError as FutureTimeoutError
from contextlib import closing, contextmanager
from pathlib import Path
from typing import Any, Iterator
from urllib.parse import urlencode

import pytest

from tests.common.helpers import http_request, stop_stravia_server
from tests.e2e.admin.test_observation_failures import _start_initialized
from tests.e2e.admin.test_observations import _create_route, _detail, _proxy, _route_interactions, _wait_for
from tests.e2e.admin.test_reversible_redaction import SECRET, REFERENCE, echo_provider, mapping_sql, set_enabled


BASE = "/api/v1/reversible-redaction"


def detect_text(env: dict[str, Any], text: str) -> list[dict[str, Any]]:
    status, body = http_request(
        "POST", f"{env['admin']}{BASE}/test", payload={"text": text}, headers=env["auth"],
    )
    assert status == 200, body
    return body["data"]["matches"]


def discoveries(env: dict[str, Any], **query: object) -> dict[str, Any]:
    status, body = http_request(
        "GET", f"{env['admin']}{BASE}/discoveries?{urlencode(query)}", headers=env["auth"],
    )
    assert status == 200, body
    return body["data"]


def key_discoveries(env: dict[str, Any], key_name: str) -> list[dict[str, Any]]:
    items = []
    cursor = None
    seen = set()
    while True:
        page = discoveries(env, **({"cursor": cursor} if cursor else {}), limit=100)
        items.extend(item for item in page["items"] if item["api_key_name"] == key_name)
        cursor = page["next_cursor"]
        if cursor is None:
            return items
        assert cursor not in seen
        seen.add(cursor)


@pytest.mark.e2e
@pytest.mark.admin
def test_catalog_tester_positions_and_read_only_boundary(admin_env: dict[str, Any]) -> None:
    status, body = http_request("GET", f"{admin_env['admin']}{BASE}/rules", headers=admin_env["auth"])
    assert status == 200, body
    catalog = body["data"]
    actual = {rule["id"]: rule for rule in catalog["rules"]}
    assert len(actual) == len(catalog["rules"])
    for canonical, obsolete in [
        ("bittrex-access-key", "bittrex-secret-key"),
        ("kingfisher.artifactory.2", "kingfisher.jfrog.1"),
        ("kingfisher.plaid.2", "kingfisher.plaid.3"),
    ]:
        assert canonical in actual
        assert obsolete not in actual
    for rule in actual.values():
        assert all(component["id"] in actual for component in rule["components"])
        assert "validate" not in rule and "validation" not in rule

    with echo_provider() as (url, received):
        env = {**admin_env, "mock": url}
        _, key = _create_route(env, "credential-tester")
        before_mappings = mapping_sql(env, "SELECT COUNT(*) FROM reversible_redaction_mappings")
        before_interactions = mapping_sql(env, "SELECT COUNT(*) FROM interaction_observations")
        before_discoveries = discoveries(env)
        text = f"中文😀\n前缀é {SECRET}\n重复🧪 {SECRET}"
        try:
            for enabled in [False, True]:
                set_enabled(env, enabled)
                matches = detect_text(env, text)
                github = [match for match in matches if match["rule_id"] == "github-pat"]
                assert len(github) == 2
                for match, line in zip(sorted(github, key=lambda item: item["start"]), [2, 3], strict=True):
                    encoded = text.encode("utf-16-le")
                    assert encoded[match["start"] * 2:match["end"] * 2].decode("utf-16-le") == SECRET
                    assert (match["start_line"], match["end_line"]) == (line, line)
                    assert match["start_column"] == 5
                    assert match["end_column"] == 5 + len(SECRET)
                assert detect_text(env, "ordinary text without a credential") == []
                status, settings = http_request("GET", f"{env['admin']}/api/v1/settings/reversible_redaction_enabled", headers=env["auth"])
                assert status == 200, settings
                assert settings["data"] == ("true" if enabled else "false")
            assert received == []
            assert mapping_sql(env, "SELECT COUNT(*) FROM reversible_redaction_mappings") == before_mappings
            assert mapping_sql(env, "SELECT COUNT(*) FROM interaction_observations") == before_interactions
            assert discoveries(env) == before_discoveries
            assert not any(SECRET in line for line in env["logs"])
            with closing(sqlite3.connect(env["data_dir"] / "db" / "gateway.db")) as database:
                assert text not in "\n".join(database.iterdump())

            for method, resource, payload in [("GET", "rules", None), ("GET", "discoveries", None), ("POST", "test", {"text": SECRET})]:
                for headers in [{}, {"authorization": f"Bearer {key}"}]:
                    status, _ = http_request(
                        method, f"{env['admin']}{BASE}/{resource}", payload=payload,
                        headers={"origin": env["admin"], "x-stravia-csrf": "1", **headers},
                    )
                    assert status == 401
            for headers in [
                {name: value for name, value in env["auth"].items() if name.lower() != "x-stravia-csrf"},
                {**env["auth"], "origin": "https://attacker.invalid"},
            ]:
                status, _ = http_request("POST", f"{env['admin']}{BASE}/test", payload={"text": SECRET}, headers=headers)
                assert status == 403
            status, invalid = http_request("POST", f"{env['admin']}{BASE}/test", payload={"text": []}, headers=env["auth"])
            assert status == 422, invalid
        finally:
            set_enabled(env, False)


@pytest.mark.e2e
@pytest.mark.admin
def test_bare_kingfisher_credentials_are_protected_and_restored(admin_env: dict[str, Any]) -> None:
    # Synthetic fixtures: no production or user-supplied credentials in the repository.
    first = "sk-m7q2b9v4x0k6n3r8s1t5w9y2z4c6d8f0g3h5j7l1p2a4e6u8"
    second = "7b2d9f4a0c6e3a8b1d5f9c2e4a6b8d0f.Q7m2Z9v4K0r6T3x8"
    text = f"中文😀 {first}\nAuthorization: Bearer {second}\n重复 {first}"
    with echo_provider() as (url, received):
        env = {**admin_env, "mock": url}
        model = "kingfisher-bare-credentials"
        _, key = _create_route(env, model)
        set_enabled(env, True)
        try:
            before = mapping_sql(env, "SELECT COUNT(*) FROM reversible_redaction_mappings")[0][0]
            matches = detect_text(env, text)
            assert {"kingfisher.openai.1", "kingfisher.zhipu.1"} <= {item["rule_id"] for item in matches}
            encoded = text.encode("utf-16-le")
            for item in matches:
                if item["rule_id"] in {"kingfisher.openai.1", "kingfisher.zhipu.1"}:
                    value = encoded[item["start"] * 2:item["end"] * 2].decode("utf-16-le")
                    assert value in {first, second}
            assert mapping_sql(env, "SELECT COUNT(*) FROM reversible_redaction_mappings")[0][0] == before
            assert received == []
            status, response = _proxy(env, key, model, [{"role": "user", "content": text}])
            assert status == 200, response
            assert response["choices"][0]["message"]["content"] == text
            outbound = received[0]["body"]["messages"][-1]["content"]
            assert first not in outbound and second not in outbound
            references = REFERENCE.findall(outbound)
            assert len(references) == 3 and len(set(references)) == 2
            assert mapping_sql(env, "SELECT COUNT(*) FROM reversible_redaction_mappings")[0][0] == before + 2
            rows = _wait_for("Kingfisher discoveries", lambda: key_discoveries(env, f"{model}-key"))
            assert rows[0]["new_credential_count"] == 2
            assert {"kingfisher.openai.1", "kingfisher.zhipu.1"} <= set(rows[0]["rule_ids"])
            assert all(first not in line and second not in line for line in env["logs"])
        finally:
            set_enabled(env, False)


@pytest.mark.e2e
@pytest.mark.admin
def test_discoveries_group_tool_continuation_and_split_new_user(admin_env: dict[str, Any]) -> None:
    second = "ghp_9Er8nQ3wM0tY5bS7uL4oG6xI2kC1dZaVfJpH"
    third = "ghp_7Cp6lO1uK8rW3zQ5sJ2mE4vG0iA9bXyTdHnF"
    with echo_provider(tool=True) as (url, received):
        env = {**admin_env, "mock": url}
        model = "credential-tool-group"
        route, key = _create_route(env, model)
        tools = [{"type": "function", "function": {"name": "configure", "parameters": {"type": "object"}}}]
        set_enabled(env, True)
        try:
            messages = [{"role": "user", "content": f"{SECRET} {SECRET}"}]
            status, first = _proxy(env, key, model, messages, body_extra={"tools": tools})
            assert status == 200, first
            first_item = _wait_for("first credential discovery", lambda: key_discoveries(env, f"{model}-key"))[0]
            assert first_item["new_credential_count"] == 1
            assistant = first["choices"][0]["message"]
            messages += [assistant, {"role": "tool", "tool_call_id": assistant["tool_calls"][0]["id"], "content": second}]
            status, response = _proxy(env, key, model, messages, body_extra={"tools": tools})
            assert status == 200, response
            grouped = _wait_for("second mapping in same interaction", lambda: (lambda rows: rows if len(rows) == 1 and rows[0]["new_credential_count"] == 2 else None)(key_discoveries(env, f"{model}-key")))[0]
            assert grouped["interaction_id"] == first_item["interaction_id"]
            assert {"user_message", "tool_result"} <= set(grouped["source_types"])
            assert "github-pat" in grouped["rule_ids"]
            detail = _wait_for("two completed runs", lambda: (lambda value: value if len(value["runs"]) == 2 else None)(_detail(env, grouped["interaction_id"])))
            assert len(detail["runs"]) == 2
            # 新 User 输入需要在快速续接窗口之外才会开启新的 Interaction（CONTEXT.md 归并规则）。
            time.sleep(2.01)
            messages += [response["choices"][0]["message"], {"role": "user", "content": third}]
            status, response = _proxy(env, key, model, messages, body_extra={"tools": tools})
            assert status == 200, response
            rows = _wait_for("new user discovery", lambda: (lambda values: values if len(values) == 2 else None)(key_discoveries(env, f"{model}-key")))
            assert [row["new_credential_count"] for row in rows] == [1, 2]
            assert rows[0]["interaction_id"] != grouped["interaction_id"]
            assert rows[0]["discovered_at"] >= rows[1]["discovered_at"]
            assert {row["interaction_id"] for row in rows} == {row["id"] for row in _route_interactions(env, route)}
            serialized = json.dumps(rows)
            assert all(secret not in serialized for secret in [SECRET, second, third])
            assert REFERENCE.search(serialized) is None
            assert all(secret not in json.dumps(received) for secret in [SECRET, second, third])
        finally:
            set_enabled(env, False)


@pytest.mark.e2e
@pytest.mark.admin
def test_observation_write_gap_follows_retention_changes(admin_env: dict[str, Any]) -> None:
    with echo_provider() as (url, _received):
        env = {**admin_env, "mock": url}
        _, key = _create_route(env, "credential-retention-gap")
        set_enabled(env, True)
        mapping_sql(env, """
            CREATE TRIGGER reject_credential_observation BEFORE INSERT ON observation_events
            WHEN NEW.kind = 'credential_mappings_created'
            BEGIN SELECT RAISE(FAIL, 'injected observation failure'); END
        """)
        try:
            status, response = _proxy(
                env, key, "credential-retention-gap", [{"role": "user", "content": SECRET}]
            )
            assert status == 200, response
            assert discoveries(env)["observation_gap"]
            status, body = http_request(
                "PUT", f"{env['admin']}/api/v1/settings/log_retention_days",
                payload={"value": "0"}, headers=env["auth"],
            )
            assert status == 200, body
            assert not discoveries(env)["observation_gap"]
        finally:
            mapping_sql(env, "DROP TRIGGER reject_credential_observation")
            set_enabled(env, False)
            status, body = http_request(
                "PUT", f"{env['admin']}/api/v1/settings/log_retention_days",
                payload={"value": "7"}, headers=env["auth"],
            )
            assert status == 200, body


@pytest.mark.e2e
@pytest.mark.admin
def test_custom_rules_crud_validation_and_protected_roundtrip(admin_env: dict[str, Any]) -> None:
    custom = f"{BASE}/custom-rules"
    literal = "Team Alpha internal phrase"
    with echo_provider() as (url, received):
        env = {**admin_env, "mock": url}
        model = "custom-credential-rules"
        _, key = _create_route(env, model)
        set_enabled(env, True)
        created: list[str] = []
        try:
            status, body = http_request("GET", f"{env['admin']}{custom}", headers=env["auth"])
            assert status == 200 and body["data"] == [], body

            for payload, field in [
                ({"name": "x", "spec": {"mode": "simple", "text": "   "}}, "text"),
                ({"name": "x", "spec": {"mode": "pattern", "regex": "(unclosed"}}, "regex"),
                ({"name": "", "spec": {"mode": "simple", "text": "abc"}}, "name"),
            ]:
                status, body = http_request("POST", f"{env['admin']}{custom}", payload=payload, headers=env["auth"])
                assert status == 400, body
                assert body["code"] == "custom_credential_rule_invalid"
                assert body["params"]["field"] == field
            status, body = http_request(
                "POST", f"{env['admin']}{custom}",
                payload={"name": "x", "spec": {"mode": "simple", "text": "abc"}},
                headers={name: value for name, value in env["auth"].items() if name.lower() != "x-stravia-csrf"},
            )
            assert status == 403

            status, body = http_request(
                "POST", f"{env['admin']}{custom}",
                payload={"name": "Phrase", "description": "e2e", "spec": {"mode": "simple", "text": literal}},
                headers=env["auth"],
            )
            assert status == 200, body
            simple = body["data"]
            created.append(simple["id"])
            assert simple["id"].startswith("custom.") and simple["enabled"] is True
            status, body = http_request(
                "POST", f"{env['admin']}{custom}",
                payload={"name": "Ticket", "spec": {
                    "mode": "pattern", "regex": r"tkt-([A-Za-z0-9]{12})", "secret_group": 1, "keywords": ["tkt-"],
                }},
                headers=env["auth"],
            )
            assert status == 200, body
            pattern = body["data"]
            created.append(pattern["id"])

            text = f"say {literal} and tkt-Ab3dE6gH9jK2, not team alpha internal phrase"
            matches = detect_text(env, text)
            by_rule = {item["rule_id"]: item for item in matches}
            encoded = text.encode("utf-16-le")
            assert encoded[by_rule[simple["id"]]["start"] * 2:by_rule[simple["id"]]["end"] * 2].decode("utf-16-le") == literal
            assert encoded[by_rule[pattern["id"]]["start"] * 2:by_rule[pattern["id"]]["end"] * 2].decode("utf-16-le") == "Ab3dE6gH9jK2"

            status, response = _proxy(env, key, model, [{"role": "user", "content": text}])
            assert status == 200, response
            assert response["choices"][0]["message"]["content"] == text
            outbound = received[0]["body"]["messages"][-1]["content"]
            assert literal not in outbound and "Ab3dE6gH9jK2" not in outbound
            assert "tkt-" in outbound and "not team alpha internal phrase" in outbound
            assert len(REFERENCE.findall(outbound)) == 2

            status, body = http_request(
                "PUT", f"{env['admin']}{custom}/{simple['id']}",
                payload={"name": "Phrase", "enabled": False, "spec": {"mode": "simple", "text": literal}},
                headers=env["auth"],
            )
            assert status == 200 and body["data"]["enabled"] is False, body
            assert simple["id"] not in {item["rule_id"] for item in detect_text(env, text)}

            status, body = http_request(
                "PUT", f"{env['admin']}{custom}/custom.missing",
                payload={"name": "x", "spec": {"mode": "simple", "text": "abc"}}, headers=env["auth"],
            )
            assert status == 404 and body["code"] == "custom_credential_rule_not_found"
        finally:
            for rule_id in created:
                http_request("DELETE", f"{env['admin']}{custom}/{rule_id}", headers=env["auth"])
            set_enabled(env, False)
        status, body = http_request("GET", f"{env['admin']}{custom}", headers=env["auth"])
        assert status == 200 and body["data"] == [], body


@contextmanager
def _sqlite_write_lock(database_path: Path) -> Iterator[sqlite3.Connection]:
    """Hold an external SQLite write transaction without blocking readers."""
    connection = sqlite3.connect(database_path, isolation_level=None, timeout=30.0)
    try:
        connection.execute("BEGIN IMMEDIATE")
        yield connection
    finally:
        try:
            connection.rollback()
        finally:
            connection.close()


@pytest.mark.parametrize("debug_enabled", [False, True], ids=["debug-off", "debug-on"])
@pytest.mark.e2e
@pytest.mark.admin
def test_discoveries_read_committed_events_without_waiting_for_blocked_writer(
    stravia_binary: Path, tmp_path: Path, debug_enabled: bool
) -> None:
    model = "credential-discovery-blocked-writer"
    # The observation writer's SQLite busy timeout is 5s per rejected request.
    # Three unrelated rejections make a flush-based read take at least 15s,
    # while a committed read must return well inside this watchdog.
    watchdog = 8.0
    with echo_provider() as (url, received):
        env, process, logs = _start_initialized(stravia_binary, tmp_path, url)
        executor: ThreadPoolExecutor | None = None
        try:
            _route, key = _create_route(env, model)
            set_enabled(env, True)
            status, _response = _proxy(
                env, key, model,
                [{"role": "user", "content": f"unrelated prefix {SECRET}"}],
            )
            assert status == 200, "credential-bearing request failed"

            def committed_page() -> dict[str, Any] | None:
                page = discoveries(env, limit=20)
                if page["items"] and page["items"][0]["status"] == "completed":
                    return page
                return None

            baseline = _wait_for("persisted credential discovery", committed_page)
            assert len(baseline["items"]) == 1
            expected = baseline["items"][0]
            assert expected["api_key_name"] == f"{model}-key"
            assert expected["new_credential_count"] == 1
            assert "github-pat" in expected["rule_ids"]
            assert "user_message" in expected["source_types"]
            assert baseline["next_cursor"] is None
            assert baseline["observation_gap"] is False

            if debug_enabled:
                status, body = http_request(
                    "PUT",
                    f"{env['admin']}/api/v1/observations/debug",
                    payload={"enabled": True, "confirmed": True},
                    headers=env["auth"],
                )
                assert status == 200, body

            executor = ThreadPoolExecutor(
                max_workers=1, thread_name_prefix="credential-discovery-read",
            )
            with _sqlite_write_lock(tmp_path / "db" / "gateway.db"):
                for _ in range(3):
                    status, _body = http_request(
                        "POST",
                        f"{env['proxy']}/v1/chat/completions",
                        payload={
                            "model": model,
                            "messages": [{"role": "user", "content": "unrelated rejected observation"}],
                        },
                        timeout=10.0,
                    )
                    assert status == 401, "unauthenticated request must be rejected"
                assert len(received) == 1, "rejected requests must not reach the provider"
                future = executor.submit(
                    http_request,
                    "GET",
                    f"{env['admin']}{BASE}/discoveries?{urlencode({'limit': 20})}",
                    headers=env["auth"],
                    timeout=watchdog + 2.0,
                )
                started = time.monotonic()
                try:
                    status, body = future.result(timeout=watchdog)
                except FutureTimeoutError:
                    if future.done():
                        raise
                    pytest.fail(
                        "credential discovery read waited for the observation writer while the "
                        f"SQLite write lock was held (waited {time.monotonic() - started:.1f}s, "
                        f"watchdog {watchdog:.0f}s, debug={debug_enabled})"
                    )
                assert status == 200, f"credential discovery read returned HTTP {status}"
                page = body["data"]
                assert page["next_cursor"] is None
                assert page["observation_gap"] is False
                assert len(page["items"]) == 1
                item = page["items"][0]
                assert item["interaction_id"] == expected["interaction_id"]
                assert item["api_key_name"] == expected["api_key_name"]
                assert item["discovered_at"] == expected["discovered_at"]
                assert item["new_credential_count"] == expected["new_credential_count"]
                assert item["rule_ids"] == expected["rule_ids"]
                assert item["source_types"] == expected["source_types"]
                assert item["status"] == expected["status"]
                assert item["observation_gap"] == expected["observation_gap"]
        finally:
            try:
                if executor is not None:
                    executor.shutdown(wait=True)
            finally:
                stop_stravia_server(process, logs)
