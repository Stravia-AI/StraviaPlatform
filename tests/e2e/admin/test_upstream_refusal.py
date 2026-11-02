"""上游 Anthropic 以 stop_reason=refusal 终止时，三种入口都必须让客户端看到拒答。"""

from __future__ import annotations

import json
import threading
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any, Iterator

import pytest

from tests.common.helpers import http_bytes
from tests.e2e.admin.test_redaction_wire import wire_route

EXPLANATION = "This request was blocked by an upstream usage policy."
# 上游 stop_details 携带的账户侧凭据；任何客户端输出都不得包含它。
CREDIT_SENTINEL = "credit-token-sentinel-7f3a"
DETAILS = {
    "type": "refusal",
    "category": "reasoning_extraction",
    "explanation": EXPLANATION,
    "fallback_credit_token": CREDIT_SENTINEL,
    "fallback_has_prefill_claim": False,
}


def _sse(event: str, payload: dict[str, Any]) -> str:
    return f"event: {event}\ndata: {json.dumps(payload)}\n\n"


@contextmanager
def refusing_anthropic_upstream() -> Iterator[tuple[str, list[dict[str, Any]]]]:
    """HTTP 200、无内容块、输出 token 为 0 的 Anthropic 拒答；记录收到的每个请求。"""
    received: list[dict[str, Any]] = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args: Any) -> None:
            pass

        def do_POST(self) -> None:
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            received.append(body)
            usage = {"input_tokens": 2, "output_tokens": 0}
            if body.get("stream"):
                data = (
                    _sse("message_start", {"type": "message_start", "message": {
                        "id": "msg_refused", "type": "message", "role": "assistant",
                        "model": body["model"], "content": [], "stop_reason": None,
                        "usage": usage,
                    }})
                    + _sse("message_delta", {"type": "message_delta", "delta": {
                        "stop_reason": "refusal", "stop_sequence": None,
                        "stop_details": DETAILS,
                    }, "usage": usage})
                    + _sse("message_stop", {"type": "message_stop"})
                ).encode()
                content_type = "text/event-stream"
            else:
                data = json.dumps({
                    "id": "msg_refused", "type": "message", "role": "assistant",
                    "model": body["model"], "content": [], "stop_reason": "refusal",
                    "stop_sequence": None, "stop_details": DETAILS, "usage": usage,
                }).encode()
                content_type = "application/json"
            self.send_response(200)
            self.send_header("Content-Type", content_type)
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


def _sse_events(raw: bytes) -> list[tuple[str | None, Any]]:
    events: list[tuple[str | None, Any]] = []
    for block in raw.decode().split("\n\n"):
        name = None
        data = None
        for line in block.splitlines():
            if line.startswith("event: "):
                name = line[len("event: "):]
            elif line.startswith("data: "):
                data = line[len("data: "):]
        if data is not None and data != "[DONE]":
            events.append((name, json.loads(data)))
    return events


def _request(ingress: str, model: str, stream: bool) -> tuple[str, dict[str, Any], dict[str, str]]:
    if ingress == "open-responses":
        return "/v1/responses", {"model": model, "input": "hi", "stream": stream}, {}
    if ingress == "openai-compatible":
        return "/v1/chat/completions", {
            "model": model, "stream": stream,
            "messages": [{"role": "user", "content": "hi"}],
        }, {}
    return "/v1/messages", {
        "model": model, "stream": stream, "max_tokens": 64,
        "messages": [{"role": "user", "content": "hi"}],
    }, {"anthropic-version": "2023-06-01"}


def _client_view(ingress: str, stream: bool, raw: bytes) -> tuple[str, str]:
    """返回客户端按各协议惯例读到的 (拒答文本, 终止原因)。"""
    if not stream:
        body = json.loads(raw)
        if ingress == "open-responses":
            parts = [part for item in body["output"] if item["type"] == "message"
                     for part in item["content"] if part["type"] == "refusal"]
            return "".join(part["refusal"] for part in parts), body["status"]
        if ingress == "openai-compatible":
            choice = body["choices"][0]
            return choice["message"].get("refusal") or "", choice["finish_reason"]
        text = "".join(block.get("text", "") for block in body["content"])
        return text, body["stop_reason"]

    events = _sse_events(raw)
    if ingress == "open-responses":
        text = "".join(payload["refusal"] for name, payload in events
                       if name == "response.refusal.done")
        status = next(payload["response"]["status"] for name, payload in events
                      if name == "response.completed")
        return text, status
    if ingress == "openai-compatible":
        choices = [payload["choices"][0] for _, payload in events if payload.get("choices")]
        text = "".join(choice["delta"].get("refusal", "") for choice in choices)
        finish = [choice["finish_reason"] for choice in choices if choice["finish_reason"]]
        return text, finish[-1]
    text = "".join(payload["delta"].get("text", "") for name, payload in events
                   if name == "content_block_delta")
    stop = next(payload["delta"]["stop_reason"] for name, payload in events
                if name == "message_delta")
    return text, stop


@pytest.mark.e2e
@pytest.mark.admin
@pytest.mark.parametrize("stream", [False, True], ids=["unary", "stream"])
@pytest.mark.parametrize(
    ("ingress", "finish"),
    [
        ("open-responses", "completed"),
        ("openai-compatible", "content_filter"),
        ("anthropic-messages", "refusal"),
    ],
)
def test_upstream_refusal_is_visible_and_not_retried(
    admin_env: dict[str, Any], ingress: str, finish: str, stream: bool,
) -> None:
    with refusing_anthropic_upstream() as (url, received):
        model = f"refusal-{ingress}-{'stream' if stream else 'unary'}"
        key = wire_route(admin_env, url, "anthropic-messages", model)
        path, payload, extra = _request(ingress, model, stream)
        status, _, raw = http_bytes(
            "POST", f"{admin_env['proxy']}{path}",
            headers={"authorization": f"Bearer {key}", **extra}, payload=payload,
        )

    assert status == 200, raw
    assert CREDIT_SENTINEL.encode() not in raw
    # 拒答是终态：不重试、不切换目标。
    assert len(received) == 1
    assert _client_view(ingress, stream, raw) == (EXPLANATION, finish)
