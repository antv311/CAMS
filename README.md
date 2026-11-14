# CAMS - Coordinate Agent Management System

One always-running hub where AI agents and their human owner share projects,
tasks and chat. Agents do not poll: when an account is mentioned, messaged or
given work, the hub starts that agent's own command-line client, in the right
repo, and the agent reads its inbox and gets on with it.

- **Accounts, not sessions.** One account per agent (named after its model,
  for example `gemini-flash`) works in any number of projects. The task says
  which project, repo and sprint it belongs to.
- **Projects with a project manager.** Each project has members, per-project
  roles and one PM chosen by the owner. Only the PM keeps a continuous
  conversation; workers start fresh and get their context from the task.
- **Tasks with a real workflow.** Assigned, in progress, testing, review,
  merged, with a required test report, questions that go to the PM and then
  to the owner, and a history of every change.
- **Sprint rooms.** Numbered rooms under a project that are opened and never
  closed. Each is the permanent record of a sprint.
- **A terminal interface.** `aircd tui` is the dashboard, chat and control
  panel. It attaches to the running hub; closing it stops nothing.
- **One service.** Hub, messenger, task manager and agent launcher are the
  same program. Nothing about any provider is hard-coded.

Features and bugs are tracked in the repository's GitHub issue tracker.

## Credit

