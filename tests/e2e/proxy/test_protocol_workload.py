"""Pure behavioral checks for deterministic real-HTTP benchmark workloads."""
import base64
import hashlib
import json
import struct
import zlib
from dataclasses import replace

import pytest

from tests.common.protocol_workload import (
    HistoryBudgetExceeded, Message, PROTOCOLS, Workload, build_request,
    canonical_digest, canonical_input, timed_response_frames,
)
from tests.common.measure_protocol_http import visible


def workload(**kwargs):
    return Workload(seed=17, min_input_tokens=50_000, max_input_tokens=50_100,
                    min_images=2, max_images=2, image_pool_size=2, **kwargs)


def test_identity_determinism_is_independent_of_scheduling():
    source = workload()
    first = source.make_plan(("case", "measure", 1))
    source.make_plan(("other", "warmup", 99))
    assert first == source.make_plan(("case", "measure", 1))
    assert first == workload().make_plan(("case", "measure", 1))
    assert first.nonce != source.make_plan(("case", "measure", 2)).nonce
    assert source.count(first.new_user.text) == first.input_tokens
    assert first.output_tokens == first.tokens_per_second * first.duration + 1
    assert source.count(first.output_text) == first.output_tokens
    assert "".join(first.output_fragments) == first.output_text
    assert first.nonce in first.output_text
    assert first.output_fragments[0] != first.nonce
    # Exercise generated bodies rather than inspecting the vocabulary constant:
    # realistic payloads must not collapse to one repeated filler token.
    assert len(set(first.new_user.text.split()[1:])) > 1000
    assert len(set(first.output_text.split()[1:])) > 100
    assert all(word.isascii() and word.isalpha()
               for word in first.new_user.text.split()[1:])


def test_full_history_is_preserved_and_budgeted():
    source = workload()
    initial = source.make_plan(("case", "measure", 1))
    history = (Message("user", "prior question", initial.new_user.images),
               Message("assistant", "prior answer"))
    plan = source.make_plan(("case", "measure", 2), history)
    assert plan.messages[:-1] == history
    assert sum(source.count(m.text) for m in plan.messages) == plan.input_tokens
    assert 50_000 <= plan.input_tokens <= 50_100
    assert plan.image_count == 2
    assert not plan.new_user.images
    with pytest.raises(HistoryBudgetExceeded, match="reset"):
        source.make_plan(("case", "measure", 3), [Message("user", " a" * 50_101)])


def test_reserved_budget_allows_full_continuation_without_truncation():
    source = Workload(seed=29, min_input_tokens=50_000, max_input_tokens=150_000,
                      min_images=0, max_images=0, image_pool_size=1)
    reserve = 100 * 60 + 65
    first = source.make_plan(("chain", "measure", 0), reserve_tokens=reserve)
    assert first.input_tokens <= 150_000 - reserve
    assert first.metadata()["reserve_tokens"] == reserve
    history = first.messages + (Message("assistant", first.output_text),)
    second = source.make_plan(("chain", "measure", 1), history)
    assert second.messages[:-1] == history
    assert 50_000 <= second.input_tokens <= 150_000
    assert sum(source.count(m.text) for m in second.messages) == second.input_tokens
    with pytest.raises(HistoryBudgetExceeded, match="reset"):
        source.make_plan(("chain", "measure", 0), reserve_tokens=100_001)


