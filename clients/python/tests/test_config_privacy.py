"""Client config locations and a privacy scan of tracked client and docs files.

The config tests check that `airc` reads profiles from an editable file in a
standard location and that flags and environment variables override it.

The privacy scan checks tracked files that ship to other users (Python client,
tests, examples, docs, scripts, README) for home-directory paths, real email
addresses, and the current machine's user and host names. It reads the names
from the running system, so no personal value is written into this file.
"""

from __future__ import annotations

import argparse
import getpass
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from aircd import cli  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parents[3]
SCAN_PATHSPECS = ["clients", "docs", "scripts", "README.md", "*.cmd"]

# Emails in fixtures must use a reserved or no-reply domain.
ALLOWED_EMAIL_DOMAINS = re.compile(
    r"@(?:[\w.-]+\.)?(?:example\.(?:com|org|net|local)|example|invalid|localhost|users\.noreply\.github\.com)$",
    re.IGNORECASE,
)
EMAIL_RE = re.compile(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}")
# A user name directly under a home root, e.g. C:\Users\<name> or /home/<name>.
HOME_PATH_RE = re.compile(
    r"(?:[A-Za-z]:[\\/]+Users[\\/]+|/home/|/Users/)(?P<name>[A-Za-z0-9._-]+)",
)
PLACEHOLDER_HOME_NAMES = {"you", "user", "username", "name", "me", "example", "runner", "public", "default"}
GITHUB_ACCOUNT_RE = re.compile(r"(?<![A-Za-z0-9.-])github\.com/(?P<account>[A-Za-z0-9-]+)", re.IGNORECASE)
# Only the upstream aircd author and linked third-party tools are allowed; another
# account link would publish a contributor's or the owner's username.
ALLOWED_GITHUB_ACCOUNTS = {"c4pt0r", "openai"}
# Names too generic to flag when they happen to be the local user or host.
GENERIC_LOCAL_NAMES = {"user", "admin", "administrator", "root", "runner", "localhost", "ubuntu", "builder"}


def _fake_os(name: str) -> types.SimpleNamespace:
    # Patching os.name itself breaks pathlib on the host OS, so swap cli's os.
    return types.SimpleNamespace(name=name, environ=os.environ)


def _ns(**kwargs) -> argparse.Namespace:
    base = {"profile": None, "server": None, "token": None}
    base.update(kwargs)
    return argparse.Namespace(**base)


