from __future__ import annotations

import base64
import io
import json
import tempfile
import threading
import time
import zipfile
from pathlib import Path
from typing import Any, Callable
from urllib.parse import urlencode
from urllib.request import Request, urlopen

import pytest

from tests.common.helpers import (
    find_free_port,
    http_bytes,
    http_request,
    initialize_server,
    minimal_mock_provider,
    start_stravia_server,
    stop_stravia_server,
    wait_for_setup_token,
    wait_until_ready,
)


def _create_route(
    env: dict[str, Any], name: str, *, retry_budget: int | None = None
) -> tuple[str, str]:
    status, body = http_request(
        "POST",
        f"{env['admin']}/api/v1/providers",
        payload={
            "name": f"{name}-provider",
            "source": {
                "type": "custom",
                "vendor": "custom",
                "protocol": "openai",
                "base_url": env["mock"],
            },
            "credential": {"type": "api_key", "value": "upstream-secret"},
        },
        headers=env["auth"],
    )
    assert status == 200, body
    provider_id = body["data"]["id"]
    status, body = http_request(
        "POST",
        f"{env['admin']}/api/v1/providers/{provider_id}/models",
        payload={"model_id": "gpt-4o-mini", "metadata": {"name": name, "tool_call": True}},
        headers=env["auth"],
    )
    assert status == 201, body

    route_payload: dict[str, Any] = {
        "model_id": name,
        "display_name": f"{name} display",
        "target_provider": provider_id,
        "target_model": "gpt-4o-mini",
    }
    if retry_budget is not None:
        route_payload["targets"] = [
            {
                "provider_id": provider_id,
                "model": "gpt-4o-mini",
                "enabled": True,
                "priority": 0,
                "target_retry_budget": retry_budget,
                "target_cooldown_ms": 0,
            }
        ]
    status, body = http_request(
        "POST",
        f"{env['admin']}/api/v1/models",
        payload=route_payload,
        headers=env["auth"],
    )
    assert status == 200, body
    route_id = body["data"]["id"]
    status, body = http_request(
        "POST",
        f"{env['admin']}/api/v1/api-keys",
        payload={"name": f"{name}-key", "model_ids": [route_id]},
        headers=env["auth"],
    )
    assert status == 200, body
    return route_id, str(body["data"]["key"])


def _proxy(
    env: dict[str, Any], api_key: str, model: str, messages: list[dict[str, Any]],
    *, extra_headers: dict[str, str] | None = None, query: str = "",
    body_extra: dict[str, Any] | None = None,
) -> tuple[int, Any]:
    headers = {"authorization": f"Bearer {api_key}", **(extra_headers or {})}
    return http_request(
        "POST",
        f"{env['proxy']}/v1/chat/completions{query}",
        payload={"model": model, "messages": messages, **(body_extra or {})},
        headers=headers,
        timeout=15.0,
    )


def _forest(env: dict[str, Any], **query: object) -> dict[str, Any]:
    params = {"anchor_at": int(time.time() * 1000) + 1_000, "window_index": 0, **query}
    status, body = http_request(
        "GET",
        f"{env['admin']}/api/v1/observations/interactions?{urlencode(params)}",
        headers=env["auth"],
    )
    assert status == 200, body
    return body["data"]


@pytest.mark.e2e
@pytest.mark.admin
@pytest.mark.parametrize("resource", ["interactions", "rejections"])
@pytest.mark.parametrize("bounds", [
    {"start_at": 1_000},
    {"end_at": 2_000},
    {"start_at": 2_000, "end_at": 1_000},
    {"start_at": 1_000, "end_at": 1_000},
    {"start_at": 1_000, "end_at": 86_401_001},
])
def test_observation_explicit_range_rejects_invalid_bounds(
    admin_env: dict[str, Any], resource: str, bounds: dict[str, int],
) -> None:
    status, body = http_request(
        "GET",
        f"{admin_env['admin']}/api/v1/observations/{resource}?{urlencode(bounds)}",
        headers=admin_env["auth"],
    )
    assert status == 400, body


def _wait_for(description: str, probe: Callable[[], Any], timeout: float = 10.0) -> Any:
    deadline = time.time() + timeout
    last: Any = None
    while time.time() < deadline:
        last = probe()
        if last:
            return last
        time.sleep(0.1)
    pytest.fail(f"timed out waiting for {description}; last={last!r}")


def _wait_for_rejection_trace(env: dict[str, Any], rejection_id: str) -> dict[str, Any]:
    def finalized_rejection() -> dict[str, Any] | None:
        status, body = http_request(
            "GET",
            f"{env['admin']}/api/v1/observations/rejections/{rejection_id}",
            headers=env["auth"],
        )
        assert status == 200, body
        detail = body["data"]
        # 拒绝记录先于 Trace 最终落盘可见，记录存在不代表捕获已完成。
        return detail if (detail.get("trace") or {}).get("status") == "complete" else None

    return _wait_for("finalized Rejected Request Trace", finalized_rejection)


def _route_interactions(env: dict[str, Any], route_id: str) -> list[dict[str, Any]]:
    page = _forest(env, model=route_id, limit=100)
    return [
        interaction
        for root in page["roots"]
        for interaction in root["interactions"]
        if interaction["first_route_id"] == route_id
    ]


def _detail(env: dict[str, Any], interaction_id: str) -> dict[str, Any]:
    status, body = http_request(
        "GET",
        f"{env['admin']}/api/v1/observations/interactions/{interaction_id}",
        headers=env["auth"],
    )
    assert status == 200, body
    return body["data"]


def _sse_event(env: dict[str, Any], after: int) -> dict[str, Any]:
    request = Request(
        f"{env['admin']}/api/v1/observations/events?after={after}",
        headers=env["auth"],
    )
    with urlopen(request, timeout=5.0) as response:
        event = ""
        event_id = ""
        data: list[str] = []
        while True:
            line = response.readline().decode("utf-8").rstrip("\r\n")
            if not line:
                if data:
                    return {"event": event, "id": event_id, "data": json.loads("\n".join(data))}
                continue
            if line.startswith("event:"):
                event = line[6:].strip()
            elif line.startswith("id:"):
                event_id = line[3:].strip()
            elif line.startswith("data:"):
                data.append(line[5:].strip())


def _tool_call_message(response: dict[str, Any]) -> dict[str, Any]:
    return response["choices"][0]["message"]


