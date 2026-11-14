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
    def test_inactivity_policy_round_trip_and_validation(self) -> None:
        self.assertNotIn("inactivity_timeout_secs", Group.from_dict(GROUP_REPLY).to_dict())
        self.assertEqual(Group.from_dict({**GROUP_REPLY, "inactivity_timeout_secs": 120}).to_dict()["inactivity_timeout_secs"], 120)
        for invalid in (0, -1, True, "60", 1.5):
            with self.subTest(invalid=invalid), self.assertRaises(AircdHttpError):
                Group.from_dict({**GROUP_REPLY, "inactivity_timeout_secs": invalid}).to_dict()

    def test_consensus_policy_survives_group_round_trip(self) -> None:
        policy = {"peer_reviewers": ["lead", "peer"], "quorum": 1}
        group = Group.from_dict({**GROUP_REPLY, "review_policy": policy})
        self.assertEqual(group.to_dict()["review_policy"], policy)

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
        blank = task_create_payload("#p", "t", parent="  ", reviewer=" @ ", blocked_by=["  ", " , "])
        self.assertNotIn("parent_task_id", blank)
        self.assertNotIn("reviewer", blank)
        self.assertNotIn("blocked_by", blank)
        deps = task_create_payload("#p", "t", blocked_by=[" task_1, task_2 ", "task_1"])
        self.assertEqual(deps["blocked_by"], ["task_1", "task_2"])


class GroupHttpClientTest(_HubCase):
    def test_task_timeout_sdk_payloads_and_paths(self) -> None:
        task = {"id": "task/1", "status": "in_progress"}
        self.reply(*[(200, {"task": task}) for _ in range(4)])
        hub = AircdHttpClient(self.url, token="member-token", invite="owner")
        hub.create_task("#one", "Timed", timeout_seconds=60)
        hub.set_task_timeout("task/1", 90)
        hub.set_task_timeout("task/1", None)
        hub.task_heartbeat("task/1")
        create, configure, inherit, heartbeat = self.hub.requests
        self.assertEqual(create["body"]["timeout_seconds"], 60)
        self.assertEqual(configure["path"], "/v1/tasks/task%2F1/timeout")
        self.assertEqual(configure["body"], {"timeout_seconds": 90})
        self.assertEqual(inherit["body"], {"timeout_seconds": None})
        self.assertEqual(heartbeat["path"], "/v1/tasks/task%2F1/heartbeat")
        for request in self.hub.requests:
            self.assertEqual(request["headers"]["authorization"], "Bearer member-token")
            self.assertNotIn("x-aircd-invite", request["headers"])


    def test_group_activity_uses_member_token_and_encoded_paths(self) -> None:
        heartbeat = {"ok": True, "principal_id": "worker", "last_activity_at": 100}
        events = [{"task_id": "task_1", "old_assignee": "a", "new_assignee": "b", "created_at": 100}]
        self.reply((200, heartbeat), (200, {"events": events}))
        hub = AircdHttpClient(self.url, token="member-token", invite="owner")
        self.assertEqual(hub.group_heartbeat("dev team"), heartbeat)
        self.assertEqual(hub.group_events("dev team"), events)
        for req, endpoint in zip(self.hub.requests, ("heartbeat", "events")):
            self.assertEqual(req["path"], f"/v1/groups/dev%20team/{endpoint}")
            self.assertEqual(req["headers"].get("authorization"), "Bearer member-token")
            self.assertNotIn("x-aircd-invite", req["headers"])

    def test_group_memory_uses_member_token_and_carries_revision_conflicts(self) -> None:
        memory = {"group": "dev équipe", "kind": "notes", "body": "context", "revision": 2}
        self.reply((200, {"memory": memory}), (200, {"memory": memory}), (409, {"error": "revision changed"}))
        hub = AircdHttpClient(self.url, token="member-token", invite="owner")
        self.assertEqual(hub.group_memory("dev équipe"), memory)
        self.assertEqual(hub.write_group_memory("dev équipe", "context", expected_revision=1), memory)
        with self.assertRaises(AircdHttpError) as caught:
            hub.write_group_memory("dev équipe", "stale", expected_revision=1)
        self.assertEqual(caught.exception.status, 409)
        for req in self.hub.requests:
            self.assertEqual(req["path"], "/v1/groups/dev%20%C3%A9quipe/memory/notes")
            self.assertEqual(req["headers"].get("authorization"), "Bearer member-token")
            self.assertNotIn("x-aircd-invite", req["headers"])
        self.assertEqual(self.hub.requests[1]["body"], {"text": "context", "append": False, "expected_revision": 1})

    def test_group_memory_requires_revision_for_replace_and_valid_kind(self) -> None:
        hub = AircdHttpClient(self.url, token="tok")
        for kind in ("", "../notes"):
            with self.assertRaises(AircdHttpError):
                hub.group_memory("dev", kind)
        with self.assertRaises(AircdHttpError):
            hub.write_group_memory("dev", "overwrite")
        self.assertEqual(self.hub.requests, [])

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

    def test_library_submits_commit_and_votes_on_exact_revision(self) -> None:
        self.reply((200, {"task": {"id": "task/1"}}), (200, {"task": {"status": "super_review"}}))
        hub = AircdHttpClient(self.url, token="tok")
        hub.set_task_status("task/1", "review", note="tests pass", branch="feature", commit="a" * 40)
        hub.peer_review_task("task/1", "review_123", "approve", note="Inspected aaaaa")
        submit, vote = self.hub.requests
        self.assertEqual(submit["body"]["commit"], "a" * 40)
        self.assertEqual(vote["path"], "/v1/tasks/task%2F1/peer-review")
        self.assertEqual(vote["body"], {"revision": "review_123", "decision": "approve", "note": "Inspected aaaaa"})


