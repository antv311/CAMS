"""Zero-dependency Python CLI for aircd (IRC + Webhooks + Roles + Locks + Git)."""

from __future__ import annotations

import argparse
import hashlib
import hmac
import http.server
import json
import os
import shlex
import socketserver
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any


DEFAULT_SERVER_URL = "http://127.0.0.1:6680"
ALLOWED_REPORT_STATUSES = ("info", "ok", "completed", "needs_input", "failed", "warning")
MANAGED_HOOK_MARKER = "# aircd-hook-managed"


class CliError(Exception):
    """Readable CLI error."""


def config_dir() -> Path:
    override = os.environ.get("AIRCD_CONFIG_DIR", "").strip()
    if override:
        return Path(override).expanduser()
    if os.name == "nt":
        local = os.environ.get("LOCALAPPDATA", "").strip()
        if local:
            return Path(local) / "aircd"
        return Path.home() / "AppData" / "Local" / "aircd"
    xdg = os.environ.get("XDG_CONFIG_HOME", "").strip()
    if xdg:
        return Path(xdg).expanduser() / "aircd"
    return Path.home() / ".config" / "aircd"


def profiles_path() -> Path:
    return config_dir() / "profiles.json"


def load_profiles() -> dict[str, Any]:
    path = profiles_path()
    if not path.exists():
        return {"default": "default", "profiles": {}}
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
        if isinstance(data, dict) and isinstance(data.get("profiles"), dict):
            return data
    except Exception:
        pass
    return {"default": "default", "profiles": {}}


def save_profiles(data: dict[str, Any]) -> None:
    path = profiles_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")


def resolve_connection(args: argparse.Namespace) -> tuple[str, str, dict[str, Any]]:
    store = load_profiles()
    profile_name = (
        getattr(args, "profile", None)
        or os.environ.get("AIRCD_PROFILE", "").strip()
        or store.get("default")
        or "default"
    )
    prof = dict(store.get("profiles", {}).get(profile_name, {}))
    server = (
        getattr(args, "server", None)
        or os.environ.get("AIRCD_HTTP_URL", "").strip()
        or os.environ.get("AIRCD_URL", "").strip()
        or prof.get("server")
        or DEFAULT_SERVER_URL
    ).rstrip("/")
    if not server.startswith(("http://", "https://")):
        server = f"http://{server}"

    token = (
        getattr(args, "token", None)
        or os.environ.get("AIRCD_TOKEN", "").strip()
        or prof.get("token")
        or ""
    )
    prof["_profile_name"] = profile_name
    return server, token, prof


def update_profile(profile_name: str, patch: dict[str, Any], make_default: bool = True) -> None:
    store = load_profiles()
    profiles = store.setdefault("profiles", {})
    existing = dict(profiles.get(profile_name, {}))
    existing.update({k: v for k, v in patch.items() if v is not None})
    profiles[profile_name] = existing
    if make_default or "default" not in store:
        store["default"] = profile_name
    save_profiles(store)


