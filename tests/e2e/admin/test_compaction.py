from __future__ import annotations

import hashlib
import json
import time
from concurrent.futures import ThreadPoolExecutor
from urllib.request import Request, urlopen
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any

import pytest

from tests.common.helpers import http_request, responses_compaction
from tests.e2e.admin.test_observations import _detail, _client_errors, _route_interactions, _wait_for


def _window_semantics(items: list[dict[str, Any]]) -> list[dict[str, Any]]:
    normalized = json.loads(json.dumps(items))
    for item in normalized:
        if item.get("type") != "message":
            continue
        content = item.get("content")
        if isinstance(content, str):
            item["content"] = [{
                "type": "output_text" if item.get("role") == "assistant" else "input_text",
                "text": content,
            }]
        for part in item.get("content", []):
            if part.get("annotations") == []:
                del part["annotations"]
    return normalized


@pytest.fixture
def compaction_provider():
    """只有完整、按原序回放原生窗口才能完成后续业务请求。"""
    window = [
        {"type": "compaction", "id": "cmp_state_one", "encrypted_content": "opaque-state-one"},
    ]
    received: list[dict[str, Any]] = []
    windows: dict[str, list[dict[str, Any]]] = {}
    lock = threading.Lock()
    release_streams = threading.Event()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_GET(self):
            if self.path != "/release":
                self.send_error(404)
                return
            release_streams.set()
            self.send_response(204)
            self.end_headers()

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["content-length"])))
            with lock:
                received.append({"path": self.path, "body": body})
            items = body.get("input", [])
            signature = hashlib.sha256(json.dumps(items, sort_keys=True).encode()).hexdigest()[:16]
            invalid = False
            for state in [item for item in items if item.get("type") == "compaction"]:
                with lock:
                    expected = windows.get(state.get("encrypted_content", ""))
                if expected is not None and (_window_semantics(items[:len(expected)]) != _window_semantics(expected) or "removed-history-sentinel" in json.dumps(items)):
                    invalid = True
            trigger = any(item.get("type") == "compaction_trigger" for item in items)
            automatic = bool(body.get("context_management"))
            compact_window = window if body.get("instructions") == "complete-window" else [{"type": "compaction", "id": f"cmp_{signature}", "encrypted_content": f"opaque-{signature}"}]
            if body.get("instructions") == "state-without-id":
                compact_window = [{"type": "compaction", "encrypted_content": f"opaque-{signature}"}]
            if trigger or automatic:
                with lock:
                    for state in compact_window:
                        if state.get("type") == "compaction":
                            windows[state["encrypted_content"]] = compact_window
            if self.path.endswith("/responses"):
                items = body.get("input", [])
                replaying = any(item.get("type") == "compaction" for item in items if isinstance(item, dict))
                if invalid:
                    result = {"error": {"code": "invalid_compacted_window", "message": "The compacted window is incomplete or altered."}}
                    status = 400
                else:
                    result = {
                        "id": f"resp_{signature}",
                        "object": "response", "created_at": 1788825600, "status": "completed",
                        "model": "native-model", "output": [{"type": "message", "id": f"msg_{signature}", "status": "completed", "role": "assistant", "content": [{"type": "output_text", "text": "continued from compacted window" if replaying else "source answer", "annotations": []}]}],
                        "usage": {"input_tokens": 10, "output_tokens": 4, "total_tokens": 14, "input_tokens_details": {"cached_tokens": 0}, "output_tokens_details": {"reasoning_tokens": 0}},
                        "completed_at": 1788825601, "incomplete_details": None,
                        "previous_response_id": None, "instructions": None, "error": None,
                        "tools": [], "tool_choice": "auto", "truncation": "disabled",
                        "parallel_tool_calls": True, "text": {"format": {"type": "text"}},
                        "top_p": None, "presence_penalty": None, "frequency_penalty": None,
                        "top_logprobs": None, "temperature": None, "reasoning": None,
                        "max_output_tokens": None, "max_tool_calls": None, "store": False,
                        "background": False, "service_tier": "default", "metadata": {},
                        "safety_identifier": None, "prompt_cache_key": None,
                    }
                    status = 200
            else:
                result, status = {"error": {"code": "unsupported_path"}}, 404
            if body.get("instructions") in ("reject-compaction", "reject-compaction-secret"):
                result = {"error": {
                    "code": "upstream_compaction_unavailable",
                    "type": "provider_capacity_error",
                    "message": "Compaction capacity is exhausted.",
                    "param": "context_management",
                    "details": {"retryable": True},
                }}
                if body["instructions"] == "reject-compaction-secret":
                    result["error"]["message"] = "rejected " + self.headers["Authorization"].removeprefix("Bearer ")
                status = 503
            if status == 200 and result.get("object") == "response" and trigger:
                result["output"] = compact_window
            elif status == 200 and result.get("object") == "response" and automatic:
                result["output"] = compact_window + result["output"]
            streaming = body.get("stream") and status == 200 and result.get("object") == "response"
            if streaming:
                events = [{"type": "response.created", "response": {**result, "status": "in_progress", "output": [], "usage": None}}]
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
                if body.get("instructions") in ("reject-stream-first", "reject-stream-after-text"):
                    failure = {"type": "error", "error": {
                        "code": "native_window_rejected", "type": "invalid_request_error",
                        "message": "The upstream rejected the compacted window.",
                        "param": "context_management", "details": {"window": "expired"},
                    }}
                    events = [failure] if body["instructions"] == "reject-stream-first" else events[:-1] + [failure]
                if body.get("instructions") in ("abort-after-state", "pause-after-state"):
                    boundary = next(i for i, event in enumerate(events) if event["type"] == "response.output_item.done" and event["item"]["type"] == "compaction")
                    events = events[:boundary + 1]
                for sequence, event in enumerate(events):
                    event["sequence_number"] = sequence
                data = "".join(f"event: {event['type']}\ndata: {json.dumps(event)}\n\n" for event in events).encode()
            else:
                data = json.dumps(result).encode()
            if body.get("instructions") == "invalid-success":
                data = b"upstream returned invalid JSON"
            self.send_response(status)
            self.send_header("content-type", "text/event-stream" if streaming else "application/json")
            paused = bool(streaming) and body.get("instructions") == "pause-after-state"
            self.send_header("content-length", str(len(data) + int(paused)))
            self.end_headers()
            if streaming and body.get("instructions") == "reject-stream-after-text":
                boundary = data.index(b"event: error\n")
                self.wfile.write(data[:boundary])
                self.wfile.flush()
                release_streams.wait()
                data = data[boundary:]
            self.wfile.write(data)
            self.wfile.flush()
            if paused:
                release_streams.wait()

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}", window, received
    finally:
        release_streams.set()
        server.shutdown()
        server.server_close()
        thread.join()


