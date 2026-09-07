from __future__ import annotations

import json
import threading
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any, Iterator

import pytest

from tests.common.helpers import http_request
from tests.e2e.admin.test_reversible_redaction import REFERENCE, SECRET, set_enabled


@contextmanager
def wire_provider(protocol: str) -> Iterator[tuple[str, list[dict[str, Any]]]]:
    received: list[dict[str, Any]] = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args: Any) -> None:
            pass

        def do_POST(self) -> None:
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            received.append({"body": body, "headers": dict(self.headers)})
            if protocol == "anthropic-messages":
                text = body["messages"][-1]["content"][0]["text"]
                response = {
                    "id": "msg-wire", "type": "message", "role": "assistant",
                    "model": body["model"], "content": [{"type": "text", "text": text}],
                    "stop_reason": "end_turn", "stop_sequence": None,
                    "usage": {"input_tokens": 10, "output_tokens": 10},
                }
            elif protocol == "google-gemini":
                text = body["contents"][-1]["parts"][0]["text"]
                response = {
                    "candidates": [{"index": 0, "content": {
                        "role": "model", "parts": [{"text": text}],
                    }, "finishReason": "STOP"}],
                    "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 10,
                                      "totalTokenCount": 20},
                }
            else:
                text = body["messages"][-1]["content"]
                response = {
                    "id": "chatcmpl-wire", "object": "chat.completion", "model": body["model"],
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": text},
                                 "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 10, "total_tokens": 20},
                }
            data = json.dumps(response).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
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
        thread.join()


def wire_route(env: dict[str, Any], url: str, protocol: str, model: str) -> str:
    status, body = http_request(
        "POST", f"{env['admin']}/api/v1/providers", headers=env["auth"],
        payload={"name": model, "source": {"type": "custom", "vendor": "custom",
                 "protocol": protocol, "base_url": url},
                 "credential": {"type": "api_key", "value": "upstream-wire-auth"}},
    )
    assert status == 200, body
    provider_id = body["data"]["id"]
    status, body = http_request(
        "POST", f"{env['admin']}/api/v1/providers/{provider_id}/models", headers=env["auth"],
        payload={"model_id": "wire-model", "metadata": {
            "tool_call": True,
            "modalities": {"input": ["text", "image"], "output": ["text"]},
        }},
    )
    assert status == 201, body
    status, body = http_request(
        "POST", f"{env['admin']}/api/v1/models", headers=env["auth"],
        payload={"model_id": model, "target_provider": provider_id, "target_model": "wire-model"},
    )
    assert status == 200, body
    route_id = body["data"]["id"]
    status, body = http_request(
        "POST", f"{env['admin']}/api/v1/api-keys", headers=env["auth"],
        payload={"name": model, "model_ids": [route_id]},
    )
    assert status == 200, body
    return str(body["data"]["key"])


