from __future__ import annotations

import copy
import hashlib
import io
import json
import time
import zipfile

import pytest

from tests.common.protocol_audit import (
    _check, _credential_proof, _credential_wire, _history_proof, _performance_request,
    extract_generation_id,
)


@pytest.mark.parametrize("corruption,reason", [
    ("provider", "provider_id_mismatch"),
    ("target", "target_id_mismatch"),
    ("turn", "attempt_model_turn_unlinked"),
    ("parent", "generation_parent_mismatch"),
    ("output", "persisted_output_digest_mismatch"),
    ("commit", "run_not_completed_and_committed"),
    ("delivery", "delivery_terminal_missing_or_failed"),
])
def test_completed_http_cannot_hide_broken_persistent_chain(corruption, reason):
    answer = "actual answer Bearer ghp_" + "A" * 36
    row = {"route_id": "route", "expected_parent": "parent", "expected_provider_id": "provider",
           "expected_target_id": "target", "expected_model": "model", "input_tokens": 50_123,
           "upstream_input_tokens": 50_000,
           "output_tokens": 301, "output_sha256": hashlib.sha256(answer.encode()).hexdigest()}
    run = {"id": "run", "generation_node_id": "generation", "generation_parent_id": "parent",
           "route_id": "route", "status": "completed", "client_output_committed": True,
           "delivery_completed_at": 123, "usage": {"input_tokens": 50_000, "output_tokens": 301,
               "coverage": {"missing_input_tokens": 0, "missing_output_tokens": 0}},
           "events": [
               {"sequence": 1, "kind": "model_turn_started", "payload": {"model_turn_id": "turn", "route_id": "route"}},
               {"sequence": 2, "kind": "target_attempt_started", "payload": {
                   "attempt_id": "attempt", "model_turn_id": "turn", "provider_id": "provider",
                   "target_id": "target", "upstream_model": "model"}},
               {"sequence": 3, "kind": "target_attempt_finished",
                "payload": {"attempt_id": "attempt", "status": "completed"}},
               {"sequence": 4, "kind": "model_turn_finished",
                "payload": {"model_turn_id": "turn", "status": "completed"}},
               {"sequence": 5, "kind": "client_visible_content",
                "payload": {"text": answer, "complete": True}},
               {"sequence": 6, "kind": "run_finished", "payload": {"delivery": {"status": "delivered"}}},
           ]}
    detail = {"interaction": {"id": "interaction", "observation_gap": False}}
    assert _check(row, detail, run)["failures"] == []
    broken = copy.deepcopy(run)
    if corruption in {"provider", "target", "turn"}:
        field = {"provider": "provider_id", "target": "target_id", "turn": "model_turn_id"}[corruption]
        broken["events"][1]["payload"][field] = "unrelated"
    elif corruption == "parent":
        broken["generation_parent_id"] = "unrelated"
    elif corruption == "output":
        broken["events"][4]["payload"]["text"] = "truncated"
    elif corruption == "commit":
        broken["client_output_committed"] = False
    else:
        broken["events"][5]["payload"]["delivery"]["status"] = "failed"
    assert reason in _check(row, detail, broken)["failures"]


def test_responses_generation_identity_from_native_frames() -> None:
    frames = b'data: {"type":"response.created","response":{"id":"generation-1"}}\n\ndata: {"type":"response.completed","response":{"id":"generation-1"}}\n\ndata: [DONE]\n\n'
    assert extract_generation_id("open-responses", frames) == "generation-1"


@pytest.mark.parametrize("protocol", ["openai-chat", "anthropic-messages", "google-content"])
def test_vendor_identity_is_not_generation_identity(protocol: str) -> None:
    assert extract_generation_id(protocol, {"id": "vendor-message", "responseId": "vendor-message"}) is None


def test_conflicting_generation_identities_fail() -> None:
    with pytest.raises(ValueError, match="conflicting Generation"):
        extract_generation_id("open-responses", [{"response": {"id": "one"}}, {"response": {"id": "two"}}])


def test_performance_matches_request_identity_not_unique_global_root() -> None:
    def span(name: str, identity: int, parent: int | None = None, request: str | None = None) -> dict:
        args = {"span_id": identity, "capture_state": "finished"}
        if parent is not None:
            args["parent_id"] = parent
        if request is not None:
            args["request_id"] = request
        return {"name": name, "ph": "X", "dur": 10, "args": args}

    snapshot = {"traceEvents": [
        span("proxy.inference_run", 1, request="other"),
        span("proxy.inference_run", 2, request="wanted"),
        span("router.select", 3, 2),
        span("vendor.execute", 4, 2),
        span("generation_chain.parent.materialize", 5, 2),
        span("sql.query", 6, 5),
    ]}
    result = _performance_request(snapshot, {"wanted"}, "parent-generation")
    assert result["root_span_id"] == 2
    assert result["failures"] == []
    assert result["sql_query_count"] == 1


