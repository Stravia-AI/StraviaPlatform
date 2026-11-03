"""Read-only storage inventory of a finished benchmark database (SQLite or PostgreSQL).

Row counts, encoded payload bytes and on-disk bytes are taken from one
snapshot-consistent read transaction (`protocol_history_audit._connect`). The
baseline layout (`turn_contents` + per-event payload) and the ADR-0089 layout
(`contents`, `observation_blocks`, reference tables) are both described by the
same table/column probe; only tables and columns that exist are reported, so a
missing ADR-0089 table is never counted as zero.
"""
from __future__ import annotations

from tests.common.protocol_history_audit import (
    _connect, _history_tables, _table_columns, history_storage_schema,
)

# (table, column) pairs whose encoded length is reported when both exist.
PAYLOAD_COLUMNS = (
    ("turn_node_payloads", "payload"),
    ("turn_contents", "content"),
    ("contents", "body"),
    ("history_markers", "segment_payload"),
    ("observation_events", "payload"),
    ("observation_events", "attrs"),
    ("observation_blocks", "envelope"),
)


def _reported(table: str) -> bool:
    return (table.startswith(("turn_", "content", "history_marker", "native_compaction"))
            or "observation" in table or table in {"reversible_redaction_mappings"})


def read_storage_inventory(args, directory) -> dict:
    connection, placeholder, backend = _connect(args, directory)
    try:
        tables = _history_tables(connection, placeholder)
        schema = history_storage_schema(tables)
        reported = sorted(table for table in tables if _reported(table))
        counts = {table: connection.execute(f'SELECT COUNT(*) FROM "{table}"').fetchone()[0] for table in reported}
        encoded = {}
        for table, column in PAYLOAD_COLUMNS:
            if table in tables and column in _table_columns(connection, placeholder, table):
                encoded[f"{table}.{column}"] = connection.execute(
                    f'SELECT COALESCE(SUM(length("{column}")), 0) FROM "{table}"').fetchone()[0]
        table_bytes, disk_error = {}, None
        if backend == "sqlite":
            try:
                owner = {name: table for name, table in connection.execute(
                    "SELECT name, tbl_name FROM sqlite_master WHERE type IN ('table','index')")}
                for name, size in connection.execute("SELECT name, SUM(pgsize) FROM dbstat GROUP BY name"):
                    table = owner.get(name)
                    if table in counts:
                        table_bytes[table] = table_bytes.get(table, 0) + size
            except Exception as error:  # dbstat is an optional SQLite build feature
                disk_error = f"{type(error).__name__}: {error}"
        else:
            for table in reported:
                table_bytes[table] = connection.execute(
                    "SELECT pg_table_size(%s::regclass) + pg_indexes_size(%s::regclass)", (table, table)).fetchone()[0]
        return {"backend": backend, "history_schema": schema, "row_counts": counts,
                "encoded_payload_bytes": encoded, "table_bytes_with_indexes": table_bytes,
                "table_bytes_error": disk_error}
    finally:
        connection.close()
