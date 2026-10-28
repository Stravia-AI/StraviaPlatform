from __future__ import annotations

import base64
import copy
import hashlib
import json
import sqlite3
import time
from contextlib import closing
from pathlib import Path
from typing import Any
from urllib.parse import urlparse
from uuid import uuid4

import pytest

from tests.common.helpers import (
    WebSession,
    find_free_port,
    http_request,
    minimal_mock_provider,
    start_stravia_server,
    stop_stravia_server,
    wait_until_ready,
)
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


# A Responses reasoning item with opaque state is a protected carrier. Chat
# ingress receives only a marker projection and must restore this exact state
# when the client replays it.
_PROJECTED_REASONING_TEXT = "Consider the user's question before describing the available coding tools."
_PROJECTED_REASONING_SIGNATURE = "opaque-projected-reasoning-state"
_PROJECTED_SOURCE_TEXT = _text("public coding assistant answer")
_PROJECTED_REPLAY_TEXT = _text("current environment answer")
_PROJECTED_STALE_SYSTEM_TEXT = "You are powered by model A."
_PROJECTED_CHANGED_SYSTEM_TEXT = "You are powered by model B."
_PROJECTED_QUESTION_TEXT = "你好，你是什么模型"
_PROJECTED_FOLLOW_UP_TEXT = "我当前是什么电脑"


def _responses_message_text(item: dict[str, Any]) -> str | None:
    content = item.get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(
            part["text"]
            for part in content
            if isinstance(part, dict) and isinstance(part.get("text"), str)
        )
    return None


def _responses_reasoning_item() -> dict[str, Any]:
    return {
        "type": "reasoning",
        "id": "rs-projected-reasoning",
        "summary": [],
        "content": [{"type": "reasoning_text", "text": _PROJECTED_REASONING_TEXT}],
        "encrypted_content": _PROJECTED_REASONING_SIGNATURE,
    }


def _responses_message(text: str) -> dict[str, Any]:
    return {
        "type": "message",
        "id": "msg-projected-reasoning",
        "status": "completed",
        "role": "assistant",
        "content": [{"type": "output_text", "text": text, "annotations": []}],
    }


def _responses_result(output: list[dict[str, Any]]) -> dict[str, Any]:
    return {
        "id": "resp-projected-reasoning",
        "object": "response",
        "created_at": 1_783_000_000,
        "status": "completed",
        "model": "gpt-4o-mini",
        "output": output,
        "usage": {
            "input_tokens": 31, "output_tokens": 17, "total_tokens": 48,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens_details": {"reasoning_tokens": 0},
        },
        "completed_at": 1_783_000_001,
        "incomplete_details": None,
        "previous_response_id": None,
        "instructions": None,
        "error": None,
        "tools": [],
        "tool_choice": "auto",
        "truncation": "disabled",
        "parallel_tool_calls": True,
        "text": {"format": {"type": "text"}},
        "top_p": None,
        "presence_penalty": None,
        "frequency_penalty": None,
        "top_logprobs": None,
        "temperature": None,
        "reasoning": None,
        "max_output_tokens": None,
        "max_tool_calls": None,
        "store": False,
        "background": False,
        "service_tier": "default",
        "metadata": {},
        "safety_identifier": None,
        "prompt_cache_key": None,
    }


