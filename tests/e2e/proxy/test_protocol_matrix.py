"""Protocol-conversion matrix: client_protocol × recorded_fixture.

For every recorded ``replay_model`` (any vendor × any upstream protocol ×
any scenario) we send a request via *every* stravia ingress path and assert:

    * status == 200
    * the scenario's anchor token survives the round-trip
    * every protocol-specific expected field name appears in the response

This validates Stravia's bidirectional protocol conversion against real
recorded LLM bytes.
"""

from __future__ import annotations

import json
import re
import time

import pytest

from tests.common.helpers import download_observation_bundle, http_request, observation_bundle_events
from tests.e2e.proxy.conftest import (
    STRAVIA_BASE_URL_PATH,
    PROTOCOLS,
    _scan_replay_models,
)

INGRESS_PROTOCOLS = list(PROTOCOLS)
OBSERVED_INGRESS_PROTOCOL = {
    "openai-chat": "openai-compatible/chat-completions/v1",
    "open-responses": "open-responses/responses/2026-04-24",
    "anthropic-messages": "anthropic-messages/messages/2023-06-01",
    "google-content": "google-gemini/generate-content/v1beta",
}

# Real LLMs tokenise the anchor (e.g. ``STRAVIA_PROBE_BASIC_STREAM``) into
# ``NY`` / ``RO`` / ``_PRO`` / ... fragments emitted across many SSE frames.
# The naive ``anchor in raw_text`` check fails because of the JSON/SSE
# framing in between. Extract every value of the text-carrying JSON keys
# (``content`` / ``text`` / ``reasoning_content`` / ``thinking`` / ``delta``)
# and concatenate them so a fragmented anchor reassembles into the original
# token, while ignoring ``modelVersion`` and other framing noise.
_TEXT_FIELD_RE = re.compile(
    r'"(?:content|text|reasoning_content|thinking|delta)"\s*:\s*"((?:[^"\\]|\\.)*)"'
)


def _extract_visible_text(text: str) -> str:
    parts: list[str] = []
    for match in _TEXT_FIELD_RE.finditer(text):
        try:
            parts.append(json.loads('"' + match.group(1) + '"'))
        except json.JSONDecodeError:
            parts.append(match.group(1))
    return "".join(parts)


# ---------------------------------------------------------------------------
# collection-phase helpers (must run before pytest fixtures kick in)
# ---------------------------------------------------------------------------


def _collect_replay_models() -> list[str]:
    out: list[str] = []
    for models in _scan_replay_models().values():
        out.extend(models)
    return sorted(out)


def _parse_replay_model(rm: str) -> tuple[str, str, str]:
    parts = rm.split("--")
    if len(parts) != 3:
        pytest.fail(f"invalid replay_model `{rm}` (expected vendor--protocol--scenario)")
    return parts[0], parts[1], parts[2]


ALL_REPLAY_MODELS = _collect_replay_models()


# ---------------------------------------------------------------------------
# request body / URL builders per ingress protocol
# ---------------------------------------------------------------------------


def _build_request_body(ingress: str, vmodel: str, stream: bool) -> dict:
    prompt = f"stravia-replay probe for {vmodel}"
    if ingress == "openai-chat":
        return {
            "model": vmodel,
            "stream": stream,
            "messages": [{"role": "user", "content": prompt}],
        }
    if ingress == "open-responses":
        return {"model": vmodel, "stream": stream, "input": prompt}
    if ingress == "anthropic-messages":
        return {
            "model": vmodel,
            "stream": stream,
            "max_tokens": 256,
            "messages": [{"role": "user", "content": prompt}],
        }
    if ingress == "google-content":
        return {
            "contents": [{"role": "user", "parts": [{"text": prompt}]}],
        }
    pytest.fail(f"unknown ingress protocol: {ingress}")


def _build_request_url(base: str, ingress: str, vmodel: str, stream: bool) -> str:
    if ingress == "openai-chat":
        return f"{base}/v1/chat/completions"
    if ingress == "open-responses":
        return f"{base}/v1/responses"
    if ingress == "anthropic-messages":
        return f"{base}/v1/messages"
    if ingress == "google-content":
        action = "streamGenerateContent" if stream else "generateContent"
        query = "?alt=sse" if stream else ""
        return f"{base}/v1beta/models/{vmodel}:{action}{query}"
    pytest.fail(f"unknown ingress protocol: {ingress}")


def _request_headers(ingress: str, api_key: str) -> dict[str, str]:
    headers = {"authorization": f"Bearer {api_key}"}
    if ingress == "anthropic-messages":
        headers["anthropic-version"] = "2023-06-01"
    return headers


# ---------------------------------------------------------------------------
# tests
# ---------------------------------------------------------------------------