@pytest.mark.e2e
@pytest.mark.admin
def test_observation_http_sse_usage_and_legacy_cutover(admin_env: dict[str, Any]) -> None:
    route_id, api_key = _create_route(admin_env, "observation-contract")

    before = _forest(admin_env)["snapshot_sequence"]
    status, response = _proxy(
        admin_env,
        api_key,
        "observation-contract",
        [{"role": "user", "content": "observation contract"}],
    )
    assert status == 200, response

    interactions = _wait_for(
        "completed Interaction",
        lambda: [
            interaction
            for interaction in _route_interactions(admin_env, route_id)
            if interaction["status"] == "completed"
        ],
        timeout=1.0,
    )
    assert len(interactions) == 1
    summary = interactions[0]
    assert summary["status"] == "completed"
    assert summary["input_preview"] == "observation contract"
    assert summary["visible_tail"] == "mock-ok-0"
    assert summary["usage"] == {
        "input_tokens": 3,
        "output_tokens": 2,
        "cache_read_tokens": None,
        "cache_write_tokens": None,
        "reasoning_tokens": None,
    }

    detail = _detail(admin_env, summary["id"])
    assert detail["interaction"]["input_preview"] == "observation contract"
    assert detail["interaction"]["usage"] == summary["usage"]
    assert len(detail["runs"]) == 1
    run = detail["runs"][0]
    assert run["client_output_committed"] is True
    assert run["status"] == "completed"
    sequences = [event["sequence"] for event in run["events"]]
    assert sequences == sorted(sequences)
    assert len(sequences) == len(set(sequences))
    assert {"run_admitted", "target_attempt_started", "usage_confirmed", "run_finished"} <= {
        event["kind"] for event in run["events"]
    }

    replay = _sse_event(admin_env, before)
    assert replay["event"] == "observation"
    assert int(replay["id"]) == replay["data"]["sequence"]
    assert replay["data"]["sequence"] > before

    for path in ("/api/v1/logs", "/api/v1/logs/removed"):
        status, _ = http_request("GET", f"{admin_env['admin']}{path}", headers=admin_env["auth"])
        assert status == 404


@pytest.mark.e2e
@pytest.mark.admin
def test_input_preview_filters_latest_user_before_unicode_limit_with_debug_off(
    admin_env: dict[str, Any],
) -> None:
    route_id, api_key = _create_route(admin_env, "observation-input-preview")
    status, state = http_request(
        "GET", f"{admin_env['admin']}/api/v1/observations/debug", headers=admin_env["auth"],
    )
    assert status == 200, state
    assert state["data"]["enabled"] is False
    status, response = _proxy(admin_env, api_key, "observation-input-preview", [
        {"role": "system", "content": "private-system-context"},
        {"role": "user", "content": "old-user-context"},
        {"role": "assistant", "content": "old-assistant-context"},
        {"role": "user", "content": [
            {"type": "text", "text": "api_key=" + "credential" * 600},
            {"type": "text", "text": "文" * 4200},
        ]},
    ])
    assert status == 200, response
    summary = _wait_for(
        "redacted ordinary-mode input preview",
        lambda: next((item for item in _route_interactions(admin_env, route_id)
                      if item["input_preview"] is not None), None),
    )
    preview = summary["input_preview"]
    assert preview == ("api_key=***\n" + "文" * 4200)[:4096]
    assert _detail(admin_env, summary["id"])["interaction"]["input_preview"] == preview


@pytest.mark.e2e
@pytest.mark.admin
def test_observation_debug_defaults_off_for_new_server(admin_env: dict[str, Any]) -> None:
    status, body = http_request(
        "GET",
        f"{admin_env['admin']}/api/v1/observations/debug",
        headers=admin_env["auth"],
    )
    assert status == 200, body
    assert body["data"]["enabled"] is False


@pytest.mark.e2e
@pytest.mark.admin
def test_observation_resources_require_admin_and_rejections_invent_no_principal(
    admin_env: dict[str, Any],
) -> None:
    protected = (
        "/api/v1/observations/interactions",
        "/api/v1/observations/rejections",
        "/api/v1/observations/events?after=0",
        "/api/v1/observations/debug",
    )
    for path in protected:
        status, _ = http_request("GET", f"{admin_env['admin']}{path}", timeout=1.0)
        assert status == 401

    status, _ = http_request(
        "POST",
        f"{admin_env['proxy']}/v1/chat/completions",
        payload={"model": "missing", "messages": [{"role": "user", "content": "no principal"}]},
    )
    assert status == 401

    def rejection() -> dict[str, Any] | None:
        status_, body = http_request(
            "GET", f"{admin_env['admin']}/api/v1/observations/rejections", headers=admin_env["auth"]
        )
        assert status_ == 200, body
        return next((item for item in body["data"]["items"] if item["status_code"] == 401), None)

    rejected = _wait_for("Rejected Request Observation", rejection)
    assert "principal" not in rejected
    assert "interaction_id" not in rejected
    assert "generation_root_id" not in rejected
    occurred_at = rejected["occurred_at"]
    for start, end, included in (
        (occurred_at, occurred_at + 1, True),
        (occurred_at - 1, occurred_at, False),
        (occurred_at + 1, occurred_at + 2, False),
        (occurred_at, occurred_at + 86_400_000, True),
    ):
        params = urlencode({"start_at": start, "end_at": end, "anchor_at": 0, "window_index": 9})
        range_status, range_body = http_request(
            "GET",
            f"{admin_env['admin']}/api/v1/observations/rejections?{params}",
            headers=admin_env["auth"],
        )
        assert range_status == 200, range_body
        assert (rejected["id"] in {item["id"] for item in range_body["data"]["items"]}) is included
    status, body = http_request(
        "GET",
        f"{admin_env['admin']}/api/v1/observations/rejections/{rejected['id']}",
        headers=admin_env["auth"],
    )
    assert status == 200, body
    events = body["data"]["events"]
    assert any(event["kind"] == "request_rejected" for event in events)
    assert all(event["rejection_id"] == rejected["id"] for event in events)
    assert all(event.get("interaction_id") is None for event in events)
    assert all(event.get("run_id") is None for event in events)


