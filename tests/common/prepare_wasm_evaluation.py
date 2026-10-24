"""Prepare private, offline Wasm evaluation fixtures from an existing SQLite snapshot.

This tool never opens the live data directory, contacts upstreams, or exports raw
content in its summary. Inputs and generated fixtures remain sensitive local data.
"""
from __future__ import annotations

import argparse
import base64
import copy
import hashlib
import json
import os
import platform
import sqlite3
from collections import Counter
from pathlib import Path
from urllib.parse import urlsplit, urlunsplit

from tests.common.protocol_history_audit import _decode, _digest, _json, _restore


def private_json(path: Path, value: object) -> None:
    path.write_text(json.dumps(value, ensure_ascii=False, separators=(",", ":")), encoding="utf-8")
    path.chmod(0o600)


def records(directory: Path):
    for path in sorted(directory.glob("*.jsonl")):
        with path.open("rb") as stream, path.open("rb") as references:
            while line := stream.readline():
                offset = stream.tell() - len(line)
                if not line.endswith(b"\n"):
                    raise ValueError("incomplete_trace_segment")
                stored = json.loads(line)
                if stored.get("trace_storage") != 1:
                    yield stored
                    continue

                def resolve(name: str):
                    if name in stored:
                        return stored[name]
                    target = stored.get(name + "_ref")
                    if type(target) is not int or not 0 <= target < offset:
                        raise ValueError("invalid_trace_reference")
                    references.seek(target)
                    previous = json.loads(references.readline())
                    if previous.get("trace_storage") != 1 or name not in previous:
                        raise ValueError("invalid_trace_reference_target")
                    return previous[name]

                result = dict(resolve("meta"))
                result.update(sequence=stored["sequence"], recorded_at=stored["recorded_at"],
                              payload=resolve("payload"))
                yield result


def wire_bytes(record: dict) -> bytes:
    value = record.get("payload")
    if value is None:
        return b""
    if isinstance(value, dict) and value.get("encoding") == "base64":
        return base64.b64decode(value["data"], validate=True)
    if not isinstance(value, str):
        raise ValueError("unsupported_wire_payload")
    if record.get("payload_encoding") == "base64":
        return base64.b64decode(value, validate=True)
    return value.encode("utf-8")


def node(connection: sqlite3.Connection, identifier: str) -> tuple[dict, dict]:
    row = connection.execute("SELECT * FROM turn_chain_nodes WHERE id=?", (identifier,)).fetchone()
    if row is None or row["kind"] != "response" or row["payload_version"] != 6 or row["storage_format"] != 2:
        raise ValueError("unsupported_history_node")
    envelope = _json(_decode(row["payload"]))
    contents = {}
    for content in connection.execute(
        "SELECT r.principal AS ref_principal,c.principal,c.content_key,c.content "
        "FROM turn_chain_node_contents r JOIN turn_chain_contents c ON c.id=r.content_id WHERE r.node_id=?",
        (identifier,),
    ):
        if content["principal"] != row["principal"] or content["ref_principal"] != row["principal"]:
            raise ValueError("history_principal_mismatch")
        raw = _decode(content["content"])
        if _digest(raw) != content["content_key"]:
            raise ValueError("history_content_digest_mismatch")
        contents[content["content_key"]] = _json(raw)
    return dict(row), _restore(envelope, contents)


def reconstruct(connection: sqlite3.Connection, run: dict, attempt: dict) -> tuple[dict, dict, int]:
    chain = []
    cursor, seen, principal = run["generation_node_id"], set(), None
    while cursor is not None:
        if cursor in seen:
            raise ValueError("history_parent_cycle")
        seen.add(cursor)
        row, payload = node(connection, cursor)
        if principal is not None and row["principal"] != principal:
            raise ValueError("history_parent_principal_mismatch")
        principal = row["principal"]
        chain.append(payload)
        if (payload.get("effective_history_mutation") or {}).get("type") == "replace":
            break
        cursor = row["parent_id"]
    items = []
    ordered = list(reversed(chain))
    for index, payload in enumerate(ordered):
        mutation = payload.get("effective_history_mutation")
        if mutation is None:
            items.extend(payload["client_delta"]["messages"])
        elif mutation["type"] == "replace":
            items = list(mutation["items"])
        elif mutation["type"] == "append":
            items.extend(mutation["items"])
        else:
            raise ValueError("unsupported_history_mutation")
        if index + 1 != len(ordered):
            items.extend(payload["effective_output"]["items"])
    final = chain[0]
    request = copy.deepcopy(final.get("effective_request") or {})
    request.update(model=attempt["upstream_model"], items=items,
                   stream={"enabled": True, "include_usage": True},
                   meta={"source_protocol": run["ingress_protocol"],
                         "vendor": {"ingress": {}, "egress": {}, "passthrough_safe": {}},
                         "media_routing": None})
    system = final.get("effective_system")
    if system is not None:
        request["instructions"] = system
    return request, final["effective_output"], len(chain)


