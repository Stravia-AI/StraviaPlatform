from __future__ import annotations

import os
import re
import subprocess
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
from pathlib import Path
from typing import Iterator

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


@pytest.mark.e2e
@pytest.mark.admin
def test_fresh_server_is_claimed_once_then_completed_and_logged_in(
    stravia_binary: Path,
) -> None:
    """Exercise the smallest complete Server setup/auth journey at the HTTP seam."""
    with tempfile.TemporaryDirectory(prefix="stravia-auth-tracer-") as data_dir:
        port = find_free_port()
        base = f"http://127.0.0.1:{port}"
        proc, logs = start_stravia_server(
            stravia_binary=stravia_binary,
            args=[
                "--host",
                "127.0.0.1",
                "--port",
                str(port),
                "--data-dir",
                data_dir,
            ],
        )
        try:
            wait_until_ready(f"{base}/api/v1/auth/state", timeout=40.0)
            token = wait_for_setup_token(logs, proc)

            visitor = WebSession(base)
            status, state = visitor.request("GET", "/api/v1/auth/state")
            assert status == 200
            assert state == {
                "mode": "setup",
                "authenticated": False,
                "setup_authorized": False,
                "username": None,
            }
            status, _ = visitor.request(
                "POST",
                "/api/v1/setup/complete",
                {
                    "database": {
                        "backend": "sqlite",
                        "path": str(Path(data_dir) / "gateway.db"),
                    },
                    "username": "owner",
                    "password": "correct horse battery staple",
                },
            )
            assert status == 401

            operator = WebSession(base)
            status, _ = operator.request(
                "POST", "/api/v1/setup/claim", {"token": token}
            )
            assert status == 204
            assert operator.request("GET", "/api/v1/status")[0] == 404
            status, _ = visitor.request(
                "POST", "/api/v1/setup/claim", {"token": token}
            )
            assert status == 401

            status, body = operator.request(
                "POST",
                "/api/v1/setup/complete",
                {
                    "database": {
                        "backend": "sqlite",
                        "path": str(Path(data_dir) / "gateway.db"),
                    },
                    "username": "owner",
                    "password": "correct horse battery staple",
                },
                timeout=40.0,
            )
            assert status == 200, body
            assert body == {"mode": "server"}

            status, state = visitor.request("GET", "/api/v1/auth/state")
            assert status == 200
            assert state == {
                "mode": "server",
                "authenticated": False,
                "setup_authorized": False,
                "username": None,
            }
            status, login = visitor.request(
                "POST",
                "/api/v1/auth/login",
                {"username": "owner", "password": "correct horse battery staple"},
            )
            assert status == 200, login
            assert login["username"] == "owner"
            assert login["access_expires_at"] < login["session_expires_at"]
            assert "access_token" not in login
            assert "refresh_token" not in login
            assert visitor.cookie_value("stravia_access")
            assert visitor.cookie_value("stravia_refresh")

            status, state = visitor.request("GET", "/api/v1/auth/state")
            assert status == 200
            assert state == {
                "mode": "server",
                "authenticated": True,
                "setup_authorized": False,
                "username": "owner",
            }
            status, _ = visitor.request("GET", "/api/v1/status")
            assert status == 200
        finally:
            stop_stravia_server(proc, logs)


@contextmanager
def _initialized_server(stravia_binary: Path) -> Iterator[dict[str, object]]:
    with tempfile.TemporaryDirectory(prefix="stravia-auth-e2e-") as data_dir:
        port = find_free_port()
        base = f"http://127.0.0.1:{port}"
        proc, logs = start_stravia_server(
            stravia_binary=stravia_binary,
            args=["--host", "127.0.0.1", "--port", str(port), "--data-dir", data_dir],
        )
        try:
            wait_until_ready(f"{base}/api/v1/auth/state", timeout=40.0)
            token = wait_for_setup_token(logs, proc)
            session = initialize_server(
                base,
                token,
                {"backend": "sqlite", "path": str(Path(data_dir) / "gateway.db")},
                username="owner",
                password="correct horse battery staple",
            )
            yield {
                "base": base,
                "data_dir": Path(data_dir),
                "config": Path(data_dir) / "server.toml",
                "proc": proc,
                "logs": logs,
                "session": session,
            }
        finally:
            stop_stravia_server(proc, logs)