class GroupCliTest(_HubCase):
    def test_task_timeout_cli_commands_and_inheritance(self) -> None:
        task = {"id": "task_1", "status": "in_progress", "kind": "task", "channel": "#one", "title": "Timed"}
        self.reply(*[(200, {"task": task}) for _ in range(4)])
        for args in [("create", "#one", "Timed", "--timeout-seconds", "60"),
                     ("timeout", "task_1", "90"), ("timeout", "task_1"), ("heartbeat", "task_1")]:
            code, _, err = self.run_cli("task", *args)
            self.assertEqual(code, 0, err)
        create, configure, inherit, heartbeat = self.hub.requests
        self.assertEqual(create["body"]["timeout_seconds"], 60)
        self.assertEqual(configure["body"], {"timeout_seconds": 90})
        self.assertEqual(inherit["body"], {"timeout_seconds": None})
        self.assertEqual(heartbeat["path"], "/v1/tasks/task_1/heartbeat")


    def test_activity_cli_and_membership_denial(self) -> None:
        self.reply((200, {"ok": True, "principal_id": "worker", "last_activity_at": 100}),
                   (200, {"events": [{"task_id": "task_1", "old_assignee": "a", "new_assignee": "b", "created_at": 100}]}),
                   (403, {"error": "current named group member required"}))
        code, out, err = self.run_cli("group", "heartbeat", "dev")
        self.assertEqual(code, 0, err)
        self.assertIn("Recorded activity for @worker", out)
        code, out, err = self.run_cli("group", "events", "dev")
        self.assertEqual(code, 0, err)
        self.assertIn("task_1: @a -> @b", out)
        code, _, err = self.run_cli("group", "heartbeat", "dev")
        self.assertEqual(code, 1)
        self.assertIn("HTTP 403", err)

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

    def test_memory_cli_files_append_clear_and_revision_guard(self) -> None:
        memory = {"group": "dev", "kind": "scratchpad", "body": "hello\né", "revision": 4}
        self.reply((200, {"memory": memory}))
        code, out, err = self.run_cli("group", "memory", "show", "dev", "--kind", "scratchpad")
        self.assertEqual((code, out.strip(), err), (0, "hello\né", ""))
        path = Path(self.cfg.name) / "context.txt"
        path.write_text("hello\né", encoding="utf-8")
        self.reply((200, {"memory": memory}), (200, {"memory": {**memory, "revision": 5}}))
        code, out, err = self.run_cli("--json", "group", "memory", "set", "dev", "--kind", "scratchpad", "--file", str(path))
        self.assertEqual(code, 0, err)
        self.assertEqual(json.loads(out)["memory"]["revision"], 5)
        self.assertEqual(self.hub.requests[-1]["body"], {"text": "hello\né", "append": False, "expected_revision": 4})
        self.reply((200, {"memory": memory}), (200, {"memory": memory}))
        self.assertEqual(self.run_cli("group", "memory", "add", "dev", "line")[0], 0)
        self.assertEqual(self.hub.requests[-1]["body"], {"text": "line", "append": True})
        self.assertEqual(self.run_cli("group", "memory", "clear", "dev", "--revision", "4")[0], 0)
        self.assertEqual(self.hub.requests[-1]["body"], {"text": "", "append": False, "expected_revision": 4})
        self.reply((409, {"error": "revision changed"}))
        code, _, err = self.run_cli("group", "memory", "set", "dev", "stale", "--revision", "3")
        self.assertEqual(code, 1)
        self.assertIn("revision changed", err)

    def test_memory_cli_requires_text_and_member_credentials(self) -> None:
        self.assertEqual(self.run_cli("group", "memory", "set", "dev")[0], 1)
        self.assertEqual(self.run_cli("group", "memory", "show", "dev", token=None)[0], 1)
        self.assertEqual(self.hub.requests, [])

    def test_memory_cli_stdin_and_invalid_files(self) -> None:
        memory = {"group": "dev", "kind": "notes", "body": "from stdin\né", "revision": 1}
        self.reply((200, {"memory": memory}))
        with mock.patch("sys.stdin", io.StringIO(memory["body"])):
            code, _, err = self.run_cli("group", "memory", "set", "dev", "--file", "-", "--revision", "0")
        self.assertEqual(code, 0, err)
        self.assertEqual(self.hub.requests[-1]["body"]["text"], memory["body"])
        invalid = Path(self.cfg.name) / "invalid.txt"
        invalid.write_bytes(b"\xff")
        for args in [("--file", str(invalid)), ("--file", str(invalid) + "missing"),
                     ("text", "--file", str(invalid))]:
            self.assertEqual(self.run_cli("group", "memory", "set", "dev", *args)[0], 1)
        self.assertEqual(len(self.hub.requests), 1)

    def test_consensus_policy_file_is_preserved_and_listed(self) -> None:
        policy = {"peer_reviewers": ["lead", "peer"], "quorum": 1}
        group = {**GROUP_REPLY, "review_policy": policy}
        self.reply((200, {"group": group}))
        path = Path(self.cfg.name) / "group.json"
        path.write_text(json.dumps(group), encoding="utf-8")
        code, out, err = self.run_cli("group", "create", "dev", "--file", str(path), "--invite", "owner")
        self.assertEqual(code, 0, err)
        self.assertEqual(self.hub.requests[0]["body"]["review_policy"], policy)
        self.assertIn("peer review: lead, peer; quorum=1", out)

    def test_consensus_submission_vote_and_task_display(self) -> None:
        task = {"id": "task_1", "status": "review", "kind": "feature", "channel": "#cams", "title": "Work",
                "review_round": {"revision": "review_123", "commit": "a" * 40, "quorum": 1, "peer_reviewers": ["peer"],
                                 "votes": [{"reviewer": "peer", "decision": "approve", "note": "Reviewed source"}]}}
        self.reply((200, {"task": task}), (200, {"task": {**task, "status": "super_review"}}), (200, {"task": task}))
        code, _, err = self.run_cli("task", "status", "task_1", "review", "--note", "Tests passed", "--commit", "a" * 40)
        self.assertEqual(code, 0, err)
        self.assertEqual(self.hub.requests[0]["body"]["commit"], "a" * 40)
        code, out, err = self.run_cli("task", "peer-review", "task_1", "approve", "--revision", "review_123", "--note", "Reviewed source")
        self.assertEqual(code, 0, err)
        self.assertIn("super_review", out)
        self.assertEqual(self.hub.requests[1]["body"], {"revision": "review_123", "decision": "approve", "note": "Reviewed source"})
        code, out, err = self.run_cli("task", "show", "task_1")
        self.assertEqual(code, 0, err)
        self.assertIn("peer revision=review_123", out)
        self.assertIn("@peer: approve - Reviewed source", out)

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

    def test_task_timeout_cli_recovery_against_real_hub(self) -> None:
        self.cli("project", "create", "#timeout")
        task = json.loads(self.cli("--json", "task", "create", "#timeout", "Timed execution",
                                  "--assign", "grplead", "--timeout-seconds", "3").stdout)["task"]
        tid = task["id"]
        self.cli("task", "status", tid, "in_progress")
        beat = json.loads(self.cli("--json", "task", "heartbeat", tid).stdout)["task"]
        self.assertIsInstance(beat["execution_heartbeat_at"], int)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            state = json.loads(self.cli("--json", "task", "show", tid).stdout)
            if state["task"]["status"] == "assigned":
                break
            time.sleep(0.2)
        self.assertEqual(state["task"]["status"], "assigned")
        self.assertEqual(sum(e["action"] == "timeout" for e in state["events"]), 1)
        self.assertEqual(self.cli("task", "heartbeat", tid, check=False).returncode, 1)
        self.cli("task", "timeout", tid)
        self.cli("task", "status", tid, "testing")
        self.cli("task", "status", tid, "review", "--note", "timeout CLI smoke passed")

    def test_group_activity_cli_against_real_hub(self) -> None:
        for nick in ("activity-lead", "activity-worker", "activity-outsider"):
            self.cli("--profile", nick, "register", "--nick", nick, "--invite", OWNER_KEY, "--no-default")
        group = {"name": "activity team", "inactivity_timeout_secs": 60,
                 "lead": {"template": "example", "model": "lead", "nick": "activity-lead"},
                 "workers": [{"template": "example", "model": "worker", "nick": "activity-worker"}]}
        path = Path(self.tmp.name) / "activity-group.json"
        path.write_text(json.dumps(group), encoding="utf-8")
        saved = json.loads(self.cli("--json", "group", "create", group["name"], "--file", str(path), "--invite", OWNER_KEY).stdout)["group"]
        self.assertEqual(saved["inactivity_timeout_secs"], 60)
        beat = json.loads(self.cli("--profile", "activity-worker", "--json", "group", "heartbeat", group["name"]).stdout)
        self.assertEqual(beat["principal_id"], "activity-worker")
        self.assertLessEqual(abs(time.time() - beat["last_activity_at"]), 5)
        events = json.loads(self.cli("--profile", "activity-lead", "--json", "group", "events", group["name"]).stdout)
        self.assertEqual(events["events"], [])
        self.assertEqual(self.cli("--profile", "activity-outsider", "group", "heartbeat", group["name"], check=False).returncode, 1)
        group["inactivity_timeout_secs"] = None
        path.write_text(json.dumps(group), encoding="utf-8")
        disabled = json.loads(self.cli("--json", "group", "create", group["name"], "--file", str(path), "--invite", OWNER_KEY).stdout)["group"]
        self.assertNotIn("inactivity_timeout_secs", disabled)

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

    def test_parent_pause_and_cancellation_cascade_against_real_hub(self) -> None:
        self.cli("project", "create", "#cascade")

        def create(title: str, parent: str | None = None) -> str:
            args = ["--json", "task", "create", "#cascade", title, "--assign", "grplead"]
            if parent:
                args.extend(["--parent", parent])
            return json.loads(self.cli(*args).stdout)["task"]["id"]

        def show(tid: str) -> dict:
            return json.loads(self.cli("--json", "task", "show", tid).stdout)

        parent = create("Parent")
        child = create("Independent blocker", parent)
        nested = create("Nested work", child)
        sibling = create("Testing work", parent)
        self.cli("task", "status", sibling, "testing")
        self.cli("task", "ask", child, "Child decision?")
        self.cli("task", "ask", parent, "Parent decision?")
        for tid in (parent, child, nested, sibling):
            self.assertEqual(show(tid)["task"]["status"], "blocked")
        denied = self.cli("task", "assign", sibling, "grplead", check=False)
        self.assertEqual(denied.returncode, 1)
        self.cli("task", "answer", parent, "Continue parent")
        self.assertEqual(show(sibling)["task"]["status"], "testing")
        self.assertEqual(show(child)["task"]["status"], "blocked")
        self.assertEqual(show(nested)["task"]["status"], "blocked")
        self.cli("task", "answer", child, "Continue child")
        self.assertEqual(show(nested)["task"]["status"], "assigned")
        self.cli("task", "ask", child, "Obsolete question?")
        self.cli("task", "status", parent, "cancelled", "--note", "Cancel the tree")
        for tid in (child, nested, sibling):
            snapshot = show(tid)
            self.assertEqual(snapshot["task"]["status"], "cancelled")
            self.assertIsNone(snapshot["question"])
            self.assertEqual(snapshot["events"][-1]["action"], "parent_cancel")
        self.assertEqual(self.cli("task", "answer", child, "Too late", check=False).returncode, 1)

    def test_consensus_cli_against_real_hub(self) -> None:
        self.cli("project", "create", "#consensus")
        for nick in ("consensus-lead", "consensus-worker", "consensus-peer"):
            self.cli("--profile", nick, "register", "--nick", nick, "--invite", OWNER_KEY, "--no-default")
            self.cli("project", "add", "#consensus", nick, "--role", "builder")
        definition = {
            "name": "consensus", "lead": {"template": "example", "model": "lead", "nick": "consensus-lead"},
            "workers": [{"template": "example", "model": "worker", "nick": nick}
                        for nick in ("consensus-worker", "consensus-peer")],
            "review_policy": {"peer_reviewers": ["consensus-lead", "consensus-peer", "consensus-worker"]},
        }
        group_file = Path(self.tmp.name) / "consensus.json"
        group_file.write_text(json.dumps(definition), encoding="utf-8")
        self.cli("group", "create", "consensus", "--file", str(group_file), "--invite", OWNER_KEY)
        self.cli("group", "attach", "consensus", "#consensus", "--invite", OWNER_KEY)
        task = json.loads(self.cli("--json", "task", "create", "#consensus", "Reviewed work",
                                   "--assign", "consensus-worker", "--reviewer", "consensus-lead").stdout)["task"]
        tid = task["id"]
        self.cli("--profile", "consensus-worker", "task", "status", tid, "testing")
        task = json.loads(self.cli("--profile", "consensus-worker", "--json", "task", "status", tid, "review",
                                   "--branch", "feat/review", "--commit", "a" * 40, "--note", "Integration tests passed").stdout)["task"]
        revision = task["review_round"]["revision"]
        self.assertEqual(task["review_round"]["quorum"], 2)
        self.assertEqual(task["review_round"]["commit"], "a" * 40)
        self.assertIn(tid, self.cli("--profile", "consensus-peer", "inbox").stdout)
        denied = self.cli("--profile", "consensus-peer", "task", "peer-review", tid, "approve",
                          "--revision", "old", "--note", "stale", check=False)
        self.assertEqual(denied.returncode, 1)
        self.assertIn("stale review revision", denied.stderr)
        for nick, decision in [("consensus-lead", "approve"), ("consensus-peer", "reject"), ("consensus-peer", "approve")]:
            reviewed = json.loads(self.cli("--profile", nick, "--json", "task", "peer-review", tid, decision,
                                           "--revision", revision, "--note", "Inspected exact submitted source").stdout)["task"]
            expected = "super_review" if nick == "consensus-peer" and decision == "approve" else "review"
            self.assertEqual(reviewed["status"], expected)
        denied = self.cli("--profile", "consensus-lead", "task", "status", tid, "merged", check=False)
        self.assertEqual(denied.returncode, 1)
        merged = json.loads(self.cli("--json", "task", "status", tid, "merged", "--note", "PM accepted peer consensus").stdout)["task"]
        self.assertEqual(merged["status"], "merged")

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

    def test_group_memory_cli_against_real_hub(self) -> None:
        for nick in ("memory-lead", "memory-worker", "memory-outsider"):
            self.cli("--profile", nick, "register", "--nick", nick, "--invite", OWNER_KEY, "--no-default")
        group = {"name": "memory équipe", "lead": {"template": "example", "model": "lead", "nick": "@MEMORY-LEAD"},
                 "workers": [{"template": "example", "model": "worker", "nick": "memory-worker"}]}
        path = Path(self.tmp.name) / "memory-group.json"
        path.write_text(json.dumps(group), encoding="utf-8")
        self.cli("group", "create", group["name"], "--file", str(path), "--invite", OWNER_KEY)
        def memory(profile, action, *args, check=True):
            return self.cli("--profile", profile, "--json", "group", "memory", action, group["name"], *args, check=check)
        empty = json.loads(memory("memory-lead", "show").stdout)["memory"]
        self.assertEqual(empty["revision"], 0)
        memory("memory-lead", "set", "persistent context", "--revision", "0")
        memory("memory-worker", "add", "worker line")
        shared = json.loads(memory("memory-worker", "show").stdout)["memory"]
        self.assertEqual((shared["body"], shared["revision"], shared["updated_by"]),
                         ("persistent context\nworker line", 2, "memory-worker"))
        self.assertEqual(memory("memory-lead", "set", "stale", "--revision", "1", check=False).returncode, 1)
        self.assertEqual(memory("memory-outsider", "show", check=False).returncode, 1)
        memory("memory-worker", "add", "scratch", "--kind", "scratchpad")
        self.assertEqual(json.loads(memory("memory-lead", "show").stdout)["memory"], shared)
        memory("memory-lead", "clear", "--revision", "2")
        self.assertEqual(json.loads(memory("memory-worker", "show").stdout)["memory"]["revision"], 3)

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
