from __future__ import annotations

import json
import sqlite3
from pathlib import Path

import pytest


LEGACY_CONFIG = {
    "url": "socks5h://user:password@127.0.0.1:1080",
    "bypass": "localhost, .example.test, 127.0.0.1",
    "force_http1": True,
}
SAVED_CONFIG = {
    "url": "https://127.0.0.1:8443",
    "bypass": "saved.test",
    "force_http1": False,
}
# name, old global value, legacy keys present, new config present, model flag after migration
CASES = [
    ("disabled", "false", True, False, False),
    ("enabled", "true", True, False, True),
    ("one", " 1\t", True, False, True),
    ("yes", "\nYeS\r", True, False, True),
    ("on", " ON ", True, False, True),
    ("trimmed_true", "\tTrUe\n", True, False, True),
    ("nbsp_true", "\u00a0true\u00a0", True, False, True),
    ("mixed_unicode_true", "\u3000\u2003TrUe\u2003\u3000", True, False, True),
    ("missing_global", None, True, False, False),
    ("missing_all_legacy", None, False, False, False),
    ("saved_config", "true", True, True, True),
    ("new_only", None, False, True, True),
    ("saved_config_disabled", "false", True, True, False),
    ("fresh", None, False, False, True),
]


@pytest.mark.e2e
@pytest.mark.storage
@pytest.mark.parametrize("backend", ["sqlite", "postgres"])
def test_outbound_proxy_migration_preserves_egress_and_configuration(
    storage_runtime: dict[str, object], backend: str,
) -> None:
    if backend == "postgres":
        pg_url = storage_runtime["pg_url"]
        if not pg_url:
            pytest.skip("postgres backend requires DB_URL")
        schema = storage_runtime["make_isolated_schema"]("stravia_proxy_migration")
        action = storage_runtime["run_schema_action"]
        args = {"pg_url": pg_url, "schema": schema}
        action("create", **args)
        try:
            output = action("verify_outbound_proxy_migration", **args)
            for case, *_ in CASES:
                assert f"outbound_proxy_migration_{case}=true" in output
        finally:
            action("drop", **args)
        return

    directory = Path(__file__).resolve().parents[3] / "backend/crates/stravia-core/migrations/sqlite"
    migrations = sorted(directory.glob("*.sql"))
    for case, global_value, legacy, saved, expected_enabled in CASES:
        with sqlite3.connect(":memory:") as connection:
            # Use every real pre-cutover migration: fixtures must satisfy the actual
            # complete schema, rather than a hand-built approximation of its tables.
            for migration in migrations:
                if migration.name >= "0016":
                    break
                connection.executescript(migration.read_text(encoding="utf-8"))
            provider_insert = (
                "INSERT INTO providers (id, name, protocol, base_url, api_key, use_proxy) "
                "VALUES (?, ?, 'openai', 'http://127.0.0.1:8080', 'fixture-key', ?)"
            )
            providers = [("migration-proxied", "Migration proxied", 1),
                         ("migration-direct", "Migration direct", 0)]
            # Old rows without any proxy settings used the missing global's false gate;
            # fresh user Providers are created only after migrations finish.
            if case != "fresh":
                connection.executemany(provider_insert, providers)
            connection.executemany(
                "INSERT INTO web_providers (id, name, kind, api_key, use_proxy) "
                "VALUES (?, ?, 'exa', 'fixture-key', ?)",
                [("migration-web-proxied", "Migration web proxied", 1),
                 ("migration-web-direct", "Migration web direct", 0)],
            )
            if legacy:
                connection.executemany(
                    "INSERT INTO settings (name, value) VALUES (?, ?)",
                    [("proxy_url", LEGACY_CONFIG["url"]),
                     ("proxy_bypass", LEGACY_CONFIG["bypass"]),
                     ("proxy_force_http1", global_value if case in {"nbsp_true", "mixed_unicode_true"} else "\tYeS\n")],
                )
            if global_value is not None:
                connection.execute(
                    "INSERT INTO settings (name, value) VALUES ('proxy_enabled', ?)",
                    (global_value,),
                )
            if saved:
                connection.executemany(
                    "INSERT INTO settings (name, value) VALUES (?, ?)",
                    [("outbound_proxy", json.dumps(SAVED_CONFIG)),
                     ("update_use_proxy", "false")],
                )
            connection.executescript(
                (directory / "0016_outbound_proxy_settings.sql").read_text(encoding="utf-8")
            )
            if case == "fresh":
                connection.executemany(provider_insert, providers)
            assert connection.execute(
                "SELECT id, use_proxy FROM providers ORDER BY id"
            ).fetchall() == [
                ("migration-direct", 0), ("migration-proxied", int(expected_enabled))
            ], case
            assert connection.execute(
                "SELECT id, use_proxy FROM web_providers ORDER BY id"
            ).fetchall() == [
                ("migration-web-direct", 0), ("migration-web-proxied", 1),
                ("web-provider-local", 0),
            ], case
            value = connection.execute(
                "SELECT value FROM settings WHERE name = 'outbound_proxy'"
            ).fetchone()[0]
            expected_config = SAVED_CONFIG if saved else LEGACY_CONFIG if legacy else {
                "url": "", "bypass": "", "force_http1": False,
            }
            assert json.loads(value) == expected_config, case
            expected_update = "true" if not saved and global_value is not None and expected_enabled else "false"
            assert connection.execute(
                "SELECT value FROM settings WHERE name = 'update_use_proxy'"
            ).fetchone() == (expected_update,), case
            assert connection.execute(
                "SELECT name FROM settings WHERE name IN "
                "('proxy_enabled', 'proxy_url', 'proxy_bypass', 'proxy_force_http1')"
            ).fetchall() == [], case