def _login(base: str, username: str = "owner", password: str = "correct horse battery staple") -> WebSession:
    session = WebSession(base)
    status, body = session.request(
        "POST", "/api/v1/auth/login", {"username": username, "password": password}
    )
    assert status == 200, body
    return session


@pytest.mark.e2e
@pytest.mark.admin
def test_storage_outage_does_not_report_logout_success_or_discard_the_session(
    stravia_binary: Path,
) -> None:
    import sqlite3

    with _initialized_server(stravia_binary) as server:
        session = server["session"]
        refresh_only = _login(server["base"])
        access_cookie = next(
            cookie for cookie in refresh_only.cookies if cookie.name == "stravia_access"
        )
        refresh_only.cookies.clear(
            access_cookie.domain, access_cookie.path, access_cookie.name
        )
        database = sqlite3.connect(server["data_dir"] / "gateway.db")
        try:
            database.execute("ALTER TABLE admin_sessions RENAME TO unavailable_sessions")
            database.commit()
            statuses = {
                "state": session.request("GET", "/api/v1/auth/state")[0],
                "management": session.request("GET", "/api/v1/status")[0],
                "credentials": session.request(
                    "PUT",
                    "/api/v1/auth/credentials",
                    {
                        "current_password": "correct horse battery staple",
                        "username": "owner",
                        "password": "correct horse battery staple",
                    },
                )[0],
                "logout": session.request("POST", "/api/v1/auth/logout")[0],
                "refresh_logout": refresh_only.request("POST", "/api/v1/auth/logout")[0],
            }
        finally:
            database.execute("ALTER TABLE unavailable_sessions RENAME TO admin_sessions")
            database.commit()
            database.close()

        assert statuses == {
            "state": 503,
            "management": 503,
            "credentials": 503,
            "logout": 503,
            "refresh_logout": 503,
        }
        assert session.request("GET", "/api/v1/status")[0] == 200
        assert refresh_only.request("POST", "/api/v1/auth/refresh")[0] == 200
        assert session.request("POST", "/api/v1/auth/logout")[0] == 204
        assert session.request("GET", "/api/v1/status")[0] == 401


@pytest.mark.e2e
@pytest.mark.admin
def test_setup_token_concurrent_claim_has_one_winner(stravia_binary: Path) -> None:
    with tempfile.TemporaryDirectory(prefix="stravia-auth-claim-") as data_dir:
        port = find_free_port()
        base = f"http://127.0.0.1:{port}"
        proc, logs = start_stravia_server(
            stravia_binary=stravia_binary,
            args=["--host", "127.0.0.1", "--port", str(port), "--data-dir", data_dir],
        )
        try:
            wait_until_ready(f"{base}/api/v1/auth/state", timeout=40.0)
            token = wait_for_setup_token(logs, proc)

            def claim(_: int) -> int:
                status, _ = WebSession(base).request(
                    "POST", "/api/v1/setup/claim", {"token": token}
                )
                return status

            with ThreadPoolExecutor(max_workers=8) as pool:
                statuses = list(pool.map(claim, range(8)))
            assert statuses.count(204) == 1
            assert all(status == 204 or status >= 400 for status in statuses)
        finally:
            stop_stravia_server(proc, logs)


@pytest.mark.e2e
@pytest.mark.admin
def test_removed_database_and_admin_token_environment_does_not_bypass_setup(
    stravia_binary: Path,
) -> None:
    with tempfile.TemporaryDirectory(prefix="stravia-auth-env-cutover-") as data_dir:
        port = find_free_port()
        base = f"http://127.0.0.1:{port}"
        proc, logs = start_stravia_server(
            stravia_binary=stravia_binary,
            args=["--host", "127.0.0.1", "--port", str(port), "--data-dir", data_dir],
            env={
                "STRAVIA_STORAGE_BACKEND": "postgres",
                "STRAVIA_POSTGRES_DSN": "postgresql://127.0.0.1:1/removed",
                "STRAVIA_ADMIN_TOKEN": "removed-static-token",
            },
        )
        try:
            wait_until_ready(f"{base}/api/v1/auth/state", timeout=40.0)
            status, state = WebSession(base).request("GET", "/api/v1/auth/state")
            assert status == 200
            assert state["mode"] == "setup"
            assert wait_for_setup_token(logs, proc)
            status, _ = http_request(
                "GET",
                f"{base}/api/v1/status",
                headers={"authorization": "Bearer removed-static-token"},
            )
            assert status == 404
            assert not (Path(data_dir) / "gateway.db").exists()
        finally:
            stop_stravia_server(proc, logs)


