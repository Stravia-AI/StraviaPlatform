from __future__ import annotations

import base64
import http.client
import io
import json
import threading
import time
import zipfile
from http.server import ThreadingHTTPServer
from pathlib import Path
from typing import Any
from urllib.parse import urlparse

import pytest

from tests.common.helpers import WebSession, _MinimalMockHandler, download_observation_bundle, http_request, observation_bundle_events
from tests.e2e.storage.conftest import LifecycleUpstream, _LifecycleHandler
from tests.e2e.storage.test_vendor_plugins import _create_api_key, _create_connection, _install_plugin
from tests.common.observation_contents import output_text
from tests.e2e.admin.test_compaction_diagnostics import _semantic, diagnostic_provider
from tests.e2e.admin.test_observations import (
    _create_route,
    _finished_detail,
    _route_interactions,
    _sse_event,
    _wait_for,
)


def _enable_debug(env: dict[str, Any]) -> None:
    status, body = http_request(
        "PUT",
        f"{env['admin']}/api/v1/observations/debug",
        payload={"enabled": True, "confirmed": True},
        headers=env["auth"],
    )
    assert status == 200, body


def _raw_post(
    env: dict[str, Any],
    body: bytes,
    *,
    authorization: str | None = None,
    chunks: list[bytes] | None = None,
) -> tuple[int, bytes]:
    endpoint = urlparse(env["proxy"])
    connection = http.client.HTTPConnection(endpoint.hostname, endpoint.port, timeout=15.0)
    connection.putrequest("POST", "/v1/chat/completions")
    connection.putheader("Content-Type", "application/json")
    if authorization is not None:
        connection.putheader("Authorization", f"Bearer {authorization}")
    if chunks is None:
        connection.putheader("Content-Length", str(len(body)))
        connection.endheaders(body)
    else:
        assert b"".join(chunks) == body
        connection.putheader("Transfer-Encoding", "chunked")
        connection.endheaders()
        for chunk in chunks:
            connection.send(f"{len(chunk):X}\r\n".encode("ascii"))
            connection.send(chunk)
            connection.send(b"\r\n")
        connection.send(b"0\r\n\r\n")
    response = connection.getresponse()
    result = response.status, response.read()
    connection.close()
    return result


def _finalized_route_detail(env: dict[str, Any], route_id: str) -> dict[str, Any] | None:
    interactions = _route_interactions(env, route_id)
    if not interactions:
        return None
    detail = _finished_detail(env, interactions[0]["id"])
    if detail is None:
        return None
    runs = detail.get("runs", [])
    if not runs or runs[0]["status"] == "running":
        return None
    if runs[0]["debug_enabled"] and (runs[0].get("trace") or {}).get("status") not in {"complete", "partial"}:
        return None
    return detail


def _request_body_events(events: list[dict[str, Any]]) -> list[dict[str, Any]]:
    chunks = [
        event
        for event in events
        if event.get("direction") == "client_to_platform"
        and event.get("transport") == "http"
        and event.get("message_type") == "body_chunk"
    ]
    if not chunks:
        # Legacy fixtures may still contain the pre-wire request_body record.
        return [
            event
            for event in events
            if event.get("direction") == "client_to_platform"
            and event.get("transport") == "http"
            and event.get("message_type") == "request_body"
        ]
    raw = bytearray()
    for event in chunks:
        payload = event.get("payload")
        if isinstance(payload, dict) and payload.get("encoding") == "base64":
            raw.extend(base64.b64decode(payload["data"]))
        elif isinstance(payload, str):
            raw.extend(payload.encode("utf-8"))
    combined = dict(chunks[0])
    try:
        combined["payload"] = bytes(raw).decode("utf-8")
    except UnicodeDecodeError:
        combined["payload"] = {"encoding": "base64", "data": base64.b64encode(raw).decode("ascii")}
    combined["message_type"] = "body_chunk"
    return [combined]