def _native_route(env: dict[str, Any], base_url: str, name: str, *, vendor: str = "custom") -> tuple[str, str]:
    status, body = http_request("POST", f"{env['admin']}/api/v1/providers", payload={
        "name": name, "source": {"type": "custom", "vendor": vendor, "channel": "default", "protocol": "open-responses", "base_url": base_url},
        "credential": {"type": "api_key", "value": "local-test-credential"},
        "vendor_options": {},
    }, headers=env["auth"])
    assert status == 200, body
    provider_id = body["data"]["id"]
    status, body = http_request("POST", f"{env['admin']}/api/v1/providers/{provider_id}/models", payload={"model_id": "native-model", "metadata": {"name": name, "modalities": {"input": ["text"], "output": ["text"]}, "reasoning_efforts": ["none", "low", "medium", "high"], "limit": {"context": 100000}}}, headers=env["auth"])
    assert status == 201, body
    status, body = http_request("POST", f"{env['admin']}/api/v1/models", payload={"model_id": name, "output_modality": "text", "targets": [{"provider_id": provider_id, "model": "native-model"}]}, headers=env["auth"])
    assert status == 200 and "data" in body, body
    route_id = body["data"]["id"]
    status, body = http_request("POST", f"{env['admin']}/api/v1/api-keys", payload={"name": name, "model_ids": [route_id]}, headers=env["auth"])
    assert status == 200, body
    return route_id, body["data"]["key"]


@pytest.mark.e2e
@pytest.mark.admin
def test_native_compact_window_continues_without_restoring_removed_history(admin_env, compaction_provider):
    base_url, window, _received = compaction_provider
    route_id, key = _native_route(admin_env, base_url, "native-compaction-roundtrip")
    headers = {"authorization": f"Bearer {key}"}
    source_input = [{"role": "user", "content": "removed-history-sentinel"}]
    status, source = http_request("POST", f"{admin_env['proxy']}/v1/responses", payload={"model": "native-compaction-roundtrip", "input": source_input}, headers=headers)
    assert status == 200, source
    status, compacted = responses_compaction(f"{admin_env['proxy']}/v1/responses", payload={"model": "native-compaction-roundtrip", "input": source_input + source["output"], "instructions": "complete-window"}, headers=headers)
    assert status == 200, compacted
    assert compacted["object"] == "response"
    assert compacted["status"] == "completed"
    assert compacted["output"] == window
    assert compacted["usage"]["input_tokens"] == 10
    # 新 User 输入需要在快速续接窗口之外才会开启新的 Interaction（CONTEXT.md 归并规则）。
    time.sleep(2.01)
    status, continued = http_request("POST", f"{admin_env['proxy']}/v1/responses", payload={"model": "native-compaction-roundtrip", "input": compacted["output"] + [{"role": "user", "content": "continue the task"}]}, headers=headers)
    assert status == 200, continued
    assert continued["output"][0]["content"][0]["text"] == "continued from compacted window"
    assert _run(admin_env, route_id, continued["id"])["generation_parent_id"] == source["id"]
    interactions = _route_interactions(admin_env, route_id)
    assert len(interactions) == 2, "only the two new User inputs create Interactions"
    details = [_detail(admin_env, item["id"]) for item in interactions]
    runs = [run for detail in details for run in detail["runs"]]
    committed = [run for run in runs if run.get("generation_node_id")]
    assert {run["generation_node_id"] for run in committed} == {source["id"], continued["id"]}
    operations = [event for item in interactions for event in item["context_events"] if event["kind"] == "compaction_operation"]
    assert len({event["payload"]["operation_id"] for event in operations}) == 1
    compact_runs = [run for run in runs if not run.get("generation_node_id")]
    assert len(compact_runs) == 1
    source_detail = next(detail for detail in details if any(run.get("generation_node_id") == source["id"] for run in detail["runs"]))
    assert compact_runs[0]["id"] in {run["id"] for run in source_detail["runs"]}
    admitted = next(event for event in compact_runs[0]["events"] if event["kind"] == "run_admitted")
    assert admitted["payload"]["has_new_user"] is False
    assert compact_runs[0]["usage"]["input_tokens"] == 10
    assert compact_runs[0]["usage"]["output_tokens"] == 4
    serialized = json.dumps({"interactions": interactions, "details": details})
    assert "opaque-state-one" not in serialized
    assert "local-test-credential" not in serialized


def _request(env, key, model, items, *, compact=False, **extra):
    payload = {"model": model, "input": items, **extra}
    headers = {"authorization": f"Bearer {key}"}
    url = f"{env['proxy']}/v1/responses"
    if compact or any(item.get("type") == "compaction_trigger" for item in items):
        return responses_compaction(url, payload, headers)
    return http_request("POST", url, payload=payload, headers=headers)


def _success(env, key, model, items, **extra):
    status, response = _request(env, key, model, items, **extra)
    assert status == 200, response
    return response


def _run(env, route, generation):
    def probe():
        for interaction in _route_interactions(env, route):
            for run in _detail(env, interaction["id"])["runs"]:
                if run.get("generation_node_id") == generation:
                    return run
        return None
    return _wait_for("delivered Generation observation", probe)


def _boundary(env, key, model, label="removed-history-sentinel"):
    items = [{"role": "user", "content": label}]
    source = _success(env, key, model, items)
    compacted = _success(env, key, model, items + source["output"], compact=True)
    assert [item["type"] for item in compacted["output"]] == ["compaction"]
    return source, compacted["output"]


@pytest.mark.e2e
@pytest.mark.admin
def test_zero_tail_repeated_state_follows_three_descendants_and_second_boundary(admin_env, compaction_provider):
    model = "native-descendants"
    route, key = _native_route(admin_env, compaction_provider[0], model)
    source, history = _boundary(admin_env, key, model)
    parent = source["id"]
    for turn in range(3):
        history = history + [{"role": "user", "content": f"distinct business turn {turn}"}]
        response = _success(admin_env, key, model, history, instructions="Continue business, not summarization")
        assert _run(admin_env, route, response["id"])["generation_parent_id"] == parent
        history += response["output"]
        parent = response["id"]
    second = _success(admin_env, key, model, history, compact=True)
    assert second["output"] != history[:1]
    response = _success(admin_env, key, model, second["output"] + [{"role": "user", "content": "after second boundary"}])
    assert _run(admin_env, route, response["id"])["generation_parent_id"] == parent


@pytest.mark.e2e
@pytest.mark.admin
def test_concurrent_native_branches_keep_independent_descendants_and_boundaries(admin_env, compaction_provider):
    model = "native-branches"
    route, key = _native_route(admin_env, compaction_provider[0], model)
    source, window = _boundary(admin_env, key, model)
    def branch(label):
        history = window + [{"role": "user", "content": label}]
        first = _success(admin_env, key, model, history)
        history += first["output"]
        second_input = history + [{"role": "user", "content": f"continue {label}"}]
        second = _success(admin_env, key, model, second_input)
        boundary = _success(admin_env, key, model, second_input + second["output"], compact=True)
        third = _success(admin_env, key, model, boundary["output"] + [{"role": "user", "content": f"finish {label}"}])
        return first, second, third
    with ThreadPoolExecutor(max_workers=2) as workers:
        branches = list(workers.map(branch, ["branch alpha", "branch beta"]))
    for first, second, third in branches:
        assert _run(admin_env, route, first["id"])["generation_parent_id"] == source["id"]
        assert _run(admin_env, route, second["id"])["generation_parent_id"] == first["id"]
        assert _run(admin_env, route, third["id"])["generation_parent_id"] == second["id"]
    assert branches[0][2]["id"] != branches[1][2]["id"]


