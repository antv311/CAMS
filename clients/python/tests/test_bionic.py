"""Unit and end-to-end tests for the LM Studio Bionic runner (`aircd.bionic`)."""

from __future__ import annotations

import http.server
import io
import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.request
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from typing import Any
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from aircd import bionic  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parents[3]
CLI_PY_ROOT = REPO_ROOT / "clients" / "python"
OWNER_KEY = "owner-key-for-bionic-tests"


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return int(s.getsockname()[1])


def _binary_path() -> Path | None:
    override = os.environ.get("AIRCD_BIN")
    if override:
        return Path(override)
    exe = "aircd.exe" if os.name == "nt" else "aircd"
    candidates = [
        REPO_ROOT / "target" / build / exe
        for build in ("release", "debug")
        if (REPO_ROOT / "target" / build / exe).exists()
    ]
    if not candidates:
        return None
    # Pick the newest build (release wins ties), matching Get-AircdExecutable.ps1.
    return max(candidates, key=lambda p: (p.stat().st_mtime, p.parent.name == "release"))


class _FakeBionicServer(http.server.ThreadingHTTPServer):
    """Programmable OpenAI-compatible server for testing ``aircd.bionic``."""

    daemon_threads = True

    def __init__(self) -> None:
        super().__init__(("127.0.0.1", 0), _FakeBionicHandler)
        self.requests: list[dict[str, Any]] = []
        self.models_reply: tuple[int, Any] = (
            200,
            {
                "object": "list",
                "data": [
                    {"id": "qwen/qwen3.8-27b", "displayName": "Qwen3.8 27B", "type": "llm"},
                    {"id": "text-embedding-nomic-embed-text-v1.5", "type": "embeddings"},
                    {"id": "google/gemma-4-26b-a4b-qat", "type": "llm"},
                ],
            },
        )
        self.chat_handler: Any = None
        self.chat_replies: list[tuple[int, Any]] = []


class _FakeBionicHandler(http.server.BaseHTTPRequestHandler):
    server: _FakeBionicServer

    def do_GET(self) -> None:
        self.server.requests.append(
            {
                "method": "GET",
                "path": self.path,
                "headers": {k.lower(): v for k, v in self.headers.items()},
                "body": None,
            }
        )
        if self.path == "/v1/models":
            status, payload = self.server.models_reply
        else:
            status, payload = (404, {"error": {"message": f"unexpected path {self.path}"}})
        self._send_json(status, payload)

    def do_POST(self) -> None:
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b""
        body = json.loads(raw.decode("utf-8")) if raw else None
        req_record = {
            "method": "POST",
            "path": self.path,
            "headers": {k.lower(): v for k, v in self.headers.items()},
            "body": body,
        }
        self.server.requests.append(req_record)
        if self.path != "/v1/chat/completions":
            self._send_json(404, {"error": {"message": f"unexpected path {self.path}"}})
            return
        if self.server.chat_handler is not None:
            status, payload = self.server.chat_handler(body, req_record)
        elif self.server.chat_replies:
            status, payload = self.server.chat_replies.pop(0)
        else:
            status, payload = (500, {"error": {"message": "no scripted reply"}})
        self._send_json(status, payload)

    def _send_json(self, status: int, payload: Any) -> None:
        raw = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def log_message(self, *_args: Any) -> None:
        pass


