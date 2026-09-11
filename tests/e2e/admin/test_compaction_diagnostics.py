from __future__ import annotations

import base64
import copy
import hashlib
import json
import time
from pathlib import Path
from typing import Any
from uuid import uuid4

import pytest

from tests.common.helpers import find_free_port, http_request, minimal_mock_provider
from tests.e2e.admin.test_observations import (
    _create_route,
    _detail,
    _forest,
    _proxy,
    _route_interactions,
    _sse_event,
    _wait_for,
)


# Deliberately substantial, deterministic interaction content, not short acknowledgments.
# The isolated calibration is documented in interaction_observation/tail.rs: an answer
# must be substantive and evidence must contain a complete interaction, not only bytes.
def _text(label: str) -> str:
    return " ".join(f"{label}: evidence paragraph {number} preserves exact meaning." for number in range(12))


def _user(text: str) -> dict[str, Any]:
    return {"role": "user", "content": text}


def _answer(text: str) -> dict[str, Any]:
    return {"role": "assistant", "content": text}


def _media_data_url(name: str, media_type: str) -> str:
    path = (
        Path(__file__).parents[3]
        / "backend"
        / "crates"
        / "stravia-core"
        / "tests"
        / "fixtures"
        / "media"
        / name
    )
    encoded = base64.b64encode(path.read_bytes()).decode()
    return f"data:{media_type};base64,{encoded}"


def _semantic(messages: list[dict[str, Any]]) -> str:
    normalized = copy.deepcopy(messages)
    for message in normalized:
        # Chat permits the same textual content as a string or text-part array.
        content = message.get("content")
        if isinstance(content, list) and all(part.get("type") == "text" for part in content):
            message["content"] = "".join(part["text"] for part in content)
        for call in message.get("tool_calls", []):
            arguments = call.get("function", {}).get("arguments")
            if isinstance(arguments, str):
                call["function"]["arguments"] = json.loads(arguments)
        if message.get("tool_calls") and message.get("content") == "":
            del message["content"]
        for field in list(message):
            if message[field] is None:
                del message[field]
    return json.dumps(normalized, sort_keys=True, separators=(",", ":"))


def _all_interactions(env: dict[str, Any], route_id: str) -> list[dict[str, Any]]:
    anchor = int(time.time() * 1000) + 1_000
    page = _forest(env, model=route_id, limit=100, anchor_at=anchor)
    items = []
    while True:
        items.extend(item for root in page["roots"] for item in root["interactions"] if item["first_route_id"] == route_id)
        if page["next_cursor"] is None:
            return items
        page = _forest(env, model=route_id, limit=100, anchor_at=anchor, cursor=page["next_cursor"])


class _LocalConversation:
    def __init__(self, env: dict[str, Any], name: str, accepted: dict[str, dict[str, Any]]) -> None:
        self.env = env
        self.name = name
        self.accepted = accepted
        self.route_id, self.key = _create_route(env, name)

    def send(
        self, messages: list[dict[str, Any]], *, answer: dict[str, Any] | None = None,
        key: str | None = None, tools: bool = False,
    ) -> tuple[dict[str, Any], dict[str, Any]]:
        before = {item["id"] for item in _all_interactions(self.env, self.route_id)}
        expected = answer or _answer(_text("answer-" + hashlib.sha256(_semantic(messages).encode()).hexdigest()[:12]))
        self.accepted[_semantic(messages)] = expected
        extra = {"tools": [{"type": "function", "function": {"name": "probe", "parameters": {"type": "object"}}}]} if tools else None
        status, body = _proxy(self.env, key or self.key, self.name, messages, body_extra=extra)
        assert status == 200, body
        returned = body["choices"][0]["message"]
        assert _semantic([returned]) == _semantic([expected])

        def completed() -> dict[str, Any] | None:
            for item in _all_interactions(self.env, self.route_id):
                if item["id"] in before:
                    continue
                detail = _detail(self.env, item["id"])
                if detail["runs"] and all(run["status"] in ("completed", "waiting_client") for run in detail["runs"]):
                    return detail
            return None

        return returned, _wait_for("delivered independent diagnostic Interaction", completed)


