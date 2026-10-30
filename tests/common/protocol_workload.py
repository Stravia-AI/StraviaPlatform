"""Deterministic, token-exact workloads for real HTTP protocol benchmarks."""
from __future__ import annotations

import base64
import hashlib
import json
import random
import re
import struct
import zlib
from dataclasses import dataclass, replace
from functools import lru_cache
from typing import Iterable

import tiktoken

PROTOCOLS = ("openai-chat", "open-responses", "anthropic-messages", "google-content")


@lru_cache(maxsize=1)
def _vocabulary() -> tuple[str, ...]:
    """Prepare diverse, independently roundtripping ASCII word tokens once."""
    encoding = tiktoken.get_encoding("cl100k_base")
    words = []
    for token in range(encoding.n_vocab):
        try:
            raw = encoding.decode_single_token_bytes(token)
        except KeyError:
            continue
        if (raw.startswith(b" ") and 4 <= len(raw) <= 17
                and raw[1:].isascii() and raw[1:].isalpha()):
            word = raw.decode("ascii")
            if encoding.encode(word, disallowed_special=()) == [token]:
                words.append(word)
    if len(words) < 1000:
        raise ValueError("cl100k_base lacks the required diverse single-token vocabulary")
    return tuple(words)


@dataclass(frozen=True)
class Image:
    data: bytes
    width: int
    height: int
    sha256: str
    media_type: str = "image/png"

    @property
    def base64(self) -> str:
        return base64.b64encode(self.data).decode("ascii")


@dataclass(frozen=True)
class Message:
    role: str
    text: str
    images: tuple[Image, ...] = ()


class HistoryBudgetExceeded(ValueError):
    """The caller must reset the conversation; history is never truncated."""


@dataclass(frozen=True)
class Plan:
    nonce: str
    messages: tuple[Message, ...]
    new_user: Message
    input_tokens: int
    image_count: int
    first_token_delay: float
    tokens_per_second: int
    duration: int
    output_text: str
    output_fragments: tuple[str, ...]
    reserve_tokens: int = 0
    credential_secrets: tuple[str, ...] = ()
    new_credential_secrets: tuple[str, ...] = ()

    @property
    def output_tokens(self) -> int:
        return len(self.output_fragments)

    def metadata(self) -> dict:
        output_sha256 = hashlib.sha256(self.output_text.encode()).hexdigest()
        return {"nonce": self.nonce, "input_tokens": self.input_tokens,
                "image_count": self.image_count, "image_tokens": None,
                "images": [{"sha256": i.sha256, "width": i.width, "height": i.height,
                            "bytes": len(i.data)} for m in self.messages for i in m.images],
                "first_token_delay": self.first_token_delay,
                "tokens_per_second": self.tokens_per_second, "duration": self.duration,
                "output_tokens": self.output_tokens, "reserve_tokens": self.reserve_tokens,
                "output_sha256": output_sha256,
                "credential_protection_enabled": bool(self.new_credential_secrets),
                "expected_new_mappings": len(self.new_credential_secrets)}


@dataclass(frozen=True)
class BoundPlan:
    wire: Plan
    client: Plan
    mappings: dict[str, str]
    new_references: tuple[str, ...]
    reused_references: int


def _message(value) -> Message:
    message = value if isinstance(value, Message) else Message(value["role"], value["text"], tuple(value.get("images", ())))
    if message.role not in ("user", "assistant", "system", "developer"):
        raise ValueError("unrecognized canonical message role")
    return message


def _png(rng: random.Random, size: int) -> Image:
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
    pixels = rng.randbytes(size * size * 3)
    rows = b"".join(b"\0" + pixels[y * size * 3:(y + 1) * size * 3] for y in range(size))
    data = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 2, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b"")
    return Image(data, size, size, hashlib.sha256(data).hexdigest())


