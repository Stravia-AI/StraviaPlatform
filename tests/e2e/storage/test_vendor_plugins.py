from __future__ import annotations

import hashlib
import json
import time
from pathlib import Path
from typing import Any
from urllib.error import HTTPError
from urllib.request import Request, urlopen

import pytest

from tests.common.helpers import (
    WebSession,
    find_free_port,
    http_request,
    initialize_server,
    start_stravia_server,
    stop_stravia_server,
    wait_for_setup_token,
    wait_until_ready,
)

LIFECYCLE_VENDOR = "fixture.lifecycle"
OTHER_VENDOR = "fixture.lifecycle.other"


def _request_component(
    session: WebSession, method: str, path: str, component: bytes
) -> tuple[int, Any]:
    headers = {
        **session.auth_headers(),
        "content-type": "application/wasm",
    }
    request = Request(
        f"{session.origin}{path}", method=method, data=component, headers=headers
    )
    try:
        with urlopen(request, timeout=30) as response:
            status = int(response.status)
            raw = response.read()
    except HTTPError as error:
        status = int(error.code)
        raw = error.read()
    return status, json.loads(raw.decode("utf-8"))


def _preview_plugin(
    session: WebSession, fixtures: Path, artifact: str
) -> dict[str, Any]:
    component = fixtures / artifact
    assert component.is_file(), (
        f"missing real vendor fixture {component}; run task build:vendor-fixtures"
    )
    status, body = _request_component(
        session, "POST", "/api/v1/vendor-plugins/import", component.read_bytes()
    )
    assert status == 200, body
    return body["data"]


def _confirm_plugin(
    session: WebSession, preview_id: str, *, allow_data_discard: bool
) -> tuple[int, Any]:
    return session.request(
        "POST",
        "/api/v1/vendor-plugins/confirm",
        {
            "preview_id": preview_id,
            "allow_data_discard": allow_data_discard,
        },
    )


def _install_plugin(
    session: WebSession, fixtures: Path, artifact: str
) -> dict[str, Any]:
    preview = _preview_plugin(session, fixtures, artifact)
    status, body = _confirm_plugin(
        session, preview["id"], allow_data_discard=False
    )
    assert status == 200, body
    assert body["data"]["source"] == "local"
    return body["data"]


def _plugins(session: WebSession) -> dict[str, dict[str, Any]]:
    status, body = session.request("GET", "/api/v1/vendor-plugins")
    assert status == 200, body
    return {plugin["vendor_id"]: plugin for plugin in body["data"]}


def _create_connection(
    session: WebSession,
    *,
    vendor: str,
    name: str,
    model_id: str,
    base_url: str,
    secret: str,
) -> dict[str, str]:
    status, body = session.request(
        "POST",
        "/api/v1/providers",
        {
            "name": name,
            "source": {
                "type": "custom",
                "vendor": vendor,
                "channel": "default",
                "protocol": "fixture-lifecycle",
                "base_url": base_url,
            },
            "credential": {"type": "fields", "values": {"apiKey": secret}},
            "vendor_options": {"mode": "base"},
        },
    )
    assert status == 200, body
    provider_id = body["data"]["id"]

    status, body = session.request(
        "POST",
        f"/api/v1/providers/{provider_id}/models",
        {
            "model_id": "fixture-model",
            "metadata": {
                "id": "fixture-model",
                "name": "Fixture model",
                "tool_call": True,
            },
        },
    )
    assert status == 201, body

    status, body = session.request(
        "POST",
        "/api/v1/models",
        {
            "model_id": model_id,
            "display_name": f"{name} route",
            "targets": [{"provider_id": provider_id, "model": "fixture-model"}],
        },
    )
    assert status == 200, body
    return {
        "provider_id": provider_id,
        "route_id": body["data"]["id"],
        "model_id": model_id,
    }


def _create_api_key(session: WebSession, route_ids: list[str]) -> dict[str, str]:
    status, body = session.request(
        "POST",
        "/api/v1/api-keys",
        {"name": "vendor lifecycle storage key", "model_ids": route_ids},
    )
    assert status == 200, body
    return {"id": body["data"]["id"], "key": body["data"]["key"]}


def _invoke(
    base_url: str,
    upstream: Any,
    api_key: str,
    model_id: str,
    *,
    version: str,
    vendor: str,
    state_before: int,
    secret: str,
) -> None:
    status, body = http_request(
        "POST",
        f"{base_url}/v1/responses",
        payload={"model": model_id, "input": "exercise persisted plugin state"},
        headers={"authorization": f"Bearer {api_key}"},
        timeout=30,
    )
    assert status == 200, body
    request = upstream.next_request()
    assert request["method"] == "POST"
    assert request["path"] == "/infer"
    assert request["headers"]["x-fixture-version"] == version
    assert request["headers"]["x-fixture-vendor"] == vendor
    assert request["headers"]["x-state-before"] == str(state_before)
    assert request["headers"]["authorization"] == f"Bearer {secret}"
    serialized = json.dumps(body)
    assert secret not in serialized


