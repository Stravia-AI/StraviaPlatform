#!/usr/bin/env python3
"""Assemble, ad-hoc codesign (with Hardened Runtime + entitlements), and package
a cross-compiled macOS Stravia Desktop binary on a Linux x64 host.

Produces the same release artifacts as native macOS Tauri bundling:
  - <basename>.dmg
  - <basename>.app.tar.gz
  - <basename>.app.tar.gz.sig (when updater signing is enabled)
"""

from __future__ import annotations

import argparse
import json
import os
import plistlib
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

REQUIRED_ENTITLEMENT = "com.apple.security.cs.allow-unsigned-executable-memory"
ARCH_FILE_MARKERS = {
    "arm64": ("Mach-O 64-bit", "arm64"),
    "x86_64": ("Mach-O 64-bit", "x86_64"),
}


def verify_macho_architecture(binary: Path, expected_arch: str) -> None:
    if expected_arch not in ARCH_FILE_MARKERS:
        raise SystemExit(f"Unsupported macOS architecture: {expected_arch}")
    output = subprocess.check_output(["file", "-b", str(binary)], text=True).strip()
    markers = ARCH_FILE_MARKERS[expected_arch]
    if not all(marker in output for marker in markers):
        raise SystemExit(
            f"Binary {binary} does not match expected Mach-O {expected_arch} format: {output}"
        )


def build_info_plist(
    *,
    product_name: str,
    executable_name: str,
    identifier: str,
    version: str,
    min_system_version: str = "11.0",
) -> bytes:
    payload = {
        "CFBundleDevelopmentRegion": "English",
        "CFBundleDisplayName": product_name,
        "CFBundleExecutable": executable_name,
        "CFBundleIconFile": "icon.icns",
        "CFBundleIdentifier": identifier,
        "CFBundleInfoDictionaryVersion": "6.0",
        "CFBundleName": product_name,
        "CFBundlePackageType": "APPL",
        "CFBundleShortVersionString": version,
        "CFBundleVersion": version,
        "CSResourcesFileMapped": True,
        "LSMinimumSystemVersion": min_system_version,
        "LSRequiresCarbon": False,
        "NSHighResolutionCapable": True,
    }
    return plistlib.dumps(payload, fmt=plistlib.FMT_XML, sort_keys=True)


def assemble_app_bundle(
    *,
    binary: Path,
    tauri_conf: Path,
    icon_icns: Path,
    staging_dir: Path,
    version: str,
) -> Path:
    config = json.loads(tauri_conf.read_text(encoding="utf-8"))
    product_name = config["productName"]
    identifier = config["identifier"]
    min_system = (
        config.get("bundle", {}).get("macOS", {}).get("minimumSystemVersion") or "11.0"
    )

    app_dir = staging_dir / f"{product_name}.app"
    contents = app_dir / "Contents"
    macos_dir = contents / "MacOS"
    resources_dir = contents / "Resources"
    macos_dir.mkdir(parents=True, exist_ok=True)
    resources_dir.mkdir(parents=True, exist_ok=True)

    exe_name = "stravia-desktop"
    dst_bin = macos_dir / exe_name
    shutil.copy2(binary, dst_bin)
    dst_bin.chmod(dst_bin.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)

    shutil.copy2(icon_icns, resources_dir / "icon.icns")
    (contents / "Info.plist").write_bytes(
        build_info_plist(
            product_name=product_name,
            executable_name=exe_name,
            identifier=identifier,
            version=version,
            min_system_version=min_system,
        )
    )
    return app_dir


def sign_and_verify_app(app_dir: Path, entitlements: Path) -> None:
    rcodesign = shutil.which("rcodesign")
    if not rcodesign:
        raise SystemExit("rcodesign (apple-codesign) is required to sign macOS bundles on Linux")

    subprocess.run(
        [
            rcodesign,
            "sign",
            "--code-signature-flags",
            "runtime",
            "--entitlements-xml-path",
            str(entitlements),
            str(app_dir),
        ],
        check=True,
    )

    executable = app_dir / "Contents" / "MacOS" / "stravia-desktop"
    info = subprocess.check_output(
        [rcodesign, "print-signature-info", str(executable)],
        text=True,
    )
    if "RUNTIME" not in info and "0x10000" not in info:
        raise SystemExit(
            f"Signed macOS executable missing Hardened Runtime flag:\n{info}"
        )
    if REQUIRED_ENTITLEMENT not in info:
        raise SystemExit(
            f"Signed macOS executable missing required entitlement {REQUIRED_ENTITLEMENT}:\n{info}"
        )