@pytest.mark.e2e
@pytest.mark.admin
def test_tool_loop_concurrent_branches_and_new_user_group_at_interaction_seam(
    admin_env: dict[str, Any],
) -> None:
    tools = [{"type": "function", "function": {"name": "local_probe", "parameters": {"type": "object"}}}]

    loop_route, loop_key = _create_route(admin_env, "observation-tool-loop")
    messages: list[dict[str, Any]] = [{"role": "user", "content": "observation-tool-loop"}]
    for round_number in range(1, 4):
        status, response = http_request(
            "POST",
            f"{admin_env['proxy']}/v1/chat/completions",
            payload={"model": "observation-tool-loop", "messages": messages, "tools": tools},
            headers={"authorization": f"Bearer {loop_key}"},
        )
        assert status == 200, response
        assistant = _tool_call_message(response)
        messages.extend(
            [assistant, {"role": "tool", "tool_call_id": assistant["tool_calls"][0]["id"], "content": f"round {round_number}"}]
        )
    status, response = http_request(
        "POST",
        f"{admin_env['proxy']}/v1/chat/completions",
        payload={"model": "observation-tool-loop", "messages": messages, "tools": tools},
        headers={"authorization": f"Bearer {loop_key}"},
    )
    assert status == 200, response
    loop_interactions = _wait_for(
        "three-round tool Interaction", lambda: _route_interactions(admin_env, loop_route)
    )
    assert len(loop_interactions) == 1
    loop_detail = _wait_for(
        "four persisted tool-loop Runs",
        lambda: (lambda detail: detail if len(detail["runs"]) == 4 else None)(
            _detail(admin_env, loop_interactions[0]["id"])
        ),
    )
    assert len(loop_detail["runs"]) == 4
    assert loop_detail["interaction"]["input_preview"] == "observation-tool-loop"
    assert all(not run["debug_enabled"] and not run["debug_events"] for run in loop_detail["runs"])
    ordinary_events = [event for run in loop_detail["runs"] for event in run["events"]]
    assert sorted(
        event["payload"]["input"]["round"]
        for event in ordinary_events if event["kind"] == "client_tool_handoff"
    ) == [1, 2, 3]
    assert {
        event["payload"]["content"]
        for event in ordinary_events if event["kind"] == "client_tool_result"
    } == {"round 1", "round 2", "round 3"}

    branch_route, branch_key = _create_route(admin_env, "observation-branch")
    root_messages = [{"role": "user", "content": "observation-branch"}]
    status, root_response = http_request(
        "POST",
        f"{admin_env['proxy']}/v1/chat/completions",
        payload={"model": "observation-branch", "messages": root_messages, "tools": tools},
        headers={"authorization": f"Bearer {branch_key}"},
    )
    assert status == 200, root_response
    assistant = _tool_call_message(root_response)
    outcomes: dict[str, tuple[int, Any]] = {}

    def continue_branch(value: str) -> None:
        outcomes[value] = http_request(
                "POST",
                f"{admin_env['proxy']}/v1/chat/completions",
                payload={
                    "model": "observation-branch",
                    "messages": root_messages
                    + [assistant, {"role": "tool", "tool_call_id": assistant["tool_calls"][0]["id"], "content": value}],
                    "tools": tools,
                },
                headers={"authorization": f"Bearer {branch_key}"},
        )

    threads = [threading.Thread(target=continue_branch, args=(value,)) for value in ("left", "right")]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    assert [status for status, _ in outcomes.values()] == [200, 200]

    branch_interactions = _wait_for(
        "concurrent branch Interaction", lambda: _route_interactions(admin_env, branch_route)
    )
    assert len(branch_interactions) == 1
    branch_detail = _wait_for(
        "three branch Runs",
        lambda: (lambda detail: detail if len(detail["runs"]) == 3 else None)(
            _detail(admin_env, branch_interactions[0]["id"])
        ),
    )
    children = [run for run in branch_detail["runs"] if run["parent_run_id"] is not None]
    assert len(children) == 2
    assert len({run["parent_run_id"] for run in children}) == 1

    completed_history = root_messages + [
        assistant,
        {"role": "tool", "tool_call_id": assistant["tool_calls"][0]["id"], "content": "left"},
        _tool_call_message(outcomes["left"][1]),
    ]
    status, response = _proxy(
        admin_env,
        branch_key,
        "observation-branch",
        completed_history + [{"role": "user", "content": "a later user turn"}],
        body_extra={"tools": tools},
    )
    assert status == 200, response
    interactions = _wait_for(
        "new-user child Interaction",
        lambda: (lambda items: items if len(items) == 2 else None)(
            _route_interactions(admin_env, branch_route)
        ),
    )
    child = next(item for item in interactions if item["id"] != branch_interactions[0]["id"])
    assert child["parent_interaction_id"] == branch_interactions[0]["id"]
    assert child["input_preview"] == "a later user turn"
    assert _detail(admin_env, branch_interactions[0]["id"])["interaction"]["input_preview"] == "observation-branch"


@pytest.mark.e2e
@pytest.mark.admin
def test_superseded_running_branch_finishes_as_user_interrupted_in_live_and_bundle(
    admin_env: dict[str, Any],
) -> None:
    route_id, api_key = _create_route(admin_env, "observation-interruption")
    tools = [
        {
            "type": "function",
            "function": {"name": "local_probe", "parameters": {"type": "object"}},
        }
    ]
    root_messages = [{"role": "user", "content": "observation-branch interruption-root"}]
    status, root_response = _proxy(
        admin_env,
        api_key,
        "observation-interruption",
        root_messages,
        body_extra={"tools": tools},
    )
    assert status == 200, root_response
    assistant = _tool_call_message(root_response)
    tool_call_id = assistant["tool_calls"][0]["id"]

    old_outcome: list[tuple[int, Any]] = []
    old_worker = threading.Thread(
        target=lambda: old_outcome.append(
            _proxy(
                admin_env,
                api_key,
                "observation-interruption",
                root_messages
                + [
                    assistant,
                    {
                        "role": "tool",
                        "tool_call_id": tool_call_id,
                        "content": "observation-delay superseded-old-branch",
                    },
                ],
                body_extra={"tools": tools},
            )
        )
    )
    old_worker.start()

    original = _wait_for(
        "running child branch",
        lambda: next(
            (
                detail
                for interaction in _route_interactions(admin_env, route_id)
                if (
                    detail := _detail(admin_env, interaction["id"])
                )["interaction"]["status"]
                == "running"
                and len(detail["runs"]) == 2
                and any(run["parent_run_id"] is not None for run in detail["runs"])
            ),
            None,
        ),
    )
    old_run = next(run for run in original["runs"] if run["parent_run_id"] is not None)
    assert old_run["status"] == "running"
    assert old_run["user_interrupted"] is False

    status, replacement_response = _proxy(
        admin_env,
        api_key,
        "observation-interruption",
        root_messages
        + [
            assistant,
            {
                "role": "tool",
                "tool_call_id": tool_call_id,
                "content": "replacement-branch",
            },
            {"role": "user", "content": "superseding user input"},
        ],
        body_extra={"tools": tools},
    )
    assert status == 200, replacement_response
    assert replacement_response["choices"][0]["message"]["content"] == "mock-ok-1"

    interrupted_while_active = _wait_for(
        "truthfully active interrupted branch",
        lambda: (lambda detail: detail if any(
            run["id"] == old_run["id"]
            and run["status"] == "running"
            and run["user_interrupted"] is True
            and run["finished_at"] is None
            for run in detail["runs"]
        ) else None)(_detail(admin_env, original["interaction"]["id"])),
    )
    assert interrupted_while_active["interaction"]["status"] == "running"

    old_worker.join(timeout=10.0)
    assert old_outcome and old_outcome[0][0] == 200
    assert old_outcome[0][1]["choices"][0]["message"]["content"] == "mock-ok-1"
    finished = _wait_for(
        "terminal user interruption",
        lambda: (lambda detail: detail if detail["interaction"]["status"] == "interrupted"
            and next(run for run in detail["runs"] if run["id"] == old_run["id"])["status"]
            == "user_interrupted"
            else None)(_detail(admin_env, original["interaction"]["id"])),
    )
    terminal_run = next(run for run in finished["runs"] if run["id"] == old_run["id"])
    assert terminal_run["terminal_reason"] == "user_interrupted"
    assert terminal_run["user_interrupted"] is True
    assert terminal_run["finished_at"] is not None
    finished_event = next(
        event for event in terminal_run["events"] if event["kind"] == "run_finished"
    )
    assert finished_event["payload"]["status"] == "user_interrupted"
    assert finished_event["payload"]["terminal_reason"] == "user_interrupted"

    status, ticket = http_request(
        "POST",
        f"{admin_env['admin']}/api/v1/observations/interactions/{original['interaction']['id']}/debug-bundle-tickets",
        payload={},
        headers=admin_env["auth"],
    )
    assert status == 200, ticket
    download_url = str(ticket["data"]["download_url"])
    if download_url.startswith("/"):
        download_url = f"{admin_env['admin']}{download_url}"
    status, _, archive = http_bytes("GET", download_url)
    assert status == 200
    with zipfile.ZipFile(io.BytesIO(archive)) as bundle:
        manifest = json.loads(bundle.read("manifest.json"))
        summary = json.loads(bundle.read("interaction.json"))
        assert manifest["status"] == "interrupted"
        assert summary["status"] == "interrupted"
        bundled_finish = next(
            event
            for event in summary["events"]
            if event["run_id"] == old_run["id"] and event["kind"] == "run_finished"
        )
        assert bundled_finish["payload"]["status"] == "user_interrupted"
        assert bundled_finish["payload"]["terminal_reason"] == "user_interrupted"


