"""Record the ADR-0089 upgrade fixture with the pre-ADR-0089 server.

The fixture is **recorded, not invented**: the server built from commit
``aecfc21e`` (the last commit before shared content storage) serves real
conversations against mocked upstreams, and the resulting SQLite tables are
exported as ``tests/e2e/fixtures/shared_content_upgrade/fixture.json``. The
file holds migration-36 rows (the schema before 0037) in a backend-neutral
form; ``tests/e2e/storage/test_shared_content_upgrade.py`` loads it into a
database prepared by the real migrations and starts the new server on it.

Usage (from the repository root)::

    git worktree add --detach ../old aecfc21e
    (cd ../old && cargo build --release -p stravia-server --no-default-features)
    STRAVIA_BINARY=../old/target/release/stravia-server \
        uv run --locked --group test python -m tests.common.record_shared_content_upgrade_fixture

``--verify-old`` replays the generated fixture into a database created by the
old server and checks, through the old binary's own readers, that the hand-added
rows (pending/renewed dependencies) are accepted as the old code understands
them.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import sqlite3
import sys
import tempfile
import time
from contextlib import closing, contextmanager
from pathlib import Path
from typing import Any, Iterator

from tests.common.shared_content_upgrade_fixture import COMPACTION_REMOVED_HISTORY, native_responses_upstream
from tests.common.helpers import (
    WebSession, find_free_port, http_request, initialize_server, start_stravia_server,
    stop_stravia_server, wait_for_setup_token, wait_until_ready,
)

REPO_ROOT = Path(__file__).resolve().parents[2]
FIXTURE_DIR = REPO_ROOT / "tests" / "e2e" / "fixtures" / "shared_content_upgrade"
SCHEMA_VERSION = 36
# 旧版 fixture 中用户设置的非默认保留天数：升级后依赖的保留窗口必须以它折算。
FIXTURE_RETENTION_DAYS = 30
LONG_QUESTION_CHARS = 6000


@contextmanager
def old_server(binary: Path, data_dir: Path, *, existing: bool = False) -> Iterator[dict[str, Any]]:
    port = find_free_port()
    base = f"http://127.0.0.1:{port}"
    process, logs = start_stravia_server(
        stravia_binary=binary,
        args=["--data-dir", str(data_dir), "--host", "127.0.0.1", "--port", str(port)],
    )
    try:
        wait_until_ready(f"{base}/api/v1/auth/state")
        if existing:
            session = WebSession(base)
            status, body = session.request("POST", "/api/v1/auth/login", {
                "username": "admin", "password": "correct horse battery staple",
            })
            assert status == 200, body
        else:
            session = initialize_server(base, wait_for_setup_token(logs, process), {"backend": "sqlite"})
        yield {"admin": base, "proxy": base, "auth": session.auth_headers(), "data_dir": data_dir}
    finally:
        stop_stravia_server(process, logs)


def admin(env: dict[str, Any], method: str, path: str, payload: Any = None, expect: int = 200) -> Any:
    status, body = http_request(method, f"{env['admin']}/api/v1/{path}", payload=payload, headers=env["auth"])
    assert status == expect, (method, path, status, body)
    return body["data"] if isinstance(body, dict) and "data" in body else body


def scenario_gemini_tool_rounds(env: dict[str, Any]) -> dict[str, Any]:
    """Responses 客户端经 Gemini 上游：受保护思考（Marker）、跨协议投影文本、批量工具结果。"""
    from tests.e2e.admin.test_observations import _create_route
    from tests.e2e.storage.test_interaction_continuation import continuation_provider, replay_round

    question = "Gemini fixture question. " + "Long user input keeps going. " * (LONG_QUESTION_CHARS // 29)
    with continuation_provider("google-gemini") as (upstream, received):
        model = "fixture-gemini"
        route, key = _create_route({**env, "mock": upstream}, model, retry_budget=0, protocol="google-gemini")
        history: list[dict[str, Any]] = [{"role": "user", "content": question}]
        responses = []
        for round_index in range(6):
            response = replay_round(env, key, model, history, round_index % 2 == 0)
            responses.append(response)
            calls = [item for item in response["output"] if item["type"] == "function_call"]
            for index, call in enumerate(calls):
                output: Any = f"synthetic result {round_index}-{index}"
                if round_index == 1 and index == 0:
                    output = [{"type": "input_text", "text": f"nested synthetic result {round_index}-{index}"}]
                history.append({"type": "function_call_output", "call_id": call["call_id"], "output": output})
        return {
            "route": route, "key": key, "model": model, "history": history,
            "response_ids": [response["id"] for response in responses], "question": question,
            "upstream_requests": len(received),
        }


def scenario_openai_reasoning(env: dict[str, Any]) -> dict[str, Any]:
    """Responses 客户端经 Chat 上游：可读推理摘要投影与嵌套工具参数。"""
    from tests.e2e.admin.test_observations import _create_route
    from tests.e2e.storage.test_interaction_continuation import continuation_provider, replay_round

    with continuation_provider("openai-compatible") as (upstream, received):
        model = "fixture-openai"
        route, key = _create_route({**env, "mock": upstream}, model, retry_budget=0)
        history: list[dict[str, Any]] = [{"role": "user", "content": "OpenAI fixture question \u2014 \u4e2d\u6587 \U0001f600"}]
        responses = []
        for round_index in range(3):
            response = replay_round(env, key, model, history, round_index == 1)
            responses.append(response)
            for index, call in enumerate(item for item in response["output"] if item["type"] == "function_call"):
                history.append({"type": "function_call_output", "call_id": call["call_id"],
                                "output": f"openai synthetic result {round_index}-{index}"})
        return {"route": route, "key": key, "model": model, "history": history,
                "response_ids": [response["id"] for response in responses]}


def scenario_redaction(env: dict[str, Any]) -> dict[str, Any]:
    """可逆脱敏：映射在首轮登记并发布，次轮复用。"""
    from tests.e2e.admin.test_observations import _create_route
    from tests.e2e.admin.test_reversible_redaction import REFERENCE, SECRET, echo_provider, set_enabled

    with echo_provider() as (upstream, received):
        scoped = {**env, "mock": upstream}
        model = "fixture-redaction"
        route, key = _create_route(scoped, model)
        set_enabled(scoped, True)
        headers = {"authorization": f"Bearer {key}"}
        first_text = f"Keep this configuration unchanged: github_token={SECRET}; end."
        status, first = http_request("POST", f"{env['proxy']}/v1/responses", payload={
            "model": model, "input": [{"role": "user", "content": first_text}]}, headers=headers)
        assert status == 200, first
        reference = REFERENCE.search(json.dumps(received[-1]["body"]))
        assert reference is not None
        history = [{"role": "user", "content": first_text}] + [
            {k: v for k, v in item.items() if k not in ("id", "status")} for item in first["output"]]
        time.sleep(2.1)  # 新的 User 输入需要越过快速续接窗口，才会形成独立 Interaction
        history.append({"role": "user", "content": f"Again {SECRET} please."})
        status, second = http_request("POST", f"{env['proxy']}/v1/responses", payload={
            "model": model, "input": history}, headers=headers)
        assert status == 200, second
        return {"route": route, "key": key, "model": model, "secret": SECRET,
                "reference": reference.group(), "response_ids": [first["id"], second["id"]],
                "first_text": first_text}


def scenario_native_compaction(env: dict[str, Any]) -> dict[str, Any]:
    """原生压缩：来源生成、交付确认与客户端回传。"""
    from tests.e2e.admin.test_compaction import _native_route

    with native_responses_upstream() as (upstream, _received):
        model = "fixture-compaction"
        route, key = _native_route(env, upstream, model)
        headers = {"authorization": f"Bearer {key}"}
        source_input = [{"role": "user", "content": COMPACTION_REMOVED_HISTORY}]
        status, source = http_request("POST", f"{env['proxy']}/v1/responses", payload={
            "model": model, "input": source_input}, headers=headers)
        assert status == 200, source
        status, compacted = http_request("POST", f"{env['proxy']}/v1/responses/compact", payload={
            "model": model, "input": source_input + source["output"]}, headers=headers)
        assert status == 200, compacted
        time.sleep(2.1)
        status, continued = http_request("POST", f"{env['proxy']}/v1/responses", payload={
            "model": model, "input": compacted["output"] + [{"role": "user", "content": "continue after compaction"}]},
            headers=headers)
        assert status == 200, continued
        return {"route": route, "key": key, "model": model, "window": compacted["output"],
                "source_id": source["id"], "continued_id": continued["id"]}


def scenario_failover(env: dict[str, Any]) -> dict[str, Any]:
    """同一轮次内第一个 Target 失败、第二次尝试成功：旧库里每个 attempt 各自成行。"""
    from tests.common.helpers import find_free_port, minimal_mock_provider
    from tests.e2e.admin.test_observations import _create_route

    port = find_free_port()
    mock_server, _ = minimal_mock_provider(port)
    try:
        model = "observation-root-retry-recovered"
        route, key = _create_route({**env, "mock": f"http://127.0.0.1:{port}"}, model, retry_budget=1)
        status, body = http_request("POST", f"{env['proxy']}/v1/responses", payload={
            "model": model, "input": [{"role": "user", "content": model}]},
            headers={"authorization": f"Bearer {key}"})
        assert status == 200, body
        return {"route": route, "key": key, "model": model}
    finally:
        mock_server.shutdown()
        mock_server.server_close()


def decode_codec(blob: bytes) -> bytes:
    """还原 storage_codec 帧（旧版 RAW=0 / zstd=1，尾部 14 字节为编码标记+长度）。"""
    from compression import zstd

    body, marker = blob[:-14], blob[-14]
    return body if marker == 0 else zstd.decompress(body)


EXPORT_TABLES = [
    # 配置：Provider / Route / Key 保持原 id（原生压缩的 Target 绑定与 Observation 都引用它们）。
    "api_keys", "providers", "provider_models", "models", "model_backends", "api_key_models", "turn_scopes", "turn_nodes", "turn_node_payloads", "turn_contents", "turn_node_contents",
    "turn_node_retention", "history_markers", "reversible_redaction_mappings", "native_compactions",
    "native_compaction_states", "native_compaction_sources", "interaction_observations",
    "inference_run_observations", "model_turn_observations", "target_attempt_observations",
    "observation_sequence", "observation_events", "observation_pending_tools", "observation_tail_sources",
    "credential_mapping_reuses",
]
TIME_COLUMN_NAMES = {
    "created_at", "updated_at", "published_at", "expires_at", "started_at", "last_active_at", "finished_at",
    "occurred_at", "delivered_at", "referenced_at", "prefix_completed_at", "lease_expires_at",
    "execution_deadline", "delivery_completed_at", "last_reused_at",
}
MINUTE = 60_000
DAY = 24 * 60 * MINUTE
VARIANTS = {
    # 名称: (published_at 相对 R, expires_at 相对 R)；published 为 None 表示未发布。
    "pending_live": (None, 30 * MINUTE),
    "pending_expired": (None, -10 * MINUTE),
    "renewed": (-40 * DAY, 3 * DAY),
    "lapsed": (-40 * DAY, -2 * DAY),
}


def new_reference(source: str, used: set[str]) -> str:
    import random

    while True:
        letters = "".join(random.choice("abcdefghijklmnopqrstuvwxyz") for _ in range(28))
        candidate = re.sub(r"[a-z]{28}", letters, source, count=1)
        if candidate not in used:
            used.add(candidate)
            return candidate


def add_synthetic_dependencies(connection: sqlite3.Connection, recorded_at: int, expectations: dict[str, Any]) -> None:
    """补入真实流量无法稳定产生的状态：未发布、续期过的与已超期的依赖。

    行由真实录制行克隆而来，仅替换引用与可观察标识；``--verify-old`` 会用旧版本二进制
    的读取路径确认旧代码接受这些行。
    """
    connection.row_factory = sqlite3.Row
    used = {row[0] for row in connection.execute("SELECT reference FROM history_markers")}
    used |= {row[0] for row in connection.execute("SELECT reference FROM reversible_redaction_mappings")}
    marker = connection.execute(
        "SELECT * FROM history_markers WHERE principal = ? AND kind = 'thinking' ORDER BY created_at LIMIT 1",
        (principal_of(connection, expectations["gemini"]["key"]),),
    ).fetchone()
    mapping = connection.execute("SELECT * FROM reversible_redaction_mappings").fetchone()
    plain_column = next(name for name in mapping.keys() if name not in (
        "reference", "principal", "published_at", "created_at", "updated_at", "expires_at", "rule_ids"))
    synthetic: dict[str, Any] = {"markers": {}, "mappings": {}, "compactions": {}}
    for variant, (published, expires) in VARIANTS.items():
        reference = new_reference(marker["reference"], used)
        signature = f"synthetic-{variant.replace('_', '-')}"
        payload = marker["segment_payload"].replace("synthetic-protected-0-0", signature)
        assert payload != marker["segment_payload"]
        row = dict(marker)
        row.update(reference=reference, segment_payload=payload, created_at=recorded_at - 5 * MINUTE,
                   updated_at=recorded_at - 5 * MINUTE,
                   published_at=None if published is None else recorded_at + published,
                   expires_at=recorded_at + expires)
        insert_row(connection, "history_markers", row)
        synthetic["markers"][variant] = {"reference": reference, "signature": signature}

        reference = new_reference(mapping["reference"], used)
        plaintext = f"synthetic-secret-{variant.replace('_', '-')}"
        row = dict(mapping)
        row.update(reference=reference, **{plain_column: plaintext}, created_at=recorded_at - 5 * MINUTE,
                   updated_at=recorded_at - 5 * MINUTE,
                   published_at=None if published is None else recorded_at + published,
                   expires_at=recorded_at + expires)
        insert_row(connection, "reversible_redaction_mappings", row)
        synthetic["mappings"][variant] = {"reference": reference, "plaintext": plaintext}
    compaction = connection.execute("SELECT * FROM native_compactions").fetchone()
    state = connection.execute("SELECT * FROM native_compaction_states").fetchone()
    for variant in ("pending_live", "pending_expired"):
        record_id = new_reference(compaction["id"], used)
        row = dict(compaction)
        payload = json.loads(compaction["payload"])
        payload["id"] = record_id
        row.update(id=record_id, payload=json.dumps(payload, separators=(",", ":")), created_at=recorded_at - 5 * MINUTE,
                   delivered_at=None, referenced_at=None, expires_at=recorded_at + VARIANTS[variant][1])
        insert_row(connection, "native_compactions", row)
        clone = dict(state)
        clone.update(record_id=record_id, native_identity=f"cmp_state_{variant}",
                     fingerprint=f"{variant}-fingerprint".ljust(64, "0"))
        insert_row(connection, "native_compaction_states", clone)
        synthetic["compactions"][variant] = {"record_id": record_id}
    expectations["synthetic"] = synthetic


def insert_row(connection: sqlite3.Connection, table: str, row: dict[str, Any]) -> None:
    names = ",".join(f'"{name}"' for name in row)
    marks = ",".join("?" for _ in row)
    connection.execute(f"INSERT INTO {table}({names}) VALUES ({marks})", list(row.values()))


def principal_of(connection: sqlite3.Connection, token: str) -> str:
    return "api-key:" + connection.execute("SELECT id FROM api_keys WHERE token = ?", (token,)).fetchone()[0]


def observation_expectations(connection: sqlite3.Connection) -> dict[str, Any]:
    """旧事件的语义摘要：升级后必须能从新形态逐项还原。"""
    result: dict[str, Any] = {}
    for interaction, preview, tail in connection.execute(
        "SELECT id, input_preview, visible_tail FROM interaction_observations ORDER BY started_at, id"
    ).fetchall():
        runs: dict[str, Any] = {}
        for run_id, kind, payload in connection.execute(
            "SELECT run_id, kind, payload FROM observation_events WHERE interaction_id = ? ORDER BY sequence",
            (interaction,),
        ):
            if run_id is None:
                continue
            body = json.loads(decode_codec(payload))
            run = runs.setdefault(run_id, {"kinds": [], "tool_results": [], "texts": []})
            run["kinds"].append(kind)
            if kind == "client_tool_result":
                run["tool_results"].append(
                    {"tool_id": body["tool_id"], "content": body["content"], "is_error": body["is_error"]})
            elif kind == "target_attempt_finished":
                run.setdefault("attempts", []).append(
                    {"status": body.get("status"), "error_code": body.get("error_code")})
            elif kind in ("model_thinking", "client_visible_content"):
                run["texts"].append({"kind": kind, "text": body["text"], "block_id": body["block_id"]})
        result[interaction] = {"input_preview": preview, "visible_tail": tail, "runs": runs}
    return result


def export_fixture(database: Path, expectations: dict[str, Any]) -> dict[str, Any]:
    with closing(sqlite3.connect(database)) as connection:
        recorded_at = connection.execute("SELECT MAX(occurred_at) FROM observation_events").fetchone()[0]
        add_synthetic_dependencies(connection, recorded_at, expectations)
        connection.commit()
        connection.row_factory = None
        expectations["observation"] = observation_expectations(connection)
        tables: dict[str, Any] = {}
        time_columns: dict[str, list[str]] = {}
        for table in EXPORT_TABLES:
            info = connection.execute(f"PRAGMA table_info({table})").fetchall()
            columns = [column[1] for column in info]
            time_columns[table] = [
                column[1] for column in info
                if column[1] in TIME_COLUMN_NAMES and column[2].upper() in ("INTEGER", "BIGINT")
            ]
            try:
                connection.execute(f"SELECT rowid FROM {table} LIMIT 0")
                order = "rowid"
            except sqlite3.OperationalError:  # WITHOUT ROWID 表按主键顺序导出
                order = ",".join(column[1] for column in sorted((c for c in info if c[5]), key=lambda c: c[5]))
            rows = [
                [{"$b": value.hex()} if isinstance(value, bytes) else value for value in row]
                for row in connection.execute(f"SELECT * FROM {table} ORDER BY {order}")
            ]
            tables[table] = {"columns": columns, "rows": rows}
        settings = dict(connection.execute(
            "SELECT name, value FROM settings WHERE name IN ('log_retention_days', 'reversible_redaction_enabled')"
        ).fetchall())
    return {
        "schema_version": SCHEMA_VERSION, "recorded_at": recorded_at, "order": EXPORT_TABLES,
        "time_columns": time_columns, "settings": settings, "tables": tables, "expectations": expectations,
    }


FOLLOW_UP = "Follow-up question asked after the upgrade."


def capture_follow_ups(binary: Path, data_dir: Path, expectations: dict[str, Any]) -> None:
    """记录旧服务端在已有历史上续接时实际发给上游的请求体。

    在数据目录副本上进行，避免追加的续接轮次混入导出的 fixture。升级后的服务端必须
    对同一段历史发出逐字节相同的上游请求。
    """
    from tests.e2e.storage.test_interaction_continuation import continuation_provider, replay_round

    with tempfile.TemporaryDirectory(prefix="adr0089-followup-") as temporary:
        copy = Path(temporary) / "data"
        shutil.copytree(data_dir, copy)
        with old_server(binary, copy, existing=True) as env:
            providers = {item["name"]: item["id"] for item in admin(env, "GET", "providers")}
            for scenario, protocol in (("gemini", "google-gemini"), ("openai", "openai-compatible")):
                spec = expectations[scenario]
                with continuation_provider(protocol) as (upstream, received):
                    admin(env, "PUT", f"providers/{providers[spec['model'] + '-provider']}", {"base_url": upstream})
                    history = [*spec["history"], {"role": "user", "content": FOLLOW_UP}]
                    replay_round(env, spec["key"], spec["model"], history, False)
                    assert len(received) == 1, received
                    spec["follow_up_upstream"] = received[0]


def record(binary: Path, data_dir: Path) -> dict[str, Any]:
    with old_server(binary, data_dir) as env:
        # 非默认保留天数；可逆脱敏开关一并保留在旧库中。
        admin(env, "PUT", "settings/log_retention_days", {"value": str(FIXTURE_RETENTION_DAYS)})
        expectations: dict[str, Any] = {}
        expectations["gemini"] = scenario_gemini_tool_rounds(env)
        expectations["openai"] = scenario_openai_reasoning(env)
        expectations["redaction"] = scenario_redaction(env)
        expectations["compaction"] = scenario_native_compaction(env)
        expectations["failover"] = scenario_failover(env)
        # 排空 writer：客户端响应先于 Observation 落库。
        time.sleep(1.0)
    capture_follow_ups(binary, data_dir, expectations)
    return export_fixture(data_dir / "db" / "gateway.db", expectations)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary", default=os.environ.get("STRAVIA_BINARY"))
    parser.add_argument("--keep", help="keep the recorded old data directory here")
    parser.add_argument("--output", default=str(FIXTURE_DIR / "fixture.json"))
    arguments = parser.parse_args()
    assert arguments.binary, "pass --binary or STRAVIA_BINARY (the pre-ADR-0089 server)"
    with tempfile.TemporaryDirectory(prefix="adr0089-record-") as temporary:
        data_dir = Path(arguments.keep or temporary)
        data_dir.mkdir(parents=True, exist_ok=True)
        fixture = record(Path(arguments.binary), data_dir)
    output = Path(arguments.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(fixture, ensure_ascii=False, indent=None, separators=(",", ":")) + "\n", encoding="utf-8")
    print(f"wrote {output} ({output.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
