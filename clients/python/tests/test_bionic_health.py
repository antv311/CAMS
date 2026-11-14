"""Tests for Bionic model readiness checks and auto-pull (`airc-bionic --health` / `--auto-pull`)."""

from __future__ import annotations

import io
import json
import os
import sys
import tempfile
import threading
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from typing import Any
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
sys.path.insert(0, str(Path(__file__).resolve().parent))

from aircd import BionicAgent, bionic  # noqa: E402
from test_bionic import _FakeBionicHandler, _FakeBionicServer  # noqa: E402

CHAT_OK = (200, {"choices": [{"message": {"role": "assistant", "content": "OK"}}]})


class _V0Handler(_FakeBionicHandler):
    """Adds LM Studio's ``/api/v0/models`` (with load ``state``) to the fake server."""

    def do_GET(self) -> None:
        if self.path == "/api/v0/models" and self.server.v0_models is not None:
            self.server.requests.append({"method": "GET", "path": self.path, "headers": {}, "body": None})
            self._send_json(200, {"object": "list", "data": self.server.v0_models})
            return
        super().do_GET()


class _V0Server(_FakeBionicServer):
    def __init__(self) -> None:
        super().__init__()
        self.RequestHandlerClass = _V0Handler
        self.v0_models: list[dict[str, Any]] | None = [
            {"id": "qwen/qwen3.8-27b", "type": "llm", "state": "not-loaded"},
            {"id": "text-embedding-nomic-embed-text-v1.5", "type": "embeddings", "state": "not-loaded"},
            {"id": "google/gemma-4-26b-a4b-qat", "type": "llm", "state": "loaded"},
        ]

    def set_state(self, model: str, state: str) -> None:
        assert self.v0_models is not None
        for entry in self.v0_models:
            if entry["id"] == model:
                entry["state"] = state
                return
        self.v0_models.append({"id": model, "type": "llm", "state": state})
        self.models_reply[1]["data"].append({"id": model, "type": "llm"})