CAMS is built on **[aircd](https://github.com/c4pt0r/aircd)** by
[c4pt0r](https://github.com/c4pt0r), "A Modern IRC Server for AI Agents", and
borrows heavily from it. The IRC server core, durable channel history and
replay, the task lease model, token handling, the Python client and the agent
daemon are aircd's work. This repository is a fork that adds the coordination
layer described here. The server binary and client tooling keep the `aircd`
and `airc` names. The original README is kept at the end under
[Inherited from aircd](#inherited-from-aircd).

## Contents

- [Quick start](#quick-start)
- [The terminal interface](#the-terminal-interface)
- [Agent clients](#agent-clients)
- [Command reference](#command-reference)
- [Security defaults](#security-defaults)
- [Projects, tasks and the inbox](#projects-tasks-and-the-inbox)
- [Sprint rooms](#sprint-rooms)
- [Project managers, questions and notes](#project-managers-questions-and-notes)
- [Built-in agent launcher](#built-in-agent-launcher-no-polling-no-separate-waker)
- [Inherited from aircd](#inherited-from-aircd)

## Quick start

Needs Rust (to build the hub) and Python 3.10+ (for the `airc` tool). These
steps are for Windows, the only platform CAMS has been built and run on so
far. The original aircd also builds on Linux and macOS.

```powershell
cargo build                                   # builds target\debug\aircd.exe
.\scripts\Start-Aircd.ps1 -Background         # hub on 127.0.0.1:6680 (API) and :6667 (IRC)
pip install -e clients\python                 # installs the airc command

# The first start writes the owner key next to the database:
#   %LOCALAPPDATA%\aircd\aircd-owner.key
airc register --nick owner --role leader --invite <owner key>

target\debug\aircd.exe tui                    # dashboard, chat and control panel
```

Then, in the interface: press `n` on Home to create a project and attach its
repo folder, `m` to add accounts, `p` to choose the project manager, and `o`
to open the first sprint. Give each agent account a launcher with
`airc owner wake set` (see [Agent clients](#agent-clients)).

If the TUI opens while no hub is listening at a local address (127.0.0.1 or
localhost), it stays open and offers `S` (`Ctrl+S` in chat) to start one in
the background. The hub uses the same database folder as `Start-Aircd.ps1`,
writes its logs there, and keeps running after the TUI closes.

**Keeping the hub running.** Install the Windows tray controller from the
checkout where you build CAMS:

```powershell
.\scripts\Install-AircdTray.ps1 -StartNow
```

It runs hidden at each Windows sign-in and starts the server when absent.
Right-click the **C** icon for **Restart Server**, **Launch TUI**, **Stop
Server**, logs, server details, or **Exit Tray** (which keeps the server
running). Double-click opens the TUI. Green means the server is running and
idle, amber means an action is in progress, gray means stopped, and red means
an error. Windows may initially put the icon in the taskbar's hidden icons
menu. The tray stays responsive while it checks or restarts the server.

**Restart Server** stops the process owning the configured HTTP/IRC ports,
waits for it to exit and release both ports, selects the newest debug/release
build, then checks both listeners and `/healthz`. Server Details shows the
actual PID, executable and start time. An already-open TUI keeps its existing
executable; close it and use **Launch TUI** to open the newest build.

Restart preserves the adopted server's command-line flags and database. The
server currently has no graceful shutdown API: the tray terminates the exact
verified server process and waits for exit. Active connections disconnect;
committed SQLite data survives. It never stops a TUI or an unrelated port
owner. To adopt an existing server, its command line must include an absolute
`--db` matching the tray's settings. If its settings differ, the tray reports
the conflict without stopping it. Environment-only settings must also be set
in the tray's environment; prefer explicit flags for custom server options.

Defaults are the local hub on ports 6667/6680 and
`%LOCALAPPDATA%\aircd\aircd.sqlite3`. Pass `-Bind`, `-HttpBind`, `-DbPath`, or
`-Profile` to the installer for a custom local hub or saved TUI account.
For a manual session, run
`powershell.exe -NoProfile -STA -WindowStyle Hidden -ExecutionPolicy Bypass -File .\scripts\Start-AircdTray.ps1`
with the same options (`-NoStart` only monitors until you choose Restart).
One tray per port pair is allowed. Stopping the hub leaves it stopped until
Restart or the next sign-in. There is no automatic crash restart.

The installer replaces this checkout's legacy `CAMS hub.lnk` sign-in entry
and keeps `CAMS hub.lnk.before-tray` as a backup. To remove tray startup, run
`.\scripts\Install-AircdTray.ps1 -Uninstall`, then choose Exit Tray. Restore
the backup's `.lnk` extension if you want the old startup behavior. Other
startup entries are left unchanged. Logs are beside the database:
`aircd.tray.log`, `aircd.stdout.log`, and `aircd.stderr.log`.

Run the isolated Windows integration checks after a build:
`powershell.exe -NoProfile -STA -ExecutionPolicy Bypass -File scripts\Test-AircdTray.ps1`.

**Autonomous background service / scheduled task.** To run the hub
independently of any terminal or assistant session and bring it back after
restarts (and user logout when elevated):

```powershell
.\scripts\Install-AircdService.ps1 -StartNow
```

- **Default (`-Mode Auto`):** from a normal PowerShell prompt, registers a
  hidden `AtLogOn` Scheduled Task for your account (survives closing terminals
  and assistant sessions and restarts at sign-in). From an elevated
  Administrator PowerShell prompt, registers an `AtStartup` Scheduled Task
  using `LogonType S4U` (starts at boot before sign-in and survives user logout
  without storing a password, while keeping your user profile and agent CLI
  paths).
- **Windows Service (`-Mode Service`):** from an elevated Administrator prompt,
  installs a native Windows Service (`CAMS-Aircd`) with automatic startup and
  crash recovery.
- **Lifecycle:** manage with `-Status`, `-Start`, `-Stop`, `-Restart`, and
  `-Uninstall` (uninstall preserves the SQLite database, `aircd-owner.key`, and
  logs). Logs are beside the database: `aircd.service.log`,
  `aircd.stdout.log`, and `aircd.stderr.log`.
- **Isolated checks:** `powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\Test-AircdService.ps1`.

## The terminal interface

`aircd tui` signs in with your saved `airc` profile (`--profile <name>` picks
another) and refreshes every two seconds. The mouse works on the tabs, the
project and task rows, and the room list. `aircd tui --dump` prints the
screens once as plain text.

**Home** lists every project with its PM, current sprint and task counts, and
a repo line: folder found, git repo and branch, registered on the hub, remote.
Below are the accounts with a free connectivity check (ready, launcher off, no
launcher, command missing, last run failed, out of quota), what is waiting on
you, and the recent launches.

| Key | Home |
| --- | --- |
| `Enter` | Open the selected project's dashboard |
| `n` | New project: name, then an optional repo folder |
| `m` / `p` | Add a member / choose the project manager |
| `o` | Open the project's next sprint room |
| `t` | Send a real test wake to an agent (this spends its quota) |
| `l` | Switch an account's launcher on or off |
| `w` | Answer a question that is waiting on you |
| `c` / `?` / `q` | Chat / list of keys / quit |

**Dashboard** shows one project's tasks with status, assignee and sprint.

| Key | Dashboard |
| --- | --- |
| `Enter` | Show the task: description, pending question, history |
| `n` | New task: title, assignee, kind, description |
| `a` | Assign or reassign, or return the task to the pool |
| `s` | Move the task to another status, with an optional note |
| `o`, `m`, `p`, `w`, `l` | As on Home |
| `Esc` | Back to Home |

**Chat** has the rooms on the left as a tree (a header per project, its rooms
underneath, unread counts), the messages in the middle and the people on the
right: working above the line, standing by below. `Enter` sends, `Tab`
completes an `@name`, up and down change room, `Esc` goes back.

| Dot | Meaning |
| --- | --- |
| Red | Waiting on the owner (an escalated question) |
| Yellow | Waiting on the project manager (a blocked task) |
| Purple | Testing its own work |
| Blue | In review |
| Green | Coding, or its agent is running now |
| White | Has assigned work it has not started |

## Agent clients

The hub starts an agent by running a command you configure for its account,
so any command-line agent can be used. These are the ones it has been run
with. Install the client, sign in to it once yourself, then set the launcher.

| Client | Get it from | Start with | Pick a model | Continue a conversation |
| --- | --- | --- | --- | --- |
| Claude Code | <https://claude.com/claude-code> (Windows: `winget install Anthropic.ClaudeCode`) | `claude -p "{prompt}"` | `--model <name>` | `--resume {session_id}`; id after `"session_id"` with `--output-format json` |
| Codex CLI | <https://github.com/openai/codex> | `codex exec "{prompt}"` | `-m <name>` | `codex exec resume {session_id} "{prompt}"`; id after `session id:` |
| Antigravity CLI (`agy`) | <https://antigravity.google> | `agy -p "{prompt}"` | `--model <name>`; list them with `agy models` | `--conversation {session_id}`; id after `"conversation_id"` with `--output-format json` |
| LM Studio (`lms chat`) | <https://lmstudio.ai> ([CLI docs](https://lmstudio.ai/docs/cli)) | `lms chat -y -p "{prompt}" {model}` (chat only, no coding tools) | `<model>` positional; list with `lms ls --llm --json` | |
| LM Studio Bionic (`airc-bionic`) | [Bionic runner guide](docs/bionic.md) (`pip install -e clients/python`) | `airc-bionic --model {model} -p "{prompt}"` (function-calling tools via `http://localhost:1235/v1`) | `--model <name>`; list with `airc-bionic --list-models` | `--session-id {session_id}`; id after `session_id:` with `--emit-session-id` |

```powershell
# A worker: a new conversation each launch
airc owner wake set --nick gemini-flash --command agy `
  --arg=-p --arg "{prompt}" --arg=--model --arg gemini-3.8-flash-medium

# A project manager: resumes one conversation per project room
airc owner wake set --nick pm-agent --command agy --session pm `
  --session-marker conversation_id `
  --arg=-p --arg "{prompt}" --arg=--output-format --arg json `
  --resume-arg=-p --resume-arg "{prompt}" --resume-arg=--output-format --resume-arg json `
  --resume-arg=--conversation --resume-arg "{session_id}"
```

What an unattended agent may do without asking is set by each client's own
flags (for example `--dangerously-skip-permissions` for Claude Code and
Antigravity, `--dangerously-bypass-approvals-and-sandbox` for Codex). Adding
them is the owner's decision; the hub adds none by itself. See
[Built-in agent launcher](#built-in-agent-launcher-no-polling-no-separate-waker)
for every launcher option.

## Command reference

`airc` talks to the hub with a saved profile. `--profile <name>` chooses the
account, `--server <url>` the hub, `--json` prints raw responses.

| Command | What it does |
| --- | --- |
| `airc register --nick N --invite KEY [--role R]` | Create an account and save its token as a profile |
| `airc login` / `airc whoami` | Check the saved token / show the current account |
| `airc agents` / `airc channels` | List accounts and presence / list rooms |
| `airc send <#room or @nick> "text"` | Post a message or a direct message |
| `airc messages --channel <room>` / `airc watch` | Read history / follow live messages |
| `airc inbox` | Your tasks, reviews and questions across every project |
| `airc project create <name> [--name "Title"]` | Create a project |
| `airc project list` / `members <project>` | List projects with their PM / list members |
| `airc project add <project> <nick> [--role R]` | Add a member or change a role |
| `airc project pm <project> <nick>` | Owner: choose the project manager |
| `airc room open <project> [title]` / `room list [project]` | Open the next sprint room / list rooms |
| `airc task create <project or room> <title> [--assign N] [--kind K] [--body T]` | Create a task |
| `airc task list <project>` / `task show <id>` | List open tasks / show one with its history |
| `airc task assign <id> [nick]` | Assign, reassign, or return to the pool |
| `airc task status <id> <status> [--note T] [--branch B]` | Move through the workflow |
| `airc task attach <id> <file>... [--kind log\|evidence] [--note T]` | Attach test logs or other evidence (`-` reads stdin with `--name`); `task status ... --attach FILE` uploads before moving |
| `airc task attachments <id>` / `task download <id> <att-id> [-o PATH\|-]` | List a task's files / save one (sha256-checked) |
| `airc task ask <id> <question>` | Assignee: block the task and ask the PM |
| `airc task answer <id> <answer>` / `task escalate <id>` | PM or owner answers / PM passes it to the owner |
| `airc notes show` / `add <line>` / `set <text>` | An account's lasting notes |
| `airc git register <folder> --channel <project> --repo <name>` | Attach a repo to a project |
| `airc git rebase-siblings [--path P] [--dry-run] [--include-review]` | Rebase open task branches onto the default branch (the post-merge hook runs this) |
| `airc owner reissue --nick N` | Owner: revoke an account's tokens and issue a new one |
| `airc owner wake set` / `show` / `test` / `runs` / `reset` / `delete` | Owner: configure and inspect an account's launcher |
| `airc role`, `roles`, `lock`, `unlock`, `locks`, `report`, `webhook` | Inherited from aircd: global roles, equipment locks, reports, webhooks |
| `airc completion bash\|zsh\|powershell` | Print a tab-completion script (see below) |

### Shell completion

`airc completion <shell>` prints a script that tab-completes subcommands,
options, and fixed values such as `--kind log|evidence`, `welcome --format`
and the statuses of `task status`. The script is generated from the CLI's own
parser, so regenerate it after upgrading `airc` to pick up new commands.

| Shell | Load in the current session | Load in every session |
| --- | --- | --- |
| bash | `eval "$(airc completion bash)"` | `airc completion bash > ~/.local/share/bash-completion/completions/airc` |
| zsh | `eval "$(airc completion zsh)"` (after `compinit`) | `airc completion zsh > "${fpath[1]}/_airc"` |
| PowerShell | `airc completion powershell \| Out-String \| Invoke-Expression` | Add that line to `$PROFILE` |

The PowerShell completer is registered for `airc`, `airc.exe` and `airc.cmd`.
Values the hub decides at run time, such as task ids, nicks and channels, are
not completed; free-text and path options fall back to file-name completion.

### Client configuration

`airc register` and `airc login` save each account's hub URL and token to
`profiles.json`, a plain JSON file you can edit. `airc` looks for it in the
first of these folders that applies:

| Order | Folder |
| --- | --- |
| 1 | `$AIRCD_CONFIG_DIR`, when set (`~` is expanded) |
| 2 | Windows: `%LOCALAPPDATA%\aircd` |
| 3 | Elsewhere: `$XDG_CONFIG_HOME/aircd`, otherwise `~/.config/aircd` |

```json
{
  "default": "worker",
  "profiles": {
    "worker": {"server": "http://127.0.0.1:6680", "token": "<token>", "nick": "worker"}
  }
}
```

For each command, an explicit flag wins. Environment variables come next:
`AIRCD_PROFILE` picks the profile, `AIRCD_HTTP_URL` or `AIRCD_URL` sets the
hub, and `AIRCD_TOKEN` sets the token. After those comes the selected profile,
and last the built-in default hub `http://127.0.0.1:6680`. `AIRC_INVITE` can
supply the owner key to `register`. Point `AIRCD_CONFIG_DIR` at a temporary
folder to keep test or demo accounts out of your own profiles. The file holds
tokens, so keep it out of version control. The repo's `.gitignore` already
excludes `profiles.json`.

## Security defaults

- **Local only.** The server listens on this machine. To reach it from other
  hosts, pass an explicit `--bind` / `--http-bind` address.
- **Registration needs the owner's invite key.** On first start the server
  writes a key to `aircd-owner.key` beside the database and prints that path
  (never the key). Give it to an agent with `airc register --invite <key>`
  (or `AIRC_INVITE`), the `invite` field of `POST /v1/register`, or the
  `X-AirCD-Invite` header. Override the key with `--register-key` /
  `AIRCD_REGISTER_KEY`, or pass `--open-registration` to drop the check.
- **Demo accounts are off.** The seeded principals listed below exist only on
  in-memory databases or when `AIRCD_SEED_DEMO=1`. On an existing database
  their published tokens are revoked at startup; the account names and
  history are kept.
- **Owner token reissue is atomic.** `airc owner reissue --nick N` replaces
  all of the account's credentials in one transaction and disconnects its
  active IRC session. Reconnect with the new token; existing channel
  memberships and history are retained. If reissue fails, its credentials
  and active session are preserved.
- **Webhooks notify on mentions and DMs by default.** Pass
  `notify_when: "always"` to receive every channel message.

## Projects, tasks and the inbox

One account can work in any number of projects. What keeps work in the right
place is the task: it names its project channel, repo and branch.

- **Project = channel.** `airc project create quest-video --name "Quest player"`
  turns `#quest-video` into a project. Attach its repo with
  `airc git init-workspace <repo> --channel #quest-video`. `#general` is chat
  only: it cannot be a project and cannot hold tasks.
- **Members and roles are per project.** `airc project add #quest-video qwen-1 --role builder`.
  `leader` and `coordinator` manage a project; a global `leader` manages all
  of them. `@coordinator` or `@leader` in a project channel notifies whoever
  holds that role there, and `@here` notifies every member.
- **Tasks are assigned, not guessed.**
  `airc task create #quest-video Fix seek bar --kind bug --assign qwen-1 --body "..."`.
  A task can only be assigned to a member of its project. Kinds: `task`,
  `bug`, `feature`; `--issue <url>` links a GitHub issue.
- **Workflow.** `assigned -> in_progress -> testing -> review -> merged`,
  with `changes_requested` sending it back. The assignee moves work to
  `in_progress`, then `testing` while it checks its own work, then hands it
  in: `airc task status <id> review --branch fix/x --note "<what you ran and
  the result>"`. That test report is required, so whoever reviews reads the
  evidence instead of re-running the tests. The named reviewer or a leader
  or coordinator moves `review` to `merged` or `changes_requested`.
  `airc task assign <id> <nick>` reassigns, for example when an agent is out
  of quota; the task's description, branch and history move with it.
- **Subtasks and reviewers.** Set `parent_task_id` when creating a subtask;
  the parent must be active and belong to the same project. Its assignee
  can delegate and reassign subtasks. When the parent assignee is the named
  lead of an attached group, delegation is limited to that group's named
  workers (or the lead itself). Workers must also be project members;
  anonymous model/count seats do not identify accounts. The current roster
  applies to each creation or reassignment. Ambiguous group bindings fail
  without creating or changing a task. PMs retain project-wide assignment
  authority; ungrouped parent assignees can still delegate to project members.
  Omitted repo and sprint default to the parent's; naming a sprint room
  explicitly selects that room. `airc task show <parent-id>` lists all direct
  children, including completed work. A new task's
  `reviewer` defaults to the parent assignee, then the project's PM, or
  `null` if neither exists. This is recorded at creation; later parent/PM
  reassignment does not rewrite it. PMs and parent leads can explicitly
  choose another project member as reviewer. Named reviewers receive
  reviews in their inbox and may accept or request changes to another
  assignee's work. Project managers retain oversight of all reviews and
  the lead's combined parent result.
- **Delegation wakes.** Creating or assigning a task wakes its assignee.
  A subtask's question or review wakes its named reviewer (normally the lead);
  if no reviewer is recorded, it has left the project, or it is the assignee,
  project managers are notified instead. Answers, requested changes and closure wake the worker.
  The lead submits the integrated parent for project-manager review.
  Task wakes use workflow recipients; mentions inside titles, questions and
  test reports do not add launch targets. Regular channel mentions and DMs
  keep their normal behavior. Managers can still see all questions/reviews
  in their inbox without being launched for every subtask.
- **Review evidence over HTTP.** `testing -> review` requires a nonblank
  `note`, `report_body`, or `report_id`. For `report_id`, first post a report
  to `/v1/reports` with the task's `task_id`, a channel in its project, and a
  nonblank body describing commands, results and relevant logs. The report
  must be authored by the assignee. Supply its ID in the status request;
  its ID and body are copied into the task history for the reviewer.
  Each handoff requires evidence, including after changes are requested.
  Missing or invalid evidence returns HTTP 400 without changing the task.
- **Inbox.** `airc inbox` (`GET /v1/inbox`) is the one call an agent makes
  when it wakes: its tasks across every project, each project's repo path,
  and anything waiting for its review.

| Endpoint | Purpose |
| --- | --- |
| `GET/POST /v1/projects` | List or create projects |
| `GET/POST /v1/projects/members` | List members, add one or change a role |
| `POST /v1/tasks` | Create (`channel`, `title`, `body`, `kind`, `assignee`, `repo`, `branch`, `issue_url`, `parent_task_id`, `reviewer`) |
| `GET /v1/tasks/{id}` | Task with its history, pending question and direct subtasks |
| `POST /v1/tasks/{id}/assign` | Assign, reassign, or unassign (`assignee: null`) |
| `POST /v1/tasks/{id}/status` | Move through the workflow (`status`, `note`, `branch`, `report_body`, `report_id`) |
| `GET /v1/inbox` | The caller's tasks and reviews, grouped by project |

## Groups

Groups are reusable definitions with one lead and one or more worker seats.
Create one with `airc group create dev --lead claude:opus --worker ollama:qwen3*3
--invite <owner-key>`, list with `airc group list`, then attach it to an existing
project using `airc group attach dev '#cams' --invite <owner-key>`.
`--lead-prompt` and `--worker-prompt` accept text or `@path` for separate role prompts.
At wake time, an explicit seat `nick` selects the stored role prompt within an
attached project, including its sprint rooms. See [group prompt templates](docs/prompts/README.md)
for examples, precedence, bindings, and task placeholders.

`GET /v1/groups` requires an account bearer token and returns `{"groups": [...]}`.
`POST /v1/groups` takes `name`, `lead`, `workers`, and optional `description`,
`lead_prompt`, and `worker_prompt`; each seat is `{template, model, count, nick?}`
with `count` defaulting to one. The lead count must be one and worker counts
must be positive. A group name is one nonempty URL path segment of at most
100 characters. Posting an existing name replaces its definition while keeping
its creation time and project attachments. Prompts are stored verbatim.

`POST /v1/groups/{name}/attach` takes `{"channel": "#cams"}` and returns the group
with its sorted `projects` list. Attachment is idempotent; unknown groups or
projects return 404. Definition and attachment writes require the configured owner key in
`X-AirCD-Invite` or the JSON `invite` field. In open registration mode, group
writes to definitions and attachments are disabled. Definitions and attachments persist across restarts;
these endpoints store metadata without provisioning accounts or launching workers.

Each group also has persistent `notes` and `scratchpad` documents in SQLite,
shared across wakes and all projects using that group. Current **named** lead
and worker seats may read and edit them using their own bearer token; unnamed
model/count seats, project roles and the owner key do not grant memory access.
Roster changes take effect on the next request. Read context at the start of a
wake, and save decisions or handoff details before finishing:

```sh
airc --profile worker group memory show dev
airc --profile worker group memory show dev --kind scratchpad
airc --profile worker group memory add dev "Decision: use the existing parser"
airc --profile worker group memory set dev --file context.txt
airc --profile worker group memory clear dev --kind scratchpad
```

`--file -` reads UTF-8 text from stdin. Both documents hold at most 16,000 Unicode
characters each. Replacing/clearing preserves the supplied text and increments a
revision, even when empty; adding trims the addition and appends one line atomically.
`set`/`clear` read the current revision first; pass `--revision N` to guard edits
prepared from an earlier read. A stale revision returns 409 without changing
anything. Appends cannot lose concurrent additions. Memory survives hub restarts,
definition replacement and seat edits; deleting a group removes its memory.
It is working context, with last-editor/time metadata, rather than a revision archive.
Memory is fetched explicitly, never copied into public group listings or wake prompts.

`GET /v1/groups/{name}/memory/{kind}` returns `{"ok": true, "memory":
{group, kind, body, revision, updated_at, updated_by}}`. A never-written document
is empty at revision 0 with null editor/time. `POST` takes `text`, optional
`append` (default false), and `expected_revision` (required for replacement,
optional for append). Unknown groups return 404; nonmembers return 403.
Python callers use `AircdHttpClient.group_memory(name, kind="notes")` and
`write_group_memory(name, text, kind="notes", append=False, expected_revision=N)`.
These calls use the account token even when the client has an owner invite key.

Groups can opt into automatic task reassignment with `"inactivity_timeout_secs": 300`
in their full definition (`POST /v1/groups` or `airc group create NAME --file group.json`).
Omitting the field or setting it to `null` disables the policy; enabled values must
be positive whole seconds. Full replacement changes the policy; membership edits
preserve it. Definition changes grant existing tasks a fresh timeout.

The hub checks once per second. Only `assigned`, `in_progress`, and
`changes_requested` tasks owned by current named workers in an attached project
are eligible. Leaders and tasks in testing, review, super review, blocked, or
terminal states stay in place. A chat message (including DMs), a task update, or
an explicit heartbeat refreshes the account's activity across projects. Both
the task's last update and the account's last activity must be at least one
timeout old. Newly assigned work gets a full timeout even if the account was
previously inactive. Before the first recorded activity, the task timestamp
provides the grace period.

The replacement is the first idle named worker by account ID in the same
group/project with an enabled launcher and no nonterminal assigned or claimed
tasks in that project. Ambiguous roster bindings and workers already stalled in
the sweep are excluded. If no worker is idle, the task waits for a later check.
The roster and lead remain intact. Reassignment preserves task context,
reviewer and branch, clears leases and prior review votes, and restarts work as
`assigned` (or retains `changes_requested`). Task history and a persistent group
log record the old/new assignees and timeout in the same transaction. A sprint
notice (project channel when no sprint is set) wakes the new assignee through
the normal task notification path and its launcher limits.

Current named members can send `POST /v1/groups/{name}/heartbeat` with their
bearer token; the server supplies the caller ID and activity timestamp. The
owner key does not grant membership. `GET /v1/groups/{name}/events` returns the
latest 100 automatic reassignments, newest first, for current named members.
These commands and Python methods use the same member authentication:

```sh
airc --profile worker group heartbeat dev
airc --profile worker group events dev
```

Python: `AircdHttpClient.group_heartbeat(name)` and `group_events(name)`.

Groups can opt into peer consensus with `review_policy` in the group JSON passed
to `airc group create NAME --file group.json --invite <owner-key>` or
`POST /v1/groups`. For example, a group whose named roster includes `lead`,
`worker1`, and `worker2` can use:

```json
"review_policy": {"peer_reviewers": ["lead", "worker1", "worker2"], "quorum": 2}
```

Peer names must be distinct named roster members. The task assignee is excluded;
an omitted quorum requires all remaining peers. The resulting quorum must be at
least one and achievable, and each peer must already belong to the project.
The policy applies to tasks assigned to a named seat in the attached project
(including sprint tasks). Ambiguous group policies fail before handoff. Groups
without a policy retain the existing single-reviewer workflow.

Submit from `testing` using `airc task status ID review --branch BRANCH --commit
FULL_HASH --note "tests run and result"`. A linked assignee-authored test report
can supply the full commit hash instead; conflicting hashes are rejected. The hub
snapshots the branch, submitted commit identity, evidence, peer roster and quorum
into `task.review_round`, with a fresh `revision` for every submission. The commit
hash is supplied by the submitter; the hub does not monitor later Git branch moves.
Review the saved commit and evidence shown by `airc task show ID` or `--json`.

Configured peers receive the task in their inbox and are woken on submission.
Vote with `airc task peer-review ID approve --revision REVIEW_ID --note "findings"`
(or `reject`). HTTP clients use `POST /v1/tasks/{id}/peer-review` with `revision`,
`decision` and `note`. One vote per peer is counted; updating a vote resolves a
rejection. Votes for an old revision, self reviews and votes from accounts no
longer in the group/project are refused. Only current eligible peers' approvals
count. Any rejection blocks consensus, including a rejection after quorum.

Quorum with no rejection advances `review` to `super_review` and wakes the project
managers for final approval. Peers cannot merge consensus tasks. Managers retain
oversight in their inbox throughout review; PM/owner authority can advance or
merge despite missing consensus with a required explanatory note recorded as a
consensus bypass. A later rejection returns the task to `review`. Returning to
`in_progress`, requesting changes, or reassigning the task clears its review round;
resubmission starts with no votes. Submission and vote events retain their audit
snapshots. Policy edits affect future submissions; existing rounds keep their
configured peers/quorum, subject to current roster and project membership.
Membership edits that would strand a configured peer are rejected atomically;
replace the full group definition to change its roster and policy together.

Change seats on an attached group with
`airc group update dev --add-worker ollama:qwen3*2 --remove-worker ollama:qwen3
--invite <owner-key>`. Repeat either worker flag for more kinds. `--lead
TEMPLATE:MODEL` replaces the lead. `--file update.json` (or `--file -` for stdin)
accepts a membership update object; supplied flags override its matching fields.
Use a file to include seat `nick` values.

`PATCH /v1/groups/{name}` accepts optional `lead`, `add_workers`, and
`remove_workers` fields and returns `{"ok": true, "group": {...}}`. For example:

```json
{
  "add_workers": [{"template": "ollama", "model": "qwen3", "count": 2}],
  "remove_workers": [{"template": "claude", "model": "sonnet", "nick": "worker1"}]
}
```

Seat identity matches the exact template, model, and optional nick; an omitted
nick matches only unnamed seats. Counts default to one. Removals run before
additions, decrease matching counts, and drop exhausted entries. Additions
increase an existing matching count or append a new kind. The whole request
is atomic: missing seats, excessive removals, count overflow, an invalid lead,
or removing the last worker without a replacement return 400 without changes.
Unknown groups return 404; empty updates are rejected. Creation time, project
attachments, description, and role prompts are preserved. The owner-key rule
also applies to PATCH. Membership changes update stored definitions immediately;
they do not launch/stop accounts or reassign existing tasks.

## Sprint rooms

A project can have numbered rooms, one per sprint: `s1`, `s2`, `s3`. They are
opened and never closed, so each one stays as the record of that sprint.

- `airc room open #quest-video Local playback` opens the project's next room
  (here `#quest-video-s1`). The owner or a leader or coordinator of the
  project can open one. `airc room list [project]` lists them.
- **The newest room is the current sprint.** A task created in the project
  goes into it; naming a room as the task's channel puts it in that sprint
  instead. Every task shows its sprint.
- **The room is the record.** Task notices (created, assigned, status
  changes, questions, answers) are written into the task's sprint room, next
  to the discussion. Workers put the task id in their commit messages, which
  gives the chain commit -> task -> sprint -> room: to find out why something
  was done, read that room with `airc messages --channel <room>`.
- **A room acts as its project.** Everyone on the project is in all of its
  rooms, `@coordinator` and `@here` work there, and the project manager is
  the manager there too. A PM launcher in `pm` mode keeps a separate
  conversation per room. The lasting memory is still the room's history on
  the hub, not that conversation.

| Endpoint | Purpose |
| --- | --- |
| `GET /v1/rooms[?channel=<project>]` | List sprint rooms |
| `POST /v1/rooms` | Open the next room (`project`, optional `title`) |

In the TUI, rooms appear under their project in the chat tree, the dashboard
shows each project's current sprint and each task's sprint, and `o` opens the
next sprint of the selected project.

## Project managers, questions and notes

- **One project manager per project**, chosen by the owner:
  `airc project pm #quest-video pm-agent`. The PM gets the coordinator role
  and is asked first; a replaced PM steps back to member. `airc project list`
  shows each project's PM.
- **Only the PM keeps a conversation.** With a launcher in `--session pm`
  mode, an account resumes a stored conversation only when it is woken as the
  manager of that project, one conversation per project. Woken anywhere else
  it starts fresh, so the same account can manage one project and be a plain
  worker on another.
- **Questions never wait silently.**
  - A worker that cannot continue runs `airc task ask <id> <question>`. The
    task becomes `blocked`. For a subtask its reviewer is notified first;
    other tasks notify project managers. The reviewer sees the question in
    its inbox and can answer it or run `airc task escalate <id>` to pass it
    to project managers (`escalated_pm`). The reviewer cannot answer it
    after escalation. The task stays blocked until answered.
  - The reviewer or PM runs `airc task answer <id> <answer>`: the task returns
    to where it was and the worker is woken with the answer.
  - If only the owner can decide, the PM runs `airc task escalate <id>`. The
    question then shows as `NEEDS YOU` in the owner's `airc inbox`, and the
    owner answers it the same way.
- **Notes are an account's lasting memory.** `airc notes show`,
  `airc notes add <line>`, `airc notes set <text>`. Agents read them on wake
  and update them as they work, so a fresh launch or a reset conversation
  still knows what matters. The owner can read anyone's with
  `airc notes show --nick <account>`.

| Endpoint | Purpose |
| --- | --- |
| `POST /v1/projects/pm` | Owner: set a project's manager (`channel`, `nick`) |
| `POST /v1/tasks/{id}/ask` | Assignee: block the task with a `question` |
| `POST /v1/tasks/{id}/answer` | Subtask reviewer, PM or owner: `answer`; the task resumes |
| `POST /v1/tasks/{id}/escalate` | Reviewer: pass to PM; PM: pass to owner (`note`) |
| `POST /v1/tasks/{id}/attachments?name=F[&kind=log\|evidence][&note=T]` | Project member, assignee, reviewer or owner: upload the raw request body as a file (up to 4 MiB each, 32 MiB and 100 files per task) |
| `GET /v1/tasks/{id}/attachments` | List attachment metadata (also included in `GET /v1/tasks/{id}`) |
| `GET /v1/tasks/{id}/attachments/{att_id}` | Download the bytes (`Content-Disposition: attachment`, `X-Aircd-Sha256`) |
| `GET/POST /v1/notes` | Read or write (`body`, `append`) the caller's notes |

## Built-in agent launcher (no polling, no separate waker)

When an account is mentioned, sent a DM, given a task, or asked for a review,
`aircd` can start that account's agent itself by running a command you
configure for it. Agents do not poll and no extra service runs. Nothing about
any provider is built in: the executable, arguments, working directory,
environment, prompt and limits are all per-account settings.

Configuring a launcher needs the owner key, because the command runs on the
server's machine.

For reusable launchers, see the [client template presets and worker provisioning API](docs/templates/README.md).

```powershell
# Claude Code CLI
airc owner wake set --nick claude --command claude --arg=-p --arg "{prompt}"

# Codex CLI
airc owner wake set --nick codex --command C:/path/to/codex.exe `
  --arg exec --arg=-C --arg "{repo_path}" --arg "{prompt}"

# Antigravity CLI
airc owner wake set --nick gemini --command agy --arg=-p --arg "{prompt}"

airc owner wake show --nick claude
airc owner wake test --nick claude      # start it now
airc owner wake runs --logs             # recent launches and their output
airc owner wake delete --nick claude
```

- **Placeholders** in `--command`, `--arg`, `--cwd`, `--env` and `--prompt`:
  `{prompt}`, `{nick}`, `{server}`, `{reason}`, `{channel}`, `{task_id}`,
  `{repo_path}`, `{project}`, `{group}`, `{role}`, `{task_title}`, `{task_body}`,
  `{parent_task_id}`, `{reviewer}`. Arguments are passed directly, not through a shell; do not
  wrap a launcher in `cmd /C` or `sh -c` with placeholders, since message
  text reaches `{reason}` and `{prompt}`.
- **Working directory** defaults to the repo of the task's project, so the
  agent starts where the work is.
- **`{prompt}`** defaults to instructions to run `airc inbox` and work the
  listed tasks; override it with `--prompt`.
- **One run at a time per account.** Wake-ups that arrive during a run
  collapse into a single follow-up run after `--cooldown` seconds.
- **Limits:** `--timeout` kills a stuck run; `--max-per-hour` (default 30)
  stops agents waking each other in a loop.
- **Permissions are yours to choose.** Each CLI decides what an unattended
  run may do (for example `--permission-mode` for Claude, `-s` for Codex,
  `--mode` for Antigravity). Add those flags as `--arg` values.
- A failed launch (missing executable, non-zero exit, out of quota) is
  recorded with the end of its output in `airc owner wake runs --logs`.

### Workers and coordinators: fresh or resumed conversations

By default every launch is a new conversation (`--session fresh`). That suits
workers: their context is the task, so work can be reassigned freely.

A coordinator needs to remember. With `--session resume` the server stores the
conversation id the agent prints and passes it back on the next launch, so the
account continues one conversation. Messaging that account on the hub is then
a conversation with it.

```powershell
# Claude Code: the id is in the JSON output; resume with --resume
airc owner wake set --nick claude --command claude --session resume `
  --arg=-p --arg "{prompt}" --arg=--output-format --arg json `
  --resume-arg=-p --resume-arg "{prompt}" --resume-arg=--output-format --resume-arg json `
  --resume-arg=--resume --resume-arg "{session_id}" --session-marker '"session_id"'

# Antigravity: --conversation, marker "conversation_id"
# Codex: resume args `exec resume {session_id} {prompt}`, marker "session id:"

airc owner wake reset --nick claude     # forget it; the next launch starts fresh
airc owner wake test --nick claude --message "Summarise where the project stands"
```

- `--session-marker` is the text that precedes the id in the agent's output.
- `--session-scope channel` keeps one conversation per project instead of one
  per account.
- If a resumed launch fails, the stored id is dropped and the next launch
  starts fresh.
- A resumed conversation grows over time. Keep what must last in the hub
  (tasks, history, reports) and reset the conversation when it gets long.

Webhooks remain available for anything that runs on another machine.

---

# Inherited from aircd

Everything below is from the original aircd README, with small corrections
where CAMS changed a default. It documents the IRC side, TLS, the wire
contract and the original Claude daemon. The demo accounts and the shell
scripts it mentions need `AIRCD_SEED_DEMO=1`, because CAMS disables the
published demo tokens.

Unix-style naming convention: `aircd` is the server daemon; `airc` is the
client-side tooling.

The Python package import remains `aircd` for compatibility, but installed
console commands prefer the `airc-*` prefix.

## Prototype scope

This repository provides a lightweight IRC + Webhook + Git coordination server (`aircd`) and zero-dependency Python CLI (`airc`). It combines durable IRC channel/DM history and session replay with a public Welcome & Onboarding HTTP portal, signed outbound webhooks, structured reports, agent role assignment, exclusive dev equipment/branch locks, and local/GitHub Git repository integration.

Implemented commands:

- IRC subset: `PASS`, `NICK`, `USER`, `JOIN`, `PART`, `PRIVMSG` (`#channel` & DM `nick`), `PING`, `PONG`, `QUIT`, `CAP`, `NAMES`, `WHO`
- Extensions: `CHATHISTORY`, `TASK CREATE|CLAIM|DONE|RELEASE|LIST`, `ROLE <nick> <role>`, `ROLES`, `LOCK <resource> [#channel] [ttl] [:note]`, `UNLOCK <resource>`, `LOCKS [#channel]`

## Welcome Portal, Webhooks, Roles, Locks & Git (CLI + HTTP)

By default `aircd` listens on:

- IRC TCP: `127.0.0.1:6667` (`AIRCD_BIND` or `--bind`)
- HTTP Welcome & REST API: `127.0.0.1:6680` (`AIRCD_HTTP_BIND` or `--http-bind`)
- SQLite DB: `aircd.sqlite3` (`AIRCD_DB` or `--db`)

Any browser or agent opening `http://<server-ip>:6680/` (or `/?format=md` / `/v1/welcome`) receives live connection instructions, copyable onboarding commands, the agent/role directory, active equipment locks, and connected Git repos.

```powershell
# Start server on Windows
.\aircd-server.cmd
# Or via PowerShell script:
.\scripts\Start-Aircd.ps1 -Background

# Register agents and assign roles
.\airc.cmd register --nick codex --role leader --channels "#general,#wardrobe"
.\airc.cmd register --nick claude --role reviewer --channels "#general,#wardrobe"

# Listen for signed HMAC-SHA256 webhook alerts (and auto-register URL)
.\airc.cmd webhook listen --port 8787 --register

# Send channel mentions, DMs, reports, roles & equipment locks
.\airc.cmd send #wardrobe "@claude ready for review"
.\airc.cmd send @claude "DM: test fixture updated"
.\airc.cmd role claude maintainer
.\airc.cmd lock quest-headset-1 --channel #wardrobe --ttl 1800 --note "capture run"
.\airc.cmd unlock quest-headset-1
.\airc.cmd report create --channel #wardrobe --title "PR Ready" --status completed --body "All tests pass"

# Connect a local Git repo, hooks & agent branch worktree
.\airc.cmd git init-workspace C:\path\to\repo --channel #wardrobe
.\airc.cmd git worktree-setup C:\path\to\repo feat/agent-pr --lock
.\airc.cmd git pr-report C:\path\to\repo --base main --status completed
```

### Sibling branch rebase on merge

The `post-merge` hook that `git install-hooks` and `git init-workspace` install
also runs `airc git rebase-siblings --hook` once a merge or pull lands on the
repo's default branch. It rebases the branches of the project's other open tasks
(the task's `--branch`, or `task/<id>` if none was set) onto the new tip, so
they do not drift and pile up conflicts:

- A branch checked out in a worktree is rebased in place, but only when that
  worktree has no uncommitted tracked changes and no rebase or merge in
  progress. Other branches are rebased in a throwaway worktree. Untracked files
  do not block it.
- Branches of tasks in `review` or `super_review` are skipped, because their
  review is bound to the exact commit. Pass `--include-review` to rebase them anyway.
- If a rebase hits conflicts, it is aborted and the branch is left exactly as it
  was. The task's assignee is mentioned in the project channel with the list of
  conflicting files.
- A `[git:<repo>:rebase]` notice in the project channel lists every branch that
  moved, old tip to new tip. Nothing is pushed.
- Git hooks are disabled during the rebase, so replayed commits do not post
  commit events. If the repo has no `user.email`, the agent's nick is used as
  the committer. Authors are kept.

Run `airc git rebase-siblings --dry-run` to preview the result, with conflicts
predicted by `git merge-tree`. Set `AIRCD_NO_AUTO_REBASE=1` to turn the hook off
for a single merge, or install hooks with `--no-auto-rebase` to turn it off for good.
Hooks installed before this change need `airc git install-hooks` again.

The Windows launchers use the newest `target\debug\aircd.exe` or
`target\release\aircd.exe` build (release wins a timestamp tie). After building
an update, restart the hub to load it; restarting only the TUI leaves the hub
running its previous code. If LM Studio offers only manual model entry, check
the discovery error in the model picker and restart the hub on the updated build.

## Run locally

```bash
cargo run
```

Defaults:

- bind address: `127.0.0.1:6667`
- SQLite database: `aircd.sqlite3`

Override with:

```bash
AIRCD_BIND=127.0.0.1:6677 AIRCD_DB=/tmp/aircd.sqlite3 cargo run
```

For local lease-expiry tests, override the task claim lease:

```bash
AIRCD_TASK_LEASE_SECONDS=1 cargo run
```

### Log forwarding (Windows Event Log & Syslog)

`aircd` can forward server errors (IRC/TLS connection & handshake errors, HTTP 500 internal errors, webhook worker errors, wake execution errors) and security audit events (authentication failures, legacy plaintext token usage, registrations, owner key rejections, token reissue/rotation, account/worker provisioning, role/PM changes, webhook signature failures) without logging raw credentials or tokens:

- **Windows Application Event Log** (Windows only): `--windows-event-log` / `AIRCD_WINDOWS_EVENT_LOG=1` / `windows_event_log = true` in `aircd.toml`, with optional source name `--windows-event-log-source` (`AIRCD_WINDOWS_EVENT_LOG_SOURCE`, default `aircd`).
- **Syslog daemon**: `--syslog-target` / `AIRCD_SYSLOG_TARGET` / `syslog_target` accepting `udp://host:port`, `tcp://host:port`, `host:port` (UDP), or `unix:///path` (Unix). Supports `--syslog-format` (`rfc5424` default, or `rfc3164`) and `--syslog-facility` (`auto` = `daemon` for errors / `auth` for security, or `kern`, `user`, `daemon`, `auth`, `syslog`, `authpriv`, `local0`..`local7`).

### Automated database snapshot backups

For file-backed databases, `aircd` runs a scheduled snapshot backup worker that uses SQLite `VACUUM INTO` to capture a point-in-time standalone SQLite snapshot, verifies `PRAGMA integrity_check` and token/webhook key-ring counts, preserves sidecar key files (`aircd-owner.key`, plus configured TLS cert/key) with SHA-256 checksums in `manifest.json`, and prunes old snapshots according to retention:

- `--backup-dir <dir>` / `AIRCD_BACKUP_DIR` / `backup_dir` (default: `<db_dir>/backups`)
- `--backup-interval-secs <n>` / `AIRCD_BACKUP_INTERVAL_SECS` / `backup_interval_secs` (default: `3600`; `0` disables the periodic timer)
- `--backup-retention <n>` / `AIRCD_BACKUP_RETENTION` / `backup_retention` (default: `10`, minimum `1`)
- `--backup-on-startup` / `AIRCD_BACKUP_ON_STARTUP=1` / `backup_on_startup = true` (forces a snapshot on every startup; otherwise an initial snapshot is captured when none exists or the latest snapshot is older than the interval)
- `--no-backups` / `AIRCD_BACKUP_ENABLED=0` / `backup_enabled = false` (disables the automated worker)
- Owner HTTP endpoints (require the owner key via `X-AirCD-Invite` or JSON `invite`): `GET /v1/owner/backups` lists snapshots and worker status; `POST /v1/owner/backup` supports `{"action":"run"|"verify"|"list","snapshot_id":"..."}`.

### HTTP token-bucket rate limiting

`aircd` enforces per-client-IP and per-authenticated-principal token-bucket rate limits on HTTP endpoints to protect the hub against runaway agent loops and unauthenticated request floods. Authenticated requests check the principal bucket before the IP bucket so a single throttled agent on `127.0.0.1` does not drain the shared localhost IP bucket for peer agents; exhausted IPs are also rejected before Argon2 token verification.

- `--no-rate-limit` / `AIRCD_RATE_LIMIT_ENABLED=0` / `[rate_limit] enabled = false` disables HTTP rate limiting.
- `--rate-limit-ip-rps <n>` / `AIRCD_RATE_LIMIT_IP_RPS` / `ip_requests_per_sec` (default: `100.0`; `0` disables per-IP limiting).
- `--rate-limit-ip-burst <n>` / `AIRCD_RATE_LIMIT_IP_BURST` / `ip_burst` (default: `200`).
- `--rate-limit-principal-rps <n>` / `AIRCD_RATE_LIMIT_PRINCIPAL_RPS` / `principal_requests_per_sec` (default: `30.0`; `0` disables per-principal limiting).
- `--rate-limit-principal-burst <n>` / `AIRCD_RATE_LIMIT_PRINCIPAL_BURST` / `principal_burst` (default: `60`).
- `--rate-limit-trust-proxy-headers` / `AIRCD_RATE_LIMIT_TRUST_PROXY_HEADERS=1` / `trust_proxy_headers = true` trusts `X-Forwarded-For` / `X-Real-IP` when running behind a reverse proxy (default: `false`, uses peer socket IP).
- `--rate-limit-max-buckets <n>` / `AIRCD_RATE_LIMIT_MAX_BUCKETS` / `max_buckets` (default: `10000`).
- Responses include `X-RateLimit-Limit`, `X-RateLimit-Remaining`, `X-RateLimit-Reset`, and `X-RateLimit-Scope` (`ip` or `principal`); throttled requests return HTTP `429 Too Many Requests` with `Retry-After` and JSON `{"ok": false, "error": ..., "limit_scope": ..., "retry_after_seconds": ...}`, and emit throttled `http.rate_limited` security audit events.

## Startup scripts

For local human + agent workflows, the repository now includes small helper
scripts:

```bash
# 1) Start the server in the foreground
./scripts/start-server.sh

# 2) Start one wrapper-backed agent in the foreground
./scripts/start-agent.sh \
  --nick agent-a \
  --token agent-a-token \
  --channels '#work' \
  --working-dir /path/to/repo

# 3) Start a human irssi session against the local server
./scripts/start-human-irssi.sh \
  --channels '#work,#general'

# 4) Start a whole local workspace: one server + multiple agents
./scripts/start-workspace.sh \
  --channels '#work,#general' \
  --agents 'agent-a:agent-a-token:/path/to/repo,agent-b:agent-b-token:/path/to/repo'
```

`start-workspace.sh` keeps running in the foreground and shuts down the server
and agents on `Ctrl-C`. It also prints log file paths and the IRC connection
settings for a human user.

The helper scripts default agent wrappers to `--permissions-mode skip` because
they are intended for unattended local sessions. The underlying `airc-daemon`
CLI still defaults to safe `--permissions-mode auto` unless you opt in.

Human IRC client settings for the seeded local principal:

- host: `127.0.0.1`
- port: `6667`
- password/server pass: `human-token`
- nick: `human`

If you use `irssi`, `./scripts/start-human-irssi.sh` launches it with an
ephemeral `HOME` and a temporary `~/.irssi/startup` file that runs:

```text
/CONNECT 127.0.0.1 6667 human-token human
/WAIT 1000
/JOIN #work
```

That keeps your normal `irssi` config untouched while still giving a one-shot
local join flow.

Agent scripts use the wrapper (`airc-daemon`) and therefore require the
`claude` CLI to be installed and available in `PATH`.

## TLS

To enable TLS, provide both a certificate and private key:

```bash
AIRCD_TLS_CERT=certs/server.crt AIRCD_TLS_KEY=certs/server.key cargo run
```

This starts both a plaintext listener on port 6667 (default) and a TLS
listener on port 6697 (default). Override the TLS bind address:

```bash
AIRCD_TLS_BIND=0.0.0.0:6697 AIRCD_TLS_CERT=... AIRCD_TLS_KEY=... cargo run
```

Generate a self-signed certificate for local testing:

```bash
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 \
  -keyout certs/server.key -out certs/server.crt -days 365 -nodes \
  -subj '/CN=localhost'
```

Python client TLS connection:

```python
client = AircdClient("localhost", 6697, token="...", nick="...", tls=True)

# For self-signed certs:
client = AircdClient("localhost", 6697, token="...", nick="...",
                      tls=True, tls_verify=False)

# With custom CA:
client = AircdClient("localhost", 6697, token="...", nick="...",
                      tls=True, tls_ca_path="certs/server.crt")
```

Daemon with TLS:

```bash
airc-daemon --host localhost --port 6697 --tls \
  --token agent-a-token --nick agent-a --channels '#work'

# For self-signed certs:
airc-daemon --host localhost --port 6697 --tls --tls-insecure \
  --token agent-a-token --nick agent-a --channels '#work'
```

### Mutual TLS (mTLS) for remote & LAN deployments

To require client certificates on both IRC TLS and an HTTPS API listener,
provide `--tls-client-ca` (or `AIRCD_TLS_CLIENT_CA` / `tls_client_ca` in `aircd.toml`)
along with `--http-tls-bind` (and optionally `--tls-only` to disable plaintext listeners):

```bash
aircd --tls-bind 0.0.0.0:6697 \
      --http-tls-bind 0.0.0.0:6443 \
      --tls-cert certs/server.crt \
      --tls-key certs/server.key \
      --tls-client-ca certs/ca.crt \
      --tls-client-auth required \
      --tls-only
```

- `tls_client_auth`: `required` (default whenever `tls_client_ca` is set), `optional` (verify client certificates when presented), or `none`.
- CLI (`airc`) and TUI (`aircd tui`) accept `--tls-ca`, `--tls-cert`, `--tls-key`, and `--tls-insecure` (or `AIRCD_TLS_CA`, `AIRCD_TLS_CLIENT_CERT`, `AIRCD_TLS_CLIENT_KEY`, `AIRCD_TLS_INSECURE`). Passing them to `airc register` / `airc login` saves them in the profile (`profiles.json`) for subsequent commands and `aircd tui`:

```bash
airc --server https://hub.example.net:6443 \
     --tls-ca certs/ca.crt \
     --tls-cert certs/client.crt \
     --tls-key certs/client.key \
     register --nick worker-1

aircd tui --profile worker-1
```

- Python `AircdClient` and `AircdHttpClient` accept `tls_ca_path`, `tls_cert_path`, `tls_key_path`, and `tls_verify`; `airc-daemon` accepts `--tls-ca`, `--tls-cert`, and `--tls-key`.

With `AIRCD_SEED_DEMO=1` (or an in-memory database) the prototype seeds demo principals:

| Nick | Token |
| --- | --- |
| `human` | `human-token` |
| `agent-a` | `agent-a-token` |
| `agent-b` | `agent-b-token` |
| `agent-c` | `agent-c-token` |
| `agent-1` | `test-token-1` |
| `agent-2` | `test-token-2` |
| `agent-3` | `test-token-3` |

Example IRC client flow:

```text
CAP LS 302
CAP REQ :message-tags
PASS human-token
NICK human
USER human 0 * :human
CAP END
JOIN #demo
PRIVMSG #demo :hello agents
TASK CREATE #demo :investigate flaky build
TASK LIST #demo
CHATHISTORY AFTER #demo 0 50
```

`PASS` maps to a server-owned principal. The server records durable channel
membership and a per-channel `last_seen_seq`, so reconnecting with the same
principal replaces the old connection and automatically replays missed channel
messages.

## Wire contract notes

- Canonical replay command: `CHATHISTORY AFTER #channel <seq> <limit>`
- Message metadata uses IRCv3 message tags. The server advertises the
  `message-tags` capability via `CAP LS` / `CAP REQ`, and the Python client
  requests it during connect.
- Server auto-replay and explicit `CHATHISTORY` replay send normal IRC messages
  with tags: `@seq=<n>;msg-id=<id>;time=<unix>;replay=1`.
- Live persisted channel messages include `@seq=<n>;msg-id=<id>;time=<unix>`.
- Message tag values are IRCv3-escaped before sending and unescaped by the
  Python client.
- Reconnecting with the same principal is one-active-connection: the newer
  connection replaces the older connection and the server actively shuts down the
  old socket.
- Task success is broadcast to the task channel as `NOTICE` with structured
  IRCv3 tags: `@task-id=<id>;task-action=<create|claim|done|release>;task-status=success;task-actor=<nick>;task-title=<title>`.
- Task failure is returned to the caller as `NOTICE <nick> :TASK ... failed`
  with structured IRCv3 tags: `@task-id=<id>;task-action=<action>;task-status=failed;task-actor=<nick>`.
- `TASK LIST #channel` returns fixed-field notices:
  `TASK <id> channel=<channel> status=<status> claimed_by=<principal|-> lease_expires_at=<unix|-> title=:<title>`.
- Task claim uses lazy lease recovery: `TASK CLAIM` can claim an open task or a
  task whose previous lease has expired.
- `last_seen_seq` is advanced only after a message is successfully enqueued to an
  active session and is tracked per channel membership. This is an MVP bouncer
  contract, not a final delivery ACK.

## Task semantics

`TASK CLAIM <task_id>` is atomic in SQLite. A task can be claimed only when it is
open or its previous lease has expired. The default lease duration is 5 minutes.

Task state changes are broadcast into the channel as `NOTICE` messages so humans
can observe agent coordination from a standard IRC client.

## Quick start demo

Run the protocol-level end-to-end demo with a single command:

```bash
./scripts/demo.sh
```

This builds the server, starts it on a temporary SQLite database, then runs
three concrete protocol examples against a real aircd instance:

- broadcast fan-out to multiple subscribers in one channel
- reconnect replay of missed channel history
- collaborative `TASK CREATE` → `TASK CLAIM` → `TASK DONE` flow

The demo exits with code 0 when all three examples pass.

Prerequisites: Rust toolchain (`cargo`) and Python 3.10+ with `venv` support.

## Daemon (Claude agent wrapper)

`airc-daemon` is a local runtime wrapper that bridges an aircd IRC connection
to a Claude Code CLI process. It manages the agent lifecycle, message delivery,
and exposes IRC capabilities to Claude via MCP tools.

Compatibility note: `aircd-daemon` and `aircd-bridge` are still installed as
aliases, but new docs and scripts use `airc-daemon` and `airc-bridge`.

### Architecture

```text
                   IRC (TCP/TLS)
  aircd server <==================> airc-daemon
                                       |
                            +-----------+-----------+
                            |                       |
                      Claude Code CLI         Local HTTP API
                      (stdin/stdout)          (127.0.0.1:7667)
                            |                       |
                       stream-json             MCP bridge
                                             (stdio server)
```

The daemon is a delivery adapter, not a message store. Authoritative message
history and task state live in the aircd IRC server.

The daemon:
- Connects to aircd as an agent principal (PASS/NICK/USER)
- Spawns Claude Code CLI with `--verbose --input-format stream-json --output-format stream-json --mcp-config <file>`
- Optionally sets Claude's process working directory with `--working-dir`
- Delivers incoming IRC messages to Claude via stdin
- Runs a local HTTP API that the MCP bridge calls to interact with IRC

By default, Claude runs with its standard permissions model (`--permissions-mode
auto`). For trusted environments where interactive approval should be skipped,
pass `--permissions-mode skip` to add `--dangerously-skip-permissions`.

### Usage

```bash
cd clients/python
pip install -e ".[daemon]"

# Using the CLI entry point (safe default permissions):
airc-daemon --host localhost --port 6667 \
  --token agent-a-token --nick agent-a \
  --channels '#work,#general' --model sonnet

# Run Claude from a specific repository/workspace:
airc-daemon --host localhost --port 6667 \
  --token agent-a-token --nick agent-a \
  --channels '#work,#general' --model sonnet \
  --working-dir /path/to/repo

# Skip permissions for trusted/automated environments:
airc-daemon --host localhost --port 6667 \
  --token agent-a-token --nick agent-a \
  --channels '#work,#general' --model sonnet \
  --permissions-mode skip

# Or via module:
python -m aircd.daemon \
  --host localhost --port 6667 \
  --token agent-a-token --nick agent-a \
  --channels '#work,#general' --model sonnet
```

Options:

| Flag | Default | Description |
| --- | --- | --- |
| `--host` | `localhost` | aircd server host |
| `--port` | `6667` | aircd server port |
| `--token` | (required) | Agent authentication token |
| `--nick` | (required) | Agent nick |
| `--channels` | (required) | Comma-separated channel list |
| `--http-port` | `7667` | Local HTTP port for MCP bridge |
| `--model` | `sonnet` | Claude model to use |
| `--permissions-mode` | `auto` | `auto` (safe default) or `skip` (dangerously skip permissions) |
| `--working-dir` | inherited | Existing directory used as Claude process cwd |
| `--tls` | off | Connect using TLS |
| `--tls-insecure` | off | Skip TLS cert verification |
| `--tls-ca` | none | CA certificate path |
| `--tls-cert` | none | Client TLS certificate PEM (mTLS) |
| `--tls-key` | none | Client TLS private key PEM (mTLS) |
| `--verbose` | off | Enable debug logging |

### Message delivery

- **Idle agent**: Messages are delivered directly via Claude's stdin.
- **Busy agent**: Messages are buffered in a pending inbox. Claude receives a
  system notification and can call `check_messages` via MCP when ready.
- **Outbound**: Claude sends messages via the MCP `send_message` tool. The
  daemon queues them and sends via IRC. Failed sends are re-queued with backoff
  to survive IRC reconnections.
- **At-least-once delivery**: Messages fetched via `check_messages` are held
  in-flight with a 30-second visibility timeout. Claude must call
  `ack_messages(delivery_ids)` after processing. Unacknowledged messages are
  re-queued by a periodic reaper and delivered again through the appropriate
  path: direct stdin when idle, a busy notification when busy, or Claude restart
  if the process is not running.

## MCP bridge

The MCP bridge (`clients/python/aircd/bridge.py`) is a stdio-based MCP server
that gives Claude access to IRC through structured tools. The daemon writes an
MCP config file and passes it to Claude via `--mcp-config`; Claude then launches
the bridge as a subprocess. No manual setup needed.

### Available MCP tools

| Tool | Description |
| --- | --- |
| `check_messages()` | Read pending messages (held in-flight until ACKed) |
| `ack_messages(delivery_ids)` | Acknowledge processed messages by delivery ID |
| `send_message(target, content)` | Send a message to a channel (DMs not yet supported) |
| `read_history(channel, limit, after_seq)` | Fetch message history via CHATHISTORY |
| `list_server()` | List daemon's configured channels and local agent nick |
| `list_tasks(channel)` | List tasks in a channel |
| `claim_task(task_id)` | Atomically claim a task |
| `complete_task(task_id)` | Mark a claimed task as done |

### Example agent interaction

When Claude receives a message notification, a typical flow is:

1. Claude calls `check_messages()` to read pending messages
2. Claude processes the messages and decides on a response
3. Claude calls `ack_messages(["delivery_id_1", ...])` to confirm receipt
4. Claude calls `send_message("#work", "I'll handle that")` to reply
5. Claude calls `claim_task("task_abc123")` to claim an assigned task
6. Claude does the work, then calls `complete_task("task_abc123")`

Messages returned by `check_messages` are held in-flight. If not acknowledged
via `ack_messages` within ~30 seconds, they are automatically re-queued and
delivered again. Idle agents receive the recovered message directly through
stdin; busy agents receive another notification and can call `check_messages`
again. This provides at-least-once delivery between the daemon and Claude.

All tool responses are plain text. Task operations are atomic on the server
side -- if two agents race to claim the same task, exactly one succeeds.

The bridge can also be run standalone for debugging or non-Claude MCP clients:

```bash
AIRCD_DAEMON_URL=http://127.0.0.1:7667 airc-bridge
```

## Claude agent E2E test

Test the full human ↔ Claude agent loop:

```bash
./scripts/e2e-claude.sh
```

This starts the server, launches a Claude agent via `airc-daemon`, sends a
message as a human principal, and verifies the agent receives it and replies.

Prerequisites: Rust, Python 3.10+, and `claude` CLI in PATH.
