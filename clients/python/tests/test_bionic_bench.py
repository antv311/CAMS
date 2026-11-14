"""Tests for the ``airc-bionic-bench`` benchmark / evaluation suite and run_agent token usage."""

from __future__ import annotations

import contextlib
import io
import json
import os
import socket
import sys
import tempfile
import threading
import unittest
from pathlib import Path
from typing import Any
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from aircd import bionic, bionic_bench  # noqa: E402
from aircd.bionic import BionicError, RunResult  # noqa: E402
from aircd.bionic_bench import Scenario  # noqa: E402
from tests.test_bionic import _FakeBionicServer, _SSEReply  # noqa: E402


def _usage(prompt: int, completion: int) -> dict[str, int]:
    return {"prompt_tokens": prompt, "completion_tokens": completion, "total_tokens": prompt + completion}


def _answer(text: str, usage: Any = None) -> dict[str, Any]:
    body: dict[str, Any] = {"choices": [{"message": {"role": "assistant", "content": text}}]}
    if usage is not None:
        body["usage"] = usage
    return body


def _tool(name: str, args: dict[str, Any], usage: Any = None, call_id: str = "tc1") -> dict[str, Any]:
    body: dict[str, Any] = {
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
    }
    if usage is not None:
        body["usage"] = usage
    return body


def _scripted_model(body: dict[str, Any], _req: Any) -> tuple[int, Any]:
    """A tiny deterministic 'model' that solves some built-in scenarios via real tool calls."""
    if body.get("max_tokens") == 1:  # health probe
        return 200, _answer("ok", _usage(5, 1))
    messages = body["messages"]
    prompt_tokens = sum(len(str(m.get("content") or "")) for m in messages) // 4
    last = messages[-1]
    users = [m["content"] for m in messages if m["role"] == "user"]
    prompt = users[-1]
    if last["role"] == "tool":
        result = json.loads(last["content"]) if last["content"].startswith("{") else {"text": last["content"]}
        if "secret.txt" in prompt:
            code = "ZEBRA-4417" if "ZEBRA-4417" in last["content"] else "unknown"
            return 200, _answer(code, _usage(prompt_tokens, 4))
        if result.get("error"):
            return 200, _answer("failed: " + result["error"], _usage(prompt_tokens, 6))
        return 200, _answer("Done.", _usage(prompt_tokens, 2))
    if "secret.txt" in prompt:
        return 200, _tool("read_file", {"path": "secret.txt"}, _usage(prompt_tokens, 12))
    if "hello.txt" in prompt:
        return 200, _tool("write_file", {"path": "hello.txt", "content": "bionic bench ok\n"}, _usage(prompt_tokens, 20))
    if "Remember this code word" in prompt:
        return 200, _answer("OK", _usage(prompt_tokens, 1))
    if "code word I gave you earlier" in prompt:
        earlier = " ".join(users[:-1])
        word = "AMBER-62" if "AMBER-62" in earlier else "NONE"
        return 200, _tool("write_file", {"path": "codeword.txt", "content": word}, _usage(prompt_tokens, 15))
    return 200, _answer("I cannot do that.", _usage(prompt_tokens, 5))


def _scenario(name: str) -> Scenario:
    return next(s for s in bionic_bench.builtin_scenarios() if s.name == name)


class _ServerCase(unittest.TestCase):
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

    def _config(self, **overrides: Any) -> bionic.BionicConfig:
        values: dict[str, Any] = {"base_url": self.base_url, "model": "m1", "cwd": self.tmp.name, "workspace_root": self.tmp.name}
        values.update(overrides)
        return bionic.BionicConfig(**values)