def _provider_fact(session: WebSession, provider_id: str) -> dict[str, Any]:
    status, body = session.request("GET", f"/api/v1/providers/{provider_id}")
    assert status == 200, body
    provider = body["data"]
    return {
        key: provider[key]
        for key in (
            "id",
            "name",
            "vendor",
            "channel",
            "protocol",
            "base_url",
            "vendor_options",
            "configured_credential_fields",
            "is_enabled",
        )
    }


def _route_fact(session: WebSession, model_id: str) -> dict[str, Any]:
    status, body = session.request("GET", f"/api/v1/models/{model_id}")
    assert status == 200, body
    route = body["data"]
    return {
        "id": route["id"],
        "model_id": route["model_id"],
        "display_name": route["display_name"],
        "target_provider": route["target_provider"],
        "target_model": route.get("target_model"),
        "is_enabled": route["is_enabled"],
        "targets": [
            {
                "id": target["id"],
                "provider_id": target["provider_id"],
                "model": target.get("model"),
                "enabled": target["enabled"],
                "priority": target["priority"],
            }
            for target in route["targets"]
        ],
    }


def _wait_for_api_key_usage(
    session: WebSession, api_key_id: str, minimum_requests: int
) -> dict[str, Any]:
    # 每次夹具调用确认一个输出 Token；请求行可能先于完成用量落库。
    deadline = time.monotonic() + 15
    last: Any = None
    while time.monotonic() < deadline:
        status, body = session.request("GET", "/api/v1/stats/api-keys")
        if status == 200:
            last = next(
                (
                    item
                    for item in body.get("data", [])
                    if item.get("api_key_id") == api_key_id
                ),
                None,
            )
            if (
                last is not None
                and last.get("request_count", 0) >= minimum_requests
                and last.get("total_output_tokens") is not None
                and last["total_output_tokens"] >= minimum_requests
            ):
                return last
        time.sleep(0.1)
    raise AssertionError(
        f"usage projection did not commit {minimum_requests} requests and output tokens: {last!r}"
    )


def _start_initialized_server(
    stravia_binary: Path,
    data_dir: Path,
    port: int,
    database: dict[str, Any],
) -> tuple[Any, list[str], WebSession]:
    base_url = f"http://127.0.0.1:{port}"
    process, logs = start_stravia_server(
        stravia_binary=stravia_binary,
        args=["--data-dir", str(data_dir), "--host", "127.0.0.1", "--port", str(port)],
    )
    try:
        wait_until_ready(f"{base_url}/api/v1/auth/state")
        session = initialize_server(base_url, wait_for_setup_token(logs, process), database)
    except BaseException:
        stop_stravia_server(process, logs)
        raise
    return process, logs, session


def _restart_server(
    stravia_binary: Path, data_dir: Path, port: int
) -> tuple[Any, list[str], WebSession]:
    base_url = f"http://127.0.0.1:{port}"
    process, logs = start_stravia_server(
        stravia_binary=stravia_binary,
        args=["--data-dir", str(data_dir), "--host", "127.0.0.1", "--port", str(port)],
    )
    try:
        wait_until_ready(f"{base_url}/api/v1/auth/state")
        session = WebSession(base_url)
        status, body = session.request(
            "POST",
            "/api/v1/auth/login",
            {"username": "admin", "password": "correct horse battery staple"},
        )
        assert status == 200, body
    except BaseException:
        stop_stravia_server(process, logs)
        raise
    return process, logs, session