@pytest.mark.e2e
@pytest.mark.admin
def test_native_evidence_conflicts_are_rejected_and_external_state_does_not_invent_parent(admin_env, compaction_provider):
    model = "native-conflicts"
    route, key = _native_route(admin_env, compaction_provider[0], model)
    source, window = _boundary(admin_env, key, model)
    other, other_window = _boundary(admin_env, key, model, "unrelated source")
    altered = [{**window[0], "encrypted_content": "altered-native-content"}]
    for items, extra in [
        (altered, {}),
        (window, {"previous_response_id": other["id"]}),
        (window + other_window, {}),
    ]:
        status, _ = _request(admin_env, key, model, items + [{"role": "user", "content": "conflicting continuation"}], **extra)
        assert 400 <= status < 500
    external = [{"type": "compaction", "id": "external-state", "encrypted_content": "external-payload"}]
    response = _success(admin_env, key, model, external + [{"role": "user", "content": "external continuation"}])
    assert _run(admin_env, route, response["id"])["generation_parent_id"] is None
    status, body = http_request("POST", f"{admin_env['admin']}/api/v1/api-keys", payload={"name": "different-principal", "model_ids": [route]}, headers=admin_env["auth"])
    assert status == 200, body
    stranger = body["data"]["key"]
    status, response = _request(admin_env, stranger, model, window + [{"role": "user", "content": "other principal"}])
    if status == 200:
        assert _run(admin_env, route, response["id"])["generation_parent_id"] is None
    else:
        assert 400 <= status < 500


@pytest.mark.e2e
@pytest.mark.admin
def test_inline_sse_publishes_immediately_replayable_state(admin_env, compaction_provider):
    model = "native-inline"
    route, key = _native_route(admin_env, compaction_provider[0], model)
    source_input = [{"role": "user", "content": "removed-history-sentinel"}]
    source = _success(admin_env, key, model, source_input)
    request = Request(f"{admin_env['proxy']}/v1/responses", data=json.dumps({
        "model": model, "input": source_input + source["output"], "stream": True,
        "context_management": [{"type": "compaction", "compact_threshold": 2000}],
    }).encode(), headers={"authorization": f"Bearer {key}", "content-type": "application/json"})
    published = None
    completed = None
    with urlopen(request, timeout=15) as stream:
        for raw in stream:
            if not raw.startswith(b"data: ") or raw[6:].strip() == b"[DONE]":
                continue
            event = json.loads(raw[6:])
            if event["type"] == "response.output_item.done" and event["item"]["type"] == "compaction":
                published = event["item"]
                # Replay while the originating response is still being consumed.
                immediate = _success(admin_env, key, model, [published, {"role": "user", "content": "immediate replay"}])
                assert _run(admin_env, route, immediate["id"])["generation_parent_id"] == source["id"]
            if event["type"] == "response.completed":
                completed = event["response"]
    assert published is not None
    assert completed is not None
    assert published in completed["output"]
    assert _run(admin_env, route, completed["id"])["generation_parent_id"] == source["id"]
    next_response = _success(admin_env, key, model, completed["output"] + [{"role": "user", "content": "next inline turn"}])
    assert _run(admin_env, route, next_response["id"])["generation_parent_id"] == completed["id"]


@pytest.mark.e2e
@pytest.mark.admin
def test_client_compaction_threshold_is_decided_by_upstream(admin_env, compaction_provider):
    model = "client-compaction-threshold"
    _route, key = _native_route(admin_env, compaction_provider[0], model)
    result = _success(
        admin_env, key, model, [{"role": "user", "content": "upstream owns its window"}],
        context_management=[{"type": "compaction", "compact_threshold": 100001}],
    )
    assert any(item["type"] == "compaction" for item in result["output"])


def _fallback_target(env, model, fallback_model):
    targets = []
    for name, priority in [(model, 10), (fallback_model, 0)]:
        status, body = http_request("GET", f"{env['admin']}/api/v1/models/{name}", headers=env["auth"])
        assert status == 200, body
        target = body["data"]["targets"][0]
        targets.append({
            "provider_id": target["provider_id"], "model": target["model"],
            "priority": priority, "enabled": True,
            "target_retry_budget": 1, "target_cooldown_ms": 0,
        })
    status, body = http_request("PUT", f"{env['admin']}/api/v1/models/{model}",
                               payload={"targets": targets}, headers=env["auth"])
    assert status == 200, body


@pytest.mark.e2e
@pytest.mark.admin
@pytest.mark.parametrize("mode", ["standalone", "controls", "trigger"])
def test_unknown_compaction_capability_is_forwarded(admin_env, compaction_provider, mode):
    model = f"unknown-compaction-{mode}"
    _route, key = _native_route(admin_env, compaction_provider[0], model, vendor="custom")
    items = [{"role": "user", "content": "let the upstream decide"}]
    extra = {}
    if mode == "standalone":
        extra = {"compact": True, "reasoning": {"effort": "low"}}
    elif mode == "controls":
        extra = {"context_management": [{"type": "compaction", "compact_threshold": 2000}]}
    else:
        items.append({"type": "compaction_trigger"})
    result = _success(admin_env, key, model, items, **extra)
    assert any(item["type"] == "compaction" for item in result["output"])
    if mode in ("standalone", "trigger"):
        outbound = compaction_provider[2][0]
        assert outbound["path"].endswith("/responses")
        assert outbound["body"]["stream"] is True
        assert outbound["body"]["input"][-1]["type"] == "compaction_trigger"
        if mode == "standalone":
            assert outbound["body"]["reasoning"]["effort"] == "low"


@pytest.mark.e2e
@pytest.mark.admin
@pytest.mark.parametrize("mode", ["standalone", "controls", "trigger"])
@pytest.mark.parametrize("vendor", ["custom", "openai"])
def test_compaction_upstream_error_is_returned_without_retry_or_failover(admin_env, compaction_provider, mode, vendor):
    model = f"compaction-error-{vendor}-{mode}"
    route, key = _native_route(admin_env, compaction_provider[0], model, vendor=vendor)
    _native_route(admin_env, compaction_provider[0], f"{model}-fallback")
    _fallback_target(admin_env, model, f"{model}-fallback")
    items = [{"role": "user", "content": "do not retry this operation"}]
    extra = {"instructions": "reject-compaction"}
    if mode == "standalone":
        extra["compact"] = True
    elif mode == "controls":
        extra["context_management"] = [{"type": "compaction", "compact_threshold": 2000}]
    else:
        items.append({"type": "compaction_trigger"})
    status, result = _request(admin_env, key, model, items, **extra)
    assert status == 503, result
    assert result == {"error": {
        "code": "upstream_compaction_unavailable", "type": "provider_capacity_error",
        "message": "Compaction capacity is exhausted.", "param": "context_management",
        "details": {"retryable": True},
    }}
    assert len(compaction_provider[2]) == 1, "neither the current nor fallback Target may retry compaction"
    failure = _wait_for(
        "upstream compaction failure",
        lambda: next(iter(_client_errors(admin_env, model=route)["items"]), None),
    )
    assert failure["error"]["source"] == "upstream"
    assert failure["error"]["status_code"] == 503
    assert failure["error"]["message"] == "Compaction capacity is exhausted."


@pytest.mark.e2e
@pytest.mark.admin
def test_compaction_error_redacts_known_connection_secret(admin_env, compaction_provider):
    model = "compaction-known-secret"
    _route, key = _native_route(admin_env, compaction_provider[0], model)
    status, result = _request(
        admin_env, key, model, [{"role": "user", "content": "compact safely"}],
        instructions="reject-compaction-secret",
        context_management=[{"type": "compaction", "compact_threshold": 2000}],
    )
    assert status == 503, result
    assert result["error"]["code"] == "upstream_compaction_unavailable"
    assert result["error"]["details"]["retryable"] is True
    assert "local-test-credential" not in json.dumps(result)
    assert len(compaction_provider[2]) == 1


