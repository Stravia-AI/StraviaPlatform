from __future__ import annotations

import base64
import io
import json
import socket
import sqlite3
import tempfile
import threading
import time
import uuid
import zipfile
from contextlib import closing
from pathlib import Path
from typing import Any, Callable
from urllib.error import HTTPError
from urllib.parse import urlencode
from http.client import HTTPConnection
from urllib.request import Request, urlopen

import pytest

from tests.common.helpers import (
    WebSession,
    download_observation_bundle,
    encode_storage_json,
    find_free_port,
    http_bytes,
    http_request,
    initialize_server,
    minimal_mock_provider,
    start_stravia_server,
    stop_stravia_server,
    wait_for_setup_token,
    wait_until_ready,
)
from tests.e2e.admin.test_observations import (
    _client_errors,
    _create_route,
    _detail,
    _finished_detail,
    _forest,
    _proxy,
    _route_interactions,
    _sse_event,
    _tool_call_message,
    _upstream_errors,
)


def _wait_for(description: str, probe: Callable[[], Any], timeout: float = 10.0) -> Any:
    deadline = time.time() + timeout
    last: Any = None
    while time.time() < deadline:
        last = probe()
        if last:
            return last
        time.sleep(0.1)
    pytest.fail(f"timed out waiting for {description}; last={last!r}")


def _start_initialized(
    stravia_binary: Path, data_dir: Path, mock_url: str
) -> tuple[dict[str, Any], Any, list[str]]:
    port = find_free_port()
    process, logs = start_stravia_server(
        stravia_binary=stravia_binary,
        args=[
            "--host",
            "127.0.0.1",
            "--port",
            str(port),
            "--data-dir",
            str(data_dir),
        ],
    )
    try:
        base = f"http://127.0.0.1:{port}"
        wait_until_ready(f"{base}/api/v1/auth/state")
        session = initialize_server(
            base,
            wait_for_setup_token(logs, process),
            {"backend": "sqlite"},
        )
    except BaseException:
        stop_stravia_server(process, logs)
        raise
    return (
        {
            "admin": base,
            "proxy": base,
            "mock": mock_url,
            "auth": session.auth_headers(),
            "data_dir": data_dir,
            "logs": logs,
            "process": process,
        },
        process,
        logs,
    )


@pytest.mark.e2e
@pytest.mark.admin
def test_trace_storage_failure_is_partial_without_changing_inference_or_continuation(
    stravia_binary: Path,
) -> None:
    mock_port = find_free_port()
    mock_server, _ = minimal_mock_provider(mock_port)
    release_response = threading.Event()
    blocked_prompt = "response while trace storage is blocked"

    class GatedProvider(mock_server.RequestHandlerClass):
        def _read_body(self) -> dict[str, Any]:
            body = super()._read_body()
            if any(
                message.get("content") == blocked_prompt
                for message in body.get("messages", [])
            ):
                release_response.wait()
            return body

    mock_server.RequestHandlerClass = GatedProvider
    try:
        with tempfile.TemporaryDirectory(prefix="stravia-trace-failure-e2e-") as temporary:
            data_dir = Path(temporary)
            env, process, logs = _start_initialized(
                stravia_binary, data_dir, f"http://127.0.0.1:{mock_port}"
            )
            try:
                route_id, api_key = _create_route(env, "observation-delay-storage-failure")
                baseline_messages = [{"role": "user", "content": "baseline response"}]
                status, baseline = _proxy(
                    env,
                    api_key,
                    "observation-delay-storage-failure",
                    baseline_messages,
                )
                assert status == 200, baseline

                status, body = http_request(
                    "PUT",
                    f"{env['admin']}/api/v1/observations/debug",
                    payload={"enabled": True, "confirmed": True},
                    headers=env["auth"],
                )
                assert status == 200, body

                trace_root = data_dir / "diagnostics" / "observation-debug"
                displaced_root = data_dir / "observation-debug-before-failure"
                trace_root.rename(displaced_root)
                trace_root.write_bytes(b"regular file blocks managed trace directories")

                failed_trace_messages = [
                    {"role": "user", "content": blocked_prompt}
                ]
                # 新 User 输入需要在快速续接窗口之外才会开启新的 Interaction（CONTEXT.md 归并规则）。
                time.sleep(2.01)
                request_outcome: list[tuple[int, Any] | Exception] = []

                def blocked_trace_request() -> None:
                    try:
                        request_outcome.append(
                            _proxy(
                                env,
                                api_key,
                                "observation-delay-storage-failure",
                                failed_trace_messages,
                            )
                        )
                    except Exception as error:
                        request_outcome.append(error)

                worker = threading.Thread(target=blocked_trace_request)
                worker.start()

                def active_partial() -> tuple[dict[str, Any], dict[str, Any]] | None:
                    for interaction in _route_interactions(env, route_id):
                        if interaction["status"] != "running" or interaction["debug_status"] != "partial":
                            continue
                        detail = _detail(env, interaction["id"])
                        if not detail["runs"] or detail["runs"][0]["status"] != "running":
                            continue
                        run = detail["runs"][0]
                        assert run["debug_enabled"] is True
                        assert run["trace"] is None
                        assert not trace_root.is_dir()
                        return interaction, detail
                    return None

                failed_interaction, active_detail = _wait_for(
                    "partial debug API before inference completion",
                    active_partial,
                )
                assert request_outcome == []
                assert active_detail["runs"][0]["trace"] is None
                status, debug_state = http_request(
                    "GET",
                    f"{env['admin']}/api/v1/observations/debug",
                    headers=env["auth"],
                )
                assert status == 200, debug_state
                assert debug_state["data"]["partial_trace_count"] >= 1

                release_response.set()
                worker.join(timeout=10.0)
                assert len(request_outcome) == 1
                assert not isinstance(request_outcome[0], Exception)
                status, under_failure = request_outcome[0]
                assert status == 200, under_failure
                assert under_failure["choices"] == baseline["choices"]
                status, immediate_debug_state = http_request(
                    "GET",
                    f"{env['admin']}/api/v1/observations/debug",
                    headers=env["auth"],
                )
                assert status == 200, immediate_debug_state
                assert immediate_debug_state["data"]["partial_trace_count"] >= 1
                failed_detail = _wait_for(
                    "completed Run after partial Trace",
                    lambda: (lambda detail: detail if detail is not None
                        and detail["runs"][0]["status"] == "completed" else None)(
                        _finished_detail(env, failed_interaction["id"])
                    ),
                )
                assert failed_detail["runs"][0]["client_output_committed"] is True
                status, debug_state = http_request(
                    "GET",
                    f"{env['admin']}/api/v1/observations/debug",
                    headers=env["auth"],
                )
                assert status == 200, debug_state
                assert debug_state["data"]["partial_trace_count"] >= 1

                continuation_messages = failed_trace_messages + [
                    under_failure["choices"][0]["message"],
                    {"role": "user", "content": "continue after trace storage failure"},
                ]
                # 新 User 输入需要在快速续接窗口之外才会开启新的 Interaction（CONTEXT.md 归并规则）。
                time.sleep(2.01)
                status, continuation = _proxy(
                    env,
                    api_key,
                    "observation-delay-storage-failure",
                    continuation_messages,
                )
                assert status == 200, continuation
                assert continuation["choices"] == baseline["choices"]

                interactions = _wait_for(
                    "Generation-associated continuation",
                    lambda: (lambda items: items if len(items) >= 3 else None)(
                        _route_interactions(env, route_id)
                    ),
                )
                child = next(
                    interaction
                    for interaction in interactions
                    if interaction["parent_interaction_id"] == failed_interaction["id"]
                )
                child_detail = _wait_for(
                    "continued Run terminal state",
                    lambda: (lambda detail: detail if detail is not None
                        and detail["runs"][0]["status"] == "completed" else None)(
                        _finished_detail(env, child["id"])
                    ),
                )
                assert child_detail["runs"][0]["client_output_committed"] is True

                status, status_body = http_request(
                    "GET", f"{env['admin']}/api/v1/status", headers=env["auth"]
                )
                assert status == 200, status_body
                assert status_body["status"] == "running"
            finally:
                release_response.set()
                stop_stravia_server(process, logs)
    finally:
        release_response.set()
        mock_server.shutdown()
        mock_server.server_close()


