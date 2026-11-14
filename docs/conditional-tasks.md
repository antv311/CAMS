# Conditional subtask workflows

Create child tasks with `condition`, `optional: true`, or both. Their initial
status is `dormant`. Dormant tasks and their descendants cannot be claimed,
started, tested, submitted, or dispatched. Reassignment preserves the gate.
They remain visible through task details, child lists, and project task lists,
but are excluded from assignee inboxes until activated.

Conditions are declarative JSON objects, fixed when the child is created:

| Condition | Meaning |
| --- | --- |
| `{"type":"parent_output_equals","pointer":"/route","value":"fast"}` | Parent completes successfully and the selected output value equals `"fast"` |
| `{"type":"parent_output_exists","pointer":"/artifact"}` | Parent completes successfully and the selected output value exists, including JSON `null` |
| `{"type":"sibling_succeeded","task_id":"task_..."}` | Another child of this same parent reaches `done` or `merged` |

Pointers use JSON Pointer syntax: `/` separates keys, `~1` means `/`, and `~0`
means `~`. An empty pointer selects the whole output object. Equality compares
JSON values, including types and nested structures; missing differs from null.
Each condition is limited to 64 KiB and pointers to 1024 bytes. No expressions,
report-text matching, network calls, or scripts are evaluated.

Outputs are JSON objects recorded atomically with successful completion:

```json
POST /v1/tasks/PARENT/status
{"status":"merged","output":{"route":"fast","artifact":"build.zip"}}
```

Use the normal assignee/testing/review workflow and reviewer permissions.
Lease holders can instead post `{"output":{...}}` to `/v1/tasks/ID/done`.
Omitted output records `{}`; output must be an object of at most 64 KiB.
Output cannot be attached before completion, edited afterward, or supplied on a
failed/cancelled transition. Existing completed tasks without recorded output
are treated as `{}` for predicates. Task details and JSON exports include the
stored output, condition, optional flag, and enablement flag.

A condition remains unresolved while its source is active. Successful source
completion resolves equality/existence or sibling success. A false predicate,
missing path, or cancelled/skipped sibling resolves the branch to terminal
`skipped` without execution. Active descendants of a skipped branch are also
skipped; previously terminal records remain unchanged. Skip decisions and
activation are one-shot, persisted, audited, and committed with the source
transition. Skipped tasks leave active lists/inboxes and do not count as
successful work or satisfy ordinary `blocked_by` dependencies, which still
require `done` or `merged`. They are terminal for child accounting. No child
transition automatically completes its parent. A parent may complete to publish
the output that activates its children.

An optional child also needs explicit enablement:

```text
airc task enable CHILD
```

This calls `POST /v1/tasks/CHILD/enable` and requires a `pm` token scope plus
project-manager/owner authority or the current parent assignee's project
membership. Reviewers and workers do not gain enablement authority merely by
being assigned to the child. Enablement can precede condition resolution; both
gates must pass before activation. A known false condition skips an optional
branch even if it was not enabled. Repeated enablement is rejected.

Parent pauses and cancellations retain their existing recursive behavior.
Pauses save dormant state and enablement across restart; resumption evaluates
ready conditions. Cancellation cancels dormant descendants. Gates cannot bypass
ancestor or dependency guards. Creating nested plans under dormant ancestors
is allowed; those nested tasks also remain dormant until their ancestors
activate. Branch and dependency wait cycles are rejected before commit.

CLI examples (JSON quoting follows the calling shell):

```text
airc task create '#project' 'Fast route' --parent PARENT --assign worker --condition '{"type":"parent_output_equals","pointer":"/route","value":"fast"}'
airc task create '#project' 'Optional audit' --parent PARENT --assign worker --optional
airc task status PARENT merged --output-json @output.json
airc task done LEASED_TASK --output-json @output.json
```

`--condition` and `--output-json` accept inline JSON or `@path` to a UTF-8 JSON
file. Python SDK methods expose `create_task(..., condition=..., optional=True)`,
`set_task_status(..., output={...})`, `done_task(..., output={...})`, and
`enable_optional_task(id)`.

Both batch APIs support these fields. Sibling conditions can use a forward
`$key` reference alongside `parent_task_id` and `blocked_by`. Bulk transitions
accept `output` on successful completion and preserve full rollback of rows,
outputs, decisions, audit events, and counters. If automatic activation or skip
would also mutate an explicitly named bulk task, the batch is rejected; submit
the source transition separately, then operate on the activated branch.