class RunAgentUsageTests(_ServerCase):
    def test_non_stream_usage_is_summed_per_turn(self) -> None:
        self.server.chat_replies = [
            (200, _tool("write_file", {"path": "a.txt", "content": "x"}, _usage(100, 10))),
            (200, _answer("done", _usage(130, 3))),
        ]
        result = bionic.run_agent("go", self._config())
        self.assertEqual(result.usage, {"prompt_tokens": 230, "completion_tokens": 13, "total_tokens": 243})
        self.assertEqual(result.turn_usage, [_usage(100, 10), _usage(130, 3)])

    def test_missing_usage_leaves_totals_empty(self) -> None:
        self.server.chat_replies = [(200, _answer("hi"))]
        result = bionic.run_agent("go", self._config())
        self.assertEqual(result.usage, {})
        self.assertEqual(result.turn_usage, [None])

    def test_partial_usage_derives_total(self) -> None:
        self.server.chat_replies = [(200, _answer("hi", {"prompt_tokens": 7, "completion_tokens": 2}))]
        self.assertEqual(bionic.run_agent("go", self._config()).usage["total_tokens"], 9)

    def test_stream_requests_and_reads_usage_chunk(self) -> None:
        reply = _SSEReply.chunks([{"role": "assistant"}, {"content": "hel"}, {"content": "lo"}], done=False)
        usage_chunk = {"object": "chat.completion.chunk", "choices": [], "usage": _usage(40, 2)}
        reply.events.append("data: " + json.dumps(usage_chunk) + "\n\n")
        reply.events.append("data: [DONE]\n\n")
        self.server.chat_replies = [(200, reply)]
        result = bionic.run_agent("go", self._config(stream=True))
        self.assertEqual(result.content, "hello")
        self.assertEqual(result.usage, _usage(40, 2))
        body = self.server.requests[-1]["body"]
        self.assertEqual(body["stream_options"], {"include_usage": True})

    def test_stream_json_fallback_keeps_usage(self) -> None:
        self.server.chat_replies = [(200, _answer("plain", _usage(9, 1)))]
        result = bionic.run_agent("go", self._config(stream=True))
        self.assertEqual((result.content, result.usage), ("plain", _usage(9, 1)))

    def test_usage_not_written_into_history(self) -> None:
        self.server.chat_replies = [
            (200, _tool("read_file", {"path": "nope.txt"}, _usage(10, 1))),
            (200, _answer("ok", _usage(12, 1))),
        ]
        bionic.run_agent("go", self._config())
        history = self.server.requests[-1]["body"]["messages"]
        self.assertTrue(all("usage" not in m for m in history))

    def test_cli_json_includes_usage(self) -> None:
        self.server.chat_replies = [(200, _answer("hi", _usage(3, 1)))]
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = bionic.main(["--base-url", self.base_url, "--model", "m1", "--cwd", self.tmp.name, "-p", "x", "--json"])
        self.assertEqual(code, 0)
        self.assertEqual(json.loads(out.getvalue())["usage"], _usage(3, 1))


class ChecksTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name).resolve()

    def test_file_equals_normalizes_newlines_and_trailing_space(self) -> None:
        (self.root / "f.txt").write_bytes(b"a  \r\nb\r\n\r\n")
        self.assertTrue(bionic_bench.run_check({"type": "file_equals", "path": "f.txt", "text": "a\nb"}, self.root, [])["ok"])
        bad = bionic_bench.run_check({"type": "file_equals", "path": "f.txt", "text": "a\nc"}, self.root, [])
        self.assertFalse(bad["ok"])
        self.assertIn("f.txt is", bad["detail"])

    def test_missing_file_and_escape(self) -> None:
        missing = bionic_bench.run_check({"type": "file_contains", "path": "x.txt", "text": "a"}, self.root, [])
        self.assertEqual((missing["ok"], missing["detail"]), (False, "x.txt was not created"))
        escape = bionic_bench.run_check({"type": "file_absent", "path": "../outside"}, self.root, [])
        self.assertFalse(escape["ok"])
        self.assertTrue(bionic_bench.run_check({"type": "file_absent", "path": "gone.txt"}, self.root, [])["ok"])

    def test_answer_checks(self) -> None:
        check = bionic_bench.run_check
        self.assertTrue(check({"type": "answer_contains", "text": "zebra"}, self.root, ["The code: ZEBRA-1"])["ok"])
        self.assertFalse(check({"type": "answer_contains", "text": "zebra", "case_sensitive": True}, self.root, ["ZEBRA"])["ok"])
        any_check = {"type": "answer_matches_any", "texts": ["2", "two"]}
        for answer in ("2", "**2**", "2.", "It has 2 lines", "Two"):
            self.assertTrue(check(any_check, self.root, [answer])["ok"], answer)
        self.assertFalse(check(any_check, self.root, ["12"])["ok"])
        first = {"type": "answer_contains", "text": "OK", "prompt": 0}
        self.assertTrue(check(first, self.root, ["OK", "later"])["ok"])
        self.assertFalse(check({"type": "answer_contains", "text": "x", "prompt": 5}, self.root, ["x"])["ok"])

    def test_command_check_uses_python_placeholder(self) -> None:
        (self.root / "ok.py").write_text("import sys; sys.exit(0)\n")
        (self.root / "bad.py").write_text("print('boom'); raise SystemExit(4)\n")
        self.assertTrue(bionic_bench.run_check({"type": "command", "command": "{python} ok.py"}, self.root, [])["ok"])
        bad = bionic_bench.run_check({"type": "command", "command": "{python} bad.py"}, self.root, [])
        self.assertFalse(bad["ok"])
        self.assertIn("exit 4", bad["detail"])
        self.assertIn("boom", bad["detail"])
        self.assertTrue(
            bionic_bench.run_check({"type": "command", "command": "{python} bad.py", "expect_exit": 4}, self.root, [])["ok"]
        )

    def test_builtin_checks_pass_on_reference_solutions(self) -> None:
        """Each built-in scenario is solvable: apply a hand-written solution and its checks pass."""
        solutions: dict[str, tuple[dict[str, str], list[str]]] = {
            "read-answer": ({}, ["ZEBRA-4417"]),
            "write-file": ({"hello.txt": "bionic bench ok\n"}, ["done"]),
            "edit-config": ({"config.ini": "[server]\nhost = 127.0.0.1\nport = 9090\nworkers = 4\n"}, ["done"]),
            "command-output": ({}, ["2870"]),
            "aggregate-files": ({"total.txt": "55\n"}, ["done"]),
            "fix-and-test": ({"mathutils.py": "def add(a, b):\n    return a + b\n\n\ndef mul(a, b):\n    return a * b\n"}, ["DONE"]),
            "session-recall": ({"codeword.txt": "AMBER-62"}, ["OK", "done"]),
            "session-followup": ({"notes.md": "# Release notes\n- fixed login timeout\n"}, ["ok", "ok", "2"]),
        }
        scenarios = bionic_bench.builtin_scenarios()
        self.assertEqual(sorted(s.name for s in scenarios), sorted(solutions))
        for scenario in scenarios:
            with self.subTest(scenario.name), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp).resolve()
                bionic_bench.seed_workspace(root, scenario.files)
                files, answers = solutions[scenario.name]
                self.assertEqual(len(answers), len(scenario.prompts))
                # unsolved, the scenario must fail (otherwise it measures nothing)
                unsolved = [bionic_bench.run_check(c, root, ["?"] * len(answers)) for c in scenario.checks]
                self.assertFalse(all(c["ok"] for c in unsolved))
                bionic_bench.seed_workspace(root, files)
                results = [bionic_bench.run_check(c, root, answers) for c in scenario.checks]
                self.assertTrue(all(c["ok"] for c in results), results)

    def test_seed_workspace_rejects_escape(self) -> None:
        with self.assertRaises(BionicError):
            bionic_bench.seed_workspace(self.root, {"../evil.txt": "x"})


