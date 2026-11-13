"""Async IRC client for aircd server.

Handles connection, authentication, channel operations, message history
replay (bouncer), and task primitives. Designed for agent use cases where
reliability and automatic reconnection matter more than full IRC compliance.
"""

from __future__ import annotations

import asyncio
import json
import logging
import ssl
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from typing import Any, Callable, Optional

logger = logging.getLogger("aircd")


@dataclass
class Message:
    """A message received from the server."""

    seq: Optional[int]  # sequence number (if from history/durable log)
    channel: str
    sender: str
    content: str
    raw: str  # original IRC line
    tags: dict[str, str] = field(default_factory=dict)  # IRCv3 message tags
    is_replay: bool = False  # True if this message came from CHATHISTORY replay


@dataclass
class Task:
    """A task from the TASK system."""

    id: str
    channel: str
    title: str
    status: str  # open, claimed, done
    claimed_by: Optional[str] = None


@dataclass
class GroupMember:
    """One seat in a group: the client template and model an account runs.

    ``count`` is how many accounts of this kind the group has (workers only;
    the lead is always one). ``nick`` names the account once it exists.
    """

    template: str
    model: str
    count: int = 1
    nick: Optional[str] = None

    @classmethod
    def parse(cls, spec: str) -> "GroupMember":
        """Parse ``TEMPLATE:MODEL`` with an optional ``*COUNT`` suffix, e.g.
        ``claude:opus`` or ``ollama:gemma3*3``. The model may itself contain
        colons (``ollama:qwen2.5-coder:7b``).
        """
        spec = spec.strip()
        template, sep, model = spec.partition(":")
        template, model = template.strip(), model.strip()
        count = 1
        head, star, tail = model.rpartition("*")
        if star:
            if not tail.strip().isdigit():
                raise ValueError(f"group member count after '*' must be a number, got {spec!r}")
            model, count = head.strip(), int(tail)
        if not sep or not template or not model:
            raise ValueError(f"group member must look like TEMPLATE:MODEL[*COUNT], got {spec!r}")
        if count < 1:
            raise ValueError(f"group member count must be at least 1, got {spec!r}")
        return cls(template=template, model=model, count=count)

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> "GroupMember":
        return cls(
            template=str(data.get("template") or ""),
            model=str(data.get("model") or ""),
            count=int(data.get("count") or 1),
            nick=data.get("nick") or None,
        )

    def to_dict(self) -> dict[str, Any]:
        out: dict[str, Any] = {"template": self.template, "model": self.model, "count": self.count}
        if self.nick:
            out["nick"] = self.nick
        return out

    def label(self) -> str:
        text = f"{self.template}:{self.model}"
        if self.count != 1:
            text += f" x{self.count}"
        if self.nick:
            text += f" (@{self.nick})"
        return text


@dataclass
class Group:
    """A reusable team: one lead that splits and reviews work, plus workers.

    A group is defined once and attached to any number of project channels.
    ``lead_prompt`` and ``worker_prompt`` are part of the definition: workers
    get no project context and only code; the lead splits, hands out, tests,
    reviews and submits.
    """

    name: str
    lead: GroupMember
    workers: list[GroupMember] = field(default_factory=list)
    description: str = ""
    lead_prompt: Optional[str] = None
    worker_prompt: Optional[str] = None
    projects: list[str] = field(default_factory=list)

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> "Group":
        def member(raw: Any) -> GroupMember:
            return GroupMember.from_dict(raw) if isinstance(raw, dict) else GroupMember.parse(str(raw))

        return cls(
            name=str(data.get("name") or ""),
            lead=member(data.get("lead") or {}),
            workers=[member(w) for w in data.get("workers") or []],
            description=str(data.get("description") or ""),
            lead_prompt=data.get("lead_prompt") or None,
            worker_prompt=data.get("worker_prompt") or None,
            projects=list(data.get("projects") or data.get("channels") or []),
        )

    def to_dict(self) -> dict[str, Any]:
        """The create payload (``projects`` is managed by attach, so it is left out)."""
        out: dict[str, Any] = {
            "name": self.name,
            "lead": self.lead.to_dict(),
            "workers": [w.to_dict() for w in self.workers],
        }
        if self.description:
            out["description"] = self.description
        if self.lead_prompt:
            out["lead_prompt"] = self.lead_prompt
        if self.worker_prompt:
            out["worker_prompt"] = self.worker_prompt
        return out

    @property
    def worker_count(self) -> int:
        return sum(w.count for w in self.workers)


