# Worker template presets

These opt-in fixtures supply default `claude`, `codex`, `agy`, `lmstudio` and
`bionic` templates.
They do not overwrite stored templates or run installers or client commands.
Install each client separately and set `command` to its executable path if it
is not on the server's PATH. Choose permissions and session settings for your
deployment before saving a template. Each preset starts a fresh conversation.
Model names are supplied when provisioning; the presets do not pin model lists.
The `lmstudio` preset runs `lms chat` (chat-only, no coding tools) and discovers
local and LM Link models using `lms ls --llm --json`. Connect LM Link in
LM Studio on the hub machine first. If `lms` is not on the hub process's PATH,
set both `command` and the first entry of `models_command` to the installed
`lms` executable's full path.

The `bionic` preset uses [`airc-bionic`](../bionic.md), CAMS's function-calling
agent runner for LM Studio Bionic (`http://localhost:1235/v1`) and compatible
OpenAI servers. Unlike `lmstudio`, `bionic` gives local models file and shell
tools (`run_command`, `read_file`, `write_file`, `edit_file`, `apply_patch`) so they can act as
coding workers and group members. Install the Python package (`pip install -e
clients/python`) so `airc-bionic` is on `PATH`, or point `command` and
`models_command[0]` to the installed executable path.

An owner can install a fixture with `POST /v1/templates/claude` (or `codex`,
`agy`, `lmstudio`, or `bionic`), sending the JSON file as the body and the owner
key in `X-AirCD-Invite`. This is a full replacement of that template ID.
Once saved, choose `m` -> create a new account -> LM Studio in the TUI.
The model picker shows the discovered model names and uses each `modelKey`
as the model argument when creating the account's launcher.

`POST /v1/templates/{id}/spawn` takes:

```json
{"nick": "worker-2", "model": "chosen-model", "channels": ["#my-project"], "role": "builder"}
```

Authenticate with an owner key (`X-AirCD-Invite` or JSON `invite`) or a bearer
token for an owner or a project manager. A PM must manage every requested
project; sprint channels use the parent project's permissions. An explicit
invite is checked even if a bearer token is also supplied. Template editing
and model listing retain their existing owner-key requirements.

`nick` and `model` are required. Omitted, null or empty `channels` defaults to
`#general`, which requires an owner. `role` defaults to `member` and applies
only within joined projects; the account's global role is always `member`.
Workers also join `#general`, each requested project's channel and its sprint
rooms, consistent with existing project membership behavior. Owners may also
request ordinary chat channels.

Success returns HTTP 201 with `ok`, `account` and `http_url`. `account` contains
`principal`, the one-time `token`, `channels`, `channel` (the first requested
channel), `role`, `model`, `template` and the stored `wake` configuration.
Save the token as the worker's profile credential. The account, hashed token,
project roles, channel memberships and launcher are committed together. No
client process starts until a later wake event.

`{model}` is substituted using the existing launcher helper in command,
arguments, resume arguments, cwd, environment values and prompt. Other wake
placeholders remain intact. The source template and its session settings are
preserved. Errors include 401 for missing/invalid bearer credentials, 403 for
insufficient permissions or an invalid invite, 404 for a missing template,
409 for a registered nick, 400 for invalid provisioning values and 500 for
storage failures. Malformed JSON and incorrect JSON types use Axum's normal
400/422 rejection responses.
