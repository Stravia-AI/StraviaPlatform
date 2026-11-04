"""Observation 正文引用与内容端点（ADR-0089 接缝 2）。

摘要只带引用与 ``raw_len``；完整正文经 ``GET /api/v1/observations/contents`` 读取。
这里只断言 HTTP 可见行为：引用能展开为完整正文、端点只返回 Observation 自己引用的正文、
ETag 与 304、请求校验，以及详情、事件和 Bundle 导出里的全文。
"""

from __future__ import annotations

import io
import json
import zipfile
from typing import Any

import pytest

from tests.common.helpers import download_observation_bundle, http_request
from tests.common.observation_contents import (
    assert_summary_has_only_refs,
    etag_of,
    fetch_contents,
    fetch_contents_raw,
    input_text,
    output_text,
    ref_ids,
    scan_contents,
)
from tests.e2e.admin.test_credential_protection import MASK_TOKEN, discovery_hit, key_discoveries
from tests.e2e.admin.test_reversible_redaction import SECRET, REFERENCE, age_out_mapping, echo_provider, set_enabled
from tests.e2e.admin.test_observations import (
    _create_route,
    _detail,
    _event_page,
    _finished_detail,
    _proxy,
    _route_interactions,
    _wait_for,
)

SYSTEM_ONLY = "system-only-context-for-turn-chain-b7e2"
LONG_INPUT = ("长输入-" + "x" * 90) * 60  # 远超旧的 4096 字符预览上限


def _completed(env: dict[str, Any], route_id: str) -> dict[str, Any]:
    interaction = _wait_for(
        "completed Interaction",
        lambda: next((
            item for item in _route_interactions(env, route_id)
            if item["status"] == "completed" and _finished_detail(env, item["id"])
        ), None),
    )
    return interaction


@pytest.mark.e2e
@pytest.mark.admin
def test_summary_refs_resolve_to_full_bodies_in_every_admin_view(admin_env: dict[str, Any]) -> None:
    model = "observation-contents-full"
    route_id, key = _create_route(admin_env, model)
    assert len(LONG_INPUT) > 4096
    status, response = _proxy(admin_env, key, model, [
        {"role": "system", "content": SYSTEM_ONLY},
        {"role": "user", "content": LONG_INPUT},
    ])
    assert status == 200, response
    summary = _completed(admin_env, route_id)
    assert_summary_has_only_refs(summary)
    assert summary["input"]["raw_len"] == len(LONG_INPUT.encode())
    assert input_text(admin_env, summary) == LONG_INPUT
    assert output_text(admin_env, summary) == "mock-ok-0"

    detail = _finished_detail(admin_env, summary["id"])
    assert detail is not None
    assert detail["input_text"] == LONG_INPUT and detail["output_text"] == "mock-ok-0"
    events = [event for run in detail["runs"] for event in run["events"]]
    recorded = [event for event in events if event["kind"] == "user_input_recorded"]
    assert len(recorded) == 1 and recorded[0]["payload"]["text"] == LONG_INPUT
    visible = [event for event in events if event["kind"] == "client_visible_content"]
    assert "".join(event["payload"]["text"] for event in visible) == "mock-ok-0"

    # 会话视图（事件分页）同样返回完整正文，不再截断到旧的 4096 字符预览。
    page = _event_page(admin_env, summary["id"], limit=500)
    paged = [
        event for run in page["runs"] for event in run["events"] if event["kind"] == "user_input_recorded"
    ]
    assert [event["payload"]["text"] for event in paged] == [LONG_INPUT]
    assert len(paged[0]["payload"]["text"]) > 4096

    # Bundle 导出的事件带完整正文，与详情一致。
    _, _, archive = download_observation_bundle(admin_env, detail)
    with zipfile.ZipFile(io.BytesIO(archive)) as bundle:
        exported = json.loads(bundle.read("interaction.json"))
    exported_inputs = [event for event in exported["events"] if event["kind"] == "user_input_recorded"]
    assert [event["payload"]["text"] for event in exported_inputs] == [LONG_INPUT]