@dataclass
class _ConnectionState:
    reader: Optional[asyncio.StreamReader] = None
    writer: Optional[asyncio.StreamWriter] = None
    registered: bool = False
    nick: Optional[str] = None
    channels: set[str] = field(default_factory=set)
    last_seen_seq: dict[str, int] = field(default_factory=dict)  # channel -> seq


class AircdClient:
    """Async client for aircd server.

    Usage::

        client = AircdClient("localhost", 6667, token="agent-token-1", nick="agent-1")
        await client.connect()
        await client.join("#work")
        await client.privmsg("#work", "hello from agent")

        # Listen for messages
        async for msg in client.messages():
            print(f"{msg.sender}: {msg.content}")

    For agent use cases with auto-reconnect::

        client = AircdClient(..., auto_reconnect=True)
        await client.connect()
        # If connection drops, client reconnects and replays missed messages
    """

    def __init__(
        self,
        host: str,
        port: int,
        token: str,
        nick: str,
        *,
        tls: bool = False,
        tls_verify: bool = True,
        tls_ca_path: Optional[str] = None,
        auto_reconnect: bool = True,
        reconnect_delay: float = 2.0,
        max_reconnect_delay: float = 60.0,
    ):
        self.host = host
        self.port = port
        self.token = token
        self.nick = nick
        self.tls = tls
        self.tls_verify = tls_verify
        self.tls_ca_path = tls_ca_path
        self.auto_reconnect = auto_reconnect
        self.reconnect_delay = reconnect_delay
        self.max_reconnect_delay = max_reconnect_delay

        self._state = _ConnectionState()
        self._message_queue: asyncio.Queue[Message] = asyncio.Queue()
        self._raw_handlers: list[Callable[[str], None]] = []
        self._read_task: Optional[asyncio.Task] = None
        self._closed = False

    # ── Connection ──────────────────────────────────────────────

    async def connect(self) -> None:
        """Connect to the server, authenticate, and register."""
        self._closed = False
        await self._do_connect()

    async def _do_connect(self) -> None:
        ssl_ctx = None
        if self.tls:
            ssl_ctx = ssl.create_default_context(cafile=self.tls_ca_path)
            if not self.tls_verify:
                ssl_ctx.check_hostname = False
                ssl_ctx.verify_mode = ssl.CERT_NONE
        reader, writer = await asyncio.open_connection(
            self.host, self.port, ssl=ssl_ctx,
        )
        self._state.reader = reader
        self._state.writer = writer
        self._state.registered = False

        # Negotiate IRCv3-style metadata tags. The server is agent-first and
        # sends tags regardless for MVP compatibility, but CAP keeps the wire
        # contract explicit for IRCv3-aware clients.
        await self._send("CAP LS 302")
        await self._send("CAP REQ :message-tags")

        # Authenticate: PASS -> NICK -> USER
        await self._send(f"PASS {self.token}")
        await self._send(f"NICK {self.nick}")
        await self._send(f"USER {self.nick} 0 * :aircd agent")
        await self._send("CAP END")

        # Wait for registration confirmation (001 RPL_WELCOME or ERR)
        while not self._state.registered:
            line = await self._readline()
            if line is None:
                raise ConnectionError("Connection closed during registration")
            _prefix, command, params, _tags = _parse_irc_line(line)
            if command == "001":
                self._state.registered = True
                # Server may override our nick
                if params:
                    self._state.nick = params[0]
            elif command == "PING":
                await self._send(f"PONG :{params[-1] if params else ''}")
            elif command in ("432", "433", "461", "462", "464"):
                raise ConnectionError(f"Registration failed: {line}")

        # Rejoin channels that aren't yet in durable membership.
        # Server auto-replays missed messages on reconnect (bouncer),
        # so we only need to JOIN — no manual CHATHISTORY needed.
        for channel in list(self._state.channels):
            await self._send(f"JOIN {channel}")

        # Start background reader
        self._read_task = asyncio.create_task(self._reader_loop())

    async def close(self) -> None:
        """Disconnect from the server."""
        self._closed = True
        if self._read_task:
            self._read_task.cancel()
            try:
                await self._read_task
            except asyncio.CancelledError:
                pass
        if self._state.writer:
            try:
                await self._send("QUIT :bye")
                self._state.writer.close()
                await self._state.writer.wait_closed()
            except Exception:
                pass
        self._state.reader = None
        self._state.writer = None

    # ── Channel operations ──────────────────────────────────────

    async def join(self, channel: str) -> None:
        """Join a channel."""
        self._state.channels.add(channel)
        await self._send(f"JOIN {channel}")

    async def part(self, channel: str) -> None:
        """Leave a channel."""
        self._state.channels.discard(channel)
        self._state.last_seen_seq.pop(channel, None)
        await self._send(f"PART {channel}")

    # ── Messaging ───────────────────────────────────────────────

    async def privmsg(self, target: str, text: str) -> None:
        """Send a message to a channel or user."""
        await self._send(f"PRIVMSG {target} :{text}")

    async def history(self, channel: str, after_seq: int = 0, limit: int = 100) -> None:
        """Request message history for a channel. Results arrive via messages()."""
        await self._send(f"CHATHISTORY AFTER {channel} {after_seq} {limit}")

    async def messages(self):
        """Async generator yielding Message objects as they arrive.

        This is the primary way to consume messages::

            async for msg in client.messages():
                print(f"[{msg.channel}] {msg.sender}: {msg.content}")
        """
        while not self._closed:
            try:
                msg = await asyncio.wait_for(self._message_queue.get(), timeout=1.0)
                yield msg
            except asyncio.TimeoutError:
                continue

    # ── Task primitives ─────────────────────────────────────────

    async def task_create(self, channel: str, title: str) -> None:
        """Create a new task in a channel."""
        await self._send(f"TASK CREATE {channel} :{title}")

    async def task_claim(self, task_id: str) -> None:
        """Atomically claim a task. Server ensures only one agent succeeds."""
        await self._send(f"TASK CLAIM {task_id}")

    async def task_done(self, task_id: str) -> None:
        """Mark a claimed task as done."""
        await self._send(f"TASK DONE {task_id}")

    async def task_release(self, task_id: str) -> None:
        """Release a claimed task back to open."""
        await self._send(f"TASK RELEASE {task_id}")

    async def task_list(self, channel: str) -> None:
        """List tasks in a channel. Results arrive as server notices."""
        await self._send(f"TASK LIST {channel}")

    # ── Roles, directory & equipment locks ──────────────────────

    async def names(self, channel: str | None = None) -> None:
        """Request channel member roster (with @/+ role prefixes)."""
        await self._send(f"NAMES {channel}" if channel else "NAMES")

    async def who(self, target: str | None = None) -> None:
        """Query online status and roles."""
        await self._send(f"WHO {target}" if target else "WHO")

    async def role_set(self, nick: str, role: str) -> None:
        """Assign an IRC role to an agent nick."""
        await self._send(f"ROLE {nick} {role}")

    async def role_list(self) -> None:
        """List all principals and their assigned roles."""
        await self._send("ROLES")

    async def lock_acquire(
        self,
        resource: str,
        channel: str | None = None,
        ttl_seconds: int | None = None,
        note: str | None = None,
    ) -> None:
        """Acquire an exclusive equipment or branch lease."""
        parts = ["LOCK", resource]
        if channel:
            parts.append(channel)
        if ttl_seconds is not None:
            parts.append(str(ttl_seconds))
        cmd = " ".join(parts)
        if note:
            cmd = f"{cmd} :{note}"
        await self._send(cmd)

    async def lock_release(self, resource: str) -> None:
        """Release an equipment or branch lease."""
        await self._send(f"UNLOCK {resource}")

    async def lock_list(self, channel: str | None = None) -> None:
        """List active equipment and branch locks."""
        await self._send(f"LOCKS {channel}" if channel else "LOCKS")

    # ── Internal ────────────────────────────────────────────────

    async def _send(self, line: str) -> None:
        if self._state.writer is None:
            raise ConnectionError("Not connected")
        self._state.writer.write((line + "\r\n").encode("utf-8"))
        await self._state.writer.drain()
        logger.debug(">> %s", line)

    async def _readline(self) -> Optional[str]:
        if self._state.reader is None:
            return None
        try:
            data = await self._state.reader.readline()
            if not data:
                return None
            line = data.decode("utf-8", errors="replace").rstrip("\r\n")
            logger.debug("<< %s", line)
            return line
        except (ConnectionError, asyncio.IncompleteReadError):
            return None

    async def _reader_loop(self) -> None:
        """Background loop: read lines, dispatch PING/PONG, queue messages."""
        delay = self.reconnect_delay
        while not self._closed:
            try:
                line = await self._readline()
                if line is None:
                    raise ConnectionError("Connection lost")

                prefix, command, params, tags = _parse_irc_line(line)

                if command == "PING":
                    await self._send(f"PONG :{params[-1] if params else ''}")
                elif command in ("PRIVMSG", "NOTICE") and len(params) >= 2:
                    sender_nick = _extract_nick(prefix)
                    # Extract seq from message tags (e.g. @seq=42)
                    seq = None
                    if "seq" in tags:
                        try:
                            seq = int(tags["seq"])
                        except ValueError:
                            pass
                    is_replay = tags.get("replay") == "1" or "batch" in tags
                    msg = Message(
                        seq=seq,
                        channel=params[0],
                        sender=sender_nick,
                        content=params[1],
                        raw=line,
                        tags=tags,
                        is_replay=is_replay,
                    )
                    # Track last seen seq for bouncer replay
                    if msg.seq is not None and msg.channel.startswith("#"):
                        self._state.last_seen_seq[msg.channel] = max(
                            self._state.last_seen_seq.get(msg.channel, 0),
                            msg.seq,
                        )
                    self._message_queue.put_nowait(msg)

                # Notify raw handlers
                for handler in self._raw_handlers:
                    handler(line)

                # Reset reconnect delay on successful read
                delay = self.reconnect_delay

            except (ConnectionError, OSError) as e:
                if self._closed:
                    return
                if not self.auto_reconnect:
                    logger.error("Connection lost: %s", e)
                    return

                logger.warning("Connection lost, reconnecting in %.1fs: %s", delay, e)
                await asyncio.sleep(delay)
                delay = min(delay * 2, self.max_reconnect_delay)
                try:
                    await self._do_connect()
                    return  # _do_connect starts a new reader loop
                except Exception as re:
                    logger.error("Reconnect failed: %s", re)
                    continue

            except asyncio.CancelledError:
                return


