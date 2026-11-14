"""Bionic prompt-prefix caching hints, KV reuse accounting and cache pre-warming."""

from __future__ import annotations

import argparse
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from typing import Any
from unittest import mock

from aircd import bionic, bionic_cache
from aircd.bionic_cache import (
    apply_cache_hints,
    cache_key_for,
    cached_tokens,
    is_local_base_url,
    prefix_fingerprint,
    resolve_mode,
)
from tests.test_bionic import _FakeBionicServer, _SSEReply

CLI_PY_ROOT = Path(__file__).resolve().parents[1]
LOCAL = "http://127.0.0.1:1235/v1"


def _ns(**kwargs: Any) -> argparse.Namespace:
    return argparse.Namespace(**kwargs)


def _clean_env() -> dict[str, str]:
    return {k: v for k, v in os.environ.items() if not k.startswith(("BIONIC_", "AIRCD_", "AIRC_"))}


class ModeTests(unittest.TestCase):
    def test_local_detection(self) -> None:
        for url in (
            "http://localhost:1235/v1",
            "http://127.0.0.1:1234/v1",
            "http://[::1]:8080/v1",
            "http://192.168.1.20:1234/v1",
            "http://10.0.0.5/v1",
            "http://studio.local:1234/v1",
            "http://gpu-box.lan/v1",
        ):
            self.assertTrue(is_local_base_url(url), url)
        for url in ("https://api.openai.com/v1", "https://openrouter.ai/api/v1", "http://8.8.8.8/v1", ""):
            self.assertFalse(is_local_base_url(url), url)

    def test_auto_resolves_per_server(self) -> None:
        self.assertEqual(resolve_mode("auto", LOCAL), "local")
        self.assertEqual(resolve_mode("auto", "https://api.openai.com/v1"), "openai")
        self.assertEqual(resolve_mode("auto", "https://openrouter.ai/api/v1"), "off")
        self.assertEqual(resolve_mode("anthropic", LOCAL), "anthropic")
        self.assertEqual(resolve_mode("", LOCAL), "local")


class HintTests(unittest.TestCase):
    def payload(self) -> dict[str, Any]:
        return {
            "model": "m",
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "do it"},
                {"role": "assistant", "content": "", "tool_calls": [{"id": "c1"}]},
                {"role": "tool", "tool_call_id": "c1", "content": "result"},
            ],
            "tools": [],
        }

    def test_off_returns_payload_unchanged(self) -> None:
        payload = self.payload()
        self.assertIs(apply_cache_hints(payload, "off", base_url=LOCAL, cache_key="k", ttl=60), payload)
        self.assertIs(apply_cache_hints(payload, "auto", base_url="https://openrouter.ai/api/v1", cache_key="k"), payload)

    def test_local_auto_adds_cache_prompt_and_key(self) -> None:
        payload = self.payload()
        out = apply_cache_hints(payload, "auto", base_url=LOCAL, cache_key="bionic-abc", ttl=900)
        self.assertIs(out["cache_prompt"], True)
        self.assertEqual(out["prompt_cache_key"], "bionic-abc")
        self.assertEqual(out["ttl"], 900)
        self.assertNotIn("id_slot", out)
        self.assertNotIn("cache_prompt", payload)  # the input is not modified
        self.assertIs(out["messages"], payload["messages"])

    def test_llamacpp_slot_and_openai_key(self) -> None:
        out = apply_cache_hints(self.payload(), "llamacpp", base_url=LOCAL, cache_key="k", slot=2)
        self.assertEqual((out["cache_prompt"], out["id_slot"]), (True, 2))
        self.assertNotIn("prompt_cache_key", out)
        out = apply_cache_hints(self.payload(), "openai", base_url="https://api.openai.com/v1", cache_key="k", slot=2)
        self.assertEqual(out["prompt_cache_key"], "k")
        self.assertNotIn("cache_prompt", out)
        self.assertNotIn("id_slot", out)
        self.assertNotIn("ttl", out)

    def test_anthropic_breakpoints_leave_history_plain(self) -> None:
        payload = self.payload()
        snapshot = json.dumps(payload)
        out = apply_cache_hints(payload, "anthropic", base_url=LOCAL)
        self.assertEqual(json.dumps(payload), snapshot)
        system, last = out["messages"][0], out["messages"][-1]
        self.assertEqual(system["content"], [{"type": "text", "text": "sys", "cache_control": {"type": "ephemeral"}}])
        self.assertEqual(last["content"][0]["cache_control"], {"type": "ephemeral"})
        self.assertEqual(last["tool_call_id"], "c1")
        # Only the system prompt and the newest user/tool turn are marked.
        self.assertEqual(out["messages"][1]["content"], "do it")
        self.assertNotIn("cache_prompt", out)

    def test_anthropic_marks_last_text_part_of_list_content(self) -> None:
        payload = {
            "messages": [
                {"role": "system", "content": "s"},
                {"role": "user", "content": [{"type": "text", "text": "a"}, {"type": "image_url", "image_url": {}}]},
            ]
        }
        out = apply_cache_hints(payload, "anthropic", base_url=LOCAL)
        self.assertEqual(out["messages"][1]["content"][0]["cache_control"], {"type": "ephemeral"})
        self.assertNotIn("cache_control", payload["messages"][1]["content"][0])

    def test_cache_key_is_stable_and_prefix_sensitive(self) -> None:
        tools = [{"type": "function", "function": {"name": "a", "parameters": {"b": 1, "a": 2}}}]
        reordered = [{"function": {"parameters": {"a": 2, "b": 1}, "name": "a"}, "type": "function"}]
        key = cache_key_for("m", "sys", tools)
        self.assertTrue(key.startswith("bionic-"))
        self.assertEqual(key, cache_key_for("m", "sys", reordered))
        self.assertNotEqual(key, cache_key_for("m2", "sys", tools))
        self.assertNotEqual(key, cache_key_for("m", "sys2", tools))
        self.assertNotEqual(prefix_fingerprint("sys", tools), prefix_fingerprint("sys", []))


