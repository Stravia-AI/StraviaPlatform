"""Read-only, snapshot-consistent verification of persisted benchmark generations.

Only fixed failure codes, identifiers, hashes and counts leave this module; decoded
history and database connection details never become report data.
"""
from __future__ import annotations

import copy
import hashlib
import ipaddress
import json
import re
import sqlite3
import time
from pathlib import Path
from urllib.parse import urlsplit

import zstandard


# ADR-0089 共享正文存储：SHA-256(域 || 格式字节 || 原始正文)，格式 0 = 文本、1 = 紧凑 JSON。
CONTENT_DIGEST_DOMAIN = b"stravia.content.v1\0"
TEXT_FORMAT, JSON_FORMAT = 0, 1
# ADR-0076 冻结基线（aecfc21e）的 turn_contents：仅 JSON 正文，域前缀不含格式字节。
LEGACY_CONTENT_DIGEST_DOMAIN = b"stravia.turn-content.v1\0"
NORMALIZED_HISTORY = "content-store-v1"
LEGACY_HISTORY = "legacy-adr0076-turn-contents"


class InvalidHistory(ValueError):
    pass


def _require(condition, code):
    if not condition:
        raise InvalidHistory(code)


def content_digest(format_code, raw):
    """Full SHA-256 over the fixed domain prefix, one format byte and original bytes."""
    _require(format_code in (TEXT_FORMAT, JSON_FORMAT), "content_format")
    digest = hashlib.sha256(CONTENT_DIGEST_DOMAIN)
    digest.update(bytes((format_code,)))
    digest.update(raw)
    return digest.digest()


def legacy_content_digest(raw):
    """Frozen ADR-0076 digest: fixed domain prefix and original JSON bytes."""
    digest = hashlib.sha256(LEGACY_CONTENT_DIGEST_DOMAIN)
    digest.update(raw)
    return digest.digest()


def history_storage_schema(tables):
    """Select the persisted history layout by table presence.

    The shared content store is authoritative whenever it exists. The legacy
    branch is a measurement adapter for the frozen pre-ADR-0089 baseline binary,
    never a runtime fallback.
    """
    if "contents" in tables and "content_scopes" in tables:
        return NORMALIZED_HISTORY
    if "turn_contents" in tables and "turn_scopes" in tables:
        return LEGACY_HISTORY
    raise InvalidHistory("history_schema")


def _decode(encoded):
    encoded = bytes(encoded)
    _require(len(encoded) >= 14, "codec_truncated")
    body, trailer = encoded[:-14], encoded[-14:]
    _require(trailer[13] == 1, "codec_version")
    _require(trailer[1:5] == b"\0" * 4, "codec_dictionary")
    size = int.from_bytes(trailer[5:13], "little")
    if trailer[0] == 0:
        raw = body
    elif trailer[0] == 1:
        _require(zstandard.frame_content_size(body) == size, "codec_frame_length")
        decoder = zstandard.ZstdDecompressor().decompressobj()
        raw = decoder.decompress(body)
        _require(decoder.eof and not decoder.unused_data, "codec_frame_boundary")
    else:
        raise InvalidHistory("codec_kind")
    _require(len(raw) == size, "codec_length")
    return raw


def _json(raw):
    def unique(pairs):
        result = {}
        for key, value in pairs:
            _require(key not in result, "json_duplicate_key")
            result[key] = value
        return result
    try:
        return json.loads(raw, object_pairs_hook=unique,
                          parse_constant=lambda _: (_ for _ in ()).throw(InvalidHistory("json_constant")))
    except (json.JSONDecodeError, UnicodeDecodeError) as error:
        raise InvalidHistory("json_invalid") from error