@pytest.mark.e2e
@pytest.mark.admin
def test_contents_endpoint_validates_ids_and_hides_everything_not_referenced_by_observation(
    admin_env: dict[str, Any],
) -> None:
    model = "observation-contents-scope"
    route_id, key = _create_route(admin_env, model)
    status, response = _proxy(admin_env, key, model, [
        {"role": "system", "content": SYSTEM_ONLY},
        {"role": "user", "content": "contents scope probe"},
    ])
    assert status == 200, response
    summary = _completed(admin_env, route_id)
    visible_ids = ref_ids(summary["input"]) + ref_ids(summary["output"])
    assert visible_ids

    for bad in ("", "0", "-1", "abc", "1,,2", "1,", "1.5", ",".join(str(i) for i in range(1, 258))):
        status, headers, raw = fetch_contents_raw(admin_env, bad)
        body = json.loads(raw)
        assert status == 400 and body["code"] == "invalid_observation_ids", (bad, status, body)
    status, body = http_request(
        "GET", f"{admin_env['admin']}/api/v1/observations/contents", headers=admin_env["auth"],
    )
    assert status == 400 and body["code"] == "invalid_observation_ids", body

    # Turn Chain 里的系统提示正文共享同一个内容存储，但不是 Observation 的引用：
    # 扫描整段 id 范围，端点返回的每一项都必须是摘要引用的正文，系统提示绝不出现。
    returned = fetch_contents(admin_env, list(range(1, 257)))
    assert set(visible_ids) <= set(returned)
    assert SYSTEM_ONLY not in json.dumps(returned)
    # 未知 id 静默省略，不报错也不占位。
    assert fetch_contents(admin_env, [10**9, visible_ids[0]]).keys() == {visible_ids[0]}
    # 重复 id 去重，顺序按首次出现。
    status, body = http_request(
        "GET",
        f"{admin_env['admin']}/api/v1/observations/contents?ids={visible_ids[-1]},{visible_ids[0]},{visible_ids[-1]}",
        headers=admin_env["auth"],
    )
    assert status == 200
    assert [item["content_id"] for item in body["data"]["contents"]] == list(dict.fromkeys(
        [visible_ids[-1], visible_ids[0]]
    ))


@pytest.mark.e2e
@pytest.mark.admin
def test_contents_etag_supports_conditional_requests_and_history_clear_hides_bodies(
    admin_env: dict[str, Any],
) -> None:
    model = "observation-contents-etag"
    route_id, key = _create_route(admin_env, model)
    status, response = _proxy(admin_env, key, model, [{"role": "user", "content": "etag probe"}])
    assert status == 200, response
    summary = _completed(admin_env, route_id)
    ids = ref_ids(summary["input"]) + ref_ids(summary["output"])

    status, headers, raw = fetch_contents_raw(admin_env, ids)
    assert status == 200
    etag = etag_of(headers)
    assert fetch_contents_raw(admin_env, ids)[1]["etag"] == etag
    status, headers, raw = fetch_contents_raw(admin_env, ids, headers={"If-None-Match": etag})
    assert status == 304 and raw == b"", (status, raw)
    status, headers, raw = fetch_contents_raw(admin_env, ids, headers={"If-None-Match": '"not-the-current-etag"'})
    assert status == 200 and etag_of(headers) == etag

    # 没有任何可见 Interaction 引用这些正文之后，端点把它们当作不存在。
    status, cleared = http_request(
        "DELETE", f"{admin_env['admin']}/api/v1/observations/history", headers=admin_env["auth"],
    )
    assert status == 200, cleared
    assert fetch_contents(admin_env, ids) == {}


@pytest.mark.e2e
@pytest.mark.admin
def test_contents_endpoint_requires_admin(admin_env: dict[str, Any]) -> None:
    status, _ = http_request("GET", f"{admin_env['admin']}/api/v1/observations/contents?ids=1")
    assert status == 401


