"""Bionic token budgets and per-task cost accounting.

Unit tests drive ``run_agent`` against the scripted OpenAI-compatible server
from ``test_bionic``; the end-to-end test bills a real compiled hub.
"""

from __future__ import annotations

import argparse
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
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from typing import Any, Optional
from unittest import mock

from aircd import bionic
from tests.test_bionic import _FakeBionicServer
from tests.test_cli_e2e import CLI_PY_ROOT, REPO_ROOT, _binary_path, _free_port


def _tool_reply(usage: Optional[dict[str, int]] = None, call_id: str = "tc") -> tuple[int, dict[str, Any]]:
    reply: dict[str, Any] = {
        "choices": [
            {
                "message": {
                    "role": "assistant",
                    "content": None,
                    "tool_calls": [
                        {
                            "id": call_id,
                            "type": "function",
                            "function": {"name": "read_file", "arguments": json.dumps({"path": "notes.txt"})},
                        }
                    ],
                }
            }
        ]
    }
    if usage is not None:
        reply["usage"] = usage
    return 200, reply


def _final_reply(text: str = "done", usage: Optional[dict[str, int]] = None) -> tuple[int, dict[str, Any]]:
    reply: dict[str, Any] = {"choices": [{"message": {"role": "assistant", "content": text}}]}
    if usage is not None:
        reply["usage"] = usage
    return 200, reply


def _usage(prompt: int, completion: int) -> dict[str, int]:
    return {"prompt_tokens": prompt, "completion_tokens": completion, "total_tokens": prompt + completion}


class _Reporter:
    """In-memory stand-in for the hub: one task with an optional budget."""

    def __init__(self, budget: Optional[int] = None, spent: int = 0, fail: bool = False) -> None:
        self.budget = budget
        self.spent = spent
        self.fail = fail
        self.calls: list[Optional[dict[str, Any]]] = []

    def __call__(self, usage: Optional[dict[str, Any]]) -> dict[str, Any]:
        self.calls.append(usage)
        if self.fail:
            raise RuntimeError("hub down")
        if usage is not None:
            self.spent += int(usage["total_tokens"])
        remaining = None if self.budget is None else self.budget - self.spent
        return {
            "task_id": "task_x",
            "total": {"total_tokens": self.spent},
            "token_budget": self.budget,
            "remaining": remaining,
            "limited_by": "task_parent" if remaining is not None else None,
            "exceeded": remaining is not None and remaining <= 0,
        }


class TokenLedgerTests(unittest.TestCase):
    def test_local_budget_counts_caps_and_exhausts(self) -> None:
        ledger = bionic.TokenLedger(100)
        self.assertTrue(ledger.active)
        self.assertEqual(ledger.remaining(), 100)
        self.assertEqual(ledger.cap_max_tokens(None), 100)
        self.assertEqual(ledger.cap_max_tokens(40), 40)
        ledger.add(_usage(60, 10))
        self.assertEqual(ledger.spent, 70)
        self.assertEqual(ledger.cap_max_tokens(None), 30)
        self.assertFalse(ledger.exhausted())
        ledger.add(_usage(30, 5), estimated=True)
        self.assertTrue(ledger.exhausted())
        snap = ledger.snapshot()
        self.assertEqual(snap["remaining"], -5)
        self.assertEqual(snap["limited_by"], "run")
        self.assertEqual(snap["estimated_turns"], 1)
        self.assertNotIn("task_id", snap)
        self.assertIn("spent 105 of 100", ledger.exhausted_message())

    def test_no_budget_is_inactive_and_never_caps(self) -> None:
        ledger = bionic.TokenLedger()
        self.assertFalse(ledger.active)
        ledger.add(_usage(5, 5))
        self.assertIsNone(ledger.remaining())
        self.assertEqual(ledger.cap_max_tokens(7), 7)
        self.assertFalse(ledger.exhausted())

    def test_hub_remaining_wins_when_tighter(self) -> None:
        reporter = _Reporter(budget=1_000, spent=950)
        ledger = bionic.TokenLedger(500, reporter=reporter, task_id="task_x")
        ledger.start()
        self.assertEqual(reporter.calls, [None])
        self.assertEqual(ledger.remaining(), 50)
        self.assertEqual(ledger.limited_by(), "task task_parent")
        ledger.add(_usage(40, 20))
        self.assertTrue(ledger.exhausted())
        self.assertEqual(reporter.calls[1]["total_tokens"], 60)
        snap = ledger.snapshot()
        self.assertEqual(snap["task_id"], "task_x")
        self.assertEqual(snap["reported_turns"], 1)
        self.assertEqual(snap["task_total_tokens"], 1_010)
        self.assertIn("task task_parent", ledger.exhausted_message())

    def test_unreachable_hub_never_fails_and_local_budget_still_applies(self) -> None:
        logs: list[str] = []
        ledger = bionic.TokenLedger(50, reporter=_Reporter(fail=True), task_id="task_x", log=logs.append)
        ledger.start()
        ledger.add(_usage(30, 30))
        self.assertTrue(ledger.exhausted())
        self.assertEqual(len(ledger.snapshot()["report_errors"]), 2)
        self.assertTrue(any("hub down" in line for line in logs))

    def test_estimate_tokens(self) -> None:
        self.assertEqual(bionic.estimate_tokens(None), 0)
        self.assertEqual(bionic.estimate_tokens("abcd"), 1)
        self.assertEqual(bionic.estimate_tokens("abcde"), 2)
        self.assertGreater(bionic.estimate_tokens([{"role": "user", "content": "x" * 400}]), 100)


