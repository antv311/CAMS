"""Groups, subtask flags and the review test-report rule (issue #23).

Library and CLI tests run against a stdlib fake hub and pin the wire contract
the hub's group and subtask endpoints are built to:

    GET  /v1/groups                  -> {"groups": [group, ...]}
    POST /v1/groups                  {name, lead, workers, description?,
                                      lead_prompt?, worker_prompt?} -> {"group"}
    POST /v1/groups/{name}/attach    {"channel": "#proj"} -> {"group"}
    PATCH /v1/groups/{name}          {lead?, add_workers?, remove_workers?} -> {"group"}
    POST /v1/tasks                   ... + parent_task_id?, reviewer?

where lead and each worker are {"template", "model", "count", "nick"?}.

The integration tests run the real aircd binary (skipped when it is not
built), including group creation/attachment and handing in a task for review
with a note or an attached report.
"""

from __future__ import annotations

import contextlib
import io
import json
import os
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.request
from pathlib import Path
from typing import Any
from unittest import mock

from aircd import cli
from aircd.client import (
    AircdHttpClient,
    AircdHttpError,
    Group,
    GroupMember,
    task_create_payload,
)

from tests.test_templates import CLI_PY_ROOT, OWNER_KEY, _binary_path, _FakeHub, _free_port

GROUP_REPLY = {
    "name": "dev",
    "description": "Opus lead with local workers",
    "lead": {"template": "claude", "model": "opus", "count": 1, "nick": "dev-lead"},
    "workers": [
        {"template": "ollama", "model": "gemma3", "count": 3},
        {"template": "ollama", "model": "qwen3", "count": 3},
    ],
    "lead_prompt": "Split the work into subtasks.\nReview every result.",
    "worker_prompt": "Write the code you are given.",
    "projects": ["#cams"],
}


class _HubCase(unittest.TestCase):
    def setUp(self) -> None:
        self.hub = _FakeHub()
        threading.Thread(target=self.hub.serve_forever, daemon=True).start()
        self.url = f"http://127.0.0.1:{self.hub.server_address[1]}"

    def tearDown(self) -> None:
        self.hub.shutdown()
        self.hub.server_close()

    def reply(self, *replies: tuple[int, Any]) -> None:
        self.hub.replies.extend(replies)


class GroupModelTest(unittest.TestCase):
    def test_parse_member_specs(self) -> None:
        self.assertEqual(GroupMember.parse("claude:opus"), GroupMember("claude", "opus", 1))
        self.assertEqual(GroupMember.parse(" ollama:gemma3*3 "), GroupMember("ollama", "gemma3", 3))
        # Models may contain colons and an 'x' without being read as a count.
        self.assertEqual(GroupMember.parse("ollama:qwen2.5-coder:7b*2"), GroupMember("ollama", "qwen2.5-coder:7b", 2))
        self.assertEqual(GroupMember.parse("ollama:mixtral-8x7b"), GroupMember("ollama", "mixtral-8x7b", 1))

    def test_parse_rejects_bad_specs(self) -> None:
        for spec in ("opus", "claude:", ":opus", "a:b*0", "a:b*two", "a:*3"):
            with self.subTest(spec=spec), self.assertRaises(ValueError):
                GroupMember.parse(spec)

    def test_group_round_trip(self) -> None:
        group = Group.from_dict(GROUP_REPLY)
        self.assertEqual(group.lead.nick, "dev-lead")
        self.assertEqual(group.worker_count, 6)
        self.assertEqual(group.projects, ["#cams"])
        payload = group.to_dict()
        self.assertNotIn("projects", payload)  # attach manages projects
        self.assertEqual(Group.from_dict(payload).to_dict(), payload)
        self.assertEqual(payload["workers"][0], {"template": "ollama", "model": "gemma3", "count": 3})

    def test_group_accepts_string_members(self) -> None:
        group = Group.from_dict({"name": "g", "lead": "claude:opus", "workers": ["ollama:gemma3*2"]})
        self.assertEqual(group.lead, GroupMember("claude", "opus"))
        self.assertEqual(group.workers, [GroupMember("ollama", "gemma3", 2)])

    def test_label(self) -> None:
        self.assertEqual(GroupMember("ollama", "gemma3", 3).label(), "ollama:gemma3 x3")
        self.assertEqual(GroupMember("claude", "opus", nick="lead").label(), "claude:opus (@lead)")