def replay_url(value: str) -> str:
    # Keep the authority/path that replay needs; drop userinfo, query, and fragment.
    parsed = urlsplit(value if "://" in value else "https://" + value)
    host = parsed.netloc.rpartition("@")[2]
    if not host:
        return "https://replay.invalid"
    return urlunsplit((parsed.scheme, host, parsed.path, "", ""))


_SECRET_OPTION_KEYS = {
    "apikey", "accesstoken", "refreshtoken", "idtoken", "clientsecret", "privatekey",
    "subscriptionkey", "sessiontoken", "accesskey", "secretkey", "secret", "password",
    "passphrase", "token", "authorization", "bearer", "credential", "credentials",
}


def sanitize_options(value, key=""):
    # Options are ordinary settings: replace secret fields and URL credentials only,
    # preserving non-secret values such as regions and resource names.
    if isinstance(value, dict):
        return {k: sanitize_options(v, k) for k, v in value.items()}
    if isinstance(value, list):
        return [sanitize_options(v, key) for v in value]
    if isinstance(value, str):
        if key.replace("-", "").replace("_", "").lower() in _SECRET_OPTION_KEYS:
            return "benchmark-local-only"
        if "://" in value or key.lower().endswith("url"):
            parsed = urlsplit(value if "://" in value else "https://" + value)
            if parsed.netloc.rpartition("@")[0] or parsed.query or parsed.fragment:
                return replay_url(value)
    return value


def synthetic(value, key=""):
    if isinstance(value, dict):
        return {k: synthetic(v, k) for k, v in value.items()}
    if isinstance(value, list):
        return [synthetic(v, key) for v in value]
    if isinstance(value, str):
        if key in {"region", "domain", "driver_key", "scheme", "scope", "scopes", "token_type", "tier", "login_method"}:
            return value
        if key.lower().endswith("url") or key in {"resource_url", "devinApiUrl", "devinWebappHost"}:
            return replay_url(value)
        if "email" in key:
            return "benchmark@example.invalid"
        if "uuid" in key or "machine_id" in key:
            return "00000000-0000-4000-8000-000000000001"
        if key == "device_id":
            return "0" * 64
        if "expires" in key or "expire_time" in key:
            return "2099-01-01T00:00:00Z"
        if key in {"uid", "subject_id", "account_id", "organization_id"}:
            return "100000001"
        return "benchmark-local-only"
    if type(value) is int and ("id" in key or "uid" in key):
        return 100000001
    return value


