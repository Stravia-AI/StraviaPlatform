from __future__ import annotations

import json
import threading
from contextlib import ExitStack
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any
from urllib.request import Request, urlopen

import pytest

from tests.e2e.admin.test_observations import (
    _create_route, _detail, _forest, _proxy, _route_interactions, _sse_event, _wait_for,
)
from tests.common.helpers import http_request


def _next_live_event(response: Any) -> dict[str, Any]:
    event, event_id, data = "", "", []
    while True:
        raw = response.readline()
        assert raw, "scoped SSE closed before its terminal boundary"
        line = raw.decode().rstrip("\r\n")
        if not line and data:
            return {"event": event, "id": event_id, "data": json.loads("\n".join(data))}
        if line.startswith("event:"):
            event = line[6:].strip()
        elif line.startswith("id:"):
            event_id = line[3:].strip()
        elif line.startswith("data:"):
            data.append(line[5:].strip())


@pytest.mark.e2e
@pytest.mark.admin
def test_scoped_current_snapshot_reconnect_and_global_redaction(admin_env: dict[str, Any]) -> None:
    release = threading.Event()
    emitted = {name: threading.Event() for name in ("scope-A", "scope-B")}

    class Provider(BaseHTTPRequestHandler):
        def log_message(self, *_args: object) -> None:
            pass

        def do_POST(self) -> None:  # noqa: N802
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            assert body["stream"] is True, body
            marker = body["messages"][-1]["content"]
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            self.wfile.write(b"data: " + json.dumps({"id": "synthetic-scoped",
                "object": "chat.completion.chunk", "model": body["model"],
                # A chunk ending at "B" can legitimately retain that prefix
                # while checking for the protected "Bearer ..." header.
                "choices": [{"index": 0, "delta": {"role": "assistant", "content": f"{marker} first body "},
                             "finish_reason": None}]}).encode() + b"\n\n")
            self.wfile.flush()
            emitted[marker].set()
            if not release.wait(timeout=20):
                return
            self.wfile.write(b"data: " + json.dumps({"id": "synthetic-scoped",
                "object": "chat.completion.chunk", "model": body["model"], "choices": [{"index": 0,
                "delta": {"content": "-actual-tail"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 4}}).encode() + b"\n\ndata: [DONE]\n\n")
            self.wfile.flush()

    provider = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    threading.Thread(target=provider.serve_forever, daemon=True).start()
    env = {**admin_env, "mock": f"http://127.0.0.1:{provider.server_port}"}
    outcomes: list[tuple[int, Any]] = []
    proxy_errors: list[BaseException] = []
    jobs: list[threading.Thread] = []
    subscriptions = ExitStack()
    scoped_responses = []
    try:
        route, key = _create_route(env, "observation-scope-isolation")
        before = _forest(env)["snapshot_sequence"]
        def request_scope(marker: str) -> None:
            try:
                outcomes.append(_proxy(
                    env, key, "observation-scope-isolation",
                    [{"role": "user", "content": marker}], body_extra={"stream": True}))
            except BaseException as error:
                proxy_errors.append(error)

        for marker in emitted:
            job = threading.Thread(target=request_scope, args=(marker,))
            job.start()
            jobs.append(job)
        for ready in emitted.values():
            assert ready.wait(timeout=10), "controlled provider never emitted its first body"
        def observed_scopes() -> list[dict[str, Any]] | None:
            items = _route_interactions(env, route)
            return items if {item["input_preview"] for item in items} == set(emitted) else None

        interactions = _wait_for("two scoped interactions", observed_scopes)
        for marker in emitted:
            interaction = next(item for item in interactions if item["input_preview"] == marker)
            last_snapshot: dict[str, Any] | None = None
            def current_scope_body() -> dict[str, Any] | None:
                nonlocal last_snapshot
                last_snapshot = _sse_event(env, 0, interaction_id=interaction["id"])
                return last_snapshot if marker in json.dumps(last_snapshot["data"]) else None

            try:
                snapshot = _wait_for("current scope body", current_scope_body)
            except BaseException as error:
                error.add_note(
                    f"scope={marker}; last_snapshot={last_snapshot!r}; "
                    f"proxy_outcomes={outcomes!r}; proxy_errors={proxy_errors!r}")
                raise
            assert snapshot["event"] == "live_snapshot"
            assert snapshot["id"] == ""
            assert ("scope-B" if marker == "scope-A" else "scope-A") not in json.dumps(snapshot["data"])
            assert snapshot["data"]["blocks"]
            reconnected = _sse_event(env, 0, interaction_id=interaction["id"])
            assert reconnected["event"] == "live_snapshot"
            assert reconnected["data"]["blocks"] == snapshot["data"]["blocks"]
            response = subscriptions.enter_context(urlopen(Request(
                f"{env['admin']}/api/v1/observations/interactions/{interaction['id']}/live",
                headers=env["auth"]), timeout=10))
            initial = _next_live_event(response)
            assert initial["event"] == "live_snapshot"
            scoped_responses.append((marker, interaction["id"], response))
        notification = _sse_event(env, before, lambda event: event["event"] == "observation")
        assert "payload" not in notification["data"]
        assert "scope-A" not in json.dumps(notification["data"])
        assert "scope-B" not in json.dumps(notification["data"])
        assert notification["data"]["root_id"]
        release.set()
        for marker, interaction_id, response in scoped_responses:
            received_tail = False
            while True:
                event = _next_live_event(response)
                assert event["id"] == ""
                assert ("scope-B" if marker == "scope-A" else "scope-A") not in json.dumps(event["data"])
                received_tail |= "-actual-tail" in json.dumps(event["data"])
                if event["event"] == "live_finished":
                    assert event["data"]["interaction_id"] == interaction_id
                    assert received_tail, "terminal boundary overtook the actually received final body"
                    break
        for job in jobs:
            job.join(timeout=10)
            assert not job.is_alive()
        assert [status for status, _ in outcomes] == [200, 200]
        assert not proxy_errors, proxy_errors
        for interaction in interactions:
            detail = _wait_for("persisted actual tail", lambda: (
                value if (value := _detail(env, interaction["id"]))["interaction"]["status"] == "completed" else None))
            assert "-actual-tail" in json.dumps(detail)
    finally:
        release.set()
        subscriptions.close()
        for job in jobs:
            job.join(timeout=10)
        provider.shutdown()
        provider.server_close()


@pytest.mark.e2e
@pytest.mark.admin
def test_root_delta_idempotence_and_expired_snapshot_recovery(admin_env: dict[str, Any]) -> None:
    route, key = _create_route(admin_env, "observation-root-delta-http")
    assert _proxy(admin_env, key, "observation-root-delta-http", [{"role": "user", "content": "synthetic delta"}])[0] == 200
    def completed_root() -> dict[str, Any] | None:
        page = _forest(admin_env)
        return page if any(
            item["status"] == "completed"
            for root in page["roots"] for item in root["interactions"]
            if item["first_route_id"] == route
        ) else None

    forest = _wait_for("completed delta root", completed_root)
    root = next(root for root in forest["roots"] if any(item["first_route_id"] == route for item in root["interactions"]))
    known = [{"id": item["id"], "last_event_sequence": item["last_event_sequence"],
              "matched": item["matched"], "debug_status": item["debug_status"]} for item in root["interactions"]]
    filters = {"anchor_at": forest["anchor_at"], "window_index": forest["window_index"]}
    query = {"filters": filters, "roots": [{"root_id": root["id"],
             "after_sequence": forest["snapshot_sequence"], "known_interactions": known}]}
    for _ in range(2):
        status, response = http_request("POST", f"{admin_env['admin']}/api/v1/observations/interactions/changes",
                                        payload=query, headers=admin_env["auth"])
        assert status == 200, response
        assert response["data"]["reset_required"] is False
        assert all(not change["interactions"] and not change["removed_interaction_ids"]
                   for change in response["data"]["changes"])
    query["roots"][0]["known_interactions"] = []
    status, response = http_request("POST", f"{admin_env['admin']}/api/v1/observations/interactions/changes",
                                    payload=query, headers=admin_env["auth"])
    assert status == 200, response
    assert response["data"]["reset_required"] is False
    discovered = [item for change in response["data"]["changes"] for item in change["interactions"]]
    assert {item["id"] for item in discovered} == {item["id"] for item in root["interactions"]}
    assert all(item["matched"] for item in discovered)
    query["roots"][0]["known_interactions"] = [{**item, "matched": False} for item in known]
    status, response = http_request("POST", f"{admin_env['admin']}/api/v1/observations/interactions/changes",
                                    payload=query, headers=admin_env["auth"])
    assert status == 200, response
    corrected = [item for change in response["data"]["changes"] for item in change["interactions"]]
    assert {item["id"] for item in corrected} == {item["id"] for item in root["interactions"]}
    assert all(item["matched"] for item in corrected)
    query["roots"][0]["after_sequence"] = forest["snapshot_sequence"] + 1000000
    status, response = http_request("POST", f"{admin_env['admin']}/api/v1/observations/interactions/changes",
                                    payload=query, headers=admin_env["auth"])
    assert status == 200, response
    assert response["data"]["reset_required"] is True
