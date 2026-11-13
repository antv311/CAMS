"""Client templates: the HTTP client library and the `airc template` commands.

The library tests run against a stdlib fake hub and check the wire contract.
The CLI tests run the real aircd binary with an owner key, and are skipped
when the binary has not been built.
"""

from __future__ import annotations

import http.server
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
from pathlib import Path
from typing import Any

from aircd.client import AircdHttpClient, AircdHttpError

REPO_ROOT = Path(__file__).resolve().parents[3]
CLI_PY_ROOT = REPO_ROOT / "clients" / "python"
OWNER_KEY = "owner-key-for-template-tests"


class _FakeHub(http.server.ThreadingHTTPServer):
    """Records each request and answers with the next queued (status, body)."""

    daemon_threads = True

    def __init__(self) -> None:
        super().__init__(("127.0.0.1", 0), _FakeHandler)
        self.requests: list[dict[str, Any]] = []
        self.replies: list[tuple[int, Any]] = []


class _FakeHandler(http.server.BaseHTTPRequestHandler):
    server: _FakeHub

    def _handle(self) -> None:
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else b""
        self.server.requests.append(
            {
                "method": self.command,
                "path": self.path,
                "headers": {k.lower(): v for k, v in self.headers.items()},
                "body": json.loads(body) if body else None,
            }
        )
        status, reply = self.server.replies.pop(0) if self.server.replies else (200, {"ok": True})
        raw = json.dumps(reply).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    do_GET = do_POST = do_DELETE = _handle

    def log_message(self, *_args: Any) -> None:
        pass