def _responses_stream_events(result: dict[str, Any]) -> list[dict[str, Any]]:
    events = [{
        "type": "response.created",
        "response": {**result, "status": "in_progress", "output": [], "usage": None},
    }]
    for index, item in enumerate(result["output"]):
        events.append({"type": "response.output_item.added", "output_index": index, "item": item})
        if item["type"] == "message":
            part = item["content"][0]
            events.extend([
                {"type": "response.content_part.added", "output_index": index, "content_index": 0, "item_id": item["id"], "part": {**part, "text": ""}},
                {"type": "response.output_text.delta", "output_index": index, "content_index": 0, "item_id": item["id"], "delta": part["text"]},
                {"type": "response.output_text.done", "output_index": index, "content_index": 0, "item_id": item["id"], "text": part["text"]},
                {"type": "response.content_part.done", "output_index": index, "content_index": 0, "item_id": item["id"], "part": part},
            ])
        events.append({"type": "response.output_item.done", "output_index": index, "item": item})
    events.append({"type": "response.completed", "response": result})
    for sequence, event in enumerate(events):
        event["sequence_number"] = sequence
    return events


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
    def __init__(
        self, env: dict[str, Any], name: str, accepted: dict[str, dict[str, Any]],
        *, protocol: str = "openai-compatible",
    ) -> None:
        self.env = env
        self.name = name
        self.accepted = accepted
        self.route_id, self.key = _create_route(env, name, protocol=protocol)

    def send(
        self, messages: list[dict[str, Any]], *, answer: dict[str, Any] | None = None,
        key: str | None = None, tools: bool = False,
        body_extra: dict[str, Any] | None = None,
    ) -> tuple[dict[str, Any], dict[str, Any]]:
        before = {
            item["id"]: item["last_event_sequence"]
            for item in _all_interactions(self.env, self.route_id)
        }
        expected = answer or _answer(_text("answer-" + hashlib.sha256(_semantic(messages).encode()).hexdigest()[:12]))
        self.accepted[_semantic(messages)] = expected
        extra = dict(body_extra or {})
        if tools:
            extra["tools"] = [{"type": "function", "function": {"name": "probe", "parameters": {"type": "object"}}}]
        status, body = _proxy(self.env, key or self.key, self.name, messages, body_extra=extra)
        assert status == 200, body
        returned = body["choices"][0]["message"]
        assert _semantic([returned]) == _semantic([expected])

        def completed() -> dict[str, Any] | None:
            for item in _all_interactions(self.env, self.route_id):
                if item["id"] in before and item["last_event_sequence"] <= before[item["id"]]:
                    continue
                detail = _detail(self.env, item["id"])
                if not detail["runs"]:
                    continue
                newest = max(detail["runs"], key=lambda run: run["started_at"])
                finished = [event for event in newest["events"] if event["kind"] == "run_finished"]
                # Detail reads Run rows before its event page, so a concurrent
                # finish can appear beside stale projection fields. Require the
                # forest read made before detail to have seen this terminal event.
                # It must also belong to this send, not a prior Run's finish.
                if newest["status"] in ("completed", "waiting_client") and finished:
                    terminal_sequence = max(event["sequence"] for event in finished)
                    if before.get(item["id"], 0) < terminal_sequence <= item["last_event_sequence"]:
                        return detail
            return None

        return returned, _wait_for("delivered diagnostic Interaction or merged Run", completed)


