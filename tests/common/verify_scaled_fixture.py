"""校验放大后的升级前库：任一版本的服务端都能经管理 API 读出抽样交互。

用旧（baseline）二进制证明生成数据是旧服务端可读的真实形态（事件载荷、Run、Turn 链解码均通过
旧读取路径），用新二进制可对升级后的同一批交互做同样的读取。SQLite 专用；在副本上操作。
"""

from __future__ import annotations

import argparse
import json
import random
import shutil
import sqlite3
import sys
from contextlib import closing
from pathlib import Path

from tests.common.helpers import (
    WebSession, find_free_port, start_stravia_server, stop_stravia_server,
    wait_for_setup_token, wait_until_ready,
)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--source", type=Path, required=True, help="scale_upgrade_fixture 生成的 data-dir")
    parser.add_argument("--work", type=Path, required=True, help="副本目录，必须不存在")
    parser.add_argument("--samples", type=int, default=40)
    parser.add_argument("--seed", type=int, default=1)
    args = parser.parse_args()
    shutil.copytree(args.source, args.work)
    with closing(sqlite3.connect(args.work / "db" / "gateway.db")) as connection:
        expected = dict(connection.execute(
            "SELECT interaction_id, COUNT(*) FROM inference_run_observations GROUP BY interaction_id").fetchall())
    ids = sorted(expected)
    sample = random.Random(args.seed).sample(ids, min(args.samples, len(ids)))
    port = find_free_port()
    base = f"http://127.0.0.1:{port}"
    process, logs = start_stravia_server(
        stravia_binary=args.binary,
        args=["--data-dir", str(args.work), "--host", "127.0.0.1", "--port", str(port)])
    failures: list[str] = []
    try:
        wait_until_ready(f"{base}/api/v1/auth/state")
        session = WebSession(base)
        status, body = session.request("POST", "/api/v1/setup/claim", {"token": wait_for_setup_token(logs, process)})
        assert status == 204, (status, body)
        status, body = session.request(
            "POST", "/api/v1/setup/complete",
            {"database": {"backend": "sqlite"}, "username": "admin", "password": "correct horse battery staple", "client_base_url": base}, timeout=3600)
        assert status == 200, (status, body)
        status, body = session.request("POST", "/api/v1/auth/login", {"username": "admin", "password": "correct horse battery staple"})
        assert status == 200, (status, body)
        headers = session.auth_headers()
        for interaction in sample:
            status, body = session.request("GET", f"/api/v1/observations/interactions/{interaction}", headers=headers)
            if status != 200:
                failures.append(f"{interaction}: HTTP {status} {str(body)[:120]}")
                continue
            runs = len(body["data"]["runs"])
            if runs != expected[interaction]:
                failures.append(f"{interaction}: runs {runs} != {expected[interaction]}")
    finally:
        stop_stravia_server(process, logs, print_tail=0)
        shutil.rmtree(args.work)
    print(json.dumps({"binary": str(args.binary), "sampled": len(sample), "failures": failures}, indent=2))
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
