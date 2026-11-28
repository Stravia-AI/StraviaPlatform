import tempfile
import unittest
import urllib.error
from pathlib import Path
from unittest import mock

import reuse_ci


def forgejo_run(
    run_id: int,
    *,
    index: int | None = None,
    workflow_id: str = "ci.yml",
    sha: str = "6e53026a78e17495506e9523bbe12215ac8349ec",
    branch: str = "dev",
    event: str = "push",
    trigger_event: str = "push",
    status: str = "running",
    repo: str = "Stravia/StraviaPlatform",
) -> dict:
    run_num = index if index is not None else run_id
    return {
        "id": run_id,
        "index_in_repo": run_num,
        "workflow_id": workflow_id,
        "commit_sha": sha,
        "prettyref": branch,
        "event": event,
        "trigger_event": trigger_event,
        "status": status,
        "html_url": f"https://git.stravia.cn/{repo}/actions/runs/{run_num}",
        "repository": {"full_name": repo},
    }


def github_run(
    run_id: int,
    *,
    run_number: int | None = None,
    path: str = ".github/workflows/ci.yml",
    sha: str = "6e53026a78e17495506e9523bbe12215ac8349ec",
    branch: str = "dev",
    event: str = "push",
    status: str = "completed",
    conclusion: str | None = "success",
    repo: str = "Stravia-AI/StraviaPlatform",
) -> dict:
    return {
        "id": run_id,
        "run_number": run_number if run_number is not None else run_id,
        "path": path,
        "head_sha": sha,
        "head_branch": branch,
        "event": event,
        "status": status,
        "conclusion": conclusion,
        "html_url": f"https://github.com/{repo}/actions/runs/{run_id}",
        "head_repository": {"full_name": repo},
    }