class CachedTokenTests(unittest.TestCase):
    def test_sources(self) -> None:
        self.assertEqual(cached_tokens({"prompt_tokens": 9, "prompt_tokens_details": {"cached_tokens": 7}}), 7)
        self.assertEqual(cached_tokens({"cache_read_input_tokens": 5}), 5)
        self.assertEqual(cached_tokens({"prompt_tokens": 9}, {"cache_n": 4, "prompt_n": 5}), 4)
        self.assertIsNone(cached_tokens({"prompt_tokens": 9}))
        self.assertIsNone(cached_tokens(None, None))
        self.assertIsNone(cached_tokens({"cached_tokens": True}))

    def test_usage_counts_include_cached_tokens_only_when_reported(self) -> None:
        self.assertEqual(
            bionic._usage_counts({"prompt_tokens": 10, "completion_tokens": 2}),
            {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12},
        )
        self.assertEqual(
            bionic._usage_counts({"prompt_tokens": 10, "completion_tokens": 2}, {"cache_n": 8})["cached_tokens"], 8
        )
        self.assertIsNone(bionic._usage_counts({}, {"cache_n": 8}))

    def test_stream_keeps_timings(self) -> None:
        events = [
            json.dumps({"choices": [{"delta": {"content": "hi"}}]}),
            json.dumps({"choices": [], "usage": {"prompt_tokens": 3, "completion_tokens": 1}, "timings": {"cache_n": 2}}),
            "[DONE]",
        ]
        msg = bionic.accumulate_stream(events)
        self.assertEqual(msg["timings"], {"cache_n": 2})
        self.assertNotIn("timings", bionic._normalize_assistant_message(msg))


class ConfigTests(unittest.TestCase):
    def resolve(self, env: dict[str, str], **args: Any) -> bionic.BionicConfig:
        with tempfile.TemporaryDirectory() as tmp, mock.patch.dict(os.environ, {**_clean_env(), **env}, clear=True):
            return bionic.resolve_config(_ns(cwd=tmp, **args))

    def test_defaults(self) -> None:
        config = self.resolve({})
        self.assertEqual((config.prompt_cache, config.cache_ttl, config.cache_slot), ("auto", None, None))

    def test_env_and_cli_precedence(self) -> None:
        env = {"BIONIC_PROMPT_CACHE": "OpenAI", "BIONIC_CACHE_TTL": "600", "BIONIC_CACHE_SLOT": "0"}
        config = self.resolve(env)
        self.assertEqual((config.prompt_cache, config.cache_ttl, config.cache_slot), ("openai", 600, 0))
        config = self.resolve(env, prompt_cache="off", cache_ttl=30, cache_slot=3)
        self.assertEqual((config.prompt_cache, config.cache_ttl, config.cache_slot), ("off", 30, 3))

    def test_invalid_values(self) -> None:
        with self.assertRaises(bionic.BionicError):
            self.resolve({"BIONIC_PROMPT_CACHE": "redis"})
        with self.assertRaises(bionic.BionicError):
            self.resolve({"BIONIC_CACHE_TTL": "0"})
        with self.assertRaises(bionic.BionicError):
            self.resolve({}, cache_slot=-1)

    def test_cli_flags_parse(self) -> None:
        args = bionic.build_parser().parse_args(
            ["--prompt-cache", "llamacpp", "--cache-ttl", "120", "--cache-slot", "1", "--warm-cache"]
        )
        self.assertEqual((args.prompt_cache, args.cache_ttl, args.cache_slot, args.warm_cache), ("llamacpp", 120, 1, True))