@pytest.mark.e2e
@pytest.mark.admin
def test_parentless_root_retry_respects_exact_principal_and_concurrency_boundaries(
    admin_env: dict[str, Any],
) -> None:
    route_id, first_key = _create_route(
        admin_env, "observation-root-retry", retry_budget=0
    )
    status, second_principal = http_request(
        "POST",
        f"{admin_env['admin']}/api/v1/api-keys",
        payload={"name": "observation-root-retry-second-key", "model_ids": [route_id]},
        headers=admin_env["auth"],
    )
    assert status == 200, second_principal
    second_key = str(second_principal["data"]["key"])

    exact_messages = [
        {"role": "user", "content": "observation-root-retry exact-boundary"}
    ]
    first_status, _ = _proxy(
        admin_env, first_key, "observation-root-retry", exact_messages
    )
    second_status, second = _proxy(
        admin_env, first_key, "observation-root-retry", exact_messages
    )
    assert first_status >= 500
    assert second_status == 200, second
    exact = _wait_for(
        "grouped exact root retry",
        lambda: (lambda items: items if items and len(
            _detail(admin_env, items[0]["id"])["runs"]
        ) == 2 else None)(_route_interactions(admin_env, route_id)),
    )
    assert len(exact) == 1

    committed_route, committed_key = _create_route(admin_env, "observation-commit-boundary")
    committed_messages = [{"role": "user", "content": "same committed root"}]
    for _ in range(2):
        status, response = _proxy(
            admin_env,
            committed_key,
            "observation-commit-boundary",
            committed_messages,
        )
        assert status == 200, response
    committed = _wait_for(
        "separate committed root repetitions",
        lambda: (lambda items: items if len(items) == 2
            and all(item["status"] == "completed" for item in items) else None)(
            _route_interactions(admin_env, committed_route)
        ),
    )
    assert all(item["status"] == "completed" for item in committed)

    principal_messages = [
        {"role": "user", "content": "observation-root-retry principal-boundary"}
    ]
    assert _proxy(admin_env, first_key, "observation-root-retry", principal_messages)[0] >= 500
    # The upstream scenario is now recoverable, but the other API key is a different Principal.
    status, response = _proxy(
        admin_env,
        second_key,
        "observation-root-retry",
        principal_messages,
    )
    assert status == 200, response
    first_principal = _wait_for(
        "failed first-principal root",
        lambda: [
            item
            for item in _route_interactions(admin_env, route_id)
            if item["id"] != exact[0]["id"]
        ],
    )
    assert first_principal
    principal_roots = _wait_for(
        "separate second-principal root",
        lambda: (lambda items: items if len(items) >= 3 else None)(
            _route_interactions(admin_env, route_id)
        ),
    )
    assert len({item["id"] for item in principal_roots}) >= 3

    changed_a = [{"role": "user", "content": "observation-root-retry fingerprint-a"}]
    changed_b = [{"role": "user", "content": "observation-root-retry fingerprint-b"}]
    assert _proxy(admin_env, first_key, "observation-root-retry", changed_a)[0] >= 500
    assert _proxy(admin_env, first_key, "observation-root-retry", changed_b)[0] >= 500
    changed = _wait_for(
        "distinct fingerprint roots",
        lambda: _route_interactions(admin_env, route_id)
        if len(_route_interactions(admin_env, route_id)) >= 5
        else None,
    )
    assert len({item["id"] for item in changed}) >= 5

    concurrent_route, concurrent_key = _create_route(admin_env, "observation-delay")
    results: list[tuple[int, Any]] = []
    threads = [
        threading.Thread(
            target=lambda: results.append(
                _proxy(
                    admin_env,
                    concurrent_key,
                    "observation-delay",
                    [{"role": "user", "content": "observation-delay concurrent-identical"}],
                )
            )
        )
        for _ in range(2)
    ]
    for thread in threads:
        thread.start()
    running = _wait_for(
        "two concurrent identical roots",
        lambda: [
            item
            for item in _route_interactions(admin_env, concurrent_route)
            if item["status"] == "running"
        ]
        if len(
            [
                item
                for item in _route_interactions(admin_env, concurrent_route)
                if item["status"] == "running"
            ]
        )
        == 2
        else None,
    )
    assert len(running) == 2
    for thread in threads:
        thread.join(timeout=10.0)
    assert sorted(status for status, _ in results) == [200, 200]