class ChooseReusableRunTests(unittest.TestCase):
    SHA = "6e53026a78e17495506e9523bbe12215ac8349ec"
    REPO = "Stravia/StraviaPlatform"

    def test_simultaneous_dev_and_main_push_breaks_tie_without_deadlock(self) -> None:
        runs = [
            forgejo_run(2570, index=78, workflow_id="release.yml", branch="v0.7.9-beta.1", status="waiting"),
            forgejo_run(2569, index=77, branch="main", status="running"),
            forgejo_run(2568, index=76, branch="dev", status="running"),
        ]

        # Earlier run (dev #2568) does not wait on newer run (main #2569).
        dev_choice = reuse_ci.choose_reusable_run(
            runs,
            repository=self.REPO,
            commit_sha=self.SHA,
            workflow_file="ci.yml",
            current_branch="dev",
            current_run_id=2568,
            current_run_number=76,
        )
        self.assertIsNone(dev_choice)

        # Later run (main #2569) reuses earlier dev #2568.
        main_choice = reuse_ci.choose_reusable_run(
            runs,
            repository=self.REPO,
            commit_sha=self.SHA,
            workflow_file="ci.yml",
            current_branch="main",
            current_run_id=2569,
            current_run_number=77,
        )
        self.assertIsNotNone(main_choice)
        assert main_choice is not None
        self.assertEqual(main_choice["id"], 2568)
        self.assertEqual(main_choice["branch"], "dev")

    def test_dev_reuses_earlier_main_run(self) -> None:
        runs = [
            forgejo_run(2569, index=77, branch="dev", status="running"),
            forgejo_run(2568, index=76, branch="main", status="waiting"),
        ]
        choice = reuse_ci.choose_reusable_run(
            runs,
            repository=self.REPO,
            commit_sha=self.SHA,
            workflow_file="ci.yml",
            current_branch="dev",
            current_run_id=2569,
            current_run_number=77,
        )
        self.assertIsNotNone(choice)
        assert choice is not None
        self.assertEqual(choice["id"], 2568)
        self.assertEqual(choice["branch"], "main")

    def test_main_prefers_completed_main_over_completed_dev(self) -> None:
        runs = [
            forgejo_run(2572, index=80, branch="main", status="running"),
            forgejo_run(2571, index=79, branch="main", status="success"),
            forgejo_run(2568, index=76, branch="dev", status="success"),
        ]
        choice = reuse_ci.choose_reusable_run(
            runs,
            repository=self.REPO,
            commit_sha=self.SHA,
            workflow_file="ci.yml",
            current_branch="main",
            current_run_id=2572,
            current_run_number=80,
        )
        self.assertIsNotNone(choice)
        assert choice is not None
        self.assertEqual(choice["id"], 2571)
        self.assertEqual(choice["branch"], "main")

    def test_already_succeeded_run_with_larger_id_is_still_reusable(self) -> None:
        runs = [
            forgejo_run(2575, index=83, branch="dev", status="success"),
            forgejo_run(2569, index=77, branch="main", status="running"),
        ]
        choice = reuse_ci.choose_reusable_run(
            runs,
            repository=self.REPO,
            commit_sha=self.SHA,
            workflow_file="ci.yml",
            current_branch="main",
            current_run_id=2569,
            current_run_number=77,
        )
        self.assertIsNotNone(choice)
        assert choice is not None
        self.assertEqual(choice["id"], 2575)

    def test_rejects_non_push_other_branches_other_repos_and_non_passing_runs(self) -> None:
        runs = [
            # Forgejo workflow_dispatch often leaves event="" and sets trigger_event="workflow_dispatch".
            forgejo_run(2501, branch="dev", event="", trigger_event="workflow_dispatch", status="success"),
            forgejo_run(2502, branch="main", event="pull_request", trigger_event="pull_request", status="success"),
            forgejo_run(2503, branch="feature/x", status="success"),
            forgejo_run(2504, branch="v0.7.9-beta.1", status="success"),
            forgejo_run(2505, workflow_id="release.yml", branch="main", status="success"),
            forgejo_run(2506, sha="deadbeef", branch="main", status="success"),
            forgejo_run(2507, repo="fork/StraviaPlatform", branch="main", status="success"),
            forgejo_run(2508, branch="dev", status="failure"),
            forgejo_run(2509, branch="dev", status="cancelled"),
            forgejo_run(2510, branch="dev", status="skipped"),
            forgejo_run(2520, branch="main", status="running"),
        ]
        choice = reuse_ci.choose_reusable_run(
            runs,
            repository=self.REPO,
            commit_sha=self.SHA,
            workflow_file="ci.yml",
            current_branch="main",
            current_run_id=2520,
        )
        self.assertIsNone(choice)

    def test_resolves_repo_scoped_run_number_when_run_id_matches_index_in_repo(self) -> None:
        runs = [
            forgejo_run(2569, index=77, branch="main", status="running"),
            forgejo_run(2568, index=76, branch="dev", status="running"),
        ]
        # Even if a runner reports 77 as GITHUB_RUN_ID, it resolves to canonical id 2569.
        choice = reuse_ci.choose_reusable_run(
            runs,
            repository=self.REPO,
            commit_sha=self.SHA,
            workflow_file="ci.yml",
            current_branch="main",
            current_run_id=77,
            current_run_number=77,
        )
        self.assertIsNotNone(choice)
        assert choice is not None
        self.assertEqual(choice["id"], 2568)

    def test_github_actions_schema_supported(self) -> None:
        runs = [
            github_run(1002, branch="main", status="in_progress", conclusion=None),
            github_run(1001, branch="dev", status="completed", conclusion="success"),
        ]
        choice = reuse_ci.choose_reusable_run(
            runs,
            repository="Stravia-AI/StraviaPlatform",
            commit_sha=self.SHA,
            workflow_file="ci.yml",
            current_branch="main",
            current_run_id=1002,
        )
        self.assertIsNotNone(choice)
        assert choice is not None
        self.assertEqual(choice["id"], 1001)
        self.assertEqual(choice["state"], "completed:success")