@pytest.fixture
def diagnostic_provider(admin_env: dict[str, Any]):
    port = find_free_port()
    server, _ = minimal_mock_provider(port)
    accepted: dict[str, dict[str, Any]] = {}

    class Provider(server.RequestHandlerClass):
        def _write_open_responses(self, body: dict[str, Any]) -> None:
            items = body.get("input")
            if not isinstance(items, list):
                items = []
            reasoning_items = [
                item for item in items
                if isinstance(item, dict) and item.get("type") == "reasoning"
            ]
            replayed = reasoning_items[0] if reasoning_items else None
            if replayed is None:
                serialized = json.dumps(body)
                if (
                    _PROJECTED_CHANGED_SYSTEM_TEXT in serialized
                    or _PROJECTED_FOLLOW_UP_TEXT in serialized
                ):
                    self._write_json(422, {"error": {"type": "invalid_context", "message": "replayed protected reasoning state is missing"}})
                    return
            if replayed is not None:
                if len(reasoning_items) != 1:
                    self._write_json(422, {"error": {"type": "invalid_context", "message": "replayed reasoning state is ambiguous"}})
                    return
                if replayed.get("encrypted_content") != _PROJECTED_REASONING_SIGNATURE:
                    self._write_json(422, {"error": {"type": "invalid_context", "message": "protected reasoning state was not restored"}})
                    return
                if "<!--sh:" in json.dumps(body):
                    self._write_json(422, {"error": {"type": "invalid_context", "message": "public reasoning projection was forwarded"}})
                    return
                expected_messages = [
                    ("system", _PROJECTED_CHANGED_SYSTEM_TEXT),
                    ("user", _PROJECTED_QUESTION_TEXT),
                    ("assistant", _PROJECTED_REASONING_TEXT),
                    ("assistant", _PROJECTED_SOURCE_TEXT),
                    ("user", _PROJECTED_FOLLOW_UP_TEXT),
                ]
                messages = [
                    (item.get("role"), _responses_message_text(item))
                    for item in items
                    if isinstance(item, dict) and item.get("type") == "message"
                ]
                extra_items = [
                    item for item in items
                    if not isinstance(item, dict)
                    or item.get("type") not in {"message", "reasoning"}
                ]
                if (
                    messages != expected_messages
                    or extra_items
                    or body.get("previous_response_id") is not None
                    or _PROJECTED_STALE_SYSTEM_TEXT in json.dumps(body)
                ):
                    self._write_json(422, {"error": {"type": "invalid_context", "message": "replayed conversation does not match the changed current window"}})
                    return
                result = _responses_result([_responses_message(_PROJECTED_REPLAY_TEXT)])
            else:
                result = _responses_result([
                    _responses_reasoning_item(),
                    _responses_message(_PROJECTED_SOURCE_TEXT),
                ])
            if not body.get("stream"):
                self._write_json(200, result)
                return
            data = b"".join(
                f"event: {event['type']}\ndata: {json.dumps(event)}\n\n".encode()
                for event in _responses_stream_events(result)
            )
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("content-length", str(len(data)))
            self.send_header("connection", "close")
            self.end_headers()
            self.wfile.write(data)
            self.wfile.flush()

        def do_POST(self) -> None:
            body = self._read_body()
            path = self.path.split("?", 1)[0]
            if path.endswith("/responses"):
                self._write_open_responses(body)
                return
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
                    "usage": {
                        "prompt_tokens": 31, "completion_tokens": 17, "total_tokens": 48,
                        "prompt_tokens_details": {"cached_tokens": 0},
                    },
                })
                self._write_sse(chunks)
                return
            self._write_json(200, {
                "id": "chatcmpl-" + uuid4().hex,
                "object": "chat.completion",
                "created": 1_783_000_000,
                "model": body["model"],
                "choices": [{"index": 0, "message": expected, "finish_reason": "tool_calls" if expected.get("tool_calls") else "stop"}],
                "usage": {
                    "prompt_tokens": 31, "completion_tokens": 17, "total_tokens": 48,
                    "prompt_tokens_details": {"cached_tokens": 0},
                },
            })

    server.RequestHandlerClass = Provider
    env = {**admin_env, "mock": f"http://127.0.0.1:{port}"}
    try:
        yield lambda label, protocol="openai-compatible": _LocalConversation(
            env, "tail-" + label + "-" + uuid4().hex[:8], accepted, protocol=protocol,
        )
    finally:
        server.shutdown()
        server.server_close()


def _newest_run(detail: dict[str, Any]) -> dict[str, Any]:
    return max(detail["runs"], key=lambda run: run["started_at"])


def _admitted(detail: dict[str, Any]) -> dict[str, Any]:
    run = _newest_run(detail)
    events = [event for event in run["events"] if event["kind"] == "run_admitted"]
    assert events
    return events[-1]["payload"]


def _restart_observation_server(admin_env: dict[str, Any], stravia_binary: Path) -> None:
    stop_stravia_server(admin_env["process"], admin_env["logs"])
    port = urlparse(admin_env["admin"]).port
    proc, logs = start_stravia_server(
        stravia_binary=stravia_binary,
        args=[
            "--host", "127.0.0.1", "--port", str(port),
            "--data-dir", str(admin_env["data_dir"]),
            "--proxy-cors-origin", admin_env["proxy_cors_origin"],
        ],
    )
    admin_env.update({"process": proc, "logs": logs})
    wait_until_ready(f"{admin_env['admin']}/api/v1/auth/state")
    session = WebSession(admin_env["admin"])
    status, body = session.request(
        "POST", "/api/v1/auth/login",
        {"username": admin_env["username"], "password": admin_env["password"]},
    )
    assert status == 200, body
    admin_env["auth"] = session.auth_headers()


