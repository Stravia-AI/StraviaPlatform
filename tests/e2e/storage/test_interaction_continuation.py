from __future__ import annotations

import io
import json
import threading
import zipfile
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Iterator

import pytest

from tests.common.helpers import (
    download_observation_bundle,
    find_free_port,
    http_bytes,
    http_request,
    initialize_server,
    start_stravia_server,
    stop_stravia_server,
    wait_for_setup_token,
    wait_until_ready,
)
from tests.e2e.admin.test_observations import _create_route, _detail, _route_interactions, _wait_for


@contextmanager
def continuation_provider(protocol: str) -> Iterator[tuple[str, list[dict[str, Any]]]]:
    """只模拟外部 wire；续接、Marker、usage 和 Observation 均走真实服务。"""
    received: list[dict[str, Any]] = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args: Any) -> None:
            pass

        def do_POST(self) -> None:  # noqa: N802
            request = json.loads(self.rfile.read(int(self.headers["content-length"])))
            received.append(request)
            round_index = min(len(received) - 1, 5)
            call_count = 0 if round_index == 5 else (
                3 if protocol == "openai-compatible" and round_index == 2 else 2
            )
            calls = [
                {"id": f"probe-{round_index}-{index}", "name": "local_probe",
                 "args": {"round": round_index, "index": index}}
                for index in range(call_count)
            ]
            if protocol == "google-gemini":
                prompt = [12484, 18751, 19475, 20909, 22449, 23570][round_index]
                cache = [0, 0, 0, 0, 20339, 20337][round_index]
                candidates = [96, 86, 66, 27, 32, 992][round_index]
                thoughts = [241, 19, 25, 19, 23, 661][round_index]
                parts = [
                    {"functionCall": {"name": call["name"], "args": call["args"]},
                     "thoughtSignature": f"synthetic-protected-{round_index}-{index}"}
                    for index, call in enumerate(calls)
                ] or [
                    {"text": "Synthetic text before protected thought."},
                    {"text": "", "thought": True, "thoughtSignature": "synthetic-protected-final"},
                    {"text": "Synthetic tool work complete."},
                ]
                response = {
                    "candidates": [{"index": 0, "content": {"role": "model", "parts": parts},
                                    "finishReason": "STOP"}],
                    "usageMetadata": {
                        "promptTokenCount": prompt, "cachedContentTokenCount": cache,
                        "candidatesTokenCount": candidates, "thoughtsTokenCount": thoughts,
                        "totalTokenCount": prompt + candidates + thoughts,
                    },
                }
                stream = ":streamGenerateContent" in self.path
                responses = [response]
                if stream and round_index == 5:
                    responses.append({"usageMetadata": response["usageMetadata"]})
                    response = {**response, "usageMetadata": {
                        **response["usageMetadata"], "promptTokenCount": 23243,
                        "totalTokenCount": 24896,
                    }}
                    responses[0] = response
            else:
                prompt = [12766, 20581, 24791, 29417, 30224, 30539][round_index]
                cache = [10000, 16000, 20000, 24000, 24000, 23749][round_index]
                output = [301, 302, 303, 304, 305, 2184][round_index]
                thoughts = [100, 101, 102, 103, 104, 1200][round_index]
                message: dict[str, Any] = {
                    "role": "assistant", "reasoning_content": f"Synthetic public summary {round_index}.",
                    "content": None if calls else "Synthetic tool work complete.",
                }
                if calls:
                    message["tool_calls"] = [
                        {"id": call["id"], "type": "function",
                         "function": {"name": call["name"], "arguments": json.dumps(call["args"])}}
                        for call in calls
                    ]
                usage = {
                    "prompt_tokens": prompt, "completion_tokens": output,
                    "total_tokens": prompt + output,
                    "prompt_tokens_details": {"cached_tokens": cache},
                    "completion_tokens_details": {"reasoning_tokens": thoughts},
                }
                stream = request.get("stream", False)
                if stream:
                    delta = {**message}
                    if calls:
                        delta["tool_calls"] = [
                            {**call, "index": index} for index, call in enumerate(message["tool_calls"])
                        ]
                    responses = [
                        {"id": f"synthetic-{round_index}", "model": request["model"],
                         "object": "chat.completion.chunk",
                         "choices": [{"index": 0, "delta": delta, "finish_reason": None}]},
                        {"id": f"synthetic-{round_index}", "model": request["model"],
                         "object": "chat.completion.chunk",
                         "choices": [{"index": 0, "delta": {},
                                      "finish_reason": "tool_calls" if calls else "stop"}],
                         "usage": usage},
                    ]
                else:
                    responses = [{
                        "id": f"synthetic-{round_index}", "model": request["model"],
                        "object": "chat.completion", "usage": usage,
                        "choices": [{"index": 0, "message": message,
                                     "finish_reason": "tool_calls" if calls else "stop"}],
                    }]
            if stream:
                data = b"".join(
                    b"data: " + json.dumps(response).encode() + b"\n\n" for response in responses
                )
                if protocol == "openai-compatible":
                    data += b"data: [DONE]\n\n"
            else:
                data = json.dumps(responses[-1]).encode()
            self.send_response(200)
            self.send_header("content-type", "text/event-stream" if stream else "application/json")
            self.send_header("content-length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}", received
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


def replay_round(env: dict[str, Any], key: str, model: str, history: list[dict[str, Any]],
                 stream: bool) -> dict[str, Any]:
    payload = {
        "model": model, "stream": stream, "input": history,
        "tools": [{"type": "function", "name": "local_probe",
                   "description": "Read a synthetic local value.",
                   "parameters": {"type": "object", "properties": {
                       "round": {"type": "integer"}, "index": {"type": "integer"}}}}],
    }
    headers = {"authorization": f"Bearer {key}"}
    if stream:
        status, _, raw = http_bytes("POST", f"{env['proxy']}/v1/responses", payload=payload, headers=headers)
        assert status == 200, raw
        events = [
            json.loads(line[6:]) for line in raw.decode().splitlines()
            if line.startswith("data: ") and line[6:] != "[DONE]"
        ]
        assert not any(event["type"] in ("response.failed", "error") for event in events), events
        response = next(event["response"] for event in events if event["type"] == "response.completed")
        delivered_items = [
            event["item"] for event in events if event["type"] == "response.output_item.done"
        ]
        assert delivered_items == response["output"], "native item-done replay changed history"
    else:
        status, response = http_request(
            "POST", f"{env['proxy']}/v1/responses", payload=payload, headers=headers,
        )
        assert status == 200, response
        delivered_items = response["output"]
    # 回放真实交付的 item；只省略协议规定的非语义身份/状态，绝不手工改写边界。
    history.extend(
        {name: value for name, value in item.items() if name not in ("id", "status")}
        for item in delivered_items
    )
    return response


def exercise_continuation(env: dict[str, Any], protocol: str, mode: str) -> dict[str, Any]:
    with continuation_provider(protocol) as (upstream, received):
        model = f"synthetic-continuation-{protocol}-{mode}"
        route, key = _create_route({**env, "mock": upstream}, model, retry_budget=0, protocol=protocol)
        status, body = http_request("PUT", f"{env['admin']}/api/v1/observations/debug",
                                   payload={"enabled": True, "confirmed": True}, headers=env["auth"])
        assert status == 200, body
        question = "Read synthetic local values, then report completion."
        history: list[dict[str, Any]] = [{"role": "user", "content": question}]
        previous_node: str | None = None
        current_detail: dict[str, Any] = {}
        delivered_usage: list[dict[str, Any]] = []
        for round_index in range(6):
            stream = mode == "stream" or (mode == "alternating" and round_index % 2 == 0)
            response = replay_round(env, key, model, history, stream)
            delivered_usage.append(response["usage"])
            calls = [item for item in response["output"] if item["type"] == "function_call"]
            history.extend({"type": "function_call_output", "call_id": call["call_id"],
                            "output": f"synthetic result {round_index}-{index}"}
                           for index, call in enumerate(calls))

            def persisted() -> dict[str, Any] | None:
                interactions = _route_interactions(env, route)
                if len(interactions) != 1:
                    return None
                detail = _detail(env, interactions[0]["id"])
                if len(detail["runs"]) != round_index + 1:
                    return None
                if not all(run["generation_node_id"] and (run.get("trace") or {}).get("status") == "complete"
                           for run in detail["runs"]):
                    return None
                return detail

            current_detail = _wait_for("persisted actual-wire continuation", persisted)
            current_run = current_detail["runs"][-1]
            assert current_run["generation_parent_id"] == previous_node
            previous_node = current_run["generation_node_id"]
            assert calls or round_index == 5
        assert len(received) == 6
        events = [event for run in current_detail["runs"] for event in run["events"]]
        previews = [event for event in events if event["kind"] == "input_preview_recorded"]
        assert len(previews) == 1, previews
        assert current_detail["interaction"]["input_preview"] == question
        tool_count = 10 if protocol == "google-gemini" else 11
        assert len([event for event in events if event["kind"] == "client_tool_handoff"]) == tool_count
        assert len([event for event in events if event["kind"] == "client_tool_result"]) == tool_count
        expected = (
            {"input_tokens": 76962, "output_tokens": 2287, "cache_read_tokens": 40676}
            if protocol == "google-gemini"
            else {"input_tokens": 30569, "output_tokens": 3699, "cache_read_tokens": 117749}
        )
        for field, value in expected.items():
            assert current_detail["interaction"]["usage"][field] == value
        assert sum(usage["input_tokens"] for usage in delivered_usage) == (
            117638 if protocol == "google-gemini" else 148318
        )
        assert sum(usage["output_tokens"] for usage in delivered_usage) == expected["output_tokens"]
        assert sum(usage["output_tokens_details"]["reasoning_tokens"] for usage in delivered_usage) == (
            988 if protocol == "google-gemini" else 1710
        )
        if protocol == "google-gemini":
            assert current_detail["runs"][-1]["usage"]["input_tokens"] == 3233
        assert current_detail["interaction"]["usage"]["coverage"]["attempt_count"] == 6
        _, _, archive = download_observation_bundle(env, current_detail)
        with zipfile.ZipFile(io.BytesIO(archive)) as bundle:
            bundled = json.loads(bundle.read("interaction.json"))["events"]
        assert len([event for event in bundled if event.get("kind") == "input_preview_recorded"]) == 1
        return {"detail": current_detail, "model": model, "route": route, "key": key, "history": history}


@pytest.mark.e2e
@pytest.mark.storage
@pytest.mark.parametrize("backend", ["sqlite", "postgres"])
@pytest.mark.parametrize("protocol", ["google-gemini", "openai-compatible"])
@pytest.mark.parametrize("mode", ["stream", "nonstream", "alternating"])
def test_actual_wire_history_preserves_ancestry_and_confirmed_usage(
    stravia_binary: Path, storage_runtime: dict[str, Any], tmp_path: Path,
    backend: str, protocol: str, mode: str,
) -> None:
    pg_url = storage_runtime["pg_url"]
    if backend == "postgres" and not pg_url:
        pytest.skip("postgres backend requires DB_URL")
    schema = None
    database: dict[str, Any] = {"backend": "sqlite"}
    if backend == "postgres":
        schema = storage_runtime["make_isolated_schema"]("stravia_continuation")
        storage_runtime["run_schema_action"]("create", pg_url=pg_url, schema=schema)
        database = {"backend": "postgres", "url": storage_runtime["postgres_dsn_for_schema"](pg_url, schema)}
    port = find_free_port()
    base = f"http://127.0.0.1:{port}"
    process = None
    logs: list[str] = []
    try:
        process, logs = start_stravia_server(
            stravia_binary=stravia_binary,
            args=["--data-dir", str(tmp_path), "--host", "127.0.0.1", "--port", str(port)]
            + storage_runtime["server_args"](backend),
        )
        wait_until_ready(f"{base}/api/v1/auth/state")
        session = initialize_server(base, wait_for_setup_token(logs, process), database)
        exercise_continuation({"admin": base, "proxy": base, "auth": session.auth_headers()}, protocol, mode)
    finally:
        if process is not None:
            stop_stravia_server(process, logs)
        if schema is not None:
            storage_runtime["run_schema_action"]("drop", pg_url=pg_url, schema=schema)
