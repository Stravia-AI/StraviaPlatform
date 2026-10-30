"""Real management-API evidence, never prompt/nonce based attribution.

Required request fields: generation_id (Responses only), or run_id obtained from
an independently verified transport/storage identity; expected_parent, route_id,
expected_provider_id, expected_target_id, expected_model, output_sha256,
input_tokens, output_tokens, upstream and enabled credential-protection evidence.
nonce/session_id are safe correlation labels, never Generation identities.
Usage is checked against protected upstream_input_tokens (NET input, cache excluded),
not the client token count before protection. Secret expectations stay in memory;
returned proofs contain only safe identities, hashes, counts and failure codes.
No database-specific fallback or PostgreSQL skip is used.
"""
from __future__ import annotations

import base64
import hashlib
import io
import json
import time
import zipfile
from collections import Counter
from typing import Any
from urllib.parse import parse_qs, urlencode, urlsplit
from urllib.request import ProxyHandler, build_opener

_DIRECTIONS = {"client_to_platform", "platform_to_upstream", "upstream_to_platform", "platform_to_client"}
_SAFE = ("generation_id", "expected_parent", "expected_parent_nonce", "route_id", "expected_provider_id", "expected_target_id", "expected_model", "output_sha256", "input_tokens", "upstream_input_tokens", "output_tokens", "client_output_tokens", "credential_protection_enabled", "expected_new_mappings", "upstream", "nonce", "session_id", "run_id")


class AuditAPIError(RuntimeError):
    pass


def extract_generation_id(protocol: str, response: Any) -> str | None:
    """Extract ONLY a product-owned Generation identity, not a vendor message ID.

    response accepts a decoded response, decoded native SSE frames, or raw SSE.
    Chat/Anthropic/Gemini IDs are upstream IDs and intentionally return None.
    Open Responses supports explicit previous_response_id; other protocols must
    replay their actual assistant history and use verified Run association.
    Thinking <!--sh:...--> references are NOT Generation identities.
    """
    if protocol != "open-responses":
        return None
    if isinstance(response, bytes):
        response = response.decode("utf-8")
    if isinstance(response, str):
        try:
            response = json.loads(response)
        except json.JSONDecodeError:
            response = [json.loads(line[5:].strip()) for line in response.splitlines()
                        if line.startswith("data:") and line[5:].strip() != "[DONE]"]
    frames = response if isinstance(response, list) else [response]
    ids = set()
    for frame in frames:
        if not isinstance(frame, dict):
            continue
        value = frame.get("response", frame)
        if isinstance(value, dict) and isinstance(value.get("id"), str):
            ids.add(value["id"])
    if len(ids) > 1:
        raise ValueError("conflicting Generation response identities")
    return next(iter(ids), None)


def _api(session: Any, method: str, path: str, payload: Any = None, *, deadline: float) -> Any:
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise AuditAPIError("audit_deadline_exceeded")
    status, body = session.request(method, path, payload, timeout=min(30, remaining))
    if status != 200:
        # Do not serialize server errors: these can contain request contents.
        raise AuditAPIError(f"management_http_{status}")
    return body.get("data", body) if isinstance(body, dict) else body