@pytest.mark.e2e
@pytest.mark.admin
def test_setup_connection_failure_can_be_corrected_in_same_session(
    stravia_binary: Path,
) -> None:
    with tempfile.TemporaryDirectory(prefix="stravia-auth-correct-") as data_dir:
        port = find_free_port()
        base = f"http://127.0.0.1:{port}"
        proc, logs = start_stravia_server(
            stravia_binary=stravia_binary,
            args=["--host", "127.0.0.1", "--port", str(port), "--data-dir", data_dir],
        )
        try:
            wait_until_ready(f"{base}/api/v1/auth/state", timeout=40.0)
            operator = WebSession(base)
            token = wait_for_setup_token(logs, proc)
            assert operator.request(
                "POST", "/api/v1/setup/claim", {"token": token}
            )[0] == 204

            status, _ = operator.request(
                "POST",
                "/api/v1/setup/test",
                {"database": {"backend": "postgres", "url": "postgresql://127.0.0.1:1/unreachable"}},
                timeout=40.0,
            )
            assert status >= 400
            database = {"backend": "sqlite", "path": str(Path(data_dir) / "gateway.db")}
            status, body = operator.request(
                "POST", "/api/v1/setup/test", {"database": database}
            )
            assert status == 204, body
            status, body = operator.request(
                "POST",
                "/api/v1/setup/complete",
                {
                    "database": database,
                    "username": "owner",
                    "password": "correct horse battery staple",
                },
                timeout=40.0,
            )
            assert status == 200, body
            assert _login(base).request("GET", "/api/v1/status")[0] == 200
        finally:
            stop_stravia_server(proc, logs)


@pytest.mark.e2e
@pytest.mark.admin
def test_concurrent_setup_completion_never_overwrites_the_admin(
    stravia_binary: Path,
) -> None:
    with tempfile.TemporaryDirectory(prefix="stravia-auth-complete-") as data_dir:
        port = find_free_port()
        base = f"http://127.0.0.1:{port}"
        proc, logs = start_stravia_server(
            stravia_binary=stravia_binary,
            args=["--host", "127.0.0.1", "--port", str(port), "--data-dir", data_dir],
        )
        try:
            wait_until_ready(f"{base}/api/v1/auth/state", timeout=40.0)
            operator = WebSession(base)
            assert operator.request(
                "POST",
                "/api/v1/setup/claim",
                {"token": wait_for_setup_token(logs, proc)},
            )[0] == 204
            database = {"backend": "sqlite", "path": str(Path(data_dir) / "gateway.db")}
            cookie = operator.cookie_header()

            def complete(credentials: tuple[str, str]) -> int:
                username, password = credentials
                status, _ = http_request(
                    "POST",
                    f"{base}/api/v1/setup/complete",
                    payload={"database": database, "username": username, "password": password},
                    headers={"cookie": cookie, "origin": base, "x-stravia-csrf": "1"},
                    timeout=40.0,
                )
                return status

            candidates = [
                ("first-owner", "first horse battery staple"),
                ("second-owner", "second horse battery staple"),
            ]
            with ThreadPoolExecutor(max_workers=2) as pool:
                statuses = list(pool.map(complete, candidates))
            assert any(status == 200 for status in statuses)

            successful_logins = 0
            for username, password in candidates:
                session = WebSession(base)
                status, _ = session.request(
                    "POST", "/api/v1/auth/login", {"username": username, "password": password}
                )
                successful_logins += status == 200
            assert successful_logins == 1
        finally:
            stop_stravia_server(proc, logs)