class BudgetConfigTests(unittest.TestCase):
    def _resolve(self, env: dict[str, str], **kwargs: Any) -> bionic.BionicConfig:
        clean = {k: v for k, v in os.environ.items() if not k.startswith(("BIONIC_", "AIRCD_WAKE"))}
        clean.update(env)
        with mock.patch.dict(os.environ, clean, clear=True):
            return bionic.resolve_config(argparse.Namespace(model="m", **kwargs))

    def test_env_and_flags(self) -> None:
        cfg = self._resolve({})
        self.assertIsNone(cfg.token_budget)
        self.assertIsNone(cfg.task_id)
        self.assertTrue(cfg.report_usage)
        cfg = self._resolve({"BIONIC_TOKEN_BUDGET": "2500", "AIRCD_WAKE_TASK": "task_wake", "BIONIC_NO_USAGE_REPORT": "1"})
        self.assertEqual(cfg.token_budget, 2500)
        self.assertEqual(cfg.task_id, "task_wake")
        self.assertFalse(cfg.report_usage)
        cfg = self._resolve({"BIONIC_TASK_ID": "task_env", "AIRCD_WAKE_TASK": "task_wake"}, token_budget=9, task_id=" task_cli ")
        self.assertEqual(cfg.token_budget, 9)
        self.assertEqual(cfg.task_id, "task_cli")
        self.assertEqual(self._resolve({"BIONIC_TASK_ID": "task_env", "AIRCD_WAKE_TASK": "w"}).task_id, "task_env")
        self.assertIsNone(self._resolve({"AIRCD_WAKE_TASK": "  "}).task_id)
        with self.assertRaises(bionic.BionicError):
            self._resolve({"BIONIC_TOKEN_BUDGET": "0"})
        with self.assertRaises(bionic.BionicError):
            self._resolve({}, token_budget=0)

    def test_parser_flags(self) -> None:
        args = bionic.build_parser().parse_args(
            ["-p", "x", "--token-budget", "77", "--task", "task_9", "--no-report-usage"]
        )
        self.assertEqual((args.token_budget, args.task_id, args.no_report_usage), (77, "task_9", True))

    def test_missing_hub_credentials_warn_but_do_not_fail(self) -> None:
        cfg = bionic.BionicConfig(task_id="task_x")
        with tempfile.TemporaryDirectory() as cfg_dir, mock.patch.dict(
            os.environ, {"AIRCD_CONFIG_DIR": cfg_dir, "AIRCD_TOKEN": "", "AIRCD_PROFILE": "", "AIRC_NICK": ""}
        ):
            err = io.StringIO()
            with redirect_stderr(err):
                ledger = bionic.ledger_for(cfg)
        self.assertIsNone(ledger.reporter)
        self.assertIn("no aircd token", err.getvalue())
        self.assertIn("no aircd token", ledger.snapshot()["report_errors"][0])


