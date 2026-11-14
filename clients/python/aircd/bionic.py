"""Function-calling agent runner for LM Studio Bionic and OpenAI-compatible servers.

Bridges CAMS wakes (or standalone CLI invocations) to a local or remote
OpenAI-compatible ``/v1/chat/completions`` endpoint such as LM Studio Bionic
(default ``http://localhost:1235/v1``), exposing file and shell tools so local
tool-calling models can operate as autonomous CAMS workers and group members.

Configuration precedence (highest wins):
    CLI flags -> environment variables -> built-in defaults.
"""

from __future__ import annotations

import argparse
import dataclasses
import json
import os
import platform
import queue
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Iterable, Iterator, Optional

DEFAULT_BASE_URL = "http://localhost:1235/v1"
DEFAULT_API_KEY = "lm-studio"
DEFAULT_MAX_TURNS = 30
DEFAULT_TIMEOUT_SECONDS = 300.0
DEFAULT_COMMAND_TIMEOUT_SECONDS = 120.0
DEFAULT_MAX_OUTPUT_CHARS = 16000
DEFAULT_PULL_TIMEOUT_SECONDS = 1800.0
STREAM_FORMATS = ("text", "jsonl")

_SESSION_ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,127}$")


class BionicError(RuntimeError):
    """Raised when configuration, model discovery, or API execution fails."""