@pytest.mark.e2e
@pytest.mark.admin
def test_failed_setup_preserves_commit_order_and_restart_replaces_setup_credentials(
    stravia_binary: Path,
) -> None:
    with tempfile.TemporaryDirectory(prefix="stravia-auth-commit-order-") as directory:
        root = Path(directory)
        config_parent = root / "config"
        config = config_parent / "server.toml"
        database = {"backend": "sqlite", "path": str(root / "database" / "gateway.db")}
        port = find_free_port()
        base = f"http://127.0.0.1:{port}"
        args = [
            "--host", "127.0.0.1", "--port", str(port),
            "--data-dir", str(root), "--config", str(config),
        ]
        proc, logs = start_stravia_server(stravia_binary=stravia_binary, args=args)
        try:
            wait_until_ready(f"{base}/api/v1/auth/state", timeout=40.0)
            original_token = wait_for_setup_token(logs, proc)
            operator = WebSession(base)
            assert operator.request(
                "POST", "/api/v1/setup/claim", {"token": original_token}
            )[0] == 204
            original_cookie = operator.cookie_header()
            config_parent.write_text("block configuration persistence", encoding="utf-8")
            status, _ = operator.request(
                "POST", "/api/v1/setup/complete",
                {"database": database, "username": "must-not-exist", "password": "valid initial password"},
                timeout=40.0,
            )
            assert status == 500
            config_parent.unlink()
            status, _ = operator.request(
                "POST", "/api/v1/setup/complete",
                {"database": database, "username": "owner", "password": ""},
                timeout=40.0,
            )
            assert status == 400
            assert config.is_file()
        finally:
            stop_stravia_server(proc, logs)

        proc, logs = start_stravia_server(stravia_binary=stravia_binary, args=args)
        try:
            wait_until_ready(f"{base}/api/v1/auth/state", timeout=40.0)
            visitor = WebSession(base)
            status, state = visitor.request(
                "GET", "/api/v1/auth/state", headers={"cookie": original_cookie}
            )
            assert status == 200
            assert state["mode"] == "setup"
            assert not state["setup_authorized"]
            assert visitor.request(
                "POST", "/api/v1/setup/claim", {"token": original_token}
            )[0] == 401
            replacement_token = wait_for_setup_token(logs, proc)
            session = initialize_server(base, replacement_token, database)
            assert session.request("GET", "/api/v1/status")[0] == 200
            assert WebSession(base).request(
                "POST", "/api/v1/auth/login",
                {"username": "must-not-exist", "password": "valid initial password"},
            )[0] == 401
        finally:
            stop_stravia_server(proc, logs)


@pytest.mark.e2e
@pytest.mark.admin
def test_cookie_csrf_refresh_rotation_and_current_session_logout(
    stravia_binary: Path,
) -> None:
    with _initialized_server(stravia_binary) as server:
        base = str(server["base"])
        first = server["session"]
        assert isinstance(first, WebSession)
        second = _login(base)

        access_cookie = next(cookie for cookie in first.cookies if cookie.name == "stravia_access")
        refresh_cookie = next(cookie for cookie in first.cookies if cookie.name == "stravia_refresh")
        assert access_cookie.has_nonstandard_attr("HttpOnly")
        assert refresh_cookie.has_nonstandard_attr("HttpOnly")
        old_access = access_cookie.value
        old_refresh = refresh_cookie.value

        status, _ = http_request(
            "POST",
            f"{base}/api/v1/auth/logout",
            headers={"cookie": first.cookie_header(), "origin": base},
        )
        assert status == 403
        status, _ = http_request(
            "POST",
            f"{base}/api/v1/auth/logout",
            headers={
                "cookie": first.cookie_header(),
                "origin": "https://attacker.invalid",
                "x-stravia-csrf": "1",
            },
        )
        assert status == 403
        assert first.request("GET", "/api/v1/status")[0] == 200

        status, body = first.request("POST", "/api/v1/auth/refresh", {})
        assert status == 200, body
        assert first.cookie_value("stravia_refresh") != old_refresh

        status, _ = http_request(
            "POST",
            f"{base}/api/v1/auth/refresh",
            payload={},
            headers={
                "cookie": f"stravia_refresh={old_refresh}",
                "origin": base,
                "x-stravia-csrf": "1",
            },
        )
        assert status == 401
        assert first.request("GET", "/api/v1/status")[0] == 200

        current_access = first.cookie_value("stravia_access")
        current_refresh = first.cookie_value("stravia_refresh")
        assert current_access and current_refresh
        status, body = first.request("POST", "/api/v1/auth/logout", {})
        assert status == 204, body
        status, _ = http_request(
            "GET",
            f"{base}/api/v1/status",
            headers={"cookie": f"stravia_access={current_access}"},
        )
        assert status == 401
        status, _ = http_request(
            "POST",
            f"{base}/api/v1/auth/refresh",
            payload={},
            headers={
                "cookie": f"stravia_refresh={current_refresh}",
                "origin": base,
                "x-stravia-csrf": "1",
            },
        )
        assert status == 401
        assert second.request("GET", "/api/v1/status")[0] == 200