@pytest.mark.e2e
@pytest.mark.admin
@pytest.mark.parametrize("malformed", [False, True])
@pytest.mark.parametrize("carrier", ["openai", "bedrock"])
def test_rejected_media_capture_preserves_raw_payload(
    admin_env: dict[str, Any], malformed: bool, carrier: str,
) -> None:
    _enable_debug(admin_env)
    media = "cHJpdmF0ZS1tZWRpYS1ieXRlcy1tdXN0LW5vdC1wZXJzaXN0"
    ordinary = json.dumps({"type": "image", "inlineData": {"data": "b3JkaW5hcnktdGV4dA=="}})
    block = ({"image": {"format": "png", "source": {"bytes": media}}} if carrier == "bedrock"
             else {"type": "image_url", "image_url": {"url": f"data:image/png;base64,{media}"}})
    raw = json.dumps({"model": "rejected-media", "messages": [{"role": "user", "content": [
        {"type": "text", "text": ordinary}, block,
    ]}]}).encode()
    if malformed:
        raw = raw[:-3]
    rejected_at = int(time.time() * 1000)
    status, response = _raw_post(admin_env, raw)
    assert 400 <= status < 500, response

    def captured() -> dict[str, Any] | None:
        status, response = http_request("GET", f"{admin_env['admin']}/api/v1/observations/rejections", headers=admin_env["auth"])
        assert status == 200, response
        for item in response["data"]["items"]:
            if not item["debug_enabled"] or item["occurred_at"] < rejected_at:
                continue
            status, response = http_request("GET", f"{admin_env['admin']}/api/v1/observations/rejections/{item['id']}", headers=admin_env["auth"])
            assert status == 200, response
            detail = response["data"]
            if (detail.get("trace") or {}).get("status") in {"complete", "partial"}:
                return detail
        return None

    detail = _wait_for("externalized rejected media trace", captured)
    assert media not in json.dumps(detail)
    _, _, archive = download_observation_bundle(admin_env, detail)
    records = observation_bundle_events(archive)
    events = _request_body_events(records)
    encoded = json.dumps(events)
    assert media in encoded
    if not malformed:
        payload = json.loads(events[0]["payload"])
        assert payload["messages"][0]["content"][0]["text"] == ordinary


@pytest.mark.e2e
@pytest.mark.admin
def test_upstream_wire_capture_matches_received_ordered_fields(admin_env: dict[str, Any]) -> None:
    received: list[list[tuple[str, str]]] = []

    class Handler(_MinimalMockHandler):
        def do_POST(self) -> None:  # noqa: N802
            received.append(list(self.headers.raw_items()))
            super().do_POST()

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.captured_requests = []
    server.stream_error_release = threading.Event()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        _enable_debug(admin_env)
        local_env = {**admin_env, "mock": f"http://127.0.0.1:{server.server_port}"}
        route_id, key = _create_route(local_env, "wire-ordered-upstream")
        status, body = http_request(
            "POST", f"{admin_env['proxy']}/v1/chat/completions",
            payload={"model": "wire-ordered-upstream", "messages": [{"role": "user", "content": "hello"}]},
            headers={"authorization": f"Bearer {key}"},
        )
        assert status == 200, body
        assert len(received) == 1
        fields = received[0]
        assert all(name.lower() != "accept" for name, _ in fields)
        assert [(name.lower(), value) for name, value in fields if name.lower() == "authorization"] == [
            ("authorization", "Bearer upstream-secret")
        ]
        detail = _wait_for("finalized ordered upstream trace", lambda: _finalized_route_detail(admin_env, route_id))
        _, _, archive = download_observation_bundle(admin_env, detail)
        records = observation_bundle_events(archive)
        head = next(record for record in records if record.get("direction") == "upstream_request" and record.get("headers"))
        assert head["schema_version"] == 3
        assert head["headers"] == [
            {"name": name, "bytes": list(("***" if name.lower() == "authorization" else value).encode("latin-1"))}
            for name, value in fields
        ]
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


