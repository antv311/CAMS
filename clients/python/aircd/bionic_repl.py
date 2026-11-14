"""Interactive multi-turn REPL and step-through debug mode for ``airc-bionic``.

``airc-bionic --repl`` keeps one conversation open across prompts so a
developer can chat with a local model, look at exactly what is in its context,
run tools by hand and replay or undo turns. ``--debug`` (or ``/step on``)
pauses before every tool call the model makes so it can be run, skipped,
edited or answered with a fake result.

Everything goes through :func:`aircd.bionic.run_agent`, so the REPL uses the
same jail, approval modes, plugins and sessions as a one-shot run.
"""

from __future__ import annotations

import dataclasses
import json
import sys
from pathlib import Path
from typing import Any, Callable, Optional

from aircd import bionic
from aircd.bionic import (
    APPROVAL_MODES,
    BionicConfig,
    BionicError,
    MaxTurnsError,
    RunAborted,
    StreamRenderer,
    ToolStep,
)

PREVIEW_CHARS = 160
RESULT_PREVIEW_CHARS = 1200
SKIPPED_RESULT = json.dumps({"error": "Skipped by the operator in debug mode; do not retry it", "skipped": True})

HELP = """\
Type a prompt to send it to the agent; the conversation carries over between prompts.
Start a multi-line prompt with a line containing only \"\"\" and end it the same way,
or end a line with a backslash to continue on the next line.

Conversation
  /context [N|all]    list the messages in context (last 20 by default)
  /show N             print message N in full as JSON
  /system             print the system prompt the model sees
  /undo               drop the last prompt and everything the agent did for it
  /retry              /undo, then send the same prompt again
  /reset              start an empty conversation
  /save FILE          write the conversation (OpenAI messages JSON) to FILE
  /load FILE          replace the conversation with one saved by /save or a session file
  /usage              token totals for this REPL session

Tools and debugging
  /tools              list the tools the model is offered
  /tool NAME [JSON]   run a tool yourself (jail and approval still apply); not added to context
  /step [on|off]      pause before each tool call to run, skip, edit or fake it
  /approval MODE      auto, prompt or read-only

Settings
  /model [ID]         show or change the model
  /models             list chat models on the server
  /set KEY VALUE      temperature, max_tokens, max_turns, timeout, command_timeout (VALUE 'none' clears)
  /stream [on|off]    stream tokens as they arrive
  /config             show the effective settings
  /help               this help
  /exit, /quit        leave (Ctrl-D / Ctrl-Z also works)
"""

STEP_PROMPT = "[step] (r)un (s)kip (e)dit (f)ake (c)ontinue (i)nspect (a)bort ? "
STEP_HELP = """\
  r, Enter  run the tool and show its result
  s         skip it; the model is told the operator skipped the call
  e         edit the arguments (JSON object) before running
  f         fake the result: type the text the model should receive instead
  c         stop pausing for the rest of this prompt
  i         show the conversation so far
  a         abort this prompt and roll it back
"""

_SETTABLE: dict[str, Callable[[str], Any]] = {
    "temperature": float,
    "max_tokens": int,
    "max_turns": int,
    "timeout": float,
    "command_timeout": float,
}
_REQUIRED = {"max_turns", "timeout", "command_timeout"}


def _clip(text: str, limit: int) -> str:
    text = text.replace("\r\n", "\n")
    return text if len(text) <= limit else text[:limit] + f"... [{len(text) - limit} more chars]"


def _one_line(text: str, limit: int = PREVIEW_CHARS) -> str:
    flat = " ".join(str(text).split())
    return flat if len(flat) <= limit else flat[: limit - 3] + "..."


def summarize_message(msg: dict[str, Any]) -> str:
    """One-line description of a chat message for ``/context``."""
    role = str(msg.get("role") or "?")
    content = msg.get("content")
    text = content if isinstance(content, str) else (json.dumps(content) if content else "")
    if role == "tool":
        return f"tool[{msg.get('tool_call_id')}] {_one_line(text)}"
    calls = msg.get("tool_calls") or []
    if calls:
        names = []
        for tc in calls:
            fn = tc.get("function") if isinstance(tc, dict) else None
            names.append(str(fn.get("name")) if isinstance(fn, dict) else "?")
        prefix = f"{text and _one_line(text, 80) + ' '}"
        return f"{role} {prefix}-> calls {', '.join(names)}"
    return f"{role} {_one_line(text)}"


