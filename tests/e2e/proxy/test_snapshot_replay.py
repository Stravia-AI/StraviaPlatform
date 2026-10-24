from __future__ import annotations

import hashlib
import http.client
import json
import sqlite3
import threading
from http.server import ThreadingHTTPServer
from pathlib import Path
from types import SimpleNamespace
from urllib.parse import urlsplit

import pytest

from tests.common.helpers import encode_storage_json
from tests.common.measure_wasm_snapshot_http import Replay
from tests.common.prepare_wasm_evaluation import prepare


@pytest.mark.proxy
def test_unrecorded_background_request_preserves_inference_response(tmp_path: Path) -> None:
    payload = b"recorded inference response"
    chunk = tmp_path / "response.bin"
    chunk.write_bytes(payload)
    server = ThreadingHTTPServer(("127.0.0.1", 0), Replay)
    server.replay_lock = threading.Lock()
    server.groups = [[{
        "status": 200,
        "headers": [["content-type", "application/octet-stream"]],
        "request_path": "/infer",
        "file": str(chunk),
    }]]
    server.received, server.position, server.disconnects = [], 0, 0
    thread = threading.Thread(target=server.serve_forever)
    thread.start()

    def post(path: str) -> tuple[int, bytes]:
        connection = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=5)
        try:
            connection.request("POST", path, body=b"request")
            response = connection.getresponse()
            return response.status, response.read()
        finally:
            connection.close()

    try:
        assert post("/background-model-discovery")[0] == 502
        assert post("/infer") == (200, payload)
        assert post("/infer")[0] == 502
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


def _prepare_snapshot(tmp_path: Path) -> SimpleNamespace:
    wasm = b"\0asm"
    digest = hashlib.sha256(wasm).hexdigest()
    components = tmp_path / "components"
    components.mkdir()
    (components / f"{digest}.wasm").write_bytes(wasm)

    database = tmp_path / "db" / "gateway.db"
    database.parent.mkdir()
    connection = sqlite3.connect(database)
    connection.executescript("""
        CREATE TABLE inference_run_observations(
            id TEXT PRIMARY KEY, generation_node_id TEXT, ingress_protocol TEXT, interaction_id TEXT);
        CREATE TABLE interaction_observations(id TEXT PRIMARY KEY, api_key_id TEXT);
        CREATE TABLE providers(
            id TEXT PRIMARY KEY, vendor TEXT, preset_key TEXT, channel TEXT, base_url TEXT,
            vendor_options TEXT, adapter_credentials TEXT, auth_mode TEXT, api_key TEXT);
        CREATE TABLE provider_oauth_credentials(
            provider_id TEXT PRIMARY KEY, driver_key TEXT, scheme TEXT, scopes TEXT, meta TEXT,
            resource_url TEXT, subject_id TEXT);
        CREATE TABLE provider_models(provider_id TEXT, model_id TEXT, metadata_json TEXT);
        CREATE TABLE vendor_plugins(vendor_id TEXT PRIMARY KEY, digest TEXT, descriptor TEXT);
        CREATE TABLE turn_chain_nodes(
            id TEXT PRIMARY KEY, parent_id TEXT, kind TEXT, principal TEXT, payload_version INTEGER,
            storage_format INTEGER, payload BLOB);
        CREATE TABLE turn_chain_contents(
            id INTEGER PRIMARY KEY, principal TEXT, content_key TEXT, content BLOB);
        CREATE TABLE turn_chain_node_contents(node_id TEXT, principal TEXT, content_id INTEGER);
    """)
    payload = {
        "client_delta": {"messages": [{"role": "user", "content": "fixture business context"}]},
        "effective_output": {
            "id": "fixture-response", "model": "fixture-model", "usage": {}, "vendor": {},
            "items": [{"type": "message", "role": "assistant",
                       "content": [{"type": "output_text", "text": "recorded reply"}]}],
        },
        "effective_request": {"model": "recorded-model"},
        "effective_system": None,
        "effective_history_mutation": None,
    }
    connection.execute(
        "INSERT INTO turn_chain_nodes VALUES(?,?,?,?,?,?,?)",
        ("generation-1", None, "response", "fixture-owner", 6, 2,
         encode_storage_json({"data": payload, "slots": [], "contents": []})),
    )
    connection.execute("INSERT INTO inference_run_observations VALUES(?,?,?,?)",
                       ("run-1", "generation-1", "openai-chat", "interaction-1"))
    connection.execute("INSERT INTO interaction_observations VALUES(?,?)", ("interaction-1", "api-key-1"))
    connection.execute(
        "INSERT INTO providers(id,vendor,preset_key,channel,base_url,vendor_options,"
        "adapter_credentials,auth_mode,api_key) VALUES(?,?,?,?,?,?,?,?,?)",
        ("provider-1", "fixture-vendor", "fixture-preset", "default",
         "https://fixture-user:fixture-password@fixture.invalid:8443/private/upstream",
         json.dumps({
             "region": "east",
             "resourceName": "fixture-resource",
             "apiKey": "fixture-option-api-key",
             "websocket_url": "wss://fixture.invalid:8443/socket",
             "baseUrl": "https://fixture-user:fixture-password@[2001:db8::1]:9443/options"
                        "?token=fixture-query-secret",
         }),
         json.dumps({
             "apiKey": "fixture-real-api-key",
             "endpoint_url": "https://fixture-user:fixture-password@fixture.invalid:7443/credentials/path",
         }),
         "oauth", None),
    )
    connection.execute(
        "INSERT INTO provider_oauth_credentials VALUES(?,?,?,?,?,?,?)",
        ("provider-1", "fixture-driver", "fixture-scheme", json.dumps(["scope:read"]),
         json.dumps({"baseUrl": "https://fixture-user:fixture-password@fixture.invalid:6443/meta/path",
                     "raw": {"nested": "fixture-real-token"}}),
         "https://fixture-user:fixture-password@fixture.invalid:5443/resource/path", "fixture-subject"),
    )
    connection.execute(
        "INSERT INTO provider_models VALUES(?,?,?)",
        ("provider-1", "fixture-model",
         json.dumps({"family": "fixture-family", "selector": "fixture-selector",
                     "capabilities": ["chat"],
                     "modalities": {"input": ["text"], "output": ["text"]}})),
    )
    connection.execute(
        "INSERT INTO vendor_plugins VALUES(?,?,?)",
        ("fixture-vendor", digest, json.dumps({"providers": [{"provider_id": "fixture-vendor"}]})),
    )
    connection.commit()
    connection.close()

    traces = tmp_path / "traces"
    (traces / "run-1").mkdir(parents=True)
    (traces / "run-1" / "manifest.json").write_text(
        json.dumps({"status": "complete", "run_id": "run-1"}), encoding="utf-8")
    selection = tmp_path / "selection.json"
    selection.write_text(json.dumps([{
        "run_id": "run-1", "vendor": "fixture-vendor", "id": "attempt-1",
        "provider_id": "provider-1", "protocol": "openai-compatible",
        "upstream_model": "fixture-model", "input_tokens": 7, "output_tokens": 9,
    }]), encoding="utf-8")
    return SimpleNamespace(database=database, traces=traces, components=components,
                           selection=selection, output=tmp_path / "output")