def _details(session: Any, route_ids: set[str], deadline: float, cache: dict, start_at: int = 0) -> dict[str, dict]:
    """One forest scan per route, one detail per changed Interaction, not per request."""
    result = {}
    for route_id in sorted(route_ids):
        query = {"model": route_id, "start_at": start_at, "end_at": int(time.time() * 1000) + 1000, "limit": 100}
        cursors = set()
        while True:
            page = _api(session, "GET", "/api/v1/observations/interactions?" + urlencode(query), deadline=deadline)
            for root in page["roots"]:
                for interaction in root["interactions"]:
                    key = interaction["id"]
                    revision = (interaction.get("last_event_sequence"), interaction.get("debug_status"))
                    old = cache.get(key)
                    if old is None or old[0] != revision:
                        detail = _api(session, "GET", f"/api/v1/observations/interactions/{key}", deadline=deadline)
                        cursor = detail.get("older_events_cursor")
                        by_run = {run["id"]: run for run in detail["runs"]}
                        seen = set()
                        while cursor is not None:
                            if cursor in seen:
                                raise AuditAPIError("event_pagination_cycle")
                            seen.add(cursor)
                            bounds = urlencode({"before_sequence": cursor, "through_sequence": detail["snapshot_sequence"], "limit": 200})
                            older = _api(session, "GET", f"/api/v1/observations/interactions/{key}/events?{bounds}", deadline=deadline)
                            for run in older["runs"]:
                                if run["id"] not in by_run:
                                    by_run[run["id"]] = run
                                else:
                                    by_run[run["id"]]["events"] = run["events"] + by_run[run["id"]]["events"]
                            cursor = older.get("next_cursor")
                        detail["runs"] = list(by_run.values())
                        cache[key] = revision, detail
                    result[key] = cache[key][1]
            cursor = page.get("next_cursor")
            if cursor is None:
                break
            if cursor in cursors:
                raise AuditAPIError("forest_pagination_cycle")
            cursors.add(cursor)
            query["cursor"] = cursor
    return result


def _wire(session: Any, detail: dict, deadline: float, credential_expectations: dict | None = None) -> dict:
    ticket = _api(session, "POST", f"/api/v1/observations/interactions/{detail['interaction']['id']}/debug-bundle-tickets",
                  {"through_sequence": detail["snapshot_sequence"]}, deadline=deadline)
    url = ticket["download_url"]
    if url.startswith("/"):
        url = session.origin + url
    # Tickets are one-use bearer URLs. Never include URL in returned evidence.
    with build_opener(ProxyHandler()).open(url, timeout=max(.001, min(30, deadline - time.monotonic()))) as response:
        archive = response.read()
    result = {}
    with zipfile.ZipFile(io.BytesIO(archive)) as bundle:
        for name in bundle.namelist():
            if not name.endswith("/events.jsonl"):
                continue
            run_id = next((run["id"] for run in detail["runs"]
                           if name.endswith(f"-{run['id']}/events.jsonl")), None)
            if run_id is None:
                continue
            counts = Counter()
            hashes = {direction: hashlib.sha256() for direction in _DIRECTIONS}
            markers = set()
            request_ids = set()
            wire_events = []
            bodies = {direction: bytearray() for direction in _DIRECTIONS}
            for line in bundle.read(name).splitlines():
                if line.strip():
                    event = json.loads(line)
                    event["direction"] = {"upstream_request": "platform_to_upstream",
                                          "upstream_response": "upstream_to_platform"}.get(
                                              event.get("direction"), event.get("direction"))
                    if event.get("headers") or event.get("url"):
                        wire_events.append({key: event.get(key) for key in ("direction", "headers", "url")})
                    if event.get("direction") == "client_to_platform":
                        headers = event.get("headers") or {}
                        marker = headers.get("x-stravia-benchmark-request")
                        if isinstance(marker, str):
                            markers.add(marker)
                        request_id = headers.get("x-request-id")
                        if isinstance(request_id, str):
                            request_ids.add(request_id)
                    if event.get("direction") in _DIRECTIONS and event.get("message_type") in {"body_chunk", "request", "request_body", "response_body"}:
                        direction = event["direction"]
                        counts[direction] += 1
                        payload = event.get("payload")
                        if isinstance(payload, str):
                            data = payload.encode("utf-8")
                        elif isinstance(payload, dict) and payload.get("encoding") == "base64":
                            data = base64.b64decode(payload["data"], validate=True)
                        else:
                            raise AuditAPIError("wire_body_not_losslessly_encoded")
                        hashes[direction].update(data)
                        if direction in {"client_to_platform", "platform_to_upstream"}:
                            bodies[direction].extend(data)
            result[run_id] = {"directions": counts, "markers": markers, "request_ids": request_ids,
                              "digests": {direction: value.hexdigest() for direction, value in hashes.items()},
                              "credential_checks": {
                                  nonce: _credential_wire(expected, wire_events, bodies)
                                  for nonce in markers
                                  if (expected := (credential_expectations or {}).get(nonce))}}
    return result


