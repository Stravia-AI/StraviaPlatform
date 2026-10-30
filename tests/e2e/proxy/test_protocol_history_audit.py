"""Storage evidence exercises real normalized-v3 SQLite rows and codec bytes."""
import hashlib
import json
import sqlite3
from types import SimpleNamespace

import pytest
import zstandard

from tests.common.protocol_history_audit import (
    LEGACY_HISTORY, NORMALIZED_HISTORY, HistoryReader, content_digest,
    history_storage_schema, read_credential_evidence, read_history_evidence,
)


def test_storage_schema_selection_prefers_normalized_and_keeps_baseline_adapter():
    assert history_storage_schema({"turn_nodes", "turn_chain_nodes"}) == NORMALIZED_HISTORY
    assert history_storage_schema({"turn_chain_nodes"}) == LEGACY_HISTORY
    with pytest.raises(ValueError):
        history_storage_schema(set())


def encode(raw, compressed=False):
    body = zstandard.ZstdCompressor(write_checksum=True).compress(raw) if compressed else raw
    return body + bytes([int(compressed)]) + b"\0" * 4 + len(raw).to_bytes(8, "little") + b"\1"


def raw(value):
    return json.dumps(value, separators=(",", ":"), ensure_ascii=False).encode()


@pytest.fixture
def history(tmp_path):
    (tmp_path / "db").mkdir()
    connection = sqlite3.connect(tmp_path / "db" / "gateway.db")
    connection.executescript("""
        CREATE TABLE turn_scopes(id INTEGER PRIMARY KEY, principal TEXT UNIQUE);
        CREATE TABLE turn_nodes(
            node_pk INTEGER PRIMARY KEY, id TEXT UNIQUE, scope_id INTEGER NOT NULL,
            kind TEXT NOT NULL, parent_pk INTEGER, payload_version INTEGER NOT NULL);
        CREATE TABLE turn_node_payloads(node_pk INTEGER PRIMARY KEY, payload BLOB NOT NULL);
        CREATE TABLE turn_contents(
            id INTEGER PRIMARY KEY, scope_id INTEGER NOT NULL, digest BLOB NOT NULL,
            content BLOB NOT NULL, UNIQUE(scope_id, digest));
        CREATE TABLE turn_node_contents(
            node_pk INTEGER NOT NULL, scope_id INTEGER NOT NULL, ordinal INTEGER NOT NULL,
            content_id INTEGER NOT NULL, PRIMARY KEY(node_pk, ordinal),
            UNIQUE(node_pk, content_id));
    """)
    item = {"role": "assistant", "content": [{"type": "text", "text": "输出😀" * 100}]}
    item_raw = raw(item)
    state = dict.fromkeys(("context_fingerprint", "model", "provider_model", "selected_target_key",
                           "canonical_controls_fingerprint", "target_namespace", "protocol"), "")
    state["context_messages"] = 0
    payload = {"client_delta": {"messages": [], "system": None},
               "client_output": [None, None], "effective_system": None,
               "effective_output": {"id": "vendor", "model": "model", "items": [],
                                    "usage": {}, "vendor": {}},
               "trusted_media_turn_ids": [], "upstream_response_id": None,
               "effective_state": state, "effective_request": None}
    envelope = {"data": payload, "slots": [["/client_output/0", 0], ["/client_output/1", 0]]}
    connection.execute("INSERT INTO turn_scopes VALUES(?,?)", (1, "private-owner"))
    connection.execute("INSERT INTO turn_nodes VALUES(?,?,?,?,?,?)",
                       (1, "generation", 1, "response", None, 6))
    connection.execute("INSERT INTO turn_node_payloads VALUES(?,?)", (1, encode(raw(envelope), True)))
    connection.execute("INSERT INTO turn_contents VALUES(?,?,?,?)",
                       (1, 1, content_digest(item_raw), encode(item_raw, True)))
    connection.execute("INSERT INTO turn_node_contents VALUES(?,?,?,?)", (1, 1, 0, 1))
    connection.commit()
    yield tmp_path, connection, envelope, item
    connection.close()