@pytest.mark.e2e
@pytest.mark.admin
def test_unrepresentable_compaction_does_not_select_a_capable_fallback(admin_env, compaction_provider):
    from tests.e2e.admin.test_observations import _create_route
    model = "compaction-unrepresentable-first"
    _route, key = _create_route(admin_env, model)
    _native_route(admin_env, compaction_provider[0], f"{model}-fallback")
    _fallback_target(admin_env, model, f"{model}-fallback")
    status, result = _request(admin_env, key, model, [{"role": "user", "content": "stay on selected Target"}], compact=True)
    assert status == 400, result
    assert result["error"]["code"] == "compaction_unsupported"
    assert compaction_provider[2] == []


@pytest.mark.e2e
@pytest.mark.admin
@pytest.mark.parametrize("phase", ["first", "after-text"])
def test_inline_compaction_stream_preserves_upstream_error(admin_env, compaction_provider, phase):
    from urllib.error import HTTPError
    model = f"compaction-stream-error-{phase}"
    _route, key = _native_route(admin_env, compaction_provider[0], model, vendor="custom")
    request = Request(f"{admin_env['proxy']}/v1/responses", data=json.dumps({
        "model": model, "input": [{"role": "user", "content": "stream an upstream decision"}],
        "context_management": [{"type": "compaction", "compact_threshold": 2000}],
        "instructions": f"reject-stream-{phase}", "stream": True,
    }).encode(), headers={"authorization": f"Bearer {key}", "content-type": "application/json"})
    try:
        response = urlopen(request, timeout=15)
    except HTTPError as error:
        response = error
    with response:
        if response.headers.get_content_type() == "application/json":
            assert phase == "first", "after-text failure must occur after client output commit"
            errors = [json.load(response)["error"]]
        else:
            events = []
            for raw in response:
                if not raw.startswith(b"data: ") or raw[6:].strip() == b"[DONE]":
                    continue
                event = json.loads(raw[6:])
                events.append(event)
                if phase == "after-text" and event.get("type") == "response.output_text.delta":
                    status, _ = http_request("GET", f"{compaction_provider[0]}/release")
                    assert status == 204
            standalone_errors = [event["error"] for event in events if event.get("type") == "error"]
            failures = [event for event in events if event.get("type") == "response.failed"]
            assert len(failures) == 1, events
            failed = failures[0]["response"]
            assert events[-1] is failures[0], "failure must be the final JSON event"
            assert failed["status"] == "failed"
            assert not any(event.get("type") == "response.completed" for event in events)
            if phase == "after-text":
                # 已提交正文的流只通过失败终态交付错误，保留完整的部分输出快照。
                assert standalone_errors == [], events
                text_indices = [index for index, event in enumerate(events)
                                if event.get("type") == "response.output_text.delta"
                                and event.get("delta") == "source answer"]
                assert text_indices and text_indices[-1] < len(events) - 1, events
                assert any(item["type"] == "message" and any(
                    part.get("type") == "output_text" and part.get("text") == "source answer"
                    for part in item.get("content", [])
                ) for item in failed["output"]), failed
                assert any(item["type"] == "compaction" for item in failed["output"]), failed
                completed_items = [event["item"] for event in events
                                   if event.get("type") == "response.output_item.done"]
                assert failed["output"] == completed_items
                # 上游只报告未知 usage；不能伪造零用量，也不能漏掉该字段。
                assert failed["usage"] is None
                errors = [failed["error"]]
            else:
                assert len(standalone_errors) == 1, events
                assert not any(event.get("type") == "response.output_text.delta" for event in events)
                assert failed["error"] == standalone_errors[0]
                errors = standalone_errors
    assert errors == [{
        "code": "native_window_rejected", "type": "invalid_request_error",
        "message": "The upstream rejected the compacted window.",
        "param": "context_management", "details": {"window": "expired"},
    }]
    assert len(compaction_provider[2]) == 1


@pytest.mark.e2e
@pytest.mark.admin
@pytest.mark.parametrize("compact", [False, True], ids=["inline", "standalone"])
def test_malformed_compaction_success_is_a_gateway_error(admin_env, compaction_provider, compact):
    model = f"compaction-malformed-{compact}"
    _route, key = _native_route(admin_env, compaction_provider[0], model)
    extra = {} if compact else {"context_management": [{"type": "compaction", "compact_threshold": 2000}]}
    status, result = _request(admin_env, key, model, [{"role": "user", "content": "validate the upstream result"}],
                              compact=compact, instructions="invalid-success", **extra)
    assert status == 502, result
    assert "error" in result
    assert len(compaction_provider[2]) == 1


@pytest.mark.e2e
@pytest.mark.admin
def test_only_explicit_client_controls_request_compaction(admin_env, compaction_provider):
    model = "native-policy"
    route, key = _native_route(admin_env, compaction_provider[0], model)
    def response(label, **extra):
        return _success(admin_env, key, model, [{"role": "user", "content": label}], **extra)
    def has_state(result):
        return any(item["type"] == "compaction" for item in result["output"])
    assert not has_state(response("no client compaction request"))
    assert not has_state(response("explicit empty remains empty", context_management=[]))
    assert not has_state(response("explicit null remains null", context_management=None))
    assert has_state(response("client requested compaction", context_management=[{"type": "compaction", "compact_threshold": 2000}]))
    assert not has_state(response("prior request does not establish a policy"))
    status, body = http_request("GET", f"{admin_env['admin']}/api/v1/models/{model}", headers=admin_env["auth"])
    assert status == 200, body
    assert "compaction_enabled" not in body["data"]
    assert "compaction_threshold" not in body["data"]
    _boundary(admin_env, key, model, "client standalone compact needs no setting")


@pytest.mark.e2e
@pytest.mark.admin
def test_registered_state_rejects_changed_target_configuration(admin_env, compaction_provider):
    model = "native-target-namespace"
    route, key = _native_route(admin_env, compaction_provider[0], model)
    _, window = _boundary(admin_env, key, model)
    status, body = http_request("GET", f"{admin_env['admin']}/api/v1/models/{model}", headers=admin_env["auth"])
    assert status == 200, body
    provider = body["data"]["targets"][0]["provider_id"]
    status, body = http_request("PUT", f"{admin_env['admin']}/api/v1/providers/{provider}", payload={"api_key": "replacement-local-account"}, headers=admin_env["auth"])
    assert status == 200, body
    status, body = _request(admin_env, key, model, window + [{"role": "user", "content": "must not replay into another account"}])
    assert status == 400, body
    assert body["error"]["code"] == "compaction_target_mismatch", body


@pytest.mark.e2e
@pytest.mark.admin
def test_unsupported_target_rejects_standalone_compaction_without_upstream_request(admin_env):
    from tests.e2e.admin.test_observations import _create_route
    model = "native-unsupported-target"
    _route, key = _create_route(admin_env, model, protocol="openai-compatible")
    captured = admin_env["mock_server"].captured_requests
    before = len(captured)
    status, body = _request(admin_env, key, model, [{"role": "user", "content": "native semantics required"}], compact=True)
    assert status == 400, body
    assert body["error"]["code"] == "compaction_unsupported", body
    assert len(captured) == before, "unsupported standalone compaction must not become ordinary inference"


