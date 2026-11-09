import json
import sqlite3
from pathlib import Path

import pytest

from tests.common.vendor_alignment import load_response_code_map, read_source, without_refresh_tokens


def test_inventory_reads_wal_without_credentials_or_writes(tmp_path: Path):
    db_dir = tmp_path / "db"
    db_dir.mkdir()
    db = sqlite3.connect(db_dir / "gateway.db")
    try:
        db.execute("PRAGMA journal_mode=WAL")
        db.execute("CREATE TABLE vendor_plugins(vendor_id TEXT, descriptor TEXT)")
        db.execute("CREATE TABLE providers(id TEXT, vendor TEXT, channel TEXT, is_enabled INTEGER, api_key TEXT)")
        db.execute("INSERT INTO vendor_plugins VALUES (?, ?)", ("component", json.dumps({"providers": [{"provider_id": "profile"}]})))
        db.execute("INSERT INTO providers VALUES ('private-id','profile','default',1,'secret-marker')")
        db.commit()
        before = db.total_changes
        inventory = read_source(tmp_path, ["component"], False)
        assert [item["provider"] for item in inventory["providers"]] == [{"vendor": "profile", "channel": "default"}]
        assert all(item["oauth"] is None and item["models"] == [] for item in inventory["providers"])
        assert "private-id" not in json.dumps(inventory)
        assert "secret-marker" not in json.dumps(inventory)
        assert db.total_changes == before
        assert db.execute("SELECT api_key FROM providers").fetchone() == ("secret-marker",)
    finally:
        db.close()


def test_nested_refresh_material_is_removed_without_discarding_access_token():
    source = {"oauth": {"access_token": "access", "refresh_token": "secret"},
              "metadata": '{"nested":{"refreshToken":"secret","id":"keep"}}'}
    result = without_refresh_tokens(source)
    assert result["oauth"] == {"access_token": "access"}
    assert json.loads(result["metadata"]) == {"nested": {"id": "keep"}}
    assert "secret" not in json.dumps(result)


def test_response_code_map_schema_and_bounds(tmp_path: Path):
    path = tmp_path / "map.json"
    valid = {"fields": ["code", "status"], "codes": {"42": "rate_limit"}}
    path.write_text(json.dumps(valid))
    assert load_response_code_map(path) == valid
    invalid = [
        {"fields": ["message"], "codes": {}},
        {"fields": ["code", "code"], "codes": {}},
        {"fields": ["code"], "codes": {"42": "upstream private string"}},
        {"fields": ["code"], "codes": {"042": "quota"}},
        {"fields": ["code"], "codes": {str(i): "quota" for i in range(65)}},
        {"fields": ["code"], "codes": {}, "extra": True},
    ]
    for item in invalid:
        path.write_text(json.dumps(item))
        with pytest.raises(ValueError):
            load_response_code_map(path)
    path.write_bytes(b" " * (16 * 1024 + 1))
    with pytest.raises(ValueError):
        load_response_code_map(path)