class BionicConfigTests(unittest.TestCase):
    def test_defaults_and_env_precedence(self) -> None:
        parser = bionic.build_parser()
        cleared = {
            k: ""
            for k in (
                "BIONIC_BASE_URL",
                "AIRCD_BIONIC_BASE_URL",
                "OPENAI_BASE_URL",
                "BIONIC_API_KEY",
                "OPENAI_API_KEY",
                "BIONIC_MODEL",
                "BIONIC_CWD",
                "BIONIC_MAX_TURNS",
                "BIONIC_TIMEOUT",
                "BIONIC_COMMAND_TIMEOUT",
                "BIONIC_MAX_OUTPUT_CHARS",
                "BIONIC_TEMPERATURE",
                "BIONIC_MAX_TOKENS",
                "BIONIC_SYSTEM_PROMPT",
                "BIONIC_SESSION_DIR",
                "BIONIC_VERBOSE",
            )
        }
        with mock.patch.dict(os.environ, cleared):
            cfg = bionic.resolve_config(parser.parse_args(["-p", "hi"]))
        self.assertEqual(cfg.base_url, "http://localhost:1235/v1")
        self.assertEqual(cfg.api_key, "lm-studio")
        self.assertEqual(cfg.max_turns, 30)
        self.assertEqual(cfg.model, "")

        with tempfile.TemporaryDirectory() as tmp:
            env = {
                **cleared,
                "BIONIC_BASE_URL": "http://127.0.0.1:2235/v1/",
                "BIONIC_API_KEY": "env-key",
                "BIONIC_MODEL": "env-model",
                "BIONIC_CWD": tmp,
                "BIONIC_MAX_TURNS": "12",
                "BIONIC_TIMEOUT": "90",
                "BIONIC_COMMAND_TIMEOUT": "45",
                "BIONIC_MAX_OUTPUT_CHARS": "4096",
                "BIONIC_TEMPERATURE": "0.2",
                "BIONIC_MAX_TOKENS": "512",
                "BIONIC_SYSTEM_PROMPT": "env-sys",
                "BIONIC_VERBOSE": "true",
            }
            with mock.patch.dict(os.environ, env):
                cfg_env = bionic.resolve_config(parser.parse_args(["-p", "hi"]))
                self.assertEqual(cfg_env.base_url, "http://127.0.0.1:2235/v1")
                self.assertEqual(cfg_env.api_key, "env-key")
                self.assertEqual(cfg_env.model, "env-model")
                self.assertEqual(cfg_env.cwd, str(Path(tmp).resolve()))
                self.assertEqual(cfg_env.max_turns, 12)
                self.assertEqual(cfg_env.timeout, 90.0)
                self.assertEqual(cfg_env.command_timeout, 45.0)
                self.assertEqual(cfg_env.max_output_chars, 4096)
                self.assertEqual(cfg_env.temperature, 0.2)
                self.assertEqual(cfg_env.max_tokens, 512)
                self.assertEqual(cfg_env.system_prompt, "env-sys")
                self.assertTrue(cfg_env.verbose)

                cfg_cli = bionic.resolve_config(
                    parser.parse_args(
                        [
                            "-p",
                            "hi",
                            "--base-url",
                            "http://127.0.0.1:9999/v1",
                            "--api-key",
                            "",
                            "--model",
                            "cli-model",
                            "--max-turns",
                            "5",
                        ]
                    )
                )
                self.assertEqual(cfg_cli.base_url, "http://127.0.0.1:9999/v1")
                self.assertEqual(cfg_cli.api_key, "")
                self.assertEqual(cfg_cli.model, "cli-model")
                self.assertEqual(cfg_cli.max_turns, 5)

    def test_invalid_config_errors(self) -> None:
        parser = bionic.build_parser()
        with self.assertRaises(bionic.BionicError):
            bionic.resolve_config(parser.parse_args(["--cwd", "/no/such/bionic/dir/xyz", "--list-models"]))
        with self.assertRaises(bionic.BionicError):
            bionic.resolve_config(parser.parse_args(["--max-turns", "0", "--list-models"]))
        with self.assertRaises(bionic.BionicError):
            bionic.resolve_model(bionic.BionicConfig(model=""))


class BionicToolsTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.cwd = self.tmp.name

    def test_write_read_edit_and_run_command(self) -> None:
        write_res = json.loads(
            bionic.exec_tool(
                "write_file",
                {"path": "sub/hello.txt", "content": "line1\nline2\nline3\n"},
                self.cwd,
            )
        )
        self.assertTrue(write_res["ok"])

        full_text = bionic.exec_tool("read_file", {"path": "sub/hello.txt"}, self.cwd)
        self.assertEqual(full_text, "line1\nline2\nline3\n")

        sliced = bionic.exec_tool(
            "read_file",
            {"path": "sub/hello.txt", "start_line": 2, "end_line": 3},
            self.cwd,
        )
        self.assertEqual(sliced, "line2\nline3\n")

        edit_res = json.loads(
            bionic.exec_tool(
                "edit_file",
                {"path": "sub/hello.txt", "target": "line2", "replacement": "updated2"},
                self.cwd,
            )
        )
        self.assertTrue(edit_res["ok"])
        self.assertEqual(
            Path(self.cwd, "sub/hello.txt").read_text(encoding="utf-8"),
            "line1\nupdated2\nline3\n",
        )

        # Unique match guard and missing target errors
        miss = json.loads(
            bionic.exec_tool(
                "edit_file",
                {"path": "sub/hello.txt", "target": "absent", "replacement": "x"},
                self.cwd,
            )
        )
        self.assertIn("not found", miss["error"])

        bionic.exec_tool("write_file", {"path": "dup.txt", "content": "dup\ndup\n"}, self.cwd)
        ambig = json.loads(
            bionic.exec_tool(
                "edit_file",
                {"path": "dup.txt", "target": "dup", "replacement": "one"},
                self.cwd,
            )
        )
        self.assertIn("matched 2 times", ambig["error"])

        # Directory read lists entries
        dir_res = json.loads(bionic.exec_tool("read_file", {"path": "."}, self.cwd))
        self.assertIn("dup.txt", dir_res["entries"])
        self.assertIn("sub/", dir_res["entries"])

        # run_command handles UTF-8 output cleanly
        cmd = f'"{sys.executable}" -c "print(\'ok — utf8\')"'
        cmd_res = json.loads(bionic.exec_tool("run_command", {"command": cmd}, self.cwd))
        self.assertEqual(cmd_res["exit_code"], 0)
        self.assertIn("ok — utf8", cmd_res["stdout"])

    def test_run_command_timeout(self) -> None:
        cmd = f'"{sys.executable}" -c "import time; time.sleep(5)"'
        res = json.loads(
            bionic.exec_tool("run_command", {"command": cmd}, self.cwd, command_timeout=0.3)
        )
        self.assertIn("timed out", res["error"])


