"""核对 ADR-0089 内容共享：每个 principal 一个 content_scope，同一正文不因 Turn / Marker / Observation 分别入库而重复存储。

只读；SQLite 传 `--data-dir`，PostgreSQL 传 `--database-url`。输出每条查询的 SQL 与结果，供写入 ADR。
"""

from __future__ import annotations

import argparse
import json
from argparse import Namespace
from pathlib import Path

from tests.common.protocol_history_audit import _connect

QUERIES = {
    "scopes": "SELECT id, principal FROM content_scopes ORDER BY id",
    "scopes_per_principal_max": "SELECT COALESCE(MAX(n), 0) FROM (SELECT COUNT(*) AS n FROM content_scopes GROUP BY principal) t",
    "contents_total": "SELECT COUNT(*) FROM contents",
    # 同一 API Key 若以两种 principal 形态（裸 id 与 `api-key:` 前缀）各有一个 scope，同一正文会被存两份（应为 0）。
    "api_keys_with_more_than_one_scope": (
        "SELECT COUNT(*) FROM (SELECT CASE WHEN principal LIKE 'api-key:%' THEN substr(principal, 9) ELSE principal END AS k "
        "FROM content_scopes GROUP BY k HAVING COUNT(*) > 1) t"),
    "bodies_in_more_than_one_scope_of_the_same_api_key": (
        "SELECT COUNT(*) FROM (SELECT CASE WHEN s.principal LIKE 'api-key:%' THEN substr(s.principal, 9) ELSE s.principal END AS k, c.digest "
        "FROM contents c JOIN content_scopes s ON s.id = c.scope_id GROUP BY k, c.digest HAVING COUNT(*) > 1) t"),
    # 同一 principal 内 (scope_id, digest) 由唯一约束保证；这里查跨 scope 的重复正文（应为 0）。
    "bodies_in_more_than_one_scope_across_api_keys": (
        "SELECT COUNT(*) FROM (SELECT digest FROM contents GROUP BY digest HAVING COUNT(DISTINCT scope_id) > 1) t"),
    "contents_by_referencing_party": (
        "SELECT (CASE WHEN t.c IS NULL THEN 0 ELSE 1 END) AS turn, (CASE WHEN m.c IS NULL THEN 0 ELSE 1 END) AS marker, "
        "(CASE WHEN o.c IS NULL THEN 0 ELSE 1 END) AS observation, COUNT(*) "
        "FROM contents c "
        "LEFT JOIN (SELECT DISTINCT content_id AS c FROM turn_node_contents) t ON t.c = c.id "
        "LEFT JOIN (SELECT DISTINCT content_id AS c FROM history_marker_contents) m ON m.c = c.id "
        "LEFT JOIN (SELECT content_id AS c FROM observation_event_contents UNION SELECT content_id FROM observation_tool_results) o ON o.c = c.id "
        "GROUP BY 1, 2, 3 ORDER BY 1, 2, 3"),
    "reference_rows": (
        "SELECT (SELECT COUNT(*) FROM turn_node_contents), (SELECT COUNT(*) FROM history_marker_contents), "
        "(SELECT COUNT(*) FROM observation_event_contents), (SELECT COUNT(*) FROM observation_tool_results)"),
    "content_bytes": "SELECT COALESCE(SUM(length(body)), 0), COALESCE(SUM(raw_len), 0) FROM contents",
}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--data-dir", type=Path)
    parser.add_argument("--database-url")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    namespace = Namespace(backend="postgres" if args.database_url else "sqlite", database_url=args.database_url)
    connection, _, backend = _connect(namespace, args.data_dir)
    report = {"backend": backend, "queries": {}}
    try:
        for name, sql in QUERIES.items():
            rows = [list(row) for row in connection.execute(sql).fetchall()]
            report["queries"][name] = {"sql": sql, "rows": rows}
    finally:
        connection.close()
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2, default=int), encoding="utf-8")
    for name, entry in report["queries"].items():
        print(name, entry["rows"])


if __name__ == "__main__":
    main()
