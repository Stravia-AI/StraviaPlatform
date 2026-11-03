"""把录制的 ADR-0089 升级 fixture 按交互（interaction）放大成接近开发库规模的升级前数据库。

fixture（`tests/e2e/fixtures/shared_content_upgrade/fixture.json`）由 ADR-0089 之前的服务端
录制。本脚本不发明新形态：它把 fixture 里的每个 interaction 视为一个“单元”——该交互的
Observation 行、事件、Run 引用的 Turn 节点链及其内容、与其载荷里出现的 Marker /
凭据映射 / 原生压缩记录——然后整单元克隆，克隆内全部 28 位随机 ID 重新生成、序号与主键
重新分配、旧存储编码（zstd + 尾部）的载荷解码后改写再编码，Turn 内容重算 digest。配置表
（api_keys / providers / models …）不克隆，克隆仍挂在原有 principal 下。

单元混合比由 `--target-events/--target-contents/--target-markers` 经非负最小二乘（相对
误差）求解并取整；达成的实际规模（含与目标的偏差）写入输出 JSON，从不假称命中目标。

`--storage-harness` 是 `stravia-storage-test-harness` 二进制，用它安装迁移 ≤36 的空库，
与 `tests/e2e/storage/test_shared_content_upgrade.py` 的做法相同。
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import random
import re
import sqlite3
import subprocess
import sys
import time
from contextlib import closing
from pathlib import Path
from typing import Any

import zstandard

from tests.common.shared_content_upgrade_fixture import (
    SETTINGS_KEYS, SQLITE_SINGLETON_TABLES, insert_postgres, insert_sqlite, load_fixture,
)

REPO_ROOT = Path(__file__).resolve().parents[2]
# 旧版（ADR-0076）Turn 内容 digest 域：sha256(域 || 解码后的原始字节)。
LEGACY_DIGEST_DOMAIN = b"stravia.turn-content.v1\0"
TOKEN = re.compile(rb"(?<![A-Za-z0-9])[a-z]{28}(?![A-Za-z0-9])")
CONFIG_TABLES = ("api_keys", "providers", "provider_models", "models", "model_backends", "api_key_models", "turn_scopes")
SEQUENCE_COLUMNS = {
    "interaction_observations": ("last_event_sequence",),
    "inference_run_observations": ("last_event_sequence",),
    "model_turn_observations": ("last_event_sequence",),
    "target_attempt_observations": ("last_event_sequence",),
}
CODEC_COLUMNS = {("turn_node_payloads", "payload"), ("observation_events", "payload"), ("turn_contents", "content")}
TRAILER, VERSION, RAW, ZSTD, MIN_COMPRESS = 14, 1, 0, 1, 128
_COMPRESSOR = zstandard.ZstdCompressor(level=1, write_checksum=True, write_content_size=True)
_DECOMPRESSOR = zstandard.ZstdDecompressor()


def codec_decode(blob: bytes) -> bytes:
    body, trailer = blob[:-TRAILER], blob[-TRAILER:]
    assert trailer[13] == VERSION and trailer[1:5] == b"\0\0\0\0", "unsupported storage codec trailer"
    expected = int.from_bytes(trailer[5:13], "little")
    raw = body if trailer[0] == RAW else _DECOMPRESSOR.decompress(body, max_output_size=expected)
    assert trailer[0] in (RAW, ZSTD) and len(raw) == expected, "storage codec length mismatch"
    return raw


def codec_encode(raw: bytes) -> bytes:
    """与服务端 `storage_codec::encode` 同构：>=128 字节才压缩且仅在更小时采用。"""
    codec, body = RAW, raw
    if len(raw) >= MIN_COMPRESS:
        compressed = _COMPRESSOR.compress(raw)
        if len(compressed) < len(raw):
            codec, body = ZSTD, compressed
    return body + bytes([codec]) + b"\0\0\0\0" + len(raw).to_bytes(8, "little") + bytes([VERSION])


def legacy_digest(raw: bytes) -> bytes:
    return hashlib.sha256(LEGACY_DIGEST_DOMAIN + raw).digest()


def fresh_token(clone: int, token: bytes) -> bytes:
    """确定性、按克隆区分的 28 位小写字母 ID。"""
    value = int.from_bytes(hashlib.sha256(b"%d:" % clone + token).digest(), "big")
    letters = []
    for _ in range(28):
        value, digit = divmod(value, 26)
        letters.append(97 + digit)
    return bytes(letters)


class NoStringLeaf(Exception):
    """内容里没有字符串叶子可以加盐。"""


def salt_json(raw: bytes, clone: int, attempt: int) -> bytes:
    """内容在同一 principal 内必须唯一：给最长字符串叶子追加克隆标记，结构保持不变。"""
    document = json.loads(raw)
    best: list[Any] = []

    def walk(node: Any, holder: Any, key: Any) -> None:
        if isinstance(node, str):
            if not best or len(node) > len(best[0][0]):
                best[:] = [(node, holder, key)]
        elif isinstance(node, dict):
            for name, child in node.items():
                walk(child, node, name)
        elif isinstance(node, list):
            for index, child in enumerate(node):
                walk(child, node, index)

    walk(document, None, None)
    if not best:
        raise NoStringLeaf
    text, holder, key = best[0]
    suffix = f" [clone {clone}.{attempt}]"
    if holder is None:
        document = text + suffix
    else:
        holder[key] = text + suffix
    return json.dumps(document, separators=(",", ":"), ensure_ascii=False).encode()


class Fixture:
    """解码后的 fixture：每表 list[dict]，blob 为 bytes。"""

    def __init__(self, data: dict[str, Any]) -> None:
        self.data = data
        self.order: list[str] = data["order"]
        self.columns = {table: spec["columns"] for table, spec in data["tables"].items()}
        self.rows: dict[str, list[dict[str, Any]]] = {}
        for table, spec in data["tables"].items():
            self.rows[table] = [
                {name: (bytes.fromhex(value["$b"]) if isinstance(value, dict) else value)
                 for name, value in zip(spec["columns"], row)}
                for row in spec["rows"]
            ]
        self.keep = {m for table in CONFIG_TABLES for row in self.rows[table] for value in row.values()
                     for m in TOKEN.findall(value if isinstance(value, bytes) else str(value).encode())}


class Unit:
    """一个 interaction 连通分量（经 parent_interaction_id / parent_run_id 相连）的完整闭包，及其规模三元组。"""

    def __init__(self, fixture: Fixture, interactions: list[dict[str, Any]]) -> None:
        rows = fixture.rows
        self.interaction_id = "+".join(i["id"] for i in interactions)
        member = {i["id"] for i in interactions}
        pick = lambda table, column="interaction_id": [r for r in rows[table] if r[column] in member]  # noqa: E731
        self.rows: dict[str, list[dict[str, Any]]] = {
            "interaction_observations": interactions,
            "inference_run_observations": pick("inference_run_observations"),
            "model_turn_observations": pick("model_turn_observations"),
            "target_attempt_observations": pick("target_attempt_observations"),
            "observation_events": pick("observation_events"),
            "observation_pending_tools": pick("observation_pending_tools"),
            "observation_tail_sources": pick("observation_tail_sources"),
            "credential_mapping_reuses": pick("credential_mapping_reuses"),
        }
        by_id = {n["id"]: n for n in rows["turn_nodes"]}
        by_pk = {n["node_pk"]: n for n in rows["turn_nodes"]}
        pks: set[int] = set()
        for run in self.rows["inference_run_observations"]:
            node = by_id.get(run["generation_node_id"])
            while node is not None and node["node_pk"] not in pks:
                pks.add(node["node_pk"])
                node = by_pk.get(node["parent_pk"])
        self.rows["turn_nodes"] = sorted((by_pk[pk] for pk in pks), key=lambda n: n["node_pk"])
        self.rows["turn_node_payloads"] = [r for r in rows["turn_node_payloads"] if r["node_pk"] in pks]
        self.rows["turn_node_contents"] = [r for r in rows["turn_node_contents"] if r["node_pk"] in pks]
        self.rows["turn_node_retention"] = [r for r in rows["turn_node_retention"] if r["node_pk"] in pks]
        content_ids = {r["content_id"] for r in self.rows["turn_node_contents"]}
        self.rows["turn_contents"] = [r for r in rows["turn_contents"] if r["id"] in content_ids]
        blobs = [codec_decode(r["payload"]) for r in self.rows["turn_node_payloads"]]
        blobs += [codec_decode(r["content"]) for r in self.rows["turn_contents"]]
        blobs += [codec_decode(r["payload"]) for r in self.rows["observation_events"]]
        blobs += [r["marker"].encode() for r in self.rows["credential_mapping_reuses"]]
        tokens = {t for blob in blobs for t in TOKEN.findall(blob)}
        self.rows["history_markers"] = [r for r in rows["history_markers"] if r["reference"].encode() in tokens]
        self.rows["reversible_redaction_mappings"] = [
            r for r in rows["reversible_redaction_mappings"] if r["reference"].encode() in tokens]
        self.rows["native_compactions"] = [r for r in rows["native_compactions"] if r["id"].encode() in tokens]
        ids = {r["id"] for r in self.rows["native_compactions"]}
        self.rows["native_compaction_states"] = [r for r in rows["native_compaction_states"] if r["record_id"] in ids]
        self.rows["native_compaction_sources"] = [r for r in rows["native_compaction_sources"] if r["record_id"] in ids]
        self.stats = {"events": len(self.rows["observation_events"]),
                      "contents": len(self.rows["turn_contents"]),
                      "markers": len(self.rows["history_markers"])}


def interaction_components(fixture: Fixture) -> list[list[dict[str, Any]]]:
    """FK 把交互相连（parent_interaction_id、跨交互的 parent_run_id）：同一连通分量必须整体克隆。"""
    parent = {r["id"]: r["id"] for r in fixture.rows["interaction_observations"]}

    def find(x: str) -> str:
        while parent[x] != x:
            parent[x] = parent[parent[x]]
            x = parent[x]
        return x

    run_owner = {r["id"]: r["interaction_id"] for r in fixture.rows["inference_run_observations"]}
    for row in fixture.rows["interaction_observations"]:
        if row["parent_interaction_id"] is not None:
            parent[find(row["id"])] = find(row["parent_interaction_id"])
    for run in fixture.rows["inference_run_observations"]:
        if run["parent_run_id"] is not None:
            parent[find(run["interaction_id"])] = find(run_owner[run["parent_run_id"]])
    groups: dict[str, list[dict[str, Any]]] = {}
    for row in fixture.rows["interaction_observations"]:
        groups.setdefault(find(row["id"]), []).append(row)
    return list(groups.values())


def solve_mix(units: list[Unit], targets: dict[str, int], base: dict[str, int]) -> list[int]:
    """额外克隆份数 x_i≥0：最小化三项指标相对误差平方和（坐标下降，再取整）。"""
    keys = ("events", "contents", "markers")
    need = {k: max(targets[k] - base[k], 0) for k in keys}
    x = [0.0] * len(units)

    def loss(vec: list[float]) -> float:
        return sum(((sum(v * u.stats[k] for v, u in zip(vec, units)) - need[k]) / max(targets[k], 1)) ** 2 for k in keys)

    step = max(need["events"] / max(min(u.stats["events"] for u in units), 1), 1.0)
    while step > 1e-3:
        improved = False
        for i in range(len(units)):
            for delta in (step, -step):
                if x[i] + delta < 0:
                    continue
                trial = x.copy()
                trial[i] += delta
                if loss(trial) < loss(x) - 1e-15:
                    x, improved = trial, True
        if not improved:
            step /= 2
    return [round(v) for v in x]


class Cloner:
    def __init__(self, fixture: Fixture, now_ms: int) -> None:
        self.fixture = fixture
        self.delta = now_ms - fixture.data["recorded_at"]
        self.time_columns = {t: set(c) for t, c in fixture.data["time_columns"].items()}
        self.next_pk = max(r["node_pk"] for r in fixture.rows["turn_nodes"]) + 1
        self.next_content = max(r["id"] for r in fixture.rows["turn_contents"]) + 1
        self.next_sequence = max(r["sequence"] for r in fixture.rows["observation_events"]) + 1
        self.used = {(r["scope_id"], r["digest"]): r["id"] for r in fixture.rows["turn_contents"]}
        self.salted = 0

    def text(self, clone: int, value: str) -> str:
        return TOKEN.sub(lambda m: m.group(0) if m.group(0) in self.fixture.keep else fresh_token(clone, m.group(0)), value.encode()).decode()

    def blob(self, clone: int, raw: bytes) -> bytes:
        return TOKEN.sub(lambda m: m.group(0) if m.group(0) in self.fixture.keep else fresh_token(clone, m.group(0)), raw)

    def content_row(self, clone: int, row: dict[str, Any], content_map: dict[int, int]) -> dict[str, Any] | None:
        """克隆一条 Turn 内容；同 principal 内必须唯一，无字符串叶子可加盐的内容（如 `[]`）复用已有行。"""
        raw = self.blob(clone, codec_decode(row["content"]))
        attempt = 0
        while (row["scope_id"], legacy_digest(raw)) in self.used:
            try:
                salted = salt_json(raw, clone, attempt := attempt + 1)
            except NoStringLeaf:
                content_map[row["id"]] = self.used[(row["scope_id"], legacy_digest(raw))]
                return None
            self.salted += 1
            raw = salted
        content_map[row["id"]] = self.next_content
        self.used[(row["scope_id"], legacy_digest(raw))] = self.next_content
        self.next_content += 1
        return {"id": content_map[row["id"]], "scope_id": row["scope_id"], "digest": legacy_digest(raw),
                "content": codec_encode(raw)}

    def clone(self, unit: Unit, clone: int) -> dict[str, list[list[Any]]]:
        pk_map = {}
        for node in unit.rows["turn_nodes"]:
            pk_map[node["node_pk"]] = self.next_pk
            self.next_pk += 1
        content_map = {}
        seq_map = {}
        for event in unit.rows["observation_events"]:
            seq_map[event["sequence"]] = self.next_sequence
            self.next_sequence += 1
        out: dict[str, list[list[Any]]] = {}
        for table in self.fixture.order:
            source = unit.rows.get(table)
            if not source:
                continue
            columns = self.fixture.columns[table]
            timed = self.time_columns.get(table, set())
            rows = []
            for row in source:
                if table == "turn_contents":
                    emitted = self.content_row(clone, row, content_map)
                    if emitted is not None:
                        rows.append([emitted[name] for name in columns])
                    continue
                values = []
                for name in columns:
                    value = row[name]
                    if (table, name) in CODEC_COLUMNS:
                        value = codec_encode(self.blob(clone, codec_decode(value)))
                    elif table == "observation_events" and name == "sequence":
                        value = seq_map[value]
                    elif name in SEQUENCE_COLUMNS.get(table, ()):
                        value = seq_map[value] if value in seq_map else value
                    elif name in ("node_pk", "parent_pk") and table in (
                            "turn_nodes", "turn_node_payloads", "turn_node_contents", "turn_node_retention"):
                        value = pk_map[value] if value is not None else None
                    elif table == "turn_node_contents" and name == "content_id":
                        value = content_map[value]
                    elif name in timed and isinstance(value, int):
                        value += self.delta
                    elif isinstance(value, str):
                        value = self.text(clone, value)
                    elif isinstance(value, bytes):
                        raise AssertionError(f"unhandled blob column {table}.{name}")
                    values.append(value)
                rows.append(values)
            out[table] = rows
        return out


# --- 装载 ---------------------------------------------------------------------------------

def create_schema(args: argparse.Namespace, sqlite_path: Path | None) -> None:
    env = {**os.environ, "STRAVIA_STORAGE_USAGE_UPGRADE_FIXTURE": "shared-content-history",
           "STRAVIA_STORAGE_BACKEND": args.backend}
    if args.backend == "postgres":
        env["STRAVIA_STORAGE_PG_URL"] = args.database_url
    else:
        env["STRAVIA_STORAGE_SQLITE_PATH"] = str(sqlite_path)
    done = subprocess.run([str(args.storage_harness)], env=env, cwd=REPO_ROOT, text=True,
                          capture_output=True, check=False, timeout=300)
    if done.returncode != 0 or "usage_upgrade_fixture=shared-content-history" not in done.stdout:
        raise RuntimeError(f"schema install failed: {done.stdout}\n{done.stderr}")


class SqliteSink:
    def __init__(self, path: Path) -> None:
        self.connection = sqlite3.connect(path, timeout=60)
        self.connection.execute("PRAGMA synchronous=OFF")

    def write(self, columns: dict[str, list[str]], batch: dict[str, list[list[Any]]], order: list[str]) -> None:
        with self.connection:
            for table in order:
                rows = batch.get(table)
                if rows:
                    names = ",".join(f'"{n}"' for n in columns[table])
                    marks = ",".join("?" for _ in columns[table])
                    self.connection.executemany(f"INSERT INTO {table}({names}) VALUES ({marks})", rows)

    def finish(self) -> None:
        with self.connection:
            top = self.connection.execute("SELECT MAX(sequence) FROM observation_events").fetchone()[0]
            self.connection.execute("UPDATE observation_sequence SET next_sequence=?", (top + 1,))
        self.connection.execute("PRAGMA wal_checkpoint(TRUNCATE)")
        self.connection.close()


class PostgresSink:
    def __init__(self, url: str) -> None:
        import psycopg
        self.connection = psycopg.connect(url)
        self.types = {}
        for table, column, data_type, udt in self.connection.execute(
                "SELECT table_name, column_name, data_type, udt_name FROM information_schema.columns "
                "WHERE table_schema = current_schema()"):
            self.types[(table, column)] = "bool" if data_type == "boolean" else udt

    def write(self, columns: dict[str, list[str]], batch: dict[str, list[list[Any]]], order: list[str]) -> None:
        with self.connection.cursor() as cursor:
            for table in order:
                rows = batch.get(table)
                if not rows:
                    continue
                kinds = [self.types[(table, n)] for n in columns[table]]
                names = ",".join(f'"{n}"' for n in columns[table])
                marks = ",".join("%s" if k in ("text", "varchar", "bpchar") else f"%s::{k}" for k in kinds)
                converted = [[bool(v) if k == "bool" and v is not None else v for v, k in zip(r, kinds)] for r in rows]
                cursor.executemany(f"INSERT INTO {table}({names}) VALUES ({marks})", converted)
        self.connection.commit()

    def finish(self) -> None:
        self.connection.execute("SELECT setval('observation_event_sequence', (SELECT MAX(sequence) FROM observation_events))")
        for table, column in self.connection.execute(
                "SELECT table_name, column_name FROM information_schema.columns WHERE table_schema = current_schema() "
                "AND pg_get_serial_sequence(quote_ident(table_name), column_name) IS NOT NULL").fetchall():
            self.connection.execute(
                "SELECT setval(pg_get_serial_sequence(%s, %s), COALESCE((SELECT MAX(" + column + ") FROM " + table + "), 0) + 1, false)",
                (table, column))
        self.connection.commit()
        self.connection.execute("ANALYZE")
        self.connection.commit()
        self.connection.close()


def inventory(args: argparse.Namespace, sqlite_path: Path | None) -> dict[str, int]:
    tables = ("observation_events", "turn_contents", "history_markers", "turn_nodes", "turn_node_contents",
              "interaction_observations", "inference_run_observations", "reversible_redaction_mappings", "native_compactions")
    if args.backend == "postgres":
        import psycopg
        with psycopg.connect(args.database_url) as connection:
            return {t: connection.execute(f"SELECT COUNT(*) FROM {t}").fetchone()[0] for t in tables}
    with closing(sqlite3.connect(sqlite_path)) as connection:
        return {t: connection.execute(f"SELECT COUNT(*) FROM {t}").fetchone()[0] for t in tables}


def verify_sample(args: argparse.Namespace, sqlite_path: Path | None, samples: int) -> dict[str, int]:
    """抽样重读：turn_contents 的 digest 与编码长度、事件载荷为合法 JSON。"""
    rng = random.Random(args.seed)
    if args.backend == "postgres":
        import psycopg
        connection = psycopg.connect(args.database_url)
        ph = "%s"
    else:
        connection = sqlite3.connect(sqlite_path)
        ph = "?"
    with closing(connection):
        top = connection.execute("SELECT MAX(id) FROM turn_contents").fetchone()[0]
        checked = 0
        for content_id in rng.sample(range(1, top + 1), min(samples, top)):
            row = connection.execute(f"SELECT digest, content FROM turn_contents WHERE id={ph}", (content_id,)).fetchone()
            if row is None:
                continue
            raw = codec_decode(bytes(row[1]))
            assert bytes(row[0]) == legacy_digest(raw), f"turn_contents {content_id} digest mismatch"
            json.loads(raw)
            checked += 1
        events = 0
        for (payload,) in connection.execute(f"SELECT payload FROM observation_events ORDER BY sequence LIMIT {samples}").fetchall():
            json.loads(codec_decode(bytes(payload)))
            events += 1
    return {"contents_verified": checked, "events_decoded": events}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--backend", choices=("sqlite", "postgres"), required=True)
    parser.add_argument("--data-dir", type=Path, help="SQLite：服务端 data-dir（必须尚不存在）；库写在 <dir>/db/gateway.db")
    parser.add_argument("--database-url", help="PostgreSQL：全新空库的 loopback URL（运行者自行创建/销毁）")
    parser.add_argument("--storage-harness", type=Path, required=True)
    parser.add_argument("--target-events", type=int, default=490_000)
    parser.add_argument("--target-contents", type=int, default=178_000)
    parser.add_argument("--target-markers", type=int, default=17_000)
    parser.add_argument("--batch-clones", type=int, default=100)
    parser.add_argument("--seed", type=int, default=1)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    sqlite_path = None
    if args.backend == "sqlite":
        if not args.data_dir or args.data_dir.exists():
            parser.error("--data-dir must be given and must not exist yet")
        sqlite_path = args.data_dir / "db" / "gateway.db"
        sqlite_path.parent.mkdir(parents=True)
    elif not args.database_url:
        parser.error("--database-url is required for postgres")
    started = time.monotonic()
    create_schema(args, sqlite_path)
    fixture = Fixture(load_fixture())
    units = [Unit(fixture, group) for group in interaction_components(fixture)]
    base = {"events": len(fixture.rows["observation_events"]), "contents": len(fixture.rows["turn_contents"]),
            "markers": len(fixture.rows["history_markers"])}
    extra = solve_mix(units, {"events": args.target_events, "contents": args.target_contents,
                              "markers": args.target_markers}, base)
    now_ms = int(time.time() * 1000)
    if args.backend == "postgres":
        insert_postgres(args.database_url, fixture.data, now_ms)
        sink: SqliteSink | PostgresSink = PostgresSink(args.database_url)
    else:
        insert_sqlite(sqlite_path, fixture.data, now_ms)
        sink = SqliteSink(sqlite_path)
    cloner = Cloner(fixture, now_ms)
    batch: dict[str, list[list[Any]]] = {}
    pending = clone = 0
    for unit, copies in zip(units, extra):
        for _ in range(copies):
            clone += 1
            for table, rows in cloner.clone(unit, clone).items():
                batch.setdefault(table, []).extend(rows)
            pending += 1
            if pending >= args.batch_clones:
                sink.write(fixture.columns, batch, fixture.order)
                batch, pending = {}, 0
    if batch:
        sink.write(fixture.columns, batch, fixture.order)
    sink.finish()
    achieved = inventory(args, sqlite_path)
    report = {
        "command": sys.argv, "backend": args.backend, "schema_version": 36,
        "storage_harness_sha256": hashlib.sha256(args.storage_harness.read_bytes()).hexdigest(),
        "fixture_sha256": hashlib.sha256(Path(load_fixture.__globals__["FIXTURE_PATH"]).read_bytes()).hexdigest(),
        "targets": {"events": args.target_events, "contents": args.target_contents, "markers": args.target_markers},
        "units": [{"interaction": u.interaction_id, **u.stats, "extra_copies": c} for u, c in zip(units, extra)],
        "clones": clone, "salted_contents": cloner.salted, "achieved_rows": achieved,
        "verification": verify_sample(args, sqlite_path, 500), "elapsed_seconds": round(time.monotonic() - started, 1),
        "settings": {name: fixture.data["settings"][name] for name in SETTINGS_KEYS},
    }
    if sqlite_path is not None:
        report["sqlite_bytes"] = sqlite_path.stat().st_size
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps({k: report[k] for k in ("clones", "achieved_rows", "targets", "elapsed_seconds", "verification")}, indent=2))


if __name__ == "__main__":
    main()
