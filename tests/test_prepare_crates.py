from __future__ import annotations

import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "patches" / "cargo" / "prepare.py"
CRATE = "example-crate"
VERSION = "1.2.3"
ARCHIVE_ROOT = f"{CRATE}-{VERSION}"
PATCH = """\
diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1 +1 @@
-pub const MESSAGE: &str = \"original\";
+pub const MESSAGE: &str = \"patched\";
"""


class PrepareCratesCliTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        subprocess.run(["git", "init", "-q", str(self.root)], check=True, capture_output=True)
        self.patch_dir = self.root / "patches" / "cargo"
        self.patch_dir.mkdir(parents=True)
        shutil.copy2(SCRIPT, self.patch_dir / "prepare.py")
        self.archive = self.root / f"{ARCHIVE_ROOT}.crate"
        self.patch = self.patch_dir / f"{CRATE}@{VERSION}.patch"
        self.patch.write_text(PATCH, encoding="utf-8")
        self.output = self.root / ".stravia-patched-crates" / f"{ARCHIVE_ROOT}-shape2"
        self.env = os.environ.copy()
        self.env["CARGO_HOME"] = str(self.root / "cargo-home")
        self.make_archive()
        self.write_manifest()

    def make_archive(
        self, extra: list[tarfile.TarInfo] | None = None, *,
        crate: str = CRATE, archive_path: Path | None = None,
    ) -> None:
        archive_root = f"{crate}-{VERSION}"
        with tarfile.open(archive_path or self.archive, "w:gz") as archive:
            files = {
                f"{archive_root}/Cargo.toml": (
                    f'[package]\nname = "{crate}"\nversion = "{VERSION}"\n'
                ).encode(),
                f"{archive_root}/src/lib.rs": b'pub const MESSAGE: &str = "original";\n',
            }
            for name, data in files.items():
                info = tarfile.TarInfo(name)
                info.size = len(data)
                archive.addfile(info, io.BytesIO(data))
            for info in extra or []:
                archive.addfile(info, io.BytesIO(b"x" * info.size) if info.isfile() else None)

    def write_manifest(self, checksum: str | None = None, revision: int = 2) -> None:
        (self.patch_dir / "crates.json").write_text(
            json.dumps([{
                "crate": CRATE,
                "version": VERSION,
                "directory": f"{ARCHIVE_ROOT}-shape{revision}",
                "sha256": checksum or hashlib.sha256(self.archive.read_bytes()).hexdigest(),
                "patch": self.patch.name,
            }]),
            encoding="utf-8",
        )

    def add_second_crate(self) -> tuple[str, Path, Path]:
        crate = "another-crate"
        archive_root = f"{crate}-{VERSION}"
        archive = self.root / f"{archive_root}.crate"
        self.make_archive(crate=crate, archive_path=archive)
        manifest = self.patch_dir / "crates.json"
        entries = json.loads(manifest.read_text(encoding="utf-8"))
        entries.append({
            "crate": crate,
            "version": VERSION,
            "directory": f"{archive_root}-shape1",
            "sha256": hashlib.sha256(archive.read_bytes()).hexdigest(),
            "patch": self.patch.name,
        })
        manifest.write_text(json.dumps(entries), encoding="utf-8")
        return crate, archive, self.output.parent / f"{archive_root}-shape1"

    def command(self, *args: str) -> list[str]:
        return [sys.executable, str(self.patch_dir / "prepare.py"), *args]

    def run_cli(self, *args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            self.command(*args), cwd=self.root, env=self.env,
            text=True, capture_output=True, timeout=30, check=False,
        )

    def assert_succeeded(self, result: subprocess.CompletedProcess[str]) -> None:
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def assert_failed(self, result: subprocess.CompletedProcess[str]) -> None:
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue((result.stdout + result.stderr).strip(), "failure should be actionable")

    def test_offline_archive_is_patched_and_check_detects_stale_patch(self) -> None:
        args = ("--offline", "--crate-file", f"{CRATE}={self.archive}")
        self.assert_failed(self.run_cli(*args, "--check"))
        self.assertFalse(self.output.exists())
        self.assert_succeeded(self.run_cli(*args))
        source = self.output / "src" / "lib.rs"
        self.assertEqual(source.read_text(encoding="utf-8"), 'pub const MESSAGE: &str = "patched";\n')
        stamp = self.output / ".prepare-stamp.json"
        before = stamp.read_bytes()
        self.assert_succeeded(self.run_cli(*args, "--check"))
        self.assert_succeeded(self.run_cli(*args))
        self.assertEqual(stamp.read_bytes(), before)
        self.patch.write_text(PATCH.replace('"patched"', '"updated"'), encoding="utf-8")
        self.assert_failed(self.run_cli(*args, "--check"))
        self.assertEqual(stamp.read_bytes(), before)
        self.assertEqual(source.read_text(encoding="utf-8"), 'pub const MESSAGE: &str = "patched";\n')
        self.assert_failed(self.run_cli(*args))
        self.assertEqual(source.read_text(encoding="utf-8"), 'pub const MESSAGE: &str = "patched";\n')
        self.assertEqual(stamp.read_bytes(), before)
        self.write_manifest(revision=3)
        self.assert_succeeded(self.run_cli(*args))
        revised = self.output.parent / f"{ARCHIVE_ROOT}-shape3" / "src" / "lib.rs"
        self.assertEqual(revised.read_text(encoding="utf-8"), 'pub const MESSAGE: &str = "updated";\n')
        self.assertEqual(source.read_text(encoding="utf-8"), 'pub const MESSAGE: &str = "patched";\n')

    def test_offline_uses_registry_cache(self) -> None:
        cache = Path(self.env["CARGO_HOME"]) / "registry" / "cache" / "custom-registry"
        cache.mkdir(parents=True)
        shutil.copy2(self.archive, cache / self.archive.name)
        self.assert_succeeded(self.run_cli("--offline"))
        self.assertEqual(
            (self.output / "src" / "lib.rs").read_text(encoding="utf-8"),
            'pub const MESSAGE: &str = "patched";\n',
        )

    def test_repeated_explicit_archives_prepare_both_crates(self) -> None:
        crate, archive, output = self.add_second_crate()
        args = ("--offline", "--crate-file", f"{CRATE}={self.archive}",
                "--crate-file", f"{crate}={archive}")
        self.assert_succeeded(self.run_cli(*args))
        self.assert_succeeded(self.run_cli(*args, "--check"))
        for directory in (self.output, output):
            self.assertEqual(
                (directory / "src" / "lib.rs").read_text(encoding="utf-8"),
                'pub const MESSAGE: &str = "patched";\n',
            )

    def test_explicit_archive_and_registry_cache_are_selected_per_crate(self) -> None:
        crate, archive, output = self.add_second_crate()
        cache = Path(self.env["CARGO_HOME"]) / "registry" / "cache" / "custom-registry"
        cache.mkdir(parents=True)
        shutil.copy2(archive, cache / archive.name)
        (cache / self.archive.name).write_bytes(b"bad cached archive")
        args = ("--offline", "--crate-file", f"{CRATE}={self.archive}")
        self.assert_succeeded(self.run_cli(*args))
        self.assert_succeeded(self.run_cli(*args, "--check"))
        for directory in (self.output, output):
            self.assertEqual(
                (directory / "src" / "lib.rs").read_text(encoding="utf-8"),
                'pub const MESSAGE: &str = "patched";\n',
            )

    def test_invalid_mappings_fail_before_preparing_any_crate(self) -> None:
        crate, archive, output = self.add_second_crate()
        cache = Path(self.env["CARGO_HOME"]) / "registry" / "cache" / "custom-registry"
        cache.mkdir(parents=True)
        shutil.copy2(archive, cache / archive.name)
        missing = self.root / "missing.crate"
        bad = self.root / "bad.crate"
        bad.write_bytes(b"wrong checksum")
        invalid_options = (
            ("--crate-file", str(self.archive)),
            ("--crate-file", f"unknown={self.archive}"),
            ("--crate-file", f"{CRATE}={self.archive}", "--crate-file", f"{CRATE}={self.archive}"),
            ("--crate-file", f"{crate}={missing}"),
            ("--crate-file", f"{crate}={bad}"),
        )
        for options in invalid_options:
            with self.subTest(options=options):
                self.assert_failed(self.run_cli("--offline", "--crate-file", f"{CRATE}={self.archive}", *options))
                self.assertFalse(self.output.exists())
                self.assertFalse(output.exists())

    def test_explicit_archive_is_checked_even_when_output_is_current(self) -> None:
        args = ("--offline", "--crate-file", f"{CRATE}={self.archive}")
        self.assert_succeeded(self.run_cli(*args))
        source = self.output / "src" / "lib.rs"
        stamp = self.output / ".prepare-stamp.json"
        before = stamp.read_bytes()
        self.archive.write_bytes(b"wrong checksum")
        for extra in ((), ("--check",)):
            with self.subTest(extra=extra):
                self.assert_failed(self.run_cli(*args, *extra))
                self.assertEqual(stamp.read_bytes(), before)
                self.assertEqual(source.read_text(encoding="utf-8"), 'pub const MESSAGE: &str = "patched";\n')
        self.archive.unlink()
        self.assert_failed(self.run_cli(*args, "--check"))
        self.assertEqual(stamp.read_bytes(), before)

    def test_explicit_bad_archive_is_not_replaced_by_valid_cache(self) -> None:
        cache = Path(self.env["CARGO_HOME"]) / "registry" / "cache" / "custom-registry"
        cache.mkdir(parents=True)
        shutil.copy2(self.archive, cache / self.archive.name)
        invalid = self.root / "invalid.crate"
        invalid.write_bytes(b"invalid archive")
        self.assert_failed(self.run_cli("--offline", "--crate-file", f"{CRATE}={invalid}"))
        self.assertFalse(self.output.exists())

    def test_corrupt_prepared_file_fails_without_replacement(self) -> None:
        args = ("--offline", "--crate-file", f"{CRATE}={self.archive}")
        self.assert_succeeded(self.run_cli(*args))
        source = self.output / "src" / "lib.rs"
        source.write_text("corrupted\n", encoding="utf-8")
        self.assert_failed(self.run_cli(*args, "--check"))
        self.assert_failed(self.run_cli(*args))
        self.assertEqual(source.read_text(encoding="utf-8"), "corrupted\n")
        self.write_manifest(revision=3)
        self.assert_succeeded(self.run_cli(*args))
        revised = self.output.parent / f"{ARCHIVE_ROOT}-shape3" / "src" / "lib.rs"
        self.assertEqual(revised.read_text(encoding="utf-8"), 'pub const MESSAGE: &str = "patched";\n')
        self.assertEqual(source.read_text(encoding="utf-8"), "corrupted\n")
        self.assert_succeeded(self.run_cli(*args, "--check"))

    def test_changed_script_requires_new_revision(self) -> None:
        args = ("--offline", "--crate-file", f"{CRATE}={self.archive}")
        self.assert_succeeded(self.run_cli(*args))
        script = self.patch_dir / "prepare.py"
        with script.open("a", encoding="utf-8") as file:
            file.write("\n# changed\n")
        self.assert_failed(self.run_cli(*args, "--check"))
        self.assert_failed(self.run_cli(*args))
        self.write_manifest(revision=3)
        self.assert_succeeded(self.run_cli(*args))
        self.assert_succeeded(self.run_cli(*args, "--check"))

    def test_offline_without_archive_fails_without_output(self) -> None:
        result = self.run_cli("--offline")
        self.assert_failed(result)
        self.assertFalse(self.output.exists())

    def test_bad_checksum_fails_without_output(self) -> None:
        self.write_manifest("0" * 64)
        result = self.run_cli("--offline", "--crate-file", f"{CRATE}={self.archive}")
        self.assert_failed(result)
        self.assertFalse(self.output.exists())

    def test_invalid_patch_does_not_publish_output(self) -> None:
        self.patch.write_text(PATCH.replace("original", "not-in-source"), encoding="utf-8")
        result = self.run_cli("--offline", "--crate-file", f"{CRATE}={self.archive}")
        self.assert_failed(result)
        self.assertFalse(self.output.exists())

    def test_malformed_archive_is_rejected(self) -> None:
        self.archive.write_bytes(b"not a crate archive")
        self.write_manifest()
        result = self.run_cli("--offline", "--crate-file", f"{CRATE}={self.archive}")
        self.assert_failed(result)
        self.assertFalse(self.output.exists())

    def test_unsafe_archive_members_are_rejected(self) -> None:
        members = []
        traversal = tarfile.TarInfo(f"{ARCHIVE_ROOT}/../../escaped")
        traversal.size = 1
        members.append(traversal)
        symlink = tarfile.TarInfo(f"{ARCHIVE_ROOT}/src/escape")
        symlink.type = tarfile.SYMTYPE
        symlink.linkname = "../../../escaped"
        members.append(symlink)
        for member in members:
            with self.subTest(member=member.name):
                self.make_archive([member])
                self.write_manifest()
                result = self.run_cli("--offline", "--crate-file", f"{CRATE}={self.archive}")
                self.assert_failed(result)
                self.assertFalse(self.output.exists())
                self.assertFalse((self.root / "escaped").exists())

    def test_concurrent_invocations_publish_one_complete_patched_tree(self) -> None:
        command = self.command("--offline", "--crate-file", f"{CRATE}={self.archive}")
        processes = [
            subprocess.Popen(command, cwd=self.root, env=self.env, text=True,
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            for _ in range(6)
        ]
        for process in processes:
            stdout, stderr = process.communicate(timeout=30)
            self.assertEqual(process.returncode, 0, stdout + stderr)
        self.assertEqual(
            (self.output / "src" / "lib.rs").read_text(encoding="utf-8"),
            'pub const MESSAGE: &str = "patched";\n',
        )
        self.assert_succeeded(self.run_cli("--offline", "--crate-file", f"{CRATE}={self.archive}", "--check"))


if __name__ == "__main__":
    unittest.main()