class FakeServerCase(unittest.TestCase):
    def setUp(self) -> None:
        self.server = _FakeBionicServer()
        import threading

        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.cwd = Path(self.tmp.name).resolve()
        self.base_url = f"http://127.0.0.1:{self.server.server_address[1]}/v1"
        env = mock.patch.dict(os.environ, _clean_env(), clear=True)
        env.start()
        self.addCleanup(env.stop)

    def config(self, **overrides: Any) -> bionic.BionicConfig:
        values = dict(
            base_url=self.base_url,
            model="qwen/qwen3.8-27b",
            cwd=str(self.cwd),
            workspace_root=str(self.cwd),
            report_usage=False,
        )
        values.update(overrides)
        return bionic.BionicConfig(**values)

    def chat_bodies(self) -> list[dict[str, Any]]:
        return [r["body"] for r in self.server.requests if r["path"] == "/v1/chat/completions"]


def _reply(message: dict[str, Any], usage: dict[str, Any] | None = None, **extra: Any) -> tuple[int, Any]:
    body: dict[str, Any] = {"choices": [{"index": 0, "message": message}], **extra}
    if usage is not None:
        body["usage"] = usage
    return 200, body


def _tool_call(call_id: str, path: str) -> dict[str, Any]:
    return {
        "role": "assistant",
        "content": "",
        "tool_calls": [
            {"id": call_id, "type": "function", "function": {"name": "read_file", "arguments": json.dumps({"path": path})}}
        ],
    }