class TaskPayloadTest(unittest.TestCase):
    def test_subtask_fields_only_when_set(self) -> None:
        plain = task_create_payload("#p", "t")
        self.assertNotIn("parent_task_id", plain)
        self.assertNotIn("reviewer", plain)
        sub = task_create_payload("#p", "t", parent=" task_1 ", reviewer="@lead")
        self.assertEqual(sub["parent_task_id"], "task_1")
        self.assertEqual(sub["reviewer"], "lead")
        blank = task_create_payload("#p", "t", parent="  ", reviewer=" @ ")
        self.assertNotIn("parent_task_id", blank)
        self.assertNotIn("reviewer", blank)


class GroupHttpClientTest(_HubCase):
    def test_update_group_sends_only_changes_and_owner_key(self) -> None:
        self.reply((200, {"group": GROUP_REPLY}))
        update = {"add_workers": [{"template": "c", "model": "m", "nick": "worker1"}],
                  "remove_workers": [{"template": "ollama", "model": "gemma3", "count": 2}]}
        group = AircdHttpClient(self.url, token="tok", invite="owner").update_group("dev team", update)
        self.assertEqual(group.projects, ["#cams"])
        req = self.hub.requests[0]
        self.assertEqual((req["method"], req["path"]), ("PATCH", "/v1/groups/dev%20team"))
        self.assertEqual(req["body"], update)
        self.assertEqual(req["headers"]["x-aircd-invite"], "owner")

    def test_update_group_validation_and_errors(self) -> None:
        hub = AircdHttpClient(self.url, token="tok")
        for update in ({}, {"add_workers": []}, {"workers": []}, {"invite": "owner"}):
            with self.subTest(update=update), self.assertRaises(AircdHttpError):
                hub.update_group("dev", update)
        with self.assertRaises(AircdHttpError):
            hub.update_group(" ", {"lead": {"template": "c", "model": "m"}})
        self.assertEqual(self.hub.requests, [])
        self.reply((404, {"error": "no group named dev"}))
        with self.assertRaises(AircdHttpError) as caught:
            hub.update_group("dev", {"lead": {"template": "c", "model": "m"}})
        self.assertEqual(caught.exception.status, 404)
        self.assertEqual(self.hub.requests[0]["headers"]["authorization"], "Bearer tok")
        self.assertNotIn("x-aircd-invite", self.hub.requests[0]["headers"])

    def test_list_groups(self) -> None:
        self.reply((200, {"groups": [GROUP_REPLY]}))
        groups = AircdHttpClient(self.url, token="tok").list_groups()
        self.assertEqual([g.name for g in groups], ["dev"])
        req = self.hub.requests[0]
        self.assertEqual((req["method"], req["path"]), ("GET", "/v1/groups"))
        self.assertEqual(req["headers"]["authorization"], "Bearer tok")
        self.assertNotIn("x-aircd-invite", req["headers"])

    def test_create_group_with_token(self) -> None:
        self.reply((200, {"ok": True, "group": GROUP_REPLY}))
        group = Group.from_dict({k: v for k, v in GROUP_REPLY.items() if k != "projects"})
        saved = AircdHttpClient(self.url, token="tok").create_group(group)
        self.assertEqual(saved.projects, ["#cams"])
        req = self.hub.requests[0]
        self.assertEqual((req["method"], req["path"]), ("POST", "/v1/groups"))
        self.assertEqual(req["body"], group.to_dict())
        self.assertNotIn("x-aircd-invite", req["headers"])

    def test_create_and_attach_send_owner_key_when_set(self) -> None:
        self.reply((200, {"group": GROUP_REPLY}), (200, {"group": GROUP_REPLY}))
        hub = AircdHttpClient(self.url, token="tok", invite="owner")
        hub.create_group(Group("dev", GroupMember("claude", "opus"), [GroupMember("ollama", "gemma3", 3)]))
        hub.attach_group("dev team", " #cams ")
        create, attach = self.hub.requests
        self.assertEqual(create["headers"]["x-aircd-invite"], "owner")
        self.assertEqual((attach["method"], attach["path"]), ("POST", "/v1/groups/dev%20team/attach"))
        self.assertEqual(attach["body"], {"channel": "#cams"})
        self.assertEqual(attach["headers"]["x-aircd-invite"], "owner")

    def test_bad_input_is_refused_before_any_request(self) -> None:
        hub = AircdHttpClient(self.url, token="tok")
        with self.assertRaises(AircdHttpError):
            hub.attach_group("dev", "cams")
        with self.assertRaises(AircdHttpError):
            hub.attach_group("  ", "#cams")
        with self.assertRaises(AircdHttpError):
            hub.create_group(Group(" ", GroupMember("claude", "opus")))
        self.assertEqual(self.hub.requests, [])

    def test_errors_carry_status(self) -> None:
        self.reply((404, {"error": "no group named dev"}))
        with self.assertRaises(AircdHttpError) as caught:
            AircdHttpClient(self.url, token="tok").attach_group("dev", "#cams")
        self.assertEqual(caught.exception.status, 404)
        self.assertIn("no group named dev", str(caught.exception))

    def test_create_subtask_and_review(self) -> None:
        self.reply((200, {"task": {"id": "task_2"}}), (200, {"task": {"id": "task_2", "status": "review"}}))
        hub = AircdHttpClient(self.url, token="tok")
        hub.create_task("#cams", "part one", assignee="w1", parent="task_1", reviewer="@lead")
        with self.assertRaises(AircdHttpError):
            hub.set_task_status("task_2", "review")
        with self.assertRaises(AircdHttpError):
            hub.set_task_status("task_2", "review", note="   ")
        self.assertEqual(hub.set_task_status("task_2", "review", note="pytest: 9 passed")["status"], "review")
        create, status = self.hub.requests
        self.assertEqual(create["body"]["parent_task_id"], "task_1")
        self.assertEqual(create["body"]["reviewer"], "lead")
        self.assertEqual(create["body"]["assignee"], "w1")
        self.assertEqual(status["path"], "/v1/tasks/task_2/status")
        self.assertEqual(status["body"], {"status": "review", "note": "pytest: 9 passed", "branch": None})


