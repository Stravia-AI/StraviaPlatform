from __future__ import annotations

import hashlib
from pathlib import Path
from typing import Any

import pytest

from tests.common.helpers import (
    WebSession,
    find_free_port,
    http_bytes,
    initialize_server,
    start_stravia_server,
    stop_stravia_server,
    wait_for_setup_token,
    wait_until_ready,
)
from tests.common.plugin_marketplace import MarketplaceFixture, REPOSITORY


@pytest.mark.e2e
@pytest.mark.storage
@pytest.mark.parametrize("backend", ["sqlite", "postgres"], ids=["sqlite", "postgres"])
def test_signed_marketplace_install_update_and_removed_source_preserve_origin(
    stravia_binary: Path,
    repo_root: Path,
    storage_runtime: dict[str, Any],
    tmp_path: Path,
    backend: str,
) -> None:
    fixtures = repo_root / "target/vendor-test-fixtures"
    assert (fixtures / "marketplace/v1/stravia-plugin-catalog.json").is_file(), "run task build:vendor-fixtures"
    upstream = MarketplaceFixture(fixtures / "marketplace")
    upstream.start()
    process = None
    logs: list[str] = []
    schema: str | None = None
    pg_url = storage_runtime["pg_url"]
    database: dict[str, Any] = {"backend": "sqlite"}
    try:
        if backend == "postgres":
            schema = storage_runtime["make_isolated_schema"]("stravia_marketplace")
            storage_runtime["run_schema_action"]("create", pg_url=pg_url, schema=schema)
            database = {"backend": "postgres", "url": storage_runtime["postgres_dsn_for_schema"](pg_url, schema)}
        port = find_free_port()
        base_url = f"http://127.0.0.1:{port}"
        process, logs = start_stravia_server(
            stravia_binary=stravia_binary,
            args=["--data-dir", str(tmp_path / backend), "--host", "127.0.0.1", "--port", str(port),
                  "--test-plugin-marketplace-url", upstream.base_url] + storage_runtime["server_args"](backend),
        )
        wait_until_ready(f"{base_url}/api/v1/auth/state")
        session = initialize_server(base_url, wait_for_setup_token(logs, process), database)
        status, headers, _ = http_bytes("GET", f"{base_url}/api/v1/plugin-sources")
        assert status == 401 and headers.get("cache-control") == "no-store"
        other_session = WebSession(base_url)
        status, body = other_session.request("POST", "/api/v1/auth/login", {"username": "admin", "password": "correct horse battery staple"})
        assert status == 200, body

        status, body = session.request("POST", "/api/v1/plugin-sources/preview", {"url": f"https://github.com/{REPOSITORY}"})
        assert status == 200, body
        source_preview = body["data"]
        assert source_preview["repository_id"] == 424242
        assert source_preview["repository"] == REPOSITORY
        status, body = other_session.request("POST", "/api/v1/plugin-sources", {"preview_id": source_preview["preview_id"]})
        assert status >= 400, body
        status, body = session.request("POST", "/api/v1/plugin-sources", {"preview_id": source_preview["preview_id"]})
        assert status == 200, body
        source_id = body["data"]["id"]

        status, body = session.request("POST", "/api/v1/plugin-marketplace/refresh", {"source_id": source_id})
        assert status == 200, body
        entry = next(entry for entry in body["data"]["entries"] if entry["source_id"] == source_id and entry["vendor_id"] == "fixture.lifecycle")
        release = entry["latest"]
        assert release["version"] == "1.0.0" and release["compatible"] is True
        request = {"source_id": source_id, "release_id": release["release_id"], "vendor_id": "fixture.lifecycle",
                   "version": release["version"], "catalog_sha256": release["catalog_sha256"]}
        status, body = session.request("POST", "/api/v1/plugin-marketplace/preview-install", request, timeout=120)
        assert status == 200, body
        preview = body["data"]
        assert preview["target_source"] == "marketplace"
        assert preview["origin"]["source_id"] == source_id
        status, body = other_session.request("POST", "/api/v1/vendor-plugins/confirm", {"preview_id": preview["id"], "allow_data_discard": False})
        assert status >= 400, body
        status, body = session.request("POST", "/api/v1/vendor-plugins/confirm", {"preview_id": preview["id"], "allow_data_discard": False}, timeout=120)
        assert status == 200, body
        installed = body["data"]
        assert installed["source"] == "marketplace" and installed["version"] == "1.0.0"
        assert installed["digest"] == hashlib.sha256((fixtures / "lifecycle-v1.wasm").read_bytes()).hexdigest()
        assert installed["origin"]["public_key_fingerprint"] == source_preview["fingerprint"]

        upstream.versions = [2, 1]
        status, body = session.request("POST", "/api/v1/plugin-marketplace/refresh", {"source_id": source_id})
        assert status == 200, body
        entry = next(entry for entry in body["data"]["entries"] if entry["source_id"] == source_id and entry["vendor_id"] == "fixture.lifecycle")
        release = entry["update"]
        assert release["version"] == "2.0.0"
        request.update({"release_id": release["release_id"], "version": release["version"], "catalog_sha256": release["catalog_sha256"]})

        upstream.corrupt_artifact = True
        status, body = session.request("POST", "/api/v1/plugin-marketplace/preview-install", request, timeout=120)
        assert status == 400 and body["error"]["code"] == "artifact_corrupt", body
        upstream.corrupt_artifact = False
        status, body = session.request("GET", "/api/v1/vendor-plugins")
        assert status == 200, body
        assert next(plugin for plugin in body["data"] if plugin["vendor_id"] == "fixture.lifecycle")["version"] == "1.0.0"

        status, body = session.request("POST", "/api/v1/plugin-marketplace/preview-install", request, timeout=120)
        assert status == 200, body
        preview = body["data"]
        assert preview["previous_version"] == "1.0.0" and preview["new_version"] == "2.0.0"
        status, body = session.request("PATCH", f"/api/v1/plugin-sources/{source_id}", {"enabled": False})
        assert status == 200, body
        status, body = session.request("POST", "/api/v1/vendor-plugins/confirm", {"preview_id": preview["id"], "allow_data_discard": False}, timeout=120)
        assert status >= 400, body
        status, body = session.request("PATCH", f"/api/v1/plugin-sources/{source_id}", {"enabled": True})
        assert status == 200, body
        status, body = session.request("POST", "/api/v1/plugin-marketplace/preview-install", request, timeout=120)
        assert status == 200, body
        status, body = session.request("POST", "/api/v1/vendor-plugins/confirm", {"preview_id": body["data"]["id"], "allow_data_discard": False}, timeout=120)
        assert status == 200, body
        assert body["data"]["version"] == "2.0.0"
        expected_origin = body["data"]["origin"]

        status, body = session.request("DELETE", f"/api/v1/plugin-sources/{source_id}")
        assert status == 200, body
        status, body = session.request("GET", "/api/v1/vendor-plugins")
        assert status == 200, body
        installed = next(plugin for plugin in body["data"] if plugin["vendor_id"] == "fixture.lifecycle")
        assert installed["version"] == "2.0.0"
        assert installed["origin"] == expected_origin
        assert installed["status"] == "ready"
        status, body = session.request("GET", "/api/v1/plugin-sources")
        assert status == 200 and all(source["id"] != source_id for source in body["data"]), body
    finally:
        if process is not None:
            stop_stravia_server(process, logs)
        upstream.close()
        if schema is not None:
            storage_runtime["run_schema_action"]("drop", pg_url=pg_url, schema=schema)