def _diagnostic(conversation: _LocalConversation, detail: dict[str, Any], status: str) -> dict[str, Any]:
    summary = detail["interaction"]
    run = _newest_run(detail)
    assert run["generation_parent_id"] is None
    assert run["client_output_committed"] is True
    events = [event for event in run["events"] if event["kind"] == "retained_tail_associated"]
    assert len(events) == 1
    event = events[0]
    assert event["payload"]["status"] == status
    forest_item = next(item for item in _all_interactions(conversation.env, conversation.route_id) if item["id"] == summary["id"])
    forest_event = next(
        item for item in forest_item["context_events"]
        if item["sequence"] == event["sequence"]
    )
    assert forest_event["kind"] == "retained_tail_associated"
    assert forest_event["interaction_id"] == summary["id"]
    assert forest_event["run_id"] == run["id"]
    assert forest_event["occurred_at"] == event["occurred_at"]
    assert forest_event["payload"] == event["payload"]
    streamed = _sse_event(conversation.env, event["sequence"] - 1)
    assert streamed["event"] == "observation"
    metadata = streamed["data"]
    # Global replay carries only routing/invalidation metadata. Diagnostic evidence
    # remains available through the persisted detail and forest context events.
    assert set(metadata) <= {
        "sequence", "occurred_at", "interaction_id", "run_id", "rejection_id",
        "kind", "root_id", "boundary",
    }
    assert int(streamed["id"]) == event["sequence"] == metadata["sequence"]
    assert metadata["occurred_at"] == event["occurred_at"]
    assert metadata["kind"] == "retained_tail_associated"
    assert metadata["interaction_id"] == summary["id"]
    assert metadata["run_id"] == run["id"]
    assert metadata["root_id"] == summary["root_id"]
    assert metadata["boundary"] is False
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
        assert detail["interaction"]["parent_interaction_id"] == source["interaction"]["id"]
        assert _admitted(detail)["grouping_reason"] == "retained_tail_linked"
        assert _admitted(detail)["diagnostic_source_run_id"] == source["runs"][0]["id"]
        assert _newest_run(detail)["generation_parent_id"] is None
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
    conversation = diagnostic_provider("projected-reasoning", protocol="open-responses")
    question = _user(_PROJECTED_QUESTION_TEXT)
    original = [{"role": "system", "content": _PROJECTED_STALE_SYSTEM_TEXT}, question]
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
    assert "<!--sh:" in returned["reasoning_content"]

    def source_finished() -> dict[str, Any] | None:
        for item in _all_interactions(conversation.env, conversation.route_id):
            detail = _detail(conversation.env, item["id"])
            if detail["runs"] and detail["runs"][0]["status"] == "completed":
                return detail
        return None

    source = _wait_for("projected reasoning delivery", source_finished)
    changed_system = {"role": "system", "content": _PROJECTED_CHANGED_SYSTEM_TEXT}
    follow_up = _user(_PROJECTED_FOLLOW_UP_TEXT)
    replay = [changed_system, question, returned, follow_up]
    final_answer = _answer(_PROJECTED_REPLAY_TEXT)
    # The Responses Provider emits an opaque reasoning item. Chat ingress receives
    # only the protected marker, and replaying that marker must restore the same
    # encrypted_content to the Provider rather than forwarding the marker or the
    # previous system instructions.
    _, detail = conversation.send(replay, answer=final_answer)
    event = _diagnostic(conversation, detail, "inferred")
    assert event["source_run_id"] == source["runs"][0]["id"]
    assert event["source_interaction_id"] == source["interaction"]["id"]
    assert detail["interaction"]["parent_interaction_id"] == source["interaction"]["id"]
    assert _admitted(detail)["grouping_reason"] == "retained_tail_linked"


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
            assistant, started = conversation.send(
                [_user("removed older interaction"), _answer("removed older answer"), question],
                answer=pending, tools=True,
            )
            # Unresolved current tool results still merge after history trim,
            # even when the Connect Client also appends a new user message.
            _, detail = conversation.send(
                [_user("local summary"), question, assistant, result, _user("new task")],
                tools=True,
            )
            assert detail["interaction"]["id"] == started["interaction"]["id"]
            assert len(detail["runs"]) == 2
            assert _admitted(detail)["grouping_reason"] == "current_tool_continuation"
            assert _newest_run(detail)["generation_parent_id"] is None
            continue
        media = _user(_text("visual question"))
        if mutation == "media":
            status, route = http_request("GET", f"{conversation.env['admin']}/api/v1/models/{conversation.name}", headers=conversation.env["auth"])
            assert status == 200, route
            provider_id = route["data"]["target_provider"]
            model = route["data"]["target_model"]
            status, record = http_request("GET", f"{conversation.env['admin']}/api/v1/providers/{provider_id}/model?model={model}", headers=conversation.env["auth"])
            assert status == 200, record
            status, updated = http_request("PUT", f"{conversation.env['admin']}/api/v1/providers/{provider_id}/model", headers=conversation.env["auth"], payload={"model_id": model, "revision": record["data"]["revision"], "changes": {"modalities.input": ["text", "image"], "modalities.output": ["text"]}})
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
    # A single unit over the byte budget still fails closed: no diagnostic
    # window is captured, so the run reports resource_limit and cannot become
    # a fingerprint source for later requests.
    _, oversized = conversation.send([*retained, _user("x" * (600 * 1024))])
    _diagnostic(conversation, oversized, "resource_limit")
    # The windowless oversized run leaves no candidate behind. The original
    # retained source remains uniquely verifiable.
    _, incomplete = conversation.send([_user("small summary"), *retained, _user("small supplied context")])
    event = _diagnostic(conversation, incomplete, "inferred")
    assert event["source_interaction_id"] is not None
    assert _admitted(incomplete)["grouping_reason"] == "retained_tail_linked"


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
    assert detail["interaction"]["id"] == source["interaction"]["id"]
    assert len(detail["runs"]) == 2
    assert _admitted(detail)["grouping_reason"] == "retained_tail_continuation"
    assert _newest_run(detail)["generation_parent_id"] is None

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
    parent = next(run for run in finished["runs"] if run["id"] == started["runs"][0]["id"])
    child = next(run for run in finished["runs"] if run["id"] != started["runs"][0]["id"])
    assert child["parent_run_id"] == parent["id"]
    assert parent["generation_node_id"] is not None
    assert child["generation_parent_id"] == parent["generation_node_id"]
    assert finished["interaction"]["usage"]["input_tokens"] == 62
    assert finished["interaction"]["usage"]["output_tokens"] == 34
    assert finished["interaction"]["usage"]["cache_read_tokens"] == 0
    assert finished["interaction"]["usage"]["reasoning_tokens"] is None


