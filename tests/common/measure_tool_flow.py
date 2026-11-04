"""ADR-0089 工具调用流实测：统计「6 轮工具调用对话」中每个 Inference Run 的 Observation 写入量。

`measure_protocol_http` 只做单轮文本；本脚本补上多轮工具调用：mock 上游按 Provider 协议
（openai-compatible / google-gemini）回答 5 轮工具调用 + 1 轮最终文本，客户端经
`/v1/responses` 入口用 function_call_output 历史逐轮回放（unary 或 stream）。

每个对话：新建 Provider/Route/API key → 切换 Observation Debug（新 epoch，perf 环形缓冲清零）→
逐轮回放，每轮轮询管理端 interaction 详情直到所有 Run 都有 generation_node_id 且 Trace 完成 →
最终轮结算后抓取一份 `/api/v1/performance/timeline` 快照并保存。

输出 JSON：命令行、二进制 sha256、后端、有效性标志和原始计数；每个对话与每个变体
（协议 × 投递方式）的 Run 数、observation_events 行数（DB 只读 + 详情 API）、写租约/显式
事务/租约内语句数（`analyze_observation_writes.analyze_case`）；全部对话结束后的存储清单。
任何对话未完成 6 轮、快照有 dropped_* 非零、Run 数不等于 6、内容校验不成立都使本次运行作废并返回非零。

不得连接生产服务；PostgreSQL 必须是回环地址上的隔离数据库。
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import re
import sys
import tempfile
import threading
import time
from collections import Counter
from contextlib import ExitStack
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Callable, Iterator
from contextlib import contextmanager
from urllib.parse import urlencode, urlsplit

from tests.common.analyze_observation_writes import analyze_case
from tests.common.helpers import (
    http_bytes,
    http_request,
    initialize_server,
    start_stravia_server,
    stop_stravia_server,
    wait_for_setup_token,
)
from tests.common.measure_protocol_http import Catalog, database_config
from tests.common.protocol_history_audit import (
    NORMALIZED_HISTORY, _connect, _history_tables, _table_columns, history_storage_schema,
)
from tests.common.protocol_storage_inventory import read_storage_inventory

PROTOCOLS = ("openai-compatible", "google-gemini")
DELIVERIES = ("unary", "stream")
ROUNDS = 6  # 与 mock 一致：5 轮工具调用 + 1 轮最终文本，不可配置
# 工具调用总数：openai-compatible 为 2+2+3+2+2，google-gemini 为 2+2+2+2+2。
TOOL_COUNT = {"openai-compatible": 11, "google-gemini": 10}
EXPECTED_INTERACTION_USAGE = {
    "google-gemini": {"input_tokens": 76962, "output_tokens": 2287, "cache_read_tokens": 40676},
    "openai-compatible": {"input_tokens": 30569, "output_tokens": 3699, "cache_read_tokens": 117749},
}
EXPECTED_DELIVERED_INPUT = {"google-gemini": 117638, "openai-compatible": 148318}
EXPECTED_DELIVERED_REASONING = {"google-gemini": 988, "openai-compatible": 1710}
QUESTION = "Read synthetic local values, then report completion."


def require(condition: object, message: str) -> None:
    """显式失败（不依赖 assert，`python -O` 下同样生效）。"""
    if not condition:
        raise RuntimeError(message)


# ---------------------------------------------------------------------------
# mock 上游与回放：复制自 tests/e2e/storage/test_interaction_continuation.py 的
# continuation_provider / replay_round（不得从 tests/e2e 导入）。
# ---------------------------------------------------------------------------

@contextmanager
def continuation_provider(protocol: str) -> Iterator[tuple[str, list[dict[str, Any]]]]:
    """只模拟外部 wire；续接、Marker、usage 和 Observation 均走真实服务。"""
    received: list[dict[str, Any]] = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args: Any) -> None:
            return None

        def do_POST(self) -> None:  # noqa: N802
            request = json.loads(self.rfile.read(int(self.headers["content-length"])))
            received.append(request)
            round_index = min(len(received) - 1, 5)
            call_count = 0 if round_index == 5 else (
                3 if protocol == "openai-compatible" and round_index == 2 else 2
            )
            calls = [
                {"id": f"probe-{round_index}-{index}", "name": "local_probe",
                 "args": {"round": round_index, "index": index}}
                for index in range(call_count)
            ]
            if protocol == "google-gemini":
                prompt = [12484, 18751, 19475, 20909, 22449, 23570][round_index]
                cache = [0, 0, 0, 0, 20339, 20337][round_index]
                candidates = [96, 86, 66, 27, 32, 992][round_index]
                thoughts = [241, 19, 25, 19, 23, 661][round_index]
                parts = [
                    {"functionCall": {"name": call["name"], "args": call["args"]},
                     "thoughtSignature": f"synthetic-protected-{round_index}-{index}"}
                    for index, call in enumerate(calls)
                ] or [
                    {"text": "Synthetic text before protected thought."},
                    {"text": "", "thought": True, "thoughtSignature": "synthetic-protected-final"},
                    {"text": "Synthetic tool work complete."},
                ]
                response = {
                    "candidates": [{"index": 0, "content": {"role": "model", "parts": parts},
                                    "finishReason": "STOP"}],
                    "usageMetadata": {
                        "promptTokenCount": prompt, "cachedContentTokenCount": cache,
                        "candidatesTokenCount": candidates, "thoughtsTokenCount": thoughts,
                        "totalTokenCount": prompt + candidates + thoughts,
                    },
                }
                stream = ":streamGenerateContent" in self.path
                responses = [response]
                if stream and round_index == 5:
                    responses.append({"usageMetadata": response["usageMetadata"]})
                    response = {**response, "usageMetadata": {
                        **response["usageMetadata"], "promptTokenCount": 23243,
                        "totalTokenCount": 24896,
                    }}
                    responses[0] = response
            else:
                prompt = [12766, 20581, 24791, 29417, 30224, 30539][round_index]
                cache = [10000, 16000, 20000, 24000, 24000, 23749][round_index]
                output = [301, 302, 303, 304, 305, 2184][round_index]
                thoughts = [100, 101, 102, 103, 104, 1200][round_index]
                message: dict[str, Any] = {
                    "role": "assistant", "reasoning_content": f"Synthetic public summary {round_index}.",
                    "content": None if calls else "Synthetic tool work complete.",
                }
                if calls:
                    message["tool_calls"] = [
                        {"id": call["id"], "type": "function",
                         "function": {"name": call["name"], "arguments": json.dumps(call["args"])}}
                        for call in calls
                    ]
                usage = {
                    "prompt_tokens": prompt, "completion_tokens": output,
                    "total_tokens": prompt + output,
                    "prompt_tokens_details": {"cached_tokens": cache},
                    "completion_tokens_details": {"reasoning_tokens": thoughts},
                }
                stream = request.get("stream", False)
                if stream:
                    delta = {**message}
                    if calls:
                        delta["tool_calls"] = [
                            {**call, "index": index} for index, call in enumerate(message["tool_calls"])
                        ]
                    responses = [
                        {"id": f"synthetic-{round_index}", "model": request["model"],
                         "object": "chat.completion.chunk",
                         "choices": [{"index": 0, "delta": delta, "finish_reason": None}]},
                        {"id": f"synthetic-{round_index}", "model": request["model"],
                         "object": "chat.completion.chunk",
                         "choices": [{"index": 0, "delta": {},
                                      "finish_reason": "tool_calls" if calls else "stop"}],
                         "usage": usage},
                    ]
                else:
                    responses = [{
                        "id": f"synthetic-{round_index}", "model": request["model"],
                        "object": "chat.completion", "usage": usage,
                        "choices": [{"index": 0, "message": message,
                                     "finish_reason": "tool_calls" if calls else "stop"}],
                    }]
            if stream:
                data = b"".join(
                    b"data: " + json.dumps(response).encode() + b"\n\n" for response in responses
                )
                if protocol == "openai-compatible":
                    data += b"data: [DONE]\n\n"
            else:
                data = json.dumps(responses[-1]).encode()
            self.send_response(200)
            self.send_header("content-type", "text/event-stream" if stream else "application/json")
            self.send_header("content-length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}", received
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


def replay_round(base: str, key: str, model: str, history: list[dict[str, Any]],
                 stream: bool) -> dict[str, Any]:
    """经 /v1/responses 入口回放一轮；history 原地追加本轮完整输出项（去掉 id/status）。"""
    payload = {
        "model": model, "stream": stream, "input": history,
        "tools": [{"type": "function", "name": "local_probe",
                   "description": "Read a synthetic local value.",
                   "parameters": {"type": "object", "properties": {
                       "round": {"type": "integer"}, "index": {"type": "integer"}}}}],
    }
    headers = {"authorization": f"Bearer {key}"}
    if stream:
        status, _, raw = http_bytes("POST", f"{base}/v1/responses", payload=payload, headers=headers)
        require(status == 200, f"replay HTTP {status}: {raw[:500]!r}")
        events = [
            json.loads(line[6:]) for line in raw.decode().splitlines()
            if line.startswith("data: ") and line[6:] != "[DONE]"
        ]
        require(not any(event["type"] in ("response.failed", "error") for event in events), events)
        response = next(event["response"] for event in events if event["type"] == "response.completed")
        delivered_items = [
            event["item"] for event in events if event["type"] == "response.output_item.done"
        ]
        require(delivered_items == response["output"], "native item-done replay changed history")
    else:
        status, response = http_request("POST", f"{base}/v1/responses", payload=payload, headers=headers)
        require(status == 200, f"replay HTTP {status}: {response!r}")
        delivered_items = response["output"]
    history.extend(
        {name: value for name, value in item.items() if name not in ("id", "status")}
        for item in delivered_items
    )
    return response


# ---------------------------------------------------------------------------
# 管理端：Provider/Route/key（同 tests/e2e/admin/test_observations.py::_create_route）与
# interaction 详情轮询。
# ---------------------------------------------------------------------------

def admin_data(session, method: str, path: str, payload: Any = None, *, expected: int = 200,
               timeout: float = 15.0) -> Any:
    status, body = session.request(method, path, payload, timeout=timeout)
    require(status == expected, f"{method} {path}: HTTP {status}, expected {expected}: {body!r}")
    return body["data"] if isinstance(body, dict) and "data" in body else body


def create_route(session, upstream: str, name: str, protocol: str) -> tuple[str, str]:
    provider = admin_data(session, "POST", "/api/v1/providers", {
        "name": f"{name}-provider",
        "source": {"type": "custom", "vendor": "custom", "channel": "default",
                   "protocol": protocol, "base_url": upstream},
        "credential": {"type": "api_key", "value": "upstream-secret"},
        "vendor_options": {},
    })
    admin_data(session, "POST", f"/api/v1/providers/{provider['id']}/models", {
        "model_id": "gpt-4o-mini",
        "metadata": {"name": name, "modalities": {"input": ["text"], "output": ["text"]}},
    }, expected=201)
    route = admin_data(session, "POST", "/api/v1/models", {
        "model_id": name, "output_modality": "text", "display_name": f"{name} display",
        "targets": [{"provider_id": provider["id"], "model": "gpt-4o-mini", "enabled": True,
                     "priority": 0, "target_retry_budget": 0, "target_cooldown_ms": 0}],
    })
    key = admin_data(session, "POST", "/api/v1/api-keys",
                     {"name": f"{name}-key", "model_ids": [route["id"]]})
    return str(route["id"]), str(key["key"])


def wait_for(description: str, probe: Callable[[], Any], *, timeout: float, interval: float) -> Any:
    """按固定间隔轮询 probe 直到真值；超时即抛错，不重试掩盖。"""
    deadline = time.monotonic() + timeout
    last: Any = None
    while True:
        last = probe()
        if last:
            return last
        if time.monotonic() >= deadline:
            raise TimeoutError(f"timed out waiting for {description}; last={last!r}")
        time.sleep(interval)


def route_interactions(session, route_id: str) -> list[dict[str, Any]]:
    query = urlencode({"anchor_at": int(time.time() * 1000) + 1_000, "window_index": 0,
                       "model": route_id, "limit": 100})
    page = admin_data(session, "GET", f"/api/v1/observations/interactions?{query}")
    return [interaction for root in page["roots"] for interaction in root["interactions"]
            if interaction["first_route_id"] == route_id]


def interaction_detail(session, interaction_id: str) -> dict[str, Any]:
    detail = admin_data(session, "GET", f"/api/v1/observations/interactions/{interaction_id}")
    cursor = detail.get("older_events_cursor")
    runs = {run["id"]: run for run in detail["runs"]}
    while cursor is not None:
        query = urlencode({"before_sequence": cursor, "through_sequence": detail["snapshot_sequence"]})
        page = admin_data(session, "GET", f"/api/v1/observations/interactions/{interaction_id}/events?{query}")
        for run in page["runs"]:
            runs[run["id"]]["events"] = run["events"] + runs[run["id"]]["events"]
        cursor = page["next_cursor"]
    return detail


def terminal_sequences(detail: dict[str, Any]) -> dict[str, int]:
    return {run["id"]: max((event["sequence"] for event in run["events"] if event["kind"] == "run_finished"),
                           default=0)
            for run in detail["runs"]}


def finished_detail(session, interaction_id: str) -> dict[str, Any] | None:
    """所有 Run 都已写入 run_finished，且再读一次终态序号不变（同 e2e `_finished_detail`）。"""
    first = terminal_sequences(interaction_detail(session, interaction_id))
    if not first or not all(first.values()):
        return None
    detail = interaction_detail(session, interaction_id)
    return detail if terminal_sequences(detail) == first else None


# ---------------------------------------------------------------------------
# 只读 DB 事件行计数
# ---------------------------------------------------------------------------

def read_event_counts(args, directory: str, run_ids: list[str]) -> dict[str, dict[str, int | None]]:
    """每个 Run 的 observation_events 行数；存在 observation_blocks.run_id 时同时给出块行数。"""
    connection, placeholder, _backend = _connect(args, directory)
    try:
        tables = _history_tables(connection, placeholder)
        require("observation_events" in tables, "observation_events table missing")
        events = {str(run_id): count for run_id, count in connection.execute(
            "SELECT run_id, COUNT(*) FROM observation_events GROUP BY run_id")}
        blocks = None
        if "observation_blocks" in tables and "run_id" in _table_columns(connection, placeholder, "observation_blocks"):
            blocks = {str(run_id): count for run_id, count in connection.execute(
                "SELECT run_id, COUNT(*) FROM observation_blocks GROUP BY run_id")}
        return {run_id: {"events": events.get(run_id, 0),
                         "blocks": None if blocks is None else blocks.get(run_id, 0)}
                for run_id in run_ids}
    finally:
        connection.close()


# ---------------------------------------------------------------------------
# 单个对话
# ---------------------------------------------------------------------------

def user_input_recorded_once(layout: str, detail: dict[str, Any], events: list[dict[str, Any]],
                             kinds: Counter) -> bool:
    """ADR-0089 布局：一条 user_input_recorded，不再有 input_preview_recorded；
    legacy 布局（baseline 二进制）：一条 input_preview_recorded，事件文本即问题原文。"""
    if layout == NORMALIZED_HISTORY:
        return (kinds["user_input_recorded"] == 1 and not kinds["input_preview_recorded"]
                and detail["input_text"] == QUESTION)
    return (kinds["input_preview_recorded"] == 1 and not kinds["user_input_recorded"]
            and any(event["payload"].get("text") == QUESTION
                    for event in events if event["kind"] == "input_preview_recorded"))


def client_tool_results_match(layout: str, detail: dict[str, Any], result_rows: list[dict[str, Any]],
                              tool_count: int) -> bool:
    """ADR-0089 布局：客户端工具结果按批一行（每个 Run 至多一行，逐项结果合计等于工具调用数）；
    legacy 布局：每个工具结果一行（payload.content），行数等于工具调用数。"""
    if layout == NORMALIZED_HISTORY:
        return (len(result_rows) <= len(detail["runs"])
                and sum(len(event["payload"]["results"]) for event in result_rows) == tool_count)
    return len(result_rows) == tool_count and all("content" in event["payload"] for event in result_rows)


def detect_layout(args, directory: str) -> str:
    connection, placeholder, _backend = _connect(args, directory)
    try:
        return history_storage_schema(_history_tables(connection, placeholder))
    finally:
        connection.close()


def run_conversation(args, session, base: str, directory: str, layout: str, protocol: str,
                     delivery: str, index: int) -> dict[str, Any]:
    stream = delivery == "stream"
    case_id = f"{protocol}--{delivery}--{index}"
    model = f"toolflow-{protocol}-{delivery}-{index}"
    with continuation_provider(protocol) as (upstream, received):
        route, key = create_route(session, upstream, model, protocol)
        # 先关后开：新 Debug epoch 清空 perf 环形缓冲，路由创建等准备工作不进入快照。
        for enabled in (False, True):
            admin_data(session, "PUT", "/api/v1/observations/debug", {"enabled": enabled, "confirmed": True})
        history: list[dict[str, Any]] = [{"role": "user", "content": QUESTION}]
        previous_node: str | None = None
        previous_run: str | None = None
        detail: dict[str, Any] = {}
        delivered_usage: list[dict[str, Any]] = []
        ancestry_failures: list[str] = []
        rounds_completed = 0
        for round_index in range(ROUNDS):
            response = replay_round(base, key, model, history, stream)
            delivered_usage.append(response["usage"])
            calls = [item for item in response["output"] if item["type"] == "function_call"]
            history.extend({"type": "function_call_output", "call_id": call["call_id"],
                            "output": f"synthetic result {round_index}-{index_}"}
                           for index_, call in enumerate(calls))

            def persisted() -> dict[str, Any] | None:
                interactions = route_interactions(session, route)
                if len(interactions) != 1:
                    return None
                current = interaction_detail(session, interactions[0]["id"])
                if len(current["runs"]) != round_index + 1:
                    return None
                if not all(run["generation_node_id"] and (run.get("trace") or {}).get("status") == "complete"
                           for run in current["runs"]):
                    return None
                return current

            detail = wait_for(f"{case_id} round {round_index} persisted", persisted,
                              timeout=args.settle_timeout, interval=args.poll_interval)
            current_run = detail["runs"][-1]
            if current_run["generation_parent_id"] != previous_node:
                ancestry_failures.append(f"round {round_index}: generation_parent_id")
            if current_run["parent_run_id"] != previous_run:
                ancestry_failures.append(f"round {round_index}: parent_run_id")
            if not all(run["status"] == "superseded" and not run["user_interrupted"]
                       for run in detail["runs"][:-1]):
                ancestry_failures.append(f"round {round_index}: earlier Runs not superseded")
            previous_node, previous_run = current_run["generation_node_id"], current_run["id"]
            if not (calls or round_index == ROUNDS - 1):
                ancestry_failures.append(f"round {round_index}: no tool calls before the final round")
            rounds_completed += 1
        # 最后一轮的终态与异步收尾也必须落盘后再抓快照，否则会漏计 Observation 写入。
        interaction_id = detail["interaction"]["id"]
        detail = wait_for(f"{case_id} terminal events", lambda: finished_detail(session, interaction_id),
                          timeout=args.settle_timeout, interval=args.poll_interval)
        snapshot = admin_data(session, "GET", "/api/v1/performance/timeline", timeout=args.audit_timeout)
        args.snapshots_dir.mkdir(parents=True, exist_ok=True)
        snapshot_path = args.snapshots_dir / f"{case_id}.json"
        snapshot_path.write_text(json.dumps(snapshot, ensure_ascii=False), encoding="utf-8")
        received_requests = len(received)

    events = [event for run in detail["runs"] for event in run["events"]]
    kinds = Counter(event["kind"] for event in events)
    result_rows = [event for event in events if event["kind"] == "client_tool_result"]
    usage = detail["interaction"]["usage"]
    expected_usage = EXPECTED_INTERACTION_USAGE[protocol]
    checks = {
        "rounds_completed": rounds_completed == ROUNDS,
        "upstream_received_6_requests": received_requests == ROUNDS,
        "run_count_6": len(detail["runs"]) == ROUNDS,
        "ancestry_chain": not ancestry_failures,
        "user_input_recorded_once": user_input_recorded_once(layout, detail, events, kinds),
        "client_tool_handoff_count": kinds["client_tool_handoff"] == TOOL_COUNT[protocol],
        "client_tool_results": client_tool_results_match(layout, detail, result_rows, TOOL_COUNT[protocol]),
        "interaction_usage": all(usage[field] == value for field, value in expected_usage.items()),
        "delivered_usage": (
            sum(item["input_tokens"] for item in delivered_usage) == EXPECTED_DELIVERED_INPUT[protocol]
            and sum(item["output_tokens"] for item in delivered_usage) == expected_usage["output_tokens"]
            and sum(item["output_tokens_details"]["reasoning_tokens"] for item in delivered_usage)
            == EXPECTED_DELIVERED_REASONING[protocol]
        ),
        "attempt_count_6": usage["coverage"]["attempt_count"] == ROUNDS,
    }
    if protocol == "google-gemini":
        checks["final_run_input_tokens"] = detail["runs"][-1]["usage"]["input_tokens"] == 3233

    analysis = analyze_case(snapshot)
    run_ids = [run["id"] for run in detail["runs"]]
    db_counts = read_event_counts(args, directory, run_ids)
    reasons = [f"check failed: {name}" for name, ok in checks.items() if not ok]
    reasons += ancestry_failures
    if not analysis["valid"]:
        reasons.append(f"snapshot invalid: dropped={analysis['dropped']} runs={analysis['runs']}")
    if analysis["runs"] != ROUNDS:
        reasons.append(f"snapshot Run spans {analysis['runs']} != {ROUNDS}")
    return {
        "id": case_id, "protocol": protocol, "delivery": delivery, "index": index,
        "route_id": route, "interaction_id": interaction_id, "run_ids": run_ids,
        "snapshot": str(snapshot_path), "upstream_requests": received_requests,
        "tool_calls": TOOL_COUNT[protocol], "checks": checks, "analysis": analysis,
        "api_event_kinds": dict(kinds),
        "api_events_per_run": [len(run["events"]) for run in detail["runs"]],
        "db_events_per_run": [db_counts[run_id]["events"] for run_id in run_ids],
        "db_blocks_per_run": [db_counts[run_id]["blocks"] for run_id in run_ids],
        "valid": not reasons, "failure_reasons": reasons,
    }


def observation_totals(analysis: dict[str, Any]) -> dict[str, int]:
    """把按 writer 动作分组的计数折成「Observation / 其余」两类总数。"""
    def split(counts: dict[str, int]) -> tuple[int, int]:
        return (sum(v for k, v in counts.items() if k != "other"), counts.get("other", 0))
    leases = split(analysis["write_leases"])
    transactions = split(analysis["explicit_transactions"])
    statements = split(analysis["statements_inside_lease"])
    return {"observation_leases": leases[0], "other_leases": leases[1],
            "observation_transactions": transactions[0], "other_transactions": transactions[1],
            "observation_statements": statements[0], "other_statements": statements[1]}


def summarize(conversations: list[dict[str, Any]]) -> dict[str, Any]:
    totals: Counter = Counter()
    for conversation in conversations:
        totals["conversations"] += 1
        totals["valid_conversations"] += conversation["valid"]
        totals["runs"] += len(conversation["run_ids"])
        totals["db_events"] += sum(conversation["db_events_per_run"])
        totals["api_events"] += sum(conversation["api_events_per_run"])
        totals.update(observation_totals(conversation["analysis"]))
    runs = totals["runs"]
    per_run = {key: round(value / runs, 3) for key, value in totals.items()
               if key not in ("conversations", "valid_conversations", "runs")} if runs else {}
    return {"totals": dict(totals), "per_run": per_run}


# ---------------------------------------------------------------------------
# 入口
# ---------------------------------------------------------------------------

def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--label", required=True)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--conversations", type=int, default=4,
                        help="每个（协议 × 投递方式）变体的对话数；每个对话固定 6 轮（Run）")
    parser.add_argument("--protocols", nargs="+", choices=PROTOCOLS, default=list(PROTOCOLS))
    parser.add_argument("--deliveries", nargs="+", choices=DELIVERIES, default=list(DELIVERIES))
    parser.add_argument("--backend", choices=("sqlite", "postgres"), default="sqlite")
    parser.add_argument("--database-url")
    parser.add_argument("--confirm-isolated-database", action="store_true")
    parser.add_argument("--redis-url")
    parser.add_argument("--data-dir", type=Path,
                        help="保留 SQLite/服务数据目录于此（必须尚不存在），否则用临时目录")
    parser.add_argument("--snapshots-dir", type=Path,
                        help="保存每个对话的 /api/v1/performance/timeline 快照（默认 <output>.snapshots）")
    parser.add_argument("--poll-interval", type=float, default=0.1, help="等待持久化的轮询间隔（秒）")
    parser.add_argument("--settle-timeout", type=float, default=30, help="单个等待点的截止时间（秒）")
    parser.add_argument("--audit-timeout", type=float, default=60, help="快照请求超时（秒）")
    parser.add_argument("--startup-timeout", type=float, default=180)
    parser.add_argument("--source-commit", default="unspecified")
    parser.add_argument("--build-condition", default="unspecified")
    args = parser.parse_args()
    if args.conversations < 1:
        parser.error("--conversations must be at least 1")
    for name in ("poll_interval", "settle_timeout", "audit_timeout", "startup_timeout"):
        if not math.isfinite(getattr(args, name)) or getattr(args, name) <= 0:
            parser.error(f"--{name.replace('_', '-')} must be positive and finite")
    if args.backend == "sqlite" and (args.database_url or args.redis_url):
        parser.error("--database-url/--redis-url require --backend postgres")
    if args.redis_url and urlsplit(args.redis_url).hostname not in ("127.0.0.1", "localhost", "::1"):
        parser.error("Redis must be a dedicated loopback instance")
    if args.snapshots_dir is None:
        args.snapshots_dir = args.output.with_name(args.output.name + ".snapshots")
    return args


def redact_urls(text: str, args: argparse.Namespace) -> str:
    text = re.sub(r"(Stravia setup token:)\s*\S+", r"\1 [redacted]", text)
    for url in (args.database_url, args.redis_url):
        if url:
            text = text.replace(url, "[connection URL omitted]")
    return text


def main() -> int:
    args = parse_args()
    with args.binary.resolve().open("rb") as source:
        sha = hashlib.file_digest(source, "sha256").hexdigest()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    result: dict[str, Any] = {
        "schema_version": 1, "kind": "tool-flow", "label": args.label, "timestamp": time.time(),
        "command_line": [redact_urls(part, args) for part in sys.argv],
        "binary": str(args.binary), "binary_sha256": sha, "backend": args.backend,
        "source_commit": args.source_commit, "build_condition": args.build_condition,
        "rounds_per_conversation": ROUNDS, "conversations_per_variant": args.conversations,
        "credential_protection": True, "snapshots_dir": str(args.snapshots_dir),
        "variants": {}, "inventory_after_variant": {}, "storage_inventory": None,
        "fatal_error": None, "valid": False,
    }
    catalog = ThreadingHTTPServer(("127.0.0.1", 0), Catalog)
    catalog.arrivals = []
    threading.Thread(target=catalog.serve_forever, daemon=True).start()
    process = None
    logs: list[str] = []
    try:
        database = database_config(args)
        with ExitStack() as cleanup:
            if args.data_dir:
                args.data_dir.mkdir(parents=True, exist_ok=False)
                directory = str(args.data_dir.resolve())
            else:
                directory = cleanup.enter_context(tempfile.TemporaryDirectory(prefix="stravia-toolflow-"))
            cleanup.callback(lambda: stop_stravia_server(process, logs, print_tail=0) if process else None)
            command = ["--host", "127.0.0.1", "--port", "0", "--data-dir", directory,
                       "--test-catalog-base-url", f"http://127.0.0.1:{catalog.server_port}"]
            if args.redis_url:
                config_path = Path(directory) / "toolflow.toml"
                config_path.write_text("[cache]\nredis_url = " + json.dumps(args.redis_url) + "\n", encoding="utf-8")
                command.extend(["--config", str(config_path)])
            process, logs = start_stravia_server(
                stravia_binary=args.binary.resolve(), args=command,
                env={"RUST_LOG": "warn,stravia_server=info"})
            token = wait_for_setup_token(logs, process, timeout=args.startup_timeout)
            match = next((m for line in logs if (m := re.search(r"Stravia startup listener opened .*?(127\.0\.0\.1:\d+)", line))), None)
            require(match, "server did not log its startup listener")
            base = "http://" + match.group(1)
            session = initialize_server(base, token, database)
            admin_data(session, "PUT", "/api/v1/settings/reversible_redaction_enabled", {"value": "true"}, expected=200)
            require(admin_data(session, "GET", "/api/v1/settings/reversible_redaction_enabled") == "true",
                    "credential protection was not enabled")
            require(not admin_data(session, "GET", "/api/v1/observations/debug")["enabled"],
                    "isolated measurement expected a fresh Observation Debug session")
            layout = detect_layout(args, directory)
            result["history_layout"] = layout
            for protocol in args.protocols:
                for delivery in args.deliveries:
                    variant = result["variants"].setdefault(
                        f"{protocol}--{delivery}", {"protocol": protocol, "delivery": delivery,
                                                    "conversations": []})
                    for index in range(args.conversations):
                        variant["conversations"].append(
                            run_conversation(args, session, base, directory, layout, protocol, delivery, index))
                        # 逐对话落盘检查点，失败时保留已完成的原始证据。
                        args.output.write_text(json.dumps(result, ensure_ascii=False, indent=2), encoding="utf-8")
                    variant["summary"] = summarize(variant["conversations"])
                    variant["valid"] = all(c["valid"] for c in variant["conversations"])
                    result["inventory_after_variant"][f"{protocol}--{delivery}"] = read_storage_inventory(args, directory)
            result["storage_inventory"] = read_storage_inventory(args, directory)
            if args.backend == "sqlite":
                database_path = Path(directory) / "db" / "gateway.db"
                result["sqlite_files"] = {
                    name: (path.stat().st_size if path.exists() else 0)
                    for name, path in (("database", database_path),
                                       ("wal", database_path.with_name(database_path.name + "-wal")),
                                       ("shm", database_path.with_name(database_path.name + "-shm")))}
            stop_stravia_server(process, logs, print_tail=0)
            process = None
    except Exception as error:
        result["fatal_error"] = redact_urls(f"{type(error).__name__}: {error}", args)
    finally:
        if process:
            stop_stravia_server(process, logs, print_tail=0)
        catalog.shutdown()
        catalog.server_close()
        conversations = [c for v in result["variants"].values() for c in v["conversations"]]
        result["summary"] = summarize(conversations) if conversations else None
        result["valid"] = (not result["fatal_error"] and bool(conversations)
                           and len(conversations) == args.conversations * len(args.protocols) * len(args.deliveries)
                           and all(c["valid"] for c in conversations))
        if not result["valid"]:
            result["server_log_tail"] = [redact_urls(line, args) for line in logs[-60:]]
        args.output.write_text(json.dumps(result, ensure_ascii=False, indent=2), encoding="utf-8")
    for name, variant in result["variants"].items():
        print(name, json.dumps(variant.get("summary", {}).get("per_run", {}), sort_keys=True))
    for conversation in (c for v in result["variants"].values() for c in v["conversations"] if not c["valid"]):
        print("INVALID", conversation["id"], conversation["failure_reasons"])
    print(json.dumps({"output": str(args.output), "valid": result["valid"], "fatal_error": result["fatal_error"],
                      "conversations": len(conversations)}))
    return 0 if result["valid"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