def proof(history):
    directory, connection, _, _ = history
    connection.commit()
    return read_history_evidence(SimpleNamespace(database_url=None), directory, ["generation"])["generation"]


def test_restores_repeated_slots_and_hashes_real_output(history):
    result = proof(history)
    text = history[3]["content"][0]["text"] * 2
    assert result["read_verified"]
    assert result["payload_valid"] and result["references_valid"] and result["fingerprint_valid"]
    assert result["output_sha256"] == hashlib.sha256(text.encode()).hexdigest()
    assert result["content_count"] == 1 and result["slot_count"] == 2
    assert "private-owner" not in json.dumps(result)
    assert text not in json.dumps(result)


@pytest.mark.parametrize("mutation,code", [
    ("DELETE FROM turn_contents", "reference_missing"),
    ("UPDATE turn_contents SET digest=zeroblob(32)", "content_fingerprint"),
    ("UPDATE turn_contents SET digest=zeroblob(31)", "content_digest"),
    ("UPDATE turn_node_contents SET content_id=99", "reference_missing"),
    ("UPDATE turn_node_contents SET scope_id=2", "reference_scope"),
    ("DELETE FROM turn_node_contents", "slot_index"),
    ("UPDATE turn_node_contents SET ordinal=1", "ordinal_sequence"),
    ("UPDATE turn_nodes SET parent_pk=1", "parent_cycle"),
    ("UPDATE turn_nodes SET parent_pk=99", "parent_missing"),
    ("UPDATE turn_nodes SET payload_version=5", "payload_version"),
    ("UPDATE turn_nodes SET kind='agent'", "node_identity"),
    ("DELETE FROM turn_node_payloads", "payload_missing"),
])
def test_rejects_corrupt_storage(history, mutation, code):
    history[1].execute(mutation)
    result = proof(history)
    assert not result["read_verified"]
    assert result["failures"] == [code]


def test_parent_scope_is_checked(history):
    connection = history[1]
    connection.execute("INSERT INTO turn_scopes VALUES(?,?)", (2, "another-owner"))
    connection.execute("INSERT INTO turn_nodes VALUES(?,?,?,?,?,?)",
                       (2, "parent", 2, "response", None, 6))
    connection.execute("INSERT INTO turn_node_payloads SELECT 2,payload FROM turn_node_payloads WHERE node_pk=1")
    connection.execute("UPDATE turn_nodes SET parent_pk=2 WHERE node_pk=1")
    assert proof(history)["failures"] == ["parent_scope"]


def test_content_scope_is_checked(history):
    connection = history[1]
    item_raw = raw(history[3])
    connection.execute("INSERT INTO turn_scopes VALUES(?,?)", (2, "another-owner"))
    connection.execute("INSERT INTO turn_contents VALUES(?,?,?,?)",
                       (2, 2, content_digest(item_raw), encode(item_raw)))
    connection.execute("UPDATE turn_node_contents SET content_id=2 WHERE node_pk=1")
    assert proof(history)["failures"] == ["reference_scope"]


def test_real_json_pointer_parent_before_child(history):
    directory, connection, envelope, item = history
    profile = {"output": [None], "a/b~c": "kept"}
    profile_raw = raw(profile)
    envelope["data"]["__open_responses_response_profile"] = None
    envelope["slots"].extend([["/__open_responses_response_profile", 1], ["/__open_responses_response_profile/output/0", 0]])
    connection.execute("INSERT INTO turn_contents VALUES(?,?,?,?)",
                       (2, 1, content_digest(profile_raw), encode(profile_raw)))
    connection.execute("INSERT INTO turn_node_contents VALUES(?,?,?,?)", (1, 1, 1, 2))
    connection.execute("UPDATE turn_node_payloads SET payload=? WHERE node_pk=1", (encode(raw(envelope)),))
    assert proof(history)["read_verified"]


def test_rejects_slot_ordinal_out_of_range(history):
    envelope = history[2]
    envelope["slots"][0][1] = 3
    history[1].execute("UPDATE turn_node_payloads SET payload=? WHERE node_pk=1", (encode(raw(envelope)),))
    assert proof(history)["failures"] == ["slot_index"]


