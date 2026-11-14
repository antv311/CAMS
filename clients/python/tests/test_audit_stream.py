"""End-to-end and client tests for GET /v1/audit/stream and `airc audit stream`."""

from __future__ import annotations

import json
import os
import queue
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request
from pathlib import Path

from aircd.client import AircdHttpClient, AircdHttpError
from tests.test_cli_e2e import CLI_PY_ROOT, REPO_ROOT, _binary_path, _free_port


OWNER_KEY = "audit-e2e-owner-key"


class AuditStreamE2ETest(unittest.TestCase):
    server_proc: subprocess.Popen[str]
    tmp_dir: tempfile.TemporaryDirectory[str]
    cfg_dir: Path
    http_port: int
    base_url: str

    @classmethod
    def setUpClass(cls) -> None:
        cls.tmp_dir = tempfile.TemporaryDirectory()
        tmp_path = Path(cls.tmp_dir.name)
        cls.cfg_dir = tmp_path / "cfg"
        cls.cfg_dir.mkdir(parents=True, exist_ok=True)
        db_path = tmp_path / "test_audit_stream.sqlite3"

        irc_port = _free_port()
        cls.http_port = _free_port()
        cls.base_url = f"http://127.0.0.1:{cls.http_port}"

        bin_path = _binary_path()
        env = dict(os.environ)
        env["AIRCD_REGISTER_KEY"] = OWNER_KEY
        cls.server_proc = subprocess.Popen(
            [
                str(bin_path),
                "--bind",
                f"127.0.0.1:{irc_port}",
                "--http-bind",
                f"127.0.0.1:{cls.http_port}",
                "--db",
                str(db_path),
            ],
            cwd=str(REPO_ROOT),
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )

        deadline = time.time() + 10.0
        ready = False
        while time.time() < deadline:
            if cls.server_proc.poll() is not None:
                out, err = cls.server_proc.communicate()
                raise RuntimeError(f"aircd exited early: {out}\n{err}")
            try:
                with urllib.request.urlopen(f"{cls.base_url}/healthz", timeout=1.0) as resp:
                    if resp.status == 200:
                        ready = True
                        break
            except Exception:
                time.sleep(0.1)
        if not ready:
            cls.server_proc.terminate()
            raise RuntimeError("aircd /healthz did not become ready in time")

    @classmethod
    def tearDownClass(cls) -> None:
        try:
            cls.server_proc.terminate()
            cls.server_proc.wait(timeout=5)
        except Exception:
            cls.server_proc.kill()
        cls.tmp_dir.cleanup()

    def _run_cli(self, *cli_args: str, check: bool = True, extra_env: dict[str, str] | None = None) -> subprocess.CompletedProcess[str]:
        env = dict(os.environ)
        env["AIRCD_CONFIG_DIR"] = str(self.cfg_dir)
        env["PYTHONPATH"] = str(CLI_PY_ROOT) + (
            os.pathsep + env["PYTHONPATH"] if env.get("PYTHONPATH") else ""
        )
        if extra_env:
            env.update(extra_env)
        proc = subprocess.run(
            [sys.executable, "-m", "aircd.cli", "--server", self.base_url, *cli_args],
            env=env,
            cwd=str(REPO_ROOT),
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
        )
        if check and proc.returncode != 0:
            self.fail(
                f"CLI {cli_args} exited {proc.returncode}\nSTDOUT:\n{proc.stdout}\nSTDERR:\n{proc.stderr}"
            )
        return proc

    def test_audit_stream_e2e_sse_ndjson_replay_and_cli(self) -> None:
        owner_client = AircdHttpClient(self.base_url, invite=OWNER_KEY)

        # Register a PM (owner) and a regular worker via the invite key.
        pm_reg = owner_client._request(
            "POST",
            "/v1/register",
            payload={"nick": "pm_audit", "role": "leader", "invite": OWNER_KEY},
        )
        pm_token = pm_reg["token"]
        pm_id = pm_reg["principal"]["id"]
        pm_client = AircdHttpClient(self.base_url, token=pm_token, invite=OWNER_KEY)

        worker_reg = owner_client._request(
            "POST",
            "/v1/register",
            payload={"nick": "worker_audit", "role": "member", "invite": OWNER_KEY},
        )
        worker_token = worker_reg["token"]
        worker_id = worker_reg["principal"]["id"]
        worker_client = AircdHttpClient(self.base_url, token=worker_token)

        # Issue a read-scoped token to verify 403 scope enforcement.
        read_tok_res = worker_client._request(
            "POST",
            "/v1/tokens",
            payload={"label": "siem-reader", "scopes": ["read"]},
        )
        read_token = read_tok_res["token"]

        # Unauthenticated stream call -> 401
        with self.assertRaises(AircdHttpError) as anon_ctx:
            list(AircdHttpClient(self.base_url).stream_audit_logs(limit=1))
        self.assertEqual(anon_ctx.exception.status, 401)

        # Non-admin (read-scoped) token -> 403
        with self.assertRaises(AircdHttpError) as read_ctx:
            list(AircdHttpClient(self.base_url, token=read_token).stream_audit_logs(limit=1))
        self.assertEqual(read_ctx.exception.status, 403)

        # Create project & role so worker can work on tasks.
        pm_client._request(
            "POST",
            "/v1/projects",
            payload={"channel": "#audit-e2e", "name": "Audit E2E"},
        )
        pm_client._request(
            "POST",
            "/v1/projects/members",
            payload={"channel": "#audit-e2e", "nick": "worker_audit", "role": "member"},
        )

        # Trigger live SSE streaming of mixed task_state + security_audit events.
        sse_events: list[dict] = []
        sse_err: list[BaseException] = []
        stream_ready = threading.Event()

        def _collect_sse() -> None:
            try:
                stream_ready.set()
                for ev in owner_client.stream_audit_logs(
                    stream_format="sse",
                    replay=50,
                    keepalive_secs=5,
                    owner=True,
                    timeout=8.0,
                ):
                    sse_events.append(ev)
                    actions_seen = {x.get("action") for x in sse_events}
                    if {"task.created", "task.status", "auth.http_failed"}.issubset(actions_seen):
                        break
            except BaseException as exc:  # pragma: no cover - surfaced below
                sse_err.append(exc)

        t = threading.Thread(target=_collect_sse, daemon=True)
        t.start()
        self.assertTrue(stream_ready.wait(timeout=2.0))
        time.sleep(0.15)

        created = pm_client.create_task(
            "#audit-e2e",
            "Stream live audit events",
            kind="feature",
            assignee=worker_id,
        )
        task_id = created["id"]
        worker_client.set_task_status(task_id, "in_progress", branch="feat/audit-e2e")
        # Trigger an auth rejection after transition to verify security_audit events stream live too.
        try:
            urllib.request.urlopen(f"{self.base_url}/v1/audit/stream", timeout=2.0)
        except urllib.error.HTTPError as err:
            self.assertEqual(err.code, 401)

        t.join(timeout=8.0)
        self.assertFalse(t.is_alive(), "SSE collector thread timed out")
        self.assertEqual(sse_err, [])

        actions = [e["action"] for e in sse_events]
        self.assertIn("task.created", actions)
        self.assertIn("task.status", actions)
        self.assertIn("auth.http_failed", actions)
        # Verify structured task_state fields.
        created_ev = next(e for e in sse_events if e["action"] == "task.created" and e.get("task_id") == task_id)
        status_ev = next(e for e in sse_events if e["action"] == "task.status" and e.get("task_id") == task_id)
        self.assertEqual(status_ev["category"], "task_state")
        self.assertEqual(status_ev["task_id"], task_id)
        self.assertEqual(status_ev["from_status"], "assigned")
        self.assertEqual(status_ev["to_status"], "in_progress")
        self.assertIsNotNone(status_ev.get("timestamp_iso"))
        # Sequences must strictly increase with no duplicates.
        seqs = [e["seq"] for e in sse_events]
        self.assertEqual(seqs, sorted(seqs))
        self.assertEqual(len(set(seqs)), len(seqs))

        # Verify Last-Event-ID resumes strictly after `task.created` without duplicating it.
        resumed = list(
            owner_client.stream_audit_logs(
                stream_format="ndjson",
                last_event_id=created_ev["seq"],
                limit=1,
                owner=True,
                timeout=5.0,
            )
        )
        self.assertEqual(len(resumed), 1)
        self.assertGreater(resumed[0]["seq"], created_ev["seq"])
        self.assertEqual(resumed[0]["action"], "task.status")

        # Verify NDJSON filtered replay returns only that task's state transitions via CLI.
        cli_proc = self._run_cli(
            "audit",
            "stream",
            "--format",
            "ndjson",
            "--category",
            "task_state",
            "--task-id",
            task_id,
            "--replay",
            "20",
            "--limit",
            "2",
            "--invite",
            OWNER_KEY,
        )
        cli_events = [json.loads(line) for line in cli_proc.stdout.strip().splitlines() if line.strip()]
        self.assertEqual(len(cli_events), 2)
        self.assertEqual([e["action"] for e in cli_events], ["task.created", "task.status"])
        self.assertTrue(all(e["task_id"] == task_id for e in cli_events))


if __name__ == "__main__":
    unittest.main()