def _restore(descriptor, contents):
    """Restore an ADR-0089 descriptor from ordinal-ordered decoded content units.

    Slot wire forms: ``[pointer, ordinal]`` JSON unit, ``[pointer, ordinal, "t"]``
    text unit, ``[pointer, ordinal, "t", prefix, suffix]`` prefix + text + suffix.
    Text units are decoded UTF-8 strings; JSON units are parsed compact JSON.
    """
    _require(isinstance(descriptor, dict) and set(descriptor) == {"data", "slots"}, "envelope_shape")
    slots = descriptor["slots"]
    _require(isinstance(slots, list), "slots_shape")
    _require(isinstance(contents, list), "contents_shape")
    data = descriptor["data"]
    seen, used = set(), set()
    for slot in slots:
        _require(isinstance(slot, list) and len(slot) in (2, 3, 5), "slot_shape")
        pointer, ordinal = slot[0], slot[1]
        _require(isinstance(pointer, str) and pointer.startswith("/") and pointer not in seen, "slot_pointer")
        _require(type(ordinal) is int and 0 <= ordinal < len(contents), "slot_index")
        if len(slot) > 2:
            _require(slot[2] == "t", "slot_kind")
        if len(slot) == 5:
            _require(isinstance(slot[3], str) and isinstance(slot[4], str), "slot_concat")
        seen.add(pointer)
        used.add(ordinal)
        value = copy.deepcopy(contents[ordinal])
        if len(slot) > 2:
            _require(isinstance(value, str), "slot_not_text")
        if len(slot) == 5:
            value = slot[3] + value + slot[4]
        tokens = pointer[1:].split("/")
        for token in tokens:
            _require(not any(token[i] == "~" and (i + 1 == len(token) or token[i + 1] not in "01") for i in range(len(token))), "slot_escape")
        tokens = [t.replace("~1", "/").replace("~0", "~") for t in tokens]
        parent = data
        for token in tokens[:-1]:
            parent = _pointer_child(parent, token)
        token = tokens[-1]
        _require(_pointer_child(parent, token) is None, "slot_target")
        if isinstance(parent, list):
            parent[int(token)] = value
        else:
            parent[token] = value
    _require(used == set(range(len(contents))), "unused_content")
    return data


def _legacy_restore(descriptor, contents):
    """Restore an ADR-0076 descriptor (JSON slots only) from ordinal-ordered decoded content values."""
    _require(isinstance(descriptor, dict) and set(descriptor) == {"data", "slots"}, "envelope_shape")
    slots = descriptor["slots"]
    _require(isinstance(slots, list), "slots_shape")
    _require(isinstance(contents, list), "contents_shape")
    data = descriptor["data"]
    seen, used = set(), set()
    for slot in slots:
        _require(isinstance(slot, list) and len(slot) == 2, "slot_shape")
        pointer, ordinal = slot
        _require(isinstance(pointer, str) and pointer.startswith("/") and pointer not in seen, "slot_pointer")
        _require(type(ordinal) is int and 0 <= ordinal < len(contents), "slot_index")
        seen.add(pointer)
        used.add(ordinal)
        tokens = pointer[1:].split("/")
        for token in tokens:
            _require(not any(token[i] == "~" and (i + 1 == len(token) or token[i + 1] not in "01") for i in range(len(token))), "slot_escape")
        tokens = [t.replace("~1", "/").replace("~0", "~") for t in tokens]
        parent = data
        for token in tokens[:-1]:
            parent = _pointer_child(parent, token)
        token = tokens[-1]
        _require(_pointer_child(parent, token) is None, "slot_target")
        if isinstance(parent, list):
            parent[int(token)] = copy.deepcopy(contents[ordinal])
        else:
            parent[token] = copy.deepcopy(contents[ordinal])
    _require(used == set(range(len(contents))), "unused_content")
    return data


def _pointer_child(value, token):
    if isinstance(value, list):
        _require(token.isascii() and token.isdigit() and (token == "0" or not token.startswith("0")), "slot_array_index")
        index = int(token)
        _require(index < len(value), "slot_missing")
        return value[index]
    _require(isinstance(value, dict) and token in value, "slot_missing")
    return value[token]


