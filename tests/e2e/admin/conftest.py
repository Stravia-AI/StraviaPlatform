from __future__ import annotations

import re
import tempfile
from pathlib import Path
from typing import Any

import pytest

from tests.common.helpers import (
    SERVER_STARTUP_TIMEOUT,
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
    mock_server, _ = minimal_mock_provider(mock_port)
    proxy_cors_origin = "https://trusted-client.example"

    try:
        with tempfile.TemporaryDirectory(prefix="stravia-admin-e2e-") as data_dir:
            proc, logs = start_stravia_server(
                stravia_binary=stravia_binary,
                args=[
                    "--host",
                    "127.0.0.1",
                    "--port",
                    "0",
                    "--data-dir",
                    data_dir,
                    "--proxy-cors-origin",
                    proxy_cors_origin,
                ],
            )
            try:
                # Let the server own its ephemeral port, rather than releasing
                # a probe socket and racing other module workers to rebind it.
                setup_token = wait_for_setup_token(
                    logs, proc, timeout=SERVER_STARTUP_TIMEOUT,
                )
                address = next(
                    (
                        match.group(1)
                        for line in logs
                        if (match := re.search(
                            r"Stravia startup listener opened .*?(127\.0\.0\.1:\d+)",
                            line,
                        ))
                    ),
                    None,
                )
                assert address is not None, "server did not report its bound listener address"
                server_port = int(address.rsplit(":", 1)[1])
                admin_base = f"http://127.0.0.1:{server_port}"
                proxy_base = admin_base
                wait_until_ready(f"{admin_base}/api/v1/auth/state")
                session = initialize_server(
                    admin_base,
                    setup_token,
                    {"backend": "sqlite"},
                )
                yield {
                    "admin": admin_base,
                    "proxy": proxy_base,
                    "proxy_cors_origin": proxy_cors_origin,
                    "mock": f"http://127.0.0.1:{mock_port}",
                    "mock_server": mock_server,
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