@pytest.mark.e2e
@pytest.mark.admin
@pytest.mark.parametrize("protocol", ["anthropic-messages", "google-gemini", "openai-compatible"])
def test_raw_encoder_carriers_are_protected_without_losing_fidelity(
    admin_env: dict[str, Any], protocol: str,
) -> None:
    # Only the late contextual occurrence identifies this synthetic value. Earlier
    # raw copies must be replaced in the same request, not just on the next turn.
    contextual = "Q8n4Vk7sT2p9X5a3Lc6D0h1R"
    opaque_data = contextual
    early = f"Earlier bare value: {contextual}"
    late = f"api_key={contextual}"
    echo = f"Echo {SECRET} and {contextual}"
    schema = {"type": "object", "properties": {
        SECRET: {"type": "string", "description": f"Credential {SECRET}"},
    }}
    cache = {"type": "ephemeral"}
    with wire_provider(protocol) as (url, received):
        model = f"redaction-wire-{protocol}"
        key = wire_route(admin_env, url, protocol, model)
        if protocol == "anthropic-messages":
            path = "/v1/messages"
            payload = {
                "model": model, "max_tokens": 100,
                "system": [{"type": "text", "text": early, "cache_control": cache}],
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": early,
                                                    "cache_control": cache}]},
                    {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": late, "signature": SECRET},
                        {"type": "image", "source": {
                            "type": "base64", "media_type": "image/png", "data": opaque_data,
                        }},
                        {"type": "tool_use", "id": "call-wire", "name": SECRET,
                         "input": {"type": "text", "value": SECRET}},
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "call-wire", "content": [
                            {"type": "text", "text": SECRET},
                            {"type": "image", "source": {
                                "type": "base64", "media_type": "image/png", "data": opaque_data,
                            }},
                        ]},
                    ]},
                    {"role": "user", "content": [{"type": "text", "text": echo}]},
                ],
                "tools": [{"name": SECRET, "description": f"Tool {SECRET}",
                           "input_schema": schema, "cache_control": cache}],
            }
        elif protocol == "google-gemini":
            path = f"/v1beta/models/{model}:generateContent"
            payload = {
                "systemInstruction": {"parts": [
                    {"text": early},
                    {"text": late, "thought": True, "thoughtSignature": SECRET},
                    {"executableCode": {"language": "PYTHON", "code": f"token = '{SECRET}'"}},
                    {"codeExecutionResult": {"outcome": "OUTCOME_OK", "output": SECRET}},
                ]},
                "contents": [
                    {"role": "model", "parts": [{"functionCall": {
                        "id": "call-json", "name": SECRET, "args": {"value": SECRET},
                    }}]},
                    {"role": "user", "parts": [{"functionResponse": {
                        "id": "call-json", "name": SECRET, "response": {
                            "type": "text", "value": SECRET, "content_kind": SECRET,
                            "items": [{"type": "text", "text": "ordinary", "value": SECRET}],
                        },
                    }}]},
                    {"role": "user", "parts": [{"text": echo}]},
                ],
                "tools": [{"googleSearch": {}, "functionDeclarations": [
                    {"name": SECRET, "description": f"Tool {SECRET}", "parameters": schema},
                ]}],
                "generationConfig": {"responseMimeType": "application/json", "responseSchema": schema},
            }
        else:
            path = "/v1/chat/completions"
            payload = {
                "model": model,
                "messages": [
                    {"role": "system", "content": early},
                    {"role": "assistant", "tool_calls": [{
                        "id": "call-wire-json", "type": "function",
                        "function": {"name": "lookup", "arguments": "{}"},
                    }]},
                    {"role": "tool", "tool_call_id": "call-wire-json",
                     "content": json.dumps([{"type": "image", "value": SECRET}]),
                     "__stravia_tool_result_content_kind": "content_blocks"},
                    {"role": "user", "content": f"{late}\n{echo}"},
                ],
                "prediction": {"type": "content", "content": f"Predict {SECRET}"},
                "response_format": {"type": "json_schema", "json_schema": {
                    "name": SECRET, "description": f"Format {SECRET}", "schema": schema,
                }},
                "user": SECRET,
            }
        set_enabled(admin_env, True)
        try:
            status, response = http_request(
                "POST", f"{admin_env['proxy']}{path}", payload=payload,
                headers={"authorization": f"Bearer {key}", "anthropic-version": "2023-06-01"},
                timeout=15.0,
            )
            assert status == 200, response
            actual = received[-1]["body"]
            headers = {name.lower(): value for name, value in received[-1]["headers"].items()}
            assert "upstream-wire-auth" in {
                headers.get("x-api-key"), headers.get("x-goog-api-key"),
                (headers.get("authorization") or "").removeprefix("Bearer "),
            }
            if protocol == "anthropic-messages":
                answer = "".join(part.get("text", "") for part in response["content"])
                protected_echo = actual["messages"][-1]["content"][0]["text"]
            elif protocol == "google-gemini":
                answer = "".join(part.get("text", "") for part in response["candidates"][0]["content"]["parts"])
                protected_echo = actual["contents"][-1]["parts"][0]["text"]
            else:
                answer = response["choices"][0]["message"]["content"]
                protected_echo = actual["messages"][-1]["content"]
            assert answer == (f"{late}\n{echo}" if protocol == "openai-compatible" else echo)
            references = REFERENCE.findall(protected_echo)
            assert len(set(references)) == 2
            secret_ref, contextual_ref = REFERENCE.findall(protected_echo.split("Echo ", 1)[1])
            assert protected_echo == answer.replace(SECRET, secret_ref).replace(contextual, contextual_ref)
            expected_schema = {"type": "object", "properties": {
                SECRET: {"type": "string", "description": f"Credential {secret_ref}"},
            }}
            if protocol == "anthropic-messages":
                assert actual["system"] == [{"type": "text", "text": early.replace(contextual, contextual_ref),
                                             "cache_control": cache}]
                assert actual["messages"][0]["content"][0]["cache_control"] == cache
                assert actual["messages"][0]["content"][0]["text"] == early.replace(contextual, contextual_ref)
                thinking, opaque, call = actual["messages"][1]["content"]
                assert thinking["thinking"] == late.replace(contextual, contextual_ref)
                assert thinking["signature"] == SECRET
                assert opaque["source"] == {
                    "type": "base64", "media_type": "image/png", "data": opaque_data,
                }
                assert call["id"] == "call-wire" and call["name"] == SECRET
                assert call["input"] == {"type": "text", "value": secret_ref}
                result = actual["messages"][2]["content"][0]
                assert result["tool_use_id"] == "call-wire"
                assert result["content"] == [
                    {"type": "text", "text": secret_ref},
                    {"type": "image", "source": {
                        "type": "base64", "media_type": "image/png", "data": opaque_data,
                    }},
                ]
                tool = actual["tools"][0]
                assert tool["cache_control"] == cache
                assert tool["input_schema"] == expected_schema
            elif protocol == "google-gemini":
                parts = actual["systemInstruction"]["parts"]
                assert parts[0]["text"] == early.replace(contextual, contextual_ref)
                assert parts[1] == {"text": late.replace(contextual, contextual_ref),
                                    "thought": True, "thoughtSignature": SECRET}
                assert parts[2]["executableCode"] == {"language": "PYTHON", "code": f"token = '{secret_ref}'"}
                assert parts[3]["codeExecutionResult"] == {"outcome": "OUTCOME_OK", "output": secret_ref}
                assert actual["tools"][0]["googleSearch"] == {}
                tool = actual["tools"][0]["functionDeclarations"][0]
                assert tool["parameters"] == expected_schema
                assert actual["generationConfig"]["responseSchema"] == expected_schema
                call = actual["contents"][0]["parts"][0]["functionCall"]
                assert call["args"] == {"value": secret_ref}
                result = actual["contents"][1]["parts"][0]["functionResponse"]
                result_wire = json.dumps(result, sort_keys=True)
                assert f'"value": {json.dumps(SECRET)}' not in result_wire
                assert json.dumps({
                    "type": "text", "value": secret_ref, "content_kind": secret_ref,
                    "items": [{"type": "text", "text": "ordinary", "value": secret_ref}],
                }, sort_keys=True) in result_wire
            else:
                result = next(message for message in actual["messages"] if message["role"] == "tool")
                assert result["content"] == json.dumps([{"type": "image", "value": secret_ref}])
                assert actual["prediction"] == {"type": "content", "content": f"Predict {secret_ref}"}
                tool = actual["response_format"]["json_schema"]
                assert tool["schema"] == expected_schema
                assert actual["user"] == SECRET
                assert tool["description"] == f"Format {secret_ref}"
            assert tool["name"] == SECRET
            assert "__stravia_tool_result_content_kind" not in json.dumps(actual)
            if protocol != "openai-compatible":
                assert tool["description"] == f"Tool {secret_ref}"
        finally:
            set_enabled(admin_env, False)