@pytest.fixture
def diagnostic_provider(admin_env: dict[str, Any]):
    port = find_free_port()
    server, _ = minimal_mock_provider(port)
    accepted: dict[str, dict[str, Any]] = {}

    class Provider(server.RequestHandlerClass):
        def do_POST(self) -> None:
            body = self._read_body()
            expected = accepted.get(_semantic(body.get("messages", [])))
            # A restored prefix, dropped item, reordered tool exchange or upstream
            # continuation shortcut is an invalid conversation, not a mock echo.
            if expected is None or body.get("previous_response_id"):
                self._write_json(422, {"error": {"type": "invalid_context", "message": "context is not an accepted conversation"}})
                return
            if body.get("stream"):
                chunks = [
                    {
                        "id": "chatcmpl-diagnostic-stream",
                        "object": "chat.completion.chunk",
                        "model": body["model"],
                        "choices": [{"index": 0, "delta": {field: expected[field]}, "finish_reason": None}],
                    }
                    for field in ("reasoning_content", "content")
                    if field in expected
                ]
                chunks.append({
                    "id": "chatcmpl-diagnostic-stream",
                    "object": "chat.completion.chunk",
                    "model": body["model"],
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 31, "completion_tokens": 17, "total_tokens": 48},
                })
                self._write_sse(chunks)
                return
            self._write_json(200, {
                "id": "chatcmpl-" + uuid4().hex,
                "object": "chat.completion",
                "created": 1_783_000_000,
                "model": body["model"],
                "choices": [{"index": 0, "message": expected, "finish_reason": "tool_calls" if expected.get("tool_calls") else "stop"}],
                "usage": {"prompt_tokens": 31, "completion_tokens": 17, "total_tokens": 48},
            })

    server.RequestHandlerClass = Provider
    env = {**admin_env, "mock": f"http://127.0.0.1:{port}"}
    try:
        yield lambda label: _LocalConversation(env, "tail-" + label + "-" + uuid4().hex[:8], accepted)
    finally:
        server.shutdown()
        server.server_close()


def _diagnostic(conversation: _LocalConversation, detail: dict[str, Any], status: str) -> dict[str, Any]:
    summary = detail["interaction"]
    assert summary["parent_interaction_id"] is None
    assert len(detail["runs"]) == 1
    run = detail["runs"][0]
    assert run["parent_run_id"] is None
    assert run["generation_parent_id"] is None
    assert run["client_output_committed"] is True
    events = [event for event in summary["context_events"] if event["kind"] == "retained_tail_associated"]
    assert len(events) == 1
    event = events[0]
    assert event["payload"]["status"] == status
    assert event in run["events"]
    forest_item = next(item for item in _all_interactions(conversation.env, conversation.route_id) if item["id"] == summary["id"])
    assert event in forest_item["context_events"]
    streamed = _sse_event(conversation.env, event["sequence"] - 1)
    assert streamed["event"] == "observation"
    assert streamed["data"] == event
    if status != "inferred":
        assert event["payload"]["source_run_id"] is None
        assert event["payload"]["source_interaction_id"] is None
    return event["payload"]


def _seed(conversation: _LocalConversation, *, prefix: str = "discarded old history", retained: list[dict[str, Any]] | None = None):
    kept = retained or [_user(_text("retained question"))]
    answer, detail = conversation.send(
        [{"role": "system", "content": "Original business instructions"}, _user(_text(prefix)), _answer(_text("old answer")), *kept],
        answer=_answer(_text("retained answer")),
    )
    return [*kept, answer], detail