class SelectCommandTests(unittest.TestCase):
    SHA = "6e53026a78e17495506e9523bbe12215ac8349ec"

    def _run_select(self, argv: list[str]) -> tuple[int, dict[str, str], str]:
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "output"
            summary = Path(tmp) / "summary"
            code = reuse_ci.main(["select", *argv, "--output", str(out), "--summary", str(summary)])
            outputs = {}
            if out.exists():
                for line in out.read_text(encoding="utf-8").splitlines():
                    k, v = line.split("=", 1)
                    outputs[k] = v
            summary_text = summary.read_text(encoding="utf-8") if summary.exists() else ""
            return code, outputs, summary_text

    def test_pr_and_dispatch_never_reuse_and_do_not_query_api(self) -> None:
        for event, ref in [
            ("pull_request", "refs/pull/12/merge"),
            ("workflow_dispatch", "refs/heads/main"),
            ("workflow_call", "refs/heads/main"),
            ("push", "refs/tags/v0.7.9-beta.1"),
            ("push", "refs/heads/feature-branch"),
        ]:
            with mock.patch.object(reuse_ci, "fetch_workflow_runs", side_effect=AssertionError("should not query")):
                code, outputs, _ = self._run_select(
                    [
                        "--api-url",
                        "https://git.stravia.cn/api/v1",
                        "--repository",
                        "Stravia/StraviaPlatform",
                        "--commit-sha",
                        self.SHA,
                        "--event-name",
                        event,
                        "--ref",
                        ref,
                        "--run-id",
                        "2569",
                    ]
                )
            self.assertEqual(code, 0)
            self.assertEqual(outputs["reuse"], "false")
            self.assertEqual(outputs["reuse_covers_macos"], "false")

    def test_main_reusing_dev_emits_reuse_true_and_covers_macos_false(self) -> None:
        runs = [
            forgejo_run(2569, index=77, branch="main", status="running"),
            forgejo_run(2568, index=76, branch="dev", status="running"),
        ]
        with mock.patch.object(reuse_ci, "fetch_workflow_runs", return_value=runs):
            code, outputs, summary = self._run_select(
                [
                    "--api-url",
                    "https://git.stravia.cn/api/v1",
                    "--repository",
                    "Stravia/StraviaPlatform",
                    "--commit-sha",
                    self.SHA,
                    "--event-name",
                    "push",
                    "--ref",
                    "refs/heads/main",
                    "--run-id",
                    "2569",
                    "--run-number",
                    "77",
                ]
            )
        self.assertEqual(code, 0)
        self.assertEqual(outputs["reuse"], "true")
        self.assertEqual(outputs["reuse_run_id"], "2568")
        self.assertEqual(outputs["reuse_branch"], "dev")
        self.assertEqual(outputs["reuse_covers_macos"], "false")
        self.assertIn("macos-cross-smoke", summary)

    def test_main_reusing_main_emits_covers_macos_true(self) -> None:
        runs = [
            forgejo_run(2571, index=79, branch="main", status="running"),
            forgejo_run(2569, index=77, branch="main", status="success"),
        ]
        with mock.patch.object(reuse_ci, "fetch_workflow_runs", return_value=runs):
            code, outputs, _ = self._run_select(
                [
                    "--api-url",
                    "https://git.stravia.cn/api/v1",
                    "--repository",
                    "Stravia/StraviaPlatform",
                    "--commit-sha",
                    self.SHA,
                    "--event-name",
                    "push",
                    "--ref",
                    "refs/heads/main",
                    "--run-id",
                    "2571",
                ]
            )
        self.assertEqual(code, 0)
        self.assertEqual(outputs["reuse"], "true")
        self.assertEqual(outputs["reuse_run_id"], "2569")
        self.assertEqual(outputs["reuse_covers_macos"], "true")

    def test_api_failure_fails_closed(self) -> None:
        with mock.patch.object(reuse_ci, "fetch_workflow_runs", side_effect=urllib.error.URLError("down")):
            code, _, _ = self._run_select(
                [
                    "--api-url",
                    "https://git.stravia.cn/api/v1",
                    "--repository",
                    "Stravia/StraviaPlatform",
                    "--commit-sha",
                    self.SHA,
                    "--event-name",
                    "push",
                    "--ref",
                    "refs/heads/main",
                    "--run-id",
                    "2569",
                ]
            )
        self.assertEqual(code, 1)