class HttpClientTemplateTest(unittest.TestCase):
    def setUp(self) -> None:
        self.hub = _FakeHub()
        threading.Thread(target=self.hub.serve_forever, daemon=True).start()
        self.url = f"http://127.0.0.1:{self.hub.server_address[1]}"
        self.client = AircdHttpClient(self.url, token="tok", invite="owner")

    def tearDown(self) -> None:
        self.hub.shutdown()
        self.hub.server_close()

    def test_list_and_get_send_the_bearer_token_only(self) -> None:
        self.hub.replies = [
            (200, {"ok": True, "templates": [{"id": "claude", "installed": True}]}),
            (200, {"ok": True, "template": {"id": "a b/c", "installed": False}}),
        ]
        self.assertEqual(self.client.list_templates(), [{"id": "claude", "installed": True}])
        self.assertEqual(self.client.get_template("a b/c")["id"], "a b/c")
        first, second = self.hub.requests
        self.assertEqual((first["method"], first["path"]), ("GET", "/v1/templates"))
        self.assertEqual(first["headers"]["authorization"], "Bearer tok")
        self.assertNotIn("x-aircd-invite", first["headers"])
        self.assertEqual(second["path"], "/v1/templates/a%20b%2Fc")

    def test_set_sends_owner_key_and_drops_read_only_fields(self) -> None:
        saved = {"id": "claude", "command": "claude", "installed": True}
        self.hub.replies = [(200, {"ok": True, "template": saved})]
        result = self.client.set_template(
            "claude",
            {"id": "x", "installed": True, "created_at": 1, "updated_at": 2, "command": "claude", "models": ["m"]},
        )
        self.assertEqual(result, saved)
        req = self.hub.requests[0]
        self.assertEqual((req["method"], req["path"]), ("POST", "/v1/templates/claude"))
        self.assertEqual(req["headers"]["x-aircd-invite"], "owner")
        self.assertEqual(req["body"], {"command": "claude", "models": ["m"]})

    def test_delete_and_models(self) -> None:
        self.hub.replies = [
            (200, {"ok": True, "deleted": True}),
            (200, {"ok": True, "template": "agy", "models": [{"id": "m1", "label": None}], "warning": None}),
        ]
        self.assertTrue(self.client.delete_template("agy"))
        models = self.client.list_template_models("agy")
        self.assertEqual(models["models"], [{"id": "m1", "label": None}])
        delete, listing = self.hub.requests
        self.assertEqual((delete["method"], delete["path"]), ("DELETE", "/v1/templates/agy"))
        self.assertEqual(delete["headers"]["x-aircd-invite"], "owner")
        self.assertEqual((listing["method"], listing["path"]), ("POST", "/v1/owner/models"))
        self.assertEqual(listing["body"], {"template": "agy"})
        self.assertEqual(listing["headers"]["x-aircd-invite"], "owner")

    def test_spawn_worker_with_owner_key(self) -> None:
        created = {"ok": True, "account": {"principal": {"nick": "w1"}, "token": "new"}, "http_url": "http://h:1"}
        self.hub.replies = [(201, created)]
        result = self.client.spawn_worker("a b", "w1", "opus", channels=["#p", "#q"], role="pm")
        self.assertEqual(result, created)
        req = self.hub.requests[0]
        self.assertEqual((req["method"], req["path"]), ("POST", "/v1/templates/a%20b/spawn"))
        self.assertEqual(req["headers"]["x-aircd-invite"], "owner")
        self.assertEqual(req["body"], {"nick": "w1", "model": "opus", "channels": ["#p", "#q"], "role": "pm"})

    def test_spawn_worker_without_owner_key_uses_the_token(self) -> None:
        self.hub.replies = [(201, {"ok": True, "account": {}})]
        AircdHttpClient(self.url, token="tok").spawn_worker("claude", "w2", "sonnet")
        req = self.hub.requests[0]
        self.assertEqual(req["path"], "/v1/templates/claude/spawn")
        self.assertEqual(req["headers"]["authorization"], "Bearer tok")
        self.assertNotIn("x-aircd-invite", req["headers"])
        self.assertEqual(req["body"], {"nick": "w2", "model": "sonnet"})

    def test_spawn_worker_errors(self) -> None:
        self.hub.replies = [(403, {"ok": False, "error": "only the owner or a manager of #p can provision"})]
        with self.assertRaises(AircdHttpError) as caught:
            self.client.spawn_worker("claude", "w3", "m", channels=["#p"])
        self.assertEqual(caught.exception.status, 403)
        self.assertIn("only the owner", str(caught.exception))
        with self.assertRaises(AircdHttpError):
            self.client.spawn_worker(" ", "w3", "m")
        self.assertEqual(len(self.hub.requests), 1)

    def test_errors_carry_status_and_server_message(self) -> None:
        self.hub.replies = [(404, {"ok": False, "error": "no client template named 'nope'"})]
        with self.assertRaises(AircdHttpError) as caught:
            self.client.get_template("nope")
        self.assertEqual(caught.exception.status, 404)
        self.assertIn("no client template named 'nope'", str(caught.exception))

    def test_owner_actions_need_a_key_before_any_request(self) -> None:
        client = AircdHttpClient(self.url, token="tok")
        for call in (
            lambda: client.set_template("x", {"command": "c"}),
            lambda: client.delete_template("x"),
            lambda: client.list_template_models("x"),
        ):
            with self.assertRaises(AircdHttpError):
                call()
        with self.assertRaises(AircdHttpError):
            client.get_template("  ")
        self.assertEqual(self.hub.requests, [])

    def test_unreachable_hub(self) -> None:
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            port = s.getsockname()[1]
        with self.assertRaises(AircdHttpError) as caught:
            AircdHttpClient(f"127.0.0.1:{port}", token="t", timeout=2).list_templates()
        self.assertIsNone(caught.exception.status)


def _binary_path() -> Path | None:
    override = os.environ.get("AIRCD_BIN")
    if override:
        return Path(override)
    exe = "aircd.exe" if os.name == "nt" else "aircd"
    for build in ("debug", "release"):
        candidate = REPO_ROOT / "target" / build / exe
        if candidate.exists():
            return candidate
    return None


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return int(s.getsockname()[1])