class GroupCliTest(_HubCase):
    """`airc group` and the task flags, run in-process against the fake hub."""

    def setUp(self) -> None:
        super().setUp()
        self.cfg = tempfile.TemporaryDirectory()
        env = {k: v for k, v in os.environ.items() if not k.startswith(("AIRCD_", "AIRC_"))}
        env["AIRCD_CONFIG_DIR"] = self.cfg.name
        patcher = mock.patch.dict(os.environ, env, clear=True)
        patcher.start()
        self.addCleanup(patcher.stop)
        self.addCleanup(self.cfg.cleanup)

    def run_cli(self, *args: str, token: str | None = "tok") -> tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        argv = ["--server", self.url, *(["--token", token] if token else []), *args]
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = cli.main(argv)
        return code, out.getvalue(), err.getvalue()

    def test_group_update_flags(self) -> None:
        self.reply((200, {"group": GROUP_REPLY}))
        code, out, err = self.run_cli(
            "group", "update", "dev", "--add-worker", "ollama:gemma3*2",
            "--add-worker", "claude:sonnet", "--remove-worker", "ollama:qwen3*3",
            "--lead", "claude:opus", "--invite", "owner",
        )
        self.assertEqual(code, 0, err)
        self.assertIn("Updated group:", out)
        req = self.hub.requests[0]
        self.assertEqual((req["method"], req["path"]), ("PATCH", "/v1/groups/dev"))
        self.assertEqual(req["headers"]["x-aircd-invite"], "owner")
        self.assertEqual(req["body"], {
            "lead": {"template": "claude", "model": "opus", "count": 1},
            "add_workers": [{"template": "ollama", "model": "gemma3", "count": 2},
                            {"template": "claude", "model": "sonnet", "count": 1}],
            "remove_workers": [{"template": "ollama", "model": "qwen3", "count": 3}],
        })

    def test_group_update_file_and_flags(self) -> None:
        self.reply((200, {"group": GROUP_REPLY}))
        update = {"lead": {"template": "claude", "model": "opus", "nick": "new-lead"},
                  "remove_workers": [{"template": "c", "model": "m", "nick": "worker1"}],
                  "add_workers": [{"template": "old", "model": "m"}]}
        path = Path(self.cfg.name) / "update.json"
        path.write_text(json.dumps(update), encoding="utf-8")
        code, out, err = self.run_cli("--json", "group", "update", "dev", "--file", str(path), "--add-worker", "ollama:qwen3")
        self.assertEqual(code, 0, err)
        self.assertEqual(json.loads(out)["group"]["name"], "dev")
        update["add_workers"] = [{"template": "ollama", "model": "qwen3", "count": 1}]
        self.assertEqual(self.hub.requests[0]["body"], update)

    def test_group_update_validation(self) -> None:
        for flags, message in [
            ([], "group update needs"),
            (["--lead", "c:m*2"], "exactly one lead"),
            (["--add-worker", "c:m*0"], "at least 1"),
            (["--remove-worker", "bad"], "TEMPLATE:MODEL"),
            (["--file", "/no/such/update.json"], "cannot read group update JSON"),
        ]:
            with self.subTest(flags=flags):
                code, _, err = self.run_cli("group", "update", "dev", *flags)
                self.assertEqual(code, 1)
                self.assertIn(message, err)
        for raw in ("[]", "bad json", '{"workers": []}'):
            with self.subTest(raw=raw), mock.patch.object(sys, "stdin", io.StringIO(raw)):
                self.assertEqual(self.run_cli("group", "update", "dev", "--file", "-")[0], 1)
        self.assertEqual(self.hub.requests, [])

    def test_group_list(self) -> None:
        self.reply((200, {"groups": [GROUP_REPLY]}), (200, {"groups": []}))
        code, out, _ = self.run_cli("group", "list")
        self.assertEqual(code, 0)
        self.assertIn("dev: lead claude:opus (@dev-lead), 6 worker(s)", out)
        self.assertIn("workers: ollama:gemma3 x3, ollama:qwen3 x3", out)
        self.assertIn("projects: #cams", out)
        self.assertIn("lead prompt: Split the work into subtasks. ...", out)
        code, out, _ = self.run_cli("group", "list")
        self.assertIn("No groups.", out)

    def test_group_create_from_flags(self) -> None:
        prompt = Path(self.cfg.name) / "worker.md"
        prompt.write_text("Only write code.\nNo project context.", encoding="utf-8")
        self.reply((200, {"group": GROUP_REPLY}))
        code, out, err = self.run_cli(
            "group", "create", "dev",
            "--lead", "claude:opus",
            "--worker", "ollama:gemma3*3", "--worker", "ollama:qwen3*3",
            "--description", "Opus lead with local workers",
            "--lead-prompt", "Split, test, review, submit.",
            "--worker-prompt", f"@{prompt}",
            "--invite", "owner",
        )
        self.assertEqual(code, 0, err)
        self.assertIn("Created group:", out)
        req = self.hub.requests[0]
        self.assertEqual(req["headers"]["x-aircd-invite"], "owner")
        self.assertEqual(req["body"], {
            "name": "dev",
            "lead": {"template": "claude", "model": "opus", "count": 1},
            "workers": [
                {"template": "ollama", "model": "gemma3", "count": 3},
                {"template": "ollama", "model": "qwen3", "count": 3},
            ],
            "description": "Opus lead with local workers",
            "lead_prompt": "Split, test, review, submit.",
            "worker_prompt": "Only write code.\nNo project context.",
        })

    def test_group_create_from_file_with_overrides(self) -> None:
        exported = json.dumps({"ok": True, "group": GROUP_REPLY})
        self.reply((200, {"group": {**GROUP_REPLY, "name": "dev2"}}))
        with mock.patch.object(sys, "stdin", io.StringIO(exported)):
            code, _, err = self.run_cli("--json", "group", "create", "dev2", "--file", "-", "--worker", "ollama:gemma3*1")
        self.assertEqual(code, 0, err)
        body = self.hub.requests[0]["body"]
        self.assertEqual(body["name"], "dev2")
        self.assertEqual(body["lead"]["nick"], "dev-lead")
        self.assertEqual(body["workers"], [{"template": "ollama", "model": "gemma3", "count": 1}])
        self.assertEqual(body["lead_prompt"], GROUP_REPLY["lead_prompt"])
        self.assertNotIn("projects", body)

    def test_group_create_validation(self) -> None:
        cases = [
            (["--worker", "ollama:gemma3"], "needs --lead"),
            (["--lead", "claude:opus"], "at least one --worker"),
            (["--lead", "claude:opus*2", "--worker", "o:g"], "exactly one lead"),
            (["--lead", "opus", "--worker", "o:g"], "TEMPLATE:MODEL"),
            (["--lead", "c:o", "--worker", "o:g", "--lead-prompt", "@/no/such/file.md"], "cannot read prompt file"),
        ]
        for flags, message in cases:
            with self.subTest(flags=flags):
                code, _, err = self.run_cli("group", "create", "dev", *flags)
                self.assertEqual(code, 1)
                self.assertIn(message, err)
        self.assertEqual(self.hub.requests, [])

    def test_group_attach_and_errors(self) -> None:
        self.reply((200, {"group": GROUP_REPLY}), (403, {"error": "only the owner or a project manager"}))
        code, out, _ = self.run_cli("group", "attach", "dev", "#cams")
        self.assertEqual(code, 0)
        self.assertIn("Attached group dev to #cams.", out)
        self.assertEqual(self.hub.requests[0]["path"], "/v1/groups/dev/attach")
        code, _, err = self.run_cli("group", "attach", "dev", "#other")
        self.assertEqual(code, 1)
        self.assertIn("HTTP 403", err)
        code, _, err = self.run_cli("group", "attach", "dev", "cams")
        self.assertEqual(code, 1)
        self.assertIn("must start with '#'", err)

    def test_group_needs_credentials(self) -> None:
        code, _, err = self.run_cli("group", "list", token=None)
        self.assertEqual(code, 1)
        self.assertIn("needs an account token", err)

    def test_task_create_with_parent_and_reviewer(self) -> None:
        task = {"id": "task_2", "kind": "task", "channel": "#cams", "title": "part one",
                "assignee": "w1", "parent_task_id": "task_1", "reviewer": "lead"}
        self.reply((200, {"task": task}))
        code, out, err = self.run_cli(
            "task", "create", "#cams", "part", "one", "--assign", "w1", "--parent", "task_1", "--reviewer", "@lead"
        )
        self.assertEqual(code, 0, err)
        self.assertIn("subtask of task_1", out)
        self.assertIn("reviewer @lead", out)
        body = self.hub.requests[0]["body"]
        self.assertEqual((body["parent_task_id"], body["reviewer"], body["title"]), ("task_1", "lead", "part one"))

    def test_task_show_lists_parent_reviewer_and_subtasks(self) -> None:
        self.reply((200, {
            "task": {"id": "task_1", "kind": "feature", "status": "in_progress", "channel": "#cams",
                     "title": "big", "parent_task_id": None, "reviewer": "pm"},
            "subtasks": [{"id": "task_2", "status": "review", "assignee": "w1", "title": "part one"}],
            "events": [],
        }))
        code, out, _ = self.run_cli("task", "show", "task_1")
        self.assertEqual(code, 0)
        self.assertIn("parent=- reviewer=pm", out)
        self.assertIn("subtask task_2 [review] @w1: part one", out)

    def test_task_show_question_waits_for_reviewer_pm_or_owner(self) -> None:
        for status, waiting in (("open", "reviewer @lead"), ("escalated_pm", "the project manager"), ("escalated", "the owner")):
            with self.subTest(status=status):
                self.reply((200, {
                    "task": {"id": "task_2", "kind": "task", "status": "blocked", "channel": "#cams",
                             "title": "child", "parent_task_id": "task_1", "reviewer": "lead"},
                    "question": {"status": status, "asked_by": "worker", "question": "Scope?"},
                }))
                code, out, err = self.run_cli("task", "show", "task_2")
                self.assertEqual(code, 0, err)
                self.assertIn(f"WAITING on {waiting}", out)

    def test_escalation_output_does_not_assume_owner_destination(self) -> None:
        self.reply((200, {"task": {"id": "task_2", "status": "blocked"}}))
        code, out, err = self.run_cli("task", "escalate", "task_2", "PM decision")
        self.assertEqual(code, 0, err)
        self.assertIn("escalated; task is blocked", out)

    def test_review_without_report_is_refused_before_the_hub(self) -> None:
        for note in ([], ["--note", "  "]):
            code, _, err = self.run_cli("task", "status", "task_2", "review", *note)
            self.assertEqual(code, 1)
            self.assertIn("needs a test report", err)
        self.assertEqual(self.hub.requests, [])

    def test_other_moves_need_no_report(self) -> None:
        self.reply((200, {"task": {"id": "task_2", "status": "testing"}}))
        code, _, _ = self.run_cli("task", "status", "task_2", "testing")
        self.assertEqual(code, 0)
        self.assertEqual(self.hub.requests[0]["body"]["note"], None)
        self.assertNotIn("report_id", self.hub.requests[0]["body"])

    def test_review_with_attached_report(self) -> None:
        report = {"id": "rep_1", "task_id": "task_2", "status": "completed", "title": "tests",
                  "summary": "pytest: 40 passed", "body": "full log"}
        self.reply((200, {"report": report}), (200, {"task": {"id": "task_2", "status": "review"}}))
        code, _, err = self.run_cli("task", "status", "task_2", "review", "--report", "rep_1", "--note", "ran pytest")
        self.assertEqual(code, 0, err)
        fetch, move = self.hub.requests
        self.assertEqual(fetch["path"], "/v1/reports/rep_1")
        self.assertEqual(move["body"]["note"], "ran pytest\nreport rep_1 [completed]: tests - pytest: 40 passed")
        self.assertEqual(move["body"]["report_id"], "rep_1")

    def test_review_rejects_report_for_another_task(self) -> None:
        self.reply((200, {"report": {"id": "rep_1", "task_id": "task_9", "title": "x"}}))
        code, _, err = self.run_cli("task", "status", "task_2", "review", "--report", "rep_1")
        self.assertEqual(code, 1)
        self.assertIn("belongs to task task_9", err)
        self.assertEqual(len(self.hub.requests), 1)