@pytest.mark.e2e
@pytest.mark.admin
def test_retained_interaction_anywhere_is_only_diagnostic_and_new_user_stays_new(diagnostic_provider) -> None:
    for placement in ("before", "after"):
        conversation = diagnostic_provider("summary-" + placement)
        retained, source = _seed(conversation)
        summary = _user("Client's local summary replaces removed private history.")
        context = [summary, *retained] if placement == "before" else [*retained, summary]
        supplied = [{"role": "system", "content": "Rebuilt top-level business instructions"}, *context, _user("A genuinely new user action")]
        answer, detail = conversation.send(supplied)
        event = _diagnostic(conversation, detail, "inferred")
        assert event["source_run_id"] == source["runs"][0]["id"]
        assert event["source_interaction_id"] == source["interaction"]["id"]
        assert detail["interaction"]["id"] != source["interaction"]["id"]
        # A Principal with no diagnostic candidates executes the identical supplied
        # window and returns the same answer. Neither request can restore old history.
        status, key = http_request("POST", f"{conversation.env['admin']}/api/v1/api-keys", headers=conversation.env["auth"], payload={"name": f"diagnostics-free-control-{placement}", "model_ids": [conversation.route_id]})
        assert status == 200, key
        _, control = conversation.send(supplied, answer=answer, key=key["data"]["key"])
        _diagnostic(conversation, control, "no_match")


@pytest.mark.e2e
@pytest.mark.admin
@pytest.mark.parametrize("stream", [False, True])
def test_projected_reasoning_links_diagnostics_after_model_instruction_change(diagnostic_provider, stream: bool) -> None:
    conversation = diagnostic_provider("projected-reasoning")
    question = _user("你好，你是什么模型")
    original = [{"role": "system", "content": "You are powered by model A."}, question]
    upstream_answer = {
        **_answer(_text("public coding assistant answer")),
        "reasoning_content": "Consider the user's question before describing the available coding tools.",
    }
    conversation.accepted[_semantic(original)] = upstream_answer
    status, body = _proxy(
        conversation.env, conversation.key, conversation.name, original,
        body_extra={"stream": stream},
    )
    assert status == 200, body
    if stream:
        assert "data: [DONE]" in body
        returned = {"role": "assistant", "content": "", "reasoning_content": ""}
        for line in body.splitlines():
            if not line.startswith("data: ") or line == "data: [DONE]":
                continue
            for choice in json.loads(line[6:]).get("choices", []):
                for field in ("content", "reasoning_content"):
                    returned[field] += choice.get("delta", {}).get(field) or ""
    else:
        returned = body["choices"][0]["message"]
    assert "stravia-history-marker:" in returned["reasoning_content"]

    def source_finished() -> dict[str, Any] | None:
        for item in _all_interactions(conversation.env, conversation.route_id):
            detail = _detail(conversation.env, item["id"])
            if detail["runs"] and detail["runs"][0]["status"] == "completed":
                return detail
        return None

    source = _wait_for("projected reasoning delivery", source_finished)
    changed_system = {"role": "system", "content": "You are powered by model B."}
    follow_up = _user("我当前是什么电脑")
    replay = [changed_system, question, returned, follow_up]
    final_answer = _answer(_text("current environment answer"))
    # The provider must receive the restored reasoning, not the public projection
    # or the previous system instructions.
    restored = [
        changed_system,
        question,
        {
            "role": "assistant",
            "provenance": "provider",
            "audience": "internal",
            "reasoning_content": upstream_answer["reasoning_content"],
        },
        _answer(upstream_answer["content"]),
        follow_up,
    ]
    conversation.accepted[_semantic(restored)] = final_answer
    _, detail = conversation.send(replay, answer=final_answer)
    event = _diagnostic(conversation, detail, "inferred")
    assert event["source_run_id"] == source["runs"][0]["id"]
    assert event["source_interaction_id"] == source["interaction"]["id"]


