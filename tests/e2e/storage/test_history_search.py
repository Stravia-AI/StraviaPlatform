"""历史检索 HTTP E2E（SQLite 与 PostgreSQL）：真实 Server、真实后台索引、loopback 假上游。

只通过代理请求与假上游收到的工具结果观察行为：多轮对话检索后按 ``path:L-M`` 回读、链路范围、
发起检索的 Interaction 不出现在结果中、跨 Key 隔离、清除历史记录，以及升级前已有历史的后台回填。
"""

from __future__ import annotations

import json
import re
import sqlite3
import threading
import uuid
from contextlib import closing, contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Iterator

import pytest

from tests.common.helpers import (
    find_free_port,
    http_request,
    initialize_server,
    start_stravia_server,
    stop_stravia_server,
    wait_for_setup_token,
    wait_until_ready,
)
from tests.e2e.admin.test_observations import (
    _create_route,
    _finished_detail,
    _proxy,
    _route_interactions,
    _wait_for,
)

pytestmark = [pytest.mark.e2e, pytest.mark.storage]
BACKENDS = pytest.mark.parametrize("backend", ["sqlite", "postgres"], ids=["sqlite", "postgres"])

ROOT = "stravia://history/"
ANSWER = "The earlier conversation has been answered."
NO_HITS = "No hits does not mean"
# 检索结果中每轮一行、每个命中一行（设计文档 §5.6）。
INTERACTION_LINE = re.compile(r"^\d+\. (" + re.escape(ROOT) + r"[A-Za-z0-9_-]+/[A-Za-z0-9_-]+) · ", re.M)
HIT_LINE = re.compile(r"^  (" + re.escape(ROOT) + r"\S+:\d+-\d+) · (user|assistant|tool call) · (.*)$", re.M)
LOOKUP_TOOL = {
    "type": "function",
    "function": {"name": "lookup", "description": "Look up a record.",
                 "parameters": {"type": "object", "properties": {}}},
}


class SearchUpstream(ThreadingHTTPServer):
    """Chat Completions 假上游：按首条用户输入选择脚本，依次发出工具调用，最后回答。

    脚本步骤为 ``{"read": <StraviaRead 参数>}`` 或 ``{"client": <工具名>}``；读取参数中的
    ``path == "{hit}"`` 取上一个检索结果里的第一个命中路径。每次最终回答时记录该轮请求中的全部工具结果。
    """

    def __init__(self) -> None:
        super().__init__(("127.0.0.1", 0), _SearchHandler)
        self.lock = threading.Lock()
        self.scripts: dict[str, list[dict[str, Any]]] = {}
        self.answers: dict[str, str] = {}
        self.results: dict[str, list[str]] = {}

    @property
    def base_url(self) -> str:
        host, port = self.server_address
        return f"http://{host}:{port}"

    def script(self, prompt: str, steps: list[dict[str, Any]], answer: str = ANSWER) -> None:
        with self.lock:
            self.scripts[prompt] = steps
            self.answers[prompt] = answer

    def respond(self, body: dict[str, Any]) -> tuple[dict[str, Any], str]:
        messages = body.get("messages", [])
        prompt = next((m.get("content", "") for m in messages if m.get("role") == "user"), "")
        results = [str(m.get("content", "")) for m in messages if m.get("role") == "tool"]
        with self.lock:
            steps = self.scripts.get(prompt, [])
            answer = self.answers.get(prompt, ANSWER)
            if len(results) >= len(steps):
                self.results[prompt] = results
        if len(results) >= len(steps):
            return {"role": "assistant", "content": answer}, "stop"
        step = steps[len(results)]
        if "client" in step:
            name, arguments = step["client"], {}
        else:
            name, arguments = "StraviaRead", dict(step["read"])
            if arguments.get("path") == "{hit}":
                arguments["path"] = hit_paths(json.loads(results[-1]))[0]
        call = {"id": f"call_{name}_{len(results)}", "type": "function",
                "function": {"name": name, "arguments": json.dumps(arguments)}}
        return {"role": "assistant", "content": None, "tool_calls": [call]}, "tool_calls"