# ── HTTP API: client templates ──────────────────────────────────


class AircdHttpError(Exception):
    """An HTTP API call failed. ``status`` is None when the hub was unreachable."""

    def __init__(self, message: str, status: Optional[int] = None):
        super().__init__(message)
        self.status = status


class AircdHttpClient:
    """Blocking client for the hub's HTTP API (stdlib only).

    Reading templates needs an account token; changing them, deleting them and
    listing a template's models need the owner key. Spawning a worker takes
    either the owner key or a project manager's token::

        hub = AircdHttpClient("http://127.0.0.1:6680", token="...", invite="owner-key")
        for t in hub.list_templates():
            print(t["id"], t["installed"])
        hub.set_template("claude", {"name": "Claude Code", "command": "claude",
                                    "args": ["-p", "{prompt}", "--model", "{model}"]})
        models = hub.list_template_models("claude")["models"]
        worker = hub.spawn_worker("claude", "builder1", "opus", channels=["#proj"])
    """

    def __init__(
        self,
        base_url: str,
        *,
        token: str = "",
        invite: str = "",
        timeout: float = 15.0,
    ):
        base_url = base_url.strip().rstrip("/")
        if not base_url.startswith(("http://", "https://")):
            base_url = f"http://{base_url}"
        self.base_url = base_url
        self.token = token
        self.invite = invite
        self.timeout = timeout

    # Templates

    def list_templates(self) -> list[dict[str, Any]]:
        """All templates, sorted by id, each with a live ``installed`` flag."""
        return self._request("GET", "/v1/templates").get("templates", [])

    def get_template(self, template_id: str) -> dict[str, Any]:
        """One template; raises AircdHttpError(status=404) when there is none."""
        return self._request("GET", _template_path(template_id))["template"]

    def set_template(self, template_id: str, template: dict[str, Any]) -> dict[str, Any]:
        """Create or fully replace a template (owner key). Returns the saved template.

        Fields: name, command (required), args, cwd, env (None unsets a
        variable), prompt, session_mode, resume_args, session_marker,
        session_scope, models, models_command. Omitted fields take defaults.
        """
        payload = {k: v for k, v in template.items() if k not in ("id", "installed", "created_at", "updated_at")}
        return self._request("POST", _template_path(template_id), payload=payload, owner=True)["template"]

    def delete_template(self, template_id: str) -> bool:
        """Delete a template (owner key); raises AircdHttpError(status=404) when there is none."""
        return bool(self._request("DELETE", _template_path(template_id), owner=True).get("deleted"))

    def list_template_models(self, template_id: str) -> dict[str, Any]:
        """A template's models (owner key): its saved list, then what its listing
        command prints on the hub. Returns ``{"template", "models": [{"id", "label"}],
        "warning"}``; ``warning`` is set when the listing command failed.
        """
        return self._request(
            "POST", "/v1/owner/models", payload={"template": template_id}, owner=True
        )

    def spawn_worker(
        self,
        template_id: str,
        nick: str,
        model: str,
        *,
        channels: Optional[list[str]] = None,
        role: Optional[str] = None,
    ) -> dict[str, Any]:
        """Provision a worker account from a template. Returns the response:
        ``{"ok", "account": {"principal", "token", "channel", "channels", "role",
        "model", "template", "wake"}, "http_url"}``. The token is shown only once.

        Sends the owner key when one is set; otherwise the account token, which
        must belong to the owner or a manager of every requested project.
        ``channels`` defaults to the hub's chat-only channel; ``role`` is the
        project role (default member).
        """
        payload: dict[str, Any] = {"nick": nick, "model": model}
        if channels:
            payload["channels"] = list(channels)
        if role:
            payload["role"] = role
        return self._request(
            "POST", _template_path(template_id) + "/spawn", payload=payload, owner=bool(self.invite)
        )

    # Groups

    def list_groups(self) -> list[Group]:
        """All groups on the hub, with the projects each is attached to."""
        return [Group.from_dict(g) for g in self._request("GET", "/v1/groups").get("groups", [])]

    def create_group(self, group: Group) -> Group:
        """Define a reusable group (lead, workers and role prompts). Returns it as saved.

        Sends the owner key when one is set; otherwise the account token.
        """
        if not group.name.strip():
            raise AircdHttpError("group name must not be empty")
        data = self._request("POST", "/v1/groups", payload=group.to_dict(), owner=bool(self.invite))
        return Group.from_dict(data["group"])

    def attach_group(self, name: str, channel: str) -> Group:
        """Attach a group to a project channel; one group can serve many projects."""
        channel = channel.strip()
        if not channel.startswith("#"):
            raise AircdHttpError(f"project channel must start with '#', got {channel!r}")
        data = self._request(
            "POST", _group_path(name) + "/attach", payload={"channel": channel}, owner=bool(self.invite)
        )
        return Group.from_dict(data["group"])

    # Tasks

    def create_task(
        self,
        channel: str,
        title: str,
        *,
        body: Optional[str] = None,
        kind: str = "task",
        assignee: Optional[str] = None,
        repo: Optional[str] = None,
        branch: Optional[str] = None,
        issue_url: Optional[str] = None,
        parent: Optional[str] = None,
        reviewer: Optional[str] = None,
    ) -> dict[str, Any]:
        """Create a task and return it. ``parent`` makes it a subtask of that task
        id; ``reviewer`` names who reviews it (the hub defaults to the parent's
        assignee for subtasks, otherwise the project manager)."""
        payload = task_create_payload(
            channel, title, body=body, kind=kind, assignee=assignee, repo=repo,
            branch=branch, issue_url=issue_url, parent=parent, reviewer=reviewer,
        )
        return self._request("POST", "/v1/tasks", payload=payload)["task"]

    def set_task_status(
        self,
        task_id: str,
        status: str,
        *,
        note: Optional[str] = None,
        branch: Optional[str] = None,
    ) -> dict[str, Any]:
        """Move a task and return it. Moving to ``review`` needs a test report as ``note``."""
        if status == "review" and not (note or "").strip():
            raise AircdHttpError(REVIEW_NEEDS_REPORT)
        path = "/v1/tasks/" + urllib.parse.quote(task_id.strip(), safe="") + "/status"
        return self._request("POST", path, payload={"status": status, "note": note, "branch": branch})["task"]

    # Internal

    def _request(
        self,
        method: str,
        path: str,
        *,
        payload: Any = None,
        owner: bool = False,
    ) -> dict[str, Any]:
        headers = {"Accept": "application/json"}
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        if owner:
            if not self.invite:
                raise AircdHttpError("this action needs the owner key (invite)")
            headers["X-AirCD-Invite"] = self.invite
        data = None
        if payload is not None:
            headers["Content-Type"] = "application/json"
            data = json.dumps(payload).encode("utf-8")
        req = urllib.request.Request(
            f"{self.base_url}{path}", data=data, headers=headers, method=method
        )
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                raw = resp.read().decode("utf-8", errors="replace")
        except urllib.error.HTTPError as err:
            raw = err.read().decode("utf-8", errors="replace")
            try:
                message = json.loads(raw).get("error") or raw
            except Exception:
                message = raw or str(err)
            raise AircdHttpError(f"HTTP {err.code} {method} {path}: {message}", err.code) from None
        except urllib.error.URLError as err:
            raise AircdHttpError(f"Cannot reach aircd at {self.base_url}: {err.reason}") from None
        return json.loads(raw) if raw.strip() else {"ok": True}