@pytest.mark.proxy
def test_prepare_replaces_url_credentials_and_preserves_replay_path(tmp_path: Path) -> None:
    args = _prepare_snapshot(tmp_path)
    result = prepare(args)
    assert result["failures"] == []

    directory = args.output / "case-01"
    provider = json.loads((directory / "provider.json").read_text(encoding="utf-8"))
    encoded = json.dumps(provider)
    assert "fixture-user" not in encoded
    assert "fixture-password" not in encoded
    assert "fixture-real-api-key" not in encoded
    assert "fixture-real-token" not in encoded
    assert "fixture-query-secret" not in encoded
    assert "fixture-option-api-key" not in encoded

    base_url = "https://fixture.invalid:8443/private/upstream"
    assert provider["base_url"] == base_url
    assert provider["operation_metadata"]["egress_base_url"] == base_url
    replay_target = urlsplit(provider["base_url"])
    assert (replay_target.hostname, replay_target.port, replay_target.path) == (
        "fixture.invalid", 8443, "/private/upstream")
    assert provider["options"]["region"] == "east"
    assert provider["options"]["resourceName"] == "fixture-resource"
    assert provider["options"]["apiKey"] == "benchmark-local-only"
    assert provider["options"]["websocket_url"] == "wss://fixture.invalid:8443/socket"
    assert provider["options"]["baseUrl"] == "https://[2001:db8::1]:9443/options"
    assert provider["credentials"]["endpoint_url"] == "https://fixture.invalid:7443/credentials/path"
    assert provider["credentials"]["baseUrl"] == "https://fixture.invalid:6443/meta/path"
    assert provider["credentials"]["resource_url"] == "https://fixture.invalid:5443/resource/path"
    assert provider["credentials"]["apiKey"] == "benchmark-local-only"
    assert provider["credentials"]["access_token"] == "benchmark-local-only"
    assert provider["credentials"]["driver_key"] == "fixture-driver"
    assert provider["credentials"]["scheme"] == "fixture-scheme"
    assert provider["credentials"]["scopes"] == ["scope:read"]
    assert provider["provider_id"] == "fixture-vendor"
    assert provider["protocol"] == "openai-compatible"
    assert provider["operation_metadata"]["egress_protocol"] == "openai-compatible"
    assert provider["model"] == "fixture-model"
    assert provider["channel"] == "default"

    request = json.loads((directory / "request.json").read_text(encoding="utf-8"))
    response = json.loads((directory / "response.json").read_text(encoding="utf-8"))
    assert request["items"][0]["content"] == "fixture business context"
    assert response["items"][0]["content"][0]["text"] == "recorded reply"

    summary = json.loads((args.output.parent / "corpus-summary.json").read_text(encoding="utf-8"))
    assert summary["real_credentials_replaced"] is True
    assert "fixture-password" not in json.dumps(summary)
