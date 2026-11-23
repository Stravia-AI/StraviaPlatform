#!/usr/bin/env python3
"""Unified GitHub & Forgejo Release API client and release notes composer.

Uses only the Python standard library so release workflows run identically on
both GitHub Actions and Forgejo Actions without `gh` or `tea` CLI dependencies.
"""

from __future__ import annotations

import argparse
import json
import mimetypes
import os
import re
import subprocess
import sys
import urllib.error
import urllib.parse
import urllib.request
import uuid
from pathlib import Path
from typing import Any

STALE_PLUGIN_ASSET_PREFIX = "stravia-vendor-"
STALE_PLUGIN_ASSET_SUFFIX = ".wasm.zst"
STALE_PLUGIN_EXACT_NAMES = {
    "stravia-plugin-catalog.json",
    "stravia-plugin-catalog.json.minisig",
    "stravia-plugin-catalog.pub",
}


def is_stale_plugin_asset(name: str) -> bool:
    return name in STALE_PLUGIN_EXACT_NAMES or (
        name.startswith(STALE_PLUGIN_ASSET_PREFIX)
        and name.endswith(STALE_PLUGIN_ASSET_SUFFIX)
    )


def detect_platform(api_url: str, override: str = "auto") -> str:
    if override in ("github", "forgejo"):
        return override
    host = (urllib.parse.urlparse(api_url).hostname or "").lower()
    if host == "api.github.com" or host.endswith(".github.com"):
        return "github"
    return "forgejo"


def extract_changelog_section(changelog_text: str, version: str) -> str:
    escaped = re.escape(version)
    match = re.search(
        rf"^## \[{escaped}\](?: - [^\n]+)?\n(?P<body>.*?)(?=^## |\Z)",
        changelog_text,
        flags=re.MULTILINE | re.DOTALL,
    )
    if match is None:
        raise SystemExit(f"CHANGELOG.md has no section for {version}")
    notes = match.group("body").strip()
    if not notes:
        raise SystemExit(f"CHANGELOG.md section for {version} is empty")
    return notes


def previous_release_tag(current_tag: str) -> str | None:
    try:
        tags = subprocess.check_output(
            ["git", "tag", "--merged", f"{current_tag}^", "--sort=-v:refname", "v*"],
            text=True,
            stderr=subprocess.DEVNULL,
        ).splitlines()
    except subprocess.CalledProcessError:
        return None
    for tag in tags:
        candidate = tag.strip()
        if candidate and candidate != current_tag:
            return candidate
    return None


def generate_commit_notes(current_tag: str, prev_tag: str | None) -> str:
    rev_range = f"{prev_tag}..{current_tag}" if prev_tag else current_tag
    try:
        commits = subprocess.check_output(
            ["git", "log", "--no-merges", "--pretty=format:- %s (%h)", rev_range],
            text=True,
        ).strip()
    except subprocess.CalledProcessError:
        commits = ""
    header = (
        f"## Commits since {prev_tag}"
        if prev_tag
        else f"## Commits in {current_tag}"
    )
    return f"{header}\n\n{commits}\n" if commits else f"{header}\n\n_No direct commits._\n"


def compose_release_notes(changelog_path: Path, version: str, tag: str) -> str:
    curated = extract_changelog_section(changelog_path.read_text(encoding="utf-8"), version)
    generated = generate_commit_notes(tag, previous_release_tag(tag))
    return f"{curated}\n\n---\n\n{generated}"


