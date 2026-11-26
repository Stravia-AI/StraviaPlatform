import unittest

import wait_for_ci


class WaitForCiTests(unittest.TestCase):
    def test_forgejo_success_run_passes(self) -> None:
        runs = [
            {
                "id": 2501,
                "workflow_id": "ci.yml",
                "commit_sha": "abc123",
                "status": "success",
                "html_url": "https://git.stravia.cn/Stravia/StraviaPlatform/actions/runs/55",
            }
        ]
        decision, run = wait_for_ci.evaluate_ci_runs(runs, commit_sha="abc123", workflow_file="ci.yml")
        self.assertEqual(decision, "success")
        self.assertEqual(run["id"], "2501")

    def test_ignores_other_workflows_and_commits(self) -> None:
        runs = [
            {"id": 10, "workflow_id": "release.yml", "commit_sha": "abc123", "status": "running"},
            {"id": 11, "workflow_id": "ci.yml", "commit_sha": "other", "status": "success"},
        ]
        decision, run = wait_for_ci.evaluate_ci_runs(runs, commit_sha="abc123", workflow_file="ci.yml")
        self.assertEqual(decision, "missing")
        self.assertIsNone(run)

    def test_running_then_failure_and_github_schema(self) -> None:
        running = [{"id": 20, "path": ".github/workflows/ci.yml", "head_sha": "abc123", "status": "in_progress"}]
        decision, _ = wait_for_ci.evaluate_ci_runs(running, commit_sha="abc123", workflow_file="ci.yml")
        self.assertEqual(decision, "pending")

        failed = [
            {
                "id": 21,
                "path": ".github/workflows/ci.yml",
                "head_sha": "abc123",
                "status": "completed",
                "conclusion": "failure",
            }
        ]
        decision, _ = wait_for_ci.evaluate_ci_runs(failed, commit_sha="abc123", workflow_file="ci.yml")
        self.assertEqual(decision, "failure")

    def test_earlier_failure_overridden_by_later_success(self) -> None:
        runs = [
            {"id": 30, "workflow_id": "ci.yml", "commit_sha": "abc123", "status": "failure"},
            {"id": 31, "workflow_id": "ci.yml", "commit_sha": "abc123", "status": "success"},
        ]
        decision, run = wait_for_ci.evaluate_ci_runs(runs, commit_sha="abc123", workflow_file="ci.yml")
        self.assertEqual(decision, "success")
        self.assertEqual(run["id"], "31")


if __name__ == "__main__":
    unittest.main()
