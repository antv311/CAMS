"""Benchmark and multi-turn evaluation suite for the Bionic agent runner.

``airc-bionic-bench`` drives :func:`aircd.bionic.run_agent` through a set of
scripted scenarios against a local OpenAI-compatible server (LM Studio Bionic by
default). Each scenario gets a fresh temporary workspace seeded with files, sends
one or more prompts in a single saved session (so later prompts can rely on
earlier turns), then checks the result deterministically: the final answer, the
files the model left behind, or a command such as a unit test.

For every run it records:

* **latency**: wall time per prompt, per-LLM-call latency, time to first token
  (``--stream`` only) and per-tool-call execution time;
* **tokens**: prompt / completion totals from the server's ``usage`` objects,
  per-turn prompt growth, completion tokens per second, and tokens spent per
  passed scenario (token efficiency);
* **memory**: resident memory of the benchmark process and, with
  ``--server-pid``, of the inference server process(es), sampled in the
  background (start, peak, mean, end). GPU memory is not visible this way.

Output is one JSON report on stdout (or ``--output``) and a readable summary on
stderr. Exit codes: 0 ok, 1 configuration error, 2 model not ready, 3 pass rate
below ``--min-pass-rate``.

Zero third-party dependencies, like the runner itself.
"""

from __future__ import annotations

import argparse
import dataclasses
import json
import os
import platform
import shlex
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Optional

from aircd import bionic
from aircd.bionic import BionicConfig, BionicError, RunResult

REPORT_VERSION = 1
DEFAULT_SAMPLE_INTERVAL = 0.25
CHECK_TYPES = ("answer_contains", "answer_matches_any", "file_equals", "file_contains", "file_absent", "command")


# --- Scenarios ---------------------------------------------------------------------------------


@dataclass
class Scenario:
    """A scripted evaluation: seed files, prompts (one session), and checks."""

    name: str
    prompts: list[str]
    checks: list[dict[str, Any]]
    description: str = ""
    files: dict[str, str] = field(default_factory=dict)
    tags: list[str] = field(default_factory=list)

    @classmethod
    def from_dict(cls, raw: Any) -> "Scenario":
        if not isinstance(raw, dict):
            raise BionicError("scenario must be a JSON object")
        name = raw.get("name")
        if not isinstance(name, str) or not name.strip():
            raise BionicError("scenario needs a non-empty 'name'")
        prompts = raw.get("prompts")
        if isinstance(raw.get("prompt"), str):
            prompts = [raw["prompt"]]
        if not isinstance(prompts, list) or not prompts or not all(isinstance(p, str) and p.strip() for p in prompts):
            raise BionicError(f"scenario {name!r}: 'prompts' must be a non-empty list of strings")
        checks = raw.get("checks")
        if not isinstance(checks, list) or not checks:
            raise BionicError(f"scenario {name!r}: 'checks' must be a non-empty list")
        for check in checks:
            if not isinstance(check, dict) or check.get("type") not in CHECK_TYPES:
                raise BionicError(f"scenario {name!r}: each check needs a 'type' in {', '.join(CHECK_TYPES)}")
        files = raw.get("files") or {}
        if not isinstance(files, dict) or not all(isinstance(k, str) and isinstance(v, str) for k, v in files.items()):
            raise BionicError(f"scenario {name!r}: 'files' must map relative paths to text")
        tags = raw.get("tags") or []
        return cls(
            name=name.strip(),
            prompts=list(prompts),
            checks=list(checks),
            description=str(raw.get("description") or ""),
            files=dict(files),
            tags=[str(t) for t in tags] if isinstance(tags, list) else [],
        )


