"""File-backed migration, SDK/CLI attributes and restart against an actual hub."""
from __future__ import annotations

import json
from contextlib import closing
import os
import sqlite3
import subprocess
import sys
import tempfile
import time
import unittest
import urllib.request
from pathlib import Path

from aircd.client import AircdHttpClient, AircdHttpError
from tests.test_cli_e2e import CLI_PY_ROOT, REPO_ROOT, _binary_path, _free_port


class TaskAttributesE2ETest(unittest.TestCase):
    def test_legacy_migration_cli_sdk_and_restart(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            base = f"http://127.0.0.1:{_free_port()}"
            db = Path(tmp) / "attributes.sqlite3"
            with closing(sqlite3.connect(db)) as conn:
                conn.executescript("""
                    CREATE TABLE tasks (id TEXT PRIMARY KEY, channel TEXT NOT NULL,
                    title TEXT NOT NULL, status TEXT NOT NULL, claimed_by TEXT,
                    lease_expires_at INTEGER, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
                    INSERT INTO tasks VALUES ('task_legacy', '#labels', 'Legacy', 'open', NULL, NULL, 1, 1);
                """)

            def start() -> subprocess.Popen:
                proc = subprocess.Popen([
                    str(_binary_path()), "--bind", f"127.0.0.1:{_free_port()}",
                    "--http-bind", base.removeprefix("http://"), "--db", str(db), "--open-registration",
                ], cwd=REPO_ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                deadline = time.monotonic() + 20
                while True:
                    try:
                        with urllib.request.urlopen(base + "/healthz", timeout=1):
                            return proc
                    except OSError:
                        if time.monotonic() > deadline or proc.poll() is not None:
                            proc.terminate()
                            proc.wait(timeout=5)
                            self.fail("temporary hub did not become ready")
                        time.sleep(0.1)

            def stop(proc: subprocess.Popen) -> None:
                proc.terminate()
                try:
                    proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait(timeout=5)

            proc = start()
            try:
                def raw(method: str, path: str, payload: dict | None = None, token: str = "") -> dict:
                    req = urllib.request.Request(base + path, method=method,
                        data=None if payload is None else json.dumps(payload).encode(), headers={
                            "Content-Type": "application/json", "Authorization": "Bearer " + token,
                        })
                    with urllib.request.urlopen(req, timeout=5) as response:
                        return json.load(response)

                token = raw("POST", "/v1/register", {"nick": "labelpm", "role": "leader"})["token"]
                raw("POST", "/v1/projects", {"channel": "#labels"}, token)
                hub = AircdHttpClient(base, token=token)

                def run(*args: str, ok: bool = True, as_json: bool = True) -> subprocess.CompletedProcess:
                    env = {**os.environ, "PYTHONPATH": str(CLI_PY_ROOT), "AIRCD_CONFIG_DIR": str(Path(tmp) / "cfg"), "PYTHONUTF8": "1"}
                    result = subprocess.run([sys.executable, "-m", "aircd.cli", "--server", base,
                        "--token", token, *(["--json"] if as_json else []), *args], cwd=REPO_ROOT, env=env,
                        capture_output=True, text=True, encoding="utf-8", timeout=15)
                    self.assertEqual(result.returncode == 0, ok, result.stdout + result.stderr)
                    return result

                legacy = raw("GET", "/v1/tasks/task_legacy", token=token)["task"]
                self.assertEqual(legacy["tags"], [])
                self.assertEqual(legacy["metadata"], {})
                meta = {"nested": {"ok": True}, "values": [1, None, "caf\u00e9"], "null": None}
                meta_file = Path(tmp) / "metadata.json"
                meta_file.write_text(json.dumps(meta), encoding="utf-8")
                special_tag = "ui,+ /% caf\u00e9"
                created = json.loads(run("task", "create", "#labels", "CLI task", "--tag", special_tag,
                    "--tag", "Release", "--tag", "Release", "--metadata", "@" + str(meta_file)).stdout)["task"]
                task_id = created["id"]
                self.assertEqual(created["tags"], ["Release", special_tag])
                self.assertEqual(created["metadata"], meta)
                self.assertEqual([t["id"] for t in hub.list_tasks("#labels", tag=special_tag)], [task_id])
                self.assertEqual(hub.list_tasks("#labels", tag="release"), [])
                listed = json.loads(run("task", "list", "#labels", "--tag", special_tag, "--priority", "normal", "--ready").stdout)
                self.assertEqual([t["id"] for t in listed["tasks"]], [task_id])
                run("task", "metadata", task_id, "[]", ok=False)
                with self.assertRaises(AircdHttpError):
                    hub.set_task_attributes(task_id, tags=[""])
                self.assertEqual(hub.list_tasks("#labels", tag=special_tag)[0]["metadata"], meta)
                self.assertEqual(hub.set_task_attributes(task_id, metadata={"revision": 2})["tags"], created["tags"])
                json.loads(run("task", "tags", task_id, "--set", "kept", "--set", special_tag).stdout)
                sdk_task = hub.create_task("#labels", "SDK task", tags=["sdk"], metadata=meta)
                self.assertEqual(sdk_task["metadata"], meta)
                batch = hub.create_task_batch([{"channel":"#labels", "title":"Batch", "tags":["batch"], "metadata":{"k":"v"}}])
                self.assertEqual(batch["tasks"][0]["metadata"], {"k":"v"})
                stop(proc)
                proc = start()
                shown = json.loads(run("task", "show", task_id).stdout)["task"]
                self.assertEqual(shown["metadata"], {"revision": 2})
                self.assertEqual(shown["tags"], ["kept", special_tag])
                self.assertIn('"revision": 2', run("task", "metadata", task_id, as_json=False).stdout)
                self.assertIn(special_tag, run("task", "tags", task_id, as_json=False).stdout)
                self.assertIn("metadata=", run("task", "show", task_id, as_json=False).stdout)
                exported = raw("GET", "/v1/tasks/export?channel=%23labels", token=token)
                self.assertEqual(next(r["task"] for r in exported["tasks"] if r["task"]["id"] == task_id)["tags"], shown["tags"])
                cleared = json.loads(run("task", "tags", task_id, "--clear").stdout)["task"]
                self.assertEqual(cleared["tags"], [])
                self.assertEqual(cleared["metadata"], {"revision":2})
                self.assertEqual(json.loads(run("task", "metadata", task_id, "{}").stdout)["task"]["metadata"], {})
                self.assertEqual(hub.list_tasks("#labels", tag=special_tag), [])
            finally:
                stop(proc)