class WaitForReusedRunTests(unittest.TestCase):
    SHA = "6e53026a78e17495506e9523bbe12215ac8349ec"

    def test_waits_through_transient_error_and_in_progress_until_success(self) -> None:
        responses: list[object] = [
            forgejo_run(2568, status="waiting"),
            urllib.error.HTTPError("https://git.stravia.cn", 503, "Service Unavailable", {}, None),
            forgejo_run(2568, status="running"),
            forgejo_run(2568, status="success"),
        ]
        clock = [0.0]

        def fake_fetch(_api, _repo, _run_id, _tok):
            item = responses.pop(0)
            if isinstance(item, Exception):
                raise item
            return item

        def fake_sleep(seconds: float) -> None:
            clock[0] += seconds

        code = reuse_ci.wait_for_reused_run(
            api_url="https://git.stravia.cn/api/v1",
            repository="Stravia/StraviaPlatform",
            commit_sha=self.SHA,
            run_id=2568,
            token="tok",
            timeout_seconds=120,
            poll_interval=10,
            fetch_fn=fake_fetch,
            sleep_fn=fake_sleep,
            monotonic_fn=lambda: clock[0],
        )
        self.assertEqual(code, 0)

    def test_fails_closed_on_cancelled_failed_sha_mismatch_404_and_timeout(self) -> None:
        for terminal in [
            forgejo_run(2568, status="cancelled"),
            forgejo_run(2568, status="failure"),
            github_run(2568, status="completed", conclusion="cancelled"),
            forgejo_run(2568, sha="different", status="success"),
        ]:
            code = reuse_ci.wait_for_reused_run(
                api_url="https://git.stravia.cn/api/v1",
                repository="Stravia/StraviaPlatform",
                commit_sha=self.SHA,
                run_id=2568,
                token="tok",
                timeout_seconds=60,
                poll_interval=5,
                fetch_fn=lambda *_args, _t=terminal: _t,
                sleep_fn=lambda _s: None,
                monotonic_fn=lambda: 0.0,
            )
            self.assertEqual(code, 1)

        # HTTP 404 fails immediately without looping until timeout.
        calls = [0]

        def fetch_404(*_args):
            calls[0] += 1
            raise urllib.error.HTTPError("https://git.stravia.cn", 404, "Not Found", {}, None)

        code = reuse_ci.wait_for_reused_run(
            api_url="https://git.stravia.cn/api/v1",
            repository="Stravia/StraviaPlatform",
            commit_sha=self.SHA,
            run_id=2568,
            token="tok",
            timeout_seconds=600,
            poll_interval=15,
            fetch_fn=fetch_404,
            sleep_fn=lambda _s: None,
            monotonic_fn=lambda: 0.0,
        )
        self.assertEqual(code, 1)
        self.assertEqual(calls[0], 1)

        # Timeout fails closed.
        clock = [0.0]

        def advance(s: float) -> None:
            clock[0] += s

        code = reuse_ci.wait_for_reused_run(
            api_url="https://git.stravia.cn/api/v1",
            repository="Stravia/StraviaPlatform",
            commit_sha=self.SHA,
            run_id=2568,
            token="tok",
            timeout_seconds=30,
            poll_interval=15,
            fetch_fn=lambda *_args: forgejo_run(2568, status="running"),
            sleep_fn=advance,
            monotonic_fn=lambda: clock[0],
        )
        self.assertEqual(code, 1)


class PaginationTests(unittest.TestCase):
    def test_fetch_workflow_runs_paginates_until_short_page(self) -> None:
        page1 = {"workflow_runs": [forgejo_run(3000 - i) for i in range(50)]}
        page2 = {"workflow_runs": [forgejo_run(2900), forgejo_run(2899)]}
        pages = [page1, page2]

        with mock.patch.object(reuse_ci, "_fetch_with_retries", side_effect=pages) as patched:
            runs = reuse_ci.fetch_workflow_runs(
                "https://git.stravia.cn/api/v1",
                "Stravia/StraviaPlatform",
                "tok",
                page_size=50,
                max_pages=5,
            )
        self.assertEqual(len(runs), 52)
        self.assertEqual(patched.call_count, 2)


if __name__ == "__main__":
    unittest.main()