class ConfigDirTests(unittest.TestCase):
    def test_override_dir_expands_home(self) -> None:
        with mock.patch.dict(os.environ, {"AIRCD_CONFIG_DIR": "~/custom-aircd"}):
            self.assertEqual(cli.config_dir(), Path("~/custom-aircd").expanduser())
            self.assertEqual(cli.profiles_path(), Path("~/custom-aircd").expanduser() / "profiles.json")

    def test_windows_uses_localappdata(self) -> None:
        env = {"AIRCD_CONFIG_DIR": "", "LOCALAPPDATA": str(Path("appdata-root"))}
        with mock.patch.dict(os.environ, env), mock.patch.object(cli, "os", _fake_os("nt")):
            self.assertEqual(cli.config_dir(), Path("appdata-root") / "aircd")

    def test_posix_uses_xdg_then_dot_config(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            with mock.patch.object(cli, "os", _fake_os("posix")), mock.patch.object(cli.Path, "home", return_value=home):
                with mock.patch.dict(os.environ, {"AIRCD_CONFIG_DIR": "", "XDG_CONFIG_HOME": str(home / "xdg")}):
                    self.assertEqual(cli.config_dir(), home / "xdg" / "aircd")
                with mock.patch.dict(os.environ, {"AIRCD_CONFIG_DIR": "", "XDG_CONFIG_HOME": ""}):
                    self.assertEqual(cli.config_dir(), home / ".config" / "aircd")


class ProfileResolutionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.cfg = Path(self.tmp.name)
        (self.cfg / "profiles.json").write_text(
            json.dumps(
                {
                    "default": "worker",
                    "profiles": {
                        "worker": {"server": "hub.example:7000", "token": "file-token", "nick": "worker"},
                        "other": {"server": "http://other.example:7001", "token": "other-token"},
                    },
                }
            ),
            encoding="utf-8",
        )
        cleared = {k: "" for k in ("AIRCD_PROFILE", "AIRCD_HTTP_URL", "AIRCD_URL", "AIRCD_TOKEN")}
        patcher = mock.patch.dict(os.environ, {"AIRCD_CONFIG_DIR": str(self.cfg), **cleared})
        patcher.start()
        self.addCleanup(patcher.stop)

    def test_hand_edited_profile_is_used(self) -> None:
        server, token, prof = cli.resolve_connection(_ns())
        self.assertEqual(server, "http://hub.example:7000")
        self.assertEqual(token, "file-token")
        self.assertEqual(prof["_profile_name"], "worker")

    def test_environment_overrides_profile(self) -> None:
        env = {"AIRCD_PROFILE": "other", "AIRCD_HTTP_URL": "http://env.example:9", "AIRCD_TOKEN": "env-token"}
        with mock.patch.dict(os.environ, env):
            server, token, prof = cli.resolve_connection(_ns())
        self.assertEqual((server, token, prof["_profile_name"]), ("http://env.example:9", "env-token", "other"))

    def test_flags_override_environment(self) -> None:
        with mock.patch.dict(os.environ, {"AIRCD_HTTP_URL": "http://env.example:9", "AIRCD_TOKEN": "env-token"}):
            server, token, _ = cli.resolve_connection(
                _ns(profile="other", server="http://flag.example:1/", token="flag-token")
            )
        self.assertEqual((server, token), ("http://flag.example:1", "flag-token"))

    def test_missing_file_falls_back_to_local_default(self) -> None:
        (self.cfg / "profiles.json").unlink()
        server, token, _ = cli.resolve_connection(_ns())
        self.assertEqual((server, token), (cli.DEFAULT_SERVER_URL, ""))

    def test_update_profile_writes_editable_json(self) -> None:
        cli.update_profile("new", {"server": "http://127.0.0.1:6680", "token": "t"}, make_default=False)
        data = json.loads((self.cfg / "profiles.json").read_text(encoding="utf-8"))
        self.assertEqual(data["default"], "worker")
        self.assertEqual(data["profiles"]["new"]["token"], "t")


def _tracked_files() -> list[Path]:
    if shutil.which("git") is None or not (REPO_ROOT / ".git").exists():
        raise unittest.SkipTest("not running inside a git checkout")
    out = subprocess.run(
        ["git", "-C", str(REPO_ROOT), "ls-files", "-z", "--", *SCAN_PATHSPECS],
        check=True,
        capture_output=True,
    ).stdout.decode("utf-8")
    return [REPO_ROOT / p for p in out.split("\0") if p]


class TrackedFilePrivacyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.texts: dict[str, str] = {}
        for path in _tracked_files():
            try:
                cls.texts[path.relative_to(REPO_ROOT).as_posix()] = path.read_text(encoding="utf-8")
            except (UnicodeDecodeError, FileNotFoundError):
                continue
        if not cls.texts:
            raise unittest.SkipTest("no tracked files found to scan")

    def _hits(self, pattern: re.Pattern[str], keep) -> list[str]:
        hits = []
        for rel, text in self.texts.items():
            for lineno, line in enumerate(text.splitlines(), 1):
                for m in pattern.finditer(line):
                    if keep(m):
                        hits.append(f"{rel}:{lineno}: {m.group(0)}")
        return hits

    def test_no_personal_home_paths(self) -> None:
        hits = self._hits(HOME_PATH_RE, lambda m: m.group("name").lower() not in PLACEHOLDER_HOME_NAMES)
        self.assertEqual(hits, [], "use a placeholder such as C:\\path\\to\\repo instead")

    def test_emails_use_reserved_domains(self) -> None:
        hits = self._hits(EMAIL_RE, lambda m: not ALLOWED_EMAIL_DOMAINS.search(m.group(0)))
        self.assertEqual(hits, [], "use an example.local or noreply address in fixtures")

    def test_github_links_name_only_upstream(self) -> None:
        hits = self._hits(GITHUB_ACCOUNT_RE, lambda m: m.group("account").lower() not in ALLOWED_GITHUB_ACCOUNTS)
        self.assertEqual(hits, [], "do not link personal GitHub accounts; describe the location instead")

    def test_local_user_and_host_names_absent(self) -> None:
        names = set()
        for value in (getpass.getuser(), socket.gethostname(), os.environ.get("USERNAME", ""), os.environ.get("COMPUTERNAME", "")):
            value = (value or "").strip()
            if len(value) >= 4 and value.lower() not in GENERIC_LOCAL_NAMES:
                names.add(value)
        if not names:
            self.skipTest("local user and host names are too generic to scan for")
        for name in sorted(names):
            pattern = re.compile(rf"(?<![A-Za-z0-9]){re.escape(name)}(?![A-Za-z0-9])", re.IGNORECASE)
            # Report the file and line only, so the name itself is not echoed into logs.
            hits = [h.rsplit(":", 1)[0] for h in self._hits(pattern, lambda m: True)]
            self.assertEqual(hits, [], "the local user or host name appears in tracked files")


if __name__ == "__main__":
    unittest.main()