class BionicWorkspaceJailTests(unittest.TestCase):
    """Workspace root jail and tool approval modes."""

    def setUp(self) -> None:
        self.outer = tempfile.TemporaryDirectory()
        self.addCleanup(self.outer.cleanup)
        base = Path(self.outer.name).resolve()
        self.root = base / "repo"
        self.root.mkdir()
        (self.root / "inside.txt").write_text("inside\n", encoding="utf-8")
        self.secret = base / "secret.txt"
        self.secret.write_text("top secret\n", encoding="utf-8")
        self.cwd = str(self.root)

    def _call(self, name: str, args: dict[str, Any], **kw: Any) -> Any:
        out = bionic.exec_tool(name, args, self.cwd, **kw)
        try:
            return json.loads(out)
        except json.JSONDecodeError:
            return out

    def test_file_tools_reject_paths_outside_root(self) -> None:
        escapes = [
            "../secret.txt",
            "sub/../../secret.txt",
            str(self.secret),
            "~/bionic-jail-probe.txt",
        ]
        if os.name == "nt":
            escapes += [str(self.secret).replace("\\", "/"), "\\bionic-jail-probe.txt"]
        for path in escapes:
            with self.subTest(path=path):
                read = self._call("read_file", {"path": path})
                self.assertIsInstance(read, dict)
                self.assertTrue(read.get("denied"), read)
                self.assertNotIn("top secret", json.dumps(read))
                wrote = self._call("write_file", {"path": path, "content": "pwned"})
                self.assertTrue(wrote.get("denied"), wrote)
                edited = self._call("edit_file", {"path": path, "target": "top", "replacement": "x"})
                self.assertTrue(edited.get("denied"), edited)
        self.assertEqual(self.secret.read_text(encoding="utf-8"), "top secret\n")
        self.assertFalse((Path.home() / "bionic-jail-probe.txt").exists())
        self.assertFalse((self.root.parent / "pwned").exists())

    def test_paths_inside_root_still_work(self) -> None:
        self.assertEqual(self._call("read_file", {"path": "inside.txt"}), "inside\n")
        self.assertEqual(self._call("read_file", {"path": str(self.root / "inside.txt")}), "inside\n")
        self.assertEqual(self._call("read_file", {"path": "a/../inside.txt"}), "inside\n")
        res = self._call("write_file", {"path": "new/dir/f.txt", "content": "ok"})
        self.assertTrue(res["ok"])
        self.assertTrue((self.root / "new" / "dir" / "f.txt").is_file())

    def test_symlink_escape_is_denied(self) -> None:
        link = self.root / "link"
        try:
            if os.name == "nt":
                # Junctions need no privileges on Windows, unlike symlinks.
                proc = subprocess.run(
                    ["cmd", "/c", "mklink", "/J", str(link), str(self.root.parent)],
                    capture_output=True,
                )
                if proc.returncode != 0:
                    self.skipTest("cannot create junction")
            else:
                link.symlink_to(self.root.parent, target_is_directory=True)
        except OSError:
            self.skipTest("cannot create symlink")
        res = self._call("read_file", {"path": "link/secret.txt"})
        self.assertTrue(res.get("denied"), res)
        res = self._call("write_file", {"path": "link/planted.txt", "content": "x"})
        self.assertTrue(res.get("denied"), res)
        self.assertFalse((self.root.parent / "planted.txt").exists())

    def test_workspace_root_wider_than_cwd(self) -> None:
        sub = self.root / "pkg"
        sub.mkdir()
        res = bionic.exec_tool("read_file", {"path": "../inside.txt"}, str(sub), workspace_root=self.cwd)
        self.assertEqual(res, "inside\n")
        res = json.loads(
            bionic.exec_tool("read_file", {"path": "../../secret.txt"}, str(sub), workspace_root=self.cwd)
        )
        self.assertTrue(res["denied"])
        # cwd outside the root is refused outright
        res = json.loads(bionic.exec_tool("read_file", {"path": "x"}, str(self.root.parent), workspace_root=self.cwd))
        self.assertIn("outside workspace root", res["error"])

    def test_check_command_allows_workspace_commands(self) -> None:
        ok = [
            "git status",
            "git log --format=%H -1 -- src/main.rs",
            f'"{sys.executable}" -m unittest tests.test_x',
            "airc --profile w1 task status task_1 review --branch feat/x --note \"ran tests/unit ok\"",
            "airc --server http://127.0.0.1:6680 inbox",
            "cat sub/../inside.txt > out/log.txt 2>&1",
            "echo hi > /dev/null" if os.name != "nt" else "echo hi > nul",
            "ls ./src | grep rs && cargo test",
        ]
        if os.name == "nt":
            ok.append("dir /b /s")
        for cmd in ok:
            with self.subTest(cmd=cmd):
                self.assertIsNone(bionic.check_command(cmd, self.cwd))

    def test_check_command_blocks_escapes(self) -> None:
        bad = [
            "cat ../secret.txt",
            f'type "{self.secret}"',
            f"cat {self.secret.as_posix()}",
            "cd .. && ls",
            "ls ~",
            "cat ~/.ssh/id_rsa",
            "echo x > ../planted.txt",
            "git -C .. status",
            "cp inside.txt --target-directory=../",
            "cat $HOME/.bashrc",
            "cat $(echo ../secret.txt)",
            "cat `echo x`",
            "rm -rf /etc/passwd",
        ]
        if os.name == "nt":
            bad += [r"type C:\Windows\win.ini", r"type \\server\share\x", r"type %USERPROFILE%\x"]
        for cmd in bad:
            with self.subTest(cmd=cmd):
                self.assertIsNotNone(bionic.check_command(cmd, self.cwd))

    def test_run_command_denied_before_execution(self) -> None:
        marker = self.root.parent / "ran.txt"
        cmd = f'"{sys.executable}" -c "open(\'ran.txt\', \'w\').close()" && echo x > ../ran.txt'
        res = self._call("run_command", {"command": cmd})
        self.assertTrue(res["denied"], res)
        self.assertFalse(marker.exists())
        self.assertFalse((self.root / "ran.txt").exists())

    def test_read_only_mode(self) -> None:
        for name, args in (
            ("run_command", {"command": "echo hi"}),
            ("write_file", {"path": "x.txt", "content": "x"}),
            ("edit_file", {"path": "inside.txt", "target": "inside", "replacement": "x"}),
        ):
            with self.subTest(tool=name):
                res = self._call(name, args, approval="read-only")
                self.assertTrue(res["denied"])
                self.assertIn("read-only", res["error"])
        self.assertFalse((self.root / "x.txt").exists())
        self.assertEqual(self._call("read_file", {"path": "inside.txt"}, approval="read-only"), "inside\n")
        names = {t["function"]["name"] for t in bionic.tools_for_approval("read-only")}
        self.assertEqual(names, {"read_file"})
        self.assertEqual(len(bionic.tools_for_approval("auto")), len(bionic.TOOLS))

    def test_prompt_mode_uses_approver(self) -> None:
        seen: list[str] = []

        def deny(name: str, args: dict[str, Any]) -> bool:
            seen.append(name)
            return False

        res = self._call("write_file", {"path": "x.txt", "content": "x"}, approval="prompt", approver=deny)
        self.assertTrue(res["denied"])
        self.assertIn("rejected", res["error"])
        self.assertFalse((self.root / "x.txt").exists())

        res = self._call(
            "write_file", {"path": "x.txt", "content": "x"}, approval="prompt", approver=lambda n, a: True
        )
        self.assertTrue(res["ok"])

        # Reads never prompt; jail violations are denied without asking.
        self.assertEqual(self._call("read_file", {"path": "inside.txt"}, approval="prompt", approver=deny), "inside\n")
        res = self._call("write_file", {"path": "../p.txt", "content": "x"}, approval="prompt", approver=deny)
        self.assertTrue(res["denied"])
        self.assertEqual(seen, ["write_file"])

    def test_prompt_mode_without_terminal_denies(self) -> None:
        with mock.patch("builtins.open", side_effect=OSError("no tty")):
            self.assertFalse(bionic.tty_approver("run_command", {"command": "echo"}))

    def test_config_workspace_root_and_approval(self) -> None:
        parser = bionic.build_parser()
        sub = self.root / "pkg"
        sub.mkdir()
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("BIONIC_WORKSPACE_ROOT", None)
            os.environ.pop("BIONIC_APPROVAL", None)
            cfg = bionic.resolve_config(parser.parse_args(["--cwd", str(sub), "--list-models"]))
            self.assertEqual(cfg.workspace_root, str(sub))
            self.assertEqual(cfg.approval, "auto")
            cfg = bionic.resolve_config(
                parser.parse_args(
                    ["--cwd", str(sub), "--workspace-root", self.cwd, "--approval", "read-only", "--list-models"]
                )
            )
            self.assertEqual(cfg.workspace_root, self.cwd)
            self.assertEqual(cfg.approval, "read-only")
            os.environ["BIONIC_APPROVAL"] = "prompt"
            os.environ["BIONIC_WORKSPACE_ROOT"] = self.cwd
            cfg = bionic.resolve_config(parser.parse_args(["--cwd", str(sub), "--list-models"]))
            self.assertEqual((cfg.workspace_root, cfg.approval), (self.cwd, "prompt"))
            os.environ["BIONIC_APPROVAL"] = "yolo"
            with self.assertRaises(bionic.BionicError):
                bionic.resolve_config(parser.parse_args(["--cwd", str(sub), "--list-models"]))
            os.environ.pop("BIONIC_APPROVAL")
            with self.assertRaises(bionic.BionicError):
                bionic.resolve_config(
                    parser.parse_args(["--cwd", str(self.root.parent), "--workspace-root", self.cwd, "--list-models"])
                )
        prompt = bionic.build_system_prompt(self.cwd, workspace_root=self.cwd, approval="read-only")
        self.assertIn("workspace root", prompt)
        self.assertIn("read-only", prompt)


