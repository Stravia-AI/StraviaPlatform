from __future__ import annotations

import hashlib
import json
from concurrent.futures import ThreadPoolExecutor
from urllib.request import Request, urlopen
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any

import pytest

from tests.common.helpers import http_request
from tests.e2e.admin.test_observations import _detail, _route_interactions, _wait_for


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
        {"type": "message", "role": "user", "content": "retained task identity"},
        {"type": "compaction", "id": "cmp_state_one", "encrypted_content": "opaque-state-one"},
        {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "retained answer", "annotations": []}]},
    ]
    received: list[dict[str, Any]] = []
    windows: dict[str, list[dict[str, Any]]] = {}
    lock = threading.Lock()
    release_streams = threading.Event()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

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
            if self.path.endswith("/responses/compact") or trigger or automatic:
                with lock:
                    for state in compact_window:
                        if state.get("type") == "compaction":
                            windows[state["encrypted_content"]] = compact_window
            if self.path.endswith("/responses/compact"):
                result = {"id": "cmp_operation_one" if body.get("instructions") == "complete-window" else f"cmp_operation_{signature}", "object": "response.compaction", "created_at": 1788825600, "output": compact_window}
                status = 200
            elif self.path.endswith("/responses"):
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
            if status == 200 and result.get("object") == "response" and (trigger or automatic):
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
                if body.get("instructions") in ("abort-after-state", "pause-after-state"):
                    boundary = next(i for i, event in enumerate(events) if event["type"] == "response.output_item.done" and event["item"]["type"] == "compaction")
                    events = events[:boundary + 1]
                for sequence, event in enumerate(events):
                    event["sequence_number"] = sequence
                data = "".join(f"event: {event['type']}\ndata: {json.dumps(event)}\n\n" for event in events).encode()
            else:
                data = json.dumps(result).encode()
            self.send_response(status)
            self.send_header("content-type", "text/event-stream" if streaming else "application/json")
            paused = bool(streaming) and body.get("instructions") == "pause-after-state"
            self.send_header("content-length", str(len(data) + int(paused)))
            self.end_headers()
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


def _native_route(env: dict[str, Any], base_url: str, name: str) -> tuple[str, str]:
    status, body = http_request("POST", f"{env['admin']}/api/v1/providers", payload={
        "name": name, "source": {"type": "custom", "vendor": "openai", "protocol": "open-responses", "base_url": base_url},
        "credential": {"type": "api_key", "value": "local-test-credential"},
    }, headers=env["auth"])
    assert status == 200, body
    provider_id = body["data"]["id"]
    status, body = http_request("POST", f"{env['admin']}/api/v1/providers/{provider_id}/models", payload={"model_id": "native-model", "metadata": {"name": name, "tool_call": True, "reasoning_options": [{"type": "effort", "values": ["none", "low", "medium", "high"]}], "limit": {"context": 100000, "output": 10000}}}, headers=env["auth"])
    assert status == 201, body
    status, body = http_request("POST", f"{env['admin']}/api/v1/models", payload={"model_id": name, "target_provider": provider_id, "target_model": "native-model"}, headers=env["auth"])
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
    status, compacted = http_request("POST", f"{admin_env['proxy']}/v1/responses/compact", payload={"model": "native-compaction-roundtrip", "input": source_input + source["output"], "instructions": "complete-window"}, headers=headers)
    assert status == 200, compacted
    assert compacted["object"] == "response.compaction"
    assert compacted["id"] == "cmp_operation_one"
    assert compacted["output"] == window
    assert "usage" not in compacted
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
    assert compact_runs[0]["usage"]["input_tokens"] is None
    assert compact_runs[0]["usage"]["output_tokens"] is None
    serialized = json.dumps({"interactions": interactions, "details": details})
    assert "opaque-state-one" not in serialized
    assert "local-test-credential" not in serialized


def _request(env, key, model, items, *, compact=False, **extra):
    return http_request("POST", f"{env['proxy']}/v1/responses" + ("/compact" if compact else ""),
                        payload={"model": model, "input": items, **extra},
                        headers={"authorization": f"Bearer {key}"})


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
def test_route_native_policy_has_one_owner_and_explicit_empty_controls_override(admin_env, compaction_provider):
    model = "native-policy"
    route, key = _native_route(admin_env, compaction_provider[0], model)
    def response(label, **extra):
        return _success(admin_env, key, model, [{"role": "user", "content": label}], **extra)
    def has_state(result):
        return any(item["type"] == "compaction" for item in result["output"])
    assert not has_state(response("default policy is off"))
    for threshold in [4000, 8000]:
        status, body = http_request("PUT", f"{admin_env['admin']}/api/v1/models/{model}", payload={"compaction_enabled": True, "compaction_threshold": threshold}, headers=admin_env["auth"])
        assert status == 200, body
        assert has_state(response(f"automatic policy {threshold}"))
    assert not has_state(response("explicit empty disables injection", context_management=[]))
    assert not has_state(response("explicit null remains null", context_management=None))
    assert has_state(response("client override", context_management=[{"type": "compaction", "compact_threshold": 2000}]))
    for invalid in [0, -1, 100001]:
        status, body = http_request("PUT", f"{admin_env['admin']}/api/v1/models/{model}", payload={"compaction_enabled": True, "compaction_threshold": invalid}, headers=admin_env["auth"])
        assert "error" in body and "COMPACTION_THRESHOLD" in str(body["error"]), body
    status, body = http_request("PUT", f"{admin_env['admin']}/api/v1/models/{model}", payload={"compaction_enabled": False}, headers=admin_env["auth"])
    assert status == 200, body
    assert not has_state(response("disabled again"))
    _boundary(admin_env, key, model, "client compact remains enabled")


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
def test_unsupported_target_does_not_silently_drop_native_semantics(admin_env):
    from tests.e2e.admin.test_observations import _create_route
    model = "native-unsupported-target"
    route, key = _create_route(admin_env, model)
    for extra in [{"compact": True}, {"context_management": [{"type": "compaction", "compact_threshold": 2000}]}]:
        status, body = _request(admin_env, key, model, [{"role": "user", "content": "native semantics required"}], **extra)
        assert status == 400, body
        assert body["error"]["code"] == "compaction_unsupported", body
    status, body = _request(admin_env, key, model, [{"type": "compaction_trigger"}])
    assert status == 400, body
    assert body["error"]["code"] == "compaction_unsupported", body


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
        trace_root = tmp_path / "observation-debug"
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
