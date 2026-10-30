from __future__ import annotations

from copy import deepcopy

import pytest

from tests.common import measure_history_paths
from tests.common.measure_history_paths import attach_performance_snapshot, compact_digest, history_input


def span(identifier, name, parent=None, state="finished"):
    args = {"span_id": identifier, "capture_state": state, "status": "completed"}
    if name == "proxy.inference_run":
        args["request_id"] = f"request-{identifier}"
    if parent is not None:
        args["parent_id"] = parent
    event = {"pid": 1, "tid": 1, "ts": identifier * 10, "name": name,
             "ph": "X" if state == "finished" else "B", "args": args}
    if state == "finished":
        event["dur"] = 5
    return event


def snapshot(*events):
    active = sum(event["args"]["capture_state"] == "active" for event in events)
    frozen = sum(event["args"]["capture_state"] == "frozen" for event in events)
    return {
        "traceEvents": list(events),
        "metadata": {
            "dropped_active": 0, "dropped_completed": 0, "dropped_sql_completed": 0,
            "dropped_resource_samples": 0, "active_spans": active, "frozen_spans": frozen,
            "frozen_total": frozen, "incomplete": active + frozen, "incomplete_total": active + frozen,
            "schema_version": 3, "debug_epoch": 1, "debug_enabled": True,
            "storage_status": "ready", "storage_errors": 0,
        },
    }


def request_events():
    return [span(1, "proxy.inference_run"), span(2, "router.select", 1),
            span(3, "generation_chain.parent.resolve", 1), span(4, "sql.query", 3)]


def measure(after, before=None):
    row = {"observation_run_id": "request-1"}
    capture = attach_performance_snapshot([row], snapshot() if before is None else before, after)
    row["background_performance_spans"] = capture["background_performance_spans"]
    return row


@pytest.mark.e2e
@pytest.mark.proxy
@pytest.mark.parametrize("epoch", [None, 0, 1])
def test_history_benchmark_disabled_controls_leave_unmeasured_costs_unknown(epoch):
    before = snapshot()
    before["metadata"].update(debug_enabled=False, debug_epoch=epoch)
    rows = [{"observation_run_id": "first"}, {"observation_run_id": "second"}]
    control = measure_history_paths.attach_disabled_performance_controls(rows, before, deepcopy(before))
    assert control["valid"]
    for row in rows:
        assert row["request_sql_span_count"] is None
        assert row["request_sql_duration_us"] is None
        assert row["history_measurements"] is None


@pytest.mark.e2e
@pytest.mark.proxy
@pytest.mark.parametrize("change", ["enabled_before", "enabled_after", "epoch_changed", "schema_changed", "missing_metadata", "active_span", "completed_span", "missing_observation", "duplicate_observation"])
def test_history_benchmark_disabled_controls_reject_activity_and_unverifiable_identity(change):
    before = snapshot()
    before["metadata"]["debug_enabled"] = False
    after = deepcopy(before)
    rows = [{"observation_run_id": "first"}, {"observation_run_id": "second"}]
    if change == "enabled_before":
        before["metadata"]["debug_enabled"] = True
    elif change == "enabled_after":
        after["metadata"]["debug_enabled"] = True
    elif change == "epoch_changed":
        after["metadata"]["debug_epoch"] += 1
    elif change == "schema_changed":
        after["metadata"]["schema_version"] = 2
    elif change == "missing_metadata":
        after.pop("metadata")
    elif change in ("active_span", "completed_span"):
        after["traceEvents"] = request_events()
        after["traceEvents"][0]["ph"] = "B" if change == "active_span" else "X"
    elif change == "missing_observation":
        rows[0].pop("observation_run_id")
    else:
        rows[1]["observation_run_id"] = "first"
    assert not measure_history_paths.attach_disabled_performance_controls(rows, before, after)["valid"]


