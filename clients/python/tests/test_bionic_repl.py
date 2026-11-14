"""Tests for the interactive Bionic REPL and step-through debug mode (`aircd.bionic_repl`)."""

from __future__ import annotations

import io
import json
import os
import subprocess
import sys
import tempfile
import threading
import unittest
from pathlib import Path
from typing import Any, Iterable

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from aircd import bionic, bionic_repl  # noqa: E402
from tests.test_bionic import _FakeBionicServer  # noqa: E402

CLI_PY_ROOT = Path(__file__).resolve().parents[1]


def _reply(content: str | None = None, *calls: tuple[str, str, dict[str, Any]], usage: int = 0) -> tuple[int, Any]:
    msg: dict[str, Any] = {"role": "assistant", "content": content}
    if calls:
        msg["tool_calls"] = [
            {"id": cid, "type": "function", "function": {"name": name, "arguments": json.dumps(args)}}
            for cid, name, args in calls
        ]
    body: dict[str, Any] = {"choices": [{"message": msg}]}
    if usage:
        body["usage"] = {"prompt_tokens": usage, "completion_tokens": 1, "total_tokens": usage + 1}
    return (200, body)


class _Script:
    """``input()`` replacement that replays lines and records the prompts shown."""

    def __init__(self, lines: Iterable[str]) -> None:
        self.lines = list(lines)
        self.prompts: list[str] = []

    def __call__(self, prompt: str) -> str:
        self.prompts.append(prompt)
        if not self.lines:
            raise EOFError
        return self.lines.pop(0)


