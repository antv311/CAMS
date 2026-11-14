"""End-to-end mTLS and HTTPS tests against the compiled aircd binary."""

from __future__ import annotations

import asyncio
import json
import os
import shutil
import socket
import ssl
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from typing import Optional

from aircd import cli as aircd_cli
from aircd.client import AircdClient, AircdHttpClient, AircdHttpError, build_ssl_context
from aircd.daemon import Daemon


REPO_ROOT = Path(__file__).resolve().parents[3]


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
    raise RuntimeError(f"aircd binary not found; build first? Checked {candidates}")


def _find_openssl() -> str:
    found = shutil.which("openssl")
    if found:
        return found
    for candidate in (
        r"C:\Program Files\Git\usr\bin\openssl.exe",
        r"C:\Program Files\Git\mingw64\bin\openssl.exe",
    ):
        if Path(candidate).is_file():
            return candidate
    raise RuntimeError("openssl executable not found on PATH or standard Windows Git locations")


def _run_openssl(openssl: str, cwd: Path, args: list[str]) -> None:
    res = subprocess.run(
        [openssl, *args],
        cwd=str(cwd),
        capture_output=True,
        text=True,
        check=False,
    )
    if res.returncode != 0:
        raise RuntimeError(f"openssl {' '.join(args)} failed:\n{res.stderr}")


def _generate_pki(workdir: Path) -> dict[str, Path]:
    openssl = _find_openssl()
    workdir.mkdir(parents=True, exist_ok=True)

    # Server/Client trusted CA
    _run_openssl(
        openssl,
        workdir,
        [
            "req",
            "-x509",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:prime256v1",
            "-keyout",
            "ca.key",
            "-out",
            "ca.crt",
            "-days",
            "2",
            "-nodes",
            "-subj",
            "/CN=AircdTestCA",
            "-addext",
            "basicConstraints=critical,CA:TRUE",
            "-addext",
            "keyUsage=critical,keyCertSign,cRLSign",
        ],
    )

    # Separate untrusted CA for negative mTLS tests
    _run_openssl(
        openssl,
        workdir,
        [
            "req",
            "-x509",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:prime256v1",
            "-keyout",
            "rogue-ca.key",
            "-out",
            "rogue-ca.crt",
            "-days",
            "2",
            "-nodes",
            "-subj",
            "/CN=RogueTestCA",
            "-addext",
            "basicConstraints=critical,CA:TRUE",
            "-addext",
            "keyUsage=critical,keyCertSign,cRLSign",
        ],
    )

    ext_file = workdir / "server_ext.cnf"
    ext_file.write_text(
        "[v3_req]\nsubjectAltName=DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth\n",
        encoding="utf-8",
    )
    _run_openssl(
        openssl,
        workdir,
        [
            "req",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:prime256v1",
            "-keyout",
            "server.key",
            "-out",
            "server.csr",
            "-nodes",
            "-subj",
            "/CN=localhost",
        ],
    )
    _run_openssl(
        openssl,
        workdir,
        [
            "x509",
            "-req",
            "-in",
            "server.csr",
            "-CA",
            "ca.crt",
            "-CAkey",
            "ca.key",
            "-CAcreateserial",
            "-out",
            "server.crt",
            "-days",
            "2",
            "-extfile",
            str(ext_file),
            "-extensions",
            "v3_req",
        ],
    )

    client_ext = workdir / "client_ext.cnf"
    client_ext.write_text(
        "[v3_client]\nextendedKeyUsage=clientAuth\n",
        encoding="utf-8",
    )

    def _issue_client(name: str, ca_prefix: str) -> tuple[Path, Path]:
        _run_openssl(
            openssl,
            workdir,
            [
                "req",
                "-newkey",
                "ec",
                "-pkeyopt",
                "ec_paramgen_curve:prime256v1",
                "-keyout",
                f"{name}.key",
                "-out",
                f"{name}.csr",
                "-nodes",
                "-subj",
                f"/CN={name}",
            ],
        )
        _run_openssl(
            openssl,
            workdir,
            [
                "x509",
                "-req",
                "-in",
                f"{name}.csr",
                "-CA",
                f"{ca_prefix}.crt",
                "-CAkey",
                f"{ca_prefix}.key",
                "-CAcreateserial",
                "-out",
                f"{name}.crt",
                "-days",
                "2",
                "-extfile",
                str(client_ext),
                "-extensions",
                "v3_client",
            ],
        )
        return workdir / f"{name}.crt", workdir / f"{name}.key"

    client_cert, client_key = _issue_client("trusted-agent", "ca")
    rogue_cert, rogue_key = _issue_client("rogue-agent", "rogue-ca")

    return {
        "ca_cert": workdir / "ca.crt",
        "server_cert": workdir / "server.crt",
        "server_key": workdir / "server.key",
        "client_cert": client_cert,
        "client_key": client_key,
        "rogue_cert": rogue_cert,
        "rogue_key": rogue_key,
    }