@pytest.mark.e2e
@pytest.mark.admin
def test_debug_switch_is_snapshotted_per_run_within_one_interaction(
    admin_env: dict[str, Any],
) -> None:
    status, _ = http_request(
        "PUT",
        f"{admin_env['admin']}/api/v1/observations/debug",
        payload={"enabled": True, "confirmed": True},
        headers=admin_env["auth"],
    )
    assert status == 200
    route_id, api_key = _create_route(admin_env, "observation-tool-loop-debug")
    tools = [{"type": "function", "function": {"name": "local_probe", "parameters": {"type": "object"}}}]
    messages: list[dict[str, Any]] = [
        {"role": "user", "content": "observation-tool-loop debug snapshots"}
    ]

    for enabled, confirmed in ((True, True), (False, False), (True, True)):
        status, state = http_request(
            "PUT",
            f"{admin_env['admin']}/api/v1/observations/debug",
            payload={"enabled": enabled, "confirmed": confirmed},
            headers=admin_env["auth"],
        )
        assert status == 200, state
        status, response = http_request(
            "POST",
            f"{admin_env['proxy']}/v1/chat/completions",
            payload={
                "model": "observation-tool-loop-debug",
                "messages": messages,
                "tools": tools,
            },
            headers={"authorization": f"Bearer {api_key}"},
        )
        assert status == 200, response
        assistant = _tool_call_message(response)
        messages.extend(
            [
                assistant,
                {
                    "role": "tool",
                    "tool_call_id": assistant["tool_calls"][0]["id"],
                    "content": f"snapshot-{enabled}-{len(messages)}",
                },
            ]
        )

    interaction = _wait_for(
        "mixed Debug Interaction", lambda: _route_interactions(admin_env, route_id)
    )[0]
    detail = _wait_for(
        "three Debug snapshot Runs",
        lambda: (lambda value: value if len(value["runs"]) == 3 else None)(
            _detail(admin_env, interaction["id"])
        ),
    )
    assert [run["debug_enabled"] for run in detail["runs"]] == [True, False, True]
    assert detail["interaction"]["debug_status"] == "partial"
    assert detail["runs"][1]["trace"] is None
    status, _ = http_request(
        "PUT",
        f"{admin_env['admin']}/api/v1/observations/debug",
        payload={"enabled": False, "confirmed": False},
        headers=admin_env["auth"],
    )
    assert status == 200


@pytest.mark.e2e
@pytest.mark.admin
def test_debug_snapshot_redaction_bundle_ticket_and_clear_active_history(
    admin_env: dict[str, Any],
) -> None:
    status, _ = http_request(
        "PUT",
        f"{admin_env['admin']}/api/v1/observations/debug",
        payload={"enabled": False, "confirmed": False},
        headers=admin_env["auth"],
    )
    assert status == 200
    status, state = http_request(
        "GET", f"{admin_env['admin']}/api/v1/observations/debug", headers=admin_env["auth"]
    )
    assert status == 200, state
    assert state["data"]["enabled"] is False
    assert state["data"]["run_limit_bytes"] == 64 * 1024 * 1024
    assert state["data"]["total_limit_bytes"] == 2 * 1024 * 1024 * 1024
    status, refused = http_request(
        "PUT",
        f"{admin_env['admin']}/api/v1/observations/debug",
        payload={"enabled": True, "confirmed": False},
        headers=admin_env["auth"],
    )
    assert status == 400
    assert refused["code"] == "debug_confirmation_required"
    status, state = http_request(
        "PUT",
        f"{admin_env['admin']}/api/v1/observations/debug",
        payload={"enabled": True, "confirmed": True},
        headers=admin_env["auth"],
    )
    assert status == 200, state
    assert state["data"]["enabled"] is True

    rejected_at = int(time.time() * 1000)
    status, _ = http_request(
        "POST",
        f"{admin_env['proxy']}/v1/chat/completions",
        payload={"model": "missing", "messages": []},
    )
    assert status == 401

    def debug_rejection() -> dict[str, Any] | None:
        status_, body = http_request(
            "GET",
            f"{admin_env['admin']}/api/v1/observations/rejections",
            headers=admin_env["auth"],
        )
        assert status_ == 200, body
        return next(
            (
                item
                for item in body["data"]["items"]
                if item["occurred_at"] >= rejected_at and item["debug_enabled"]
            ),
            None,
        )

    rejected = _wait_for("Debug Rejected Request", debug_rejection)
    rejected_detail = _wait_for_rejection_trace(admin_env, rejected["id"])
    rejected_directions = {
        event.get("direction")
        for event in rejected_detail["debug_events"]
        if isinstance(event, dict) and event.get("layer") == "wire"
    }
    assert rejected_directions == {"client_to_platform", "platform_to_client"}
    assert "upstream_request" not in rejected_directions
    assert "upstream_response" not in rejected_directions

    route_id, api_key = _create_route(admin_env, "observation-debug")
    sentinel = "STRAVIA_CREDENTIAL_SENTINEL_7da9"
    status, response = _proxy(
        admin_env,
        api_key,
        "observation-debug",
        [{"role": "user", "content": "debug payload"}],
        extra_headers={"x-api-key": sentinel, "cookie": f"session={sentinel}"},
        query=f"?access_token={sentinel}",
        body_extra={"metadata": {"nested": {"password": sentinel}}},
    )
    assert status == 200, response
    interaction = _wait_for("debug Interaction", lambda: _route_interactions(admin_env, route_id))[0]
    detail = _wait_for(
        "finalized Debug trace",
        lambda: (lambda value: value if value["runs"][0].get("trace")
            and value["runs"][0]["trace"]["status"] == "complete" else None)(
            _detail(admin_env, interaction["id"])
        ),
    )
    run = detail["runs"][0]
    assert run["debug_enabled"] is True
    assert run["trace"]["enabled"] is True

    stages = [event.get("stage") for event in run["debug_events"] if isinstance(event, dict)]
    required = {
        "decoded_request",
        "restored_request",
        "effective_model_request",
        "canonical_request",
        "canonical_delta",
        "canonical_terminal_response",
        "response_after_hook",
        "client_projection_event",
        "delivery_terminal",
    }
    assert required <= set(stages)
    directions = {event.get("direction") for event in run["debug_events"] if isinstance(event, dict)}
    assert {
        "client_to_platform",
        "upstream_request",
        "upstream_response",
        "platform_to_client",
    } <= directions

    status, ticket = http_request(
        "POST",
        f"{admin_env['admin']}/api/v1/observations/interactions/{interaction['id']}/debug-bundle-tickets",
        payload={"through_sequence": detail["snapshot_sequence"]},
        headers=admin_env["auth"],
    )
    assert status == 200, ticket
    ticket_data = ticket["data"]
    download_url = ticket_data["download_url"]
    if download_url.startswith("/"):
        download_url = f"{admin_env['admin']}{download_url}"
    status, headers, archive = http_bytes("GET", download_url)
    assert status == 200
    assert "application/zip" in headers["content-type"]
    assert headers["cache-control"] == "no-store"
    assert headers["referrer-policy"] == "no-referrer"
    assert headers["x-content-type-options"] == "nosniff"
    assert "content-length" not in headers
    with zipfile.ZipFile(io.BytesIO(archive)) as bundle:
        names = bundle.namelist()
        assert names[:3] == ["manifest.json", "interaction.json", "README.txt"]
        manifest = json.loads(bundle.read("manifest.json"))
        assert manifest["through_event_sequence"] == ticket_data["through_sequence"]
        assert manifest["kind"] == "interaction"
        assert manifest["status"] == "completed"
        assert manifest["completeness"] == "complete"
        assert manifest["fidelity"] == "application_protocol_capture_not_packet_capture"
        readme = bundle.read("README.txt").lower()
        assert b"application-protocol" in readme
        assert b"not a packet capture" in readme
        for name in names:
            assert sentinel.encode() not in bundle.read(name)

    replay_status, _, replay_body = http_bytes("GET", download_url)
    random_status, _, random_body = http_bytes(
        "GET", f"{admin_env['admin']}/api/v1/observations/debug-bundles/not-a-ticket"
    )
    assert replay_status == random_status
    assert replay_body == random_body
    assert sentinel.encode() not in replay_body

    artifacts = Path(admin_env["data_dir"])
    for path in artifacts.rglob("*"):
        if path.is_file():
            assert sentinel.encode() not in path.read_bytes(), path
    assert sentinel not in "\n".join(admin_env["logs"])
    assert sentinel not in json.dumps(detail)

    waiting_route, waiting_key = _create_route(admin_env, "observation-branch-clear")
    status, waiting_response = http_request(
        "POST",
        f"{admin_env['proxy']}/v1/chat/completions",
        payload={
            "model": "observation-branch-clear",
            "messages": [{"role": "user", "content": "observation-branch clear"}],
            "tools": [
                {
                    "type": "function",
                    "function": {"name": "local_probe", "parameters": {"type": "object"}},
                }
            ],
        },
        headers={"authorization": f"Bearer {waiting_key}"},
    )
    assert status == 200, waiting_response
    waiting = _wait_for(
        "waiting-client Interaction",
        lambda: next(
            (
                item
                for item in _route_interactions(admin_env, waiting_route)
                if item["status"] == "waiting_client"
            ),
            None,
        ),
    )

    delay_route, delay_key = _create_route(admin_env, "observation-delay-clear")
    result: list[tuple[int, Any]] = []
    worker = threading.Thread(
        target=lambda: result.append(
            _proxy(
                admin_env,
                delay_key,
                "observation-delay-clear",
                [{"role": "user", "content": "observation-delay"}],
            )
        )
    )
    worker.start()
    active = _wait_for(
        "running Interaction",
        lambda: next(
            (item for item in _route_interactions(admin_env, delay_route) if item["status"] == "running"),
            None,
        ),
    )
    status, cleared = http_request(
        "DELETE",
        f"{admin_env['admin']}/api/v1/observations/history",
        headers=admin_env["auth"],
    )
    assert status == 200, cleared
    assert cleared["data"]["skipped_active"] >= 2
    assert any(item["id"] == active["id"] for item in _route_interactions(admin_env, delay_route))
    assert any(item["id"] == waiting["id"] for item in _route_interactions(admin_env, waiting_route))
    worker.join(timeout=10.0)
    assert result and result[0][0] == 200