@pytest.mark.e2e
@pytest.mark.admin
def test_chat_target_omits_unrepresentable_inline_compaction_and_preserves_business_text(admin_env):
    from tests.e2e.admin.test_observations import _create_route
    model = "native-inline-omitted-controls"
    _route, key = _create_route(admin_env, model, protocol="openai-compatible")
    text = "continue the business request without native inline compaction"
    items = [{"role": "user", "content": text}]
    extra = {"context_management": [{"type": "compaction", "compact_threshold": 2000}]}
    captured = admin_env["mock_server"].captured_requests
    before = len(captured)
    # ADR-0006 omits unrepresentable inference controls; it does not emulate compact.
    result = _success(admin_env, key, model, items, **extra)
    assert result["status"] == "completed", result
    assert not any(item["type"] == "compaction" for item in result["output"])
    assert any(part.get("text") == "mock-ok-0" for item in result["output"]
               if item["type"] == "message" for part in item["content"]), result
    outbound = captured[before:]
    assert len(outbound) == 1, "omitted compaction must not cause a retry or additional operation"
    request = outbound[0]
    assert "context_management" not in request
    assert "input" not in request
    assert any(message.get("role") == "user" and message.get("content") == text
               for message in request["messages"]), request
    assert not any(message.get("type") in ("compaction", "compaction_trigger")
                   for message in request["messages"]), request


@pytest.mark.e2e
@pytest.mark.admin
def test_observation_trace_failure_cannot_break_native_continuation(stravia_binary, tmp_path, compaction_provider):
    from tests.common.helpers import stop_stravia_server
    from tests.e2e.admin.test_observation_failures import _start_initialized
    env, process, logs = _start_initialized(stravia_binary, tmp_path, compaction_provider[0])
    try:
        model = "native-trace-failure"
        route, key = _native_route(env, compaction_provider[0], model)
        source, window = _boundary(env, key, model)
        status, body = http_request("PUT", f"{env['admin']}/api/v1/observations/debug", payload={"enabled": True, "confirmed": True}, headers=env["auth"])
        assert status == 200, body
        trace_root = tmp_path / "diagnostics" / "observation-debug"
        trace_root.rename(tmp_path / "observation-debug-before-failure")
        trace_root.write_bytes(b"block managed trace storage")
        continued = _success(env, key, model, window + [{"role": "user", "content": "continue with broken diagnostics"}])
        run = _run(env, route, continued["id"])
        assert run["generation_parent_id"] == source["id"]
        assert run["trace"]["status"] == "partial"
        assert "storage_error" in run["trace"]["reasons"]
        next_window = _success(env, key, model, window + [{"role": "user", "content": "continue with broken diagnostics"}] + continued["output"], compact=True)["output"]
        final = _success(env, key, model, next_window + [{"role": "user", "content": "native publication still works"}])
        assert _run(env, route, final["id"])["generation_parent_id"] == continued["id"]
    finally:
        stop_stravia_server(process, logs)


@pytest.mark.e2e
@pytest.mark.admin
def test_delivered_native_state_survives_aborted_generation_without_false_commit(admin_env, compaction_provider):
    model = "native-aborted-generation"
    route, key = _native_route(admin_env, compaction_provider[0], model)
    original = [{"role": "user", "content": "removed-history-sentinel"}]
    source = _success(admin_env, key, model, original)
    request = Request(f"{admin_env['proxy']}/v1/responses", data=json.dumps({
        "model": model, "input": original + source["output"],
        "context_management": [{"type": "compaction", "compact_threshold": 2000}],
        "instructions": "abort-after-state", "stream": True,
    }).encode(), headers={"authorization": f"Bearer {key}", "content-type": "application/json"})
    state = None
    terminal_types = []
    with urlopen(request, timeout=15) as stream:
        for raw in stream:
            if not raw.startswith(b"data: ") or raw[6:].strip() == b"[DONE]":
                continue
            event = json.loads(raw[6:])
            terminal_types.append(event.get("type"))
            if event.get("type") == "response.output_item.done" and event["item"]["type"] == "compaction":
                state = event["item"]
    assert state is not None
    assert "response.completed" not in terminal_types
    resumed = _success(admin_env, key, model, [state, {"role": "user", "content": "resume published state after abort"}])
    assert _run(admin_env, route, resumed["id"])["generation_parent_id"] == source["id"]
    def terminated():
        runs = [run for row in _route_interactions(admin_env, route) for run in _detail(admin_env, row["id"])["runs"]]
        return runs if len(runs) >= 3 and all(run["status"] != "running" for run in runs) else None
    runs = _wait_for("truthful aborted native Run", terminated)
    assert {run["generation_node_id"] for run in runs if run.get("generation_node_id")} == {source["id"], resumed["id"]}


@pytest.mark.e2e
@pytest.mark.admin
def test_identity_free_state_resolves_by_complete_native_content(admin_env, compaction_provider):
    model = "native-no-state-id"
    route, key = _native_route(admin_env, compaction_provider[0], model)
    history = [{"role": "user", "content": "removed-history-sentinel"}]
    source = _success(admin_env, key, model, history)
    compacted = _success(admin_env, key, model, history + source["output"], compact=True, instructions="state-without-id")
    assert "id" not in compacted["output"][0]
    resumed = _success(admin_env, key, model, compacted["output"] + [{"role": "user", "content": "identity-free continuation"}])
    assert _run(admin_env, route, resumed["id"])["generation_parent_id"] == source["id"]


_ANTHROPIC_COMPACTION_BETA = "compact-2026-09-04"
_ANTHROPIC_THREADS_BETA = "message-threads-2026-08-12"
_ANTHROPIC_LOOKUP_TOOL = {
    "type": "function",
    "name": "lookup",
    "description": "Look up a value from the local archive.",
    "parameters": {
        "type": "object",
        "properties": {"subject": {"type": "string"}},
        "required": ["subject"],
    },
}


def _sse_events(raw: str) -> list[dict[str, Any]]:
    events: list[dict[str, Any]] = []
    for line in raw.splitlines():
        if not line.startswith("data:"):
            continue
        data = line[5:].strip()
        if data and data != "[DONE]":
            events.append(json.loads(data))
    return events


def _completed_response(raw: str) -> tuple[dict[str, Any], list[dict[str, Any]]]:
    events = _sse_events(raw)
    completed = [
        event["response"] for event in events
        if event.get("type") == "response.completed"
    ]
    assert len(completed) == 1, events
    return completed[0], events


def _response_output(payload: Any) -> list[dict[str, Any]]:
    if isinstance(payload, dict):
        result = payload
    else:
        result, _events = _completed_response(payload)
    output = result.get("output")
    assert isinstance(output, list), result
    return output