def _credential_wire(expected: dict, events: list[dict], bodies: dict) -> dict:
    """Summarize actual archive bytes without retaining secrets or references."""
    def authorization(direction: str) -> bool:
        values = [value for event in events if event.get("direction") == direction
                  for key, raw in (event.get("headers") or {}).items() if key.lower() == "authorization"
                  for value in (raw if isinstance(raw, list) else [raw])]
        return bool(values) and all(value == "***" for value in values)

    secrets = expected.get("secrets", ())
    native_headers = [
        value for event in events if event.get("direction") == "platform_to_upstream"
        for key, raw in (event.get("headers") or {}).items() if key.lower() == "x-api-key"
        for value in (raw if isinstance(raw, list) else [raw])]
    native_query = [
        value for event in events if event.get("direction") == "platform_to_upstream"
        for value in parse_qs(urlsplit(event.get("url") or "").query).get("key", [])]
    return {
        "client_authorization_masked": authorization("client_to_platform"),
        "upstream_authorization_masked": authorization("platform_to_upstream"),
        "native_credentials_preserved": all(value == "benchmark-only" for value in native_headers + native_query),
        "native_header_count": len(native_headers),
        "native_query_count": len(native_query),
        "client_body_contains_credentials": bool(secrets) and all(
            secret.encode() in bodies["client_to_platform"] for secret in secrets),
        "upstream_body_protected": bool(bodies["platform_to_upstream"]) and all(
            secret.encode() not in bodies["platform_to_upstream"] for secret in secrets),
    }


def _credential_proof(row: dict, run: dict, expected: dict,
                      capture: dict, proof: dict, principal: str | None) -> dict:
    failures = []
    secrets, references = expected.get("secrets", ()), expected.get("references", ())
    new = expected.get("new_references", ())
    if (row.get("credential_protection_enabled") is not True
            or not secrets or len(secrets) != len(references) or len(set(references)) != len(references)
            or len(new) != 2 or row.get("expected_new_mappings") != 2
            or not set(new).issubset(references)):
        failures.append("credential_expectations_missing_or_invalid")
    def hits(kind: str) -> list[dict]:
        return [discovery for event in run.get("events", []) if event.get("kind") == kind
                for discovery in event.get("payload", {}).get("discoveries", [])]

    discoveries = hits("credential_mappings_created")
    # 复用事件在每次重放历史时重复出现，敏感内容与占位符的约束与新增事件相同。
    recorded = discoveries + hits("credential_mappings_reused")
    if len(discoveries) != 2 or any(
            not isinstance(discovery.get("rule_ids"), list) or not discovery["rule_ids"]
            or "user_message" not in discovery.get("source_types", []) for discovery in discoveries):
        failures.append("credential_discovery_missing_or_mismatched")
    # 占位符对所有 Run 记录；除 `marker` 外不得出现任何占位符，且 marker 必须是本次请求的占位符。
    markers = [discovery["marker"] for discovery in recorded if "marker" in discovery]
    unmarked = [{key: value for key, value in discovery.items() if key != "marker"} for discovery in recorded]
    if (any(item in json.dumps(recorded, ensure_ascii=False) for item in secrets)
            or any(item in json.dumps(unmarked, ensure_ascii=False) for item in references)
            or not set(markers) <= set(references)):
        failures.append("credential_discovery_sensitive_payload")
    checks = capture.get("credential_checks", {}).get(row.get("nonce"), {})
    for field in ("client_authorization_masked",
                  "client_body_contains_credentials", "upstream_body_protected",
                  "native_credentials_preserved"):
        if checks.get(field) is not True:
            failures.append("credential_wire_" + field + "_unverified")
    upstream = row.get("upstream")
    if upstream in {"openai-chat", "open-responses"} and checks.get("upstream_authorization_masked") is not True:
        failures.append("credential_wire_upstream_authorization_unverified")
    if (upstream == "anthropic-messages" and not checks.get("native_header_count")
            or upstream == "google-content" and not checks.get("native_query_count")
            or upstream not in {"openai-chat", "open-responses", "anthropic-messages", "google-content"}):
        failures.append("credential_wire_native_credentials_missing")
    if (proof.get("read_verified") is not True or proof.get("backend") not in {"sqlite", "postgres"}
            or proof.get("published") is not True or proof.get("unexpired") is not True
            or proof.get("failures") or proof.get("mapping_count") != len(references)
            or proof.get("new_mapping_count") != 2):
        failures.append("credential_storage_missing_or_invalid")
    if not principal or proof.get("principal_sha256") != principal:
        failures.append("credential_storage_principal_mismatch")
    return {"failures": failures, "mapping_count": len(references),
            "discovery_count": len(discoveries), "verified": not failures}