BUILTIN_SCENARIOS: list[Scenario] = [
    Scenario(
        name="read-answer",
        description="Read one file and report a value from it (1 tool call expected).",
        tags=["single-turn", "read"],
        files={"secret.txt": "The deployment code is ZEBRA-4417.\n"},
        prompts=["Read secret.txt and reply with only the deployment code it contains."],
        checks=[{"type": "answer_contains", "text": "ZEBRA-4417"}],
    ),
    Scenario(
        name="write-file",
        description="Create a file with exact contents.",
        tags=["single-turn", "write"],
        prompts=["Create a file named hello.txt whose only line is exactly: bionic bench ok"],
        checks=[{"type": "file_equals", "path": "hello.txt", "text": "bionic bench ok"}],
    ),
    Scenario(
        name="edit-config",
        description="Change one value in an existing file without touching the rest.",
        tags=["single-turn", "edit"],
        files={"config.ini": "[server]\nhost = 127.0.0.1\nport = 8080\nworkers = 4\n"},
        prompts=["In config.ini change the port to 9090. Do not change anything else in the file."],
        checks=[
            {
                "type": "file_equals",
                "path": "config.ini",
                "text": "[server]\nhost = 127.0.0.1\nport = 9090\nworkers = 4",
            }
        ],
    ),
    Scenario(
        name="command-output",
        description="Run a script and report what it prints.",
        tags=["single-turn", "command"],
        files={"compute.py": "print(sum(i * i for i in range(1, 21)))\n"},
        prompts=["Run the command `{python} compute.py` and reply with only the number it prints."],
        checks=[{"type": "answer_contains", "text": "2870"}],
    ),
    Scenario(
        name="aggregate-files",
        description="Read several files, combine their data, and write the result.",
        tags=["multi-step", "read", "write"],
        files={
            "data/a.txt": "12\n7\n",
            "data/b.txt": "30\n",
            "data/c.txt": "1\n2\n3\n",
            "data/readme.md": "Ignore this file: 1000\n",
        },
        prompts=[
            "Add up every number in the .txt files inside the data directory (ignore other file types) "
            "and write only the total to total.txt."
        ],
        checks=[{"type": "file_equals", "path": "total.txt", "text": "55"}],
    ),
    Scenario(
        name="fix-and-test",
        description="Find and fix a bug, then confirm with the unit tests (edit + command loop).",
        tags=["multi-step", "edit", "command"],
        files={
            "mathutils.py": (
                "def add(a, b):\n    return a - b\n\n\ndef mul(a, b):\n    return a * b\n"
            ),
            "test_mathutils.py": (
                "import unittest\n\nfrom mathutils import add, mul\n\n\n"
                "class MathTests(unittest.TestCase):\n"
                "    def test_add(self):\n        self.assertEqual(add(2, 3), 5)\n\n"
                "    def test_mul(self):\n        self.assertEqual(mul(4, 5), 20)\n\n\n"
                "if __name__ == '__main__':\n    unittest.main()\n"
            ),
        },
        prompts=[
            "The tests in test_mathutils.py fail. Fix the bug in mathutils.py (do not edit the test file), "
            "run `{python} -m unittest test_mathutils` to confirm they pass, then reply DONE."
        ],
        checks=[
            {"type": "command", "command": "{python} -m unittest test_mathutils"},
            {
                "type": "file_contains",
                "path": "test_mathutils.py",
                "text": "self.assertEqual(add(2, 3), 5)",
            },
        ],
    ),
    Scenario(
        name="session-recall",
        description="Multi-turn: a fact given in prompt 1 must be used in prompt 2 of the same session.",
        tags=["multi-turn", "memory", "write"],
        prompts=[
            "Remember this code word for later: AMBER-62. Do not call any tools; reply only OK.",
            "Write the code word I gave you earlier into codeword.txt, with nothing else in the file.",
        ],
        checks=[{"type": "file_equals", "path": "codeword.txt", "text": "AMBER-62"}],
    ),
    Scenario(
        name="session-followup",
        description="Multi-turn: prompt 2 refers to a file created in prompt 1 without naming it.",
        tags=["multi-turn", "write", "edit"],
        prompts=[
            "Create notes.md containing a single line: # Release notes",
            "Append a new last line '- fixed login timeout' to the file you just created.",
            "How many lines does that file have now? Reply with only the number.",
        ],
        checks=[
            {"type": "file_equals", "path": "notes.md", "text": "# Release notes\n- fixed login timeout"},
            {"type": "answer_matches_any", "texts": ["2", "two"]},
        ],
    ),
]


def builtin_scenarios() -> list[Scenario]:
    return [dataclasses.replace(s, files=dict(s.files), checks=[dict(c) for c in s.checks]) for s in BUILTIN_SCENARIOS]


def load_scenarios_file(path: str) -> list[Scenario]:
    try:
        raw = json.loads(Path(path).read_text(encoding="utf-8"))
    except OSError as err:
        raise BionicError(f"cannot read scenarios file {path}: {err}") from err
    except json.JSONDecodeError as err:
        raise BionicError(f"scenarios file {path} is not valid JSON: {err}") from err
    if isinstance(raw, dict):
        raw = raw.get("scenarios")
    if not isinstance(raw, list) or not raw:
        raise BionicError(f"scenarios file {path} must be a JSON list (or {{\"scenarios\": [...]}})")
    return [Scenario.from_dict(item) for item in raw]


def select_scenarios(scenarios: list[Scenario], names: Optional[list[str]], tags: Optional[list[str]]) -> list[Scenario]:
    chosen = scenarios
    if names:
        known = {s.name: s for s in scenarios}
        missing = [n for n in names if n not in known]
        if missing:
            raise BionicError(f"unknown scenario(s): {', '.join(missing)} (known: {', '.join(known)})")
        chosen = [known[n] for n in names]
    if tags:
        chosen = [s for s in chosen if set(tags) & set(s.tags)]
    if not chosen:
        raise BionicError("no scenarios selected")
    names_seen: set[str] = set()
    for s in chosen:
        if s.name in names_seen:
            raise BionicError(f"duplicate scenario name: {s.name}")
        names_seen.add(s.name)
    return chosen


# --- Placeholders, workspace, checks -----------------------------------------------------------


def python_command() -> str:
    """The current interpreter as a shell-ready token (quoted when it has spaces)."""
    exe = sys.executable or "python"
    if os.name == "nt":
        return f'"{exe}"' if " " in exe else exe
    return shlex.quote(exe)


def expand(text: str) -> str:
    return text.replace("{python}", python_command())