@pytest.mark.e2e
@pytest.mark.admin
def test_changing_credentials_revokes_every_existing_session(
    stravia_binary: Path,
) -> None:
    with _initialized_server(stravia_binary) as server:
        base = str(server["base"])
        first = server["session"]
        assert isinstance(first, WebSession)
        second = _login(base)

        status, body = first.request(
            "PUT",
            "/api/v1/auth/credentials",
            {
                "current_password": "correct horse battery staple",
                "username": "renamed-owner",
                "password": "an even better horse battery staple",
            },
        )
        assert status == 204, body
        assert first.request("GET", "/api/v1/status")[0] == 401
        assert second.request("GET", "/api/v1/status")[0] == 401

        rejected = WebSession(base)
        status, _ = rejected.request(
            "POST",
            "/api/v1/auth/login",
            {"username": "owner", "password": "correct horse battery staple"},
        )
        assert status == 401
        current = _login(
            base, "renamed-owner", "an even better horse battery staple"
        )
        assert current.request("GET", "/api/v1/status")[0] == 200


@pytest.mark.e2e
@pytest.mark.admin
def test_configured_server_restart_uses_saved_database_without_reopening_setup(
    stravia_binary: Path,
) -> None:
    with _initialized_server(stravia_binary) as server:
        base = str(server["base"])
        proc = server["proc"]
        logs = server["logs"]
        assert isinstance(proc, subprocess.Popen)
        assert isinstance(logs, list)
        stop_stravia_server(proc, logs)

        port = find_free_port()
        restarted_base = f"http://127.0.0.1:{port}"
        restarted, restarted_logs = start_stravia_server(
            stravia_binary=stravia_binary,
            args=[
                "--host",
                "127.0.0.1",
                "--port",
                str(port),
                "--data-dir",
                str(server["data_dir"]),
            ],
        )
        try:
            wait_until_ready(f"{restarted_base}/api/v1/auth/state", timeout=40.0)
            status, state = WebSession(restarted_base).request("GET", "/api/v1/auth/state")
            assert status == 200
            assert state["mode"] == "server"
            assert state["setup_authorized"] is False
            session = _login(restarted_base)
            assert session.request("GET", "/api/v1/status")[0] == 200
            assert not any("Stravia setup token:" in line for line in restarted_logs)
        finally:
            stop_stravia_server(restarted, restarted_logs)


@pytest.mark.e2e
@pytest.mark.admin
def test_interactive_recovery_preserves_data_and_revokes_all_sessions(
    stravia_binary: Path,
) -> None:
    with _initialized_server(stravia_binary) as server:
        base = str(server["base"])
        first = server["session"]
        assert isinstance(first, WebSession)
        second = _login(base)
        old_cookie_headers = [first.cookie_header(), second.cookie_header()]

        status, body = first.request(
            "POST",
            "/api/v1/providers",
            {
                "name": "survives-recovery",
                "source": {
                    "type": "custom",
                    "vendor": "custom",
                    "protocol": "openai",
                    "base_url": "http://127.0.0.1:9/v1",
                },
                "credential": {"type": "api_key", "value": "unused"},
            },
        )
        assert status == 200, body
        provider_id = body["data"]["id"]

        proc = server["proc"]
        logs = server["logs"]
        assert isinstance(proc, subprocess.Popen)
        assert isinstance(logs, list)
        stop_stravia_server(proc, logs)

        new_password = "replacement horse battery staple"
        _recover_in_terminal(
            stravia_binary,
            Path(str(server["config"])),
            "recovered-owner",
            new_password,
        )

        port = find_free_port()
        recovered_base = f"http://127.0.0.1:{port}"
        recovered, recovered_logs = start_stravia_server(
            stravia_binary=stravia_binary,
            args=[
                "--host",
                "127.0.0.1",
                "--port",
                str(port),
                "--data-dir",
                str(server["data_dir"]),
                "--config",
                str(server["config"]),
            ],
        )
        try:
            wait_until_ready(f"{recovered_base}/api/v1/auth/state", timeout=40.0)
            for cookie_header in old_cookie_headers:
                status, _ = http_request(
                    "GET",
                    f"{recovered_base}/api/v1/status",
                    headers={"cookie": cookie_header},
                )
                assert status == 401

            session = _login(recovered_base, "recovered-owner", new_password)
            status, body = session.request("GET", "/api/v1/providers")
            assert status == 200, body
            assert provider_id in {provider["id"] for provider in body["data"]}
        finally:
            stop_stravia_server(recovered, recovered_logs)