@pytest.mark.e2e
@pytest.mark.admin
def test_weak_role_and_discontinuous_evidence_cannot_associate(diagnostic_provider) -> None:
    cases = ("system-only", "short-answer", "user-only", "no-tail", "changed-role", "internal-system", "reordered", "gap")
    for case in cases:
        conversation = diagnostic_provider(case)
        question = _user(_text("retained question"))
        answer = _answer("OK" if case == "short-answer" else _text("retained answer"))
        _, source = conversation.send([{"role": "system", "content": _text("common system")}, _user("removed history"), _answer("removed answer"), question], answer=answer)
        pair = [question, answer]
        if case == "system-only":
            pair = [{"role": "system", "content": _text("common system")}]
        elif case == "user-only":
            pair = [question]
        elif case == "no-tail":
            pair = [_user(_text("unrelated summary"))]
        elif case == "changed-role":
            pair = [_answer(question["content"]), answer]
        elif case == "internal-system":
            pair = [question, {"role": "system", "content": "new internal instruction"}, answer]
        elif case == "reordered":
            pair = [answer, question]
        elif case == "gap":
            pair = [question, _user("inserted unmatched message"), answer]
        _, detail = conversation.send([*pair, _user("new work after " + case)])
        _diagnostic(conversation, detail, "no_match")
        assert detail["interaction"]["id"] != source["interaction"]["id"]


@pytest.mark.e2e
@pytest.mark.admin
def test_tool_correlations_and_media_are_part_of_exact_retained_evidence(diagnostic_provider) -> None:
    call = {"role": "assistant", "tool_calls": [{"id": "call-retained", "type": "function", "function": {"name": "probe", "arguments": '{"path":"original"}'}}]}
    result = {"role": "tool", "tool_call_id": "call-retained", "content": _text("original tool result")}
    for mutation in ("complete-tool", "tool-id", "tool-arguments", "tool-result", "unclosed-call", "media"):
        conversation = diagnostic_provider(mutation)
        if mutation == "unclosed-call":
            question = _user(_text("question awaiting tool completion"))
            pending = {**copy.deepcopy(call), "content": _text("substantive explanation before tool execution")}
            assistant, _ = conversation.send(
                [_user("removed older interaction"), _answer("removed older answer"), question],
                answer=pending, tools=True,
            )
            # The retained source ends with an unresolved tool call. The new
            # request closes it legally, but that newly supplied result cannot
            # retrospectively turn the old suffix into a complete interaction.
            _, detail = conversation.send(
                [_user("local summary"), question, assistant, result, _user("new task")],
                tools=True,
            )
            _diagnostic(conversation, detail, "no_match")
            continue
        media = _user(_text("visual question"))
        if mutation == "media":
            status, route = http_request("GET", f"{conversation.env['admin']}/api/v1/models/{conversation.name}", headers=conversation.env["auth"])
            assert status == 200, route
            provider_id = route["data"]["target_provider"]
            model = route["data"]["target_model"]
            status, record = http_request("GET", f"{conversation.env['admin']}/api/v1/providers/{provider_id}/model?model={model}", headers=conversation.env["auth"])
            assert status == 200, record
            metadata = {**record["data"]["metadata"], "modalities": {"input": ["text", "image"], "output": ["text"]}}
            status, updated = http_request("PUT", f"{conversation.env['admin']}/api/v1/providers/{provider_id}/model", headers=conversation.env["auth"], payload={"model_id": model, "metadata": metadata, "revision": record["data"]["revision"]})
            assert status == 200, updated
            media = {"role": "user", "content": [{"type": "text", "text": _text("visual question")}, {"type": "image_url", "image_url": {"url": _media_data_url("transparent.png", "image/png")}}]}
        kept = [media] if mutation == "media" else [_user(_text("tool question")), call, result]
        retained, _ = _seed(conversation, retained=kept)
        changed = copy.deepcopy(retained)
        if mutation == "tool-id":
            changed[1]["tool_calls"][0]["id"] = "call-rewritten"
            changed[2]["tool_call_id"] = "call-rewritten"
        elif mutation == "tool-arguments":
            changed[1]["tool_calls"][0]["function"]["arguments"] = '{"path":"rewritten"}'
        elif mutation == "tool-result":
            changed[2]["content"] = _text("rewritten result")
        elif mutation == "media":
            changed[0]["content"][1]["image_url"]["url"] = _media_data_url("static.webp", "image/webp")
        _, detail = conversation.send([_user("local summary"), *changed, _user("new task")], tools=mutation != "media")
        _diagnostic(conversation, detail, "inferred" if mutation == "complete-tool" else "no_match")