def test_storage_proof_does_not_accept_missing_parent_principal() -> None:
    proof = {
        "generation_id": "child", "parent_id": "parent", "principal_sha256": "a" * 64,
        "output_sha256": "b" * 64, "payload_valid": True, "references_valid": True,
        "fingerprint_valid": True, "backend": "postgres", "read_verified": True,
    }
    result = _history_proof("child", "parent", {"child": proof})
    assert result["failures"] == ["history_parent_storage_or_principal_mismatch"]


def credential_fixture():
    expected = {"secrets": ("ghp_" + "A" * 36, "ghp_" + "B" * 36),
                "references": ("<!--sr:" + "a" * 28 + "-->", "<!--sr:" + "b" * 28 + "-->")}
    expected["new_references"] = expected["references"]
    row = {"nonce": "nonce", "upstream": "openai-chat", "expected_new_mappings": 2,
           "credential_protection_enabled": True}
    events = [
        {"direction": "client_to_platform", "headers": {"AUTHORIZATION": ["***", "***"]}},
        {"direction": "platform_to_upstream", "headers": {"Authorization": "***",
                                                         "x-extra": expected["secrets"][0]}}]
    bodies = {"client_to_platform": " ".join(expected["secrets"]).encode(),
              "platform_to_upstream": " ".join(expected["references"]).encode()}
    capture = {"credential_checks": {"nonce": _credential_wire(expected, events, bodies)}}
    run = {"events": [{"kind": "credential_mappings_created", "payload": {
        "discoveries": [{"rule_ids": ["github"], "source_types": ["user_message"]} for _ in range(2)]}}]}
    proof = {"read_verified": True, "backend": "postgres", "published": True, "unexpired": True,
             "mapping_count": 2, "new_mapping_count": 2, "principal_sha256": "a" * 64}
    return row, run, expected, capture, proof, "a" * 64