def test_rejects_unknown_codec(history):
    history[1].execute("UPDATE turn_contents SET content=?", (b"invalid" + b"\0" * 13 + b"\2",))
    assert proof(history)["failures"] == ["codec_version"]


def test_rejects_malformed_descriptor_json(history):
    history[1].execute("UPDATE turn_node_payloads SET payload=? WHERE node_pk=1", (encode(b"{"),))
    assert proof(history)["failures"] == ["json_invalid"]


def test_rejects_malformed_content_json(history):
    body = b"{"
    history[1].execute("UPDATE turn_contents SET digest=?,content=?",
                       (content_digest(body), encode(body)))
    assert proof(history)["failures"] == ["json_invalid"]


def test_complete_ancestor_chain_is_restored(history):
    connection = history[1]
    connection.execute("INSERT INTO turn_nodes VALUES(?,?,?,?,?,?)",
                       (2, "parent", 1, "response", None, 6))
    connection.execute("INSERT INTO turn_node_payloads SELECT 2,payload FROM turn_node_payloads WHERE node_pk=1")
    connection.execute("INSERT INTO turn_node_contents SELECT 2,scope_id,ordinal,content_id FROM turn_node_contents WHERE node_pk=1")
    connection.execute("UPDATE turn_nodes SET parent_pk=2 WHERE node_pk=1")
    result = proof(history)
    assert result["read_verified"] and result["ancestor_count"] == 1
    assert result["parent_id"] == "parent"
    assert result["content_count"] == 2


def test_restore_preserves_arbitrary_precision_integers(history):
    directory, connection, envelope, item = history
    number = b"1234567890123456789012345678901234567890"
    envelope["data"]["opaque_big"] = None
    envelope["slots"].append(["/opaque_big", 1])
    connection.execute("INSERT INTO turn_contents VALUES(?,?,?,?)",
                       (2, 1, content_digest(number), encode(number)))
    connection.execute("INSERT INTO turn_node_contents VALUES(?,?,?,?)", (1, 1, 1, 2))
    connection.execute("UPDATE turn_node_payloads SET payload=? WHERE node_pk=1", (encode(raw(envelope)),))
    reader = HistoryReader(connection, "?")
    data, count, slots = reader.restore(reader.node("generation"))
    assert data["opaque_big"] == int(number)
    assert count == 2 and slots == 3


def test_decodes_escaped_pointer_tokens(history):
    envelope = history[2]
    envelope["data"]["a/b~c"] = None
    envelope["slots"].append(["/a~1b~0c", 0])
    history[1].execute("UPDATE turn_node_payloads SET payload=? WHERE node_pk=1", (encode(raw(envelope)),))
    assert proof(history)["read_verified"]


def test_reference_rows_must_cover_descriptor_slots(history):
    history[1].execute("DELETE FROM turn_node_contents")
    assert proof(history)["failures"] == ["slot_index"]


def test_unreferenced_content_ordinal_is_rejected(history):
    directory, connection, envelope, item = history
    connection.execute("INSERT INTO turn_contents VALUES(?,?,?,?)",
                       (2, 1, content_digest(b"null"), encode(b"null")))
    connection.execute("INSERT INTO turn_node_contents VALUES(?,?,?,?)", (1, 1, 1, 2))
    assert proof(history)["failures"] == ["unused_content"]


def test_legacy_contents_array_is_rejected(history):
    envelope = history[2]
    envelope["contents"] = []
    history[1].execute("UPDATE turn_node_payloads SET payload=? WHERE node_pk=1", (encode(raw(envelope)),))
    assert proof(history)["failures"] == ["envelope_shape"]


def test_repeated_slot_pointer_is_rejected(history):
    envelope = history[2]
    envelope["slots"].append(["/client_output/0", 0])
    history[1].execute("UPDATE turn_node_payloads SET payload=? WHERE node_pk=1", (encode(raw(envelope)),))
    assert proof(history)["failures"] == ["slot_pointer"]


