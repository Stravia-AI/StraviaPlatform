from __future__ import annotations

import json
import re
import sqlite3
import threading
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing, contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any, Callable, Iterator
from urllib.request import Request, urlopen

import pytest

from tests.common.helpers import http_bytes, http_request
from tests.e2e.admin.test_observations import _create_route, _proxy


SECRET = "ghp_8Dq7mP2vL9sX4aR6tK3nF5wH1jB0cYzUeIoG"
REFERENCE = re.compile(r"~stravia-secret:[0-9a-f]{32}~")


@contextmanager
def echo_provider(
    *, tool: bool = False, transform: Callable[[str], str] | None = None,
    tool_values: Callable[[str], list[str]] | None = None,
    escaped_references: bool = False, finish_gate: threading.Event | None = None,
    reject: bool = False,
) -> Iterator[tuple[str, list[dict[str, Any]]]]:
    received: list[dict[str, Any]] = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args: Any) -> None:
            pass

        def do_POST(self) -> None:
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            received.append({"body": body, "authorization": self.headers.get("Authorization")})
            if reject:
                data = b'{"error":{"message":"synthetic unavailable","type":"server_error"}}'
                self.send_response(503)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)
                return
            message = body["messages"][-1]
            content = message.get("content") or ""
            if transform is not None:
                content = transform(content)
            answer: dict[str, Any] = {"role": "assistant", "content": content}
            finish = "stop"
            emit_tool = tool and message.get("role") != "tool"
            if emit_tool:
                values = tool_values(content) if tool_values else [content]
                calls = []
                for index, value in enumerate(values):
                    arguments = json.dumps({"value": value}, ensure_ascii=True)
                    if escaped_references:
                        arguments = REFERENCE.sub(
                            lambda match: "".join(f"\\u{ord(ch):04x}" for ch in match.group()),
                            arguments,
                        )
                    calls.append({
                        "id": f"call-redaction-{index}", "type": "function",
                        "function": {"name": "configure", "arguments": arguments},
                    })
                answer = {
                    "role": "assistant",
                    "content": None,
                    "tool_calls": calls,
                }
                finish = "tool_calls"
            if body.get("stream"):
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()

                def send(delta: dict[str, Any], reason: str | None = None) -> None:
                    event = {
                        "id": "chatcmpl-redaction", "object": "chat.completion.chunk",
                        "model": body["model"],
                        "choices": [{"index": 0, "delta": delta, "finish_reason": reason}],
                    }
                    self.wfile.write(f"data: {json.dumps(event)}\n\n".encode())
                    self.wfile.flush()

                send({"role": "assistant"})
                if emit_tool:
                    calls = answer["tool_calls"]
                    for index, call in enumerate(calls):
                        send({"tool_calls": [{
                            "index": index, "id": call["id"], "type": "function",
                            "function": {"name": "configure", "arguments": ""},
                        }]})
                    for offset in range(max(len(call["function"]["arguments"]) for call in calls)):
                        for index, call in enumerate(calls):
                            arguments = call["function"]["arguments"]
                            if offset < len(arguments):
                                send({"tool_calls": [{
                                    "index": index, "function": {"arguments": arguments[offset]},
                                }]})
                else:
                    for character in content:
                        send({"content": character})
                if finish_gate is not None:
                    assert finish_gate.wait(10), "client received no text before provider completion"
                send({}, finish)
                self.wfile.write(b"data: [DONE]\n\n")
                self.wfile.flush()
                return
            response = {
                "id": "chatcmpl-redaction",
                "object": "chat.completion",
                "model": body["model"],
                "choices": [{
                    "index": 0,
                    "message": answer,
                    "finish_reason": finish,
                }],
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


def set_enabled(env: dict[str, Any], enabled: bool) -> None:
    status, body = http_request(
        "PUT",
        f"{env['admin']}/api/v1/settings/reversible_redaction_enabled",
        payload={"value": "true" if enabled else "false"},
        headers=env["auth"],
    )
    assert status == 200, body


def mapping_sql(env: dict[str, Any], sql: str, *parameters: Any) -> list[tuple[Any, ...]]:
    with closing(sqlite3.connect(env["data_dir"] / "gateway.db", isolation_level=None)) as database:
        return database.execute(sql, parameters).fetchall()


@pytest.mark.e2e
@pytest.mark.admin
def test_reversible_redaction_plaintext_roundtrip(admin_env: dict[str, Any]) -> None:
    with echo_provider() as (url, received):
        env = {**admin_env, "mock": url}
        _, api_key = _create_route(env, "reversible-roundtrip")
        set_enabled(env, True)
        try:
            text = f"Keep this configuration unchanged: github_token={SECRET}; end."
            status, body = _proxy(
                env, api_key, "reversible-roundtrip", [{"role": "user", "content": text}],
            )
            assert status == 200, body
            upstream_text = received[-1]["body"]["messages"][-1]["content"]
            assert SECRET not in upstream_text
            reference = REFERENCE.search(upstream_text)
            assert reference is not None
            assert upstream_text == text.replace(SECRET, reference.group())
            assert body["choices"][0]["message"]["content"] == text
            assert received[-1]["authorization"] == "Bearer upstream-secret"
        finally:
            set_enabled(env, False)


@pytest.mark.e2e
@pytest.mark.admin
def test_redaction_dictionary_isolation_concurrency_and_switches(admin_env: dict[str, Any]) -> None:
    from tests.e2e.admin.test_credential_protection import key_discoveries
    from tests.e2e.admin.test_observations import _wait_for

    with echo_provider() as (url, received):
        env = {**admin_env, "mock": url}
        model = "reversible-isolation"
        route_id, first_key = _create_route(env, model)
        status, body = http_request(
            "POST", f"{env['admin']}/api/v1/api-keys",
            payload={"name": "isolated-second-key", "model_ids": [route_id]}, headers=env["auth"],
        )
        assert status == 200, body
        second_key = body["data"]["key"]
        secret = "Q8n4Vk7sT2p9X5a3Lc6D0h1R"

        def send(key: str, text: str) -> str:
            status, body = _proxy(env, key, model, [{"role": "user", "content": text}])
            assert status == 200, body
            return body["choices"][0]["message"]["content"]

        set_enabled(env, False)
        assert send(first_key, f"api_key={secret}") == f"api_key={secret}"
        assert received[-1]["body"]["messages"][-1]["content"] == f"api_key={secret}"
        set_enabled(env, True)
        try:
            text = f"Earlier bare value: {secret}\nLater configuration: api_key={secret}"
            earlier, later = text.split("\n")
            status, body = _proxy(env, first_key, model, [
                {"role": "system", "content": earlier},
                {"role": "user", "content": later},
            ])
            assert status == 200, body
            assert body["choices"][0]["message"]["content"] == later
            protected = received[-1]["body"]["messages"][-1]["content"]
            reference = REFERENCE.search(protected)
            assert reference is not None
            reference = reference.group()
            assert protected == later.replace(secret, reference)
            assert received[-1]["body"]["messages"][0]["content"] == earlier.replace(secret, reference)
            assert send(first_key, secret) == secret
            assert received[-1]["body"]["messages"][-1]["content"] == reference
            assert send(first_key, reference) == secret
            assert send(second_key, reference) == reference
            assert send(second_key, secret) == secret
            assert received[-1]["body"]["messages"][-1]["content"] == secret
            before = len(received)
            with ThreadPoolExecutor(max_workers=4) as executor:
                outputs = list(executor.map(lambda _: send(first_key, secret), range(4)))
            assert outputs == [secret] * 4
            assert all(
                request["body"]["messages"][-1]["content"] == reference
                for request in received[before:]
            )
            assert send(second_key, f"api_key={secret}") == f"api_key={secret}"
            second_reference = REFERENCE.search(received[-1]["body"]["messages"][-1]["content"])
            assert second_reference is not None and second_reference.group() != reference
            set_enabled(env, False)
            assert send(first_key, secret) == secret
            assert received[-1]["body"]["messages"][-1]["content"] == secret
            assert send(first_key, reference) == secret
            set_enabled(env, True)
            assert send(first_key, secret) == secret
            assert received[-1]["body"]["messages"][-1]["content"] == reference
            unknown = "~stravia-secret:00000000000000000000000000000000~"
            assert send(first_key, unknown) == unknown
            for key_name in [f"{model}-key", "isolated-second-key"]:
                rows = _wait_for("isolated first mapping discovery", lambda: key_discoveries(env, key_name))
                assert len(rows) == 1
                assert rows[0]["new_credential_count"] == 1
                assert secret not in json.dumps(rows)
                assert REFERENCE.search(json.dumps(rows)) is None
        finally:
            set_enabled(env, False)


@pytest.mark.e2e
@pytest.mark.admin
@pytest.mark.parametrize("failure", ["read", "intern", "publish", "publish-stream"])
def test_redaction_storage_failures_never_bypass_protection(
    admin_env: dict[str, Any], failure: str,
) -> None:
    from tests.e2e.admin.test_credential_protection import key_discoveries
    from tests.e2e.admin.test_observations import _route_interactions, _wait_for

    with echo_provider() as (url, received):
        env = {**admin_env, "mock": url}
        model = f"reversible-failure-{failure}"
        route, key = _create_route(env, model)
        committed_before = mapping_sql(env, "SELECT COUNT(*) FROM turn_chain_nodes")
        published_before = mapping_sql(env, "SELECT COUNT(*) FROM history_markers WHERE published_at IS NOT NULL")
        set_enabled(env, failure != "read")
        if failure == "read":
            # 关闭检测后，还原所需的真实存储故障也不能被当成未知引用。
            mapping_sql(env, "ALTER TABLE reversible_redaction_mappings RENAME TO redaction_e2e_unavailable")
        else:
            operation = "INSERT" if failure == "intern" else "UPDATE"
            condition = "" if failure == "intern" else "WHEN NEW.published_at IS NOT NULL"
            mapping_sql(env, f"""
                CREATE TRIGGER redaction_e2e_failure BEFORE {operation}
                ON reversible_redaction_mappings {condition}
                BEGIN SELECT RAISE(FAIL, 'injected mapping failure'); END
            """)
        try:
            text = SECRET if failure != "read" else "~stravia-secret:00000000000000000000000000000000~"
            if failure == "publish-stream":
                status, _, raw = http_bytes(
                    "POST", f"{env['proxy']}/v1/chat/completions",
                    payload={
                        "model": model, "stream": True,
                        "messages": [{"role": "user", "content": text}],
                    },
                    headers={"authorization": f"Bearer {key}"},
                )
                assert status == 200, raw
                events = [
                    json.loads(line[6:]) for line in raw.splitlines()
                    if line.startswith(b"data: {")
                ]
                failures = [event for event in events if "error" in event]
                assert failures and failures[-1]["error"]["code"] == "stream_mid_error"
                assert "injected" not in json.dumps(failures)
                assert SECRET not in json.dumps(failures)
                assert [
                    choice["finish_reason"] for event in events
                    for choice in event.get("choices", []) if choice.get("finish_reason") is not None
                ] == ["failed"]
            else:
                status, body = _proxy(env, key, model, [{"role": "user", "content": text}])
                assert status == 502, body
                assert body["error"]["code"] == "reversible_redaction_failed"
                assert "injected" not in json.dumps(body)
                assert SECRET not in json.dumps(body)
            assert len(received) == (1 if failure.startswith("publish") else 0)
            _wait_for(
                "failed protection interaction",
                lambda: [row for row in _route_interactions(env, route) if row["status"] == "interrupted"],
            )
            rows = key_discoveries(env, f"{model}-key")
            if failure.startswith("publish"):
                assert len(rows) == 1
                assert rows[0]["new_credential_count"] == 1
                assert rows[0]["status"] == "interrupted"
                assert SECRET not in json.dumps(rows)
                assert REFERENCE.search(json.dumps(rows)) is None
            else:
                assert rows == []
            if failure.startswith("publish"):
                wire = json.dumps(received[0]["body"])
                assert SECRET not in wire
                reference = REFERENCE.search(wire)
                assert reference is not None
                assert mapping_sql(
                    env, "SELECT published_at FROM reversible_redaction_mappings WHERE reference = ?",
                    reference.group(),
                ) == [(None,)]
            assert mapping_sql(env, "SELECT COUNT(*) FROM turn_chain_nodes") == committed_before
            assert mapping_sql(
                env, "SELECT COUNT(*) FROM history_markers WHERE published_at IS NOT NULL",
            ) == published_before
            assert not any(SECRET in line for line in env["logs"])
        finally:
            if failure == "read":
                mapping_sql(env, "ALTER TABLE redaction_e2e_unavailable RENAME TO reversible_redaction_mappings")
            else:
                mapping_sql(env, "DROP TRIGGER redaction_e2e_failure")
            set_enabled(env, False)


@pytest.mark.e2e
@pytest.mark.admin
def test_expired_reference_stays_literal_and_is_not_revived(admin_env: dict[str, Any]) -> None:
    from tests.e2e.admin.test_credential_protection import key_discoveries
    from tests.e2e.admin.test_observations import _wait_for

    with echo_provider() as (url, received):
        env = {**admin_env, "mock": url}
        model = "reversible-expiry"
        _, key = _create_route(env, model)
        set_enabled(env, True)
        try:
            status, body = _proxy(env, key, model, [{"role": "user", "content": SECRET}])
            assert status == 200, body
            assert body["choices"][0]["message"]["content"] == SECRET
            reference = REFERENCE.search(json.dumps(received[-1]["body"]))
            assert reference is not None
            reference = reference.group()
            mapping_sql(
                env, "UPDATE reversible_redaction_mappings SET expires_at = 0 WHERE reference = ?",
                reference,
            )
            set_enabled(env, False)
            status, body = _proxy(env, key, model, [{"role": "user", "content": reference}])
            assert status == 200, body
            assert body["choices"][0]["message"]["content"] == reference
            assert mapping_sql(
                env, "SELECT expires_at FROM reversible_redaction_mappings WHERE reference = ?", reference,
            ) == [(0,)]
            set_enabled(env, True)
            status, body = _proxy(env, key, model, [{"role": "user", "content": SECRET}])
            assert status == 200, body
            assert body["choices"][0]["message"]["content"] == SECRET
            replacement = REFERENCE.search(json.dumps(received[-1]["body"]))
            assert replacement is not None and replacement.group() != reference
            rows = _wait_for(
                "expired mapping recreated discovery",
                lambda: (lambda values: values if sum(row["new_credential_count"] for row in values) == 2 else None)(
                    key_discoveries(env, f"{model}-key")
                ),
            )
            assert sum(row["new_credential_count"] for row in rows) == 2
        finally:
            set_enabled(env, False)


@pytest.mark.e2e
@pytest.mark.admin
@pytest.mark.parametrize("tool", [False, True], ids=["answer", "tool-json"])
def test_streamed_redaction_restores_every_fragment_and_json_escape(
    admin_env: dict[str, Any], tool: bool,
) -> None:
    with echo_provider(tool=tool) as (url, received):
        env = {**admin_env, "mock": url}
        model = f"reversible-stream-{tool}"
        _, key = _create_route(env, model)
        set_enabled(env, True)
        try:
            text = f'中文🙂 before "{SECRET}" \\ path\nagain {SECRET}; after'
            payload: dict[str, Any] = {
                "model": model, "stream": True,
                "messages": [{"role": "user", "content": text}],
            }
            if tool:
                payload["tools"] = [{
                    "type": "function",
                    "function": {
                        "name": "configure",
                        "parameters": {
                            "type": "object", "properties": {"value": {"type": "string"}},
                            "required": ["value"],
                        },
                    },
                }]
            status, _, raw = http_bytes(
                "POST", f"{env['proxy']}/v1/chat/completions",
                payload=payload, headers={"authorization": f"Bearer {key}"},
            )
            assert status == 200, raw
            assert SECRET not in json.dumps(received[-1]["body"])
            events = [
                json.loads(line[6:]) for line in raw.decode().splitlines()
                if line.startswith("data: {")
            ]
            assert not any("error" in event for event in events), events
            deltas = [event["choices"][0]["delta"] for event in events if event.get("choices")]
            if tool:
                arguments = "".join(
                    call.get("function", {}).get("arguments", "")
                    for delta in deltas for call in delta.get("tool_calls", [])
                )
                assert json.loads(arguments) == {"value": text}
            else:
                assert "".join(delta.get("content", "") for delta in deltas) == text
            assert b"data: [DONE]" in raw
        finally:
            set_enabled(env, False)


@pytest.mark.e2e
@pytest.mark.admin
def test_target_retries_and_failover_keep_provider_text_protected(admin_env: dict[str, Any]) -> None:
    with echo_provider(reject=True) as (failed_url, rejected), echo_provider() as (url, received):
        env = {**admin_env, "mock": failed_url}
        model = "reversible-failover"
        _, key = _create_route(env, model, retry_budget=1)
        status, body = http_request("GET", f"{env['admin']}/api/v1/models/{model}", headers=env["auth"])
        assert status == 200, body
        first_provider = body["data"]["targets"][0]["provider_id"]
        status, body = http_request(
            "POST", f"{env['admin']}/api/v1/providers",
            payload={
                "name": "redaction-fallback",
                "source": {"type": "custom", "vendor": "custom", "protocol": "openai", "base_url": url},
                "credential": {"type": "api_key", "value": "upstream-secret"},
            },
            headers=env["auth"],
        )
        assert status == 200, body
        second_provider = body["data"]["id"]
        status, body = http_request(
            "POST", f"{env['admin']}/api/v1/providers/{second_provider}/models",
            payload={"model_id": "gpt-4o-mini", "metadata": {"name": "fallback", "tool_call": True}},
            headers=env["auth"],
        )
        assert status == 201, body
        status, body = http_request(
            "PUT", f"{env['admin']}/api/v1/models/{model}",
            payload={"targets": [
                {"provider_id": first_provider, "model": "gpt-4o-mini", "priority": 1,
                 "target_retry_budget": 1, "target_cooldown_ms": 0},
                {"provider_id": second_provider, "model": "gpt-4o-mini", "priority": 0,
                 "target_retry_budget": 0, "target_cooldown_ms": 0},
            ]},
            headers=env["auth"],
        )
        assert status == 200, body
        set_enabled(env, True)
        try:
            status, body = _proxy(env, key, model, [{"role": "user", "content": SECRET}])
            assert status == 200, body
            assert body["choices"][0]["message"]["content"] == SECRET
            assert len(rejected) == 2
            assert len(received) == 1
            references = set()
            for request in [*rejected, *received]:
                wire = json.dumps(request["body"])
                assert SECRET not in wire
                references.update(REFERENCE.findall(wire))
            assert len(references) == 1
        finally:
            set_enabled(env, False)


@pytest.mark.e2e
@pytest.mark.admin
def test_redaction_delivers_plain_text_before_provider_completion(admin_env: dict[str, Any]) -> None:
    finish_gate = threading.Event()
    with echo_provider(finish_gate=finish_gate) as (url, received):
        env = {**admin_env, "mock": url}
        model = "reversible-live-text"
        _, key = _create_route(env, model)
        set_enabled(env, True)
        text = f"Visible before completion: {SECRET}; end ~stravia-sec"
        try:
            request = Request(
                f"{env['proxy']}/v1/chat/completions",
                data=json.dumps({
                    "model": model, "stream": True,
                    "messages": [{"role": "user", "content": text}],
                }).encode(),
                headers={"authorization": f"Bearer {key}", "content-type": "application/json"},
                method="POST",
            )
            reconstructed = ""
            done = False
            with urlopen(request, timeout=15) as response:
                for raw in response:
                    if raw.strip() == b"data: [DONE]":
                        done = True
                    if not raw.startswith(b"data: {"):
                        continue
                    event = json.loads(raw[6:])
                    assert "error" not in event, event
                    for choice in event.get("choices", []):
                        reconstructed += choice["delta"].get("content", "")
                    if reconstructed.startswith("Visible") and not finish_gate.is_set():
                        # Provider 的结束事件仍被测试闸门阻塞，客户端已收到普通文本。
                        finish_gate.set()
            assert done
            assert reconstructed == text
            assert SECRET not in json.dumps(received[0]["body"])
        finally:
            finish_gate.set()
            set_enabled(env, False)


@pytest.mark.e2e
@pytest.mark.admin
def test_interleaved_tool_fields_do_not_join_partial_references(admin_env: dict[str, Any]) -> None:
    def values(content: str) -> list[str]:
        reference = REFERENCE.search(content)
        assert reference is not None
        return [reference.group()[:23], reference.group()[23:], reference.group()]

    with echo_provider(tool=True, tool_values=values, escaped_references=True) as (url, received):
        env = {**admin_env, "mock": url}
        model = "reversible-parallel-tools"
        _, key = _create_route(env, model)
        set_enabled(env, True)
        try:
            status, _, raw = http_bytes(
                "POST", f"{env['proxy']}/v1/chat/completions",
                payload={
                    "model": model, "stream": True,
                    "messages": [{"role": "user", "content": SECRET}],
                    "tools": [{"type": "function", "function": {
                        "name": "configure", "parameters": {
                            "type": "object", "properties": {"value": {"type": "string"}},
                        },
                    }}],
                },
                headers={"authorization": f"Bearer {key}"},
            )
            assert status == 200, raw
            arguments: dict[int, str] = {}
            for line in raw.splitlines():
                if not line.startswith(b"data: {"):
                    continue
                event = json.loads(line[6:])
                assert "error" not in event, event
                for choice in event.get("choices", []):
                    for call in choice["delta"].get("tool_calls", []):
                        index = call["index"]
                        arguments[index] = arguments.get(index, "") + call.get("function", {}).get("arguments", "")
            reference = REFERENCE.search(json.dumps(received[0]["body"]))
            assert reference is not None
            assert {index: json.loads(value)["value"] for index, value in arguments.items()} == {
                0: reference.group()[:23], 1: reference.group()[23:], 2: SECRET,
            }
            assert b"data: [DONE]" in raw
        finally:
            set_enabled(env, False)


@pytest.mark.e2e
@pytest.mark.admin
def test_client_tool_execution_and_result_protection_follow_switch(admin_env: dict[str, Any]) -> None:
    with echo_provider(tool=True) as (url, received):
        env = {**admin_env, "mock": url}
        model = "reversible-client-tool"
        _, key = _create_route(env, model)
        set_enabled(env, True)
        tools = [{"type": "function", "function": {
            "name": "configure", "parameters": {
                "type": "object", "properties": {"value": {"type": "string"}},
            },
        }}]
        try:
            reference = None
            for enabled in [True, False]:
                set_enabled(env, enabled)
                user = {"role": "user", "content": SECRET if enabled else reference}
                status, body = http_request(
                    "POST", f"{env['proxy']}/v1/chat/completions",
                    payload={"model": model, "messages": [user], "tools": tools},
                    headers={"authorization": f"Bearer {key}"},
                )
                assert status == 200, body
                assistant = body["choices"][0]["message"]
                call = assistant["tool_calls"][0]
                configured = json.loads(call["function"]["arguments"])["value"]
                assert configured == SECRET
                if enabled:
                    match = REFERENCE.search(json.dumps(received[-1]["body"]))
                    assert match is not None
                    reference = match.group()
                destination = env["data_dir"] / "client-tool-credential"
                destination.write_text(configured, encoding="utf-8")
                result = json.dumps({
                    "configured": destination.read_text(encoding="utf-8") == SECRET,
                    "api_key": SECRET,
                })
                status, body = _proxy(env, key, model, [
                    user, assistant, {"role": "tool", "tool_call_id": call["id"], "content": result},
                ])
                assert status == 200, body
                assert json.loads(body["choices"][0]["message"]["content"]) == {
                    "configured": True, "api_key": SECRET,
                }
                assert (SECRET in json.dumps(received[-1]["body"])) is not enabled
        finally:
            set_enabled(env, False)


@pytest.mark.e2e
@pytest.mark.admin
def test_redaction_covers_system_history_and_client_tool_result(admin_env: dict[str, Any]) -> None:
    with echo_provider() as (url, received):
        env = {**admin_env, "mock": url}
        model = "reversible-text-sources"
        _, key = _create_route(env, model)
        set_enabled(env, True)
        try:
            call = {
                "id": "call-history", "type": "function",
                "function": {"name": "configure", "arguments": json.dumps({"api_key": SECRET})},
            }
            messages = [
                {"role": "system", "content": f"Use token {SECRET}"},
                {"role": "user", "content": f"Historical token {SECRET}"},
                {"role": "assistant", "content": None, "tool_calls": [call]},
                {"role": "tool", "tool_call_id": call["id"], "content": f"Result: {SECRET}"},
                {"role": "user", "content": f"Now confirm {SECRET}"},
            ]
            status, body = _proxy(env, key, model, messages)
            assert status == 200, body
            actual = received[-1]["body"]["messages"]
            assert SECRET not in json.dumps(actual)
            references = set(REFERENCE.findall(json.dumps(actual)))
            assert len(references) == 1
            reference = references.pop()
            assert actual[0]["content"] == f"Use token {reference}"
            assert json.loads(actual[2]["tool_calls"][0]["function"]["arguments"]) == {"api_key": reference}
            assert actual[3]["content"] == f"Result: {reference}"
            assert body["choices"][0]["message"]["content"] == f"Now confirm {SECRET}"
        finally:
            set_enabled(env, False)


@pytest.mark.e2e
@pytest.mark.admin
def test_bundled_rules_apply_composites_filters_and_multiline_secrets(
    admin_env: dict[str, Any],
) -> None:
    from tests.e2e.admin.test_credential_protection import detect_text

    client_id = "AIK_CLIENT_e7fb9d1bb335069c097c2a02"
    # 使用可辨识的合成值覆盖熵与组合规则，避免将完整凭据格式写入 Git。
    client_secret = "AIK_SECRET_" + "0123456789abcdef" * 4
    private_key = (
        "-----BEGIN PRIVATE KEY-----\n"
        "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4vMDEyMzQ1Njc4OTo7PD0+P0BBQkNERUZH\n"
        "-----END PRIVATE KEY-----"
    )
    with echo_provider() as (url, received):
        env = {**admin_env, "mock": url}
        model = "reversible-rule-semantics"
        _, key = _create_route(env, model)
        set_enabled(env, True)
        try:
            def send(text: str) -> str:
                status, body = _proxy(env, key, model, [{"role": "user", "content": text}])
                assert status == 200, body
                assert body["choices"][0]["message"]["content"] == text
                protected = received[-1]["body"]["messages"][-1]["content"]
                matches = detect_text(env, text)
                encoded = text.encode("utf-16-le")
                matched = {
                    encoded[item["start"] * 2:item["end"] * 2].decode("utf-16-le")
                    for item in matches
                }
                assert bool(matched) == bool(REFERENCE.search(protected))
                assert all(value not in protected for value in matched)
                return protected

            generic_secret = "Q8n4Vk7sT2p9X5a3Lc6D0h1R"
            following_line = "\nimport { x } from 'pkg'"
            protected = send(f"api_key={generic_secret}{following_line}")
            assert generic_secret not in protected
            assert protected.endswith(following_line)
            long_secret = "Q8Z2V7K5N4J6X9W0" * 512
            protected = send(f"api_key={long_secret}")
            assert REFERENCE.fullmatch(protected.removeprefix("api_key="))
            assert send(client_secret) == client_secret
            low_entropy = f"{client_id}\nAIK_SECRET_{'A' * 64}"
            assert send(low_entropy) == low_entropy
            normal = "api_key=${SYNTHETIC_KEY}; ordinary prose, not a credential"
            assert send(normal) == normal
            composite = f"{client_id}\n{client_secret}"
            protected = send(composite)
            assert client_id not in protected and client_secret not in protected
            assert len(set(REFERENCE.findall(protected))) == 2
            protected = send(f"before\n{private_key}\nafter")
            assert "PRIVATE KEY" not in protected
            assert REFERENCE.fullmatch(protected.removeprefix("before\n").removesuffix("\nafter"))
        finally:
            set_enabled(env, False)


@pytest.mark.e2e
@pytest.mark.admin
@pytest.mark.parametrize("protocol", ["responses", "anthropic", "gemini"])
def test_reversible_redaction_cross_protocol_http(
    admin_env: dict[str, Any], protocol: str,
) -> None:
    with echo_provider() as (url, received):
        env = {**admin_env, "mock": url}
        model = f"reversible-{protocol}"
        _, key = _create_route(env, model)
        text = f"Keep this token: {SECRET}"
        if protocol == "responses":
            path = "/v1/responses"
            payload = {"model": model, "input": text}
        elif protocol == "anthropic":
            path = "/v1/messages"
            payload = {
                "model": model, "max_tokens": 100,
                "messages": [{"role": "user", "content": text}],
            }
        else:
            path = f"/v1beta/models/{model}:generateContent"
            payload = {"contents": [{"role": "user", "parts": [{"text": text}]}]}
        set_enabled(env, True)
        try:
            status, body = http_request(
                "POST", f"{env['proxy']}{path}", payload=payload,
                headers={"authorization": f"Bearer {key}", "anthropic-version": "2023-06-01"},
            )
            assert status == 200, body
            assert SECRET not in json.dumps(received[-1]["body"])
            assert REFERENCE.search(json.dumps(received[-1]["body"]))
            if protocol == "responses":
                answer = "".join(
                    part.get("text", "") for item in body["output"]
                    for part in item.get("content", [])
                )
            elif protocol == "anthropic":
                answer = "".join(part.get("text", "") for part in body["content"])
            else:
                answer = "".join(
                    part.get("text", "") for part in body["candidates"][0]["content"]["parts"]
                )
            assert answer == text
        finally:
            set_enabled(env, False)