@pytest.mark.e2e
@pytest.mark.admin
def test_candidate_cap_never_reports_the_indexed_subset_as_unique(diagnostic_provider) -> None:
    conversation = diagnostic_provider("candidate-cap")
    retained, source = _seed(conversation)
    # Independent questions keep Generation roots distinct. Sharing the delivered
    # answer makes all 129 Runs genuine fingerprint candidates for the cap probe,
    # although only one full question/answer pair can match it. The original seed
    # has a different answer and must still be uniquely inferable after saturation.
    shared_answer = _answer(_text("shared independent cap answer"))
    for number in range(129):
        messages = [_user(_text(f"independent cap source {number}"))]
        conversation.accepted[_semantic(messages)] = shared_answer
        status, body = _proxy(conversation.env, conversation.key, conversation.name, messages)
        assert status == 200, body

    def sources_finished() -> bool:
        # root_total proves admission, not finish. A late source finish after
        # send's pre-request watermark could otherwise be mistaken for its result.
        # Read only lifecycle/index facts, never poll for the diagnostic assertion.
        with closing(sqlite3.connect(conversation.env["data_dir"] / "db" / "gateway.db")) as connection:
            completed = connection.execute(
                "SELECT COUNT(*) FROM inference_run_observations r "
                "JOIN interaction_observations i ON i.id=r.interaction_id "
                "JOIN observation_tail_sources s ON s.run_id=r.id "
                "WHERE r.route_id=? AND r.status='completed' AND EXISTS ("
                "SELECT 1 FROM observation_events e WHERE e.run_id=r.id "
                "AND e.kind='run_finished' AND e.sequence<=i.last_event_sequence)",
                (conversation.route_id,),
            ).fetchone()[0]
        return completed == 130

    _wait_for("all candidate sources finished and indexed", sources_finished)
    try:
        _, detail = conversation.send([_user("summary after candidate saturation"), *retained, _user("new task despite diagnostic cap")])
        event = _diagnostic(conversation, detail, "inferred")
        assert event["source_interaction_id"] == source["interaction"]["id"]
        assert event["source_run_id"] == _newest_run(source)["id"]
        assert _admitted(detail)["grouping_reason"] == "retained_tail_linked"

        # Fingerprint saturation cannot turn the one fully matching indexed pair
        # into a unique source: the whole 129-candidate search exceeds the budget.
        _, capped = conversation.send([
            _user("summary retaining one independent source"),
            _user(_text("independent cap source 0")), shared_answer,
            _user("new task after candidate overflow"),
        ])
        capped_event = _diagnostic(conversation, capped, "resource_limit")
        assert capped_event["source_interaction_id"] is None
        assert capped_event["source_run_id"] is None
        assert _admitted(capped)["grouping_reason"] == "new_root"
        assert capped["interaction"]["parent_interaction_id"] is None
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
    trace_root = conversation.env["data_dir"] / "diagnostics" / "observation-debug"
    displaced = conversation.env["data_dir"] / ("diagnostic-trace-" + uuid4().hex)
    trace_root.rename(displaced)
    trace_root.write_bytes(b"injected trace-storage failure")
    try:
        supplied = [_user("local summary during observation failure"), *retained, _user("new business action")]
        _, detail = conversation.send(supplied)
        event = _diagnostic(conversation, detail, "inferred")
        assert event["source_run_id"] == source["runs"][0]["id"]
        assert detail["interaction"]["parent_interaction_id"] == source["interaction"]["id"]
        assert _admitted(detail)["grouping_reason"] == "retained_tail_linked"
        assert detail["interaction"]["debug_status"] == "partial"
        assert detail["runs"][0]["trace"]["status"] == "partial"
        assert "storage_error" in detail["runs"][0]["trace"]["reasons"]
    finally:
        trace_root.unlink()
        displaced.rename(trace_root)
        status, body = http_request("PUT", f"{conversation.env['admin']}/api/v1/observations/debug", headers=conversation.env["auth"], payload={"enabled": False})
        assert status == 200, body


