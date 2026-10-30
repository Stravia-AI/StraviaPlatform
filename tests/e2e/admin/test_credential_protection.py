from __future__ import annotations

import json
import random
import sqlite3
import string
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


def key_discoveries(env: dict[str, Any], key_name: str, **query: object) -> list[dict[str, Any]]:
    items = []
    cursor = None
    seen = set()
    while True:
        page = discoveries(env, **({"cursor": cursor} if cursor else {}), limit=100, **query)
        items.extend(item for item in page["items"] if item["api_key_name"] == key_name)
        cursor = page["next_cursor"]
        if cursor is None:
            return items
        assert cursor not in seen
        seen.add(cursor)


def pairs(hits: list[dict[str, Any]]) -> set[tuple[str, str, bool | None]]:
    return {(hit["rule_id"], hit["source_type"], hit.get("latest_input")) for hit in hits}


def hit_events(env: dict[str, Any], interaction_id: str) -> list[tuple[dict[str, Any], dict[str, Any]]]:
    """Run 事件流里的命中事件，连同所属 Run（用于读取 Debug 快照）。"""
    return [
        (run, event)
        for run in _detail(env, interaction_id)["runs"]
        for event in run["events"]
        if event["kind"] in {"credential_mappings_created", "credential_mappings_reused"}
    ]


def synthetic_token(label: str, index: int) -> str:
    """确定性的 GitHub PAT 形态合成令牌；不是任何真实凭据。"""
    alphabet = string.ascii_letters + string.digits
    return "ghp_" + "".join(random.Random(f"{label}-{index}").choices(alphabet, k=36))


def set_debug(env: dict[str, Any], enabled: bool) -> None:
    status, body = http_request(
        "PUT", f"{env['admin']}/api/v1/observations/debug",
        payload={"enabled": enabled, "confirmed": enabled}, headers=env["auth"],
    )
    assert status == 200, body


