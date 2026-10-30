"""Deterministic, isolated real-HTTP history-path benchmark (not a pytest test).

Run as ``python -m tests.common.measure_history_paths``. Samples are raw, not
estimates; invalid/unexercised paths fail closed. Use identical arguments for
both frozen binaries. Compact windows must match the fake provider's actual
output semantics; rejected-session operations must have an observed null parent.
Candidate, SQL, load and materialization counts are measurements, not semantic
correctness gates: a projection can legitimately avoid unrelated candidates.
An independent prefix compact case
adds a new user to the penultimate generation and must find that successful source
after scanning newer same-depth decoys; it does not replace the exact-window case.
No protocol-matrix coverage is implied.
Report schema 4 distinguishes Debug-enabled causal evidence from explicitly
disabled normal-runtime measurements. Enabled runs export the timeline only
before and after each repetition, then partition it by observed Run identity.
Disabled runs retain HTTP/history checks but report unknown SQL/stage costs.
Resource sampling excludes snapshots and offline attribution, as in schema 3
and unlike schema 2's repeated-snapshot window.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import re
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
from collections import Counter
from contextlib import closing
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qsl, unquote, urlencode, urlsplit

from tests.common.helpers import (
    CompleteRequestHTTPConnection, initialize_server, start_stravia_server,
    stop_stravia_server, wait_for_setup_token,
)
from tests.common.measure_protocol_http import (
    Catalog, ProcessReader, database_config, encode, response_frames, response_text,
)
from tests.common.protocol_history_audit import NORMALIZED_HISTORY, history_storage_schema


class MeasurementFailure(RuntimeError):
    """Only explicitly sanitized benchmark diagnostics may enter reports."""


def ensure_administrator_access(session):
    access = next((cookie for cookie in session.cookies if cookie.name == "stravia_access"), None)
    if access is not None and access.expires is not None and access.expires > time.time() + 60:
        return
    # Renew before the next diagnostic call, not by replaying failed requests.
    status, _ = session.request("POST", "/api/v1/auth/refresh", {}, timeout=60)
    if status != 200:
        raise MeasurementFailure(f"administrator refresh HTTP {status}")


def semantics(items):
    """Same native-window normalization as admin/test_compaction.py."""
    items = json.loads(json.dumps(items))
    for item in items:
        if item.get("type") in ("function_call", "function_call_output"):
            item.pop("id", None)
            item.pop("status", None)
            continue
        if item.get("role"):
            item.setdefault("type", "message")
        if item.get("type") != "message":
            continue
        if isinstance(item.get("content"), str):
            item["content"] = [{"type": "output_text" if item["role"] == "assistant" else "input_text", "text": item["content"]}]
        for part in item.get("content", []):
            if part.get("annotations") == []:
                del part["annotations"]
        for key in ("id", "status"):
            item.pop(key, None)
    return items


def history_input(user, padding, shape):
    if shape == "text":
        return [user]
    identity = hashlib.sha256(user["content"].encode()).hexdigest()
    arguments = {
        "context": identity,
        "chunks": [{"index": index, "text": padding[start:start + 512], "flags": [True, None, []], "empty": {}}
                   for index, start in enumerate(range(0, len(padding), 512))],
        "exact_integer": 18446744073709551617,
    }
    call_id = "call_" + identity[:24]
    # 完整的客户端历史工具回合，不执行平台工具；参数原文及 call_id 均参与上游验真。
    return [user, {"type": "function_call", "call_id": call_id, "name": "recorded_lookup",
                   "arguments": json.dumps(arguments, sort_keys=True, separators=(",", ":"), ensure_ascii=False)},
            {"type": "function_call_output", "call_id": call_id, "output": "recorded-result-" + identity}]


def compact_semantics(items):
    """Preserve the full native window except Gateway-assigned item identities."""
    items = semantics(items)
    for item in items:
        item.pop("id", None)
    return items


def compact_digest(items):
    """Ignore JSON object key order while preserving window and content order."""
    canonical = json.dumps(compact_semantics(items), sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode("utf-8")
    return hashlib.sha256(canonical).hexdigest()


class NativeProvider(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        items = body.get("input", [])
        with self.server.lock:
            expectation = self.server.expected
            previous_id = body.get("previous_response_id")
            previous_history = self.server.response_histories.get(previous_id) if previous_id else None
        effective_items = (previous_history + items) if previous_history is not None else items
        error_code = "unknown_previous_response" if previous_id and previous_history is None else None
        if semantics(effective_items) != semantics(expectation["input"]):
            error_code = error_code or "synthetic_history_mismatch"
        compact = self.path == "/v1/responses/compact"
        if self.path not in ("/v1/responses", "/v1/responses/compact"):
            error_code = error_code or "synthetic_unsupported_path"
        valid = error_code is None
        signature = hashlib.sha256(encode(semantics(effective_items))).hexdigest()[:24]
        text = expectation["text"]
        if compact:
            signature = hashlib.sha256(encode({"input": semantics(effective_items), "instructions": body.get("instructions")})).hexdigest()[:24]
            window = [
                {"type": "message", "role": "user", "content": "retained task identity"},
                {"type": "compaction", "id": "cmp_" + signature, "encrypted_content": "opaque-" + signature},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "retained answer", "annotations": []}]},
            ]
            result = {"id": "cmp_operation_" + signature, "object": "response.compaction", "created_at": 1788825600, "output": window}
        else:
            result = response_frames("open-responses", "native-model", text, False)[0][1]
            result["id"] = "resp_" + signature
            result["output"][0]["id"] = "msg_" + signature
        if valid and not compact:
            with self.server.lock:
                self.server.response_histories[result["id"]] = json.loads(json.dumps(effective_items + result["output"]))
        if not valid:
            result = {"error": {"code": error_code}}
        data = encode(result)
        self.send_response(200 if valid else 400)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)
        with self.server.lock:
            self.server.arrivals.append({"path": self.path, "valid": valid, "error_code": error_code,
                                         "native_previous_response": bool(previous_id), "previous_response_known": previous_history is not None if previous_id else None,
                                         "input_sha256": hashlib.sha256(encode(semantics(items))).hexdigest(), "input_items": len(items),
                                         "effective_input_sha256": hashlib.sha256(encode(semantics(effective_items))).hexdigest(), "effective_input_items": len(effective_items),
                                         # Digest the fake provider's actual output, not the Gateway response.
                                         # Runs after the write; excluded from Rust CPU, but may overlap client wall time.
                                         "compact_output_sha256": compact_digest(result["output"]) if compact and valid else None})


def admin(session, method, path, body=None, expected=200):
    ensure_administrator_access(session)
    status, value = session.request(method, path, body, timeout=60)
    if status != expected:
        # Response bodies can contain credentials; never include them in errors.
        raise MeasurementFailure(f"admin {method} {path}: HTTP {status}")
    return value["data"]


def provision(session, port, suffix):
    targets = []
    for index in range(2):
        provider = admin(session, "POST", "/api/v1/providers", {
            "name": f"history-{suffix}-{index}",
            "source": {"type": "custom", "vendor": "custom", "channel": "default", "protocol": "open-responses", "base_url": f"http://127.0.0.1:{port}/v1"},
            "credential": {"type": "api_key", "value": "local-test-credential"}, "vendor_options": {},
        })
        admin(session, "POST", f"/api/v1/providers/{provider['id']}/models", {
            "model_id": "native-model", "metadata": {"name": "Native benchmark", "modalities": {"input": ["text"], "output": ["text"]}, "reasoning_efforts": ["none", "low", "medium", "high"], "limit": {"context": 100000}},
        }, 201)
        targets.append({"provider_id": provider["id"], "model": "native-model", "enabled": True, "priority": index, "target_retry_budget": 0})
    model = "history-" + suffix
    route = admin(session, "POST", "/api/v1/models", {"model_id": model, "output_modality": "text", "targets": targets})
    key = admin(session, "POST", "/api/v1/api-keys", {"name": model, "model_ids": [route["id"]]})["key"]
    return model, route["id"], key


def observe(session, route, generation, directory, exclude_runs=(), *, require_trace=True):
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        query = urlencode({"model": route, "anchor_at": int(time.time() * 1000) + 1000, "window_index": 0, "limit": 100})
        forest = admin(session, "GET", "/api/v1/observations/interactions?" + query)
        for root in forest["roots"]:
            for interaction in root["interactions"]:
                detail = admin(session, "GET", "/api/v1/observations/interactions/" + interaction["id"])
                for run in detail["runs"]:
                    if run["id"] in exclude_runs or run.get("generation_node_id") != generation:
                        continue
                    trace = run.get("trace") or {}
                    if require_trace and trace.get("status") != "complete":
                        continue
                    if not require_trace and run.get("status") != "completed":
                        continue
                    return run
        time.sleep(.1)
    raise MeasurementFailure("completed Generation observation/required trace not available")


def unknown_projection_fixture(args, directory, route, key, candidate_ids):
    """Clear only nullable derived proofs for this driver's own fresh Principal.

    Table presence selects normalized v3 or the frozen legacy layout; this is a
    measurement adapter for baseline binaries, not a product compatibility path.
    """
    fields = ("prefix_projection_version", "prefix_controls_fingerprint", "prefix_context_fingerprint")
    report = {"backend": args.backend, "operation": "set derived candidate projection to NULL outside inference timer",
              "unknown_fields": [], "updated_nodes": 0, "candidate_nodes": len(candidate_ids),
              "performance_scope": "conservative unknown-projection fallback; not normal projected-data benefit"}
    assignments = ", ".join(field + " = NULL" for field in fields)

    def table_and_schema(tables):
        schema = history_storage_schema(tables)
        return ("turn_nodes" if schema == NORMALIZED_HISTORY else "turn_chain_nodes"), schema

    def scope_guard(schema, key_sql, route_sql):
        effective = (f"(SELECT 'api-key:' || k.id FROM api_keys k "
                     "JOIN api_key_models km ON km.api_key_id = k.id "
                     f"WHERE k.token = {key_sql} AND km.model_id = {route_sql})")
        if schema == NORMALIZED_HISTORY:
            return f"scope_id = (SELECT s.id FROM turn_scopes s WHERE s.principal = {effective})"
        return f"principal = {effective}"

    if args.backend == "sqlite":
        path = Path(directory) / "db" / "gateway.db"
        if not path.is_file():
            raise MeasurementFailure("isolated SQLite database not found")
        with closing(sqlite3.connect(path, timeout=30)) as connection, connection:
            table, schema = table_and_schema(
                {row[0] for row in connection.execute("SELECT name FROM sqlite_master WHERE type='table'")})
            report["history_schema"] = schema
            present = {row[1] for row in connection.execute(f"PRAGMA table_info({table})")}
            available = set(fields) & present
            if not available:
                report["status"] = "baseline schema has no projection columns; no writes"
                return report
            if available != set(fields):
                raise MeasurementFailure("incomplete candidate projection schema")
            owner = scope_guard(schema, "?", "?")
            placeholders = ",".join("?" for _ in candidate_ids)
            cursor = connection.execute(
                f"UPDATE {table} SET {assignments} WHERE id IN ({placeholders}) AND {owner}",
                (*candidate_ids, key, route),
            )
            report["updated_nodes"] = cursor.rowcount
            if cursor.rowcount != len(candidate_ids):
                raise MeasurementFailure("isolated projection fixture did not update every candidate")
    else:
        # Test tooling only; psql is explicitly reported and never a production dependency.
        address = urlsplit(args.database_url)
        userinfo, separator, hostinfo = address.netloc.rpartition("@")
        clean_netloc = userinfo.split(":", 1)[0] + "@" + hostinfo if separator else address.netloc
        password = unquote(address.password) if address.password is not None else None
        options = parse_qsl(address.query, keep_blank_values=True)
        for name, value in options:
            if name == "password":
                password = value
        clean_url = address._replace(netloc=clean_netloc,
                                     query=urlencode([(name, value) for name, value in options if name != "password"])).geturl()
        connection_env = dict(os.environ)
        if password is not None:
            connection_env["PGPASSWORD"] = password

        def query(sql):
            completed = subprocess.run(
                [args.psql, "--no-psqlrc", "--dbname", clean_url, "--tuples-only", "--no-align", "--set", "ON_ERROR_STOP=1"],
                input=sql, text=True, capture_output=True, timeout=60, env=connection_env,
            )
            if completed.returncode:
                raise MeasurementFailure("isolated PostgreSQL projection fixture failed")
            return [line for line in completed.stdout.splitlines() if line]

        def literal(value):
            return "'" + value.replace("'", "''") + "'"

        table, schema = table_and_schema(set(query(
            "SELECT table_name FROM information_schema.tables WHERE table_schema=current_schema();")))
        report["history_schema"] = schema
        names = ",".join(literal(field) for field in fields)
        available = set(query("SELECT column_name FROM information_schema.columns WHERE table_schema=current_schema() "
                              f"AND table_name={literal(table)} AND column_name IN ({names});"))
        report["tool"] = "psql"
        if not available:
            report["status"] = "baseline schema has no projection columns; no writes"
            return report
        if available != set(fields):
            raise MeasurementFailure("incomplete candidate projection schema")
        identifiers = ",".join(literal(value) for value in candidate_ids)
        # A count guard inside the transaction prevents partial fixture writes.
        guard = f"id IN ({identifiers}) AND {scope_guard(schema, literal(key), literal(route))}"
        sql = ("BEGIN; SET LOCAL standard_conforming_strings=on; "
               f"DO $fixture$ BEGIN IF (SELECT count(*) FROM {table} WHERE {guard}) <> {len(candidate_ids)} "
               "THEN RAISE EXCEPTION 'isolated fixture count mismatch'; END IF; END $fixture$; "
               f"WITH changed AS (UPDATE {table} SET {assignments} WHERE {guard} RETURNING id) "
               "SELECT count(*) FROM changed; COMMIT;")
        output = query(sql)
        counts = [int(line) for line in output if line.isdecimal()]
        if counts != [len(candidate_ids)]:
            raise MeasurementFailure("isolated PostgreSQL projection fixture count unavailable")
        report["updated_nodes"] = counts[0]
    report.update(status="applied", unknown_fields=list(fields))
    return report


class RequestTransport:
    """Own inference sockets only; admin traffic is deliberately separate."""

    def __init__(self, mode):
        self.mode = mode
        self.connections = {}

    def acquire(self, base):
        address = urlsplit(base)
        target = (address.hostname, address.port)
        connection = self.connections.get(target) if self.mode == "persistent" else None
        if connection is None:
            connection = CompleteRequestHTTPConnection(*target, timeout=120)
            if self.mode == "persistent":
                self.connections[target] = connection
        return connection

    def release(self, connection, failed=False):
        if self.mode == "fresh" or failed or connection.sock is None:
            connection.close()
            self.connections = {target: value for target, value in self.connections.items() if value is not connection}

    def close(self):
        for connection in self.connections.values():
            connection.close()
        self.connections.clear()


def send(transport, base, key, model, items, upstream, case, expected_parent, text, previous=None, compact=False):
    body = {"model": model, "input": items}
    if previous:
        body["previous_response_id"] = previous
    if "instructions" in case:
        body["instructions"] = case["instructions"]
    with upstream.lock:
        upstream.expected = {"input": case["upstream_input"], "text": text}
        arrivals_start = len(upstream.arrivals)
    connection = transport.acquire(base)
    raw_body = encode(body)
    started = time.perf_counter()
    request_started = None
    row = {"case": case["name"], "round": case["round"], "expected_parent": expected_parent, "request_bytes": len(raw_body), "input_sha256": hashlib.sha256(encode(items)).hexdigest(), "status": None, "terminal_valid": False, "content_valid": False, "error": None, "first_content_ms": None, "first_content_semantics": "unary full response available"}
    result = None
    row.update(http_connection_mode=transport.mode, socket_reused=connection.sock is not None, connect_ms=0.0)
    try:
        if connection.sock is None:
            connect_started = time.perf_counter()
            try:
                connection.connect()
            finally:
                row["connect_ms"] = (time.perf_counter() - connect_started) * 1000
        request_started = time.perf_counter()
        connection.request("POST", "/v1/responses" + ("/compact" if compact else ""), raw_body, {
            "Authorization": "Bearer " + key, "Content-Type": "application/json",
            # Public ingress contract: enables session candidate indexing by
            # semantic-unit count before canonical-history verification.
            "x-session-id": "history-paths-v1:" + model,
        })
        response = connection.getresponse()
        row["status"] = response.status
        raw = response.read()
        row["latency_ms"] = (time.perf_counter() - started) * 1000
        row["http_request_ms"] = (time.perf_counter() - request_started) * 1000
        result = json.loads(raw)
        error = result.get("error") if isinstance(result, dict) else None
        code = error.get("code") if isinstance(error, dict) else None
        safe_codes = {"synthetic_history_mismatch", "unknown_previous_response", "synthetic_unsupported_path",
                      "invalid_previous_response_id", "invalid_request_error", "upstream_error", "upstream_request_failed",
                      "parent_generation_not_found", "generation_not_found", "invalid_compacted_window"}
        row["response_error_code"] = code if isinstance(code, str) and code in safe_codes else ("unrecognized_error_code" if code is not None else None)
        if compact:
            row["terminal_valid"] = result.get("object") == "response.compaction" and bool(result.get("output"))
            # Compare against this provider arrival after synchronization below.
            row["content_valid"] = False
        else:
            visible = response_text("open-responses", raw, False)
            row["terminal_valid"] = (
                result.get("object") == "response"
                and result.get("status") == "completed"
                and result.get("error") is None
                and result.get("incomplete_details") is None
                and isinstance(result.get("id"), str) and bool(result["id"])
                and isinstance(result.get("output"), list)
                and bool(result["output"])
                and all(item.get("status") == "completed" for item in result["output"] if item.get("type") == "message")
            )
            row["content_valid"] = visible == text
            if visible:
                row["first_content_ms"] = row["latency_ms"]
        row["generation_id"] = result.get("id")
    except Exception as error:
        row["error"] = type(error).__name__
    finally:
        row.setdefault("latency_ms", (time.perf_counter() - started) * 1000)
        row.setdefault("http_request_ms", (time.perf_counter() - request_started) * 1000 if request_started is not None else None)
        transport.release(connection, failed=row["error"] is not None)
    # Handler appends after write; synchronize without mistaking a scheduling race for failure.
    deadline = time.monotonic() + 5
    while len(upstream.arrivals) == arrivals_start and time.monotonic() < deadline:
        time.sleep(.005)
    with upstream.lock:
        row["upstream"] = list(upstream.arrivals[arrivals_start:])
    if compact and isinstance(result, dict) and isinstance(result.get("output"), list) and len(row["upstream"]) == 1:
        expected_sha256 = row["upstream"][0].get("compact_output_sha256")
        row["compact_output_sha256"] = compact_digest(result["output"])
        row["content_valid"] = expected_sha256 is not None and row["compact_output_sha256"] == expected_sha256
    row["valid"] = row["status"] == 200 and row["terminal_valid"] and row["content_valid"] and not row["error"] and len(row["upstream"]) == 1 and row["upstream"][0]["valid"]
    return row, result


def timeline(session):
    ensure_administrator_access(session)
    status, value = session.request("GET", "/api/v1/performance/timeline", timeout=60)
    if status != 200:
        raise MeasurementFailure(f"performance timeline HTTP {status}")
    return value


def report_progress(result, row):
    print(json.dumps({"completed_requests": len(result["requests"]), "case": row["case"],
                      "round": row["round"], "repetition": row["repetition"]}), flush=True)


def attach_performance_snapshot(rows, before, snapshot):
    """整轮只索引一次因果森林；通过已观测 Run ID 归属，不依赖时间窗或请求顺序。"""
    metadata_fields = ("dropped_active", "dropped_completed", "dropped_sql_completed", "dropped_resource_samples",
                       "incomplete", "incomplete_total", "active_spans", "frozen_spans", "frozen_total",
                       "storage_errors", "debug_epoch")
    loss_fields = ("dropped_active", "dropped_completed", "dropped_sql_completed", "dropped_resource_samples",
                   "frozen_spans", "frozen_total", "storage_errors")
    metadata_before = {key: before.get("metadata", {}).get(key) for key in metadata_fields}
    metadata_after = {key: snapshot.get("metadata", {}).get(key) for key in metadata_fields}
    metadata_valid = all(
        type(metadata_before[key]) is int and type(metadata_after[key]) is int
        and metadata_before[key] >= 0 and metadata_after[key] >= 0
        and (key not in loss_fields or metadata_after[key] <= metadata_before[key])
        for key in metadata_fields
    ) and metadata_before["debug_epoch"] == metadata_after["debug_epoch"] and all(
        value.get("metadata", {}).get("schema_version") == 3
        and value.get("metadata", {}).get("storage_status") == "ready"
        and value.get("metadata", {}).get("debug_enabled") is True
        for value in (before, snapshot)
    )
    capture_errors = []
    for label, value in (("before", before), ("after", snapshot)):
        states = {"active": 0, "frozen": 0}
        for event in value.get("traceEvents", []):
            if event.get("ph") not in ("B", "X"):
                continue
            state = event.get("args", {}).get("capture_state")
            if (state, event["ph"]) not in (("active", "B"), ("frozen", "B"), ("finished", "X")):
                capture_errors.append(f"{label}: invalid capture state: {event.get('name')}")
            if isinstance(state, str) and state in states:
                states[state] += 1
            if state == "frozen":
                capture_errors.append(f"{label}: frozen span: {event.get('name')}")
            if state == "finished" and not isinstance(event.get("dur"), (int, float)):
                capture_errors.append(f"{label}: missing duration: {event.get('name')}")
        for state, field in (("active", "active_spans"), ("frozen", "frozen_spans")):
            if value.get("metadata", {}).get(field) != states[state]:
                capture_errors.append(f"{label}: {field} does not match retained spans")
    raw_events = [event for event in snapshot.get("traceEvents", []) if event.get("ph") in ("B", "X")]
    by_id = {event["args"]["span_id"]: event for event in raw_events
             if type(event.get("args", {}).get("span_id")) is int}
    if len(by_id) != len(raw_events):
        capture_errors.append("missing or duplicate span_id")
    prior_ids = {event.get("args", {}).get("span_id") for event in before.get("traceEvents", [])
                 if event.get("ph") in ("B", "X")}
    contexts = {}
    for identifier in by_id:
        cursor, path, seen = identifier, [], set()
        while cursor not in contexts:
            if cursor in seen:
                capture_errors.append(f"causal cycle: {by_id[identifier]['name']}")
                contexts[cursor] = (None, "unattributed")
                break
            seen.add(cursor)
            event = by_id[cursor]
            parent = event.get("args", {}).get("parent_id")
            if parent is None:
                name = event["name"]
                kind = ("inference" if name == "proxy.inference_run" else
                        "observation" if name.startswith("observation.") else
                        "unrelated_sql" if name in ("sql.query", "sql.pool.acquire") else "unattributed")
                contexts[cursor] = (cursor, kind)
                break
            path.append(cursor)
            if type(parent) is not int or parent not in by_id:
                capture_errors.append(f"missing parent: {event['name']}")
                contexts[cursor] = (None, "unattributed")
                break
            cursor = parent
        root, kind = contexts[cursor]
        for member in reversed(path):
            if kind == "unrelated_sql":
                kind = "unattributed"
            if kind == "inference" and by_id[member]["name"] == "observation.command.process":
                kind = "observation"
            contexts[member] = (root, kind)
    groups, roots_by_request, safe_events, active = {}, {}, [], []
    for event in raw_events:
        raw_args = event.get("args", {})
        identifier = raw_args.get("span_id")
        root, kind = contexts.get(identifier, (None, "unattributed"))
        # 只输出固定名称与数值归因；不复制请求身份以外的字符串、正文或凭据。
        args = {key: value for key, value in raw_args.items()
                if isinstance(value, (int, float, bool))
                and not any(word in key.lower() for word in ("token", "key", "secret"))}
        state = raw_args.get("capture_state")
        if state in ("active", "frozen", "finished"):
            args["capture_state"] = state
        safe = {"name": event["name"], "ts_us": event.get("ts"), "dur_us": event.get("dur"), "args": args,
                "sql_count": args.get("sql_query_count", 0), "sql_duration_us": args.get("sql_query_duration_us", 0)}
        if event.get("ph") == "X":
            safe_events.append(safe)
            groups.setdefault(root, []).append((safe, kind))
        if state == "active":
            active.append((root, kind, {"name": event["name"], "span_id": identifier,
                                       "parent_id": raw_args.get("parent_id")}))
        if event["name"] == "proxy.inference_run" and identifier not in prior_ids:
            request_id = raw_args.get("request_id")
            if isinstance(request_id, str):
                roots_by_request.setdefault(request_id, []).append(safe)
            else:
                capture_errors.append("missing inference request identity")
    wanted = [row.get("observation_run_id") for row in rows]
    if any(not isinstance(identity, str) or not identity for identity in wanted) or len(set(wanted)) != len(wanted):
        capture_errors.append("missing or duplicate observed request identity")
    unclaimed = set(roots_by_request).difference(wanted)
    if unclaimed:
        capture_errors.append("unclaimed inference request identity")
    unattributed_active = [dict(span, attribution=kind) for _, kind, span in active if kind == "unattributed"]
    observation_active = [dict(span, attribution=kind) for _, kind, span in active if kind == "observation"]
    routing_unattributed = []
    for span in safe_events:
        name = span["name"]
        if not name.startswith(("router.select", "generation_chain.", "turn_chain.")):
            continue
        _, kind = contexts.get(span["args"].get("span_id"), (None, "unattributed"))
        if kind != "inference" and not (kind == "observation" and name.startswith(("generation_chain.", "turn_chain."))):
            routing_unattributed.append(name)
    fresh_incomplete = [span["name"] for _, _, span in active if span["span_id"] not in prior_ids]
    global_incomplete = [event["name"] for event in raw_events if event.get("ph") == "B"]
    selected_roots = set()
    for row in rows:
        roots = roots_by_request.get(row.get("observation_run_id"), [])
        root_id = roots[0]["args"].get("span_id") if len(roots) == 1 else None
        if root_id is not None:
            selected_roots.add(root_id)
        associated = groups.get(root_id, []) if root_id is not None else []
        events = [span for span, _ in associated]
        request_spans = [span for span, kind in associated if kind == "inference"]
        request_history = [span for span in request_spans
                           if span["name"].startswith(("router.select", "generation_chain.", "turn_chain."))]
        active_request = [dict(span, attribution="inference") for root, kind, span in active
                          if kind == "inference" and root == root_id]
        row.update(
            performance_spans=events,
            performance_stage_counts=dict(Counter(span["name"] for span in events)),
            trace_metadata_before=metadata_before, trace_metadata_after=metadata_after,
            inference_root_span_count=len(roots), inference_root_span_id=root_id,
            fresh_incomplete_spans=fresh_incomplete, global_incomplete_spans=global_incomplete,
            active_request_spans=active_request, active_observation_spans=observation_active,
            active_unrelated_spans=[dict(span, attribution="other_inference" if kind == "inference" else kind)
                                    for root, kind, span in active
                                    if kind == "unrelated_sql" or (kind == "inference" and root != root_id)],
            unattributed_active_spans=unattributed_active, trace_capture_errors=capture_errors,
            trace_metadata_valid=metadata_valid and not capture_errors and not active_request and not unattributed_active,
            request_sql_span_count=sum(span["name"] == "sql.query" for span in request_spans),
            request_sql_duration_us=sum(span["dur_us"] for span in request_spans
                                        if span["name"] == "sql.query" and isinstance(span["dur_us"], (int, float))),
            request_subtree_span_count=len(request_spans),
            observation_history_spans=[span for span, kind in associated if kind == "observation"
                                       and span["name"].startswith(("generation_chain.", "turn_chain."))],
            history_measurements=[{"name": span["name"], "dur_us": span["dur_us"], "sql_count": span["sql_count"],
                                   "sql_duration_us": span["sql_duration_us"], "counters": span["args"]}
                                  for span in request_history if span["name"].startswith(("generation_chain.", "turn_chain."))],
            unattributed_routing_history_spans=routing_unattributed,
            request_span_valid=(len(roots) == 1 and type(root_id) is int
                                and roots[0]["args"].get("capture_state") == "finished"
                                and isinstance(roots[0].get("dur_us"), (int, float))
                                and bool(request_history) and not routing_unattributed and not active_request and not unclaimed),
            trace_attribution="按真实 Observation Run ID 选择唯一已结束推理根并索引完整因果森林；observation 单列，其他请求不计入该请求；不均摊后台成本、不累加嵌套 SQL 计数",
        )
    return {"trace_metadata_before": metadata_before, "trace_metadata_after": metadata_after,
            "trace_capture_errors": capture_errors,
            "background_performance_spans": [span for span in safe_events
                                              if contexts.get(span["args"].get("span_id"), (None, "unattributed"))[0]
                                              not in selected_roots],
            "valid": bool(rows) and all(row["trace_metadata_valid"] and row["request_span_valid"] for row in rows)}



def attach_disabled_performance_controls(rows, before, after):
    """只验证显式关闭 Debug 的控制条件，不把未采集的 SQL 或阶段成本记成零。"""
    errors = []
    metadata = []
    for label, snapshot in (("before", before), ("after", after)):
        meta = snapshot.get("metadata") if isinstance(snapshot, dict) else None
        events = snapshot.get("traceEvents") if isinstance(snapshot, dict) else None
        if not isinstance(meta, dict):
            errors.append(label + ": missing metadata")
            metadata.append({})
            continue
        epoch = meta.get("debug_epoch")
        if (type(meta.get("schema_version")) is not int or meta["schema_version"] != 3
                or meta.get("debug_enabled") is not False or "debug_epoch" not in meta
                or not (epoch is None or (type(epoch) is int and epoch >= 0))):
            errors.append(label + ": Debug-disabled control is not established")
        if not isinstance(events, list) or any(not isinstance(event, dict) or event.get("ph") not in ("M", "C") for event in events):
            errors.append(label + ": unexpected performance spans")
        metadata.append({key: meta.get(key) for key in ("schema_version", "debug_enabled", "debug_epoch")})
    if metadata[0].get("debug_epoch") != metadata[1].get("debug_epoch"):
        errors.append("Debug session changed during the measurement")
    identities = [row.get("observation_run_id") for row in rows]
    if not rows or any(not isinstance(identity, str) or not identity for identity in identities):
        errors.append("missing observed request identity")
    elif len(set(identities)) != len(identities):
        errors.append("duplicate observed request identity")
    for row in rows:
        row.update(request_sql_span_count=None, request_sql_duration_us=None, history_measurements=None)
    return {"metadata_before": metadata[0], "metadata_after": metadata[1],
            "control_errors": errors, "valid": not errors}


def process_io_counters(pid):
    if not sys.platform.startswith("linux"):
        return None
    fields = dict(line.split(":", 1) for line in Path(f"/proc/{pid}/io").read_text().splitlines())
    return {key: int(fields[key]) for key in ("read_bytes", "write_bytes", "cancelled_write_bytes")}


def finalize_validity(result, args):
    rows = result["requests"]
    performance_debug = result["performance_debug"]
    captures = result["performance_captures"] if performance_debug else result["debug_disabled_controls"]
    if len(captures) != args.repetitions or any(not capture["valid"] for capture in captures):
        result["failure_reasons"].append("missing or invalid whole-repetition performance capture/control")
    if any(not row["valid"] for row in rows):
        result["failure_reasons"].append("HTTP, parent, terminal or content validation failed")
    expected = args.repetitions * (3 * args.rounds + 3 * args.candidates + 2 * args.shared_rounds + 10)
    if len(rows) != expected:
        result["failure_reasons"].append(f"missing requests: {len(rows)} of {expected}")
    compact_rows = [row for row in rows if row["case"] == "native-compact-operation"]
    if len(compact_rows) != args.repetitions:
        result["failure_reasons"].append(f"missing native compact operations: {len(compact_rows)} of {args.repetitions}")
    evidence = []
    for row in compact_rows:
        spans = [span for span in row.get("performance_spans", []) if span["name"] == "generation_chain.parent.discover_prefix"]
        candidate_counts = [span["args"].get("candidate_count", 0) for span in spans] if performance_debug else None
        valid = row.get("parent_valid", False) and row.get("content_valid", False)
        evidence.append({"repetition": row["repetition"], "discovery_span_count": len(spans) if performance_debug else None, "candidate_counts": candidate_counts, "valid": valid})
        if not valid:
            result["failure_reasons"].append("native compact source parent or full output-window digest mismatch")
    result["native_discovery_evidence"] = evidence
    prefix_rows = [row for row in rows if row["case"] == "native-prefix-compact-operation"]
    if len(prefix_rows) != args.repetitions:
        result["failure_reasons"].append(f"missing native prefix compact operations: {len(prefix_rows)} of {args.repetitions}")
    prefix_evidence = []
    for row in prefix_rows:
        spans = [span for span in row.get("performance_spans", []) if span["name"] == "generation_chain.parent.discover_prefix"]
        candidate_counts = [span["args"].get("candidate_count", 0) for span in spans] if performance_debug else None
        valid = row.get("parent_valid", False) and row.get("content_valid", False)
        prefix_evidence.append({"repetition": row["repetition"], "discovery_span_count": len(spans) if performance_debug else None, "candidate_counts": candidate_counts,
                                "expected_parent": row["expected_parent"], "observed_parent": row.get("observed_parent"),
                                "parent_valid": row.get("parent_valid", False), "valid": valid})
        if not valid:
            result["failure_reasons"].append("native prefix case lacks its successful source parent or full output-window digest")
    result["native_prefix_discovery_evidence"] = prefix_evidence
    rejected_rows = [row for row in rows if row["case"] == "rejected-session-candidates"]
    if len(rejected_rows) != args.repetitions:
        result["failure_reasons"].append(f"missing rejected-session operations: {len(rejected_rows)} of {args.repetitions}")
    rejected_evidence = []
    for row in rejected_rows:
        spans = [span for span in row.get("performance_spans", []) if span["name"] == "generation_chain.parent.discover_prefix"]
        candidate_counts = [span["args"].get("candidate_count", 0) for span in spans] if performance_debug else None
        valid = row.get("parent_valid", False) and row.get("observed_parent") is None and row.get("content_valid", False)
        rejected_evidence.append({"repetition": row["repetition"], "discovery_span_count": len(spans) if performance_debug else None, "candidate_counts": candidate_counts,
                                  "observed_parent": row.get("observed_parent"), "parent_valid": row.get("parent_valid", False), "valid": valid})
        if not valid:
            result["failure_reasons"].append("rejected-session case lacks a verified null parent or full output-window digest")
    result["rejected_session_discovery_evidence"] = rejected_evidence
    shared_evidence = []
    for name in ("shared-ancestor-match", "shared-ancestor-reject",
                 "shared-ancestor-unprojected-candidates-match", "shared-ancestor-unprojected-candidates-reject"):
        probes = [row for row in rows if row["case"] == name]
        if len(probes) != args.repetitions:
            result["failure_reasons"].append(f"missing {name} operations: {len(probes)} of {args.repetitions}")
        for row in probes:
            valid = row.get("parent_valid", False) and row.get("content_valid", False)
            if name.endswith("-reject"):
                valid = valid and row.get("observed_parent") is None
            shared_evidence.append({"case": name, "repetition": row["repetition"],
                                    "expected_parent": row["expected_parent"], "observed_parent": row.get("observed_parent"),
                                    "seeded_newer_decoys": args.candidates, "shared_rounds": args.shared_rounds,
                                    "history_measurements": row.get("history_measurements", []), "valid": valid})
            if not valid:
                result["failure_reasons"].append(f"{name} real parent/content oracle failed")
    result["shared_history_evidence"] = shared_evidence
    result["expected_request_count"] = expected
    if performance_debug and any(not row.get("performance_spans") for row in rows):
        result["failure_reasons"].append("missing per-request performance spans")
    if performance_debug and any(not row.get("trace_metadata_valid") for row in rows):
        result["failure_reasons"].append("trace metadata missing, changed Debug session, storage failure, dropped/frozen spans, invalid causal chains or unfinished request spans")
    if performance_debug and any(not row.get("request_span_valid") for row in rows):
        result["failure_reasons"].append("request trace lacks exactly one completed inference root or routing/history spans are outside its subtree")
    if result["rss_peak_bytes"] is None:
        result["failure_reasons"].append("no server RSS samples")
    result["valid"] = not result["failure_reasons"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--label", required=True)
    parser.add_argument("--backend", choices=("sqlite", "postgres"), default="sqlite")
    parser.add_argument("--database-url")
    parser.add_argument("--redis-url", help="Dedicated loopback Redis for PostgreSQL runtime cache")
    parser.add_argument("--confirm-isolated-database", action="store_true",
                        help="Confirm database and any supplied Redis are dedicated isolated benchmark resources")
    parser.add_argument("--repetitions", type=int, default=3)
    parser.add_argument("--rounds", type=int, default=8)
    parser.add_argument("--candidates", type=int, default=48)
    parser.add_argument("--prompt-bytes", type=int, default=8192)
    parser.add_argument("--history-items", choices=("text", "tool-json"), default="text",
                        help="Add a completed client-owned tool call/result to each new user; tool-json retains structured arguments and exact large integers")
    parser.add_argument("--shared-rounds", type=int, default=8, help="Rounds in the independent common-ancestor chain")
    parser.add_argument("--cache-capacity-mb", type=int, default=0, help="SharedCache capacity; 0 disables it for cold-path measurement")
    parser.add_argument("--http-connection", choices=("fresh", "persistent"), default="fresh",
                        help="Inference socket lifetime; connect_ms is separate from http_request_ms")
    parser.add_argument("--psql", default="psql", help="PostgreSQL-only test fixture executable; no Python DB driver required")
    parser.add_argument("--trace-completed-capacity", type=int, default=20000,
                        help="Dedicated server completed-span memory buffers; overflow spills to temporary files and actual loss remains invalid")
    parser.add_argument("--performance-debug", choices=("enabled", "disabled"), default="enabled",
                        help="Disabled measures normal runtime cost with the same HTTP/history checks, but cannot report causal spans or SQL costs")
    parser.add_argument("--sample-interval", type=float, default=.05)
    args = parser.parse_args()
    performance_debug = args.performance_debug == "enabled"
    if args.repetitions < 1 or args.rounds < 2 or args.candidates < 2 or args.prompt_bytes < 128 or args.sample_interval <= 0:
        parser.error("repetitions >=1, rounds >=2, candidates >=2, prompt-bytes >=128 and positive sample-interval required")
    if args.shared_rounds < 2 or args.cache_capacity_mb < 0:
        parser.error("shared-rounds >=2 and cache-capacity-mb >=0 required")
    if args.trace_completed_capacity < 1:
        parser.error("trace-completed-capacity >=1 required")
    if args.backend == "postgres" and args.repetitions != 1:
        parser.error("Postgres requires --repetitions 1 and a fresh dedicated database per invocation")
    if args.backend == "postgres" and not args.redis_url:
        parser.error("Postgres requires --redis-url for its dedicated loopback runtime cache")
    if args.redis_url:
        redis = urlsplit(args.redis_url)
        if redis.scheme not in ("redis", "rediss") or redis.hostname not in ("127.0.0.1", "localhost", "::1") or not args.confirm_isolated_database:
            parser.error("Redis requires a loopback redis/rediss URL and --confirm-isolated-database; never supply production storage")
    database = database_config(args)
    binary = args.binary.resolve()
    with binary.open("rb") as source:
        sha = hashlib.file_digest(source, "sha256").hexdigest()
    metadata = {name: str(value) if isinstance(value, Path) else value for name, value in vars(args).items()}
    metadata["database_url"] = "isolated loopback database (credentials omitted)" if args.database_url else None
    metadata["redis_url"] = "isolated loopback Redis (credentials omitted)" if args.redis_url else None
    result = {"schema_version": 4, "label": args.label, "binary_sha256": sha, "platform": platform.platform(), "workload": metadata, "seed": "history-paths-v1", "session_header": "x-session-id=history-paths-v1:<model>; fixed per route across root, candidates and compact", "compaction_source": "canonical full history without response item identities; same controls as source generations", "native_prefix_compaction_source": "independent successful penultimate-generation prefix plus a deterministic new user; newer same-depth decoys must not be selected over that source; not the original exact-window case", "candidate_depth": "main chain minus one round; distinct content in shared session namespace", "rejected_session_case": "unchanged canonical full history with legal mismatched instructions; observed null parent required, projection filtering allowed", "cache": {"capacity_mb": args.cache_capacity_mb}, "performance_debug": performance_debug, "ordinary_observation_history": True, "projection_fixtures": [], "samples": [], "requests": [], "valid": False, "failure_reasons": []}
    result.update(
        performance_captures=[],
        debug_disabled_controls=[],
        process_io_windows=[],
        trace_collection=("one initial and one final snapshot per repetition; observed Run ID selects the request causal subtree; unassigned spans reported once per capture" if performance_debug
                          else "Debug explicitly disabled; initial/final controls checked; no causal span or SQL-cost evidence collected"),
        resource_sampling_scope="provisioning, inference and observation inspection; excludes initial/final full timeline export and offline attribution; not comparable to schema 2 whole-window CPU/RSS",
        process_io_scope="Linux process I/O counters across the resource window, including all server files/threads; not database-only I/O or device write amplification; null on unsupported platforms",
        http_timing={"latency_ms": "connect plus inference HTTP exchange through complete body",
                     "connect_ms": "explicit socket connect only; zero for reused sockets",
                     "http_request_ms": "inference HTTP exchange through complete body, excluding connect"},
        shared_ancestor_cases={
            "shared-ancestor": "normal complete projections; controls equal, canonical histories differ; filtering allowed",
            "shared-ancestor-unprojected-candidates": "independent route and Principal; only candidate derived projection proofs set NULL outside timers; conservative fallback, not normal-data performance",
        },
        measurement_contract="candidate counts, stage occurrence counts and span-local SQL/load/materialize counters are measurements only; do not sum nested SQL spans",
    )
    padding = "".join(hashlib.sha256(f"history-paths-v1:{i}".encode()).hexdigest() for i in range((args.prompt_bytes + 63) // 64))[:args.prompt_bytes]
    upstream = ThreadingHTTPServer(("127.0.0.1", 0), NativeProvider)
    upstream.lock = threading.Lock()
    upstream.expected = {}
    upstream.response_histories = {}
    upstream.arrivals = []
    catalog = ThreadingHTTPServer(("127.0.0.1", 0), Catalog)
    catalog.arrivals = []
    threads = []
    for server in (upstream, catalog):
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        threads.append(thread)
    stage = "resource-startup"
    try:
        for repetition in range(args.repetitions):
            with tempfile.TemporaryDirectory(prefix="stravia-history-bench-") as directory:
                process = None
                logs = []
                stop = threading.Event()
                sampler = reader = None
                transport = RequestTransport(args.http_connection)
                try:
                    config_path = Path(directory) / "benchmark.toml"
                    config_text = f"[cache]\ncapacity_mb = {args.cache_capacity_mb}\n"
                    if args.redis_url:
                        config_text += "redis_url = " + json.dumps(args.redis_url) + "\n"
                    config_path.write_text(config_text, encoding="utf-8")
                    command = ["--host", "127.0.0.1", "--port", "0", "--data-dir", directory, "--config", str(config_path), "--test-catalog-base-url", f"http://127.0.0.1:{catalog.server_port}"]
                    stage = f"repetition-{repetition}:server-start"
                    process, logs = start_stravia_server(stravia_binary=binary, args=command, env={
                        "RUST_LOG": "warn,stravia_server=info",
                        "STRAVIA_PERF_TRACE_COMPLETED_CAPACITY": str(args.trace_completed_capacity),
                        "STRAVIA_PERF_SQL_COMPLETED_CAPACITY": str(args.trace_completed_capacity),
                    })
                    stage = f"repetition-{repetition}:setup-token"
                    token = wait_for_setup_token(logs, process, timeout=180)
                    listener = next(re.search(r"Stravia startup listener opened .*?(127\.0\.0\.1:\d+)", line).group(1) for line in logs if re.search(r"Stravia startup listener opened .*?(127\.0\.0\.1:\d+)", line))
                    base = "http://" + listener
                    stage = f"repetition-{repetition}:http-setup"
                    session = initialize_server(base, token, database)
                    stage = f"repetition-{repetition}:debug-control"
                    admin(session, "PUT", "/api/v1/observations/debug", {"enabled": performance_debug, "confirmed": True})
                    capture_before = timeline(session)
                    first_request = len(result["requests"])
                    io_before = process_io_counters(process.pid)
                    reader = ProcessReader(process.pid)
                    def sample():
                        while not stop.is_set():
                            try:
                                result["samples"].append({"repetition": repetition, "timestamp": time.time(), **reader.read()})
                            except Exception as error:
                                result["failure_reasons"].append("RSS sampler: " + type(error).__name__)
                                return
                            stop.wait(args.sample_interval)
                    sampler = threading.Thread(target=sample, daemon=True)
                    result["samples"].append({"repetition": repetition, "timestamp": time.time(), **reader.read()})
                    sampler.start()
                    for mode in ("explicit-parent", "automatic-full-history", "native-compaction"):
                        stage = f"repetition-{repetition}:{mode}:provision"
                        model, route, key = provision(session, upstream.server_port, f"{repetition}-{mode}")
                        history = []
                        parent = None
                        main_chain_ids = []
                        main_chain_histories = []
                        for round_index in range(args.rounds):
                            stage = f"repetition-{repetition}:{mode}:round-{round_index}"
                            user = {"role": "user", "content": f"{mode}:{round_index}:" + padding}
                            text = "answer-" + hashlib.sha256(user["content"].encode()).hexdigest()
                            new_items = history_input(user, padding, args.history_items)
                            full = history + new_items
                            items = new_items if mode == "explicit-parent" else full
                            row, response = send(transport, base, key, model, items, upstream, {"name": mode, "round": round_index, "upstream_input": full}, parent, text, previous=parent if mode == "explicit-parent" else None)
                            row.update(repetition=repetition, route_id=route)
                            result["requests"].append(row)
                            if not row["valid"]:
                                raise MeasurementFailure("generation HTTP/content/upstream validation failed")
                            run = observe(session, route, response["id"], directory, require_trace=performance_debug)
                            row.update(observation_run_id=run["id"], observed_parent=run.get("generation_parent_id"))
                            row["parent_valid"] = "generation_parent_id" in run and row["observed_parent"] == parent
                            row["valid"] = row["valid"] and row["parent_valid"]
                            report_progress(result, row)
                            history = full + response["output"]
                            parent = response["id"]
                            main_chain_ids.append(parent)
                            main_chain_histories.append(history)
                        if mode == "native-compaction":
                            branch_parent = main_chain_ids[-3] if args.rounds >= 3 else None
                            branch_prefix = main_chain_histories[-3] if args.rounds >= 3 else []
                            for candidate in range(args.candidates):
                                stage = f"repetition-{repetition}:native-candidate-{candidate}"
                                decoy_user = {"role": "user", "content": f"native-compaction:decoy-{candidate}:" + padding}
                                decoy_input = branch_prefix + history_input(decoy_user, padding, args.history_items)
                                decoy_text = "answer-" + hashlib.sha256(decoy_user["content"].encode()).hexdigest()
                                decoy_row, decoy_response = send(transport, base, key, model, decoy_input, upstream, {"name": "native-discovery-candidate", "round": candidate, "upstream_input": decoy_input}, branch_parent, decoy_text)
                                decoy_row.update(repetition=repetition, route_id=route)
                                result["requests"].append(decoy_row)
                                if not decoy_row["valid"]:
                                    raise MeasurementFailure("native candidate generation validation failed")
                                decoy_run = observe(session, route, decoy_response["id"], directory, require_trace=performance_debug)
                                decoy_row.update(observation_run_id=decoy_run["id"], observed_parent=decoy_run.get("generation_parent_id"))
                                decoy_row["parent_valid"] = "generation_parent_id" in decoy_run and decoy_row["observed_parent"] == branch_parent
                                decoy_row["valid"] = decoy_row["valid"] and decoy_row["parent_valid"]
                                report_progress(result, decoy_row)
                            # Independent successful prefix-source case: append a new user
                            # to the penultimate generation, not the original exact window.
                            stage = f"repetition-{repetition}:native-prefix-compact-operation"
                            prefix_parent = main_chain_ids[-2]
                            prefix_user = {"role": "user", "content": "native-compaction:prefix-new-user:" + padding}
                            prefix_input = semantics(main_chain_histories[-2] + history_input(prefix_user, padding, args.history_items))
                            prefix_row, _ = send(
                                transport, base, key, model, prefix_input, upstream,
                                {"name": "native-prefix-compact-operation", "round": args.rounds,
                                 "upstream_input": prefix_input}, prefix_parent, "", compact=True,
                            )
                            prefix_row.update(repetition=repetition, route_id=route,
                                              parent_validation="successful penultimate-generation prefix plus a new user; newer same-depth decoys must be rejected")
                            result["requests"].append(prefix_row)
                            if not prefix_row["valid"]:
                                raise MeasurementFailure("native prefix compact HTTP/content validation failed")
                            prefix_run = observe(session, route, None, directory, require_trace=performance_debug)
                            prefix_row["observation_run_id"] = prefix_run["id"]
                            prefix_row["observed_parent"] = prefix_run.get("generation_parent_id")
                            prefix_row["parent_valid"] = "generation_parent_id" in prefix_run and prefix_row["observed_parent"] == prefix_parent
                            prefix_row["valid"] = prefix_row["valid"] and prefix_row["parent_valid"]
                            report_progress(result, prefix_row)
                            stage = f"repetition-{repetition}:native-compact-operation"
                            compact_input = semantics(history)
                            row, compacted = send(transport, base, key, model, compact_input, upstream, {"name": "native-compact-operation", "round": args.rounds, "upstream_input": compact_input}, parent, "", compact=True)
                            row.update(repetition=repetition, route_id=route)
                            result["requests"].append(row)
                            if not row["valid"]:
                                raise MeasurementFailure("native compact operation validation failed")
                            compact_run = observe(session, route, None, directory, exclude_runs={prefix_run["id"]}, require_trace=performance_debug)
                            row["observation_run_id"] = compact_run["id"]
                            row["observed_parent"] = compact_run.get("generation_parent_id")
                            row["parent_valid"] = "generation_parent_id" in compact_run and row["observed_parent"] == parent
                            row["valid"] = row["valid"] and row["parent_valid"]
                            report_progress(result, row)
                            user = {"role": "user", "content": "continue compacted task:" + padding}
                            full = compacted["output"] + history_input(user, padding, args.history_items)
                            text = "continued from compacted window"
                            stage = f"repetition-{repetition}:native-compacted-continuation"
                            row, response = send(transport, base, key, model, full, upstream, {"name": "native-compacted-continuation", "round": args.rounds + 1, "upstream_input": full}, parent, text)
                            row.update(repetition=repetition, route_id=route)
                            result["requests"].append(row)
                            if not row["valid"]:
                                raise MeasurementFailure("native continuation validation failed")
                            run = observe(session, route, response["id"], directory, require_trace=performance_debug)
                            row.update(observation_run_id=run["id"], observed_parent=run.get("generation_parent_id"))
                            row["parent_valid"] = "generation_parent_id" in run and row["observed_parent"] == parent
                            row["valid"] = row["valid"] and row["parent_valid"]
                            report_progress(result, row)
                            # Deliberately mismatch legal controls, not content or
                            # session identity. A projection may reject these
                            # candidates without any full-history reads.
                            stage = f"repetition-{repetition}:rejected-session-candidates"
                            rejected_row, _ = send(
                                transport, base, key, model, compact_input, upstream,
                                {"name": "rejected-session-candidates", "round": args.rounds + 2,
                                 "upstream_input": compact_input,
                                 "instructions": "history benchmark: independently compact this unchanged window"},
                                None, "", compact=True,
                            )
                            rejected_row.update(repetition=repetition, route_id=route,
                                                parent_validation="standalone control-mismatch operation; not a successful source continuation")
                            result["requests"].append(rejected_row)
                            if not rejected_row["valid"]:
                                raise MeasurementFailure("rejected session candidate compact HTTP/content validation failed")
                            rejected_run = observe(session, route, None, directory, exclude_runs={prefix_run["id"], compact_run["id"]}, require_trace=performance_debug)
                            rejected_row["observation_run_id"] = rejected_run["id"]
                            rejected_row["observed_parent"] = rejected_run.get("generation_parent_id")
                            rejected_row["parent_valid"] = "generation_parent_id" in rejected_run and rejected_row["observed_parent"] is None
                            rejected_row["valid"] = rejected_row["valid"] and rejected_row["parent_valid"]
                            report_progress(result, rejected_row)
                    def shared_workload(unprojected):
                        # Each workload owns its route, API key Principal and session.
                        suffix = "shared-ancestor-unprojected-candidates" if unprojected else "shared-ancestor"
                        model, route, key = provision(session, upstream.server_port, f"{repetition}-{suffix}")

                        def checked_request(name, index, items, expected_parent, text):
                            row, response = send(
                                transport, base, key, model, items, upstream,
                                {"name": name, "round": index, "upstream_input": items},
                                expected_parent, text,
                            )
                            row.update(repetition=repetition, route_id=route, shared_workload=suffix)
                            result["requests"].append(row)
                            if not row["valid"]:
                                raise MeasurementFailure("shared ancestor HTTP/content/upstream validation failed")
                            run = observe(session, route, response["id"], directory, require_trace=performance_debug)
                            row.update(observation_run_id=run["id"], observed_parent=run.get("generation_parent_id"))
                            row["parent_valid"] = "generation_parent_id" in run and row["observed_parent"] == expected_parent
                            row["valid"] = row["valid"] and row["parent_valid"]
                            report_progress(result, row)
                            return response

                        shared_history = []
                        ancestor = None
                        candidate_ids = []
                        for index in range(args.shared_rounds):
                            user = {"role": "user", "content": f"{suffix}:{index}:" + padding}
                            full = shared_history + history_input(user, padding, args.history_items)
                            response = checked_request(suffix, index, full, ancestor, f"shared-answer-{index}")
                            shared_history = full + response["output"]
                            ancestor = response["id"]
                        source = source_history = None
                        for index in range(args.candidates + 1):
                            user = {"role": "user", "content": f"{suffix}:branch:{index}:" + padding}
                            full = shared_history + history_input(user, padding, args.history_items)
                            response = checked_request(suffix + ("-source" if index == 0 else "-decoy"),
                                                       index, full, ancestor, f"branch-answer-{index}")
                            candidate_ids.append(response["id"])
                            if index == 0:
                                source = response["id"]
                                source_history = full + response["output"]
                        if unprojected:
                            # Derived-index fixture only, outside inference timers.
                            # Facts, ordinal, TTL and Principal remain untouched.
                            fixture = unknown_projection_fixture(args, directory, route, key, candidate_ids)
                            fixture.update(repetition=repetition, route_id=route, workload=suffix)
                            result["projection_fixtures"].append(fixture)
                        probe_user = {"role": "user", "content": suffix + ":probe:" + padding}
                        probe_input = source_history + history_input(probe_user, padding, args.history_items)
                        # Same controls and lengths, but no canonical ancestor can
                        # match the altered first message. Reject before matching
                        # so the successful continuation cannot become a decoy.
                        rejected_input = json.loads(json.dumps(probe_input))
                        rejected_input[0]["content"] = suffix + ":unmatched-root:" + padding
                        checked_request(suffix + "-reject", 0, rejected_input, None, "independent rejected-history answer")
                        checked_request(suffix + "-match", 0, probe_input, source, "matched oldest branch answer")

                    stage = f"repetition-{repetition}:shared-ancestor"
                    shared_workload(False)
                    stage = f"repetition-{repetition}:shared-ancestor-unprojected-candidates"
                    shared_workload(True)
                    stop.set()
                    sampler.join()
                    result["samples"].append({"repetition": repetition, "timestamp": time.time(), **reader.read()})
                    io_after = process_io_counters(process.pid)
                    io_delta = None if io_before is None or io_after is None else {key: io_after[key] - value for key, value in io_before.items()}
                    if io_delta is not None and any(value < 0 for value in io_delta.values()):
                        raise MeasurementFailure("server I/O counters moved backwards")
                    result["process_io_windows"].append({"repetition": repetition, "counters": io_delta})
                    stage = f"repetition-{repetition}:performance-capture"
                    print(json.dumps({"stage": stage, "completed_requests": len(result["requests"])}), flush=True)
                    capture_after = timeline(session)
                    capture = (attach_performance_snapshot(result["requests"][first_request:], capture_before, capture_after) if performance_debug
                               else attach_disabled_performance_controls(result["requests"][first_request:], capture_before, capture_after))
                    capture["repetition"] = repetition
                    result["performance_captures" if performance_debug else "debug_disabled_controls"].append(capture)
                finally:
                    transport.close()
                    stop.set()
                    if sampler:
                        sampler.join(timeout=5)
                    if reader:
                        reader.close()
                    if process:
                        stop_stravia_server(process, logs, print_tail=0)
    except Exception as error:
        message = str(error) if isinstance(error, MeasurementFailure) else "exception body omitted to protect credentials"
        result["failure_reasons"].append(f"stage={stage}: {type(error).__name__} ({message})")
    finally:
        for server in (upstream, catalog):
            server.shutdown()
            server.server_close()
        for thread in threads:
            thread.join(timeout=5)
        result["catalog_arrivals"] = catalog.arrivals
        result["rss_peak_bytes"] = max((sample["rss_bytes"] for sample in result["samples"]), default=None)
        finalize_validity(result, args)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, ensure_ascii=False, indent=2), encoding="utf-8")
    return 0 if result["valid"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
