"""Tests of `airc task export`: CSV flattening, and JSON/CSV export against the compiled hub."""

from __future__ import annotations

import csv
import io
import json
import os
import subprocess
import sys
import tempfile
import time
import unittest
import urllib.request
from pathlib import Path

from aircd import cli
from tests.test_cli_e2e import CLI_PY_ROOT, REPO_ROOT, _binary_path, _free_port

SAMPLE = {
    "ok": True,
    "sprint": "#p-s1",
    "summary": {"tasks": 1},
    "tasks": [
        {
            "task": {
                "id": "task_a", "channel": "#p", "sprint": "#p-s1", "kind": "bug", "priority": "high",
                "title": 'Fix "quotes", commas', "status": "merged", "assignee": "dev", "reviewer": None,
                "blocked_by": ["task_x", "task_y"], "created_at": 0, "updated_at": 90,
            },
            "metrics": {
                "started_at": 10, "first_review_at": 50, "completed_at": 90, "lead_time_seconds": 90,
                "cycle_time_seconds": 80, "review_seconds": 40, "review_rounds": 1, "changes_requested": 0,
                "time_in_status": {"in_progress": 40, "review": 40},
            },
            "events": [
                {"actor_id": "dev", "action": "status", "from_status": "assigned",
                 "to_status": "in_progress", "note": None, "created_at": 10},
                {"actor_id": "pm", "action": "status", "from_status": "review",
                 "to_status": "merged", "note": "lgtm", "created_at": 90},
            ],
        }
    ],
}


class TaskExportRowsTest(unittest.TestCase):
    def test_task_rows_flatten_metrics_and_format_times(self) -> None:
        rows = list(csv.DictReader(io.StringIO(cli.render_task_export_csv(SAMPLE))))
        self.assertEqual(len(rows), 1)
        row = rows[0]
        self.assertEqual(row["title"], 'Fix "quotes", commas')
        self.assertEqual(row["priority"], "high")
        self.assertEqual(row["blocked_by"], "task_x;task_y")
        self.assertEqual(row["created_at"], "1970-01-01T00:00:00Z")
        self.assertEqual(row["completed_at"], "1970-01-01T00:01:30Z")
        self.assertEqual(row["cycle_time_seconds"], "80")
        self.assertEqual(row["reviewer"], "")
        self.assertEqual(row["time_in_status"], "in_progress=40;review=40")
        self.assertEqual(list(row), cli.TASK_EXPORT_COLUMNS)
        self.assertEqual((row["own_tokens"], row["total_tokens"], row["token_budget"]), ("", "", ""))

    def test_task_rows_carry_token_metrics(self) -> None:
        import copy

        data = copy.deepcopy(SAMPLE)
        data["tasks"][0]["metrics"]["tokens"] = {
            "own": {"total_tokens": 120}, "total": {"total_tokens": 450}, "token_budget": 1000,
            "remaining": 550, "exceeded": False,
        }
        row = next(csv.DictReader(io.StringIO(cli.render_task_export_csv(data))))
        self.assertEqual((row["own_tokens"], row["total_tokens"], row["token_budget"]), ("120", "450", "1000"))

    def test_event_rows_one_per_audit_entry(self) -> None:
        rows = list(csv.DictReader(io.StringIO(cli.render_task_export_csv(SAMPLE, events=True))))
        self.assertEqual([r["to_status"] for r in rows], ["in_progress", "merged"])
        self.assertEqual(rows[1]["note"], "lgtm")
        self.assertEqual(rows[1]["task_id"], "task_a")
        self.assertEqual(rows[0]["at"], "1970-01-01T00:00:10Z")
        self.assertEqual(rows[0]["note"], "")

    def test_duration_formatting(self) -> None:
        self.assertEqual(cli._fmt_duration(None), "-")
        self.assertEqual(cli._fmt_duration(75), "1m15s")
        self.assertEqual(cli._fmt_duration(3 * 3600 + 120), "3h02m")
        self.assertEqual(cli._fmt_duration(2 * 86400 + 5 * 3600), "2d05h")