def _template_path(template_id: str) -> str:
    template_id = template_id.strip()
    if not template_id:
        raise AircdHttpError("template id must not be empty")
    return "/v1/templates/" + urllib.parse.quote(template_id, safe="")


REVIEW_NEEDS_REPORT = (
    "handing in for review needs a test report: pass --note \"<what you ran and the result>\" "
    "or --report <report id>"
)


def _group_path(name: str) -> str:
    name = name.strip()
    if not name:
        raise AircdHttpError("group name must not be empty")
    return "/v1/groups/" + urllib.parse.quote(name, safe="")


def _clean_nick(nick: Optional[str]) -> Optional[str]:
    nick = (nick or "").strip().lstrip("@").strip()
    return nick or None


def task_create_payload(
    channel: str,
    title: str,
    *,
    body: Optional[str] = None,
    kind: str = "task",
    assignee: Optional[str] = None,
    repo: Optional[str] = None,
    branch: Optional[str] = None,
    issue_url: Optional[str] = None,
    parent: Optional[str] = None,
    reviewer: Optional[str] = None,
) -> dict[str, Any]:
    """The POST /v1/tasks body. ``parent_task_id`` and ``reviewer`` are only sent
    when set, so a hub without subtask support sees the same request as before."""
    payload: dict[str, Any] = {
        "channel": channel,
        "title": title,
        "body": body,
        "kind": kind,
        "assignee": assignee,
        "repo": repo,
        "branch": branch,
        "issue_url": issue_url,
    }
    parent = (parent or "").strip()
    if parent:
        payload["parent_task_id"] = parent
    reviewer = _clean_nick(reviewer)
    if reviewer:
        payload["reviewer"] = reviewer
    return payload