def _run_cli(
    args: list[str],
    cfg_dir: Path,
    extra_env: Optional[dict[str, str]] = None,
) -> subprocess.CompletedProcess[str]:
    env = {
        **os.environ,
        "AIRCD_CONFIG_DIR": str(cfg_dir),
        "PYTHONPATH": str(REPO_ROOT / "clients" / "python"),
        "PYTHONIOENCODING": "utf-8",
    }
    if extra_env:
        env.update(extra_env)
    return subprocess.run(
        [sys.executable, "-m", "aircd.cli", *args],
        cwd=str(REPO_ROOT),
        env=env,
        capture_output=True,
        text=True,
        timeout=20,
    )


class AircdMtlsTests(unittest.TestCase):
    tmp_dir: tempfile.TemporaryDirectory[str]
    pki: dict[str, Path]
    server_proc: subprocess.Popen[str]
    cfg_dir: Path
    irc_port: int
    irc_tls_port: int
    http_port: int
    https_port: int
    https_url: str

    @classmethod
    def setUpClass(cls) -> None:
        cls.tmp_dir = tempfile.TemporaryDirectory()
        cls.addClassCleanup(cls.tmp_dir.cleanup)
        tmp_path = Path(cls.tmp_dir.name)
        cls.cfg_dir = tmp_path / "cfg"
        cls.cfg_dir.mkdir(parents=True, exist_ok=True)
        cls.pki = _generate_pki(tmp_path / "pki")
        db_path = tmp_path / "mtls_hub.sqlite3"

        used: set[int] = set()
        ports: list[int] = []
        while len(ports) < 4:
            p = _free_port()
            if p not in used:
                used.add(p)
                ports.append(p)
        cls.irc_port, cls.irc_tls_port, cls.http_port, cls.https_port = ports
        cls.https_url = f"https://127.0.0.1:{cls.https_port}"

        cls.server_proc = subprocess.Popen(
            [
                str(_binary_path()),
                "--bind",
                f"127.0.0.1:{cls.irc_port}",
                "--http-bind",
                f"127.0.0.1:{cls.http_port}",
                "--db",
                str(db_path),
                "--open-registration",
                "--tls-cert",
                str(cls.pki["server_cert"]),
                "--tls-key",
                str(cls.pki["server_key"]),
                "--tls-bind",
                f"127.0.0.1:{cls.irc_tls_port}",
                "--tls-client-ca",
                str(cls.pki["ca_cert"]),
                "--tls-client-auth",
                "required",
                "--http-tls-bind",
                f"127.0.0.1:{cls.https_port}",
                "--tls-only",
            ],
            cwd=str(REPO_ROOT),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        cls.addClassCleanup(cls._stop_server)

        # Wait until HTTPS /healthz responds with a trusted client cert.
        probe = AircdHttpClient(
            cls.https_url,
            timeout=3.0,
            tls_ca_path=str(cls.pki["ca_cert"]),
            tls_cert_path=str(cls.pki["client_cert"]),
            tls_key_path=str(cls.pki["client_key"]),
        )
        deadline = time.monotonic() + 15.0
        last_err: Optional[Exception] = None
        while time.monotonic() < deadline:
            if cls.server_proc.poll() is not None:
                out, err = cls.server_proc.communicate(timeout=2)
                raise RuntimeError(f"aircd exited early ({cls.server_proc.returncode}): {out}\n{err}")
            try:
                health = probe._request("GET", "/healthz")
                if health.get("ok"):
                    return
            except Exception as exc:
                last_err = exc
            time.sleep(0.15)
        raise RuntimeError(f"aircd HTTPS mTLS listener failed to become ready: {last_err}")

    @classmethod
    def _stop_server(cls) -> None:
        if getattr(cls, "server_proc", None) is not None:
            cls.server_proc.terminate()
            try:
                cls.server_proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                cls.server_proc.kill()
                cls.server_proc.wait(timeout=3)
            for stream in (cls.server_proc.stdout, cls.server_proc.stderr):
                if stream:
                    stream.close()
            cls.server_proc = None  # type: ignore[assignment]

    def test_01_tls_only_disables_plaintext_ports_and_healthz_reports_mtls(self) -> None:
        for port in (self.irc_port, self.http_port):
            with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
                s.settimeout(1.0)
                rc = s.connect_ex(("127.0.0.1", port))
                self.assertNotEqual(rc, 0, f"Plaintext port {port} should be closed under --tls-only")

        hub = AircdHttpClient(
            self.https_url,
            tls_ca_path=str(self.pki["ca_cert"]),
            tls_cert_path=str(self.pki["client_cert"]),
            tls_key_path=str(self.pki["client_key"]),
        )
        health = hub._request("GET", "/healthz")
        self.assertTrue(health["ok"])
        self.assertEqual(health["tls_port"], self.irc_tls_port)
        self.assertEqual(health["http_tls_port"], self.https_port)
        self.assertEqual(health["tls_client_auth"], "required")
        self.assertTrue(health["tls_only"])

    def test_02_https_mtls_rejects_missing_or_untrusted_client_cert(self) -> None:
        # Missing client cert: only CA supplied
        no_cert_client = AircdHttpClient(
            self.https_url,
            tls_ca_path=str(self.pki["ca_cert"]),
        )
        with self.assertRaises(AircdHttpError):
            no_cert_client._request("GET", "/healthz")

        # Untrusted client cert: signed by rogue CA
        rogue_client = AircdHttpClient(
            self.https_url,
            tls_ca_path=str(self.pki["ca_cert"]),
            tls_cert_path=str(self.pki["rogue_cert"]),
            tls_key_path=str(self.pki["rogue_key"]),
        )
        with self.assertRaises(AircdHttpError):
            rogue_client._request("GET", "/healthz")

    def test_03_cli_persists_tls_settings_and_communicates_over_https_mtls(self) -> None:
        reg = _run_cli(
            [
                "--server",
                self.https_url,
                "--tls-ca",
                str(self.pki["ca_cert"]),
                "--tls-cert",
                str(self.pki["client_cert"]),
                "--tls-key",
                str(self.pki["client_key"]),
                "register",
                "--nick",
                "mtls-alice",
            ],
            self.cfg_dir,
        )
        self.assertEqual(reg.returncode, 0, f"register failed: {reg.stderr}\n{reg.stdout}")

        profiles = json.loads((self.cfg_dir / "profiles.json").read_text(encoding="utf-8"))
        saved = profiles["profiles"]["mtls-alice"]
        self.assertEqual(saved["server"], self.https_url)
        self.assertEqual(saved["tls_ca"], str(self.pki["ca_cert"]))
        self.assertEqual(saved["tls_cert"], str(self.pki["client_cert"]))
        self.assertEqual(saved["tls_key"], str(self.pki["client_key"]))

        # Subsequent CLI calls reuse saved TLS paths from profiles.json automatically.
        who = _run_cli(["--profile", "mtls-alice", "whoami"], self.cfg_dir)
        self.assertEqual(who.returncode, 0, f"whoami failed: {who.stderr}\n{who.stdout}")
        self.assertIn("mtls-alice", who.stdout)

        # CLI rejects --tls-key without --tls-cert (using a fresh config dir without saved cert).
        empty_cfg = Path(self.tmp_dir.name) / "empty_cfg"
        empty_cfg.mkdir(exist_ok=True)
        bad = _run_cli(
            [
                "--server",
                self.https_url,
                "--tls-key",
                str(self.pki["client_key"]),
                "whoami",
            ],
            empty_cfg,
        )
        self.assertNotEqual(bad.returncode, 0)
        self.assertIn("--tls-cert is required", bad.stderr)

    def test_04_irc_tls_client_mtls_enforcement_and_roundtrip(self) -> None:
        hub = AircdHttpClient(
            self.https_url,
            tls_ca_path=str(self.pki["ca_cert"]),
            tls_cert_path=str(self.pki["client_cert"]),
            tls_key_path=str(self.pki["client_key"]),
        )
        acct = hub._request("POST", "/v1/register", payload={"nick": "mtls-irc-user"})
        token = acct["token"]

        async def _scenario() -> None:
            # 1. Trusted mTLS IRC client succeeds and receives welcome message.
            trusted = AircdClient(
                "127.0.0.1",
                self.irc_tls_port,
                token=token,
                nick="mtls-irc-user",
                tls=True,
                tls_ca_path=str(self.pki["ca_cert"]),
                tls_cert_path=str(self.pki["client_cert"]),
                tls_key_path=str(self.pki["client_key"]),
                auto_reconnect=False,
            )
            await asyncio.wait_for(trusted.connect(), timeout=5.0)
            self.assertEqual(trusted._state.nick, "mtls-irc-user")
            await trusted.close()

            # 2. Client without a client certificate is rejected by mTLS IRC listener.
            no_cert = AircdClient(
                "127.0.0.1",
                self.irc_tls_port,
                token=token,
                nick="mtls-no-cert",
                tls=True,
                tls_ca_path=str(self.pki["ca_cert"]),
                auto_reconnect=False,
            )
            with self.assertRaises((ssl.SSLError, ConnectionError, OSError)):
                await asyncio.wait_for(no_cert.connect(), timeout=5.0)
            await no_cert.close()

            # 3. Client presenting an untrusted CA's certificate is rejected.
            rogue = AircdClient(
                "127.0.0.1",
                self.irc_tls_port,
                token=token,
                nick="mtls-rogue",
                tls=True,
                tls_ca_path=str(self.pki["ca_cert"]),
                tls_cert_path=str(self.pki["rogue_cert"]),
                tls_key_path=str(self.pki["rogue_key"]),
                auto_reconnect=False,
            )
            with self.assertRaises((ssl.SSLError, ConnectionError, OSError)):
                await asyncio.wait_for(rogue.connect(), timeout=5.0)
            await rogue.close()

        asyncio.run(_scenario())

    def test_05_validation_helpers(self) -> None:
        with self.assertRaises(ValueError):
            build_ssl_context(tls_key_path=str(self.pki["client_key"]))

        with self.assertRaises(ValueError):
            Daemon(
                host="127.0.0.1",
                port=self.irc_tls_port,
                token="tok",
                nick="daemon-test",
                channels=["#cams"],
                tls=True,
                tls_key_path=str(self.pki["client_key"]),
            )

        with self.assertRaises(aircd_cli.CliError):
            aircd_cli._http_client_from_prof(
                self.https_url,
                "tok",
                {"_tls": {"tls_key": str(self.pki["client_key"])}},
            )


class AircdTlsHotReloadTests(unittest.TestCase):
    def test_hot_reload_via_file_watcher_and_owner_api_preserves_active_irc(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            pki1 = _generate_pki(tmp_path / "pki1")
            pki2 = _generate_pki(tmp_path / "pki2")

            live_dir = tmp_path / "live"
            live_dir.mkdir(parents=True, exist_ok=True)
            live_ca = live_dir / "ca.crt"
            live_cert = live_dir / "server.crt"
            live_key = live_dir / "server.key"
            shutil.copy2(pki1["ca_cert"], live_ca)
            shutil.copy2(pki1["server_cert"], live_cert)
            shutil.copy2(pki1["server_key"], live_key)

            used: set[int] = set()
            ports: list[int] = []
            while len(ports) < 4:
                p = _free_port()
                if p not in used:
                    used.add(p)
                    ports.append(p)
            irc_port, irc_tls_port, http_port, https_port = ports
            https_url = f"https://127.0.0.1:{https_port}"
            invite_key = "hot-reload-owner-secret"

            proc = subprocess.Popen(
                [
                    str(_binary_path()),
                    "--bind",
                    f"127.0.0.1:{irc_port}",
                    "--http-bind",
                    f"127.0.0.1:{http_port}",
                    "--db",
                    str(tmp_path / "reload.sqlite3"),
                    "--register-key",
                    invite_key,
                    "--tls-cert",
                    str(live_cert),
                    "--tls-key",
                    str(live_key),
                    "--tls-bind",
                    f"127.0.0.1:{irc_tls_port}",
                    "--tls-client-ca",
                    str(live_ca),
                    "--tls-client-auth",
                    "required",
                    "--http-tls-bind",
                    f"127.0.0.1:{https_port}",
                    "--tls-only",
                    "--tls-watch-interval-ms",
                    "100",
                    "--tls-reload-debounce-ms",
                    "50",
                ],
                cwd=str(REPO_ROOT),
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            try:
                hub1 = AircdHttpClient(
                    https_url,
                    timeout=3.0,
                    tls_ca_path=str(pki1["ca_cert"]),
                    tls_cert_path=str(pki1["client_cert"]),
                    tls_key_path=str(pki1["client_key"]),
                )
                deadline = time.monotonic() + 15.0
                while time.monotonic() < deadline:
                    if proc.poll() is not None:
                        out, err = proc.communicate(timeout=2)
                        raise RuntimeError(f"aircd exited early: {out}\n{err}")
                    try:
                        h = hub1._request("GET", "/healthz")
                        if h.get("ok"):
                            break
                    except Exception:
                        time.sleep(0.15)
                else:
                    raise RuntimeError("aircd did not become ready")

                acct = hub1._request(
                    "POST",
                    "/v1/register",
                    payload={"nick": "hot-irc-agent", "invite": invite_key},
                )
                token = acct["token"]

                # Open an active mTLS IRC socket and keep it alive across both reloads.
                ctx1 = build_ssl_context(
                    tls_ca_path=str(pki1["ca_cert"]),
                    tls_cert_path=str(pki1["client_cert"]),
                    tls_key_path=str(pki1["client_key"]),
                )
                raw_sock = socket.create_connection(("127.0.0.1", irc_tls_port), timeout=5.0)
                irc_sock = ctx1.wrap_socket(raw_sock, server_hostname="localhost")
                try:
                    irc_sock.sendall(
                        f"CAP REQ :echo-message\r\nPASS {token}\r\nNICK hot-irc-agent\r\nUSER hot-irc-agent 0 * :live\r\nCAP END\r\nJOIN #ops\r\n".encode(
                            "utf-8"
                        )
                    )
                    buf = b""
                    join_deadline = time.monotonic() + 5.0
                    while time.monotonic() < join_deadline:
                        buf += irc_sock.recv(4096)
                        if b"JOIN #ops" in buf:
                            break
                    self.assertIn(b"JOIN #ops", buf)

                    # 1. File-watcher rotation: switch server cert+key to pki2 and trust BOTH CA1 and CA2.
                    combined_ca = pki1["ca_cert"].read_text(encoding="utf-8") + "\n" + pki2["ca_cert"].read_text(
                        encoding="utf-8"
                    )
                    live_ca.write_text(combined_ca, encoding="utf-8")
                    shutil.copy2(pki2["server_cert"], live_cert)
                    shutil.copy2(pki2["server_key"], live_key)

                    hub2 = AircdHttpClient(
                        https_url,
                        timeout=3.0,
                        invite=invite_key,
                        tls_ca_path=str(pki2["ca_cert"]),
                        tls_cert_path=str(pki2["client_cert"]),
                        tls_key_path=str(pki2["client_key"]),
                    )
                    watch_deadline = time.monotonic() + 10.0
                    reloaded_ok = False
                    last_err: Optional[Exception] = None
                    while time.monotonic() < watch_deadline:
                        try:
                            status_resp = hub2._request(
                                "GET",
                                "/v1/owner/tls",
                                owner=True,
                            )
                            if status_resp["status"]["active"]["generation"] >= 2:
                                reloaded_ok = True
                                self.assertEqual(status_resp["status"]["last_trigger"], "file_watcher")
                                self.assertEqual(status_resp["status"]["active"]["client_ca_count"], 2)
                                break
                        except (AircdHttpError, ssl.SSLError, OSError) as exc:
                            last_err = exc
                        time.sleep(0.1)
                    self.assertTrue(reloaded_ok, f"file watcher did not hot-reload within timeout: {last_err}")

                    # Active IRC connection from before reload still works without reconnecting.
                    irc_sock.sendall(b"PRIVMSG #ops :alive-after-watcher\r\n")
                    msg_deadline = time.monotonic() + 5.0
                    msg_buf = b""
                    while time.monotonic() < msg_deadline:
                        msg_buf += irc_sock.recv(4096)
                        if b"alive-after-watcher" in msg_buf:
                            break
                    self.assertIn(b"alive-after-watcher", msg_buf)

                    # 2. Explicit API reload: replace CA bundle with CA2 only (revoking CA1 for new connections).
                    shutil.copy2(pki2["ca_cert"], live_ca)
                    api_reload = hub2._request(
                        "POST",
                        "/v1/owner/tls/reload",
                        payload={"invite": invite_key, "reason": "revoke_ca1"},
                    )
                    self.assertTrue(api_reload["reloaded"])
                    self.assertGreaterEqual(api_reload["status"]["active"]["generation"], 3)
                    self.assertEqual(api_reload["status"]["active"]["client_ca_count"], 1)

                    # New connection with CA1 client cert is now rejected even trusting CA2 server cert.
                    revoked_client = AircdHttpClient(
                        https_url,
                        timeout=3.0,
                        tls_ca_path=str(pki2["ca_cert"]),
                        tls_cert_path=str(pki1["client_cert"]),
                        tls_key_path=str(pki1["client_key"]),
                    )
                    with self.assertRaises((ssl.SSLError, AircdHttpError, OSError)):
                        revoked_client._request("GET", "/healthz")

                    # Existing IRC connection from CA1 remains connected and responsive!
                    irc_sock.sendall(b"PRIVMSG #ops :still-alive-after-api-reload\r\n")
                    msg2_deadline = time.monotonic() + 5.0
                    msg2_buf = b""
                    while time.monotonic() < msg2_deadline:
                        msg2_buf += irc_sock.recv(4096)
                        if b"still-alive-after-api-reload" in msg2_buf:
                            break
                    self.assertIn(b"still-alive-after-api-reload", msg2_buf)
                finally:
                    irc_sock.close()
            finally:
                proc.terminate()
                try:
                    proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait(timeout=3)
                for s in (proc.stdout, proc.stderr):
                    if s:
                        s.close()


if __name__ == "__main__":
    unittest.main()