def test_rejects_non_loopback_postgres_without_connecting(tmp_path):
    result = read_history_evidence(
        SimpleNamespace(database_url="postgresql://secret:token@192.0.2.1/db"),
        tmp_path, ["generation"],
    )["generation"]
    assert not result["read_verified"]
    assert result["failures"] == ["database_endpoint"]
    assert "secret" not in json.dumps(result) and "token" not in json.dumps(result)


@pytest.fixture
def credentials(tmp_path, monkeypatch):
    monkeypatch.setattr("tests.common.protocol_history_audit.time.time_ns",
                        lambda: 2_000_000_000_000_000_000)
    (tmp_path / "db").mkdir()
    connection = sqlite3.connect(tmp_path / "db" / "gateway.db")
    connection.executescript("""
        CREATE TABLE reversible_redaction_mappings(
            reference TEXT PRIMARY KEY NOT NULL, principal TEXT NOT NULL,
            secret TEXT NOT NULL, published_at INTEGER, created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL, expires_at INTEGER NOT NULL);
    """)
    secrets = ("ghp_" + "A" * 36, "ghp_" + "B" * 36)
    references = ("<!--sr:" + "a" * 28 + "-->", "<!--sr:" + "b" * 28 + "-->")
    for secret, reference in zip(secrets, references):
        connection.execute("INSERT INTO reversible_redaction_mappings VALUES(?,?,?,?,?,?,?)",
                           (reference, "private-owner", secret, 1_999_999_999_000,
                            1_999_999_999_000, 1_999_999_999_000, 2_000_000_001_000))
    connection.commit()
    expected = {"first": {"secrets": secrets, "references": references,
                          "new_references": references, "reused_references": 0},
                "second": {"secrets": secrets, "references": references,
                           "new_references": (), "reused_references": 2}}
    yield tmp_path, connection, expected
    connection.close()


def credential_proof(credentials):
    directory, connection, expected = credentials
    connection.commit()
    return read_credential_evidence(SimpleNamespace(database_url=None), directory, expected)


def test_reads_plaintext_mapping_publication_and_reused_identity(credentials):
    result = credential_proof(credentials)
    for nonce, new_count in (("first", 2), ("second", 0)):
        assert result[nonce] == {
            "read_verified": True,
            "principal_sha256": hashlib.sha256(b"private-owner").hexdigest(),
            "mapping_count": 2, "new_mapping_count": new_count,
            "published": True, "unexpired": True, "backend": "sqlite", "failures": [],
        }
    serialized = json.dumps(result)
    assert "private-owner" not in serialized
    for value in (*credentials[2]["first"]["secrets"],
                  *credentials[2]["first"]["references"]):
        assert value not in serialized
    assert credentials[1].execute("SELECT COUNT(*) FROM reversible_redaction_mappings").fetchone() == (2,)


@pytest.mark.parametrize("mutation,code", [
    ("DELETE FROM reversible_redaction_mappings", "credential_mapping_missing"),
    ("UPDATE reversible_redaction_mappings SET secret='wrong'", "credential_secret"),
    ("UPDATE reversible_redaction_mappings SET published_at=NULL", "credential_unpublished"),
    ("UPDATE reversible_redaction_mappings SET published_at=2000000000001", "credential_unpublished"),
    ("UPDATE reversible_redaction_mappings SET expires_at=2000000000000", "credential_expired"),
    ("UPDATE reversible_redaction_mappings SET principal='' ", "credential_principal"),
    ("UPDATE reversible_redaction_mappings SET principal=secret", "credential_principal"),
])
def test_credential_storage_rejects_real_row_mutations(credentials, mutation, code):
    credentials[1].execute(mutation)
    result = credential_proof(credentials)
    assert not result["first"]["read_verified"]
    assert result["first"]["failures"] == [code]