def create_updater_archive(app_dir: Path, output_tar_gz: Path) -> None:
    output_tar_gz.parent.mkdir(parents=True, exist_ok=True)
    with tarfile.open(output_tar_gz, "w:gz") as archive:
        archive.add(app_dir, arcname=app_dir.name)
    # Verify archive integrity immediately.
    with tarfile.open(output_tar_gz, "r:gz") as archive:
        names = archive.getnames()
        expected_entry = f"{app_dir.name}/Contents/MacOS/stravia-desktop"
        if expected_entry not in names:
            raise SystemExit(f"Updater archive missing {expected_entry}")


def sign_updater_archive(archive_path: Path, tauri_cli_version: str) -> Path:
    sig_path = archive_path.with_name(f"{archive_path.name}.sig")
    if not os.environ.get("TAURI_SIGNING_PRIVATE_KEY"):
        raise SystemExit("TAURI_SIGNING_PRIVATE_KEY is required to sign updater archive")
    subprocess.run(
        [
            "bunx",
            "--bun",
            f"@tauri-apps/cli@{tauri_cli_version}",
            "signer",
            "sign",
            str(archive_path),
        ],
        check=True,
    )
    if not sig_path.is_file() or not sig_path.read_text(encoding="utf-8").strip():
        raise SystemExit(f"Expected non-empty updater signature at {sig_path}")
    return sig_path


def create_dmg(app_dir: Path, output_dmg: Path, volume_name: str) -> None:
    genisoimage = shutil.which("genisoimage") or shutil.which("mkisofs")
    if not genisoimage:
        raise SystemExit("genisoimage (cdrkit) is required to build macOS .dmg images on Linux")

    output_dmg.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as dmg_stage:
        stage = Path(dmg_stage)
        shutil.copytree(app_dir, stage / app_dir.name, symlinks=True)
        os.symlink("/Applications", stage / "Applications")
        subprocess.run(
            [
                genisoimage,
                "-V",
                volume_name,
                "-D",
                "-R",
                "-apple",
                "-no-pad",
                "-o",
                str(output_dmg),
                str(stage),
            ],
            check=True,
        )
    if not output_dmg.is_file() or output_dmg.stat().st_size == 0:
        raise SystemExit(f"Failed to create non-empty DMG: {output_dmg}")


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Package and codesign cross-compiled macOS Stravia Desktop on Linux"
    )
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--arch", choices=["arm64", "x86_64"], required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument(
        "--tauri-conf",
        type=Path,
        default=Path("backend/apps/stravia-desktop/tauri.conf.json"),
    )
    parser.add_argument(
        "--entitlements",
        type=Path,
        default=Path("backend/apps/stravia-desktop/Entitlements.plist"),
    )
    parser.add_argument(
        "--icon",
        type=Path,
        default=Path("backend/apps/stravia-desktop/icons/icon.icns"),
    )
    parser.add_argument("--tauri-cli-version", default="2.12.3")
    parser.add_argument(
        "--skip-updater-sign",
        action="store_true",
        help="Skip Tauri updater .sig generation (for CI/local dry-run validation)",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> None:
    args = parse_args(argv)
    binary = args.binary.resolve(strict=True)
    verify_macho_architecture(binary, args.arch)

    suffix = "darwin-aarch64" if args.arch == "arm64" else "darwin-x86_64"
    basename = f"stravia-desktop-v{args.version}-{suffix}"
    args.output_dir.mkdir(parents=True, exist_ok=True)

    with tempfile.TemporaryDirectory() as tmp:
        stage = Path(tmp)
        app_dir = assemble_app_bundle(
            binary=binary,
            tauri_conf=args.tauri_conf,
            icon_icns=args.icon,
            staging_dir=stage,
            version=args.version,
        )
        sign_and_verify_app(app_dir, args.entitlements)

        archive_path = args.output_dir / f"{basename}.app.tar.gz"
        create_updater_archive(app_dir, archive_path)
        if not args.skip_updater_sign:
            sign_updater_archive(archive_path, args.tauri_cli_version)

        dmg_path = args.output_dir / f"{basename}.dmg"
        create_dmg(app_dir, dmg_path, volume_name=app_dir.stem)

    print(f"Packaged macOS {suffix} release assets in {args.output_dir}")


if __name__ == "__main__":
    main()