def provider_fixture(connection: sqlite3.Connection, attempt: dict, descriptor: dict) -> dict:
    saved = dict(connection.execute("SELECT * FROM providers WHERE id=?", (attempt["provider_id"],)).fetchone())
    profiles = descriptor["providers"]
    profile = next((p for p in profiles if p["provider_id"] == saved["vendor"]), None)
    if profile is None and len(profiles) == 1:
        profile = profiles[0]
    if profile is None:
        profile = next((p for p in profiles if p["provider_id"] == saved["preset_key"]), None)
    if profile is None:
        raise ValueError("profile_not_found")
    base_url = replay_url(saved["base_url"])
    options = sanitize_options(json.loads(saved["vendor_options"] or "{}"))
    credentials = synthetic(json.loads(saved["adapter_credentials"] or "{}"))
    if saved["auth_mode"] == "oauth":
        oauth = connection.execute("SELECT * FROM provider_oauth_credentials WHERE provider_id=?",
                                   (saved["id"],)).fetchone()
        if oauth is not None:
            meta = synthetic(json.loads(oauth["meta"] or "{}"))
            credentials.update(meta)
            credentials["raw"] = copy.deepcopy(meta.get("raw", meta))
            credentials.update(access_token="benchmark-local-only", refresh_token="benchmark-local-only",
                               expires_at="2099-01-01T00:00:00Z", driver_key=oauth["driver_key"],
                               scheme=oauth["scheme"], scopes=json.loads(oauth["scopes"] or "[]"))
            for key in ["resource_url", "subject_id"]:
                if oauth[key]:
                    credentials[key] = synthetic(oauth[key], key)
    elif saved["api_key"]:
        credentials.setdefault("apiKey", "benchmark-local-only")
    model_row = connection.execute(
        "SELECT model_id,metadata_json FROM provider_models WHERE provider_id=? AND model_id=?",
        (saved["id"], attempt["upstream_model"])).fetchone()
    model_metadata = None
    if model_row is not None:
        extensions = json.loads(model_row["metadata_json"] or "{}")
        capabilities = set(extensions.get("capabilities") or [])
        if "image" in ((extensions.get("modalities") or {}).get("input") or []):
            capabilities.add("image_input")
        model_metadata = {"id": model_row["model_id"], "family": extensions.get("family"),
                          "selector": extensions.get("selector"), "capabilities": sorted(capabilities),
                          "extensions": extensions}
    return {"provider_id": profile["provider_id"], "channel": saved["channel"] or "default",
            "base_url": base_url, "protocol": attempt["protocol"],
            "options": options, "credentials": credentials,
            "model": attempt["upstream_model"], "model_metadata": model_metadata, "client_headers": [],
            "operation_metadata": {"egress_protocol": attempt["protocol"],
                                   "egress_base_url": base_url, "transport_preference": "http-only",
                                   "host_platform": {"Linux": "linux", "Darwin": "macos", "Windows": "windows"}[platform.system()],
                                   "session_affinity": "offline-evaluation", "transport_affinity": "offline-evaluation"}}