@pytest.mark.e2e
@pytest.mark.admin
def test_manual_wasm_preserves_interleaved_headers_on_upstream_wire(
    admin_env: dict[str, Any], repo_root: Path,
) -> None:
    received: list[list[tuple[str, str]]] = []

    class Handler(_LifecycleHandler):
        def do_POST(self) -> None:  # noqa: N802
            received.append(list(self.headers.raw_items()))
            super().do_POST()

    server = LifecycleUpstream()
    server.RequestHandlerClass = Handler
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        session = WebSession(admin_env["admin"])
        status, body = session.request(
            "POST", "/api/v1/auth/login",
            {"username": admin_env["username"], "password": admin_env["password"]},
        )
        assert status == 200, body
        _install_plugin(session, repo_root / "target" / "vendor-test-fixtures", "manual-entrypoints.wasm")
        connection = _create_connection(
            session, vendor="fixture.manual-entrypoints", name="wire-manual-provider",
            model_id="wire-manual-ordered", base_url=server.base_url,
            secret="wire-manual-credential",
        )
        status, body = session.request(
            "PUT", f"/api/v1/providers/{connection['provider_id']}",
            {"vendor_options": {"mode": "ordered-headers"}},
        )
        assert status == 200, body
        key = _create_api_key(session, [connection["route_id"]])["key"]
        _enable_debug(admin_env)
        status, body = http_request(
            "POST", f"{admin_env['proxy']}/v1/responses",
            payload={"model": connection["model_id"], "input": "exercise ordered headers"},
            headers={"authorization": f"Bearer {key}"}, timeout=30,
        )
        assert status == 200, body
        assert len(received) == 1
        fields = received[0]
        assert all(name.lower() != "accept" for name, _ in fields)
        assert [(name, value.encode("latin-1")) for name, value in fields if name.lower().startswith("x-wire-")] == [
            ("X-Wire-Duplicate", b"first"),
            ("x-Wire-Between", b"middle"),
            ("x-wire-duplicate", b"\x80\xff"),
        ]
        detail = _wait_for(
            "finalized manual ordered upstream trace",
            lambda: _finalized_route_detail(admin_env, connection["route_id"]),
        )
        _, _, archive = download_observation_bundle(admin_env, detail)
        records = observation_bundle_events(archive)
        head = next(record for record in records if record.get("direction") == "upstream_request" and record.get("headers"))
        assert head["schema_version"] == 3
        assert head["headers"] == [
            {"name": name, "bytes": list(("***" if name.lower() == "authorization" else value).encode("latin-1"))}
            for name, value in fields
        ]
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


@pytest.mark.e2e
@pytest.mark.admin
def test_http_wire_capture_preserves_exact_nonsecret_body(admin_env: dict[str, Any]) -> None:
    _enable_debug(admin_env)
    route_id, api_key = _create_route(admin_env, "wire-body-fidelity")
    raw = (
        '{\n  "model" : "wire-body-fidelity",\n'
        '  "messages" : [ { "role" : "user", "content" : "keep  spacing" } ]\n}\n'
    ).encode("utf-8")

    status, response = _raw_post(admin_env, raw, authorization=api_key)
    assert status == 200, response

    detail = _wait_for(
        "finalized exact-body trace",
        lambda: _finalized_route_detail(admin_env, route_id),
    )
    _, _, archive = download_observation_bundle(admin_env, detail)
    records = observation_bundle_events(archive)
    events = _request_body_events(records)
    assert len(events) == 1
    assert events[0]["payload"] == raw.decode("utf-8")
    assert isinstance(events[0]["payload"], str)


@pytest.mark.e2e
@pytest.mark.admin
@pytest.mark.parametrize("stream", [False, True], ids=["json", "sse"])
def test_observation_decodes_exact_large_unicode_upstream_content(
    admin_env: dict[str, Any], diagnostic_provider: Any, stream: bool,
) -> None:
    _enable_debug(admin_env)
    conversation = diagnostic_provider(f"wire-unicode-{stream}")
    text = "\n\n".join(
        f"段落 {number}: café — 保留完整的上游文本和空白。 " + "large upstream body 文本 " * 80
        for number in range(32)
    )
    messages = [{"role": "user", "content": "return the prepared Unicode paragraphs"}]
    answer = {"role": "assistant", "content": text}
    if stream:
        # The existing HTTP provider sends this prepared answer, independently of
        # the request text; exercise its SSE transport as well as JSON delivery.
        conversation.accepted[_semantic(messages)] = answer
        status, response = _raw_post(
            conversation.env,
            json.dumps({"model": conversation.name, "messages": messages, "stream": True}).encode("utf-8"),
            authorization=conversation.key,
        )
        assert status == 200, response
        returned = "".join(
            json.loads(line[6:])["choices"][0]["delta"].get("content", "")
            for line in response.decode("utf-8").splitlines()
            if line.startswith("data: {")
        )
        assert returned == text
    else:
        returned, _ = conversation.send(messages, answer=answer)
        assert returned["content"] == text

    detail = _wait_for(
        "finalized large Unicode upstream trace",
        lambda: _finalized_route_detail(conversation.env, conversation.route_id),
    )
    content_events = [
        event for event in detail["runs"][0]["events"]
        if event["kind"] == "client_visible_content"
    ]
    assert "".join(event["payload"]["text"] for event in content_events) == text
    _, _, archive = download_observation_bundle(conversation.env, detail)
    records = observation_bundle_events(archive)
    assert {event["direction"] for event in records} == {
        "client_to_platform", "upstream_request", "upstream_response", "platform_to_client",
    }


