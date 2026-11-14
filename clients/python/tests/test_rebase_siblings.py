"""Tests for `airc git rebase-siblings` and the post-merge hook that runs it."""

from __future__ import annotations

import contextlib
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from aircd import cli  # noqa: E402
from tests.test_worktree import _FakeHub  # noqa: E402

TASKS_PATH = "/v1/tasks?channel=%23cams"


def git(cwd: Path, *args: str, check: bool = True) -> str:
    proc = subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True, check=False)
    if check and proc.returncode != 0:
        raise AssertionError(f"git {' '.join(args)} failed: {proc.stderr}")
    return proc.stdout.strip()


class RebaseSiblingsTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        root = Path(self.tmp.name)
        self.cfg_dir = root / "cfg"
        self.cfg_dir.mkdir()
        self.repo = root / "sample-repo"
        self.repo.mkdir()
        git(self.repo, "init", "-b", "main")
        git(self.repo, "config", "user.email", "dev@example.local")
        git(self.repo, "config", "user.name", "Dev")
        self.commit_file("README.md", "line one\nline two\n", "init")

        self.hub = _FakeHub()
        self.url = self.hub.start()
        self.addCleanup(self.hub.stop)
        self.hub.routes[("GET", "/v1/gits")] = (
            200,
            {"ok": True, "git_repos": [{"name": "sample-repo", "channel": "#cams", "local_path": str(self.repo), "default_branch": "main"}]},
        )
        self.hub.routes[("POST", "/v1/git/event")] = (201, {"ok": True, "channel": "#cams"})
        self.hub.routes[("POST", "/v1/messages")] = (201, {"ok": True, "message": {}})
        self.tasks: list[dict] = []
        self.hub.routes[("GET", TASKS_PATH)] = (200, {"ok": True, "tasks": self.tasks})

        env = {k: v for k, v in os.environ.items() if not k.startswith(("AIRCD_", "AIRC_"))}
        env["AIRCD_CONFIG_DIR"] = str(self.cfg_dir)
        patcher = mock.patch.dict(os.environ, env, clear=True)
        patcher.start()
        self.addCleanup(patcher.stop)

    # helpers -------------------------------------------------------------

    def commit_file(self, name: str, text: str, msg: str, cwd: Path | None = None) -> None:
        where = cwd or self.repo
        (where / name).write_text(text, encoding="utf-8")
        git(where, "add", name)
        git(where, "commit", "-q", "-m", msg)

    def branch_with_commit(self, branch: str, name: str, text: str) -> str:
        git(self.repo, "checkout", "-q", "-b", branch, "main")
        self.commit_file(name, text, f"work on {branch}")
        git(self.repo, "checkout", "-q", "main")
        return git(self.repo, "rev-parse", branch)

    def add_task(self, tid: str, status: str, branch: str | None, assignee: str = "dev1") -> None:
        self.tasks.append({"id": tid, "channel": "#cams", "status": status, "repo": "sample-repo", "branch": branch, "assignee": assignee})

    def run_cli(self, *args: str) -> tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        argv = ["--server", self.url, "--token", "tok", *args]
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = cli.main(argv)
        return code, out.getvalue(), err.getvalue()

    def rebase(self, *extra: str) -> dict:
        code, out, err = self.run_cli("--json", "git", "rebase-siblings", "--path", str(self.repo), *extra)
        self.assertEqual(code, 0, err)
        return json.loads(out)

    def by_branch(self, res: dict) -> dict[str, dict]:
        return {r["branch"]: r for r in res["branches"]}

    def posts(self, path: str) -> list[dict]:
        return [r["body"] for r in self.hub.requests if r["method"] == "POST" and r["path"] == path]

    def worktree_count(self) -> int:
        return git(self.repo, "worktree", "list", "--porcelain").count("worktree ")

    # tests ---------------------------------------------------------------

    def test_rebases_open_siblings_and_skips_review_merged_and_current(self) -> None:
        self.branch_with_commit("feat/a", "a.txt", "a\n")
        self.branch_with_commit("task/t_default", "d.txt", "d\n")  # task with no branch set
        review_old = self.branch_with_commit("feat/in-review", "r.txt", "r\n")
        self.branch_with_commit("feat/merged-later", "m.txt", "m\n")
        git(self.repo, "merge", "-q", "--no-ff", "-m", "merge m", "feat/merged-later")
        self.commit_file("main.txt", "main moved\n", "main moves on")
        main_sha = git(self.repo, "rev-parse", "main")
        self.add_task("t_a", "in_progress", "feat/a")
        self.add_task("t_default", "assigned", None)
        self.add_task("t_review", "review", "feat/in-review")
        self.add_task("t_merged", "testing", "feat/merged-later")
        self.add_task("t_nobranch", "open", None)
        self.add_task("t_other_repo", "in_progress", "feat/a", assignee="x")
        self.tasks[-1]["repo"] = "other-repo"

        res = self.rebase()
        rows = self.by_branch(res)
        self.assertEqual(rows["feat/a"]["result"], "rebased")
        self.assertEqual(rows["task/t_default"]["result"], "rebased")
        self.assertEqual(rows["feat/in-review"]["result"], "skipped")
        self.assertEqual(rows["feat/merged-later"]["result"], "fast_forwarded")
        self.assertEqual(git(self.repo, "rev-parse", "feat/merged-later"), main_sha)
        self.assertEqual(set(rows), {"feat/a", "task/t_default", "feat/in-review", "feat/merged-later"})
        self.assertTrue(res["ok"])
        for branch in ("feat/a", "task/t_default"):
            self.assertEqual(git(self.repo, "merge-base", branch, "main"), main_sha)
        self.assertEqual(git(self.repo, "rev-parse", "feat/in-review"), review_old)
        self.assertEqual(git(self.repo, "rev-parse", "--abbrev-ref", "HEAD"), "main")
        self.assertEqual(self.worktree_count(), 1, "temporary worktrees must be removed")

        events = self.posts("/v1/git/event")
        self.assertEqual(len(events), 1)
        self.assertEqual(events[0]["event"], "rebase")
        self.assertIn("feat/a", events[0]["summary"])
        self.assertEqual(self.posts("/v1/messages"), [], "clean rebases must not mention anyone")

        # Re-running is a no-op.
        self.hub.requests.clear()
        rows = self.by_branch(self.rebase())
        self.assertEqual(rows["feat/a"]["result"], "up_to_date")
        self.assertEqual(self.posts("/v1/git/event"), [])

        # --include-review opts in to rebasing review branches.
        rows = self.by_branch(self.rebase("--include-review"))
        self.assertEqual(rows["feat/in-review"]["result"], "rebased")

    def test_conflict_is_aborted_branch_unchanged_and_owner_mentioned(self) -> None:
        old = self.branch_with_commit("feat/clash", "README.md", "line one\nbranch edit\n")
        self.commit_file("README.md", "line one\nmain edit\n", "main edits readme")
        self.add_task("t_clash", "in_progress", "feat/clash", assignee="dev7")

        res = self.rebase()
        row = self.by_branch(res)["feat/clash"]
        self.assertEqual(row["result"], "conflict")
        self.assertEqual(row["files"], ["README.md"])
        self.assertFalse(res["ok"])
        self.assertEqual(git(self.repo, "rev-parse", "feat/clash"), old)
        self.assertEqual(self.worktree_count(), 1)
        msgs = self.posts("/v1/messages")
        self.assertEqual(len(msgs), 1)
        self.assertEqual(msgs[0]["target"], "#cams")
        self.assertIn("@dev7", msgs[0]["body"])
        self.assertIn("README.md", msgs[0]["body"])

    def test_checked_out_worktree_rebased_in_place_unless_dirty(self) -> None:
        self.branch_with_commit("feat/wt", "w.txt", "w\n")
        wt = Path(self.tmp.name) / "wt"
        git(self.repo, "worktree", "add", "-q", str(wt), "feat/wt")
        self.commit_file("main.txt", "x\n", "main moves")
        self.add_task("t_wt", "in_progress", "feat/wt")

        (wt / "w.txt").write_text("uncommitted\n", encoding="utf-8")
        old = git(self.repo, "rev-parse", "feat/wt")
        row = self.by_branch(self.rebase())["feat/wt"]
        self.assertEqual(row["result"], "skipped")
        self.assertIn("uncommitted", row["detail"])
        self.assertEqual(git(self.repo, "rev-parse", "feat/wt"), old)

        git(wt, "checkout", "--", "w.txt")
        (wt / "scratch.log").write_text("untracked files do not block\n", encoding="utf-8")
        row = self.by_branch(self.rebase())["feat/wt"]
        self.assertEqual(row["result"], "rebased")
        self.assertEqual(git(wt, "rev-parse", "HEAD"), git(self.repo, "rev-parse", "feat/wt"))
        self.assertTrue((wt / "main.txt").exists())

    def test_dry_run_predicts_without_changing_anything(self) -> None:
        clean_old = self.branch_with_commit("feat/clean", "c.txt", "c\n")
        clash_old = self.branch_with_commit("feat/clash", "README.md", "line one\nbranch\n")
        self.commit_file("README.md", "line one\nmain\n", "main edit")
        self.add_task("t1", "in_progress", "feat/clean")
        self.add_task("t2", "in_progress", "feat/clash")

        rows = self.by_branch(self.rebase("--dry-run"))
        self.assertEqual(rows["feat/clean"]["result"], "would_rebase")
        self.assertEqual(rows["feat/clash"]["result"], "would_conflict")
        self.assertEqual(rows["feat/clash"]["files"], ["README.md"])
        self.assertEqual(git(self.repo, "rev-parse", "feat/clean"), clean_old)
        self.assertEqual(git(self.repo, "rev-parse", "feat/clash"), clash_old)
        self.assertEqual(self.posts("/v1/git/event") + self.posts("/v1/messages"), [])

    def test_uses_agent_nick_as_committer_when_repo_has_no_identity(self) -> None:
        self.branch_with_commit("feat/a", "a.txt", "a\n")
        self.commit_file("main.txt", "x\n", "main moves")
        git(self.repo, "config", "--unset", "user.email")
        git(self.repo, "config", "--unset", "user.name")
        self.add_task("t_a", "in_progress", "feat/a")
        isolated = {"GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"}
        (self.cfg_dir / "profiles.json").write_text(
            json.dumps({"default": "p", "profiles": {"p": {"nick": "rebaser", "token": "tok", "server": self.url}}}),
            encoding="utf-8",
        )
        with mock.patch.dict(os.environ, isolated):
            row = self.by_branch(self.rebase())["feat/a"]
        self.assertEqual(row["result"], "rebased", row)
        self.assertEqual(git(self.repo, "log", "-1", "--format=%cn|%an", "feat/a"), "rebaser|Dev")

    def test_hook_mode_is_silent_noop_off_the_base_branch(self) -> None:
        self.branch_with_commit("feat/a", "a.txt", "a\n")
        self.add_task("t_a", "in_progress", "feat/a")
        git(self.repo, "checkout", "-q", "feat/a")
        code, out, err = self.run_cli("git", "rebase-siblings", "--hook", "--path", str(self.repo))
        self.assertEqual((code, out, err), (0, "", ""))
        self.assertFalse(any(r["path"].startswith("/v1/tasks") for r in self.hub.requests))

        with mock.patch.dict(os.environ, {"AIRCD_NO_AUTO_REBASE": "1"}):
            git(self.repo, "checkout", "-q", "main")
            self.commit_file("main.txt", "x\n", "main moves")
            self.hub.requests.clear()
            code, out, _ = self.run_cli("git", "rebase-siblings", "--hook", "--path", str(self.repo))
        self.assertEqual((code, out, self.hub.requests), (0, "", []))

    def test_hook_mode_never_fails_the_merge(self) -> None:
        self.hub.routes[("GET", TASKS_PATH)] = (500, {"error": "boom"})
        code, out, err = self.run_cli("git", "rebase-siblings", "--hook", "--path", str(self.repo))
        self.assertEqual(code, 0)
        self.assertIn("sibling rebase skipped", err)

    def test_post_merge_hook_rebases_siblings_end_to_end(self) -> None:
        (self.cfg_dir / "profiles.json").write_text(
            json.dumps({"default": "p", "profiles": {"p": {"nick": "pm", "token": "tok", "server": self.url}}}),
            encoding="utf-8",
        )
        code, out, err = self.run_cli("--profile", "p", "git", "install-hooks", str(self.repo), "--repo", "sample-repo", "--channel", "#cams")
        self.assertEqual(code, 0, err)
        hook = (self.repo / ".git" / "hooks" / "post-merge").read_text(encoding="utf-8")
        self.assertIn("rebase-siblings --hook", hook)

        self.branch_with_commit("feat/sibling", "s.txt", "s\n")
        self.branch_with_commit("feat/done", "d.txt", "d\n")
        self.add_task("t_sib", "in_progress", "feat/sibling")
        self.hub.requests.clear()
        git(self.repo, "merge", "-q", "--no-ff", "-m", "Merge feat/done", "feat/done")

        main_sha = git(self.repo, "rev-parse", "main")
        self.assertEqual(git(self.repo, "merge-base", "feat/sibling", "main"), main_sha)
        rebase_events = [e for e in self.posts("/v1/git/event") if e["event"] == "rebase"]
        self.assertEqual(len(rebase_events), 1)
        self.assertIn("feat/sibling", rebase_events[0]["summary"])
        # The replayed commits must not fire post-commit hook events.
        self.assertEqual([e for e in self.posts("/v1/git/event") if e["event"] == "commit"], [])

    def test_install_hooks_can_opt_out(self) -> None:
        (self.cfg_dir / "profiles.json").write_text(
            json.dumps({"default": "p", "profiles": {"p": {"nick": "pm", "token": "tok", "server": self.url}}}),
            encoding="utf-8",
        )
        code, _, err = self.run_cli("--profile", "p", "git", "install-hooks", str(self.repo), "--no-auto-rebase")
        self.assertEqual(code, 0, err)
        hook = (self.repo / ".git" / "hooks" / "post-merge").read_text(encoding="utf-8")
        self.assertNotIn("rebase-siblings", hook)
        self.assertIn("emit-hook", hook)


if __name__ == "__main__":
    unittest.main()