@pytest.mark.parametrize("protocol", PROTOCOLS)
def test_multimodal_roundtrip_and_loss_detection(protocol):
    source = workload()
    first = source.make_plan(("case", "measure", 1))
    plan = source.make_plan(("case", "measure", 2), [Message("user", "question", first.new_user.images), Message("assistant", "answer")])
    body = build_request(protocol, "model", True, plan)
    control = (body["generationConfig"]["maxOutputTokens"] if protocol == "google-content"
               else body["max_output_tokens" if protocol == "open-responses" else "max_tokens"])
    other_body = build_request(protocol, "model", True, first)
    other_control = (other_body["generationConfig"]["maxOutputTokens"] if protocol == "google-content"
                     else other_body["max_output_tokens" if protocol == "open-responses" else "max_tokens"])
    assert control == other_control
    assert canonical_digest(canonical_input(protocol, body)) == canonical_digest(plan.messages)
    parsed = canonical_input(protocol, body)
    assert canonical_digest([Message(m.role, m.text) for m in parsed]) != canonical_digest(plan.messages)
    image = first.new_user.images[0]
    assert image.width == image.height == 512
    assert image.data[:8] == b"\x89PNG\r\n\x1a\n"
    pos, chunks = 8, []
    while pos < len(image.data):
        size = struct.unpack(">I", image.data[pos:pos + 4])[0]
        kind = image.data[pos + 4:pos + 8]
        data = image.data[pos + 8:pos + 8 + size]
        assert zlib.crc32(kind + data) == struct.unpack(">I", image.data[pos + 8 + size:pos + 12 + size])[0]
        if kind == b"IDAT":
            chunks.append(data)
        pos += size + 12
    pixels = zlib.decompress(b"".join(chunks))
    assert len(pixels) == 512 * (1 + 512 * 3)
    assert len(set(pixels)) == 256
    assert base64.b64decode(image.base64) == image.data


@pytest.mark.parametrize("protocol", PROTOCOLS)
def test_stream_deadlines_text_usage_and_terminal(protocol):
    source = workload(min_tokens_per_second=30, max_tokens_per_second=30,
                      min_duration=10, max_duration=10)
    plan = source.make_plan(("case", "measure", 1))
    frames = list(timed_response_frames(protocol, "model", plan, True))
    assert [f[0] for f in frames] == sorted(f[0] for f in frames)
    deltas = [(offset, visible(protocol, value, True)) for offset, _, value in frames if visible(protocol, value, True)]
    assert len(deltas) == plan.output_tokens
    assert "".join(text for _, text in deltas) == plan.output_text
    assert deltas[0][0] == plan.first_token_delay
    assert deltas[-1][0] - deltas[0][0] == plan.duration
    assert all(abs((right[0] - left[0]) - 1 / 30) < 1e-12 for left, right in zip(deltas, deltas[1:]))
    terminal = frames[-1][2]
    objects = [value for _, _, value in frames if isinstance(value, dict)]
    if protocol == "openai-chat":
        assert terminal == "[DONE]"
        assert objects[-1]["usage"]["completion_tokens"] == plan.output_tokens
        assert objects[-2]["choices"][0]["finish_reason"] == "stop"
    elif protocol == "open-responses":
        assert terminal["type"] == "response.completed"
        assert terminal["response"]["usage"]["output_tokens"] == plan.output_tokens
    elif protocol == "anthropic-messages":
        assert terminal["type"] == "message_stop"
        assert objects[-2]["usage"]["output_tokens"] == plan.output_tokens
    else:
        assert terminal["candidates"][0]["finishReason"] == "STOP"
        assert terminal["usageMetadata"]["candidatesTokenCount"] == plan.output_tokens
    nonstream = list(timed_response_frames(protocol, "model", plan, False))
    assert len(nonstream) == 1
    assert nonstream[0][0] == plan.first_token_delay + plan.duration
    assert visible(protocol, nonstream[0][2], False) == plan.output_text


def protected_messages(plan, mappings):
    result = []
    for message in plan.messages:
        text = message.text
        for secret, reference in mappings.items():
            text = text.replace(secret, reference)
        result.append(replace(message, text=text))
    return tuple(result)


def probe_workload(**kwargs):
    return Workload(seed=31, min_input_tokens=1000, max_input_tokens=8000,
                    credential_probes=True, min_images=1, max_images=1,
                    image_pool_size=1, image_size=4, **kwargs)


def references(plan, offset=0):
    return {secret: "<!--sr:" + chr(97 + index + offset) * 28 + "-->"
            for index, secret in enumerate(plan.new_credential_secrets)}