@pytest.mark.e2e
@pytest.mark.admin
def test_catalog_tester_positions_and_read_only_boundary(admin_env: dict[str, Any]) -> None:
    status, body = http_request("GET", f"{admin_env['admin']}{BASE}/rules", headers=admin_env["auth"])
    assert status == 200, body
    catalog = body["data"]
    actual = {rule["id"]: rule for rule in catalog["rules"]}
    assert len(actual) == len(catalog["rules"])
    assert "bittrex-access-key" in actual
    assert "bittrex-secret-key" not in actual
    assert not [rule_id for rule_id in actual if rule_id.startswith("kingfisher.")]
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
            # 同一 Interaction 的第二次请求：工具结果里的新凭据是新增；重放的用户消息与工具调用只是历史，不产生复用记录。
            # 复用只来自第一次响应：模型在工具调用参数里用了这个凭据。
            assert ("github-pat", "user_message", True) in pairs(grouped["created"])
            assert any(hit["source_type"] == "tool_result" for hit in grouped["created"])
            assert pairs(grouped["reused"]) == {("github-pat", "tool_arguments", None)}
            detail = _wait_for("two completed runs", lambda: (lambda value: value if len(value["runs"]) == 2 else None)(_detail(env, grouped["interaction_id"])))
            assert len(detail["runs"]) == 2
            # 块级定位的契约：工具来源带客户端的工具调用 ID，用户消息只带是否为最新输入，其余来源两者都没有。
            call_id = assistant["tool_calls"][0]["id"]
            locators = [
                locator
                for _, event in hit_events(env, grouped["interaction_id"])
                for discovery in event["payload"]["discoveries"]
                for locator in discovery["locators"]
            ]
            assert {locator["tool_id"] for locator in locators if locator["source_type"] == "tool_result"} == {call_id}
            assert {locator["tool_id"] for locator in locators if locator["source_type"] == "tool_arguments"} == {call_id}
            assert all(
                ("tool_id" in locator) == (locator["source_type"] in {"tool_arguments", "tool_result"})
                and ("latest_input" in locator) == (locator["source_type"] == "user_message")
                for locator in locators
            ), locators
            assert any(locator.get("latest_input") is True for locator in locators)
            # 新 User 输入需要在快速续接窗口之外才会开启新的 Interaction（CONTEXT.md 归并规则）。
            time.sleep(2.01)
            messages += [response["choices"][0]["message"], {"role": "user", "content": third}]
            status, response = _proxy(env, key, model, messages, body_extra={"tools": tools})
            assert status == 200, response
            rows = _wait_for("new user discovery", lambda: (lambda values: values if len(values) == 2 else None)(key_discoveries(env, f"{model}-key")))
            assert [row["new_credential_count"] for row in rows] == [1, 2]
            assert rows[0]["interaction_id"] != grouped["interaction_id"]
            assert rows[0]["discovered_at"] >= rows[1]["discovered_at"]
            newest = rows[0]
            assert any(hit["source_type"] == "user_message" and hit["latest_input"] is True for hit in newest["created"])
            # 客户端重放的旧用户消息与旧工具结果都只是历史，不产生复用记录；
            # 复用只来自模型这一轮在工具调用参数里使用新凭据。
            assert pairs(newest["reused"]) == {("github-pat", "tool_arguments", None)}
            # 新一轮用户消息再次提交已知凭据，模型又在工具调用里用了它：两处都是复用。
            time.sleep(2.01)
            messages += [response["choices"][0]["message"], {"role": "user", "content": f"again {SECRET}"}]
            status, response = _proxy(env, key, model, messages, body_extra={"tools": tools})
            assert status == 200, response
            rows = _wait_for("reuse discovery", lambda: (lambda values: values if len(values) == 3 else None)(key_discoveries(env, f"{model}-key")))
            assert [row["new_credential_count"] for row in rows] == [0, 1, 2]
            reuse = rows[0]
            assert reuse["created"] == []
            assert pairs(reuse["reused"]) == {("github-pat", "user_message", True), ("github-pat", "tool_arguments", None)}
            name = f"{model}-key"
            created_only = key_discoveries(env, name, kind="created")
            assert [row["interaction_id"] for row in created_only] == [row["interaction_id"] for row in rows[1:]]
            assert all(row["reused"] == [] for row in created_only)
            # 三个 Interaction 里模型都在工具调用参数中使用了凭据，所以都有复用记录。
            reused_only = key_discoveries(env, name, kind="reused")
            assert [row["interaction_id"] for row in reused_only] == [row["interaction_id"] for row in rows]
            assert all(row["created"] == [] and row["new_credential_count"] == 0 for row in reused_only)
            status, invalid = http_request(
                "GET", f"{env['admin']}{BASE}/discoveries?kind=everything", headers=env["auth"],
            )
            assert status in {400, 422}, invalid
            assert {row["interaction_id"] for row in rows} == {row["id"] for row in _route_interactions(env, route)}
            serialized = json.dumps(rows)
            assert all(secret not in serialized for secret in [SECRET, second, third])
            assert REFERENCE.search(serialized) is None
            assert all(secret not in json.dumps(received) for secret in [SECRET, second, third])
        finally:
            set_enabled(env, False)