class ReleaseClient:
    """Release REST API client compatible with both GitHub and Forgejo."""

    def __init__(
        self,
        api_url: str,
        repository: str,
        token: str,
        platform: str = "auto",
    ) -> None:
        self.api_url = api_url.rstrip("/")
        self.base_url = f"{self.api_url}/repos/{repository}"
        self.token = token
        self.platform = detect_platform(self.api_url, platform)

    def _headers(self, content_type: str | None = "application/json") -> dict[str, str]:
        if self.platform == "github":
            headers = {
                "Accept": "application/vnd.github+json",
                "Authorization": f"Bearer {self.token}",
                "X-GitHub-Api-Version": "2022-11-28",
            }
        else:
            headers = {
                "Accept": "application/json",
                "Authorization": f"token {self.token}",
            }
        if content_type:
            headers["Content-Type"] = content_type
        return headers

    def request_json(
        self,
        method: str,
        path: str,
        payload: dict[str, Any] | None = None,
        allow_404: bool = False,
    ) -> Any:
        data = json.dumps(payload).encode("utf-8") if payload is not None else None
        req = urllib.request.Request(
            f"{self.base_url}{path}",
            data=data,
            headers=self._headers("application/json" if data is not None else None),
            method=method,
        )
        try:
            with urllib.request.urlopen(req, timeout=60) as resp:
                body = resp.read()
                return json.loads(body.decode("utf-8")) if body else None
        except urllib.error.HTTPError as err:
            try:
                if allow_404 and err.code == 404:
                    return None
                detail = err.read().decode("utf-8", errors="replace")
            finally:
                err.close()
            raise RuntimeError(
                f"{self.platform} API {method} {path} failed ({err.code}): {detail}"
            ) from err

    def get_release_by_tag(self, tag: str) -> dict[str, Any] | None:
        encoded = urllib.parse.quote(tag, safe="")
        release = self.request_json("GET", f"/releases/tags/{encoded}", allow_404=True)
        if release is not None or self.platform != "github":
            return release
        # GitHub's /releases/tags/{tag} omits draft releases; scan /releases to find drafts.
        releases = self.request_json("GET", "/releases?per_page=100") or []
        for candidate in releases:
            if candidate.get("tag_name") == tag:
                return candidate
        return None

    def create_release(
        self, tag: str, title: str, body: str, draft: bool, prerelease: bool
    ) -> dict[str, Any]:
        return self.request_json(
            "POST",
            "/releases",
            payload={
                "tag_name": tag,
                "name": title,
                "body": body,
                "draft": draft,
                "prerelease": prerelease,
            },
        )

    def update_release(
        self,
        release_id: int,
        draft: bool,
        prerelease: bool,
        title: str | None = None,
        body: str | None = None,
    ) -> dict[str, Any]:
        payload: dict[str, Any] = {
            "draft": draft,
            "prerelease": prerelease,
        }
        if title is not None:
            payload["name"] = title
        if body is not None:
            payload["body"] = body
        return self.request_json("PATCH", f"/releases/{release_id}", payload=payload)

    def list_assets(self, release_id: int) -> list[dict[str, Any]]:
        result = self.request_json("GET", f"/releases/{release_id}/assets")
        return list(result or [])

    def delete_asset(self, release_id: int, asset_id: int) -> None:
        if self.platform == "github":
            self.request_json("DELETE", f"/releases/assets/{asset_id}")
        else:
            self.request_json("DELETE", f"/releases/{release_id}/assets/{asset_id}")

    def upload_asset(self, release: dict[str, Any], file_path: Path) -> dict[str, Any]:
        filename = file_path.name
        mime_type = mimetypes.guess_type(filename)[0] or "application/octet-stream"
        file_bytes = file_path.read_bytes()
        encoded_name = urllib.parse.quote(filename, safe="")

        if self.platform == "github":
            upload_template = release.get("upload_url")
            if not upload_template:
                raise RuntimeError("GitHub release payload missing upload_url")
            upload_base = re.sub(r"\{[^}]*\}$", "", str(upload_template))
            url = f"{upload_base}?name={encoded_name}"
            req = urllib.request.Request(
                url,
                data=file_bytes,
                headers=self._headers(mime_type),
                method="POST",
            )
        else:
            release_id = int(release["id"])
            boundary = f"----StraviaRelease{uuid.uuid4().hex}"
            body = (
                f"--{boundary}\r\n"
                f'Content-Disposition: form-data; name="attachment"; filename="{filename}"\r\n'
                f"Content-Type: {mime_type}\r\n\r\n"
            ).encode("utf-8") + file_bytes + f"\r\n--{boundary}--\r\n".encode("utf-8")
            url = f"{self.base_url}/releases/{release_id}/assets?name={encoded_name}"
            req = urllib.request.Request(
                url,
                data=body,
                headers=self._headers(f"multipart/form-data; boundary={boundary}"),
                method="POST",
            )

        try:
            with urllib.request.urlopen(req, timeout=300) as resp:
                resp_body = resp.read()
                return json.loads(resp_body.decode("utf-8")) if resp_body else {}
        except urllib.error.HTTPError as err:
            try:
                detail = err.read().decode("utf-8", errors="replace")
            finally:
                err.close()
            raise RuntimeError(
                f"Uploading asset {filename} ({self.platform}) failed ({err.code}): {detail}"
            ) from err


