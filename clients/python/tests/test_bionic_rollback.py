"""Bionic git workspace snapshots and auto-rollback on tool failure."""

from __future__ import annotations

import argparse
import io
import json
import os
import subprocess
import sys
import tempfile
import threading
import unittest
from pathlib import Path
from typing import Any
from unittest import mock

from aircd import bionic
from aircd.bionic_rollback import (
    ROLLBACK_REF_PREFIX,
    SNAPSHOT_REF,
    GitSnapshotter,
    RollbackGuard,
    SnapshotError,
    annotate_result,
    failure_reason,
)
from tests.test_bionic import _FakeBionicServer

CLI_PY_ROOT = Path(__file__).resolve().parents[1]


def git(cwd: str | Path, *args: str) -> str:
    proc = subprocess.run(
        ["git", "-c", "user.name=t", "-c", "user.email=t@t", *args],
        cwd=str(cwd),
        capture_output=True,
        text=True,
        check=True,
    )
    return proc.stdout.strip()


def make_repo(root: Path, *, autocrlf: str = "true") -> None:
    git(root, "init", "-q", "-b", "main")
    git(root, "config", "core.autocrlf", autocrlf)
    (root / ".gitignore").write_bytes(b"build/\n")
    (root / ".gitattributes").write_bytes(b"* text=auto\n")
    (root / "keep.txt").write_bytes(b"line one\r\nline two\r\n")
    (root / "lf.txt").write_bytes(b"unix\nendings\n")
    (root / "src").mkdir()
    (root / "src" / "main.rs").write_bytes(b"fn main() {}\n")
    git(root, "add", "-A")
    git(root, "commit", "-q", "-m", "init")


def tree_bytes(root: Path) -> dict[str, bytes]:
    out = {}
    for path in sorted(root.rglob("*")):
        rel = path.relative_to(root).as_posix()
        if rel == ".git" or rel.startswith(".git/") or not path.is_file():
            continue
        out[rel] = path.read_bytes()
    return out


class RepoTestCase(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name).resolve() / "repo"
        self.root.mkdir()
        make_repo(self.root)
        # Work-tree state that is not committed must survive a rollback too.
        (self.root / "draft.md").write_bytes(b"untracked draft\r\n")
        (self.root / "keep.txt").write_bytes(b"line one\r\nline two\r\nedited, unstaged\r\n")