TOOLS: list[dict[str, Any]] = [
    {
        "type": "function",
        "function": {
            "name": "run_command",
            "description": (
                "Execute a shell command in the working directory and return "
                "JSON with exit_code, stdout, and stderr."
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Shell command to execute.",
                    }
                },
                "required": ["command"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "read_file",
            "description": (
                "Read text content from a file path, optionally limited to an "
                "inclusive 1-based line range."
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path to read."},
                    "start_line": {
                        "type": "integer",
                        "description": "Optional 1-based starting line number.",
                    },
                    "end_line": {
                        "type": "integer",
                        "description": "Optional 1-based ending line number (inclusive).",
                    },
                },
                "required": ["path"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "write_file",
            "description": (
                "Write full text content to a file path, creating parent "
                "directories if needed."
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path to write."},
                    "content": {"type": "string", "description": "Full file content."},
                },
                "required": ["path", "content"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "edit_file",
            "description": (
                "Replace one exact unique occurrence of target text in an "
                "existing file with replacement text."
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path to edit."},
                    "target": {
                        "type": "string",
                        "description": "Exact existing text to replace (must match once).",
                    },
                    "replacement": {
                        "type": "string",
                        "description": "Replacement text.",
                    },
                },
                "required": ["path", "target", "replacement"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "apply_patch",
            "description": (
                "Apply a unified diff (as produced by `diff -u` or `git diff`) to one or "
                "more files. Hunks are located by their context and removed lines, so "
                "line numbers in @@ headers may be approximate or omitted (`@@ @@`). "
                "Use `--- /dev/null` to create a file and `+++ /dev/null` to delete one. "
                "All hunks must apply or no file is changed. Returns JSON describing "
                "each file changed."
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "patch": {
                        "type": "string",
                        "description": (
                            "Unified diff text with ---/+++ file headers and @@ hunks; "
                            "context lines start with a space, removals with '-', "
                            "additions with '+'."
                        ),
                    },
                    "path": {
                        "type": "string",
                        "description": (
                            "Optional target file for a single-file patch; required when "
                            "the patch has no ---/+++ headers and overrides the header path."
                        ),
                    },
                    "dry_run": {
                        "type": "boolean",
                        "description": "Validate the patch and report the result without writing.",
                    },
                },
                "required": ["patch"],
            },
        },
    },
]


class MaxTurnsError(BionicError):
    """``run_agent`` used every turn without a final answer; carries the work done so far."""

    def __init__(
        self,
        message: str,
        *,
        turns: int = 0,
        tool_calls: int = 0,
        usage: Optional[dict[str, int]] = None,
        turn_usage: Optional[list[Optional[dict[str, int]]]] = None,
    ) -> None:
        super().__init__(message)
        self.turns = turns
        self.tool_calls = tool_calls
        self.usage = dict(usage or {})
        self.turn_usage = list(turn_usage or [])


@dataclass
class BionicConfig:
    """Runtime settings for the Bionic agent runner."""

    base_url: str = DEFAULT_BASE_URL
    api_key: str = DEFAULT_API_KEY
    model: str = ""
    cwd: str = ""
    max_turns: int = DEFAULT_MAX_TURNS
    timeout: float = DEFAULT_TIMEOUT_SECONDS
    command_timeout: float = DEFAULT_COMMAND_TIMEOUT_SECONDS
    max_output_chars: int = DEFAULT_MAX_OUTPUT_CHARS
    temperature: Optional[float] = None
    max_tokens: Optional[int] = None
    system_prompt: Optional[str] = None
    session_id: Optional[str] = None
    session_dir: Optional[str] = None
    emit_session_id: bool = False
    verbose: bool = False
    workspace_root: str = ""
    approval: str = "auto"
    stream: bool = False
    stream_format: str = "text"
    auto_pull: bool = False
    lms_path: str = ""
    pull_timeout: float = DEFAULT_PULL_TIMEOUT_SECONDS
    #: Extra plugin tool directories (absolute paths), loaded in addition to
    #: ``<workspace_root>/.aircd/tools``; see :mod:`aircd.bionic_plugins`.
    tools_dirs: list[str] = field(default_factory=list)
    #: Load user-defined plugin tools at all (``--no-plugins`` turns this off).
    plugins: bool = True


@dataclass
class RunResult:
    """Outcome of a single Bionic agent run."""

    content: str
    model: str
    turns: int
    tool_calls: int
    session_id: Optional[str] = None
    #: Token totals summed over every turn that reported ``usage``
    #: (``prompt_tokens``, ``completion_tokens``, ``total_tokens``); empty when
    #: the server sent no usage.
    usage: dict[str, int] = field(default_factory=dict)
    #: Per-turn ``usage`` objects in turn order (``None`` for a turn without one).
    turn_usage: list[Optional[dict[str, int]]] = field(default_factory=list)


def _env_first(*names: str) -> str:
    for name in names:
        val = os.environ.get(name, "").strip()
        if val:
            return val
    return ""


def _parse_int(raw: str, name: str, *, minimum: int = 1) -> int:
    try:
        value = int(raw)
    except ValueError as err:
        raise BionicError(f"{name} must be an integer, got {raw!r}") from err
    if value < minimum:
        raise BionicError(f"{name} must be >= {minimum}, got {value}")
    return value


def _parse_float(raw: str, name: str, *, minimum: Optional[float] = None) -> float:
    try:
        value = float(raw)
    except ValueError as err:
        raise BionicError(f"{name} must be a number, got {raw!r}") from err
    if minimum is not None and value <= minimum:
        raise BionicError(f"{name} must be > {minimum}, got {value}")
    return value


def _parse_bool_env(name: str) -> bool:
    return os.environ.get(name, "").strip().lower() in ("1", "true", "yes", "on")


def normalize_base_url(url: str) -> str:
    """Normalize an OpenAI-compatible base URL."""
    clean = url.strip().rstrip("/")
    if not clean:
        raise BionicError("base URL must not be empty")
    if not clean.startswith(("http://", "https://")):
        clean = f"http://{clean}"
    return clean


def resolve_config(args: argparse.Namespace) -> BionicConfig:
    """Resolve runtime configuration from CLI flags, environment, and defaults."""
    raw_url = (
        getattr(args, "base_url", None)
        or _env_first("BIONIC_BASE_URL", "AIRCD_BIONIC_BASE_URL", "OPENAI_BASE_URL")
        or DEFAULT_BASE_URL
    )
    base_url = normalize_base_url(raw_url)

    api_key = getattr(args, "api_key", None)
    if api_key is None:
        api_key = _env_first("BIONIC_API_KEY", "OPENAI_API_KEY") or DEFAULT_API_KEY

    model = (getattr(args, "model", None) or os.environ.get("BIONIC_MODEL", "")).strip()

    cwd_raw = getattr(args, "cwd", None) or os.environ.get("BIONIC_CWD", "") or os.getcwd()
    cwd_path = Path(cwd_raw).expanduser().resolve()
    if not cwd_path.is_dir():
        raise BionicError(f"working directory does not exist: {cwd_path}")

    root_raw = getattr(args, "workspace_root", None) or os.environ.get("BIONIC_WORKSPACE_ROOT", "").strip()
    root_path = Path(root_raw).expanduser().resolve() if root_raw else cwd_path
    if not root_path.is_dir():
        raise BionicError(f"workspace root does not exist: {root_path}")
    if not is_within(cwd_path, root_path):
        raise BionicError(f"working directory {cwd_path} is outside workspace root {root_path}")

    approval = (getattr(args, "approval", None) or os.environ.get("BIONIC_APPROVAL", "").strip() or "auto").lower()
    if approval not in APPROVAL_MODES:
        raise BionicError(f"approval mode must be one of {', '.join(APPROVAL_MODES)}, got {approval!r}")

    max_turns = getattr(args, "max_turns", None)
    if max_turns is None:
        env_turns = os.environ.get("BIONIC_MAX_TURNS", "").strip()
        max_turns = _parse_int(env_turns, "BIONIC_MAX_TURNS", minimum=1) if env_turns else DEFAULT_MAX_TURNS
    elif max_turns < 1:
        raise BionicError(f"--max-turns must be >= 1, got {max_turns}")

    timeout = getattr(args, "timeout", None)
    if timeout is None:
        env_timeout = os.environ.get("BIONIC_TIMEOUT", "").strip()
        timeout = _parse_float(env_timeout, "BIONIC_TIMEOUT", minimum=0.0) if env_timeout else DEFAULT_TIMEOUT_SECONDS
    elif timeout <= 0:
        raise BionicError(f"--timeout must be > 0, got {timeout}")

    command_timeout = getattr(args, "command_timeout", None)
    if command_timeout is None:
        env_cmd_timeout = os.environ.get("BIONIC_COMMAND_TIMEOUT", "").strip()
        command_timeout = (
            _parse_float(env_cmd_timeout, "BIONIC_COMMAND_TIMEOUT", minimum=0.0)
            if env_cmd_timeout
            else DEFAULT_COMMAND_TIMEOUT_SECONDS
        )
    elif command_timeout <= 0:
        raise BionicError(f"--command-timeout must be > 0, got {command_timeout}")

    max_output_chars = getattr(args, "max_output_chars", None)
    if max_output_chars is None:
        env_max_out = os.environ.get("BIONIC_MAX_OUTPUT_CHARS", "").strip()
        max_output_chars = (
            _parse_int(env_max_out, "BIONIC_MAX_OUTPUT_CHARS", minimum=256)
            if env_max_out
            else DEFAULT_MAX_OUTPUT_CHARS
        )
    elif max_output_chars < 256:
        raise BionicError(f"--max-output-chars must be >= 256, got {max_output_chars}")

    temperature = getattr(args, "temperature", None)
    if temperature is None:
        env_temp = os.environ.get("BIONIC_TEMPERATURE", "").strip()
        if env_temp:
            temperature = _parse_float(env_temp, "BIONIC_TEMPERATURE")

    max_tokens = getattr(args, "max_tokens", None)
    if max_tokens is None:
        env_max_tok = os.environ.get("BIONIC_MAX_TOKENS", "").strip()
        if env_max_tok:
            max_tokens = _parse_int(env_max_tok, "BIONIC_MAX_TOKENS", minimum=1)

    system_prompt = getattr(args, "system_prompt", None)
    system_prompt_file = getattr(args, "system_prompt_file", None)
    if system_prompt_file:
        try:
            system_prompt = Path(system_prompt_file).read_text(encoding="utf-8")
        except OSError as err:
            raise BionicError(f"cannot read system prompt file {system_prompt_file}: {err}") from err
    elif system_prompt is None:
        env_sys = os.environ.get("BIONIC_SYSTEM_PROMPT")
        if env_sys:
            system_prompt = env_sys

    session_id = getattr(args, "session_id", None)
    if session_id is not None:
        session_id = session_id.strip() or None
    session_dir = getattr(args, "session_dir", None) or os.environ.get("BIONIC_SESSION_DIR", "").strip() or None
    emit_session_id = bool(getattr(args, "emit_session_id", False) or session_id is not None)
    verbose = bool(getattr(args, "verbose", False) or _parse_bool_env("BIONIC_VERBOSE"))

    cli_stream_format = getattr(args, "stream_format", None)
    stream_format = (cli_stream_format or os.environ.get("BIONIC_STREAM_FORMAT", "").strip() or "text").lower()
    if stream_format not in STREAM_FORMATS:
        raise BionicError(f"stream format must be one of {', '.join(STREAM_FORMATS)}, got {stream_format!r}")
    # --stream-format on the command line implies --stream; the env var alone does not.
    stream = bool(getattr(args, "stream", False) or cli_stream_format or _parse_bool_env("BIONIC_STREAM"))

    auto_pull = bool(getattr(args, "auto_pull", False) or _parse_bool_env("BIONIC_AUTO_PULL"))
    lms_path = (getattr(args, "lms_path", None) or os.environ.get("BIONIC_LMS", "")).strip()
    pull_timeout = getattr(args, "pull_timeout", None)
    if pull_timeout is None:
        env_pull = os.environ.get("BIONIC_PULL_TIMEOUT", "").strip()
        pull_timeout = (
            _parse_float(env_pull, "BIONIC_PULL_TIMEOUT", minimum=0.0) if env_pull else DEFAULT_PULL_TIMEOUT_SECONDS
        )
    elif pull_timeout <= 0:
        raise BionicError(f"--pull-timeout must be > 0, got {pull_timeout}")

    raw_tools_dirs = getattr(args, "tools_dirs", None)
    if raw_tools_dirs is None:
        env_dirs = os.environ.get("BIONIC_TOOLS_DIR", "")
        raw_tools_dirs = [d for d in env_dirs.split(os.pathsep) if d.strip()]
    elif isinstance(raw_tools_dirs, str):
        raw_tools_dirs = [raw_tools_dirs]
    tools_dirs: list[str] = []
    for raw_dir in raw_tools_dirs:
        dir_path = Path(str(raw_dir).strip()).expanduser().resolve()
        if not dir_path.is_dir():
            raise BionicError(f"tools directory does not exist: {dir_path}")
        if str(dir_path) not in tools_dirs:
            tools_dirs.append(str(dir_path))
    plugins = getattr(args, "plugins", None)
    if plugins is None:
        plugins = not (getattr(args, "no_plugins", False) or _parse_bool_env("BIONIC_NO_PLUGINS"))

    return BionicConfig(
        base_url=base_url,
        api_key=api_key,
        model=model,
        cwd=str(cwd_path),
        max_turns=max_turns,
        timeout=timeout,
        command_timeout=command_timeout,
        max_output_chars=max_output_chars,
        temperature=temperature,
        max_tokens=max_tokens,
        system_prompt=system_prompt,
        session_id=session_id,
        session_dir=session_dir,
        emit_session_id=emit_session_id,
        verbose=verbose,
        workspace_root=str(root_path),
        approval=approval,
        stream=stream,
        stream_format=stream_format,
        auto_pull=auto_pull,
        lms_path=lms_path,
        pull_timeout=float(pull_timeout),
        tools_dirs=tools_dirs,
        plugins=bool(plugins),
    )


class WorkspaceViolation(ValueError):
    """Raised when a tool call targets a path outside the workspace root."""


def _real(path: Path) -> Path:
    """Fully resolve ``path`` (following symlinks/junctions) without requiring existence."""
    return Path(os.path.realpath(path))


def is_within(path: Path, root: Path) -> bool:
    """Return True when the fully resolved ``path`` is ``root`` or lies beneath it."""
    real_path = _real(path)
    real_root = _real(root)
    return real_path == real_root or real_root in real_path.parents


def resolve_path(cwd: str, path_str: str, workspace_root: Optional[str] = None) -> Path:
    """Resolve ``path_str`` against ``cwd`` and enforce the workspace jail.

    The result is fully resolved (``..`` segments collapsed and symlinks or
    junctions followed), so traversal, absolute paths, ``~`` and links that
    point outside ``workspace_root`` (default: ``cwd``) raise
    :class:`WorkspaceViolation`.
    """
    if not isinstance(path_str, str) or not path_str.strip():
        raise ValueError("path must be a non-empty string")
    if "\x00" in path_str:
        raise ValueError("path must not contain NUL characters")
    root = Path(workspace_root or cwd)
    p = Path(path_str).expanduser()
    # Path(cwd) / p also handles drive-relative ("D:x") and rooted ("\\x") Windows forms.
    full = _real(Path(cwd) / p)
    if not is_within(full, root):
        raise WorkspaceViolation(f"Path {path_str!r} resolves to {full}, outside workspace root {_real(root)}")
    return full


APPROVAL_MODES = ("auto", "prompt", "read-only")
MUTATING_TOOLS = frozenset({"run_command", "write_file", "edit_file", "apply_patch"})

# Splits a command line into quoted strings, shell operators, and bare words.
_CMD_TOKEN_RE = re.compile(r'"[^"]*"?|\'[^\']*\'?|&&|\|\||>>|[|;&<>()]|[^\s"\'|;&<>()]+')
_SEGMENT_BREAKS = frozenset({"&&", "||", "|", ";", "&", "(", ")"})
_REDIRECTS = frozenset({">", ">>", "<"})
_DEVICE_PATHS = frozenset({"/dev/null", "/dev/stdout", "/dev/stderr", "/dev/stdin", "nul", "con"})
_WIN_DRIVE_RE = re.compile(r"^[A-Za-z]:")
_WIN_SWITCH_RE = re.compile(r"^/[^/\\]*$")
_NESTED_SHELL_RE = re.compile(r"\$\(|`")


def _looks_like_path(value: str) -> bool:
    if not value or "://" in value:
        return False
    if value.startswith(("/", "\\", "~")) or _WIN_DRIVE_RE.match(value):
        return True
    if "$" in value or "%" in value:
        return True
    parts = re.split(r"[\\/]", value)
    return ".." in parts or len(parts) > 1


def check_command(command: str, cwd: str, workspace_root: Optional[str] = None) -> Optional[str]:
    """Best-effort static check that a shell command stays inside the workspace.

    Returns a human-readable reason when the command references a path outside
    ``workspace_root`` (default: ``cwd``), otherwise ``None``. The executable in
    each pipeline segment may live anywhere (so ``python``, ``git`` or an
    absolute interpreter path still work); every argument, redirect target and
    ``--flag=value`` that looks like a path must resolve inside the root.
    Commands can still escape through interpreted code (for example
    ``python -c``), so this complements, not replaces, OS-level sandboxing.
    """
    root = workspace_root or cwd
    on_windows = os.name == "nt"
    if _NESTED_SHELL_RE.search(command):
        return "command substitution ($(...) or backticks) is not allowed"
    expect_executable = True
    for raw in _CMD_TOKEN_RE.findall(command):
        if raw in _SEGMENT_BREAKS:
            expect_executable = True
            continue
        if raw in _REDIRECTS:
            continue
        token = raw[1:-1] if len(raw) >= 2 and raw[0] == raw[-1] and raw[0] in "\"'" else raw.strip("\"'")
        if expect_executable:
            expect_executable = False
            # Leading VAR=value assignments are not the executable.
            if re.match(r"^[A-Za-z_][A-Za-z0-9_]*=", token):
                expect_executable = True
                token = token.split("=", 1)[1]
            else:
                continue
        candidates = [token]
        if token.startswith("-"):
            candidates = [token.split("=", 1)[1]] if "=" in token else []
        elif "=" in token and not _looks_like_path(token.split("=", 1)[0]):
            candidates = [token.split("=", 1)[1]]
        for cand in candidates:
            if not _looks_like_path(cand) or cand.lower() in _DEVICE_PATHS:
                continue
            if on_windows and _WIN_SWITCH_RE.match(cand) and not _WIN_DRIVE_RE.match(cand):
                continue  # cmd.exe switch such as /b or /s
            expanded = os.path.expandvars(cand)
            if on_windows:
                expanded = re.sub(r"%([^%]+)%", lambda m: os.environ.get(m.group(1), m.group(0)), expanded)
            if re.search(r"\$\{?[A-Za-z_]|%[A-Za-z_][^%]*%", expanded):
                return f"argument {cand!r} references an unknown environment variable path"
            try:
                resolve_path(cwd, expanded, root)
            except (WorkspaceViolation, ValueError, OSError):
                return f"argument {cand!r} resolves outside workspace root {_real(Path(root))}"
    return None


class PatchError(ValueError):
    """Raised when a unified diff cannot be parsed or applied."""


_DEV_NULL = "/dev/null"
_HUNK_HEADER_RE = re.compile(r"^@@+\s*(?:-(\d+)(?:,(\d+))?)?\s*(?:\+(\d+)(?:,(\d+))?)?\s*@@+")
_HUNK_LINE_PREFIXES = (" ", "-", "+", "\\")


@dataclass
class _Hunk:
    old_start: Optional[int]
    lines: list[tuple[str, str]]
    old_no_eol: bool = False
    new_no_eol: bool = False

    @property
    def old_lines(self) -> list[str]:
        return [text for tag, text in self.lines if tag in (" ", "-")]

    @property
    def new_lines(self) -> list[str]:
        return [text for tag, text in self.lines if tag in (" ", "+")]


@dataclass
class _FilePatch:
    old_path: Optional[str]
    new_path: Optional[str]
    hunks: list[_Hunk]


def _header_path(raw: str) -> Optional[str]:
    # Drop a trailing "\t<timestamp>" and surrounding quotes from ---/+++ headers.
    value = raw.split("\t", 1)[0].strip()
    if len(value) >= 2 and value[0] == value[-1] == '"':
        value = value[1:-1]
    if not value or value == _DEV_NULL:
        return None
    return value


def parse_unified_diff(text: str) -> list[_FilePatch]:
    """Parse unified diff text into file patches.

    Lenient towards model output: ``@@`` line numbers are optional, blank lines
    inside a hunk are read as blank context, and git metadata lines are skipped.
    A hunk with no ---/+++ header before it gets ``old_path``/``new_path`` of
    ``""``, meaning "use the caller-supplied path".
    """
    if not isinstance(text, str) or not text.strip():
        raise PatchError("patch must be a non-empty string")
    lines = text.replace("\r\n", "\n").replace("\r", "\n").split("\n")
    if lines and lines[-1] == "":
        lines.pop()

    files: list[_FilePatch] = []
    current: Optional[_FilePatch] = None
    i = 0
    while i < len(lines):
        line = lines[i]
        if line.startswith("--- ") and i + 1 < len(lines) and lines[i + 1].startswith("+++ "):
            current = _FilePatch(_header_path(line[4:]), _header_path(lines[i + 1][4:]), [])
            files.append(current)
            i += 2
            continue
        match = _HUNK_HEADER_RE.match(line)
        if not match:
            i += 1  # "diff --git", "index ...", mode lines, or prose around the diff
            continue
        if current is None:
            current = _FilePatch("", "", [])
            files.append(current)
        old_start = int(match.group(1)) if match.group(1) is not None else None
        old_left = int(match.group(2) or 1) if match.group(1) is not None else None
        new_left = int(match.group(4) or 1) if match.group(3) is not None else None
        counted = old_left is not None and new_left is not None
        hunk = _Hunk(old_start, [])
        i += 1
        while i < len(lines):
            body = lines[i]
            if _HUNK_HEADER_RE.match(body):
                break
            if body.startswith("--- ") and i + 1 < len(lines) and lines[i + 1].startswith("+++ "):
                break
            # Model-written @@ counts are often wrong, so they never cut a hunk short;
            # they only decide whether a blank line is context or the end of the hunk.
            exhausted = counted and old_left <= 0 and new_left <= 0
            if body == "" and (exhausted or i + 1 == len(lines)):
                break
            if body == "":
                tag, content = " ", ""
            elif body[0] in _HUNK_LINE_PREFIXES:
                tag, content = body[0], body[1:]
            else:
                break
            if tag == "\\":
                if hunk.lines:
                    last_tag = hunk.lines[-1][0]
                    if last_tag in (" ", "-"):
                        hunk.old_no_eol = True
                    if last_tag in (" ", "+"):
                        hunk.new_no_eol = True
            else:
                hunk.lines.append((tag, content))
                if counted:
                    if tag in (" ", "-"):
                        old_left -= 1
                    if tag in (" ", "+"):
                        new_left -= 1
            i += 1
        if not hunk.lines:
            raise PatchError(f"hunk {len(current.hunks) + 1} is empty")
        current.hunks.append(hunk)

    files = [f for f in files if f.hunks]
    if not files:
        raise PatchError("patch contains no @@ hunks")
    return files


def _find_block(
    lines: list[str], block: list[str], start: int, preferred: Optional[int]
) -> tuple[int, int]:
    """Locate ``block`` in ``lines[start:]``; return (index, fuzz level).

    Fuzz 0 is an exact match, 1 ignores trailing whitespace and 2 ignores all
    leading/trailing whitespace. With several candidates the one nearest
    ``preferred`` wins; without a line hint an ambiguous match is an error.
    """
    normalizers: list[Callable[[str], str]] = [lambda s: s, str.rstrip, str.strip]
    n = len(block)
    for fuzz, norm in enumerate(normalizers):
        want = [norm(s) for s in block]
        hits = [
            idx
            for idx in range(start, len(lines) - n + 1)
            if [norm(s) for s in lines[idx : idx + n]] == want
        ]
        if not hits:
            continue
        if preferred is None:
            if len(hits) > 1:
                raise PatchError(
                    f"context matches {len(hits)} places (lines {', '.join(str(h + 1) for h in hits[:5])}); "
                    "add more context or @@ line numbers"
                )
            return hits[0], fuzz
        return min(hits, key=lambda h: (abs(h - preferred), h)), fuzz
    raise PatchError("context/removed lines not found in file")


def _apply_hunks(
    lines: list[str], final_newline: bool, hunks: list[_Hunk], label: str
) -> tuple[list[str], bool, list[dict[str, Any]]]:
    out = list(lines)
    cursor = 0
    delta = 0
    report: list[dict[str, Any]] = []
    for number, hunk in enumerate(hunks, 1):
        old_block = hunk.old_lines
        new_block = hunk.new_lines
        preferred = None if hunk.old_start is None else max(0, hunk.old_start - 1 + delta)
        if not old_block:
            if preferred is None and out:
                raise PatchError(f"{label}: hunk {number} has no context lines; add context or an @@ line number")
            # "@@ -N,0 +M,k @@" inserts after old line N (N=0: at the top).
            idx = 0 if hunk.old_start is None else min(max(0, hunk.old_start + delta), len(out))
            preferred, fuzz = idx, 0
        else:
            try:
                idx, fuzz = _find_block(out, old_block, cursor, preferred)
            except PatchError as err:
                first = next((s for s in old_block if s.strip()), old_block[0])
                raise PatchError(f"{label}: hunk {number}: {err} (first expected line: {first!r})") from err
        out[idx : idx + len(old_block)] = new_block
        entry: dict[str, Any] = {"hunk": number, "line": idx + 1}
        if fuzz:
            entry["fuzz"] = "trailing-whitespace" if fuzz == 1 else "whitespace"
        if preferred is not None and idx != preferred:
            entry["offset"] = idx - preferred
        report.append(entry)
        cursor = idx + len(new_block)
        delta += len(new_block) - len(old_block)
        if idx + len(new_block) == len(out):
            if hunk.new_no_eol:
                final_newline = False
            elif hunk.old_no_eol:
                final_newline = True
    return out, final_newline, report


def _split_text(text: str) -> tuple[list[str], bool, str]:
    eol = "\r\n" if "\r\n" in text else "\n"
    norm = text.replace("\r\n", "\n")
    if norm == "":
        return [], True, eol
    final_newline = norm.endswith("\n")
    body = norm[:-1] if final_newline else norm
    return body.split("\n"), final_newline, eol


def _join_text(lines: list[str], final_newline: bool, eol: str) -> str:
    if not lines:
        return ""
    return eol.join(lines) + (eol if final_newline else "")


def _patch_target(cwd: str, root: str, raw: str) -> Path:
    # git-style a/ and b/ prefixes are dropped unless the prefixed path itself exists.
    if raw[:2] in ("a/", "b/"):
        stripped = raw[2:]
        if not Path(cwd, raw).exists() or Path(cwd, stripped).exists():
            raw = stripped
    return resolve_path(cwd, raw, root)


def _plan_patch(args: dict[str, Any], cwd: str, root: str) -> list[tuple[_FilePatch, Optional[Path], Optional[Path]]]:
    """Parse the patch and resolve every source/destination path inside the jail."""
    files = parse_unified_diff(args.get("patch"))  # type: ignore[arg-type]
    override = args.get("path")
    if override is not None and not isinstance(override, str):
        raise PatchError("apply_patch 'path' must be a string")
    override = (override or "").strip() or None
    if override and len(files) > 1:
        raise PatchError("'path' can only be used with a single-file patch")
    plan = []
    for fp in files:
        old_raw, new_raw = fp.old_path, fp.new_path
        if override:
            old_raw = override if old_raw is not None else None
            new_raw = override if new_raw is not None else None
        if old_raw == "" or new_raw == "":
            raise PatchError("patch has no ---/+++ file headers; pass 'path'")
        if old_raw is None and new_raw is None:
            raise PatchError("patch header has /dev/null on both sides")
        src = _patch_target(cwd, root, old_raw) if old_raw is not None else None
        dst = _patch_target(cwd, root, new_raw) if new_raw is not None else None
        plan.append((fp, src, dst))
    return plan


def apply_patch_plan(
    plan: list[tuple[_FilePatch, Optional[Path], Optional[Path]]], *, dry_run: bool = False
) -> dict[str, Any]:
    """Apply a planned patch atomically: every hunk must apply before any write."""
    state: dict[Path, Optional[tuple[list[str], bool, str]]] = {}
    originals: dict[Path, Optional[bytes]] = {}

    def load(path: Path) -> Optional[tuple[list[str], bool, str]]:
        if path not in state:
            if path.is_dir():
                raise PatchError(f"path is a directory: {path}")
            if path.exists():
                raw = path.read_bytes()
                originals[path] = raw
                try:
                    state[path] = _split_text(raw.decode("utf-8"))
                except UnicodeDecodeError as err:
                    raise PatchError(f"{path} is not valid UTF-8 text") from err
            else:
                originals[path] = None
                state[path] = None
        return state[path]

    report: list[dict[str, Any]] = []
    for fp, src, dst in plan:
        added = sum(1 for h in fp.hunks for tag, _ in h.lines if tag == "+")
        removed = sum(1 for h in fp.hunks for tag, _ in h.lines if tag == "-")
        if src is None:
            assert dst is not None
            existing = load(dst)
            if existing is not None and existing[0]:
                raise PatchError(f"{dst}: patch creates the file but it already exists")
            lines, final_nl, eol = existing or ([], True, "\n")
            action = "created"
        else:
            existing = load(src)
            if existing is None:
                raise PatchError(f"file not found: {src}")
            lines, final_nl, eol = existing
            action = "modified" if dst == src else ("deleted" if dst is None else "renamed")
        new_lines, final_nl, hunk_report = _apply_hunks(lines, final_nl, fp.hunks, str(src or dst))
        if dst is None:
            if new_lines:
                raise PatchError(f"{src}: patch deletes the file but {len(new_lines)} line(s) would remain")
            state[src] = None  # type: ignore[index]
        else:
            if src is not None and src != dst:
                if load(dst) is not None:
                    raise PatchError(f"{dst}: rename target already exists")
                state[src] = None
            state[dst] = (new_lines, final_nl, eol)
        entry: dict[str, Any] = {
            "path": str(dst or src),
            "action": action,
            "hunks": hunk_report,
            "added": added,
            "removed": removed,
        }
        if action == "renamed":
            entry["from"] = str(src)
        report.append(entry)

    if not dry_run:
        written: list[Path] = []
        try:
            for path, value in state.items():
                if value is None:
                    if originals.get(path) is not None:
                        written.append(path)
                        path.unlink()
                    continue
                data = _join_text(*value).encode("utf-8")
                if data == originals.get(path):
                    continue
                written.append(path)
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(data)
        except OSError as err:
            for path in written:  # best-effort rollback
                try:
                    if originals.get(path) is None:
                        path.unlink(missing_ok=True)
                    else:
                        path.write_bytes(originals[path])  # type: ignore[arg-type]
                except OSError:
                    pass
            raise PatchError(f"failed writing patch result: {err}") from err
    return {"ok": True, "dry_run": dry_run, "files": report}


def _describe_tool_call(name: str, args: dict[str, Any]) -> str:
    if name == "run_command":
        return f"run_command: {args.get('command')}"
    if name == "write_file":
        content = args.get("content")
        size = len(content) if isinstance(content, str) else 0
        return f"write_file: {args.get('path')} ({size} chars)"
    if name == "edit_file":
        return f"edit_file: {args.get('path')}"
    if name == "apply_patch":
        files = args.get("_files") or ([args.get("path")] if args.get("path") else [])
        return f"apply_patch: {', '.join(str(f) for f in files) or '(patch)'}"
    return f"{name}: {json.dumps(args)[:200]}"


def tty_approver(name: str, args: dict[str, Any]) -> bool:
    """Ask the operator on the controlling terminal; deny when none is attached."""
    in_dev, out_dev = ("CONIN$", "CONOUT$") if os.name == "nt" else ("/dev/tty", "/dev/tty")
    try:
        with open(in_dev, "r", encoding="utf-8", errors="replace") as tin, open(
            out_dev, "w", encoding="utf-8", errors="replace"
        ) as tout:
            tout.write(f"[approval] Allow {_describe_tool_call(name, args)}? [y/N] ")
            tout.flush()
            answer = tin.readline()
    except OSError:
        return False
    return answer.strip().lower() in ("y", "yes")


BUILTIN_TOOL_NAMES = frozenset(t["function"]["name"] for t in TOOLS)


def tools_for_approval(approval: str, registry: Any = None) -> list[dict[str, Any]]:
    """Return the tool schemas the model should see for an approval mode.

    ``registry`` (a :class:`aircd.bionic_plugins.ToolRegistry`) appends plugin
    tools; ``read-only`` keeps only the non-mutating ones.
    """
    if approval == "read-only":
        tools = [t for t in TOOLS if t["function"]["name"] not in MUTATING_TOOLS]
    else:
        tools = list(TOOLS)
    if registry is not None:
        tools.extend(registry.schemas(approval))
    return tools


def plugin_tool_dirs(config: BionicConfig) -> list[str]:
    """Directories whose plugins a run loads: ``<workspace_root>/.aircd/tools`` (when it
    exists) followed by ``config.tools_dirs``.

    The implicit workspace directory is skipped in ``read-only`` mode, so a
    review run never imports code that lives in the repository it inspects;
    directories passed explicitly are always loaded. Empty when plugins are off.
    """
    if not config.plugins:
        return []
    dirs: list[str] = []
    if config.approval != "read-only":
        default_dir = Path(config.workspace_root or config.cwd or os.getcwd()) / ".aircd" / "tools"
        if default_dir.is_dir():
            dirs.append(str(default_dir.resolve()))
    for extra in config.tools_dirs:
        if extra not in dirs:
            dirs.append(extra)
    return dirs


def load_tool_registry(config: BionicConfig) -> Any:
    """Load and validate the plugin tools for ``config``; raises ``PluginError`` on a bad plugin."""
    from aircd.bionic_plugins import load_registry

    return load_registry(plugin_tool_dirs(config), builtin_names=BUILTIN_TOOL_NAMES)


def describe_tools(config: BionicConfig, registry: Any = None) -> list[dict[str, Any]]:
    """Summaries of every tool a run would offer the model (``--list-tools`` output)."""
    if registry is None:
        registry = load_tool_registry(config)
    out: list[dict[str, Any]] = []
    for schema in tools_for_approval(config.approval, registry):
        fn = schema["function"]
        plugin = registry.get(fn["name"]) if fn["name"] not in BUILTIN_TOOL_NAMES else None
        out.append(
            {
                "name": fn["name"],
                "description": fn["description"],
                "source": plugin.source if plugin else "builtin",
                "mutating": plugin.mutating if plugin else fn["name"] in MUTATING_TOOLS,
                "parameters": fn["parameters"],
            }
        )
    return out


def _decode_output(data: str | bytes | None) -> str:
    if data is None:
        return ""
    if isinstance(data, bytes):
        return data.decode("utf-8", errors="replace")
    return data


def exec_tool(
    name: str,
    args: dict[str, Any],
    cwd: str,
    *,
    command_timeout: float = DEFAULT_COMMAND_TIMEOUT_SECONDS,
    max_output_chars: int = DEFAULT_MAX_OUTPUT_CHARS,
    log: Optional[Callable[[str], None]] = None,
    workspace_root: Optional[str] = None,
    approval: str = "auto",
    approver: Optional[Callable[[str, dict[str, Any]], bool]] = None,
    registry: Any = None,
) -> str:
    """Execute one agent tool call and serialize its result as text/JSON.

    Every file path and command argument is confined to ``workspace_root``
    (default: ``cwd``). ``approval`` controls mutating tools: ``auto`` runs
    them, ``read-only`` refuses them, and ``prompt`` asks ``approver``
    (default: :func:`tty_approver`) before each one. Names not built in are
    looked up in ``registry`` (plugin tools), whose ``mutating`` flag gets
    the same approval treatment.
    """
    if not isinstance(args, dict):
        return json.dumps({"error": f"Tool {name} arguments must be a JSON object"})
    if approval not in APPROVAL_MODES:
        return json.dumps({"error": f"Unknown approval mode {approval!r}"})
    root = workspace_root or cwd
    if not is_within(Path(cwd), Path(root)):
        return json.dumps({"error": f"Working directory {cwd} is outside workspace root {root}"})

    try:
        plugin = registry.get(name) if registry is not None and name not in BUILTIN_TOOL_NAMES else None
        if plugin is not None:
            return _exec_plugin_tool(
                plugin,
                args,
                cwd,
                root,
                command_timeout=command_timeout,
                max_output_chars=max_output_chars,
                log=log,
                approval=approval,
                approver=approver,
            )
        return _exec_tool_jailed(
            name,
            args,
            cwd,
            root,
            command_timeout=command_timeout,
            max_output_chars=max_output_chars,
            log=log,
            approval=approval,
            approver=approver,
        )
    except WorkspaceViolation as err:
        if log:
            log(f"[jail] denied {name}: {err}")
        return json.dumps({"error": f"Denied: {err}", "denied": True})


def _exec_plugin_tool(
    plugin: Any,
    args: dict[str, Any],
    cwd: str,
    root: str,
    *,
    command_timeout: float,
    max_output_chars: int,
    log: Optional[Callable[[str], None]],
    approval: str,
    approver: Optional[Callable[[str, dict[str, Any]], bool]],
) -> str:
    from aircd.bionic_plugins import ToolArgumentError, ToolContext, run_plugin_tool, validate_arguments

    if plugin.mutating:
        if approval == "read-only":
            return json.dumps({"error": f"Denied: {plugin.name} is disabled in read-only approval mode", "denied": True})
        if approval == "prompt":
            try:
                validate_arguments(plugin, args)  # don't ask the operator about a call that cannot run
            except ToolArgumentError as err:
                return json.dumps({"error": str(err)})
            if not (approver or tty_approver)(plugin.name, args):
                if log:
                    log(f"[approval] rejected {_describe_tool_call(plugin.name, args)}")
                return json.dumps({"error": f"Denied: operator rejected {plugin.name}", "denied": True})
    ctx = ToolContext(
        cwd=cwd,
        workspace_root=root,
        command_timeout=command_timeout,
        max_output_chars=max_output_chars,
        log=log,
    )
    return run_plugin_tool(plugin, args, ctx)


def _exec_tool_jailed(
    name: str,
    args: dict[str, Any],
    cwd: str,
    root: str,
    *,
    command_timeout: float,
    max_output_chars: int,
    log: Optional[Callable[[str], None]],
    approval: str,
    approver: Optional[Callable[[str, dict[str, Any]], bool]],
) -> str:
    if name in MUTATING_TOOLS:
        if approval == "read-only":
            return json.dumps({"error": f"Denied: {name} is disabled in read-only approval mode", "denied": True})
        if name == "run_command":
            cmd = args.get("command")
            if isinstance(cmd, str) and cmd.strip():
                reason = check_command(cmd, cwd, root)
                if reason:
                    raise WorkspaceViolation(reason)
        elif name == "apply_patch":
            try:
                plan = _plan_patch(args, cwd, root)
            except PatchError as err:
                return json.dumps({"error": f"apply_patch: {err}"})
            files = sorted({str(p) for _, src, dst in plan for p in (src, dst) if p is not None})
            args = {**args, "_files": files}
            if bool(args.get("dry_run")):
                approval = "auto"  # a dry run writes nothing, so it never needs approval
        else:
            resolve_path(cwd, str(args.get("path") or ""), root)
        if approval == "prompt":
            if not (approver or tty_approver)(name, args):
                if log:
                    log(f"[approval] rejected {_describe_tool_call(name, args)}")
                return json.dumps({"error": f"Denied: operator rejected {name}", "denied": True})

    if name == "run_command":
        cmd = args.get("command")
        if not isinstance(cmd, str) or not cmd.strip():
            return json.dumps({"error": "run_command requires a non-empty 'command' string"})
        if log:
            log(f"[tool] run_command: {cmd}")
        half_limit = max(256, max_output_chars // 2)
        try:
            proc = subprocess.run(
                cmd,
                shell=True,
                cwd=cwd,
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                timeout=command_timeout,
            )
        except subprocess.TimeoutExpired as exc:
            stdout = _decode_output(exc.stdout)[-half_limit:]
            stderr = _decode_output(exc.stderr)[-half_limit:]
            return json.dumps(
                {
                    "error": f"Command timed out after {command_timeout}s",
                    "stdout": stdout,
                    "stderr": stderr,
                }
            )
        stdout = proc.stdout or ""
        stderr = proc.stderr or ""
        truncated = len(stdout) > half_limit or len(stderr) > half_limit
        return json.dumps(
            {
                "exit_code": proc.returncode,
                "stdout": stdout[-half_limit:],
                "stderr": stderr[-half_limit:],
                "truncated": truncated,
            }
        )

    if name == "write_file":
        path_val = args.get("path")
        content = args.get("content")
        if not isinstance(content, str):
            return json.dumps({"error": "write_file requires a string 'content'"})
        target = resolve_path(cwd, str(path_val or ""), root)
        if log:
            log(f"[tool] write_file: {target}")
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content, encoding="utf-8")
        return json.dumps({"ok": True, "path": str(target), "bytes": len(content.encode("utf-8"))})

    if name == "edit_file":
        path_val = args.get("path")
        target_text = args.get("target")
        replacement = args.get("replacement")
        if not isinstance(target_text, str) or not target_text:
            return json.dumps({"error": "edit_file requires a non-empty string 'target'"})
        if not isinstance(replacement, str):
            return json.dumps({"error": "edit_file requires a string 'replacement'"})
        target = resolve_path(cwd, str(path_val or ""), root)
        if not target.exists():
            return json.dumps({"error": f"File not found: {target}"})
        if target.is_dir():
            return json.dumps({"error": f"Path is a directory: {target}"})
        if log:
            log(f"[tool] edit_file: {target}")
        original = target.read_text(encoding="utf-8", errors="replace")
        matches = original.count(target_text)
        if matches == 0:
            return json.dumps({"error": "Target text not found in file"})
        if matches > 1:
            return json.dumps(
                {
                    "error": (
                        f"Target text matched {matches} times; include more surrounding "
                        "context so it matches uniquely"
                    )
                }
            )
        updated = original.replace(target_text, replacement, 1)
        target.write_text(updated, encoding="utf-8")
        return json.dumps({"ok": True, "path": str(target)})

    if name == "apply_patch":
        try:
            plan = _plan_patch(args, cwd, root)
            if log:
                log(f"[tool] {_describe_tool_call(name, args)}")
            return json.dumps(apply_patch_plan(plan, dry_run=bool(args.get("dry_run"))))
        except PatchError as err:
            return json.dumps({"error": f"apply_patch: {err}"})

    if name == "read_file":
        target = resolve_path(cwd, str(args.get("path") or ""), root)
        if not target.exists():
            return json.dumps({"error": f"File not found: {target}"})
        if target.is_dir():
            entries = sorted(p.name + ("/" if p.is_dir() else "") for p in target.iterdir())
            return json.dumps({"error": f"Path is a directory: {target}", "entries": entries[:200]})
        if log:
            log(f"[tool] read_file: {target}")
        text = target.read_text(encoding="utf-8", errors="replace")
        start_line = args.get("start_line")
        end_line = args.get("end_line")
        if start_line is not None or end_line is not None:
            lines = text.splitlines(keepends=True)
            start = 1 if start_line is None else max(1, int(start_line))
            end = len(lines) if end_line is None else min(len(lines), int(end_line))
            if start > end and len(lines) > 0:
                return json.dumps({"error": f"Invalid line range {start}..{end} (file has {len(lines)} lines)"})
            text = "".join(lines[start - 1 : end])
        if len(text) > max_output_chars:
            return text[:max_output_chars] + f"\n...[truncated at {max_output_chars} chars; use start_line/end_line]"
        return text

    return json.dumps({"error": f"Unknown tool: {name}"})


def _build_request(
    url: str,
    *,
    method: str,
    payload: Optional[dict[str, Any]],
    api_key: str,
    accept: str = "application/json",
) -> urllib.request.Request:
    headers = {"Accept": accept}
    data: Optional[bytes] = None
    if payload is not None:
        headers["Content-Type"] = "application/json"
        data = json.dumps(payload).encode("utf-8")
    if api_key:
        headers["Authorization"] = f"Bearer {api_key}"
    return urllib.request.Request(url, data=data, headers=headers, method=method)


def _error_detail(parsed: Any, fallback: str) -> str:
    err_obj = parsed.get("error") if isinstance(parsed, dict) else None
    if isinstance(err_obj, dict) and err_obj.get("message"):
        return str(err_obj["message"])
    if err_obj:
        return str(err_obj)
    return fallback


def _http_error(err: urllib.error.HTTPError, url: str) -> BionicError:
    detail = ""
    try:
        body = err.read().decode("utf-8", errors="replace").strip()
        if body:
            try:
                detail = _error_detail(json.loads(body), body)
            except json.JSONDecodeError:
                detail = body
    except Exception:
        detail = ""
    msg = f"Bionic API HTTP {err.code} from {url}"
    if detail:
        msg = f"{msg}: {detail}"
    return BionicError(msg)


def _parse_json_object(raw: str, url: str) -> dict[str, Any]:
    try:
        parsed = json.loads(raw)
    except json.JSONDecodeError as err:
        raise BionicError(f"Invalid JSON response from {url}: {err}") from err
    if not isinstance(parsed, dict):
        raise BionicError(f"Expected JSON object from {url}, got {type(parsed).__name__}")
    return parsed


def _http_json(
    url: str,
    *,
    method: str = "GET",
    payload: Optional[dict[str, Any]] = None,
    api_key: str = "",
    timeout: float = DEFAULT_TIMEOUT_SECONDS,
) -> dict[str, Any]:
    req = _build_request(url, method=method, payload=payload, api_key=api_key)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            raw = resp.read().decode("utf-8", errors="replace")
    except urllib.error.HTTPError as err:
        raise _http_error(err, url) from err
    except urllib.error.URLError as err:
        raise BionicError(f"Could not connect to Bionic API at {url}: {err.reason}") from err
    return _parse_json_object(raw, url)


StreamEmit = Callable[[str, str], None]


def iter_sse_data(lines: Iterable[bytes]) -> Iterator[str]:
    """Yield the ``data`` payload of each Server-Sent Event in a byte-line stream.

    Multi-line ``data:`` fields are joined with newlines; comments (``:``) and
    other fields (``event:``, ``id:``, ``retry:``) are ignored.
    """
    data_lines: list[str] = []
    for raw in lines:
        # UTF-8 continuation bytes never equal b"\n", so per-line decoding is safe.
        line = raw.decode("utf-8", errors="replace").rstrip("\r\n")
        if not line:
            if data_lines:
                yield "\n".join(data_lines)
                data_lines = []
            continue
        if line.startswith(":"):
            continue
        field, _, value = line.partition(":")
        if field != "data":
            continue
        data_lines.append(value[1:] if value.startswith(" ") else value)
    if data_lines:
        yield "\n".join(data_lines)


def accumulate_stream(events: Iterable[str], emit: Optional[StreamEmit] = None) -> dict[str, Any]:
    """Fold OpenAI ``chat.completion.chunk`` SSE payloads into one assistant message.

    ``emit(kind, text)`` is called for each ``"token"`` (content) and
    ``"reasoning"`` delta as it arrives. Tool-call fragments are merged by
    ``index`` so the returned message matches a non-streamed ``message``.
    """
    content: list[str] = []
    reasoning: list[str] = []
    calls: dict[int, dict[str, Any]] = {}
    usage: Optional[dict[str, int]] = None
    saw_chunk = False
    for data in events:
        if data.strip() == "[DONE]":
            break
        try:
            obj = json.loads(data)
        except json.JSONDecodeError as err:
            raise BionicError(f"Invalid JSON in Bionic stream chunk: {err}: {data[:200]}") from err
        if not isinstance(obj, dict):
            continue
        if obj.get("error"):
            raise BionicError(f"Bionic API stream error: {_error_detail(obj, json.dumps(obj)[:400])}")
        saw_chunk = True
        if isinstance(obj.get("usage"), dict):
            usage = obj["usage"]
        choices = obj.get("choices")
        if not isinstance(choices, list) or not choices or not isinstance(choices[0], dict):
            continue  # e.g. a trailing usage-only chunk
        delta = choices[0].get("delta")
        if not isinstance(delta, dict):
            delta = choices[0].get("message") if isinstance(choices[0].get("message"), dict) else {}

        text = delta.get("content")
        if isinstance(text, str) and text:
            content.append(text)
            if emit is not None:
                emit("token", text)
        think = delta.get("reasoning_content")
        if think is None:
            think = delta.get("reasoning")
        if isinstance(think, str) and think:
            reasoning.append(think)
            if emit is not None:
                emit("reasoning", think)

        for pos, frag in enumerate(delta.get("tool_calls") or []):
            if not isinstance(frag, dict):
                continue
            idx = frag.get("index")
            if not isinstance(idx, int):
                idx = pos
            slot = calls.setdefault(idx, {"id": "", "type": "function", "function": {"name": "", "arguments": ""}})
            if frag.get("id"):
                slot["id"] = str(frag["id"])
            if frag.get("type"):
                slot["type"] = str(frag["type"])
            fn = frag.get("function")
            if not isinstance(fn, dict):
                continue
            name = fn.get("name")
            if isinstance(name, str) and name and not slot["function"]["name"].endswith(name):
                slot["function"]["name"] += name
            args = fn.get("arguments")
            if isinstance(args, str):
                slot["function"]["arguments"] += args
            elif args is not None:
                slot["function"]["arguments"] = json.dumps(args)

    if not saw_chunk:
        raise BionicError("Bionic API stream ended without any completion chunks")

    msg: dict[str, Any] = {"role": "assistant", "content": "".join(content) if content else None}
    if reasoning:
        msg["reasoning_content"] = "".join(reasoning)
    if calls:
        tool_calls = []
        for idx in sorted(calls):
            call = calls[idx]
            if not call["id"]:
                del call["id"]
            tool_calls.append(call)
        msg["tool_calls"] = tool_calls
    if usage is not None:
        msg["usage"] = usage
    return msg


def _http_chat_stream(
    url: str,
    *,
    payload: dict[str, Any],
    api_key: str = "",
    timeout: float = DEFAULT_TIMEOUT_SECONDS,
    emit: Optional[StreamEmit] = None,
) -> dict[str, Any]:
    """POST a streaming chat completion and return the assembled assistant message.

    Falls back to a plain JSON body when the server ignores ``"stream": true``.
    """
    req = _build_request(
        url,
        method="POST",
        payload={**payload, "stream": True, "stream_options": {"include_usage": True}},
        api_key=api_key,
        accept="text/event-stream, application/json",
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            ctype = (resp.headers.get("Content-Type") or "").lower()
            if "text/event-stream" not in ctype:
                body = _parse_json_object(resp.read().decode("utf-8", errors="replace"), url)
                choices = body.get("choices")
                msg = choices[0].get("message") if isinstance(choices, list) and choices and isinstance(choices[0], dict) else None
                if not isinstance(msg, dict):
                    raise BionicError(f"Bionic API returned no choices: {json.dumps(body)[:400]}")
                if isinstance(body.get("usage"), dict):
                    msg = {**msg, "usage": body["usage"]}
                if emit is not None and isinstance(msg.get("content"), str) and msg["content"]:
                    emit("token", msg["content"])
                return msg
            return accumulate_stream(iter_sse_data(resp), emit)
    except urllib.error.HTTPError as err:
        raise _http_error(err, url) from err
    except urllib.error.URLError as err:
        raise BionicError(f"Could not connect to Bionic API at {url}: {err.reason}") from err
    except OSError as err:  # socket timeout / reset mid-stream
        raise BionicError(f"Bionic API stream from {url} was interrupted: {err}") from err


def _is_embedding_model(entry: dict[str, Any], model_id: str) -> bool:
    model_type = str(entry.get("type") or "").strip().lower()
    if model_type in ("embedding", "embeddings"):
        return True
    tokens = set(re.split(r"[-_/.:]+", model_id.lower()))
    return "embedding" in tokens or "embed" in tokens


def list_models(
    base_url: str,
    *,
    api_key: str = "",
    timeout: float = 30.0,
    include_all: bool = False,
) -> list[dict[str, str]]:
    """Query ``GET {base_url}/models`` and return normalized model entries."""
    url = normalize_base_url(base_url) + "/models"
    body = _http_json(url, method="GET", api_key=api_key, timeout=timeout)
    raw_list = body.get("data")
    if not isinstance(raw_list, list):
        raw_list = body.get("models")
    if not isinstance(raw_list, list):
        raise BionicError(f"Model list response from {url} did not contain 'data' or 'models'")

    out: list[dict[str, str]] = []
    seen: set[str] = set()
    for entry in raw_list:
        if not isinstance(entry, dict):
            continue
        model_id = str(entry.get("id") or entry.get("modelKey") or "").strip()
        if not model_id or model_id in seen:
            continue
        if not include_all and _is_embedding_model(entry, model_id):
            continue
        seen.add(model_id)
        item: dict[str, str] = {"id": model_id}
        display = str(entry.get("display_name") or entry.get("displayName") or "").strip()
        if display and display != model_id:
            item["display_name"] = display
        out.append(item)
    return out


def resolve_model(config: BionicConfig) -> str:
    """Resolve the model name, supporting ``auto`` discovery via ``/models``."""
    model = config.model.strip()
    if not model:
        raise BionicError(
            "Model is required: pass --model <id>, set BIONIC_MODEL, or use --model auto"
        )
    if model.lower() == "auto":
        models = list_models(config.base_url, api_key=config.api_key, timeout=min(config.timeout, 30.0))
        if not models:
            raise BionicError(f"No usable chat models returned by {config.base_url}/models")
        return models[0]["id"]
    return model


# --- Model readiness and auto-pull --------------------------------------------------------------

READY_STATES = ("loaded",)


def server_root(base_url: str) -> str:
    """Strip an OpenAI-style ``/v1`` suffix to get the server root (for LM Studio's ``/api/v0``)."""
    root = normalize_base_url(base_url)
    if root.endswith("/v1"):
        root = root[: -len("/v1")]
    return root


def model_states(base_url: str, *, api_key: str = "", timeout: float = 30.0) -> Optional[dict[str, str]]:
    """Return ``{model_id: state}`` from LM Studio's ``GET /api/v0/models``.

    ``state`` is typically ``loaded`` or ``not-loaded``. Returns ``None`` when the
    server does not offer that endpoint (plain OpenAI-compatible servers), so
    callers fall back to ``/v1/models`` presence plus a completion probe.
    """
    url = server_root(base_url) + "/api/v0/models"
    try:
        body = _http_json(url, method="GET", api_key=api_key, timeout=timeout)
    except BionicError:
        return None
    raw_list = body.get("data")
    if not isinstance(raw_list, list):
        return None
    states: dict[str, str] = {}
    for entry in raw_list:
        if not isinstance(entry, dict):
            continue
        model_id = str(entry.get("id") or "").strip()
        if model_id:
            states[model_id] = str(entry.get("state") or "unknown").strip().lower() or "unknown"
    return states


def find_lms(lms_path: str = "") -> Optional[str]:
    """Locate the LM Studio ``lms`` CLI: explicit path, then ``BIONIC_LMS``, then ``PATH``."""
    candidate = (lms_path or os.environ.get("BIONIC_LMS", "")).strip()
    if candidate:
        found = shutil.which(candidate)
        if found:
            return found
        return candidate if Path(candidate).is_file() else None
    return shutil.which("lms")


def _run_lms(lms: str, args: list[str], timeout: float) -> tuple[bool, str]:
    try:
        proc = subprocess.run(
            [lms, *args],
            capture_output=True,
            timeout=timeout,
            stdin=subprocess.DEVNULL,
        )
    except subprocess.TimeoutExpired:
        return False, f"timed out after {timeout:g}s"
    except OSError as err:
        return False, str(err)
    out = (_decode_output(proc.stdout) + _decode_output(proc.stderr)).strip()
    if len(out) > 2000:
        out = "..." + out[-2000:]
    return proc.returncode == 0, out


def probe_model(config: BionicConfig, model: str) -> dict[str, Any]:
    """Send a one-token chat completion to confirm ``model`` actually answers.

    LM Studio loads a not-yet-loaded model on demand (JIT) for this request, so
    a successful probe also means the model is now resident.
    """
    url = normalize_base_url(config.base_url) + "/chat/completions"
    payload = {
        "model": model,
        "messages": [{"role": "user", "content": "Reply with OK."}],
        "max_tokens": 1,
        "temperature": 0,
        "stream": False,
    }
    started = time.monotonic()
    try:
        resp = _http_json(url, method="POST", payload=payload, api_key=config.api_key, timeout=config.timeout)
    except BionicError as err:
        return {"ok": False, "latency_ms": int((time.monotonic() - started) * 1000), "error": str(err)}
    latency_ms = int((time.monotonic() - started) * 1000)
    choices = resp.get("choices")
    if not isinstance(choices, list) or not choices:
        return {"ok": False, "latency_ms": latency_ms, "error": f"no choices in reply: {json.dumps(resp)[:200]}"}
    return {"ok": True, "latency_ms": latency_ms, "error": None}


def check_health(
    config: BionicConfig,
    *,
    probe: Optional[bool] = True,
    auto_pull: Optional[bool] = None,
    log: Optional[Callable[[str], None]] = None,
) -> dict[str, Any]:
    """Check that the Bionic server is up and the configured model is loaded and answering.

    Steps: list ``/models`` (server reachable), resolve the model (``auto`` prefers
    an already-loaded chat model), look up its load state via LM Studio's
    ``/api/v0/models`` when available, optionally download (``lms get``) and load
    (``lms load``) it when ``auto_pull`` is set, then optionally probe it with a
    one-token completion. ``probe=None`` probes only when the load state is not
    already known to be ``loaded``. ``auto_pull=None`` uses ``config.auto_pull``.

    Returns a JSON-serializable report whose ``ok`` key is true when the model is
    ready for a task. Never raises for an unhealthy server; the reason is in
    ``error``.
    """
    say = log or (lambda _msg: None)
    pull = config.auto_pull if auto_pull is None else bool(auto_pull)
    list_timeout = min(config.timeout, 30.0)
    report: dict[str, Any] = {
        "ok": False,
        "base_url": normalize_base_url(config.base_url),
        "server": {"reachable": False},
        "model": None,
        "available": False,
        "state": None,
        "actions": [],
        "probe": None,
        "error": None,
    }

    def fail(msg: str) -> dict[str, Any]:
        report["error"] = msg
        return report

    try:
        listed = list_models(config.base_url, api_key=config.api_key, timeout=list_timeout)
    except BionicError as err:
        report["server"]["error"] = str(err)
        return fail(str(err))
    report["server"] = {"reachable": True, "models": len(listed)}
    states = model_states(config.base_url, api_key=config.api_key, timeout=list_timeout)

    requested = config.model.strip()
    if not requested:
        report["ok"] = bool(listed)
        report["available_models"] = [m["id"] for m in listed]
        if not listed:
            return fail(f"No usable chat models returned by {report['base_url']}/models")
        return report

    if requested.lower() == "auto":
        if not listed:
            return fail(f"No usable chat models returned by {report['base_url']}/models")
        loaded = [m["id"] for m in listed if states and states.get(m["id"]) in READY_STATES]
        model = loaded[0] if loaded else listed[0]["id"]
    else:
        model = requested
    report["model"] = model

    def refresh_state() -> None:
        nonlocal states, listed
        ids = {m["id"] for m in listed}
        report["available"] = model in ids or bool(states and model in states)
        if states is not None and model in states:
            report["state"] = states[model]
        else:
            report["state"] = "unknown" if report["available"] else "missing"

    refresh_state()

    if pull and (not report["available"] or report["state"] not in READY_STATES):
        lms = find_lms(config.lms_path)
        if lms is None:
            report["actions"].append(
                {"action": "lms", "ok": False, "detail": "lms CLI not found (set --lms or BIONIC_LMS)"}
            )
        else:
            if not report["available"]:
                say(f"[health] downloading {model} with lms get")
                ok, detail = _run_lms(lms, ["get", model, "--yes"], config.pull_timeout)
                report["actions"].append({"action": "get", "ok": ok, "detail": detail})
                if not ok:
                    return fail(f"lms get {model} failed: {detail or 'no output'}")
            say(f"[health] loading {model} with lms load")
            ok, detail = _run_lms(lms, ["load", model, "--yes"], config.pull_timeout)
            report["actions"].append({"action": "load", "ok": ok, "detail": detail})
            if not ok:
                return fail(f"lms load {model} failed: {detail or 'no output'}")
            try:
                listed = list_models(config.base_url, api_key=config.api_key, timeout=list_timeout)
            except BionicError as err:
                return fail(str(err))
            states = model_states(config.base_url, api_key=config.api_key, timeout=list_timeout)
            refresh_state()
            if report["state"] == "missing":
                # Some servers only list loaded models after a refresh; lms said it loaded.
                report["available"] = True
                report["state"] = "unknown"

    if not report["available"]:
        hint = " (use --auto-pull to download it)" if not pull else ""
        return fail(f"Model {model!r} is not available on {report['base_url']}{hint}")

    should_probe = probe if probe is not None else report["state"] not in READY_STATES
    if should_probe:
        say(f"[health] probing {model}")
        report["probe"] = probe_model(config, model)
        if not report["probe"]["ok"]:
            return fail(f"Model {model!r} did not answer: {report['probe']['error']}")
        if states is not None:
            refreshed = model_states(config.base_url, api_key=config.api_key, timeout=list_timeout)
            if refreshed is not None and refreshed.get(model):
                report["state"] = refreshed[model]
        report["ok"] = True
        return report

    if report["state"] not in READY_STATES and report["state"] != "unknown":
        return fail(f"Model {model!r} is {report['state']}, not loaded (use --auto-pull to load it)")
    report["ok"] = True
    return report


def ensure_model_ready(config: BionicConfig, *, log: Optional[Callable[[str], None]] = None) -> dict[str, Any]:
    """Pre-task readiness check: run :func:`check_health` with auto-pull, raise if not ready."""
    report = check_health(config, probe=None, auto_pull=True, log=log)
    if not report["ok"]:
        raise BionicError(f"Bionic model not ready: {report['error']}")
    return report


def build_system_prompt(
    cwd: str,
    custom_prompt: Optional[str] = None,
    *,
    workspace_root: Optional[str] = None,
    approval: str = "auto",
    plugin_tools: Optional[list[str]] = None,
) -> str:
    """Build the system prompt for the Bionic coding agent.

    ``plugin_tools`` names the user-defined tools offered alongside the built-ins.
    """
    if custom_prompt is not None:
        return custom_prompt
    os_name = platform.system() or os.name
    root = workspace_root or cwd
    extra = ", ".join(plugin_tools or [])
    if approval == "read-only":
        tool_line = (
            "You are in read-only mode: only the read_file tool is available, so inspect "
            "files and report findings without attempting changes."
            if not extra
            else f"You are in read-only mode: only read_file and the read-only project tools ({extra}) "
            "are available, so inspect files and report findings without attempting changes."
        )
    else:
        tool_line = (
            "Use the provided tools (run_command, read_file, write_file, edit_file, apply_patch) "
            "to inspect files, make changes, run coordination commands (such as `airc`), "
            "and verify tests before replying."
        )
        if extra:
            tool_line += f" Project-specific tools are also available: {extra}."
    parts = [
        f"You are an autonomous coding agent running on {os_name}.",
        f"Your working directory is {cwd}.",
        (
            f"All file paths and command arguments must stay inside the workspace root {root}; "
            "tool calls that reference paths outside it are denied."
        ),
        tool_line,
        (
            "Read files before modifying them, prefer apply_patch (unified diff) or edit_file "
            "for targeted changes instead of rewriting whole files with write_file, "
            "and never fabricate command output or file contents. When finished, give "
            "a concise, accurate final answer."
        ),
    ]
    nick = os.environ.get("AIRC_NICK", "").strip()
    server = os.environ.get("AIRC_SERVER", "").strip()
    if nick or server:
        hub_bits = [bit for bit in (f"@{nick}" if nick else "", f"hub {server}" if server else "") if bit]
        parts.append(f"CAMS runtime context: {', '.join(hub_bits)}.")
    return " ".join(parts)


def _resolve_session_file(session_id: str, session_dir: Optional[str]) -> Path:
    if not _SESSION_ID_RE.match(session_id):
        raise BionicError(
            "session_id must contain only ASCII letters, digits, '-' or '_' (max 128 chars)"
        )
    root = Path(session_dir).expanduser() if session_dir else Path(tempfile.gettempdir()) / "aircd-bionic-sessions"
    return root / f"{session_id}.json"


def _load_session_messages(path: Path) -> Optional[list[dict[str, Any]]]:
    if not path.is_file():
        return None
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None
    if isinstance(data, list) and all(isinstance(m, dict) for m in data):
        return data
    return None


def _save_session_messages(path: Path, messages: list[dict[str, Any]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp_path = path.with_name(f".{path.name}.{uuid.uuid4().hex}.tmp")
    tmp_path.write_text(json.dumps(messages), encoding="utf-8")
    tmp_path.replace(path)


def _normalize_assistant_message(msg: dict[str, Any]) -> dict[str, Any]:
    """Keep only standard OpenAI assistant fields when feeding history back."""
    cleaned: dict[str, Any] = {
        "role": "assistant",
        "content": msg.get("content") if msg.get("content") is not None else "",
    }
    tool_calls = msg.get("tool_calls")
    if isinstance(tool_calls, list) and tool_calls:
        cleaned["tool_calls"] = tool_calls
    return cleaned


_USAGE_KEYS = ("prompt_tokens", "completion_tokens", "total_tokens")


def _usage_counts(raw: Any) -> Optional[dict[str, int]]:
    """Pick the integer token counts out of an OpenAI ``usage`` object."""
    if not isinstance(raw, dict):
        return None
    counts = {k: int(raw[k]) for k in _USAGE_KEYS if isinstance(raw.get(k), (int, float)) and not isinstance(raw.get(k), bool)}
    if counts and "total_tokens" not in counts:
        counts["total_tokens"] = counts.get("prompt_tokens", 0) + counts.get("completion_tokens", 0)
    return counts or None


def _tool_result_error(result: str) -> Optional[str]:
    try:
        parsed = json.loads(result)
    except (TypeError, json.JSONDecodeError):
        return None
    if isinstance(parsed, dict) and parsed.get("error"):
        return str(parsed["error"])
    return None


class StreamRenderer:
    """Render ``run_agent`` progress events to a text stream (stderr by default).

    ``text`` writes tokens as they arrive plus ``[tool]`` lines; ``jsonl``
    writes one JSON event per line for machine-readable execution logs.
    """

    def __init__(self, fmt: str = "text", out: Any = None) -> None:
        if fmt not in STREAM_FORMATS:
            raise BionicError(f"stream format must be one of {', '.join(STREAM_FORMATS)}, got {fmt!r}")
        self.fmt = fmt
        self.out = out if out is not None else sys.stderr
        self._mode = ""  # "", "token" or "reasoning": what the current line holds

    def _write(self, text: str) -> None:
        try:
            self.out.write(text)
        except UnicodeEncodeError:
            enc = getattr(self.out, "encoding", None) or "ascii"
            self.out.write(text.encode(enc, errors="replace").decode(enc, errors="replace"))
        self.out.flush()

    def _end_line(self) -> None:
        if self._mode:
            self._write("\n")
            self._mode = ""

    def __call__(self, event: dict[str, Any]) -> None:
        if self.fmt == "jsonl":
            self._write(json.dumps(event, ensure_ascii=False) + "\n")
            return
        kind = event.get("type")
        if kind in ("token", "reasoning"):
            if self._mode != kind:
                self._end_line()
                if kind == "reasoning":
                    self._write("[thinking] ")
                self._mode = kind
            self._write(str(event.get("text") or ""))
        elif kind == "tool_call":
            self._end_line()
            args = event.get("arguments") or ""
            try:
                parsed = json.loads(args) if isinstance(args, str) and args else {}
            except json.JSONDecodeError:
                parsed = None
            desc = (
                _describe_tool_call(str(event.get("name") or ""), parsed)
                if isinstance(parsed, dict)
                else f"{event.get('name')} {str(args)[:200]}"
            )
            self._write(f"[tool] {desc}\n")
        elif kind == "tool_result":
            self._end_line()
            if event.get("error"):
                self._write(f"[tool] {event.get('name')} failed: {event['error']}\n")
        elif kind == "done":
            self._end_line()


class RunAborted(BionicError):
    """Raised by a ``before_tool`` hook to stop a run (e.g. the REPL's abort)."""


@dataclass
class ToolStep:
    """One pending tool call handed to a ``run_agent`` ``before_tool`` hook."""

    turn: int
    id: str
    name: str
    #: Parsed arguments; the hook may replace or mutate them before execution.
    arguments: Any
    runner: Callable[[Any], str] = field(repr=False, default=lambda _args: "")
    #: Set by :meth:`execute`; when the hook returns ``None`` after executing,
    #: this result is used instead of running the tool a second time.
    result: Optional[str] = None

    def execute(self) -> str:
        """Run the tool now with the current ``arguments`` and remember the result."""
        self.result = self.runner(self.arguments)
        return self.result


ToolHook = Callable[[ToolStep], Optional[str]]


def system_prompt_for(config: BionicConfig, tool_schemas: list[dict[str, Any]]) -> str:
    """System prompt ``run_agent`` sends for ``config`` with these tool schemas."""
    plugin_names = [t["function"]["name"] for t in tool_schemas if t["function"]["name"] not in BUILTIN_TOOL_NAMES]
    return build_system_prompt(
        config.cwd,
        config.system_prompt,
        workspace_root=config.workspace_root or config.cwd,
        approval=config.approval,
        plugin_tools=plugin_names,
    )


def run_agent(
    prompt: str,
    config: BionicConfig,
    *,
    log: Optional[Callable[[str], None]] = None,
    approver: Optional[Callable[[str, dict[str, Any]], bool]] = None,
    on_event: Optional[Callable[[dict[str, Any]], None]] = None,
    messages: Optional[list[dict[str, Any]]] = None,
    before_tool: Optional["ToolHook"] = None,
) -> RunResult:
    """Run the OpenAI tool-calling loop until a final response or ``max_turns``.

    ``on_event`` receives progress events as dicts with a ``type`` key:
    ``turn``, ``token`` / ``reasoning`` (only when ``config.stream``),
    ``tool_call``, ``tool_result`` and ``done``.

    ``messages`` lets the caller own the conversation (the interactive REPL
    does): the list is extended in place with the prompt, assistant turns and
    tool results, its system message is refreshed, and ``config.session_id``
    is only used as the save target, never loaded over it.

    ``before_tool`` is called before each well-formed tool call with a
    :class:`ToolStep`; it may edit ``step.arguments``, run the tool itself
    via ``step.execute()``, or return a string to use as the tool result
    instead of executing. Returning ``None`` executes the tool normally.
    Raising :class:`RunAborted` stops the run.
    """

    def notify(event: dict[str, Any]) -> None:
        if on_event is not None:
            on_event(event)

    # Load plugins first so a broken plugin fails before any server traffic.
    registry = load_tool_registry(config)
    if config.auto_pull:
        report = ensure_model_ready(config, log=log)
        notify({"type": "health", "report": report})
        model = str(report["model"])
    else:
        model = resolve_model(config)
    tool_schemas = tools_for_approval(config.approval, registry)
    if log and len(registry):
        log(f"[plugins] loaded {len(registry)} tool(s) from {', '.join(registry.dirs)}: {', '.join(registry.tools)}")
    system_prompt = system_prompt_for(config, tool_schemas)
    chat_url = normalize_base_url(config.base_url) + "/chat/completions"

    active_session_id = config.session_id
    session_file: Optional[Path] = None
    caller_owns_messages = messages is not None

    if active_session_id is not None:
        session_file = _resolve_session_file(active_session_id, config.session_dir)
        if not caller_owns_messages:
            messages = _load_session_messages(session_file)
    elif config.emit_session_id:
        active_session_id = uuid.uuid4().hex
        session_file = _resolve_session_file(active_session_id, config.session_dir)

    if messages is None:
        messages = [{"role": "system", "content": system_prompt}]
    elif messages and messages[0].get("role") == "system":
        messages[0] = {"role": "system", "content": system_prompt}
    elif caller_owns_messages and not messages:
        messages.append({"role": "system", "content": system_prompt})
    messages.append({"role": "user", "content": prompt})

    total_tool_calls = 0
    usage_totals: dict[str, int] = {}
    turn_usage: list[Optional[dict[str, int]]] = []
    for turn in range(1, config.max_turns + 1):
        payload: dict[str, Any] = {
            "model": model,
            "messages": messages,
            "tools": tool_schemas,
            "tool_choice": "auto",
        }
        if config.temperature is not None:
            payload["temperature"] = config.temperature
        if config.max_tokens is not None:
            payload["max_tokens"] = config.max_tokens

        notify({"type": "turn", "turn": turn, "model": model})
        if config.stream:
            msg = _http_chat_stream(
                chat_url,
                payload=payload,
                api_key=config.api_key,
                timeout=config.timeout,
                emit=lambda kind, text, _turn=turn: notify({"type": kind, "turn": _turn, "text": text}),
            )
        else:
            resp = _http_json(
                chat_url,
                method="POST",
                payload=payload,
                api_key=config.api_key,
                timeout=config.timeout,
            )
            choices = resp.get("choices")
            if not isinstance(choices, list) or not choices:
                raise BionicError(f"Bionic API returned no choices: {json.dumps(resp)[:400]}")
            msg = choices[0].get("message")
            if not isinstance(msg, dict):
                raise BionicError(f"Bionic API choice missing 'message': {json.dumps(choices[0])[:400]}")
            if isinstance(resp.get("usage"), dict):
                msg = {**msg, "usage": resp["usage"]}

        usage = _usage_counts(msg.get("usage"))
        turn_usage.append(usage)
        for key, value in (usage or {}).items():
            usage_totals[key] = usage_totals.get(key, 0) + value

        cleaned = _normalize_assistant_message(msg)
        messages.append(cleaned)

        tool_calls = cleaned.get("tool_calls") or []
        if not tool_calls:
            final_text = str(cleaned.get("content") or "").strip()
            if not final_text and msg.get("reasoning_content"):
                final_text = str(msg["reasoning_content"]).strip()
            if session_file is not None:
                _save_session_messages(session_file, messages)
            notify({"type": "done", "turns": turn, "tool_calls": total_tool_calls})
            return RunResult(
                content=final_text,
                model=model,
                turns=turn,
                tool_calls=total_tool_calls,
                session_id=active_session_id,
                usage=usage_totals,
                turn_usage=turn_usage,
            )

        for idx, tc in enumerate(tool_calls):
            total_tool_calls += 1
            tc_id = str((tc.get("id") if isinstance(tc, dict) else None) or f"call_{turn}_{idx}")
            if isinstance(tc, dict) and not tc.get("id"):
                tc["id"] = tc_id  # keep assistant tool_calls and tool replies paired in history
            fn = tc.get("function") if isinstance(tc, dict) else None
            name = str(fn.get("name") or "") if isinstance(fn, dict) else ""
            if not isinstance(fn, dict):
                result = json.dumps({"error": "Malformed tool_call: missing 'function'"})
            else:
                raw_args = fn.get("arguments")
                notify(
                    {
                        "type": "tool_call",
                        "turn": turn,
                        "id": tc_id,
                        "name": name,
                        "arguments": raw_args if isinstance(raw_args, str) else json.dumps(raw_args),
                    }
                )
                try:
                    if raw_args is None or raw_args == "":
                        parsed_args: Any = {}
                    elif isinstance(raw_args, str):
                        parsed_args = json.loads(raw_args)
                    else:
                        parsed_args = raw_args

                    def _execute(args: Any, _name: str = name) -> str:
                        return exec_tool(
                            _name,
                            args,
                            config.cwd,
                            command_timeout=config.command_timeout,
                            max_output_chars=config.max_output_chars,
                            log=log,
                            workspace_root=config.workspace_root or config.cwd,
                            approval=config.approval,
                            approver=approver,
                            registry=registry,
                        )

                    override: Optional[str] = None
                    if before_tool is not None:
                        step = ToolStep(turn=turn, id=tc_id, name=name, arguments=parsed_args, runner=_execute)
                        override = before_tool(step)
                        if override is None and step.result is not None:
                            override = step.result  # the hook already ran it via step.execute()
                        parsed_args = step.arguments
                    result = override if override is not None else _execute(parsed_args)
                except RunAborted:
                    raise
                except Exception as exc:
                    result = json.dumps({"error": str(exc)})
            notify(
                {
                    "type": "tool_result",
                    "turn": turn,
                    "id": tc_id,
                    "name": name,
                    "error": _tool_result_error(result),
                    "chars": len(result),
                }
            )
            messages.append(
                {
                    "role": "tool",
                    "tool_call_id": tc_id,
                    "content": result,
                }
            )

    if session_file is not None:
        _save_session_messages(session_file, messages)
    raise MaxTurnsError(
        f"Reached max_turns={config.max_turns} without a final response",
        turns=config.max_turns,
        tool_calls=total_tool_calls,
        usage=usage_totals,
        turn_usage=turn_usage,
    )


Approver = Callable[[str, dict[str, Any]], bool]
EventHandler = Callable[[dict[str, Any]], None]

_CONFIG_FIELDS = frozenset(f.name for f in dataclasses.fields(BionicConfig))
_AGENT_OPTIONS = _CONFIG_FIELDS | {"system_prompt_file"}


class _StreamCancelled(BaseException):
    """Raised inside a worker thread to abort a run whose consumer went away.

    Derives from ``BaseException`` so the tool loop's ``except Exception``
    guards do not swallow it.
    """


def _config_from_options(options: dict[str, Any]) -> BionicConfig:
    unknown = sorted(set(options) - _AGENT_OPTIONS)
    if unknown:
        raise TypeError(f"unknown BionicAgent option(s): {', '.join(unknown)}")
    config = resolve_config(argparse.Namespace(**options))
    # resolve_config follows CLI semantics (stream_format implies stream); an
    # explicit SDK ``stream=`` value always wins.
    if "stream" in options:
        config.stream = bool(options["stream"])
    return config


class BionicAgent:
    """Programmatic client for running Bionic agent workflows from Python.

    Wraps :func:`run_agent` with validated configuration, cached model
    resolution, optional conversation continuity, and an event iterator::

        from aircd import BionicAgent

        agent = BionicAgent(model="auto", cwd="repo", approval="read-only", keep_session=True)
        print(agent.run("Summarize README.md").content)
        print(agent.run("Now list its section headings").content)  # same conversation

        for event in agent.stream("Explain src/main.rs"):
            if event["type"] == "token":
                print(event["text"], end="", flush=True)

    Options are the :class:`BionicConfig` field names (plus ``system_prompt_file``)
    and resolve exactly like the CLI: explicit arguments, then ``BIONIC_*``
    environment variables, then defaults. Pass a ready ``config=`` instead to
    skip resolution entirely.
    """

    def __init__(
        self,
        config: Optional[BionicConfig] = None,
        *,
        approver: Optional[Approver] = None,
        on_event: Optional[EventHandler] = None,
        log: Optional[Callable[[str], None]] = None,
        keep_session: bool = False,
        **options: Any,
    ) -> None:
        if config is not None and options:
            raise TypeError("pass either config= or keyword options, not both")
        self._config = dataclasses.replace(config) if config is not None else _config_from_options(options)
        self._approver = approver
        self._on_event = on_event
        self._log = log
        self._resolved_model: Optional[str] = None
        self.keep_session = keep_session
        self.session_id: Optional[str] = self._config.session_id

    @classmethod
    def from_env(cls, **kwargs: Any) -> "BionicAgent":
        """Build an agent purely from ``BIONIC_*`` environment variables (plus ``kwargs``)."""
        return cls(**kwargs)

    @property
    def config(self) -> BionicConfig:
        """A copy of the resolved configuration (mutating it does not affect the agent)."""
        return dataclasses.replace(self._config)

    @property
    def model(self) -> str:
        """The concrete model id; ``auto`` is discovered via ``/models`` once and cached."""
        if self._resolved_model is None:
            self._resolved_model = resolve_model(self._config)
        return self._resolved_model

    def with_options(self, **options: Any) -> "BionicAgent":
        """Return a new agent with some options changed, re-validated like the constructor.

        Handlers, ``keep_session`` and the current ``session_id`` carry over
        unless overridden (``keep_session=``/``session_id=`` are accepted).
        """
        keep_session = bool(options.pop("keep_session", self.keep_session))
        merged = dataclasses.asdict(self._config)
        merged["session_id"] = self.session_id
        if "system_prompt_file" in options:
            merged.pop("system_prompt", None)
        merged.update(options)
        clone = BionicAgent(
            _config_from_options(merged),
            approver=self._approver,
            on_event=self._on_event,
            log=self._log,
            keep_session=keep_session,
        )
        if self._resolved_model is not None and clone._config.model == self._config.model and (
            clone._config.base_url == self._config.base_url
        ):
            clone._resolved_model = self._resolved_model
        return clone

    def list_models(self, *, include_all: bool = False) -> list[dict[str, str]]:
        """List models served by the configured endpoint (embeddings excluded by default)."""
        return list_models(
            self._config.base_url,
            api_key=self._config.api_key,
            timeout=min(self._config.timeout, 30.0),
            include_all=include_all,
        )

    def health(self, *, probe: Optional[bool] = True, auto_pull: Optional[bool] = None) -> dict[str, Any]:
        """Check server and model readiness; see :func:`check_health`. Caches a resolved ``auto`` model."""
        report = check_health(self._config, probe=probe, auto_pull=auto_pull, log=self._log)
        if report["ok"] and report.get("model") and self._resolved_model is None:
            self._resolved_model = str(report["model"])
        return report

    def tools(self) -> list[dict[str, Any]]:
        """Describe the built-in and plugin tools a run would offer (same as ``--list-tools``).

        Raises :class:`aircd.bionic_plugins.PluginError` when a plugin is invalid.
        """
        return describe_tools(self._config)

    def new_session(self) -> None:
        """Forget the current conversation; the next ``keep_session`` run starts fresh."""
        self.session_id = None

    def _run_config(self, session_id: Optional[str], stream: Optional[bool]) -> BionicConfig:
        sid = session_id if session_id is not None else self.session_id
        return dataclasses.replace(
            self._config,
            model=self.model,
            session_id=sid,
            emit_session_id=bool(self._config.emit_session_id or self.keep_session or sid is not None),
            stream=self._config.stream if stream is None else bool(stream),
        )

    def run(
        self,
        prompt: str,
        *,
        session_id: Optional[str] = None,
        stream: Optional[bool] = None,
        on_event: Optional[EventHandler] = None,
        approver: Optional[Approver] = None,
    ) -> RunResult:
        """Run one prompt to completion and return its :class:`RunResult`.

        ``session_id`` resumes (or creates) that saved conversation for this
        call only. With ``keep_session=True`` the agent otherwise continues its
        own conversation across calls and updates :attr:`session_id`.
        ``on_event`` receives progress events in addition to the agent-wide
        handler; ``stream=True`` adds ``token``/``reasoning`` events.
        """
        if not isinstance(prompt, str) or not prompt.strip():
            raise BionicError("prompt must not be empty")
        config = self._run_config(session_id, stream)
        handlers = [h for h in (self._on_event, on_event) if h is not None]

        def dispatch(event: dict[str, Any]) -> None:
            for handler in handlers:
                handler(event)

        result = run_agent(
            prompt,
            config,
            log=self._log,
            approver=approver or self._approver,
            on_event=dispatch if handlers else None,
        )
        if self.keep_session and session_id is None:
            self.session_id = result.session_id
        return result

    def stream(
        self,
        prompt: str,
        *,
        session_id: Optional[str] = None,
        approver: Optional[Approver] = None,
    ) -> Iterator[dict[str, Any]]:
        """Run ``prompt`` with SSE streaming and yield progress events as they happen.

        Yields the same events as ``run(..., on_event=...)`` and finishes with
        ``{"type": "result", "result": RunResult}``. Errors are raised from the
        iterator. Closing the iterator early cancels the run at its next event.
        """
        if not isinstance(prompt, str) or not prompt.strip():
            raise BionicError("prompt must not be empty")
        self.model  # resolve (and surface errors) in the caller's thread
        events: "queue.Queue[tuple[str, Any]]" = queue.Queue()
        cancelled = threading.Event()

        def on_event(event: dict[str, Any]) -> None:
            if cancelled.is_set():
                raise _StreamCancelled()
            events.put(("event", event))

        def worker() -> None:
            try:
                result = self.run(prompt, session_id=session_id, stream=True, on_event=on_event, approver=approver)
            except _StreamCancelled:
                events.put(("cancelled", None))
            except BaseException as exc:  # re-raised in the consumer thread
                events.put(("error", exc))
            else:
                events.put(("result", result))

        thread = threading.Thread(target=worker, name="bionic-agent-stream", daemon=True)
        thread.start()
        try:
            while True:
                kind, value = events.get()
                if kind == "event":
                    yield value
                elif kind == "result":
                    yield {"type": "result", "result": value}
                    return
                elif kind == "error":
                    raise value
                else:
                    return
        finally:
            cancelled.set()


def build_parser() -> argparse.ArgumentParser:
    """Create the CLI argument parser for ``airc-bionic``."""
    parser = argparse.ArgumentParser(
        prog="airc-bionic",
        description="Function-calling agent runner for LM Studio Bionic and OpenAI-compatible servers.",
    )
    parser.add_argument("-p", "--prompt", help="Prompt to send to the agent (pass '-' to read stdin)")
    parser.add_argument(
        "--model",
        help="Model ID to run (or 'auto' to pick the first chat model; env: BIONIC_MODEL)",
    )
    parser.add_argument(
        "--base-url",
        help=f"OpenAI-compatible base URL (env: BIONIC_BASE_URL, default: {DEFAULT_BASE_URL})",
    )
    parser.add_argument(
        "--api-key",
        help="Bearer API key (env: BIONIC_API_KEY, default: lm-studio)",
    )
    parser.add_argument(
        "--cwd",
        help="Working directory for file and shell tools (env: BIONIC_CWD, default: current directory)",
    )
    parser.add_argument(
        "--workspace-root",
        help=(
            "Directory that file and command tools are jailed to; must contain --cwd "
            "(env: BIONIC_WORKSPACE_ROOT, default: the working directory)"
        ),
    )
    parser.add_argument(
        "--approval",
        choices=APPROVAL_MODES,
        help=(
            "Tool approval mode: auto runs jailed tools unattended, prompt asks on the "
            "terminal before each run_command/write_file/edit_file/apply_patch (denied when no terminal), "
            "read-only exposes only read_file (env: BIONIC_APPROVAL, default: auto)"
        ),
    )
    parser.add_argument(
        "--max-turns",
        type=int,
        help=f"Maximum LLM tool-calling turns (env: BIONIC_MAX_TURNS, default: {DEFAULT_MAX_TURNS})",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        help=f"HTTP request timeout in seconds (env: BIONIC_TIMEOUT, default: {int(DEFAULT_TIMEOUT_SECONDS)})",
    )
    parser.add_argument(
        "--command-timeout",
        type=float,
        help=(
            "Timeout in seconds per run_command tool call "
            f"(env: BIONIC_COMMAND_TIMEOUT, default: {int(DEFAULT_COMMAND_TIMEOUT_SECONDS)})"
        ),
    )
    parser.add_argument(
        "--max-output-chars",
        type=int,
        help=f"Maximum characters returned from read_file/run_command (default: {DEFAULT_MAX_OUTPUT_CHARS})",
    )
    parser.add_argument("--temperature", type=float, help="Sampling temperature (env: BIONIC_TEMPERATURE)")
    parser.add_argument("--max-tokens", type=int, help="Max completion tokens per turn (env: BIONIC_MAX_TOKENS)")
    parser.add_argument("--system-prompt", help="Override default system prompt (env: BIONIC_SYSTEM_PROMPT)")
    parser.add_argument("--system-prompt-file", help="Read system prompt override from a UTF-8 file")
    parser.add_argument("--session-id", help="Resume or create a saved local session by ID")
    parser.add_argument("--session-dir", help="Directory for saved sessions (env: BIONIC_SESSION_DIR)")
    parser.add_argument(
        "--emit-session-id",
        action="store_true",
        help="Emit `session_id: <id>` to stderr for aircd resume/pm session tracking",
    )
    parser.add_argument(
        "--list-models",
        action="store_true",
        help="Query /models on the Bionic server and print JSON for aircd model discovery",
    )
    parser.add_argument(
        "--all-models",
        action="store_true",
        help="Include embedding models when listing models with --list-models",
    )
    parser.add_argument(
        "--stream",
        action="store_true",
        help=(
            "Stream tokens and tool activity to stderr as they arrive via SSE; stdout is unchanged "
            "(env: BIONIC_STREAM)"
        ),
    )
    parser.add_argument(
        "--stream-format",
        choices=STREAM_FORMATS,
        help=(
            "Live feedback format: text (tokens + [tool] lines) or jsonl (one JSON event per line); "
            "implies --stream (env: BIONIC_STREAM_FORMAT, default: text)"
        ),
    )
    parser.add_argument(
        "--health",
        action="store_true",
        help=(
            "Check that the server is reachable and --model is available, loaded and answering; "
            "print a JSON report and exit 0 when ready, 2 when not"
        ),
    )
    parser.add_argument(
        "--no-probe",
        action="store_true",
        help="With --health, skip the one-token completion probe (report load state only)",
    )
    parser.add_argument(
        "--auto-pull",
        action="store_true",
        help=(
            "Before running (or with --health), download the model with `lms get` and load it with "
            "`lms load` when it is missing or not loaded, then confirm it answers (env: BIONIC_AUTO_PULL)"
        ),
    )
    parser.add_argument("--lms", dest="lms_path", help="Path to the LM Studio `lms` CLI (env: BIONIC_LMS)")
    parser.add_argument(
        "--pull-timeout",
        type=float,
        help=(
            "Timeout in seconds for each `lms get`/`lms load` "
            f"(env: BIONIC_PULL_TIMEOUT, default: {int(DEFAULT_PULL_TIMEOUT_SECONDS)})"
        ),
    )
    parser.add_argument(
        "--tools-dir",
        dest="tools_dirs",
        action="append",
        metavar="DIR",
        help=(
            "Load user-defined tool plugins (*.py) from DIR in addition to <workspace-root>/.aircd/tools; "
            f"repeatable (env: BIONIC_TOOLS_DIR, {os.pathsep!r}-separated)"
        ),
    )
    parser.add_argument(
        "--no-plugins",
        action="store_true",
        help="Do not load any tool plugins, including .aircd/tools (env: BIONIC_NO_PLUGINS)",
    )
    parser.add_argument(
        "--list-tools",
        action="store_true",
        help="Validate plugins and print the tools the model would be offered as JSON, then exit",
    )
    parser.add_argument(
        "--repl",
        action="store_true",
        help=(
            "Start an interactive multi-turn session: chat with the agent, inspect the context, "
            "run tools by hand and step through tool calls (type /help inside)"
        ),
    )
    parser.add_argument(
        "--debug",
        action="store_true",
        help="Start the REPL in step mode: pause before every tool call to run, skip, edit or fake it (implies --repl)",
    )
    parser.add_argument("--json", action="store_true", help="Print structured JSON result to stdout")
    parser.add_argument("-v", "--verbose", action="store_true", help="Log tool calls to stderr")
    return parser


def main(argv: Optional[list[str]] = None) -> int:
    """CLI entrypoint for ``airc-bionic``."""
    parser = build_parser()
    args = parser.parse_args(argv)

    try:
        config = resolve_config(args)
        if args.list_models:
            models = list_models(
                config.base_url,
                api_key=config.api_key,
                timeout=min(config.timeout, 30.0),
                include_all=args.all_models,
            )
            print(json.dumps({"data": models}))
            return 0

        if args.list_tools:
            registry = load_tool_registry(config)
            print(json.dumps({"dirs": registry.dirs, "tools": describe_tools(config, registry)}))
            return 0

        if args.health:
            log = (lambda msg: print(msg, file=sys.stderr)) if config.verbose else None
            report = check_health(config, probe=not args.no_probe, auto_pull=config.auto_pull, log=log)
            print(json.dumps(report))
            if not report["ok"]:
                print(f"error: {report['error']}", file=sys.stderr)
                return 2
            return 0

        if args.repl or args.debug:
            from aircd.bionic_repl import run_repl

            prompt = args.prompt
            if prompt == "-":
                prompt = sys.stdin.read()
            return run_repl(config, step=args.debug, initial_prompt=prompt)

        if not args.prompt:
            parser.error(
                "-p/--prompt is required unless --repl, --list-models, --list-tools or --health is specified"
            )

        prompt = sys.stdin.read() if args.prompt == "-" else args.prompt
        if not prompt.strip():
            raise BionicError("prompt must not be empty")

        log = (lambda msg: print(msg, file=sys.stderr)) if config.verbose else None
        on_event = StreamRenderer(config.stream_format) if config.stream else None
        result = run_agent(prompt, config, log=log, on_event=on_event)
        if config.emit_session_id and result.session_id:
            print(f"session_id: {result.session_id}", file=sys.stderr)
        if args.json:
            print(
                json.dumps(
                    {
                        "ok": True,
                        "model": result.model,
                        "turns": result.turns,
                        "tool_calls": result.tool_calls,
                        "session_id": result.session_id,
                        "usage": result.usage,
                        "content": result.content,
                    }
                )
            )
        else:
            print(result.content)
        return 0
    except BionicError as err:
        print(f"error: {err}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    # ``python -m aircd.bionic`` runs this file as ``__main__`` while
    # ``aircd.bionic_plugins`` imports ``aircd.bionic``; run the package copy so
    # both share one set of classes (BionicError, WorkspaceViolation, ...).
    from aircd.bionic import main as _package_main

    raise SystemExit(_package_main())