@pytest.mark.e2e
@pytest.mark.proxy
@pytest.mark.parametrize("change", ["arguments", "call_binding", "result"])
def test_history_benchmark_tool_window_detects_argument_binding_and_result_changes(change):
    original = history_input({"role": "user", "content": "a recorded tool turn"}, "x" * 4096, "tool-json")
    changed = deepcopy(original)
    if change == "arguments":
        changed[1]["arguments"] = changed[1]["arguments"].replace("18446744073709551617", "18446744073709551618")
    elif change == "call_binding":
        changed[2]["call_id"] = "different-call"
    else:
        changed[2]["output"] = "different recorded result"
    assert compact_digest(original) != compact_digest(changed)


@pytest.mark.e2e
@pytest.mark.proxy
def test_history_benchmark_partitions_shared_snapshot_by_observed_request_identity():
    first = request_events()
    first[0]["args"]["request_id"] = "observed-first"
    second = [span(20, "proxy.inference_run"), span(21, "router.select", 20),
              span(22, "turn_chain.persist", 20), span(23, "sql.query", 22)]
    second[0]["args"]["request_id"] = "observed-second"
    second[-1]["dur"] = 17
    for event in first + second:
        event["ts"] = 100
    rows = [{"observation_run_id": "observed-second"},
            {"observation_run_id": "observed-first"}]
    writer = [span(30, "observation.command.process", 20), span(31, "sql.query", 30)]
    writer[-1]["dur"] = 19
    background = [span(50, "observation.flush"), span(51, "sql.query", 50), span(60, "sql.query")]
    background[1]["dur"], background[2]["dur"] = 11, 13
    capture = attach_performance_snapshot(rows, snapshot(), snapshot(*first, *second, *writer, *background))
    assert all(row["trace_metadata_valid"] and row["request_span_valid"] for row in rows)
    assert [row["inference_root_span_id"] for row in rows] == [20, 1]
    assert [row["request_sql_duration_us"] for row in rows] == [17, 5]
    assert [row["request_sql_span_count"] for row in rows] == [1, 1]
    retained = capture["background_performance_spans"] + [event for row in rows for event in row["performance_spans"]]
    assert sum(event["dur_us"] for event in retained if event["name"] == "sql.query") == 65


@pytest.mark.e2e
@pytest.mark.proxy
@pytest.mark.parametrize("change", ["missing_observation", "missing_trace_identity", "foreign_trace_identity",
                                   "duplicate_observation", "duplicate_trace_identity", "preexisting_root"])
def test_history_benchmark_requires_unique_new_observed_request_identity(change):
    events = request_events()
    rows = [{"observation_run_id": "request-1"}]
    before = snapshot()
    if change == "missing_observation":
        rows[0].clear()
    elif change == "missing_trace_identity":
        del events[0]["args"]["request_id"]
    elif change == "foreign_trace_identity":
        events[0]["args"]["request_id"] = "foreign-request"
    elif change == "duplicate_observation":
        rows.append(dict(rows[0]))
    elif change == "duplicate_trace_identity":
        duplicate = span(20, "proxy.inference_run")
        duplicate["args"]["request_id"] = "request-1"
        events.append(duplicate)
    else:
        before = snapshot(deepcopy(events[0]))
    capture = attach_performance_snapshot(rows, before, snapshot(*events))
    assert not capture["valid"]
    assert all(not row["trace_metadata_valid"] or not row["request_span_valid"] for row in rows)


@pytest.mark.e2e
@pytest.mark.proxy
@pytest.mark.parametrize("parent", [1, 3, 4])
def test_history_benchmark_rejects_unfinished_request_descendant(parent):
    row = measure(snapshot(*request_events(), span(5, "sql.query", parent, "active")))
    assert not row["trace_metadata_valid"]
    assert not row["request_span_valid"]
    assert [event["span_id"] for event in row["active_request_spans"]] == [5]
    assert row["request_sql_span_count"] == 1
    assert row["request_sql_duration_us"] == 5


