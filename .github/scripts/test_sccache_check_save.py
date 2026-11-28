import os
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "sccache_check_save.sh"


class SccacheCheckSaveTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmp.name)
        self.output_file = self.tmp / "github_output.txt"
        self.fake_sccache = self.tmp / "fake-sccache"

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def _write_fake_sccache(self, stats_output: str) -> None:
        self.fake_sccache.write_text(
            "#!/usr/bin/env bash\n"
            f"cat <<'EOF'\n{stats_output}\nEOF\n",
            encoding="utf-8",
        )
        self.fake_sccache.chmod(0o755)

    def _run(
        self,
        stats_output: str,
        *,
        matched_key: str,
        expected_prefix: str = "sccache-linux-x64-abc123-",
        min_misses: int | None = None,
        extra_env: dict[str, str] | None = None,
    ) -> tuple[str, str]:
        self._write_fake_sccache(stats_output)
        if self.output_file.exists():
            self.output_file.unlink()
        env = dict(
            os.environ,
            SCCACHE_BIN=str(self.fake_sccache),
            CACHE_MATCHED_KEY=matched_key,
            EXPECTED_KEY_PREFIX=expected_prefix,
            GITHUB_OUTPUT=str(self.output_file),
        )
        for remote_var in (
            "SCCACHE_WEBDAV_ENDPOINT",
            "SCCACHE_ENDPOINT",
            "SCCACHE_BUCKET",
            "SCCACHE_REDIS",
            "SCCACHE_REDIS_ENDPOINT",
            "SCCACHE_GCS_BUCKET",
            "SCCACHE_AZURE_CONNECTION_STRING",
            "SCCACHE_GHA_ENABLED",
        ):
            env.pop(remote_var, None)
        if min_misses is not None:
            env["SCCACHE_SAVE_MIN_MISSES"] = str(min_misses)
        if extra_env:
            env.update(extra_env)
        proc = subprocess.run(
            ["bash", str(SCRIPT)],
            env=env,
            capture_output=True,
            text=True,
            check=True,
        )
        outputs = dict(
            line.split("=", 1)
            for line in self.output_file.read_text(encoding="utf-8").splitlines()
            if "=" in line
        )
        return outputs["should_save"], proc.stdout

    def test_skips_save_on_warm_key_with_few_misses(self) -> None:
        stats = "\n".join(
            [
                "Compile requests                   4471",
                "Cache hits                         3395",
                "Cache hits (C/C++)                  966",
                "Cache hits (Rust)                  2429",
                "Cache misses                          6",
                "Cache misses (C/C++)                  2",
                "Cache misses (Rust)                   4",
            ]
        )
        decision, _ = self._run(stats, matched_key="sccache-linux-x64-abc123-2502")
        self.assertEqual(decision, "false")

    def test_saves_on_warm_key_when_misses_meet_threshold(self) -> None:
        stats = "\n".join(
            [
                "Cache hits                          936",
                "Cache misses                        588",
                "Cache misses (Rust)                 587",
            ]
        )
        decision, _ = self._run(stats, matched_key="sccache-linux-x64-abc123-2506")
        self.assertEqual(decision, "true")

    def test_saves_when_matched_key_is_from_previous_lock_or_empty(self) -> None:
        stats = "\n".join(
            [
                "Cache hits                         1200",
                "Cache misses                          2",
            ]
        )
        for matched_key in ("", "sccache-linux-x64-oldhash-2400"):
            with self.subTest(matched_key=matched_key):
                decision, _ = self._run(stats, matched_key=matched_key)
                self.assertEqual(decision, "true")

    def test_skips_save_when_zero_compilations_ran(self) -> None:
        stats = "\n".join(
            [
                "Compile requests                      0",
                "Cache hits                            0",
                "Cache misses                          0",
            ]
        )
        decision, _ = self._run(stats, matched_key="")
        self.assertEqual(decision, "false")

    def test_skips_save_when_remote_sccache_backend_is_active(self) -> None:
        stats = "\n".join(
            [
                "Cache hits                          100",
                "Cache misses                        500",
            ]
        )
        for extra_env in (
            {"SCCACHE_WEBDAV_ENDPOINT": "http://192.168.123.100:18426"},
            {"SCCACHE_ENDPOINT": "http://127.0.0.1:9000", "SCCACHE_BUCKET": "sccache"},
            {"SCCACHE_GHA_ENABLED": "true"},
        ):
            with self.subTest(extra_env=extra_env):
                decision, stdout = self._run(
                    stats,
                    matched_key="",
                    extra_env=extra_env,
                )
                self.assertEqual(decision, "false")
                self.assertIn("remote sccache backend is active", stdout)


if __name__ == "__main__":
    unittest.main()
