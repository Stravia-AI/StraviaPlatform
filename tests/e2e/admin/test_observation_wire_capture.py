from __future__ import annotations

import http.client
import io
import json
import sqlite3
import time
import zipfile
from contextlib import closing
from pathlib import Path
from typing import Any
from urllib.parse import urlparse

import pytest

from tests.common.helpers import http_bytes, http_request
from tests.e2e.admin.test_observations import (
    _create_route,
    _detail,
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
    detail = _detail(env, interactions[0]["id"])
    runs = detail.get("runs", [])
    if not runs or runs[0]["status"] == "running":
        return None
    if runs[0]["debug_enabled"] and (runs[0].get("trace") or {}).get("status") not in {"complete", "partial"}:
        return None
    return detail


def _request_body_events(detail: dict[str, Any]) -> list[dict[str, Any]]:
    debug_events = detail.get("debug_events")
    if debug_events is None:
        debug_events = detail["runs"][0]["debug_events"]
    return [
        event
        for event in debug_events
        if event.get("direction") == "client_to_platform"
        and event.get("transport") == "http"
        and event.get("message_type") == "request_body"
    ]


@pytest.mark.e2e
@pytest.mark.admin
@pytest.mark.parametrize("malformed", [False, True])
@pytest.mark.parametrize("carrier", ["openai", "bedrock"])
def test_rejected_media_capture_omits_payload_and_declares_loss(
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
            if any(event.get("message_type") == "request_body" for event in detail.get("debug_events", [])):
                return detail
        return None

    detail = _wait_for("externalized rejected media trace", captured)
    assert media not in json.dumps(detail)
    events = _request_body_events(detail)
    assert any("unrecoverable" in json.dumps(event["payload"]) for event in events)
    assert all(event["representation"] == "artifact_externalized" for event in events)
    if not malformed:
        payload = json.loads(events[0]["payload"])
        assert payload["messages"][0]["content"][0]["text"] == ordinary


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
    events = _request_body_events(detail)
    assert len(events) == 1
    assert events[0]["payload"] == raw.decode("utf-8")
    assert isinstance(events[0]["payload"], str)


@pytest.mark.e2e
@pytest.mark.admin
@pytest.mark.parametrize(
    "debug_enabled,fragmented",
    [(True, False), (False, True)],
    ids=["debug-complete-message", "ordinary-fragmented-content"],
)
def test_client_visible_credentials_are_redacted_from_observation_artifacts(
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
    model = "visible-content-fragmented" if fragmented else "visible-content-redaction"
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
        "finalized visible-content redaction trace",
        lambda: _finalized_route_detail(admin_env, route_id),
    )
    serialized_detail = json.dumps(detail)
    assert sentinel not in serialized_detail, [
        (event.get("direction"), event.get("stage"), event.get("message_type"))
        for run in detail["runs"]
        for event in run["debug_events"]
        if sentinel in json.dumps(event)
    ]
    assert safe in serialized_detail
    assert sentinel not in detail["interaction"]["visible_tail"]
    assert safe in detail["interaction"]["visible_tail"]

    visible_event = next(
        event
        for event in detail["runs"][0]["events"]
        if event["kind"] == "client_visible_content_delta"
    )
    replay = _sse_event(admin_env, visible_event["sequence"] - 1)
    serialized_replay = json.dumps(replay)
    assert sentinel not in serialized_replay
    assert safe in serialized_replay

    status, ticket = http_request(
        "POST",
        f"{admin_env['admin']}/api/v1/observations/interactions/{detail['interaction']['id']}/debug-bundle-tickets",
        payload={"through_sequence": detail["snapshot_sequence"]},
        headers=admin_env["auth"],
    )
    assert status == 200, ticket
    download_url = ticket["data"]["download_url"]
    if download_url.startswith("/"):
        download_url = f"{admin_env['admin']}{download_url}"
    status, _, archive = http_bytes("GET", download_url)
    assert status == 200
    with zipfile.ZipFile(io.BytesIO(archive)) as bundle:
        contents = [bundle.read(name) for name in bundle.namelist()]
    assert all(sentinel.encode() not in content for content in contents)
    assert any(safe.encode() in content for content in contents)

    data_dir = Path(admin_env["data_dir"])
    with closing(sqlite3.connect(data_dir / "gateway.db")) as connection:
        tails = connection.execute(
            "SELECT visible_tail FROM interaction_observations WHERE id = ?",
            (detail["interaction"]["id"],),
        ).fetchall()
        events = connection.execute(
            "SELECT payload FROM observation_events WHERE interaction_id = ?",
            (detail["interaction"]["id"],),
        ).fetchall()
    assert all(sentinel not in value for (value,) in tails + events)
    # Generation Chain preserves business content; only diagnostic artifacts use this policy.
    trace = detail["runs"][0]["trace"]
    if debug_enabled:
        assert trace is not None
        for event in detail["runs"][0]["debug_events"]:
            if event.get("transport") != "sse" or not isinstance(event.get("payload"), str):
                continue
            for line in event["payload"].splitlines():
                if line.startswith("data: ") and line[6:] != "[DONE]":
                    json.loads(line[6:])
        trace_dir = data_dir / "observation-debug" / trace["trace_id"]
        for path in trace_dir.rglob("*"):
            if path.is_file():
                assert sentinel.encode() not in path.read_bytes(), path
    else:
        assert trace is None
        assert detail["runs"][0]["debug_events"] == []


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
            if any(
                event.get("message_type") == "request_body"
                and event.get("payload") == malformed.decode("utf-8")
                for event in detail.get("debug_events", [])
            ):
                return detail
        return None

    detail = _wait_for("rejected predecode body trace", captured_rejection)
    events = _request_body_events(detail)
    assert len(events) == 1
    assert events[0]["payload"] == malformed.decode("utf-8")
    response_heads = [
        event
        for event in detail["debug_events"]
        if event.get("direction") == "platform_to_client"
        and event.get("message_type") == "response_head"
    ]
    response_chunks = [
        event["payload"]
        for event in detail["debug_events"]
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
def test_chunk_split_http_credential_is_redacted_only_after_complete_body(
    admin_env: dict[str, Any],
) -> None:
    _enable_debug(admin_env)
    route_id, api_key = _create_route(admin_env, "wire-chunk-redaction")
    sentinel = "WIRE_CHUNK_SECRET_830ce9"
    raw = json.dumps(
        {
            "model": "wire-chunk-redaction",
            "messages": [{"role": "user", "content": "chunked request"}],
            "metadata": {"api_key": sentinel},
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
    serialized = json.dumps(detail)
    assert sentinel not in serialized
    events = _request_body_events(detail)
    assert len(events) == 1
    assert isinstance(events[0]["payload"], str)
    assert "***" in events[0]["payload"]