def _recover_in_terminal(binary: Path, config: Path, username: str, password: str) -> None:
    """Use a real terminal so hidden password input is exercised, not bypassed."""
    argv = [str(binary), "--config", str(config), "recover-admin"]
    deadline = time.monotonic() + 40
    if os.name == "nt":
        from winpty import PtyProcess

        terminal = PtyProcess.spawn(argv, dimensions=(40, 160))

        def read_chunk() -> str:
            terminal.fileobj.settimeout(max(0.01, deadline - time.monotonic()))
            return terminal.read()

        def send(value: str) -> None:
            terminal.write(value + "\r\n")

        def finish() -> int:
            return terminal.wait()

        def close() -> None:
            terminal.close(force=True)
    else:
        import errno
        import pty
        import select
        import signal

        pid, fd = pty.fork()
        if pid == 0:
            os.execv(str(binary), argv)
        reaped = False

        def read_chunk() -> str:
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not select.select([fd], [], [], remaining)[0]:
                raise TimeoutError("credential recovery terminal timed out")
            try:
                data = os.read(fd, 4096)
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
                raise EOFError from error
            if not data:
                raise EOFError
            return data.decode("utf-8")

        def send(value: str) -> None:
            os.write(fd, (value + "\n").encode())

        def finish() -> int:
            nonlocal reaped
            _, status = os.waitpid(pid, 0)
            reaped = True
            return os.waitstatus_to_exitcode(status)

        def close() -> None:
            os.close(fd)
            if not reaped:
                os.kill(pid, signal.SIGKILL)
                os.waitpid(pid, 0)

    transcript = ""
    try:
        for prompt, value in (
            ("New administrator username: ", username),
            ("New administrator password: ", password),
            ("Confirm new administrator password: ", password),
        ):
            while prompt not in re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", transcript):
                transcript += read_chunk()
            send(value)
        while True:
            try:
                transcript += read_chunk()
            except EOFError:
                break
        assert finish() == 0, "credential recovery failed"
        password_was_echoed = password in transcript
        assert not password_was_echoed, "credential recovery echoed the password"
    finally:
        close()


@pytest.mark.e2e
@pytest.mark.admin
@pytest.mark.parametrize(
    "config_text",
    [
        "this is not valid TOML = [",
        "[database]\nbackend = \"postgres\"\nurl = \"postgresql://127.0.0.1:1/unreachable\"\n",
    ],
    ids=["malformed", "unreachable"],
)
def test_invalid_config_fails_without_setup_or_sqlite_fallback(
    stravia_binary: Path, config_text: str
) -> None:
    with tempfile.TemporaryDirectory(prefix="stravia-auth-invalid-config-") as data_dir:
        config = Path(data_dir) / "server.toml"
        config.write_text(config_text, encoding="utf-8")
        result = subprocess.run(
            [
                str(stravia_binary),
                "--host",
                "127.0.0.1",
                "--port",
                str(find_free_port()),
                "--data-dir",
                data_dir,
                "--config",
                str(config),
            ],
            capture_output=True,
            text=True,
            timeout=20.0,
            check=False,
        )
        output = result.stdout + result.stderr
        assert result.returncode != 0
        assert "Stravia setup token:" not in output
        assert not (Path(data_dir) / "gateway.db").exists()
