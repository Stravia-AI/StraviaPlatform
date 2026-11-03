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
)
from tests.e2e.admin.test_observations import (
    _create_route,
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
    status, headers, raw = fetch_contents_raw(admin_env, ids, headers={"If-None-Match": '"' + "0" * 64 + '"'})
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
