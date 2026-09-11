from __future__ import annotations

import json
import sqlite3
import tomllib
from contextlib import closing
from pathlib import Path
from typing import Any
from urllib.parse import urlencode

import pytest

from tests.common.helpers import http_request
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
def test_catalog_tester_positions_and_read_only_boundary(admin_env: dict[str, Any], repo_root: Path) -> None:
    snapshot = tomllib.loads((repo_root / "backend/crates/stravia-credential-protection/src/detection/betterleaks.toml").read_text(encoding="utf-8"))
    status, body = http_request("GET", f"{admin_env['admin']}{BASE}/rules", headers=admin_env["auth"])
    assert status == 200, body
    catalog = body["data"]
    actual = {rule["id"]: rule for rule in catalog["rules"]}
    expected = {rule["id"]: rule for rule in snapshot["rules"]}
    kingfisher = json.loads((repo_root / "backend/crates/stravia-credential-protection/src/detection/kingfisher.json").read_text(encoding="utf-8"))
    imported = {rule["id"]: rule for rule in kingfisher["rules"]}
    assert actual.keys() == expected.keys() | imported.keys()
    assert len(catalog["rules"]) == len(expected) + len(imported)
    for rule_id, source in imported.items():
        rule = actual[rule_id]
        assert rule["regex"] == source["pattern"]
        assert rule["name"] == source["name"]
        assert rule["skip_report"] == (not source.get("visible", True))
        assert rule["confidence"] == source.get("confidence", "medium")
        assert "validate" not in rule and "validation" not in rule
    assert catalog["prefilter"] == snapshot["prefilter"]
    assert catalog["filter"] == snapshot["filter"]
    for rule_id, source in expected.items():
        rule = actual[rule_id]
        assert rule["regex"] == source.get("regex")
        assert rule["path"] == source.get("path")
        assert rule["filter"] == source.get("filter", "")
        assert rule["components"] == [{"id": part["id"], "within": part.get("within", ""), "optional": part.get("optional", False)} for part in source.get("components", [])]
        assert rule["name"] and rule["target"] and rule["description"]
        assert "validate" not in rule

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
            with closing(sqlite3.connect(env["data_dir"] / "gateway.db")) as database:
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