@pytest.mark.e2e
@pytest.mark.admin
def test_equal_strength_sources_remain_ambiguous_not_latest_parent(diagnostic_provider) -> None:
    conversation = diagnostic_provider("ambiguous")
    retained, left = _seed(conversation, prefix="left branch private prefix")
    _, right = _seed(conversation, prefix="right branch private prefix")
    assert left["runs"][0]["id"] != right["runs"][0]["id"]
    _, detail = conversation.send([_user("summary"), *retained, _user("new ambiguous branch")])
    event = _diagnostic(conversation, detail, "ambiguous")
    assert event["candidate_count"] >= 2


@pytest.mark.e2e
@pytest.mark.admin
def test_incomplete_index_and_oversized_search_leave_inference_unchanged(diagnostic_provider) -> None:
    conversation = diagnostic_provider("resource")
    retained, _ = _seed(conversation)
    _, oversized = conversation.send([_user("x" * (600 * 1024)), *retained, _user("large supplied context")])
    _diagnostic(conversation, oversized, "resource_limit")
    # This completed source cannot fit in the diagnostic index. A later small
    # request must not treat the remaining indexed matching source as unique.
    _, incomplete = conversation.send([_user("small summary"), *retained, _user("small supplied context")])
    _diagnostic(conversation, incomplete, "index_unavailable")


@pytest.mark.e2e
@pytest.mark.admin
def test_history_cleanup_does_not_resurrect_a_diagnostic_source(diagnostic_provider) -> None:
    conversation = diagnostic_provider("cleanup")
    retained, source = _seed(conversation)
    status, body = http_request("DELETE", f"{conversation.env['admin']}/api/v1/observations/history", headers=conversation.env["auth"])
    assert status == 200, body
    assert _forest(conversation.env, model=conversation.route_id)["roots"] == []
    _, detail = conversation.send([_user("summary after cleanup"), *retained, _user("new action after cleanup")])
    _diagnostic(conversation, detail, "no_match")
    status, _ = http_request("GET", f"{conversation.env['admin']}/api/v1/observations/interactions/{source['interaction']['id']}", headers=conversation.env["auth"])
    assert status == 404


