import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "prepare_cargo_deps.sh"


class PrepareCargoDepsTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmp.name)
        self.log_file = self.tmp / "calls.jsonl"
        self.fake_task = self.tmp / "fake-task"
        self.fake_cargo = self.tmp / "fake-cargo"
        for name, path in (("task", self.fake_task), ("cargo", self.fake_cargo)):
            path.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, sys\n"
                f"with open(os.environ['CALL_LOG'], 'a', encoding='utf-8') as fh:\n"
                f"    fh.write(json.dumps({{'bin': '{name}', 'args': sys.argv[1:]}}) + '\\n')\n",
                encoding="utf-8",
            )
            path.chmod(0o755)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def _run(self, cache_hit: str) -> list[dict[str, object]]:
        if self.log_file.exists():
            self.log_file.unlink()
        env = dict(
            os.environ,
            CARGO_CACHE_HIT=cache_hit,
            TASK_BIN=str(self.fake_task),
            CARGO_BIN=str(self.fake_cargo),
            CALL_LOG=str(self.log_file),
        )
        subprocess.run(
            ["bash", str(SCRIPT)],
            cwd=self.tmp,
            env=env,
            check=True,
        )
        return [
            json.loads(line)
            for line in self.log_file.read_text(encoding="utf-8").splitlines()
            if line.strip()
        ]

    def test_cache_hit_skips_cargo_fetch_without_mutating_patch_mtimes(self) -> None:
        patch_toml = self.tmp / "target" / "patch" / "rustls-0.23.46" / "Cargo.toml"
        patch_toml.parent.mkdir(parents=True)
        patch_toml.write_text("[package]\nname = 'rustls'\n", encoding="utf-8")
        old_time = 1_600_000_000
        os.utime(patch_toml, (old_time, old_time))

        calls = self._run("true")
        self.assertEqual(int(patch_toml.stat().st_mtime), old_time)
        self.assertEqual(calls, [{"bin": "task", "args": ["prepare:crates"]}])

    def test_cache_miss_runs_prepare_and_full_cargo_fetch(self) -> None:
        calls = self._run("false")
        self.assertEqual(
            calls,
            [
                {"bin": "task", "args": ["prepare:crates"]},
                {"bin": "cargo", "args": ["fetch", "--locked"]},
            ],
        )


if __name__ == "__main__":
    unittest.main()