@pytest.mark.e2e
@pytest.mark.admin
def test_bundle_ticket_is_invalidated_by_process_restart(stravia_binary: Path) -> None:
    mock_port = find_free_port()
    mock_server, _ = minimal_mock_provider(mock_port)
    try:
        with tempfile.TemporaryDirectory(prefix="stravia-ticket-restart-e2e-") as temporary:
            data_dir = Path(temporary)
            env, process, logs = _start_initialized(
                stravia_binary, data_dir, f"http://127.0.0.1:{mock_port}"
            )
            route_id = ""
            ticket_url = ""
            try:
                route_id, api_key = _create_route(env, "observation-ticket-restart")
                status, response = _proxy(
                    env,
                    api_key,
                    "observation-ticket-restart",
                    [{"role": "user", "content": "restart invalidates ticket"}],
                )
                assert status == 200, response
                interaction = _wait_for(
                    "ticket Interaction", lambda: _route_interactions(env, route_id)
                )[0]
                status, body = http_request(
                    "POST",
                    f"{env['admin']}/api/v1/observations/interactions/{interaction['id']}/debug-bundle-tickets",
                    payload={},
                    headers=env["auth"],
                )
                assert status == 200, body
                ticket_url = str(body["data"]["download_url"])
                if ticket_url.startswith("/"):
                    ticket_url = f"{env['admin']}{ticket_url}"
            finally:
                stop_stravia_server(process, logs)

            restart_port = find_free_port()
            restarted, restarted_logs = start_stravia_server(
                stravia_binary=stravia_binary,
                args=[
                    "--host",
                    "127.0.0.1",
                    "--port",
                    str(restart_port),
                    "--data-dir",
                    str(data_dir),
                ],
            )
            restart_base = f"http://127.0.0.1:{restart_port}"
            try:
                wait_until_ready(f"{restart_base}/api/v1/auth/state")
                old_path = "/" + ticket_url.split("/", 3)[-1]
                status, headers, body = http_bytes("GET", f"{restart_base}{old_path}")
                assert status == 404
                assert headers["cache-control"] == "no-store"
                assert body
                decoded = body.decode("utf-8")
                assert "bundle_unavailable" in decoded
                assert ticket_url.rsplit("/", 1)[-1] not in decoded

                session = WebSession(restart_base)
                status, login = session.request(
                    "POST",
                    "/api/v1/auth/login",
                    {"username": "admin", "password": "correct horse battery staple"},
                )
                assert status == 200, login
                status, forest = session.request(
                    "GET", f"/api/v1/observations/interactions?model={route_id}"
                )
                assert status == 200, forest
                assert forest["data"]["root_total"] == 1
            finally:
                stop_stravia_server(restarted, restarted_logs)
    finally:
        mock_server.shutdown()
        mock_server.server_close()


@pytest.mark.e2e
@pytest.mark.admin
def test_startup_reconciliation_completes_trace_tombstone(stravia_binary: Path) -> None:
    mock_port = find_free_port()
    mock_server, _ = minimal_mock_provider(mock_port)
    try:
        with tempfile.TemporaryDirectory(prefix="stravia-tombstone-restart-e2e-") as temporary:
            data_dir = Path(temporary)
            env, process, logs = _start_initialized(
                stravia_binary, data_dir, f"http://127.0.0.1:{mock_port}"
            )
            trace_id = ""
            try:
                status, body = http_request(
                    "PUT",
                    f"{env['admin']}/api/v1/observations/debug",
                    payload={"enabled": True, "confirmed": True},
                    headers=env["auth"],
                )
                assert status == 200, body
                route_id, api_key = _create_route(env, "observation-tombstone")
                status, response = _proxy(
                    env,
                    api_key,
                    "observation-tombstone",
                    [{"role": "user", "content": "persist trace before tombstone"}],
                )
                assert status == 200, response
                interaction = _wait_for(
                    "tombstone Interaction", lambda: _route_interactions(env, route_id)
                )[0]
                def finished_trace() -> dict[str, Any] | None:
                    value = _detail(env, interaction["id"])
                    trace = value["runs"][0].get("trace")
                    if not trace:
                        return None
                    path = data_dir / "diagnostics" / "observation-debug" / trace["trace_id"] / "manifest.json"
                    try:
                        manifest = json.loads(path.read_text(encoding="utf-8"))
                    except (OSError, json.JSONDecodeError):
                        return None
                    return value if manifest["completed_at"] is not None else None

                detail = _wait_for("finished Trace manifest", finished_trace)
                trace_id = detail["runs"][0]["trace"]["trace_id"]
            finally:
                stop_stravia_server(process, logs)

            trace_directory = data_dir / "diagnostics" / "observation-debug" / trace_id
            assert trace_directory.is_dir()
            manifest_path = trace_directory / "manifest.json"
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
            assert manifest["schema_version"] == 2
            assert manifest["trace_id"] == trace_id
            manifest["tombstoned"] = True
            manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
            deleting_directory = trace_directory.with_name(f".deleting-{trace_id}")
            trace_directory.rename(deleting_directory)

            restart_port = find_free_port()
            restarted, restarted_logs = start_stravia_server(
                stravia_binary=stravia_binary,
                args=[
                    "--host",
                    "127.0.0.1",
                    "--port",
                    str(restart_port),
                    "--data-dir",
                    str(data_dir),
                ],
            )
            restart_base = f"http://127.0.0.1:{restart_port}"
            try:
                wait_until_ready(f"{restart_base}/readyz")
                deadline = time.time() + 10.0
                while deleting_directory.exists() and time.time() < deadline:
                    time.sleep(0.1)
                assert not trace_directory.exists()
                assert not deleting_directory.exists()
                status, readiness = http_request("GET", f"{restart_base}/readyz")
                assert status == 200, readiness
            finally:
                stop_stravia_server(restarted, restarted_logs)
    finally:
        mock_server.shutdown()
        mock_server.server_close()


@pytest.mark.e2e
@pytest.mark.admin
def test_restart_interrupts_running_activity_and_pending_client_tools(
    stravia_binary: Path,
) -> None:
    mock_port = find_free_port()
    mock_server, _ = minimal_mock_provider(mock_port)
    received_request = threading.Event()
    release_request = threading.Event()
    blocked_prompt = "interrupt this live request across restart"

    class GatedProvider(mock_server.RequestHandlerClass):
        def _read_body(self) -> dict[str, Any]:
            body = super()._read_body()
            if any(
                message.get("content") == blocked_prompt
                for message in body.get("messages", [])
            ):
                received_request.set()
                release_request.wait()
            return body

        def do_POST(self) -> None:
            try:
                super().do_POST()
            except (BrokenPipeError, ConnectionResetError):
                # 被测试的进程已中断；门控释放后的上游响应没有接收方。
                pass

    mock_server.RequestHandlerClass = GatedProvider
    try:
        with tempfile.TemporaryDirectory(prefix="stravia-running-restart-e2e-") as temporary:
            data_dir = Path(temporary)
            env, process, logs = _start_initialized(
                stravia_binary, data_dir, f"http://127.0.0.1:{mock_port}"
            )
            running_id = ""
            waiting_id = ""
            request_outcome: list[object] = []
            try:
                waiting_route, waiting_key = _create_route(
                    env, "observation-branch-restart"
                )
                status, response = http_request(
                    "POST",
                    f"{env['proxy']}/v1/chat/completions",
                    payload={
                        "model": "observation-branch-restart",
                        "messages": [{"role": "user", "content": "observation-branch wait across restart"}],
                        "tools": [
                            {
                                "type": "function",
                                "function": {
                                    "name": "local_probe",
                                    "parameters": {"type": "object"},
                                },
                            }
                        ],
                    },
                    headers={"authorization": f"Bearer {waiting_key}"},
                )
                assert status == 200, response
                waiting = _wait_for(
                    "waiting-client before restart",
                    lambda: next(
                        (
                            item
                            for item in _route_interactions(env, waiting_route)
                            if item["status"] == "waiting_client"
                        ),
                        None,
                    ),
                )
                waiting_id = waiting["id"]

                running_route, running_key = _create_route(
                    env, "observation-delay-restart"
                )

                def delayed_request() -> None:
                    try:
                        request_outcome.append(
                            _proxy(
                                env,
                                running_key,
                                "observation-delay-restart",
                                [{"role": "user", "content": blocked_prompt}],
                            )
                        )
                    except Exception as error:  # The server is deliberately stopped mid-request.
                        request_outcome.append(error)

                worker = threading.Thread(target=delayed_request)
                worker.start()
                running = _wait_for(
                    "running activity before restart",
                    lambda: next(
                        (
                            item
                            for item in _route_interactions(env, running_route)
                            if item["status"] == "running"
                        ),
                        None,
                    ),
                )
                running_id = running["id"]
                assert received_request.wait(timeout=10.0), "upstream did not receive the live request"
                assert request_outcome == [], "the gated request must remain undelivered"
                assert _detail(env, running_id)["runs"][0]["status"] == "running"
            finally:
                # 模拟未收口的进程中断，不允许优雅关闭把 running 请求转为终态。
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=10.0)
                release_request.set()
                stop_stravia_server(process, logs)
            worker.join(timeout=10.0)
            assert request_outcome

            restart_port = find_free_port()
            restarted, restarted_logs = start_stravia_server(
                stravia_binary=stravia_binary,
                args=[
                    "--host",
                    "127.0.0.1",
                    "--port",
                    str(restart_port),
                    "--data-dir",
                    str(data_dir),
                ],
            )
            restart_base = f"http://127.0.0.1:{restart_port}"
            try:
                wait_until_ready(f"{restart_base}/api/v1/auth/state")
                session = WebSession(restart_base)
                status, login = session.request(
                    "POST",
                    "/api/v1/auth/login",
                    {"username": "admin", "password": "correct horse battery staple"},
                )
                assert status == 200, login
                restarted_env = {
                    **env,
                    "admin": restart_base,
                    "proxy": restart_base,
                    "auth": session.auth_headers(),
                }
                interrupted = _wait_for(
                    "restart-interrupted Run",
                    lambda: (lambda detail: detail if detail["interaction"]["status"] == "interrupted" else None)(
                        _detail(restarted_env, running_id)
                    ),
                )
                assert interrupted["runs"][0]["status"] == "interrupted"
                assert interrupted["runs"][0]["terminal_reason"] == "process_restarted"
                assert any(
                    event["kind"] == "run_state_changed" and event["payload"].get("reason") == "process_restarted"
                    for event in interrupted["runs"][0]["events"]
                )

                waiting = _detail(restarted_env, waiting_id)
                assert waiting["interaction"]["status"] == "interrupted"
                assert waiting["runs"][0]["status"] == "interrupted"
                assert waiting["runs"][0]["terminal_reason"] == "process_restarted"
            finally:
                stop_stravia_server(restarted, restarted_logs)
    finally:
        release_request.set()
        mock_server.shutdown()
        mock_server.server_close()


