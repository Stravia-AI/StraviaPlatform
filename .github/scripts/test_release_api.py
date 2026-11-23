#!/usr/bin/env python3
"""Unit tests for .github/scripts/release_api.py (GitHub & Forgejo modes)."""

from __future__ import annotations

import json
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import release_api as api  # noqa: E402


class MockForgeHandler(BaseHTTPRequestHandler):
    state: dict = {}

    def log_message(self, format: str, *args) -> None:  # noqa: A003
        return

    def _send(self, code: int, body: object | None = None) -> None:
        raw = json.dumps(body).encode("utf-8") if body is not None else b""
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        if raw:
            self.wfile.write(raw)

    def do_GET(self) -> None:  # noqa: N802
        if self.path == "/api/v1/repos/o/r/releases/tags/v1.0.0":
            rel = self.state.get("release")
            self._send(200, rel) if rel else self._send(404, {"message": "not found"})
            return
        if self.path == "/api/v1/repos/o/r/releases?per_page=100":
            rel = self.state.get("release")
            self._send(200, [rel] if rel else [])
            return
        if self.path == "/api/v1/repos/o/r/releases/42/assets":
            self._send(200, self.state.get("assets", []))
            return
        self._send(404, {"path": self.path})

    def do_POST(self) -> None:  # noqa: N802
        length = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(length)
        if self.path == "/api/v1/repos/o/r/releases":
            payload = json.loads(body.decode("utf-8"))
            self.state["release"] = {
                "id": 42,
                "upload_url": f"http://127.0.0.1:{self.server.server_port}/uploads/42/assets{{?name,label}}",
                **payload,
            }
            self._send(201, self.state["release"])
            return
        if self.path.startswith("/api/v1/repos/o/r/releases/42/assets?name="):
            name = self.path.split("name=", 1)[1]
            self.state.setdefault("uploaded_forgejo", []).append(name)
            self._send(201, {"id": 100 + len(self.state["uploaded_forgejo"]), "name": name})
            return
        if self.path.startswith("/uploads/42/assets?name="):
            name = self.path.split("name=", 1)[1]
            self.state.setdefault("uploaded_github", []).append((name, body))
            self._send(201, {"id": 200 + len(self.state["uploaded_github"]), "name": name})
            return
        self._send(404, {"path": self.path})

    def do_PATCH(self) -> None:  # noqa: N802
        length = int(self.headers.get("Content-Length", "0"))
        payload = json.loads(self.rfile.read(length).decode("utf-8"))
        self.state["release"].update(payload)
        self._send(200, self.state["release"])

    def do_DELETE(self) -> None:  # noqa: N802
        if "/releases/42/assets/" in self.path:
            asset_id = int(self.path.rsplit("/", 1)[-1])
            self.state.setdefault("deleted_forgejo", []).append(asset_id)
            self._send(204)
            return
        if "/releases/assets/" in self.path:
            asset_id = int(self.path.rsplit("/", 1)[-1])
            self.state.setdefault("deleted_github", []).append(asset_id)
            self._send(204)
            return
        self._send(404)


class ReleaseApiTests(unittest.TestCase):
    def setUp(self) -> None:
        MockForgeHandler.state = {}
        self.server = HTTPServer(("127.0.0.1", 0), MockForgeHandler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.api_url = f"http://127.0.0.1:{self.server.server_port}/api/v1"

    def tearDown(self) -> None:
        self.server.shutdown()
        self.server.server_close()

    def test_detect_platform(self) -> None:
        self.assertEqual(api.detect_platform("https://api.github.com"), "github")
        self.assertEqual(api.detect_platform("https://codeberg.org/api/v1"), "forgejo")
        self.assertEqual(api.detect_platform("https://git.internal/api/v1"), "forgejo")
        self.assertEqual(api.detect_platform("http://127.0.0.1:8080/api/v1", "github"), "github")

    def test_extract_changelog_section(self) -> None:
        changelog = "# Changelog\n\n## [1.0.0] - 2026-11-22\n\n- Added unified CI\n\n## [0.9.0]\n\n- Old\n"
        self.assertEqual(api.extract_changelog_section(changelog, "1.0.0"), "- Added unified CI")

    def test_sync_draft_and_publish_forgejo(self) -> None:
        MockForgeHandler.state["assets"] = [
            {"id": 1, "name": "stravia-vendor-old.wasm.zst"},
            {"id": 2, "name": "latest.json"},
            {"id": 3, "name": "keep-unrelated.txt"},
        ]
        client = api.ReleaseClient(self.api_url, "o/r", "tok", platform="forgejo")
        with tempfile.TemporaryDirectory() as tmp:
            assets_dir = Path(tmp)
            (assets_dir / "latest.json").write_text("{}", encoding="utf-8")
            (assets_dir / "SHA256SUMS.txt").write_text("abc", encoding="utf-8")
            api.sync_draft_release(client, "v1.0.0", "Stravia v1.0.0", "notes", False, assets_dir)

        self.assertTrue(MockForgeHandler.state["release"]["draft"])
        self.assertEqual(MockForgeHandler.state["deleted_forgejo"], [1, 2])
        self.assertEqual(MockForgeHandler.state["uploaded_forgejo"], ["SHA256SUMS.txt", "latest.json"])

        api.publish_release(client, "v1.0.0", False)
        self.assertFalse(MockForgeHandler.state["release"]["draft"])

    def test_sync_draft_and_publish_github(self) -> None:
        MockForgeHandler.state["assets"] = [
            {"id": 10, "name": "stravia-plugin-catalog.json"},
            {"id": 11, "name": "latest.json"},
        ]
        client = api.ReleaseClient(self.api_url, "o/r", "ghp_tok", platform="github")
        with tempfile.TemporaryDirectory() as tmp:
            assets_dir = Path(tmp)
            (assets_dir / "latest.json").write_text('{"ok":true}', encoding="utf-8")
            api.sync_draft_release(client, "v1.0.0", "Stravia v1.0.0", "notes", False, assets_dir)

        self.assertEqual(MockForgeHandler.state["deleted_github"], [10, 11])
        self.assertEqual(MockForgeHandler.state["uploaded_github"], [("latest.json", b'{"ok":true}')])

        api.publish_release(client, "v1.0.0", False)
        self.assertFalse(MockForgeHandler.state["release"]["draft"])


if __name__ == "__main__":
    unittest.main()