def _norm(text: str) -> str:
    return "\n".join(line.rstrip() for line in text.replace("\r\n", "\n").strip().split("\n"))


def seed_workspace(root: Path, files: dict[str, str]) -> None:
    for rel, content in files.items():
        target = (root / rel).resolve()
        if not bionic.is_within(target, root):
            raise BionicError(f"scenario file escapes the workspace: {rel}")
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content, encoding="utf-8", newline="")


def run_check(check: dict[str, Any], root: Path, answers: list[str], *, timeout: float = 120.0) -> dict[str, Any]:
    """Evaluate one check; returns ``{"type", "ok", "detail"}``."""
    kind = check.get("type")
    out: dict[str, Any] = {"type": kind, "ok": False, "detail": ""}
    idx = check.get("prompt", -1)
    answer = ""
    if answers:
        try:
            answer = answers[int(idx)]
        except (IndexError, ValueError, TypeError):
            out["detail"] = f"no answer for prompt index {idx}"
            return out

    if kind in ("answer_contains", "answer_matches_any"):
        wanted = [str(check["text"])] if kind == "answer_contains" else [str(t) for t in check.get("texts") or []]
        hay = answer if check.get("case_sensitive") else answer.lower()
        if kind == "answer_matches_any":
            # exact token match after stripping punctuation, e.g. "2." or "**2**"
            hay_norm = hay.strip().strip("*_`.'\" \n").lower()
            out["ok"] = any(hay_norm == w.lower() for w in wanted) or any(
                w.lower() in hay.split() for w in wanted
            )
        else:
            out["ok"] = any((w if check.get("case_sensitive") else w.lower()) in hay for w in wanted)
        out["detail"] = "" if out["ok"] else f"answer {answer[:200]!r} lacks {wanted!r}"
        return out

    if kind in ("file_equals", "file_contains", "file_absent"):
        rel = str(check.get("path") or "")
        target = (root / rel).resolve()
        if not rel or not bionic.is_within(target, root):
            out["detail"] = f"bad check path {rel!r}"
            return out
        if kind == "file_absent":
            out["ok"] = not target.exists()
            out["detail"] = "" if out["ok"] else f"{rel} exists"
            return out
        if not target.is_file():
            out["detail"] = f"{rel} was not created"
            return out
        text = target.read_text(encoding="utf-8", errors="replace")
        expected = str(check.get("text") or "")
        out["ok"] = _norm(text) == _norm(expected) if kind == "file_equals" else expected in text
        out["detail"] = "" if out["ok"] else f"{rel} is {text[:200]!r}"
        return out

    if kind == "command":
        command = expand(str(check.get("command") or ""))
        expect = int(check.get("expect_exit", 0))
        # A fresh bytecode cache: an edit that keeps a module's size within the
        # same second as an earlier import would otherwise run the stale .pyc.
        cache = tempfile.mkdtemp(prefix="bionic-bench-pyc-")
        env = {**os.environ, "PYTHONPYCACHEPREFIX": cache, "PYTHONDONTWRITEBYTECODE": "1"}
        try:
            proc = subprocess.run(
                command,
                shell=True,
                cwd=str(root),
                env=env,
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                timeout=float(check.get("timeout", timeout)),
            )
        except subprocess.TimeoutExpired:
            out["detail"] = f"check command timed out: {command}"
            return out
        finally:
            _rmtree(cache)
        out["ok"] = proc.returncode == expect
        if not out["ok"]:
            tail = (proc.stdout + proc.stderr).strip()[-400:]
            out["detail"] = f"exit {proc.returncode} (want {expect}): {tail}"
        return out

    out["detail"] = f"unknown check type {kind!r}"
    return out


# --- Memory sampling ---------------------------------------------------------------------------


def _windows_rss(pid: Optional[int]) -> Optional[int]:
    import ctypes
    from ctypes import wintypes

    class PROCESS_MEMORY_COUNTERS(ctypes.Structure):
        _fields_ = [
            ("cb", wintypes.DWORD),
            ("PageFaultCount", wintypes.DWORD),
            ("PeakWorkingSetSize", ctypes.c_size_t),
            ("WorkingSetSize", ctypes.c_size_t),
            ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
            ("QuotaPagedPoolUsage", ctypes.c_size_t),
            ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
            ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
            ("PagefileUsage", ctypes.c_size_t),
            ("PeakPagefileUsage", ctypes.c_size_t),
        ]

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.OpenProcess.restype = wintypes.HANDLE
    kernel32.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    kernel32.GetCurrentProcess.restype = wintypes.HANDLE
    kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
    kernel32.K32GetProcessMemoryInfo.argtypes = [wintypes.HANDLE, ctypes.c_void_p, wintypes.DWORD]
    kernel32.K32GetProcessMemoryInfo.restype = wintypes.BOOL

    own = pid is None or pid == os.getpid()
    if own:
        handle = kernel32.GetCurrentProcess()
    else:
        handle = kernel32.OpenProcess(0x1000 | 0x0010, False, int(pid))  # QUERY_LIMITED_INFORMATION | VM_READ
        if not handle:
            handle = kernel32.OpenProcess(0x1000, False, int(pid))
        if not handle:
            return None
    try:
        counters = PROCESS_MEMORY_COUNTERS()
        counters.cb = ctypes.sizeof(counters)
        if not kernel32.K32GetProcessMemoryInfo(handle, ctypes.byref(counters), counters.cb):
            return None
        return int(counters.WorkingSetSize)
    finally:
        if not own:
            kernel32.CloseHandle(handle)