# ── IRC line parser ─────────────────────────────────────────────


def _parse_irc_line(line: str) -> tuple[str, str, list[str], dict[str, str]]:
    """Parse an IRC protocol line into (prefix, command, params, tags).

    Returns:
        (prefix, command, [params...], {tag_key: tag_value})
        prefix is empty string if not present.
        tags is empty dict if no message tags.
    """
    prefix = ""
    trailing = ""
    tags: dict[str, str] = {}

    if line.startswith("@"):
        tag_end = line.index(" ")
        tag_str = line[1:tag_end]
        for part in tag_str.split(";"):
            if "=" in part:
                k, v = part.split("=", 1)
                tags[k] = _unescape_tag_value(v)
            else:
                tags[part] = ""
        line = line[tag_end + 1 :]

    if line.startswith(":"):
        space = line.index(" ")
        prefix = line[1:space]
        line = line[space + 1 :]

    if " :" in line:
        idx = line.index(" :")
        trailing = line[idx + 2 :]
        line = line[:idx]

    parts = line.split()
    command = parts[0].upper() if parts else ""
    params = parts[1:] if len(parts) > 1 else []
    if trailing:
        params.append(trailing)

    return prefix, command, params, tags


def _extract_nick(prefix: str) -> str:
    """Extract nick from IRC prefix like 'nick!user@host'."""
    if "!" in prefix:
        return prefix.split("!")[0]
    return prefix


def _unescape_tag_value(value: str) -> str:
    """Unescape an IRCv3 message tag value."""
    output: list[str] = []
    index = 0
    while index < len(value):
        character = value[index]
        if character != "\\":
            output.append(character)
            index += 1
            continue

        index += 1
        if index >= len(value):
            output.append("\\")
            break

        escaped = value[index]
        output.append(
            {
                ":": ";",
                "s": " ",
                "\\": "\\",
                "r": "\r",
                "n": "\n",
            }.get(escaped, escaped)
        )
        index += 1

    return "".join(output)