def _seed_resolved_waiting_siblings(data_dir: Path, interaction_id: str) -> str:
    """Seed resolved tool siblings in the current schema of a stopped isolated instance."""
    database = data_dir / "db" / "gateway.db"
    assert data_dir.is_absolute() and database.is_file()
    with closing(sqlite3.connect(database)) as connection:
        connection.row_factory = sqlite3.Row
        connection.execute("PRAGMA foreign_keys = ON")
        root = dict(connection.execute(
            "SELECT * FROM inference_run_observations WHERE interaction_id = ?",
            (interaction_id,),
        ).fetchone())
        assert root["status"] == "completed"
        waiting_id = f"req-{uuid.uuid4()}"
        result_id = f"req-{uuid.uuid4()}"
        sequence = connection.execute(
            "SELECT next_sequence FROM observation_sequence WHERE singleton_id = 1"
        ).fetchone()[0]
        for run_id, run_status, records in (
            (waiting_id, "waiting_client", [
                ("run_admitted", {"parent_run_id": root["id"]}),
                ("client_tool_handoff", {"tool_id": "restart-a", "name": "local_probe", "input": {}}),
                ("client_tool_handoff", {"tool_id": "restart-b", "name": "local_probe", "input": {}}),
                ("run_finished", {"status": "waiting_client", "delivery": {"status": "delivered", "reason": None}}),
            ]),
            (result_id, "completed", [
                ("run_admitted", {"parent_run_id": root["id"]}),
                ("client_tool_result", {"tool_id": "restart-a", "content": "ok", "is_error": False}),
                ("client_tool_result", {"tool_id": "restart-b", "content": "tool failed", "is_error": True}),
                ("run_finished", {"status": "completed", "delivery": {"status": "delivered", "reason": None}}),
            ]),
        ):
            run = {**root, "id": run_id, "parent_run_id": root["id"],
                   "generation_node_id": None, "generation_parent_id": root["generation_node_id"],
                   "status": run_status, "last_event_sequence": sequence + len(records) - 1}
            columns = ", ".join(run)
            placeholders = ", ".join("?" for _ in run)
            connection.execute(
                f"INSERT INTO inference_run_observations ({columns}) VALUES ({placeholders})",
                tuple(run.values()),
            )
            for kind, payload in records:
                connection.execute(
                    "INSERT INTO observation_events "
                    "(sequence, occurred_at, interaction_id, run_id, kind, payload, expires_at, tool_id, operation_id) "
                    "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    (sequence, root["last_active_at"], interaction_id, run_id,
                     kind, encode_storage_json(payload), root["expires_at"],
                     payload.get("tool_id"), payload.get("operation_id")),
                )
                sequence += 1
        connection.execute(
            "UPDATE observation_sequence SET next_sequence = ? WHERE singleton_id = 1",
            (sequence,),
        )
        connection.execute(
            "UPDATE interaction_observations SET status = 'waiting_client', last_event_sequence = ? WHERE id = ?",
            (sequence - 1, interaction_id),
        )
        connection.commit()
    return waiting_id


@pytest.mark.e2e
@pytest.mark.admin
def test_restart_reconciles_waiting_interactions(stravia_binary: Path) -> None:
    mock_port = find_free_port()
    mock_server, _ = minimal_mock_provider(mock_port)
    try:
        with tempfile.TemporaryDirectory(prefix="stravia-wait-recovery-e2e-") as temporary:
            data_dir = Path(temporary).resolve()
            env, process, logs = _start_initialized(
                stravia_binary, data_dir, f"http://127.0.0.1:{mock_port}"
            )
            try:
                route_id, api_key = _create_route(env, "observation-recovery-completed")
                status, response = _proxy(env, api_key, "observation-recovery-completed", [
                    {"role": "user", "content": "final response before restart"},
                ])
                assert status == 200, response
                completed = _wait_for("completed seed", lambda: next((
                    item for item in _route_interactions(env, route_id)
                    if item["status"] == "completed"
                ), None))
                pending_route, pending_key = _create_route(env, "observation-branch-recovery")

                def create_waiting(current_env: dict[str, Any], prompt: str) -> dict[str, Any]:
                    status, response = http_request(
                        "POST", f"{current_env['proxy']}/v1/chat/completions",
                        payload={"model": "observation-branch-recovery",
                                 "messages": [{"role": "user", "content": prompt}],
                                 "tools": [{"type": "function", "function": {
                                     "name": "local_probe", "parameters": {"type": "object"},
                                 }}]},
                        headers={"authorization": f"Bearer {pending_key}"},
                    )
                    assert status == 200, response

                    def finished_waiting() -> dict[str, Any] | None:
                        item = next((
                            item for item in _route_interactions(current_env, pending_route)
                            if item["status"] == "waiting_client"
                        ), None)
                        if item is None:
                            return None
                        run = _detail(current_env, item["id"])["runs"][0]
                        return item if any(
                            event["kind"] == "run_finished" for event in run["events"]
                        ) else None

                    return _wait_for("pending tool interaction", finished_waiting)

                pending = create_waiting(env, "observation-branch pending before restart")
                download_observation_bundle(env, _detail(env, pending["id"]))
                pending_detail = _detail(env, pending["id"])
                before_active_at = pending_detail["interaction"]["last_active_at"]
                before = pending_detail["runs"][0]
            finally:
                stop_stravia_server(process, logs)

            resolved_run = _seed_resolved_waiting_siblings(data_dir, completed["id"])
            recovery_events: list[dict[str, Any]] | None = None
            for restart_index in range(2):
                port = find_free_port()
                process, logs = start_stravia_server(stravia_binary=stravia_binary, args=[
                    "--host", "127.0.0.1", "--port", str(port), "--data-dir", str(data_dir),
                ])
                base = f"http://127.0.0.1:{port}"
                try:
                    wait_until_ready(f"{base}/api/v1/auth/state")
                    session = WebSession(base)
                    status, login = session.request("POST", "/api/v1/auth/login", {
                        "username": "admin", "password": "correct horse battery staple",
                    })
                    assert status == 200, login
                    current = {**env, "admin": base, "proxy": base, "auth": session.auth_headers()}
                    for interaction_id, expected in ((completed["id"], "completed"), (pending["id"], "interrupted")):
                        detail = _detail(current, interaction_id)
                        assert detail["interaction"]["status"] == expected
                        forest = _forest(current, limit=100)
                        projected = {item["id"]: item for root in forest["roots"] for item in root["interactions"]}
                        assert projected[interaction_id]["status"] == expected
                        _, _, archive = download_observation_bundle(current, detail)
                        with zipfile.ZipFile(io.BytesIO(archive)) as bundle:
                            exported = json.loads(bundle.read("interaction.json"))
                        assert exported["status"] == expected
                    resolved = _detail(current, completed["id"])
                    historical = next(run for run in resolved["runs"] if run["id"] == resolved_run)
                    assert historical["status"] == "waiting_client"
                    assert not any(event["kind"] == "run_state_changed" and event["payload"].get("reason") == "process_restarted" for event in historical["events"])
                    recovered_detail = _detail(current, pending["id"])
                    assert recovered_detail["interaction"]["last_active_at"] == before_active_at
                    recovered = recovered_detail["runs"][0]
                    assert recovered["status"] == "interrupted"
                    assert recovered["terminal_reason"] == "process_restarted"
                    for field in ("generation_node_id", "generation_parent_id", "finished_at", "client_output_committed"):
                        assert recovered[field] == before[field]
                    assert [event for event in recovered["events"] if event["kind"] == "run_finished"] == [
                        event for event in before["events"] if event["kind"] == "run_finished"
                    ]
                    events = [event for event in recovered["events"] if event["kind"] == "run_state_changed" and event["payload"].get("reason") == "process_restarted"]
                    assert len(events) == 1
                    assert events[0]["payload"] == {"status": "interrupted", "reason": "process_restarted"}
                    if recovery_events is not None:
                        assert events == recovery_events
                    recovery_events = events
                    if restart_index == 1:
                        live = create_waiting(current, "observation-branch live after restart")
                        status, cleared = http_request(
                            "DELETE", f"{base}/api/v1/observations/history", headers=current["auth"],
                        )
                        assert status == 200, cleared
                        assert cleared["data"]["skipped_active"] == 1
                        assert _detail(current, live["id"])["interaction"]["status"] == "waiting_client"
                        assert not _route_interactions(current, route_id)
                        assert {item["id"] for item in _route_interactions(current, pending_route)} == {live["id"]}
                finally:
                    stop_stravia_server(process, logs)
    finally:
        mock_server.shutdown()
        mock_server.server_close()


