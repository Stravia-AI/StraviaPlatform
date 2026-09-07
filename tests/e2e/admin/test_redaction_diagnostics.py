from __future__ import annotations

import io
import json
import zipfile
from typing import Any
from urllib.request import Request, urlopen

import pytest

from tests.common.helpers import http_bytes, http_request
from tests.e2e.admin.test_observations import (
    _create_route, _detail, _proxy, _route_interactions, _wait_for,
)
from tests.e2e.admin.test_reversible_redaction import REFERENCE, SECRET, echo_provider, set_enabled


@pytest.mark.e2e
@pytest.mark.admin
@pytest.mark.parametrize("tool,stream", [(False, False), (False, True), (True, False), (True, True)])
@pytest.mark.parametrize("enabled", [True, False])
def test_restored_plaintext_is_scrubbed_from_diagnostics(
    admin_env: dict[str, Any], tool: bool, stream: bool, enabled: bool,
) -> None:
    def reply(content: str) -> str:
        reference = REFERENCE.search(content)
        assert reference is not None
        return f"ordinary-before {reference.group()} ordinary-after"

    with echo_provider(tool=tool, transform=reply) as (url, received):
        env = {**admin_env, "mock": url}
        model = f"diagnostic-redaction-{tool}-{stream}-{enabled}"
        route_id, api_key = _create_route(env, model)
        set_enabled(env, True)
        try:
            status, body = _proxy(env, api_key, model, [{"role": "user", "content": f"github_token={SECRET}"}])
            assert status == 200, body
            reference = REFERENCE.search(received[-1]["body"]["messages"][-1]["content"])
            assert reference is not None
            previous = {item["id"] for item in _wait_for(
                "seed observation", lambda: _route_interactions(env, route_id),
            )}
            status, state = http_request(
                "PUT", f"{env['admin']}/api/v1/observations/debug",
                payload={"enabled": True, "confirmed": True}, headers=env["auth"],
            )
            assert status == 200, state
            set_enabled(env, enabled)
            messages = [{"role": "user", "content": reference.group()}]
            expected = f"ordinary-before {SECRET} ordinary-after"
            if stream:
                request = Request(
                    f"{env['proxy']}/v1/chat/completions",
                    data=json.dumps({"model": model, "messages": messages, "stream": True}).encode(),
                    headers={"authorization": f"Bearer {api_key}", "content-type": "application/json"},
                )
                chunks: list[str] = []
                with urlopen(request, timeout=15) as response:
                    for line in response:
                        if not line.startswith(b"data:") or line.strip() == b"data: [DONE]":
                            continue
                        event = json.loads(line[5:])
                        for choice in event.get("choices", []):
                            delta = choice.get("delta", {})
                            if tool:
                                chunks.extend(call.get("function", {}).get("arguments", "") for call in delta.get("tool_calls", []))
                            else:
                                chunks.append(delta.get("content") or "")
                result = "".join(chunks)
                assert (json.loads(result)["value"] if tool else result) == expected
            else:
                status, body = _proxy(env, api_key, model, messages)
                assert status == 200, body
                message = body["choices"][0]["message"]
                result = json.loads(message["tool_calls"][0]["function"]["arguments"])["value"] if tool else message["content"]
                assert result == expected

            def finished() -> dict[str, Any] | None:
                for interaction in _route_interactions(env, route_id):
                    if interaction["id"] in previous:
                        continue
                    detail = _detail(env, interaction["id"])
                    if detail["runs"] and all((run.get("trace") or {}).get("status") == "complete" for run in detail["runs"]):
                        return detail
                return None

            detail = _wait_for("scrubbed finalized trace", finished)
            assert SECRET not in json.dumps(detail)
            if not tool:
                assert detail["interaction"]["visible_tail"] == "ordinary-before *** ordinary-after"
            events = [event for run in detail["runs"] for event in run["debug_events"]]
            for predicate in (
                lambda event: event.get("stage") == "client_projection_event",
                lambda event: event.get("direction") == "platform_to_client",
            ):
                records = [event for event in events if predicate(event)]
                encoded = json.dumps(records)
                assert records and "***" in encoded
                assert SECRET not in encoded
            terminal = json.dumps([event for event in events if event.get("stage") == "canonical_terminal_response"])
            assert "ordinary-before" in terminal and "ordinary-after" in terminal
            status, ticket = http_request(
                "POST", f"{env['admin']}/api/v1/observations/interactions/{detail['interaction']['id']}/debug-bundle-tickets",
                payload={"through_sequence": detail["snapshot_sequence"]}, headers=env["auth"],
            )
            assert status == 200, ticket
            download = ticket["data"]["download_url"]
            status, _, archive = http_bytes("GET", f"{env['admin']}{download}" if download.startswith("/") else download)
            assert status == 200
            with zipfile.ZipFile(io.BytesIO(archive)) as bundle:
                manifest = json.loads(bundle.read("manifest.json"))
                for run in manifest["runs"]:
                    assert run["capture_status"] == "complete"
                archived = [bundle.read(name) for name in bundle.namelist()]
                assert any(b"ordinary-before" in payload for payload in archived)
                for payload in archived:
                    assert SECRET.encode() not in payload
        finally:
            set_enabled(env, False)
            http_request(
                "PUT", f"{env['admin']}/api/v1/observations/debug",
                payload={"enabled": False, "confirmed": False}, headers=env["auth"],
            )