@pytest.mark.parametrize("protocol", PROTOCOLS)
def test_gateway_instruction_is_counted_without_discarding_conversation_content(protocol):
    source = probe_workload()
    plan = source.make_plan(("instruction", protocol))
    mapping = references(plan)
    body = build_request(protocol, "model", True,
                         replace(plan, messages=protected_messages(plan, mapping)))
    instruction = "Synthetic gateway-owned protection instruction."
    if protocol == "openai-chat":
        body["messages"].insert(0, {"role": "system", "content": instruction})
    elif protocol == "open-responses":
        body["instructions"] = instruction
    elif protocol == "anthropic-messages":
        body["system"] = [{"type": "text", "text": instruction}]
    else:
        body["system_instruction"] = {"parts": [{"text": instruction}]}
    actual = canonical_input(protocol, body)
    bound = source.bind_protected_plan(plan, actual, {}, instruction)
    assert bound.wire.input_tokens == source.count(instruction) + sum(
        source.count(message.text) for message in protected_messages(plan, mapping))
    assert bound.client.messages == plan.messages
    with pytest.raises(ValueError, match="instruction mismatch"):
        source.bind_protected_plan(plan, actual, {}, "Changed policy")
    with pytest.raises(ValueError, match="message count mismatch"):
        source.bind_protected_plan(plan, actual[:1] + actual, {}, instruction)
    changed = actual[:-1] + (replace(actual[-1], text=actual[-1].text + " altered"),)
    with pytest.raises(ValueError, match="text mismatch"):
        source.bind_protected_plan(plan, changed, {}, instruction)


@pytest.mark.parametrize("protocol", PROTOCOLS)
@pytest.mark.parametrize("rate,duration", [(30, 10), (100, 60)])
def test_bound_probes_preserve_full_semantics_and_exact_model_pacing(protocol, rate, duration):
    source = probe_workload(min_tokens_per_second=rate, max_tokens_per_second=rate,
                            min_duration=duration, max_duration=duration)
    plan = source.make_plan(("protected", protocol))
    mappings = references(plan)
    actual = canonical_input(protocol, build_request(
        protocol, "model", True, replace(plan, messages=protected_messages(plan, mappings))))
    bound = source.bind_protected_plan(plan, actual, {})
    assert bound.mappings == mappings
    assert bound.new_references == tuple(mappings.values())
    assert bound.reused_references == 0
    assert bound.wire.messages == actual
    assert bound.client.messages == plan.messages
    assert bound.client.input_tokens == plan.input_tokens
    assert bound.wire.input_tokens == sum(source.count(m.text) for m in actual)
    assert bound.wire.output_tokens == rate * duration + 1
    assert source.count(bound.wire.output_text) == bound.wire.output_tokens
    assert "".join(bound.wire.output_fragments) == bound.wire.output_text
    assert "".join(bound.client.output_fragments) == bound.client.output_text
    assert source.count(bound.client.output_text) == bound.client.output_tokens
    restored = bound.wire.output_text
    for secret, reference in mappings.items():
        assert plan.new_user.text.count(secret) == bound.client.output_text.count(secret)
        assert secret not in bound.wire.output_text
        restored = restored.replace(reference, secret)
    assert restored == bound.client.output_text
    frames = list(timed_response_frames(protocol, "model", bound.wire, True))
    deltas = [(offset, visible(protocol, value, True)) for offset, _, value in frames
              if visible(protocol, value, True)]
    assert len(deltas) == bound.wire.output_tokens
    assert "".join(text for _, text in deltas) == bound.wire.output_text
    assert deltas[-1][0] - deltas[0][0] == pytest.approx(duration)
    assert all(abs(right[0] - left[0] - 1 / rate) < 1e-12
               for left, right in zip(deltas, deltas[1:]))
    metadata = bound.client.metadata()
    assert metadata["expected_new_mappings"] == 2
    assert metadata["credential_protection_enabled"]
    assert metadata["output_sha256"] == hashlib.sha256(restored.encode()).hexdigest()
    serialized = json.dumps(metadata)
    assert all(value not in serialized for value in (*mappings, *mappings.values()))