class Workload:
    def __init__(self, seed=1, min_input_tokens=50_000, max_input_tokens=150_000,
                 min_first_token_delay=1, max_first_token_delay=10,
                 min_tokens_per_second=30, max_tokens_per_second=100,
                 min_duration=10, max_duration=60, min_images=0, max_images=10,
                 image_size=512, image_pool_size=10, credential_probes=False):
        self.encoding = tiktoken.get_encoding("cl100k_base")
        self.vocabulary = _vocabulary()
        self.seed = seed
        self.credential_probes = credential_probes
        self.input_range = (min_input_tokens, max_input_tokens)
        self.delay_range = (min_first_token_delay, max_first_token_delay)
        self.rate_range = (min_tokens_per_second, max_tokens_per_second)
        self.duration_range = (min_duration, max_duration)
        self.image_range = (min_images, max_images)
        for low, high in (self.input_range, self.delay_range, self.rate_range, self.duration_range, self.image_range):
            if low < 0 or low > high:
                raise ValueError("invalid workload range")
        if min_tokens_per_second < 1 or min_duration < 1 or image_size < 2 or image_pool_size < 1:
            raise ValueError("positive rate/duration/pool and nontrivial image dimensions required")
        self.images = tuple(_png(self._rng(("image", i)), image_size) for i in range(image_pool_size))

    def _rng(self, identity):
        payload = json.dumps([self.seed, identity], sort_keys=True, separators=(",", ":"))
        return random.Random(int.from_bytes(hashlib.sha256(payload.encode()).digest(), "big"))

    def count(self, text):
        return len(self.encoding.encode(text, disallowed_special=()))

    def make_plan(self, identity, history=(), reserve_tokens=0) -> Plan:
        """Sample the input target conditioned on history and continuation reserve."""
        if not isinstance(reserve_tokens, int) or reserve_tokens < 0:
            raise ValueError("reserve_tokens must be a nonnegative integer")
        history = tuple(_message(m) for m in history)
        if any(message.role not in ("user", "assistant") for message in history):
            raise ValueError("workload history supports user and assistant roles only")
        rng = self._rng(identity)
        nonce = "STRAVIA_BENCH_" + f"{rng.getrandbits(128):032x}"
        prefix = nonce + "\n"
        history_secrets = tuple(dict.fromkeys(
            re.findall(r"ghp_[A-Za-z0-9]{36}", "\n".join(m.text for m in history)))) if self.credential_probes else ()
        secrets = ()
        if self.credential_probes:
            alphabet = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"
            generated = []
            while len(generated) < 2:
                secret = "ghp_" + "".join(rng.choices(alphabet, k=36))
                if secret not in history_secrets and secret not in generated:
                    generated.append(secret)
            secrets = tuple(generated)
            prefix += " ".join((secrets[0], secrets[1], secrets[0])) + "\n"
        history_tokens = sum(self.count(m.text) for m in history)
        minimum = history_tokens + self.count(prefix)
        upper = self.input_range[1] - reserve_tokens
        if max(self.input_range[0], minimum) > upper:
            raise HistoryBudgetExceeded("history plus new request exceeds input budget; reset required")
        target = rng.randint(max(self.input_range[0], minimum), upper)
        text = prefix + "".join(rng.choices(self.vocabulary, k=target - minimum))
        if history_tokens + self.count(text) != target:
            raise ValueError("token filler boundary does not preserve exact cl100k_base budget")
        prior_images = sum(len(m.images) for m in history)
        if prior_images > self.image_range[1]:
            raise HistoryBudgetExceeded("history exceeds image budget; reset required")
        total_images = rng.randint(max(self.image_range[0], prior_images), self.image_range[1])
        user = Message("user", text, tuple(rng.choice(self.images) for _ in range(total_images - prior_images)))
        rate = rng.randint(*self.rate_range)
        duration = rng.randint(*self.duration_range)
        output_count = rate * duration + 1
        output_prefix = prefix
        if output_count < self.count(output_prefix):
            raise ValueError("output budget cannot contain the correlation nonce")
        output_text = output_prefix + "".join(rng.choices(
            self.vocabulary, k=output_count - self.count(output_prefix)))
        ids = self.encoding.encode(output_text, disallowed_special=())
        if len(ids) != output_count:
            raise ValueError("output token budget mismatch")
        fragments = tuple(self.encoding.decode_single_token_bytes(token).decode("utf-8") for token in ids)
        return Plan(nonce, history + (user,), user, target, total_images,
                    rng.uniform(*self.delay_range), rate, duration, output_text, fragments,
                    reserve_tokens, tuple(dict.fromkeys(history_secrets + secrets)), secrets)

    def _fragments(self, text):
        return tuple(self.encoding.decode_single_token_bytes(token).decode("utf-8")
                     for token in self.encoding.encode(text, disallowed_special=()))

    def bind_protected_plan(self, plan, actual_messages, known_mappings,
                            gateway_instruction=None) -> BoundPlan:
        """Validate complete normalized input before accepting any learned mapping.

        The caller serializes this operation and commits ``mappings`` under its
        principal lock. Error strings deliberately contain no request material.
        """
        actual_messages = tuple(_message(m) for m in actual_messages)
        conversation = actual_messages
        if gateway_instruction is not None:
            if not actual_messages or actual_messages[0] != Message("system", gateway_instruction):
                raise ValueError("gateway credential instruction mismatch")
            conversation = actual_messages[1:]
        if not plan.credential_secrets:
            if actual_messages != plan.messages:
                raise ValueError("protected input semantic mismatch")
            return BoundPlan(plan, plan, {}, (), 0)
        reference = re.compile(r"<!--sr:[a-z]{28}-->")
        mappings = {}
        occupied = {}
        for secret, value in known_mappings.items():
            if not reference.fullmatch(value) or value in occupied:
                raise ValueError("invalid known credential mapping")
            occupied[value] = secret
        if len(conversation) != len(plan.messages):
            raise ValueError("protected input message count mismatch")
        secret_pattern = re.compile("|".join(re.escape(s) for s in sorted(
            plan.credential_secrets, key=len, reverse=True)))
        for expected, actual in zip(plan.messages, conversation):
            if expected.role != actual.role or expected.images != actual.images:
                raise ValueError("protected input role or image mismatch")
            position = actual_position = 0
            for occurrence in secret_pattern.finditer(expected.text):
                literal = expected.text[position:occurrence.start()]
                if not actual.text.startswith(literal, actual_position):
                    raise ValueError("protected input text mismatch")
                actual_position += len(literal)
                matched = reference.match(actual.text, actual_position)
                if matched is None:
                    raise ValueError("protected input text mismatch")
                value = matched.group()
                actual_position = matched.end()
                position = occurrence.end()
                secret = occurrence.group()
                previous = mappings.get(secret, known_mappings.get(secret))
                if previous is not None and previous != value:
                    raise ValueError("credential reference changed")
                if value in occupied and occupied[value] != secret:
                    raise ValueError("credential reference collision")
                mappings[secret] = value
                occupied[value] = secret
            suffix = expected.text[position:]
            if (len(actual.text) - actual_position != len(suffix)
                    or not actual.text.startswith(suffix, actual_position)):
                raise ValueError("protected input text mismatch")
        if set(mappings) != set(plan.credential_secrets):
            raise ValueError("credential mapping coverage mismatch")
        old_secrets = set(plan.credential_secrets) - set(plan.new_credential_secrets)
        if any(secret not in known_mappings for secret in old_secrets):
            raise ValueError("history credential mapping missing")
        if any(secret in known_mappings for secret in plan.new_credential_secrets):
            raise ValueError("new credential mapping already exists")
        prefix = plan.nonce + "\n" + " ".join(
            mappings[s] for s in (plan.new_credential_secrets[0],
                                  plan.new_credential_secrets[1],
                                  plan.new_credential_secrets[0])) + "\n"
        remaining = plan.output_tokens - self.count(prefix)
        if remaining < 0:
            raise ValueError("protected output budget cannot contain credential probes")
        rng = self._rng(("protected-output", plan.nonce))
        output = prefix + "".join(rng.choices(self.vocabulary, k=remaining))
        fragments = self._fragments(output)
        if len(fragments) != plan.output_tokens:
            raise ValueError("protected output token budget mismatch")
        restored = output
        for secret, value in mappings.items():
            restored = restored.replace(value, secret)
        wire = replace(plan, messages=actual_messages, new_user=actual_messages[-1],
                       input_tokens=sum(self.count(m.text) for m in actual_messages),
                       output_text=output, output_fragments=fragments)
        client = replace(plan, output_text=restored, output_fragments=self._fragments(restored))
        return BoundPlan(wire, client, mappings,
                         tuple(mappings[s] for s in plan.new_credential_secrets),
                         len(old_secrets))