@pytest.mark.e2e
@pytest.mark.admin
def test_known_credentials_are_attributed_by_the_rules_stored_with_their_mapping(admin_env: dict[str, Any]) -> None:
    bare = "Q8n4Vk7sT2p9X5a3Lc6D0h1R"
    with echo_provider() as (url, received):
        env = {**admin_env, "mock": url}
        model = "credential-stored-rules"
        route, key = _create_route(env, model)
        set_enabled(env, True)
        try:
            status, body = _proxy(env, key, model, [{"role": "user", "content": f'api_key = "{bare}"'}])
            assert status == 200, body
            stored = mapping_sql(env, "SELECT rule_ids FROM reversible_redaction_mappings WHERE secret = ?", bare)[0][0]
            rules = json.loads(stored)
            assert rules, "the mapping keeps the rules that detected it"
            # 裸值本身没有检测器认得出的上下文，只能靠已知映射定位。
            assert detect_text(env, bare) == []

            def reuse_events(source: str) -> list[dict[str, Any]]:
                return [
                    event
                    for interaction in _route_interactions(env, route)
                    for _, event in hit_events(env, interaction["id"])
                    if event["kind"] == "credential_mappings_reused"
                    and any(
                        locator["source_type"] == source
                        for discovery in event["payload"]["discoveries"]
                        for locator in discovery["locators"]
                    )
                ]

            status, body = _proxy(env, key, model, [{"role": "user", "content": f"use {bare}"}])
            assert status == 200, body
            assert bare not in json.dumps(received[-1]["body"])
            events = _wait_for("reuse of the bare credential", lambda: reuse_events("user_message") or None)
            locators = [locator for discovery in events[0]["payload"]["discoveries"] for locator in discovery["locators"]]
            assert {locator["rule_id"] for locator in locators} == set(rules)

            # 创建时没有记下规则的映射（升级前创建）照常保护，但不能编造归因，也就不产生复用记录。
            mapping_sql(env, "UPDATE reversible_redaction_mappings SET rule_ids = '[]' WHERE secret = ?", bare)
            status, body = _proxy(env, key, model, [{"role": "user", "content": f"again {bare}"}])
            assert status == 200, body
            sent = received[-1]["body"]["messages"][-1]["content"]
            assert bare not in sent and REFERENCE.search(sent)
            # 换一类来源再提交一次：它的事件写入后，上一次请求若会产生记录也早已写完。
            mapping_sql(env, "UPDATE reversible_redaction_mappings SET rule_ids = ? WHERE secret = ?", stored, bare)
            status, body = _proxy(env, key, model, [{"role": "system", "content": f"note {bare}"}, {"role": "user", "content": "hi"}])
            assert status == 200, body
            _wait_for("reuse in instructions", lambda: reuse_events("system_or_history") or None)
            assert len(reuse_events("user_message")) == 1
        finally:
            set_enabled(env, False)


@pytest.mark.e2e
@pytest.mark.admin
def test_hit_write_failure_keeps_the_request_and_marks_only_its_interaction(admin_env: dict[str, Any]) -> None:
    with echo_provider() as (url, _received):
        env = {**admin_env, "mock": url}
        model = "credential-hit-write-gap"
        route, key = _create_route(env, model)
        set_enabled(env, True)
        mapping_sql(env, """
            CREATE TRIGGER reject_credential_observation BEFORE INSERT ON observation_events
            WHEN NEW.kind = 'credential_mappings_created'
            BEGIN SELECT RAISE(FAIL, 'injected observation failure'); END
        """)
        try:
            status, response = _proxy(env, key, model, [{"role": "user", "content": SECRET}])
            assert status == 200, response
            assert response["choices"][0]["message"]["content"] == SECRET
            interaction = _wait_for(
                "Interaction with the missing hit event",
                lambda: next((item for item in _route_interactions(env, route) if item["observation_gap"]), None),
            )
            page = discoveries(env)
            assert "observation_gap" not in page
            assert all(item["interaction_id"] != interaction["id"] for item in page["items"])
        finally:
            mapping_sql(env, "DROP TRIGGER reject_credential_observation")
            set_enabled(env, False)


@pytest.mark.e2e
@pytest.mark.admin
def test_failed_upstream_request_keeps_its_hits(admin_env: dict[str, Any]) -> None:
    with echo_provider(reject=True) as (url, received):
        env = {**admin_env, "mock": url}
        model = "credential-hit-failed"
        _, key = _create_route(env, model)
        set_enabled(env, True)
        try:
            status, _ = _proxy(env, key, model, [{"role": "user", "content": SECRET}])
            assert status != 200
            assert received and SECRET not in json.dumps(received)
            rows = _wait_for("hit of the failed request", lambda: key_discoveries(env, f"{model}-key"))
            assert pairs(rows[0]["created"]) == {("github-pat", "user_message", True)}
            assert rows[0]["status"] != "completed"
        finally:
            set_enabled(env, False)