def process_rss(pid: Optional[int] = None) -> Optional[int]:
    """Resident memory in bytes of ``pid`` (default: this process), or ``None`` if unavailable."""
    try:
        if os.name == "nt":
            return _windows_rss(pid)
        status = Path(f"/proc/{pid or os.getpid()}/status")
        if status.exists():
            for line in status.read_text().splitlines():
                if line.startswith("VmRSS:"):
                    return int(line.split()[1]) * 1024
            return None
        if pid is None or pid == os.getpid():
            import resource

            peak = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
            return int(peak) if sys.platform == "darwin" else int(peak) * 1024
    except Exception:
        return None
    return None


def _mb(value: Optional[float]) -> Optional[float]:
    return None if value is None else round(value / (1024 * 1024), 1)


class MemorySampler:
    """Background sampler of resident memory for this process and optional server PIDs."""

    def __init__(
        self,
        server_pids: Optional[list[int]] = None,
        *,
        interval: float = DEFAULT_SAMPLE_INTERVAL,
        reader: Callable[[Optional[int]], Optional[int]] = process_rss,
    ) -> None:
        self.targets: dict[str, Optional[int]] = {"runner": None}
        for pid in server_pids or []:
            self.targets[f"server:{pid}"] = pid
        self.interval = interval
        self.reader = reader
        self._samples: dict[str, list[int]] = {}
        self._stop = threading.Event()
        self._thread: Optional[threading.Thread] = None
        self._lock = threading.Lock()

    def _sample_once(self) -> None:
        for label, pid in self.targets.items():
            value = self.reader(pid)
            if value is not None:
                with self._lock:
                    self._samples.setdefault(label, []).append(value)

    def _loop(self) -> None:
        while not self._stop.wait(self.interval):
            self._sample_once()

    def start(self) -> "MemorySampler":
        self._samples = {}
        self._stop.clear()
        self._sample_once()
        self._thread = threading.Thread(target=self._loop, name="bionic-bench-mem", daemon=True)
        self._thread.start()
        return self

    def stop(self) -> dict[str, Any]:
        self._stop.set()
        if self._thread is not None:
            self._thread.join(timeout=max(1.0, self.interval * 4))
        self._sample_once()
        out: dict[str, Any] = {}
        with self._lock:
            for label in self.targets:
                values = self._samples.get(label) or []
                if not values:
                    out[label] = None
                    continue
                out[label] = {
                    "start_mb": _mb(values[0]),
                    "peak_mb": _mb(max(values)),
                    "mean_mb": _mb(statistics.fmean(values)),
                    "end_mb": _mb(values[-1]),
                    "delta_mb": _mb(values[-1] - values[0]),
                    "samples": len(values),
                }
        return out


# --- Timing from run_agent events --------------------------------------------------------------


class EventTimer:
    """Turn ``run_agent`` progress events into LLM-call, first-token and tool latencies.

    An LLM call starts at a ``turn`` event and ends at the next ``tool_call`` or
    ``done`` event. A tool call runs from its ``tool_call`` event to the matching
    ``tool_result``.
    """

    def __init__(self, clock: Callable[[], float] = time.perf_counter) -> None:
        self.clock = clock
        self.llm_ms: list[float] = []
        self.ttft_ms: list[float] = []
        self.tool_ms: list[dict[str, Any]] = []
        self.turns = 0
        self.tool_calls = 0
        self._turn_start: Optional[float] = None
        self._first_token_seen = False
        self._tool_start: dict[str, float] = {}

    def _close_turn(self, now: float) -> None:
        if self._turn_start is not None:
            self.llm_ms.append((now - self._turn_start) * 1000.0)
            self._turn_start = None

    def __call__(self, event: dict[str, Any]) -> None:
        now = self.clock()
        kind = event.get("type")
        if kind == "turn":
            self._close_turn(now)
            self.turns += 1
            self._turn_start = now
            self._first_token_seen = False
        elif kind in ("token", "reasoning"):
            if self._turn_start is not None and not self._first_token_seen:
                self.ttft_ms.append((now - self._turn_start) * 1000.0)
                self._first_token_seen = True
        elif kind == "tool_call":
            self._close_turn(now)
            self.tool_calls += 1
            self._tool_start[str(event.get("id"))] = now
        elif kind == "tool_result":
            started = self._tool_start.pop(str(event.get("id")), None)
            if started is not None:
                self.tool_ms.append(
                    {
                        "name": event.get("name"),
                        "ms": round((now - started) * 1000.0, 2),
                        "error": event.get("error"),
                    }
                )
        elif kind == "done":
            self._close_turn(now)


# --- Statistics --------------------------------------------------------------------------------