class RunAgentBudgetTests(unittest.TestCase):
    def setUp(self) -> None:
        self.server = _FakeBionicServer()
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        self.base_url = f"http://127.0.0.1:{self.server.server_address[1]}/v1"
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        Path(self.tmp.name, "notes.txt").write_text("hello\n", encoding="utf-8")

    def config(self, **kwargs: Any) -> bionic.BionicConfig:
        return bionic.BionicConfig(
            base_url=self.base_url, model="m", cwd=self.tmp.name, workspace_root=self.tmp.name,
            plugins=False, **kwargs,
        )

    def chat_requests(self) -> list[dict[str, Any]]:
        return [r for r in self.server.requests if r["path"] == "/v1/chat/completions"]

    def test_stops_before_a_turn_once_the_run_budget_is_spent(self) -> None:
        self.server.chat_replies = [
            _tool_reply(_usage(80, 20), "a"),
            _tool_reply(_usage(90, 30), "b"),
            _final_reply("never reached"),
        ]
        events: list[dict[str, Any]] = []
        with self.assertRaises(bionic.TokenBudgetError) as ctx:
            bionic.run_agent("go", self.config(token_budget=150, max_tokens=500), on_event=events.append)
        err = ctx.exception
        self.assertIsInstance(err, bionic.MaxTurnsError)
        self.assertEqual(err.turns, 2)
        self.assertEqual(err.tool_calls, 2)
        self.assertEqual(err.usage["total_tokens"], 220)
        self.assertEqual(err.budget["remaining"], -70)
        requests = self.chat_requests()
        self.assertEqual(len(requests), 2)
        self.assertEqual(requests[0]["body"]["max_tokens"], 150)
        self.assertEqual(requests[1]["body"]["max_tokens"], 50, "completion is capped to what is left")
        budget_events = [e for e in events if e["type"] == "budget"]
        self.assertEqual([e["spent"] for e in budget_events], [100, 220])

    def test_final_answer_within_budget_reports_the_snapshot(self) -> None:
        self.server.chat_replies = [_tool_reply(_usage(10, 5)), _final_reply("ok", _usage(20, 5))]
        result = bionic.run_agent("go", self.config(token_budget=1_000))
        self.assertEqual(result.content, "ok")
        self.assertEqual(result.budget["spent"], 40)
        self.assertEqual(result.budget["remaining"], 960)
        self.assertFalse(result.budget["exhausted"])
        # Without a budget or task the request is unchanged and no snapshot is kept.
        self.server.chat_replies = [_final_reply("plain", _usage(1, 1))]
        result = bionic.run_agent("go", self.config())
        self.assertIsNone(result.budget)
        self.assertNotIn("max_tokens", self.chat_requests()[-1]["body"])

    def test_estimates_tokens_when_the_server_sends_no_usage(self) -> None:
        self.server.chat_replies = [_tool_reply(None), _tool_reply(None, "b"), _final_reply("x")]
        with self.assertRaises(bionic.TokenBudgetError) as ctx:
            bionic.run_agent("go " + "x" * 2_000, self.config(token_budget=600))
        self.assertEqual(ctx.exception.usage, {}, "the server-reported usage stays empty")
        self.assertGreaterEqual(ctx.exception.budget["estimated_turns"], 1)
        self.assertGreater(ctx.exception.budget["spent"], 600)

    def test_hub_task_is_billed_each_turn_and_its_budget_is_obeyed(self) -> None:
        reporter = _Reporter(budget=300, spent=100)
        self.server.chat_replies = [_tool_reply(_usage(150, 10)), _tool_reply(_usage(60, 10), "b"), _final_reply()]
        with self.assertRaises(bionic.TokenBudgetError) as ctx:
            bionic.run_agent("go", self.config(task_id="task_x"), usage_reporter=reporter)
        self.assertEqual(reporter.calls[0], None)
        self.assertEqual([c["total_tokens"] for c in reporter.calls[1:]], [160, 70])
        self.assertEqual(ctx.exception.budget["limited_by"], "task task_parent")
        self.assertEqual(len(self.chat_requests()), 2)

    def test_exhausted_hub_budget_refuses_to_start(self) -> None:
        reporter = _Reporter(budget=100, spent=100)
        with self.assertRaises(bionic.TokenBudgetError) as ctx:
            bionic.run_agent("go", self.config(task_id="task_x"), usage_reporter=reporter)
        self.assertEqual(ctx.exception.turns, 0)
        self.assertEqual(self.chat_requests(), [])

    def test_unreachable_hub_does_not_stop_the_run(self) -> None:
        self.server.chat_replies = [_final_reply("fine", _usage(5, 5))]
        result = bionic.run_agent("go", self.config(task_id="task_x"), usage_reporter=_Reporter(fail=True))
        self.assertEqual(result.content, "fine")
        self.assertEqual(len(result.budget["report_errors"]), 2)

    def test_cli_exit_code_and_json_on_budget_stop(self) -> None:
        self.server.chat_replies = [_tool_reply(_usage(80, 20))]
        out, err = io.StringIO(), io.StringIO()
        env = {k: v for k, v in os.environ.items() if not k.startswith(("BIONIC_", "AIRCD_WAKE"))}
        with mock.patch.dict(os.environ, env, clear=True), redirect_stdout(out), redirect_stderr(err):
            rc = bionic.main([
                "--base-url", self.base_url, "--model", "m", "--cwd", self.tmp.name, "--no-plugins",
                "--token-budget", "50", "--json", "-p", "go",
            ])
        self.assertEqual(rc, 3)
        payload = json.loads(out.getvalue())
        self.assertFalse(payload["ok"])
        self.assertEqual(payload["reason"], "token_budget")
        self.assertEqual(payload["budget"]["spent"], 100)
        self.assertIn("token budget exhausted", err.getvalue())