@pytest.mark.e2e
@pytest.mark.proxy
@pytest.mark.parametrize("ingress_protocol", INGRESS_PROTOCOLS)
@pytest.mark.parametrize("replay_model", ALL_REPLAY_MODELS)
def test_protocol_matrix(
    stravia_proxy_base: dict[str, object],
    scenario_metadata: dict[str, dict],
    ingress_protocol: str,
    replay_model: str,
) -> None:
    _, _, scenario_name = _parse_replay_model(replay_model)
    meta = scenario_metadata.get(scenario_name)
    if meta is None:
        pytest.fail(
            f"replay_model `{replay_model}` references unknown scenario "
            f"`{scenario_name}` (run `stravia-tools print-scenarios` to inspect)"
        )

    stream = bool(meta["stream"])
    anchor: str = meta["anchor"]
    expected = meta["expected_fields"].get(ingress_protocol, [])

    proxy_base = str(stravia_proxy_base["base"])
    api_key = str(stravia_proxy_base["api_key"])
    url = _build_request_url(proxy_base, ingress_protocol, replay_model, stream)
    body = _build_request_body(ingress_protocol, replay_model, stream)

    request_started_at = int(time.time() * 1000)
    status, raw = http_request(
        "POST",
        url,
        body,
        headers=_request_headers(ingress_protocol, api_key),
        timeout=20.0,
    )
    text = raw if isinstance(raw, str) else json.dumps(raw)

    if status == 422:
        assert "STRAVIA_PROTOCOL_LOSSY_REJECTED" in text, (
            f"{ingress_protocol} <- {replay_model}: unexpected 422 body={text[:512]}"
        )
        return

    assert status == 200, (
        f"{ingress_protocol} <- {replay_model}: HTTP {status}, body={text[:512]}"
    )

    # Tool-use scenarios: the model is supposed to emit a tool_call, NOT echo
    # the anchor token. We trust ``expected_fields`` (e.g. ``functionCall``,
    # ``tool_use``) to assert the structured tool-call survived conversion.
    if scenario_name != "tool-use-stream":
        visible = _extract_visible_text(text)
        assert anchor in visible, (
            f"{ingress_protocol} <- {replay_model}: anchor `{anchor}` missing "
            f"from converted response (first 512 bytes: {text[:512]})"
        )

    for field in expected:
        assert field in text, (
            f"{ingress_protocol} <- {replay_model}: expected field "
            f"`{field}` missing from converted response (first 512 bytes: {text[:512]})"
        )

    route_ids = stravia_proxy_base["route_ids"]
    assert isinstance(route_ids, dict)
    route_id = str(route_ids[replay_model])
    admin_headers = stravia_proxy_base["admin_headers"]
    assert isinstance(admin_headers, dict)
    deadline = time.time() + 10.0
    observed: dict | None = None
    while time.time() < deadline:
        query_status, forest = http_request(
            "GET",
            f"{proxy_base}/api/v1/observations/interactions?model={route_id}&limit=100",
            headers=admin_headers,
        )
        assert query_status == 200, forest
        interactions = [
            interaction
            for root in forest["data"]["roots"]
            for interaction in root["interactions"]
            if interaction["first_route_id"] == route_id
            and interaction["started_at"] >= request_started_at
        ]
        if interactions:
            latest = max(interactions, key=lambda item: item["last_event_sequence"])
            detail_status, detail = http_request(
                "GET",
                f"{proxy_base}/api/v1/observations/interactions/{latest['id']}",
                headers=admin_headers,
            )
            assert detail_status == 200, detail
            candidate = detail["data"]
            if (
                candidate["runs"]
                and candidate["runs"][-1]["ingress_protocol"]
                == OBSERVED_INGRESS_PROTOCOL[ingress_protocol]
                and candidate["runs"][-1]["status"] != "running"
                and (candidate["runs"][-1].get("trace") or {}).get("status") == "complete"
            ):
                observed = candidate
                break
        time.sleep(0.1)
    assert observed is not None, f"Observation not persisted for {ingress_protocol} <- {replay_model}"
    run = observed["runs"][-1]
    assert run["ingress_protocol"] == OBSERVED_INGRESS_PROTOCOL[ingress_protocol]
    sequences = [event["sequence"] for event in run["events"]]
    assert sequences == sorted(sequences)
    kinds = [event["kind"] for event in run["events"]]
    assert kinds.index("target_attempt_started") < kinds.index("target_attempt_finished")
    assert kinds.index("target_attempt_finished") < kinds.index("run_finished")
    assert run["client_output_committed"] is True

    _, _, archive = download_observation_bundle(
        {"admin": proxy_base, "auth": admin_headers}, observed,
    )
    # 入站抓包发生在 Run 准入之前，事件自身尚无 run_id；归属由 ZIP 的 Run 目录确定。
    trace = observation_bundle_events(archive, run_id=run["id"])
    directions = {event.get("direction") for event in trace if isinstance(event, dict)}
    assert {
        "client_to_platform",
        "upstream_request",
        "upstream_response",
        "platform_to_client",
    } <= directions
    stages = {event.get("stage") for event in trace if isinstance(event, dict)}
    assert {
        "decoded_request",
        "restored_request",
        "effective_model_request",
        "canonical_request",
        "canonical_terminal_response",
        "response_after_hook",
        "client_projection_event",
        "delivery_terminal",
    } <= stages


# ---------------------------------------------------------------------------
# sanity: ensure the in-memory protocol map mirrors stravia-server's YAML schema
# ---------------------------------------------------------------------------


def test_protocol_map_complete() -> None:
    assert set(STRAVIA_BASE_URL_PATH.keys()) == set(INGRESS_PROTOCOLS)