@pytest.mark.e2e
@pytest.mark.admin
def test_hit_events_carry_placeholder_markers_only_for_debug_runs(admin_env: dict[str, Any]) -> None:
    other = synthetic_token("marker", 0)
    with echo_provider() as (url, received):
        env = {**admin_env, "mock": url}
        model = "credential-hit-markers"
        route, key = _create_route(env, model)
        set_enabled(env, True)
        try:
            status, response = _proxy(env, key, model, [{"role": "user", "content": SECRET}])
            assert status == 200, response
            set_debug(env, True)
            status, response = _proxy(env, key, model, [{"role": "user", "content": f"{SECRET} {other}"}])
            assert status == 200, response
            outbound = received[-1]["body"]["messages"][-1]["content"]
            references = set(REFERENCE.findall(outbound))
            assert len(references) == 2 and other not in outbound

            def debug_run_events() -> list[tuple[dict[str, Any], dict[str, Any]]] | None:
                events = [
                    pair for interaction in _route_interactions(env, route)
                    for pair in hit_events(env, interaction["id"])
                ]
                return events if {run["debug_enabled"] for run, _ in events} == {False, True} else None

            events = _wait_for("hit events from a plain and a Debug Run", debug_run_events)
            debug_markers: set[str] = set()
            for run, event in events:
                for discovery in event["payload"]["discoveries"]:
                    if run["debug_enabled"]:
                        assert REFERENCE.fullmatch(discovery["marker"]), discovery
                        debug_markers.add(discovery["marker"])
                    else:
                        assert "marker" not in discovery, discovery
            assert debug_markers == references
            # Debug 关闭后的复用同样不得带占位符。
            set_debug(env, False)
            status, response = _proxy(env, key, model, [{"role": "user", "content": SECRET}])
            assert status == 200, response

            def plain_reuse() -> bool | None:
                return any(
                    not run["debug_enabled"] and event["kind"] == "credential_mappings_reused"
                    for interaction in _route_interactions(env, route)
                    for run, event in hit_events(env, interaction["id"])
                ) or None

            _wait_for("a reuse event from a plain Run", plain_reuse)
            for interaction in _route_interactions(env, route):
                for run, event in hit_events(env, interaction["id"]):
                    assert run["debug_enabled"] or all("marker" not in item for item in event["payload"]["discoveries"])
            listed = json.dumps(discoveries(env))
            assert REFERENCE.search(listed) is None and '"marker"' not in listed
        finally:
            set_debug(env, False)
            set_enabled(env, False)


@pytest.mark.e2e
@pytest.mark.admin
def test_hit_event_caps_discoveries_and_counts_the_overflow(admin_env: dict[str, Any]) -> None:
    tokens = [synthetic_token("cap", index) for index in range(51)]
    with echo_provider() as (url, received):
        env = {**admin_env, "mock": url}
        model = "credential-hit-cap"
        route, key = _create_route(env, model)
        set_enabled(env, True)
        try:
            assert len(detect_text(env, " ".join(tokens))) == 51
            status, response = _proxy(env, key, model, [{"role": "user", "content": " ".join(tokens)}])
            assert status == 200, response
            assert not any(token in json.dumps(received) for token in tokens)
            row = _wait_for("capped hit row", lambda: key_discoveries(env, f"{model}-key"))[0]
            assert row["new_credential_count"] == 51
            assert pairs(row["created"]) == {("github-pat", "user_message", True)}
            (_, event), = hit_events(env, row["interaction_id"])
            assert len(event["payload"]["discoveries"]) == 50
            assert event["payload"]["omitted_discoveries"] == 1
        finally:
            set_enabled(env, False)


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
            assert "observation_gap" not in baseline
            assert len(baseline["items"]) == 1
            expected = baseline["items"][0]
            assert expected["api_key_name"] == f"{model}-key"
            assert expected["new_credential_count"] == 1
            assert ("github-pat", "user_message", True) in pairs(expected["created"])
            assert baseline["next_cursor"] is None
            assert expected["observation_gap"] is False

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
                assert "observation_gap" not in page
                assert page["next_cursor"] is None
                assert len(page["items"]) == 1
                item = page["items"][0]
                assert item["interaction_id"] == expected["interaction_id"]
                assert item["api_key_name"] == expected["api_key_name"]
                assert item["discovered_at"] == expected["discovered_at"]
                assert item["new_credential_count"] == expected["new_credential_count"]
                assert pairs(item["created"]) == pairs(expected["created"])
                assert pairs(item["reused"]) == pairs(expected["reused"])
                assert item["status"] == expected["status"]
                assert item["observation_gap"] == expected["observation_gap"]
        finally:
            try:
                if executor is not None:
                    executor.shutdown(wait=True)
            finally:
                stop_stravia_server(process, logs)
