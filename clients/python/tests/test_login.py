"""Unit tests for `airc login` and repeated `--rotate` label uniqueness."""

from __future__ import annotations

import contextlib
import http.server
import io
import json
import os
import sys
import tempfile
import threading
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from aircd import cli  # noqa: E402


class _FakeLoginHub:
    def __init__(self) -> None:
        self.requests: list[dict] = []
        self.seen_labels: set[tuple[str, str]] = set()
        self.counter = 0

    def start(self) -> str:
        hub = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self) -> None:  # noqa: N802
                length = int(self.headers.get("Content-Length", "0"))
                raw = self.rfile.read(length).decode("utf-8") if length else ""
                body = json.loads(raw) if raw else {}
                hub.requests.append(
                    {
                        "method": "POST",
                        "path": self.path,
                        "auth": self.headers.get("Authorization", ""),
                        "body": body,
                    }
                )
                if self.path != "/v1/login":
                    self._reply(404, {"error": f"unmocked {self.path}"})
                    return

                rotated_token = None
                if body.get("rotate_token"):
                    label = (body.get("label") or "cli").strip() or "cli"
                    key = ("alice", label)
                    if key in hub.seen_labels:
                        self._reply(500, {"error": "insert new token"})
                        return
                    hub.seen_labels.add(key)
                    hub.counter += 1
                    rotated_token = f"airc_rotated_{hub.counter}"

                self._reply(
                    200,
                    {
                        "ok": True,
                        "principal": {"id": "alice", "nick": "alice", "role": "member"},
                        "channels": body.get("channels") or ["#general"],
                        "webhook": None,
                        "rotated_token": rotated_token,
                    },
                )

            def _reply(self, status: int, body: dict) -> None:
                payload = json.dumps(body).encode("utf-8")
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def log_message(self, format: str, *args: object) -> None:
                return

        self._httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=self._httpd.serve_forever, daemon=True).start()
        host, port = self._httpd.server_address
        return f"http://{host}:{port}"

    def stop(self) -> None:
        self._httpd.shutdown()
        self._httpd.server_close()


class LoginRotateCliTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.cfg_dir = Path(self.tmp.name) / "cfg"
        self.cfg_dir.mkdir()

        self.hub = _FakeLoginHub()
        self.url = self.hub.start()
        self.addCleanup(self.hub.stop)

        env = {k: v for k, v in os.environ.items() if not k.startswith(("AIRCD_", "AIRC_"))}
        env["AIRCD_CONFIG_DIR"] = str(self.cfg_dir)
        patcher = mock.patch.dict(os.environ, env, clear=True)
        patcher.start()
        self.addCleanup(patcher.stop)

        cli.update_profile(
            "alice",
            {"server": self.url, "token": "airc_initial", "nick": "alice", "id": "alice", "role": "member"},
            make_default=True,
        )

    def run_cli(self, *args: str) -> tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        argv = ["--server", self.url, *args]
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            rc = cli.main(argv)
        return rc, out.getvalue(), err.getvalue()

    def test_repeated_rotate_uses_unique_labels_and_updates_profile(self) -> None:
        labels: list[str] = []
        expected_auth = "Bearer airc_initial"

        for idx in range(1, 4):
            rc, out, _ = self.run_cli("--profile", "alice", "--json", "login", "--rotate")
            self.assertEqual(rc, 0)
            data = json.loads(out)
            self.assertEqual(data["rotated_token"], f"airc_rotated_{idx}")

            req = self.hub.requests[-1]
            self.assertEqual(req["auth"], expected_auth)
            self.assertTrue(req["body"]["rotate_token"])
            label = req["body"]["label"]
            self.assertTrue(label.startswith("cli-login-"))
            self.assertNotEqual(label, "cli-login")
            labels.append(label)
            expected_auth = f"Bearer airc_rotated_{idx}"

        self.assertEqual(len(set(labels)), 3)
        saved = cli.load_profiles()["profiles"]["alice"]["token"]
        self.assertEqual(saved, "airc_rotated_3")

    def test_login_without_rotate_keeps_token(self) -> None:
        rc, out, _ = self.run_cli("--profile", "alice", "--json", "login", "--channels", "#general")
        self.assertEqual(rc, 0)
        data = json.loads(out)
        self.assertIsNone(data["rotated_token"])
        req = self.hub.requests[-1]
        self.assertFalse(req["body"]["rotate_token"])
        self.assertEqual(req["body"]["label"], "cli-login")
        self.assertEqual(cli.load_profiles()["profiles"]["alice"]["token"], "airc_initial")


if __name__ == "__main__":
    unittest.main(verbosity=2)