@pytest.mark.e2e
@pytest.mark.storage
@pytest.mark.parametrize("backend", ["sqlite", "postgres"], ids=["sqlite", "postgres"])
def test_real_vendor_plugin_lifecycle_is_equivalent_across_storage_backends(
    stravia_binary: Path,
    repo_root: Path,
    storage_runtime: dict[str, object],
    lifecycle_upstream: Any,
    tmp_path: Path,
    backend: str,
) -> None:
    pg_url = storage_runtime["pg_url"]
    schema: str | None = None
    database: dict[str, Any] = {"backend": "sqlite"}

    fixtures = repo_root / "target" / "vendor-test-fixtures"
    data_dir = tmp_path / backend
    port = find_free_port()
    base_url = f"http://127.0.0.1:{port}"
    process = None
    logs: list[str] = []

    first_secret = "fixture-first-credential"
    second_secret = "fixture-second-credential"
    other_secret = "fixture-other-credential"

    try:
        if backend == "postgres":
            assert isinstance(pg_url, str) and pg_url, (
                "postgres vendor-plugin storage coverage requires the explicitly injected DB_URL"
            )
            new_schema = storage_runtime["make_isolated_schema"]("stravia_vendor_plugins")  # type: ignore[operator]
            storage_runtime["run_schema_action"](  # type: ignore[operator]
                "create",
                work_dir=storage_runtime["work_dir"],
                pg_url=pg_url,
                schema=new_schema,
            )
            schema = new_schema
            database = {
                "backend": "postgres",
                "url": storage_runtime["postgres_dsn_for_schema"](pg_url, schema),  # type: ignore[operator]
            }

        process, logs, session = _start_initialized_server(
            stravia_binary, data_dir, port, database
        )
        initial_plugins = _plugins(session)
        assert {
            vendor_id
            for vendor_id, plugin in initial_plugins.items()
            if plugin["source"] == "builtin"
        } == {"base"}
        assert initial_plugins["base"]["status"] == "ready"
        assert not list((data_dir / "plugins").rglob("*.wasm"))
        stop_stravia_server(process, logs)
        process = None
        process, logs, session = _restart_server(stravia_binary, data_dir, port)
        assert _plugins(session)["base"]["status"] == "ready"
        assert not list((data_dir / "plugins").rglob("*.wasm"))
        installed = _install_plugin(session, fixtures, "lifecycle-v1.wasm")
        assert (installed["vendor_id"], installed["version"]) == (
            LIFECYCLE_VENDOR,
            "1.0.0",
        )
        other_installed = _install_plugin(
            session, fixtures, "lifecycle-other-v1.wasm"
        )
        assert (other_installed["vendor_id"], other_installed["version"]) == (
            OTHER_VENDOR,
            "1.0.0",
        )
        artifact_dir = data_dir / "plugins" / "artifacts"
        artifact_files = list(artifact_dir.iterdir())
        assert len(artifact_files) == 2
        assert all(
            artifact.is_file()
            and artifact.suffix == ".wasm"
            and len(artifact.stem) == 64
            and all(character in "0123456789abcdef" for character in artifact.stem)
            for artifact in artifact_files
        )

        first = _create_connection(
            session,
            vendor=LIFECYCLE_VENDOR,
            name="first lifecycle account",
            model_id=f"{backend}-lifecycle-first",
            base_url=lifecycle_upstream.base_url,
            secret=first_secret,
        )
        second = _create_connection(
            session,
            vendor=LIFECYCLE_VENDOR,
            name="second lifecycle account",
            model_id=f"{backend}-lifecycle-second",
            base_url=lifecycle_upstream.base_url,
            secret=second_secret,
        )
        other = _create_connection(
            session,
            vendor=OTHER_VENDOR,
            name="unrelated lifecycle account",
            model_id=f"{backend}-lifecycle-other",
            base_url=lifecycle_upstream.base_url,
            secret=other_secret,
        )
        api_key = _create_api_key(
            session, [first["route_id"], second["route_id"], other["route_id"]]
        )

        _invoke(
            base_url,
            lifecycle_upstream,
            api_key["key"],
            first["model_id"],
            version="1.0.0",
            vendor=LIFECYCLE_VENDOR,
            state_before=0,
            secret=first_secret,
        )
        _invoke(
            base_url,
            lifecycle_upstream,
            api_key["key"],
            second["model_id"],
            version="1.0.0",
            vendor=LIFECYCLE_VENDOR,
            state_before=0,
            secret=second_secret,
        )
        _invoke(
            base_url,
            lifecycle_upstream,
            api_key["key"],
            first["model_id"],
            version="1.0.0",
            vendor=LIFECYCLE_VENDOR,
            state_before=1,
            secret=first_secret,
        )
        _invoke(
            base_url,
            lifecycle_upstream,
            api_key["key"],
            other["model_id"],
            version="1.0.0",
            vendor=OTHER_VENDOR,
            state_before=0,
            secret=other_secret,
        )

        # Windows 的 terminate 会直接终止进程；先确认异步统计已提交，再验证重启保留。
        assert _wait_for_api_key_usage(session, api_key["id"], 4)["request_count"] == 4
        stop_stravia_server(process, logs)
        process = None
        process, logs, session = _restart_server(stravia_binary, data_dir, port)
        plugins = _plugins(session)
        assert (plugins[LIFECYCLE_VENDOR]["version"], plugins[LIFECYCLE_VENDOR]["source"]) == (
            "1.0.0",
            "local",
        )
        assert (plugins[OTHER_VENDOR]["version"], plugins[OTHER_VENDOR]["source"]) == (
            "1.0.0",
            "local",
        )
        _invoke(
            base_url,
            lifecycle_upstream,
            api_key["key"],
            first["model_id"],
            version="1.0.0",
            vendor=LIFECYCLE_VENDOR,
            state_before=2,
            secret=first_secret,
        )
        _invoke(
            base_url,
            lifecycle_upstream,
            api_key["key"],
            second["model_id"],
            version="1.0.0",
            vendor=LIFECYCLE_VENDOR,
            state_before=1,
            secret=second_secret,
        )
        _invoke(
            base_url,
            lifecycle_upstream,
            api_key["key"],
            other["model_id"],
            version="1.0.0",
            vendor=OTHER_VENDOR,
            state_before=1,
            secret=other_secret,
        )
        usage_before_update = _wait_for_api_key_usage(session, api_key["id"], 7)

        compatible = _preview_plugin(session, fixtures, "lifecycle-v2.wasm")
        assert compatible["previous_version"] == "1.0.0"
        assert compatible["new_version"] == "2.0.0"
        assert compatible["discarded_data"] == []
        status, body = _confirm_plugin(
            session, compatible["id"], allow_data_discard=False
        )
        assert status == 200, body
        assert body["data"]["version"] == "2.0.0"
        _invoke(
            base_url,
            lifecycle_upstream,
            api_key["key"],
            first["model_id"],
            version="2.0.0",
            vendor=LIFECYCLE_VENDOR,
            state_before=3,
            secret=first_secret,
        )

        provider_facts = {
            connection["provider_id"]: _provider_fact(session, connection["provider_id"])
            for connection in (first, second, other)
        }
        route_facts = {
            connection["model_id"]: _route_fact(session, connection["model_id"])
            for connection in (first, second, other)
        }

        incompatible = _preview_plugin(session, fixtures, "lifecycle-v3.wasm")
        assert incompatible["previous_version"] == "2.0.0"
        assert incompatible["new_version"] == "3.0.0"
        assert incompatible["cancels_active_operations"] is True
        discarded = {
            item["provider"]["id"]: item["kinds"]
            for item in incompatible["discarded_data"]
        }
        assert discarded == {
            first["provider_id"]: ["private_state"],
            second["provider_id"]: ["private_state"],
        }
        assert _plugins(session)[LIFECYCLE_VENDOR]["version"] == "2.0.0"
        _invoke(
            base_url,
            lifecycle_upstream,
            api_key["key"],
            first["model_id"],
            version="2.0.0",
            vendor=LIFECYCLE_VENDOR,
            state_before=4,
            secret=first_secret,
        )

        status, body = _confirm_plugin(
            session, incompatible["id"], allow_data_discard=False
        )
        assert status == 400, body
        assert _plugins(session)[LIFECYCLE_VENDOR]["version"] == "2.0.0"
        _invoke(
            base_url,
            lifecycle_upstream,
            api_key["key"],
            second["model_id"],
            version="2.0.0",
            vendor=LIFECYCLE_VENDOR,
            state_before=2,
            secret=second_secret,
        )
        assert {
            provider_id: _provider_fact(session, provider_id)
            for provider_id in provider_facts
        } == provider_facts
        assert {
            model_id: _route_fact(session, model_id) for model_id in route_facts
        } == route_facts

        status, body = _confirm_plugin(
            session, incompatible["id"], allow_data_discard=True
        )
        assert status == 200, body
        assert body["data"]["version"] == "3.0.0"
        _invoke(
            base_url,
            lifecycle_upstream,
            api_key["key"],
            first["model_id"],
            version="3.0.0",
            vendor=LIFECYCLE_VENDOR,
            state_before=0,
            secret=first_secret,
        )
        _invoke(
            base_url,
            lifecycle_upstream,
            api_key["key"],
            second["model_id"],
            version="3.0.0",
            vendor=LIFECYCLE_VENDOR,
            state_before=0,
            secret=second_secret,
        )
        _invoke(
            base_url,
            lifecycle_upstream,
            api_key["key"],
            other["model_id"],
            version="1.0.0",
            vendor=OTHER_VENDOR,
            state_before=2,
            secret=other_secret,
        )
        assert {
            provider_id: _provider_fact(session, provider_id)
            for provider_id in provider_facts
        } == provider_facts
        assert {
            model_id: _route_fact(session, model_id) for model_id in route_facts
        } == route_facts
        assert _plugins(session)[OTHER_VENDOR]["version"] == "1.0.0"
        _wait_for_api_key_usage(
            session,
            api_key["id"],
            usage_before_update["request_count"] + 6,
        )

        stop_stravia_server(process, logs)
        process = None
        process, logs, session = _restart_server(stravia_binary, data_dir, port)
        plugins = _plugins(session)
        assert (plugins[LIFECYCLE_VENDOR]["version"], plugins[LIFECYCLE_VENDOR]["source"]) == (
            "3.0.0",
            "local",
        )
        assert (plugins[OTHER_VENDOR]["version"], plugins[OTHER_VENDOR]["source"]) == (
            "1.0.0",
            "local",
        )
        _invoke(
            base_url,
            lifecycle_upstream,
            api_key["key"],
            first["model_id"],
            version="3.0.0",
            vendor=LIFECYCLE_VENDOR,
            state_before=1,
            secret=first_secret,
        )
        _invoke(
            base_url,
            lifecycle_upstream,
            api_key["key"],
            other["model_id"],
            version="1.0.0",
            vendor=OTHER_VENDOR,
            state_before=3,
            secret=other_secret,
        )
        assert {
            provider_id: _provider_fact(session, provider_id)
            for provider_id in provider_facts
        } == provider_facts
        assert {
            model_id: _route_fact(session, model_id) for model_id in route_facts
        } == route_facts
        usage_after_restart = _wait_for_api_key_usage(
            session,
            api_key["id"],
            usage_before_update["request_count"] + 8,
        )
        assert (
            usage_after_restart["request_count"],
            usage_after_restart["total_output_tokens"],
        ) == (15, 15)
        assert usage_after_restart["api_key_name"] == usage_before_update["api_key_name"]

        # 缺失产物的异常插件仍须可卸载；管理操作不能依赖组件成功加载。
        stop_stravia_server(process, logs)
        process = None
        digest = hashlib.sha256((fixtures / "lifecycle-v3.wasm").read_bytes()).hexdigest()
        (artifact_dir / f"{digest}.wasm").unlink()
        process, logs, session = _restart_server(stravia_binary, data_dir, port)
        assert _plugins(session)[LIFECYCLE_VENDOR]["status"] == "unavailable"
        # configured_credential_fields 依赖已加载描述符；组件不可用时它为空，不代表凭据被删除。
        unavailable_provider_facts = {
            provider_id: _provider_fact(session, provider_id)
            for provider_id in provider_facts
        }

        status, body = session.request("DELETE", "/api/v1/vendor-plugins/base")
        assert status == 400, body
        assert _plugins(session)["base"]["status"] == "ready"
        status, body = session.request(
            "DELETE", f"/api/v1/vendor-plugins/{LIFECYCLE_VENDOR}"
        )
        assert status == 200, body
        assert LIFECYCLE_VENDOR not in _plugins(session)
        assert {
            provider_id: _provider_fact(session, provider_id)
            for provider_id in provider_facts
        } == unavailable_provider_facts
        assert {
            model_id: _route_fact(session, model_id) for model_id in route_facts
        } == route_facts
        status, body = http_request(
            "POST",
            f"{base_url}/v1/responses",
            payload={"model": first["model_id"], "input": "uninstalled plugin"},
            headers={"authorization": f"Bearer {api_key['key']}"},
        )
        assert status >= 400, body
        _invoke(
            base_url,
            lifecycle_upstream,
            api_key["key"],
            other["model_id"],
            version="1.0.0",
            vendor=OTHER_VENDOR,
            state_before=4,
            secret=other_secret,
        )
        stop_stravia_server(process, logs)
        process = None
        process, logs, session = _restart_server(stravia_binary, data_dir, port)
        assert LIFECYCLE_VENDOR not in _plugins(session)
        assert _provider_fact(session, first["provider_id"]) == unavailable_provider_facts[first["provider_id"]]
        assert _route_fact(session, first["model_id"]) == route_facts[first["model_id"]]
        reinstall = _preview_plugin(session, fixtures, "lifecycle-v3.wasm")
        retained_data = {
            item["provider"]["id"]: item["kinds"]
            for item in reinstall["discarded_data"]
        }
        for connection in (first, second):
            assert "credentials" in retained_data[connection["provider_id"]]
            assert "private_state" in retained_data[connection["provider_id"]]
    finally:
        if process is not None:
            stop_stravia_server(process, logs)
        if schema is not None:
            storage_runtime["run_schema_action"](  # type: ignore[operator]
                "drop",
                work_dir=storage_runtime["work_dir"],
                pg_url=pg_url,
                schema=schema,
            )