@pytest.mark.e2e
@pytest.mark.proxy
@pytest.mark.parametrize("other_root", [False, True])
def test_history_benchmark_reports_unrelated_live_sql_without_charging_request(other_root):
    unrelated = [span(10, "proxy.inference_run")] if other_root else []
    before = snapshot(*unrelated)
    after = snapshot(*unrelated, *request_events(),
                     span(11, "sql.query", 10 if other_root else None, "active"))
    row = measure(after, before)
    assert row["trace_metadata_valid"]
    assert row["request_span_valid"]
    assert row["trace_metadata_after"]["incomplete_total"] > row["trace_metadata_before"]["incomplete_total"]
    assert row["global_incomplete_spans"] == ["sql.query"]
    assert row["active_unrelated_spans"] == [{
        "name": "sql.query", "span_id": 11, "parent_id": 10 if other_root else None,
        "attribution": "other_inference" if other_root else "unrelated_sql",
    }]
    assert row["request_sql_span_count"] == 1
    assert row["request_sql_duration_us"] == 5


@pytest.mark.e2e
@pytest.mark.proxy
@pytest.mark.parametrize("command", [False, True])
def test_history_benchmark_separates_observation_causal_chain(command):
    background = span(10, "observation.command.process" if command else "observation.flush",
                      1 if command else None, "active")
    after = snapshot(*request_events(), background,
                     span(11, "turn_chain.persist", 10),
                     span(12, "sql.query", 11, "active"))
    row = measure(after)
    assert row["trace_metadata_valid"]
    assert row["request_span_valid"]
    assert [event["span_id"] for event in row["active_observation_spans"]] == [10, 12]
    history = row["observation_history_spans"] + [event for event in row["background_performance_spans"]
                                                if event["name"].startswith("turn_chain.")]
    assert [event["name"] for event in history] == ["turn_chain.persist"]
    assert [event["name"] for event in row["history_measurements"]] == ["generation_chain.parent.resolve"]
    assert row["request_sql_span_count"] == 1
    assert row["request_subtree_span_count"] == 4


@pytest.mark.e2e
@pytest.mark.proxy
@pytest.mark.parametrize("field", [
    "dropped_active", "dropped_completed", "dropped_sql_completed", "dropped_resource_samples",
    "frozen_spans", "frozen_total", "storage_errors",
])
def test_history_benchmark_rejects_growing_loss_counters(field):
    after = snapshot(*request_events())
    after["metadata"][field] += 1
    assert not measure(after)["trace_metadata_valid"]


@pytest.mark.e2e
@pytest.mark.proxy
@pytest.mark.parametrize("field", ["active_spans", "frozen_spans", "frozen_total"])
@pytest.mark.parametrize("side", ["before", "after"])
def test_history_benchmark_requires_lifecycle_counters(field, side):
    before, after = snapshot(), snapshot(*request_events())
    del (before if side == "before" else after)["metadata"][field]
    assert not measure(after, before)["trace_metadata_valid"]


@pytest.mark.e2e
@pytest.mark.proxy
@pytest.mark.parametrize("state", ["frozen", "unknown", None])
def test_history_benchmark_rejects_non_live_begin_states(state):
    after = snapshot(*request_events(), span(10, "sql.query", state="active"))
    if state is None:
        del after["traceEvents"][-1]["args"]["capture_state"]
    else:
        after["traceEvents"][-1]["args"]["capture_state"] = state
    row = measure(after)
    assert not row["trace_metadata_valid"]
    assert row["trace_capture_errors"]


@pytest.mark.e2e
@pytest.mark.proxy
def test_history_benchmark_rejects_retained_frozen_span_without_counter_growth():
    frozen = span(10, "sql.query", state="frozen")
    row = measure(snapshot(*request_events(), frozen), snapshot(deepcopy(frozen)))
    assert row["trace_metadata_before"]["frozen_total"] == row["trace_metadata_after"]["frozen_total"] == 1
    assert not row["trace_metadata_valid"]
    assert any("frozen span" in error for error in row["trace_capture_errors"])


@pytest.mark.e2e
@pytest.mark.proxy
@pytest.mark.parametrize("state", ["active", "frozen", None])
def test_history_benchmark_requires_finished_capture_state_for_complete_events(state):
    after = snapshot(*request_events())
    if state is None:
        del after["traceEvents"][3]["args"]["capture_state"]
    else:
        after["traceEvents"][3]["args"]["capture_state"] = state
    row = measure(after)
    assert not row["trace_metadata_valid"]
    assert row["trace_capture_errors"]