def percentile(values: list[float], pct: float) -> Optional[float]:
    """Linear-interpolated percentile (``pct`` in 0..100); ``None`` for no data."""
    if not values:
        return None
    ordered = sorted(values)
    if len(ordered) == 1:
        return ordered[0]
    rank = (len(ordered) - 1) * pct / 100.0
    lo = int(rank)
    hi = min(lo + 1, len(ordered) - 1)
    return ordered[lo] + (ordered[hi] - ordered[lo]) * (rank - lo)


def describe(values: list[float]) -> dict[str, Any]:
    if not values:
        return {"count": 0, "mean": None, "p50": None, "p95": None, "min": None, "max": None}
    r = lambda v: None if v is None else round(v, 2)  # noqa: E731
    return {
        "count": len(values),
        "mean": r(statistics.fmean(values)),
        "p50": r(percentile(values, 50)),
        "p95": r(percentile(values, 95)),
        "min": r(min(values)),
        "max": r(max(values)),
    }


# --- Running -----------------------------------------------------------------------------------


def run_scenario(
    scenario: Scenario,
    config: BionicConfig,
    *,
    iteration: int = 1,
    sampler_factory: Optional[Callable[[], MemorySampler]] = None,
    keep_workspace: bool = False,
    log: Optional[Callable[[str], None]] = None,
    runner: Callable[..., RunResult] = bionic.run_agent,
) -> dict[str, Any]:
    """Run one scenario end to end and return its JSON-serializable result."""
    tmp = tempfile.mkdtemp(prefix=f"bionic-bench-{scenario.name}-")
    root = Path(tmp).resolve() / "workspace"
    root.mkdir()
    seed_workspace(root, scenario.files)
    run_config = dataclasses.replace(
        config,
        cwd=str(root),
        workspace_root=str(root),
        session_id=f"bench-{uuid.uuid4().hex[:12]}",
        session_dir=str(Path(tmp) / "sessions"),
        emit_session_id=False,
        auto_pull=False,
    )

    timer = EventTimer()
    sampler = sampler_factory() if sampler_factory else None
    prompts_out: list[dict[str, Any]] = []
    answers: list[str] = []
    error: Optional[str] = None
    if sampler is not None:
        sampler.start()
    # Keep the model's own `python` runs from caching bytecode in the workspace
    # (same stale-.pyc hazard as in run_check); restored afterwards.
    saved_bytecode_flag = os.environ.get("PYTHONDONTWRITEBYTECODE")
    os.environ["PYTHONDONTWRITEBYTECODE"] = "1"
    started = time.perf_counter()
    try:
        for idx, prompt in enumerate(scenario.prompts):
            text = expand(prompt)
            t0 = time.perf_counter()
            try:
                result = runner(text, run_config, log=log, on_event=timer)
            except Exception as err:  # a crash in one scenario must not abort the suite
                error = f"prompt {idx + 1}: {err}"
                failed: dict[str, Any] = {
                    "index": idx,
                    "ok": False,
                    "wall_ms": round((time.perf_counter() - t0) * 1000, 2),
                    "error": str(err),
                }
                if isinstance(err, bionic.MaxTurnsError):
                    # count the tokens a run burned before giving up
                    failed.update(
                        turns=err.turns, tool_calls=err.tool_calls, usage=err.usage, turn_usage=err.turn_usage
                    )
                prompts_out.append(failed)
                break
            answers.append(result.content)
            prompts_out.append(
                {
                    "index": idx,
                    "ok": True,
                    "wall_ms": round((time.perf_counter() - t0) * 1000, 2),
                    "turns": result.turns,
                    "tool_calls": result.tool_calls,
                    "usage": result.usage,
                    "turn_usage": result.turn_usage,
                    "answer": result.content[:500],
                }
            )
    finally:
        wall_ms = (time.perf_counter() - started) * 1000.0
        if saved_bytecode_flag is None:
            os.environ.pop("PYTHONDONTWRITEBYTECODE", None)
        else:
            os.environ["PYTHONDONTWRITEBYTECODE"] = saved_bytecode_flag
        memory = sampler.stop() if sampler is not None else None

    checks: list[dict[str, Any]] = []
    if error is None:
        checks = [run_check(c, root, answers, timeout=config.command_timeout) for c in scenario.checks]
    passed = error is None and all(c["ok"] for c in checks)

    usage = {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}
    usage_reported = False
    prompt_growth: list[int] = []
    for p in prompts_out:
        for key, value in (p.get("usage") or {}).items():
            usage[key] = usage.get(key, 0) + value
            usage_reported = True
        prompt_growth.extend(u["prompt_tokens"] for u in p.get("turn_usage") or [] if u and "prompt_tokens" in u)
    llm_s = sum(timer.llm_ms) / 1000.0
    tokens_per_s = round(usage["completion_tokens"] / llm_s, 2) if usage_reported and llm_s > 0 else None

    out = {
        "scenario": scenario.name,
        "iteration": iteration,
        "tags": scenario.tags,
        "passed": passed,
        "error": error,
        "checks": checks,
        "prompts": prompts_out,
        # from events, so a failed prompt's partial work is counted too
        "turns": max(timer.turns, sum(p.get("turns", 0) for p in prompts_out)),
        "tool_calls": max(timer.tool_calls, sum(p.get("tool_calls", 0) for p in prompts_out)),
        "tool_errors": sum(1 for t in timer.tool_ms if t["error"]),
        "latency": {
            "wall_ms": round(wall_ms, 2),
            "llm_ms": [round(v, 2) for v in timer.llm_ms],
            "ttft_ms": [round(v, 2) for v in timer.ttft_ms],
            "tools": timer.tool_ms,
        },
        "tokens": {
            **(usage if usage_reported else {k: None for k in usage}),
            "reported": usage_reported,
            "prompt_tokens_per_turn": prompt_growth,
            "completion_tokens_per_s": tokens_per_s,
        },
        "memory": memory,
        "workspace": str(root) if keep_workspace else None,
    }
    if not keep_workspace:
        _rmtree(tmp)
    return out