@pytest.mark.e2e
@pytest.mark.admin
@pytest.mark.parametrize("restart", [False, True])
def test_thinking_only_input_replay_keeps_one_receipt_and_independent_branches(
    diagnostic_provider, admin_env: dict[str, Any], stravia_binary: Path, restart: bool,
) -> None:
    conversation = diagnostic_provider("receipt-replay")
    question = _user("Run a probe and use its result.")
    call_id = "call-receipt-" + uuid4().hex
    call = {
        "role": "assistant",
        "tool_calls": [{
            "id": call_id,
            "type": "function",
            "function": {"name": "probe", "arguments": "{}"},
        }],
    }
    assistant, _ = conversation.send([question], answer=call, tools=True)
    original = [
        question, assistant,
        {"role": "tool", "tool_call_id": call_id, "content": "first result"},
    ]
    conversation.accepted[_semantic(original)] = {
        "role": "assistant", "reasoning_content": "Review the probe result before continuing.",
    }
    tools = {"tools": [{
        "type": "function",
        "function": {"name": "probe", "parameters": {"type": "object"}},
    }]}
    status, body = _proxy(
        conversation.env, conversation.key, conversation.name, original, body_extra=tools,
    )
    assert status == 200, body
    message = body["choices"][0]["message"]
    assert message.get("content") in (None, "")
    assert message["reasoning_content"]
    assert body["choices"][0]["finish_reason"] == "stop"

    def received() -> dict[str, Any] | None:
        for item in _all_interactions(conversation.env, conversation.route_id):
            detail = _detail(conversation.env, item["id"])
            for run in detail["runs"]:
                results = [event for event in run["events"] if event["kind"] == "client_tool_result"]
                if (
                    run["status"] == "completed"
                    and any(event["kind"] == "run_finished" for event in run["events"])
                    and any(event["payload"]["tool_id"] == call_id for event in results)
                ):
                    return detail
        return None

    first = _wait_for("thinking-only result receipt persisted", received)
    first_receipt = next(
        run for run in first["runs"]
        if any(event["kind"] == "client_tool_result" for event in run["events"])
    )
    if restart:
        _restart_observation_server(admin_env, stravia_binary)
        conversation.env["auth"] = admin_env["auth"]

    # The client omits the thinking-only output and only appends a reminder.
    _, replay = conversation.send([*original, _user("Continue the work.")], tools=True)
    replay_run = _newest_run(replay)
    assert _admitted(replay)["grouping_reason"] not in (
        "current_tool_continuation", "pending_tool_result",
    )
    assert not any(event["kind"] == "client_tool_result" for event in replay_run["events"])
    assert replay_run["generation_parent_id"] == first_receipt["generation_parent_id"]

    # An identical independent invocation, unlike a strict user-only extension,
    # is still a branch receipt. Changed values are not erased either.
    for messages, result in [
        (original, "first result"),
        ([*original[:-1], {**original[-1], "content": "branch result"}], "branch result"),
    ]:
        _, branch = conversation.send(messages, tools=True)
        newest = _newest_run(branch)
        events = [event["payload"] for event in newest["events"] if event["kind"] == "client_tool_result"]
        assert len(events) == 1
        assert events[0]["tool_id"] == call_id
        assert events[0]["content"] == result

    details = [
        _detail(conversation.env, item["id"])
        for item in _all_interactions(conversation.env, conversation.route_id)
    ]
    runs = [run for detail in details for run in detail["runs"]]
    assert len(runs) == 5
    results = [
        event["payload"] for run in runs for event in run["events"]
        if event["kind"] == "client_tool_result"
    ]
    assert sorted(event["content"] for event in results) == [
        "branch result", "first result", "first result",
    ]
    assert all(run["status"] != "waiting_client" for run in runs)