@unittest.skipIf(_binary_path() is None, "aircd binary not built (cargo build)")
class SubtaskCliIntegrationTest(unittest.TestCase):
    """The task flags and the review rule against a real hub."""

    @classmethod
    def setUpClass(cls) -> None:
        cls.tmp = tempfile.TemporaryDirectory()
        tmp = Path(cls.tmp.name)
        cls.cfg_dir = tmp / "cfg"
        http_port = _free_port()
        cls.base_url = f"http://127.0.0.1:{http_port}"
        cls.proc = subprocess.Popen(
            [
                str(_binary_path()),
                "--bind", f"127.0.0.1:{_free_port()}",
                "--http-bind", f"127.0.0.1:{http_port}",
                "--db", str(tmp / "groups.sqlite3"),
                "--register-key", OWNER_KEY,
            ],
            cwd=str(tmp),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        deadline = time.time() + 15
        while True:
            try:
                with urllib.request.urlopen(f"{cls.base_url}/healthz", timeout=1):
                    break
            except Exception:
                if cls.proc.poll() is not None or time.time() > deadline:
                    cls.proc.kill()
                    raise RuntimeError("aircd did not start")
                time.sleep(0.1)
        cls.cli("register", "--nick", "grplead", "--role", "coordinator", "--invite", OWNER_KEY)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.proc.terminate()
        cls.proc.wait(timeout=10)
        cls.tmp.cleanup()

    @classmethod
    def cli(cls, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
        env = {k: v for k, v in os.environ.items() if not k.startswith(("AIRCD_", "AIRC_"))}
        env["AIRCD_CONFIG_DIR"] = str(cls.cfg_dir)
        env["PYTHONPATH"] = str(CLI_PY_ROOT)
        proc = subprocess.run(
            [sys.executable, "-m", "aircd.cli", "--server", cls.base_url, *args],
            env=env, capture_output=True, text=True, encoding="utf-8",
        )
        if check and proc.returncode != 0:
            raise AssertionError(f"airc {' '.join(args)} failed:\n{proc.stdout}\n{proc.stderr}")
        return proc

    def test_subtask_flags_and_review_report(self) -> None:
        self.cli("project", "create", "#grp")
        parent = json.loads(self.cli("--json", "task", "create", "#grp", "parent", "work", "--assign", "grplead").stdout)["task"]
        child = json.loads(self.cli(
            "--json", "task", "create", "#grp", "child", "work",
            "--assign", "grplead", "--parent", parent["id"], "--reviewer", "@grplead",
        ).stdout)["task"]
        self.assertEqual(child["title"], "child work")
        if "parent_task_id" in child:  # once the hub stores subtasks
            self.assertEqual(child["parent_task_id"], parent["id"])
            self.assertEqual(child["reviewer"], "grplead")

        tid = child["id"]
        self.cli("task", "status", tid, "in_progress")
        self.cli("task", "status", tid, "testing")
        refused = self.cli("task", "status", tid, "review", check=False)
        self.assertEqual(refused.returncode, 1)
        self.assertIn("needs a test report", refused.stderr)

        rep = json.loads(self.cli(
            "--json", "report", "create", "--channel", "#grp", "--title", "unit tests",
            "--status", "completed", "--summary", "12 passed", "--task-id", tid,
            "--body", "Synthetic integration fixture: unit tests passed.",
        ).stdout)["report"]
        moved = json.loads(self.cli("--json", "task", "status", tid, "review", "--report", rep["id"]).stdout)["task"]
        self.assertEqual(moved["status"], "review")
        shown = json.loads(self.cli("--json", "task", "show", tid).stdout)
        notes = [ev.get("note") or "" for ev in shown["events"]]
        self.assertTrue(any(f"report {rep['id']}" in n and "12 passed" in n for n in notes), notes)

    def test_group_lead_question_delegation_against_real_hub(self) -> None:
        self.cli("project", "create", "#delegate")
        for nick in ("delegate-lead", "delegate-worker"):
            self.cli("--profile", nick, "register", "--nick", nick, "--invite", OWNER_KEY, "--no-default")
            self.cli("project", "add", "#delegate", nick, "--role", "builder")
        definition = {
            "name": "delegation", "lead": {"template": "example", "model": "lead", "nick": "delegate-lead"},
            "workers": [{"template": "example", "model": "worker", "nick": "delegate-worker"}],
        }
        group_file = Path(self.tmp.name) / "delegation.json"
        group_file.write_text(json.dumps(definition), encoding="utf-8")
        self.cli("group", "create", "delegation", "--file", str(group_file), "--invite", OWNER_KEY)
        self.cli("group", "attach", "delegation", "#delegate", "--invite", OWNER_KEY)
        parent = json.loads(self.cli("--json", "task", "create", "#delegate", "parent", "--assign", "delegate-lead").stdout)["task"]
        child = json.loads(self.cli("--profile", "delegate-lead", "--json", "task", "create", "#delegate", "child",
                                   "--parent", parent["id"], "--assign", "delegate-worker").stdout)["task"]
        self.assertEqual(child["reviewer"], "delegate-lead")
        self.cli("--profile", "delegate-worker", "task", "ask", child["id"], "Scope?")
        shown = self.cli("--profile", "delegate-lead", "task", "show", child["id"]).stdout
        self.assertIn("WAITING on reviewer @delegate-lead", shown)
        self.cli("--profile", "delegate-lead", "task", "escalate", child["id"], "PM decision")
        self.assertIn("WAITING on the project manager", self.cli("task", "show", child["id"]).stdout)
        self.cli("task", "answer", child["id"], "Approved")
        parent_shown = self.cli("--profile", "delegate-lead", "task", "show", parent["id"]).stdout
        self.assertIn(f"subtask {child['id']} [assigned] @delegate-worker", parent_shown)

    def test_real_hub_groups(self) -> None:
        listed = self.cli("--json", "group", "list", check=False)
        self.assertEqual(listed.returncode, 0, listed.stderr)
        created = json.loads(self.cli(
            "--json", "group", "create", "itest", "--lead", "claude:opus", "--worker", "ollama:gemma3*2",
            "--description", "Integration team", "--lead-prompt", "Split.\nReview.\n",
            "--worker-prompt", "Code only.\n", "--invite", OWNER_KEY,
        ).stdout)["group"]
        self.assertEqual(created["lead"]["model"], "opus")
        self.assertEqual(created["workers"][0]["count"], 2)
        self.assertEqual(created["lead_prompt"], "Split.\nReview.\n")
        self.assertEqual(created["worker_prompt"], "Code only.\n")
        self.cli("project", "create", "#grp2")
        attached = json.loads(self.cli("--json", "group", "attach", "itest", "#grp2", "--invite", OWNER_KEY).stdout)
        self.assertIn("#grp2", attached["group"]["projects"])
        repeated = json.loads(self.cli("--json", "group", "attach", "itest", "#grp2", "--invite", OWNER_KEY).stdout)
        self.assertEqual(repeated["group"], attached["group"])
        listed = json.loads(self.cli("--json", "group", "list").stdout)
        self.assertIn(attached["group"], listed["groups"])
        denied = self.cli("group", "update", "itest", "--add-worker", "c:m", check=False)
        self.assertEqual(denied.returncode, 1)
        self.assertIn("HTTP 403", denied.stderr)
        updated = json.loads(self.cli(
            "--json", "group", "update", "itest", "--remove-worker", "ollama:gemma3",
            "--add-worker", "claude:sonnet*2", "--invite", OWNER_KEY,
        ).stdout)["group"]
        self.assertEqual(updated["workers"], [
            {"template": "ollama", "model": "gemma3", "count": 1},
            {"template": "claude", "model": "sonnet", "count": 2},
        ])
        self.assertEqual(updated["projects"], ["#grp2"])
        self.assertEqual(updated["lead_prompt"], created["lead_prompt"])
        self.assertEqual(updated["worker_prompt"], created["worker_prompt"])
        failed = self.cli("group", "update", "itest", "--remove-worker", "claude:sonnet*3", "--invite", OWNER_KEY, check=False)
        self.assertEqual(failed.returncode, 1)
        self.assertIn("HTTP 400", failed.stderr)
        listed = json.loads(self.cli("--json", "group", "list").stdout)
        self.assertIn(updated, listed["groups"])


if __name__ == "__main__":
    unittest.main()