@pytest.mark.e2e
@pytest.mark.admin
def test_rejected_debug_bundle_records_real_error_without_inventing_execution(
    admin_env: dict[str, Any],
) -> None:
    status, state = http_request(
        "PUT",
        f"{admin_env['admin']}/api/v1/observations/debug",
        payload={"enabled": True, "confirmed": True},
        headers=admin_env["auth"],
    )
    assert status == 200, state
    before_page = _forest(admin_env)
    before = before_page["snapshot_sequence"]
    before_interactions = {
        item["id"]
        for root in before_page["roots"]
        for item in root["interactions"]
    }
    rejected_at = int(time.time() * 1000)

    status, rejected_response = http_request(
        "POST",
        f"{admin_env['proxy']}/v1/chat/completions",
        payload={"model": "missing", "messages": [{"role": "user", "content": "reject me"}]},
    )
    assert status == 401, rejected_response
    error_message = rejected_response["error"]["message"]
    assert error_message

    def latest_rejection() -> dict[str, Any] | None:
        status_, body = http_request(
            "GET",
            f"{admin_env['admin']}/api/v1/observations/rejections",
            headers=admin_env["auth"],
        )
        assert status_ == 200, body
        return next(
            (
                item
                for item in body["data"]["items"]
                if item["status_code"] == 401
                and item["debug_enabled"]
                and item["occurred_at"] >= rejected_at
            ),
            None,
        )

    rejected = _wait_for("captured Rejected Request", latest_rejection)
    detail = _wait_for_rejection_trace(admin_env, rejected["id"])
    assert {
        event["direction"]
        for event in detail["debug_events"]
        if event.get("layer") == "wire"
    } == {"client_to_platform", "platform_to_client"}
    after_page = _forest(admin_env)
    assert after_page["snapshot_sequence"] > before
    assert {
        item["id"]
        for root in after_page["roots"]
        for item in root["interactions"]
    } == before_interactions

    status, ticket = http_request(
        "POST",
        f"{admin_env['admin']}/api/v1/observations/rejections/{rejected['id']}/debug-bundle-tickets",
        payload={"through_sequence": detail["snapshot_sequence"]},
        headers=admin_env["auth"],
    )
    assert status == 200, ticket
    download_url = str(ticket["data"]["download_url"])
    if download_url.startswith("/"):
        download_url = f"{admin_env['admin']}{download_url}"
    status, _, archive = http_bytes("GET", download_url)
    assert status == 200
    with zipfile.ZipFile(io.BytesIO(archive)) as bundle:
        assert set(bundle.namelist()) == {
            "manifest.json",
            "rejected-request.json",
            "README.txt",
            "trace/events.jsonl",
        }
        manifest = json.loads(bundle.read("manifest.json"))
        summary = json.loads(bundle.read("rejected-request.json"))
        assert manifest["schema_version"] == 1
        assert manifest["kind"] == "rejected_request"
        assert manifest["resource_id"] == rejected["id"]
        assert "runs" not in manifest
        assert manifest["rejected_request"]["rejection_id"] == rejected["id"]
        assert "run_id" not in manifest["rejected_request"]
        assert summary["rejection_id"] == rejected["id"]
        assert summary.get("interaction_id") is None
        assert all(
            event.get("interaction_id") is None and event.get("run_id") is None
            for event in summary["events"]
        )
        trace_payloads: list[bytes] = []
        for line in bundle.read("trace/events.jsonl").splitlines():
            event = json.loads(line)
            payload = event.get("payload")
            if event.get("payload_encoding") == "base64" and isinstance(payload, str):
                trace_payloads.append(base64.b64decode(payload))
            else:
                trace_payloads.append(json.dumps(payload, sort_keys=True).encode())
        assert error_message.encode() in b"\n".join(trace_payloads)