class ScenarioLoadingTests(unittest.TestCase):
    def test_from_dict_validation(self) -> None:
        ok = Scenario.from_dict({"name": "s", "prompt": "do it", "checks": [{"type": "file_absent", "path": "x"}]})
        self.assertEqual(ok.prompts, ["do it"])
        for bad in (
            [],
            {"prompts": ["x"], "checks": [{"type": "command"}]},
            {"name": "s", "prompts": [], "checks": [{"type": "command"}]},
            {"name": "s", "prompts": ["x"], "checks": []},
            {"name": "s", "prompts": ["x"], "checks": [{"type": "nope"}]},
            {"name": "s", "prompts": ["x"], "checks": [{"type": "command"}], "files": {"a": 1}},
        ):
            with self.subTest(bad=bad), self.assertRaises(BionicError):
                Scenario.from_dict(bad)

    def test_load_file_and_select(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "s.json"
            path.write_text(
                json.dumps({"scenarios": [{"name": "custom", "tags": ["x"], "prompts": ["p"], "checks": [{"type": "command", "command": "exit 0"}]}]}),
                encoding="utf-8",
            )
            loaded = bionic_bench.load_scenarios_file(str(path))
            self.assertEqual([s.name for s in loaded], ["custom"])
            (Path(tmp) / "bad.json").write_text("{", encoding="utf-8")
            with self.assertRaises(BionicError):
                bionic_bench.load_scenarios_file(str(Path(tmp) / "bad.json"))
        builtins = bionic_bench.builtin_scenarios()
        multi = bionic_bench.select_scenarios(builtins, None, ["multi-turn"])
        self.assertEqual({s.name for s in multi}, {"session-recall", "session-followup"})
        self.assertTrue(all(len(s.prompts) > 1 for s in multi))
        with self.assertRaises(BionicError):
            bionic_bench.select_scenarios(builtins, ["missing"], None)
        with self.assertRaises(BionicError):
            bionic_bench.select_scenarios(builtins, None, ["no-such-tag"])
        with self.assertRaises(BionicError):
            bionic_bench.select_scenarios(builtins + builtins[:1], None, None)

    def test_builtin_copies_are_independent(self) -> None:
        first = bionic_bench.builtin_scenarios()
        first[0].files["x"] = "y"
        self.assertNotIn("x", bionic_bench.builtin_scenarios()[0].files)


class MetricsTests(unittest.TestCase):
    def test_percentile_and_describe(self) -> None:
        self.assertIsNone(bionic_bench.percentile([], 50))
        self.assertEqual(bionic_bench.percentile([5.0], 95), 5.0)
        self.assertEqual(bionic_bench.percentile([1, 2, 3, 4], 50), 2.5)
        self.assertAlmostEqual(bionic_bench.percentile(list(range(1, 101)), 95), 95.05)
        d = bionic_bench.describe([10.0, 20.0, 30.0])
        self.assertEqual((d["count"], d["mean"], d["p50"], d["min"], d["max"]), (3, 20.0, 20.0, 10.0, 30.0))
        self.assertEqual(bionic_bench.describe([])["count"], 0)

    def test_event_timer(self) -> None:
        now = [0.0]
        timer = bionic_bench.EventTimer(clock=lambda: now[0])

        def at(t: float, event: dict[str, Any]) -> None:
            now[0] = t
            timer(event)

        at(0.0, {"type": "turn", "turn": 1})
        at(0.2, {"type": "reasoning", "text": "hm"})
        at(0.3, {"type": "token", "text": "x"})
        at(1.0, {"type": "tool_call", "id": "a", "name": "read_file"})
        at(1.05, {"type": "tool_result", "id": "a", "name": "read_file", "error": None})
        at(1.05, {"type": "tool_call", "id": "b", "name": "run_command"})
        at(1.55, {"type": "tool_result", "id": "b", "name": "run_command", "error": "boom"})
        at(2.0, {"type": "turn", "turn": 2})
        at(2.5, {"type": "done"})
        self.assertEqual([round(v) for v in timer.llm_ms], [1000, 500])
        self.assertEqual([round(v) for v in timer.ttft_ms], [200])
        self.assertEqual([(t["name"], round(t["ms"]), t["error"]) for t in timer.tool_ms], [("read_file", 50, None), ("run_command", 500, "boom")])

    def test_memory_sampler_with_fake_reader(self) -> None:
        values = {None: iter([100 << 20, 150 << 20, 120 << 20] + [120 << 20] * 50), 42: iter([1 << 30] * 60)}
        sampler = bionic_bench.MemorySampler([42, 99], interval=0.01, reader=lambda pid: next(values[pid]) if pid in values else None)
        sampler.start()
        threading.Event().wait(0.03)
        mem = sampler.stop()
        self.assertEqual(mem["runner"]["start_mb"], 100.0)
        self.assertEqual(mem["runner"]["peak_mb"], 150.0)
        self.assertEqual(mem["server:42"]["peak_mb"], 1024.0)
        self.assertIsNone(mem["server:99"])

    def test_process_rss_real(self) -> None:
        own = bionic_bench.process_rss()
        self.assertIsInstance(own, int)
        self.assertGreater(own, 1 << 20)
        self.assertEqual(bionic_bench.process_rss(os.getpid()) is not None, True)
        self.assertIsNone(bionic_bench.process_rss(2**31 - 7))


class SuiteTests(_ServerCase):
    def setUp(self) -> None:
        super().setUp()
        self.server.chat_handler = _scripted_model

    def test_suite_runs_scenarios_and_reports_metrics(self) -> None:
        scenarios = [_scenario("read-answer"), _scenario("write-file"), _scenario("session-recall"), _scenario("edit-config")]
        report = bionic_bench.run_suite(scenarios, self._config(), repeat=2, sample_interval=0.05, server_pids=[os.getpid()])
        summary = report["summary"]
        by = summary["by_scenario"]
        self.assertEqual(summary["runs"], 8)
        self.assertEqual({k: v["passed"] for k, v in by.items()}, {"read-answer": 2, "write-file": 2, "session-recall": 2, "edit-config": 0})
        self.assertEqual(summary["pass_rate"], 0.75)

        runs = {(r["scenario"], r["iteration"]): r for r in report["runs"]}
        read = runs[("read-answer", 1)]
        self.assertEqual((read["turns"], read["tool_calls"], read["tool_errors"]), (2, 1, 0))
        self.assertEqual(len(read["latency"]["llm_ms"]), 2)
        self.assertEqual([t["name"] for t in read["latency"]["tools"]], ["read_file"])
        self.assertEqual(read["tokens"]["completion_tokens"], 16)
        self.assertTrue(read["tokens"]["reported"])
        self.assertEqual(len(read["tokens"]["prompt_tokens_per_turn"]), 2)
        self.assertLess(read["tokens"]["prompt_tokens_per_turn"][0], read["tokens"]["prompt_tokens_per_turn"][1])
        self.assertIsNotNone(read["tokens"]["completion_tokens_per_s"])
        self.assertGreater(read["memory"]["runner"]["peak_mb"], 1)
        self.assertGreater(read["memory"][f"server:{os.getpid()}"]["peak_mb"], 1)
        self.assertIsNone(read["workspace"])

        # multi-turn: the second prompt only succeeds because the session carried prompt 1
        recall = runs[("session-recall", 1)]
        self.assertEqual(len(recall["prompts"]), 2)
        self.assertEqual(recall["prompts"][0]["answer"], "OK")
        self.assertTrue(recall["passed"])

        failed = runs[("edit-config", 1)]
        self.assertFalse(failed["passed"])
        self.assertIn("config.ini is", failed["checks"][0]["detail"])

        self.assertEqual(summary["latency_ms"]["llm_call"]["count"], sum(len(r["latency"]["llm_ms"]) for r in report["runs"]))
        self.assertEqual(set(summary["latency_ms"]["per_tool"]), {"read_file", "write_file"})
        self.assertGreater(summary["tokens"]["tokens_per_pass"], summary["tokens"]["tokens_per_run_mean"])
        self.assertIsNone(summary["latency_ms"]["first_token"]["p50"])
        self.assertGreater(summary["memory_mb"]["server_peak"], 1)
        text = bionic_bench.format_summary(report)
        self.assertIn("6/8 (75%)", text)
        self.assertIn("session-recall", text)
        json.dumps(report)  # serializable

    def test_stream_mode_records_first_token(self) -> None:
        def handler(body: dict[str, Any], _req: Any) -> tuple[int, Any]:
            return 200, _SSEReply.chunks([{"role": "assistant"}, {"content": "ZEBRA-"}, {"content": "4417"}])

        self.server.chat_handler = handler
        report = bionic_bench.run_suite([_scenario("read-answer")], self._config(stream=True))
        run = report["runs"][0]
        self.assertTrue(run["passed"])
        self.assertEqual(len(run["latency"]["ttft_ms"]), 1)
        self.assertFalse(run["tokens"]["reported"])
        self.assertIsNone(run["tokens"]["total_tokens"])
        self.assertIsNone(report["summary"]["tokens"]["tokens_per_pass"])

    def test_max_turns_failure_keeps_partial_work(self) -> None:
        def loop(body: dict[str, Any], _req: Any) -> tuple[int, Any]:
            return 200, _tool("read_file", {"path": "secret.txt"}, _usage(50, 5))

        self.server.chat_handler = loop
        report = bionic_bench.run_suite([_scenario("read-answer")], self._config(max_turns=3))
        run = report["runs"][0]
        self.assertFalse(run["passed"])
        self.assertIn("max_turns=3", run["error"])
        self.assertEqual((run["turns"], run["tool_calls"]), (3, 3))
        self.assertEqual(run["tokens"]["total_tokens"], 165)
        self.assertEqual(run["tokens"]["prompt_tokens_per_turn"], [50, 50, 50])
        self.assertEqual(report["summary"]["tokens"]["total_tokens"], 165)
        self.assertIsNone(report["summary"]["tokens"]["tokens_per_pass"])

    def test_run_agent_max_turns_error_carries_usage(self) -> None:
        self.server.chat_handler = lambda body, req: (200, _tool("read_file", {"path": "x"}, _usage(10, 1)))
        with self.assertRaises(bionic.MaxTurnsError) as ctx:
            bionic.run_agent("go", self._config(max_turns=2))
        err = ctx.exception
        self.assertIsInstance(err, BionicError)
        self.assertEqual((err.turns, err.tool_calls, err.usage["total_tokens"], len(err.turn_usage)), (2, 2, 22, 2))

    def test_runner_error_is_recorded_and_suite_continues(self) -> None:
        calls = {"n": 0}

        def flaky(prompt: str, config: Any, **_kw: Any) -> RunResult:
            calls["n"] += 1
            if calls["n"] == 1:
                raise BionicError("Reached max_turns=3 without a final response")
            Path(config.cwd, "hello.txt").write_text("bionic bench ok")
            return RunResult(content="ok", model="m1", turns=1, tool_calls=0)

        report = bionic_bench.run_suite([_scenario("write-file")], self._config(), repeat=2, runner=flaky)
        first, second = report["runs"]
        self.assertFalse(first["passed"])
        self.assertIn("max_turns", first["error"])
        self.assertEqual(first["checks"], [])
        self.assertTrue(second["passed"])

    def test_each_run_gets_a_fresh_jailed_workspace(self) -> None:
        seen: list[tuple[str, str, str]] = []

        def spy(prompt: str, config: Any, **_kw: Any) -> RunResult:
            seen.append((config.cwd, config.workspace_root, config.session_id))
            self.assertFalse(config.auto_pull)
            self.assertTrue((Path(config.cwd) / "secret.txt").is_file())
            return RunResult(content="ZEBRA-4417", model="m1", turns=1, tool_calls=0)

        report = bionic_bench.run_suite([_scenario("read-answer")], self._config(auto_pull=True), repeat=2, runner=spy, keep_workspace=True)
        self.assertEqual(len({s[0] for s in seen}), 2)
        self.assertTrue(all(cwd == root for cwd, root, _ in seen))
        self.assertEqual(len({s[2] for s in seen}), 2)
        for run in report["runs"]:
            self.assertTrue(Path(run["workspace"]).is_dir())
            bionic_bench._rmtree(str(Path(run["workspace"]).parent))


class CliTests(_ServerCase):
    def setUp(self) -> None:
        super().setUp()
        self.server.chat_handler = _scripted_model

    def _main(self, *argv: str) -> tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = bionic_bench.main(list(argv))
        return code, out.getvalue(), err.getvalue()

    def test_list_scenarios(self) -> None:
        code, out, _ = self._main("--list-scenarios", "--tag", "multi-turn")
        self.assertEqual(code, 0)
        self.assertIn("session-recall", out)
        self.assertNotIn("write-file", out)

    def test_run_writes_report_and_summary(self) -> None:
        output = Path(self.tmp.name) / "report.json"
        code, out, err = self._main(
            "--base-url", self.base_url, "--model", "auto", "--scenario", "read-answer",
            "--scenario", "session-recall", "--output", str(output), "--min-pass-rate", "1",
        )
        self.assertEqual(code, 0, err)
        self.assertEqual(out, "")
        report = json.loads(output.read_text(encoding="utf-8"))
        self.assertTrue(report["ok"])
        self.assertEqual(report["environment"]["model"], "qwen/qwen3.8-27b")
        self.assertTrue(report["environment"]["health"]["ok"])
        self.assertEqual(report["summary"]["passed"], 2)
        self.assertIn("pass rate      2/2", err)
        # warm-up probe ran before the scenarios
        first_chat = next(r for r in self.server.requests if r["path"] == "/v1/chat/completions")
        self.assertEqual(first_chat["body"]["max_tokens"], 1)

    def test_min_pass_rate_failure_exits_3(self) -> None:
        code, out, err = self._main("--base-url", self.base_url, "--model", "google/gemma-4-26b-a4b-qat", "--scenario", "edit-config", "--min-pass-rate", "0.5", "--no-warmup", "-q")
        self.assertEqual(code, 3)
        self.assertFalse(json.loads(out)["ok"])
        self.assertEqual(err, "")
        self.assertFalse(any(r["body"] and r["body"].get("max_tokens") == 1 for r in self.server.requests if r["method"] == "POST"))

    def test_unreachable_server_exits_2(self) -> None:
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            port = s.getsockname()[1]
        code, out, err = self._main("--base-url", f"http://127.0.0.1:{port}/v1", "--model", "m1", "--timeout", "2")
        self.assertEqual(code, 2)
        self.assertFalse(json.loads(out)["health"]["ok"])
        self.assertIn("model not ready", err)

    def test_config_errors_exit_1(self) -> None:
        self.assertEqual(self._main("--base-url", self.base_url, "--scenario", "read-answer")[0], 1)
        self.assertEqual(self._main("--base-url", self.base_url, "--model", "m1", "--repeat", "0")[0], 1)
        self.assertEqual(self._main("--base-url", self.base_url, "--model", "m1", "--min-pass-rate", "2")[0], 1)
        code, _, err = self._main("--scenario", "nope", "--list-scenarios")
        self.assertEqual(code, 1)
        self.assertIn("unknown scenario", err)


if __name__ == "__main__":
    unittest.main()