@pytest.mark.e2e
@pytest.mark.proxy
@pytest.mark.parametrize("events", [
    [span(10, "sql.query", 99, "active")],
    [span(10, "observation.command.process", 99, "active"), span(11, "sql.query", 10, "active")],
    [span(10, "observation.command.process", 11, "active"), span(11, "sql.query", 10, "active")],
    [span(10, "unknown.business", state="active"), span(11, "sql.query", 10, "active")],
    [span(10, "unknown.business"), span(11, "sql.query", 10, "active")],
    [span(10, "sql.unknown", state="active")],
    [span(10, "sql.query", 99)],
])
def test_history_benchmark_does_not_hide_unknown_or_broken_causal_chains(events):
    row = measure(snapshot(*request_events(), *deepcopy(events)))
    assert not row["trace_metadata_valid"]
    assert row["trace_capture_errors"] or row["unattributed_active_spans"]
    assert not row["active_unrelated_spans"]


@pytest.mark.e2e
@pytest.mark.proxy
def test_history_benchmark_does_not_copy_invalid_capture_state_into_report():
    after = snapshot(*request_events())
    private_state = "private-invalid-capture-state"
    after["traceEvents"][-1]["args"]["capture_state"] = private_state
    row = measure(after)
    assert not row["trace_metadata_valid"]
    assert private_state not in repr(row)


@pytest.mark.e2e
@pytest.mark.proxy
@pytest.mark.parametrize("change", ["missing_root", "duplicate_root", "unattributed_history", "background_router"])
def test_history_benchmark_preserves_root_and_history_acceptance(change):
    events = request_events()
    if change == "missing_root":
        events = events[1:]
    elif change == "duplicate_root":
        events.append(span(10, "proxy.inference_run"))
    elif change == "background_router":
        events.extend([span(10, "observation.flush"), span(11, "router.select", 10)])
    else:
        events.append(span(10, "generation_chain.parent.resolve"))
    assert not measure(snapshot(*events))["request_span_valid"]


@pytest.mark.e2e
@pytest.mark.proxy
def test_history_benchmark_accepts_active_completion_without_counter_loss():
    active = span(10, "sql.query", state="active")
    before = snapshot(active)
    before["metadata"]["frozen_total"] = 2
    before["metadata"]["incomplete_total"] = 3
    after = snapshot(*request_events(), span(10, "sql.query"))
    after["metadata"]["frozen_total"] = 2
    after["metadata"]["incomplete_total"] = 2
    row = measure(after, before)
    assert row["trace_metadata_valid"]
    assert row["request_span_valid"]
    assert row["request_sql_span_count"] == 1
    assert row["active_unrelated_spans"] == []


@pytest.mark.e2e
@pytest.mark.proxy
@pytest.mark.parametrize("change", ["session_changed", "debug_disabled", "storage_error"])
def test_history_benchmark_rejects_reset_or_unreadable_recording(change):
    before, after = snapshot(), snapshot(*request_events())
    if change == "session_changed":
        after["metadata"]["debug_epoch"] += 1
    elif change == "debug_disabled":
        after["metadata"]["debug_enabled"] = False
    else:
        before["metadata"]["storage_status"] = "error"
        after["metadata"]["storage_status"] = "error"
        before["metadata"]["storage_errors"] = 1
        after["metadata"]["storage_errors"] = 1
    assert not measure(after, before)["trace_metadata_valid"]


@pytest.mark.e2e
@pytest.mark.proxy
def test_history_benchmark_accepts_successful_disk_spill_without_treating_it_as_loss():
    before, after = snapshot(), snapshot(*request_events())
    for field in ("spilled_completed", "spilled_sql_completed", "spilled_resource_samples"):
        before["metadata"][field] = 0
        after["metadata"][field] = 4
    row = measure(after, before)
    assert row["trace_metadata_valid"]
    assert row["request_span_valid"]
    assert row["request_sql_span_count"] == 1