def build_request(protocol, model, stream, plan, history=None, explicit_parent=None):
    messages = plan.messages if history is None else tuple(_message(m) for m in history) + (plan.new_user,)
    def parts(m):
        text_type = "output_text" if protocol == "open-responses" and m.role == "assistant" else "input_text" if protocol == "open-responses" else "text"
        result = [{"type": text_type, "text": m.text}]
        for image in m.images:
            url = "data:" + image.media_type + ";base64," + image.base64
            if protocol == "openai-chat":
                result.append({"type": "image_url", "image_url": {"url": url}})
            elif protocol == "open-responses":
                result.append({"type": "input_image", "image_url": url})
            else:
                result.append({"type": "image", "source": {"type": "base64", "media_type": image.media_type, "data": image.base64}})
        return result
    if protocol == "google-content":
        body = {"contents": [{"role": "model" if m.role == "assistant" else "user", "parts": [{"text": m.text}] + [{"inlineData": {"mimeType": i.media_type, "data": i.base64}} for i in m.images]} for m in messages], "generationConfig": {"maxOutputTokens": 10000}}
    elif protocol in PROTOCOLS:
        body = {"model": model, "stream": stream, "input" if protocol == "open-responses" else "messages": [{"role": m.role, "content": parts(m)} for m in messages]}
        body["max_output_tokens" if protocol == "open-responses" else "max_tokens"] = 10000
        if protocol == "openai-chat" and stream:
            body["stream_options"] = {"include_usage": True}
    else:
        raise ValueError("unknown protocol")
    if explicit_parent is not None:
        if protocol != "open-responses":
            raise ValueError("explicit parent requires Responses protocol")
        body["previous_response_id"] = explicit_parent
    return body