@unittest.skipIf(_binary_path() is None, "aircd binary not built (cargo build)")
class TemplateCliE2ETest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.tmp = tempfile.TemporaryDirectory()
        tmp = Path(cls.tmp.name)
        cls.cfg_dir = tmp / "cfg"
        http_port = _free_port()
        cls.base_url = f"http://127.0.0.1:{http_port}"
        cls.proc = subprocess.Popen(
            [
                str(_binary_path()),
                "--bind", f"127.0.0.1:{_free_port()}",
                "--http-bind", f"127.0.0.1:{http_port}",
                "--db", str(tmp / "templates.sqlite3"),
                "--register-key", OWNER_KEY,
            ],
            cwd=str(tmp),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        deadline = time.time() + 15
        while True:
            try:
                with urllib.request.urlopen(f"{cls.base_url}/healthz", timeout=1):
                    break
            except Exception:
                if cls.proc.poll() is not None or time.time() > deadline:
                    cls.proc.kill()
                    raise RuntimeError("aircd did not start")
                time.sleep(0.1)
        cls.cli("register", "--nick", "tplmember", "--invite", OWNER_KEY)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.proc.terminate()
        cls.proc.wait(timeout=10)
        cls.tmp.cleanup()

    @classmethod
    def cli(cls, *args: str, check: bool = True, stdin: str | None = None) -> subprocess.CompletedProcess[str]:
        env = {k: v for k, v in os.environ.items() if not k.startswith(("AIRCD_", "AIRC_"))}
        env["AIRCD_CONFIG_DIR"] = str(cls.cfg_dir)
        env["PYTHONPATH"] = str(CLI_PY_ROOT)
        proc = subprocess.run(
            [sys.executable, "-m", "aircd.cli", "--server", cls.base_url, *args],
            env=env,
            input=stdin,
            capture_output=True,
            text=True,
            encoding="utf-8",
        )
        if check and proc.returncode != 0:
            raise AssertionError(f"airc {' '.join(args)} failed:\n{proc.stdout}\n{proc.stderr}")
        return proc

    def test_template_lifecycle(self) -> None:
        empty = self.cli("template", "list")
        self.assertIn("No client templates", empty.stdout)

        # Owner actions refuse to run without the key.
        refused = self.cli("template", "set", "py", "--command", sys.executable, check=False)
        self.assertEqual(refused.returncode, 1)
        self.assertIn("needs the owner key", refused.stderr)
        wrong = self.cli("template", "delete", "py", "--invite", "wrong", check=False)
        self.assertEqual(wrong.returncode, 1)
        self.assertIn("HTTP 403", wrong.stderr)

        listing = f'"{sys.executable}" -c "print(\'listed-a\'); print(\'model-b\')"'
        saved = self.cli(
            "--json", "template", "set", "py",
            "--invite", OWNER_KEY,
            "--name", "Python agent",
            "--command", sys.executable,
            "--arg=-c", "--arg", "print('{model}')",
            "--env", "FOO=bar", "--unset-env", "SECRET",
            "--session", "resume", "--resume-arg=--resume={session_id}",
            "--session-marker", "session:",
            "--model", "model-b", "--model", "model-a",
            "--models-command", listing,
        )
        template = json.loads(saved.stdout)["template"]
        self.assertEqual(template["id"], "py")
        self.assertTrue(template["installed"])
        self.assertEqual(template["args"], ["-c", "print('{model}')"])
        self.assertEqual(template["env"], {"FOO": "bar", "SECRET": None})
        self.assertEqual(template["models"], ["model-b", "model-a"])
        self.assertEqual(template["models_command"][1:], ["-c", "print('listed-a'); print('model-b')"])
        self.assertEqual(template["session_mode"], "resume")

        self.cli("template", "set", "ghost", "--invite", OWNER_KEY, "--command", "no-such-program-xyz")

        rows = self.cli("template", "list").stdout.splitlines()
        self.assertEqual([r.split()[0] for r in rows], ["ghost", "py"])
        self.assertIn("missing", rows[0])
        self.assertIn("installed", rows[1])
        self.assertIn("models=2+command", rows[1])

        shown = self.cli("template", "show", "py").stdout
        self.assertIn("py (Python agent): installed", shown)
        self.assertIn("models: model-b, model-a", shown)

        # The saved list comes first, then new ids from the listing command.
        models = self.cli("template", "models", "py", "--invite", OWNER_KEY).stdout.split()
        self.assertEqual(models, ["model-b", "model-a", "listed-a"])

        # Round trip: edit the JSON from `show`, replacing only what the flags say.
        exported = self.cli("--json", "template", "show", "py").stdout
        renamed = json.loads(
            self.cli(
                "--json", "template", "set", "py", "--invite", OWNER_KEY,
                "--file", "-", "--name", "Renamed", "--models-command", "",
                stdin=exported,
            ).stdout
        )["template"]
        self.assertEqual(renamed["name"], "Renamed")
        self.assertEqual(renamed["models_command"], [])
        self.assertEqual(renamed["args"], template["args"])
        self.assertEqual(renamed["env"], template["env"])
        self.assertEqual(renamed["created_at"], template["created_at"])

        no_command = self.cli("template", "set", "bare", "--invite", OWNER_KEY, check=False)
        self.assertEqual(no_command.returncode, 1)
        self.assertIn("needs --command", no_command.stderr)

        self.assertIn("Deleted template ghost.", self.cli("template", "delete", "ghost", "--invite", OWNER_KEY).stdout)
        gone = self.cli("template", "show", "ghost", check=False)
        self.assertEqual(gone.returncode, 1)
        self.assertIn("HTTP 404", gone.stderr)
        again = self.cli("template", "delete", "ghost", "--invite", OWNER_KEY, check=False)
        self.assertIn("HTTP 404", again.stderr)

    def test_template_spawn(self) -> None:
        self.cli("template", "set", "spawnable", "--invite", OWNER_KEY,
                 "--command", sys.executable, "--arg=-c", "--arg", "print('{model}')")

        spawned = json.loads(
            self.cli(
                "--json", "template", "spawn", "spawnable", "worker1",
                "--model", "model-x", "--channel", "#spawnroom", "--channel", "#other",
                "--role", "builder", "--invite", OWNER_KEY,
            ).stdout
        )
        account = spawned["account"]
        self.assertEqual(account["principal"]["nick"], "worker1")
        self.assertEqual(account["template"], "spawnable")
        self.assertEqual(account["model"], "model-x")
        self.assertEqual(account["role"], "builder")
        self.assertEqual(account["channel"], "#spawnroom")
        self.assertIn("#spawnroom", account["channels"])
        self.assertIn("#other", account["channels"])
        self.assertEqual(account["wake"]["command"], sys.executable)
        self.assertTrue(spawned["http_url"].startswith("http://"))

        # The new token works for the new account.
        me = json.loads(self.cli("--json", "--token", account["token"], "whoami").stdout)
        self.assertEqual(me["principal"]["nick"], "worker1")

        text = self.cli("template", "spawn", "spawnable", "worker2", "--model", "m2", "--invite", OWNER_KEY).stdout
        self.assertIn("Spawned worker2 from template spawnable (model m2, role member)", text)
        self.assertIn("token:", text)
        self.assertIn("--profile worker2 login", text)

        duplicate = self.cli("template", "spawn", "spawnable", "worker1", "--model", "m", "--invite", OWNER_KEY, check=False)
        self.assertEqual(duplicate.returncode, 1)
        self.assertIn("HTTP 409", duplicate.stderr)
        missing = self.cli("template", "spawn", "nope", "worker9", "--model", "m", "--invite", OWNER_KEY, check=False)
        self.assertIn("HTTP 404", missing.stderr)
        bad_model = self.cli("template", "spawn", "spawnable", "worker9", "--model", "{x}", "--invite", OWNER_KEY, check=False)
        self.assertIn("HTTP 400", bad_model.stderr)
        no_model = self.cli("template", "spawn", "spawnable", "worker9", check=False)
        self.assertEqual(no_model.returncode, 2)
        self.assertIn("--model", no_model.stderr)

        # Without the owner key the profile's token is sent; a plain member is refused.
        member = self.cli("template", "spawn", "spawnable", "worker9", "--model", "m", check=False)
        self.assertEqual(member.returncode, 1)
        self.assertIn("HTTP 403", member.stderr)
        # A wrong owner key never falls back to the token.
        wrong = self.cli("template", "spawn", "spawnable", "worker9", "--model", "m", "--invite", "wrong", check=False)
        self.assertIn("HTTP 403", wrong.stderr)
        # No token and no key: refused before any request.
        nothing = self.cli("--profile", "nobody", "template", "spawn", "spawnable", "worker9", "--model", "m", check=False)
        self.assertEqual(nothing.returncode, 1)
        self.assertIn("needs a manager's token", nothing.stderr)


if __name__ == "__main__":
    unittest.main()