@pytest.mark.e2e
@pytest.mark.admin
@pytest.mark.parametrize("new_generation_root", [False, True])
@pytest.mark.parametrize("restart", [False, True])
def test_editing_user_after_received_tool_result_creates_child_interaction(
    diagnostic_provider, admin_env: dict[str, Any], stravia_binary: Path,
    new_generation_root: bool, restart: bool,
) -> None:
    conversation = diagnostic_provider("edited-user-branch")
    question = _user("Run a browser probe and report its result.")
    call_id = "call-edit-" + uuid4().hex
    call = {
        "role": "assistant",
        "tool_calls": [{
            "id": call_id,
            "type": "function",
            "function": {"name": "probe", "arguments": "{}"},
        }],
    }
    assistant, source = conversation.send([question], answer=call, tools=True)
    source_run = _newest_run(source)
    history = [question, assistant, {"role": "tool", "tool_call_id": call_id, "content": "probe result"}]
    _, original = conversation.send([*history, _user("Use CloakBrowser for OMP.")], tools=True)
    assert original["interaction"]["id"] == source["interaction"]["id"]

    if restart:
        _restart_observation_server(admin_env, stravia_binary)
        conversation.env["auth"] = admin_env["auth"]

    _, branch = conversation.send(
        [*history, _user("Use CloakBrowser for OMP and preserve UA-CH.")],
        tools=True, body_extra={"temperature": 0.25} if new_generation_root else None,
    )
    assert branch["interaction"]["id"] != original["interaction"]["id"]
    assert branch["interaction"]["parent_interaction_id"] == original["interaction"]["id"]
    run = _newest_run(branch)
    assert run["parent_run_id"] == source_run["id"]
    assert run["generation_parent_id"] == (None if new_generation_root else source_run["generation_node_id"])
    assert branch["interaction"]["input_preview"] == "Use CloakBrowser for OMP and preserve UA-CH."
    assert run["status"] == "completed"
    assert not any(event["kind"] == "client_tool_result" for event in run["events"])