class SnapshotterTests(RepoTestCase):
    def test_restore_is_byte_exact_and_removes_created_files(self) -> None:
        snapper = GitSnapshotter(str(self.root))
        self.addCleanup(snapper.close)
        before = tree_bytes(self.root)
        snap = snapper.take("run_command")
        self.assertEqual(git(self.root, "rev-parse", SNAPSHOT_REF), snap.commit)

        (self.root / "keep.txt").write_bytes(b"clobbered\n")
        (self.root / "lf.txt").unlink()
        (self.root / "draft.md").write_bytes(b"changed\n")
        (self.root / "src" / "new.rs").write_bytes(b"// new\n")
        (self.root / "newdir" / "deep").mkdir(parents=True)
        (self.root / "newdir" / "deep" / "x.txt").write_bytes(b"x")
        (self.root / "build").mkdir()
        (self.root / "build" / "out.o").write_bytes(b"ignored artifact")
        self.assertEqual(
            sorted(snapper.changed_since(snap)),
            ["draft.md", "keep.txt", "lf.txt", "newdir/deep/x.txt", "src/new.rs"],
        )

        report = snapper.restore(snap, reason="run_command exit code 1")
        self.assertTrue(report.restored)
        self.assertEqual(len(report.changed), 5)
        after = tree_bytes(self.root)
        # Ignored files are outside the snapshot and are left alone.
        self.assertEqual(after.pop("build/out.o"), b"ignored artifact")
        self.assertEqual(after, before)
        self.assertEqual(snapper.changed_since(snap), [])
        # The real index still shows the user's own unstaged edit, untouched.
        status = git(self.root, "status", "--porcelain")
        self.assertIn("M keep.txt", status.splitlines()[0])
        self.assertIn("?? draft.md", status)

    def test_discarded_changes_are_kept_as_a_stash_shaped_commit(self) -> None:
        snapper = GitSnapshotter(str(self.root))
        self.addCleanup(snapper.close)
        snap = snapper.take("write_file")
        (self.root / "src" / "main.rs").write_bytes(b"fn main() { broken }\n")
        report = snapper.restore(snap, reason="write_file error: boom")
        self.assertEqual(report.saved_ref, ROLLBACK_REF_PREFIX + snap.id)
        self.assertEqual(git(self.root, "rev-parse", report.saved_ref), report.saved_commit)
        self.assertEqual(git(self.root, "show", f"{report.saved_commit}:src/main.rs"), "fn main() { broken }")
        parents = git(self.root, "rev-list", "--parents", "-n1", report.saved_commit).split()
        self.assertEqual(len(parents), 3)  # commit, base, index: stash-shaped
        self.assertEqual(parents[1], snap.commit)
        self.assertEqual(git(self.root, "stash", "list"), "")  # the shared stash list is never touched
        diff = git(self.root, "stash", "show", "-p", report.saved_commit)
        self.assertIn("+fn main() { broken }", diff)
        self.assertNotIn("keep.txt", diff)  # only the discarded change, not the user's own edits
        # The documented recovery path works on the dirty work tree.
        patch = subprocess.run(
            ["git", "stash", "show", "-p", "--binary", report.saved_commit],
            cwd=self.root, capture_output=True, check=True,
        ).stdout
        subprocess.run(["git", "apply"], cwd=self.root, input=patch, capture_output=True, check=True)
        # (git apply writes with the repo's own autocrlf setting, as any checkout does)
        restored = (self.root / "src" / "main.rs").read_bytes().replace(b"\r\n", b"\n")
        self.assertEqual(restored, b"fn main() { broken }\n")

    def test_head_and_index_moved_by_the_tool_are_put_back(self) -> None:
        snapper = GitSnapshotter(str(self.root))
        self.addCleanup(snapper.close)
        head = git(self.root, "rev-parse", "HEAD")
        snap = snapper.take("run_command")
        (self.root / "src" / "main.rs").write_bytes(b"fn main() { 1 }\n")
        git(self.root, "add", "src/main.rs")
        git(self.root, "commit", "-q", "-m", "tool commit")
        git(self.root, "add", "draft.md")
        report = snapper.restore(snap, reason="exit code 2")
        self.assertTrue(report.head_restored)
        self.assertTrue(report.index_restored)
        self.assertEqual(git(self.root, "rev-parse", "HEAD"), head)
        self.assertEqual(git(self.root, "symbolic-ref", "HEAD"), "refs/heads/main")
        self.assertEqual((self.root / "src" / "main.rs").read_bytes(), b"fn main() {}\n")
        status = git(self.root, "status", "--porcelain")
        self.assertIn("?? draft.md", status)
        self.assertNotIn("src/main.rs", status)

    def test_scope_is_limited_to_the_workspace_subdirectory(self) -> None:
        snapper = GitSnapshotter(str(self.root / "src"))
        self.addCleanup(snapper.close)
        self.assertEqual(snapper.scope, "src")
        snap = snapper.take()
        (self.root / "src" / "main.rs").write_bytes(b"changed\n")
        (self.root / "src" / "extra.rs").write_bytes(b"extra\n")
        (self.root / "outside.txt").write_bytes(b"not mine\n")
        report = snapper.restore(snap, reason="exit code 1")
        self.assertEqual(sorted(report.changed), ["src/extra.rs", "src/main.rs"])
        self.assertEqual((self.root / "src" / "main.rs").read_bytes(), b"fn main() {}\n")
        self.assertFalse((self.root / "src" / "extra.rs").exists())
        self.assertEqual((self.root / "outside.txt").read_bytes(), b"not mine\n")

    def test_unchanged_workspace_restores_nothing(self) -> None:
        snapper = GitSnapshotter(str(self.root))
        self.addCleanup(snapper.close)
        snap = snapper.take()
        report = snapper.restore(snap, reason="denied")
        self.assertFalse(report.restored)
        self.assertEqual(report.changed, [])
        self.assertIsNone(report.saved_ref)

    def test_repo_without_commits(self) -> None:
        bare = Path(self.tmp.name).resolve() / "fresh"
        bare.mkdir()
        git(bare, "init", "-q")
        (bare / "a.txt").write_bytes(b"a")
        snapper = GitSnapshotter(str(bare))
        self.addCleanup(snapper.close)
        snap = snapper.take()
        self.assertIsNone(snap.head)
        (bare / "a.txt").write_bytes(b"b")
        (bare / "b.txt").write_bytes(b"new")
        report = snapper.restore(snap, reason="exit code 1")
        self.assertTrue(report.restored)
        self.assertEqual((bare / "a.txt").read_bytes(), b"a")
        self.assertFalse((bare / "b.txt").exists())

    def test_private_index_is_removed_on_close(self) -> None:
        snapper = GitSnapshotter(str(self.root))
        snapper.take()
        leftovers = list((self.root / ".git").glob("bionic-index-*"))
        self.assertEqual(len(leftovers), 1)
        snapper.close()
        self.assertEqual(list((self.root / ".git").glob("bionic-index-*")), [])

    def test_guard_runs_unprotected_when_the_snapshot_fails(self) -> None:
        logs: list[str] = []
        guard = RollbackGuard(str(self.root), log=logs.append)
        self.addCleanup(guard.close)
        with mock.patch.object(GitSnapshotter, "take", side_effect=SnapshotError("git add failed: locked")):
            result = guard.run("run_command", {}, lambda _a: '{"exit_code": 1}')
        self.assertEqual(result, '{"exit_code": 1}')
        self.assertIn("running unprotected", logs[0])

    def test_failed_restore_is_reported_not_hidden(self) -> None:
        guard = RollbackGuard(str(self.root))
        self.addCleanup(guard.close)
        with mock.patch.object(GitSnapshotter, "restore", side_effect=SnapshotError("git add failed: locked")):
            result = json.loads(guard.run("run_command", {}, lambda _a: '{"exit_code": 1}'))
        self.assertFalse(result["rolled_back"])
        self.assertIn("locked", result["rollback"]["warnings"][0])

    def test_add_failure_raises_and_resets_the_private_index(self) -> None:
        snapper = GitSnapshotter(str(self.root))
        self.addCleanup(snapper.close)
        snapper.take()
        failed = subprocess.CompletedProcess([], 128, "", "fatal: unable to read")
        with mock.patch("aircd.bionic_rollback._run_git", return_value=failed):
            with self.assertRaises(SnapshotError):
                snapper.take()
        self.assertEqual(list((self.root / ".git").glob("bionic-index-*")), [])

    def test_not_a_git_work_tree(self) -> None:
        plain = Path(self.tmp.name).resolve() / "plain"
        plain.mkdir()
        with mock.patch.dict(os.environ, {"GIT_CEILING_DIRECTORIES": str(Path(self.tmp.name).resolve())}):
            with self.assertRaises(SnapshotError):
                GitSnapshotter(str(plain))
            logs: list[str] = []
            guard = RollbackGuard(str(plain), log=logs.append)
        self.assertFalse(guard.available)
        self.assertIn("not inside a git work tree", logs[0])
        self.assertEqual(guard.run("write_file", {}, lambda _a: '{"error": "x"}'), '{"error": "x"}')


