#!/usr/bin/env python3
"""Wait for an existing CI workflow run on the target commit SHA to succeed.

Used by release.yml so tag releases never duplicate CI execution, while still
blocking release publication until CI passes on the exact same commit.
Supports both Forgejo Actions and GitHub Actions REST API schemas.
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
from typing import Any


IN_PROGRESS_STATES = {"queued", "in_progress", "running", "waiting", "pending", "blocked"}
SUCCESS_STATES = {"success", "completed:success"}
FAILURE_STATES = {
    "failure",
    "cancelled",
    "canceled",
    "timed_out",
    "action_required",
    "stale",
    "completed:failure",
    "completed:cancelled",
    "completed:timed_out",
    "completed:action_required",
    "completed:stale",
}


def normalize_run(run: dict[str, Any]) -> dict[str, str]:
    workflow_id = str(run.get("workflow_id") or "")
    path = str(run.get("path") or "")
    workflow_name = workflow_id or os.path.basename(path)
    sha = str(run.get("commit_sha") or run.get("head_sha") or "")
    status = str(run.get("status") or "").lower()
    conclusion = str(run.get("conclusion") or "").lower()
    state = f"{status}:{conclusion}" if status == "completed" and conclusion else status
    return {
        "id": str(run.get("id") or ""),
        "workflow": workflow_name,
        "sha": sha,
        "state": state,
        "url": str(run.get("html_url") or ""),
    }


def evaluate_ci_runs(
    runs: list[dict[str, Any]],
    *,
    commit_sha: str,
    workflow_file: str,
) -> tuple[str, dict[str, str] | None]:
    """Return ('success' | 'pending' | 'failure' | 'missing', matched_run)."""
    matched: list[dict[str, str]] = []
    for raw in runs:
        norm = normalize_run(raw)
        if norm["sha"] != commit_sha:
            continue
        if norm["workflow"] != workflow_file and not norm["workflow"].endswith(f"/{workflow_file}"):
            continue
        matched.append(norm)

    if not matched:
        return "missing", None

    # Prefer highest run id (latest execution for this commit SHA).
    matched.sort(key=lambda item: int(item["id"]) if item["id"].isdigit() else 0, reverse=True)
    for item in matched:
        if item["state"] in SUCCESS_STATES:
            return "success", item
    latest = matched[0]
    if latest["state"] in IN_PROGRESS_STATES:
        return "pending", latest
    if latest["state"] in FAILURE_STATES:
        return "failure", latest
    return "pending", latest


def fetch_workflow_runs(api_url: str, repository: str, token: str, limit: int = 50) -> list[dict[str, Any]]:
    base = api_url.rstrip("/")
    query = urllib.parse.urlencode({"limit": limit, "per_page": limit})
    url = f"{base}/repos/{repository}/actions/runs?{query}"
    headers = {"Accept": "application/json", "User-Agent": "stravia-release-ci-gate"}
    if token:
        # Forgejo accepts `token <tok>` and `Bearer <tok>`; GitHub accepts `Bearer <tok>`.
        headers["Authorization"] = f"Bearer {token}" if "api.github.com" in base else f"token {token}"
    req = urllib.request.Request(url, headers=headers)
    with urllib.request.urlopen(req, timeout=20) as resp:
        payload = json.loads(resp.read().decode("utf-8"))
    return list(payload.get("workflow_runs") or [])


def wait_for_ci(
    *,
    api_url: str,
    repository: str,
    commit_sha: str,
    workflow_file: str,
    token: str,
    timeout_seconds: int,
    poll_interval: int,
    missing_grace_seconds: int,
) -> int:
    started = time.monotonic()
    last_log = ""
    while True:
        elapsed = time.monotonic() - started
        try:
            runs = fetch_workflow_runs(api_url, repository, token)
        except (urllib.error.URLError, TimeoutError, json.JSONDecodeError) as exc:
            if elapsed >= timeout_seconds:
                print(f"ERROR: failed to query CI status before timeout: {exc}", file=sys.stderr)
                return 1
            print(f"Transient error querying CI runs ({exc}); retrying in {poll_interval}s...", flush=True)
            time.sleep(poll_interval)
            continue

        decision, run = evaluate_ci_runs(runs, commit_sha=commit_sha, workflow_file=workflow_file)
        if decision == "success":
            print(f"CI passed for {commit_sha[:12]}: run #{run['id']} ({run['url']})", flush=True)
            return 0
        if decision == "failure":
            print(
                f"ERROR: CI failed for {commit_sha[:12]}: run #{run['id']} state={run['state']} ({run['url']})",
                file=sys.stderr,
            )
            return 1
        if decision == "missing" and elapsed >= missing_grace_seconds:
            print(
                f"ERROR: no {workflow_file} run found for commit {commit_sha} after {int(elapsed)}s. "
                "Push to main/dev or trigger CI on this commit before releasing.",
                file=sys.stderr,
            )
            return 1
        if elapsed >= timeout_seconds:
            print(
                f"ERROR: timed out after {timeout_seconds}s waiting for {workflow_file} on {commit_sha[:12]}.",
                file=sys.stderr,
            )
            return 1

        status_msg = (
            f"Waiting for {workflow_file} on {commit_sha[:12]}: "
            + (f"run #{run['id']} state={run['state']} ({run['url']})" if run else "no run visible yet")
        )
        if status_msg != last_log:
            print(status_msg, flush=True)
            last_log = status_msg
        time.sleep(poll_interval)


def main() -> int:
    parser = argparse.ArgumentParser(description="Wait for CI workflow to pass on a commit SHA.")
    parser.add_argument("--api-url", default=os.environ.get("GITHUB_API_URL", ""))
    parser.add_argument("--repository", default=os.environ.get("GITHUB_REPOSITORY", ""))
    parser.add_argument("--commit-sha", default=os.environ.get("GITHUB_SHA", ""))
    parser.add_argument("--workflow-file", default="ci.yml")
    parser.add_argument("--timeout-seconds", type=int, default=3600)
    parser.add_argument("--poll-interval", type=int, default=15)
    parser.add_argument("--missing-grace-seconds", type=int, default=90)
    args = parser.parse_args()

    if not args.api_url or not args.repository or not args.commit_sha:
        parser.error("--api-url, --repository, and --commit-sha are required")

    token = os.environ.get("GITHUB_TOKEN", "")
    return wait_for_ci(
        api_url=args.api_url,
        repository=args.repository,
        commit_sha=args.commit_sha,
        workflow_file=args.workflow_file,
        token=token,
        timeout_seconds=args.timeout_seconds,
        poll_interval=args.poll_interval,
        missing_grace_seconds=args.missing_grace_seconds,
    )


if __name__ == "__main__":
    raise SystemExit(main())
