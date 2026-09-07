from __future__ import annotations

import tempfile
from pathlib import Path
from typing import Any

import pytest

from tests.common.helpers import (
    find_free_port,
    initialize_server,
    minimal_mock_provider,
    start_stravia_server,
    stop_stravia_server,
    wait_for_setup_token,
    wait_until_ready,
)


@pytest.fixture(scope="module")
def admin_env(stravia_binary: Path) -> dict[str, Any]:
    mock_port = find_free_port()
    server_port = find_free_port()

    mock_server, _ = minimal_mock_provider(mock_port)

    try:
        with tempfile.TemporaryDirectory(prefix="stravia-admin-e2e-") as data_dir:
            proc, logs = start_stravia_server(
                stravia_binary=stravia_binary,
                args=[
                    "--host",
                    "127.0.0.1",
                    "--port",
                    str(server_port),
                    "--data-dir",
                    data_dir,
                ],
            )
            admin_base = f"http://127.0.0.1:{server_port}"
            proxy_base = admin_base
            wait_until_ready(f"{admin_base}/api/v1/auth/state", timeout=40.0)
            setup_token = wait_for_setup_token(logs, proc)
            session = initialize_server(
                admin_base,
                setup_token,
                {"backend": "sqlite", "path": str(Path(data_dir) / "gateway.db")},
            )

            try:
                yield {
                    "admin": admin_base,
                    "proxy": proxy_base,
                    "mock": f"http://127.0.0.1:{mock_port}",
                    "auth": session.auth_headers(),
                    "username": "admin",
                    "password": "correct horse battery staple",
                    "data_dir": Path(data_dir),
                    "logs": logs,
                    "process": proc,
                }
            finally:
                stop_stravia_server(proc, logs)
    finally:
        mock_server.shutdown()
        mock_server.server_close()