def _decode_image(data, media_type):
    raw = base64.b64decode(data, validate=True)
    if media_type != "image/png" or raw[:8] != b"\x89PNG\r\n\x1a\n" or len(raw) < 24:
        raise ValueError("expected valid PNG image")
    width, height = struct.unpack(">II", raw[16:24])
    return Image(raw, width, height, hashlib.sha256(raw).hexdigest(), media_type)


def canonical_input(protocol, body):
    if protocol not in PROTOCOLS:
        raise ValueError("unknown protocol")
    items = body.get("contents" if protocol == "google-content" else "input" if protocol == "open-responses" else "messages", [])
    if isinstance(items, str):
        items = [{"role": "user", "content": items}]
    result = []
    if protocol == "open-responses":
        instruction = body.get("instructions")
        if instruction is not None:
            if not isinstance(instruction, str):
                raise ValueError("invalid Responses instructions")
            result.append(Message("system", instruction))
    elif protocol == "anthropic-messages":
        instruction = body.get("system")
        if isinstance(instruction, str):
            result.append(Message("system", instruction))
        elif instruction is not None:
            if not isinstance(instruction, list) or any(
                    part.get("type") != "text" or not isinstance(part.get("text"), str)
                    for part in instruction):
                raise ValueError("unrecognized Anthropic system content")
            result.append(Message("system", "".join(part["text"] for part in instruction)))
    elif protocol == "google-content":
        if "systemInstruction" in body and "system_instruction" in body:
            raise ValueError("ambiguous Gemini system instruction")
        instruction = body.get("systemInstruction", body.get("system_instruction"))
        if instruction is not None:
            parts = instruction.get("parts")
            if not isinstance(parts, list) or any(
                    set(part) != {"text"} or not isinstance(part["text"], str) for part in parts):
                raise ValueError("unrecognized Gemini system content")
            result.append(Message("system", "".join(part["text"] for part in parts)))
    for item in items:
        role = item.get("role", "user")
        role = "assistant" if role == "model" else role
        content = item.get("parts", []) if protocol == "google-content" else item.get("content", [])
        if isinstance(content, str):
            result.append(_message({"role": role, "text": content}))
            continue
        texts, images = [], []
        for part in content:
            if "text" in part and part.get("type", "text") in ("text", "input_text", "output_text"):
                texts.append(part["text"])
            elif "inlineData" in part or "inline_data" in part:
                image = part.get("inlineData", part.get("inline_data"))
                images.append(_decode_image(image["data"], image.get("mimeType", image.get("mime_type"))))
            elif part.get("type") in ("image_url", "input_image"):
                url = part["image_url"]
                url = url["url"] if isinstance(url, dict) else url
                header, data = url.split(",", 1)
                if not header.startswith("data:") or not header.endswith(";base64"):
                    raise ValueError("expected inline base64 image")
                images.append(_decode_image(data, header[5:-7]))
            elif part.get("type") == "image":
                source = part["source"]
                images.append(_decode_image(source["data"], source["media_type"]))
            else:
                raise ValueError("unrecognized canonical input content")
        result.append(_message({"role": role, "text": "".join(texts), "images": images}))
    return tuple(result)