class BionicServerLoopTests(unittest.TestCase):
    def setUp(self) -> None:
        self.server = _FakeBionicServer()
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        self.base_url = f"http://127.0.0.1:{self.server.server_address[1]}/v1"
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)

    def test_list_models_filters_embeddings_and_cli_outputs_json(self) -> None:
        models = bionic.list_models(self.base_url, api_key="k1")
        self.assertEqual(
            models,
            [
                {"id": "qwen/qwen3.8-27b", "display_name": "Qwen3.8 27B"},
                {"id": "google/gemma-4-26b-a4b-qat"},
            ],
        )
        all_models = bionic.list_models(self.base_url, api_key="k1", include_all=True)
        self.assertEqual(len(all_models), 3)
        self.assertEqual(self.server.requests[0]["headers"]["authorization"], "Bearer k1")

        buf = io.StringIO()
        with redirect_stdout(buf):
            rc = bionic.main(["--base-url", self.base_url, "--list-models"])
        self.assertEqual(rc, 0)
        parsed = json.loads(buf.getvalue())
        self.assertEqual([m["id"] for m in parsed["data"]], ["qwen/qwen3.8-27b", "google/gemma-4-26b-a4b-qat"])

    def test_multi_turn_tool_loop_and_auto_model(self) -> None:
        self.server.chat_replies = [
            (
                200,
                {
                    "choices": [
                        {
                            "message": {
                                "role": "assistant",
                                "content": None,
                                "reasoning_content": "Let us create the file first.",
                                "tool_calls": [
                                    {
                                        "id": "tc1",
                                        "type": "function",
                                        "function": {
                                            "name": "write_file",
                                            "arguments": json.dumps({"path": "note.txt", "content": "hello world\n"}),
                                        },
                                    }
                                ],
                            }
                        }
                    ]
                },
            ),
            (
                200,
                {
                    "choices": [
                        {
                            "message": {
                                "role": "assistant",
                                "content": "",
                                "tool_calls": [
                                    {
                                        "id": "tc2",
                                        "type": "function",
                                        "function": {
                                            "name": "edit_file",
                                            "arguments": json.dumps(
                                                {
                                                    "path": "note.txt",
                                                    "target": "hello world",
                                                    "replacement": "hello bionic",
                                                }
                                            ),
                                        },
                                    },
                                    {
                                        "type": "function",
                                        "function": {
                                            "name": "run_command",
                                            "arguments": json.dumps(
                                                {
                                                    "command": f'"{sys.executable}" -c "print(open(\'note.txt\').read().strip())"'
                                                }
                                            ),
                                        },
                                    },
                                ],
                            }
                        }
                    ]
                },
            ),
            (
                200,
                {
                    "choices": [
                        {
                            "message": {
                                "role": "assistant",
                                "content": "Updated note.txt to hello bionic.",
                            }
                        }
                    ]
                },
            ),
        ]

        out = io.StringIO()
        err = io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            rc = bionic.main(
                [
                    "--base-url",
                    self.base_url,
                    "--model",
                    "auto",
                    "--cwd",
                    self.tmp.name,
                    "--temperature",
                    "0.1",
                    "--max-tokens",
                    "256",
                    "-v",
                    "-p",
                    "Create and edit note.txt",
                ]
            )
        self.assertEqual(rc, 0)
        self.assertEqual(out.getvalue().strip(), "Updated note.txt to hello bionic.")
        self.assertIn("[tool] write_file", err.getvalue())
        self.assertEqual(Path(self.tmp.name, "note.txt").read_text(encoding="utf-8"), "hello bionic\n")

        chat_reqs = [r for r in self.server.requests if r["path"] == "/v1/chat/completions"]
        self.assertEqual(len(chat_reqs), 3)
        # First auto-selected model should be the first non-embedding model
        self.assertEqual(chat_reqs[0]["body"]["model"], "qwen/qwen3.8-27b")
        self.assertEqual(chat_reqs[0]["body"]["temperature"], 0.1)
        self.assertEqual(chat_reqs[0]["body"]["max_tokens"], 256)
        # Verify reasoning_content was stripped and content=None normalized to ""
        second_turn_assistant = chat_reqs[1]["body"]["messages"][2]
        self.assertEqual(second_turn_assistant["content"], "")
        self.assertNotIn("reasoning_content", second_turn_assistant)
        # Verify missing tool_call id got synthesized
        third_turn_tool_msgs = [m for m in chat_reqs[2]["body"]["messages"] if m["role"] == "tool"]
        self.assertEqual(third_turn_tool_msgs[0]["tool_call_id"], "tc1")
        self.assertEqual(third_turn_tool_msgs[1]["tool_call_id"], "tc2")
        self.assertEqual(third_turn_tool_msgs[2]["tool_call_id"], "call_2_1")
        self.assertIn("hello bionic", json.loads(third_turn_tool_msgs[2]["content"])["stdout"])

    def test_malformed_tool_args_recovers_and_max_turns_fails(self) -> None:
        self.server.chat_replies = [
            (
                200,
                {
                    "choices": [
                        {
                            "message": {
                                "role": "assistant",
                                "content": "",
                                "tool_calls": [
                                    {
                                        "id": "bad1",
                                        "type": "function",
                                        "function": {"name": "read_file", "arguments": "{not-valid-json"},
                                    }
                                ],
                            }
                        }
                    ]
                },
            ),
            (
                200,
                {"choices": [{"message": {"role": "assistant", "content": "Recovered after tool error."}}]},
            ),
        ]
        res = bionic.run_agent(
            "Test recovery",
            bionic.BionicConfig(base_url=self.base_url, model="m1", cwd=self.tmp.name, max_turns=3),
        )
        self.assertEqual(res.content, "Recovered after tool error.")
        self.assertEqual(res.turns, 2)

        # Max turns exceeded -> exit code 1
        self.server.chat_replies = [
            (
                200,
                {
                    "choices": [
                        {
                            "message": {
                                "role": "assistant",
                                "content": "",
                                "tool_calls": [
                                    {
                                        "id": "loop1",
                                        "type": "function",
                                        "function": {"name": "read_file", "arguments": '{"path": "missing.txt"}'},
                                    }
                                ],
                            }
                        }
                    ]
                },
            )
        ]
        err = io.StringIO()
        with redirect_stderr(err):
            rc = bionic.main(
                ["--base-url", self.base_url, "--model", "m1", "--cwd", self.tmp.name, "--max-turns", "1", "-p", "loop"]
            )
        self.assertEqual(rc, 1)
        self.assertIn("max_turns=1", err.getvalue())

    def test_http_error_and_session_persistence(self) -> None:
        self.server.chat_replies = [(400, {"error": {"message": "model not loaded"}})]
        err = io.StringIO()
        with redirect_stderr(err):
            rc = bionic.main(["--base-url", self.base_url, "--model", "m1", "--cwd", self.tmp.name, "-p", "hi"])
        self.assertEqual(rc, 1)
        self.assertIn("HTTP 400", err.getvalue())
        self.assertIn("model not loaded", err.getvalue())

        # Session emit and resume
        sess_dir = Path(self.tmp.name) / "sessions"
        self.server.chat_replies = [
            (200, {"choices": [{"message": {"role": "assistant", "content": "First reply."}}]}),
            (200, {"choices": [{"message": {"role": "assistant", "content": "Second reply."}}]}),
        ]
        out1, err1 = io.StringIO(), io.StringIO()
        with redirect_stdout(out1), redirect_stderr(err1):
            rc1 = bionic.main(
                [
                    "--base-url",
                    self.base_url,
                    "--model",
                    "m1",
                    "--cwd",
                    self.tmp.name,
                    "--session-dir",
                    str(sess_dir),
                    "--emit-session-id",
                    "-p",
                    "remember alpha",
                ]
            )
        self.assertEqual(rc1, 0)
        sess_line = err1.getvalue().strip()
        self.assertTrue(sess_line.startswith("session_id: "))
        sess_id = sess_line.split(":", 1)[1].strip()

        self.server.requests.clear()
        out2 = io.StringIO()
        with redirect_stdout(out2):
            rc2 = bionic.main(
                [
                    "--base-url",
                    self.base_url,
                    "--model",
                    "m1",
                    "--cwd",
                    self.tmp.name,
                    "--session-dir",
                    str(sess_dir),
                    "--session-id",
                    sess_id,
                    "-p",
                    "what did i say?",
                ]
            )
        self.assertEqual(rc2, 0)
        sent_msgs = self.server.requests[-1]["body"]["messages"]
        user_contents = [m["content"] for m in sent_msgs if m["role"] == "user"]
        self.assertEqual(user_contents, ["remember alpha", "what did i say?"])

        # Path traversal session ID rejected
        with self.assertRaises(bionic.BionicError):
            bionic._resolve_session_file("../escape", str(sess_dir))