class TaskExportE2ETest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.tmp_dir = tempfile.TemporaryDirectory()
        cls.tmp = Path(cls.tmp_dir.name)
        cls.cfg_dir = cls.tmp / "cfg"
        cls.cfg_dir.mkdir()
        http_port = _free_port()
        cls.base_url = f"http://127.0.0.1:{http_port}"
        cls.server_proc = subprocess.Popen(
            [
                str(_binary_path()),
                "--bind", f"127.0.0.1:{_free_port()}",
                "--http-bind", f"127.0.0.1:{http_port}",
                "--db", str(cls.tmp / "export.sqlite3"),
                "--open-registration",
            ],
            cwd=str(REPO_ROOT),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        deadline = time.time() + 10.0
        while True:
            try:
                with urllib.request.urlopen(f"{cls.base_url}/healthz", timeout=1.0):
                    break
            except Exception:
                if time.time() > deadline or cls.server_proc.poll() is not None:
                    cls.server_proc.kill()
                    raise RuntimeError("aircd did not become ready")
                time.sleep(0.1)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.server_proc.terminate()
        try:
            cls.server_proc.wait(timeout=5)
        except Exception:
            cls.server_proc.kill()
        cls.tmp_dir.cleanup()

    def cli(self, *args: str, check: bool = True) -> subprocess.CompletedProcess[bytes]:
        env = dict(os.environ)
        env["AIRCD_CONFIG_DIR"] = str(self.cfg_dir)
        env["PYTHONPATH"] = str(CLI_PY_ROOT)
        env["PYTHONUTF8"] = "1"
        proc = subprocess.run(
            [sys.executable, "-m", "aircd.cli", "--server", self.base_url, *args],
            env=env, cwd=str(self.tmp), capture_output=True,
        )
        if check and proc.returncode != 0:
            self.fail(f"CLI {args} exited {proc.returncode}\n{proc.stdout!r}\n{proc.stderr!r}")
        return proc

    def jcli(self, *args: str) -> dict:
        return json.loads(self.cli(*args).stdout.decode("utf-8"))

    def test_export_sprint_as_json_and_csv(self) -> None:
        self.jcli("--profile", "pm", "--json", "register", "--nick", "xpm", "--role", "leader")
        self.jcli("--profile", "dev", "--json", "register", "--nick", "xdev", "--role", "builder")
        self.jcli("--profile", "pm", "--json", "project", "create", "#xproj")
        self.jcli("--profile", "pm", "--json", "project", "add", "#xproj", "xdev")
        self.cli("--profile", "pm", "room", "open", "#xproj")
        done_id = self.jcli("--profile", "pm", "--json", "task", "create", "#xproj", "Ship it",
                            "--assign", "xdev")["task"]["id"]
        open_id = self.jcli("--profile", "pm", "--json", "task", "create", "#xproj", "Later",
                            "--assign", "xdev")["task"]["id"]
        self.cli("--profile", "dev", "task", "status", done_id, "in_progress")
        self.cli("--profile", "dev", "task", "status", done_id, "testing")
        self.cli("--profile", "dev", "task", "status", done_id, "review", "--branch", "feat/x",
                 "--note", "tests ok")
        self.cli("--profile", "pm", "task", "status", done_id, "merged")

        data = self.jcli("--profile", "dev", "task", "export", "--sprint", "#xproj-s1")
        self.assertEqual(data["sprint"], "#xproj-s1")
        self.assertEqual([t["task"]["id"] for t in data["tasks"]], [done_id, open_id])
        self.assertEqual(data["summary"]["completed"], 1)
        self.assertEqual(data["summary"]["review_rounds"], 1)
        self.assertEqual(data["tasks"][0]["events"][-1]["to_status"], "merged")
        self.assertIsNotNone(data["tasks"][0]["metrics"]["cycle_time_seconds"])

        out = self.tmp / "s1.csv"
        msg = self.cli("--profile", "pm", "task", "export", "#xproj", "-o", str(out)).stdout.decode()
        self.assertIn("Wrote 2 tasks", msg)
        self.assertIn("1 completed", msg)
        raw = out.read_bytes()
        self.assertNotIn(b"\r\n", raw)
        rows = list(csv.DictReader(io.StringIO(raw.decode("utf-8"))))
        self.assertEqual([r["status"] for r in rows], ["merged", "assigned"])
        self.assertTrue(rows[0]["completed_at"].endswith("Z"))

        again = self.cli("--profile", "pm", "task", "export", "#xproj", "-o", str(out), check=False)
        self.assertNotEqual(again.returncode, 0)
        self.assertIn(b"already exists", again.stderr)
        self.cli("--profile", "pm", "task", "export", "#xproj", "-o", str(out), "--force", "--events")
        events = list(csv.DictReader(io.StringIO(out.read_text(encoding="utf-8"))))
        self.assertIn("merged", [r["to_status"] for r in events])

        bad = self.cli("--profile", "pm", "task", "export", check=False)
        self.assertNotEqual(bad.returncode, 0)
        self.assertIn(b"--sprint", bad.stderr)


if __name__ == "__main__":
    unittest.main()