@pytest.mark.e2e
@pytest.mark.admin
def test_expired_waiting_client_is_removed_with_events_and_trace(
    stravia_binary: Path,
) -> None:
    mock_port = find_free_port()
    mock_server, _ = minimal_mock_provider(mock_port)
    try:
        with tempfile.TemporaryDirectory(prefix="stravia-waiting-retention-e2e-") as temporary:
            data_dir = Path(temporary)
            env, process, logs = _start_initialized(
                stravia_binary, data_dir, f"http://127.0.0.1:{mock_port}"
            )
            try:
                status, body = http_request(
                    "PUT",
                    f"{env['admin']}/api/v1/observations/debug",
                    payload={"enabled": True, "confirmed": True},
                    headers=env["auth"],
                )
                assert status == 200, body
                route_id, api_key = _create_route(env, "observation-branch-retention")
                status, response = http_request(
                    "POST",
                    f"{env['proxy']}/v1/chat/completions",
                    payload={
                        "model": "observation-branch-retention",
                        "messages": [{"role": "user", "content": "observation-branch: expire this waiting timeline"}],
                        "tools": [
                            {
                                "type": "function",
                                "function": {
                                    "name": "local_probe",
                                    "parameters": {"type": "object"},
                                },
                            }
                        ],
                    },
                    headers={"authorization": f"Bearer {api_key}"},
                )
                assert status == 200, response
                waiting = _wait_for(
                    "waiting-client retention fixture",
                    lambda: next(
                        (
                            item
                            for item in _route_interactions(env, route_id)
                            if item["status"] == "waiting_client"
                        ),
                        None,
                    ),
                )
                detail = _detail(env, waiting["id"])
                run_id = detail["runs"][0]["id"]
                trace = detail["runs"][0]["trace"]
                assert trace is not None
                trace_directory = data_dir / "diagnostics" / "observation-debug" / trace["trace_id"]
                assert trace_directory.is_dir()

                status, cleared = http_request(
                    "DELETE",
                    f"{env['admin']}/api/v1/observations/history",
                    headers=env["auth"],
                )
                assert status == 200, cleared
                assert cleared["data"]["skipped_active"] == 1
                assert _detail(env, waiting["id"])["interaction"]["status"] == "waiting_client"

                status, setting = http_request(
                    "PUT",
                    f"{env['admin']}/api/v1/settings/log_retention_days",
                    payload={"value": "0"},
                    headers=env["auth"],
                )
                assert status == 200, setting

                _wait_for(
                    "expired waiting-client removal",
                    lambda: not _route_interactions(env, route_id),
                )
                assert not trace_directory.exists()
                with closing(sqlite3.connect(data_dir / "db" / "gateway.db")) as connection:
                    assert connection.execute(
                        "SELECT COUNT(*) FROM interaction_observations WHERE id = ?",
                        (waiting["id"],),
                    ).fetchone()[0] == 0
                    assert connection.execute(
                        "SELECT COUNT(*) FROM inference_run_observations WHERE id = ?",
                        (run_id,),
                    ).fetchone()[0] == 0
                    assert connection.execute(
                        "SELECT COUNT(*) FROM observation_events WHERE interaction_id = ?",
                        (waiting["id"],),
                    ).fetchone()[0] == 0
                assert not (trace_directory / "manifest.json").exists()
                assert not trace_directory.with_name(f".deleting-{trace['trace_id']}").exists()
            finally:
                stop_stravia_server(process, logs)
    finally:
        mock_server.shutdown()
        mock_server.server_close()


@pytest.mark.e2e
@pytest.mark.admin
def test_idle_waiting_client_expires_to_disconnected_on_retention_sweep(
    stravia_binary: Path,
) -> None:
    mock_port = find_free_port()
    mock_server, _ = minimal_mock_provider(mock_port)
    try:
        with tempfile.TemporaryDirectory(prefix="stravia-waiting-idle-e2e-") as temporary:
            data_dir = Path(temporary)
            env, process, logs = _start_initialized(
                stravia_binary, data_dir, f"http://127.0.0.1:{mock_port}"
            )
            try:
                route_id, api_key = _create_route(env, "observation-branch-idle")
                status, response = http_request(
                    "POST",
                    f"{env['proxy']}/v1/chat/completions",
                    payload={
                        "model": "observation-branch-idle",
                        "messages": [{"role": "user", "content": "observation-branch: idle waiting expiry"}],
                        "tools": [
                            {
                                "type": "function",
                                "function": {
                                    "name": "local_probe",
                                    "parameters": {"type": "object"},
                                },
                            }
                        ],
                    },
                    headers={"authorization": f"Bearer {api_key}"},
                )
                assert status == 200, response
                def finished_waiting() -> dict[str, Any] | None:
                    item = next(
                        (
                            item
                            for item in _route_interactions(env, route_id)
                            if item["status"] == "waiting_client"
                        ),
                        None,
                    )
                    if item is None:
                        return None
                    run = _detail(env, item["id"])["runs"][0]
                    return item if any(
                        event["kind"] == "run_finished" for event in run["events"]
                    ) else None

                waiting = _wait_for("idle waiting fixture", finished_waiting)
                # 借 bundle 导出通道排空 writer 队列，保证没有迟到事件把
                # last_active_at 重新刷成新值覆盖下面的陈旧化 UPDATE。
                download_observation_bundle(env, _detail(env, waiting["id"]))
                # 服务器仍在运行：WAL 下单次 UPDATE 不会与短事务冲突，
                # 不能用重启制造陈旧记录，重启恢复会先把它标成 interrupted。
                stale_at = int(time.time() * 1000) - 25 * 60 * 60 * 1000
                with closing(
                    sqlite3.connect(data_dir / "db" / "gateway.db", timeout=10)
                ) as connection:
                    connection.execute(
                        "UPDATE inference_run_observations SET last_active_at = ? "
                        "WHERE interaction_id = ? AND status = 'waiting_client'",
                        (stale_at, waiting["id"]),
                    )
                    connection.execute(
                        "UPDATE interaction_observations SET last_active_at = ? WHERE id = ?",
                        (stale_at, waiting["id"]),
                    )
                    connection.commit()
                status, setting = http_request(
                    "PUT",
                    f"{env['admin']}/api/v1/settings/log_retention_days",
                    payload={"value": "30"},
                    headers=env["auth"],
                )
                assert status == 200, setting

                def expired() -> dict[str, Any] | None:
                    detail = _detail(env, waiting["id"])
                    return (
                        detail
                        if detail["interaction"]["status"] == "disconnected"
                        else None
                    )

                detail = _wait_for("idle wait expiry", expired)
                run = detail["runs"][0]
                assert run["status"] == "disconnected"
                assert run["terminal_reason"] == "client_wait_expired"
                assert detail["interaction"]["last_active_at"] == stale_at
                now = int(time.time() * 1000)
                for start_at, end_at, expected in (
                    (now - 600_000, now + 1, False),
                    (stale_at, stale_at + 600_000, True),
                ):
                    forest = _forest(env, start_at=start_at, end_at=end_at)
                    assert forest["root_total"] == int(expected), forest
                transitions = [
                    event for event in run["events"] if event["kind"] == "run_state_changed"
                ]
                assert transitions[-1]["payload"] == {
                    "status": "disconnected",
                    "reason": "client_wait_expired",
                }
            finally:
                stop_stravia_server(process, logs)
    finally:
        mock_server.shutdown()
        mock_server.server_close()


@pytest.mark.e2e
@pytest.mark.admin
def test_history_cleanup_expires_event_cursor_without_rewinding_sequence(
    stravia_binary: Path,
) -> None:
    mock_port = find_free_port()
    mock_server, _ = minimal_mock_provider(mock_port)
    try:
        with tempfile.TemporaryDirectory(prefix="stravia-clear-cursor-e2e-") as temporary:
            data_dir = Path(temporary)
            env, process, logs = _start_initialized(
                stravia_binary, data_dir, f"http://127.0.0.1:{mock_port}"
            )
            try:
                route_id, api_key = _create_route(env, "observation-clear-cursor-isolated")
                status, response = _proxy(
                    env,
                    api_key,
                    "observation-clear-cursor-isolated",
                    [{"role": "user", "content": "historical interaction"}],
                )
                assert status == 200, response
                interaction = _wait_for(
                    "historical Interaction", lambda: _route_interactions(env, route_id)
                )[0]
                interaction_sequence = _detail(env, interaction["id"])["snapshot_sequence"]

                status, rejected_response = http_request(
                    "POST",
                    f"{env['proxy']}/v1/chat/completions",
                    payload={"model": "missing", "messages": []},
                )
                assert status == 401, rejected_response

                def historical_rejection_sequence() -> int | None:
                    # 只看全局序列推进会在 run 终态事件先落盘时提前满足；
                    # 等拒绝记录本身可见，才能保证其事件已进入全局序列。
                    status_, body = http_request(
                        "GET",
                        f"{env['admin']}/api/v1/observations/rejections",
                        headers=env["auth"],
                    )
                    assert status_ == 200, body
                    if not any(item["status_code"] == 401 for item in body["data"]["items"]):
                        return None
                    sequence = _forest(env)["snapshot_sequence"]
                    return sequence if sequence > interaction_sequence else None

                old_sequence = _wait_for(
                    "historical rejection event",
                    historical_rejection_sequence,
                )

                status, cleared = http_request(
                    "DELETE",
                    f"{env['admin']}/api/v1/observations/history",
                    headers=env["auth"],
                )
                assert status == 200, cleared
                assert cleared["data"] == {
                    "deleted_interactions": 1,
                    "deleted_rejections": 1,
                    "skipped_active": 0,
                }
                empty = _forest(env, model=route_id)
                assert empty["roots"] == []
                status, rejections = http_request(
                    "GET",
                    f"{env['admin']}/api/v1/observations/rejections",
                    headers=env["auth"],
                )
                assert status == 200, rejections
                assert rejections["data"]["items"] == []

                reset = _sse_event(env, old_sequence - 1)
                assert reset["event"] == "reset_required"
                assert reset["data"]["reset_required"] is True
                # Purging rows does not rewind the persistent stream allocator.
                # A forest watermark instead describes surviving committed rows.
                assert reset["data"]["snapshot_sequence"] >= old_sequence

                status, response = _proxy(
                    env,
                    api_key,
                    "observation-clear-cursor-isolated",
                    [{"role": "user", "content": "activity after history reset"}],
                )
                assert status == 200, response
                replacement = _wait_for(
                    "post-cleanup Interaction", lambda: _route_interactions(env, route_id)
                )[0]
                detail = _detail(env, replacement["id"])
                replacement_sequences = [
                    event["sequence"] for run in detail["runs"] for event in run["events"]
                ]
                assert min(replacement_sequences) > old_sequence
            finally:
                stop_stravia_server(process, logs)
    finally:
        mock_server.shutdown()
        mock_server.server_close()