def _rmtree(path: str) -> None:
    import shutil

    shutil.rmtree(path, ignore_errors=True)


def summarize(runs: list[dict[str, Any]]) -> dict[str, Any]:
    """Aggregate per-run results into suite-level metrics."""
    passed = [r for r in runs if r["passed"]]
    llm = [v for r in runs for v in r["latency"]["llm_ms"]]
    ttft = [v for r in runs for v in r["latency"]["ttft_ms"]]
    tool = [t["ms"] for r in runs for t in r["latency"]["tools"]]
    wall = [r["latency"]["wall_ms"] for r in runs]
    reported = [r for r in runs if r["tokens"]["reported"]]
    total_tokens = sum(r["tokens"]["total_tokens"] or 0 for r in reported)
    passed_reported = [r for r in passed if r["tokens"]["reported"]]
    tps = [r["tokens"]["completion_tokens_per_s"] for r in reported if r["tokens"]["completion_tokens_per_s"]]

    per_tool: dict[str, list[float]] = {}
    for r in runs:
        for t in r["latency"]["tools"]:
            per_tool.setdefault(str(t["name"]), []).append(t["ms"])

    def mem_peak(label_prefix: str) -> Optional[float]:
        peaks = [
            m["peak_mb"]
            for r in runs
            for label, m in (r.get("memory") or {}).items()
            if m and label.startswith(label_prefix)
        ]
        return max(peaks) if peaks else None

    by_scenario: dict[str, dict[str, Any]] = {}
    for r in runs:
        entry = by_scenario.setdefault(r["scenario"], {"runs": 0, "passed": 0, "wall_ms": [], "total_tokens": []})
        entry["runs"] += 1
        entry["passed"] += 1 if r["passed"] else 0
        entry["wall_ms"].append(r["latency"]["wall_ms"])
        if r["tokens"]["reported"]:
            entry["total_tokens"].append(r["tokens"]["total_tokens"])
    for entry in by_scenario.values():
        entry["pass_rate"] = round(entry["passed"] / entry["runs"], 3)
        entry["wall_ms_mean"] = round(statistics.fmean(entry.pop("wall_ms")), 2)
        toks = entry.pop("total_tokens")
        entry["total_tokens_mean"] = round(statistics.fmean(toks), 1) if toks else None

    return {
        "runs": len(runs),
        "passed": len(passed),
        "pass_rate": round(len(passed) / len(runs), 3) if runs else 0.0,
        "turns_mean": round(statistics.fmean(r["turns"] for r in runs), 2) if runs else None,
        "tool_calls_mean": round(statistics.fmean(r["tool_calls"] for r in runs), 2) if runs else None,
        "tool_errors": sum(r["tool_errors"] for r in runs),
        "latency_ms": {
            "scenario_wall": describe(wall),
            "llm_call": describe(llm),
            "first_token": describe(ttft),
            "tool_exec": describe(tool),
            "per_tool": {name: describe(vals) for name, vals in sorted(per_tool.items())},
        },
        "tokens": {
            "reported_runs": len(reported),
            "prompt_tokens": sum(r["tokens"]["prompt_tokens"] or 0 for r in reported) if reported else None,
            "completion_tokens": sum(r["tokens"]["completion_tokens"] or 0 for r in reported) if reported else None,
            "total_tokens": total_tokens if reported else None,
            "tokens_per_run_mean": round(total_tokens / len(reported), 1) if reported else None,
            # all tokens spent (failed runs included) divided by successes: lower is better
            "tokens_per_pass": round(total_tokens / len(passed_reported), 1) if passed_reported else None,
            "completion_tokens_per_s": describe(tps),
        },
        "memory_mb": {"runner_peak": mem_peak("runner"), "server_peak": mem_peak("server:")},
        "by_scenario": by_scenario,
    }


