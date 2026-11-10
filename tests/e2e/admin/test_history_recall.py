from __future__ import annotations

import json
import re
import uuid
from typing import Any

import pytest

from tests.common.helpers import http_request
from tests.e2e.admin.test_observations import (
    _create_route, _finished_detail, _proxy, _route_interactions, _wait_for,
)

ROOT = "stravia://history/"
SCOPE_ERROR = "History Recall is only available through this API key's transparent injection."
UNAVAILABLE = "History entry unavailable: it does not exist, has expired, or was cleared."
ANSWER = "The earlier conversation has been answered."
MODEL = "history-recall-e2e"


@pytest.fixture(scope="module")
def recall(admin_env: dict[str, Any]):
    server = admin_env["mock_server"]
    original = server.RequestHandlerClass
    phrase = "history recall unique " + uuid.uuid4().hex

    class RecallProvider(original):
        def do_POST(self) -> None:
            body = self._read_body()
            server.captured_requests.append(body)
            messages = body.get("messages", [])
            prompt = next((m.get("content", "") for m in messages if m.get("role") == "user"), "")
            results = [m.get("content", "") for m in messages if m.get("role") == "tool"]
            path = None
            if isinstance(prompt, str) and prompt.startswith("recall-long-seed") and not results:
                path = ROOT
            elif isinstance(prompt, str) and prompt.startswith("recall-browse"):
                if not results:
                    path = ROOT
                elif len(results) == 1:
                    page = json.loads(results[-1])
                    line = next((line for line in page["content"].splitlines() if phrase in line), "")
                    match = re.search(re.escape(ROOT) + r"[A-Za-z0-9_-]+/", line)
                    path = match.group(0) if match else None
                elif len(results) == 2:
                    page = json.loads(results[-1])
                    match = re.search(re.escape(ROOT) + r"[A-Za-z0-9_-]+/[A-Za-z0-9_-]+", page["content"])
                    path = match.group(0) if match else None
            elif isinstance(prompt, str) and prompt.startswith("recall-read ") and not results:
                path = prompt.removeprefix("recall-read ")
            if path is not None:
                message = {"role": "assistant", "content": None, "tool_calls": [{
                    "id": f"call_recall_{len(results)}", "type": "function",
                    "function": {"name": "StraviaRead", "arguments": json.dumps({"path": path})},
                }]}
                finish = "tool_calls"
            else:
                content = ("\n".join(f"long answer line {i:03d}" for i in range(250))
                           if isinstance(prompt, str) and prompt.startswith("recall-long-seed") else ANSWER)
                message = {"role": "assistant", "content": content}
                finish = "stop"
            model = body.get("model", "mock")
            if body.get("stream"):
                delta = dict(message)
                if "tool_calls" in delta:
                    delta["tool_calls"] = [{"index": 0, **delta["tool_calls"][0]}]
                self._write_sse([
                    {"id": "chatcmpl-recall", "object": "chat.completion.chunk", "model": model,
                     "choices": [{"index": 0, "delta": delta, "finish_reason": None}]},
                    {"id": "chatcmpl-recall", "object": "chat.completion.chunk", "model": model,
                     "choices": [{"index": 0, "delta": {}, "finish_reason": finish}],
                     "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}},
                ])
            else:
                self._write_json(200, {"id": "chatcmpl-recall", "object": "chat.completion", "model": model,
                    "choices": [{"index": 0, "message": message, "finish_reason": finish}],
                    "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}})

    server.RequestHandlerClass = RecallProvider
    try:
        route, _ = _create_route(admin_env, MODEL)

        def key(name: str, history: bool, mcp: bool = False) -> str:
            status, body = http_request("POST", f"{admin_env['admin']}/api/v1/api-keys", payload={
                "name": name, "model_ids": [route], "transparent_injection_enabled": True,
                "transparent_injections": ["history_recall"] if history else [], "mcp_access_enabled": mcp,
            }, headers=admin_env["auth"])
            assert status == 200, body
            return body["data"]["key"]

        a, b, c = key("recall-a", True, True), key("recall-b", True), key("recall-c", False)
        status, response = _proxy(admin_env, a, MODEL, [{"role": "user", "content": phrase}])
        assert status == 200, response
        assert response["choices"][0]["message"]["content"] == ANSWER

        def persisted():
            for item in _route_interactions(admin_env, route):
                detail = _finished_detail(admin_env, item["id"])
                if detail and detail["interaction"]["status"] == "completed":
                    return item
            return None

        interaction = _wait_for("persisted seed conversation", persisted)
        yield {"env": admin_env, "server": server, "route": route, "phrase": phrase, "a": a, "b": b, "c": c,
               "transcript": f"{ROOT}{interaction['root_id']}/{interaction['id']}"}
    finally:
        server.RequestHandlerClass = original


def _request(recall: dict[str, Any], key: str, prompt: str, *, explicit: bool = False):
    server = recall["server"]
    start = len(server.captured_requests)
    extra = {}
    if explicit:
        extra["tools"] = [{"type": "function", "function": {"name": "StraviaRead",
            "description": "Read a path", "parameters": {"type": "object", "properties": {
                "path": {"type": "string"}}, "required": ["path"]}}}]
    status, response = _proxy(recall["env"], key, MODEL, [{"role": "user", "content": prompt}], body_extra=extra)
    assert status == 200, response
    return response, server.captured_requests[start:]


def _tool_results(requests: list[dict[str, Any]]) -> list[str]:
    return [m["content"] for m in requests[-1]["messages"] if m.get("role") == "tool"]


def _description(request: dict[str, Any]) -> str:
    return next(t["function"].get("description", "") for t in request["tools"]
                if t.get("function", {}).get("name") == "StraviaRead")


@pytest.mark.e2e
@pytest.mark.admin
def test_injected_history_browse_and_transcript(recall: dict[str, Any]) -> None:
    response, requests = _request(recall, recall["a"], "recall-browse")
    assert response["choices"][0]["message"]["content"] == ANSWER
    assert ROOT in _description(requests[0])
    results = _tool_results(requests)
    assert len(results) == 3, results
    root, chain, transcript = [json.loads(result) for result in results]
    for page in (root, chain, transcript):
        assert page["representation"] == "markdown"
        assert "read_path" in page and "has_more" in page and "limitations" in page
    assert recall["phrase"] in root["content"]
    assert recall["transcript"] in chain["content"]
    assert "## user\n" + recall["phrase"] in transcript["content"]
    assert "## assistant\n" + ANSWER in transcript["content"]


def _isolated_history_key(recall: dict[str, Any]) -> dict[str, Any]:
    env = recall["env"]
    status, body = http_request("POST", f"{env['admin']}/api/v1/api-keys", payload={
        "name": "recall-" + uuid.uuid4().hex, "model_ids": [recall["route"]],
        "transparent_injection_enabled": True, "transparent_injections": ["history_recall"],
    }, headers=env["auth"])
    assert status == 200, body
    return body["data"]


@pytest.mark.e2e
@pytest.mark.admin
def test_injected_history_ranges_tool_file_and_pagination(recall: dict[str, Any]) -> None:
    key = _isolated_history_key(recall)["key"]
    env, route = recall["env"], recall["route"]
    before = {item["id"] for item in _route_interactions(env, route)}
    response, seed_requests = _request(recall, key, "recall-long-seed")
    answer_lines = [f"long answer line {i:03d}" for i in range(250)]
    assert response["choices"][0]["message"]["content"].splitlines() == answer_lines
    seed_result = json.loads(_tool_results(seed_requests)[0])
    assert ROOT in seed_result["content"]

    def persisted():
        for item in _route_interactions(env, route):
            if item["id"] not in before:
                detail = _finished_detail(env, item["id"])
                if detail and detail["interaction"]["status"] == "completed":
                    return item
        return None

    interaction = _wait_for("persisted long seed conversation", persisted)
    transcript = f"{ROOT}{interaction['root_id']}/{interaction['id']}"
    _, requests = _request(recall, key, "recall-read " + transcript + ":1-3")
    selected = json.loads(_tool_results(requests)[0])
    assert selected["returned_ranges"] == [{"start_line": 1, "end_line": 3}]
    assert selected["content"].splitlines() == [
        f"# Interaction {interaction['id']}",
        f"- chain: {ROOT}{interaction['root_id']}/",
        "- parent: none",
    ]

    _, requests = _request(recall, key, "recall-read " + transcript + "/tools/1")
    tool = json.loads(_tool_results(requests)[0])["content"]
    assert '"path": "' + ROOT + '"' in tool
    assert "## result 1" in tool
    assert "representation" in tool and "markdown" in tool
    assert seed_result["content"].splitlines()[0] in tool

    path = transcript
    numbered_lines = []
    seen_paths = set()
    while path:
        assert path not in seen_paths
        seen_paths.add(path)
        _, requests = _request(recall, key, "recall-read " + path)
        page = json.loads(_tool_results(requests)[0])
        lines = page["content"].splitlines()
        numbers = [number for part in page["returned_ranges"]
                   for number in range(part["start_line"], part["end_line"] + 1)]
        assert len(lines) == len(numbers) <= 200
        assert len(page["content"].encode()) <= 32 * 1024
        numbered_lines.extend(zip(numbers, lines))
        path = page.get("next_path")
        assert page["has_more"] == bool(path)
    assert len(seen_paths) > 1
    assert [number for number, _ in numbered_lines] == list(range(1, len(numbered_lines) + 1))
    transcript_lines = [line for _, line in numbered_lines]
    start = transcript_lines.index(answer_lines[0])
    assert transcript_lines[start:start + len(answer_lines)] == answer_lines


@pytest.mark.e2e
@pytest.mark.admin
def test_removing_history_injection_revokes_same_key(recall: dict[str, Any]) -> None:
    key = _isolated_history_key(recall)
    _, requests = _request(recall, key["key"], "recall-read " + ROOT)
    assert ROOT in _description(requests[0])
    assert json.loads(_tool_results(requests)[0])["representation"] == "markdown"

    env = recall["env"]
    status, body = http_request("PUT", f"{env['admin']}/api/v1/api-keys/{key['id']}", {
        "transparent_injection_enabled": True, "transparent_injections": [],
    }, env["auth"])
    assert status == 200, body
    _, requests = _request(recall, key["key"], "recall-read " + ROOT)
    assert ROOT not in _description(requests[0])
    assert SCOPE_ERROR in _tool_results(requests)[0]


@pytest.mark.e2e
@pytest.mark.admin
def test_other_key_cannot_list_or_read_history(recall: dict[str, Any]) -> None:
    _, listing = _request(recall, recall["b"], "recall-read " + ROOT)
    page = json.loads(_tool_results(listing)[0])
    assert recall["phrase"] not in page["content"]
    _, foreign = _request(recall, recall["b"], "recall-read " + recall["transcript"])
    assert UNAVAILABLE in _tool_results(foreign)[0]


@pytest.mark.e2e
@pytest.mark.admin
def test_injection_without_history_has_no_history_scope(recall: dict[str, Any]) -> None:
    _, requests = _request(recall, recall["c"], "recall-read " + ROOT)
    assert ROOT not in _description(requests[0])
    assert SCOPE_ERROR in _tool_results(requests)[0]


@pytest.mark.e2e
@pytest.mark.admin
def test_explicit_read_declaration_has_no_history_scope(recall: dict[str, Any]) -> None:
    _, requests = _request(recall, recall["a"], "recall-read " + ROOT, explicit=True)
    assert ROOT not in _description(requests[0])
    assert SCOPE_ERROR in _tool_results(requests)[0]


@pytest.mark.e2e
@pytest.mark.admin
def test_mcp_read_has_no_history_scope(recall: dict[str, Any]) -> None:
    status, body = http_request("POST", f"{recall['env']['proxy']}/mcp", {
        "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
            "name": "StraviaRead", "arguments": {"path": ROOT}, "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {},
            },
        },
    }, {"Authorization": f"Bearer {recall['a']}", "Accept": "application/json, text/event-stream",
        "MCP-Protocol-Version": "2026-07-28", "Mcp-Method": "tools/call", "Mcp-Name": "StraviaRead"}, timeout=60)
    assert status == 200, body
    assert body["result"]["isError"] is True, body
    assert SCOPE_ERROR in json.dumps(body["result"])
