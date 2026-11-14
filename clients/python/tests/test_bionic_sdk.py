"""Tests for the programmatic ``aircd.BionicAgent`` SDK client."""

from __future__ import annotations

import json
import os
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from typing import Any
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import aircd  # noqa: E402
from aircd import bionic  # noqa: E402
from tests.test_bionic import _FakeBionicServer, _SSEReply  # noqa: E402


def _answer(text: str) -> tuple[int, Any]:
    return (200, {"choices": [{"message": {"role": "assistant", "content": text}}]})


def _tool_call(name: str, args: dict[str, Any], call_id: str = "tc1") -> tuple[int, Any]:
    return (
        200,
        {
            "choices": [
                {
                    "message": {
                        "role": "assistant",
                        "content": None,
                        "tool_calls": [
                            {"id": call_id, "type": "function", "function": {"name": name, "arguments": json.dumps(args)}}
                        ],
                    }
                }
            ]
        },
    )


class BionicAgentTests(unittest.TestCase):
    def setUp(self) -> None:
        env = mock.patch.dict(os.environ)
        env.start()
        self.addCleanup(env.stop)
        for key in [k for k in os.environ if k.startswith(("BIONIC_", "AIRCD_BIONIC_", "OPENAI_"))]:
            del os.environ[key]

        self.server = _FakeBionicServer()
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        self.base_url = f"http://127.0.0.1:{self.server.server_address[1]}/v1"
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.sessions = str(Path(self.tmp.name) / "sessions")

    def _agent(self, **options: Any) -> aircd.BionicAgent:
        values: dict[str, Any] = {
            "base_url": self.base_url,
            "model": "m1",
            "cwd": self.tmp.name,
            "session_dir": self.sessions,
        }
        values.update(options)
        return aircd.BionicAgent(**values)

    def _chat_bodies(self) -> list[dict[str, Any]]:
        return [r["body"] for r in self.server.requests if r["path"] == "/v1/chat/completions"]

    def test_exported_from_package_and_client_module(self) -> None:
        from aircd.client import BionicAgent as FromClient

        self.assertIs(aircd.BionicAgent, bionic.BionicAgent)
        self.assertIs(FromClient, bionic.BionicAgent)
        for name in ("BionicAgent", "BionicConfig", "BionicError", "RunResult"):
            self.assertIn(name, aircd.__all__)

    def test_options_resolve_like_cli_and_validate(self) -> None:
        os.environ["BIONIC_MAX_TURNS"] = "7"
        os.environ["BIONIC_MODEL"] = "env-model"
        agent = aircd.BionicAgent(base_url=self.base_url.removeprefix("http://") + "/", cwd=self.tmp.name)
        cfg = agent.config
        self.assertEqual(cfg.base_url, self.base_url)  # normalized
        self.assertEqual((cfg.model, cfg.max_turns), ("env-model", 7))  # env fallback
        self.assertEqual(self._agent(max_turns=3).config.max_turns, 3)  # explicit wins
        self.assertEqual(cfg.workspace_root, str(Path(self.tmp.name).resolve()))

        # config is a copy
        cfg.max_turns = 99
        self.assertEqual(agent.config.max_turns, 7)

        # stream_format alone does not force streaming when stream= is explicit
        self.assertFalse(self._agent(stream=False, stream_format="jsonl").config.stream)
        self.assertTrue(self._agent(stream_format="jsonl").config.stream)

        with self.assertRaises(TypeError):
            self._agent(max_turn=3)
        with self.assertRaises(TypeError):
            aircd.BionicAgent(bionic.BionicConfig(), model="x")
        with self.assertRaises(bionic.BionicError):
            self._agent(approval="yolo")
        with self.assertRaises(bionic.BionicError):
            self._agent(cwd=str(Path(self.tmp.name) / "missing"))

        explicit = bionic.BionicConfig(base_url=self.base_url, model="m2", cwd=self.tmp.name)
        self.assertEqual(aircd.BionicAgent(explicit).config, explicit)

    def test_run_tool_loop_auto_model_cached_and_handlers(self) -> None:
        self.server.chat_replies = [
            _tool_call("write_file", {"path": "note.txt", "content": "sdk\n"}),
            _answer("Wrote note.txt"),
            _answer("Second run."),
        ]
        agent_events: list[dict[str, Any]] = []
        call_events: list[dict[str, Any]] = []
        agent = self._agent(model="auto", on_event=agent_events.append)
        self.assertEqual(agent.model, "qwen/qwen3.8-27b")

        result = agent.run("write a note", on_event=call_events.append)
        self.assertIsInstance(result, aircd.RunResult)
        self.assertEqual((result.content, result.model, result.turns, result.tool_calls), ("Wrote note.txt", "qwen/qwen3.8-27b", 2, 1))
        self.assertIsNone(result.session_id)
        self.assertEqual((Path(self.tmp.name) / "note.txt").read_text(encoding="utf-8"), "sdk\n")
        self.assertEqual(agent_events, call_events)
        self.assertEqual([e["type"] for e in agent_events], ["turn", "tool_call", "tool_result", "turn", "done"])

        agent.run("again")
        model_lookups = [r for r in self.server.requests if r["path"] == "/v1/models"]
        self.assertEqual(len(model_lookups), 1)  # auto discovery cached across runs
        self.assertTrue(all(b["model"] == "qwen/qwen3.8-27b" for b in self._chat_bodies()))
        self.assertTrue(all("stream" not in b for b in self._chat_bodies()))

        with self.assertRaises(bionic.BionicError):
            agent.run("   ")
        with self.assertRaises(bionic.BionicError):
            self._agent(model="").run("hi")

    def test_keep_session_continues_conversation(self) -> None:
        self.server.chat_replies = [_answer("A"), _answer("B"), _answer("C"), _answer("D")]
        agent = self._agent(keep_session=True)
        first = agent.run("remember alpha")
        self.assertIsNotNone(first.session_id)
        self.assertEqual(agent.session_id, first.session_id)
        self.assertTrue((Path(self.sessions) / f"{first.session_id}.json").is_file())

        second = agent.run("what did i say?")
        self.assertEqual(second.session_id, first.session_id)
        users = [m["content"] for m in self._chat_bodies()[-1]["messages"] if m["role"] == "user"]
        self.assertEqual(users, ["remember alpha", "what did i say?"])

        # An explicit session_id is a one-off and does not replace the agent's own.
        other = agent.run("side quest", session_id="side-1")
        self.assertEqual(other.session_id, "side-1")
        self.assertEqual(agent.session_id, first.session_id)

        agent.new_session()
        fresh = agent.run("new topic")
        self.assertNotEqual(fresh.session_id, first.session_id)
        users = [m["content"] for m in self._chat_bodies()[-1]["messages"] if m["role"] == "user"]
        self.assertEqual(users, ["new topic"])

        # Without keep_session each run is independent.
        self.server.chat_replies = [_answer("x"), _answer("y")]
        plain = self._agent()
        plain.run("one")
        plain.run("two")
        users = [m["content"] for m in self._chat_bodies()[-1]["messages"] if m["role"] == "user"]
        self.assertEqual(users, ["two"])
        self.assertIsNone(plain.session_id)

    def test_stream_yields_events_then_result(self) -> None:
        args = json.dumps({"path": "out.txt", "content": "streamed\n"})
        self.server.chat_replies = [
            (
                200,
                _SSEReply.chunks(
                    [{"role": "assistant", "tool_calls": [{"index": 0, "id": "c1", "type": "function", "function": {"name": "write_file", "arguments": args}}]}]
                ),
            ),
            (200, _SSEReply.chunks([{"role": "assistant", "content": "Wrote "}, {"content": "out.txt"}])),
        ]
        events = list(self._agent().stream("make out.txt"))
        kinds = [e["type"] for e in events]
        self.assertEqual(kinds, ["turn", "tool_call", "tool_result", "turn", "token", "token", "done", "result"])
        result = events[-1]["result"]
        self.assertEqual((result.content, result.turns, result.tool_calls), ("Wrote out.txt", 2, 1))
        self.assertTrue(all(b["stream"] is True for b in self._chat_bodies()))
        self.assertEqual((Path(self.tmp.name) / "out.txt").read_text(encoding="utf-8"), "streamed\n")

    def test_stream_raises_errors_and_cancels_on_close(self) -> None:
        self.server.chat_replies = [(400, {"error": {"message": "model not loaded"}})]
        with self.assertRaisesRegex(bionic.BionicError, "model not loaded"):
            list(self._agent().stream("hi"))

        # Abandon the iterator after the first event: the run stops before the next turn.
        gate = threading.Event()
        replies = [
            _tool_call("write_file", {"path": "never.txt", "content": "no\n"}),
            _answer("should not be requested"),
        ]

        def held_reply(_body: Any, _req: Any) -> tuple[int, Any]:
            gate.wait(5)  # hold the first turn until the consumer has gone away
            return replies.pop(0)

        self.server.chat_handler = held_reply
        gen = self._agent().stream("go")
        self.assertEqual(next(gen)["type"], "turn")
        gen.close()
        gate.set()
        deadline = time.time() + 5
        while time.time() < deadline and any(t.name == "bionic-agent-stream" for t in threading.enumerate()):
            time.sleep(0.05)
        self.assertFalse(any(t.name == "bionic-agent-stream" for t in threading.enumerate()))
        self.assertFalse((Path(self.tmp.name) / "never.txt").exists())
        self.assertEqual(len(replies), 1)  # second turn never requested

    def test_prompt_approval_uses_sdk_approver(self) -> None:
        self.server.chat_replies = [
            _tool_call("write_file", {"path": "denied.txt", "content": "x"}),
            _answer("done"),
        ]
        seen: list[str] = []
        agent = self._agent(approval="prompt", approver=lambda name, _args: seen.append(name) or False)
        self.assertEqual(agent.run("write it").content, "done")
        self.assertEqual(seen, ["write_file"])
        self.assertFalse((Path(self.tmp.name) / "denied.txt").exists())
        tool_msg = [m for m in self._chat_bodies()[-1]["messages"] if m["role"] == "tool"][0]
        self.assertIn("denied", tool_msg["content"].lower())

    def test_with_options_revalidates_and_carries_state(self) -> None:
        self.server.chat_replies = [_answer("A"), _answer("B")]
        events: list[dict[str, Any]] = []
        agent = self._agent(model="auto", keep_session=True, on_event=events.append)
        agent.run("first")
        clone = agent.with_options(max_turns=2, system_prompt="Be terse.")
        self.assertEqual(clone.config.max_turns, 2)
        self.assertEqual(clone.session_id, agent.session_id)
        self.assertTrue(clone.keep_session)
        self.assertEqual(agent.config.max_turns, bionic.DEFAULT_MAX_TURNS)  # original untouched

        clone.run("second")
        body = self._chat_bodies()[-1]
        self.assertEqual(body["messages"][0], {"role": "system", "content": "Be terse."})
        self.assertEqual([m["content"] for m in body["messages"] if m["role"] == "user"], ["first", "second"])
        self.assertEqual(len([r for r in self.server.requests if r["path"] == "/v1/models"]), 1)
        self.assertEqual(sum(1 for e in events if e["type"] == "done"), 2)

        self.assertFalse(agent.with_options(keep_session=False).keep_session)
        with self.assertRaises(bionic.BionicError):
            agent.with_options(max_turns=0)
        with self.assertRaises(TypeError):
            agent.with_options(bogus=1)

    def test_list_models(self) -> None:
        agent = self._agent(api_key="k9")
        self.assertEqual([m["id"] for m in agent.list_models()], ["qwen/qwen3.8-27b", "google/gemma-4-26b-a4b-qat"])
        self.assertEqual(len(agent.list_models(include_all=True)), 3)
        self.assertEqual(self.server.requests[0]["headers"]["authorization"], "Bearer k9")


if __name__ == "__main__":
    unittest.main()
