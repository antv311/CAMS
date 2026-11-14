# Task tags and metadata

Tasks expose `tags: []` and `metadata: {}` by default, including migrated tasks.
Tags are case-sensitive strings, sorted and deduplicated. Spaces, commas and
Unicode are literal parts of a tag. Each task accepts at most 64 input tags;
each tag must be nonblank, contain no control characters and fit in 128 UTF-8
bytes. Metadata is a JSON object with arbitrary keys and JSON values, including
nested objects, arrays and null values, up to 65,536 serialized UTF-8 bytes.

Both `POST /v1/tasks` and `POST /v1/tasks/batch` accept `tags` and `metadata`.
All validation participates in the existing atomic create transaction.
`GET /v1/tasks?channel=%23cams&tag=release` matches exact tag membership and
combines with the existing priority, repo and blocked filters. The list still
excludes terminal tasks and keeps its existing dispatch order. URL-encode tag
values. Metadata values are returned in task reads, inboxes and JSON exports;
metadata filtering is not supported.

`PATCH /v1/tasks/{id}/attributes` accepts `{"tags":["release"],"metadata":{"ticket":42}}`.
Each supplied field replaces that entire field; omitted fields are preserved.
`[]` clears tags and `{}` clears metadata. Null, unknown fields, invalid values
and empty requests are rejected. Updates are atomic and require a write-scoped
token plus the task's assignee, current unexpired lease holder, reviewer, parent
lead or project manager (including the owner). Attributes can be edited on
completed tasks. Updates preserve status, leases, branch gates and immutable
completion output; they do not dispatch workers. A changed update records one
`attributes` audit event indicating which fields changed, without copying
metadata values into chat or audit notes. Identical updates add no event.

```sh
airc task create '#cams' 'Ship UI' --tag ui --tag release --metadata '{"ticket":42}'
airc task list '#cams' --tag release
airc task tags TASK_ID                      # show
airc task tags TASK_ID --set ui --set ready  # replace
airc task tags TASK_ID --clear
airc task metadata TASK_ID                  # show
airc task metadata TASK_ID '@metadata.json' # replace from UTF-8 JSON file
airc task metadata TASK_ID '{}'             # clear
```

Creation also accepts `--metadata @path`. `airc task show` displays both fields.
Shell completions include the new flags and subcommands automatically.

```python
task = hub.create_task("#cams", "Ship UI", tags=["ui"], metadata={"ticket": 42})
hub.list_tasks("#cams", tag="ui")
hub.set_task_attributes(task["id"], tags=["ready"])
hub.set_task_attributes(task["id"], metadata={"ticket": 43})
```

The Python SDK and CLI omit unset creation fields for compatibility with older
hubs. Attribute editing and tag filtering require a hub with this feature.
