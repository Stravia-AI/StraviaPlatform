"""Observation 正文引用的读取辅助（ADR-0089）。

列表、变更与详情里的 ``InteractionSummary`` 只携带 ``input`` / ``output`` 的内容引用与
``raw_len``，不再有 ``input_preview`` / ``visible_tail``。测试需要正文时与 WebUI 一样，
经 ``GET /api/v1/observations/contents`` 按引用批量读取。
"""

from __future__ import annotations

import json
from typing import Any

from tests.common.helpers import http_bytes, http_request

# 输入引用以 ``\n`` 连接；可见输出块内的文本单元以 ``\n\n`` 连接。
INPUT_SEPARATOR = "\n"
OUTPUT_SEPARATOR = "\n\n"


def contents_url(env: dict[str, Any], ids: list[int] | str) -> str:
    joined = ids if isinstance(ids, str) else ",".join(str(item) for item in ids)
    return f"{env['admin']}/api/v1/observations/contents?ids={joined}"


def fetch_contents_raw(
    env: dict[str, Any],
    ids: list[int] | str,
    *,
    headers: dict[str, str] | None = None,
) -> tuple[int, dict[str, str], bytes]:
    """原样返回状态、响应头与响应体，用于校验 ETag / 304 / 400。"""
    return http_bytes("GET", contents_url(env, ids), headers={**env["auth"], **(headers or {})})


def fetch_contents(env: dict[str, Any], ids: list[int]) -> dict[int, dict[str, Any]]:
    """批量读取正文，按 ``content_id`` 索引；不可见的 id 不会出现在结果里。"""
    if not ids:
        return {}
    status, body = http_request("GET", contents_url(env, ids), headers=env["auth"])
    assert status == 200, body
    return {item["content_id"]: item for item in body["data"]["contents"]}


def text_of(content: dict[str, Any]) -> str:
    """文本单元取字符串，JSON 单元取紧凑 JSON 文本。"""
    if content["format"] == "text":
        assert isinstance(content["body"], str), content
        return content["body"]
    return json.dumps(content["body"], ensure_ascii=False, separators=(",", ":"))


def ref_ids(part: dict[str, Any]) -> list[int]:
    return [ref["content_id"] for ref in part["refs"]]


def resolve_part(env: dict[str, Any], part: dict[str, Any], separator: str) -> str:
    """把摘要里的 ``{refs, raw_len}`` 展开为完整正文，并校验 ``raw_len`` 等于各引用字节数之和。"""
    ids = ref_ids(part)
    contents = fetch_contents(env, ids)
    assert set(contents) == set(ids), f"summary refs {ids} not all visible: {sorted(contents)}"
    assert part["raw_len"] == sum(contents[item]["raw_len"] for item in ids), part
    for ref in part["refs"]:
        assert contents[ref["content_id"]]["raw_len"] == ref["raw_len"], (ref, contents[ref["content_id"]])
    return separator.join(text_of(contents[item]) for item in ids)


def input_text(env: dict[str, Any], summary: dict[str, Any]) -> str:
    """摘要的完整用户输入；还没有输入引用时为空串。"""
    return resolve_part(env, summary["input"], INPUT_SEPARATOR)


def output_text(env: dict[str, Any], summary: dict[str, Any]) -> str:
    """摘要的完整最新可见输出；还没有可见块时为空串。"""
    return resolve_part(env, summary["output"], OUTPUT_SEPARATOR)


def assert_summary_has_only_refs(summary: dict[str, Any]) -> None:
    """摘要只含引用与长度，旧的内联预览字段已删除。"""
    for removed in ("input_preview", "visible_tail", "truncated"):
        assert removed not in summary, (removed, summary)
    for name in ("input", "output"):
        part = summary[name]
        assert set(part) == {"refs", "raw_len"}, part
        assert isinstance(part["raw_len"], int) and part["raw_len"] >= 0
        for ref in part["refs"]:
            assert set(ref) == {"content_id", "raw_len"}, ref
        if not part["refs"]:
            assert part["raw_len"] == 0


def etag_of(headers: dict[str, str]) -> str:
    etag = headers.get("etag")
    assert etag and etag.startswith('"') and etag.endswith('"'), headers
    assert len(etag) == 66 and all(char in "0123456789abcdef" for char in etag[1:-1]), etag
    return etag


def attempt_finishes(events: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """按事件顺序返回每个 attempt 的结束事实。

    最后一个 attempt 的结束并入 ``model_turn_finished.payload.attempt``，其余 attempt 是
    独立的 ``target_attempt_finished`` 行；两者的 payload 形态一致。
    """
    finishes: list[dict[str, Any]] = []
    for event in events:
        if event["kind"] == "target_attempt_finished":
            finishes.append(event["payload"])
        elif event["kind"] == "model_turn_finished" and event["payload"].get("attempt"):
            finishes.append(event["payload"]["attempt"])
    return finishes


def attempt_starts(events: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """按事件顺序返回每个 attempt 的开始事实：首个并入 ``model_turn_started.payload.attempt``。"""
    starts: list[dict[str, Any]] = []
    for event in events:
        if event["kind"] == "target_attempt_started":
            starts.append(event["payload"])
        elif event["kind"] == "model_turn_started" and event["payload"].get("attempt"):
            starts.append(event["payload"]["attempt"])
    return starts
