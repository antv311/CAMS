# Group role prompts

Groups store `lead_prompt` and `worker_prompt` verbatim in the hub database.
The examples here are opt-in text files: edit them for your team, then save
their contents in the group definition. The server does not read these files
at wake time. Existing definitions and launcher configuration are preserved
until you explicitly replace them.

For example, after provisioning the accounts and configuring their launchers:

```powershell
airc group create dev --file docs/prompts/dev-group.json --lead-prompt '@docs/prompts/group-lead.md' --worker-prompt '@docs/prompts/group-worker.md' --invite <owner-key>
airc group attach dev '#my-project' --invite <owner-key>
```

Replace the example models and nicknames first. Group creation is a full
replacement by name; include all seats and both prompts when updating it.
The HTTP equivalent is `POST /v1/groups` with the same JSON and the actual
prompt text in `lead_prompt` and `worker_prompt`. An `@path` is a CLI file
input convention, not a server-side path. Group writes require the owner key.

A seat's explicit `nick` binds an existing account to its prompt, ignoring
case and an optional leading `@`. A counted seat without `nick` is planning
metadata; it cannot identify an account by model. Give each provisioned worker
its own named seat with `count: 1`. These examples do not provision accounts.

On each wake, the hub resolves a sprint room to its project and looks for that
nick among groups attached to the project. A matching lead uses `lead_prompt`;
a matching worker uses `worker_prompt`. This also applies to owner-triggered
wakes with a project channel. DMs and unattached projects retain the account
prompt. Binding a nick to multiple attached groups, or both roles in one
group, records a failed wake with an ambiguity error and launches no process.
Bind each account to one group role per project.

A stored role prompt takes precedence over the account's launcher `prompt`.
`null` falls back to that account prompt, then the existing PM/member default.
An empty string is an intentional empty prompt. Group edits take effect on the
next launch, including queued wakes; they do not modify launcher arguments,
permissions, session settings, or a running conversation. Use fresh sessions
for workers that should receive only their current implementation assignment.

Templates may use these placeholders in prompts, command, arguments, cwd and
environment values:

| Placeholder | Value |
| --- | --- |
| `{nick}`, `{server}`, `{reason}` | Account nickname, hub URL, wake reason |
| `{channel}`, `{project}` | Event channel and its parent project |
| `{task_id}`, `{repo_path}` | Event task ID and resolved local repository path |
| `{group}`, `{role}` | Matched group name and `group lead` / `group worker`; without a group, empty name and existing PM/member role |
| `{task_title}`, `{task_body}` | Only the triggering task's title and full description |
| `{parent_task_id}`, `{reviewer}` | Triggering task's parent ID and reviewer account ID |
| `{prompt}` | Selected, expanded prompt, available to launcher fields |
| `{session_id}` | Stored conversation ID, available to launcher fields |

Missing task fields are empty. A message wake does not guess a task from an
inbox. Task details are populated only when the event's task belongs to the
resolved project. Workers can fetch their exact task when woken by a message;
their prompt need not include project notes or unrelated tasks. Keep enough
acceptance criteria and repo information in each subtask for a fresh worker.

Expansion is one pass per template. Unknown placeholders and braces in
inserted values remain literal; task bodies and messages are never evaluated
as templates. Arguments are passed directly to the executable. Avoid shell
wrappers that would evaluate prompt or message text as commands.