@pytest.fixture
def anthropic_compaction_provider():
    """本地 Anthropic 上游：普通推理交付签名思考+工具调用，压缩只接受原生 SSE。"""
    prefix_marker = "retired-history-marker-3f19"
    expected: dict[str, Any] = {
        "prefix_marker": prefix_marker,
        "prefix": [{"role": "user", "content": f"{prefix_marker}: keep the archive layout"}],
        "prompt": "Use lookup for the current budget.",
        "compact_instructions": "Summarize the retired requirements and keep the archive constraint.",
        "truncate_instructions": "truncate the compaction stream before message_stop",
        "error_instructions": "fail the compaction stream after the block",
        "continuation_prompt": "Continue after the compacted tail.",
        "thinking": "Check the lookup result before continuing.",
        "signature": "sig-tail-native-1",
        "text": "I will look up the budget.",
        "tool_call_id": "toolu_lookup_anchor_01",
        "tool_name": "lookup",
        "tool_input": {"subject": "budget"},
        "tool_result": "budget=42",
        "summary": "The archived layout constraint remains in force.",
        "compaction_signature": "sig-compaction-native-1",
        "tool_changes": [
            {
                "type": "tool_added",
                "name": "lookup",
                "input_schema": {"type": "object", "properties": {"subject": {"type": "string"}}},
            },
            {"type": "tool_removed", "name": "legacy_archive_reader"},
        ],
        "compaction_extra": {"opaque_iteration": {"kept": True, "weight": 3}},
        "continuation": "continued after the signed native compaction tail",
        "stream_error_message": "upstream refused the signed compaction window",
    }
    expected["block"] = {
        "type": "compaction",
        "content": expected["summary"],
        "signature": expected["compaction_signature"],
        "tool_changes": expected["tool_changes"],
        "opaque_future_field": expected["compaction_extra"],
    }
    received: list[dict[str, Any]] = []
    lock = threading.Lock()

    def betas(headers: dict[str, str]) -> list[str]:
        return [beta.strip() for beta in headers.get("anthropic-beta", "").split(",") if beta.strip()]

    def messages_json(body: dict[str, Any]) -> str:
        return json.dumps(body.get("messages", []), ensure_ascii=False, sort_keys=True)

    def compaction_blocks(messages: Any) -> list[dict[str, Any]]:
        if not isinstance(messages, list):
            return []
        blocks: list[dict[str, Any]] = []
        for message in messages:
            content = message.get("content") if isinstance(message, dict) else None
            if isinstance(content, list):
                blocks.extend(
                    block for block in content
                    if isinstance(block, dict) and block.get("type") == "compaction"
                )
        return blocks

    def check_first(body: dict[str, Any]) -> list[str]:
        errors: list[str] = []
        if body.get("stream") is not True:
            errors.append("the first inference must stream the signed tail")
        if body.get("compaction") is not None:
            errors.append("a signed-tail inference must not request compaction")
        serialized = messages_json(body)
        if expected["prefix_marker"] not in serialized:
            errors.append("the first inference lost the earlier prefix")
        if expected["prompt"] not in serialized:
            errors.append("the first inference lost the current prompt")
        tools = {
            tool.get("name") for tool in body.get("tools", [])
            if isinstance(tool, dict) and isinstance(tool.get("name"), str)
        }
        if expected["tool_name"] not in tools:
            errors.append("the first inference lost the tool declaration")
        if compaction_blocks(body.get("messages")):
            errors.append("a signed-tail inference replayed native state")
        return errors

    def check_compact(body: dict[str, Any], headers: dict[str, str]) -> list[str]:
        errors: list[str] = []
        if body.get("stream") is not True:
            errors.append("native compaction must force stream:true")
        control = body.get("compaction")
        if not isinstance(control, dict) or control.get("type") != "summarize":
            errors.append(f"native compaction control is malformed: {control!r}")
        elif set(control) - {"type", "instructions"}:
            errors.append(f"native compaction control has unsupported fields: {control!r}")
        enabled_betas = betas(headers)
        if _ANTHROPIC_COMPACTION_BETA not in enabled_betas:
            errors.append("native compaction beta is missing")
        if _ANTHROPIC_THREADS_BETA in enabled_betas:
            errors.append("thread beta cannot be combined with compaction")
        for key in ("thread", "diagnostics", "context_management", "stop_sequences"):
            if key in body:
                errors.append(f"{key} cannot be combined with compaction")
        if "tool_choice" in body:
            errors.append("tool_choice cannot be combined with compaction")
        output_config = body.get("output_config")
        if isinstance(output_config, dict) and "format" in output_config:
            errors.append("output_config.format cannot be combined with compaction")
        if expected["prefix_marker"] not in messages_json(body):
            errors.append("the compaction request lost the complete prefix history")
        if compaction_blocks(body.get("messages")):
            errors.append("the compaction request already replays native state")
        return errors

    def check_replay(body: dict[str, Any], headers: dict[str, str]) -> list[str]:
        errors: list[str] = []
        enabled_betas = betas(headers)
        if _ANTHROPIC_COMPACTION_BETA not in enabled_betas:
            errors.append("native replay must enable the compaction beta")
        if _ANTHROPIC_THREADS_BETA in enabled_betas:
            errors.append("thread beta cannot be combined with native replay")
        for key in ("thread", "diagnostics", "context_management"):
            if key in body:
                errors.append(f"{key} cannot be combined with native replay")
        if body.get("compaction") is not None:
            errors.append("a native replay is not a standalone compaction request")
        messages = body.get("messages")
        if not isinstance(messages, list) or len(messages) != 2:
            return errors + [f"native replay must contain exactly the compaction and tool turn: {messages!r}"]
        first, second = messages
        if first.get("role") != "assistant" or second.get("role") != "user":
            errors.append("native replay must keep the assistant tail before the tool result")
            return errors
        content = first.get("content")
        if not isinstance(content, list) or len(content) != 4:
            return errors + [f"native replay tail is not compaction/thinking/text/tool_use: {content!r}"]
        if content[0] != expected["block"]:
            errors.append("native replay altered the compaction signature, tool_changes or extra fields")
        expected_tail = [
            {"type": "thinking", "thinking": expected["thinking"], "signature": expected["signature"]},
            {"type": "text", "text": expected["text"]},
            {
                "type": "tool_use",
                "id": expected["tool_call_id"],
                "name": expected["tool_name"],
                "input": expected["tool_input"],
            },
        ]
        if content[1:] != expected_tail:
            errors.append(f"native replay changed the signed tail order or content: {content[1:]!r}")
        if len(compaction_blocks(messages)) != 1:
            errors.append("native replay must carry exactly one compaction block")
        if expected["prefix_marker"] in messages_json(body):
            errors.append("native replay revived the summarized history")
        second_content = second.get("content")
        if not isinstance(second_content, list):
            return errors + [f"native replay tool turn is malformed: {second_content!r}"]
        results = [
            block for block in second_content
            if isinstance(block, dict) and block.get("type") == "tool_result"
        ]
        if len(results) != 1:
            errors.append(f"native replay must pair the tool call with one result: {results!r}")
        else:
            result = results[0]
            if result.get("tool_use_id") != expected["tool_call_id"]:
                errors.append("native replay tool_result references the wrong tool call")
            if result.get("content") != expected["tool_result"]:
                errors.append("native replay changed the tool result")
        texts = [
            block.get("text") for block in second_content
            if isinstance(block, dict) and block.get("type") == "text"
        ]
        if expected["continuation_prompt"] not in texts:
            errors.append("native replay lost the continuation prompt")
        return errors

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def _json(self, status: int, payload: Any) -> None:
            data = json.dumps(payload, ensure_ascii=False).encode()
            self.send_response(status)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            self.wfile.flush()

        def _sse(self, events: list[tuple[str, dict[str, Any]]]) -> None:
            data = "".join(
                f"event: {name}\ndata: {json.dumps(payload, ensure_ascii=False)}\n\n"
                for name, payload in events
            ).encode()
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("content-length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            self.wfile.flush()

        def _reject(self, errors: list[str]) -> None:
            self._json(400, {
                "type": "error",
                "error": {
                    "type": "invalid_request_error",
                    "code": "invalid_native_compaction_window",
                    "message": "; ".join(errors),
                },
            })

        def _starter_events(self) -> list[tuple[str, dict[str, Any]]]:
            return [
                ("message_start", {
                    "type": "message_start",
                    "message": {
                        "id": "msg_native_tail",
                        "type": "message",
                        "role": "assistant",
                        "model": "anthropic-upstream-model",
                        "content": [],
                        "stop_reason": None,
                        "stop_sequence": None,
                        "usage": {"input_tokens": 21, "output_tokens": 1},
                    },
                }),
                ("ping", {"type": "ping"}),
                ("content_block_start", {
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "thinking", "thinking": ""},
                }),
                ("content_block_delta", {
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "thinking_delta", "thinking": expected["thinking"]},
                }),
                ("content_block_delta", {
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "signature_delta", "signature": expected["signature"]},
                }),
                ("content_block_stop", {"type": "content_block_stop", "index": 0}),
                ("content_block_start", {
                    "type": "content_block_start",
                    "index": 1,
                    "content_block": {"type": "text", "text": ""},
                }),
                ("content_block_delta", {
                    "type": "content_block_delta",
                    "index": 1,
                    "delta": {"type": "text_delta", "text": expected["text"]},
                }),
                ("content_block_stop", {"type": "content_block_stop", "index": 1}),
                ("ping", {"type": "ping"}),
                ("content_block_start", {
                    "type": "content_block_start",
                    "index": 2,
                    "content_block": {
                        "type": "tool_use",
                        "id": expected["tool_call_id"],
                        "name": expected["tool_name"],
                        "input": {},
                    },
                }),
                ("content_block_delta", {
                    "type": "content_block_delta",
                    "index": 2,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": json.dumps(expected["tool_input"]),
                    },
                }),
                ("content_block_stop", {"type": "content_block_stop", "index": 2}),
                ("message_delta", {
                    "type": "message_delta",
                    "delta": {"stop_reason": "tool_use", "stop_sequence": None},
                    "usage": {"output_tokens": 18},
                }),
                ("message_stop", {"type": "message_stop"}),
            ]

        def _compaction_events(self, *, truncate: bool = False) -> list[tuple[str, dict[str, Any]]]:
            usage = {
                "input_tokens": 0,
                "output_tokens": 0,
                "iterations": [
                    {"type": "compaction", "input_tokens": 1200, "output_tokens": 300},
                    {"type": "message", "input_tokens": 5, "output_tokens": 1},
                ],
                "opaque_usage": {"keep": True},
            }
            events = [
                ("message_start", {
                    "type": "message_start",
                    "message": {
                        "id": "msg_native_compaction",
                        "type": "message",
                        "role": "assistant",
                        "model": "anthropic-upstream-model",
                        "content": [],
                        "stop_reason": None,
                        "stop_sequence": None,
                        "usage": {"input_tokens": 0, "output_tokens": 0},
                    },
                }),
                ("ping", {"type": "ping"}),
                ("content_block_start", {
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": expected["block"],
                }),
                ("ping", {"type": "ping"}),
                ("content_block_stop", {"type": "content_block_stop", "index": 0}),
                ("message_delta", {
                    "type": "message_delta",
                    "delta": {"stop_reason": "compaction", "stop_sequence": None},
                    "usage": usage,
                }),
            ]
            if not truncate:
                events.append(("message_stop", {"type": "message_stop"}))
            return events

        def do_POST(self):
            path = self.path.split("?", 1)[0]
            length = int(self.headers.get("content-length", "0"))
            try:
                body = json.loads(self.rfile.read(length) or b"{}")
            except json.JSONDecodeError:
                self._reject(["request body is not JSON"])
                return
            headers = {key.lower(): value for key, value in self.headers.items()}
            with lock:
                received.append({"path": path, "body": body, "headers": headers})
            if path != "/v1/messages":
                self._json(404, {"type": "error", "error": {"type": "not_found_error", "message": path}})
                return
            if body.get("compaction") is not None:
                errors = check_compact(body, headers)
                if errors:
                    self._reject(errors)
                    return
                instruction_text = json.dumps(
                    [body.get("system"), body.get("messages")],
                    ensure_ascii=False,
                )
                if expected["truncate_instructions"] in instruction_text:
                    self._sse(self._compaction_events(truncate=True))
                elif expected["error_instructions"] in instruction_text:
                    self._sse([
                        *self._compaction_events(truncate=True),
                        ("error", {
                            "type": "error",
                            "error": {
                                "type": "overloaded_error",
                                "message": expected["stream_error_message"],
                            },
                        }),
                    ])
                else:
                    self._sse(self._compaction_events())
                return
            if compaction_blocks(body.get("messages")):
                errors = check_replay(body, headers)
                if errors:
                    self._reject(errors)
                    return
                if body.get("stream") is True:
                    self._sse([
                        ("message_start", {
                            "type": "message_start",
                            "message": {
                                "id": "msg_native_continuation",
                                "type": "message",
                                "role": "assistant",
                                "model": "anthropic-upstream-model",
                                "content": [],
                                "stop_reason": None,
                                "stop_sequence": None,
                                "usage": {"input_tokens": 12, "output_tokens": 1},
                            },
                        }),
                        ("content_block_start", {
                            "type": "content_block_start",
                            "index": 0,
                            "content_block": {"type": "text", "text": ""},
                        }),
                        ("content_block_delta", {
                            "type": "content_block_delta",
                            "index": 0,
                            "delta": {"type": "text_delta", "text": expected["continuation"]},
                        }),
                        ("content_block_stop", {"type": "content_block_stop", "index": 0}),
                        ("message_delta", {
                            "type": "message_delta",
                            "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                            "usage": {"output_tokens": 4},
                        }),
                        ("message_stop", {"type": "message_stop"}),
                    ])
                else:
                    self._json(200, {
                        "id": "msg_native_continuation",
                        "type": "message",
                        "role": "assistant",
                        "model": "anthropic-upstream-model",
                        "content": [{"type": "text", "text": expected["continuation"]}],
                        "stop_reason": "end_turn",
                        "stop_sequence": None,
                        "usage": {"input_tokens": 12, "output_tokens": 4},
                    })
                return
            errors = check_first(body)
            if errors:
                self._reject(errors)
                return
            self._sse(self._starter_events())

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}", expected, received
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


