#!/usr/bin/env python3
"""Reuse verified or in-progress CI runs for identical commits on main/dev pushes.

Supports both Forgejo Actions and GitHub Actions REST API schemas using only the
Python standard library.

Two subcommands decouple selection from waiting so `main` can run `macos-cross-smoke`
in parallel while waiting on a reused `dev` CI run:

  - `select`: Fast decision step inside `changes` job; emits GITHUB_OUTPUT flags
    (`reuse`, `reuse_run_id`, `reuse_branch`, `reuse_url`, `reuse_covers_macos`).
  - `wait`: Lightweight `wait-reused-ci` job that polls the chosen run until it
    succeeds or fails closed on failure, cancellation, SHA mismatch, or timeout.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any

ELIGIBLE_BRANCHES = {"main", "dev"}
IN_PROGRESS_STATES = {"queued", "in_progress", "running", "waiting", "pending", "blocked", "requested"}
SUCCESS_STATES = {"success", "completed:success"}
FAILURE_STATES = {
    "failure",
    "cancelled",
    "canceled",
    "timed_out",
    "action_required",
    "stale",
    "skipped",
    "neutral",
    "completed:failure",
    "completed:cancelled",
    "completed:canceled",
    "completed:timed_out",
    "completed:action_required",
    "completed:stale",
    "completed:skipped",
    "completed:neutral",
}
TRANSIENT_HTTP_CODES = {429, 500, 502, 503, 504}


def _auth_header(api_url: str, token: str) -> str:
    host = (urllib.parse.urlparse(api_url).hostname or "").lower()
    if host == "api.github.com" or host.endswith(".github.com"):
        return f"Bearer {token}"
    return f"token {token}"


def _normalize_branch(ref_or_branch: str) -> str:
    value = ref_or_branch.strip()
    if value.startswith("refs/heads/"):
        return value[len("refs/heads/") :]
    return value


def _workflow_matches(workflow_name: str, workflow_file: str) -> bool:
    return workflow_name == workflow_file or workflow_name.endswith(f"/{workflow_file}")


def _extract_event(raw: dict[str, Any]) -> str:
    # Forgejo may leave `event` empty on `workflow_dispatch` while populating `trigger_event`.
    event = str(raw.get("event") or "").strip().lower()
    trigger_event = str(raw.get("trigger_event") or "").strip().lower()
    if trigger_event and event and trigger_event != event:
        return trigger_event
    return event or trigger_event


def normalize_run(raw: dict[str, Any]) -> dict[str, Any]:
    raw_id = raw.get("id")
    run_id = int(raw_id) if isinstance(raw_id, int) or (isinstance(raw_id, str) and raw_id.isdigit()) else 0
    raw_number = raw.get("index_in_repo")
    if raw_number is None:
        raw_number = raw.get("run_number")
    run_number = (
        int(raw_number)
        if isinstance(raw_number, int) or (isinstance(raw_number, str) and raw_number.isdigit())
        else 0
    )
    workflow_id = str(raw.get("workflow_id") or "")
    path = str(raw.get("path") or "")
    workflow = workflow_id or os.path.basename(path)
    sha = str(raw.get("commit_sha") or raw.get("head_sha") or "")
    branch = _normalize_branch(str(raw.get("prettyref") or raw.get("head_branch") or ""))
    event = _extract_event(raw)

    repo_obj = raw.get("head_repository") or raw.get("repository") or {}
    repo_full_name = str(repo_obj.get("full_name") or "") if isinstance(repo_obj, dict) else ""

    status = str(raw.get("status") or "").strip().lower()
    conclusion = str(raw.get("conclusion") or "").strip().lower()
    state = f"{status}:{conclusion}" if status == "completed" and conclusion else status

    return {
        "id": run_id,
        "run_number": run_number,
        "workflow": workflow,
        "sha": sha,
        "branch": branch,
        "event": event,
        "repository": repo_full_name,
        "state": state,
        "url": str(raw.get("html_url") or ""),
    }


def resolve_current_run_id(
    normalized_runs: list[dict[str, Any]],
    *,
    current_run_id: int,
    current_run_number: int,
    commit_sha: str,
    workflow_file: str,
    current_branch: str,
) -> int:
    """Resolve the API's canonical run ID for the active run.

    Guards against runners that report repo-scoped run numbers in GITHUB_RUN_ID.
    """
    if any(run["id"] == current_run_id for run in normalized_runs):
        return current_run_id
    target_number = current_run_number or current_run_id
    if target_number > 0:
        for run in normalized_runs:
            if (
                run["run_number"] == target_number
                and run["sha"] == commit_sha
                and _workflow_matches(run["workflow"], workflow_file)
                and run["event"] == "push"
                and run["branch"] == current_branch
            ):
                return int(run["id"])
    return current_run_id


def choose_reusable_run(
    runs: list[dict[str, Any]],
    *,
    repository: str,
    commit_sha: str,
    workflow_file: str,
    current_branch: str,
    current_run_id: int,
    current_run_number: int = 0,
) -> dict[str, Any] | None:
    """Choose a verified or strictly earlier in-progress push CI run for the same commit."""
    normalized = [normalize_run(raw) for raw in runs]
    canonical_current_id = resolve_current_run_id(
        normalized,
        current_run_id=current_run_id,
        current_run_number=current_run_number,
        commit_sha=commit_sha,
        workflow_file=workflow_file,
        current_branch=current_branch,
    )

    repo_lower = repository.lower()
    candidates: list[dict[str, Any]] = []
    for run in normalized:
        run_id = run["id"]
        if run_id <= 0 or run_id == canonical_current_id:
            continue
        if run["sha"] != commit_sha:
            continue
        if not _workflow_matches(run["workflow"], workflow_file):
            continue
        if run["event"] != "push":
            continue
        if run["branch"] not in ELIGIBLE_BRANCHES:
            continue
        if run["repository"] and run["repository"].lower() != repo_lower:
            continue

        state = run["state"]
        if state in SUCCESS_STATES:
            candidates.append(run)
        elif state in IN_PROGRESS_STATES and run_id < canonical_current_id:
            # Strict total order on run ID prevents circular waiting between concurrent runs.
            candidates.append(run)

    if not candidates:
        return None

    def sort_key(run: dict[str, Any]) -> tuple[bool, bool, int]:
        is_success = run["state"] in SUCCESS_STATES
        covers_macos = run["branch"] == "main"
        prefer_macos = current_branch == "main"
        return (not is_success, not covers_macos if prefer_macos else False, run["id"])

    return min(candidates, key=sort_key)


def _request_json(url: str, *, api_url: str, token: str, timeout: int = 20) -> Any:
    headers = {
        "Accept": "application/json",
        "User-Agent": "stravia-ci-reuse",
    }
    if token:
        headers["Authorization"] = _auth_header(api_url, token)
    req = urllib.request.Request(url, headers=headers)
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read().decode("utf-8"))


def fetch_workflow_runs(
    api_url: str,
    repository: str,
    token: str,
    *,
    page_size: int = 50,
    max_pages: int = 5,
    retries: int = 3,
) -> list[dict[str, Any]]:
    base = api_url.rstrip("/")
    results: list[dict[str, Any]] = []
    for page in range(1, max_pages + 1):
        query = urllib.parse.urlencode({"limit": page_size, "per_page": page_size, "page": page})
        url = f"{base}/repos/{repository}/actions/runs?{query}"
        payload = _fetch_with_retries(url, api_url=api_url, token=token, retries=retries)
        batch = list(payload.get("workflow_runs") or [])
        results.extend(batch)
        if len(batch) < page_size:
            break
    return results


def fetch_single_run(api_url: str, repository: str, run_id: int, token: str) -> dict[str, Any]:
    base = api_url.rstrip("/")
    url = f"{base}/repos/{repository}/actions/runs/{run_id}"
    payload = _request_json(url, api_url=api_url, token=token)
    if not isinstance(payload, dict):
        raise ValueError(f"Unexpected response payload for run {run_id}")
    return payload


def _fetch_with_retries(url: str, *, api_url: str, token: str, retries: int) -> dict[str, Any]:
    last_error: Exception | None = None
    for attempt in range(retries):
        try:
            payload = _request_json(url, api_url=api_url, token=token)
            if not isinstance(payload, dict):
                raise ValueError("Unexpected non-object JSON payload")
            return payload
        except urllib.error.HTTPError as exc:
            if exc.code not in TRANSIENT_HTTP_CODES or attempt + 1 >= retries:
                raise
            last_error = exc
        except (urllib.error.URLError, TimeoutError, json.JSONDecodeError, ValueError) as exc:
            if attempt + 1 >= retries:
                raise
            last_error = exc
        time.sleep(2 * (attempt + 1))
    assert last_error is not None
    raise last_error


def _write_outputs(output_path: str | None, values: dict[str, str]) -> None:
    if not output_path:
        return
    with Path(output_path).open("a", encoding="utf-8") as fh:
        for key, value in values.items():
            fh.write(f"{key}={value}\n")


def _write_summary(summary_path: str | None, line: str) -> None:
    if not summary_path:
        return
    with Path(summary_path).open("a", encoding="utf-8") as fh:
        fh.write(f"{line}\n")


def select_command(args: argparse.Namespace) -> int:
    current_branch = _normalize_branch(args.ref)
    if args.event_name != "push" or args.ref not in {"refs/heads/main", "refs/heads/dev"}:
        _write_outputs(
            args.output,
            {
                "reuse": "false",
                "reuse_run_id": "",
                "reuse_branch": "",
                "reuse_url": "",
                "reuse_covers_macos": "false",
            },
        )
        print(
            f"CI reuse skipped for event={args.event_name!r}, ref={args.ref!r}; running workflow normally.",
            flush=True,
        )
        return 0

    if not args.api_url or not args.repository or not args.commit_sha:
        print("ERROR: --api-url, --repository, and --commit-sha are required for push reuse checks.", file=sys.stderr)
        return 1

    token = os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN") or ""
    try:
        runs = fetch_workflow_runs(
            args.api_url,
            args.repository,
            token,
            page_size=args.page_size,
            max_pages=args.max_pages,
        )
    except Exception as exc:
        print(f"ERROR: failed to query workflow runs for CI reuse: {exc}", file=sys.stderr)
        return 1

    candidate = choose_reusable_run(
        runs,
        repository=args.repository,
        commit_sha=args.commit_sha,
        workflow_file=args.workflow_file,
        current_branch=current_branch,
        current_run_id=args.run_id,
        current_run_number=args.run_number,
    )

    if candidate is None:
        _write_outputs(
            args.output,
            {
                "reuse": "false",
                "reuse_run_id": "",
                "reuse_branch": "",
                "reuse_url": "",
                "reuse_covers_macos": "false",
            },
        )
        msg = f"No reusable CI run found for {args.commit_sha[:12]} on {current_branch}; running full CI."
        print(msg, flush=True)
        _write_summary(args.summary, msg)
        return 0

    covers_macos = candidate["branch"] == "main"
    _write_outputs(
        args.output,
        {
            "reuse": "true",
            "reuse_run_id": str(candidate["id"]),
            "reuse_branch": str(candidate["branch"]),
            "reuse_url": str(candidate["url"]),
            "reuse_covers_macos": "true" if covers_macos else "false",
        },
    )
    run_ref = candidate["url"] or f"run #{candidate['id']}"
    extra = (
        ""
        if covers_macos or current_branch != "main"
        else " (`macos-cross-smoke` will still run on `main`)."
    )
    msg = (
        f"Reusing {candidate['branch']} CI run #{candidate['id']} "
        f"(state={candidate['state']}) for {args.commit_sha[:12]}: {run_ref}{extra}"
    )
    print(msg, flush=True)
    _write_summary(args.summary, msg)
    return 0


def wait_for_reused_run(
    *,
    api_url: str,
    repository: str,
    commit_sha: str,
    run_id: int,
    token: str,
    timeout_seconds: int,
    poll_interval: int,
    summary_path: str | None = None,
    fetch_fn=fetch_single_run,
    sleep_fn=time.sleep,
    monotonic_fn=time.monotonic,
) -> int:
    started = monotonic_fn()
    last_log = ""
    while True:
        elapsed = monotonic_fn() - started
        try:
            raw = fetch_fn(api_url, repository, run_id, token)
        except urllib.error.HTTPError as exc:
            if exc.code not in TRANSIENT_HTTP_CODES or elapsed >= timeout_seconds:
                print(
                    f"ERROR: failed to query reused CI run #{run_id} (HTTP {exc.code}): {exc}",
                    file=sys.stderr,
                )
                return 1
            print(
                f"Transient HTTP {exc.code} querying reused CI run #{run_id}; retrying in {poll_interval}s...",
                flush=True,
            )
            sleep_fn(poll_interval)
            continue
        except (urllib.error.URLError, TimeoutError, json.JSONDecodeError, ValueError) as exc:
            if elapsed >= timeout_seconds:
                print(
                    f"ERROR: failed to query reused CI run #{run_id} before timeout: {exc}",
                    file=sys.stderr,
                )
                return 1
            print(
                f"Transient error querying reused CI run #{run_id} ({exc}); retrying in {poll_interval}s...",
                flush=True,
            )
            sleep_fn(poll_interval)
            continue

        run = normalize_run(raw)
        if run["sha"] != commit_sha:
            print(
                f"ERROR: reused CI run #{run_id} points to {run['sha']!r}, expected {commit_sha!r}.",
                file=sys.stderr,
            )
            return 1

        state = run["state"]
        run_ref = run["url"] or f"run #{run_id}"
        if state in SUCCESS_STATES:
            msg = f"Reused CI run #{run_id} passed for {commit_sha[:12]}: {run_ref}"
            print(msg, flush=True)
            _write_summary(summary_path, msg)
            return 0
        if state in FAILURE_STATES or state not in IN_PROGRESS_STATES:
            print(
                f"ERROR: reused CI run #{run_id} did not succeed (state={state!r}): {run_ref}",
                file=sys.stderr,
            )
            return 1
        if elapsed >= timeout_seconds:
            print(
                f"ERROR: timed out after {timeout_seconds}s waiting for reused CI run #{run_id} ({run_ref}).",
                file=sys.stderr,
            )
            return 1

        status_msg = f"Waiting for reused CI run #{run_id} on {commit_sha[:12]}: state={state} ({run_ref})"
        if status_msg != last_log:
            print(status_msg, flush=True)
            last_log = status_msg
        sleep_fn(poll_interval)


def wait_command(args: argparse.Namespace) -> int:
    if not args.api_url or not args.repository or not args.commit_sha or args.run_id <= 0:
        print("ERROR: --api-url, --repository, --commit-sha, and positive --run-id are required.", file=sys.stderr)
        return 1
    token = os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN") or ""
    return wait_for_reused_run(
        api_url=args.api_url,
        repository=args.repository,
        commit_sha=args.commit_sha,
        run_id=args.run_id,
        token=token,
        timeout_seconds=args.timeout_seconds,
        poll_interval=args.poll_interval,
        summary_path=args.summary,
    )


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Reuse identical-commit CI runs on main/dev pushes.")
    subparsers = parser.add_subparsers(dest="command", required=True)

    select_p = subparsers.add_parser("select", help="Detect a reusable CI run and emit job outputs.")
    select_p.add_argument("--api-url", default=os.environ.get("GITHUB_API_URL", ""))
    select_p.add_argument("--repository", default=os.environ.get("GITHUB_REPOSITORY", ""))
    select_p.add_argument("--commit-sha", default=os.environ.get("GITHUB_SHA", ""))
    select_p.add_argument("--event-name", default=os.environ.get("GITHUB_EVENT_NAME", ""))
    select_p.add_argument("--ref", default=os.environ.get("GITHUB_REF", ""))
    select_p.add_argument("--run-id", type=int, default=int(os.environ.get("GITHUB_RUN_ID", "0") or "0"))
    select_p.add_argument(
        "--run-number", type=int, default=int(os.environ.get("GITHUB_RUN_NUMBER", "0") or "0")
    )
    select_p.add_argument("--workflow-file", default="ci.yml")
    select_p.add_argument("--page-size", type=int, default=50)
    select_p.add_argument("--max-pages", type=int, default=5)
    select_p.add_argument("--output", default=os.environ.get("GITHUB_OUTPUT"))
    select_p.add_argument("--summary", default=os.environ.get("GITHUB_STEP_SUMMARY"))
    select_p.set_defaults(func=select_command)

    wait_p = subparsers.add_parser("wait", help="Wait for a previously selected reusable CI run to pass.")
    wait_p.add_argument("--api-url", default=os.environ.get("GITHUB_API_URL", ""))
    wait_p.add_argument("--repository", default=os.environ.get("GITHUB_REPOSITORY", ""))
    wait_p.add_argument("--commit-sha", default=os.environ.get("GITHUB_SHA", ""))
    wait_p.add_argument("--run-id", type=int, required=True)
    wait_p.add_argument("--timeout-seconds", type=int, default=8400)
    wait_p.add_argument("--poll-interval", type=int, default=15)
    wait_p.add_argument("--summary", default=os.environ.get("GITHUB_STEP_SUMMARY"))
    wait_p.set_defaults(func=wait_command)

    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    return int(args.func(args))


if __name__ == "__main__":
    raise SystemExit(main())