@pytest.mark.parametrize("identity", ["run", "header"])
@pytest.mark.parametrize("progress", ["already_delivered", "pending_then_delivered", "never_delivered"])
def test_audit_waits_for_delivery_after_completed_trace(monkeypatch, identity, progress):
    from tests.common import protocol_audit

    row, discoveries, expected, _, credential_proof, principal = credential_fixture()
    answer = "completed answer"
    digest = hashlib.sha256(answer.encode()).hexdigest()
    row.update(route_id="route", expected_parent=None, expected_provider_id="provider",
               expected_target_id="target", expected_model="model", input_tokens=10,
               output_tokens=3, output_sha256=digest)
    if identity == "run":
        row["run_id"] = "run"
    bodies = {"client_to_platform": " ".join(expected["secrets"]),
              "platform_to_upstream": " ".join(expected["references"]),
              "upstream_to_platform": answer, "platform_to_client": answer}
    row["wire_digests"] = {direction: hashlib.sha256(body.encode()).hexdigest()
                           for direction, body in bodies.items()}
    wire = [{"direction": direction, "message_type": "request_body", "payload": body,
             "headers": {"Authorization": "***", "x-stravia-benchmark-request": "nonce"}}
            for direction, body in bodies.items()]
    archive = io.BytesIO()
    with zipfile.ZipFile(archive, "w") as bundle:
        bundle.writestr("capture-run/events.jsonl", "\n".join(json.dumps(event) for event in wire))

    class Opener:
        def open(self, *args, **kwargs):
            return io.BytesIO(archive.getvalue())

    events = [
        {"kind": "model_turn_started", "payload": {"model_turn_id": "turn", "route_id": "route"}},
        {"kind": "target_attempt_started", "payload": {"attempt_id": "attempt", "model_turn_id": "turn",
            "provider_id": "provider", "target_id": "target", "upstream_model": "model"}},
        {"kind": "target_attempt_finished", "payload": {"attempt_id": "attempt", "status": "completed"}},
        {"kind": "model_turn_finished", "payload": {"model_turn_id": "turn", "status": "completed"}},
        discoveries["events"][0],
        {"kind": "client_visible_content", "payload": {"text": answer, "complete": True}},
        {"kind": "run_finished", "payload": {"delivery": {"status": "delivered"}}},
    ]
    for sequence, event in enumerate(events, 1):
        event["sequence"] = sequence
    terminal = {"id": "run", "route_id": "route", "status": "completed",
                "trace": {"status": "complete", "reasons": []},
                "generation_node_id": "generation", "generation_parent_id": None,
                "client_output_committed": True, "delivery_completed_at": 123,
                "usage": {"input_tokens": 10, "output_tokens": 3,
                          "coverage": {"missing_input_tokens": 0, "missing_output_tokens": 0}},
                "events": events}
    before_delivery = copy.deepcopy(terminal)
    before_delivery.update(generation_node_id=None, client_output_committed=False, events=events[:-1])

    class Session:
        def __init__(self):
            self.revision = 0

        def request(self, method, path, payload=None, *, timeout):
            if path.startswith("/api/v1/observations/interactions?"):
                self.revision = (2 if progress == "already_delivered" or
                                 progress == "pending_then_delivered" and self.revision else 1)
                return 200, {"roots": [{"interactions": [{"id": "interaction",
                    "last_event_sequence": 5 + self.revision, "debug_status": "complete"}]}]}
            if path.endswith("/debug-bundle-tickets"):
                return 200, {"download_url": "https://capture.test"}
            assert path == "/api/v1/observations/interactions/interaction"
            return 200, {"interaction": {"id": "interaction", "observation_gap": False},
                         "snapshot_sequence": 5 + self.revision,
                         "runs": [copy.deepcopy(before_delivery if self.revision == 1 else terminal)]}

    clock = [0.0]
    monkeypatch.setattr(protocol_audit.time, "monotonic", lambda: clock[0])
    monkeypatch.setattr(protocol_audit.time, "sleep", lambda seconds: clock.__setitem__(0, clock[0] + seconds))
    monkeypatch.setattr(protocol_audit, "build_opener", lambda *args: Opener())
    metadata = {"storage_status": "ready", "debug_enabled": True,
                **{name: 0 for name in ("dropped_active", "dropped_completed", "dropped_sql_completed",
                    "dropped_resource_samples", "frozen_spans", "frozen_total", "storage_errors")}}
    spans = [{"name": name, "ph": "X", "dur": 10,
              "args": {"span_id": index, "capture_state": "finished",
                       **({"request_id": "run"} if index == 1 else {"parent_id": 1})}}
             for index, name in enumerate(("proxy.inference_run", "router.select", "vendor.execute",
                                            "generation_chain.history.persist", "sql.query"), 1)]
    history = {"generation": {"generation_id": "generation", "parent_id": None,
        "principal_sha256": principal, "output_sha256": digest, "backend": "postgres",
        "read_verified": True, "payload_valid": True, "references_valid": True, "fingerprint_valid": True}}
    result = protocol_audit.audit_requests(Session(), [row], None, require_debug=True,
        performance_snapshot={"metadata": metadata, "traceEvents": spans}, history_evidence=history,
        credential_expectations={"nonce": expected}, credential_evidence={"nonce": credential_proof})
    if progress == "never_delivered":
        assert not result["full_chain_verified"]
        assert not result["requests"][0].get("verified")
        assert result["requests"][0].get("generation_id") is None
        return
    assert result["full_chain_verified"], result["requests"][0]["failures"]
    assert result["requests"][0]["generation_id"] == "generation"
    assert result["requests"][0]["delivery_status"] == "delivered"


def test_credential_evidence_accepts_real_preservation_boundary():
    args = credential_fixture()
    assert _credential_proof(*args)["failures"] == []
    # ADR0063 does not mask arbitrary headers, body text, or URL parameters.
    expected = args[2]
    checks = _credential_wire(expected, [
        {"direction": "client_to_platform", "headers": {"Authorization": "***"}},
        {"direction": "platform_to_upstream", "headers": {"x-api-key": "benchmark-only"},
         "url": "https://example.test/?key=benchmark-only"}],
        {"client_to_platform": " ".join(expected["secrets"]).encode(),
         "platform_to_upstream": " ".join(expected["references"]).encode()})
    assert checks["native_credentials_preserved"]
    assert checks["client_body_contains_credentials"]
    assert checks["upstream_body_protected"]


def test_credential_evidence_accepts_placeholder_markers_from_debug_runs():
    args = credential_fixture()
    args[1]["debug_enabled"] = True
    for discovery, reference in zip(args[1]["events"][0]["payload"]["discoveries"], args[2]["references"], strict=True):
        discovery["marker"] = reference
    assert _credential_proof(*args)["failures"] == []