class RunAgentCacheTests(FakeServerCase):
    def test_prefix_is_byte_stable_and_hints_sent_every_turn(self) -> None:
        (self.cwd / "a.txt").write_text("alpha", encoding="utf-8")
        self.server.chat_replies = [
            _reply(_tool_call("c1", "a.txt"), {"prompt_tokens": 100, "completion_tokens": 5}),
            _reply(
                {"role": "assistant", "content": "done"},
                {"prompt_tokens": 140, "completion_tokens": 3, "prompt_tokens_details": {"cached_tokens": 96}},
            ),
        ]
        events: list[dict[str, Any]] = []
        logs: list[str] = []
        result = bionic.run_agent("read a.txt", self.config(cache_ttl=300), on_event=events.append, log=logs.append)
        self.assertEqual(result.content, "done")
        first, second = self.chat_bodies()
        for body in (first, second):
            self.assertIs(body["cache_prompt"], True)
            self.assertEqual(body["ttl"], 300)
            self.assertTrue(body["prompt_cache_key"].startswith("bionic-"))
        self.assertEqual(first["prompt_cache_key"], second["prompt_cache_key"])
        # Turn 2 starts with exactly the bytes turn 1 sent: system prompt, tools, user turn.
        self.assertEqual(json.dumps(second["tools"]), json.dumps(first["tools"]))
        prefix = json.dumps(first["messages"])[:-1]
        self.assertTrue(json.dumps(second["messages"]).startswith(prefix))
        # Plain string content (no cache_control parts) in local mode.
        self.assertIsInstance(second["messages"][0]["content"], str)
        self.assertEqual(result.usage["cached_tokens"], 96)
        self.assertEqual(result.turn_usage[0].get("cached_tokens"), None)
        cache_events = [e for e in events if e["type"] == "cache"]
        self.assertEqual(cache_events, [{"type": "cache", "turn": 2, "cached_tokens": 96, "prompt_tokens": 140}])
        self.assertTrue(any("96/140 prompt tokens reused" in line for line in logs))

    def test_key_shared_across_runs_and_changes_with_prefix(self) -> None:
        self.server.chat_replies = [_reply({"role": "assistant", "content": "x"}) for _ in range(3)]
        bionic.run_agent("one", self.config())
        bionic.run_agent("two", self.config())
        bionic.run_agent("three", self.config(approval="read-only"))
        keys = [b["prompt_cache_key"] for b in self.chat_bodies()]
        self.assertEqual(keys[0], keys[1])
        self.assertNotEqual(keys[0], keys[2])

    def test_off_sends_no_hints(self) -> None:
        self.server.chat_replies = [_reply({"role": "assistant", "content": "x"})]
        bionic.run_agent("hi", self.config(prompt_cache="off", cache_ttl=60))
        body = self.chat_bodies()[0]
        for key in ("cache_prompt", "prompt_cache_key", "ttl", "id_slot"):
            self.assertNotIn(key, body)

    def test_anthropic_mode_does_not_leak_into_saved_session(self) -> None:
        (self.cwd / "a.txt").write_text("alpha", encoding="utf-8")
        self.server.chat_replies = [
            _reply(_tool_call("c1", "a.txt")),
            _reply({"role": "assistant", "content": "done"}),
        ]
        sessions = self.cwd / "sessions"
        config = self.config(prompt_cache="anthropic", session_id="s1", session_dir=str(sessions))
        bionic.run_agent("read a.txt", config)
        second = self.chat_bodies()[1]
        self.assertEqual(second["messages"][0]["content"][0]["cache_control"], {"type": "ephemeral"})
        self.assertEqual(second["messages"][-1]["role"], "tool")
        self.assertEqual(second["messages"][-1]["content"][0]["cache_control"], {"type": "ephemeral"})
        saved = json.loads((sessions / "s1.json").read_text(encoding="utf-8"))
        self.assertNotIn("cache_control", json.dumps(saved))
        self.assertIsInstance(saved[0]["content"], str)

    def test_llamacpp_timings_counted_streaming(self) -> None:
        chunk = {"choices": [{"index": 0, "delta": {"content": "ok"}}]}
        tail = {"choices": [], "usage": {"prompt_tokens": 50, "completion_tokens": 1}, "timings": {"cache_n": 42}}
        self.server.chat_replies = [
            (200, _SSEReply(["data: " + json.dumps(chunk) + "\n\n", "data: " + json.dumps(tail) + "\n\n", "data: [DONE]\n\n"]))
        ]
        result = bionic.run_agent("hi", self.config(stream=True, prompt_cache="llamacpp", cache_slot=1))
        body = self.chat_bodies()[0]
        self.assertEqual((body["cache_prompt"], body["id_slot"]), (True, 1))
        self.assertNotIn("prompt_cache_key", body)
        self.assertEqual(result.usage["cached_tokens"], 42)

    def test_changed_system_prompt_is_logged_on_resume(self) -> None:
        self.server.chat_replies = [_reply({"role": "assistant", "content": "x"}) for _ in range(3)]
        sessions = self.cwd / "sessions"
        bionic.run_agent("one", self.config(session_id="s", session_dir=str(sessions)))
        logs: list[str] = []
        bionic.run_agent("two", self.config(session_id="s", session_dir=str(sessions)), log=logs.append)
        self.assertFalse(any("system prompt changed" in line for line in logs))
        bionic.run_agent(
            "three", self.config(session_id="s", session_dir=str(sessions), system_prompt="new"), log=logs.append
        )
        self.assertTrue(any("system prompt changed" in line for line in logs))