def _output(payload):
    _require(isinstance(payload, dict), "payload_shape")
    required = {"client_delta", "effective_system", "effective_output", "trusted_media_turn_ids", "upstream_response_id", "effective_state", "effective_request"}
    _require(required <= set(payload), "payload_fields")
    _require(isinstance(payload["client_delta"], dict) and isinstance(payload["effective_output"], dict) and isinstance(payload["effective_state"], dict), "payload_fields")
    delta, response, state = payload["client_delta"], payload["effective_output"], payload["effective_state"]
    _require(isinstance(delta.get("messages"), list), "payload_delta")
    _require(isinstance(response.get("id"), str) and isinstance(response.get("model"), str)
             and isinstance(response.get("items"), list) and isinstance(response.get("usage"), dict)
             and isinstance(response.get("vendor"), dict), "payload_response")
    state_strings = ("context_fingerprint", "model", "provider_model", "selected_target_key",
                     "canonical_controls_fingerprint", "target_namespace", "protocol")
    _require(all(isinstance(state.get(key), str) for key in state_strings)
             and type(state.get("context_messages")) is int
             and state["context_messages"] >= 0, "payload_state")
    _require(isinstance(payload["trusted_media_turn_ids"], list)
             and all(isinstance(value, str) for value in payload["trusted_media_turn_ids"]), "payload_media")
    # Current writes always persist the protocol-specific client projection.
    # Refuse legacy/missing projection rather than guessing from vendor JSON.
    items = payload.get("client_output")
    _require(isinstance(items, list), "client_output_missing")
    text = []
    for item in items:
        _require(isinstance(item, dict) and isinstance(item.get("role"), str), "output_item")
        content = item.get("content")
        if isinstance(content, str):
            text.append(content)
        else:
            _require(isinstance(content, list), "output_content")
            for block in content:
                _require(isinstance(block, dict) and isinstance(block.get("type"), str), "output_block")
                if block["type"] == "text":
                    _require(isinstance(block.get("text"), str), "output_text")
                    text.append(block["text"])
    return hashlib.sha256("".join(text).encode()).hexdigest()


class _SqlReader:
    def __init__(self, connection, placeholder):
        self.connection, self.placeholder = connection, placeholder

    def rows(self, query, values):
        return self.connection.execute(query.replace("?", self.placeholder), values).fetchall()


class _NodeReader(_SqlReader):
    """Node identity, parent and output verification shared by both layouts."""

    scope_table = None

    def node(self, identifier):
        rows = self.rows(
            "SELECT n.node_pk,n.id,n.parent_pk,parent.id,parent.scope_id,parent.kind,"
            "n.kind,n.scope_id,s.principal,n.payload_version,p.payload "
            f"FROM turn_nodes n JOIN {self.scope_table} s ON s.id=n.scope_id "
            "LEFT JOIN turn_nodes parent ON parent.node_pk=n.parent_pk "
            "LEFT JOIN turn_node_payloads p ON p.node_pk=n.node_pk WHERE n.id=?",
            (identifier,),
        )
        _require(len(rows) == 1, "node_missing")
        (node_pk, node_id, parent_pk, parent_id, parent_scope, parent_kind,
         kind, scope_id, principal, payload_version, payload) = rows[0]
        _require(node_id == identifier and isinstance(principal, str) and bool(principal), "node_identity")
        if parent_pk is not None:
            _require(parent_id is not None, "parent_missing")
            _require(parent_scope == scope_id and parent_kind == kind, "parent_scope")
        _require(payload is not None, "payload_missing")
        return {"node_pk": node_pk, "id": node_id, "parent_id": parent_id,
                "kind": kind, "scope_id": scope_id, "principal": principal,
                "payload_version": payload_version, "payload": payload}

    def identity(self, node):
        return node["parent_id"], node["principal"]

    def verify(self, identifier, principal):
        node = self.node(identifier)
        _require(node["id"] == identifier and node["principal"] == principal
                 and node["kind"] == "response", "node_identity")
        _require(node["payload_version"] == 6, "payload_version")
        payload, count, slots = self.restore(node)
        return node["parent_id"], _output(payload), count, slots


def _unit(format_code, raw):
    """Decode one verified content unit: text -> str, JSON -> parsed value."""
    if format_code == TEXT_FORMAT:
        try:
            return raw.decode("utf-8")
        except UnicodeDecodeError as error:
            raise InvalidHistory("text_invalid") from error
    return _json(raw)


def verified_unit(row):
    """Verify one ``contents`` row (format, raw_len, digest, codec) and return its value.

    row = (digest, format, raw_len, body). Shared by History and Observation proofs.
    """
    digest, format_code, raw_len, encoded = row
    _require(isinstance(digest, (bytes, bytearray, memoryview)) and len(digest) == 32, "content_digest")
    _require(format_code in (TEXT_FORMAT, JSON_FORMAT), "content_format")
    raw = _decode(encoded)
    _require(type(raw_len) is int and len(raw) == raw_len, "content_raw_len")
    _require(content_digest(format_code, raw) == bytes(digest), "content_fingerprint")
    return _unit(format_code, raw)


