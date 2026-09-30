"""Client tool argument passthrough E2E.

Upstream Chat Completions providers sometimes emit ``function.arguments``
that are not well-formed JSON (the WorkBuddy/deepseek incident truncated the
final ``}``). Client-owned tool arguments are opaque transport for Stravia:
they must reach the client byte-for-byte, may never fail the stream, and must
not be completed, replaced, or swallowed. Platform-owned tools keep their
execution-side validation: malformed arguments must still be refused.
"""

from __future__ import annotations

import json
from typing import Any
from urllib.request import Request, urlopen

import pytest

from tests.common.helpers import http_request


MALFORMED_ARGUMENTS = '{"path":"/tmp/x"'  # missing the closing brace


def _create_route(env: dict[str, Any], name: str) -> tuple[str, str]:
    status, body = http_request(
        "POST",
        f"{env['admin']}/api/v1/providers",
        payload={
            "name": f"{name}-provider",
            "source": {
                "type": "custom",
                "vendor": "custom",
                "channel": "default",
                "protocol": "openai-compatible",
                "base_url": env["mock"],
            },
            "credential": {"type": "api_key", "value": "upstream-secret"},
            "vendor_options": {},
        },
        headers=env["auth"],
    )
    assert status == 200, body
    provider_id = body["data"]["id"]
    status, body = http_request(
        "POST",
        f"{env['admin']}/api/v1/providers/{provider_id}/models",
        payload={"model_id": "gpt-4o-mini", "metadata": {"name": name}},
        headers=env["auth"],
    )
    assert status == 201, body
    status, body = http_request(
        "POST",
        f"{env['admin']}/api/v1/models",
        payload={
            "model_id": name,
            "display_name": f"{name} display",
            "targets": [{"provider_id": provider_id, "model": "gpt-4o-mini"}],
        },
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


def _sse_responses(
    env: dict[str, Any], api_key: str, payload: dict[str, Any]
) -> list[dict[str, Any]]:
    request = Request(
        f"{env['proxy']}/v1/responses",
        data=json.dumps({**payload, "stream": True}).encode(),
        headers={
            "authorization": f"Bearer {api_key}",
            "content-type": "application/json",
        },
    )
    events: list[dict[str, Any]] = []
    with urlopen(request, timeout=15) as stream:
        for raw in stream:
            line = raw.strip()
            if not line.startswith(b"data: ") or line[6:].strip() == b"[DONE]":
                continue
            events.append(json.loads(line[6:]))
    return events


def _function_call(completed: dict[str, Any]) -> dict[str, Any]:
    return next(
        item for item in completed["output"] if item["type"] == "function_call"
    )


def _completed_event(events: list[dict[str, Any]]) -> dict[str, Any]:
    types = [event["type"] for event in events]
    assert "error" not in types, events
    assert "response.failed" not in types, events
    assert "response.completed" in types, events
    return next(
        event["response"] for event in events if event["type"] == "response.completed"
    )


@pytest.mark.e2e
@pytest.mark.admin
def test_client_tool_malformed_arguments_stream_through_verbatim(
    admin_env: dict[str, Any],
) -> None:
    _route, api_key = _create_route(admin_env, "passthrough-malformed")
    events = _sse_responses(
        admin_env,
        api_key,
        {
            "model": "passthrough-malformed",
            "input": [{"role": "user", "content": "passthrough-malformed-tool-args"}],
            "tools": [
                {
                    "type": "function",
                    "name": "local_probe",
                    "parameters": {"type": "object"},
                }
            ],
        },
    )

    completed = _completed_event(events)
    call = _function_call(completed)
    # The upstream bytes arrive at the client byte-for-byte.
    assert call["arguments"] == MALFORMED_ARGUMENTS
    arguments_done = next(
        event
        for event in events
        if event["type"] == "response.function_call_arguments.done"
    )
    assert arguments_done["arguments"] == MALFORMED_ARGUMENTS
    item_done = next(
        event["item"]
        for event in events
        if event["type"] == "response.output_item.done"
        and event["item"]["type"] == "function_call"
    )
    assert item_done["arguments"] == MALFORMED_ARGUMENTS


@pytest.mark.e2e
@pytest.mark.admin
def test_client_tool_empty_arguments_stream_through_verbatim(
    admin_env: dict[str, Any],
) -> None:
    _route, api_key = _create_route(admin_env, "passthrough-empty")
    events = _sse_responses(
        admin_env,
        api_key,
        {
            "model": "passthrough-empty",
            "input": [{"role": "user", "content": "passthrough-empty-tool-args"}],
            "tools": [
                {
                    "type": "function",
                    "name": "local_probe",
                    "parameters": {"type": "object"},
                }
            ],
        },
    )

    completed = _completed_event(events)
    call = _function_call(completed)
    assert call["arguments"] == ""


@pytest.mark.e2e
@pytest.mark.admin
def test_malformed_arguments_commit_to_generation_history_and_round_trip(
    admin_env: dict[str, Any],
) -> None:
    _route, api_key = _create_route(admin_env, "passthrough-history")
    events = _sse_responses(
        admin_env,
        api_key,
        {
            "model": "passthrough-history",
            "input": [{"role": "user", "content": "passthrough-malformed-tool-args"}],
            "tools": [
                {
                    "type": "function",
                    "name": "local_probe",
                    "parameters": {"type": "object"},
                }
            ],
        },
    )
    completed = _completed_event(events)
    call = _function_call(completed)

    # The delivered response must be committed to the Generation Chain:
    # continuing by opaque previous_response_id restores the same verbatim
    # arguments into the next upstream request.
    status, continuation = http_request(
        "POST",
        f"{admin_env['proxy']}/v1/responses",
        payload={
            "model": "passthrough-history",
            "previous_response_id": completed["id"],
            "input": [
                {
                    "type": "function_call_output",
                    "call_id": call["call_id"],
                    "output": "probe done",
                }
            ],
        },
        headers={"authorization": f"Bearer {api_key}"},
    )
    assert status == 200, continuation
    assert continuation["status"] == "completed", continuation

    # The upstream must see the exact bytes again — not re-encoded JSON, not
    # a fabricated {} — proving restore and egress encode stay verbatim.
    upstream_arguments = [
        tool_call["function"]["arguments"]
        for body in admin_env["mock_server"].captured_requests
        for message in body.get("messages", [])
        for tool_call in message.get("tool_calls") or []
        if isinstance(tool_call.get("function"), dict)
    ]
    assert MALFORMED_ARGUMENTS in upstream_arguments, upstream_arguments

    # Clients may also echo the verbatim output item back explicitly; the
    # decoder must accept it instead of rejecting its own delivery.
    echoed = dict(call)
    status, replayed = http_request(
        "POST",
        f"{admin_env['proxy']}/v1/responses",
        payload={
            "model": "passthrough-history",
            "input": [
                {"role": "user", "content": "probe again"},
                echoed,
                {
                    "type": "function_call_output",
                    "call_id": call["call_id"],
                    "output": "probe done",
                },
            ],
        },
        headers={"authorization": f"Bearer {api_key}"},
    )
    assert status == 200, replayed
    assert replayed["status"] == "completed", replayed


@pytest.mark.e2e
@pytest.mark.admin
def test_platform_tool_malformed_arguments_are_refused_at_execution(
    admin_env: dict[str, Any],
) -> None:
    _route, api_key = _create_route(admin_env, "passthrough-platform")
    events = _sse_responses(
        admin_env,
        api_key,
        {
            "model": "passthrough-platform",
            "input": [
                {"role": "user", "content": "passthrough-platform-malformed-tool-args"}
            ],
            "tools": [
                {
                    "type": "function",
                    "name": "StraviaRead",
                    "parameters": {"type": "object"},
                }
            ],
        },
    )

    types = [event["type"] for event in events]
    # Per the PlatformTool contract, argument errors become an is_error
    # PlatformToolResult sent back to the provider — the run does not fail.
    assert "response.failed" not in types, events
    completed = _completed_event(events)
    assert completed["status"] == "completed", completed

    # The refusal must be observable at the execution boundary: the upstream
    # continuation carries the input-validation error as the tool result,
    # proving StraviaRead never executed.
    upstream_messages = json.dumps(
        [
            message
            for body in admin_env["mock_server"].captured_requests
            for message in body.get("messages", [])
        ]
    )
    assert "invalid tool arguments" in upstream_messages, upstream_messages