@pytest.mark.e2e
@pytest.mark.admin
def test_anthropic_encoded_tool_blocks_preserve_wire_shape_and_media(
    admin_env: dict[str, Any],
) -> None:
    credential = "Q8n4Vk7sT2p9X5a3Lc6D0h1R"
    with wire_provider("anthropic-messages") as (url, received):
        model = "redaction-encoded-tool-blocks"
        key = wire_route(admin_env, url, "anthropic-messages", model)
        set_enabled(admin_env, True)
        try:
            status, response = http_request(
                "POST", f"{admin_env['proxy']}/v1/messages",
                headers={"authorization": f"Bearer {key}", "anthropic-version": "2023-06-01"},
                payload={
                    "model": model, "max_tokens": 100,
                    "messages": [
                        {"role": "user", "content": "Look up the record"},
                        {"role": "assistant", "content": [{
                            "type": "tool_use", "id": "call-media", "name": "lookup", "input": {},
                        }]},
                        {"role": "user", "content": [{
                            "type": "tool_result", "tool_use_id": "call-media", "content": [
                                {"type": "text", "text": f"api_key={credential}"},
                                {"type": "image", "source": {
                                    "type": "base64", "media_type": "image/png", "data": credential,
                                }},
                            ],
                        }]},
                        {"role": "assistant", "content": "Result received"},
                        {"role": "user", "content": f"Echo {credential}"},
                    ],
                },
            )
            assert status == 200, response
            actual = received[-1]["body"]
            reference = REFERENCE.findall(actual["messages"][-1]["content"][0]["text"])[0]
            result = actual["messages"][2]["content"][0]
            assert isinstance(result["content"], str)
            assert json.loads(result["content"]) == [
                {"type": "text", "text": f"api_key={reference}"},
                {"type": "image", "source": {
                    "type": "base64", "media_type": "image/png", "data": credential,
                }},
            ]
            assert "__stravia_tool_result_content_kind" not in json.dumps(actual)
            assert response["content"][0]["text"] == f"Echo {credential}"
        finally:
            set_enabled(admin_env, False)