def _check(row: dict, detail: dict, run: dict) -> dict:
    evidence = {key: row[key] for key in _SAFE if key in row}
    failures = []
    evidence.update(interaction_id=detail["interaction"]["id"], run_id=run["id"],
                    generation_id=run.get("generation_node_id"), generation_parent_id=run.get("generation_parent_id"),
                    status=run["status"], client_output_committed=run.get("client_output_committed"),
                    delivery_completed_at=run.get("delivery_completed_at"))
    if run.get("generation_node_id") is None:
        failures.append("missing_generation_node")
    for actual, expected, name in ((run.get("generation_parent_id"), row["expected_parent"], "generation_parent"),
                                   (run.get("route_id"), row["route_id"], "route")):
        if actual != expected:
            failures.append(name + "_mismatch")
    if run["status"] != "completed" or run.get("client_output_committed") is not True:
        failures.append("run_not_completed_and_committed")
    if run.get("delivery_completed_at") is None:
        failures.append("delivery_not_completed")
    events = sorted(run.get("events", []), key=lambda event: event["sequence"])
    if detail["interaction"].get("observation_gap") or any(event["kind"] == "observation_gap" for event in events):
        failures.append("observation_gap")
    sequences = [event["sequence"] for event in events]
    if len(sequences) != len(set(sequences)):
        failures.append("duplicate_event_sequence")
    turns = {}
    turn_terminals = {}
    attempts = {}
    terminals = {}
    contents = []
    delivery = None
    for event in events:
        payload = event["payload"]
        kind = event["kind"]
        if kind == "model_turn_started":
            turns[payload["model_turn_id"]] = payload
        elif kind == "model_turn_finished":
            turn_terminals[payload["model_turn_id"]] = payload
        elif kind == "target_attempt_started":
            attempts[payload["attempt_id"]] = payload
        elif kind == "target_attempt_finished":
            terminals[payload["attempt_id"]] = payload
        elif kind == "client_visible_content":
            contents.append(payload)
        elif kind == "run_finished":
            delivery = payload.get("delivery")
    evidence["delivery_status"] = (delivery or {}).get("status")
    if evidence["delivery_status"] != "delivered":
        failures.append("delivery_terminal_missing_or_failed")
    evidence["model_turn_ids"] = sorted(turns)
    if any(turn_terminals.get(identity, {}).get("status") != "completed" for identity in turns):
        failures.append("model_turn_terminal_missing_or_failed")
    if any(turn.get("route_id") != row["route_id"] for turn in turns.values()):
        failures.append("model_turn_route_mismatch")
    evidence["attempts"] = []
    if len(attempts) != 1 or not turns:
        failures.append("expected_single_target_attempt_and_model_turn")
    for attempt_id, attempt in attempts.items():
        terminal = terminals.get(attempt_id, {})
        evidence["attempts"].append({key: attempt.get(key) for key in ("attempt_id", "model_turn_id", "target_id", "provider_id", "upstream_model")}
                                   | {"status": terminal.get("status")})
        if attempt.get("model_turn_id") not in turns:
            failures.append("attempt_model_turn_unlinked")
        for field, expected in (("provider_id", "expected_provider_id"), ("target_id", "expected_target_id"), ("upstream_model", "expected_model")):
            if attempt.get(field) != row[expected]:
                failures.append(field + "_mismatch")
        if terminal.get("status") != "completed":
            failures.append("attempt_terminal_missing_or_failed")
    usage = run.get("usage", {})
    evidence["observed_usage"] = {key: usage.get(key) for key in ("input_tokens", "output_tokens", "coverage")}
    for field in ("input_tokens", "output_tokens"):
        if usage.get(field) != (row.get("upstream_input_tokens", row[field]) if field == "input_tokens" else row[field]):
            failures.append(field + "_mismatch")
        if (usage.get("coverage") or {}).get("missing_" + field, 1) != 0:
            failures.append(field + "_coverage_missing")
    digest = hashlib.sha256()
    for content in contents:
        digest.update(content.get("text", "").encode("utf-8"))
    evidence["persisted_output_sha256"] = digest.hexdigest()
    evidence["persisted_output_items"] = len(contents)
    if not contents or any(content.get("complete") is not True for content in contents):
        failures.append("persisted_visible_output_incomplete")
    if digest.hexdigest() != row["output_sha256"]:
        failures.append("persisted_output_digest_mismatch")
    evidence["failures"] = failures
    return evidence