def _scan_around(env: dict[str, Any], ids: list[int]) -> dict[int, dict[str, Any]]:
    return scan_contents(env, min(ids) - 300, max(ids) + 300)


@pytest.mark.e2e
@pytest.mark.admin
def test_contents_serve_every_principals_observation_but_no_turn_chain_only_content(
    admin_env: dict[str, Any],
) -> None:
    """管理员视角覆盖所有主体：两把 API Key 各自的 Observation 正文都可读，各自 Turn 独有的系统提示都不可读。"""
    systems = {"a": "system-only-for-principal-a-4c1d", "b": "system-only-for-principal-b-9f3a"}
    summaries = {}
    for name, system in systems.items():
        model = f"observation-contents-principal-{name}"
        route_id, key = _create_route(admin_env, model)
        status, response = _proxy(admin_env, key, model, [
            {"role": "system", "content": system},
            {"role": "user", "content": f"hello from principal {name}"},
        ])
        assert status == 200, response
        summaries[name] = _completed(admin_env, route_id)
    ids = {name: ref_ids(summary["input"]) + ref_ids(summary["output"]) for name, summary in summaries.items()}
    assert ids["a"] and ids["b"] and not set(ids["a"]) & set(ids["b"])

    returned = fetch_contents(admin_env, ids["a"] + ids["b"])
    assert set(returned) == set(ids["a"] + ids["b"])
    for name in systems:
        assert "".join(str(returned[item]["body"]) for item in ref_ids(summaries[name]["input"])) == f"hello from principal {name}"
    scanned = _scan_around(admin_env, ids["a"] + ids["b"])
    assert set(ids["a"] + ids["b"]) <= set(scanned)
    assert not [system for system in systems.values() if system in json.dumps(scanned)]


@pytest.mark.e2e
@pytest.mark.admin
def test_contents_never_expose_protected_thinking_that_only_a_marker_references(
    admin_env: dict[str, Any],
) -> None:
    """受保护思考签名由 History Marker 承载，续接需要它，但它不是 Observation 的引用，内容端点按不存在处理。"""
    from tests.e2e.storage.test_interaction_continuation import continuation_provider, replay_round

    model = "observation-contents-marker-only"
    with continuation_provider("google-gemini") as (upstream, received):
        env = {**admin_env, "mock": upstream}
        route_id, key = _create_route(env, model, retry_budget=0, protocol="google-gemini")
        history: list[dict[str, Any]] = [{"role": "user", "content": "marker-only probe"}]
        response = replay_round(env, key, model, history, False)
        calls = [item for item in response["output"] if item["type"] == "function_call"]
        assert calls
        history.extend({"type": "function_call_output", "call_id": call["call_id"], "output": "tool result"} for call in calls)
        replay_round(env, key, model, history, False)
        # 对照：第二轮上游请求里带着由 Marker 还原的签名，证明签名确实被存下并被续接使用。
        assert "synthetic-protected-0-0" in json.dumps(received[1])

    interaction = _wait_for(
        "continued Interaction",
        lambda: next(iter(_route_interactions(env, route_id)), None),
    )
    detail = _wait_for(
        "both Runs persisted",
        lambda: (lambda d: d if len(d["runs"]) == 2 and all(run["events"] for run in d["runs"]) else None)(
            _detail(env, interaction["id"])
        ),
    )
    ids = ref_ids(detail["interaction"]["input"]) + ref_ids(detail["interaction"]["output"])
    assert ids
    scanned = _scan_around(env, ids)
    assert set(ids) <= set(scanned)
    assert "synthetic-protected" not in json.dumps(scanned)


