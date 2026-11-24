import json
import os
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

WRAPPER = Path(__file__).resolve().parent / "xwin_clang_wrapper.sh"


class XwinClangWrapperTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmp.name)
        self.fake_clang = self.tmp / "fake-clang"
        self.fake_clang.write_text(
            "#!/usr/bin/env python3\n"
            "import json, sys\n"
            "print(json.dumps(sys.argv[1:]))\n",
            encoding="utf-8",
        )
        self.fake_clang.chmod(0o755)
        self.shim_clang = self.tmp / "clang"
        self.shim_clang.write_bytes(WRAPPER.read_bytes())
        self.shim_clang.chmod(0o755)
        self.shim_clang_cl = self.tmp / "clang-cl"
        self.shim_clang_cl.symlink_to(self.shim_clang)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def _run(self, binary: Path, args: list[str]) -> list[str]:
        env = dict(os.environ, STRAVIA_REAL_CLANG=str(self.fake_clang))
        proc = subprocess.run(
            [str(binary), *args],
            env=env,
            capture_output=True,
            text=True,
            check=True,
        )
        return json.loads(proc.stdout)

    def test_translates_split_and_attached_imsvc_flags_for_clang(self) -> None:
        got = self._run(
            self.shim_clang,
            [
                "--target=aarch64-pc-windows-msvc",
                "-O3",
                "/imsvc",
                "/cache/xwin/crt/include",
                "/imsvc",
                "/cache with spaces/sdk/include/ucrt",
                "/imsvc/cache/xwin/sdk/include/um",
                "-c",
                "curve25519.c",
            ],
        )
        self.assertEqual(
            got,
            [
                "--target=aarch64-pc-windows-msvc",
                "-O3",
                "-isystem",
                "/cache/xwin/crt/include",
                "-isystem",
                "/cache with spaces/sdk/include/ucrt",
                "-isystem",
                "/cache/xwin/sdk/include/um",
                "-c",
                "curve25519.c",
            ],
        )

    def test_passthrough_when_no_imsvc_present(self) -> None:
        got = self._run(self.shim_clang, ["--version"])
        self.assertEqual(got, ["--version"])

    def test_preserves_clang_cl_driver_mode_and_imsvc(self) -> None:
        got = self._run(
            self.shim_clang_cl,
            ["--target=aarch64-pc-windows-msvc", "/imsvc", "/cache/xwin/crt/include"],
        )
        self.assertEqual(
            got,
            [
                "--driver-mode=cl",
                "--target=aarch64-pc-windows-msvc",
                "/imsvc",
                "/cache/xwin/crt/include",
            ],
        )


if __name__ == "__main__":
    unittest.main()
