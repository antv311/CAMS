"""Unit tests for `airc task worktree` lifecycle management."""

from __future__ import annotations

import contextlib
import http.server
import io
import json
import os
import subprocess
import sys
import tempfile
import threading
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from aircd import cli  # noqa: E402


class _FakeHub:
    def __init__(self) -> None:
        self.requests: list[dict] = []
        self.routes: dict[tuple[str, str], tuple[int, dict]] = {}

    def start(self) -> str:
        hub = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self) -> None:  # noqa: N802
                self._handle("GET")

            def do_POST(self) -> None:  # noqa: N802
                self._handle("POST")

            def _handle(self, method: str) -> None:
                length = int(self.headers.get("Content-Length", "0"))
                raw = self.rfile.read(length).decode("utf-8") if length else ""
                hub.requests.append(
                    {
                        "method": method,
                        "path": self.path,
                        "body": json.loads(raw) if raw else None,
                    }
                )
                status, body = hub.routes.get((method, self.path), (404, {"error": f"unmocked {method} {self.path}"}))
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


class TaskWorktreeCliTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        root = Path(self.tmp.name)
        self.cfg_dir = root / "cfg"
        self.cfg_dir.mkdir()
        self.repo_dir = root / "sample-repo"
        self.repo_dir.mkdir()
        for cmd in (
            ["git", "init", "-b", "main"],
            ["git", "config", "user.email", "dev@example.local"],
            ["git", "config", "user.name", "Dev"],
        ):
            subprocess.run(cmd, cwd=self.repo_dir, check=True, capture_output=True)
        (self.repo_dir / "README.md").write_text("hello\n", encoding="utf-8")
        subprocess.run(["git", "add", "README.md"], cwd=self.repo_dir, check=True, capture_output=True)
        subprocess.run(["git", "commit", "-m", "init"], cwd=self.repo_dir, check=True, capture_output=True)

        self.hub = _FakeHub()
        self.url = self.hub.start()
        self.addCleanup(self.hub.stop)

        env = {k: v for k, v in os.environ.items() if not k.startswith(("AIRCD_", "AIRC_"))}
        env["AIRCD_CONFIG_DIR"] = str(self.cfg_dir)
        patcher = mock.patch.dict(os.environ, env, clear=True)
        patcher.start()
        self.addCleanup(patcher.stop)

    def run_cli(self, *args: str) -> tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        argv = ["--server", self.url, "--token", "tok", *args]
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = cli.main(argv)
        return code, out.getvalue(), err.getvalue()

    def test_worktree_create_reuse_list_and_remove(self) -> None:
        task = {
            "id": "task_123",
            "channel": "#cams",
            "title": "Add feature",
            "repo": "sample-repo",
            "branch": "feat/add-feature",
        }
        repo = {
            "name": "sample-repo",
            "channel": "#cams",
            "local_path": str(self.repo_dir),
            "default_branch": "main",
        }
        self.hub.routes[("GET", "/v1/tasks/task_123")] = (200, {"ok": True, "task": task})
        self.hub.routes[("GET", "/v1/gits")] = (200, {"ok": True, "git_repos": [repo]})
        self.hub.routes[("POST", "/v1/locks")] = (201, {"ok": True, "lock": {"resource": "repo:sample-repo:feat/add-feature"}})
        self.hub.routes[("POST", "/v1/git/event")] = (201, {"ok": True, "channel": "#cams"})

        code, out, err = self.run_cli("--json", "task", "worktree", "task_123", "--lock")
        self.assertEqual(code, 0, err)
        res = json.loads(out)
        expected_wt = (self.repo_dir / ".aircd" / "worktrees" / "feat" / "add-feature").resolve()
        self.assertTrue(res["created"])
        self.assertFalse(res["reused"])
        self.assertEqual(Path(res["worktree"]).resolve(), expected_wt)
        self.assertTrue((expected_wt / ".git").exists())
        self.assertIsNotNone(res["lock"])

        # Second invocation reuses existing worktree idempotently
        code, out, err = self.run_cli("--json", "task", "worktree", "task_123")
        self.assertEqual(code, 0, err)
        reused = json.loads(out)
        self.assertFalse(reused["created"])
        self.assertTrue(reused["reused"])

        # List worktrees
        code, out, err = self.run_cli("--json", "task", "worktree", "--list", "--path", str(expected_wt))
        self.assertEqual(code, 0, err)
        listed = json.loads(out)
        self.assertEqual(Path(listed["repo_root"]).resolve(), self.repo_dir.resolve())
        self.assertEqual(len(listed["worktrees"]), 2)

        # Remove worktree and delete branch
        code, out, err = self.run_cli("--json", "task", "worktree", "task_123", "--remove", "--delete-branch")
        self.assertEqual(code, 0, err)
        removed = json.loads(out)
        self.assertTrue(removed["removed"])
        self.assertFalse(expected_wt.exists())

    def test_defaults_branch_to_task_id_and_rejects_unsafe_branch(self) -> None:
        task = {
            "id": "task_999",
            "channel": "#cams",
            "title": "No branch yet",
            "repo": "sample-repo",
            "branch": None,
        }
        repo = {
            "name": "sample-repo",
            "channel": "#cams",
            "local_path": str(self.repo_dir),
            "default_branch": "main",
        }
        self.hub.routes[("GET", "/v1/tasks/task_999")] = (200, {"ok": True, "task": task})
        self.hub.routes[("GET", "/v1/gits")] = (200, {"ok": True, "git_repos": [repo]})

        # Unsafe branch rejected
        code, _out, err = self.run_cli("task", "worktree", "task_999", "--branch", "../escape")
        self.assertEqual(code, 1)
        self.assertIn("unsafe", err)

        # Default branch uses task/<id>
        code, out, err = self.run_cli("--json", "task", "worktree", "task_999")
        self.assertEqual(code, 0, err)
        res = json.loads(out)
        self.assertEqual(res["branch"], "task/task_999")
        expected_wt = (self.repo_dir / ".aircd" / "worktrees" / "task" / "task_999").resolve()
        self.assertEqual(Path(res["worktree"]).resolve(), expected_wt)
        self.assertTrue(expected_wt.exists())


if __name__ == "__main__":
    unittest.main(verbosity=2)