@pytest.mark.e2e
@pytest.mark.admin
@pytest.mark.parametrize(
    "debug_enabled,fragmented",
    [(True, False), (False, True)],
    ids=["debug-complete-message", "ordinary-fragmented-content"],
)
def test_client_visible_content_is_preserved_in_observation_artifacts(
    admin_env: dict[str, Any],
    debug_enabled: bool,
    fragmented: bool,
) -> None:
    status, body = http_request(
        "PUT",
        f"{admin_env['admin']}/api/v1/observations/debug",
        payload={"enabled": debug_enabled, "confirmed": debug_enabled},
        headers=admin_env["auth"],
    )
    assert status == 200, body
    model = "visible-content-fragmented" if fragmented else "visible-content-preservation"
    route_id, api_key = _create_route(admin_env, model)
    sentinel = "VISIBLE_RESPONSE_SECRET_7c91"
    safe = "retain-visible-business-output"
    expected_content = (
        f"{safe} Authorization: Bearer {sentinel} "
        f"callback=https://user:{sentinel}@example.test/cb?signature={sentinel} "
        f'metadata={{"api_key":"{sentinel}"}} '
        f"form=name=Ada&access_token={sentinel}"
    )
    raw = json.dumps(
        {
            "model": model,
            "messages": [{
                "role": "user",
                "content": "observation-visible-credential-fragmented" if fragmented else "observation-visible-credential",
            }],
            "stream": True,
        }
    ).encode("utf-8")

    status, response = _raw_post(admin_env, raw, authorization=api_key)
    assert status == 200, response
    client_content = "".join(
        json.loads(line[6:])["choices"][0]["delta"].get("content", "")
        for line in response.decode("utf-8").splitlines()
        if line.startswith("data: {")
    )
    assert client_content == expected_content

    detail = _wait_for(
        "finalized visible-content trace",
        lambda: _finalized_route_detail(admin_env, route_id),
    )
    assert detail["output_text"] == expected_content
    assert output_text(admin_env, detail["interaction"]) == expected_content

    visible_event = next(
        event
        for event in detail["runs"][0]["events"]
        if event["kind"] == "client_visible_content"
    )
    persisted_content = "".join(
        event["payload"]["text"]
        for event in detail["runs"][0]["events"]
        if event["kind"] == "client_visible_content"
    )
    assert persisted_content == expected_content

    replay = _sse_event(
        admin_env,
        visible_event["sequence"] - 1,
        lambda event: event["event"] == "observation"
        and event["data"]["sequence"] == visible_event["sequence"],
    )
    serialized_replay = json.dumps(replay)
    assert sentinel not in serialized_replay
    assert safe not in serialized_replay
    assert "payload" not in replay["data"]
    assert "text" not in replay["data"]
    assert set(replay["data"]) <= {
        "sequence", "occurred_at", "interaction_id", "root_id", "run_id",
        "rejection_id", "kind", "boundary",
    }
    assert int(replay["id"]) == visible_event["sequence"]
    assert replay["data"]["kind"] == visible_event["kind"]
    assert replay["data"]["interaction_id"] == detail["interaction"]["id"]
    assert replay["data"]["run_id"] == detail["runs"][0]["id"]
    assert replay["data"]["root_id"] == detail["interaction"]["root_id"]

    snapshot = _sse_event(
        admin_env, 0, interaction_id=detail["interaction"]["id"],
    )
    assert snapshot["event"] == "live_snapshot"
    assert snapshot["id"] == ""
    # Finalized text may already have left the volatile mirror. Its authoritative
    # business content is checked above in the actual persisted detail.
    for block in snapshot["data"]["blocks"]:
        assert block["interaction_id"] == detail["interaction"]["id"]
        assert block["run_id"] == detail["runs"][0]["id"]

    _, _, archive = download_observation_bundle(admin_env, detail)
    records = observation_bundle_events(archive)
    with zipfile.ZipFile(io.BytesIO(archive)) as bundle:
        contents = [bundle.read(name) for name in bundle.namelist()]
        exported = json.loads(bundle.read("interaction.json"))
    exported_content = "".join(
        event["payload"]["text"]
        for event in exported["events"]
        if event["kind"] == "client_visible_content"
    )
    assert exported_content == persisted_content
    assert exported_content == expected_content
    assert any(sentinel.encode() in content for content in contents)
    assert any(safe.encode() in content for content in contents)

    data_dir = Path(admin_env["data_dir"])
    # Ordinary facts preserve business content even when wire capture is off.
    trace = detail["runs"][0]["trace"]
    if debug_enabled:
        assert trace is not None
        client_head = next(
            record for record in records
            if record["direction"] == "client_to_platform" and record["headers"]
        )
        assert client_head["headers"]["authorization"] == "***"
        for event in records:
            if event.get("transport") != "sse" or not isinstance(event.get("payload"), str):
                continue
            for line in event["payload"].splitlines():
                if line.startswith("data: ") and line[6:] != "[DONE]":
                    json.loads(line[6:])
        trace_dir = data_dir / "diagnostics" / "observation-debug" / trace["trace_id"]
        manifest = json.loads((trace_dir / "manifest.json").read_text(encoding="utf-8"))
        assert manifest["schema_version"] >= 2
        assert manifest["trace_id"] == trace["trace_id"]
        assert manifest["run_id"] == detail["runs"][0]["id"]
        assert manifest["status"] == trace["status"]
        assert manifest["completed_at"] is not None
        assert manifest["tombstoned"] is False
    else:
        assert trace is None
        assert records == []