def sync_draft_release(
    client: ReleaseClient,
    tag: str,
    title: str,
    notes: str,
    prerelease: bool,
    assets_dir: Path,
) -> dict[str, Any]:
    release = client.get_release_by_tag(tag)
    if release is None:
        release = client.create_release(
            tag=tag, title=title, body=notes, draft=True, prerelease=prerelease
        )
    else:
        release = client.update_release(
            release_id=int(release["id"]),
            title=title,
            body=notes,
            draft=True,
            prerelease=prerelease,
        )

    release_id = int(release["id"])
    existing_assets = client.list_assets(release_id)
    new_files = sorted(p for p in assets_dir.iterdir() if p.is_file())
    new_names = {p.name for p in new_files}

    for asset in existing_assets:
        name = str(asset.get("name", ""))
        if name in new_names or is_stale_plugin_asset(name):
            client.delete_asset(release_id, int(asset["id"]))

    for file_path in new_files:
        client.upload_asset(release, file_path)

    return release


def publish_release(
    client: ReleaseClient,
    tag: str,
    prerelease: bool,
    title: str | None = None,
    notes: str | None = None,
) -> dict[str, Any]:
    release = client.get_release_by_tag(tag)
    if release is None:
        raise SystemExit(f"Draft release for tag {tag} does not exist")
    return client.update_release(
        release_id=int(release["id"]),
        draft=False,
        prerelease=prerelease,
        title=title,
        body=notes,
    )


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Unified GitHub & Forgejo release management helper"
    )
    sub = parser.add_subparsers(dest="command", required=True)

    notes_cmd = sub.add_parser("compose-notes", help="Compose release notes markdown")
    notes_cmd.add_argument("--changelog", required=True, type=Path)
    notes_cmd.add_argument("--version", required=True)
    notes_cmd.add_argument("--tag", required=True)
    notes_cmd.add_argument("--output", required=True, type=Path)

    for name in ("upsert-draft", "publish"):
        cmd = sub.add_parser(name)
        cmd.add_argument("--api-url", default=os.environ.get("GITHUB_API_URL", ""))
        cmd.add_argument("--repository", default=os.environ.get("GITHUB_REPOSITORY", ""))
        cmd.add_argument("--token", default=os.environ.get("GITHUB_TOKEN", ""))
        cmd.add_argument(
            "--platform",
            choices=("auto", "github", "forgejo"),
            default="auto",
        )
        cmd.add_argument("--tag", required=True)
        cmd.add_argument("--prerelease", default="false")
        if name == "upsert-draft":
            cmd.add_argument("--title", required=True)
            cmd.add_argument("--notes-file", required=True, type=Path)
            cmd.add_argument("--assets-dir", required=True, type=Path)
        else:
            cmd.add_argument("--title", default=None)
            cmd.add_argument("--notes-file", default=None, type=Path)

    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    if args.command == "compose-notes":
        notes = compose_release_notes(args.changelog, args.version, args.tag)
        args.output.write_text(notes, encoding="utf-8")
        return 0

    if not args.api_url or not args.repository or not args.token:
        raise SystemExit(
            "api-url, repository, and token (GITHUB_API_URL/GITHUB_REPOSITORY/GITHUB_TOKEN) are required"
        )

    client = ReleaseClient(
        args.api_url,
        args.repository,
        args.token,
        platform=args.platform,
    )
    notes = args.notes_file.read_text(encoding="utf-8") if args.notes_file else None
    prerelease = str(args.prerelease).lower() == "true"

    if args.command == "upsert-draft":
        sync_draft_release(
            client=client,
            tag=args.tag,
            title=args.title,
            notes=notes or "",
            prerelease=prerelease,
            assets_dir=args.assets_dir,
        )
    elif args.command == "publish":
        publish_release(
            client=client,
            tag=args.tag,
            prerelease=prerelease,
            title=args.title,
            notes=notes,
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
