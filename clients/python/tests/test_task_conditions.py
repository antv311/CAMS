"""Conditional workflows through the SDK and CLI on a real file-backed hub."""
from __future__ import annotations

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
from aircd.client import AircdHttpClient, AircdHttpError, task_create_payload
from tests.test_cli_e2e import CLI_PY_ROOT, REPO_ROOT, _binary_path, _free_port


class TaskConditionInputTest(unittest.TestCase):
    def test_json_file_and_object_input(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "output.json"
            path.write_text('{"route":"go"}', encoding="utf-8")
            self.assertEqual(cli._task_json_object("@" + str(path), "output"), {"route": "go"})
        for raw in ["[]", "null", "42", "bad"]:
            with self.assertRaises(cli.CliError):
                cli._task_json_object(raw, "output")
        self.assertIsNone(cli._task_json_object(None, "output"))
        payload = task_create_payload("#one", "Ordinary")
        self.assertNotIn("condition", payload)
        self.assertNotIn("optional", payload)


class TaskConditionsE2ETest(unittest.TestCase):
    def test_sdk_cli_branching_and_restart(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            base = f"http://127.0.0.1:{_free_port()}"
            db = Path(tmp) / "conditions.sqlite3"

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

                token = raw("POST", "/v1/register", {"nick": "branchpm", "role": "leader"})["token"]
                raw("POST", "/v1/projects", {"channel": "#branch"}, token)
                hub = AircdHttpClient(base, token=token)

                def run(*args: str, ok: bool = True) -> subprocess.CompletedProcess:
                    env = {**os.environ, "PYTHONPATH": str(CLI_PY_ROOT), "AIRCD_CONFIG_DIR": str(Path(tmp) / "cfg"), "PYTHONUTF8": "1"}
                    result = subprocess.run([sys.executable, "-m", "aircd.cli", "--server", base,
                        "--token", token, "--json", *args], cwd=REPO_ROOT, env=env,
                        capture_output=True, text=True, encoding="utf-8", timeout=15)
                    if ok:
                        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                    else:
                        self.assertNotEqual(result.returncode, 0)
                    return result

                parent = hub.create_task("#branch", "Choose", assignee="branchpm")["id"]
                predicate = {"type": "parent_output_equals", "pointer": "/route", "value": "go"}
                child = hub.create_task("#branch", "Chosen", parent=parent, assignee="branchpm",
                    condition=predicate, optional=True)
                self.assertEqual(child["status"], "dormant")
                created = json.loads(run("task", "create", "#branch", "Other", "--parent", parent,
                    "--assign", "branchpm", "--condition", json.dumps({**predicate, "value": "stop"})).stdout)["task"]
                self.assertEqual(created["status"], "dormant")
                enabled = json.loads(run("task", "enable", child["id"]).stdout)["task"]
                self.assertTrue(enabled["optional_enabled"])
                self.assertEqual(enabled["status"], "dormant")
                with self.assertRaises(AircdHttpError):
                    hub.set_task_status(child["id"], "testing")
                hub.set_task_status(parent, "testing")
                hub.set_task_status(parent, "review", note="SDK conditional workflow checks passed")
                stop(proc)
                proc = start()
                # Both the review state and the enabled-but-unresolved gate survived restart.
                shown = json.loads(run("task", "show", child["id"]).stdout)["task"]
                self.assertEqual(shown["status"], "dormant")
                self.assertTrue(shown["optional_enabled"])
                hub.set_task_status(parent, "merged", output={"route": "go"})
                self.assertEqual(raw("GET", f"/v1/tasks/{child['id']}", token=token)["task"]["status"], "assigned")
                self.assertEqual(raw("GET", f"/v1/tasks/{created['id']}", token=token)["task"]["status"], "skipped")
                run("task", "status", parent, "merged", "--output-json", '{"route":"stop"}', ok=False)
                shown_text = run("task", "show", parent).stdout
                self.assertEqual(json.loads(shown_text)["task"]["output"], {"route": "go"})

                optional = hub.create_task("#branch", "Enable via SDK", parent=child["id"], optional=True)
                self.assertEqual(hub.enable_optional_task(optional["id"])["status"], "open")
                run("task", "claim", optional["id"])
                output_file = Path(tmp) / "output.json"
                output_file.write_text('{"ok":true}', encoding="utf-8")
                done = json.loads(run("task", "done", optional["id"], "--output-json", "@" + str(output_file)).stdout)["task"]
                self.assertEqual(done["output"], {"ok": True})
                leased = hub.create_task("#branch", "SDK lease")
                run("task", "claim", leased["id"])
                self.assertEqual(hub.done_task(leased["id"], output={"count": 1})["output"], {"count": 1})
            finally:
                stop(proc)