def test_credential_principal_must_be_shared_across_requests(credentials):
    expected = credentials[2]
    for index, nonce in enumerate(("first", "second")):
        expected[nonce] = {"secrets": (expected[nonce]["secrets"][index],),
                           "references": (expected[nonce]["references"][index],),
                           "new_references": (), "reused_references": 1}
    credentials[1].execute(
        "UPDATE reversible_redaction_mappings SET principal='other-owner' WHERE secret=?",
        (expected["second"]["secrets"][0],))
    result = credential_proof(credentials)
    assert result["first"]["read_verified"]
    assert result["second"]["failures"] == ["credential_principal"]


@pytest.mark.parametrize("change,code", [
    ({"references": ("<!--sr:" + "a" * 28 + "-->",) * 2}, "credential_distinct_mappings"),
    ({"reused_references": 1}, "credential_reuse_count"),
    ({"new_references": ("<!--sr:" + "z" * 28 + "-->",)}, "credential_reuse_count"),
    ({"secrets": ()}, "credential_expectations"),
])
def test_rejects_incomplete_or_inconsistent_credential_expectations(credentials, change, code):
    credentials[2]["first"].update(change)
    assert credential_proof(credentials)["first"]["failures"] == [code]


def test_rejects_changed_reference_for_reused_secret(credentials):
    expected = credentials[2]["second"]
    changed = "<!--sr:" + "c" * 28 + "-->"
    credentials[1].execute(
        "INSERT INTO reversible_redaction_mappings "
        "SELECT ?,principal,secret,published_at,created_at,updated_at,expires_at "
        "FROM reversible_redaction_mappings WHERE reference=?",
        (changed, expected["references"][0]))
    expected["references"] = (changed, expected["references"][1])
    assert credential_proof(credentials)["second"]["failures"] == ["credential_reused_identity"]


def test_duplicate_reference_rows_cannot_prove_mapping(credentials):
    credentials[1].executescript("""
        ALTER TABLE reversible_redaction_mappings RENAME TO original_mappings;
        CREATE TABLE reversible_redaction_mappings AS SELECT * FROM original_mappings;
        INSERT INTO reversible_redaction_mappings SELECT * FROM original_mappings;
    """)
    assert credential_proof(credentials)["first"]["failures"] == ["credential_mapping_missing"]


def test_credential_reader_uses_real_readonly_snapshot(credentials, monkeypatch):
    from tests.common import protocol_history_audit

    original_connect = protocol_history_audit._connect
    observed = []

    def connect(args, directory):
        connection, placeholder, backend = original_connect(args, directory)
        assert connection.in_transaction
        assert connection.execute("PRAGMA query_only").fetchone() == (1,)
        with pytest.raises(sqlite3.OperationalError):
            connection.execute("DELETE FROM reversible_redaction_mappings")
        observed.append(True)
        return connection, placeholder, backend

    monkeypatch.setattr(protocol_history_audit, "_connect", connect)
    assert credential_proof(credentials)["first"]["read_verified"]
    assert observed == [True]


def test_missing_mapping_table_returns_only_safe_failure(tmp_path):
    (tmp_path / "db").mkdir()
    sqlite3.connect(tmp_path / "db" / "gateway.db").close()
    expected = {"nonce": {"secrets": ("private-secret",),
                          "references": ("<!--sr:" + "a" * 28 + "-->",),
                          "new_references": (), "reused_references": 1}}
    result = read_credential_evidence(SimpleNamespace(database_url=None), tmp_path, expected)
    assert result["nonce"]["failures"] == ["credential_read_failed"]
    assert "private-secret" not in json.dumps(result)


def test_postgres_reader_adapts_bound_query_without_interpolating_secrets(credentials):
    # A real SQLite cursor verifies placeholder adaptation semantics; the
    # production PostgreSQL connection uses psycopg's %s rather than SQLite's ?.
    class PostgresParameters:
        def execute(self, query, values):
            assert query.count("%s") == 1 and "?" not in query
            assert values[0] not in query
            return credentials[1].execute(query.replace("%s", "?"), values)

    reader = HistoryReader(PostgresParameters(), "%s")
    reference = credentials[2]["first"]["references"][0]
    rows = reader.rows("SELECT secret FROM reversible_redaction_mappings WHERE reference=?",
                       (reference,))
    assert rows == [(credentials[2]["first"]["secrets"][0],)]