def run_suite(
    scenarios: list[Scenario],
    config: BionicConfig,
    *,
    repeat: int = 1,
    server_pids: Optional[list[int]] = None,
    sample_interval: float = DEFAULT_SAMPLE_INTERVAL,
    keep_workspace: bool = False,
    progress: Optional[Callable[[str], None]] = None,
    log: Optional[Callable[[str], None]] = None,
    runner: Callable[..., RunResult] = bionic.run_agent,
    health: Optional[dict[str, Any]] = None,
) -> dict[str, Any]:
    """Run ``scenarios`` ``repeat`` times each and return the full report."""
    say = progress or (lambda _m: None)

    def sampler_factory() -> MemorySampler:
        return MemorySampler(server_pids, interval=sample_interval)

    runs: list[dict[str, Any]] = []
    started = time.time()
    total = len(scenarios) * repeat
    for iteration in range(1, repeat + 1):
        for scenario in scenarios:
            say(f"[{len(runs) + 1}/{total}] {scenario.name} (iteration {iteration}) ...")
            result = run_scenario(
                scenario,
                config,
                iteration=iteration,
                sampler_factory=sampler_factory,
                keep_workspace=keep_workspace,
                log=log,
                runner=runner,
            )
            runs.append(result)
            status = "PASS" if result["passed"] else "FAIL"
            detail = result["error"] or "; ".join(c["detail"] for c in result["checks"] if not c["ok"])
            say(
                f"    {status} in {result['latency']['wall_ms'] / 1000:.1f}s, {result['turns']} turns, "
                f"{result['tool_calls']} tool calls" + (f": {detail}" if detail else "")
            )
    return {
        "version": REPORT_VERSION,
        "started_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(started)),
        "duration_s": round(time.time() - started, 2),
        "environment": {
            "base_url": bionic.normalize_base_url(config.base_url),
            "model": config.model,
            "stream": config.stream,
            "temperature": config.temperature,
            "max_tokens": config.max_tokens,
            "max_turns": config.max_turns,
            "repeat": repeat,
            "python": platform.python_version(),
            "platform": platform.platform(),
            "server_pids": list(server_pids or []),
            "health": health,
        },
        "summary": summarize(runs),
        "runs": runs,
    }


# --- Text summary ------------------------------------------------------------------------------


def _fmt(value: Any, unit: str = "") -> str:
    if value is None:
        return "-"
    if isinstance(value, float):
        return f"{value:,.1f}{unit}"
    return f"{value:,}{unit}"


def format_summary(report: dict[str, Any]) -> str:
    s = report["summary"]
    env = report["environment"]
    lat = s["latency_ms"]
    tok = s["tokens"]
    lines = [
        f"Bionic benchmark: {env['model']} @ {env['base_url']} (stream={env['stream']}, repeat={env['repeat']})",
        f"  pass rate      {s['passed']}/{s['runs']} ({s['pass_rate'] * 100:.0f}%), "
        f"{_fmt(s['turns_mean'])} turns and {_fmt(s['tool_calls_mean'])} tool calls per run, "
        f"{s['tool_errors']} tool errors",
        f"  scenario wall  mean {_fmt(lat['scenario_wall']['mean'], ' ms')}, p95 {_fmt(lat['scenario_wall']['p95'], ' ms')}",
        f"  LLM call       p50 {_fmt(lat['llm_call']['p50'], ' ms')}, p95 {_fmt(lat['llm_call']['p95'], ' ms')} "
        f"({lat['llm_call']['count']} calls)",
        f"  first token    p50 {_fmt(lat['first_token']['p50'], ' ms')}, p95 {_fmt(lat['first_token']['p95'], ' ms')}",
        f"  tool exec      p50 {_fmt(lat['tool_exec']['p50'], ' ms')}, p95 {_fmt(lat['tool_exec']['p95'], ' ms')}",
        f"  tokens         {_fmt(tok['prompt_tokens'])} prompt + {_fmt(tok['completion_tokens'])} completion; "
        f"{_fmt(tok['tokens_per_run_mean'])} per run, {_fmt(tok['tokens_per_pass'])} per pass, "
        f"{_fmt(tok['completion_tokens_per_s']['mean'], ' tok/s')}",
        f"  memory         runner peak {_fmt(s['memory_mb']['runner_peak'], ' MB')}, "
        f"server peak {_fmt(s['memory_mb']['server_peak'], ' MB')}",
        "  scenarios:",
    ]
    width = max((len(n) for n in s["by_scenario"]), default=0)
    for name, entry in s["by_scenario"].items():
        lines.append(
            f"    {name.ljust(width)}  {entry['passed']}/{entry['runs']} pass  "
            f"{_fmt(entry['wall_ms_mean'], ' ms'):>12}  {_fmt(entry['total_tokens_mean']):>8} tok"
        )
    return "\n".join(lines)