def canonical_digest(messages):
    values = [{"role": m.role, "text": m.text, "images": [{"sha256": i.sha256, "media_type": i.media_type, "width": i.width, "height": i.height} for i in m.images]} for m in map(_message, messages)]
    return hashlib.sha256(json.dumps(values, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()).hexdigest()


def timed_response_frames(protocol, model, plan, stream) -> Iterable[tuple[float, str | None, dict | str]]:
    """Offsets are absolute deadlines relative to completed upstream request reading."""
    if protocol not in PROTOCOLS:
        raise ValueError("unknown protocol")
    rid = ("resp_" if protocol == "open-responses" else "msg_" if protocol == "anthropic-messages" else "chatcmpl_") + plan.nonce.removeprefix("STRAVIA_BENCH_")
    mid = "msg_" + plan.nonce.removeprefix("STRAVIA_BENCH_")
    usage = {"input_tokens": plan.input_tokens, "output_tokens": plan.output_tokens}
    end = plan.first_token_delay + plan.duration
    item = {"id": mid, "type": "message", "role": "assistant", "status": "completed", "content": [{"type": "output_text", "text": plan.output_text, "annotations": []}]}
    response = {
        "id": rid, "object": "response", "created_at": 1, "completed_at": 2,
        "status": "completed", "model": model, "output": [item],
        "incomplete_details": None, "previous_response_id": None,
        "instructions": None, "error": None, "tools": [], "tool_choice": "auto",
        "truncation": "disabled", "parallel_tool_calls": True,
        "text": {"format": {"type": "text"}}, "top_p": None,
        "presence_penalty": None, "frequency_penalty": None, "top_logprobs": None,
        "temperature": None, "reasoning": None, "max_output_tokens": None,
        "max_tool_calls": None, "store": False, "background": False,
        "service_tier": "default", "metadata": {}, "safety_identifier": None,
        "prompt_cache_key": None,
        "usage": {**usage, "total_tokens": plan.input_tokens + plan.output_tokens,
                  "input_tokens_details": {"cached_tokens": 0},
                  "output_tokens_details": {"reasoning_tokens": 0}},
    }
    chat_usage = {"prompt_tokens": plan.input_tokens, "completion_tokens": plan.output_tokens,
                  "total_tokens": plan.input_tokens + plan.output_tokens,
                  "prompt_tokens_details": {"cached_tokens": 0},
                  "completion_tokens_details": {"reasoning_tokens": 0}}
    def chat(delta, finish=None):
        return {"id": rid, "object": "chat.completion.chunk", "created": 1, "model": model, "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}
    def gemini(text, final=False):
        value = {"responseId": rid, "modelVersion": model, "candidates": [{"index": 0, "content": {"role": "model", "parts": [{"text": text}]}}]}
        if final:
            value["candidates"][0]["finishReason"] = "STOP"
            value["usageMetadata"] = {"promptTokenCount": plan.input_tokens, "candidatesTokenCount": plan.output_tokens,
                                      "totalTokenCount": plan.input_tokens + plan.output_tokens,
                                      "cachedContentTokenCount": 0, "thoughtsTokenCount": 0}
        return value
    anthropic_usage = {**usage, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}
    message = {"id": rid, "type": "message", "role": "assistant", "model": model, "content": [{"type": "text", "text": plan.output_text}], "stop_reason": "end_turn", "stop_sequence": None, "usage": anthropic_usage}
    if not stream:
        if protocol == "openai-chat":
            value = {"id": rid, "object": "chat.completion", "created": 1, "model": model, "choices": [{"index": 0, "message": {"role": "assistant", "content": plan.output_text}, "finish_reason": "stop"}], "usage": chat_usage}
        else:
            value = response if protocol == "open-responses" else message if protocol == "anthropic-messages" else gemini(plan.output_text, True)
        yield end, None, value
        return
    seq = 0
    def event(name, **body):
        nonlocal seq
        value = {"type": name, "sequence_number": seq, **body}
        seq += 1
        return name, value
    if protocol == "openai-chat":
        yield 0, None, chat({"role": "assistant", "content": ""})
    elif protocol == "anthropic-messages":
        yield 0, "message_start", {"type": "message_start", "message": {**message, "content": [], "stop_reason": None, "usage": {**anthropic_usage, "output_tokens": 0}}}
        yield 0, "content_block_start", {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}
    elif protocol == "open-responses":
        for name, body in (("response.created", {"response": {**response, "status": "in_progress", "completed_at": None, "output": [], "usage": None}}), ("response.in_progress", {"response": {**response, "status": "in_progress", "completed_at": None, "output": [], "usage": None}}), ("response.output_item.added", {"output_index": 0, "item": {**item, "status": "in_progress", "content": []}}), ("response.content_part.added", {"item_id": mid, "output_index": 0, "content_index": 0, "part": {"type": "output_text", "text": "", "annotations": []}})):
            name, value = event(name, **body)
            yield 0, name, value
    for index, fragment in enumerate(plan.output_fragments):
        offset = plan.first_token_delay + index / plan.tokens_per_second
        name = None
        if protocol == "openai-chat":
            value = chat({"content": fragment})
        elif protocol == "anthropic-messages":
            name = "content_block_delta"
            value = {"type": name, "index": 0, "delta": {"type": "text_delta", "text": fragment}}
        elif protocol == "open-responses":
            name, value = event("response.output_text.delta", item_id=mid, output_index=0, content_index=0, delta=fragment)
        else:
            value = gemini(fragment)
        yield offset, name, value
    if protocol == "openai-chat":
        yield end, None, chat({}, "stop")
        yield end, None, {"id": rid, "object": "chat.completion.chunk", "created": 1, "model": model, "choices": [], "usage": chat_usage}
        yield end, None, "[DONE]"
    elif protocol == "anthropic-messages":
        yield end, "content_block_stop", {"type": "content_block_stop", "index": 0}
        yield end, "message_delta", {"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": None}, "usage": {"output_tokens": plan.output_tokens}}
        yield end, "message_stop", {"type": "message_stop"}
    elif protocol == "google-content":
        yield end, None, gemini("", True)
    else:
        for name, body in (("response.output_text.done", {"item_id": mid, "output_index": 0, "content_index": 0, "text": plan.output_text}), ("response.content_part.done", {"item_id": mid, "output_index": 0, "content_index": 0, "part": item["content"][0]}), ("response.output_item.done", {"output_index": 0, "item": item}), ("response.completed", {"response": response})):
            name, value = event(name, **body)
            yield end, name, value