class HistoryReader(_NodeReader):
    """Strict ADR-0089 shared-content reader for audit evidence and fixture export."""

    scope_table = "content_scopes"

    def restore(self, node):
        """Decode one descriptor and validate its complete ordinal/content set."""
        rows = self.rows(
            "SELECT c.ordinal,c.content_id,c.scope_id,t.scope_id,t.digest,t.format,t.raw_len,t.body "
            "FROM turn_node_contents c "
            "JOIN turn_nodes n ON n.node_pk=c.node_pk "
            "JOIN content_scopes s ON s.id=n.scope_id "
            "LEFT JOIN contents t ON t.id=c.content_id "
            "WHERE c.node_pk=? AND s.principal=? ORDER BY c.ordinal",
            (node["node_pk"], node["principal"]),
        )
        descriptor = _json(_decode(node["payload"]))
        contents, content_ids = [], set()
        for expected, (ordinal, content_id, mapping_scope, content_scope, *unit) in enumerate(rows):
            _require(ordinal == expected, "ordinal_sequence")
            _require(mapping_scope == node["scope_id"], "reference_scope")
            _require(content_scope is not None and unit[3] is not None, "reference_missing")
            _require(content_scope == node["scope_id"], "reference_scope")
            _require(content_id not in content_ids, "reference_duplicate")
            content_ids.add(content_id)
            contents.append(verified_unit(unit))
        data = _restore(descriptor, contents)
        return data, len(rows), len(descriptor["slots"])


class _LegacyHistoryReader(_NodeReader):
    """Read-only adapter for the frozen pre-ADR-0089 ``turn_contents`` layout (baseline binary only)."""

    scope_table = "turn_scopes"

    def restore(self, node):
        rows = self.rows(
            "SELECT c.ordinal,c.content_id,c.scope_id,t.scope_id,t.digest,t.content "
            "FROM turn_node_contents c "
            "JOIN turn_nodes n ON n.node_pk=c.node_pk "
            "JOIN turn_scopes s ON s.id=n.scope_id "
            "LEFT JOIN turn_contents t ON t.id=c.content_id "
            "WHERE c.node_pk=? AND s.principal=? ORDER BY c.ordinal",
            (node["node_pk"], node["principal"]),
        )
        descriptor = _json(_decode(node["payload"]))
        contents, content_ids = [], set()
        for expected, (ordinal, content_id, mapping_scope, content_scope, digest, encoded) in enumerate(rows):
            _require(ordinal == expected, "ordinal_sequence")
            _require(mapping_scope == node["scope_id"], "reference_scope")
            _require(content_scope is not None and encoded is not None, "reference_missing")
            _require(content_scope == node["scope_id"], "reference_scope")
            _require(content_id not in content_ids, "reference_duplicate")
            content_ids.add(content_id)
            _require(isinstance(digest, (bytes, bytearray, memoryview)) and len(digest) == 32,
                     "content_digest")
            raw = _decode(encoded)
            _require(legacy_content_digest(raw) == bytes(digest), "content_fingerprint")
            contents.append(_json(raw))
        data = _legacy_restore(descriptor, contents)
        return data, len(rows), len(descriptor["slots"])


def _history_tables(connection, placeholder):
    query = ("SELECT name FROM sqlite_master WHERE type='table'" if placeholder == "?"
             else "SELECT table_name FROM information_schema.tables WHERE table_schema=current_schema()")
    return {row[0] for row in connection.execute(query).fetchall()}


def _table_columns(connection, placeholder, table):
    if placeholder == "?":
        return {row[1] for row in connection.execute(f'PRAGMA table_info("{table}")').fetchall()}
    return {row[0] for row in connection.execute(
        "SELECT column_name FROM information_schema.columns WHERE table_schema=current_schema() AND table_name=%s",
        (table,)).fetchall()}


def _history_reader(connection, placeholder, allow_legacy=False):
    tables = _history_tables(connection, placeholder)
    schema = history_storage_schema(tables)
    if schema == LEGACY_HISTORY and not allow_legacy:
        raise InvalidHistory("history_schema")
    reader = HistoryReader if schema == NORMALIZED_HISTORY else _LegacyHistoryReader
    return reader(connection, placeholder)


