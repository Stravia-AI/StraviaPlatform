"""Load the recorded ADR-0089 upgrade fixture into a migration-36 database.

The fixture (``tests/e2e/fixtures/shared_content_upgrade/fixture.json``) is
recorded from the pre-ADR-0089 server by
``tests.common.record_shared_content_upgrade_fixture``. Rows keep the old
schema's column names; epoch-millisecond columns are shifted at load time so
the newest recorded event lands on the supplied ``now``. That keeps every
retention window (absolute pending deadlines, renewed dependencies, the
non-default ``log_retention_days``) at the same distance from the clock
regardless of when the test runs.
"""

from __future__ import annotations

import json
import sqlite3
import threading
from contextlib import closing, contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Iterator

FIXTURE_PATH = Path(__file__).resolve().parents[1] / "e2e" / "fixtures" / "shared_content_upgrade" / "fixture.json"
SCHEMA_VERSION = 36
# SQLite 用单行表分配 Observation 序号，迁移已写入初值：加载前先清空，再写入录制值。
# PostgreSQL 使用序列 ``observation_event_sequence``，该表不存在。
SQLITE_SINGLETON_TABLES = ("observation_sequence",)
SETTINGS_KEYS = ("log_retention_days", "reversible_redaction_enabled")
COMPACTION_REMOVED_HISTORY = "compaction fixture removed history"
# 原生压缩的 Target 绑定指纹包含 Provider 的 base_url：录制与回放必须使用同一个地址，
# 因此该 mock 固定在非临时端口范围内的端口，而不是随机端口。
COMPACTION_UPSTREAM_PORT = 28431
COMPACTION_STATE = {"type": "compaction", "id": "cmp_state_fixture", "encrypted_content": "opaque-fixture-state"}


@contextmanager
def native_responses_upstream() -> Iterator[tuple[str, list[dict[str, Any]]]]:
    """最小 open-responses 上游（固定端口）：``/responses/compact`` 返回固定窗口，其余请求回显一段文本。"""
    received: list[dict[str, Any]] = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args: Any) -> None:
            return

        def do_POST(self) -> None:  # noqa: N802
            body = json.loads(self.rfile.read(int(self.headers["content-length"])))
            received.append({"path": self.path, "body": body})
            if self.path.endswith("/responses/compact"):
                result = {"id": "cmp_operation_fixture", "object": "response.compaction", "created_at": 1788825600,
                          "output": [COMPACTION_STATE]}
            else:
                replaying = any(isinstance(item, dict) and item.get("type") == "compaction"
                                for item in body.get("input", []))
                result = {
                    "id": "resp_fixture", "object": "response", "created_at": 1788825600, "status": "completed",
                    "model": "native-model",
                    "output": [{"type": "message", "id": "msg_fixture", "status": "completed", "role": "assistant",
                                "content": [{"type": "output_text", "annotations": [],
                                             "text": "continued from compacted window" if replaying else "source answer"}]}],
                    "usage": {"input_tokens": 10, "output_tokens": 4, "total_tokens": 14,
                              "input_tokens_details": {"cached_tokens": 0},
                              "output_tokens_details": {"reasoning_tokens": 0}},
                    "completed_at": 1788825601, "incomplete_details": None, "previous_response_id": None,
                    "instructions": None, "error": None, "tools": [], "tool_choice": "auto",
                    "truncation": "disabled", "parallel_tool_calls": True, "text": {"format": {"type": "text"}},
                    "top_p": None, "presence_penalty": None, "frequency_penalty": None, "top_logprobs": None,
                    "temperature": None, "reasoning": None, "max_output_tokens": None, "max_tool_calls": None,
                    "store": False, "background": False, "service_tier": "default", "metadata": {},
                    "safety_identifier": None, "prompt_cache_key": None,
                }
            data = json.dumps(result).encode()
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

    server = ThreadingHTTPServer(("127.0.0.1", COMPACTION_UPSTREAM_PORT), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}", received
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