class PrewarmTests(FakeServerCase):
    def test_prewarm_sends_the_run_prefix(self) -> None:
        self.server.chat_replies = [
            _reply({"role": "assistant", "content": "R"}, {"prompt_tokens": 812, "completion_tokens": 1}),
            _reply({"role": "assistant", "content": "done"}),
        ]
        config = self.config(cache_ttl=900)
        report = bionic_cache.prewarm(config)
        self.assertTrue(report["ok"], report)
        self.assertEqual(report["prompt_tokens"], 812)
        self.assertIsNone(report["cached_tokens"])
        self.assertEqual(report["mode"], "local")
        bionic.run_agent("real work", config)
        warm, run = self.chat_bodies()
        self.assertEqual(warm["max_tokens"], 1)
        self.assertEqual(warm["ttl"], 900)
        self.assertEqual(warm["prompt_cache_key"], run["prompt_cache_key"])
        self.assertEqual(report["cache_key"], run["prompt_cache_key"])
        self.assertEqual(warm["messages"][0], run["messages"][0])
        self.assertEqual(json.dumps(warm["tools"]), json.dumps(run["tools"]))

    def test_prewarm_reports_failure(self) -> None:
        self.server.chat_replies = [(500, {"error": {"message": "model crashed"}})]
        report = bionic_cache.prewarm(self.config())
        self.assertFalse(report["ok"])
        self.assertIn("model crashed", report["error"])

    def test_sdk_warm(self) -> None:
        self.server.chat_replies = [_reply({"role": "assistant", "content": "R"}, {"prompt_tokens": 5, "completion_tokens": 1})]
        agent = bionic.BionicAgent(base_url=self.base_url, model="auto", cwd=str(self.cwd), report_usage=False)
        report = agent.warm()
        self.assertTrue(report["ok"])
        self.assertEqual(report["model"], "qwen/qwen3.8-27b")

    def test_cli_warm_cache(self) -> None:
        self.server.chat_replies = [_reply({"role": "assistant", "content": "R"}, {"prompt_tokens": 7, "completion_tokens": 1})]
        out, err = io.StringIO(), io.StringIO()
        with mock.patch("sys.stdout", out), mock.patch("sys.stderr", err):
            code = bionic.main(["--warm-cache", "--base-url", self.base_url, "--model", "auto", "--cwd", str(self.cwd)])
        self.assertEqual(code, 0, err.getvalue())
        report = json.loads(out.getvalue())
        self.assertTrue(report["ok"])
        self.assertEqual(report["prompt_tokens"], 7)

    def test_cli_warm_cache_failure_and_prompt_conflict(self) -> None:
        self.server.chat_replies = [(500, {"error": {"message": "nope"}})]
        out, err = io.StringIO(), io.StringIO()
        with mock.patch("sys.stdout", out), mock.patch("sys.stderr", err):
            code = bionic.main(["--warm-cache", "--base-url", self.base_url, "--model", "m", "--cwd", str(self.cwd)])
        self.assertEqual(code, 2)
        self.assertIn("nope", err.getvalue())
        with mock.patch("sys.stderr", io.StringIO()), self.assertRaises(SystemExit):
            bionic.main(["--warm-cache", "-p", "x", "--base-url", self.base_url, "--model", "m"])

    def test_module_entrypoint_subprocess(self) -> None:
        self.server.chat_replies = [_reply({"role": "assistant", "content": "R"}, {"prompt_tokens": 3, "completion_tokens": 1})]
        env = {**_clean_env(), "PYTHONPATH": str(CLI_PY_ROOT), "PYTHONUTF8": "1"}
        proc = subprocess.run(
            [sys.executable, "-m", "aircd.bionic", "--warm-cache", "--base-url", self.base_url, "--model", "m", "--cwd", str(self.cwd)],
            capture_output=True,
            text=True,
            env=env,
            timeout=60,
        )
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertTrue(json.loads(proc.stdout)["ok"])


class ReplWarmTests(FakeServerCase):
    def test_repl_warms_in_background_before_first_prompt(self) -> None:
        from aircd.bionic_repl import BionicRepl

        self.server.chat_replies = [
            _reply({"role": "assistant", "content": "R"}, {"prompt_tokens": 9, "completion_tokens": 1}),
            _reply(
                {"role": "assistant", "content": "hello"},
                {"prompt_tokens": 12, "completion_tokens": 1, "prompt_tokens_details": {"cached_tokens": 9}},
            ),
        ]
        lines = iter(["hi", "/usage", "/exit"])
        out = io.StringIO()
        repl = BionicRepl(self.config(), input_fn=lambda _p: next(lines), out=out)
        self.assertEqual(repl.loop(warm_cache=True), 0)
        warm, run = self.chat_bodies()
        self.assertEqual(warm["max_tokens"], 1)
        self.assertEqual(warm["prompt_cache_key"], run["prompt_cache_key"])
        self.assertTrue(repl.warm_report["ok"])
        self.assertIn('"cached_tokens": 9', out.getvalue())

    def test_warm_command_and_missing_model(self) -> None:
        from aircd.bionic_repl import BionicRepl

        self.server.chat_replies = [_reply({"role": "assistant", "content": "R"}, {"prompt_tokens": 4, "completion_tokens": 1})]
        out = io.StringIO()
        repl = BionicRepl(self.config(), input_fn=lambda _p: "", out=out)
        repl.handle_command("/warm")
        self.assertIn("pre-warmed 4 prompt tokens", out.getvalue())
        out2 = io.StringIO()
        repl2 = BionicRepl(self.config(model=""), input_fn=lambda _p: "", out=out2)
        repl2.start_warm()
        self.assertIn("no model set", out2.getvalue())
        self.assertIsNone(repl2._warm_thread)
        repl2.handle_command("/warm")
        self.assertIn("pre-warm failed", out2.getvalue())


if __name__ == "__main__":
    unittest.main()