@pytest.mark.e2e
@pytest.mark.storage
def test_postgres_key_retrust_on_another_instance_invalidates_pending_installations(
    stravia_binary: Path,
    repo_root: Path,
    storage_runtime: dict[str, Any],
    tmp_path: Path,
) -> None:
    upstream = MarketplaceFixture(repo_root / "target/vendor-test-fixtures/marketplace")
    upstream.versions = [2]
    upstream.start()
    processes: list[tuple[Any, list[str]]] = []
    schema: str | None = None
    pg_url = storage_runtime["pg_url"]
    try:
        schema = storage_runtime["make_isolated_schema"]("stravia_market_trust")
        storage_runtime["run_schema_action"]("create", pg_url=pg_url, schema=schema)
        database = {"backend": "postgres", "url": storage_runtime["postgres_dsn_for_schema"](pg_url, schema)}
        sessions: list[WebSession] = []
        for index in range(2):
            port = find_free_port()
            base_url = f"http://127.0.0.1:{port}"
            config_args = ["--config", str(tmp_path / "node-0/server.toml")] if index else []
            process, logs = start_stravia_server(
                stravia_binary=stravia_binary,
                args=["--data-dir", str(tmp_path / f"node-{index}"), "--host", "127.0.0.1", "--port", str(port),
                      "--test-plugin-marketplace-url", upstream.base_url] + config_args + storage_runtime["server_args"]("postgres"),
            )
            processes.append((process, logs))
            wait_until_ready(f"{base_url}/readyz" if index else f"{base_url}/api/v1/auth/state")
            if index == 0:
                sessions.append(initialize_server(base_url, wait_for_setup_token(logs, process), database))
            else:
                session = WebSession(base_url)
                status, body = session.request("POST", "/api/v1/auth/login", {"username": "admin", "password": "correct horse battery staple"})
                assert status == 200, body
                sessions.append(session)
        first, second = sessions
        status, body = first.request("POST", "/api/v1/plugin-sources/preview", {"url": f"https://github.com/{REPOSITORY}"})
        assert status == 200, body
        status, body = first.request("POST", "/api/v1/plugin-sources", {"preview_id": body["data"]["preview_id"]})
        assert status == 200, body
        source_id = body["data"]["id"]
        status, body = first.request("POST", "/api/v1/plugin-marketplace/refresh", {"source_id": source_id})
        assert status == 200, body
        release = next(entry for entry in body["data"]["entries"] if entry["source_id"] == source_id)["latest"]
        request = {"source_id": source_id, "release_id": release["release_id"], "vendor_id": release["vendor_id"],
                   "version": release["version"], "catalog_sha256": release["catalog_sha256"]}
        status, body = first.request("POST", "/api/v1/plugin-marketplace/preview-install", request, timeout=120)
        assert status == 200, body
        pending = body["data"]["id"]

        upstream.rotated = True
        status, body = second.request("POST", "/api/v1/plugin-marketplace/refresh", {"source_id": source_id})
        assert status == 200, body
        assert next(source for source in body["data"]["sources"] if source["id"] == source_id)["error"]["code"] == "key_changed"
        status, body = second.request("POST", f"/api/v1/plugin-sources/{source_id}/trust-preview", {})
        assert status == 200, body
        trust = body["data"]
        status, body = second.request("POST", f"/api/v1/plugin-sources/{source_id}/trust-confirm", {"preview_id": trust["preview_id"]})
        assert status == 200, body
        status, body = first.request("POST", "/api/v1/vendor-plugins/confirm", {"preview_id": pending, "allow_data_discard": False})
        assert status == 400, body
        status, body = first.request("GET", "/api/v1/vendor-plugins")
        assert status == 200 and all(plugin["vendor_id"] != "fixture.lifecycle" for plugin in body["data"]), body

        status, body = first.request("GET", "/api/v1/plugin-marketplace")
        assert status == 200, body
        release = next(entry for entry in body["data"]["entries"] if entry["source_id"] == source_id)["latest"]
        request["catalog_sha256"] = release["catalog_sha256"]
        status, body = first.request("POST", "/api/v1/plugin-marketplace/preview-install", request, timeout=120)
        assert status == 200, body
        status, body = first.request("POST", "/api/v1/vendor-plugins/confirm", {"preview_id": body["data"]["id"], "allow_data_discard": False}, timeout=120)
        assert status == 200, body
        assert body["data"]["origin"]["public_key_fingerprint"] == trust["fingerprint"]
    finally:
        for process, logs in reversed(processes):
            stop_stravia_server(process, logs)
        upstream.close()
        if schema is not None:
            storage_runtime["run_schema_action"]("drop", pg_url=pg_url, schema=schema)
