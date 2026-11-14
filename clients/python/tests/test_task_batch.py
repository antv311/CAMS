"""Exercise SDK task batches against a real temporary, file-backed hub."""
from __future__ import annotations

import json
import subprocess
import tempfile
import time
import unittest
import urllib.request
from pathlib import Path

from aircd.client import AircdHttpClient, AircdHttpError
from tests.test_cli_e2e import REPO_ROOT, _binary_path, _free_port


class TaskBatchE2ETest(unittest.TestCase):
    def test_backlog_creation_bulk_workflow_and_rollback(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            base = f"http://127.0.0.1:{_free_port()}"
            proc = subprocess.Popen([
                str(_binary_path()), "--bind", f"127.0.0.1:{_free_port()}",
                "--http-bind", base.removeprefix("http://"),
                "--db", str(Path(tmp) / "batch.sqlite3"), "--open-registration",
            ], cwd=REPO_ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            try:
                deadline = time.monotonic() + 20
                while True:
                    try:
                        with urllib.request.urlopen(base + "/healthz", timeout=1):
                            break
                    except OSError:
                        if time.monotonic() > deadline or proc.poll() is not None:
                            self.fail("temporary hub did not become ready")
                        time.sleep(0.1)

                def raw(path: str, payload: dict, token: str = "") -> dict:
                    req = urllib.request.Request(base + path, data=json.dumps(payload).encode(), headers={
                        "Content-Type": "application/json", "Authorization": "Bearer " + token,
                    })
                    with urllib.request.urlopen(req, timeout=5) as response:
                        return json.load(response)

                token = raw("/v1/register", {"nick": "batchpm", "role": "leader"})["token"]
                raw("/v1/projects", {"channel": "#batch"}, token)
                hub = AircdHttpClient(base, token=token)
                result = hub.create_task_batch([
                    {"key": "a", "channel": "#batch", "title": "First", "assignee": "batchpm"},
                    {"key": "b", "channel": "#batch", "title": "Later", "assignee": "batchpm", "blocked_by": ["$a"]},
                ])
                a, b = result["task_ids"]["a"], result["task_ids"]["b"]
                self.assertEqual([task["id"] for task in result["tasks"]], [a, b])
                with self.assertRaises(AircdHttpError) as rejected:
                    hub.bulk_task_status([
                        {"task_id": a, "status": "testing"},
                        {"task_id": b, "status": "testing"},
                    ])
                self.assertEqual(rejected.exception.status, 400)
                hub.set_task_status(a, "testing")  # proves the earlier mutation rolled back
                hub.set_task_status(a, "review", note="SDK end-to-end checks passed")
                changed = hub.bulk_task_status([
                    {"task_id": a, "status": "merged"},
                    {"task_id": b, "status": "testing"},
                ])
                self.assertEqual([task["status"] for task in changed], ["merged", "testing"])
                with self.assertRaises(AircdHttpError):
                    hub.create_task_batch([
                        {"channel": "#batch", "title": "Rolled back"},
                        {"channel": "#batch", "title": "Invalid", "blocked_by": ["missing"]},
                    ])
                with urllib.request.urlopen(urllib.request.Request(
                    base + "/v1/tasks?channel=batch&all=true", headers={"Authorization": "Bearer " + token}
                )) as response:
                    tasks = json.load(response)["tasks"]
                self.assertFalse(any(task["title"] == "Rolled back" for task in tasks))
            finally:
                proc.terminate()
                try:
                    proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait(timeout=5)
