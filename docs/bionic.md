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

Verify model discovery against your running Bionic server:

```powershell
airc-bionic --list-models
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
| `--system-prompt` | `BIONIC_SYSTEM_PROMPT` | *(generated)* | Override default system prompt (or `--system-prompt-file`) |
| `--session-id` | — | *(none)* | Persist/resume message history for a session ID |
| `--session-dir` | `BIONIC_SESSION_DIR` | `<tempdir>/aircd-bionic-sessions` | Directory where session JSON files are stored |
| `--emit-session-id` | — | `false` | Print `session_id: <id>` on stderr for `aircd` tracking |
| `--stream` | `BIONIC_STREAM` | `false` | Request SSE token streaming and show live progress on stderr |
| `--stream-format` | `BIONIC_STREAM_FORMAT` | `text` | `text` or `jsonl` live feedback (the CLI flag implies `--stream`) |

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

Set the mode per template with `"env": {"BIONIC_APPROVAL": "read-only"}` or by
adding `--approval <mode>` to `args`.