@pytest.mark.e2e
@pytest.mark.admin
def test_contents_and_detail_mask_credentials_and_hit_detail_degrades_after_mapping_reclaimed(
    admin_env: dict[str, Any],
) -> None:
    """凭据遮蔽同样作用于内容端点；映射被保留期清扫回收后，命中详情降级而不是报错。

    回收通过保存 ``log_retention_days`` 触发真实清扫；与 ``test_expired_reference_stays_literal…``
    相同，只把引用该映射的 Turn 节点与映射的发布时间回拨到 20 天前（既有回拨先例）。
    """
    with echo_provider() as (url, received):
        env = {**admin_env, "mock": url}
        model = "observation-contents-credential"
        route_id, key = _create_route(env, model)
        set_enabled(env, True)
        try:
            status, body = _proxy(env, key, model, [{"role": "user", "content": f"token {SECRET}"}])
            assert status == 200, body
            reference = REFERENCE.search(json.dumps(received[-1]["body"]))
            assert reference is not None, "the upstream saw a placeholder, not the credential"
            summary = _completed(env, route_id)
            row = _wait_for("created mapping row", lambda: key_discoveries(env, f"{model}-key"))[0]

            ids = ref_ids(summary["input"]) + ref_ids(summary["output"])
            returned = fetch_contents(env, ids)
            assert returned
            assert SECRET not in json.dumps(returned), "the contents endpoint masks credentials"
            assert "token ***" in json.dumps(returned)
            detail = _finished_detail(env, summary["id"])
            assert detail["input_text"] == "token ***"
            # 同一组引用只要遮蔽结果不变，ETag 保持稳定。
            first_etag = etag_of(fetch_contents_raw(env, ids)[1])
            assert etag_of(fetch_contents_raw(env, ids)[1]) == first_etag

            before = discovery_hit(env, row["id"])
            assert before["state"] == "available" and before["masked"] is True
            assert before["hit_basis"] == "secret"

            age_out_mapping(env, reference.group())

            # 映射回收后命中详情走既有退化路径：正文仍可取、仍被遮蔽，但依据由映射退化为规则。
            after = discovery_hit(env, row["id"])
            assert after["state"] == "available" and after["masked"] is True
            assert after["hit_basis"] == "rule"
            assert SECRET not in json.dumps(after) and after["text"] == "token " + MASK_TOKEN
            assert any(match["hit"] and match["rule_id"] == "github-pat" for match in after["matches"])
            # 主体已没有有效映射，内容端点不再扫描（遮蔽以现有映射为准），ETag 随正文变化而变化。
            assert etag_of(fetch_contents_raw(env, ids)[1]) != first_etag
        finally:
            set_enabled(env, False)


@pytest.mark.e2e
@pytest.mark.admin
def test_contents_masking_is_scoped_to_each_principals_own_mappings(admin_env: dict[str, Any]) -> None:
    """遮蔽只用 Interaction 所属主体自己的映射：另一把 Key 上没有映射的同一文本不会被主体 A 的映射遮蔽。"""
    with echo_provider() as (url, received):
        env = {**admin_env, "mock": url}
        masked_route, masked_key = _create_route(env, "observation-contents-scope-masked")
        plain_route, plain_key = _create_route(env, "observation-contents-scope-plain")
        set_enabled(env, True)
        try:
            status, body = _proxy(env, masked_key, "observation-contents-scope-masked",
                                  [{"role": "user", "content": f"scoped {SECRET}"}])
            assert status == 200, body
            masked_summary = _completed(env, masked_route)
        finally:
            set_enabled(env, False)
        # 保护关闭时同一明文落在另一主体的 Interaction 里，该主体没有任何映射。
        status, body = _proxy(env, plain_key, "observation-contents-scope-plain",
                              [{"role": "user", "content": f"scoped {SECRET}"}])
        assert status == 200, body
        plain_summary = _completed(env, plain_route)
        masked = fetch_contents(env, ref_ids(masked_summary["input"]))
        plain = fetch_contents(env, ref_ids(plain_summary["input"]))
        assert [item["body"] for item in masked.values()] == ["scoped ***"]
        assert [item["body"] for item in plain.values()] == [f"scoped {SECRET}"]