@pytest.mark.parametrize("mutation", [
    "plaintext", "unknown", "partial", "alias", "changed-repeat",
    "suffix", "role", "image", "missing-message",
])
def test_binding_rejects_semantic_or_mapping_mutations_without_committing(mutation):
    source = probe_workload()
    plan = source.make_plan("mutation")
    mappings = references(plan)
    actual = list(protected_messages(plan, mappings))
    first, second = mappings.values()
    text = actual[-1].text
    if mutation == "plaintext":
        text = text.replace(first, plan.new_credential_secrets[0])
    elif mutation == "unknown":
        text = text.replace(first, "<!--sr:UNKNOWN-->")
    elif mutation == "partial":
        text = text.replace(first, first[:-1])
    elif mutation == "alias":
        text = text.replace(second, first)
    elif mutation == "changed-repeat":
        text = text.replace(first, "<!--sr:" + "z" * 28 + "-->", 1)
    elif mutation == "suffix":
        text += " changed"
    actual[-1] = replace(actual[-1], text=text)
    if mutation == "role":
        actual[-1] = replace(actual[-1], role="assistant")
    elif mutation == "image":
        actual[-1] = replace(actual[-1], images=())
    elif mutation == "missing-message":
        actual = []
    known = {}
    with pytest.raises(ValueError):
        source.bind_protected_plan(plan, actual, known)
    assert known == {}


def test_multiround_reuses_every_full_history_mapping_and_rejects_changes():
    source = probe_workload(min_tokens_per_second=30, max_tokens_per_second=30,
                            min_duration=10, max_duration=10)
    first = source.make_plan("first", reserve_tokens=1500)
    mappings = references(first)
    bound = source.bind_protected_plan(first, protected_messages(first, mappings), {})
    history = bound.client.messages + (Message("assistant", bound.client.output_text),)
    second = source.make_plan("second", history)
    combined = {**mappings, **references(second, offset=2)}
    actual = protected_messages(second, combined)
    followup = source.bind_protected_plan(second, actual, mappings)
    assert followup.reused_references == 2
    assert followup.mappings == combined
    assert followup.client.messages[:-1] == history
    assert len(second.credential_secrets) == 4
    assert followup.wire.messages[:-1] == protected_messages(
        replace(second, messages=history), combined)
    changed = dict(combined)
    changed[first.new_credential_secrets[0]] = "<!--sr:" + "z" * 28 + "-->"
    with pytest.raises(ValueError, match="changed"):
        source.bind_protected_plan(second, protected_messages(second, changed), mappings)
    with pytest.raises(ValueError, match="missing"):
        source.bind_protected_plan(second, actual, {})
    assert mappings == references(first)


def test_probe_budgets_fail_instead_of_truncating_and_legacy_binding_is_trivial():
    tiny = Workload(min_input_tokens=1, max_input_tokens=2, credential_probes=True,
                    min_images=0, max_images=0, image_size=4, image_pool_size=1)
    with pytest.raises(HistoryBudgetExceeded):
        tiny.make_plan("too-small")
    source = probe_workload(min_tokens_per_second=1, max_tokens_per_second=1,
                            min_duration=1, max_duration=1)
    with pytest.raises(ValueError, match="output budget"):
        source.make_plan("too-small-output")
    legacy = workload()
    plan = legacy.make_plan("legacy")
    bound = legacy.bind_protected_plan(plan, plan.messages, {})
    assert bound.wire is plan and bound.client is plan
    assert bound.mappings == {} and bound.new_references == ()
    with pytest.raises(ValueError, match="semantic"):
        legacy.bind_protected_plan(plan, (), {})


def test_binding_rejects_known_reference_alias_and_protected_output_overflow():
    source = probe_workload()
    plan = source.make_plan("mapping-boundaries")
    mappings = references(plan)
    actual = protected_messages(plan, mappings)
    unrelated = {"ghp_" + "0" * 36: next(iter(mappings.values()))}
    with pytest.raises(ValueError, match="collision"):
        source.bind_protected_plan(plan, actual, unrelated)
    assert len(unrelated) == 1
    with pytest.raises(ValueError, match="output budget"):
        source.bind_protected_plan(
            replace(plan, output_fragments=("one", "two")), actual, {})


def test_registered_probe_metadata_uses_full_text_not_just_nonce():
    source = probe_workload()
    plan = source.make_plan("full-hash")
    bound = source.bind_protected_plan(plan, protected_messages(plan, references(plan)), {})
    modified = replace(bound.client, output_text=bound.client.output_text + " suffix")
    assert modified.metadata()["output_sha256"] != bound.client.metadata()["output_sha256"]