def prepare(args) -> dict:
    args.output.mkdir(mode=0o700, parents=True, exist_ok=True)
    args.output.chmod(0o700)
    connection = sqlite3.connect(args.database.resolve().as_uri() + "?mode=ro", uri=True)
    connection.row_factory = sqlite3.Row
    connection.execute("PRAGMA query_only=ON")
    connection.execute("BEGIN")
    manifests = {}
    for path in args.traces.glob("*/manifest.json"):
        manifest = json.loads(path.read_text())
        if manifest.get("status") == "complete" and manifest.get("run_id"):
            manifests[manifest["run_id"]] = path.parent
    selection = json.loads(args.selection.read_text())
    cases, summary, failures = [], [], []
    try:
        for index, attempt in enumerate(selection, 1):
            label = f"case-{index:02}"
            destination = args.output / label
            destination.mkdir(mode=0o700, exist_ok=True)
            try:
                run = dict(connection.execute("SELECT * FROM inference_run_observations WHERE id=?",
                                              (attempt["run_id"],)).fetchone())
                request, response, ancestor_count = reconstruct(connection, run, attempt)
                plugin = dict(connection.execute("SELECT * FROM vendor_plugins WHERE vendor_id=?",
                                                 (attempt["vendor"],)).fetchone())
                component = args.components / (plugin["digest"] + ".wasm")
                if not component.is_file() or hashlib.sha256(component.read_bytes()).hexdigest() != plugin["digest"]:
                    raise ValueError("component_digest_mismatch")
                provider = provider_fixture(connection, attempt, json.loads(plugin["descriptor"]))
                chunks, exchange, header = [], -1, None
                request_path = None
                omitted_control_frames = Counter()
                upstream_request_bytes, client_request_bytes = [], []
                captured_headers = None
                trace_directory = manifests.get(attempt["run_id"])
                trace_records = records(trace_directory) if trace_directory is not None else ()
                client_path = "/v1/responses"
                for record in trace_records:
                    direction = record.get("direction")
                    if record.get("transport") == "websocket" and record.get("message_type") in {"ping", "pong", "close"}:
                        omitted_control_frames[record["message_type"]] += 1
                        continue
                    if direction == "client_to_platform" and record.get("message_type") == "body_chunk":
                        client_request_bytes.append(wire_bytes(record))
                    if direction == "client_to_platform" and record.get("url"):
                        client_path = urlsplit(record["url"]).path
                    if record.get("attempt_id") not in (None, attempt["id"]):
                        continue
                    if direction == "upstream_request":
                        exchange += 1
                        header = None
                        request_path = urlsplit(record["url"]).path if record.get("url") else None
                        body = wire_bytes(record)
                        if body:
                            upstream_request_bytes.append(body)
                    if direction != "upstream_response":
                        continue
                    if record.get("message_type") in {"response_headers", "response_head"}:
                        captured_headers = record.get("headers") or {}
                        allowed = {"content-type", "content-encoding"}
                        headers = [[k, str(v)] for k, v in captured_headers.items() if k.lower() in allowed]
                        header = {"kind": "http", "status": record.get("status_code") or 200, "headers": headers,
                                  "exchange": max(exchange, 0), "request_path": request_path}
                        continue
                    if record.get("payload") is None:
                        continue
                    body = wire_bytes(record)
                    transport = record.get("transport") or "http"
                    kind = "http" if transport != "websocket" else (
                        "ws-binary" if record.get("payload_encoding") == "base64" else "ws-text")
                    path = destination / f"chunk-{len(chunks):05}.bin"
                    path.write_bytes(body)
                    path.chmod(0o600)
                    entry = dict(header or {"status": record.get("status_code") or 200,
                                            "headers": [["content-type", "text/event-stream"]],
                                            "exchange": max(exchange, 0), "request_path": request_path})
                    entry.update(kind=kind, file=str(path.resolve()))
                    chunks.append(entry)
                if any(chunk["kind"].startswith("ws-") for chunk in chunks):
                    provider["operation_metadata"]["transport_preference"] = "automatic"
                for name, value in [("request", request), ("response", response), ("provider", provider), ("chunks", chunks)]:
                    private_json(destination / (name + ".json"), value)
                for name, values in [("captured-upstream-request", upstream_request_bytes),
                                     ("captured-client-request", client_request_bytes)]:
                    path = destination / (name + ".bin")
                    path.write_bytes(b"".join(values))
                    path.chmod(0o600)
                interaction = connection.execute("SELECT api_key_id FROM interaction_observations WHERE id=?",
                                                 (run["interaction_id"],)).fetchone()
                private_json(destination / "http-context-private.json",
                             {"api_key_id": interaction["api_key_id"] if interaction else None,
                              "provider_id": attempt["provider_id"], "path": client_path,
                              "ingress_protocol": run["ingress_protocol"]})
                cases.append({"id": label, "vendor": attempt["vendor"], "component": str(component.resolve()),
                              **{name: str((destination / (name + ".json")).resolve())
                                 for name in ["request", "response", "provider", "chunks"]}})
                request_raw = (destination / "request.json").read_bytes()
                summary.append({"id": label, "vendor": attempt["vendor"], "input_tokens": attempt["input_tokens"],
                                "output_tokens": attempt["output_tokens"], "request_bytes": len(request_raw),
                                "response_bytes": (destination / "response.json").stat().st_size,
                                "request_sha256": hashlib.sha256(request_raw).hexdigest(),
                                "items": len(request["items"]), "tools": len(request.get("tools") or []),
                                "ancestors_decoded": ancestor_count, "chunks": len(chunks),
                                "chunk_bytes": sum(Path(c["file"]).stat().st_size for c in chunks),
                                "exchanges": len({c["exchange"] for c in chunks}),
                                "recorded_stream_available": bool(chunks),
                                "omitted_transport_control_frames": dict(omitted_control_frames),
                                "transports": sorted({c["kind"] for c in chunks})})
            except (ValueError, KeyError, TypeError) as error:
                failures.append({"id": label, "code": str(error) if isinstance(error, ValueError)
                                 and str(error).replace("_", "").isalnum() else "fixture_preparation_failed"})
    finally:
        connection.close()
    private_json(args.output.parent / "corpus.json", {"cases": cases})
    result = {"schema_version": 1, "source": "consistent-sqlite-snapshot-and-complete-wire-traces",
              "sensitive_fixtures": True, "real_credentials_replaced": True,
              "request_scope": "reconstructed-effective-history-and-controls; not byte-identical-original-ingress",
              "cases": summary, "failures": failures,
              "vendor_counts": dict(Counter(c["vendor"] for c in cases))}
    private_json(args.output.parent / "corpus-summary.json", result)
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ["database", "traces", "components", "selection", "output"]:
        parser.add_argument("--" + name, type=Path, required=True)
    args = parser.parse_args()
    os.umask(0o077)
    try:
        result = prepare(args)
    except Exception:
        print(json.dumps({"valid": False, "code": "fixture_preparation_failed"}))
        return 1
    print(json.dumps({"cases": len(result["cases"]), "vendor_counts": result["vendor_counts"],
                      "failures": result["failures"]}))
    return 1 if result["failures"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
