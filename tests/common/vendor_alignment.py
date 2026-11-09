#!/usr/bin/env python3
"""Run opt-in vendor alignment against a read-only source and an isolated Core runner.

The Core unit-test runner must already be built. No network is attempted without
--allow-live. Source credentials travel only through private subprocess pipes.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import sqlite3
import stat
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[2]
SLUG = re.compile(r"[a-zA-Z0-9][a-zA-Z0-9._-]{0,127}\Z")
MODEL_ID = re.compile(r"[a-zA-Z0-9][a-zA-Z0-9._:/@+\-]{0,255}\Z")
AI_ERROR_KINDS = {"authentication_error", "authorization_error", "not_found_error", "rate_limit_error", "quota_exceeded", "invalid_request", "server_error", "service_unavailable", "timeout", "content_filtered", "context_length_exceeded", "model_not_available", "stream_mid_error", "unexpected_eof", "unknown"}
OPERATIONS = ("infer", "media-image", "discover", "allowance")
STATUSES = {"ok", "invalid_input", "setup_failed", "execution_failed", "invalid_output", "unauthorized", "image_model_unavailable"}
PROVIDER_COLUMNS = ("vendor", "channel", "protocol", "base_url", "models_source", "static_models", "api_key", "adapter_credentials", "vendor_options", "auth_mode", "use_proxy")
OAUTH_COLUMNS = ("driver_key", "scheme", "access_token", "expires_at", "resource_url", "subject_id", "scopes", "meta")


def read_source(data_root: Path, plugin_ids: list[str], live: bool) -> dict:
    """Read one WAL-consistent snapshot, without copying or migrating the source."""
    db = (data_root / "db" / "gateway.db").resolve()
    connection = sqlite3.connect(db.as_uri() + "?mode=ro", uri=True)
    connection.row_factory = sqlite3.Row
    try:
        connection.execute("PRAGMA query_only=ON")
        connection.execute("BEGIN")
        descriptors = []
        for row in connection.execute("SELECT vendor_id,descriptor FROM vendor_plugins"):
            try:
                descriptor = json.loads(row["descriptor"])
            except (ValueError, TypeError):
                continue
            profiles = descriptor.get("providers", []) if isinstance(descriptor, dict) else []
            descriptors.append({"plugin_id": row["vendor_id"], "profiles": [p.get("provider_id") for p in profiles if isinstance(p, dict)]})
        selected = set(plugin_ids)
        selected.update(p for entry in descriptors if entry["plugin_id"] in selected for p in entry["profiles"] if isinstance(p, str))
        if not selected:
            return {"providers": [], "descriptors": descriptors}
        columns = PROVIDER_COLUMNS if live else ("vendor", "channel")
        placeholders = ",".join("?" for _ in selected)
        providers = []
        query = "SELECT id," + ",".join(columns) + " FROM providers WHERE is_enabled=1 AND vendor IN (" + placeholders + ") ORDER BY id"
        for index, row in enumerate(connection.execute(query, tuple(selected)), start=1):
            item = dict(row)
            provider_id = item.pop("id")
            item_oauth = None
            models = []
            if live:
                oauth = connection.execute(
                    "SELECT " + ",".join(OAUTH_COLUMNS) + ",status FROM provider_oauth_credentials WHERE provider_id=?", (provider_id,)
                ).fetchone()
                item_oauth = dict(oauth) if oauth else None
                if item_oauth and item_oauth.pop("status") != "connected":
                    item_oauth = None
                models = [dict(model) for model in connection.execute(
                    "SELECT model_id,metadata_json FROM provider_models WHERE provider_id=? AND presence='present' AND selection_policy!='force_disabled' ORDER BY model_id", (provider_id,)
                )]
            if live:
                item["use_proxy"] = bool(item["use_proxy"])
            providers.append(without_refresh_tokens({"provider": item, "oauth": item_oauth, "models": models, "connection_index": index}))
        proxy = None
        if live and any(item["provider"]["use_proxy"] for item in providers):
            setting = connection.execute("SELECT value FROM settings WHERE name='outbound_proxy'").fetchone()
            proxy = setting["value"] if setting else None
        return {"providers": providers, "descriptors": descriptors, "outbound_proxy": proxy}
    finally:
        connection.close()


def source_snapshot(data_root: Path, plugin_ids: list[str], live: bool, sudo_reader: bool) -> dict:
    if not sudo_reader:
        return read_source(data_root, plugin_ids, live)
    # Only the read-only snapshot helper runs with elevated privileges. Its
    # stdout is an anonymous pipe, never inherited by the terminal or runner.
    result = subprocess.run(["sudo", "-n", sys.executable, str(Path(__file__).resolve()), "--_reader", str(data_root), "live" if live else "inventory", *plugin_ids], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, check=True)
    return json.loads(result.stdout)


def components(component: Path | None, vendors: list[str]) -> dict[str, Path]:
    if component is not None:
        if len(vendors) != 1:
            raise ValueError("component_requires_one_vendor")
        return {vendors[0]: component.resolve(strict=True)}
    manifest = ROOT / "target" / "vendor-plugins-all" / "manifest.json"
    entries = json.loads(manifest.read_text(encoding="utf-8"))
    result = {}
    for entry in entries:
        vendor, name, digest = entry["vendor_id"], entry["file"], entry["sha256"]
        if not SLUG.fullmatch(vendor) or Path(name).name != name or not re.fullmatch(r"[0-9a-f]{64}", digest):
            raise ValueError("invalid_manifest")
        artifact = manifest.parent / name
        if hashlib.sha256(artifact.read_bytes()).hexdigest() != digest:
            raise ValueError("component_digest_mismatch")
        result[vendor] = artifact.resolve()
    if not result or (vendors and not set(vendors).issubset(result)):
        raise ValueError("component_missing")
    return {key: value for key, value in result.items() if not vendors or key in vendors}


def safe_slug(value: object) -> str:
    return value if isinstance(value, str) and SLUG.fullmatch(value) else "unknown"


def without_refresh_tokens(value: object) -> object:
    """Strip refresh material even when source metadata is JSON encoded."""
    if isinstance(value, dict):
        return {key: without_refresh_tokens(item) for key, item in value.items()
                if re.sub(r"[^a-z0-9]", "", key.lower()) != "refreshtoken"}
    if isinstance(value, list):
        return [without_refresh_tokens(item) for item in value]
    if isinstance(value, str):
        try:
            parsed = json.loads(value)
        except ValueError:
            return value
        if isinstance(parsed, (dict, list)):
            return json.dumps(without_refresh_tokens(parsed), separators=(",", ":"))
    return value


RESPONSE_CATEGORIES = {"unknown", "model", "authentication", "permission", "quota", "token_validation", "session", "unsupported", "login_expired", "duplicate_request", "quota_or_billing", "content_policy", "custom_model_service", "custom_model_auth", "custom_model_unavailable", "queued", "input_limits", "rate_limit"}


def load_response_code_map(path: Path) -> dict:
    with path.open("rb") as source:
        data = source.read(16 * 1024 + 1)
    if len(data) > 16 * 1024:
        raise ValueError("invalid_response_code_map")
    try:
        mapping = json.loads(data)
        if type(mapping) is not dict or set(mapping) != {"fields", "codes"}:
            raise ValueError
        fields, codes = mapping["fields"], mapping["codes"]
        if (type(fields) is not list or len(fields) > 4
                or any(type(field) is not str or field not in {"code", "type", "status", "error"} for field in fields)
                or len(set(fields)) != len(fields) or type(codes) is not dict or len(codes) > 64):
            raise ValueError
        for code, category in codes.items():
            if (not re.fullmatch(r"(?:0|-[1-9][0-9]*|[1-9][0-9]*)", code)
                    or len(code) > 20 or not -(2**63) <= int(code) < 2**63
                    or type(category) is not str or category not in RESPONSE_CATEGORIES):
                raise ValueError
        return mapping
    except (ValueError, TypeError, UnicodeDecodeError):
        raise ValueError("invalid_response_code_map") from None


def safe_response_facts(facts: object) -> bool:
    if facts is None:
        return True
    if not isinstance(facts, dict) or set(facts) != {"content_coding", "body_kind", "html", "keys", "error_kind", "category", "truncated"}:
        return False
    keys = facts["keys"]
    return (facts["content_coding"] in {"identity", "gzip", "deflate", "br", "zstd", "unknown"}
            and facts["body_kind"] in {"empty", "json_object", "json_other", "non_json", "unknown"}
            and type(facts["html"]) is bool and type(facts["truncated"]) is bool
            and facts["error_kind"] in {"absent", "null", "boolean", "number", "string", "array", "object", "unknown"}
            and facts["category"] in RESPONSE_CATEGORIES
            and isinstance(keys, dict) and set(keys) == {"error", "message", "code", "detail", "errors", "type", "status", "data"}
            and all(type(flag) is bool for flag in keys.values()))


def safe_http_observation(observation: object) -> bool:
    if observation is None:
        return True
    return (isinstance(observation, dict)
            and set(observation) == {"response_count", "last_status", "subscription_exhausted"}
            and type(observation["response_count"]) is int
            and observation["response_count"] >= 0
            and type(observation["last_status"]) is int
            and 100 <= observation["last_status"] <= 599
            and type(observation["subscription_exhausted"]) is bool)


def run_core(binary: Path, payload: dict) -> dict:
    read_fd, write_fd = os.pipe()
    temporary = None
    try:
        temporary = tempfile.TemporaryDirectory(prefix="stravia-alignment-")
        env = os.environ.copy()
        env["TMPDIR"] = temporary.name
        env["STRAVIA_ALIGNMENT_REPORT_FD"] = str(write_fd)
        # No inherited dev manifest or tracing configuration may redirect the
        # isolated run or expose sensitive diagnostic output.
        env.pop("STRAVIA_DEV_VENDOR_MANIFEST", None)
        env["RUST_LOG"] = "off"
        proc = subprocess.Popen(
            [str(binary), "--ignored", "--exact", "plugin::execution::alignment_tests::live_alignment", "--nocapture", "--test-threads=1"],
            stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            env=env, pass_fds=(write_fd,),
        )
        os.close(write_fd)
        write_fd = -1
        try:
            proc.communicate(json.dumps(payload, separators=(",", ":")).encode(), timeout=180)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.communicate()
            return {"status": "timeout", "count": 0}
        with os.fdopen(read_fd, "rb") as report:
            read_fd = -1
            data = report.read(4097)
        if proc.returncode != 0 or len(data) > 4096:
            return {"status": "runner_failed", "count": 0}
        result = json.loads(data)
        if (not isinstance(result, dict) or set(result) != {"status", "count", "stage", "error_kind", "upstream_status", "operation_supported", "diagnostic_facts", "selected_model_present", "model_selection_origin", "response_facts", "model_error_kind", "retry_after_ms", "http_observation"}
                or (result["model_error_kind"] is not None and (type(result["model_error_kind"]) is not str or result["model_error_kind"] not in AI_ERROR_KINDS))
                or (result["retry_after_ms"] is not None and (type(result["retry_after_ms"]) is not int or not 0 <= result["retry_after_ms"] <= 2**64 - 1))
                or not safe_http_observation(result["http_observation"])
                or result["model_selection_origin"] not in {None, "explicit", "discovery"}
                or not safe_response_facts(result["response_facts"])
                or not isinstance(result["diagnostic_facts"], dict)
                or set(result["diagnostic_facts"]) != {"codex_history_boundaries_unavailable", "model_not_found", "output_token_parameter", "validation", "quota"}
                or any(type(fact) is not bool for fact in result["diagnostic_facts"].values())
                or (result["selected_model_present"] is not None and type(result["selected_model_present"]) is not bool)
                or result["status"] not in STATUSES or type(result["count"]) is not int
                or not 0 <= result["count"] <= 1000000
                or result["stage"] not in {"prepare", "gateway_init", "background_stop", "proxy_setting", "plugin_read", "plugin_preview", "plugin_confirm", "provider_create", "oauth_store", "model_create", "principal_create", "principal_auth", "chain_begin", "prompt_admission", "plugin_acquire", "connection_snapshot", "provider_snapshot", "discover_image_model", "execute", "decode"}
                or result["error_kind"] not in {None, "invalid_input", "setup", "unsupported", "upstream", "auth", "invalid_response", "cancelled", "deadline", "resource_exhausted", "trapped", "invalid_output", "image_model_unavailable", "other"}
                or (result["upstream_status"] is not None and (type(result["upstream_status"]) is not int or not 100 <= result["upstream_status"] <= 599))
                or (result["operation_supported"] is not None and type(result["operation_supported"]) is not bool)
                or (result["status"] == "ok" and result["count"] == 0)):
            return {"status": "runner_failed", "count": 0}
        return result
    except (OSError, ValueError, TypeError):
        return {"status": "runner_failed", "count": 0}
    finally:
        if write_fd >= 0:
            os.close(write_fd)
        if read_fd >= 0:
            os.close(read_fd)
        if temporary is not None:
            temporary.cleanup()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--data-root", type=Path, required=True)
    parser.add_argument("--vendor", action="append", default=[])
    parser.add_argument("--channel", action="append", default=[], help="restrict to matching source channels")
    parser.add_argument("--component", type=Path)
    parser.add_argument("--scenario", action="append", choices=OPERATIONS)
    parser.add_argument("--model")
    parser.add_argument("--allow-live", action="store_true")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--runner", type=Path, default=os.environ.get("STRAVIA_ALIGNMENT_RUNNER"), help="previously built stravia_core unit-test executable")
    parser.add_argument("--response-code-map", type=Path, help="bounded JSON diagnostic response code mapping")
    parser.add_argument("--sudo-reader", action="store_true", help="use sudo -n only for the read-only SQLite snapshot")
    args = parser.parse_args()
    if any(not SLUG.fullmatch(v) for v in args.vendor):
        parser.error("invalid vendor identifier")
    if any(not SLUG.fullmatch(channel) for channel in args.channel):
        parser.error("invalid channel identifier")
    if args.model is not None and not MODEL_ID.fullmatch(args.model):
        parser.error("invalid model identifier")
    if args.output:
        output = args.output.resolve()
        if output.is_relative_to(ROOT) or output.is_relative_to(args.data_root.resolve()):
            parser.error("output must be outside the source checkout and data root")
    report = {"mode": "live" if args.allow_live else "inventory", "results": [],
              "request_alignment": "not_compared", "boundary": "no_official_client_success_claimed",
              "source_context": "fresh_isolated_single_prompt_no_existing_conversation"}
    try:
        response_code_map = load_response_code_map(args.response_code_map) if args.response_code_map else None
        artifacts = components(args.component, args.vendor)
        snapshot = source_snapshot(args.data_root, list(artifacts), args.allow_live, args.sudo_reader)
        ownership = {p: entry["plugin_id"] for entry in snapshot["descriptors"] for p in entry["profiles"] if isinstance(p, str)}
        operations = args.scenario or ["infer"]
        if args.allow_live and (not args.runner or not Path(args.runner).is_file()):
            raise ValueError("runner_missing")
        for plugin_id, artifact in artifacts.items():
            component_digest = hashlib.sha256(artifact.read_bytes()).hexdigest()
            records = [item for item in snapshot["providers"] if item["provider"]["vendor"] == plugin_id or ownership.get(item["provider"]["vendor"]) == plugin_id]
            if not records and plugin_id == "opencode-free" and plugin_id in args.vendor:
                # Its public free-tier discovery requires no saved connection.
                # Inference remains blocked until the runner verifies a model
                # against a freshly discovered free-model list.
                records = [{"provider": {"vendor": plugin_id, "channel": "default",
                                         "protocol": "openai-compatible", "base_url": "https://opencode.ai/inference/v1",
                                         "models_source": None, "static_models": None, "api_key": "",
                                         "adapter_credentials": "{}", "vendor_options": "{}", "auth_mode": "apikey", "use_proxy": False},
                            "oauth": None, "models": [], "anonymous": True}]
            if args.channel:
                records = [record for record in records if record["provider"]["channel"] in args.channel]
            if not records:
                report["results"].append({"vendor": plugin_id, "component_sha256": component_digest,
                                          "connection_index": None, "model": args.model, "metadata_matched": False,
                                          "status": "channel_not_found" if args.channel else "missing_credentials", "count": 0})
                continue
            for record in records:
                provider = record["provider"]
                identity = {"vendor": plugin_id, "channel": safe_slug(provider["channel"]), "component_sha256": component_digest,
                            "connection_index": record.get("connection_index")}
                for operation in operations:
                    models = record["models"]
                    selected = (next((m for m in models if m["model_id"] == args.model), None) if args.model
                                else None if operation == "media-image" else next(iter(models), None))
                    model = args.model or (selected["model_id"] if selected else None)
                    metadata = json.loads(selected["metadata_json"]) if selected else None
                    valid_model = model is None or (isinstance(model, str) and MODEL_ID.fullmatch(model) is not None)
                    metadata_matched = (model is not None and isinstance(metadata, dict)
                                        and metadata.get("id") == model)
                    result = {**identity, "operation": operation, "model": model if valid_model else None,
                              "metadata_matched": False, "status": "inventory", "count": 0}
                    if not valid_model:
                        result["status"] = "invalid_model_id"
                    elif args.allow_live:
                        if record.get("anonymous") and (operation not in ("infer", "discover") or (operation == "infer" and not args.model)):
                            result["status"] = "free_model_not_verified"
                        elif provider["auth_mode"] == "oauth" and not record["oauth"]:
                            result["status"] = "missing_credentials"
                        else:
                            payload = {"wasm_path": str(artifact), "plugin_id": plugin_id, "provider": provider,
                                       "oauth": record["oauth"], "outbound_proxy": snapshot.get("outbound_proxy") if provider.get("use_proxy") else None,
                                       "model": model,
                                       "model_metadata": metadata, "operation": operation, "input": {},
                                       "response_code_map": response_code_map}
                            result["metadata_matched"] = metadata_matched
                            result.update(run_core(Path(args.runner).resolve(), without_refresh_tokens(payload)))
                    report["results"].append(result)
        status = 0 if (report["results"] and all(
            r["status"] != "channel_not_found" and r["status"] != "invalid_model_id"
            and (not args.allow_live or r["status"] == "ok") for r in report["results"])) else 1
    except ValueError as error:
        code = str(error)
        allowed = {"component_requires_one_vendor", "invalid_manifest", "component_digest_mismatch", "component_missing", "runner_missing"}
        report["results"].append({"status": code if code in allowed else "input_or_runner_error", "count": 0,
                                  "model": args.model, "metadata_matched": False, "connection_index": None})
        status = 1
    except (OSError, KeyError, TypeError, sqlite3.Error, subprocess.SubprocessError):
        report["results"].append({"status": "input_or_runner_error", "count": 0,
                                  "model": args.model, "metadata_matched": False, "connection_index": None})
        status = 1
    encoded = json.dumps(report, ensure_ascii=True, indent=2) + "\n"
    if args.output:
        args.output.write_text(encoded, encoding="utf-8")
    else:
        sys.stdout.write(encoded)
    return status


if __name__ == "__main__":
    if len(sys.argv) >= 4 and sys.argv[1] == "--_reader" and sys.argv[3] in ("live", "inventory"):
        if not stat.S_ISFIFO(os.fstat(sys.stdout.fileno()).st_mode):
            sys.exit(1)
        try:
            json.dump(read_source(Path(sys.argv[2]), sys.argv[4:], sys.argv[3] == "live"), sys.stdout, separators=(",", ":"))
        except (OSError, sqlite3.Error, ValueError):
            sys.exit(1)
    else:
        sys.exit(main())
