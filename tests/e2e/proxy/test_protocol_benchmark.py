from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from types import SimpleNamespace

import pytest

from tests.common.measure_protocol_http import MARKER, prompt_from, run_load


@pytest.mark.e2e
@pytest.mark.proxy
@pytest.mark.parametrize("protocol", ["openai-chat", "open-responses", "anthropic-messages", "google-content"])
@pytest.mark.parametrize("terminal", ["error", "disconnect"])
def test_benchmark_rejects_failed_or_missing_terminal_after_visible_text(protocol: str, terminal: str) -> None:
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args: object) -> None:
            pass

        def do_POST(self) -> None:  # noqa: N802
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            nonce = MARKER.search(prompt_from(body, protocol)).group()
            with server.lock:
                server.arrivals.append({"nonce": nonce, "valid": True})
            text = {
                "openai-chat": {"choices": [{"delta": {"content": nonce}, "finish_reason": None}]},
                "open-responses": {"type": "response.output_text.delta", "delta": nonce},
                "anthropic-messages": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": nonce}},
                "google-content": {"candidates": [{"content": {"parts": [{"text": nonce}]}}]},
            }[protocol]
            frames = [text]
            if terminal == "error":
                frames.append({"type": "response.failed" if protocol == "open-responses" else "error",
                               "error": {"code": "fixture_failure", "message": "failed after text"}})
            payload = b"".join(("data: " + json.dumps(frame) + "\n\n").encode() for frame in frames)
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(payload)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(payload)
            self.close_connection = True

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.protocol = protocol
    server.lock = threading.Lock()
    server.expected = {}
    server.arrivals = []
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    args = SimpleNamespace(payload_bytes=1024, padding="a" * 1024, concurrency=1,
                           qps=1, request_timeout=5, min_qps_ratio=.95)
    try:
        measured = run_load(args, f"http://127.0.0.1:{server.server_port}", "benchmark-only",
                            protocol, server, True, "open-loop", 1, "measure")
        assert measured["completed_requests"] == 1
        assert measured["requests"][0]["status"] == 200
        assert measured["requests"][0]["ttft_ms"] is not None
        assert measured["errors"] == 1
        assert measured["successful_qps"] == 0
        assert not measured["valid"]
        expected_error = "protocol error terminal" if terminal == "error" else "missing successful protocol terminal"
        assert expected_error in measured["requests"][0]["error"]
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)