@pytest.mark.e2e
@pytest.mark.admin
def test_running_bundle_is_fixed_at_its_watermark_and_never_claims_completion(
    admin_env: dict[str, Any],
) -> None:
    status, state = http_request(
        "PUT",
        f"{admin_env['admin']}/api/v1/observations/debug",
        payload={"enabled": True, "confirmed": True},
        headers=admin_env["auth"],
    )
    assert status == 200, state
    route_id, api_key = _create_route(admin_env, "observation-delay-snapshot")
    outcome: list[tuple[int, Any]] = []
    worker = threading.Thread(
        target=lambda: outcome.append(
            _proxy(
                admin_env,
                api_key,
                "observation-delay-snapshot",
                [{"role": "user", "content": "observation-delay snapshot while genuinely active"}],
            )
        )
    )
    worker.start()
    running = _wait_for(
        "running snapshot Interaction",
        lambda: next(
            (
                item
                for item in _route_interactions(admin_env, route_id)
                if item["status"] == "running"
            ),
            None,
        ),
    )
    running_detail = _detail(admin_env, running["id"])
    through_sequence = running_detail["snapshot_sequence"]
    status, ticket = http_request(
        "POST",
        f"{admin_env['admin']}/api/v1/observations/interactions/{running['id']}/debug-bundle-tickets",
        payload={"through_sequence": through_sequence},
        headers=admin_env["auth"],
    )
    assert status == 200, ticket
    assert ticket["data"]["through_sequence"] == through_sequence

    worker.join(timeout=10.0)
    assert outcome and outcome[0][0] == 200
    finished = _wait_for(
        "completed live Interaction",
        lambda: (lambda value: value if value["interaction"]["status"] == "completed" else None)(
            _detail(admin_env, running["id"])
        ),
    )
    assert finished["snapshot_sequence"] > through_sequence

    download_url = str(ticket["data"]["download_url"])
    if download_url.startswith("/"):
        download_url = f"{admin_env['admin']}{download_url}"
    status, _, archive = http_bytes("GET", download_url)
    assert status == 200
    with zipfile.ZipFile(io.BytesIO(archive)) as bundle:
        manifest = json.loads(bundle.read("manifest.json"))
        summary = json.loads(bundle.read("interaction.json"))
        assert manifest["through_event_sequence"] == through_sequence
        assert manifest["status"] == "running"
        assert manifest["completeness"] != "complete"
        assert summary["status"] == "running"
        assert summary["through_event_sequence"] == through_sequence
        assert all(
            event["sequence"] <= through_sequence for event in summary["events"]
        )
        for name in bundle.namelist():
            if not name.startswith("runs/"):
                continue
            for line in bundle.read(name).splitlines():
                assert json.loads(line)["sequence"] <= through_sequence


@pytest.mark.e2e
@pytest.mark.admin
def test_bundle_tickets_expire_after_real_ttl_and_fail_uniformly(
    admin_env: dict[str, Any],
) -> None:
    route_id, api_key = _create_route(admin_env, "observation-ticket-contract")
    status, response = _proxy(
        admin_env,
        api_key,
        "observation-ticket-contract",
        [{"role": "user", "content": "resource-bound ticket"}],
    )
    assert status == 200, response
    interaction = _wait_for(
        "ticket Interaction", lambda: _route_interactions(admin_env, route_id)
    )[0]

    def issue_ticket() -> dict[str, Any]:
        status_, body = http_request(
            "POST",
            f"{admin_env['admin']}/api/v1/observations/interactions/{interaction['id']}/debug-bundle-tickets",
            payload={},
            headers=admin_env["auth"],
        )
        assert status_ == 200, body
        return body["data"]

    issued_at = int(time.time() * 1000)
    expiring = issue_ticket()
    assert 59_000 <= expiring["expires_at"] - issued_at <= 61_000
    usable = issue_ticket()
    usable_url = str(usable["download_url"])
    if usable_url.startswith("/"):
        usable_url = f"{admin_env['admin']}{usable_url}"
    status, _, archive = http_bytes("GET", usable_url)
    assert status == 200
    with zipfile.ZipFile(io.BytesIO(archive)) as bundle:
        manifest = json.loads(bundle.read("manifest.json"))
        assert manifest["kind"] == "interaction"
        assert manifest["resource_id"] == interaction["id"]
        assert manifest["runs"][0]["run_id"] == _detail(admin_env, interaction["id"])["runs"][0]["id"]
        assert "rejection_id" not in manifest["runs"][0]

    replay = http_bytes("GET", usable_url)
    random = http_bytes(
        "GET",
        f"{admin_env['admin']}/api/v1/observations/debug-bundles/{'0' * 64}",
    )
    assert (replay[0], replay[2]) == (random[0], random[2])
    assert replay[1]["cache-control"] == random[1]["cache-control"] == "no-store"

    remaining = (expiring["expires_at"] - int(time.time() * 1000)) / 1000
    if remaining > 0:
        time.sleep(remaining + 0.1)
    expiring_url = str(expiring["download_url"])
    if expiring_url.startswith("/"):
        expiring_url = f"{admin_env['admin']}{expiring_url}"
    expired = http_bytes("GET", expiring_url)
    assert (expired[0], expired[2]) == (random[0], random[2])
    assert expired[1]["cache-control"] == "no-store"


