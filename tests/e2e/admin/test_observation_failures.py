from __future__ import annotations

import sqlite3
import tempfile
import threading
import time
from contextlib import closing
from pathlib import Path
from typing import Any, Callable

import pytest

from tests.common.helpers import (
    WebSession,
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
    _create_route,
    _detail,
    _forest,
    _proxy,
    _route_interactions,
    _sse_event,
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
    base = f"http://127.0.0.1:{port}"
    wait_until_ready(f"{base}/api/v1/auth/state", timeout=40.0)
    session = initialize_server(
        base,
        wait_for_setup_token(logs, process),
        {"backend": "sqlite", "path": str(data_dir / "gateway.db")},
    )
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

                trace_root = data_dir / "observation-debug"
                displaced_root = data_dir / "observation-debug-before-failure"
                trace_root.rename(displaced_root)
                trace_root.write_bytes(b"regular file blocks managed trace directories")

                failed_trace_messages = [
                    {"role": "user", "content": blocked_prompt}
                ]
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
                        trace = detail["runs"][0]["trace"]
                        if not trace or trace["status"] != "partial":
                            continue
                        try:
                            with closing(sqlite3.connect(data_dir / "gateway.db")) as connection:
                                persisted = connection.execute(
                                    "SELECT status, partial_reason, completed_at FROM debug_trace_manifests WHERE run_id = ?",
                                    (detail["runs"][0]["id"],),
                                ).fetchone()
                        except sqlite3.OperationalError:
                            return None
                        if (
                            persisted
                            and persisted[0] == "partial"
                            and "storage_error" in persisted[1]
                            and persisted[2] is None
                        ):
                            return interaction, detail
                    return None

                failed_interaction, active_detail = _wait_for(
                    "persistent partial Trace manifest before inference completion",
                    active_partial,
                )
                assert request_outcome == []
                trace = active_detail["runs"][0]["trace"]
                assert "storage_error" in trace["reasons"]
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
                    lambda: (lambda detail: detail if detail["runs"][0]["status"] == "completed" else None)(
                        _detail(env, failed_interaction["id"])
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
                    lambda: (lambda detail: detail if detail["runs"][0]["status"] == "completed" else None)(
                        _detail(env, child["id"])
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
                wait_until_ready(f"{restart_base}/api/v1/auth/state", timeout=40.0)
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
            database = data_dir / "gateway.db"
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
                detail = _wait_for(
                    "finished Trace manifest",
                    lambda: (lambda value: value if value["runs"][0].get("trace") else None)(
                        _detail(env, interaction["id"])
                    ),
                )
                trace_id = detail["runs"][0]["trace"]["trace_id"]
            finally:
                stop_stravia_server(process, logs)

            trace_directory = data_dir / "observation-debug" / trace_id
            assert trace_directory.is_dir()
            with closing(sqlite3.connect(database)) as connection:
                updated = connection.execute(
                    "UPDATE debug_trace_manifests SET tombstoned = 1 WHERE trace_id = ?",
                    (trace_id,),
                ).rowcount
                connection.commit()
            assert updated == 1

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
                wait_until_ready(f"{restart_base}/readyz", timeout=40.0)
                deadline = time.time() + 10.0
                while trace_directory.exists() and time.time() < deadline:
                    time.sleep(0.1)
                assert not trace_directory.exists()
                with closing(sqlite3.connect(database)) as connection:
                    count = connection.execute(
                        "SELECT COUNT(*) FROM debug_trace_manifests WHERE trace_id = ?",
                        (trace_id,),
                    ).fetchone()[0]
                assert count == 0
            finally:
                stop_stravia_server(restarted, restarted_logs)
    finally:
        mock_server.shutdown()
        mock_server.server_close()


@pytest.mark.e2e
@pytest.mark.admin
def test_restart_interrupts_running_activity_but_preserves_waiting_client(
    stravia_binary: Path,
) -> None:
    mock_port = find_free_port()
    mock_server, _ = minimal_mock_provider(mock_port)
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
                                [{"role": "user", "content": "observation-delay interrupt this live request"}],
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
            finally:
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
                wait_until_ready(f"{restart_base}/api/v1/auth/state", timeout=40.0)
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
                    event["kind"] == "process_restarted"
                    for event in interrupted["runs"][0]["events"]
                )

                waiting = _detail(restarted_env, waiting_id)
                assert waiting["interaction"]["status"] == "waiting_client"
                assert waiting["runs"][0]["status"] == "waiting_client"
                assert waiting["runs"][0]["terminal_reason"] is None
            finally:
                stop_stravia_server(restarted, restarted_logs)
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
                trace_directory = data_dir / "observation-debug" / trace["trace_id"]
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
                with closing(sqlite3.connect(data_dir / "gateway.db")) as connection:
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
                    assert connection.execute(
                        "SELECT COUNT(*) FROM debug_trace_manifests WHERE trace_id = ?",
                        (trace["trace_id"],),
                    ).fetchone()[0] == 0
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
                old_sequence = _wait_for(
                    "historical rejection event",
                    lambda: (lambda sequence: sequence if sequence > interaction_sequence else None)(
                        _forest(env)["snapshot_sequence"]
                    ),
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
                assert empty["snapshot_sequence"] == old_sequence

                reset = _sse_event(env, old_sequence - 1)
                assert reset["event"] == "reset_required"
                assert reset["data"] == {
                    "reset_required": True,
                    "snapshot_sequence": old_sequence,
                }

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
                assert _detail(env, replacement["id"])["snapshot_sequence"] > old_sequence
            finally:
                stop_stravia_server(process, logs)
    finally:
        mock_server.shutdown()
        mock_server.server_close()