@pytest.mark.e2e
@pytest.mark.admin
def test_rejected_json_records_actual_body_without_execution_metadata(
    admin_env: dict[str, Any],
) -> None:
    _enable_debug(admin_env)
    malformed = b'{\n  "model": "broken",\n  "messages": [\n'
    rejected_at = int(time.time() * 1000)

    status, response = _raw_post(admin_env, malformed)
    assert 400 <= status < 500

    def captured_rejection() -> dict[str, Any] | None:
        status_, response = http_request(
            "GET",
            f"{admin_env['admin']}/api/v1/observations/rejections",
            headers=admin_env["auth"],
        )
        assert status_ == 200, response
        for item in response["data"]["items"]:
            if not item["debug_enabled"] or item["occurred_at"] < rejected_at:
                continue
            status_, candidate = http_request(
                "GET",
                f"{admin_env['admin']}/api/v1/observations/rejections/{item['id']}",
                headers=admin_env["auth"],
            )
            assert status_ == 200, candidate
            detail = candidate["data"]
            if (detail.get("trace") or {}).get("status") == "complete":
                return detail
        return None

    detail = _wait_for("rejected predecode body trace", captured_rejection)
    _, _, archive = download_observation_bundle(admin_env, detail)
    records = observation_bundle_events(archive)
    events = _request_body_events(records)
    assert len(events) == 1
    assert events[0]["payload"] == malformed.decode("utf-8")
    response_heads = [
        event
        for event in records
        if event.get("direction") == "platform_to_client"
        and event.get("message_type") == "response_head"
    ]
    response_chunks = [
        event["payload"]
        for event in records
        if event.get("direction") == "platform_to_client"
        and event.get("message_type") == "body_chunk"
    ]
    assert len(response_heads) == 1
    assert response_heads[0]["status_code"] == status
    assert "".join(response_chunks).encode("utf-8") == response
    assert "principal" not in detail["rejection"]
    assert "interaction_id" not in detail["rejection"]
    assert "run_id" not in detail["rejection"]
    assert all(event.get("interaction_id") is None for event in detail["events"])
    assert all(event.get("run_id") is None for event in detail["events"])


@pytest.mark.e2e
@pytest.mark.admin
def test_chunk_split_http_body_preserves_credential_named_metadata(
    admin_env: dict[str, Any],
) -> None:
    _enable_debug(admin_env)
    route_id, api_key = _create_route(admin_env, "wire-chunk-redaction")
    sentinel = "WIRE_CHUNK_SECRET_830ce9"
    raw = json.dumps(
        {
            "model": "wire-chunk-redaction",
            "messages": [{"role": "user", "content": "chunked request"}],
            "metadata": {"api_key": sentinel, "Authorization": sentinel},
        },
        separators=(",", ":"),
    ).encode("utf-8")
    secret_start = raw.index(sentinel.encode("utf-8"))
    chunks = [
        raw[: secret_start + 4],
        raw[secret_start + 4 : secret_start + 11],
        raw[secret_start + 11 :],
    ]

    status, response = _raw_post(
        admin_env,
        raw,
        authorization=api_key,
        chunks=chunks,
    )
    assert status == 200, response

    detail = _wait_for(
        "finalized chunk-redaction trace",
        lambda: _finalized_route_detail(admin_env, route_id),
    )
    _, _, archive = download_observation_bundle(admin_env, detail)
    records = observation_bundle_events(archive)
    events = _request_body_events(records)
    assert len(events) == 1
    assert isinstance(events[0]["payload"], str)
    assert events[0]["payload"] == raw.decode("utf-8")