def _anthropic_route(env: dict[str, Any], base_url: str, name: str) -> tuple[str, str]:
    """创建 custom Anthropic Messages Target；请求必须真正走上游 loopback。"""
    status, body = http_request("POST", f"{env['admin']}/api/v1/providers", payload={
        "name": name,
        "source": {
            "type": "custom", "vendor": "custom", "channel": "default",
            "protocol": "anthropic-messages", "base_url": base_url,
        },
        "credential": {"type": "api_key", "value": "local-anthropic-credential"},
        "vendor_options": {},
    }, headers=env["auth"])
    assert status == 200, body
    provider_id = body["data"]["id"]
    status, body = http_request(
        "POST", f"{env['admin']}/api/v1/providers/{provider_id}/models",
        payload={
            "model_id": "anthropic-upstream-model",
            "metadata": {
                "name": name,
                "modalities": {"input": ["text"], "output": ["text"]},
                "reasoning_efforts": ["none", "low", "medium", "high"],
                "limit": {"context": 100000},
            },
        },
        headers=env["auth"],
    )
    assert status == 201, body
    status, body = http_request(
        "POST", f"{env['admin']}/api/v1/models",
        payload={
            "model_id": name,
            "output_modality": "text",
            "targets": [{"provider_id": provider_id, "model": "anthropic-upstream-model"}],
        },
        headers=env["auth"],
    )
    assert status == 200 and "data" in body, body
    route_id = body["data"]["id"]
    status, body = http_request(
        "POST", f"{env['admin']}/api/v1/api-keys",
        payload={"name": name, "model_ids": [route_id]},
        headers=env["auth"],
    )
    assert status == 200, body
    return route_id, body["data"]["key"]


