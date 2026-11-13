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
import uuid
from pathlib import Path
from typing import Any

from aircd.client import REVIEW_NEEDS_REPORT, AircdHttpClient, AircdHttpError, Group, GroupMember, task_create_payload


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
        payload["channel"] = args.channel
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
            if wake.get("session_mode") in ("resume", "pm"):
                lines.append(
                    f"session={wake['session_mode']} (per {'project' if wake['session_mode'] == 'pm' else wake['session_scope']}) marker={wake.get('session_marker')!r} "
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
    rotate = bool(args.rotate)
    label = f"cli-login-{uuid.uuid4().hex[:12]}" if rotate else "cli-login"
    data = http_json(
        server,
        "POST",
        "/v1/login",
        token=token,
        payload={
            "channels": channels,
            "rotate_token": rotate,
            "label": label,
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
            payload=task_create_payload(
                args.channel,
                " ".join(args.title),
                body=args.body,
                kind=args.kind,
                assignee=args.assign,
                repo=args.repo,
                branch=args.branch,
                issue_url=args.issue,
                parent=args.parent,
                reviewer=args.reviewer,
            ),
        )
        t = data["task"]
        owner = f" -> @{t['assignee']}" if t.get("assignee") else ""
        lines = [f"Created {t['kind']} {t['id']} in {t['channel']}{owner}: {t['title']}"]
        if t.get("parent_task_id"):
            lines.append(f"  subtask of {t['parent_task_id']}")
        if t.get("reviewer"):
            lines.append(f"  reviewer @{t['reviewer']}")
        print_out(args, data, lines)
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
        note = review_note(server, token, args) if args.status == "review" else args.note
        payload = {"status": args.status, "note": note, "branch": args.branch}
        report_id = (getattr(args, "report", None) or "").strip()
        if args.status == "review" and report_id:
            # The hub validates the report (assignee-authored, same task) and
            # snapshots it into the task history.
            payload["report_id"] = report_id
        data = http_json(
            server,
            "POST",
            f"/v1/tasks/{urllib.parse.quote(args.id)}/status",
            token=token,
            payload=payload,
        )
        t = data["task"]
        print_out(args, data, [f"Task {t['id']} -> {t['status']} (branch={t.get('branch') or '-'})"])
        return 0
    if args.task_cmd in ("ask", "answer", "escalate"):
        text = " ".join(args.text or []).strip()
        field = {"ask": "question", "answer": "answer", "escalate": "note"}[args.task_cmd]
        data = http_json(
            server,
            "POST",
            f"/v1/tasks/{urllib.parse.quote(args.id)}/{args.task_cmd}",
            token=token,
            payload={field: text or None},
        )
        t = data["task"]
        done = {"ask": "asked; task is", "answer": "answered; task is", "escalate": "escalated to the owner; task is"}
        print_out(args, data, [f"Task {t['id']} {done[args.task_cmd]} {t['status']}"])
        return 0
    if args.task_cmd == "show":
        data = http_json(server, "GET", f"/v1/tasks/{urllib.parse.quote(args.id)}", token=token)
        t = data["task"]
        lines = [
            f"{t['id']} [{t['kind']}/{t['status']}] {t['channel']}: {t['title']}",
            f"assignee={t.get('assignee') or '-'} repo={t.get('repo') or '-'} "
            f"branch={t.get('branch') or '-'} issue={t.get('issue_url') or '-'} sprint={t.get('sprint') or '-'}",
        ]
        if t.get("parent_task_id") or t.get("reviewer"):
            lines.append(f"parent={t.get('parent_task_id') or '-'} reviewer={t.get('reviewer') or '-'}")
        for child in data.get("subtasks") or []:
            lines.append(
                f"  subtask {child['id']} [{child.get('status', '?')}] "
                f"@{child.get('assignee') or '-'}: {child.get('title', '')}"
            )
        if t.get("body"):
            lines.append(t["body"])
        q = data.get("question")
        if q:
            waiting = "the owner" if q["status"] == "escalated" else "the project manager"
            lines.append(f"WAITING on {waiting}: @{q['asked_by']} asks: {q['question']}")
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
    if args.task_cmd == "worktree":
        return _cmd_task_worktree(args, server, token, _prof)
    raise CliError("Unsupported task subcommand")


def _primary_repo_root(path: Path) -> Path:
    """Resolve the primary git repository root (never nested inside another worktree)."""
    top = Path(_run_git(path, ["rev-parse", "--show-toplevel"])).resolve()
    common_raw = _run_git(top, ["rev-parse", "--git-common-dir"])
    common = Path(common_raw)
    if not common.is_absolute():
        common = (top / common).resolve()
    else:
        common = common.resolve()
    if common.name == ".git":
        return common.parent
    return top


def _validate_branch_name(repo_root: Path, branch: str) -> str:
    clean = branch.strip()
    if not clean or clean.startswith(("/", "\\")) or ".." in clean or ":" in clean:
        raise CliError(f"unsafe task branch name: {branch!r}")
    _run_git(repo_root, ["check-ref-format", "--branch", clean])
    return clean


def _parse_worktree_list(repo_root: Path) -> list[dict[str, Any]]:
    raw = _run_git(repo_root, ["worktree", "list", "--porcelain"])
    items: list[dict[str, Any]] = []
    current: dict[str, Any] | None = None
    for line in raw.splitlines():
        line = line.strip()
        if not line:
            if current is not None:
                items.append(current)
                current = None
            continue
        if line.startswith("worktree "):
            if current is not None:
                items.append(current)
            wt_path = str(Path(line[9:].strip()).resolve())
            current = {"path": wt_path, "head": None, "branch": None, "bare": False}
        elif current is not None:
            if line.startswith("HEAD "):
                current["head"] = line[5:].strip()
            elif line.startswith("branch "):
                ref = line[7:].strip()
                current["branch"] = ref[len("refs/heads/"):] if ref.startswith("refs/heads/") else ref
            elif line == "bare":
                current["bare"] = True
    if current is not None:
        items.append(current)
    return items


def _resolve_task_repo(
    server: str, token: str, task: dict[str, Any]
) -> dict[str, Any] | None:
    repos = http_json(server, "GET", "/v1/gits", token=token).get("git_repos", [])
    channel = task.get("channel")
    task_repo = (task.get("repo") or "").strip()
    if task_repo:
        for r in repos:
            if r.get("channel") == channel and r.get("name", "").lower() == task_repo.lower():
                return r
        for r in repos:
            if r.get("name", "").lower() == task_repo.lower():
                return r
        return None
    for r in repos:
        if r.get("channel") == channel:
            return r
    return None


def _ref_exists(repo_root: Path, ref: str) -> bool:
    return (
        subprocess.run(
            ["git", "-C", str(repo_root), "show-ref", "--verify", "--quiet", ref],
            capture_output=True,
        ).returncode
        == 0
    )


def _cmd_task_worktree(
    args: argparse.Namespace, server: str, token: str, prof: dict[str, Any]
) -> int:
    if args.list:
        matched_repo = None
        if args.path:
            seed = Path(args.path).resolve()
        elif args.id:
            task = http_json(server, "GET", f"/v1/tasks/{urllib.parse.quote(args.id)}", token=token)["task"]
            matched_repo = _resolve_task_repo(server, token, task)
            if not matched_repo or not matched_repo.get("local_path"):
                raise CliError(f"no git repo is registered for {task['channel']} and --path was not provided")
            seed = Path(matched_repo["local_path"]).resolve()
        else:
            seed = Path(".").resolve()
        root = _primary_repo_root(seed)
        worktrees = _parse_worktree_list(root)
        lines = [f"{w['path']} [{w.get('branch') or '(detached)'}]" for w in worktrees]
        print_out(args, {"ok": True, "repo_root": str(root), "worktrees": worktrees}, lines or ["No worktrees."])
        return 0

    if not args.id:
        raise CliError("task id is required (or pass --list)")

    task = http_json(server, "GET", f"/v1/tasks/{urllib.parse.quote(args.id)}", token=token)["task"]
    matched_repo = _resolve_task_repo(server, token, task)
    if args.path:
        seed = Path(args.path).resolve()
    elif matched_repo and matched_repo.get("local_path"):
        seed = Path(matched_repo["local_path"]).resolve()
    else:
        try:
            seed = _primary_repo_root(Path(".").resolve())
        except CliError:
            raise CliError(f"no git repo is registered for {task['channel']} and --path was not provided")

    root = _primary_repo_root(seed)
    repo_name = (matched_repo or {}).get("name") or task.get("repo") or root.name
    raw_branch = args.branch or task.get("branch") or f"task/{task['id']}"
    branch = _validate_branch_name(root, raw_branch)
    worktree_dir = (
        Path(args.worktree_dir).resolve()
        if args.worktree_dir
        else (root / ".aircd" / "worktrees").joinpath(*branch.split("/")).resolve()
    )

    existing = _parse_worktree_list(root)
    match_wt = next((w for w in existing if os.path.normcase(w["path"]) == os.path.normcase(str(worktree_dir))), None)

    if args.remove:
        if match_wt is not None or worktree_dir.exists():
            rm_args = ["worktree", "remove"]
            if args.force:
                rm_args.append("--force")
            rm_args.append(str(worktree_dir))
            _run_git(root, rm_args)
        _run_git(root, ["worktree", "prune"])
        if args.delete_branch and _ref_exists(root, f"refs/heads/{branch}"):
            _run_git(root, ["branch", "-D" if args.force else "-d", branch])
        out = {
            "ok": True,
            "removed": True,
            "task_id": task["id"],
            "repo_root": str(root),
            "branch": branch,
            "worktree": str(worktree_dir),
        }
        print_out(args, out, [f"Removed worktree: {worktree_dir} (branch={branch})"])
        return 0

    created = False
    reused = False
    if match_wt is not None:
        if match_wt.get("branch") and match_wt["branch"] != branch:
            raise CliError(f"worktree {worktree_dir} is on branch {match_wt['branch']}, expected {branch}")
        reused = True
    else:
        if worktree_dir.exists() and any(worktree_dir.iterdir()):
            raise CliError(f"path already exists and is not a registered git worktree: {worktree_dir}")
        worktree_dir.parent.mkdir(parents=True, exist_ok=True)
        if _ref_exists(root, f"refs/heads/{branch}"):
            _run_git(root, ["worktree", "add", str(worktree_dir), branch])
        else:
            base_ref = args.base
            if not base_ref:
                default_branch = (matched_repo or {}).get("default_branch")
                if default_branch and _ref_exists(root, f"refs/heads/{default_branch}"):
                    base_ref = default_branch
                elif _ref_exists(root, "refs/heads/main"):
                    base_ref = "main"
                else:
                    base_ref = "HEAD"
            _run_git(root, ["worktree", "add", "-b", branch, str(worktree_dir), base_ref])
        created = True

    lock_res = None
    if args.lock:
        lock_res = http_json(
            server,
            "POST",
            "/v1/locks",
            token=token,
            payload={
                "resource": f"repo:{repo_name}:{branch}",
                "channel": task["channel"],
                "ttl_seconds": args.ttl,
                "note": f"task {task['id']} worktree owned by @{prof.get('nick') or 'agent'}",
            },
        )
    if created:
        try:
            http_json(
                server,
                "POST",
                "/v1/git/event",
                token=token,
                payload={
                    "repo": repo_name,
                    "channel": task["channel"],
                    "event": "worktree",
                    "branch": branch,
                    "summary": f"task {task['id']} worktree at {worktree_dir}",
                },
                timeout=3.0,
            )
        except CliError:
            pass

    out = {
        "ok": True,
        "task_id": task["id"],
        "repo": repo_name,
        "repo_root": str(root),
        "branch": branch,
        "worktree": str(worktree_dir),
        "created": created,
        "reused": reused,
        "lock": lock_res,
    }
    state = "reused" if reused else "created"
    print_out(args, out, [f"Task {task['id']} worktree ({state}): {worktree_dir} (branch={branch})"])
    return 0


def review_note(server: str, token: str, args: argparse.Namespace) -> str:
    """The test report sent when handing a task in for review.

    Either --note, or --report naming a report posted with `airc report create
    --task-id <id>` (its title and summary become the note), or both. Refused
    before reaching the hub when there is neither.
    """
    note = (args.note or "").strip()
    report_id = (getattr(args, "report", None) or "").strip()
    if report_id:
        rep = http_json(server, "GET", f"/v1/reports/{urllib.parse.quote(report_id)}", token=token)["report"]
        if rep.get("task_id") and rep["task_id"] != args.id:
            raise CliError(f"report {report_id} belongs to task {rep['task_id']}, not {args.id}")
        detail = (rep.get("summary") or rep.get("body") or "").strip()
        attached = f"report {rep['id']} [{rep.get('status', '?')}]: {rep.get('title', '')}"
        if detail:
            attached += f" - {detail}"
        note = f"{note}\n{attached}" if note else attached
    if not note:
        raise CliError(REVIEW_NEEDS_REPORT)
    return note


# ── Groups ────────────────────────────────────────────────────


def group_client(args: argparse.Namespace) -> AircdHttpClient:
    """Reads use the account token; create and attach also send the owner key when given."""
    server, token, _prof = resolve_connection(args)
    invite = (getattr(args, "invite", None) or os.environ.get("AIRC_INVITE", "")).strip()
    if not token and not invite:
        raise CliError("airc group needs an account token (airc register/login) or the owner key (--invite or AIRC_INVITE)")
    return AircdHttpClient(server, token=token, invite=invite)


def group_from_args(args: argparse.Namespace) -> Group:
    """--file (JSON) first, then any flags given."""
    data: dict[str, Any] = {}
    if args.file:
        try:
            raw = sys.stdin.read() if args.file == "-" else Path(args.file).read_text(encoding="utf-8")
            loaded = json.loads(raw)
        except (OSError, json.JSONDecodeError) as err:
            raise CliError(f"cannot read group JSON from {args.file}: {err}") from None
        if isinstance(loaded, dict) and isinstance(loaded.get("group"), dict):
            loaded = loaded["group"]
        if not isinstance(loaded, dict):
            raise CliError("group JSON must be an object")
        data.update(loaded)
    data["name"] = args.name
    for key, value in (
        ("description", args.description),
        ("lead_prompt", _text_or_file(args.lead_prompt)),
        ("worker_prompt", _text_or_file(args.worker_prompt)),
    ):
        if value is not None:
            data[key] = value
    try:
        if args.lead:
            data["lead"] = GroupMember.parse(args.lead).to_dict()
        if args.worker:
            data["workers"] = [GroupMember.parse(spec).to_dict() for spec in args.worker]
        if not data.get("lead"):
            raise CliError("group create needs --lead TEMPLATE:MODEL (or a \"lead\" in --file)")
        group = Group.from_dict(data)
    except ValueError as err:
        raise CliError(str(err)) from None
    if group.lead.count != 1:
        raise CliError("a group has exactly one lead; drop the count from --lead")
    if not group.workers:
        raise CliError("group create needs at least one --worker TEMPLATE:MODEL[*COUNT]")
    return group


def _text_or_file(value: str | None) -> str | None:
    """'@path' reads the prompt from a file; anything else is the text itself."""
    if value is None or not value.startswith("@"):
        return value
    try:
        return Path(value[1:]).read_text(encoding="utf-8")
    except OSError as err:
        raise CliError(f"cannot read prompt file {value[1:]}: {err}") from None


def group_lines(g: Group) -> list[str]:
    lines = [
        f"{g.name}: lead {g.lead.label()}, {g.worker_count} worker(s)" + (f"  {g.description}" if g.description else ""),
        "  workers: " + (", ".join(w.label() for w in g.workers) or "-"),
        "  projects: " + (", ".join(g.projects) or "-"),
    ]
    for label, prompt in (("lead prompt", g.lead_prompt), ("worker prompt", g.worker_prompt)):
        if prompt:
            first = prompt.strip().splitlines()[0] if prompt.strip() else ""
            lines.append(f"  {label}: {first}{' ...' if len(prompt.strip().splitlines()) > 1 else ''}")
    return lines


def cmd_group(args: argparse.Namespace) -> int:
    hub = group_client(args)
    action = args.group_cmd
    try:
        if action == "list":
            groups = hub.list_groups()
            data = {"ok": True, "groups": [{**g.to_dict(), "projects": g.projects} for g in groups]}
            lines = [line for g in groups for line in group_lines(g)] or [
                "No groups. Define one with: airc group create <name> --lead TEMPLATE:MODEL --worker TEMPLATE:MODEL*3"
            ]
        elif action == "create":
            group = hub.create_group(group_from_args(args))
            data = {"ok": True, "group": {**group.to_dict(), "projects": group.projects}}
            lines = ["Created group:", *group_lines(group)]
        elif action == "attach":
            group = hub.attach_group(args.name, args.channel)
            data = {"ok": True, "group": {**group.to_dict(), "projects": group.projects}}
            lines = [f"Attached group {group.name} to {args.channel}.", *group_lines(group)]
        else:
            raise CliError(f"Unsupported group subcommand: {action}")
    except AircdHttpError as err:
        raise CliError(str(err)) from None
    print_out(args, data, lines)
    return 0


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
                f"  {t['id']} [{t['kind']}/{t['status']}] sprint={t.get('sprint') or '-'} branch={t.get('branch') or '-'}: {t['title']}"
            )
        for t in proj.get("to_review", []):
            lines.append(
                f"  REVIEW {t['id']} by @{t.get('assignee') or '-'} branch={t.get('branch') or '-'}: {t['title']}"
            )
        for q in proj.get("questions", []):
            lines.append(f"  QUESTION {q['task_id']} from @{q['asked_by']}: {q['question']}")
    for q in inbox.get("needs_you", []):
        note = f" ({q['escalation_note']})" if q.get("escalation_note") else ""
        lines.append(f"NEEDS YOU {q['channel']} {q['task_id']} from @{q['asked_by']}: {q['question']}{note}")
    print_out(args, data, lines)
    return 0


def cmd_room(args: argparse.Namespace) -> int:
    """Sprint rooms: numbered rooms under a project, opened and never closed."""
    server, token, _prof = require_token(args)
    if args.room_cmd == "open":
        data = http_json(
            server,
            "POST",
            "/v1/rooms",
            token=token,
            payload={"project": args.project, "title": " ".join(args.title) or None},
        )
        r = data["room"]
        title = f": {r['title']}" if r.get("title") else ""
        print_out(args, data, [f"Opened sprint {r['label']} of {r['project']}{title} - room {r['channel']}"])
        return 0
    qs = f"?{urllib.parse.urlencode({'channel': args.project})}" if args.project else ""
    data = http_json(server, "GET", f"/v1/rooms{qs}", token=token)
    lines = [
        f"{r['project']} {r['label']}  {r['channel']}  {r.get('title') or ''}".rstrip()
        for r in data.get("rooms", [])
    ]
    print_out(args, data, lines or ["No sprint rooms yet."])
    return 0


def cmd_notes(args: argparse.Namespace) -> int:
    """An account's lasting memory on the hub."""
    server, token, _prof = require_token(args)
    if args.notes_cmd == "show":
        qs = f"?{urllib.parse.urlencode({'nick': args.nick})}" if args.nick else ""
        data = http_json(server, "GET", f"/v1/notes{qs}", token=token)
    else:
        data = http_json(
            server,
            "POST",
            "/v1/notes",
            token=token,
            payload={"body": " ".join(args.text), "append": args.notes_cmd == "add"},
        )
    notes = data["notes"]
    print_out(args, data, [notes["body"] or f"@{notes['principal_id']} has no notes yet."])
    return 0


def cmd_project(args: argparse.Namespace) -> int:
    server, token, _prof = require_token(args)
    if args.project_cmd == "pm":
        data = http_json(
            server,
            "POST",
            "/v1/projects/pm",
            token=token,
            payload={"channel": args.channel, "nick": args.nick},
        )
        p = data["project"]
        print_out(args, data, [f"PM of {p['channel']} is now @{p['pm']}"])
        return 0
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
        lines = [
            f"{p['channel']}: {p['name']} (PM: {'@' + p['pm'] if p.get('pm') else 'none'})"
            for p in data.get("projects", [])
        ]
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


# ── Client templates ──────────────────────────────────────────


def template_client(args: argparse.Namespace, *, owner: bool) -> AircdHttpClient:
    """Reads need an account token; changes and model listing need the owner key.
    spawn takes the owner key when given, else the profile's token."""
    if args.template_cmd == "spawn":
        server, token, _prof = resolve_connection(args)
        invite = (getattr(args, "invite", None) or os.environ.get("AIRC_INVITE", "")).strip()
        if not invite and not token:
            raise CliError("template spawn needs a manager's token (airc register/login) or the owner key (--invite or AIRC_INVITE)")
        return AircdHttpClient(server, token=token, invite=invite)
    if owner:
        server, token, _prof = resolve_connection(args)
        invite = (getattr(args, "invite", None) or os.environ.get("AIRC_INVITE", "")).strip()
        if not invite:
            raise CliError(f"template {args.template_cmd} needs the owner key: pass --invite or set AIRC_INVITE")
        return AircdHttpClient(server, token=token, invite=invite)
    server, token, _prof = require_token(args)
    return AircdHttpClient(server, token=token)


def template_payload(args: argparse.Namespace) -> dict[str, Any]:
    """The full template to save: --file (JSON) first, then any flags given."""
    template: dict[str, Any] = {}
    if args.file:
        try:
            raw = sys.stdin.read() if args.file == "-" else Path(args.file).read_text(encoding="utf-8")
            loaded = json.loads(raw)
        except (OSError, json.JSONDecodeError) as err:
            raise CliError(f"cannot read template JSON from {args.file}: {err}") from None
        # Accept the output of `airc --json template show` as well as a bare template.
        if isinstance(loaded, dict) and isinstance(loaded.get("template"), dict):
            loaded = loaded["template"]
        if not isinstance(loaded, dict):
            raise CliError("template JSON must be an object")
        template.update(loaded)
    flags = {
        "name": args.name,
        "command": args.command,
        "cwd": args.cwd,
        "prompt": args.prompt,
        "session_mode": args.session,
        "session_marker": args.session_marker,
        "session_scope": args.session_scope,
        "args": args.arg,
        "resume_args": args.resume_arg,
        "models": args.model,
    }
    template.update({k: v for k, v in flags.items() if v is not None})
    if args.models_command is not None:
        command_line = args.models_command
        if os.name == "nt":
            # Keep the backslashes in Windows paths such as C:\Tools\agy.exe.
            command_line = command_line.replace("\\", "\\\\")
        template["models_command"] = shlex.split(command_line)
    if args.env or args.unset_env:
        env = dict(template.get("env") or {})
        for item in args.env or []:
            key, sep, value = item.partition("=")
            env[key] = value if sep else None
        for key in args.unset_env or []:
            env[key] = None
        template["env"] = env
    if not str(template.get("command") or "").strip():
        raise CliError('template set needs --command (or a "command" in --file)')
    if not str(template.get("name") or "").strip():
        template["name"] = args.id
    return template


def template_lines(t: dict[str, Any]) -> list[str]:
    state = "installed" if t.get("installed") else "not installed"
    lines = [
        f"{t['id']} ({t.get('name') or t['id']}): {state}",
        f"command: {' '.join(shlex.quote(p) for p in [t['command'], *(t.get('args') or [])])}",
        f"cwd={t.get('cwd') or '{repo_path}'} session={t.get('session_mode')} (per {t.get('session_scope')})",
    ]
    if t.get("resume_args"):
        lines.append(f"resume args: {' '.join(t['resume_args'])} marker={t.get('session_marker')!r}")
    if t.get("env"):
        lines.append(
            "env: " + " ".join(f"{k}={v}" if v is not None else f"-{k}" for k, v in sorted(t["env"].items()))
        )
    if t.get("prompt"):
        lines.append(f"prompt: {t['prompt']}")
    lines.append(f"models: {', '.join(t.get('models') or []) or '-'}")
    if t.get("models_command"):
        lines.append(f"models command: {' '.join(shlex.quote(p) for p in t['models_command'])}")
    return lines


def cmd_template(args: argparse.Namespace) -> int:
    action = args.template_cmd
    hub = template_client(args, owner=action in ("set", "delete", "models"))
    data: dict[str, Any]
    try:
        if action == "list":
            templates = hub.list_templates()
            data = {"ok": True, "templates": templates}
            lines = [
                f"{t['id']:<16} {'installed' if t.get('installed') else 'missing':<10} "
                f"{t['command']:<20} models={len(t.get('models') or [])}"
                f"{'+command' if t.get('models_command') else ''}  {t.get('name') or ''}"
                for t in templates
            ] or ["No client templates. Add one with: airc template set <id> --command <program> --invite <owner key>"]
        elif action == "show":
            template = hub.get_template(args.id)
            data, lines = {"ok": True, "template": template}, template_lines(template)
        elif action == "set":
            template = hub.set_template(args.id, template_payload(args))
            data, lines = {"ok": True, "template": template}, ["Saved template:", *template_lines(template)]
        elif action == "delete":
            data = {"ok": True, "deleted": hub.delete_template(args.id)}
            lines = [f"Deleted template {args.id}."]
        elif action == "models":
            data = hub.list_template_models(args.id)
            lines = [m["id"] + (f"  {m['label']}" if m.get("label") else "") for m in data.get("models", [])]
            if data.get("warning"):
                print(f"warning: models command failed: {data['warning']}", file=sys.stderr)
        elif action == "spawn":
            data = hub.spawn_worker(
                args.id, args.nick, args.model, channels=args.channel, role=args.role
            )
            account = data.get("account") or {}
            principal = account.get("principal") or {}
            lines = [
                f"Spawned {principal.get('nick', args.nick)} from template {account.get('template', args.id)} "
                f"(model {account.get('model', args.model)}, role {account.get('role', '')})",
                f"  channels: {', '.join(account.get('channels') or [])}",
                f"  token:    {account.get('token', '')}  (shown once; save it now)",
                f"  save as profile: airc --server {data.get('http_url', '')} --token {account.get('token', '')} "
                f"--profile {principal.get('nick', args.nick)} login --channels {account.get('channel', '#general')}",
            ]
        else:
            raise CliError(f"Unsupported template subcommand: {action}")
    except AircdHttpError as err:
        raise CliError(str(err)) from None
    print_out(args, data, lines)
    return 0


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
    p_wset.add_argument("--max-per-hour", type=int, help="Launch limit per hour (default 30, 0 = unlimited)")
    p_wset.add_argument(
        "--session",
        choices=["fresh", "resume", "pm"],
        default="fresh",
        help="fresh: new conversation per launch. resume: always continue one. pm: continue a per-project conversation only when woken as that project's manager",
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
            wake_sub_parser.add_argument("--channel", help="Launch it in this project channel")
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

    p_tpl = sub.add_parser(
        "template", help="Client templates: how a kind of agent (claude, codex, ...) is launched"
    )
    tpl_sub = p_tpl.add_subparsers(dest="template_cmd", required=True)
    tpl_sub.add_parser(
        "list", help="List templates and whether each command is installed on the hub"
    ).set_defaults(func=cmd_template)
    p_tshow = tpl_sub.add_parser("show", help="Show one template")
    p_tshow.add_argument("id")
    p_tshow.set_defaults(func=cmd_template)
    p_tset = tpl_sub.add_parser(
        "set",
        help="Create or replace a template (owner). Placeholders include {model} {prompt} {nick} {channel}",
        description="Saves the whole template; fields not given take their defaults. "
        "Start from JSON with --file (or --file - for stdin, e.g. the output of 'airc --json template show ID'), "
        "then any flags given override it.",
    )
    p_tset.add_argument("id")
    p_tset.add_argument("--file", help="Template JSON to start from ('-' reads stdin)")
    p_tset.add_argument("--name", help="Display name (default: the id)")
    p_tset.add_argument("--command", help="Executable (full path, or a name on the hub's PATH)")
    p_tset.add_argument("--arg", action="append", help="One argument template; repeat for each. Use --arg=-p for dash values")
    p_tset.add_argument("--cwd", help="Working directory template (default: the task's repo path)")
    p_tset.add_argument("--env", action="append", metavar="KEY=VALUE")
    p_tset.add_argument("--unset-env", action="append", metavar="KEY", help="Remove a variable from the agent's environment")
    p_tset.add_argument("--prompt", help="Prompt template for {prompt}")
    p_tset.add_argument("--session", choices=["fresh", "resume", "pm"], help="Conversation mode (default fresh)")
    p_tset.add_argument("--resume-arg", action="append", help="Argument template used once a conversation is stored; {session_id} is its id")
    p_tset.add_argument("--session-marker", help="Text that precedes the conversation id in the agent's output")
    p_tset.add_argument("--session-scope", choices=["account", "channel"], help="One conversation per account or per project channel")
    p_tset.add_argument("--model", action="append", help="A model this template offers; repeat for each")
    p_tset.add_argument(
        "--models-command",
        help="Command, program first, that prints the models, e.g. \"agy models\" (split like a shell; \"\" clears it)",
    )
    p_tdel = tpl_sub.add_parser("delete", help="Delete a template (owner)")
    p_tdel.add_argument("id")
    p_tmod = tpl_sub.add_parser(
        "models", help="List a template's models; runs its models command on the hub (owner)"
    )
    p_tmod.add_argument("id")
    p_tspawn = tpl_sub.add_parser(
        "spawn",
        help="Provision a worker account from a template (owner, or a manager of every channel's project)",
        description="Creates an account that wakes with the template's command and the given model. "
        "Uses the owner key when given, else this profile's token. The new token is printed once.",
    )
    p_tspawn.add_argument("id", help="Template id")
    p_tspawn.add_argument("nick", help="Nick for the new account")
    p_tspawn.add_argument("--model", required=True, help="Model to run, e.g. one from 'airc template models ID'")
    p_tspawn.add_argument("--channel", action="append", help="Channel to join; repeat for each (default: chat only)")
    p_tspawn.add_argument("--role", help="Project role (default member)")
    p_tspawn.add_argument("--invite", help="Owner key (or set AIRC_INVITE); omit to use this profile's token")
    p_tspawn.set_defaults(func=cmd_template)
    for owner_parser in (p_tset, p_tdel, p_tmod):
        owner_parser.add_argument("--invite", help="Owner key (or set AIRC_INVITE)")
        owner_parser.set_defaults(func=cmd_template)

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
    p_tc.add_argument("--parent", metavar="TASK_ID", help="Make this a subtask of TASK_ID (a group lead splitting work)")
    p_tc.add_argument("--reviewer", metavar="NICK", help="Who reviews it (default: the parent's assignee, else the PM)")
    p_tc.set_defaults(func=cmd_task)

    p_tas = task_sub.add_parser("assign", help="Assign or reassign a task; omit NICK to return it to the pool")
    p_tas.add_argument("id")
    p_tas.add_argument("nick", nargs="?")
    p_tas.set_defaults(func=cmd_task)

    p_tst = task_sub.add_parser("status", help="Move a task: in_progress, testing, review (needs --note with your test report), changes_requested, merged, cancelled")
    p_tst.add_argument("id")
    p_tst.add_argument("status")
    p_tst.add_argument("--note", help="Your test report: what you ran and the result (needed for review)")
    p_tst.add_argument("--report", metavar="REPORT_ID", help="Attach a report from 'airc report create --task-id ID' as the test report")
    p_tst.add_argument("--branch")
    p_tst.set_defaults(func=cmd_task)

    for name, text in (
        ("ask", "Assignee: park the task and ask the project manager a question"),
        ("answer", "PM or owner: answer the pending question; the task resumes"),
        ("escalate", "PM: pass the pending question up to the owner"),
    ):
        p_tq = task_sub.add_parser(name, help=text)
        p_tq.add_argument("id")
        p_tq.add_argument("text", nargs="*")
        p_tq.set_defaults(func=cmd_task)

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

    p_tw = task_sub.add_parser(
        "worktree",
        help="Provision or remove an isolated git worktree at .aircd/worktrees/<branch> for a task",
    )
    p_tw.add_argument("id", nargs="?", help="Task id (required unless --list is used)")
    p_tw.add_argument("--branch", help="Branch name (defaults to task.branch or task/<id>)")
    p_tw.add_argument("--base", help="Base ref when creating a new branch")
    p_tw.add_argument("--path", help="Local git repository path override")
    p_tw.add_argument("--worktree-dir", help="Explicit worktree directory override")
    p_tw.add_argument("--lock", action="store_true", help="Acquire a branch lock on the hub")
    p_tw.add_argument("--ttl", type=int, default=3600)
    p_tw.add_argument("--remove", action="store_true", help="Remove the task worktree")
    p_tw.add_argument("--force", action="store_true", help="Force removal or branch deletion")
    p_tw.add_argument("--delete-branch", action="store_true", help="Delete local branch with --remove")
    p_tw.add_argument("--list", action="store_true", help="List git worktrees")
    p_tw.set_defaults(func=cmd_task)

    p_group = sub.add_parser("group", help="Reusable teams: a lead that splits and reviews work, plus workers")
    group_sub = p_group.add_subparsers(dest="group_cmd", required=True)
    p_gl = group_sub.add_parser("list", help="List groups and the projects they serve")
    p_gc = group_sub.add_parser("create", help="Define a group once; attach it to projects later")
    p_gc.add_argument("name")
    p_gc.add_argument("--lead", metavar="TEMPLATE:MODEL", help="The lead's client template and model, e.g. claude:opus")
    p_gc.add_argument(
        "--worker", action="append", metavar="TEMPLATE:MODEL[*N]",
        help="Workers of one kind, e.g. ollama:gemma3*3; repeat for each kind",
    )
    p_gc.add_argument("--description")
    p_gc.add_argument("--lead-prompt", help="Lead instructions (split, hand out, test, review, submit); @file reads a file")
    p_gc.add_argument("--worker-prompt", help="Worker instructions (code only, no project context); @file reads a file")
    p_gc.add_argument("--file", help="Group JSON to start from ('-' reads stdin)")
    p_ga = group_sub.add_parser("attach", help="Attach a group to a project channel")
    p_ga.add_argument("name")
    p_ga.add_argument("channel")
    for group_parser in (p_gl, p_gc, p_ga):
        group_parser.add_argument("--invite", help="Owner key (or set AIRC_INVITE); omit to use this profile's token")
        group_parser.set_defaults(func=cmd_group)

    p_room = sub.add_parser("room",help="Sprint rooms under a project (opened, never closed)")
    room_sub = p_room.add_subparsers(dest="room_cmd", required=True)
    p_ro = room_sub.add_parser("open", help="Open the project's next sprint room; new tasks go there")
    p_ro.add_argument("project")
    p_ro.add_argument("title", nargs="*")
    p_ro.set_defaults(func=cmd_room)
    p_rl = room_sub.add_parser("list")
    p_rl.add_argument("project", nargs="?")
    p_rl.set_defaults(func=cmd_room)

    p_notes = sub.add_parser("notes", help="Your account's lasting notes on the hub")
    notes_sub = p_notes.add_subparsers(dest="notes_cmd", required=True)
    p_ns = notes_sub.add_parser("show")
    p_ns.add_argument("--nick", help="Owner only: read another account's notes")
    p_ns.set_defaults(func=cmd_notes)
    p_nset = notes_sub.add_parser("set", help="Replace your notes")
    p_nset.add_argument("text", nargs="+")
    p_nset.set_defaults(func=cmd_notes)
    p_nadd = notes_sub.add_parser("add", help="Add a line to your notes")
    p_nadd.add_argument("text", nargs="+")
    p_nadd.set_defaults(func=cmd_notes)

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
    p_ppm = proj_sub.add_parser("pm", help="Owner: choose the project manager of a project")
    p_ppm.add_argument("channel")
    p_ppm.add_argument("nick")
    p_ppm.set_defaults(func=cmd_project)
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