class BionicHealthTests(unittest.TestCase):
    def setUp(self) -> None:
        self.server = _V0Server()
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        self.base_url = f"http://127.0.0.1:{self.server.server_address[1]}/v1"
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        env = mock.patch.dict(os.environ, {}, clear=False)
        env.start()
        self.addCleanup(env.stop)
        for name in ("BIONIC_AUTO_PULL", "BIONIC_LMS", "BIONIC_PULL_TIMEOUT", "BIONIC_MODEL"):
            os.environ.pop(name, None)

    def config(self, **kw: Any) -> bionic.BionicConfig:
        kw.setdefault("base_url", self.base_url)
        kw.setdefault("cwd", self.tmp.name)
        kw.setdefault("timeout", 10.0)
        return bionic.BionicConfig(**kw)

    def chat_paths(self) -> list[dict[str, Any]]:
        return [r for r in self.server.requests if r["path"] == "/v1/chat/completions"]

    def fake_lms(self, calls: list[list[str]], *, fail: str = ""):
        def run(lms: str, args: list[str], timeout: float) -> tuple[bool, str]:
            calls.append(args)
            if args[0] == fail:
                return False, f"{fail} exploded"
            if args[0] == "get":
                self.server.set_state(args[1], "not-loaded")
            elif args[0] == "load":
                self.server.set_state(args[1], "loaded")
            return True, f"{args[0]} done"

        return run

    # --- config -------------------------------------------------------------------------------

    def test_config_flags_and_env(self) -> None:
        parser = bionic.build_parser()
        cfg = bionic.resolve_config(parser.parse_args(["--cwd", self.tmp.name]))
        self.assertFalse(cfg.auto_pull)
        self.assertEqual(cfg.pull_timeout, bionic.DEFAULT_PULL_TIMEOUT_SECONDS)
        with mock.patch.dict(
            os.environ, {"BIONIC_AUTO_PULL": "1", "BIONIC_LMS": "C:/lms.exe", "BIONIC_PULL_TIMEOUT": "60"}
        ):
            cfg = bionic.resolve_config(parser.parse_args(["--cwd", self.tmp.name]))
            self.assertTrue(cfg.auto_pull)
            self.assertEqual((cfg.lms_path, cfg.pull_timeout), ("C:/lms.exe", 60.0))
            cfg = bionic.resolve_config(
                parser.parse_args(["--cwd", self.tmp.name, "--lms", "other", "--pull-timeout", "5"])
            )
            self.assertEqual((cfg.lms_path, cfg.pull_timeout), ("other", 5.0))
        with self.assertRaises(bionic.BionicError):
            bionic.resolve_config(parser.parse_args(["--cwd", self.tmp.name, "--pull-timeout", "0"]))
        self.assertEqual(bionic.server_root("http://h:1235/v1/"), "http://h:1235")
        self.assertEqual(bionic.server_root("h:9/api"), "http://h:9/api")

    # --- check_health -------------------------------------------------------------------------

    def test_loaded_model_is_ready_and_probed(self) -> None:
        self.server.chat_replies = [CHAT_OK]
        report = bionic.check_health(self.config(model="google/gemma-4-26b-a4b-qat"))
        self.assertTrue(report["ok"], report)
        self.assertEqual(report["state"], "loaded")
        self.assertTrue(report["probe"]["ok"])
        body = self.chat_paths()[0]["body"]
        self.assertEqual((body["model"], body["max_tokens"]), ("google/gemma-4-26b-a4b-qat", 1))
        json.dumps(report)

    def test_not_loaded_without_probe_is_not_ready(self) -> None:
        report = bionic.check_health(self.config(model="qwen/qwen3.8-27b"), probe=False)
        self.assertFalse(report["ok"])
        self.assertEqual(report["state"], "not-loaded")
        self.assertIn("--auto-pull", report["error"])
        self.assertEqual(self.chat_paths(), [])

    def test_auto_model_prefers_loaded(self) -> None:
        report = bionic.check_health(self.config(model="auto"), probe=False)
        self.assertTrue(report["ok"], report)
        self.assertEqual(report["model"], "google/gemma-4-26b-a4b-qat")

    def test_missing_model_and_unreachable_server(self) -> None:
        report = bionic.check_health(self.config(model="nope/model"), probe=False)
        self.assertFalse(report["ok"])
        self.assertEqual((report["available"], report["state"]), (False, "missing"))
        self.assertIn("not available", report["error"])

        dead = bionic.check_health(self.config(model="x", base_url="http://127.0.0.1:1/v1", timeout=2.0))
        self.assertFalse(dead["ok"])
        self.assertFalse(dead["server"]["reachable"])
        self.assertIn("Could not connect", dead["error"])

    def test_no_model_reports_server_only(self) -> None:
        report = bionic.check_health(self.config(model=""))
        self.assertTrue(report["ok"])
        self.assertEqual(report["available_models"], ["qwen/qwen3.8-27b", "google/gemma-4-26b-a4b-qat"])

    def test_plain_openai_server_falls_back_to_probe(self) -> None:
        self.server.v0_models = None
        self.server.chat_replies = [(500, {"error": {"message": "model crashed"}})]
        report = bionic.check_health(self.config(model="qwen/qwen3.8-27b"), probe=None)
        self.assertEqual(report["state"], "unknown")
        self.assertFalse(report["ok"])
        self.assertIn("model crashed", report["error"])

        self.server.chat_replies = [CHAT_OK]
        self.assertTrue(bionic.check_health(self.config(model="qwen/qwen3.8-27b"), probe=None)["ok"])

    def test_auto_pull_loads_not_loaded_model(self) -> None:
        calls: list[list[str]] = []
        with mock.patch.object(bionic, "find_lms", return_value="lms"), mock.patch.object(
            bionic, "_run_lms", side_effect=self.fake_lms(calls)
        ):
            report = bionic.check_health(self.config(model="qwen/qwen3.8-27b"), probe=None, auto_pull=True)
        self.assertTrue(report["ok"], report)
        self.assertEqual(calls, [["load", "qwen/qwen3.8-27b", "--yes"]])
        self.assertEqual(report["state"], "loaded")
        self.assertIsNone(report["probe"])  # loaded state confirmed, no probe needed

    def test_auto_pull_downloads_missing_model(self) -> None:
        calls: list[list[str]] = []
        with mock.patch.object(bionic, "find_lms", return_value="lms"), mock.patch.object(
            bionic, "_run_lms", side_effect=self.fake_lms(calls)
        ):
            report = bionic.check_health(self.config(model="new/model", auto_pull=True), probe=False)
        self.assertTrue(report["ok"], report)
        self.assertEqual([c[0] for c in calls], ["get", "load"])
        self.assertEqual([a["action"] for a in report["actions"]], ["get", "load"])

    def test_auto_pull_failures(self) -> None:
        calls: list[list[str]] = []
        with mock.patch.object(bionic, "find_lms", return_value="lms"), mock.patch.object(
            bionic, "_run_lms", side_effect=self.fake_lms(calls, fail="get")
        ):
            report = bionic.check_health(self.config(model="new/model"), auto_pull=True)
        self.assertFalse(report["ok"])
        self.assertIn("lms get new/model failed: get exploded", report["error"])
        self.assertEqual(len(calls), 1)

        with mock.patch.object(bionic, "find_lms", return_value=None):
            report = bionic.check_health(self.config(model="qwen/qwen3.8-27b"), probe=False, auto_pull=True)
        self.assertFalse(report["ok"])
        self.assertIn("lms CLI not found", report["actions"][0]["detail"])

    def test_real_lms_subprocess(self) -> None:
        """Run a stand-in ``lms`` executable to cover the subprocess path end to end."""
        log = Path(self.tmp.name) / "lms-calls.txt"
        script = Path(self.tmp.name) / "fake_lms.py"
        script.write_text(
            "import sys\n"
            f"open({str(log)!r}, 'a').write(' '.join(sys.argv[1:]) + '\\n')\n"
            "print('fake lms ok')\n",
            encoding="utf-8",
        )
        if os.name == "nt":
            lms = Path(self.tmp.name) / "lms.cmd"
            lms.write_text(f'@"{sys.executable}" "{script}" %*\r\n', encoding="utf-8")
        else:
            lms = Path(self.tmp.name) / "lms"
            lms.write_text(f'#!/bin/sh\nexec "{sys.executable}" "{script}" "$@"\n', encoding="utf-8")
            lms.chmod(0o755)
        self.assertEqual(bionic.find_lms(str(lms)), str(lms))
        ok, out = bionic._run_lms(str(lms), ["load", "m", "--yes"], 30)
        self.assertTrue(ok)
        self.assertIn("fake lms ok", out)
        self.assertEqual(log.read_text().strip(), "load m --yes")
        self.assertIsNone(bionic.find_lms(str(Path(self.tmp.name) / "missing-lms")))

    # --- CLI and run preflight ----------------------------------------------------------------

    def test_cli_health_exit_codes(self) -> None:
        self.server.chat_replies = [CHAT_OK]
        out = io.StringIO()
        with redirect_stdout(out):
            rc = bionic.main(["--base-url", self.base_url, "--cwd", self.tmp.name, "--health", "--model", "auto"])
        self.assertEqual(rc, 0)
        self.assertTrue(json.loads(out.getvalue())["ok"])

        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            rc = bionic.main(
                ["--base-url", self.base_url, "--cwd", self.tmp.name, "--health", "--no-probe", "--model", "qwen/qwen3.8-27b"]
            )
        self.assertEqual(rc, 2)
        self.assertEqual(json.loads(out.getvalue())["state"], "not-loaded")
        self.assertIn("not loaded", err.getvalue())

    def test_run_with_auto_pull_loads_before_first_turn(self) -> None:
        calls: list[list[str]] = []
        self.server.chat_replies = [CHAT_OK]
        out = io.StringIO()
        with mock.patch.object(bionic, "find_lms", return_value="lms"), mock.patch.object(
            bionic, "_run_lms", side_effect=self.fake_lms(calls)
        ), redirect_stdout(out):
            rc = bionic.main(
                ["--base-url", self.base_url, "--cwd", self.tmp.name, "--model", "qwen/qwen3.8-27b", "--auto-pull", "-p", "hi"]
            )
        self.assertEqual(rc, 0)
        self.assertEqual(out.getvalue().strip(), "OK")
        self.assertEqual(calls, [["load", "qwen/qwen3.8-27b", "--yes"]])
        chats = self.chat_paths()
        self.assertEqual(len(chats), 1)  # no probe: state confirmed loaded
        self.assertIn("tools", chats[0]["body"])

    def test_run_with_auto_pull_fails_fast_when_not_ready(self) -> None:
        err = io.StringIO()
        with mock.patch.object(bionic, "find_lms", return_value=None), redirect_stderr(err):
            rc = bionic.main(
                ["--base-url", self.base_url, "--cwd", self.tmp.name, "--model", "nope/x", "--auto-pull", "-p", "hi"]
            )
        self.assertEqual(rc, 1)
        self.assertIn("Bionic model not ready", err.getvalue())
        self.assertEqual(self.chat_paths(), [])

    def test_sdk_health_and_auto_pull_option(self) -> None:
        agent = BionicAgent(base_url=self.base_url, cwd=self.tmp.name, model="auto", timeout=10.0)
        report = agent.health(probe=False)
        self.assertTrue(report["ok"])
        self.assertEqual(agent.model, "google/gemma-4-26b-a4b-qat")

        calls: list[list[str]] = []
        events: list[dict[str, Any]] = []
        self.server.chat_replies = [CHAT_OK]
        agent2 = agent.with_options(model="qwen/qwen3.8-27b", auto_pull=True)
        self.assertTrue(agent2.config.auto_pull)
        with mock.patch.object(bionic, "find_lms", return_value="lms"), mock.patch.object(
            bionic, "_run_lms", side_effect=self.fake_lms(calls)
        ):
            result = agent2.run("hi", on_event=events.append)
        self.assertEqual(result.content, "OK")
        self.assertEqual(events[0]["type"], "health")
        self.assertTrue(events[0]["report"]["ok"])


if __name__ == "__main__":
    unittest.main()