def http_json(
    server: str,
    method: str,
    path: str,
    *,
    token: str = "",
    payload: Any = None,
    timeout: float = 15.0,
) -> dict[str, Any]:
    url = f"{server.rstrip('/')}{path}"
    headers = {"Accept": "application/json"}
    data_bytes = None
    if token:
        headers["Authorization"] = f"Bearer {token}"
    if payload is not None:
        headers["Content-Type"] = "application/json"
        data_bytes = json.dumps(payload).encode("utf-8")

    req = urllib.request.Request(url, data=data_bytes, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            raw = resp.read().decode("utf-8", errors="replace")
            return json.loads(raw) if raw.strip() else {"ok": True}
    except urllib.error.HTTPError as err:
        raw = err.read().decode("utf-8", errors="replace")
        try:
            parsed = json.loads(raw)
            msg = parsed.get("error") or raw
        except Exception:
            msg = raw or str(err)
        raise CliError(f"HTTP {err.code} {method} {path}: {msg}") from None
    except urllib.error.URLError as err:
        raise CliError(f"Cannot reach aircd at {server}: {err.reason}") from None


def http_text(server: str, path: str, timeout: float = 10.0) -> str:
    url = f"{server.rstrip('/')}{path}"
    req = urllib.request.Request(url, method="GET")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.read().decode("utf-8", errors="replace")
    except urllib.error.URLError as err:
        raise CliError(f"Cannot reach aircd at {server}: {err.reason}") from None


def require_token(args: argparse.Namespace) -> tuple[str, str, dict[str, Any]]:
    server, token, prof = resolve_connection(args)
    if not token:
        raise CliError(
            f"No saved token for profile '{prof.get('_profile_name')}'. "
            f"Run: airc register --server {server} --nick <name> (or pass --token)"
        )
    return server, token, prof


def print_out(args: argparse.Namespace, data: Any, human_lines: list[str] | None = None) -> None:
    if getattr(args, "json", False) or human_lines is None:
        print(json.dumps(data, indent=2))
    else:
        for line in human_lines:
            print(line)


# ── Commands ──────────────────────────────────────────────────


def cmd_welcome(args: argparse.Namespace) -> int:
    server, _token, _prof = resolve_connection(args)
    fmt = args.format
    if fmt == "json":
        data = http_json(server, "GET", "/v1/welcome")
        print(json.dumps(data, indent=2))
    else:
        text = http_text(server, f"/?format={urllib.parse.quote(fmt)}")
        print(text)
    return 0


def cmd_register(args: argparse.Namespace) -> int:
    server, _token, prof = resolve_connection(args)
    channels = [c.strip() for c in (args.channels or "#general").split(",") if c.strip()]
    data = http_json(
        server,
        "POST",
        "/v1/register",
        payload={
            "nick": args.nick,
            "role": args.role,
            "team": args.team,
            "channels": channels,
            "invite": args.invite or os.environ.get("AIRC_INVITE") or None,
        },
    )
    profile_name = args.profile or data["principal"]["nick"].lower()
    update_profile(
        profile_name,
        {
            "server": server,
            "token": data["token"],
            "nick": data["principal"]["nick"],
            "id": data["principal"]["id"],
            "role": data["principal"]["role"],
            "irc_host": data.get("irc_host"),
            "irc_port": data.get("irc_port"),
        },
        make_default=not args.no_default,
    )
    print_out(
        args,
        data,
        [
            f"Registered @{data['principal']['nick']} (role={data['principal']['role']}) on {server}",
            f"Profile saved: '{profile_name}' -> {profiles_path()}",
            f"IRC endpoint: {data.get('irc_host')}:{data.get('irc_port')}",
            f"Channels joined: {', '.join(data.get('channels', []))}",
        ],
    )
    return 0


def cmd_owner_wake(args: argparse.Namespace) -> int:
    """Configure how the server starts an account's agent (owner only)."""
    server, _token, _prof = resolve_connection(args)
    invite = args.invite or os.environ.get("AIRC_INVITE")
    if not invite:
        raise SystemExit("owner wake needs the owner key: pass --invite or set AIRC_INVITE")
    payload: dict[str, Any] = {"invite": invite, "action": args.wake_cmd, "nick": args.nick}
    if args.wake_cmd == "set":
        env: dict[str, Any] = {}
        for item in args.env or []:
            key, sep, value = item.partition("=")
            env[key] = value if sep else None
        for key in args.unset_env or []:
            env[key] = None
        payload.update(
            {
                "command": args.command,
                "args": args.arg or [],
                "cwd": args.cwd,
                "env": env,
                "prompt": args.prompt,
                "enabled": not args.disabled,
                "cooldown_seconds": args.cooldown,
                "timeout_seconds": args.timeout,
                "max_runs_per_hour": args.max_per_hour,
                "session_mode": args.session,
                "resume_args": args.resume_arg or [],
                "session_marker": args.session_marker,
                "session_scope": args.session_scope,
            }
        )
    if args.wake_cmd == "runs":
        payload["limit"] = args.limit
    if args.wake_cmd == "test":
        payload["message"] = args.message
    data = http_json(server, "POST", "/v1/owner/wake", payload=payload)

    lines: list[str] = []
    if args.wake_cmd in ("set", "show"):
        wake = data.get("wake")
        if not wake:
            lines.append(f"@{args.nick} has no launcher configured.")
        else:
            state = "enabled" if wake["enabled"] else "disabled"
            lines.append(f"@{wake['principal_id']} launcher ({state}): {wake['command']} {' '.join(wake['args'])}")
            lines.append(
                f"cwd={wake.get('cwd') or '{repo_path}'} cooldown={wake['cooldown_seconds']}s "
                f"timeout={wake['timeout_seconds']}s max/hour={wake['max_runs_per_hour']}"
            )
            if wake.get("session_mode") == "resume":
                lines.append(
                    f"session=resume (per {wake['session_scope']}) marker={wake.get('session_marker')!r} "
                    f"resume args: {' '.join(wake.get('resume_args') or [])}"
                )
    elif args.wake_cmd == "reset":
        lines.append(f"Forgot {data.get('removed', 0)} stored conversation(s); the next launch starts fresh.")
    elif args.wake_cmd == "delete":
        lines.append("Launcher removed." if data.get("removed") else "No launcher was configured.")
    elif args.wake_cmd == "test":
        lines.append("Launch started." if data.get("started") else "Not started: no enabled launcher for that account.")
    elif args.wake_cmd == "runs":
        for run in data.get("runs", []):
            lines.append(
                f"{run['status']:8} @{run['principal_id']} exit={run.get('exit_code')} "
                f"task={run.get('task_id') or '-'} :: {run['reason'][:90]}"
            )
            if args.logs and run.get("log_tail"):
                lines.append(run["log_tail"])
        lines = lines or ["No runs yet."]
    print_out(args, data, lines)
    return 0


def cmd_owner_reissue(args: argparse.Namespace) -> int:
    server, _token, _prof = resolve_connection(args)
    invite = args.invite or os.environ.get("AIRC_INVITE")
    if not invite:
        raise SystemExit("owner reissue needs the owner key: pass --invite or set AIRC_INVITE")
    data = http_json(
        server,
        "POST",
        "/v1/owner/reissue",
        payload={"nick": args.nick, "invite": invite},
    )
    profile_name = args.profile or data["principal"]["nick"].lower()
    update_profile(
        profile_name,
        {
            "server": server,
            "token": data["token"],
            "nick": data["principal"]["nick"],
            "id": data["principal"]["id"],
            "role": data["principal"]["role"],
        },
        make_default=args.make_default,
    )
    print_out(
        args,
        data,
        [
            f"Reissued token for @{data['principal']['nick']} on {server}; all earlier tokens are revoked",
            f"Profile saved: '{profile_name}' -> {profiles_path()}",
        ],
    )
    return 0


def cmd_login(args: argparse.Namespace) -> int:
    server, token, prof = require_token(args)
    channels = [c.strip() for c in (args.channels or "#general").split(",") if c.strip()]
    data = http_json(
        server,
        "POST",
        "/v1/login",
        token=token,
        payload={
            "channels": channels,
            "rotate_token": bool(args.rotate),
            "label": "cli-login",
        },
    )
    active_token = data.get("rotated_token") or token
    profile_name = args.profile or prof.get("_profile_name") or data["principal"]["nick"].lower()
    update_profile(
        profile_name,
        {
            "server": server,
            "token": active_token,
            "nick": data["principal"]["nick"],
            "id": data["principal"]["id"],
            "role": data["principal"]["role"],
        },
        make_default=True,
    )
    print_out(
        args,
        data,
        [
            f"Logged in as @{data['principal']['nick']} (role={data['principal']['role']})",
            f"Channels: {', '.join(data.get('channels', []))}",
            f"Profile updated: '{profile_name}' ({profiles_path()})",
        ],
    )
    return 0


def cmd_whoami(args: argparse.Namespace) -> int:
    server, token, _prof = require_token(args)
    data = http_json(server, "GET", "/v1/me", token=token)
    p = data["principal"]
    wh = data.get("webhook")
    print_out(
        args,
        data,
        [
            f"Agent: @{p['nick']} (id={p['id']}, role={p['role']}, team={p.get('team') or '-'})",
            f"Webhook: {wh['url']} (notify_when={wh['notify_when']})" if wh else "Webhook: not configured",
        ],
    )
    return 0


def cmd_agents(args: argparse.Namespace) -> int:
    server, _token, _prof = resolve_connection(args)
    data = http_json(server, "GET", "/v1/agents")
    lines = [
        f"@{a['nick']:<16} role={a['role']:<12} status={'online' if a['online'] else 'offline':<7} webhook={'yes' if a['webhook_configured'] else 'no'}"
        for a in data.get("agents", [])
    ]
    print_out(args, data, lines or ["No agents registered."])
    return 0


def cmd_channels(args: argparse.Namespace) -> int:
    server, _token, _prof = resolve_connection(args)
    data = http_json(server, "GET", "/v1/channels")
    lines = [
        f"{c['channel']:<24} messages={c['message_count']}"
        for c in data.get("channels", [])
    ]
    print_out(args, data, lines or ["No active channels."])
    return 0


def cmd_join(args: argparse.Namespace) -> int:
    server, token, _prof = require_token(args)
    data = http_json(server, "POST", "/v1/channels/join", token=token, payload={"channels": args.channels})
    print_out(args, data, [f"Joined: {', '.join(data.get('channels', []))}"])
    return 0


def cmd_send(args: argparse.Namespace) -> int:
    server, token, _prof = require_token(args)
    body = " ".join(args.message).strip()
    data = http_json(
        server,
        "POST",
        "/v1/messages",
        token=token,
        payload={"target": args.target, "body": body},
    )
    m = data["message"]
    print_out(args, data, [f"[{m['channel']}] #{m['seq']} sent ({m['msg_id']}): {m['body']}"])
    return 0


def cmd_messages(args: argparse.Namespace) -> int:
    server, token, _prof = require_token(args)
    params: dict[str, Any] = {"after_seq": args.after_seq, "limit": args.limit}
    if args.dm:
        params["dm"] = args.dm
    else:
        params["channel"] = args.channel
    qs = urllib.parse.urlencode(params)
    data = http_json(server, "GET", f"/v1/messages?{qs}", token=token)
    lines = [
        f"[{m['channel']}] #{m['seq']} {m['sender_id']}: {m['body']}"
        for m in data.get("messages", [])
    ]
    print_out(args, data, lines or ["(no messages)"])
    return 0


def cmd_watch(args: argparse.Namespace) -> int:
    server, token, _prof = require_connection(args)
    if not token:
        raise CliError("watch requires authentication; run `airc register` first.")
    after_seq = args.after_seq
    try:
        while True:
            params: dict[str, Any] = {"after_seq": after_seq, "limit": 100}
            if args.dm:
                params["dm"] = args.dm
            else:
                params["channel"] = args.channel
            qs = urllib.parse.urlencode(params)
            data = http_json(server, "GET", f"/v1/messages?{qs}", token=token)
            for m in data.get("messages", []):
                after_seq = max(after_seq, int(m["seq"]))
                if args.json:
                    print(json.dumps(m), flush=True)
                else:
                    print(f"[{m['channel']}] #{m['seq']} {m['sender_id']}: {m['body']}", flush=True)
            time.sleep(args.interval)
    except KeyboardInterrupt:
        return 0


def cmd_role(args: argparse.Namespace) -> int:
    server, token, _prof = require_token(args)
    data = http_json(
        server,
        "POST",
        "/v1/roles",
        token=token,
        payload={"nick": args.nick, "role": args.role, "channel": args.channel},
    )
    p = data["principal"]
    print_out(args, data, [f"Role updated: @{p['nick']} -> {p['role']}"])
    return 0


def cmd_roles(args: argparse.Namespace) -> int:
    return cmd_agents(args)


def cmd_lock(args: argparse.Namespace) -> int:
    server, token, _prof = require_token(args)
    data = http_json(
        server,
        "POST",
        "/v1/locks",
        token=token,
        payload={
            "resource": args.resource,
            "channel": args.channel,
            "ttl_seconds": args.ttl,
            "note": args.note,
        },
    )
    lk = data["lock"]
    print_out(
        args,
        data,
        [f"LOCKED {lk['resource']} by @{lk['owner_nick']} in {lk['channel']} (expires_at={lk['expires_at']})"],
    )
    return 0


def cmd_unlock(args: argparse.Namespace) -> int:
    server, token, _prof = require_token(args)
    data = http_json(
        server,
        "POST",
        "/v1/locks/release",
        token=token,
        payload={"resource": args.resource, "force": bool(args.force)},
    )
    rel = data.get("released")
    print_out(
        args,
        data,
        [f"UNLOCKED {rel['resource']}" if rel else f"Resource '{args.resource}' was not locked."],
    )
    return 0


def cmd_locks(args: argparse.Namespace) -> int:
    server, _token, _prof = resolve_connection(args)
    qs = f"?{urllib.parse.urlencode({'channel': args.channel})}" if args.channel else ""
    data = http_json(server, "GET", f"/v1/locks{qs}")
    lines = [
        f"{lk['resource']:<24} holder=@{lk['owner_nick']:<14} channel={lk['channel']:<16} note={lk.get('note') or '-'}"
        for lk in data.get("locks", [])
    ]
    print_out(args, data, lines or ["No active equipment or branch locks."])
    return 0


def cmd_report(args: argparse.Namespace) -> int:
    server, token, _prof = require_token(args)
    if args.report_cmd == "create":
        body = args.body or ""
        if args.body_file:
            body = Path(args.body_file).read_text(encoding="utf-8")
        data = http_json(
            server,
            "POST",
            "/v1/reports",
            token=token,
            payload={
                "channel": args.channel,
                "title": args.title,
                "status": args.status,
                "summary": args.summary,
                "body": body,
                "task_id": args.task_id,
                "git_repo": args.repo,
                "git_branch": args.branch,
                "git_commit": args.commit,
            },
        )
        rep = data["report"]
        print_out(
            args,
            data,
            [f"Report {rep['id']} [{rep['status']}] posted to {rep['channel']}: {rep['title']}"],
        )
        return 0

    if args.report_cmd == "list":
        params = {"limit": args.limit}
        if args.channel:
            params["channel"] = args.channel
        if args.author:
            params["author"] = args.author
        data = http_json(server, "GET", f"/v1/reports?{urllib.parse.urlencode(params)}", token=token)
        lines = [
            f"{r['id']} [{r['status']}] {r['channel']} @{r['author_nick']}: {r['title']}"
            for r in data.get("reports", [])
        ]
        print_out(args, data, lines or ["No reports found."])
        return 0

    if args.report_cmd == "show":
        data = http_json(server, "GET", f"/v1/reports/{urllib.parse.quote(args.id)}", token=token)
        r = data["report"]
        print_out(
            args,
            data,
            [
                f"ID: {r['id']}  Status: {r['status']}  Channel: {r['channel']}  Author: @{r['author_nick']}",
                f"Title: {r['title']}",
                f"Git: {r.get('git_repo') or '-'} @ {r.get('git_branch') or '-'} ({r.get('git_commit') or '-'})",
                "---",
                r.get("body") or r.get("summary") or "",
            ],
        )
        return 0

    raise CliError("Unsupported report subcommand")


def cmd_task(args: argparse.Namespace) -> int:
    server, token, _prof = require_token(args)
    if args.task_cmd == "create":
        data = http_json(
            server,
            "POST",
            "/v1/tasks",
            token=token,
            payload={
                "channel": args.channel,
                "title": " ".join(args.title),
                "body": args.body,
                "kind": args.kind,
                "assignee": args.assign,
                "repo": args.repo,
                "branch": args.branch,
                "issue_url": args.issue,
            },
        )
        t = data["task"]
        owner = f" -> @{t['assignee']}" if t.get("assignee") else ""
        print_out(args, data, [f"Created {t['kind']} {t['id']} in {t['channel']}{owner}: {t['title']}"])
        return 0
    if args.task_cmd == "assign":
        data = http_json(
            server,
            "POST",
            f"/v1/tasks/{urllib.parse.quote(args.id)}/assign",
            token=token,
            payload={"assignee": args.nick},
        )
        t = data["task"]
        print_out(args, data, [f"Task {t['id']} -> {t['status']} (assignee={t.get('assignee') or '-'})"])
        return 0
    if args.task_cmd == "status":
        data = http_json(
            server,
            "POST",
            f"/v1/tasks/{urllib.parse.quote(args.id)}/status",
            token=token,
            payload={"status": args.status, "note": args.note, "branch": args.branch},
        )
        t = data["task"]
        print_out(args, data, [f"Task {t['id']} -> {t['status']} (branch={t.get('branch') or '-'})"])
        return 0
    if args.task_cmd == "show":
        data = http_json(server, "GET", f"/v1/tasks/{urllib.parse.quote(args.id)}", token=token)
        t = data["task"]
        lines = [
            f"{t['id']} [{t['kind']}/{t['status']}] {t['channel']}: {t['title']}",
            f"assignee={t.get('assignee') or '-'} repo={t.get('repo') or '-'} "
            f"branch={t.get('branch') or '-'} issue={t.get('issue_url') or '-'}",
        ]
        if t.get("body"):
            lines.append(t["body"])
        for ev in data.get("events", []):
            note = f" - {ev['note']}" if ev.get("note") else ""
            lines.append(
                f"  {ev['action']} {ev.get('from_status') or '-'} -> {ev.get('to_status') or '-'} "
                f"by @{ev['actor_id']}{note}"
            )
        print_out(args, data, lines)
        return 0
    if args.task_cmd == "list":
        qs = urllib.parse.urlencode({"channel": args.channel})
        data = http_json(server, "GET", f"/v1/tasks?{qs}", token=token)
        lines = [
            f"{t['id']} [{t['status']}] claimed_by={t.get('claimed_by') or '-'}: {t['title']}"
            for t in data.get("tasks", [])
        ]
        print_out(args, data, lines or [f"No tasks in {args.channel}."])
        return 0
    if args.task_cmd in ("claim", "done", "release"):
        data = http_json(
            server,
            "POST",
            f"/v1/tasks/{urllib.parse.quote(args.id)}/{args.task_cmd}",
            token=token,
        )
        t = data["task"]
        print_out(args, data, [f"Task {t['id']} -> {t['status']} (claimed_by={t.get('claimed_by') or '-'})"])
        return 0
    raise CliError("Unsupported task subcommand")


def cmd_inbox(args: argparse.Namespace) -> int:
    """Everything this account has to do, grouped by project."""
    server, token, _prof = require_token(args)
    data = http_json(server, "GET", "/v1/inbox", token=token)
    inbox = data["inbox"]
    lines = [
        f"@{inbox['account']}: {inbox['task_count']} task(s), "
        f"{inbox['review_count']} waiting for your review"
    ]
    for proj in inbox.get("projects", []):
        label = proj.get("project") or proj["channel"]
        role = f" (you: {proj['my_role']})" if proj.get("my_role") else ""
        lines.append(f"{proj['channel']} - {label}{role}")
        for repo in proj.get("repos", []):
            lines.append(f"  repo {repo['name']}: {repo['local_path']}")
        for t in proj.get("tasks", []):
            lines.append(
                f"  {t['id']} [{t['kind']}/{t['status']}] branch={t.get('branch') or '-'}: {t['title']}"
            )
        for t in proj.get("to_review", []):
            lines.append(
                f"  REVIEW {t['id']} by @{t.get('assignee') or '-'} branch={t.get('branch') or '-'}: {t['title']}"
            )
    print_out(args, data, lines)
    return 0


def cmd_project(args: argparse.Namespace) -> int:
    server, token, _prof = require_token(args)
    if args.project_cmd == "create":
        data = http_json(
            server,
            "POST",
            "/v1/projects",
            token=token,
            payload={"channel": args.channel, "name": args.name, "description": args.description},
        )
        p = data["project"]
        print_out(args, data, [f"Created project {p['channel']}: {p['name']}"])
        return 0
    if args.project_cmd == "list":
        data = http_json(server, "GET", "/v1/projects", token=token)
        lines = [f"{p['channel']}: {p['name']}" for p in data.get("projects", [])]
        print_out(args, data, lines or ["No projects yet."])
        return 0
    if args.project_cmd == "add":
        data = http_json(
            server,
            "POST",
            "/v1/projects/members",
            token=token,
            payload={"channel": args.channel, "nick": args.nick, "role": args.role},
        )
        m = data["member"]
        print_out(args, data, [f"@{m['principal_id']} is {m['role']} in {data['channel']}"])
        return 0
    if args.project_cmd == "members":
        qs = urllib.parse.urlencode({"channel": args.channel})
        data = http_json(server, "GET", f"/v1/projects/members?{qs}", token=token)
        lines = [f"@{m['principal_id']}: {m['role']}" for m in data.get("members", [])]
        print_out(args, data, lines or [f"No members in {args.channel}."])
        return 0
    raise CliError("Unsupported project subcommand")


def cmd_webhook(args: argparse.Namespace) -> int:
    server, token, prof = require_token(args)
    if args.webhook_cmd == "set":
        events = [e.strip() for e in args.events.split(",") if e.strip()] if args.events else None
        data = http_json(
            server,
            "POST",
            "/v1/webhooks",
            token=token,
            payload={
                "url": args.url,
                "events": events,
                "notify_when": args.notify_when,
                "rotate_secret": bool(args.rotate_secret),
            },
        )
        update_profile(prof["_profile_name"], {"webhook_secret": data["secret"]}, make_default=False)
        wh = data["webhook"]
        print_out(
            args,
            data,
            [
                f"Webhook saved for @{wh['principal_id']}: {wh['url']}",
                f"Events: {', '.join(wh['events'])} | notify_when={wh['notify_when']}",
                f"Signing secret saved to profile '{prof['_profile_name']}'.",
            ],
        )
        return 0

    if args.webhook_cmd == "status":
        data = http_json(server, "GET", "/v1/webhooks", token=token)
        wh = data.get("webhook")
        if not wh:
            print_out(args, data, ["No webhook configured."])
        else:
            print_out(
                args,
                data,
                [
                    f"URL: {wh['url']} (enabled={wh['enabled']}, notify_when={wh['notify_when']})",
                    f"Events: {', '.join(wh['events'])}",
                    f"Last status: {wh.get('last_status')} error={wh.get('last_error') or 'none'}",
                ],
            )
        return 0

    if args.webhook_cmd == "test":
        data = http_json(server, "POST", "/v1/webhooks/test", token=token)
        d = data["delivery"]
        print_out(args, data, [f"Webhook test {d['status']} (HTTP {d.get('http_status')}): {d['id']}"])
        return 0

    if args.webhook_cmd == "delete":
        data = http_json(server, "DELETE", "/v1/webhooks", token=token)
        print_out(args, data, [f"Webhook removed: {data.get('deleted')}"])
        return 0

    if args.webhook_cmd == "deliveries":
        qs = urllib.parse.urlencode({"limit": args.limit})
        data = http_json(server, "GET", f"/v1/webhooks/deliveries?{qs}", token=token)
        lines = [
            f"{d['id']} {d['event_type']} {d['channel']} status={d['status']} attempts={d['attempts']} http={d.get('http_status')}"
            for d in data.get("deliveries", [])
        ]
        print_out(args, data, lines or ["No webhook deliveries recorded."])
        return 0

    if args.webhook_cmd == "listen":
        return run_webhook_listener(args, server, token, prof)

    raise CliError("Unsupported webhook subcommand")


def run_webhook_listener(
    args: argparse.Namespace,
    server: str,
    token: str,
    prof: dict[str, Any],
) -> int:
    secret = args.secret or prof.get("webhook_secret") or ""
    route_path = args.path if args.path.startswith("/") else f"/{args.path}"
    exec_cmd = args.exec_cmd

    class ThreadedHTTPServer(socketserver.ThreadingMixIn, http.server.HTTPServer):
        daemon_threads = True
        allow_reuse_address = True

    def make_handler(signing_secret: str):
        class HookHandler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                parsed = urllib.parse.urlparse(self.path)
                if parsed.path != route_path:
                    self.send_response(404)
                    self.end_headers()
                    return
                length = int(self.headers.get("Content-Length", "0"))
                raw_body = self.rfile.read(length)

                if signing_secret:
                    sig_header = (
                        self.headers.get("X-AirCD-Signature-256")
                        or self.headers.get("X-Agent-Ping-Signature-256")
                        or ""
                    )
                    expected = "sha256=" + hmac.new(
                        signing_secret.encode("utf-8"), raw_body, hashlib.sha256
                    ).hexdigest()
                    if not hmac.compare_digest(expected, sig_header):
                        self.send_response(401)
                        self.end_headers()
                        self.wfile.write(b'{"ok":false,"error":"invalid signature"}')
                        return

                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.end_headers()
                self.wfile.write(b'{"ok":true}')

                try:
                    event = json.loads(raw_body.decode("utf-8", errors="replace"))
                except Exception:
                    event = {"raw": raw_body.decode("utf-8", errors="replace")}

                if args.json:
                    print(json.dumps(event), flush=True)
                else:
                    print(
                        f"[{event.get('event')}] {event.get('channel')} from={event.get('from')}: {event.get('message')}",
                        flush=True,
                    )

                if exec_cmd:
                    env = dict(os.environ)
                    env["AIRCD_EVENT"] = json.dumps(event)
                    subprocess.Popen(
                        exec_cmd,
                        shell=True,
                        stdin=subprocess.PIPE,
                        env=env,
                    ).communicate(input=raw_body)

            def log_message(self, format: str, *args: Any) -> None:
                return

        return HookHandler

    # Bind socket first so ephemeral port 0 resolves before registering callback.
    httpd = ThreadedHTTPServer((args.bind, args.port), make_handler(secret))
    actual_port = httpd.server_address[1]
    callback_host = "127.0.0.1" if args.bind == "0.0.0.0" else args.bind
    callback_url = args.public_url or f"http://{callback_host}:{actual_port}{route_path}"

    if args.register:
        events = [e.strip() for e in args.events.split(",") if e.strip()] if args.events else None
        reg_res = http_json(
            server,
            "POST",
            "/v1/webhooks",
            token=token,
            payload={"url": callback_url, "events": events, "notify_when": args.notify_when},
        )
        secret = reg_res["secret"]
        update_profile(prof["_profile_name"], {"webhook_secret": secret}, make_default=False)
        httpd.RequestHandlerClass = make_handler(secret)
        print(f"Registered webhook {callback_url} on {server}", flush=True)

    print(f"Listening for signed webhooks on http://{args.bind}:{actual_port}{route_path}", flush=True)
    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        return 0
    finally:
        httpd.server_close()
    return 0


# ── Git integration commands ──────────────────────────────────


def _run_git(repo_path: Path, git_args: list[str]) -> str:
    proc = subprocess.run(
        ["git", "-C", str(repo_path), *git_args],
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    if proc.returncode != 0:
        raise CliError(f"git {' '.join(git_args)} failed: {proc.stderr.strip() or proc.stdout.strip()}")
    return proc.stdout.strip()


def _detect_git_meta(repo_path: Path) -> tuple[str, str, str | None, str]:
    root = Path(_run_git(repo_path, ["rev-parse", "--show-toplevel"]))
    repo_name = root.name
    try:
        branch = _run_git(root, ["rev-parse", "--abbrev-ref", "HEAD"])
    except CliError:
        branch = "main"
    remote = None
    try:
        remote = _run_git(root, ["remote", "get-url", "origin"])
    except CliError:
        pass
    return str(root), repo_name, remote, branch


def _git_channel(repo_name: str, explicit: str | None) -> str:
    if explicit:
        return explicit if explicit.startswith("#") else f"#{explicit}"
    cleaned = "".join(ch if ch.isalnum() or ch in "-_." else "-" for ch in repo_name.lower()).strip("-")
    return f"#{cleaned or 'git'}"


def _install_single_hook(hooks_dir: Path, hook_name: str, script_body: str) -> Path:
    hooks_dir.mkdir(parents=True, exist_ok=True)
    hook_path = hooks_dir / hook_name
    if hook_path.exists():
        existing = hook_path.read_text(encoding="utf-8", errors="replace")
        if MANAGED_HOOK_MARKER not in existing:
            backup = hooks_dir / f"{hook_name}.aircd-orig"
            if not backup.exists():
                backup.write_text(existing, encoding="utf-8")
            script_body += (
                f"\nORIG_HOOK=\"$(dirname \"$0\")/{hook_name}.aircd-orig\"\n"
                f"if [ -x \"$ORIG_HOOK\" ]; then \"$ORIG_HOOK\" \"$@\"; fi\n"
            )
    hook_path.write_text(script_body, encoding="utf-8", newline="\n")
    try:
        os.chmod(hook_path, 0o755)
    except OSError:
        pass
    return hook_path


def _install_git_hooks(
    repo_root: Path,
    repo_name: str,
    channel: str,
    server: str,
    profile_name: str,
) -> list[str]:
    hooks_rel = _run_git(repo_root, ["rev-parse", "--git-path", "hooks"])
    hooks_dir = Path(hooks_rel)
    if not hooks_dir.is_absolute():
        hooks_dir = repo_root / hooks_dir

    py_exe = Path(sys.executable).as_posix()
    cli_root = Path(__file__).resolve().parent.parent.as_posix()
    cfg_dir = config_dir().as_posix()

    installed: list[str] = []
    for hook_name, event_kind in (("post-commit", "commit"), ("post-merge", "merge")):
        script = (
            "#!/bin/sh\n"
            f"{MANAGED_HOOK_MARKER}\n"
            f"AIRCD_CONFIG_DIR={shlex.quote(cfg_dir)} "
            f"PYTHONPATH={shlex.quote(cli_root)} "
            f"{shlex.quote(py_exe)} -m aircd.cli "
            f"--server {shlex.quote(server)} --profile {shlex.quote(profile_name)} "
            f"git emit-hook --repo {shlex.quote(repo_name)} --channel {shlex.quote(channel)} "
            f"--event {shlex.quote(event_kind)} --path {shlex.quote(repo_root.as_posix())} "
            "|| true\n"
        )
        installed.append(str(_install_single_hook(hooks_dir, hook_name, script)))
    return installed


def cmd_git(args: argparse.Namespace) -> int:
    if args.git_cmd == "list":
        server, _token, _prof = resolve_connection(args)
        data = http_json(server, "GET", "/v1/gits")
        lines = [
            f"{g['name']:<24} {g['channel']:<20} branch={g.get('last_branch') or g.get('default_branch') or '-'} path={g['local_path']}"
            for g in data.get("git_repos", [])
        ]
        print_out(args, data, lines or ["No connected git repositories."])
        return 0

    if args.git_cmd == "status":
        target = Path(args.path).resolve()
        root, repo_name, remote, branch = _detect_git_meta(target)
        status = _run_git(Path(root), ["status", "--short"])
        info = {"root": root, "repo": repo_name, "branch": branch, "remote": remote, "dirty": status.splitlines()}
        print_out(
            args,
            info,
            [f"Repo: {repo_name} ({root})", f"Branch: {branch}", f"Remote: {remote or '(local-only)'}", status or "Working tree clean"],
        )
        return 0

    if args.git_cmd == "register":
        server, token, _prof = require_token(args)
        target = Path(args.path).resolve()
        root, detected_name, remote, branch = _detect_git_meta(target)
        repo_name = args.repo or detected_name
        channel = _git_channel(repo_name, args.channel)
        data = http_json(
            server,
            "POST",
            "/v1/gits",
            token=token,
            payload={
                "name": repo_name,
                "channel": channel,
                "local_path": root,
                "remote_url": remote,
                "default_branch": args.branch or branch,
                "webhook_secret": args.secret,
            },
        )
        lines = [f"Registered git repo '{data['git_repo']['name']}' -> {data['git_repo']['channel']} ({root})"]
        if data.get("webhook_secret"):
            lines.append(f"GitHub webhook secret (save now): {data['webhook_secret']}")
            lines.append(f"GitHub webhook URL: {server}/v1/git/webhook/{data['git_repo']['name']}")
        print_out(args, data, lines)
        return 0

    if args.git_cmd == "install-hooks":
        server, _token, prof = require_token(args)
        target = Path(args.path).resolve()
        root, detected_name, _remote, _branch = _detect_git_meta(target)
        repo_name = args.repo or detected_name
        channel = _git_channel(repo_name, args.channel)
        installed = _install_git_hooks(Path(root), repo_name, channel, server, prof["_profile_name"])
        print_out(
            args,
            {"ok": True, "hooks": installed, "repo": repo_name, "channel": channel},
            [f"Installed hooks for '{repo_name}' ({channel}):", *[f"  - {p}" for p in installed]],
        )
        return 0

    if args.git_cmd == "init-workspace":
        server, token, prof = require_token(args)
        target = Path(args.path).resolve()
        root, detected_name, remote, branch = _detect_git_meta(target)
        repo_name = args.repo or detected_name
        channel = _git_channel(repo_name, args.channel)
        reg = http_json(
            server,
            "POST",
            "/v1/gits",
            token=token,
            payload={
                "name": repo_name,
                "channel": channel,
                "local_path": root,
                "remote_url": remote,
                "default_branch": args.branch or branch,
            },
        )
        hooks = _install_git_hooks(Path(root), reg["git_repo"]["name"], reg["git_repo"]["channel"], server, prof["_profile_name"])
        out = {"ok": True, "git_repo": reg["git_repo"], "webhook_secret": reg.get("webhook_secret"), "hooks": hooks}
        print_out(
            args,
            out,
            [
                f"Workspace ready for '{reg['git_repo']['name']}' in {reg['git_repo']['channel']}",
                f"Hooks installed: {len(hooks)}",
                *(["GitHub webhook secret: " + reg["webhook_secret"]] if reg.get("webhook_secret") else []),
            ],
        )
        return 0

    if args.git_cmd == "emit-hook":
        server, token, _prof = resolve_connection(args)
        if not token:
            return 0
        target = Path(args.path).resolve()
        branch = _run_git(target, ["rev-parse", "--abbrev-ref", "HEAD"])
        commit = _run_git(target, ["rev-parse", "--short=12", "HEAD"])
        subject = _run_git(target, ["log", "-1", "--pretty=%s"])
        try:
            http_json(
                server,
                "POST",
                "/v1/git/event",
                token=token,
                payload={
                    "repo": args.repo,
                    "channel": args.channel,
                    "event": args.event,
                    "branch": branch,
                    "commit": commit,
                    "title": subject,
                    "summary": f"{commit} {subject}",
                },
                timeout=3.0,
            )
        except CliError:
            return 0
        return 0

    if args.git_cmd == "worktree-setup":
        server, token, prof = require_token(args)
        target = Path(args.path).resolve()
        root_str, detected_name, _remote, current_branch = _detect_git_meta(target)
        root = Path(root_str)
        safe_branch = "".join(ch if ch.isalnum() or ch in "-_." else "-" for ch in args.branch).strip("-")
        worktree_dir = Path(args.worktree_dir) if args.worktree_dir else (root.parent / f"{root.name}-worktrees" / safe_branch)
        worktree_dir.parent.mkdir(parents=True, exist_ok=True)
        exists = subprocess.run(
            ["git", "-C", str(root), "show-ref", "--verify", "--quiet", f"refs/heads/{args.branch}"],
            capture_output=True,
        ).returncode == 0
        if exists:
            _run_git(root, ["worktree", "add", str(worktree_dir), args.branch])
        else:
            _run_git(root, ["worktree", "add", "-b", args.branch, str(worktree_dir), args.base or current_branch])

        channel = _git_channel(detected_name, args.channel)
        lock_res = None
        if args.lock:
            lock_res = http_json(
                server,
                "POST",
                "/v1/locks",
                token=token,
                payload={
                    "resource": f"repo:{detected_name}:{args.branch}",
                    "channel": channel,
                    "ttl_seconds": args.ttl,
                    "note": f"worktree owned by @{prof.get('nick') or 'agent'}",
                },
            )
        ev = http_json(
            server,
            "POST",
            "/v1/git/event",
            token=token,
            payload={
                "repo": detected_name,
                "channel": channel,
                "event": "worktree",
                "branch": args.branch,
                "summary": f"started worktree {worktree_dir}",
            },
        )
        out = {"ok": True, "worktree": str(worktree_dir), "branch": args.branch, "channel": ev["channel"], "lock": lock_res}
        print_out(args, out, [f"Worktree created: {worktree_dir} (branch={args.branch}, channel={ev['channel']})"])
        return 0

    if args.git_cmd == "pr-report":
        server, token, _prof = require_token(args)
        target = Path(args.path).resolve()
        root_str, detected_name, _remote, branch = _detect_git_meta(target)
        root = Path(root_str)
        commit = _run_git(root, ["rev-parse", "--short=12", "HEAD"])
        has_base = subprocess.run(
            ["git", "-C", str(root), "rev-parse", "--verify", "--quiet", args.base],
            capture_output=True,
        ).returncode == 0
        log_range = f"{args.base}..HEAD" if has_base and branch != args.base else "-n 5"
        log_args = ["log", "--oneline", *log_range.split()]
        commits_text = _run_git(root, log_args)
        diff_stat = _run_git(root, ["diff", "--stat", f"{args.base}...HEAD"]) if has_base and branch != args.base else ""
        channel = _git_channel(detected_name, args.channel)
        title = args.title or f"PR ready: {detected_name} branch {branch}"
        body = "\n".join(
            part
            for part in [
                args.body or "",
                f"Branch: `{branch}` @ `{commit}` (base `{args.base}`)",
                "Commits:\n```\n" + (commits_text or "(none)") + "\n```",
                ("Diff stat:\n```\n" + diff_stat + "\n```") if diff_stat else "",
            ]
            if part
        )
        rep = http_json(
            server,
            "POST",
            "/v1/reports",
            token=token,
            payload={
                "channel": channel,
                "title": title,
                "status": args.status,
                "body": body,
                "git_repo": detected_name,
                "git_branch": branch,
                "git_commit": commit,
            },
        )
        r = rep["report"]
        print_out(args, rep, [f"PR Report {r['id']} posted to {r['channel']}: {r['title']}"])
        return 0

    raise CliError("Unsupported git subcommand")


# ── Argument parser ───────────────────────────────────────────


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="airc", description="aircd CLI (IRC + Webhooks + Roles + Locks + Git)")
    parser.add_argument("--server", help=f"aircd HTTP URL (default {DEFAULT_SERVER_URL})")
    parser.add_argument("--token", help="Bearer auth token (overrides saved profile)")
    parser.add_argument("--profile", help="Saved local profile name")
    parser.add_argument("--json", action="store_true", help="Output machine-readable JSON")

    sub = parser.add_subparsers(dest="cmd", required=True)

    p_wel = sub.add_parser("welcome", help="Fetch server welcome page / instructions")
    p_wel.add_argument("--format", choices=["json", "md", "html"], default="md")
    p_wel.set_defaults(func=cmd_welcome)

    p_reg = sub.add_parser("register", help="Register a new agent nick & save token")
    p_reg.add_argument("--nick", required=True)
    p_reg.add_argument("--role", default="member")
    p_reg.add_argument("--team")
    p_reg.add_argument("--channels", default="#general")
    p_reg.add_argument("--no-default", action="store_true")
    p_reg.add_argument(
        "--invite",
        help="Owner registration key (or set AIRC_INVITE); required unless the server allows open registration",
    )
    p_reg.set_defaults(func=cmd_register)

    p_owner = sub.add_parser("owner", help="Owner-only administration (needs the owner key)")
    owner_sub = p_owner.add_subparsers(dest="owner_cmd", required=True)
    p_reissue = owner_sub.add_parser(
        "reissue", help="Revoke all of an agent's tokens and save a fresh one to its profile"
    )
    p_reissue.add_argument("--nick", required=True)
    p_reissue.add_argument("--invite", help="Owner key (or set AIRC_INVITE)")
    p_reissue.add_argument(
        "--make-default", action="store_true", help="Also make this the default profile"
    )
    p_reissue.set_defaults(func=cmd_owner_reissue)

    p_wake = owner_sub.add_parser(
        "wake", help="Configure the command the server runs to start an account's agent"
    )
    wake_sub = p_wake.add_subparsers(dest="wake_cmd", required=True)
    p_wset = wake_sub.add_parser(
        "set",
        help="Placeholders: {prompt} {nick} {server} {reason} {channel} {task_id} {repo_path}",
    )
    p_wset.add_argument("--command", required=True, help="Executable (full path, or a name on the server's PATH)")
    p_wset.add_argument("--arg", action="append", help="One argument template; repeat for each. Use --arg=-p for dash values")
    p_wset.add_argument("--cwd", help="Working directory template (default: the task's repo path)")
    p_wset.add_argument("--env", action="append", metavar="KEY=VALUE")
    p_wset.add_argument("--unset-env", action="append", metavar="KEY", help="Remove a variable from the agent's environment")
    p_wset.add_argument("--prompt", help="Prompt template for {prompt} (default: built-in inbox instructions)")
    p_wset.add_argument("--disabled", action="store_true")
    p_wset.add_argument("--cooldown", type=int, help="Seconds between runs (default 20)")
    p_wset.add_argument("--timeout", type=int, help="Kill a run after this many seconds (default 1800)")
    p_wset.add_argument("--max-per-hour", type=int, help="Launch limit per hour (default 12, 0 = unlimited)")
    p_wset.add_argument(
        "--session",
        choices=["fresh", "resume"],
        default="fresh",
        help="fresh: new conversation per launch (workers). resume: continue one conversation (coordinators)",
    )
    p_wset.add_argument("--resume-arg", action="append", help="Argument template used once a conversation is stored; {session_id} is its id")
    p_wset.add_argument("--session-marker", help="Text that precedes the conversation id in the agent's output")
    p_wset.add_argument("--session-scope", choices=["account", "channel"], default="account", help="One conversation per account, or one per project channel")
    wake_parsers = [p_wset]
    for name, text in (
        ("show", "Show an account's launcher"),
        ("delete", "Remove an account's launcher"),
        ("test", "Start the account's agent now"),
        ("reset", "Forget the stored conversation so the next launch starts fresh"),
    ):
        wake_sub_parser = wake_sub.add_parser(name, help=text)
        if name == "test":
            wake_sub_parser.add_argument("--message", help="Reason or instruction handed to the agent")
        wake_parsers.append(wake_sub_parser)
    p_wruns = wake_sub.add_parser("runs", help="List recent launches")
    p_wruns.add_argument("--limit", type=int, default=20)
    p_wruns.add_argument("--logs", action="store_true", help="Include the end of each run's output")
    p_wruns.add_argument("--nick")
    p_wruns.add_argument("--invite", help="Owner key (or set AIRC_INVITE)")
    p_wruns.set_defaults(func=cmd_owner_wake)
    for wake_parser in wake_parsers:
        wake_parser.add_argument("--nick", required=True)
        wake_parser.add_argument("--invite", help="Owner key (or set AIRC_INVITE)")
        wake_parser.set_defaults(func=cmd_owner_wake)

    p_log = sub.add_parser("login", help="Verify token, join channels & refresh profile")
    p_log.add_argument("--channels", default="#general")
    p_log.add_argument("--rotate", action="store_true")
    p_log.set_defaults(func=cmd_login)

    sub.add_parser("whoami", help="Show authenticated profile").set_defaults(func=cmd_whoami)
    sub.add_parser("agents", help="List agents, roles & presence").set_defaults(func=cmd_agents)
    sub.add_parser("channels", help="List active channels").set_defaults(func=cmd_channels)

    p_join = sub.add_parser("join", help="Join one or more IRC channels")
    p_join.add_argument("channels", nargs="+")
    p_join.set_defaults(func=cmd_join)

    p_send = sub.add_parser("send", help="Send a message to #channel or DM @nick")
    p_send.add_argument("target")
    p_send.add_argument("message", nargs="+")
    p_send.set_defaults(func=cmd_send)

    p_msgs = sub.add_parser("messages", help="Read channel or DM message history")
    p_msgs.add_argument("--channel", default="#general")
    p_msgs.add_argument("--dm", help="Peer @nick or 'all'")
    p_msgs.add_argument("--after-seq", type=int, default=0)
    p_msgs.add_argument("--limit", type=int, default=100)
    p_msgs.set_defaults(func=cmd_messages)

    p_watch = sub.add_parser("watch", help="Tail live channel or DM messages")
    p_watch.add_argument("--channel", default="#general")
    p_watch.add_argument("--dm")
    p_watch.add_argument("--after-seq", type=int, default=0)
    p_watch.add_argument("--interval", type=float, default=1.5)
    p_watch.set_defaults(func=cmd_watch)

    p_role = sub.add_parser("role", help="Assign role to an agent")
    p_role.add_argument("nick")
    p_role.add_argument("role")
    p_role.add_argument("--channel")
    p_role.set_defaults(func=cmd_role)

    sub.add_parser("roles", help="List agents and roles").set_defaults(func=cmd_roles)

    p_lock = sub.add_parser("lock", help="Reserve shared dev equipment or branch")
    p_lock.add_argument("resource")
    p_lock.add_argument("--channel", default="#general")
    p_lock.add_argument("--ttl", type=int, default=1800)
    p_lock.add_argument("--note")
    p_lock.set_defaults(func=cmd_lock)

    p_unlock = sub.add_parser("unlock", help="Release equipment or branch lock")
    p_unlock.add_argument("resource")
    p_unlock.add_argument("--force", action="store_true")
    p_unlock.set_defaults(func=cmd_unlock)

    p_locks = sub.add_parser("locks", help="List active equipment & branch locks")
    p_locks.add_argument("--channel")
    p_locks.set_defaults(func=cmd_locks)

    # Report subcommands
    p_rep = sub.add_parser("report", help="Create or read structured status/completion reports")
    rep_sub = p_rep.add_subparsers(dest="report_cmd", required=True)
    p_rc = rep_sub.add_parser("create")
    p_rc.add_argument("--channel", default="#general")
    p_rc.add_argument("--title", required=True)
    p_rc.add_argument("--status", choices=ALLOWED_REPORT_STATUSES, default="info")
    p_rc.add_argument("--summary")
    p_rc.add_argument("--body", default="")
    p_rc.add_argument("--body-file")
    p_rc.add_argument("--task-id")
    p_rc.add_argument("--repo")
    p_rc.add_argument("--branch")
    p_rc.add_argument("--commit")
    p_rc.set_defaults(func=cmd_report)

    p_rl = rep_sub.add_parser("list")
    p_rl.add_argument("--channel")
    p_rl.add_argument("--author")
    p_rl.add_argument("--limit", type=int, default=30)
    p_rl.set_defaults(func=cmd_report)

    p_rs = rep_sub.add_parser("show")
    p_rs.add_argument("id")
    p_rs.set_defaults(func=cmd_report)

    # Task subcommands
    p_task = sub.add_parser("task", help="Coordinate channel tasks (create/list/claim/done/release)")
    task_sub = p_task.add_subparsers(dest="task_cmd", required=True)
    p_tc = task_sub.add_parser("create")
    p_tc.add_argument("channel")
    p_tc.add_argument("title", nargs="+")
    p_tc.add_argument("--body", help="Full description; enough that a new assignee need not read the chat")
    p_tc.add_argument("--kind", choices=["task", "bug", "feature"], default="task")
    p_tc.add_argument("--assign", metavar="NICK", help="Project member to give the task to")
    p_tc.add_argument("--repo")
    p_tc.add_argument("--branch")
    p_tc.add_argument("--issue", metavar="URL", help="Linked GitHub issue")
    p_tc.set_defaults(func=cmd_task)

    p_tas = task_sub.add_parser("assign", help="Assign or reassign a task; omit NICK to return it to the pool")
    p_tas.add_argument("id")
    p_tas.add_argument("nick", nargs="?")
    p_tas.set_defaults(func=cmd_task)

    p_tst = task_sub.add_parser("status", help="Move a task: in_progress, review, changes_requested, merged, cancelled")
    p_tst.add_argument("id")
    p_tst.add_argument("status")
    p_tst.add_argument("--note")
    p_tst.add_argument("--branch")
    p_tst.set_defaults(func=cmd_task)

    p_tsh = task_sub.add_parser("show", help="Show a task with its history")
    p_tsh.add_argument("id")
    p_tsh.set_defaults(func=cmd_task)

    p_tl = task_sub.add_parser("list")
    p_tl.add_argument("channel", nargs="?", default="#general")
    p_tl.set_defaults(func=cmd_task)

    for action in ("claim", "done", "release"):
        p_ta = task_sub.add_parser(action)
        p_ta.add_argument("id")
        p_ta.set_defaults(func=cmd_task)

    p_inbox = sub.add_parser("inbox", help="Show your tasks and reviews across all projects")
    p_inbox.set_defaults(func=cmd_inbox)

    p_proj = sub.add_parser("project", help="Create projects and manage their members")
    proj_sub = p_proj.add_subparsers(dest="project_cmd", required=True)
    p_pc = proj_sub.add_parser("create")
    p_pc.add_argument("channel")
    p_pc.add_argument("--name")
    p_pc.add_argument("--description")
    p_pc.set_defaults(func=cmd_project)
    p_pl = proj_sub.add_parser("list")
    p_pl.set_defaults(func=cmd_project)
    p_pa = proj_sub.add_parser("add", help="Add an account to a project or change its role")
    p_pa.add_argument("channel")
    p_pa.add_argument("nick")
    p_pa.add_argument("--role", default="member")
    p_pa.set_defaults(func=cmd_project)
    p_pm = proj_sub.add_parser("members")
    p_pm.add_argument("channel")
    p_pm.set_defaults(func=cmd_project)

    # Webhook subcommands
    p_wh = sub.add_parser("webhook", help="Configure, test & listen for signed agent webhooks")
    wh_sub = p_wh.add_subparsers(dest="webhook_cmd", required=True)
    p_ws = wh_sub.add_parser("set")
    p_ws.add_argument("url")
    p_ws.add_argument("--events", help="Comma-separated event filter")
    p_ws.add_argument("--notify-when", choices=["always", "offline", "mentions"], default="mentions")
    p_ws.add_argument("--rotate-secret", action="store_true")
    p_ws.set_defaults(func=cmd_webhook)

    wh_sub.add_parser("status").set_defaults(func=cmd_webhook)
    wh_sub.add_parser("test").set_defaults(func=cmd_webhook)
    wh_sub.add_parser("delete").set_defaults(func=cmd_webhook)

    p_wd = wh_sub.add_parser("deliveries")
    p_wd.add_argument("--limit", type=int, default=30)
    p_wd.set_defaults(func=cmd_webhook)

    p_wl = wh_sub.add_parser("listen")
    p_wl.add_argument("--bind", default="127.0.0.1")
    p_wl.add_argument("--port", type=int, default=8787)
    p_wl.add_argument("--path", default="/hook")
    p_wl.add_argument("--public-url", help="External callback URL to register")
    p_wl.add_argument("--secret", help="Expected HMAC signing secret")
    p_wl.add_argument("--events", help="Comma-separated events when using --register")
    p_wl.add_argument("--notify-when", choices=["always", "offline", "mentions"], default="mentions")
    p_wl.add_argument("--register", action="store_true", help="Register this listener with aircd")
    p_wl.add_argument("--exec", dest="exec_cmd", help="Shell command run with AIRCD_EVENT JSON")
    p_wl.set_defaults(func=cmd_webhook)

    # Git subcommands
    p_git = sub.add_parser("git", help="Connect local Git repos, hooks, worktrees & PR reports")
    git_sub = p_git.add_subparsers(dest="git_cmd", required=True)
    git_sub.add_parser("list").set_defaults(func=cmd_git)

    p_gs = git_sub.add_parser("status")
    p_gs.add_argument("path", nargs="?", default=".")
    p_gs.set_defaults(func=cmd_git)

    for subname in ("register", "install-hooks", "init-workspace"):
        p_gr = git_sub.add_parser(subname)
        p_gr.add_argument("path", nargs="?", default=".")
        p_gr.add_argument("--repo")
        p_gr.add_argument("--channel")
        if subname != "install-hooks":
            p_gr.add_argument("--branch")
        if subname == "register":
            p_gr.add_argument("--secret")
        p_gr.set_defaults(func=cmd_git)

    p_ge = git_sub.add_parser("emit-hook")
    p_ge.add_argument("--repo", required=True)
    p_ge.add_argument("--channel")
    p_ge.add_argument("--event", default="commit")
    p_ge.add_argument("--path", default=".")
    p_ge.set_defaults(func=cmd_git)

    p_gw = git_sub.add_parser("worktree-setup")
    p_gw.add_argument("path")
    p_gw.add_argument("branch")
    p_gw.add_argument("--base")
    p_gw.add_argument("--worktree-dir")
    p_gw.add_argument("--channel")
    p_gw.add_argument("--lock", action="store_true")
    p_gw.add_argument("--ttl", type=int, default=3600)
    p_gw.set_defaults(func=cmd_git)

    p_gp = git_sub.add_parser("pr-report")
    p_gp.add_argument("path", nargs="?", default=".")
    p_gp.add_argument("--base", default="main")
    p_gp.add_argument("--title")
    p_gp.add_argument("--status", choices=ALLOWED_REPORT_STATUSES, default="completed")
    p_gp.add_argument("--body", default="")
    p_gp.add_argument("--channel")
    p_gp.set_defaults(func=cmd_git)

    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        return args.func(args)
    except CliError as err:
        print(f"error: {err}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