# ── 客户端错误 / 上游错误（ADR-0083） ──


def _provider(
    env: dict[str, Any],
    name: str,
    base_url: str,
    *,
    input_modalities: tuple[str, ...] = ("text",),
    reasoning_efforts: tuple[str, ...] = (),
) -> str:
    status, body = http_request(
        "POST",
        f"{env['admin']}/api/v1/providers",
        payload={
            "name": name,
            "source": {
                "type": "custom",
                "vendor": "custom",
                "channel": "default",
                "protocol": "openai-compatible",
                "base_url": base_url,
            },
            "credential": {"type": "api_key", "value": "upstream-key"},
            "vendor_options": {},
        },
        headers=env["auth"],
    )
    assert status == 200, body
    provider_id = body["data"]["id"]
    status, body = http_request(
        "POST",
        f"{env['admin']}/api/v1/providers/{provider_id}/models",
        payload={
            "model_id": "gpt-4o-mini",
            "metadata": {
                "name": name,
                "modalities": {"input": list(input_modalities), "output": ["text"]},
                **({"reasoning_efforts": list(reasoning_efforts)} if reasoning_efforts else {}),
            },
        },
        headers=env["auth"],
    )
    assert status == 201, body
    return provider_id


def _route_with_targets(
    env: dict[str, Any], model: str, targets: list[dict[str, Any]]
) -> tuple[str, str]:
    status, body = http_request(
        "POST",
        f"{env['admin']}/api/v1/models",
        payload={
            "model_id": model,
            "output_modality": "text",
            "display_name": f"{model} display",
            "targets": targets,
        },
        headers=env["auth"],
    )
    assert status == 200, body
    route_id = body["data"]["id"]
    status, body = http_request(
        "POST",
        f"{env['admin']}/api/v1/api-keys",
        payload={"name": f"{model}-key", "model_ids": [route_id]},
        headers=env["auth"],
    )
    assert status == 200, body
    return route_id, body["data"]["key"]


def _provider_id(env: dict[str, Any], name: str) -> str:
    status, body = http_request(
        "GET", f"{env['admin']}/api/v1/providers", headers=env["auth"]
    )
    assert status == 200, body
    return next(item["id"] for item in body["data"] if item["name"] == name)


def _window(started_at: int) -> dict[str, int]:
    return {"start_at": started_at, "end_at": started_at + 120_000}


@pytest.mark.e2e
@pytest.mark.admin
def test_client_errors_classify_rejected_requests(admin_env: dict[str, Any]) -> None:
    started_at = int(time.time() * 1000)
    model = "observation-client-rejected"
    route_id, key = _create_route(admin_env, model)
    other_model = "observation-client-rejected-other"
    _create_route(admin_env, other_model)

    # 非法 JSON 在解码阶段被拒绝：准入前拒绝记录，属于错误的请求。
    raw = Request(
        f"{admin_env['proxy']}/v1/chat/completions",
        data=b'{"model":"broken",',
        headers={"authorization": f"Bearer {key}", "content-type": "application/json"},
        method="POST",
    )
    with pytest.raises(HTTPError) as invalid_json:
        urlopen(raw, timeout=15)
    assert invalid_json.value.code == 400

    # 错误的 Key 在准入前被拒绝。
    status, _ = http_request(
        "POST",
        f"{admin_env['proxy']}/v1/chat/completions",
        payload={"model": model, "messages": [{"role": "user", "content": "bad key"}]},
        headers={"authorization": "Bearer not-a-key"},
    )
    assert status == 401

    # 模型不存在：已准入但未进入 Target 选择。
    status, _ = _proxy(
        admin_env,
        key,
        "observation-client-rejected-missing",
        [{"role": "user", "content": "missing model"}],
    )
    assert status == 404

    # 无权使用模型：已准入但未进入 Target 选择。
    status, _ = _proxy(
        admin_env,
        key,
        other_model,
        [{"role": "user", "content": "forbidden model"}],
    )
    assert status == 403

    # 请求 Hook 拒绝：拒绝码由 Hook 指定（invalid_input），不能按码推断分类。
    status, hook = http_request(
        "POST",
        f"{admin_env['proxy']}/v1/responses",
        payload={
            "model": model,
            "input": [{"role": "user", "content": "search the web"}],
            "tools": [
                {
                    "type": "web_search",
                    "filters": {
                        "allowed_domains": [f"domain-{index}.example" for index in range(21)]
                    },
                }
            ],
        },
        headers={"authorization": f"Bearer {key}"},
    )
    assert status == 400, hook
    assert hook["error"]["code"] == "invalid_request", hook

    window = _window(started_at)

    def rejected_page() -> dict[str, Any] | None:
        page = _client_errors(admin_env, **window, limit=100)
        return page if page["total"] == 5 else None

    page = _wait_for("five rejected client errors", rejected_page)
    assert {item["kind"] for item in page["items"]} == {"rejection", "run"}
    assert {item["type"] for item in page["items"]} == {"rejected"}
    codes = {item["error"]["code"] for item in page["items"]}
    # Hook 拒绝码由 Hook 指定，分类不依赖错误码，只断言可稳定枚举的两种。
    assert {"model_not_found", "STRAVIA_FORBIDDEN"} <= codes
    rejection_statuses = {
        item["error"]["status_code"]
        for item in page["items"]
        if item["kind"] == "rejection"
    }
    assert rejection_statuses == {400, 401}
    # 记录种类与类型正交：未进入 Target 选择的运行仍是 kind=run。
    run_items = [item for item in page["items"] if item["kind"] == "run"]
    assert len(run_items) == 3
    assert all(item["run_id"] is not None for item in run_items)

    failed = _client_errors(admin_env, **window, type="failed", limit=100)
    assert failed["total"] == 0 and failed["items"] == []
    rejected = _client_errors(admin_env, **window, type="rejected", limit=100)
    assert rejected["total"] == 5
    assert [item["id"] for item in rejected["items"]] == [
        item["id"] for item in page["items"]
    ]
    # 供应商筛选经由尝试：错误的请求没有尝试，不会匹配。模型筛选仍可命中
    # 已匹配路由后才被请求 Hook 拒绝的记录，因此这里不断言为 0。
    assert _client_errors(admin_env, **window, provider="missing-provider")["total"] == 0