@unittest.skipIf(_binary_path() is None, "aircd binary not built (cargo build)")
class BionicGroupWakeE2ETest(unittest.TestCase):
    """Verify docs/templates/bionic.json, model discovery, groups, and wake execution."""

    @classmethod
    def setUpClass(cls) -> None:
        cls.tmp = tempfile.TemporaryDirectory()
        tmp = Path(cls.tmp.name)
        cls.cfg_dir = tmp / "cfg"
        cls.repo_dir = tmp / "repo"
        cls.repo_dir.mkdir(parents=True, exist_ok=True)
        subprocess.run(["git", "init", "-b", "main"], cwd=cls.repo_dir, check=True, capture_output=True)
        subprocess.run(["git", "config", "user.name", "Bionic Test"], cwd=cls.repo_dir, check=True, capture_output=True)
        subprocess.run(
            ["git", "config", "user.email", "bionic-test@example.local"],
            cwd=cls.repo_dir,
            check=True,
            capture_output=True,
        )
        (cls.repo_dir / "README.md").write_text("init\n", encoding="utf-8")
        subprocess.run(["git", "add", "README.md"], cwd=cls.repo_dir, check=True, capture_output=True)
        subprocess.run(["git", "commit", "-m", "init"], cwd=cls.repo_dir, check=True, capture_output=True)
        http_port = _free_port()
        cls.hub_url = f"http://127.0.0.1:{http_port}"

        cls.bionic_srv = _FakeBionicServer()
        threading.Thread(target=cls.bionic_srv.serve_forever, daemon=True).start()
        cls.bionic_url = f"http://127.0.0.1:{cls.bionic_srv.server_address[1]}/v1"

        env = os.environ.copy()
        env["BIONIC_BASE_URL"] = cls.bionic_url
        env["PYTHONPATH"] = str(CLI_PY_ROOT)
        cls.proc = subprocess.Popen(
            [
                str(_binary_path()),
                "--bind",
                f"127.0.0.1:{_free_port()}",
                "--http-bind",
                f"127.0.0.1:{http_port}",
                "--db",
                str(tmp / "bionic-e2e.sqlite3"),
                "--register-key",
                OWNER_KEY,
            ],
            cwd=str(tmp),
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        deadline = time.time() + 15
        while True:
            try:
                with urllib.request.urlopen(f"{cls.hub_url}/healthz", timeout=1):
                    break
            except Exception:
                if cls.proc.poll() is not None or time.time() > deadline:
                    cls.proc.kill()
                    raise RuntimeError("aircd did not start")
                time.sleep(0.1)

        cls.cli("register", "--nick", "bionicpm", "--role", "leader", "--invite", OWNER_KEY)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.proc.terminate()
        cls.proc.wait(timeout=10)
        cls.bionic_srv.shutdown()
        cls.bionic_srv.server_close()
        cls.tmp.cleanup()

    @classmethod
    def cli(cls, *args: str, check: bool = True, stdin: str | None = None) -> subprocess.CompletedProcess[str]:
        env = {k: v for k, v in os.environ.items() if not k.startswith(("AIRCD_", "AIRC_"))}
        env["AIRCD_CONFIG_DIR"] = str(cls.cfg_dir)
        env["PYTHONPATH"] = str(CLI_PY_ROOT)
        proc = subprocess.run(
            [sys.executable, "-m", "aircd.cli", "--server", cls.hub_url, *args],
            env=env,
            input=stdin,
            capture_output=True,
            text=True,
            encoding="utf-8",
        )
        if check and proc.returncode != 0:
            raise AssertionError(f"airc {' '.join(args)} failed:\n{proc.stdout}\n{proc.stderr}")
        return proc

    def test_bionic_template_models_group_and_tool_calling_wake(self) -> None:
        fixture = json.loads((REPO_ROOT / "docs" / "templates" / "bionic.json").read_text(encoding="utf-8"))
        self.assertEqual(fixture["command"], "airc-bionic")
        self.assertEqual(fixture["models_command"], ["airc-bionic", "--list-models"])
        self.assertEqual(fixture["session_mode"], "fresh")

        # Adapt command/models_command to invoke this checkout's module via sys.executable
        # so the test passes even without `airc-bionic` installed globally on PATH.
        runnable_template = {
            **fixture,
            "command": sys.executable,
            "args": ["-m", "aircd.bionic", *fixture["args"]],
            "models_command": [sys.executable, "-m", "aircd.bionic", "--list-models"],
            "env": {
                "BIONIC_BASE_URL": self.bionic_url,
                "PYTHONPATH": str(CLI_PY_ROOT),
                "AIRCD_CONFIG_DIR": str(self.cfg_dir),
            },
        }
        saved = json.loads(
            self.cli(
                "--json",
                "template",
                "set",
                "bionic",
                "--invite",
                OWNER_KEY,
                "--file",
                "-",
                stdin=json.dumps(runnable_template),
            ).stdout
        )["template"]
        self.assertTrue(saved["installed"])

        models_out = self.cli("template", "models", "bionic", "--invite", OWNER_KEY).stdout
        self.assertIn("qwen/qwen3.8-27b", models_out)
        self.assertIn("google/gemma-4-26b-a4b-qat", models_out)
        self.assertNotIn("text-embedding", models_out)

        # Create project + register repo before spawning workers into #local
        self.cli("--profile", "bionicpm", "project", "create", "#local", "--name", "Local Bionic Project")
        self.cli("--profile", "bionicpm", "git", "register", str(self.repo_dir), "--channel", "#local", "--repo", "localrepo")

        # Spawn group lead and worker from the bionic template
        lead_spawn = json.loads(
            self.cli(
                "--json",
                "template",
                "spawn",
                "bionic",
                "bionic-lead",
                "--model",
                "qwen/qwen3.8-27b",
                "--channel",
                "#local",
                "--invite",
                OWNER_KEY,
            ).stdout
        )
        worker_spawn = json.loads(
            self.cli(
                "--json",
                "template",
                "spawn",
                "bionic",
                "bionic-worker",
                "--model",
                "google/gemma-4-26b-a4b-qat",
                "--channel",
                "#local",
                "--invite",
                OWNER_KEY,
            ).stdout
        )

        # Save profiles so the spawned worker can run `airc` via run_command
        self.cli(
            "--profile",
            "bionic-worker",
            "--token",
            worker_spawn["account"]["token"],
            "login",
            "--channels",
            "#local",
        )
        self.cli(
            "--profile",
            "bionic-lead",
            "--token",
            lead_spawn["account"]["token"],
            "login",
            "--channels",
            "#local",
        )

        group_def = {
            "name": "bionic-team",
            "description": "Local tool-calling team",
            "lead": {"template": "bionic", "model": "qwen/qwen3.8-27b", "nick": "bionic-lead"},
            "workers": [
                {"template": "bionic", "model": "google/gemma-4-26b-a4b-qat", "count": 1, "nick": "bionic-worker"}
            ],
            "lead_prompt": "Lead {nick} in {group}: {task_id} {task_title}",
            "worker_prompt": (
                "Worker {nick} in group {group} ({role}) on {server}. "
                "Task {task_id} in {repo_path}: {task_body}"
            ),
        }
        self.cli("group", "create", "bionic-team", "--file", "-", "--invite", OWNER_KEY, stdin=json.dumps(group_def))
        self.cli("group", "attach", "bionic-team", "#local", "--invite", OWNER_KEY)

        # Program the fake Bionic model to write a file and advance the task to review via `airc`
        captured_prompts: list[str] = []
        turn_counter = {"n": 0}

        def handle_chat(body: dict[str, Any], _req: dict[str, Any]) -> tuple[int, Any]:
            turn_counter["n"] += 1
            if turn_counter["n"] == 1:
                prompt_text = body["messages"][1]["content"]
                captured_prompts.append(prompt_text)
                import re

                match = re.search(r"task_[0-9a-f]+", prompt_text)
                assert match, f"missing task id in prompt: {prompt_text}"
                tid = match.group(0)
                airc_cmd = (
                    f'"{sys.executable}" -m aircd.cli --server {self.hub_url} --profile bionic-worker'
                )
                return (
                    200,
                    {
                        "choices": [
                            {
                                "message": {
                                    "role": "assistant",
                                    "content": "",
                                    "tool_calls": [
                                        {
                                            "id": "w1",
                                            "type": "function",
                                            "function": {
                                                "name": "write_file",
                                                "arguments": json.dumps(
                                                    {"path": "deliverable.txt", "content": "bionic tool output\n"}
                                                ),
                                            },
                                        },
                                        {
                                            "id": "w2",
                                            "type": "function",
                                            "function": {
                                                "name": "run_command",
                                                "arguments": json.dumps(
                                                    {
                                                        "command": (
                                                            f'{airc_cmd} task status {tid} in_progress && '
                                                            f'{airc_cmd} task status {tid} testing && '
                                                            f'{airc_cmd} task status {tid} review '
                                                            f'--branch feat/bionic-test '
                                                            f'--note "wrote deliverable.txt"'
                                                        )
                                                    }
                                                ),
                                            },
                                        },
                                    ],
                                }
                            }
                        ]
                    },
                )
            return (
                200,
                {
                    "choices": [
                        {"message": {"role": "assistant", "content": "Completed deliverable.txt and submitted review."}}
                    ]
                },
            )

        self.bionic_srv.chat_handler = handle_chat

        # Create a subtask assigned to bionic-worker; this wakes the worker automatically.
        created_task = json.loads(
            self.cli(
                "--profile",
                "bionicpm",
                "--json",
                "task",
                "create",
                "#local",
                "Implement local deliverable",
                "--assign",
                "bionic-worker",
                "--repo",
                "localrepo",
                "--body",
                "Write deliverable.txt and submit for review.",
            ).stdout
        )["task"]

        # Wait for the wake launcher -> aircd.bionic -> tool calls -> task status review
        deadline = time.time() + 20
        status = ""
        while time.time() < deadline:
            shown = json.loads(
                self.cli("--profile", "bionicpm", "--json", "task", "show", created_task["id"]).stdout
            )["task"]
            status = shown["status"]
            if status == "review":
                break
            time.sleep(0.2)

        runs = json.loads(
            self.cli("--json", "owner", "wake", "runs", "--nick", "bionic-worker", "--invite", OWNER_KEY, "--logs").stdout
        )["runs"]
        self.assertEqual(status, "review", f"runs={runs}")
        self.assertEqual((self.repo_dir / "deliverable.txt").read_text(encoding="utf-8"), "bionic tool output\n")
        self.assertTrue(captured_prompts)
        self.assertIn("Worker bionic-worker in group bionic-team (group worker)", captured_prompts[0])
        self.assertIn(created_task["id"], captured_prompts[0])

        # Verify wake_runs recorded success
        self.assertTrue(any(r["status"] == "ok" for r in runs), f"unexpected runs: {runs}")


if __name__ == "__main__":
    unittest.main()