class TokenBudgetHubE2ETest(unittest.TestCase):
    """`airc task budget/usage` and `airc-bionic --task` against the compiled hub."""

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
                "--db", str(cls.tmp / "budget.sqlite3"),
                "--open-registration",
            ],
            cwd=str(REPO_ROOT),
            env={**os.environ, "AIRCD_SEED_DEMO": "1"},
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
        cls.model = _FakeBionicServer()
        threading.Thread(target=cls.model.serve_forever, daemon=True).start()
        cls.model_url = f"http://127.0.0.1:{cls.model.server_address[1]}/v1"

    @classmethod
    def tearDownClass(cls) -> None:
        cls.model.shutdown()
        cls.model.server_close()
        cls.server_proc.terminate()
        try:
            cls.server_proc.wait(timeout=5)
        except Exception:
            cls.server_proc.kill()
        cls.tmp_dir.cleanup()

    def env(self, **extra: str) -> dict[str, str]:
        env = {k: v for k, v in os.environ.items() if not k.startswith(("BIONIC_", "AIRCD_", "AIRC_"))}
        env.update(
            AIRCD_CONFIG_DIR=str(self.cfg_dir), PYTHONPATH=str(CLI_PY_ROOT), PYTHONUTF8="1", **extra,
        )
        return env

    def cli(self, *args: str, check: bool = True) -> subprocess.CompletedProcess[bytes]:
        proc = subprocess.run(
            [sys.executable, "-m", "aircd.cli", "--server", self.base_url, *args],
            env=self.env(), cwd=str(self.tmp), capture_output=True,
        )
        if check and proc.returncode != 0:
            self.fail(f"CLI {args} exited {proc.returncode}\n{proc.stdout!r}\n{proc.stderr!r}")
        return proc

    def jcli(self, *args: str) -> dict:
        return json.loads(self.cli(*args).stdout.decode("utf-8"))

    def test_budget_usage_and_bionic_billing(self) -> None:
        self.jcli("--profile", "pm", "--json", "register", "--nick", "budpm", "--role", "leader")
        self.jcli("--profile", "dev", "--json", "register", "--nick", "buddev", "--role", "builder")
        self.jcli("--profile", "out", "--json", "register", "--nick", "budout", "--role", "builder")
        self.jcli("--profile", "pm", "--json", "project", "create", "#budproj")
        self.jcli("--profile", "pm", "--json", "project", "add", "#budproj", "buddev")
        created = self.jcli(
            "--profile", "pm", "--json", "task", "create", "#budproj", "Parent", "--assign", "budpm",
            "--token-budget", "1,000",
        )
        parent = created["task"]["id"]
        self.assertEqual(created["usage"]["token_budget"], 1000)
        child = self.jcli(
            "--profile", "pm", "--json", "task", "create", "#budproj", "Child", "--assign", "buddev",
            "--parent", parent,
        )["task"]["id"]

        # Manual records: the developer may bill, an outsider may not.
        rec = self.jcli("--profile", "dev", "--json", "task", "usage", child, "--prompt", "300",
                        "--completion", "100", "--session", "manual", "--model", "hand")
        self.assertEqual(rec["usage"]["remaining"], 600)
        denied = self.cli("--profile", "out", "task", "usage", child, "--prompt", "1", check=False)
        self.assertNotEqual(denied.returncode, 0)
        self.assertIn(b"403", denied.stderr)
        denied = self.cli("--profile", "dev", "task", "budget", child, "5", check=False)
        self.assertNotEqual(denied.returncode, 0)

        # Bionic bills the child; the parent's budget stops it after two turns.
        self.model.chat_replies = [
            _tool_reply(_usage(250, 50), "a"),
            _tool_reply(_usage(300, 50), "b"),
            _final_reply("never"),
        ]
        work = self.tmp / "work"
        work.mkdir()
        (work / "notes.txt").write_text("hi\n", encoding="utf-8")
        run = subprocess.run(
            [sys.executable, "-m", "aircd.bionic", "--base-url", self.model_url, "--model", "gemma",
             "--cwd", str(work), "--no-plugins", "--json", "-p", "work"],
            env=self.env(AIRCD_PROFILE="dev", AIRC_SERVER=self.base_url, AIRCD_WAKE_TASK=child),
            capture_output=True, cwd=str(work),
        )
        self.assertEqual(run.returncode, 3, run.stderr.decode())
        out = json.loads(run.stdout.decode())
        self.assertEqual(out["reason"], "token_budget")
        self.assertEqual(out["budget"]["task_id"], child)
        self.assertEqual(out["budget"]["reported_turns"], 2)
        self.assertEqual(out["budget"]["limited_by"], f"task {parent}")
        self.assertEqual(out["budget"]["remaining"], -50)

        usage = self.jcli("--profile", "pm", "--json", "task", "usage", child)["usage"]
        self.assertEqual(usage["own"]["total_tokens"], 1050)
        self.assertTrue(usage["exceeded"])
        sessions = {s["session_id"]: s for s in usage["by_session"]}
        self.assertEqual(sessions["manual"]["total_tokens"], 400)
        bionic_sessions = [s for s in usage["by_session"] if s["session_id"] != "manual"]
        self.assertEqual(len(bionic_sessions), 1)
        self.assertEqual(bionic_sessions[0]["model"], "gemma")
        self.assertEqual(bionic_sessions[0]["records"], 2)
        self.assertEqual(bionic_sessions[0]["recorded_by"], "buddev")

        human = self.cli("--profile", "pm", "task", "usage", parent).stdout.decode()
        self.assertIn("subtasks: 1,050 total", human)
        self.assertIn("EXCEEDED", human)
        self.assertIn(f"{child}: 1,050 tokens", human)
        shown = self.cli("--profile", "pm", "task", "show", child).stdout.decode()
        self.assertIn("tokens: 1,050 total; -50 EXCEEDED", shown)
        self.assertIn(f"limited by parent {parent}", shown)
        shown = self.cli("--profile", "pm", "task", "show", parent).stdout.decode()
        self.assertIn("token_budget_exceeded", shown)

        # A new run refuses to start while the budget is spent...
        again = subprocess.run(
            [sys.executable, "-m", "aircd.bionic", "--base-url", self.model_url, "--model", "gemma",
             "--cwd", str(work), "--no-plugins", "--task", child, "-p", "work"],
            env=self.env(AIRCD_PROFILE="dev", AIRC_SERVER=self.base_url), capture_output=True, cwd=str(work),
        )
        self.assertEqual(again.returncode, 3)
        self.assertEqual(self.model.chat_replies, [_final_reply("never")])
        # ...until the PM clears it.
        cleared = self.jcli("--profile", "pm", "--json", "task", "budget", parent, "none")
        self.assertIsNone(cleared["usage"]["token_budget"])
        self.assertFalse(self.jcli("--profile", "pm", "--json", "task", "budget", child)["usage"]["exceeded"])

        # Token metrics ride along in the task export.
        req = urllib.request.Request(
            f"{self.base_url}/v1/tasks/export?channel=%23budproj&events=false",
            headers={"Authorization": "Bearer " + json.loads((self.cfg_dir / "profiles.json").read_text())
                     ["profiles"]["pm"]["token"]},
        )
        with urllib.request.urlopen(req, timeout=5) as resp:
            export = json.loads(resp.read())
        self.assertEqual(export["token_totals"]["total_tokens"], 1050)
        rows = {row["task"]["id"]: row for row in export["tasks"]}
        self.assertEqual(rows[parent]["metrics"]["tokens"]["total"]["total_tokens"], 1050)
        self.assertEqual(rows[child]["metrics"]["tokens"]["own"]["total_tokens"], 1050)


if __name__ == "__main__":
    unittest.main()