@pytest.mark.e2e
@pytest.mark.admin
def test_client_errors_classify_failed_requests(admin_env: dict[str, Any]) -> None:
    started_at = int(time.time() * 1000)
    admin_env["mock_server"].stream_error_release.clear()

    no_target_model = "observation-client-no-target"
    _, no_target_key = _create_route(admin_env, no_target_model, retry_budget=0)
    provider_id = _provider_id(admin_env, f"{no_target_model}-provider")
    status, _ = http_request(
        "PUT",
        f"{admin_env['admin']}/api/v1/providers/{provider_id}",
        payload={"is_enabled": False},
        headers=admin_env["auth"],
    )
    assert status == 200
    status, _ = _proxy(
        admin_env,
        no_target_key,
        no_target_model,
        [{"role": "user", "content": no_target_model}],
    )
    assert status >= 500

    upstream_model = "observation-client-upstream-failed"
    _, upstream_key = _create_route(admin_env, upstream_model, retry_budget=0)
    status, _ = _proxy(
        admin_env,
        upstream_key,
        upstream_model,
        [{"role": "user", "content": "observation-always-failed"}],
    )
    assert status >= 500

    stream_model = "observation-client-stream-error"
    _, stream_key = _create_route(admin_env, stream_model, retry_budget=0)
    request = Request(
        f"{admin_env['proxy']}/v1/chat/completions",
        data=json.dumps(
            {
                "model": stream_model,
                "stream": True,
                "messages": [{"role": "user", "content": "observation-final-stream-error"}],
            }
        ).encode(),
        headers={
            "authorization": f"Bearer {stream_key}",
            "content-type": "application/json",
        },
    )
    with urlopen(request, timeout=15) as response:
        assert response.status == 200
        admin_env["mock_server"].stream_error_release.set()
        assert "partial" in response.read().decode()

    timeout_model = "observation-client-first-token-timeout"
    _, timeout_key = _create_route(
        admin_env, timeout_model, retry_budget=0, first_token_timeout_ms=1_000
    )
    status, _ = _proxy(
        admin_env,
        timeout_key,
        timeout_model,
        [{"role": "user", "content": "observation-delay"}],
    )
    assert status >= 500

    refused_model = "observation-client-connection-refused"
    with socket.socket() as unavailable:
        unavailable.bind(("127.0.0.1", 0))
        refused_env = {
            **admin_env,
            "mock": f"http://127.0.0.1:{unavailable.getsockname()[1]}",
        }
        _, refused_key = _create_route(refused_env, refused_model, retry_budget=0)
        status, _ = _proxy(
            admin_env,
            refused_key,
            refused_model,
            [{"role": "user", "content": refused_model}],
        )
    assert status >= 500

    quota_model = "observation-client-quota-suspended"
    _, quota_key = _create_route(admin_env, quota_model, retry_budget=0)
    quota_provider = _provider_id(admin_env, f"{quota_model}-provider")
    database = Path(admin_env["data_dir"]) / "db" / "gateway.db"
    with closing(sqlite3.connect(database)) as connection:
        connection.execute(
            "INSERT OR REPLACE INTO provider_allowance_suspensions(provider_id,suspended,triggered_keys,evidence_completed_at) VALUES(?,1,'[]',?)",
            (quota_provider, int(time.time() * 1000)),
        )
        connection.commit()
    status, _ = _proxy(
        admin_env,
        quota_key,
        quota_model,
        [{"role": "user", "content": quota_model}],
    )
    assert status == 429

    window = _window(started_at)

    def failed_page() -> dict[str, Any] | None:
        page = _client_errors(admin_env, **window, type="failed", limit=100)
        return page if page["total"] == 6 else None

    page = _wait_for("six failed client errors", failed_page, timeout=20)
    assert {item["type"] for item in page["items"]} == {"failed"}
    by_model = {item["model"]: item for item in page["items"]}
    assert set(by_model) == {
        no_target_model,
        upstream_model,
        stream_model,
        timeout_model,
        refused_model,
        quota_model,
    }
    assert by_model[no_target_model]["error"]["source"] == "platform"
    assert by_model[upstream_model]["error"]["source"] == "upstream"
    assert by_model[upstream_model]["error"]["status_code"] == 503
    assert by_model[stream_model]["error"]["source"] == "upstream"
    assert "timeout" in by_model[timeout_model]["error"]["code"]
    assert by_model[refused_model]["error"]["source"] == "upstream"
    assert by_model[quota_model]["error"]["code"] == "allowance_suspended"
    # 每个失败请求都有运行与交互标识。
    assert all(item["run_id"] and item["interaction_id"] for item in page["items"])

    assert _client_errors(admin_env, **window, type="rejected")["total"] == 0


@pytest.mark.e2e
@pytest.mark.admin
def test_client_errors_exclude_recovered_and_cancelled_requests(
    admin_env: dict[str, Any],
) -> None:
    started_at = int(time.time() * 1000)
    recovered_model = "observation-client-recovered"
    _, recovered_key = _create_route(admin_env, recovered_model, retry_budget=1)
    status, _ = _proxy(
        admin_env,
        recovered_key,
        recovered_model,
        [{"role": "user", "content": "observation-root-retry client-error"}],
    )
    assert status == 200

    cancelled_model = "observation-client-cancelled"
    cancelled_route, cancelled_key = _create_route(admin_env, cancelled_model, retry_budget=0)
    admin_env["mock_server"].stream_error_release.clear()
    request = Request(
        f"{admin_env['proxy']}/v1/chat/completions",
        data=json.dumps(
            {
                "model": cancelled_model,
                "stream": True,
                "messages": [{"role": "user", "content": "observation-stream-disconnect"}],
            }
        ).encode(),
        headers={
            "authorization": f"Bearer {cancelled_key}",
            "content-type": "application/json",
        },
    )
    try:
        with urlopen(request, timeout=15) as response:
            assert response.status == 200
            while line := response.readline():
                if line.startswith(b"data:"):
                    event = json.loads(line[5:].strip())
                    if any(
                        "partial" in (choice.get("delta", {}).get("content") or "")
                        for choice in event.get("choices", [])
                    ):
                        break
        # 关闭客户端不代表服务端已经处理断开；先观察取消终态，再释放上游失败。
        interaction = _wait_for(
            "cancelled streaming Interaction",
            lambda: _route_interactions(admin_env, cancelled_route),
        )[0]
        detail = _wait_for(
            "cancelled Run finalization",
            lambda: _finished_detail(admin_env, interaction["id"]),
        )
        assert detail["runs"][0]["status"] in ("cancelled", "interrupted")
    finally:
        admin_env["mock_server"].stream_error_release.set()

    window = _window(started_at)
    assert _client_errors(admin_env, **window, limit=100)["items"] == []
    # 恢复的请求仍产生“已恢复”的上游错误；取消的尝试不是上游错误。
    upstream = _upstream_errors(admin_env, **window, limit=100)
    assert {item["outcome"] for item in upstream["items"]} == {"recovered"}
    assert {item["model"] for item in upstream["items"]} == {recovered_model}


@pytest.mark.e2e
@pytest.mark.admin
def test_client_errors_type_filter_totals_pagination_and_legacy_paths(
    admin_env: dict[str, Any],
) -> None:
    started_at = int(time.time() * 1000)
    failed_model = "observation-client-filter-failed"
    _, failed_key = _create_route(admin_env, failed_model, retry_budget=0)
    status, _ = _proxy(
        admin_env,
        failed_key,
        failed_model,
        [{"role": "user", "content": "observation-always-failed"}],
    )
    assert status >= 500
    status, _ = http_request(
        "POST",
        f"{admin_env['proxy']}/v1/chat/completions",
        payload={"model": failed_model, "messages": [{"role": "user", "content": "unauthorized"}]},
    )
    assert status == 401

    window = _window(started_at)

    def both() -> dict[str, Any] | None:
        page = _client_errors(admin_env, **window, limit=100)
        return page if page["total"] == 2 else None

    whole = _wait_for("mixed client errors", both)
    assert {item["type"] for item in whole["items"]} == {"failed", "rejected"}
    assert {item["kind"] for item in whole["items"]} == {"run", "rejection"}

    for request_type in ("failed", "rejected"):
        subset = _client_errors(admin_env, **window, type=request_type, limit=100)
        assert subset["total"] == 1
        assert {item["type"] for item in subset["items"]} == {request_type}
    assert (
        _client_errors(admin_env, **window, type="failed")["total"]
        + _client_errors(admin_env, **window, type="rejected")["total"]
        == whole["total"]
    )

    seen: list[tuple[str, str]] = []
    cursor = None
    while True:
        page = _client_errors(
            admin_env,
            **window,
            limit=1,
            **({"cursor": cursor} if cursor else {}),
        )
        assert page["total"] == 2
        seen.extend((item["kind"], item["id"]) for item in page["items"])
        cursor = page["next_cursor"]
        if cursor is None:
            break
        assert len(seen) <= 2
    assert seen == [(item["kind"], item["id"]) for item in whole["items"]]
    assert len(set(seen)) == 2

    status, body = http_request(
        "GET",
        f"{admin_env['admin']}/api/v1/observations/client-errors?{urlencode({**window, 'type': 'other'})}",
        headers=admin_env["auth"],
    )
    assert status == 400, body
    assert body["code"] == "invalid_observation_filter", body

    # 旧失败请求路径不再提供。
    for path in (
        "/api/v1/observations/failed-requests",
        "/api/v1/observations/failed-requests/run/missing",
        "/api/v1/observations/failed-requests/rejection/missing",
    ):
        status, _ = http_request(
            "GET", f"{admin_env['admin']}{path}", headers=admin_env["auth"]
        )
        assert status == 404, path