def _performance_request(snapshot: dict, request_ids: set[str], expected_parent: str | None) -> dict:
    events = [event for event in snapshot.get("traceEvents", []) if event.get("ph") in {"X", "B"}]
    roots = [event for event in events if event.get("name") == "proxy.inference_run"
             and event.get("args", {}).get("request_id") in request_ids]
    result = {"failures": [], "stage_counts": {}, "sql_query_count": 0}
    if len(roots) != 1:
        result["failures"].append("performance_request_root_missing_or_ambiguous")
        return result
    by_id = {}
    duplicate = False
    for event in events:
        span_id = event.get("args", {}).get("span_id")
        if type(span_id) is int:
            duplicate |= span_id in by_id
            by_id[span_id] = event
    if duplicate:
        result["failures"].append("performance_duplicate_span_identity")
    root_id = roots[0].get("args", {}).get("span_id")
    result["root_span_id"] = root_id
    descendants = []
    for event in events:
        current = event
        seen = set()
        while current is not None:
            span_id = current.get("args", {}).get("span_id")
            if span_id in seen:
                result["failures"].append("performance_causal_cycle")
                break
            seen.add(span_id)
            if span_id == root_id:
                descendants.append(event)
                break
            current = by_id.get(current.get("args", {}).get("parent_id"))
    counts = Counter(event.get("name") for event in descendants)
    result["stage_counts"] = dict(counts)
    if any(event.get("ph") != "X" or event.get("args", {}).get("capture_state") != "finished"
           or not isinstance(event.get("dur"), (int, float)) for event in descendants):
        result["failures"].append("performance_request_subtree_incomplete")
    for label, prefixes in (("routing", ("router.",)), ("provider", ("vendor.execute",)),
                            ("history", ("generation_chain.", "turn_chain."))):
        if not any(isinstance(name, str) and name.startswith(prefixes) for name in counts):
            result["failures"].append("performance_request_" + label + "_stage_missing")
    if expected_parent and not any(isinstance(name, str) and name.startswith(
            ("generation_chain.parent.", "generation_chain.ancestor.", "turn_chain.")) for name in counts):
        result["failures"].append("performance_parent_read_stage_missing")
    sql = [event for event in descendants if event.get("name") == "sql.query"]
    result["sql_query_count"] = len(sql)
    result["sql_duration_us"] = sum(event.get("dur", 0) for event in sql)
    if not sql:
        result["failures"].append("performance_request_sql_missing")
    result["failures"] = sorted(set(result["failures"]))
    return result


def _history_proof(generation_id: str, expected_parent: str | None, proofs: dict) -> dict:
    """Consume read-only storage proofs; raw payload/principal must stay upstream.

    proofs[generation_id] requires generation_id,parent_id,principal_sha256,
    output_sha256,payload_valid,references_valid,fingerprint_valid,backend,
    read_verified. The parent must also have a proof under the same principal.
    """
    proof = proofs.get(generation_id) or {}
    failures = list(proof.get("failures") or [])
    if proof.get("read_verified") is not True or proof.get("backend") not in {"sqlite", "postgres"}:
        failures.append("history_storage_read_unverified")
    if proof.get("generation_id") != generation_id or proof.get("parent_id") != expected_parent:
        failures.append("history_storage_identity_or_parent_mismatch")
    for field in ("payload_valid", "references_valid", "fingerprint_valid"):
        if proof.get(field) is not True:
            failures.append("history_storage_" + field + "_missing")
    principal = proof.get("principal_sha256")
    if not isinstance(principal, str) or len(principal) != 64:
        failures.append("history_storage_principal_missing")
    if expected_parent:
        parent = proofs.get(expected_parent) or {}
        if parent.get("read_verified") is not True or parent.get("principal_sha256") != principal:
            failures.append("history_parent_storage_or_principal_mismatch")
    return {"failures": failures, "backend": proof.get("backend"), "output_sha256": proof.get("output_sha256"),
            "principal_sha256": principal}


