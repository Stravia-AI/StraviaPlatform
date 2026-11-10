from __future__ import annotations

import argparse
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import unquote, urlsplit

CATALOG = "stravia-plugin-catalog.json"
PUBLIC_KEY = "stravia-plugin-catalog.pub"
REPOSITORY = "fixture/plugins"
REPOSITORY_ID = 424242


class MarketplaceFixture(ThreadingHTTPServer):
    """Real signed release bytes behind an isolated updater/GitHub HTTP interface."""

    def __init__(self, fixtures: Path, port: int = 0) -> None:
        self.fixtures = fixtures
        self.versions = [1]
        self.rotated = False
        self.corrupt_artifact = False
        super().__init__(("127.0.0.1", port), _Handler)

    @property
    def base_url(self) -> str:
        return f"http://127.0.0.1:{self.server_address[1]}"

    def directory(self, version: int) -> Path:
        return self.fixtures / ("rotated" if self.rotated else f"v{version}")

    def release(self, version: int) -> dict[str, object]:
        directory = self.directory(version)
        catalog = json.loads((directory / CATALOG).read_text())
        names = [CATALOG, f"{CATALOG}.minisig", PUBLIC_KEY, catalog["releases"][0]["artifact"]["filename"]]
        tag = f"v{version}.0.0"
        prefix = f"{self.base_url}/api/v1/plugin-repositories/{REPOSITORY}/releases/{tag}/assets"
        return {
            "id": 1000 + version,
            "tag_name": tag,
            "html_url": f"https://github.com/{REPOSITORY}/releases/tag/{tag}",
            "draft": False,
            "prerelease": False,
            "published_at": "2026-01-01T00:00:00Z",
            "assets": [
                {"id": version * 100 + index, "name": name, "size": (directory / name).stat().st_size,
                 "browser_download_url": f"{prefix}/{name}"}
                for index, name in enumerate(names)
            ],
        }

    def start(self) -> None:
        threading.Thread(target=self.serve_forever, daemon=True).start()

    def close(self) -> None:
        self.shutdown()
        self.server_close()


class _Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt: str, *args: object) -> None:
        return

    def do_GET(self) -> None:  # noqa: N802
        server = self.server
        assert isinstance(server, MarketplaceFixture)
        path = unquote(urlsplit(self.path).path)
        prefix = f"/api/v1/plugin-repositories/{REPOSITORY}"
        payload: bytes | None = None
        content_type = "application/json"
        if path in {prefix, f"/repositories/{REPOSITORY_ID}", f"/repos/{REPOSITORY}"}:
            payload = json.dumps({"id": REPOSITORY_ID, "full_name": REPOSITORY, "private": False, "mirrored": True}).encode()
        elif path in {f"{prefix}/releases", f"/repositories/{REPOSITORY_ID}/releases", f"/repos/{REPOSITORY}/releases"}:
            payload = json.dumps([server.release(version) for version in server.versions]).encode()
        elif path.startswith(f"{prefix}/releases/"):
            remainder = path.removeprefix(f"{prefix}/releases/").split("/")
            if len(remainder) == 3 and remainder[1] == "assets":
                tag, _, filename = remainder
                for version in server.versions:
                    release = server.release(version)
                    if tag == release["tag_name"] and any(asset["name"] == filename for asset in release["assets"]):
                        payload = (server.directory(version) / filename).read_bytes()
                        if filename.endswith(".zst"):
                            content_type = "application/zstd"
                            if server.corrupt_artifact:
                                payload = payload[:-1] + bytes([payload[-1] ^ 1])
                        elif filename != CATALOG:
                            content_type = "text/plain"
                        break
        if payload is None:
            self.send_response(404)
            payload = b'{"code":"not_cataloged","message":"No fixture repository or asset"}'
        else:
            self.send_response(200)
        self.send_header("content-type", content_type)
        self.send_header("content-length", str(len(payload)))
        self.send_header("connection", "close")
        self.end_headers()
        self.wfile.write(payload)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--fixtures", type=Path, required=True)
    parser.add_argument("--port", type=int, required=True)
    args = parser.parse_args()
    fixture = MarketplaceFixture(args.fixtures, args.port)
    fixture.versions = [2, 1]
    print(f"Marketplace fixture ready: {fixture.base_url}", flush=True)
    try:
        fixture.serve_forever()
    finally:
        fixture.server_close()