@pytest.mark.e2e
@pytest.mark.admin
def test_upstream_errors_list_detail_filters_and_retention(
    admin_env: dict[str, Any],
) -> None:
    started_at = int(time.time() * 1000)
    recovered_model = "observation-upstream-recovered"
    recovered_route, recovered_key = _create_route(
        admin_env, recovered_model, retry_budget=1
    )
    status, _ = _proxy(
        admin_env,
        recovered_key,
        recovered_model,
        [{"role": "user", "content": "observation-root-retry upstream-list"}],
    )
    assert status == 200

    failed_model = "observation-upstream-caused"
    failed_route, failed_key = _create_route(admin_env, failed_model, retry_budget=0)
    status, _ = _proxy(
        admin_env,
        failed_key,
        failed_model,
        [{"role": "user", "content": "observation-always-failed"}],
    )
    assert status >= 500

    window = _window(started_at)

    def both() -> dict[str, Any] | None:
        page = _upstream_errors(admin_env, **window, limit=100)
        outcomes = {item["outcome"] for item in page["items"]}
        # 尝试行先于所属运行终态落库；等到派生结果稳定再断言。
        return page if page["total"] == 2 and outcomes == {"recovered", "caused_failure"} else None

    whole = _wait_for("upstream errors for both runs", both)
    by_model = {item["model"]: item for item in whole["items"]}
    recovered = by_model[recovered_model]
    caused = by_model[failed_model]
    assert recovered["outcome"] == "recovered"
    assert caused["outcome"] == "caused_failure"
    for item in (recovered, caused):
        assert item["error"]["source"] == "upstream"
        assert item["provider_id"] is not None
        assert item["upstream_model"] == "gpt-4o-mini"
        assert item["model_display_name"] == f"{item['model']} display"
        assert item["api_key_name"] == f"{item['model']}-key"
        assert item["run_id"] and item["request_id"] == item["run_id"]
        assert item["interaction_id"] and item["root_id"]
        assert item["started_at"] >= started_at
        assert item["duration_ms"] is not None
        assert item["debug_status"] == "none"
        assert item["observation_gap"] is False
    assert recovered["error"]["status_code"] == 503
    assert recovered["error"]["message"] == "retry"
    # 恢复的请求不产生客户端错误。
    assert _client_errors(admin_env, **window, type="failed")["total"] == 1

    recovered_id = recovered["id"]
    recovered_run = recovered["run_id"]
    detail_status, body = http_request(
        "GET",
        f"{admin_env['admin']}/api/v1/observations/upstream-errors/{recovered_id}",
        headers=admin_env["auth"],
    )
    assert detail_status == 200, body
    detail = body["data"]
    assert detail["error"]["id"] == recovered_id
    assert [attempt["status"] for attempt in detail["attempts"]] == ["failed", "completed"]
    assert [attempt["error_source"] for attempt in detail["attempts"]] == ["upstream", None]
    assert detail["attempts"][0]["status_code"] == 503
    assert detail["attempts"][1]["duration_ms"] is not None
    assert detail["events"] and all(
        event["run_id"] == recovered_run for event in detail["events"]
    )
    assert detail["snapshot_sequence"] > 0
    assert detail["trace"] is None

    # 供应商、模型、API Key 与结果筛选。
    for key, value in (
        ("provider", recovered["provider_id"]),
        ("model", recovered_route),
        ("api_key", f"{recovered_model}-key"),
    ):
        filtered = _upstream_errors(admin_env, **window, **{key: value}, limit=100)
        assert filtered["total"] == 1, key
        assert filtered["items"][0]["id"] == recovered_id
    assert _upstream_errors(admin_env, **window, model=failed_route, limit=100)["total"] == 1
    assert (
        _upstream_errors(admin_env, **window, outcome="recovered", limit=100)["total"] == 1
    )
    assert (
        _upstream_errors(admin_env, **window, outcome="caused_failure", limit=100)["total"]
        == 1
    )
    assert (
        _upstream_errors(admin_env, **window, outcome="interrupted", limit=100)["total"] == 0
    )
    status, body = http_request(
        "GET",
        f"{admin_env['admin']}/api/v1/observations/upstream-errors?{urlencode({**window, 'outcome': 'other'})}",
        headers=admin_env["auth"],
    )
    assert status == 400, body
    assert body["code"] == "invalid_observation_filter", body

    # 游标分页保持总数与顺序。
    seen: list[str] = []
    cursor = None
    while True:
        page = _upstream_errors(
            admin_env, **window, limit=1, **({"cursor": cursor} if cursor else {})
        )
        assert page["total"] == 2
        seen.extend(item["id"] for item in page["items"])
        cursor = page["next_cursor"]
        if cursor is None:
            break
        assert len(seen) <= 2
    assert seen == [item["id"] for item in whole["items"]]

    status, _ = http_request(
        "GET",
        f"{admin_env['admin']}/api/v1/observations/upstream-errors/missing",
        headers=admin_env["auth"],
    )
    assert status == 404

    # 明细随所属运行过期；统计桶不受影响（本测试只验证列表口径）。
    database = Path(admin_env["data_dir"]) / "db" / "gateway.db"
    with closing(sqlite3.connect(database)) as connection:
        connection.execute(
            "UPDATE inference_run_observations SET expires_at=? WHERE id=?",
            (int(time.time() * 1000) - 1, recovered_run),
        )
        connection.commit()
    assert recovered_id not in {
        item["id"] for item in _upstream_errors(admin_env, **window, limit=100)["items"]
    }
    status, _ = http_request(
        "GET",
        f"{admin_env['admin']}/api/v1/observations/upstream-errors/{recovered_id}",
        headers=admin_env["auth"],
    )
    assert status == 404


@pytest.mark.e2e
@pytest.mark.admin
def test_upstream_errors_exclude_non_upstream_attempts(admin_env: dict[str, Any]) -> None:
    started_at = int(time.time() * 1000)
    model = "observation-upstream-exclusions"
    route_id, key = _create_route(admin_env, model, retry_budget=0)
    status, _ = _proxy(
        admin_env,
        key,
        model,
        [{"role": "user", "content": "observation-always-failed"}],
    )
    assert status >= 500

    window = _window(started_at)
    row = _wait_for(
        "upstream error for exclusion probe",
        lambda: next(iter(_upstream_errors(admin_env, **window)["items"]), None),
    )
    database = Path(admin_env["data_dir"]) / "db" / "gateway.db"
    # 来源缺失的失败尝试保持未知，既不计上游错误也不计上游请求。
    with closing(sqlite3.connect(database)) as connection:
        connection.execute(
            "UPDATE target_attempt_observations SET error_source=NULL WHERE id=?",
            (row["id"],),
        )
        connection.commit()
    assert _upstream_errors(admin_env, **window)["items"] == []
    # 本地准备失败等平台来源的尝试同样不进入上游错误。
    with closing(sqlite3.connect(database)) as connection:
        connection.execute(
            "UPDATE target_attempt_observations SET error_source='platform' WHERE id=?",
            (row["id"],),
        )
        connection.commit()
    assert _upstream_errors(admin_env, **window)["items"] == []
    # 未发送（RPM 占满）与取消/中断的尝试不会以 failed+upstream 落库；
    # 无可用 Target 的运行没有尝试。
    no_target = "observation-upstream-no-attempt"
    _, no_target_key = _create_route(admin_env, no_target, retry_budget=0)
    provider_id = _provider_id(admin_env, f"{no_target}-provider")
    status, _ = http_request(
        "PUT",
        f"{admin_env['admin']}/api/v1/providers/{provider_id}",
        payload={"is_enabled": False},
        headers=admin_env["auth"],
    )
    assert status == 200
    status, _ = _proxy(
        admin_env, no_target_key, no_target, [{"role": "user", "content": no_target}]
    )
    assert status >= 400
    assert (
        _upstream_errors(admin_env, **window, model=route_id, limit=100)["total"] == 0
    )


@pytest.mark.e2e
@pytest.mark.admin
def test_upstream_errors_report_interrupted_outcome(admin_env: dict[str, Any]) -> None:
    started_at = int(time.time() * 1000)
    model = "observation-upstream-interrupted"
    hang_port = find_free_port()
    hang_mock, _ = minimal_mock_provider(hang_port)

    class HangingStream(hang_mock.RequestHandlerClass):
        def do_POST(self) -> None:  # noqa: N802
            self._read_body()
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("connection", "close")
            self.end_headers()
            chunk = {
                "id": "chatcmpl-hang",
                "object": "chat.completion.chunk",
                "model": "gpt-4o-mini",
                "choices": [
                    {
                        "index": 0,
                        "delta": {"role": "assistant", "content": "partial"},
                        "finish_reason": None,
                    }
                ],
            }
            self.wfile.write(b"data: " + json.dumps(chunk).encode() + b"\n\n")
            self.wfile.flush()
            time.sleep(30.0)

    hang_mock.RequestHandlerClass = HangingStream
    try:
        first = _provider(admin_env, f"{model}-first", admin_env["mock"])
        second = _provider(
            admin_env, f"{model}-second", f"http://127.0.0.1:{hang_port}"
        )
        route_id, key = _route_with_targets(
            admin_env,
            model,
            [
                {
                    "provider_id": first,
                    "model": "gpt-4o-mini",
                    "enabled": True,
                    "priority": 1,
                    "target_retry_budget": 0,
                    "target_cooldown_ms": 0,
                },
                {
                    "provider_id": second,
                    "model": "gpt-4o-mini",
                    "enabled": True,
                    "priority": 0,
                    "target_retry_budget": 0,
                    "target_cooldown_ms": 0,
                },
            ],
        )
        window = _window(started_at)
        # 第一个 Target 立即以 5xx 失败并落库；第二个 Target 先发一块再挂起。
        # 客户端读到首块后断开，运行才被取消，因此存在一条 source=upstream
        # 的上游错误，其派生结果为 interrupted。
        request = Request(
            f"{admin_env['proxy']}/v1/chat/completions",
            data=json.dumps(
                {
                    "model": model,
                    "stream": True,
                    "messages": [
                        {"role": "user", "content": "observation-always-failed cancel"}
                    ],
                }
            ).encode(),
            headers={
                "authorization": f"Bearer {key}",
                "content-type": "application/json",
            },
        )
        response = urlopen(request, timeout=15)
        try:
            assert response.status == 200
            saw_partial = False
            while line := response.readline():
                if line.startswith(b"data:") and b"partial" in line:
                    saw_partial = True
                    break
            assert saw_partial
            first_row = _wait_for(
                "first upstream error before abort",
                lambda: next(
                    iter(_upstream_errors(admin_env, **window, model=route_id)["items"]),
                    None,
                ),
            )
            assert first_row["outcome"] == "in_progress"
            assert first_row["error"]["source"] == "upstream"
        finally:
            response.close()
        row = _wait_for(
            "interrupted upstream error",
            lambda: next(
                (
                    item
                    for item in _upstream_errors(admin_env, **window, model=route_id)[
                        "items"
                    ]
                    if item["outcome"] == "interrupted"
                ),
                None,
            ),
            timeout=20,
        )
        assert row["id"] == first_row["id"]
        assert row["provider_name"] == f"{model}-first"
        assert row["error"]["source"] == "upstream"
        # 取消与断线不是客户端错误。
        assert _client_errors(admin_env, **window, model=route_id)["items"] == []
    finally:
        hang_mock.shutdown()
        hang_mock.server_close()