class FailureReasonTests(unittest.TestCase):
    def test_classification(self) -> None:
        self.assertIsNone(failure_reason(json.dumps({"exit_code": 0, "stdout": ""})))
        self.assertIsNone(failure_reason(json.dumps({"ok": True})))
        self.assertIsNone(failure_reason("plain text from a plugin"))
        self.assertEqual(failure_reason(json.dumps({"exit_code": 3})), "exit code 3")
        self.assertEqual(failure_reason(json.dumps({"error": "Command timed out"})), "error: Command timed out")
        self.assertTrue(failure_reason(json.dumps({"error": "Denied: no", "denied": True})).startswith("denied"))
        self.assertEqual(failure_reason(json.dumps({"ok": False})), "tool reported ok=false")

    def test_annotate_non_json_result(self) -> None:
        from aircd.bionic_rollback import RollbackReport

        out = json.loads(annotate_result("raw", RollbackReport(restored=True, reason="r", snapshot="abc")))
        self.assertEqual(out["result"], "raw")
        self.assertTrue(out["rolled_back"])


def _call(name: str, args: dict[str, Any], call_id: str) -> tuple[int, dict[str, Any]]:
    return 200, {
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


def _final(text: str = "done") -> tuple[int, dict[str, Any]]:
    return 200, {"choices": [{"message": {"role": "assistant", "content": text}}]}


PY = sys.executable.replace("\\", "/")


def _py(code: str) -> str:
    """A shell command running ``code`` with this Python (quotes safe for cmd.exe and sh)."""
    return f'"{PY}" -c "{code}"'


class RunAgentRollbackTests(RepoTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.server = _FakeBionicServer()
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        self.base_url = f"http://127.0.0.1:{self.server.server_address[1]}/v1"

    def config(self, **kwargs: Any) -> bionic.BionicConfig:
        kwargs.setdefault("rollback", True)
        return bionic.BionicConfig(
            base_url=self.base_url, model="m", cwd=str(self.root), workspace_root=str(self.root),
            plugins=False, **kwargs,
        )

    def tool_results(self) -> list[dict[str, Any]]:
        last = [r for r in self.server.requests if r["path"] == "/v1/chat/completions"][-1]
        return [json.loads(m["content"]) for m in last["body"]["messages"] if m["role"] == "tool"]

    def test_failed_command_is_rolled_back_and_successful_one_kept(self) -> None:
        self.server.chat_replies = [
            _call("run_command", {"command": _py("open('bad.txt','w').write('x'); raise SystemExit(4)")}, "a"),
            _call("run_command", {"command": _py("open('good.txt','w').write('y')")}, "b"),
            _final(),
        ]
        events: list[dict[str, Any]] = []
        logs: list[str] = []
        result = bionic.run_agent("go", self.config(), on_event=events.append, log=logs.append)
        self.assertEqual(result.content, "done")
        self.assertFalse((self.root / "bad.txt").exists())
        self.assertEqual((self.root / "good.txt").read_text(), "y")
        failed, ok = self.tool_results()
        self.assertEqual(failed["exit_code"], 4)
        self.assertTrue(failed["rolled_back"])
        self.assertEqual(failed["rollback"]["changed"], ["bad.txt"])
        self.assertTrue(failed["rollback"]["saved_ref"].startswith(ROLLBACK_REF_PREFIX))
        self.assertNotIn("rolled_back", ok)
        kinds = [e["type"] for e in events]
        self.assertEqual(kinds.count("snapshot"), 2)
        self.assertEqual(kinds.count("rollback"), 1)
        self.assertTrue(any("[rollback] restored workspace" in line for line in logs))
        self.assertEqual(list((self.root / ".git").glob("bionic-index-*")), [])

    def test_failed_edit_leaves_workspace_untouched(self) -> None:
        before = tree_bytes(self.root)
        self.server.chat_replies = [
            _call("edit_file", {"path": "keep.txt", "target": "nope", "replacement": "z"}, "a"),
            _final(),
        ]
        bionic.run_agent("go", self.config())
        (result,) = self.tool_results()
        self.assertIn("not found", result["error"])
        self.assertNotIn("rolled_back", result)  # nothing changed, so nothing to report
        self.assertEqual(tree_bytes(self.root), before)

    def test_review_rejection_reverts_a_successful_write(self) -> None:
        self.server.chat_replies = [
            _call("write_file", {"path": "src/main.rs", "content": "fn main() { panic!() }\n"}, "a"),
            _call("write_file", {"path": "notes.md", "content": "ok\n"}, "b"),
            _final(),
        ]
        seen: list[tuple[str, Any]] = []

        def review(name: str, args: Any, result: str) -> bool:
            seen.append((name, args["path"]))
            return args["path"] != "src/main.rs"

        bionic.run_agent("go", self.config(), review_tool=review)
        self.assertEqual(seen, [("write_file", "src/main.rs"), ("write_file", "notes.md")])
        self.assertEqual((self.root / "src" / "main.rs").read_bytes(), b"fn main() {}\n")
        self.assertEqual((self.root / "notes.md").read_text(), "ok\n")
        rejected, kept = self.tool_results()
        self.assertTrue(rejected["ok"])
        self.assertTrue(rejected["rolled_back"])
        self.assertEqual(rejected["rollback"]["reason"], "write_file rejected on review")
        self.assertNotIn("rolled_back", kept)

    def test_approval_rejection_runs_nothing_and_reports_nothing(self) -> None:
        before = tree_bytes(self.root)
        self.server.chat_replies = [_call("write_file", {"path": "x.txt", "content": "x"}, "a"), _final()]
        bionic.run_agent("go", self.config(approval="prompt"), approver=lambda _n, _a: False)
        (result,) = self.tool_results()
        self.assertTrue(result["denied"])
        self.assertNotIn("rolled_back", result)
        self.assertEqual(tree_bytes(self.root), before)

    def test_off_by_default_and_in_read_only_mode(self) -> None:
        self.server.chat_replies = [
            _call("run_command", {"command": _py("open('bad.txt','w').write('x'); raise SystemExit(1)")}, "a"),
            _final(),
        ]
        events: list[dict[str, Any]] = []
        bionic.run_agent("go", self.config(rollback=False), on_event=events.append)
        self.assertTrue((self.root / "bad.txt").exists())
        self.assertNotIn("snapshot", [e["type"] for e in events])
        self.assertIsNone(bionic.rollback_guard_for(self.config(approval="read-only")))
        guard = bionic.rollback_guard_for(self.config())
        self.assertIsNotNone(guard)
        guard.close()

    def test_dry_run_patch_and_reads_take_no_snapshot(self) -> None:
        self.assertFalse(bionic._is_mutating_call("read_file", {"path": "a"}, None))
        self.assertFalse(bionic._is_mutating_call("apply_patch", {"patch": "", "dry_run": True}, None))
        self.assertTrue(bionic._is_mutating_call("apply_patch", {"patch": ""}, None))
        self.assertTrue(bionic._is_mutating_call("run_command", {"command": "x"}, None))

    def test_stream_renderer_prints_rollback(self) -> None:
        out = io.StringIO()
        bionic.StreamRenderer("text", out)(
            {
                "type": "rollback",
                "name": "run_command",
                "restored": True,
                "reason": "run_command exit code 1",
                "changed": ["a", "b"],
                "saved_ref": "refs/bionic/rollbacks/abc",
                "warnings": ["HEAD moved"],
            }
        )
        text = out.getvalue()
        self.assertIn("[rollback] run_command: workspace restored (2 path(s), run_command exit code 1)", text)
        self.assertIn("refs/bionic/rollbacks/abc", text)
        self.assertIn("[rollback] warning: HEAD moved", text)

    def test_cli_flag_end_to_end(self) -> None:
        self.server.chat_replies = [
            _call("run_command", {"command": _py("open('bad.txt','w').write('x'); raise SystemExit(9)")}, "a"),
            _final("finished"),
        ]
        env = {k: v for k, v in os.environ.items() if not k.startswith("BIONIC_")}
        env["PYTHONPATH"] = str(CLI_PY_ROOT)
        env["PYTHONUTF8"] = "1"
        proc = subprocess.run(
            [
                sys.executable, "-m", "aircd.bionic", "--base-url", self.base_url, "--model", "m",
                "--cwd", str(self.root), "--rollback", "--no-plugins", "--stream", "-p", "go",
            ],
            capture_output=True, text=True, env=env, timeout=120,
        )
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("finished", proc.stdout)
        self.assertIn("[rollback] run_command: workspace restored", proc.stderr)
        self.assertFalse((self.root / "bad.txt").exists())


class ConfigTests(unittest.TestCase):
    def test_flag_and_env(self) -> None:
        with tempfile.TemporaryDirectory() as tmp, mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("BIONIC_ROLLBACK", None)
            parser = bionic.build_parser()
            self.assertFalse(bionic.resolve_config(parser.parse_args(["--cwd", tmp, "-p", "p"])).rollback)
            self.assertTrue(bionic.resolve_config(parser.parse_args(["--cwd", tmp, "--rollback", "-p", "p"])).rollback)
            os.environ["BIONIC_ROLLBACK"] = "1"
            self.assertTrue(bionic.resolve_config(parser.parse_args(["--cwd", tmp, "-p", "p"])).rollback)
            self.assertTrue(bionic.resolve_config(argparse.Namespace(cwd=tmp)).rollback)

    def test_sdk_option(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            agent = bionic.BionicAgent(cwd=tmp, rollback=True, review_tool=lambda *_a: True)
            self.assertTrue(agent.config.rollback)
            self.assertTrue(agent.with_options(max_turns=3)._review_tool is agent._review_tool)


if __name__ == "__main__":
    unittest.main()