def _connect(args, directory):
    url = getattr(args, "database_url", None)
    if not url:
        path = (Path(directory) / "db" / "gateway.db").resolve()
        connection = sqlite3.connect(path.as_uri() + "?mode=ro", uri=True)
        connection.execute("PRAGMA query_only=ON")
        connection.execute("BEGIN")
        return connection, "?", "sqlite"
    parsed = urlsplit(url)
    _require(parsed.scheme in {"postgres", "postgresql"} and parsed.hostname is not None, "database_endpoint")
    host = parsed.hostname
    _require(host == "localhost" or ipaddress.ip_address(host).is_loopback, "database_endpoint")
    # URL options can override host/service routing; benchmark URLs have none.
    _require(not parsed.query and not parsed.fragment, "database_options")
    import psycopg
    connection = psycopg.connect(url, autocommit=True)
    connection.execute("BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
    return connection, "%s", "postgres"


def read_history_evidence(args, directory, generation_ids, *, allow_legacy=False):
    """Verify every resolved generation and its complete principal-scoped ancestry.

    ``allow_legacy`` is only for the frozen pre-ADR-0089 benchmark baseline binary;
    current product tests require the shared content-store schema.
    """
    identifiers = list(dict.fromkeys(generation_ids))
    result = {}
    connection = None
    backend = "postgres" if getattr(args, "database_url", None) else "sqlite"
    try:
        connection, placeholder, backend = _connect(args, directory)
        reader = _history_reader(connection, placeholder, allow_legacy)
        for identifier in identifiers:
            proof = {"generation_id": identifier, "parent_id": None, "principal_sha256": None,
                     "output_sha256": None, "payload_valid": False, "references_valid": False,
                     "fingerprint_valid": False, "backend": backend, "read_verified": False,
                     "failures": [], "ancestor_count": 0, "content_count": 0, "slot_count": 0}
            result[identifier] = proof
            try:
                node = reader.node(identifier)
                proof["parent_id"], principal = reader.identity(node)
                proof["principal_sha256"] = hashlib.sha256(principal.encode()).hexdigest()
                seen, cursor = set(), identifier
                while cursor is not None:
                    _require(cursor not in seen, "parent_cycle")
                    seen.add(cursor)
                    parent, output, count, slots = reader.verify(cursor, principal)
                    if cursor == identifier:
                        proof["output_sha256"] = output
                    proof["content_count"] += count
                    proof["slot_count"] += slots
                    cursor = parent
                proof.update(payload_valid=True, references_valid=True, fingerprint_valid=True,
                             read_verified=True, ancestor_count=len(seen) - 1)
            except Exception as error:
                proof["failures"].append(str(error) if isinstance(error, InvalidHistory) else "history_decode_failed")
    except Exception as error:
        code = str(error) if isinstance(error, InvalidHistory) else "history_read_failed"
        for identifier in identifiers:
            result[identifier] = {"generation_id": identifier, "parent_id": None,
                                  "principal_sha256": None, "output_sha256": None,
                                  "payload_valid": False, "references_valid": False,
                                  "fingerprint_valid": False, "backend": backend,
                                  "read_verified": False, "failures": [code]}
    finally:
        if connection is not None:
            connection.close()
    return result


def _observation_new(reader, run_id):
    """ADR-0089 layout: blocks and event/tool-result references into the shared content store."""
    owner = reader.rows(
        "SELECT i.principal FROM inference_run_observations r "
        "JOIN interaction_observations i ON i.id=r.interaction_id WHERE r.id=?", (run_id,))
    _require(len(owner) == 1, "observation_run_missing")
    interaction_principal = owner[0][0]
    sequences = [row[0] for row in reader.rows(
        "SELECT sequence FROM observation_events WHERE run_id=? ORDER BY sequence", (run_id,))]
    _require(bool(sequences), "observation_events_missing")
    scopes, refs = set(), 0
    units = {}
    for table, ordinal_column in (("observation_event_contents", "ordinal"), ("observation_tool_results", "ordinal")):
        for row in reader.rows(
                f"SELECT r.sequence,r.{ordinal_column},c.id,s.principal,c.digest,c.format,c.raw_len,c.body "
                f"FROM {table} r JOIN observation_events e ON e.sequence=r.sequence "
                "LEFT JOIN contents c ON c.id=r.content_id "
                "LEFT JOIN content_scopes s ON s.id=c.scope_id "
                f"WHERE e.run_id=? ORDER BY r.sequence,r.{ordinal_column}", (run_id,)):
            sequence, ordinal, content_id, principal, *unit = row
            _require(content_id is not None and principal is not None and unit[3] is not None, "observation_reference_missing")
            units.setdefault((table, sequence), []).append((ordinal, verified_unit(unit)))
            scopes.add(principal)
            refs += 1
    for rows in units.values():
        _require([ordinal for ordinal, _ in rows] == list(range(len(rows))), "observation_ordinal_sequence")
    visible = []
    blocks = reader.rows(
        "SELECT sequence,channel,complete FROM observation_blocks WHERE run_id=? ORDER BY sequence", (run_id,))
    for sequence, channel, complete in blocks:
        _require(channel in ("thinking", "visible") and complete in (0, 1), "observation_block_shape")
        if channel == "visible" and complete == 1:
            text = [value for _, value in units.get(("observation_event_contents", sequence), [])]
            _require(all(isinstance(value, str) for value in text), "observation_visible_not_text")
            visible.extend(text)
    return {"output_sha256": hashlib.sha256("".join(visible).encode()).hexdigest(),
            "visible_blocks": sum(1 for _, channel, complete in blocks if channel == "visible" and complete == 1),
            "content_refs": refs, "scopes": scopes, "interaction_principal": interaction_principal}


def _observation_legacy(reader, run_id):
    """Frozen baseline layout: whole event payloads are codec-encoded JSON rows."""
    rows = reader.rows(
        "SELECT sequence,kind,payload FROM observation_events WHERE run_id=? ORDER BY sequence", (run_id,))
    _require(bool(rows), "observation_events_missing")
    visible = []
    for _sequence, kind, payload in rows:
        value = _json(_decode(payload))
        _require(isinstance(value, dict), "observation_payload_shape")
        if kind == "client_visible_content" and value.get("complete") is True:
            _require(isinstance(value.get("text"), str), "observation_visible_not_text")
            visible.append(value["text"])
    return {"output_sha256": hashlib.sha256("".join(visible).encode()).hexdigest(),
            "visible_blocks": len(visible), "content_refs": 0, "scopes": set(), "interaction_principal": None}


def read_observation_evidence(args, directory, run_ids, *, allow_legacy=False):
    """Verify each Run's persisted Observation text straight from storage.

    The ADR-0089 layout is read through ``observation_blocks`` and the event /
    tool-result reference tables with the same digest, length and codec checks
    as Turn Chain contents. ``allow_legacy`` additionally accepts the frozen
    baseline payload-per-event layout. Text is hashed unmasked: admin reads
    mask credentials (ADR-0088), the stored bytes must equal the client body.
    """
    identifiers = list(dict.fromkeys(run_ids))
    backend = "postgres" if getattr(args, "database_url", None) else "sqlite"
    result = {identifier: {"run_id": identifier, "backend": backend, "layout": None, "read_verified": False,
                           "output_sha256": None, "visible_blocks": 0, "content_refs": 0,
                           "content_scope_sha256": [], "interaction_principal_sha256": None, "failures": []}
              for identifier in identifiers}
    connection = None
    try:
        connection, placeholder, backend = _connect(args, directory)
        tables = _history_tables(connection, placeholder)
        reader = _SqlReader(connection, placeholder)
        if "observation_event_contents" in tables and "observation_blocks" in tables:
            layout, read = NORMALIZED_HISTORY, _observation_new
        else:
            layout, read = LEGACY_HISTORY, _observation_legacy
            _require(allow_legacy and "observation_events" in tables, "observation_schema")
        for identifier, proof in result.items():
            proof.update(backend=backend, layout=layout)
            try:
                found = read(reader, identifier)
                proof.update(output_sha256=found["output_sha256"], visible_blocks=found["visible_blocks"],
                             content_refs=found["content_refs"], read_verified=True,
                             content_scope_sha256=sorted(hashlib.sha256(scope.encode()).hexdigest() for scope in found["scopes"]))
                if found["interaction_principal"] is not None:
                    proof["interaction_principal_sha256"] = hashlib.sha256(found["interaction_principal"].encode()).hexdigest()
            except Exception as error:
                proof["failures"].append(str(error) if isinstance(error, InvalidHistory) else "observation_decode_failed")
    except Exception as error:
        code = str(error) if isinstance(error, InvalidHistory) else "observation_read_failed"
        for proof in result.values():
            proof["failures"] = [code]
    finally:
        if connection is not None:
            connection.close()
    return result


def read_credential_evidence(args, directory, expectations):
    """Read committed plaintext mappings without exposing their values or identities.

    Both storage backends persist secret/reference/principal as TEXT and timestamps
    as epoch milliseconds (SQLite INTEGER, PostgreSQL BIGINT).
    """
    backend = "postgres" if getattr(args, "database_url", None) else "sqlite"
    result = {
        nonce: {"read_verified": False, "principal_sha256": None,
                "mapping_count": 0, "new_mapping_count": 0,
                "published": False, "unexpired": False, "backend": backend,
                "failures": []}
        for nonce in expectations
    }
    if not expectations:
        return result
    connection = None
    try:
        connection, placeholder, backend = _connect(args, directory)
        reader = _SqlReader(connection, placeholder)
        now = time.time_ns() // 1_000_000
        mapping_columns = _table_columns(connection, placeholder, "reversible_redaction_mappings")
        legacy_expiry = "expires_at" in mapping_columns
        # 空闲过期：保留期从最近一次使用起算，published_at 只表示已发布过。
        idle_expiry = "last_used_at" in mapping_columns
        retention_ms = 0
        if not legacy_expiry:
            configured = reader.rows("SELECT value FROM settings WHERE name='log_retention_days'", ())
            retention_ms = int(configured[0][0] if configured else 7) * 86_400_000
        identities, reverse_identities = {}, {}
        principal = None
        for nonce, expected in expectations.items():
            proof = result[nonce]
            proof["backend"] = backend
            try:
                secrets, references = expected["secrets"], expected["references"]
                new = expected["new_references"]
                _require(isinstance(secrets, (tuple, list)) and
                         isinstance(references, (tuple, list)) and
                         isinstance(new, (tuple, list)), "credential_expectations")
                _require(len(secrets) == len(references) and len(secrets) > 0 and
                         all(isinstance(secret, str) and secret for secret in secrets) and
                         all(isinstance(ref, str) and re.fullmatch(r"<!--sr:[a-z]{28}-->", ref)
                             for ref in references), "credential_expectations")
                _require(len(set(secrets)) == len(secrets) and
                         len(set(references)) == len(references),
                         "credential_distinct_mappings")
                _require(all(isinstance(ref, str) for ref in new) and
                         len(set(new)) == len(new) and set(new) <= set(references) and
                         type(expected["reused_references"]) is int and
                         expected["reused_references"] == len(references) - len(new),
                         "credential_reuse_count")
                for secret, reference in zip(secrets, references):
                    _require(identities.get(secret, reference) == reference and
                             reverse_identities.get(reference, secret) == secret,
                             "credential_reused_identity")
                    # ADR-0089 removed the mapping expires_at column; a later change made the
                    # window idle-based: a mapping is kept for the retention window counted
                    # from its last use (last_used_at), and from published_at before that.
                    deadline_column = "expires_at" if legacy_expiry else "pending_until"
                    used_column = "last_used_at" if idle_expiry else "published_at"
                    rows = reader.rows(
                        f"SELECT principal,secret,published_at,{deadline_column},{used_column} "
                        "FROM reversible_redaction_mappings WHERE reference=?", (reference,))
                    _require(len(rows) == 1, "credential_mapping_missing")
                    owner, stored_secret, published_at, deadline, last_used = rows[0]
                    _require(isinstance(owner, str) and bool(owner), "credential_principal")
                    _require(principal is None or principal == owner, "credential_principal")
                    _require(isinstance(stored_secret, str) and stored_secret == secret,
                             "credential_secret")
                    _require(type(published_at) is int and 0 <= published_at <= now,
                             "credential_unpublished")
                    if legacy_expiry:
                        _require(type(deadline) is int and deadline > now, "credential_expired")
                    else:
                        _require(deadline is None, "credential_unpublished")
                        _require(type(last_used) is int and last_used + retention_ms > now,
                                 "credential_expired")
                    principal = owner
                    identities[secret] = reference
                    reverse_identities[reference] = secret
                    proof["mapping_count"] += 1
                proof.update(read_verified=True,
                             principal_sha256=hashlib.sha256(principal.encode()).hexdigest(),
                             new_mapping_count=len(new), published=True, unexpired=True)
            except Exception as error:
                proof["failures"].append(str(error) if isinstance(error, InvalidHistory)
                                         else "credential_read_failed")
    except Exception as error:
        code = str(error) if isinstance(error, InvalidHistory) else "credential_read_failed"
        for proof in result.values():
            proof["failures"] = [code]
    finally:
        if connection is not None:
            connection.close()
    return result