@pytest.mark.e2e
@pytest.mark.admin
def test_root_batches_filters_and_fixed_anchor_reload_preserve_complete_context(
    admin_env: dict[str, Any],
) -> None:
    parent_route, _ = _create_route(admin_env, "observation-branch-context")
    child_route, _ = _create_route(admin_env, "observation-child-context")
    status, key_body = http_request(
        "POST",
        f"{admin_env['admin']}/api/v1/api-keys",
        payload={
            "name": "observation-context-key",
            "model_ids": [parent_route, child_route],
        },
        headers=admin_env["auth"],
    )
    assert status == 200, key_body
    api_key = str(key_body["data"]["key"])
    tools = [
        {
            "type": "function",
            "name": "local_probe",
            "parameters": {"type": "object"},
        }
    ]
    root_messages = [{"role": "user", "content": "observation-branch full root filter context"}]
    status, first = http_request(
        "POST",
        f"{admin_env['proxy']}/v1/responses",
        payload={"model": "observation-branch-context", "input": root_messages, "tools": tools},
        headers={"authorization": f"Bearer {api_key}"},
    )
    assert status == 200, first
    call = next(item for item in first["output"] if item["type"] == "function_call")
    status, completed = http_request(
        "POST",
        f"{admin_env['proxy']}/v1/responses",
        payload={
            "model": "observation-branch-context",
            "previous_response_id": first["id"],
            "input": [{"type": "function_call_output", "call_id": call["call_id"], "output": "context result"}],
        },
        headers={"authorization": f"Bearer {api_key}"},
    )
    assert status == 200, completed
    old_anchor = int(time.time() * 1000)
    status, child = http_request(
        "POST",
        f"{admin_env['proxy']}/v1/responses",
        payload={
            "model": "observation-child-context",
            "previous_response_id": completed["id"],
            "input": [{"role": "user", "content": "child turn"}],
        },
        headers={"authorization": f"Bearer {api_key}"},
    )
    assert status == 200, child

    filtered = _wait_for(
        "completed filtered root",
        lambda: (lambda page: page if page["root_total"] == 1 and all(
            item["status"] == "completed"
            for root in page["roots"] for item in root["interactions"]
        ) else None)(
            _forest(admin_env, anchor_at=old_anchor, model=child_route, limit=10)
        ),
    )
    assert len(filtered["roots"]) == 1
    context = filtered["roots"][0]["interactions"]
    assert len(context) == 2
    assert [item["first_route_id"] for item in context] == [parent_route, child_route]
    assert [item["matched"] for item in context] == [False, True]
    assert context[1]["parent_interaction_id"] == context[0]["id"]
    last_active_at = max(item["last_active_at"] for item in context)
    for start, end, included in (
        (last_active_at, last_active_at + 1, True),
        (last_active_at - 1, last_active_at, False),
        (last_active_at + 1, last_active_at + 2, False),
        (last_active_at, last_active_at + 86_400_000, True),
    ):
        bounded = _forest(
            admin_env, start_at=start, end_at=end,
            anchor_at=0, window_index=9, model=child_route,
        )
        assert bounded["window_start"] == start
        assert bounded["window_end"] == end
        assert bounded["root_total"] == int(included)
        if included:
            assert {item["id"] for item in bounded["roots"][0]["interactions"]} == {
                item["id"] for item in context
            }
            assert [item["matched"] for item in bounded["roots"][0]["interactions"]] == [False, True]
        else:
            assert bounded["roots"] == []
    status, detail_body = http_request(
        "GET",
        f"{admin_env['admin']}/api/v1/observations/interactions/{context[1]['id']}?model={child_route}&start_at=0&end_at=1",
        headers=admin_env["auth"],
    )
    assert status == 200, detail_body
    child_detail = detail_body["data"]
    assert [item["id"] for item in child_detail["root"]["interactions"]] == [
        item["id"] for item in context
    ]
    assert [item["matched"] for item in child_detail["root"]["interactions"]] == [
        False,
        True,
    ]
    assert child_detail["interaction"]["id"] == context[1]["id"]
    assert child_detail["runs"]
    assert {run["route_id"] for run in child_detail["runs"]} == {child_route}

    batch_route, batch_key = _create_route(admin_env, "observation-root-batches")
    for number in range(3):
        status, response = _proxy(
            admin_env,
            batch_key,
            "observation-root-batches",
            [{"role": "user", "content": f"independent root {number}"}],
        )
        assert status == 200, response
    page = _wait_for(
        "three root batches",
        lambda: (lambda value: value if value["root_total"] == 3 else None)(
            _forest(admin_env, anchor_at=old_anchor, model=batch_route, limit=1)
        ),
    )
    seen: list[str] = []
    while True:
        assert len(page["roots"]) == 1
        seen.append(page["roots"][0]["id"])
        cursor = page["next_cursor"]
        if cursor is None:
            break
        page = _forest(
            admin_env,
            anchor_at=old_anchor,
            model=batch_route,
            limit=1,
            cursor=cursor,
        )
    assert len(seen) == len(set(seen)) == 3

    reset = _sse_event(admin_env, page["snapshot_sequence"] + 10_000)
    assert reset["event"] == "reset_required"
    reloaded = _forest(admin_env, anchor_at=old_anchor, model=batch_route, limit=10)
    assert {root["id"] for root in reloaded["roots"]} == set(seen)


@pytest.mark.e2e
@pytest.mark.admin
def test_clear_history_resets_expired_cursor_without_rewinding_sequence(
    admin_env: dict[str, Any],
) -> None:
    route_id, api_key = _create_route(admin_env, "observation-clear-cursor")
    status, response = _proxy(
        admin_env,
        api_key,
        "observation-clear-cursor",
        [{"role": "user", "content": "historical row to clear"}],
    )
    assert status == 200, response
    interaction = _wait_for(
        "completed clear candidate", lambda: _route_interactions(admin_env, route_id)
    )[0]
    interaction_sequence = _detail(admin_env, interaction["id"])["snapshot_sequence"]
    rejected_at = int(time.time() * 1000)
    status, rejection_response = http_request(
        "POST",
        f"{admin_env['proxy']}/v1/chat/completions",
        payload={"model": "missing", "messages": []},
    )
    assert status == 401, rejection_response
    old_sequence = _wait_for(
        "rejection sequence",
        lambda: (lambda value: value if value > interaction_sequence else None)(
            _forest(admin_env)["snapshot_sequence"]
        ),
    )

    def cleanup_rejection() -> dict[str, Any] | None:
        status_, body = http_request(
            "GET",
            f"{admin_env['admin']}/api/v1/observations/rejections",
            headers=admin_env["auth"],
        )
        assert status_ == 200, body
        return next(
            (
                item
                for item in body["data"]["items"]
                if item["occurred_at"] >= rejected_at and item["status_code"] == 401
            ),
            None,
        )

    rejection = _wait_for("clear candidate rejection", cleanup_rejection)
    status, cleared = http_request(
        "DELETE",
        f"{admin_env['admin']}/api/v1/observations/history",
        headers=admin_env["auth"],
    )
    assert status == 200, cleared
    assert cleared["data"]["deleted_interactions"] >= 1
    assert cleared["data"]["deleted_rejections"] >= 1
    after_clear = _forest(admin_env, model=route_id)
    assert after_clear["snapshot_sequence"] == old_sequence
    assert after_clear["roots"] == []
    status, rejections = http_request(
        "GET",
        f"{admin_env['admin']}/api/v1/observations/rejections",
        headers=admin_env["auth"],
    )
    assert status == 200, rejections
    assert all(item["id"] != rejection["id"] for item in rejections["data"]["items"])

    reset = _sse_event(admin_env, old_sequence + 1)
    assert reset["event"] == "reset_required"
    assert reset["data"]["snapshot_sequence"] == old_sequence

    status, response = _proxy(
        admin_env,
        api_key,
        "observation-clear-cursor",
        [{"role": "user", "content": "new history after clear"}],
    )
    assert status == 200, response
    replacement = _wait_for(
        "post-clear Interaction", lambda: _route_interactions(admin_env, route_id)
    )[0]
    assert _detail(admin_env, replacement["id"])["snapshot_sequence"] > old_sequence