@pytest.mark.e2e
@pytest.mark.admin
def test_retained_block_before_appended_tool_result_does_not_enable_execution_parent(diagnostic_provider) -> None:
    conversation = diagnostic_provider("appended-tool")
    retained, source = _seed(conversation)
    call = {"role": "assistant", "tool_calls": [{"id": "call-new", "type": "function", "function": {"name": "probe", "arguments": '{"path":"new"}'}}]}
    supplied = [_user("local summary"), *retained, call, {"role": "tool", "tool_call_id": "call-new", "content": _text("new tool result")}]
    _, detail = conversation.send(supplied, tools=True)
    event = _diagnostic(conversation, detail, "inferred")
    assert event["source_run_id"] == source["runs"][0]["id"]

    # Contrast with a proven, uncompressed tool continuation: no new User means
    # another Run of the same Interaction, not an inferred execution edge.
    ordinary = diagnostic_provider("ordinary-tool")
    first = [_user(_text("start a real tool interaction"))]
    assistant, started = ordinary.send(first, answer=call, tools=True)
    continuation = [*first, assistant, {"role": "tool", "tool_call_id": "call-new", "content": "finished"}]
    expected = _answer(_text("ordinary tool finished"))
    ordinary.accepted[_semantic(continuation)] = expected
    status, body = _proxy(ordinary.env, ordinary.key, ordinary.name, continuation, body_extra={"tools": [{"type": "function", "function": {"name": "probe", "parameters": {"type": "object"}}}]})
    assert status == 200, body
    def completed_tool_interaction():
        detail = _detail(ordinary.env, started["interaction"]["id"])
        return detail if len(detail["runs"]) == 2 and detail["interaction"]["status"] == "completed" else None
    finished = _wait_for("completed tool Interaction with two Runs", completed_tool_interaction)
    assert len(_route_interactions(ordinary.env, ordinary.route_id)) == 1
    child = next(run for run in finished["runs"] if run["id"] != started["runs"][0]["id"])
    assert child["parent_run_id"] == started["runs"][0]["id"]
    assert child["generation_parent_id"] == started["runs"][0]["generation_node_id"]
    assert finished["interaction"]["usage"]["input_tokens"] == 62
    assert finished["interaction"]["usage"]["output_tokens"] == 34
    assert finished["interaction"]["usage"]["reasoning_tokens"] is None


@pytest.mark.e2e
@pytest.mark.admin
def test_candidate_cap_never_reports_the_indexed_subset_as_unique(diagnostic_provider) -> None:
    conversation = diagnostic_provider("candidate-cap")
    retained, _ = _seed(conversation)
    # More independent completed conversations than the bounded search can
    # exhaust. Each has a different semantic source, not repeated retries.
    for number in range(129):
        messages = [_user(f"independent cap source {number}")]
        conversation.accepted[_semantic(messages)] = _answer(_text(f"independent answer {number}"))
        status, body = _proxy(conversation.env, conversation.key, conversation.name, messages)
        assert status == 200, body
    _wait_for("all candidate sources persisted", lambda: _forest(conversation.env, model=conversation.route_id)["root_total"] >= 130)
    _, detail = conversation.send([_user("summary after candidate saturation"), *retained, _user("new task despite diagnostic cap")])
    try:
        _diagnostic(conversation, detail, "resource_limit")
    finally:
        status, body = http_request("DELETE", f"{conversation.env['admin']}/api/v1/observations/history", headers=conversation.env["auth"])
        assert status == 200, body


@pytest.mark.e2e
@pytest.mark.admin
def test_trace_failure_preserves_exact_compacted_inference_and_reports_partial(diagnostic_provider) -> None:
    conversation = diagnostic_provider("trace-failure")
    retained, source = _seed(conversation)
    status, body = http_request("PUT", f"{conversation.env['admin']}/api/v1/observations/debug", headers=conversation.env["auth"], payload={"enabled": True, "confirmed": True})
    assert status == 200, body
    trace_root = conversation.env["data_dir"] / "observation-debug"
    displaced = conversation.env["data_dir"] / ("diagnostic-trace-" + uuid4().hex)
    trace_root.rename(displaced)
    trace_root.write_bytes(b"injected trace-storage failure")
    try:
        supplied = [_user("local summary during observation failure"), *retained, _user("new business action")]
        _, detail = conversation.send(supplied)
        event = _diagnostic(conversation, detail, "inferred")
        assert event["source_run_id"] == source["runs"][0]["id"]
        assert detail["interaction"]["debug_status"] == "partial"
        assert detail["runs"][0]["trace"]["status"] == "partial"
        assert "storage_error" in detail["runs"][0]["trace"]["reasons"]
    finally:
        trace_root.unlink()
        displaced.rename(trace_root)
        status, body = http_request("PUT", f"{conversation.env['admin']}/api/v1/observations/debug", headers=conversation.env["auth"], payload={"enabled": False})
        assert status == 200, body