class _SearchHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args: Any) -> None:
        return

    def do_POST(self) -> None:  # noqa: N802
        length = int(self.headers.get("content-length", "0"))
        body = json.loads(self.rfile.read(length) or b"{}")
        if self.path.split("?", 1)[0] != "/v1/chat/completions":
            self._send(404, "application/json", json.dumps({"error": "not found"}).encode())
            return
        server = self.server
        assert isinstance(server, SearchUpstream)
        message, finish = server.respond(body)
        model = str(body.get("model", "mock"))
        usage = {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
        if body.get("stream"):
            delta = dict(message)
            if "tool_calls" in delta:
                delta["tool_calls"] = [{"index": 0, **delta["tool_calls"][0]}]
            chunks = [
                {"id": "chatcmpl-search", "object": "chat.completion.chunk", "model": model,
                 "choices": [{"index": 0, "delta": delta, "finish_reason": None}]},
                {"id": "chatcmpl-search", "object": "chat.completion.chunk", "model": model,
                 "choices": [{"index": 0, "delta": {}, "finish_reason": finish}], "usage": usage},
            ]
            payload = b"".join(f"data: {json.dumps(chunk)}\n\n".encode() for chunk in chunks)
            self._send(200, "text/event-stream", payload + b"data: [DONE]\n\n")
        else:
            self._send(200, "application/json", json.dumps({
                "id": "chatcmpl-search", "object": "chat.completion", "model": model,
                "choices": [{"index": 0, "message": message, "finish_reason": finish}], "usage": usage,
            }).encode())

    def _send(self, status: int, content_type: str, payload: bytes) -> None:
        self.send_response(status)
        self.send_header("content-type", content_type)
        self.send_header("content-length", str(len(payload)))
        self.send_header("connection", "close")
        self.end_headers()
        self.wfile.write(payload)
        self.wfile.flush()


@contextmanager
def search_upstream() -> Iterator[SearchUpstream]:
    upstream = SearchUpstream()
    thread = threading.Thread(target=upstream.serve_forever, daemon=True)
    thread.start()
    try:
        yield upstream
    finally:
        upstream.shutdown()
        upstream.server_close()
        thread.join(timeout=5)


class HistoryServer:
    """一套隔离 Server；PostgreSQL 使用独立 schema 与隔离 Redis。支持停机后在同一份数据上重启。"""

    def __init__(self, stravia_binary: Path, storage_runtime: dict[str, Any], tmp_path: Path, backend: str) -> None:
        self.binary = stravia_binary
        self.backend = backend
        self.data_dir = tmp_path
        self.base = f"http://127.0.0.1:{find_free_port()}"
        self.args = ["--data-dir", str(tmp_path), "--host", "127.0.0.1",
                     "--port", self.base.rsplit(":", 1)[1], *storage_runtime["server_args"](backend)]
        self.process: Any = None
        self.logs: list[str] = []
        self.dsn: str | None = None
        if backend == "postgres":
            pg_url = storage_runtime["pg_url"]
            if not pg_url:
                pytest.fail("PostgreSQL history search E2E requires isolated DB_URL", pytrace=False)
            self.schema = storage_runtime["make_isolated_schema"]("stravia_history_search")
            storage_runtime["run_schema_action"]("create", pg_url=pg_url, schema=self.schema)
            self.dsn = storage_runtime["postgres_dsn_for_schema"](pg_url, self.schema)

    def start(self) -> None:
        self.process, self.logs = start_stravia_server(stravia_binary=self.binary, args=self.args)
        wait_until_ready(f"{self.base}/api/v1/auth/state")

    def stop(self) -> None:
        if self.process is not None:
            stop_stravia_server(self.process, self.logs)
            self.process = None


@contextmanager
def history_server(
    stravia_binary: Path, storage_runtime: dict[str, Any], tmp_path: Path, backend: str, upstream: SearchUpstream,
) -> Iterator[tuple[HistoryServer, dict[str, Any]]]:
    server = HistoryServer(stravia_binary, storage_runtime, tmp_path, backend)
    try:
        server.start()
        database = {"backend": "postgres", "url": server.dsn} if server.dsn else {"backend": "sqlite"}
        session = initialize_server(server.base, wait_for_setup_token(server.logs, server.process), database)
        yield server, {"admin": server.base, "proxy": server.base, "auth": session.auth_headers(),
                       "mock": upstream.base_url}
    finally:
        server.stop()
        if server.dsn is not None:
            storage_runtime["run_schema_action"]("drop", pg_url=storage_runtime["pg_url"], schema=server.schema)


def hit_paths(page: dict[str, Any]) -> list[str]:
    return [match.group(1) for match in HIT_LINE.finditer(page["content"])]


def interaction_paths(page: dict[str, Any]) -> list[str]:
    return INTERACTION_LINE.findall(page["content"])


def history_key(env: dict[str, Any], route: str, name: str) -> str:
    status, body = http_request("POST", f"{env['admin']}/api/v1/api-keys", payload={
        "name": name, "model_ids": [route], "transparent_injection_enabled": True,
        "transparent_injections": ["history_recall"],
    }, headers=env["auth"])
    assert status == 200, body
    return body["data"]["key"]


def turn(
    upstream: SearchUpstream, env: dict[str, Any], key: str, model: str, messages: list[dict[str, Any]],
    steps: list[dict[str, Any]] | None = None, *, answer: str = ANSWER, tools: list[dict[str, Any]] | None = None,
) -> tuple[dict[str, Any], list[str]]:
    """发送一轮请求；返回客户端收到的消息与假上游在最终回答前收到的工具结果。"""
    prompt = next(m["content"] for m in messages if m["role"] == "user")
    upstream.script(prompt, steps or [], answer)
    status, response = _proxy(env, key, model, messages, body_extra={"tools": tools} if tools else None)
    assert status == 200, response
    return response["choices"][0]["message"], upstream.results.get(prompt, [])


def search(
    upstream: SearchUpstream, env: dict[str, Any], key: str, model: str, question: str, path: str = ROOT,
    *, read_hit: bool = False,
) -> tuple[dict[str, Any], dict[str, Any] | None]:
    """用一个不含检索词的新 Interaction 检索，返回检索页与（可选）命中行的读取结果。"""
    steps: list[dict[str, Any]] = [{"read": {"path": path, "question": question}}]
    if read_hit:
        steps.append({"read": {"path": "{hit}"}})
    message, results = turn(upstream, env, key, model, [{"role": "user", "content": f"probe {uuid.uuid4().hex}"}], steps)
    assert message["content"] == ANSWER, message
    assert len(results) == len(steps), results
    page = json.loads(results[0])
    assert page["representation"] == "markdown" and page["read_path"] == path, page
    return page, json.loads(results[1]) if read_hit else None


def wait_for_hits(
    upstream: SearchUpstream, env: dict[str, Any], key: str, model: str, question: str, count: int, path: str = ROOT,
) -> dict[str, Any]:
    """后台索引异步追赶：检索到预期轮数为止。"""
    def probe() -> dict[str, Any] | None:
        page, _ = search(upstream, env, key, model, question, path)
        return page if len(interaction_paths(page)) == count else None
    return _wait_for(f"indexed history for {question!r}", probe, timeout=30.0)


def completed(env: dict[str, Any], route: str, count: int) -> list[dict[str, Any]]:
    def probe() -> list[dict[str, Any]] | None:
        items = _route_interactions(env, route)
        done = [item for item in items
                if (detail := _finished_detail(env, item["id"])) and detail["interaction"]["status"] == "completed"]
        return done if len(done) >= count else None
    return _wait_for("persisted history interactions", probe, timeout=20.0)


@BACKENDS
def test_history_search_reads_hits_excludes_issuer_isolates_keys_and_clears(
    stravia_binary: Path, storage_runtime: dict[str, Any], tmp_path: Path, backend: str,
) -> None:
    tag = uuid.uuid4().hex[:12]
    model = f"history-search-{tag}"
    zebra, cobalt, needle = f"zebra{tag}", f"cobalt{tag}", f"needle{tag}"
    with search_upstream() as upstream, history_server(stravia_binary, storage_runtime, tmp_path, backend, upstream) as (_, env):
        route, _ = _create_route(env, model)
        owner = history_key(env, route, f"owner-{tag}")
        other = history_key(env, route, f"other-{tag}")

        # 两轮对话在归并窗口内续写，归入同一个 Interaction（ADR-0053），各轮输入都可检索。
        first = [{"role": "user", "content": f"PG 迁移 {zebra} 升级钩子失败"}]
        reply, _ = turn(upstream, env, owner, model, first, answer="Split the backfill into batches.")
        second = [*first, reply, {"role": "user", "content": f"{cobalt} 仍然超时"}]
        turn(upstream, env, owner, model, second)
        completed(env, route, 1)

        page = wait_for_hits(upstream, env, owner, model, f"迁移 {zebra}", 1)
        assert any("Retention" in item for item in page["limitations"]), page
        page, selected = search(upstream, env, owner, model, f"迁移 {zebra}", read_hit=True)
        [transcript] = interaction_paths(page)
        [hit] = HIT_LINE.findall(page["content"])
        assert hit[0].startswith(transcript + ":") and hit[1] == "user", page
        assert "«迁移»" in hit[2] and f"«{zebra}»" in hit[2], page
        assert selected is not None and f"PG 迁移 {zebra} 升级钩子失败" in selected["content"], selected
        first_line = int(hit[0].rsplit(":", 1)[1].split("-")[0])

        chain = transcript.rsplit("/", 1)[0] + "/"
        wait_for_hits(upstream, env, owner, model, cobalt, 1, chain)
        page, selected = search(upstream, env, owner, model, cobalt, chain, read_hit=True)
        assert interaction_paths(page) == [transcript], page
        [later] = HIT_LINE.findall(page["content"])
        assert later[1] == "user" and int(later[0].rsplit(":", 1)[1].split("-")[0]) > first_line, page
        assert selected is not None and f"{cobalt} 仍然超时" in selected["content"], selected

        # 同一个 Interaction 拆成两个 Run：等它被别的轮检索到（已建索引），再在它的续接 Run 中检索。
        issuer = [{"role": "user", "content": f"look up {needle}"}]
        handoff, _ = turn(upstream, env, owner, model, issuer, [{"client": "lookup"}], tools=[LOOKUP_TOOL])
        assert handoff["tool_calls"][0]["function"]["name"] == "lookup", handoff
        [issuer_path] = interaction_paths(wait_for_hits(upstream, env, owner, model, needle, 1))
        resumed = [*issuer, handoff, {"role": "tool", "tool_call_id": handoff["tool_calls"][0]["id"], "content": "record"}]
        message, results = turn(upstream, env, owner, model, resumed,
                                [{"client": "lookup"}, {"read": {"path": ROOT, "question": needle}}], tools=[LOOKUP_TOOL])
        assert message["content"] == ANSWER, message
        own = json.loads(results[1])
        assert issuer_path not in own["content"] and not interaction_paths(own), own
        assert any(NO_HITS in item for item in own["limitations"]), own
        # 续接没有新建 Interaction：之后检索仍只命中原来那一轮。
        page, _ = search(upstream, env, owner, model, needle)
        assert interaction_paths(page) == [issuer_path], page

        foreign, _ = search(upstream, env, other, model, f"迁移 {zebra}")
        assert not interaction_paths(foreign) and not hit_paths(foreign), foreign
        assert any(NO_HITS in item for item in foreign["limitations"]), foreign

        status, body = http_request("DELETE", f"{env['admin']}/api/v1/observations/history", headers=env["auth"])
        assert status == 200, body
        cleared, _ = search(upstream, env, owner, model, f"迁移 {zebra}")
        assert not interaction_paths(cleared), cleared
        assert any(NO_HITS in item for item in cleared["limitations"]), cleared


def downgrade_to_pre_search_schema(server: HistoryServer) -> None:
    """0051 只含 DDL：删除它创建的对象与迁移记录，即得到升级前（≤0050）的真实库形态与已有历史。"""
    if server.dsn is not None:
        import psycopg

        with psycopg.connect(server.dsn) as connection:
            connection.execute("DROP TABLE history_search_segments, history_search_progress")
            connection.execute("DELETE FROM _sqlx_migrations WHERE version = 51")
            [(latest,)] = connection.execute("SELECT MAX(version) FROM _sqlx_migrations").fetchall()
    else:
        with closing(sqlite3.connect(server.data_dir / "db" / "gateway.db")) as connection, connection:
            connection.executescript(
                "DROP TABLE history_search_segments_fts; DROP TABLE history_search_segments;"
                " DROP TABLE history_search_progress; DELETE FROM _sqlx_migrations WHERE version = 51;"
            )
            [(latest,)] = connection.execute("SELECT MAX(version) FROM _sqlx_migrations").fetchall()
    assert latest == 50, latest


@BACKENDS
def test_history_search_backfills_history_recorded_before_upgrade(
    stravia_binary: Path, storage_runtime: dict[str, Any], tmp_path: Path, backend: str,
) -> None:
    tag = uuid.uuid4().hex[:12]
    model = f"history-backfill-{tag}"
    phrase = f"backfill{tag}"
    with search_upstream() as upstream, history_server(stravia_binary, storage_runtime, tmp_path, backend, upstream) as (server, env):
        route, _ = _create_route(env, model)
        key = history_key(env, route, f"backfill-{tag}")
        turn(upstream, env, key, model, [{"role": "user", "content": f"remember {phrase}"}])
        completed(env, route, 1)
        server.stop()
        downgrade_to_pre_search_schema(server)

        server.start()
        page = wait_for_hits(upstream, env, key, model, phrase, 1)
        page, selected = search(upstream, env, key, model, phrase, read_hit=True)
        assert hit_paths(page), page
        assert selected is not None and f"remember {phrase}" in selected["content"], selected
