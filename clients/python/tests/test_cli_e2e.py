"""End-to-end tests using stdlib unittest against the compiled aircd binary."""

from __future__ import annotations

import hashlib
import hmac
import http.server
import json
import os
import queue
import socket
import socketserver
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[3]
CLI_PY_ROOT = REPO_ROOT / "clients" / "python"


def _free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return int(s.getsockname()[1])


def _binary_path() -> Path:
    override = os.environ.get("AIRCD_BIN")
    if override:
        return Path(override)
    candidates = [
        REPO_ROOT / "target" / "debug" / ("aircd.exe" if os.name == "nt" else "aircd"),
        REPO_ROOT / "target" / "release" / ("aircd.exe" if os.name == "nt" else "aircd"),
    ]
    for c in candidates:
        if c.exists():
            return c
    raise RuntimeError(f"aircd binary not found; built cargo test/build first? Checked {candidates}")


class AircdCliE2ETest(unittest.TestCase):
    server_proc: subprocess.Popen[str]
    tmp_dir: tempfile.TemporaryDirectory[str]
    cfg_dir: Path
    irc_port: int
    http_port: int
    base_url: str
    hook_queue: queue.Queue[tuple[dict[str, str], bytes]]
    hook_httpd: socketserver.TCPServer
    hook_port: int

    @classmethod
    def setUpClass(cls) -> None:
        cls.tmp_dir = tempfile.TemporaryDirectory()
        tmp_path = Path(cls.tmp_dir.name)
        cls.cfg_dir = tmp_path / "cfg"
        cls.cfg_dir.mkdir(parents=True, exist_ok=True)
        db_path = tmp_path / "test_e2e.sqlite3"

        cls.irc_port = _free_port()
        cls.http_port = _free_port()
        cls.base_url = f"http://127.0.0.1:{cls.http_port}"

        bin_path = _binary_path()
        cls.server_proc = subprocess.Popen(
            [
                str(bin_path),
                "--bind",
                f"127.0.0.1:{cls.irc_port}",
                "--http-bind",
                f"127.0.0.1:{cls.http_port}",
                "--db",
                str(db_path),
                "--open-registration",
            ],
            cwd=str(REPO_ROOT),
            env={**os.environ, "AIRCD_SEED_DEMO": "1"},
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )

        # Wait up to 10s for /healthz
        deadline = time.time() + 10.0
        ready = False
        while time.time() < deadline:
            if cls.server_proc.poll() is not None:
                out, err = cls.server_proc.communicate()
                raise RuntimeError(f"aircd exited early: {out}\n{err}")
            try:
                with urllib.request.urlopen(f"{cls.base_url}/healthz", timeout=1.0) as r:
                    if r.status == 200:
                        ready = True
                        break
            except Exception:
                time.sleep(0.1)
        if not ready:
            cls.server_proc.terminate()
            raise RuntimeError("aircd /healthz did not become ready in time")

        # Start stdlib webhook receiver
        cls.hook_queue = queue.Queue()
        q = cls.hook_queue

        class ThreadedHook(socketserver.ThreadingMixIn, http.server.HTTPServer):
            daemon_threads = True

        class HookHandler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                length = int(self.headers.get("Content-Length", "0"))
                raw = self.rfile.read(length)
                hdrs = {k.lower(): v for k, v in self.headers.items()}
                q.put((hdrs, raw))
                self.send_response(200)
                self.end_headers()
                self.wfile.write(b'{"ok":true}')

            def log_message(self, format, *args):
                return

        cls.hook_httpd = ThreadedHook(("127.0.0.1", 0), HookHandler)
        cls.hook_port = cls.hook_httpd.server_address[1]
        threading.Thread(target=cls.hook_httpd.serve_forever, daemon=True).start()

    @classmethod
    def tearDownClass(cls) -> None:
        try:
            cls.hook_httpd.shutdown()
            cls.hook_httpd.server_close()
        except Exception:
            pass
        try:
            cls.server_proc.terminate()
            cls.server_proc.wait(timeout=5)
        except Exception:
            cls.server_proc.kill()
        cls.tmp_dir.cleanup()

    def _run_cli(self, *cli_args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
        env = dict(os.environ)
        env["AIRCD_CONFIG_DIR"] = str(self.cfg_dir)
        env["PYTHONPATH"] = str(CLI_PY_ROOT) + (
            os.pathsep + env["PYTHONPATH"] if env.get("PYTHONPATH") else ""
        )
        proc = subprocess.run(
            [sys.executable, "-m", "aircd.cli", "--server", self.base_url, *cli_args],
            env=env,
            cwd=str(REPO_ROOT),
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
        )
        if check and proc.returncode != 0:
            self.fail(
                f"CLI {cli_args} exited {proc.returncode}\nSTDOUT:\n{proc.stdout}\nSTDERR:\n{proc.stderr}"
            )
        return proc

    def _drain_hooks(self) -> None:
        while not self.hook_queue.empty():
            try:
                self.hook_queue.get_nowait()
            except queue.Empty:
                break

    def _next_hook(self, secret: str, timeout: float = 5.0) -> dict:
        hdrs, raw = self.hook_queue.get(timeout=timeout)
        sig = hdrs.get("x-aircd-signature-256", "")
        expected = "sha256=" + hmac.new(secret.encode("utf-8"), raw, hashlib.sha256).hexdigest()
        self.assertTrue(hmac.compare_digest(expected, sig), f"bad webhook signature: {sig}")
        return json.loads(raw.decode("utf-8"))

    def test_01_welcome_page_and_registration_flow(self) -> None:
        with urllib.request.urlopen(self.base_url + "/", timeout=3.0) as resp:
            html = resp.read().decode("utf-8")
        self.assertIn("aircd", html)
        self.assertIn(str(self.irc_port), html)
        self.assertNotIn("human-token", html)

        wel = json.loads(self._run_cli("welcome", "--format", "json").stdout)
        self.assertTrue(wel["ok"])
        self.assertEqual(wel["connection"]["irc_port"], self.irc_port)

        reg_codex = json.loads(
            self._run_cli(
                "--profile", "codex", "--json",
                "register", "--nick", "codex", "--role", "leader", "--channels", "#general,#wardrobe",
            ).stdout
        )
        self.assertEqual(reg_codex["principal"]["role"], "leader")
        self.assertTrue(reg_codex["token"].startswith("airc_"))

        # Duplicate nick fails cleanly.
        dup = self._run_cli("register", "--nick", "codex", check=False)
        self.assertNotEqual(dup.returncode, 0)
        self.assertIn("409", dup.stderr)

        reg_claude = json.loads(
            self._run_cli(
                "--profile", "claude", "--json",
                "register", "--nick", "claude", "--role", "reviewer", "--channels", "#general,#wardrobe",
            ).stdout
        )
        self.assertEqual(reg_claude["principal"]["role"], "reviewer")

    def test_02_webhooks_messages_dm_irc_roles_locks_and_reports(self) -> None:
        self._drain_hooks()
        hook_url = f"http://127.0.0.1:{self.hook_port}/hook"
        wh = json.loads(
            self._run_cli(
                "--profile", "claude", "--json",
                "webhook", "set", hook_url, "--notify-when", "always",
            ).stdout
        )
        secret = wh["secret"]

        # Test ping
        test_res = json.loads(self._run_cli("--profile", "claude", "--json", "webhook", "test").stdout)
        self.assertEqual(test_res["delivery"]["status"], "delivered")
        ping_ev = self._next_hook(secret)
        self.assertEqual(ping_ev["event"], "connection_test")

        # Live IRC socket joins #wardrobe and verifies live PRIVMSG delivery
        codex_profile = json.loads((self.cfg_dir / "profiles.json").read_text())["profiles"]["codex"]
        with socket.create_connection(("127.0.0.1", self.irc_port), timeout=5.0) as irc_sock:
            irc_sock.settimeout(4.0)
            f = irc_sock.makefile("r", encoding="utf-8", errors="replace")
            irc_sock.sendall(
                (
                    f"PASS {codex_profile['token']}\r\n"
                    f"NICK codex\r\n"
                    f"USER codex 0 * :codex\r\n"
                ).encode("utf-8")
            )
            welcomed = False
            while not welcomed:
                line = f.readline()
                self.assertTrue(line, "IRC connection dropped")
                if " 001 " in line:
                    welcomed = True

            # Send DM via CLI from claude -> @codex, confirm IRC live receipt!
            self._run_cli("--profile", "claude", "send", "@codex", "DM check: IRC live works")
            seen_dm = False
            deadline = time.time() + 3.0
            while time.time() < deadline:
                line = f.readline()
                if "PRIVMSG codex :DM check: IRC live works" in line:
                    seen_dm = True
                    break
            self.assertTrue(seen_dm, "IRC socket did not receive live DM")

        # Channel mention -> triggers signed 'mention' webhook to claude
        self._run_cli("--profile", "codex", "send", "#wardrobe", "@claude check PR #12")
        ev = self._next_hook(secret)
        self.assertEqual(ev["event"], "mention")
        self.assertEqual(ev["from"], "codex")

        # Roles via CLI
        self._run_cli("--profile", "codex", "role", "claude", "coordinator", "--channel", "#wardrobe")
        ev_role = self._next_hook(secret)
        self.assertEqual(ev_role["event"], "role")

        # Dev equipment lock contention
        self._run_cli(
            "--profile", "codex", "lock", "quest-headset-1",
            "--channel", "#wardrobe", "--ttl", "600", "--note", "VR test",
        )
        ev_lock = self._next_hook(secret)
        self.assertEqual(ev_lock["event"], "lock")

        conflict = self._run_cli(
            "--profile", "claude", "lock", "quest-headset-1", "--channel", "#wardrobe",
            check=False,
        )
        self.assertNotEqual(conflict.returncode, 0)
        self.assertIn("409", conflict.stderr)

        # Unlock then acquire
        self._run_cli("--profile", "codex", "unlock", "quest-headset-1")
        _ = self._next_hook(secret)
        self._run_cli("--profile", "claude", "lock", "quest-headset-1", "--channel", "#wardrobe")
        self._run_cli("--profile", "claude", "unlock", "quest-headset-1")

        # Report creation and lookup
        rep = json.loads(
            self._run_cli(
                "--profile", "codex", "--json",
                "report", "create",
                "--channel", "#wardrobe",
                "--title", "E2E synthesis ok",
                "--status", "completed",
                "--body", "Detailed verification markdown report.",
            ).stdout
        )
        rep_id = rep["report"]["id"]
        ev_rep = self._next_hook(secret)
        self.assertEqual(ev_rep["event"], "report")
        self.assertEqual(ev_rep["report"]["id"], rep_id)

        shown = json.loads(self._run_cli("--profile", "claude", "--json", "report", "show", rep_id).stdout)
        self.assertEqual(shown["report"]["body"], "Detailed verification markdown report.")

    def test_03_git_workspace_local_hook_worktree_and_pr_report(self) -> None:
        self._drain_hooks()
        repo_dir = Path(self.tmp_dir.name) / "sample-repo"
        repo_dir.mkdir()
        for cmd in (
            ["git", "init", "-b", "main"],
            ["git", "config", "user.email", "codex@example.local"],
            ["git", "config", "user.name", "Codex"],
        ):
            subprocess.run(cmd, cwd=repo_dir, check=True, capture_output=True)
        (repo_dir / "README.md").write_text("init\n", encoding="utf-8")
        subprocess.run(["git", "add", "README.md"], cwd=repo_dir, check=True, capture_output=True)
        subprocess.run(["git", "commit", "-m", "initial commit"], cwd=repo_dir, check=True, capture_output=True)

        # Connect repo & install hooks
        init_res = json.loads(
            self._run_cli(
                "--profile", "codex", "--json",
                "git", "init-workspace", str(repo_dir), "--channel", "#wardrobe",
            ).stdout
        )
        self.assertEqual(init_res["git_repo"]["name"], "sample-repo")
        self.assertEqual(len(init_res["hooks"]), 2)

        claude_hook = json.loads(
            self._run_cli("--profile", "claude", "--json", "webhook", "status").stdout
        )["webhook"]
        # Fetch claude secret from profile file
        secret = json.loads((self.cfg_dir / "profiles.json").read_text())["profiles"]["claude"]["webhook_secret"]
        self.assertTrue(claude_hook["enabled"])

        # Create a worktree + branch lock, commit inside worktree, confirm post-commit hook fires webhook!
        wt_res = json.loads(
            self._run_cli(
                "--profile", "codex", "--json",
                "git", "worktree-setup", str(repo_dir), "feat/ui-polish", "--channel", "#wardrobe", "--lock",
            ).stdout
        )
        wt_dir = Path(wt_res["worktree"])
        self.assertTrue(wt_dir.exists())
        _ = self._next_hook(secret)  # lock event
        _ = self._next_hook(secret)  # worktree event

        (wt_dir / "feature.txt").write_text("new feature\n", encoding="utf-8")
        subprocess.run(["git", "add", "feature.txt"], cwd=wt_dir, check=True, capture_output=True)
        subprocess.run(["git", "commit", "-m", "add feature file"], cwd=wt_dir, check=True, capture_output=True)

        commit_ev = self._next_hook(secret, timeout=8.0)
        self.assertEqual(commit_ev["event"], "git")
        self.assertEqual(commit_ev["git"]["repo"], "sample-repo")
        self.assertEqual(commit_ev["git"]["branch"], "feat/ui-polish")
        self.assertIn("add feature file", commit_ev["message"])

        # Submit PR readiness report from the worktree
        pr_rep = json.loads(
            self._run_cli(
                "--profile", "codex", "--json",
                "git", "pr-report", str(wt_dir), "--base", "main", "--channel", "#wardrobe", "--status", "completed",
            ).stdout
        )
        self.assertEqual(pr_rep["report"]["git_branch"], "feat/ui-polish")
        pr_ev = self._next_hook(secret)
        self.assertEqual(pr_ev["event"], "report")

    def test_04_task_repo_validation_and_task_worktree_lifecycle(self) -> None:
        self._drain_hooks()
        repo_dir = Path(self.tmp_dir.name) / "wt-repo"
        repo_dir.mkdir()
        for cmd in (
            ["git", "init", "-b", "main"],
            ["git", "config", "user.email", "codex@example.local"],
            ["git", "config", "user.name", "Codex"],
        ):
            subprocess.run(cmd, cwd=repo_dir, check=True, capture_output=True)
        (repo_dir / "README.md").write_text("wt init\n", encoding="utf-8")
        subprocess.run(["git", "add", "README.md"], cwd=repo_dir, check=True, capture_output=True)
        subprocess.run(["git", "commit", "-m", "initial commit"], cwd=repo_dir, check=True, capture_output=True)

        self._run_cli("--profile", "codex", "--json", "project", "create", "#wtproj")
        self._run_cli("--profile", "codex", "--json", "project", "add", "#wtproj", "claude")
        reg = json.loads(
            self._run_cli(
                "--profile", "codex", "--json",
                "git", "register", str(repo_dir), "--channel", "#wtproj",
            ).stdout
        )
        self.assertEqual(reg["git_repo"]["name"], "wt-repo")

        # Repo attached to #wardrobe (sample-repo) is rejected for #wtproj
        bad = self._run_cli(
            "--profile", "codex",
            "task", "create", "#wtproj", "Wrong repo", "--repo", "sample-repo",
            check=False,
        )
        self.assertNotEqual(bad.returncode, 0)
        self.assertIn("400", bad.stderr)
        self.assertIn("#wardrobe", bad.stderr)

        created_task = json.loads(
            self._run_cli(
                "--profile", "codex", "--json",
                "task", "create", "#wtproj", "Worktree feature",
                "--assign", "claude", "--repo", "wt-repo", "--branch", "feat/e2e-wt",
            ).stdout
        )["task"]
        task_id = created_task["id"]
        self.assertEqual(created_task["repo"], "wt-repo")

        wt = json.loads(
            self._run_cli(
                "--profile", "codex", "--json",
                "task", "worktree", task_id, "--lock",
            ).stdout
        )
        expected_wt = (repo_dir / ".aircd" / "worktrees" / "feat" / "e2e-wt").resolve()
        self.assertTrue(wt["created"])
        self.assertFalse(wt["reused"])
        self.assertEqual(Path(wt["worktree"]).resolve(), expected_wt)
        self.assertTrue((expected_wt / ".git").exists())
        self.assertIsNotNone(wt["lock"])

        reused = json.loads(
            self._run_cli(
                "--profile", "codex", "--json",
                "task", "worktree", task_id,
            ).stdout
        )
        self.assertFalse(reused["created"])
        self.assertTrue(reused["reused"])

        removed = json.loads(
            self._run_cli(
                "--profile", "codex", "--json",
                "task", "worktree", task_id, "--remove", "--delete-branch",
            ).stdout
        )
        self.assertTrue(removed["removed"])
        self.assertFalse(expected_wt.exists())

    def test_05_repeated_login_rotate_for_same_account(self) -> None:
        reg = json.loads(
            self._run_cli(
                "--profile", "rotator", "--json",
                "register", "--nick", "rotator", "--channels", "#general",
            ).stdout
        )
        initial_token = reg["token"]
        self.assertTrue(initial_token.startswith("airc_"))

        tokens = [initial_token]
        for idx in range(3):
            res = json.loads(
                self._run_cli(
                    "--profile", "rotator", "--json",
                    "login", "--rotate", "--channels", "#general",
                ).stdout
            )
            rotated = res.get("rotated_token")
            self.assertIsNotNone(rotated, f"rotation #{idx + 1} did not return rotated_token")
            self.assertNotIn(rotated, tokens, f"rotation #{idx + 1} reused an earlier token")
            tokens.append(rotated)

            saved = json.loads((self.cfg_dir / "profiles.json").read_text())["profiles"]["rotator"]["token"]
            self.assertEqual(saved, rotated)

            me = json.loads(self._run_cli("--profile", "rotator", "--json", "whoami").stdout)
            self.assertEqual(me["principal"]["nick"], "rotator")

        # Non-rotating login preserves current token, and a subsequent rotate still succeeds.
        no_rot = json.loads(
            self._run_cli(
                "--profile", "rotator", "--json",
                "login", "--channels", "#general",
            ).stdout
        )
        self.assertIsNone(no_rot.get("rotated_token"))
        saved_after_plain = json.loads((self.cfg_dir / "profiles.json").read_text())["profiles"]["rotator"]["token"]
        self.assertEqual(saved_after_plain, tokens[-1])

        res_after = json.loads(
            self._run_cli(
                "--profile", "rotator", "--json",
                "login", "--rotate", "--channels", "#general",
            ).stdout
        )
        self.assertIsNotNone(res_after.get("rotated_token"))
        self.assertNotIn(res_after["rotated_token"], tokens)

    def test_06_task_priority_queue_and_inbox_dispatch(self) -> None:
        self._run_cli(
            "--profile", "codex",
            "project", "create", "prio", "--name", "Priority Queue",
        )
        self._run_cli(
            "--profile", "codex",
            "project", "add", "prio", "claude", "--role", "member",
        )
        t_low = json.loads(
            self._run_cli(
                "--profile", "codex", "--json",
                "task", "create", "#prio", "Low task",
                "--priority", "low", "--assign", "claude",
            ).stdout
        )["task"]
        t_normal = json.loads(
            self._run_cli(
                "--profile", "codex", "--json",
                "task", "create", "#prio", "Normal task",
                "--assign", "claude",
            ).stdout
        )["task"]
        t_urgent = json.loads(
            self._run_cli(
                "--profile", "codex", "--json",
                "task", "create", "#prio", "Urgent task",
                "--priority", "urgent", "--assign", "claude",
            ).stdout
        )["task"]
        t_high = json.loads(
            self._run_cli(
                "--profile", "codex", "--json",
                "task", "create", "#prio", "High task",
                "--priority", "high", "--assign", "claude",
            ).stdout
        )["task"]

        self.assertEqual(t_low["priority"], "low")
        self.assertEqual(t_normal["priority"], "normal")
        self.assertEqual(t_urgent["priority"], "urgent")
        self.assertEqual(t_high["priority"], "high")

        listed = json.loads(
            self._run_cli("--profile", "claude", "--json", "task", "list", "#prio").stdout
        )["tasks"]
        self.assertEqual(
            [t["id"] for t in listed],
            [t_urgent["id"], t_high["id"], t_normal["id"], t_low["id"]],
        )

        only_high = json.loads(
            self._run_cli(
                "--profile", "claude", "--json",
                "task", "list", "#prio", "--priority", "high",
            ).stdout
        )["tasks"]
        self.assertEqual([t["id"] for t in only_high], [t_high["id"]])

        inbox = json.loads(
            self._run_cli("--profile", "claude", "--json", "inbox").stdout
        )["inbox"]
        self.assertEqual(inbox["next_task"]["id"], t_urgent["id"])
        prio_proj = next(p for p in inbox["projects"] if p["channel"] == "#prio")
        self.assertEqual(
            [t["id"] for t in prio_proj["tasks"]],
            [t_urgent["id"], t_high["id"], t_normal["id"], t_low["id"]],
        )

        # Bump t_low to urgent; FIFO places it ahead of t_urgent.
        bumped = json.loads(
            self._run_cli(
                "--profile", "codex", "--json",
                "task", "priority", t_low["id"], "urgent",
            ).stdout
        )["task"]
        self.assertEqual(bumped["priority"], "urgent")

        inbox_after = json.loads(
            self._run_cli("--profile", "claude", "--json", "inbox").stdout
        )["inbox"]
        prio_proj_after = next(p for p in inbox_after["projects"] if p["channel"] == "#prio")
        self.assertEqual(
            [t["id"] for t in prio_proj_after["tasks"]],
            [t_low["id"], t_urgent["id"], t_high["id"], t_normal["id"]],
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