@pytest.mark.e2e
@pytest.mark.admin
def test_anthropic_native_compaction_round_trip_replays_signed_tail_in_order(
    admin_env, anthropic_compaction_provider,
):
    base_url, expected, received = anthropic_compaction_provider
    model = "anthropic-native-compaction-round-trip"
    _route_id, key = _anthropic_route(admin_env, base_url, model)
    headers = {"authorization": f"Bearer {key}"}

    # 1. 普通流式推理取得 Responses 的 opaque reasoning 载体和 tool_use 尾部。
    status, raw = http_request(
        "POST", f"{admin_env['proxy']}/v1/responses",
        payload={
            "model": model,
            "input": expected["prefix"] + [{"role": "user", "content": expected["prompt"]}],
            "tools": [_ANTHROPIC_LOOKUP_TOOL],
            "reasoning": {"effort": "high"},
            "stream": True,
        },
        headers=headers,
    )
    assert status == 200, raw
    tail = _response_output(raw)
    assert [item["type"] for item in tail] == ["reasoning", "message", "function_call"], tail
    reasoning, message, function_call = tail
    assert reasoning["encrypted_content"]
    assert reasoning["encrypted_content"] != expected["signature"], "the Responses carrier must stay opaque"
    assert any(
        expected["thinking"] in part.get("text", "")
        for part in reasoning.get("content", [])
        if isinstance(part, dict)
    ), reasoning
    assert message["content"][0]["text"] == expected["text"]
    assert function_call["call_id"] == expected["tool_call_id"]
    assert function_call["name"] == expected["tool_name"]
    assert json.loads(function_call["arguments"]) == expected["tool_input"]

    # 2. Responses compact 必须把原生压缩改成 stream:true 的 Anthropic SSE 请求。
    status, compacted = responses_compaction(
        f"{admin_env['proxy']}/v1/responses",
        payload={
            "model": model,
            "input": expected["prefix"],
            "instructions": expected["compact_instructions"],
        },
        headers=headers,
    )
    assert status == 200, compacted
    compact_request = received[-1]
    assert compact_request["path"] == "/v1/messages"
    assert compact_request["body"]["stream"] is True
    assert compact_request["body"]["compaction"]["type"] == "summarize"
    assert expected["compact_instructions"] in json.dumps(
        compact_request["body"].get("system"),
        ensure_ascii=False,
    )
    assert compact_request["body"]["compaction"]["instructions"] != expected["compact_instructions"]
    enabled_betas = [
        beta.strip()
        for beta in compact_request["headers"].get("anthropic-beta", "").split(",")
        if beta.strip()
    ]
    assert _ANTHROPIC_COMPACTION_BETA in enabled_betas
    assert _ANTHROPIC_THREADS_BETA not in enabled_betas
    assert not any(key in compact_request["body"] for key in (
        "thread", "diagnostics", "context_management", "stop_sequences",
    ))
    assert expected["prefix_marker"] in json.dumps(compact_request["body"]["messages"])
    assert compacted["object"] == "response"
    assert len(compacted["output"]) == 1
    state = compacted["output"][0]
    assert state["type"] == "compaction"
    assert state["id"].startswith("cmp_")
    assert state["encrypted_content"] != expected["compaction_signature"]
    assert compacted["usage"]["input_tokens"] == 1200
    assert compacted["usage"]["output_tokens"] == 300
    assert compacted["usage"]["total_tokens"] == 1500

    # 3. 回放压缩状态、真实签名尾部与工具结果；上游逐字校验 blocks 顺序。
    status, continued = http_request(
        "POST", f"{admin_env['proxy']}/v1/responses",
        payload={
            "model": model,
            "input": compacted["output"] + tail + [
                {
                    "type": "function_call_output",
                    "call_id": function_call["call_id"],
                    "output": expected["tool_result"],
                },
                {"role": "user", "content": expected["continuation_prompt"]},
            ],
            "tools": [_ANTHROPIC_LOOKUP_TOOL],
        },
        headers=headers,
    )
    assert status == 200, continued
    replay_request = received[-1]
    assert replay_request["path"] == "/v1/messages"
    replay_betas = [
        beta.strip()
        for beta in replay_request["headers"].get("anthropic-beta", "").split(",")
        if beta.strip()
    ]
    assert _ANTHROPIC_COMPACTION_BETA in replay_betas
    assert _ANTHROPIC_THREADS_BETA not in replay_betas
    first_turn = replay_request["body"]["messages"][0]
    assert [block["type"] for block in first_turn["content"]] == [
        "compaction", "thinking", "text", "tool_use",
    ], first_turn
    assert first_turn["content"][0] == expected["block"]
    assert first_turn["content"][1]["signature"] == expected["signature"]
    assert first_turn["content"][2]["text"] == expected["text"]
    assert first_turn["content"][3]["id"] == expected["tool_call_id"]
    assert expected["prefix_marker"] not in json.dumps(replay_request["body"]["messages"])
    output = _response_output(continued)
    assert output[0]["type"] == "message"
    assert output[0]["content"][0]["text"] == expected["continuation"]
    assert len(received) == 3, "one signed-tail stream, one compact request, one replay"


@pytest.mark.e2e
@pytest.mark.admin
@pytest.mark.parametrize("mode", ["truncated", "stream-error"])
def test_anthropic_compaction_stream_requires_message_stop_and_preserves_error(
    admin_env, anthropic_compaction_provider, mode,
):
    base_url, expected, received = anthropic_compaction_provider
    model = f"anthropic-compaction-{mode}"
    _route_id, key = _anthropic_route(admin_env, base_url, model)
    instructions = (
        expected["truncate_instructions"]
        if mode == "truncated"
        else expected["error_instructions"]
    )
    status, response = responses_compaction(
        f"{admin_env['proxy']}/v1/responses",
        payload={
            "model": model,
            "input": expected["prefix"],
            "instructions": instructions,
        },
        headers={"authorization": f"Bearer {key}"},
    )
    assert status != 200, response
    assert not (isinstance(response, dict) and response.get("status") == "completed")
    if mode == "truncated":
        assert status == 502, response
    else:
        assert response["error"]["message"] == expected["stream_error_message"], response
    assert len(received) == 1, "a failed compaction stream must not retry or commit a state"
