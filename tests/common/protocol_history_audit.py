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


CONTENT_DIGEST_DOMAIN = b"stravia.turn-content.v1\0"
NORMALIZED_HISTORY = "normalized-v3"
LEGACY_HISTORY = "legacy-format2"


class InvalidHistory(ValueError):
    pass


def _require(condition, code):
    if not condition:
        raise InvalidHistory(code)


def content_digest(raw):
    """Full SHA-256 over the fixed domain prefix and original JSON bytes."""
    digest = hashlib.sha256(CONTENT_DIGEST_DOMAIN)
    digest.update(raw)
    return digest.digest()


def history_storage_schema(tables):
    """Select the persisted history layout by table presence.

    Normalized v3 is authoritative whenever it exists. The legacy branch is a
    measurement adapter for frozen pre-v3 binaries, never a runtime fallback.
    """
    if "turn_nodes" in tables:
        return NORMALIZED_HISTORY
    if "turn_chain_nodes" in tables:
        return LEGACY_HISTORY
    raise InvalidHistory("history_schema")


def _legacy_digest(raw):
    number = int.from_bytes(hashlib.sha256(raw).digest(), "big")
    digits = []
    for _ in range(55):
        number, digit = divmod(number, 26)
        digits.append(chr(97 + digit))
    return "".join(reversed(digits))


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
    """Restore a v3 descriptor from ordinal-ordered decoded content values."""
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


def _legacy_restore(envelope, contents):
    _require(isinstance(envelope, dict) and set(envelope) == {"data", "slots", "contents"}, "envelope_shape")
    keys, slots = envelope["contents"], envelope["slots"]
    _require(isinstance(keys, list) and all(isinstance(k, str) for k in keys), "contents_shape")
    _require(len(set(keys)) == len(keys) and set(keys) == set(contents), "reference_set")
    _require(isinstance(slots, list), "slots_shape")
    data = envelope["data"]
    seen, used = set(), set()
    for slot in slots:
        _require(isinstance(slot, list) and len(slot) == 2, "slot_shape")
        pointer, index = slot
        _require(isinstance(pointer, str) and pointer.startswith("/") and pointer not in seen, "slot_pointer")
        _require(type(index) is int and 0 <= index < len(keys), "slot_index")
        seen.add(pointer)
        used.add(index)
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
            parent[int(token)] = copy.deepcopy(contents[keys[index]])
        else:
            parent[token] = copy.deepcopy(contents[keys[index]])
    _require(used == set(range(len(keys))), "unused_content")
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


class HistoryReader(_SqlReader):
    """Strict normalized-v3 reader shared by audit evidence and fixture export."""

    def node(self, identifier):
        rows = self.rows(
            "SELECT n.node_pk,n.id,n.parent_pk,parent.id,parent.scope_id,parent.kind,"
            "n.kind,n.scope_id,s.principal,n.payload_version,p.payload "
            "FROM turn_nodes n JOIN turn_scopes s ON s.id=n.scope_id "
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

    def restore(self, node):
        """Decode one descriptor and validate its complete ordinal/content set."""
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
            _require(content_digest(raw) == bytes(digest), "content_fingerprint")
            contents.append(_json(raw))
        data = _restore(descriptor, contents)
        return data, len(rows), len(descriptor["slots"])

    def identity(self, node):
        return node["parent_id"], node["principal"]

    def verify(self, identifier, principal):
        node = self.node(identifier)
        _require(node["id"] == identifier and node["principal"] == principal
                 and node["kind"] == "response", "node_identity")
        _require(node["payload_version"] == 6, "payload_version")
        payload, count, slots = self.restore(node)
        return node["parent_id"], _output(payload), count, slots


class _LegacyHistoryReader(_SqlReader):
    """Read-only adapter for frozen pre-v3 benchmark binaries only."""

    def node(self, identifier):
        rows = self.rows("SELECT id,parent_id,kind,principal,payload_version,storage_format,payload FROM turn_chain_nodes WHERE id=?", (identifier,))
        _require(len(rows) == 1, "node_missing")
        return rows[0]

    def identity(self, node):
        return node[1], node[3]

    def verify(self, identifier, principal):
        node = self.node(identifier)
        node_id, parent_id, kind, owner, version, storage_format, encoded = node
        _require(node_id == identifier and owner == principal and kind == "response", "node_identity")
        _require(version == 6 and storage_format == 2, "payload_version")
        envelope = _json(_decode(encoded))
        rows = self.rows("SELECT r.principal,c.principal,c.content_key,c.content FROM turn_chain_node_contents r LEFT JOIN turn_chain_contents c ON c.id=r.content_id WHERE r.node_id=?", (identifier,))
        contents = {}
        for ref_owner, content_owner, key, encoded_content in rows:
            _require(ref_owner == principal and content_owner == principal and encoded_content is not None, "reference_principal")
            _require(key not in contents, "reference_duplicate")
            raw = _decode(encoded_content)
            _require(_legacy_digest(raw) == key, "content_fingerprint")
            contents[key] = _json(raw)
        payload = _legacy_restore(envelope, contents)
        output = _output(payload)
        return parent_id, output, len(rows), len(envelope["slots"])


def _history_tables(connection, placeholder):
    query = ("SELECT name FROM sqlite_master WHERE type='table'" if placeholder == "?"
             else "SELECT table_name FROM information_schema.tables WHERE table_schema=current_schema()")
    return {row[0] for row in connection.execute(query).fetchall()}


def _history_reader(connection, placeholder, allow_legacy=False):
    tables = _history_tables(connection, placeholder)
    if not allow_legacy and "turn_nodes" not in tables:
        raise InvalidHistory("history_schema")
    schema = history_storage_schema(tables)
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

    ``allow_legacy`` is only for frozen pre-v3 benchmark binaries; current
    product tests require the normalized schema.
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
                    rows = reader.rows(
                        "SELECT principal,secret,published_at,expires_at "
                        "FROM reversible_redaction_mappings WHERE reference=?", (reference,))
                    _require(len(rows) == 1, "credential_mapping_missing")
                    owner, stored_secret, published_at, expires_at = rows[0]
                    _require(isinstance(owner, str) and bool(owner), "credential_principal")
                    _require(principal is None or principal == owner, "credential_principal")
                    _require(isinstance(stored_secret, str) and stored_secret == secret,
                             "credential_secret")
                    _require(type(published_at) is int and 0 <= published_at <= now,
                             "credential_unpublished")
                    _require(type(expires_at) is int and expires_at > now,
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
