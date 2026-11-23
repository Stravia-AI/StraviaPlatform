import importlib.util
import json
import plistlib
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "package_macos_cross", Path(__file__).with_name("package_macos_cross.py")
)
packager = importlib.util.module_from_spec(spec)
spec.loader.exec_module(packager)


class PackageMacosCrossTests(unittest.TestCase):
    def test_verify_macho_architecture_accepts_expected_and_rejects_elf(self) -> None:
        with patch.object(
            packager.subprocess,
            "check_output",
            return_value="Mach-O 64-bit arm64 executable, flags:<NOUNDEFS|DYLDLINK|TWOLEVEL|PIE>\n",
        ):
            packager.verify_macho_architecture(Path("fake"), "arm64")

        with patch.object(
            packager.subprocess,
            "check_output",
            return_value="ELF 64-bit LSB pie executable, x86-64\n",
        ):
            with self.assertRaises(SystemExit):
                packager.verify_macho_architecture(Path("fake"), "x86_64")

    def test_assemble_app_bundle_and_archive(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            binary = root / "stravia-desktop"
            binary.write_bytes(b"fake-macho")
            icon = root / "icon.icns"
            icon.write_bytes(b"icns")
            conf = root / "tauri.conf.json"
            conf.write_text(
                json.dumps(
                    {
                        "productName": "Stravia",
                        "identifier": "cn.stravia.agent-infra",
                        "bundle": {"macOS": {"minimumSystemVersion": "12.0"}},
                    }
                ),
                encoding="utf-8",
            )

            app_dir = packager.assemble_app_bundle(
                binary=binary,
                tauri_conf=conf,
                icon_icns=icon,
                staging_dir=root / "stage",
                version="0.7.4",
            )
            self.assertEqual(app_dir.name, "Stravia.app")
            info = plistlib.loads((app_dir / "Contents/Info.plist").read_bytes())
            self.assertEqual(info["CFBundleIdentifier"], "cn.stravia.agent-infra")
            self.assertEqual(info["CFBundleShortVersionString"], "0.7.4")
            self.assertEqual(info["LSMinimumSystemVersion"], "12.0")

            archive = root / "out/stravia.app.tar.gz"
            packager.create_updater_archive(app_dir, archive)
            with tarfile.open(archive, "r:gz") as tar:
                self.assertIn("Stravia.app/Contents/MacOS/stravia-desktop", tar.getnames())
                self.assertIn("Stravia.app/Contents/Info.plist", tar.getnames())
                self.assertIn("Stravia.app/Contents/Resources/icon.icns", tar.getnames())


if __name__ == "__main__":
    unittest.main()
