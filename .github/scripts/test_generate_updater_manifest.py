import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("generate_updater_manifest.py")
VERSION = "1.2.3"
PLATFORMS = {
    "linux-x86_64": f"stravia-desktop-v{VERSION}-linux-x86_64.AppImage",
    "linux-aarch64": f"stravia-desktop-v{VERSION}-linux-aarch64.AppImage",
    "windows-x86_64": f"stravia-desktop-v{VERSION}-windows-x86_64-setup.exe",
    "windows-aarch64": f"stravia-desktop-v{VERSION}-windows-aarch64-setup.exe",
}


class GenerateUpdaterManifestTests(unittest.TestCase):
    def create_artifacts(self, root: Path) -> None:
        for platform, filename in PLATFORMS.items():
            (root / filename).write_bytes(f"artifact:{platform}".encode())
            (root / f"{filename}.sig").write_text(f"signature:{platform}\n", encoding="utf-8")

    def run_generator(self, root: Path) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "--artifacts",
                str(root),
                "--version",
                VERSION,
                "--published-at",
                "2026-09-05T00:00:00Z",
                "--output",
                str(root / "stravia-updater.json"),
            ],
            text=True,
            capture_output=True,
            check=False,
        )

    def test_generates_four_signed_https_platform_entries(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.create_artifacts(root)
            result = self.run_generator(root)
            self.assertEqual(result.returncode, 0, result.stderr)

            manifest = json.loads((root / "stravia-updater.json").read_text(encoding="utf-8"))
            self.assertEqual(manifest["version"], VERSION)
            self.assertEqual(set(manifest["platforms"]), set(PLATFORMS))
            for platform, filename in PLATFORMS.items():
                entry = manifest["platforms"][platform]
                self.assertEqual(entry["signature"], f"signature:{platform}")
                self.assertEqual(
                    entry["url"],
                    f"https://github.com/Stravia-AI/StraviaPlatform/releases/download/v{VERSION}/{filename}",
                )

    def test_fails_when_any_signature_is_missing(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.create_artifacts(root)
            (root / f"{PLATFORMS['windows-aarch64']}.sig").unlink()
            result = self.run_generator(root)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("windows-aarch64", result.stderr)


if __name__ == "__main__":
    unittest.main()