class BionicRepl:
    """Stateful interactive session around :func:`aircd.bionic.run_agent`.

    ``input_fn`` and ``out`` are injectable so tests (and embedders) can drive
    the REPL without a terminal. ``input_fn`` should raise ``EOFError`` at end
    of input, like :func:`input`.
    """

    def __init__(
        self,
        config: BionicConfig,
        *,
        step: bool = False,
        input_fn: Callable[[str], str] = input,
        out: Any = None,
    ) -> None:
        self.config = dataclasses.replace(config, emit_session_id=False)
        self.step = step
        self.input = input_fn
        self.out = out if out is not None else sys.stdout
        self.messages: list[dict[str, Any]] = []
        self.usage: dict[str, int] = {}
        self._stepping = False
        self._registry: Any = None
        self._session_file: Optional[Path] = None
        if self.config.session_id:
            self._session_file = bionic._resolve_session_file(self.config.session_id, self.config.session_dir)
            self.messages = bionic._load_session_messages(self._session_file) or []

    # --- output helpers -------------------------------------------------------------------------

    def say(self, text: str = "") -> None:
        try:
            self.out.write(text + "\n")
        except UnicodeEncodeError:
            enc = getattr(self.out, "encoding", None) or "ascii"
            self.out.write(text.encode(enc, errors="replace").decode(enc, errors="replace") + "\n")
        self.out.flush()

    def ask_line(self, prompt: str) -> str:
        self.out.flush()
        return self.input(prompt)

    # --- context helpers ------------------------------------------------------------------------

    def registry(self) -> Any:
        if self._registry is None:
            self._registry = bionic.load_tool_registry(self.config)
        return self._registry

    def system_prompt(self) -> str:
        if self.messages and self.messages[0].get("role") == "system":
            return str(self.messages[0].get("content") or "")
        schemas = bionic.tools_for_approval(self.config.approval, self.registry())
        return bionic.system_prompt_for(self.config, schemas)

    def _last_user_index(self) -> Optional[int]:
        for idx in range(len(self.messages) - 1, -1, -1):
            if self.messages[idx].get("role") == "user":
                return idx
        return None

    def _persist(self) -> None:
        """Mirror an /undo, /reset or /load into the ``--session-id`` file."""
        if self._session_file is None:
            return
        if self.messages:
            bionic._save_session_messages(self._session_file, self.messages)
        else:
            try:
                self._session_file.unlink()
            except OSError:
                pass

    # --- approval and step hooks ----------------------------------------------------------------

    def approve(self, name: str, args: dict[str, Any]) -> bool:
        """``--approval prompt`` approver that asks through the REPL's own input."""
        try:
            answer = self.ask_line(f"[approval] Allow {bionic._describe_tool_call(name, args)}? [y/N] ")
        except EOFError:
            return False
        return answer.strip().lower() in ("y", "yes")

    def before_tool(self, step: ToolStep) -> Optional[str]:
        if not self._stepping:
            return None
        self.say(f"[step] turn {step.turn}, call {step.id}: {step.name}")
        self.say(_clip(json.dumps(step.arguments, indent=2, ensure_ascii=False), RESULT_PREVIEW_CHARS))
        while True:
            try:
                choice = self.ask_line(STEP_PROMPT).strip().lower()
            except EOFError:
                raise RunAborted("input closed during step mode") from None
            if choice in ("", "r", "run"):
                result = step.execute()
                err = bionic._tool_result_error(result)
                self.say(f"[step] result ({len(result)} chars{', error: ' + err if err else ''}):")
                self.say(_clip(result, RESULT_PREVIEW_CHARS))
                return result
            if choice in ("s", "skip"):
                self.say("[step] skipped")
                return SKIPPED_RESULT
            if choice in ("e", "edit"):
                raw = self.read_block("[step] new arguments JSON> ")
                if raw is None:
                    continue
                try:
                    parsed = json.loads(raw)
                except json.JSONDecodeError as err:
                    self.say(f"[step] invalid JSON: {err}")
                    continue
                if not isinstance(parsed, dict):
                    self.say("[step] arguments must be a JSON object")
                    continue
                step.arguments = parsed
                self.say(_clip(json.dumps(parsed, indent=2, ensure_ascii=False), RESULT_PREVIEW_CHARS))
                continue
            if choice in ("f", "fake"):
                raw = self.read_block("[step] fake result> ")
                if raw is None:
                    continue
                self.say("[step] returning fake result")
                return raw
            if choice in ("c", "continue"):
                self._stepping = False
                return None
            if choice in ("i", "inspect"):
                self.cmd_context("all")
                continue
            if choice in ("a", "abort"):
                raise RunAborted("aborted by the operator")
            self.say(STEP_HELP.rstrip("\n"))

    # --- input ----------------------------------------------------------------------------------

    def read_block(self, prompt: str) -> Optional[str]:
        """Read one logical input: a line, ``\\``-continued lines, or a triple-quoted block."""
        try:
            line = self.ask_line(prompt)
        except EOFError:
            return None
        if line.strip() == '"""':
            lines: list[str] = []
            while True:
                try:
                    nxt = self.ask_line("... ")
                except EOFError:
                    break
                if nxt.strip() == '"""':
                    break
                lines.append(nxt)
            return "\n".join(lines)
        parts = [line]
        while parts[-1].endswith("\\"):
            parts[-1] = parts[-1][:-1]
            try:
                parts.append(self.ask_line("... "))
            except EOFError:
                break
        return "\n".join(parts)

    # --- running prompts ------------------------------------------------------------------------

    def send(self, prompt: str) -> bool:
        """Send one prompt; on failure the conversation is rolled back. Returns success."""
        mark = len(self.messages)
        self._stepping = self.step
        renderer = StreamRenderer("text", out=self.out)
        streamed = {"tokens": False}

        def on_event(event: dict[str, Any]) -> None:
            if event.get("type") == "token":
                streamed["tokens"] = True
            renderer(event)

        log = self.say if self.config.verbose else None
        try:
            result = bionic.run_agent(
                prompt,
                self.config,
                log=log,
                approver=self.approve,
                on_event=on_event,
                messages=self.messages,
                before_tool=self.before_tool,
            )
        except KeyboardInterrupt:
            del self.messages[mark:]
            self.say("\n[interrupted] prompt rolled back")
            return False
        except MaxTurnsError as err:
            self._add_usage(err.usage)
            self.say(f"[error] {err} (the partial transcript is kept; /undo drops it)")
            return False
        except BionicError as err:
            del self.messages[mark:]
            label = "aborted" if isinstance(err, RunAborted) else "error"
            self.say(f"[{label}] {err} (prompt rolled back)")
            return False
        finally:
            self._stepping = False
        self._add_usage(result.usage)
        if result.content and not (self.config.stream and streamed["tokens"]):
            self.say(result.content)
        tokens = f", {result.usage['total_tokens']} tokens" if "total_tokens" in result.usage else ""
        self.say(f"[{result.model}: {result.turns} turn(s), {result.tool_calls} tool call(s){tokens}]")
        return True

    def _add_usage(self, usage: dict[str, int]) -> None:
        for key, value in (usage or {}).items():
            self.usage[key] = self.usage.get(key, 0) + value

    # --- commands -------------------------------------------------------------------------------

    def cmd_context(self, arg: str = "") -> None:
        if not self.messages:
            self.say("(empty conversation; the system prompt is added on the first prompt, see /system)")
            return
        start = 0
        if arg != "all":
            count = int(arg) if arg.isdigit() else 20
            start = max(0, len(self.messages) - count)
        if start:
            self.say(f"... {start} earlier message(s); /context all shows everything")
        for idx in range(start, len(self.messages)):
            self.say(f"[{idx}] {summarize_message(self.messages[idx])}")
        chars = sum(len(json.dumps(m, ensure_ascii=False)) for m in self.messages)
        self.say(f"({len(self.messages)} messages, ~{chars} chars)")

    def cmd_show(self, arg: str) -> None:
        try:
            idx = int(arg)
            msg = self.messages[idx]
        except (ValueError, IndexError):
            self.say(f"usage: /show N with N from 0 to {len(self.messages) - 1}" if self.messages else "(empty)")
            return
        self.say(json.dumps(msg, indent=2, ensure_ascii=False))

    def cmd_undo(self) -> Optional[str]:
        idx = self._last_user_index()
        if idx is None:
            self.say("nothing to undo")
            return None
        prompt = str(self.messages[idx].get("content") or "")
        dropped = len(self.messages) - idx
        del self.messages[idx:]
        if len(self.messages) == 1 and self.messages[0].get("role") == "system":
            self.messages.clear()
        self._persist()
        self.say(f"dropped {dropped} message(s): {_one_line(prompt, 80)}")
        return prompt

    def cmd_tool(self, arg: str) -> None:
        name, _, raw = arg.partition(" ")
        if not name:
            self.say("usage: /tool NAME [JSON arguments]")
            return
        try:
            args = json.loads(raw) if raw.strip() else {}
        except json.JSONDecodeError as err:
            self.say(f"invalid JSON arguments: {err}")
            return
        result = bionic.exec_tool(
            name,
            args,
            self.config.cwd,
            command_timeout=self.config.command_timeout,
            max_output_chars=self.config.max_output_chars,
            log=self.say if self.config.verbose else None,
            workspace_root=self.config.workspace_root or self.config.cwd,
            approval=self.config.approval,
            approver=self.approve,
            registry=self.registry(),
        )
        self.say(result)

    def cmd_tools(self) -> None:
        for tool in bionic.describe_tools(self.config, self.registry()):
            flags = [tool["source"]] + (["mutating"] if tool["mutating"] else [])
            params = ", ".join((tool["parameters"].get("properties") or {}).keys())
            self.say(f"{tool['name']}({params}) [{', '.join(flags)}]")
            self.say(f"    {_one_line(tool['description'], 120)}")

    def cmd_set(self, arg: str) -> None:
        key, _, value = arg.strip().partition(" ")
        key = key.replace("-", "_")
        value = value.strip()
        if key not in _SETTABLE or not value:
            self.say(f"usage: /set KEY VALUE with KEY one of {', '.join(_SETTABLE)}")
            return
        if value.lower() == "none":
            if key in _REQUIRED:
                self.say(f"{key} cannot be cleared")
                return
            parsed: Any = None
        else:
            try:
                parsed = _SETTABLE[key](value)
            except ValueError:
                self.say(f"invalid value for {key}: {value!r}")
                return
            if key in _REQUIRED and parsed <= 0:
                self.say(f"{key} must be positive")
                return
        setattr(self.config, key, parsed)
        self.say(f"{key} = {parsed}")

    def cmd_config(self) -> None:
        data = dataclasses.asdict(self.config)
        if data.get("api_key"):
            data["api_key"] = "***"
        data["step"] = self.step
        data["messages"] = len(self.messages)
        for key, value in data.items():
            self.say(f"{key}: {value}")

    def _toggle(self, arg: str, current: bool, name: str) -> Optional[bool]:
        arg = arg.strip().lower()
        if arg in ("", "toggle"):
            return not current
        if arg in ("on", "true", "1", "yes"):
            return True
        if arg in ("off", "false", "0", "no"):
            return False
        self.say(f"usage: /{name} [on|off]")
        return None

    def cmd_save(self, arg: str) -> None:
        if not arg:
            self.say("usage: /save FILE")
            return
        path = Path(arg).expanduser()
        try:
            path.write_text(json.dumps(self.messages, indent=2, ensure_ascii=False), encoding="utf-8")
        except OSError as err:
            self.say(f"cannot write {path}: {err}")
            return
        self.say(f"saved {len(self.messages)} message(s) to {path}")

    def cmd_load(self, arg: str) -> None:
        if not arg:
            self.say("usage: /load FILE")
            return
        path = Path(arg).expanduser()
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as err:
            self.say(f"cannot load {path}: {err}")
            return
        if not isinstance(data, list) or not all(isinstance(m, dict) and "role" in m for m in data):
            self.say(f"{path} is not a list of chat messages")
            return
        self.messages[:] = data
        self._persist()
        self.say(f"loaded {len(data)} message(s) from {path}")

    def handle_command(self, line: str) -> bool:
        """Run one ``/command``. Returns False when the REPL should exit."""
        cmd, _, arg = line[1:].partition(" ")
        cmd = cmd.lower()
        arg = arg.strip()
        if cmd in ("exit", "quit", "q"):
            return False
        if cmd in ("help", "h", "?"):
            self.say(HELP.rstrip("\n"))
        elif cmd == "context":
            self.cmd_context(arg)
        elif cmd == "show":
            self.cmd_show(arg)
        elif cmd == "system":
            self.say(self.system_prompt())
        elif cmd == "undo":
            self.cmd_undo()
        elif cmd == "retry":
            prompt = self.cmd_undo()
            if prompt is not None:
                self.send(prompt)
        elif cmd == "reset":
            self.messages.clear()
            self._persist()
            self.say("conversation cleared")
        elif cmd == "save":
            self.cmd_save(arg)
        elif cmd == "load":
            self.cmd_load(arg)
        elif cmd == "usage":
            self.say(json.dumps(self.usage) if self.usage else "no usage reported yet")
        elif cmd == "tools":
            self.cmd_tools()
        elif cmd == "tool":
            self.cmd_tool(arg)
        elif cmd == "step":
            value = self._toggle(arg, self.step, "step")
            if value is not None:
                self.step = value
                self.say(f"step mode {'on' if value else 'off'}")
        elif cmd == "stream":
            value = self._toggle(arg, self.config.stream, "stream")
            if value is not None:
                self.config.stream = value
                self.say(f"streaming {'on' if value else 'off'}")
        elif cmd == "approval":
            if arg not in APPROVAL_MODES:
                self.say(f"approval: {self.config.approval} (choose one of {', '.join(APPROVAL_MODES)})")
            else:
                self.config.approval = arg
                self.say(f"approval = {arg}")
        elif cmd == "model":
            if arg:
                self.config.model = arg
            self.say(f"model = {self.config.model or '(unset: /model ID or /model auto)'}")
        elif cmd == "models":
            models = bionic.list_models(
                self.config.base_url, api_key=self.config.api_key, timeout=min(self.config.timeout, 30.0)
            )
            for entry in models:
                mark = "*" if entry["id"] == self.config.model else " "
                self.say(f"{mark} {entry['id']}")
        elif cmd == "set":
            self.cmd_set(arg)
        elif cmd == "config":
            self.cmd_config()
        else:
            self.say(f"unknown command /{cmd}; /help lists commands")
        return True

    # --- main loop ------------------------------------------------------------------------------

    def banner(self) -> None:
        model = self.config.model or "(none: use /model)"
        self.say(f"airc-bionic REPL - model {model} at {self.config.base_url}")
        self.say(
            f"cwd {self.config.cwd}, approval {self.config.approval}"
            + (", step mode on" if self.step else "")
            + (f", session {self.config.session_id} ({len(self.messages)} messages)" if self.config.session_id else "")
        )
        self.say("/help for commands, /exit to quit")

    def loop(self, initial_prompt: Optional[str] = None) -> int:
        self.banner()
        if initial_prompt and initial_prompt.strip():
            self.send(initial_prompt)
        while True:
            try:
                text = self.read_block("bionic[step]> " if self.step else "bionic> ")
            except KeyboardInterrupt:
                self.say("")
                continue
            if text is None:
                self.say("")
                return 0
            if not text.strip():
                continue
            stripped = text.strip()
            if stripped.startswith("/") and "\n" not in stripped:
                try:
                    if not self.handle_command(stripped):
                        return 0
                except KeyboardInterrupt:
                    self.say("\n[interrupted]")
                except BionicError as err:
                    self.say(f"[error] {err}")
                continue
            self.send(text)


def run_repl(
    config: BionicConfig,
    *,
    step: bool = False,
    initial_prompt: Optional[str] = None,
    input_fn: Callable[[str], str] = input,
    out: Any = None,
) -> int:
    """Entry point for ``airc-bionic --repl`` / ``--debug``."""
    if config.auto_pull:
        # Make the model ready once up front instead of before every prompt.
        report = bionic.ensure_model_ready(config, log=lambda msg: print(msg, file=sys.stderr))
        config = dataclasses.replace(config, model=str(report["model"]), auto_pull=False)
    if input_fn is input:
        try:
            import readline  # noqa: F401  (line editing and history where available)
        except ImportError:
            pass
    return BionicRepl(config, step=step, input_fn=input_fn, out=out).loop(initial_prompt)
