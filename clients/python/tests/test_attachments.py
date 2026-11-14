"""End-to-end tests of `airc task attach / attachments / download` against the compiled hub."""

from __future__ import annotations

import hashlib
import json
import os
import subprocess
import sys
import tempfile
import time
import unittest
import urllib.request
from pathlib import Path

from tests.test_cli_e2e import CLI_PY_ROOT, REPO_ROOT, _binary_path, _free_port


class TaskAttachmentsE2ETest(unittest.TestCase):
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
                "--db", str(cls.tmp / "attach.sqlite3"),
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

    @classmethod
    def tearDownClass(cls) -> None:
        cls.server_proc.terminate()
        try:
            cls.server_proc.wait(timeout=5)
        except Exception:
            cls.server_proc.kill()
        cls.tmp_dir.cleanup()

    def cli(self, *args: str, check: bool = True, stdin: bytes | None = None) -> subprocess.CompletedProcess[bytes]:
        env = dict(os.environ)
        env["AIRCD_CONFIG_DIR"] = str(self.cfg_dir)
        env["PYTHONPATH"] = str(CLI_PY_ROOT)
        env["PYTHONUTF8"] = "1"
        proc = subprocess.run(
            [sys.executable, "-m", "aircd.cli", "--server", self.base_url, *args],
            env=env, cwd=str(self.tmp), input=stdin, capture_output=True,
        )
        if check and proc.returncode != 0:
            self.fail(f"CLI {args} exited {proc.returncode}\n{proc.stdout!r}\n{proc.stderr!r}")
        return proc

    def jcli(self, *args: str) -> dict:
        return json.loads(self.cli(*args).stdout.decode("utf-8"))

    def test_attach_list_show_download_and_review_with_attach(self) -> None:
        self.jcli("--profile", "pm", "--json", "register", "--nick", "attpm", "--role", "leader")
        self.jcli("--profile", "dev", "--json", "register", "--nick", "attdev", "--role", "builder")
        self.jcli("--profile", "out", "--json", "register", "--nick", "attout", "--role", "builder")
        self.jcli("--profile", "pm", "--json", "project", "create", "#attproj")
        self.jcli("--profile", "pm", "--json", "project", "add", "#attproj", "attdev")
        task_id = self.jcli(
            "--profile", "pm", "--json", "task", "create", "#attproj", "Evidence", "--assign", "attdev",
        )["task"]["id"]

        src = self.tmp / "src"
        src.mkdir()
        log = src / "cargo-test.log"
        log.write_bytes(b"running 9 tests\ntest result: ok. 9 passed\n")
        shot = src / "shot.png"
        shot.write_bytes(b"\x89PNG\r\n\x1a\n\x00binary")

        out = self.jcli(
            "--profile", "dev", "--json", "task", "attach", task_id, str(log), str(shot),
            "--note", "cargo test",
        )
        self.assertEqual([a["filename"] for a in out["attachments"]], ["cargo-test.log", "shot.png"])
        self.assertEqual(out["attachments"][1]["content_type"], "image/png")
        log_id = out["attachments"][0]["id"]

        human = self.cli("--profile", "dev", "task", "attach", task_id, "-", "--name", "stdin.txt",
                         "--kind", "evidence", stdin=b"from stdin").stdout.decode()
        self.assertIn("[evidence] stdin.txt", human)

        listed = self.cli("--profile", "pm", "task", "attachments", task_id).stdout.decode()
        self.assertIn(log_id, listed)
        self.assertIn("cargo-test.log", listed)
        self.assertIn("by @attdev - cargo test", listed)
        shown = self.cli("--profile", "pm", "task", "show", task_id).stdout.decode()
        self.assertIn("attachments (3)", shown)
        self.assertIn("attach - -> - by @attdev - log cargo-test.log", shown)

        saved = self.jcli("--profile", "pm", "--json", "task", "download", task_id, log_id)
        dest = self.tmp / "cargo-test.log"
        self.assertEqual(Path(saved["path"]).name, "cargo-test.log")
        self.assertEqual(saved["sha256"], hashlib.sha256(log.read_bytes()).hexdigest())
        self.assertEqual(dest.read_bytes(), log.read_bytes())
        # The default destination is the original name in the cwd, which now exists.
        again = self.cli("--profile", "pm", "task", "download", task_id, log_id, check=False)
        self.assertNotEqual(again.returncode, 0)
        self.assertIn(b"already exists", again.stderr)
        dl_dir = self.tmp / "dl"
        dl_dir.mkdir()
        self.cli("--profile", "pm", "task", "download", task_id, log_id, "-o", str(dl_dir))
        self.assertEqual((dl_dir / "cargo-test.log").read_bytes(), dest.read_bytes())
        raw = self.cli("--profile", "pm", "task", "download", task_id, out["attachments"][1]["id"], "-o", "-").stdout
        self.assertEqual(raw, shot.read_bytes())

        denied = self.cli("--profile", "out", "task", "attach", task_id, str(log), check=False)
        self.assertNotEqual(denied.returncode, 0)
        self.assertIn(b"403", denied.stderr)
        missing = self.cli("--profile", "dev", "task", "attach", task_id, str(self.tmp / "nope.log"), check=False)
        self.assertIn(b"no such file", missing.stderr)
        big = self.tmp / "big.log"
        big.write_bytes(b"x" * (4 * 1024 * 1024 + 1))
        too_big = self.cli("--profile", "dev", "task", "attach", task_id, str(big), check=False)
        self.assertNotEqual(too_big.returncode, 0)
        self.assertIn(b"trim the log", too_big.stderr)

        self.cli("--profile", "dev", "task", "status", task_id, "testing")
        report = self.tmp / "unittest.log"
        report.write_text("Ran 12 tests\nOK\n", encoding="utf-8")
        moved = self.jcli(
            "--profile", "dev", "--json", "task", "status", task_id, "review",
            "--branch", "feat/x", "--note", "unittest: 12 OK (log attached)", "--attach", str(report),
        )
        self.assertEqual(moved["task"]["status"], "review")
        self.assertEqual(moved["attachments"][0]["filename"], "unittest.log")
        final = self.jcli("--profile", "pm", "--json", "task", "attachments", task_id)["attachments"]
        self.assertEqual(len(final), 4)


if __name__ == "__main__":
    unittest.main()
