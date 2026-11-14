# LM Studio Bionic Agent Runner (`airc-bionic`)

`airc-bionic` (`clients/python/aircd/bionic.py`) is CAMS's built-in OpenAI-compatible
function-calling runner for LM Studio Bionic (default `http://localhost:1235/v1`)
and compatible local inference servers.

## Why a separate runner from `lmstudio`

The [`lmstudio`](templates/lmstudio.json) preset invokes `lms chat -y -p "{prompt}" "{model}"`.
`lms chat` is a pure text chat command: it cannot inspect files, edit code, run
tests, or execute `airc` coordination commands.

`airc-bionic` runs a multi-turn tool-calling loop against `/v1/chat/completions`
and exposes five tools to any tool-capable local model:

- `run_command` — execute shell commands in the task working directory (`airc`, `cargo`, `python`, `git`, test suites).
- `read_file` — read UTF-8 files with optional `start_line` / `end_line` ranges.
- `write_file` — write full file contents and create parent directories.
- `edit_file` — perform unique search-and-replace edits on existing files.
- `apply_patch` — apply a unified diff to one or more files (see below).

Projects can add their own tools as Python plugins; see
[Custom tools](#custom-tools-plugins).

### Patch tool

`apply_patch` takes a unified diff in `patch` (the `diff -u` / `git diff`
format) so a model can change a few lines without rewriting the whole file:

- Hunks are located by their context and `-` lines, not by line number alone.
  `@@` line numbers may be wrong or left out (`@@ @@`); when the context matches
  more than once, the match nearest the `@@` line number wins, and with no line
  number an ambiguous match is an error. If no exact match exists, a match that
  ignores trailing whitespace, then one that ignores indentation, is tried; the
  result reports any `offset` or `fuzz` used.
- `--- /dev/null` creates a file, `+++ /dev/null` deletes one (every line must
  be removed), and different `---`/`+++` paths rename. `a/` and `b/` prefixes
  are stripped, and `\ No newline at end of file` markers are honored.
- A file's CRLF or LF line endings are kept.
- The patch is all-or-nothing: if any hunk in any file fails, nothing is written
  and the error names the file, the hunk and its first expected line.
- `path` sets the target file of a single-file patch (needed when the patch has
  no `---`/`+++` headers). `dry_run: true` validates and reports without
  writing, and is never prompted for in `prompt` mode.
- Every source and destination path is checked against the workspace root.

## Installation

Install the Python package in editable mode so `airc-bionic` is on `PATH`:

```powershell
pip install -e clients/python
```

Without installing on `PATH`, you can invoke the runner directly with
`PYTHONPATH=clients/python python -m aircd.bionic` (or `.\airc-bionic.cmd` from
the repository root on Windows). Zero third-party Python packages are required.

Verify model discovery and readiness against your running Bionic server:

```powershell
airc-bionic --list-models
airc-bionic --health --model auto
```

## Configuration

Precedence order: **CLI flags > environment variables > built-in defaults**.
No paths, model IDs, or hostnames outside `localhost` are hardcoded.

| CLI flag | Environment variable | Default | Purpose |
| --- | --- | --- | --- |
| `--base-url` | `BIONIC_BASE_URL`, `AIRCD_BIONIC_BASE_URL`, `OPENAI_BASE_URL` | `http://localhost:1235/v1` | OpenAI-compatible API base URL |
| `--api-key` | `BIONIC_API_KEY`, `OPENAI_API_KEY` | `lm-studio` | Bearer token (`""` omits header) |
| `--model` | `BIONIC_MODEL` | *(required, or `auto`)* | Model ID, or `auto` to pick the first chat model |
| `--cwd` | `BIONIC_CWD` | current directory | Working directory for file and shell tools |
| `--workspace-root` | `BIONIC_WORKSPACE_ROOT` | the working directory | Directory every tool call is jailed to (must contain `--cwd`) |
| `--approval` | `BIONIC_APPROVAL` | `auto` | Tool approval mode: `auto`, `prompt`, or `read-only` (see below) |
| `--max-turns` | `BIONIC_MAX_TURNS` | `30` | Maximum LLM/tool turns before failing |
| `--timeout` | `BIONIC_TIMEOUT` | `300` | HTTP request timeout in seconds |
| `--command-timeout` | `BIONIC_COMMAND_TIMEOUT` | `120` | Per-`run_command` timeout in seconds |
| `--max-output-chars` | `BIONIC_MAX_OUTPUT_CHARS` | `16000` | Truncation limit for `read_file` and `run_command` |
| `--temperature` | `BIONIC_TEMPERATURE` | *(unset)* | Optional sampling temperature |
| `--max-tokens` | `BIONIC_MAX_TOKENS` | *(unset)* | Optional completion token cap per turn |
| `--token-budget` | `BIONIC_TOKEN_BUDGET` | *(unset)* | Stop before a new turn once the run has spent this many total tokens (see [Token budgets](#token-budgets-and-task-cost-accounting)) |
| `--task` | `BIONIC_TASK_ID`, then `AIRCD_WAKE_TASK` | *(the woken task)* | Hub task billed for every turn; its budget (and its parents') is enforced |
| `--no-report-usage` | `BIONIC_NO_USAGE_REPORT` | `false` | Do not send token usage to the hub task |
| `--system-prompt` | `BIONIC_SYSTEM_PROMPT` | *(generated)* | Override default system prompt (or `--system-prompt-file`) |
| `--session-id` | — | *(none)* | Persist/resume message history for a session ID |
| `--session-dir` | `BIONIC_SESSION_DIR` | `<tempdir>/aircd-bionic-sessions` | Directory where session JSON files are stored |
| `--emit-session-id` | — | `false` | Print `session_id: <id>` on stderr for `aircd` tracking |
| `--stream` | `BIONIC_STREAM` | `false` | Request SSE token streaming and show live progress on stderr |
| `--stream-format` | `BIONIC_STREAM_FORMAT` | `text` | `text` or `jsonl` live feedback (the CLI flag implies `--stream`) |
| `--auto-pull` | `BIONIC_AUTO_PULL` | `false` | Download/load the model with `lms` and confirm it answers before the first turn |
| `--lms` | `BIONIC_LMS` | `lms` on `PATH` | Path to the LM Studio `lms` CLI used by `--auto-pull` |
| `--pull-timeout` | `BIONIC_PULL_TIMEOUT` | `1800` | Timeout in seconds for each `lms get` / `lms load` |
| `--tools-dir` (repeatable) | `BIONIC_TOOLS_DIR` (`os.pathsep`-separated) | *(none)* | Extra plugin tool directories, loaded after `<workspace-root>/.aircd/tools` |
| `--no-plugins` | `BIONIC_NO_PLUGINS` | `false` | Load no plugin tools at all |
| `--list-tools` | — | — | Validate plugins, print the offered tools as JSON, and exit |

### Readiness check (`--health`) and auto-pull (`--auto-pull`)

`airc-bionic --health --model <id|auto>` checks, in order, that:

1. the server answers `GET /models` (`server.reachable`);
2. the model is listed (`available`); `auto` picks the first chat model that is
   already loaded, else the first chat model;
3. its load `state` is `loaded`, read from LM Studio's `GET /api/v0/models`
   (servers without that endpoint report `unknown`);
4. it answers a one-token chat completion (`probe`, skipped with `--no-probe`).
   LM Studio loads a model on demand for this request, so a passing probe also
   means the model is now resident.

It prints one JSON report on stdout and exits **0** when the model is ready and
**2** when it is not (the reason is in `error` and on stderr). Exit 1 still
means a configuration error. Without `--model` it reports only the server and
its `available_models`.

```powershell
airc-bionic --health --model auto
# {"ok": true, "model": "google/gemma-4-26b-a4b-qat", "available": true, "state": "loaded",
#  "probe": {"ok": true, "latency_ms": 412, "error": null}, "actions": [], ...}
```

With `--auto-pull` (or `BIONIC_AUTO_PULL=1`), a missing model is downloaded with
`lms get <model> --yes` and a not-loaded one is loaded with `lms load <model>
--yes`; each step is recorded in `actions`. The `lms` CLI is found through
`--lms`, `BIONIC_LMS`, or `PATH`, and each step is bounded by `--pull-timeout`.

On a normal run, `--auto-pull` does the same check as a pre-task gate before the
first turn: the model is pulled and loaded if needed, and probed unless LM Studio
already reports it `loaded`. If the model cannot be made ready, the run fails
with `error: Bionic model not ready: ...` before any tool call. To enable it for
hub wakes, set `"env": {"BIONIC_AUTO_PULL": "1"}` on the `bionic` template (or add
`--auto-pull` to `args`). Make sure the hub's `PATH` contains `lms`
(`~/.lmstudio/bin`) or set `BIONIC_LMS`. A download can take a long time, so keep
`--pull-timeout` in mind. From Python, `BionicAgent(..., auto_pull=True)` gates
each `run()`, and `agent.health(probe=True, auto_pull=None)` returns the same
report. With `auto_pull`, the run also emits a `{"type": "health", "report": ...}`
event first.

### Streaming (`--stream`)

With `--stream`, each turn is sent with `"stream": true` and the
`text/event-stream` reply is assembled from `chat.completion.chunk` deltas:
content tokens, `reasoning_content`/`reasoning` deltas, and tool-call fragments
merged by `index` (missing tool-call IDs get a generated `call_<turn>_<n>` that
is also written into history, so tool replies stay paired). If the server
ignores the flag and returns plain JSON, the runner uses that body as-is.

Live feedback always goes to **stderr**. **stdout is unchanged**: it still holds
only the final answer (or the `--json` object), so `aircd` wakes and scripts
that parse stdout keep working.

- `--stream-format text` (default) prints tokens as they arrive, reasoning on
  `[thinking] …` lines, and `[tool] write_file: out.txt (9 chars)` style lines
  for each tool call (plus `[tool] <name> failed: …` on errors).
- `--stream-format jsonl` prints one JSON event per line, for real-time execution
  logs: `{"type":"turn","turn":1,"model":…}`, `{"type":"token","turn":1,"text":…}`,
  `{"type":"reasoning",…}`, `{"type":"tool_call","id":…,"name":…,"arguments":…}`,
  `{"type":"tool_result","id":…,"name":…,"error":null,"chars":…}`, and
  `{"type":"done","turns":…,"tool_calls":…}`.

```powershell
airc-bionic --model auto --stream -p "Run the tests and summarize failures"
airc-bionic --model auto --stream-format jsonl -p "..." 2> run-log.jsonl
```

From Python, pass `on_event=` to `aircd.bionic.run_agent` to receive the same
event dicts (`turn`, `tool_call`, `tool_result`, `done` fire in both modes;
`token`/`reasoning` only when `config.stream` is set). `StreamRenderer` is the
renderer the CLI uses.

Because `aircd` captures a worker's stderr into the wake transcript, enabling
`BIONIC_STREAM` on a hub template makes those transcripts include the model's
token stream; leave it off for templates unless you want that.

### Token budgets and task cost accounting

Every turn's `usage` (prompt, completion and total tokens) is counted. When the
server sends no `usage`, Bionic estimates it from message sizes (about four
characters per token) so a budget still holds.

- **Run budget.** `--token-budget N` caps the total tokens of one run (or of a
  whole REPL session). Before each turn Bionic checks what is left; once it is
  spent the run stops with `TokenBudgetError` (a `MaxTurnsError` subclass),
  the CLI exits with code **3**, and `--json` prints
  `{"ok": false, "reason": "token_budget", "budget": {...}}`. A turn never asks
  for more completion tokens than remain (`max_tokens` is lowered).
- **Hub task billing.** With `--task ID`, or automatically when the hub wakes
  the worker for a task (`AIRCD_WAKE_TASK`), each turn is sent to
  `POST /v1/tasks/{id}/usage` with the session id and model. The hub keeps
  the records and rolls them up through the subtask tree. The hub URL and
  token come from `AIRCD_PROFILE` (or the profile named after `AIRC_NICK`),
  `AIRCD_TOKEN`, and `AIRCD_HTTP_URL` / `AIRC_SERVER`, the same sources `airc`
  uses. If there are no credentials or the hub can't be reached, Bionic prints
  a warning and keeps running under the local budget.
- **Task budgets.** A PM, or the lead who owns the parent task, sets a ceiling
  with `airc task budget <id> <tokens>` (or `airc task create ... --token-budget N`).
  A budget covers the task's whole subtree, and every ancestor's budget also
  applies, so subtasks share their parent's allowance. Bionic reads the tightest
  remaining allowance before its first turn and after each report. It refuses to
  start, or stops before the next turn, once that allowance reaches zero.

```bash
airc task budget task_parent 200000          # PM: cap the parent and its subtasks
airc-bionic --task task_child -p "Fix the flaky test"
airc task usage task_parent                  # own / subtask / total tokens, sessions, budgets
airc task budget task_parent none            # lift the cap
```

`RunResult.budget` (SDK) and the `budget` key of `--json` output carry the
ledger snapshot: `token_budget`, `spent`, `remaining`, `limited_by`
(`run` or `task <id>`), `estimated_turns`, and for hub tasks `task_id`,
`reported_turns` and `task_total_tokens`. With `--stream-format jsonl` a
`budget` event follows each turn.

### Interactive REPL and debug mode (`--repl`, `--debug`)

`airc-bionic --repl` opens an interactive session that keeps one conversation
across prompts, which is useful for trying prompts against a local model,
checking exactly what the model sees, and testing tools without writing a
script. All the usual flags apply (`--model`, `--cwd`, `--workspace-root`,
`--approval`, `--tools-dir`, `--stream`, `--session-id` …), and every prompt goes
through the same jail, approval and plugin code as a one-shot run. `-p` sends a
first prompt before the REPL starts reading input.

```powershell
airc-bionic --model auto --repl
airc-bionic --model auto --debug            # same, with step mode on from the start
airc-bionic --model auto --repl --session-id scratch   # resume/persist a saved session
```

Type a prompt to send it. For a multi-line prompt, put `"""` on a line by itself
before and after it, or end each line with `\`. Commands:

| Command | What it does |
|---|---|
| `/context [N\|all]` | List the messages in context with their index (last 20 by default) |
| `/show N` | Print message N in full as JSON (tool-call arguments, raw tool results) |
| `/system` | Print the system prompt |
| `/undo` | Drop the last prompt and everything the agent did for it |
| `/retry` | `/undo`, then send the same prompt again |
| `/reset` | Start an empty conversation |
| `/save FILE`, `/load FILE` | Write or read the conversation as OpenAI messages JSON (session files work too) |
| `/usage` | Token totals for the REPL session |
| `/tools` | List the tools the model is offered (built-in and plugin) |
| `/tool NAME [JSON]` | Run a tool yourself, e.g. `/tool read_file {"path": "README.md"}`; jail and approval still apply, and the result is not added to the context |
| `/step [on\|off]` | Toggle step mode |
| `/approval MODE` | Switch between `auto`, `prompt` and `read-only` |
| `/model [ID]`, `/models` | Show or change the model; list chat models on the server |
| `/set KEY VALUE` | `temperature`, `max_tokens`, `max_turns`, `timeout`, `command_timeout` (`none` clears the optional ones) |
| `/stream [on\|off]` | Stream tokens while the model writes |
| `/config` | Show the effective settings (API key masked) |
| `/exit` | Quit (Ctrl-D, or Ctrl-Z then Enter on Windows, works too) |

In **step mode** the REPL pauses before every tool call the model makes, shows
the call's name and arguments, and asks what to do:

- `r` / Enter runs the tool and shows its result;
- `s` skips it, and the model gets `{"error": "Skipped by the operator …", "skipped": true}`;
- `e` takes replacement arguments (a JSON object) and then asks again;
- `f` takes text to send back as the tool result without running anything;
- `c` stops pausing for the rest of the current prompt;
- `i` prints the conversation so far;
- `a` aborts the prompt.

Edited arguments only change what runs: the transcript keeps the arguments the
model actually sent. If a prompt is aborted, interrupted with Ctrl-C or fails
with a server error, it is rolled back, so the next prompt starts from a clean
context. When `max_turns` is hit, the partial transcript is kept so you can
inspect it, and `/undo` removes it. With `--approval prompt`, approval
questions are asked inside the REPL.

From Python, `aircd.bionic_repl.BionicRepl` takes an `input_fn` and `out`
stream to drive the same session programmatically. The building blocks are the
`messages=` (caller-owned conversation list) and `before_tool=` (a hook that
receives a `ToolStep`, can edit `step.arguments`, call `step.execute()`, return
a replacement result, or raise `RunAborted`) parameters of
`aircd.bionic.run_agent`.

## Python SDK (`BionicAgent`)

For programmatic workflows, `aircd.BionicAgent` (also importable from
`aircd.client` and `aircd.bionic`) wraps the same tool-calling loop the CLI
uses, without spawning a subprocess:

```python
from aircd import BionicAgent, BionicError

agent = BionicAgent(model="auto", cwd="path/to/repo", approval="read-only", keep_session=True)

result = agent.run("Summarize README.md")          # RunResult(content, model, turns, tool_calls, session_id)
follow = agent.run("Now list its section headings")  # continues the same conversation
print(follow.content, agent.session_id)

for event in agent.stream("Explain src/main.rs"):  # SSE streaming, events as they happen
    if event["type"] == "token":
        print(event["text"], end="", flush=True)
    elif event["type"] == "result":
        final = event["result"]
```

- **Options** are the `BionicConfig` field names (`model`, `base_url`,
  `api_key`, `cwd`, `workspace_root`, `approval`, `max_turns`, `timeout`,
  `command_timeout`, `max_output_chars`, `temperature`, `max_tokens`,
  `system_prompt`, `session_dir`, `stream`, `stream_format`, …) plus
  `system_prompt_file`. They are validated and resolved exactly like CLI flags
  (argument → `BIONIC_*` environment variable → default); unknown names raise
  `TypeError`, invalid values raise `BionicError`. Pass `config=BionicConfig(...)`
  instead to skip resolution.
- **Model**: `agent.model` resolves `auto` through `/models` once and caches it
  for every later run. `agent.list_models(include_all=False)` queries the server.
- **Sessions**: `keep_session=True` carries the conversation across `run()`
  calls (saved under `session_dir` like `--session-id`) and keeps
  `agent.session_id` current; `agent.new_session()` starts over. `run(...,
  session_id="id")` resumes a specific saved session for one call.
- **Events**: `on_event=` (agent-wide, or per `run()` call) receives the event
  dicts described under [Streaming](#streaming---stream); `run(..., stream=True)`
  adds `token`/`reasoning` events. `agent.stream(prompt)` runs on a background
  thread, yields the events, finishes with `{"type": "result", "result":
  RunResult}`, and raises any error from the iterator. Closing the iterator early
  cancels the run at its next event.
- **Approval**: with `approval="prompt"`, pass `approver=lambda name, args: ...`
  to approve mutating tool calls in code; without one, the terminal is asked.
- `agent.with_options(max_turns=5, approval="auto")` returns a re-validated copy
  that keeps handlers, the session and the cached model.
- **Plugins**: `tools_dirs=[...]` and `plugins=False` mirror `--tools-dir` and
  `--no-plugins`; `agent.tools()` returns the `--list-tools` descriptions.

See `clients/python/examples/bionic_workflow.py` for a runnable example.

`RunResult.usage` holds the token totals (`prompt_tokens`, `completion_tokens`,
`total_tokens`) summed over the turns whose response carried a `usage` object, and
`RunResult.turn_usage` holds the per-turn values. It is `{}` when the server sends no
usage. Streamed requests ask for usage with `"stream_options": {"include_usage": true}`.
`--json` output includes `usage`. When a run runs out of turns it raises
`MaxTurnsError` (a `BionicError`), which carries `turns`, `tool_calls`, `usage`
and `turn_usage` for the work done so far.

## Custom tools (plugins)

A plugin is a Python file that adds tools the model can call next to the five
built-ins. `airc-bionic` loads every `*.py` file in
`<workspace-root>/.aircd/tools/` (when that directory exists) and then in each
`--tools-dir` / `BIONIC_TOOLS_DIR` directory. Files whose names start with `_`
or `.` are skipped, so they can hold shared helpers. Plugins are loaded again at
the start of every run.

Each file defines either `TOOLS`, a list of tool definitions, or `TOOL`, a
single definition:

```python
# .aircd/tools/word_count.py
def count_words(args, ctx):
    text = ctx.resolve(args["path"]).read_text(encoding="utf-8")
    return {"path": args["path"], "words": len(text.split())}

TOOLS = [
    {
        "name": "count_words",
        "description": "Count whitespace-separated words in a workspace file.",
        "parameters": {
            "type": "object",
            "properties": {"path": {"type": "string", "minLength": 1}},
            "required": ["path"],
            "additionalProperties": False,
        },
        "handler": count_words,
        "mutating": False,
    }
]
```

| Key | Required | Meaning |
| --- | --- | --- |
| `name` | yes | Function name, matching `^[A-Za-z0-9_-]{1,64}$`. It must not clash with a built-in tool or another plugin. |
| `description` | yes | Non-empty text shown to the model. |
| `parameters` | yes | JSON Schema with `"type": "object"` for the arguments (supported subset below). |
| `handler` | yes | `handler(args, ctx)`, called with the validated arguments dict and a `ToolContext`. |
| `mutating` | no (default `true`) | Whether the tool changes anything. Mutating tools are hidden and refused in `read-only` mode and are asked about in `prompt` mode, like `write_file`. |

**Strict validation at load time.** The whole load fails, and the run stops
before it contacts the server, if any plugin breaks one of these rules. The
error names the file:

- the file can't be imported, raises an error, or calls `exit()`
- it defines neither `TOOLS` nor `TOOL`, or both, or `TOOLS` is empty
- a definition has unknown or missing keys, a bad or duplicate name, or a
  handler that can't take `(args, ctx)`
- `parameters` isn't plain JSON, or uses a keyword outside the supported subset

The supported keywords are listed below; anything else, such as `oneOf`,
`$ref` or `format`, is refused instead of being silently ignored.

| Type | Keywords |
| --- | --- |
| any | `type` (required, one string), `description`, `title`, `enum`, `default` |
| `string` | `minLength`, `maxLength`, `pattern` |
| `integer` / `number` | `minimum`, `maximum` |
| `array` | `items`, `minItems`, `maxItems` |
| `object` | `properties`, `required` (names must be declared), `additionalProperties` (`true`/`false`) |

`enum` values and `default` must match the declared type, and schemas can nest
at most 16 levels deep. Run `airc-bionic --list-tools` to check your plugins.
It prints `{"dirs": [...], "tools": [{"name", "description", "source", "mutating", "parameters"}, ...]}`
for the current approval mode and exits with code 1 and the error if a plugin is invalid.

**Argument validation at call time.** Before a handler runs, the arguments the
model sent are checked against `parameters`. The checks cover types (`true` is
not an integer, and `3.0` counts as one), required properties, `enum`,
lengths, ranges, `pattern`, array items, and `additionalProperties: false`.
Extra properties are allowed unless you set it, as in JSON Schema. An invalid
call returns `{"error": "invalid arguments for <tool>: $.path is required; ..."}`
to the model so it can retry. The handler is not called, and in `prompt` mode
the operator is not asked.

**Handler contract.**

- `ctx.cwd`, `ctx.workspace_root`, `ctx.command_timeout`, `ctx.max_output_chars`
  and `ctx.log` describe the run.
- `ctx.resolve(path)` resolves a path inside the workspace jail. A path outside
  the root is reported to the model as a denied call, the same way built-in
  tools report it.
- A `str` return value is sent to the model unchanged. Any other value is sent
  JSON-encoded. Output longer than `--max-output-chars` is truncated.
- An exception, including `SystemExit`, becomes `{"error": "<tool> failed: <Type>: <message>"}`.
  A value that can't be JSON-encoded is reported the same way.

The system prompt lists the plugin tool names. `--verbose` logs which
directories and tools were loaded.

## Benchmark and multi-turn evaluation (`airc-bionic-bench`)

`airc-bionic-bench` (`clients/python/aircd/bionic_bench.py`, or `.\airc-bionic-bench.cmd`)
runs the tool-calling loop through scripted scenarios against a local model. It
measures latency, memory and token efficiency and checks each result:

```powershell
airc-bionic-bench --model auto --output bench.json            # all built-in scenarios
airc-bionic-bench --model auto --stream --repeat 3 --tag multi-turn
airc-bionic-bench --list-scenarios
```

Each scenario run gets a fresh temporary workspace, seeded with files, that the
tools are jailed to. The prompts run in order in one saved session, so a later
prompt can depend on an earlier one. Then deterministic checks decide pass or fail.

| Scenario | Tags | What it exercises |
| --- | --- | --- |
| `read-answer` | single-turn, read | read a file, report a value |
| `write-file` | single-turn, write | create a file with exact contents |
| `edit-config` | single-turn, edit | change one value, leave the rest of the file intact |
| `command-output` | single-turn, command | run a script and report its output |
| `aggregate-files` | multi-step | discover and read several files, combine, write the result |
| `fix-and-test` | multi-step | fix a bug and confirm it with the unit tests (checked by re-running them) |
| `session-recall` | multi-turn, memory | use a fact from prompt 1 in prompt 2 |
| `session-followup` | multi-turn | three prompts that refer back to "the file you just created" |

Before the first scenario, the model is checked with the same logic as `--health`
(exit **2** if it is not ready). It is also warmed up with a one-token probe, so the
first run doesn't include load time (`--no-warmup` keeps load time in).
`--auto-pull` pulls and loads the model first.

Metrics per run and aggregated (`summary`), with mean / p50 / p95 / min / max:

- **Latency**: scenario wall time, each LLM call (from the request to the first
  tool call or final answer), time to first token (with `--stream`), and tool
  execution time, overall and per tool.
- **Tokens**: prompt and completion totals from the server's `usage`, prompt
  tokens per turn (context growth across a multi-turn run), completion tokens
  per second, `tokens_per_run_mean`, and `tokens_per_pass` (all tokens spent,
  failed runs included, divided by passed runs; lower is better). Runs that end
  at `--max-turns` still count their tokens.
- **Memory**: resident memory of the benchmark process, and with
  `--server-pid <pid>` (repeatable) of the inference server processes. Memory is
  sampled in the background every `--sample-interval` seconds (start / peak /
  mean / end / delta). It doesn't cover GPU VRAM or a server on another machine.
- Pass rate overall and per scenario, turns and tool calls per run, tool errors.

The full JSON report goes to stdout or `--output`. A readable summary and
per-run progress go to stderr (`-q` silences them). Exit codes: **0** done, **1**
configuration error, **2** model not ready, **3** pass rate below
`--min-pass-rate` (use this as a CI gate). Connection flags are the same as for
`airc-bionic`: `--base-url`, `--api-key`, `--model`, `--max-turns`, `--timeout`,
`--command-timeout`, `--temperature`, `--max-tokens`, and the matching `BIONIC_*`
variables. Tools always run in `auto` approval inside the throwaway workspace.

Custom scenarios go in a JSON file passed with `--scenarios-file`. They replace
the built-ins unless you add `--with-builtin`. `{python}` in prompts and commands
expands to the current interpreter:

```json
{"scenarios": [{
  "name": "rename-fn", "tags": ["edit"],
  "files": {"app.py": "def old():\n    return 1\n"},
  "prompts": ["Rename the function old to new in app.py."],
  "checks": [
    {"type": "file_contains", "path": "app.py", "text": "def new("},
    {"type": "command", "command": "{python} -c \"import app; app.new()\""}
  ]
}]}
```

Check types: `answer_contains` (`text`, optional `case_sensitive`),
`answer_matches_any` (`texts`: the answer is one of them, or contains one as a
word), `file_equals` (compares after trimming trailing whitespace; line endings
don't matter), `file_contains`, `file_absent`, and `command` (`expect_exit`,
default 0, and `timeout`). Answer checks use the last prompt's answer unless
`prompt` sets an index. `--keep-workspaces` keeps each workspace for inspection.

## Client template (`docs/templates/bionic.json`)

Register the preset on your hub (owner key required):

```powershell
airc template set bionic --file docs/templates/bionic.json --invite <owner-key>
airc template models bionic --invite <owner-key>
```

`docs/templates/bionic.json` uses `"models_command": ["airc-bionic", "--list-models"]`,
which queries `{base_url}/models`, filters out embedding models, and returns the
available chat/tool models to the hub and TUI model picker.

If `airc-bionic` is not on the hub service's `PATH`, set `command` and the first
entry of `models_command` to the full path of `airc-bionic` (or set `command`
to your `python` executable and prepend `"-m", "aircd.bionic"` to `args` and
`models_command`). If your Bionic server runs on a non-default port or host,
set `"env": {"BIONIC_BASE_URL": "http://127.0.0.1:1235/v1"}` on the template.

Spawn a worker account from the template:

```powershell
airc template spawn bionic bionic-worker-1 --model <model-id> --channel "#my-project" --invite <owner-key>
```

Save the printed one-time token as the worker's `airc` profile so `run_command`
tool calls such as `airc --profile bionic-worker-1 inbox` authenticate automatically:

```powershell
airc --server http://127.0.0.1:6680 --token <token> --profile bionic-worker-1 login --channels "#my-project"
```

## Using Bionic with CAMS groups

Because `aircd` passes the resolved group role prompt (`lead_prompt` or
`worker_prompt`) as `{prompt}` and launches the runner inside the task's
repository (`{repo_path}`), local tool-calling models can fill both worker and
lead seats in a CAMS group:

```json
{
  "name": "local-dev",
  "description": "Local tool-calling Bionic lead and workers",
  "lead": {"template": "bionic", "model": "chosen-lead-model", "nick": "bionic-lead"},
  "workers": [
    {"template": "bionic", "model": "chosen-worker-model", "count": 1, "nick": "bionic-worker-1"},
    {"template": "bionic", "model": "chosen-worker-model", "count": 1, "nick": "bionic-worker-2"}
  ],
  "lead_prompt": null,
  "worker_prompt": null
}
```

1. Spawn each named seat from the `bionic` template and save its `airc` profile.
2. Create and attach the group with role prompts from `docs/prompts/`:

   ```powershell
   airc group create local-dev --file local-dev.json --lead-prompt "@docs/prompts/group-lead.md" --worker-prompt "@docs/prompts/group-worker.md" --invite <owner-key>
   airc group attach local-dev "#my-project" --invite <owner-key>
   ```

3. When assigned a task or subtask, `aircd` wakes the seat with `airc-bionic --model <model> -p "<expanded role prompt>"`. The local model calls `run_command`, `read_file`, `apply_patch`, `edit_file`, and `write_file` to inspect the task, edit the repo on a branch, run tests, commit with the task ID, and submit for review via `airc task status <id> review`.

### Optional conversation resume / PM mode

For coordinator or PM accounts that should continue a conversation across wakes,
configure `session_mode` as `"resume"` or `"pm"` with `session_marker: "session_id:"`
and `resume_args`:

```json
{
  "args": ["--model", "{model}", "--emit-session-id", "-p", "{prompt}"],
  "session_mode": "pm",
  "session_marker": "session_id:",
  "session_scope": "channel",
  "resume_args": ["--model", "{model}", "--session-id", "{session_id}", "-p", "{prompt}"]
}
```

`airc-bionic` stores the conversation transcript under `--session-dir` (or
`BIONIC_SESSION_DIR`) and emits `session_id: <id>` on `stderr` for `aircd` to
store and pass back on subsequent wakes.

## Security considerations

`airc-bionic` runs tool calls with the OS permissions of the hub/runner
process. Only attach it to trusted projects, use dedicated worker profiles, and
adjust `--max-turns` and `--command-timeout` as needed for your workload.

### Workspace root jail

Every tool call is confined to the workspace root (`--workspace-root`, default
the working directory, which `aircd` sets to the task's `{repo_path}`):

- `read_file`, `write_file`, `edit_file` and `apply_patch` resolve the requested path fully,
  collapsing `..` and following symlinks and Windows junctions, then refuse it
  unless the result is inside the root. Absolute paths, `~`, drive-relative
  (`D:x`) and rooted (`\x`) Windows paths, and links that point out of the
  repository are all denied.
- `run_command` is checked before it runs. The executable in each pipeline
  segment may be anywhere (`python`, `git`, `airc`, an absolute interpreter
  path), but every argument, redirect target and `--flag=value` that looks like
  a path must resolve inside the root. Commands that use `..` to leave the root,
  absolute or `~` paths outside it, `$VAR`/`%VAR%` paths that expand outside it,
  or command substitution (`$(...)`, backticks) are refused. URLs and device
  paths such as `/dev/null` and `nul` are allowed, as are `cmd.exe` switches
  such as `/b`.

A denied call returns `{"error": "Denied: ...", "denied": true}` to the model so
it can correct itself; nothing runs or is written. The command check reads the
command text without running it, so code an interpreter runs (for example
`python -c "..."`) can still reach outside the root. For untrusted models, also
run the worker as a restricted OS user or inside a container.

### Tool approval modes

| Mode | Behavior |
| --- | --- |
| `auto` (default) | Calls that pass the jail check run without asking. Use for unattended `aircd` wakes. |
| `prompt` | Before each `run_command`, `write_file`, `edit_file` or `apply_patch`, asks `Allow ...? [y/N]` on the controlling terminal (`/dev/tty` or `CONIN$`). If no terminal is attached, the call is denied. Reads never prompt. |
| `read-only` | Only `read_file` is offered to the model; mutating tools are refused. Use for review or analysis seats. |

Plugin tools follow their `mutating` flag in every mode. In `read-only` mode,
`airc-bionic` doesn't load `<workspace-root>/.aircd/tools` at all, so a review
seat never imports code from the repository it is reviewing. Directories passed
with `--tools-dir` / `BIONIC_TOOLS_DIR` are still loaded, and only their
non-mutating tools are offered.

Set the mode per template with `"env": {"BIONIC_APPROVAL": "read-only"}` or by
adding `--approval <mode>` to `args`.

### Plugin trust

A plugin is ordinary Python. It runs inside the runner process with the
runner's permissions as soon as the file is imported. Only the paths a handler
passes through `ctx.resolve` are jailed; a handler that opens files or starts
processes directly is not. Treat `.aircd/tools/` like any other executable code
in the repository. Use `--no-plugins` for repositories you don't trust.