def load_fixture(path: Path = FIXTURE_PATH) -> dict[str, Any]:
    fixture = json.loads(path.read_text(encoding="utf-8"))
    assert fixture["schema_version"] == SCHEMA_VERSION, fixture["schema_version"]
    return fixture


def _decode(value: Any) -> Any:
    if isinstance(value, dict):
        return bytes.fromhex(value["$b"])
    return value


def shifted_rows(fixture: dict[str, Any], now_ms: int) -> Iterator[tuple[str, list[str], list[list[Any]]]]:
    delta = now_ms - fixture["recorded_at"]
    for table in fixture["order"]:
        spec = fixture["tables"][table]
        columns = spec["columns"]
        time_indexes = {columns.index(name) for name in fixture["time_columns"].get(table, [])}
        rows = []
        for row in spec["rows"]:
            rows.append([
                (value + delta) if index in time_indexes and isinstance(value, int) else _decode(value)
                for index, value in enumerate(row)
            ])
        yield table, columns, rows


def insert_sqlite(path: Path, fixture: dict[str, Any], now_ms: int) -> None:
    with closing(sqlite3.connect(path, timeout=30)) as connection, connection:
        for table, columns, rows in shifted_rows(fixture, now_ms):
            if table in SQLITE_SINGLETON_TABLES:
                connection.execute(f"DELETE FROM {table}")
            names = ",".join(f'"{name}"' for name in columns)
            marks = ",".join("?" for _ in columns)
            connection.executemany(f"INSERT INTO {table}({names}) VALUES ({marks})", rows)
        for name in SETTINGS_KEYS:
            connection.execute(
                "INSERT INTO settings(name, value) VALUES (?, ?) "
                "ON CONFLICT(name) DO UPDATE SET value = excluded.value",
                (name, fixture["settings"][name]),
            )


def insert_postgres(dsn: str, fixture: dict[str, Any], now_ms: int) -> None:
    import psycopg

    with psycopg.connect(dsn) as connection:
        types: dict[tuple[str, str], str] = {}
        for table, column, data_type, udt in connection.execute(
            "SELECT table_name, column_name, data_type, udt_name FROM information_schema.columns "
            "WHERE table_schema = current_schema()"
        ):
            types[(table, column)] = "bool" if data_type == "boolean" else udt
        for table, columns, rows in shifted_rows(fixture, now_ms):
            if table in SQLITE_SINGLETON_TABLES:
                continue
            kinds = [types[(table, name)] for name in columns]
            names = ",".join(f'"{name}"' for name in columns)
            marks = ",".join("%s" if kind in ("text", "varchar", "bpchar") else f"%s::{kind}" for kind in kinds)
            converted = [
                [bool(value) if kind == "bool" and value is not None else value for value, kind in zip(row, kinds)]
                for row in rows
            ]
            with connection.cursor() as cursor:
                cursor.executemany(f"INSERT INTO {table}({names}) VALUES ({marks})", converted)
        for name in SETTINGS_KEYS:
            connection.execute(
                "INSERT INTO settings(name, value) VALUES (%s, %s) "
                "ON CONFLICT(name) DO UPDATE SET value = excluded.value",
                (name, fixture["settings"][name]),
            )
        # 显式写入序号不会推进序列：新服务端后续事件必须排在录制事件之后。
        connection.execute("SELECT setval('observation_event_sequence', (SELECT MAX(sequence) FROM observation_events))")
        # 同理：显式写入的自增主键（如 turn_scopes.id）也要把序列推到最大值之后，
        # 否则服务端升级时为新主体创建作用域会撞上已有主键。
        serials = connection.execute(
            "SELECT table_name, column_name FROM information_schema.columns "
            "WHERE table_schema = current_schema() "
            "AND pg_get_serial_sequence(quote_ident(table_name), column_name) IS NOT NULL"
        ).fetchall()
        for table, column in serials:
            connection.execute(
                f"SELECT setval(pg_get_serial_sequence(%s, %s), "
                f"COALESCE((SELECT MAX({column}) FROM {table}), 0) + 1, false)",
                (table, column),
            )