def _performance_index(snapshot: dict) -> dict:
    """Partition the causal forest once rather than scanning all roots per row."""
    events = [event for event in snapshot.get("traceEvents", []) if event.get("ph") in {"X", "B"}]
    by_id = {event.get("args", {}).get("span_id"): event for event in events}
    groups = {}
    for event in events:
        cursor = event
        seen = set()
        while cursor:
            args = cursor.get("args", {})
            identity = args.get("span_id")
            if identity in seen:
                break
            seen.add(identity)
            if cursor.get("name") == "proxy.inference_run":
                groups.setdefault(args.get("request_id"), []).append(event)
                break
            cursor = by_id.get(args.get("parent_id"))
    return {identity: {"traceEvents": spans} for identity, spans in groups.items()}


def _audit_ready(run: dict, require_debug: bool) -> bool:
    if run["status"] == "running":
        return False
    if require_debug and (run.get("trace") or {}).get("status") not in {"complete", "partial"}:
        return False
    # Delivery receipts and complete traces can precede final run settlement and Generation publication.
    # A failed delivery is also terminal; _check must reject it rather than waiting for success.
    return any(event["kind"] == "run_finished"
               and (event["payload"].get("delivery") or {}).get("status") is not None
               for event in run.get("events", []))


def audit_requests(session: Any, requests: list[dict], routes: Any, timeout: float = 30,
                   *, require_debug: bool = False, performance_snapshot: Any = None,
                   history_evidence: Any = None, credential_expectations: dict | None = None,
                   credential_evidence: Any = None) -> dict:
    """Return safe evidence and explicit failures, including incomplete coverage.

    routes is accepted as fixture context; expected selection is mandatory per row.
    Polling is bounded by a monotonic deadline and observed revision changes. A
    missing/partial trace is never accepted as success. No benchmark-time calls.
    """
    del routes
    deadline = time.monotonic() + timeout
    required = {"expected_parent", "route_id", "expected_provider_id", "expected_target_id", "expected_model", "output_sha256", "input_tokens", "output_tokens", "credential_protection_enabled", "expected_new_mappings", "upstream"}
    for row in requests:
        missing = required - row.keys()
        if missing:
            raise ValueError("audit request missing fields: " + ",".join(sorted(missing)))
    cache: dict = {}
    details = {}
    api_failure = None
    start_at = max(0, int(min((row.get("started_at", 0) for row in requests), default=0) * 1000) - 1000)
    while time.monotonic() < deadline:
        try:
            details = _details(session, {row["route_id"] for row in requests}, deadline, cache, start_at)
            runs = [(detail, run) for detail in details.values() for run in detail["runs"]]
            ready = True
            for row in requests:
                matches = [(detail, run) for detail, run in runs if
                           (row.get("generation_id") and run.get("generation_node_id") == row["generation_id"])
                           or (row.get("run_id") and run["id"] == row["run_id"])]
                if not row.get("generation_id") and not row.get("run_id") and require_debug:
                    # Header lookup happens in the bundle, once delivery and trace
                    # terminals are present. Do not spend the download deadline
                    # waiting for a vendor response ID to become a Generation ID.
                    candidates = [run for _, run in runs if run.get("route_id") == row["route_id"]]
                    if not candidates or any(not _audit_ready(run, require_debug) for run in candidates):
                        ready = False
                    continue
                if len(matches) != 1 or not _audit_ready(matches[0][1], require_debug):
                    ready = False
            if ready:
                break
        except AuditAPIError as error:
            api_failure = str(error)
            break
        remaining = deadline - time.monotonic()
        if remaining > 0:
            time.sleep(min(.1, remaining))
    runs = [(detail, run) for detail in details.values() for run in detail["runs"]]
    wire_cache = {}
    # Correlate an explicit, captured HTTP header, never assistant/prompt text.
    # Download each Interaction once, even with many Runs in a continuation.
    marker_runs = {}
    if require_debug:
        for key, detail in details.items():
            try:
                wire_cache[key] = _wire(session, detail, deadline, credential_expectations)
            except Exception:
                wire_cache[key] = {}
            for run_id, capture in wire_cache[key].items():
                for marker in capture["markers"]:
                    marker_runs.setdefault(marker, []).append(run_id)
    resolved = []
    for original in requests:
        row = dict(original)
        if not row.get("generation_id") and not row.get("run_id"):
            candidates = marker_runs.get(row.get("nonce"), [])
            if len(candidates) == 1:
                row["run_id"] = candidates[0]
        resolved.append(row)
    by_nonce = {}
    by_run_id = {}
    by_generation_id = {}
    for detail, run in runs:
        by_run_id.setdefault(run["id"], []).append((detail, run))
        if run.get("generation_node_id"):
            by_generation_id.setdefault(run["generation_node_id"], []).append((detail, run))
    for row in resolved:
        found_pairs = by_run_id.get(row.get("run_id"), []) or by_generation_id.get(row.get("generation_id"), [])
        found = [run for _, run in found_pairs]
        if len(found) == 1:
            by_nonce[row.get("nonce")] = found[0].get("generation_node_id")
    for row in resolved:
        if row.get("expected_parent_nonce"):
            row["expected_parent"] = by_nonce.get(row["expected_parent_nonce"])
    if callable(history_evidence):
        generation_ids = {run.get("generation_node_id") for row in resolved
                          for _, run in (by_run_id.get(row.get("run_id"), []) or
                                         by_generation_id.get(row.get("generation_id"), []))}
        generation_ids.update(row.get("expected_parent") for row in resolved)
        generation_ids.discard(None)
        # The callback reads real storage once after identity resolution; it
        # must return only safe proof fields, never payloads or credentials.
        try:
            history_evidence = history_evidence(sorted(generation_ids))
        except Exception:
            history_evidence = {identity: {"failures": ["history_storage_reader_failed"]}
                                for identity in generation_ids}
    if callable(performance_snapshot):
        try:
            performance_snapshot = performance_snapshot()
        except Exception:
            performance_snapshot = {}
    performance_by_request = _performance_index(performance_snapshot or {})
    credential_rows = {row.get("nonce"): (credential_expectations or {}).get(row.get("nonce"), {})
                       for row in resolved}
    if callable(credential_evidence):
        try:
            credential_evidence = credential_evidence(credential_rows)
        except Exception:
            credential_evidence = {}
    evidence = []
    for row in resolved:
        matches_by_id = {run["id"]: (detail, run) for detail, run in
                         by_run_id.get(row.get("run_id"), []) + by_generation_id.get(row.get("generation_id"), [])}
        matches = list(matches_by_id.values())
        if len(matches) != 1:
            evidence.append({key: row[key] for key in _SAFE if key in row} | {
                "failures": [api_failure or ("identity_not_available" if not row.get("generation_id") and not row.get("run_id") else "generation_run_missing_or_ambiguous")],
                "matched_runs": len(matches)})
            continue
        detail, run = matches[0]
        item = _check(row, detail, run)
        if row.get("expected_parent_nonce") and row.get("expected_parent") is None:
            item["failures"].append("expected_parent_nonce_unresolved")
        if row.get("expected_parent"):
            parents = by_generation_id.get(row["expected_parent"], [])
            item["history_parent_run_ids"] = [r["id"] for _, r in parents]
            if len(parents) != 1:
                item["failures"].append("history_parent_observation_missing_or_ambiguous")
        if require_debug:
            trace = run.get("trace") or {}
            item["trace_status"] = trace.get("status")
            if trace.get("status") != "complete" or trace.get("reasons"):
                item["failures"].append("debug_trace_incomplete")
            key = detail["interaction"]["id"]
            if key not in wire_cache:
                try:
                    wire_cache[key] = _wire(session, detail, deadline, credential_expectations)
                except Exception:
                    wire_cache[key] = {}
            directions = wire_cache[key].get(run["id"], {}).get("directions", Counter())
            item["wire_body_records"] = dict(directions)
            if set(directions) != _DIRECTIONS:
                item["failures"].append("four_direction_wire_capture_missing")
            item["wire_digests"] = wire_cache[key].get(run["id"], {}).get("digests", {})
            expected_digests = row.get("wire_digests") or {}
            if set(expected_digests) != _DIRECTIONS or any(
                not expected_digests.get(direction)
                or item["wire_digests"].get(direction) != expected_digests[direction]
                for direction in _DIRECTIONS
            ):
                item["failures"].append("four_direction_wire_body_digest_mismatch")
            # RequestContext generates this internal identity; RunStart.id uses
            # the same value. The caller's x-request-id header is NOT this ID.
            item["performance"] = _performance_request(performance_by_request.get(run["id"], {}),
                                                       {run["id"]}, row.get("expected_parent"))
            item["failures"].extend(item["performance"]["failures"])
        item["history_storage"] = _history_proof(run.get("generation_node_id"), row.get("expected_parent"), history_evidence or {})
        item["failures"].extend(item["history_storage"]["failures"])
        if item["history_storage"]["output_sha256"] != row["output_sha256"]:
            item["failures"].append("history_storage_output_digest_mismatch")
        item["credential_protection"] = _credential_proof(
            row, run, credential_rows.get(row.get("nonce"), {}),
            wire_cache.get(detail["interaction"]["id"], {}).get(run["id"], {}),
            (credential_evidence or {}).get(row.get("nonce"), {}),
            item["history_storage"].get("principal_sha256"))
        item["failures"].extend(item["credential_protection"]["failures"])
        item["verified"] = not item["failures"]
        evidence.append(item)
    metadata = (performance_snapshot or {}).get("metadata", {})
    safe_metadata = {key: value for key, value in metadata.items() if isinstance(value, (int, bool))}
    stage_counts = Counter(event.get("name") for event in (performance_snapshot or {}).get("traceEvents", [])
                           if event.get("ph") == "X" and isinstance(event.get("name"), str))
    performance_failures = []
    if require_debug:
        if not performance_snapshot:
            performance_failures.append("performance_snapshot_missing")
        if metadata.get("storage_status") != "ready" or metadata.get("debug_enabled") is not True:
            performance_failures.append("performance_storage_not_ready")
        loss_fields = ("dropped_active", "dropped_completed", "dropped_sql_completed",
                       "dropped_resource_samples", "frozen_spans", "frozen_total", "storage_errors")
        if any(type(metadata.get(key)) is not int or metadata[key] != 0 for key in loss_fields):
            performance_failures.append("performance_capture_loss_or_missing_metadata")
        for label, prefixes in (("routing", ("router.",)), ("history", ("generation_chain.", "turn_chain.")),
                                ("inference", ("proxy.inference_run",))):
            if not any(name.startswith(prefixes) for name in stage_counts):
                performance_failures.append("performance_" + label + "_stage_missing")
    return {"requests": evidence, "requested": len(requests), "matched": sum("interaction_id" in row for row in evidence),
            "verified": sum(bool(row.get("verified")) for row in evidence), "require_debug": require_debug,
            "performance_metadata": safe_metadata, "performance_stage_counts": dict(stage_counts),
            "full_chain_verified": bool(requests) and require_debug and not performance_failures and all(row.get("verified") for row in evidence),
            "failures": ([api_failure] if api_failure else []) + performance_failures + ([] if require_debug else ["debug_capture_not_required"]),
            "coverage": {"ordinary_observation": True, "wire_required": require_debug,
                         "credential_protection": bool(evidence) and all(
                             row.get("credential_protection", {}).get("verified") for row in evidence),
                         "performance_request_correlation": bool(evidence) and all(
                             "performance" in row and not row["performance"]["failures"] for row in evidence),
                         "history_storage_read": bool(evidence) and all(
                             "history_storage" in row and not row["history_storage"]["failures"] for row in evidence)}}
