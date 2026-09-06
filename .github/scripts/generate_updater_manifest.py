import argparse
import json
import re
from datetime import datetime
from pathlib import Path

SEMVER = re.compile(
    r"(?:0|[1-9][0-9]*)\."
    r"(?:0|[1-9][0-9]*)\."
    r"(?:0|[1-9][0-9]*)"
    r"(?:-(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)"
    r"(?:\.(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*))*)?"
)
PLATFORM_FILENAMES = {
    "linux-aarch64": "stravia-desktop-v{version}-linux-aarch64.AppImage",
    "linux-x86_64": "stravia-desktop-v{version}-linux-x86_64.AppImage",
    "windows-aarch64": "stravia-desktop-v{version}-windows-aarch64-setup.exe",
    "windows-x86_64": "stravia-desktop-v{version}-windows-x86_64-setup.exe",
}
REPOSITORY_URL = "https://github.com/Stravia-AI/StraviaPlatform"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Generate Stravia's signed Tauri updater manifest")
    parser.add_argument("--artifacts", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--published-at", required=True)
    parser.add_argument("--output", type=Path, required=True)
    return parser.parse_args()


def validate_inputs(version: str, published_at: str) -> None:
    if SEMVER.fullmatch(version) is None:
        raise SystemExit(f"version is not valid SemVer: {version}")
    try:
        datetime.fromisoformat(published_at.replace("Z", "+00:00"))
    except ValueError as error:
        raise SystemExit(f"published-at is not RFC 3339: {published_at}") from error


def generate_manifest(artifacts: Path, version: str, published_at: str) -> dict[str, object]:
    validate_inputs(version, published_at)
    platforms: dict[str, dict[str, str]] = {}
    for platform, template in PLATFORM_FILENAMES.items():
        filename = template.format(version=version)
        artifact = artifacts / filename
        signature_file = artifacts / f"{filename}.sig"
        if not artifact.is_file():
            raise SystemExit(f"missing updater artifact for {platform}: {artifact}")
        if not signature_file.is_file():
            raise SystemExit(f"missing updater signature for {platform}: {signature_file}")
        signature = signature_file.read_text(encoding="utf-8").strip()
        if not signature:
            raise SystemExit(f"empty updater signature for {platform}: {signature_file}")
        platforms[platform] = {
            "signature": signature,
            "url": f"{REPOSITORY_URL}/releases/download/v{version}/{filename}",
        }

    release_url = f"{REPOSITORY_URL}/releases/tag/v{version}"
    return {
        "version": version,
        "pub_date": published_at,
        "release_notes_url": release_url,
        "platforms": platforms,
    }


def main() -> None:
    args = parse_args()
    manifest = generate_manifest(args.artifacts, args.version, args.published_at)
    args.output.write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )


if __name__ == "__main__":
    main()