class BionicReplTests(unittest.TestCase):
    def setUp(self) -> None:
        self.server = _FakeBionicServer()
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        self.base_url = f"http://127.0.0.1:{self.server.server_address[1]}/v1"
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.cwd = self.tmp.name

    def config(self, **kw: Any) -> bionic.BionicConfig:
        base = dict(base_url=self.base_url, model="m1", cwd=self.cwd, workspace_root=self.cwd, plugins=False)
        base.update(kw)
        return bionic.BionicConfig(**base)

    def run_repl(self, lines: list[str], *, step: bool = False, **kw: Any) -> tuple[bionic_repl.BionicRepl, str]:
        out = io.StringIO()
        repl = bionic_repl.BionicRepl(self.config(**kw), step=step, input_fn=_Script(lines), out=out)
        self.assertEqual(repl.loop(), 0)
        return repl, out.getvalue()

    def chat_bodies(self) -> list[dict[str, Any]]:
        return [r["body"] for r in self.server.requests if r["path"] == "/v1/chat/completions"]

    def test_multi_turn_conversation_carries_context(self) -> None:
        self.server.chat_replies = [_reply("Noted: alpha.", usage=10), _reply("You said alpha.", usage=20)]
        repl, out = self.run_repl(["remember alpha", "what did I say?", "/usage", "/context"])
        self.assertIn("Noted: alpha.", out)
        self.assertIn("You said alpha.", out)
        second = self.chat_bodies()[1]["messages"]
        self.assertEqual([m["role"] for m in second], ["system", "user", "assistant", "user"])
        self.assertEqual(second[1]["content"], "remember alpha")
        self.assertEqual(repl.usage["total_tokens"], 32)
        self.assertIn('"total_tokens": 32', out)
        self.assertIn("[4] assistant You said alpha.", out)
        self.assertIn("(5 messages", out)

    def test_multiline_prompt_and_show_and_system(self) -> None:
        self.server.chat_replies = [_reply("ok")]
        _, out = self.run_repl(['"""', "line one", "line two", '"""', "/show 1", "/system"])
        self.assertEqual(self.chat_bodies()[0]["messages"][1]["content"], "line one\nline two")
        self.assertIn('"content": "line one\\nline two"', out)
        self.assertIn("You are an autonomous coding agent", out)

    def test_backslash_continuation(self) -> None:
        self.server.chat_replies = [_reply("ok")]
        self.run_repl(["first \\", "second"])
        self.assertEqual(self.chat_bodies()[0]["messages"][1]["content"], "first \nsecond")

    def test_tool_calls_render_and_undo_retry_reset(self) -> None:
        self.server.chat_replies = [
            _reply(None, ("c1", "write_file", {"path": "a.txt", "content": "hi"})),
            _reply("wrote it"),
            _reply("second try"),
        ]
        repl, out = self.run_repl(["make a.txt", "/retry"])
        self.assertIn("[tool] write_file: a.txt (2 chars)", out)
        self.assertEqual(Path(self.cwd, "a.txt").read_text(), "hi")
        self.assertIn("dropped 4 message(s): make a.txt", out)
        self.assertIn("second try", out)
        last = self.chat_bodies()[-1]["messages"]
        self.assertEqual([m["role"] for m in last], ["system", "user"])
        self.assertEqual(len(repl.messages), 3)

        repl.handle_command("/undo")
        self.assertEqual(repl.messages, [])
        repl.handle_command("/undo")
        self.assertIn("nothing to undo", repl.out.getvalue())

    def test_step_mode_run_skip_edit_fake_continue(self) -> None:
        self.server.chat_replies = [
            _reply(
                None,
                ("c1", "write_file", {"path": "one.txt", "content": "1"}),
                ("c2", "write_file", {"path": "two.txt", "content": "2"}),
                ("c3", "write_file", {"path": "three.txt", "content": "3"}),
                ("c4", "read_file", {"path": "nothing.txt"}),
            ),
            _reply(None, ("c5", "write_file", {"path": "five.txt", "content": "5"})),
            _reply("done"),
        ]
        lines = [
            "go",
            "",  # c1: run
            "s",  # c2: skip
            "e",  # c3: edit arguments, then run
            '{"path": "edited.txt", "content": "E"}',
            "r",
            "?",  # c4: help, then fake
            "f",
            "pretend contents",
            "c",  # c5: stop pausing
        ]
        _, out = self.run_repl(lines, step=True)
        self.assertTrue(Path(self.cwd, "one.txt").exists())
        self.assertFalse(Path(self.cwd, "two.txt").exists())
        self.assertFalse(Path(self.cwd, "three.txt").exists())
        self.assertEqual(Path(self.cwd, "edited.txt").read_text(), "E")
        self.assertTrue(Path(self.cwd, "five.txt").exists())
        self.assertIn("[step] turn 1, call c1: write_file", out)
        self.assertIn("fake the result", out)  # step help after "?"

        tool_msgs = {m["tool_call_id"]: m["content"] for m in self.chat_bodies()[1]["messages"] if m["role"] == "tool"}
        self.assertTrue(json.loads(tool_msgs["c1"])["ok"])
        self.assertTrue(json.loads(tool_msgs["c2"])["skipped"])
        self.assertIn("edited.txt", tool_msgs["c3"])
        self.assertEqual(tool_msgs["c4"], "pretend contents")
        # The edited arguments stay what the model sent in the transcript.
        assistant = self.chat_bodies()[1]["messages"][2]
        self.assertIn("three.txt", assistant["tool_calls"][2]["function"]["arguments"])
        self.assertIn("call c5", out)

    def test_step_abort_and_server_error_roll_back(self) -> None:
        self.server.chat_replies = [
            _reply(None, ("c1", "write_file", {"path": "x.txt", "content": "x"})),
            (500, {"error": {"message": "boom"}}),
            _reply("fine"),
        ]
        repl, out = self.run_repl(["/step on", "first", "a", "/step off", "second", "third"])
        self.assertIn("[aborted] aborted by the operator (prompt rolled back)", out)
        self.assertIn("[error]", out)
        self.assertIn("boom", out)
        self.assertFalse(Path(self.cwd, "x.txt").exists())
        self.assertEqual([m["role"] for m in repl.messages], ["system", "user", "assistant"])
        self.assertEqual(repl.messages[1]["content"], "third")

    def test_step_eof_aborts(self) -> None:
        self.server.chat_replies = [_reply(None, ("c1", "write_file", {"path": "x.txt", "content": "x"}))]
        repl, out = self.run_repl(["go"], step=True)
        self.assertIn("input closed during step mode", out)
        self.assertEqual(repl.messages, [])

    def test_prompt_approval_uses_repl_input(self) -> None:
        self.server.chat_replies = [
            _reply(None, ("c1", "write_file", {"path": "y.txt", "content": "y"})),
            _reply(None, ("c2", "write_file", {"path": "n.txt", "content": "n"})),
            _reply("ok"),
        ]
        script = _Script(["go", "y", "n"])
        out = io.StringIO()
        repl = bionic_repl.BionicRepl(self.config(approval="prompt"), input_fn=script, out=out)
        repl.loop()
        self.assertTrue(Path(self.cwd, "y.txt").exists())
        self.assertFalse(Path(self.cwd, "n.txt").exists())
        self.assertIn("[approval] Allow write_file: y.txt (1 chars)? [y/N] ", script.prompts)

    def test_manual_tool_and_tools_listing(self) -> None:
        Path(self.cwd, "r.txt").write_text("readable\n")
        repl, out = self.run_repl(
            [
                '/tool read_file {"path": "r.txt"}',
                "/tool read_file {bad",
                "/tool",
                "/tools",
                '/tool read_file {"path": "../x"}',
            ]
        )
        self.assertIn("readable", out)
        self.assertIn("invalid JSON arguments", out)
        self.assertIn("usage: /tool NAME", out)
        self.assertIn("apply_patch(", out)
        self.assertIn("Denied", out)
        self.assertEqual(repl.messages, [])
        self.assertEqual(self.chat_bodies(), [])

    def test_settings_commands(self) -> None:
        self.server.chat_replies = [_reply("ok")]
        repl, out = self.run_repl(
            [
                "/set temperature 0.3",
                "/set max-tokens 64",
                "/set max_turns 0",
                "/set max_turns none",
                "/set bogus 1",
                "/model m2",
                "/approval read-only",
                "/approval nope",
                "/stream on",
                "/config",
                "/models",
                "hello",
                "/whatever",
            ]
        )
        body = self.chat_bodies()[0]
        self.assertEqual(body["temperature"], 0.3)
        self.assertEqual(body["max_tokens"], 64)
        self.assertEqual(body["model"], "m2")
        self.assertEqual([t["function"]["name"] for t in body["tools"]], ["read_file"])
        self.assertTrue(body.get("stream"))
        self.assertIn("max_turns must be positive", out)
        self.assertIn("max_turns cannot be cleared", out)
        self.assertIn("usage: /set KEY VALUE", out)
        self.assertIn("choose one of auto, prompt, read-only", out)
        self.assertIn("api_key: ***", out)
        self.assertIn("  qwen/qwen3.8-27b", out)
        self.assertIn("unknown command /whatever", out)
        self.assertEqual(repl.config.max_turns, bionic.DEFAULT_MAX_TURNS)

    def test_save_load_and_session_persistence(self) -> None:
        self.server.chat_replies = [_reply("one"), _reply("two")]
        sess_dir = str(Path(self.cwd) / "sessions")
        transcript = Path(self.cwd) / "t.json"
        repl, out = self.run_repl(
            ["hello", f"/save {transcript}", "/reset", f"/load {transcript}", "/load nope.json"],
            session_id="s1",
            session_dir=sess_dir,
        )
        self.assertIn("saved 3 message(s)", out)
        self.assertIn("loaded 3 message(s)", out)
        self.assertIn("cannot load", out)
        session_file = Path(sess_dir) / "s1.json"
        self.assertEqual(len(json.loads(session_file.read_text())), 3)

        # A new REPL on the same session resumes it.
        repl2, out2 = self.run_repl(["again"], session_id="s1", session_dir=sess_dir)
        self.assertIn("session s1 (3 messages)", out2)
        users = [m["content"] for m in self.chat_bodies()[-1]["messages"] if m["role"] == "user"]
        self.assertEqual(users, ["hello", "again"])
        self.assertEqual(len(json.loads(session_file.read_text())), 5)

        repl2.handle_command("/reset")
        self.assertFalse(session_file.exists())

    def test_max_turns_keeps_partial_transcript(self) -> None:
        self.server.chat_handler = lambda body, req: _reply(None, ("c", "read_file", {"path": "none"}))
        repl, out = self.run_repl(["/set max_turns 2", "loop"])
        self.assertIn("max_turns=2", out)
        self.assertEqual(len(repl.messages), 6)

    def test_run_agent_before_tool_hook_contract(self) -> None:
        self.server.chat_replies = [
            _reply(None, ("c1", "write_file", {"path": "h.txt", "content": "h"})),
            _reply("done"),
        ]
        seen: list[bionic.ToolStep] = []

        def hook(step: bionic.ToolStep) -> None:
            seen.append(step)
            step.execute()  # executed by the hook: run_agent must not run it again
            Path(self.cwd, "h.txt").unlink()
            return None

        messages: list[dict[str, Any]] = []
        result = bionic.run_agent("x", self.config(), messages=messages, before_tool=hook)
        self.assertEqual(result.content, "done")
        self.assertFalse(Path(self.cwd, "h.txt").exists())
        self.assertEqual(seen[0].name, "write_file")
        self.assertEqual([m["role"] for m in messages], ["system", "user", "assistant", "tool", "assistant"])

    def test_cli_repl_flag_end_to_end(self) -> None:
        self.server.chat_replies = [_reply("first answer"), _reply("second answer")]
        env = {**os.environ, "PYTHONUTF8": "1", "PYTHONPATH": str(CLI_PY_ROOT)}
        proc = subprocess.run(
            [
                sys.executable,
                "-m",
                "aircd.bionic",
                "--repl",
                "--base-url",
                self.base_url,
                "--model",
                "m1",
                "--cwd",
                self.cwd,
                "--no-plugins",
                "-p",
                "hi",
            ],
            input="and again\n/context\n/exit\n",
            capture_output=True,
            text=True,
            env=env,
            cwd=str(CLI_PY_ROOT),
            timeout=60,
        )
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("airc-bionic REPL", proc.stdout)
        self.assertIn("first answer", proc.stdout)
        self.assertIn("second answer", proc.stdout)
        self.assertIn("[3] user and again", proc.stdout)

    def test_cli_debug_implies_repl(self) -> None:
        parser = bionic.build_parser()
        args = parser.parse_args(["--debug"])
        self.assertTrue(args.debug)
        self.assertFalse(args.repl)
        self.server.chat_replies = [_reply(None, ("c1", "write_file", {"path": "d.txt", "content": "d"})), _reply("ok")]
        env = {**os.environ, "PYTHONUTF8": "1", "PYTHONPATH": str(CLI_PY_ROOT)}
        proc = subprocess.run(
            [
                sys.executable,
                "-m",
                "aircd.bionic",
                "--debug",
                "--base-url",
                self.base_url,
                "--model",
                "m1",
                "--cwd",
                self.cwd,
                "--no-plugins",
            ],
            input="write d\ns\n",
            capture_output=True,
            text=True,
            env=env,
            cwd=str(CLI_PY_ROOT),
            timeout=60,
        )
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("step mode on", proc.stdout)
        self.assertIn("[step] skipped", proc.stdout)
        self.assertFalse(Path(self.cwd, "d.txt").exists())


if __name__ == "__main__":
    unittest.main()