# --- CLI ---------------------------------------------------------------------------------------


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="airc-bionic-bench",
        description=(
            "Benchmark multi-turn tool calling (latency, memory, token efficiency) against a local "
            "OpenAI-compatible model server, using scripted scenarios with deterministic checks."
        ),
    )
    parser.add_argument("--base-url", help="OpenAI-compatible API base URL (env: BIONIC_BASE_URL)")
    parser.add_argument("--api-key", help="Bearer token (env: BIONIC_API_KEY)")
    parser.add_argument("--model", help="Model ID, or 'auto' (env: BIONIC_MODEL)")
    parser.add_argument("--scenario", action="append", dest="scenarios", metavar="NAME", help="Run only this scenario (repeatable)")
    parser.add_argument("--tag", action="append", dest="tags", metavar="TAG", help="Run only scenarios with this tag (repeatable)")
    parser.add_argument("--scenarios-file", help="JSON file of custom scenarios (replaces the built-ins unless --with-builtin)")
    parser.add_argument("--with-builtin", action="store_true", help="With --scenarios-file, also run the built-in scenarios")
    parser.add_argument("--list-scenarios", action="store_true", help="List scenarios and exit")
    parser.add_argument("--repeat", type=int, default=1, help="Run each scenario N times (default 1)")
    parser.add_argument("--max-turns", type=int, help="Max LLM/tool turns per prompt (env: BIONIC_MAX_TURNS)")
    parser.add_argument("--timeout", type=float, help="HTTP timeout in seconds (env: BIONIC_TIMEOUT)")
    parser.add_argument("--command-timeout", type=float, help="run_command timeout in seconds (env: BIONIC_COMMAND_TIMEOUT)")
    parser.add_argument("--temperature", type=float, help="Sampling temperature (env: BIONIC_TEMPERATURE)")
    parser.add_argument("--max-tokens", type=int, help="Completion token cap per turn (env: BIONIC_MAX_TOKENS)")
    parser.add_argument("--stream", action="store_true", help="Stream responses (adds time-to-first-token)")
    parser.add_argument("--auto-pull", action="store_true", help="Download/load the model with lms before benchmarking")
    parser.add_argument("--lms", dest="lms_path", help="Path to the LM Studio lms CLI (env: BIONIC_LMS)")
    parser.add_argument("--pull-timeout", type=float, help="Timeout for each lms get/load (env: BIONIC_PULL_TIMEOUT)")
    parser.add_argument("--no-warmup", action="store_true", help="Skip the warm-up probe, so the first run includes model load time")
    parser.add_argument(
        "--server-pid",
        action="append",
        type=int,
        dest="server_pids",
        metavar="PID",
        help="Also sample resident memory of this inference server process (repeatable)",
    )
    parser.add_argument("--sample-interval", type=float, default=DEFAULT_SAMPLE_INTERVAL, help="Memory sample interval in seconds")
    parser.add_argument("--min-pass-rate", type=float, help="Exit 3 when the pass rate (0..1) is below this value")
    parser.add_argument("--output", help="Write the JSON report to this file instead of stdout")
    parser.add_argument("--keep-workspaces", action="store_true", help="Keep scenario workspaces for inspection")
    parser.add_argument("-q", "--quiet", action="store_true", help="No progress or summary on stderr")
    parser.add_argument("-v", "--verbose", action="store_true", help="Log tool calls to stderr")
    return parser


def main(argv: Optional[list[str]] = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    err = (lambda msg: None) if args.quiet else (lambda msg: print(msg, file=sys.stderr, flush=True))

    try:
        scenarios = builtin_scenarios()
        if args.scenarios_file:
            custom = load_scenarios_file(args.scenarios_file)
            scenarios = scenarios + custom if args.with_builtin else custom
        scenarios = select_scenarios(scenarios, args.scenarios, args.tags)

        if args.list_scenarios:
            for s in scenarios:
                print(f"{s.name:<18} [{', '.join(s.tags)}] {len(s.prompts)} prompt(s) - {s.description}")
            return 0

        if args.repeat < 1:
            raise BionicError("--repeat must be at least 1")
        if args.min_pass_rate is not None and not 0.0 <= args.min_pass_rate <= 1.0:
            raise BionicError("--min-pass-rate must be between 0 and 1")
        if args.sample_interval <= 0:
            raise BionicError("--sample-interval must be positive")

        config = bionic.resolve_config(args)
        config.approval = "auto"
        if not config.model:
            raise BionicError("Model is required: pass --model <id>, set BIONIC_MODEL, or use --model auto")

        log = (lambda msg: print(msg, file=sys.stderr)) if config.verbose else None
        health = bionic.check_health(config, probe=not args.no_warmup, auto_pull=config.auto_pull, log=log)
        if not health["ok"]:
            print(json.dumps({"ok": False, "health": health}))
            print(f"error: model not ready: {health['error']}", file=sys.stderr)
            return 2
        config.model = str(health["model"])
        config.auto_pull = False
        err(f"model {config.model} ready; running {len(scenarios)} scenario(s) x {args.repeat}")

        report = run_suite(
            scenarios,
            config,
            repeat=args.repeat,
            server_pids=args.server_pids,
            sample_interval=args.sample_interval,
            keep_workspace=args.keep_workspaces,
            progress=err,
            log=log,
            health=health,
        )
        report["ok"] = args.min_pass_rate is None or report["summary"]["pass_rate"] >= args.min_pass_rate
        text = json.dumps(report, indent=2)
        if args.output:
            Path(args.output).write_text(text + "\n", encoding="utf-8")
            err(f"report written to {args.output}")
        else:
            print(text)
        err(format_summary(report))
        if not report["ok"]:
            err(f"error: pass rate {report['summary']['pass_rate']:.3f} below --min-pass-rate {args.min_pass_rate}")
            return 3
        return 0
    except BionicError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