@pytest.mark.parametrize("mutation", [
    "secret_discovery_leak", "reference_discovery_leak", "disabled", "missing_expectations", "missing_wire", "partial_mask",
    "discovery_absent", "discovery_count", "discovery_source", "discovery_rule",
    "marker_without_debug", "marker_unknown",
    "missing_storage", "partial_storage", "principal",
])
def test_credential_proof_fails_closed_without_exposing_sensitive_values(mutation):
    args = list(credential_fixture())
    if mutation == "secret_discovery_leak":
        args[1]["events"][0]["payload"]["discoveries"][0]["secret"] = args[2]["secrets"][0]
    elif mutation == "reference_discovery_leak":
        args[1]["events"][0]["payload"]["discoveries"][0]["reference"] = args[2]["references"][0]
    elif mutation == "disabled":
        args[0]["credential_protection_enabled"] = False
    elif mutation == "missing_expectations":
        args[2] = {}
    elif mutation == "missing_wire":
        args[3] = {}
    elif mutation == "partial_mask":
        args[3]["credential_checks"]["nonce"]["client_authorization_masked"] = False
    elif mutation == "marker_without_debug":
        for discovery, reference in zip(args[1]["events"][0]["payload"]["discoveries"], args[2]["references"], strict=True):
            discovery["marker"] = reference
    elif mutation == "marker_unknown":
        args[1]["debug_enabled"] = True
        args[1]["events"][0]["payload"]["discoveries"][0]["marker"] = "<!--sr:" + "c" * 28 + "-->"
    elif mutation.startswith("discovery"):
        discoveries = args[1]["events"][0]["payload"]["discoveries"]
        if mutation == "discovery_absent":
            args[1]["events"] = []
        elif mutation == "discovery_count":
            discoveries.pop()
        else:
            discoveries[0]["source_types" if mutation == "discovery_source" else "rule_ids"] = []
    elif mutation == "missing_storage":
        args[4] = {}
    elif mutation == "partial_storage":
        args[4]["published"] = False
    else:
        args[4]["principal_sha256"] = "b" * 64
    result = _credential_proof(*args)
    assert result["failures"]
    assert not result["verified"]
    for value in (*credential_fixture()[2]["secrets"], *credential_fixture()[2]["references"]):
        assert value not in str(result)


@pytest.mark.parametrize("headers", [{}, {"authorization": "***suffix"},
                                      {"Authorization": ["***", "Bearer raw"]}])
def test_wire_authorization_requires_every_actual_value_masked(headers):
    expected = credential_fixture()[2]
    result = _credential_wire(expected, [{"direction": "client_to_platform", "headers": headers}],
                              {"client_to_platform": b"", "platform_to_upstream": b""})
    assert not result["client_authorization_masked"]
    assert not result["client_body_contains_credentials"]
    assert not result["upstream_body_protected"]


@pytest.mark.parametrize("protocol", ["anthropic-messages", "google-content"])
def test_native_wire_credential_must_be_present_not_only_vacuously_preserved(protocol):
    args = list(credential_fixture())
    args[0]["upstream"] = protocol
    result = _credential_proof(*args)
    assert "credential_wire_native_credentials_missing" in result["failures"]
    assert not result["verified"]


@pytest.mark.parametrize("upstream_leak", [False, True])
def test_actual_downloaded_bundle_credential_boundaries(monkeypatch, upstream_leak):
    from tests.common import protocol_audit

    expected = credential_fixture()[2]
    secret = expected["secrets"][0]
    events = [
        {"direction": "client_to_platform", "message_type": "request",
         "headers": {"Authorization": ["***"], "x-extra": secret,
                     "x-stravia-benchmark-request": "nonce"},
         "payload": " ".join(expected["secrets"])},
        {"direction": "upstream_request", "message_type": "request",
         "headers": {"authorization": "***"},
         "payload": secret if upstream_leak else " ".join(expected["references"])},
        {"kind": "client_visible_content",
         "payload": {"text": secret}},
    ]
    archive = io.BytesIO()
    with zipfile.ZipFile(archive, "w") as bundle:
        bundle.writestr("capture-run/events.jsonl", "\n".join(json.dumps(event) for event in events))
        bundle.writestr("manifest.json", json.dumps({"preview": secret}))

    class Opener:
        def open(self, *args, **kwargs):
            return io.BytesIO(archive.getvalue())

    monkeypatch.setattr(protocol_audit, "_api", lambda *args, **kwargs: {"download_url": "https://capture.test"})
    monkeypatch.setattr(protocol_audit, "build_opener", lambda *args: Opener())
    result = protocol_audit._wire(None, {"interaction": {"id": "interaction"},
                                       "snapshot_sequence": 1, "runs": [{"id": "run"}]},
                                  time.monotonic() + 30, {"nonce": expected})
    checks = result["run"]["credential_checks"]["nonce"]
    assert checks["upstream_body_protected"] is not upstream_leak
    assert result["run"]["digests"]["client_to_platform"] == hashlib.sha256(
        " ".join(expected["secrets"]).encode()).hexdigest()
    assert checks["client_authorization_masked"]
    assert checks["upstream_authorization_masked"]
    assert secret not in str(result)
    assert expected["references"][0] not in str(result)