@pytest.mark.e2e
@pytest.mark.admin
def test_upstream_errors_mark_client_tool_handoff_as_recovered(
    admin_env: dict[str, Any],
) -> None:
    started_at = int(time.time() * 1000)
    model = "observation-upstream-tool-handoff"
    route_id, key = _create_route(admin_env, model, retry_budget=1)
    tools = [
        {
            "type": "function",
            "function": {"name": "local_probe", "parameters": {"type": "object"}},
        }
    ]
    messages: list[dict[str, Any]] = [
        {"role": "user", "content": "observation-root-retry observation-tool-loop"}
    ]
    status, response = http_request(
        "POST",
        f"{admin_env['proxy']}/v1/chat/completions",
        payload={"model": model, "messages": messages, "tools": tools},
        headers={"authorization": f"Bearer {key}"},
    )
    assert status == 200, response
    assistant = _tool_call_message(response)
    assert assistant["tool_calls"][0]["function"]["name"] == "local_probe"
    window = _window(started_at)
    row = _wait_for(
        "upstream error for tool handoff",
        lambda: next(
            iter(_upstream_errors(admin_env, **window, model=route_id)["items"]),
            None,
        ),
    )
    # 运行把工具调用交给 Connect Client 后以 waiting_client 结束，这是成功响应。
    assert row["outcome"] == "recovered"
    assert row["error"]["source"] == "upstream"
    assert row["error"]["status_code"] == 503
    assert _client_errors(admin_env, **window, model=route_id)["items"] == []

    # 续接请求接替等待中的父运行；superseded 是成功的工具交接，不是中断。
    messages = [
        *messages,
        assistant,
        {
            "role": "tool",
            "tool_call_id": assistant["tool_calls"][0]["id"],
            "content": "done",
        },
    ]
    status, follow = http_request(
        "POST",
        f"{admin_env['proxy']}/v1/chat/completions",
        payload={"model": model, "messages": messages, "tools": tools},
        headers={"authorization": f"Bearer {key}"},
    )
    assert status == 200, follow

    def superseded() -> dict[str, Any] | None:
        detail = _detail(admin_env, row["interaction_id"])
        current = next(
            (run for run in detail["runs"] if run["id"] == row["run_id"]), None
        )
        return current if current and current["status"] == "superseded" else None

    _wait_for("parent run superseded by continuation", superseded)
    again = next(
        item
        for item in _upstream_errors(admin_env, **window, model=route_id)["items"]
        if item["id"] == row["id"]
    )
    assert again["outcome"] == "recovered"


@pytest.mark.e2e
@pytest.mark.admin
def test_upstream_errors_include_capability_attempts(admin_env: dict[str, Any]) -> None:
    started_at = int(time.time() * 1000)
    vision_model = "observation-capability-vision"
    vision_name = "observation-capability-vision-provider"
    vision_port = find_free_port()
    vision_mock, _ = minimal_mock_provider(vision_port)

    class FailingVision(vision_mock.RequestHandlerClass):
        def do_POST(self) -> None:  # noqa: N802
            self._read_body()
            self._write_json(
                503,
                {
                    "error": {
                        "type": "vision_unavailable",
                        "message": "vision upstream down",
                    }
                },
            )

    vision_mock.RequestHandlerClass = FailingVision
    try:
        vision_provider = _provider(
            admin_env,
            vision_name,
            f"http://127.0.0.1:{vision_port}",
            input_modalities=("text", "image"),
            reasoning_efforts=("medium",),
        )
        status, body = http_request(
            "POST",
            f"{admin_env['admin']}/api/v1/models",
            payload={
                "model_id": vision_model,
                "output_modality": "text",
                "targets": [
                    {
                        "provider_id": vision_provider,
                        "model": "gpt-4o-mini",
                        "first_token_timeout_ms": 120_000,
                        "target_retry_budget": 0,
                        "target_cooldown_ms": 0,
                    }
                ],
            },
            headers=admin_env["auth"],
        )
        assert status == 200, body
        vision_route = body["data"]["id"]
        status, body = http_request(
            "PUT",
            f"{admin_env['admin']}/api/v1/media-understanding/image",
            payload={
                "enabled": True,
                "model_id": vision_route,
                "thinking_level": "medium",
            },
            headers=admin_env["auth"],
        )
        assert status == 200, body
        status, body = http_request(
            "POST",
            f"{admin_env['admin']}/api/v1/api-keys",
            payload={
                "name": "observation-capability-key",
                "model_ids": [vision_route],
                "mcp_access_enabled": True,
                "transparent_injection_enabled": True,
                "inject_media_understanding": True,
            },
            headers=admin_env["auth"],
        )
        assert status == 200, body
        key = body["data"]["key"]
        auth = {"Authorization": f"Bearer {key}"}
        image = base64.b64decode(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII="
        )
        status, upload = http_request(
            "POST",
            f"{admin_env['proxy']}/v1/artifacts/uploads",
            {"mime_type": "image/png", "size": len(image)},
            auth,
        )
        assert status == 201, upload
        connection = HTTPConnection(
            "127.0.0.1", int(admin_env["proxy"].rsplit(":", 1)[1]), timeout=20
        )
        try:
            connection.request(
                "PUT",
                f"/v1/artifacts/uploads/{upload['upload_id']}/parts/1",
                image,
                {**auth, "x-upload-token": upload["upload_token"]},
            )
            part_response = connection.getresponse()
            assert part_response.status == 200
            part = json.loads(part_response.read())
        finally:
            connection.close()
        status, artifact = http_request(
            "POST",
            f"{admin_env['proxy']}/v1/artifacts/uploads/{upload['upload_id']}/complete",
            {"upload_token": upload["upload_token"], "parts": [part]},
            auth,
        )
        assert status == 200 and artifact["mime_type"] == "image/png", artifact
        # 媒体理解由 MCP 工具同步触发；它经 Provider 发出的尝试属于 capability。
        status, queried = http_request(
            "POST",
            f"{admin_env['proxy']}/mcp",
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "StraviaRead",
                    "arguments": {"path": artifact["path"]},
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities": {},
                    },
                },
            },
            {
                **auth,
                "Accept": "application/json, text/event-stream",
                "MCP-Protocol-Version": "2026-07-28",
                "Mcp-Method": "tools/call",
                "Mcp-Name": "StraviaRead",
            },
            timeout=60,
        )
        assert status == 200, queried
        row = _wait_for(
            "capability upstream error",
            lambda: next(
                (
                    item
                    for item in _upstream_errors(
                        admin_env, **_window(started_at), limit=100
                    )["items"]
                    if item["provider_name"] == vision_name
                ),
                None,
            ),
            timeout=30,
        )
        assert row["upstream_model"] == "gpt-4o-mini"
        assert row["error"]["source"] == "upstream"
        assert row["error"]["status_code"] == 503
        assert row["error"]["message"] == "vision upstream down"
        assert row["run_id"] and row["interaction_id"]
    finally:
        vision_mock.shutdown()
        vision_mock.server_close()


@pytest.mark.e2e
@pytest.mark.admin
def test_observation_sse_change_carries_error_source(admin_env: dict[str, Any]) -> None:
    model = "observation-sse-error-source"
    route_id, key = _create_route(admin_env, model, retry_budget=1)
    before = _forest(admin_env)["snapshot_sequence"]
    status, _ = _proxy(
        admin_env,
        key,
        model,
        [{"role": "user", "content": "observation-root-retry sse"}],
    )
    assert status == 200

    def failed_attempt(observed: dict[str, Any]) -> bool:
        data = observed["data"]
        return (
            observed["event"] == "observation"
            and data["kind"] == "target_attempt_finished"
            and data.get("error_source") == "upstream"
        )

    change = _sse_event(admin_env, before, failed_attempt)
    assert change["data"]["error_source"] == "upstream"

    completed = _sse_event(
        admin_env,
        before,
        lambda observed: (
            observed["event"] == "observation"
            and observed["data"]["kind"] == "target_attempt_finished"
            and "error_source" not in observed["data"]
        ),
    )
    assert "error_source" not in completed["data"]
