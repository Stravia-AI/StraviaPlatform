from __future__ import annotations

import tempfile
from pathlib import Path

import pytest

from tests.common.helpers import (
    find_free_port,
    minimal_mock_provider,
    start_stravia_server,
    stop_stravia_server,
    wait_until_ready,
)


@pytest.fixture(scope="module")
def admin_env(stravia_binary: Path) -> dict[str, str]:
    admin_token = "admin-e2e-token"
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
                    "--admin-token",
                    admin_token,
                ],
            )
            admin_base = f"http://127.0.0.1:{server_port}"
            proxy_base = admin_base
            auth_headers = {"authorization": f"Bearer {admin_token}"}

            wait_until_ready(
                f"{admin_base}/api/v1/status",
                timeout=40.0,
                headers=auth_headers,
            )

            try:
                yield {
                    "admin": admin_base,
                    "proxy": proxy_base,
                    "mock": f"http://127.0.0.1:{mock_port}",
                    "auth": auth_headers,
                }
            finally:
                stop_stravia_server(proc, logs)
    finally:
        mock_server.shutdown()
        mock_server.server_close()
