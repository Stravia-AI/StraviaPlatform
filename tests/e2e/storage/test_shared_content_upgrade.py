"""ADR-0089 升级验收（SQLite 与 PostgreSQL）：旧版本录制的数据库 → 新版本服务端。

fixture 由 ``tests.common.record_shared_content_upgrade_fixture`` 用 ADR-0089 之前的服务端
（``aecfc21e``）经 HTTP 录制：真实的 Turn 链、受保护思考 Marker、可逆脱敏映射、原生压缩与
Observation 事件，外加少量由真实行克隆的“未发布 / 续期 / 已过期”依赖行。这里只做两件事：

1. 由存储 harness 安装真实的迁移 1..36，再写入 fixture（时间列整体平移，使最新事件落在当下）；
2. 启动**新**服务端，只通过 HTTP 断言升级后的可观察行为。

保留期的验证方式（HTTP 没有“指定时间点清扫”的钩子）：升级后保存一次 ``log_retention_days``
会立即触发保留清扫（见 ``test_retention_*``），以真实时钟执行；fixture 里的续期依赖在旧库中
还有几天才过期，所以清扫发生在旧 ``expires_at`` 之前，续期依赖必须仍可解析，而旧库里已过期的
依赖必须不可解析、也不会被复活。
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import time
from contextlib import ExitStack, contextmanager
from pathlib import Path
from typing import Any, Iterator

import pytest

from tests.common.helpers import (
    http_request, initialize_server, start_stravia_server, stop_stravia_server,
    wait_for_setup_token, wait_until_ready, find_free_port,
)
from tests.common.shared_content_upgrade_fixture import (
    COMPACTION_REMOVED_HISTORY, COMPACTION_STATE, FIXTURE_PATH, insert_postgres, insert_sqlite, load_fixture,
    native_responses_upstream,
)
from tests.common.observation_contents import (
    assert_summary_has_only_refs, attempt_finishes, input_text, output_text,
)
from tests.e2e.admin.test_credential_protection import discovery_hit, discoveries
from tests.e2e.admin.test_observations import _detail
from tests.e2e.admin.test_reversible_redaction import REFERENCE, SECRET, echo_provider
from tests.e2e.storage.conftest import REPO_ROOT, storage_harness_binary
from tests.e2e.storage.test_interaction_continuation import continuation_provider, replay_round

# 原生压缩的 Target 指纹把 Provider 的 base_url 算进了命名空间，fixture 因此固定使用
# COMPACTION_UPSTREAM_PORT；同一时刻只能有一个测试占用它。并行（xdist ``--dist loadgroup``）时
# 用同一个 xdist_group 把本文件的用例串行到同一个 worker 上。
pytestmark = [pytest.mark.e2e, pytest.mark.storage, pytest.mark.xdist_group("shared-content-upgrade-fixed-port")]

FOLLOW_UP = "Follow-up question asked after the upgrade."
BACKENDS = pytest.mark.parametrize("backend", ["sqlite", "postgres"], ids=["sqlite", "postgres"])


@pytest.fixture(scope="module")
def fixture() -> dict[str, Any]:
    return load_fixture()


@contextmanager
def upgraded_server(
    stravia_binary: Path, storage_runtime: dict[str, Any], tmp_path: Path, backend: str,
    fixture: dict[str, Any],
) -> Iterator[dict[str, Any]]:
    """harness 安装迁移 ≤36 → 写入录制数据 → 启动新服务端并完成初始化（执行 0037+ 迁移）。"""
    schema = None
    process = None
    logs: list[str] = []
    database: dict[str, Any] = {"backend": "sqlite"}
    prepare_env = os.environ.copy()
    prepare_env["STRAVIA_STORAGE_USAGE_UPGRADE_FIXTURE"] = "shared-content-history"
    prepare_env["STRAVIA_STORAGE_BACKEND"] = backend
    try:
        if backend == "postgres":
            pg_url = storage_runtime["pg_url"]
            if not pg_url:
                pytest.fail("PostgreSQL upgrade HTTP acceptance requires isolated DB_URL")
            schema = storage_runtime["make_isolated_schema"]("stravia_shared_content_upgrade")
            storage_runtime["run_schema_action"]("create", pg_url=pg_url, schema=schema)
            dsn = storage_runtime["postgres_dsn_for_schema"](pg_url, schema)
            database = {"backend": "postgres", "url": dsn}
            prepare_env["STRAVIA_STORAGE_PG_URL"] = dsn
        else:
            sqlite_path = tmp_path / "db" / "gateway.db"
            sqlite_path.parent.mkdir(parents=True)
            prepare_env["STRAVIA_STORAGE_SQLITE_PATH"] = str(sqlite_path)
        prepared = subprocess.run(
            [str(storage_harness_binary())], env=prepare_env, cwd=REPO_ROOT,
            text=True, encoding="utf-8", capture_output=True, check=False, timeout=120,
        )
        assert prepared.returncode == 0, (prepared.stdout, prepared.stderr)
        assert "usage_upgrade_fixture=shared-content-history" in prepared.stdout

        now_ms = int(time.time() * 1000)
        if backend == "postgres":
            insert_postgres(database["url"], fixture, now_ms)
        else:
            insert_sqlite(sqlite_path, fixture, now_ms)

        base = f"http://127.0.0.1:{find_free_port()}"
        args = ["--data-dir", str(tmp_path), "--host", "127.0.0.1",
                "--port", base.rsplit(":", 1)[1], *storage_runtime["server_args"](backend)]
        process, logs = start_stravia_server(stravia_binary=stravia_binary, args=args)
        wait_until_ready(f"{base}/api/v1/auth/state")
        try:
            session = initialize_server(base, wait_for_setup_token(logs, process), database)
        except AssertionError as error:
            stop_stravia_server(process, logs)
            raise AssertionError(f"{error}\n" + "\n".join(logs[-40:])) from error
        env: dict[str, Any] = {"admin": base, "proxy": base, "auth": session.auth_headers(), "data_dir": tmp_path,
                               "backend": backend, "now_ms": now_ms}
        with ExitStack() as stack:
            env["received"] = attach_upstreams(env, fixture["expectations"], stack)
            yield env
    finally:
        if process is not None:
            stop_stravia_server(process, logs)
        if schema is not None:
            storage_runtime["run_schema_action"]("drop", pg_url=storage_runtime["pg_url"], schema=schema)


def repoint_provider(env: dict[str, Any], name: str, upstream: str) -> None:
    """录制库里的 Provider 指向已不存在的 mock 端口：改为本次测试的上游，其余配置与 id 保持升级后原样。"""
    status, body = http_request("GET", f"{env['admin']}/api/v1/providers", headers=env["auth"])
    assert status == 200, body
    provider = next(item for item in body["data"] if item["name"] == name)
    status, body = http_request(
        "PUT", f"{env['admin']}/api/v1/providers/{provider['id']}", payload={"base_url": upstream},
        headers=env["auth"],
    )
    assert status == 200, body


def attach_upstreams(env: dict[str, Any], expectations: dict[str, Any], stack: ExitStack) -> dict[str, list[dict[str, Any]]]:
    """每个录制场景的上游换成本次测试的 mock，收集上游实际收到的请求。"""
    received: dict[str, list[dict[str, Any]]] = {}
    for scenario, protocol, provider in (
        ("gemini", "google-gemini", "fixture-gemini-provider"),
        ("openai", "openai-compatible", "fixture-openai-provider"),
    ):
        upstream, received[scenario] = stack.enter_context(continuation_provider(protocol))
        repoint_provider(env, provider, upstream)
    upstream, received["redaction"] = stack.enter_context(echo_provider())
    repoint_provider(env, "fixture-redaction-provider", upstream)
    # 原生压缩绑定了录制时的 base_url（固定端口），不改写 Provider。
    _, received["compaction"] = stack.enter_context(native_responses_upstream())
    return received


def continue_conversation(env: dict[str, Any], spec: dict[str, Any], scenario: str) -> dict[str, Any]:
    """向新服务端续接旧会话，返回上游实际收到的请求体。"""
    received = env["received"][scenario]
    before = len(received)
    history = [*spec["history"], {"role": "user", "content": FOLLOW_UP}]
    replay_round(env, spec["key"], spec["model"], history, False)
    assert len(received) == before + 1, "exactly one upstream request per continuation"
    return received[-1]


def restored(env: dict[str, Any], key: str, model: str, text: str) -> str:
    status, body = http_request(
        "POST", f"{env['proxy']}/v1/chat/completions",
        payload={"model": model, "messages": [{"role": "user", "content": text}]},
        headers={"authorization": f"Bearer {key}"}, timeout=15.0,
    )
    assert status == 200, body
    return body["choices"][0]["message"]["content"]


def marker_signature_reaches_upstream(env: dict[str, Any], spec: dict[str, Any], marker: dict[str, str]) -> bool:
    """客户端回放含该 Marker 的推理项：上游 Gemini 请求里能否还原出受保护签名。"""
    received = env["received"]["gemini"]
    history = [
        {"role": "user", "content": f"marker probe {marker['reference']}"},
        {"type": "reasoning", "summary": [],
         "content": [{"type": "reasoning_text", "text": f"<!--sh:{marker['reference']}-->\n"}]},
        {"type": "function_call", "call_id": "probe", "name": "local_probe", "arguments": "{}"},
        {"type": "function_call_output", "call_id": "probe", "output": "ok"},
    ]
    before = len(received)
    replay_round(env, spec["key"], spec["model"], history, False)
    assert len(received) == before + 1
    return marker["signature"] in json.dumps(received[-1])


def gemini_content_parts(body: dict[str, Any]) -> list[dict[str, Any]]:
    """Ignore only grouping of consecutive user function responses, not their order or contents."""
    contents = []
    for content in body["contents"]:
        parts = content.get("parts", [])
        if (set(content) == {"role", "parts"} and content["role"] == "user" and parts
                and all("functionResponse" in part for part in parts)):
            contents.extend({"role": "user", "parts": [part]} for part in parts)
        else:
            contents.append(content)
    return contents


def assert_turn_content_and_markers(env: dict[str, Any], fixture: dict[str, Any]) -> None:
    """Turn 内容与 Marker 回放：保留旧上游请求的内容、签名和顺序。"""
    expectations = fixture["expectations"]
    for scenario in ("gemini", "openai"):
        spec = expectations[scenario]
        actual = continue_conversation(env, spec, scenario)
        expected = spec["follow_up_upstream"]
        if scenario == "gemini":
            assert {k: v for k, v in actual.items() if k != "contents"} == {
                k: v for k, v in expected.items() if k != "contents"
            }
            assert gemini_content_parts(actual) == gemini_content_parts(expected), scenario
            gemini = json.dumps(actual)
        else:
            assert actual == expected, scenario
    # 6000 字符的长问题、受保护思考签名、嵌套工具结果都在续接请求里还原。
    assert expectations["gemini"]["question"] in gemini
    assert "synthetic-protected-4-1" in gemini and "nested synthetic result 1-0" in gemini


def assert_dependency_resolution(env: dict[str, Any], fixture: dict[str, Any]) -> None:
    """合成依赖：旧库里可解析的升级后仍可解析，旧库里已过期 / 未发布的 Marker 不会被复活。"""
    expectations = fixture["expectations"]
    synthetic = expectations["synthetic"]
    resolvable = {"pending_live": True, "pending_expired": False, "renewed": True, "lapsed": False}
    redaction = expectations["redaction"]
    for variant, expected in resolvable.items():
        mapping = synthetic["mappings"][variant]
        content = restored(env, redaction["key"], redaction["model"], f"x {mapping['reference']}")
        assert content == (f"x {mapping['plaintext']}" if expected else f"x {mapping['reference']}"), variant
    gemini = expectations["gemini"]
    # 只有“已发布且未过期”的 Marker 能被客户端回放还原；未发布的旧库里本来就不可解析。
    marker_resolvable = {"pending_live": False, "pending_expired": False, "renewed": True, "lapsed": False}
    for variant, expected in marker_resolvable.items():
        assert marker_signature_reaches_upstream(env, gemini, synthetic["markers"][variant]) is expected, variant


def assert_mapping_and_hit_detail(env: dict[str, Any], fixture: dict[str, Any]) -> None:
    """已录制的映射：复用同一引用、管理面命中详情可展示明文。"""
    redaction = fixture["expectations"]["redaction"]
    content = restored(env, redaction["key"], redaction["model"], f"Again {SECRET} please.")
    assert content == f"Again {SECRET} please."
    reference = REFERENCE.search(json.dumps(env["received"]["redaction"][-1]["body"]))
    assert reference is not None and reference.group() == redaction["reference"], (
        "the upgraded mapping must be reused for the same plaintext"
    )
    items = discoveries(env, limit=100)["items"]
    created = [item for item in items if item["api_key_name"] == f"{redaction['model']}-key"]
    assert created, items
    revealed = discovery_hit(env, created[-1]["id"], reveal=True)
    assert revealed["masked"] is False and SECRET in revealed["text"], revealed


def converted_kinds(old_kinds: list[str]) -> list[str]:
    """旧库逐行事件在升级后的预期事件种类（顺序无关，按多重集比较）。

    - ``input_preview_recorded`` 改名 ``user_input_recorded``；
    - 每个 Model Turn 的首个 ``target_attempt_started`` 并入 ``model_turn_started``；
    - 每个 Model Turn 的最后一个 ``target_attempt_finished`` 并入 ``model_turn_finished``
      （被取代的中间 attempt 保留各自的行）；
    - ``client_tool_result`` 旧库每条结果一行，升级逐行转换为只含一条结果的批（不跨行合并）。
    """
    out: list[str] = []
    turn: list[str] = []
    in_turn = False
    for kind in old_kinds:
        if kind == "model_turn_started":
            in_turn, turn = True, []
        if in_turn and kind not in ("model_turn_started", "model_turn_finished"):
            turn.append(kind)
        if kind == "model_turn_finished":
            starts = [i for i, item in enumerate(turn) if item == "target_attempt_started"]
            finishes = [i for i, item in enumerate(turn) if item == "target_attempt_finished"]
            drop = {starts[0], finishes[-1]} if starts and finishes else set()
            out.extend(item for i, item in enumerate(turn) if i not in drop)
            in_turn = False
        if not in_turn or kind in ("model_turn_started", "model_turn_finished"):
            out.append(kind)
    out = ["user_input_recorded" if kind == "input_preview_recorded" else kind for kind in out]
    return out


def assert_observation_converted(env: dict[str, Any], fixture: dict[str, Any]) -> None:
    """旧库的 Observation 经升级后在管理面读到的时间线、批量工具结果与全文。"""
    problems: list[str] = []

    def masked(text: str) -> str:
        # 旧预览里是明文凭据；管理面按当前有效映射把它遮蔽为 ***（占位符原样保留）。
        return re.sub(r"ghp_[A-Za-z0-9]{36}", "***", text)

    for interaction_id, expected in fixture["expectations"]["observation"].items():
        detail = _detail(env, interaction_id)
        try:
            assert_summary_has_only_refs(detail["interaction"])
            # 旧的 4096 字符截断预览升级后原样保留：没有可恢复的更长正文，也不凭空补全。
            assert detail["input_text"] == masked(expected["input_preview"]), interaction_id
            assert input_text(env, detail["interaction"]) == masked(expected["input_preview"])
            # 详情的 output_text 是整条 Interaction 的可见输出，与旧 visible_tail 同口径；
            # 摘要只引用最新一个可见块，所以它是详情输出的结尾。
            tail = masked(expected["visible_tail"]).strip()
            latest_block = output_text(env, detail["interaction"]).strip()
            assert detail["output_text"].strip() == tail, (interaction_id, detail["output_text"], tail)
            assert bool(latest_block) == bool(tail), interaction_id
            assert tail.endswith(latest_block), (interaction_id, latest_block, tail)
        except AssertionError as error:
            problems.append(f"{interaction_id}: {error!r}"[:900])

        runs = {run["id"]: run for run in detail["runs"]}
        assert set(runs) == set(expected["runs"]), interaction_id
        for run_id, spec in expected["runs"].items():
            try:
                events = sorted(runs[run_id]["events"], key=lambda event: event["sequence"])
                kinds = [event["kind"] for event in events]
                want = converted_kinds(spec["kinds"])
                # 启动时的对账会把仍在等待客户端的 Run 再记一次 run_state_changed，其余种类必须与旧库逐一对应。
                assert [k for k in sorted(kinds) if k != "run_state_changed"] == [
                    k for k in sorted(want) if k != "run_state_changed"], (run_id, " ".join(kinds), " ".join(want))
                assert kinds.count("run_state_changed") >= want.count("run_state_changed"), (run_id, kinds)
                assert not {"input_preview_recorded", "usage_confirmed"} & set(kinds)
                if "model_turn_started" in kinds:
                    assert kinds.index("model_turn_started") < kinds.index("model_turn_finished")
                # 合并后的结束行带最后一个 attempt。
                finishes = [event for event in events if event["kind"] == "model_turn_finished"]
                assert all(event["payload"].get("attempt") for event in finishes), (run_id, finishes)
                starts = [event for event in events if event["kind"] == "model_turn_started"]
                assert all(event["payload"].get("attempt") for event in starts), (run_id, starts)
                # failover：被取代的 attempt 保留各自的结束行，最后一个 attempt 的结束并入 model_turn_finished。
                recorded = [{"status": a["status"], "error_code": a.get("error_code")} for a in attempt_finishes(events)]
                assert recorded == spec.get("attempts", recorded), (run_id, recorded, spec.get("attempts"))
                if len(spec.get("attempts", [])) > 1:
                    standalone = [e for e in events if e["kind"] == "target_attempt_finished"]
                    assert len(standalone) == len(spec["attempts"]) - 1, (run_id, [e["kind"] for e in events])
                batches = [event for event in events if event["kind"] == "client_tool_result"]
                expected_results = spec["tool_results"]
                if expected_results:
                    # 升级只逐行转换，不跨行合并：每条旧结果仍是一行、一条结果的批。
                    assert all(len(batch["payload"]["results"]) == 1 for batch in batches), (run_id, batches)
                    assert [batch["payload"]["results"][0] for batch in batches] == expected_results, run_id
                else:
                    assert not batches
                texts = [
                    {"kind": event["kind"], "text": event["payload"]["text"], "block_id": event["payload"]["block_id"]}
                    for event in events if event["kind"] in ("model_thinking", "client_visible_content")
                ]
                assert texts == [{**item, "text": masked(item["text"])} for item in spec["texts"]], (run_id, texts)
            except AssertionError as error:
                problems.append(f"{interaction_id}/{run_id}: {error!r}"[:900])
    # 汇总所有 Run 的差异一次性报告，避免第一处差异掩盖其余转换问题。
    assert not problems, "\n".join(problems)


def assert_error_details_survive(env: dict[str, Any], fixture: dict[str, Any]) -> None:
    """升级前最终失败的请求：客户端错误与上游错误的列表、详情仍给出旧服务端记录的错误对象与归属。"""
    spec = fixture["expectations"]["upstream_failure"]
    now = int(time.time() * 1000)
    window = f"start_at={now - 3_600_000}&end_at={now + 60_000}&limit=100"

    def fetch(path: str) -> Any:
        status, body = http_request("GET", f"{env['admin']}/api/v1/observations/{path}", headers=env["auth"])
        assert status == 200, (path, body)
        return body["data"]

    client = spec["client_error"]
    listed = [item for item in fetch(f"client-errors?{window}&type=failed")["items"] if item["id"] == client["id"]]
    assert len(listed) == 1, client["id"]
    assert listed[0]["error"] == client["error"] and listed[0]["model"] == client["model"]
    request = fetch(f"client-errors/{client['kind']}/{client['id']}")["request"]
    assert request["error"] == client["detail_error"] and request["type"] == client["type"]

    upstream = spec["upstream_error"]
    listed = [item for item in fetch(f"upstream-errors?{window}")["items"] if item["id"] == upstream["id"]]
    assert len(listed) == 1, upstream["id"]
    assert {key: listed[0][key] for key in (
        "outcome", "error", "provider_id", "provider_name", "upstream_model", "model", "api_key_name",
    )} == {key: upstream[key] for key in (
        "outcome", "error", "provider_id", "provider_name", "upstream_model", "model", "api_key_name",
    )}
    detail = fetch(f"upstream-errors/{upstream['id']}")
    assert detail["error"]["id"] == upstream["id"] and detail["error"]["error"] == upstream["error"]
    assert [attempt["status"] for attempt in detail["attempts"]] == upstream["detail_attempt_statuses"]


def assert_native_compaction_survives(env: dict[str, Any], fixture: dict[str, Any]) -> None:
    compaction = fixture["expectations"]["compaction"]
    status, continued = http_request(
        "POST", f"{env['proxy']}/v1/responses",
        payload={"model": compaction["model"], "input": [*compaction["window"], {"role": "user", "content": FOLLOW_UP}]},
        headers={"authorization": f"Bearer {compaction['key']}"},
    )
    assert status == 200, continued
    assert continued["output"][0]["content"][0]["text"] == "continued from compacted window"
    upstream_input = json.dumps(env["received"]["compaction"][-1]["body"]["input"])
    assert COMPACTION_STATE["encrypted_content"] in upstream_input
    assert COMPACTION_REMOVED_HISTORY not in upstream_input


@BACKENDS
def test_upgrade_keeps_turn_content_markers_mappings_and_compactions(
    stravia_binary: Path, storage_runtime: dict[str, Any], tmp_path: Path, backend: str,
    fixture: dict[str, Any],
) -> None:
    with upgraded_server(stravia_binary, storage_runtime, tmp_path, backend, fixture) as env:
        status, body = http_request("GET", f"{env['admin']}/api/v1/settings/log_retention_days", headers=env["auth"])
        assert status == 200 and str(body["data"]) == fixture["settings"]["log_retention_days"], body
        # 先读升级后的原始时间线：随后的续接请求会给同一 Interaction 追加新 Run。
        assert_observation_converted(env, fixture)
        assert_error_details_survive(env, fixture)
        assert_turn_content_and_markers(env, fixture)
        assert_dependency_resolution(env, fixture)
        assert_mapping_and_hit_detail(env, fixture)
        assert_native_compaction_survives(env, fixture)


@BACKENDS
def test_upgrade_does_not_shorten_dependency_expiry_across_retention_sweep(
    stravia_binary: Path, storage_runtime: dict[str, Any], tmp_path: Path, backend: str,
    fixture: dict[str, Any],
) -> None:
    """续期过的依赖在旧库里还有几天寿命：升级后立即清扫不得提前删掉它们（也不得复活已过期的）。"""
    with upgraded_server(stravia_binary, storage_runtime, tmp_path, backend, fixture) as env:
        status, body = http_request(
            "PUT", f"{env['admin']}/api/v1/settings/log_retention_days",
            payload={"value": fixture["settings"]["log_retention_days"]}, headers=env["auth"],
        )
        assert status == 200, body
        assert_observation_converted(env, fixture)
        assert_error_details_survive(env, fixture)
        assert_dependency_resolution(env, fixture)
        assert_turn_content_and_markers(env, fixture)
        assert_mapping_and_hit_detail(env, fixture)
        assert_native_compaction_survives(env, fixture)
