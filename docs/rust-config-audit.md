# Rust privacy and editable configuration audit

Task: `task_9e6dcb5fbed447c8878b3368e29b1b6d`. Baseline: `e7249f8`.
Scope: every Rust source under `src/` and `Cargo.toml`, including test modules.

## Findings and changes

- Personal nick and repository-owner identifiers occurred in TUI and wake test
  fixtures. These now use generic account, project, and repository values.
  Mention-parser fixtures also use synthetic worker/reviewer names.
- No personal home directory, fixed machine name, real credential, or
  user-specific runtime default was identified in the scoped source. Cargo's
  manifest contains package metadata and public dependencies only.
- Server addresses, DB location, TLS paths, and owner invite settings already
  accepted flags/environment variables. They now also accept an editable TOML
  file. Existing defaults are preserved when no file exists.
- Rust's profile directory lookup did not honor `XDG_CONFIG_HOME`, unlike the
  Python CLI. Server config discovery and TUI profile lookup now share the
  same directory resolver, including this override and blank-value fallback.
- Local `aircd.toml` files are ignored by Git because they may contain paths or
  credentials. The generic [example](../aircd.toml.example) is tracked.

## Configure the Rust server

Copy `aircd.toml.example` to `aircd.toml` in the configuration directory, edit
the settings, and restart the server. Select another file using
`aircd --config <path>` or `AIRCD_CONFIG`. An explicitly selected file must
exist. A missing default file is allowed. Unknown fields, malformed TOML, and
wrong value types fail startup; errors omit the file contents to avoid leaking
credentials.

Directory lookup, in order:

1. `AIRCD_CONFIG_DIR`.
2. `%LOCALAPPDATA%/aircd` on Windows.
3. `$XDG_CONFIG_HOME/aircd`.
4. `$HOME/.config/aircd`, then `$USERPROFILE/.config/aircd`.
5. `./.config/aircd` if no home directory is available.

Blank directory variables are skipped. This directory also contains the TUI's
editable `profiles.json` (server URL, profile name/nick, and issued token).

Settings use **flags > environment > file > defaults**:

| TOML field | Flag | Environment | Default |
| --- | --- | --- | --- |
| `bind` | `--bind` | `AIRCD_BIND` | `127.0.0.1:6667` |
| `http_bind` | `--http-bind` | `AIRCD_HTTP_BIND` | `127.0.0.1:6680` |
| `db` | `--db` | `AIRCD_DB` | `aircd.sqlite3` |
| `tls_cert` | `--tls-cert` | `AIRCD_TLS_CERT` | absent |
| `tls_key` | `--tls-key` | `AIRCD_TLS_KEY` | absent |
| `tls_bind` | `--tls-bind` | `AIRCD_TLS_BIND` | `127.0.0.1:6697` |
| `register_key` | `--register-key` | `AIRCD_REGISTER_KEY` | generated once beside DB |
| `open_registration` | `--open-registration` | `AIRCD_OPEN_REGISTRATION` | `false` |

Relative DB and TLS paths from the file resolve beside that file. Without a
file, the DB default stays relative to the current working directory. Paths
from flags/environment keep their existing working-directory semantics.
`:memory:` remains a special DB value. Values are literal: shell variables,
`%VAR%`, `$VAR`, and `~` are not expanded in TOML strings. Set both TLS paths to
enable TLS. Registration defaults to an owner invite key; no real key is in
the example. Boolean environment overrides accept true/false, yes/no, or 1/0.

Existing launchers pass explicit flags, which take precedence over the file.
TUI hub startup explicitly selects the profile directory's DB and the HTTP
address being opened. Change `AIRCD_CONFIG_DIR` to relocate that DB, or start
the server directly with the desired file/flags before attaching the TUI.

## Other persisted or environment-specific settings

| Area audited | Source of values |
| --- | --- |
| Authentication, tokens | Registered principals and hashed issued tokens in SQLite; random token generation, no fixed live token |
| Owner invite | Explicit config/env/flag or random `aircd-owner.key` beside DB |
| Webhooks and Git | Editable SQLite/API configuration for URLs, repo paths, branches, secrets; new secrets generated randomly |
| Wake and worker templates | Editable SQLite/API configuration for executable, args, cwd, env, prompts, models, sessions, rate limits |
| Groups/projects/rooms/tasks | Editable SQLite/API data for members, paths, roles, prompts, project and sprint names |
| History/locks/notes/reports/protocol | Caller-supplied or stored values; protocol names and generic examples only |
| Host discovery | Bind addresses and `COMPUTERNAME`/`HOSTNAME`; no fixed personal hostname |
| Task lease | `AIRCD_TASK_LEASE_SECONDS` (default 300 seconds) |
| Demo/test credentials | In-memory DBs or explicit `AIRCD_SEED_DEMO=1`; existing normal disk startup revokes published demo credentials |

Loopback/default ports, public provider command names, generic example paths,
reserved example domains, and deliberately public demo/test secrets are not
personal configuration. No local profiles, live DBs, installed launchers,
running services, remote branches, or published history were modified by this
audit. Historical cleanup is documented separately in `history-cleanup.md`.

## Verification

Config tests cover directory discovery, missing/default/explicit files,
relative paths, flag/environment precedence, TLS pairs, empty values, unknown
settings, safe parse errors, and the checked-in example. The task's handoff
includes full Rust regression results and an isolated server startup report.

- `cargo test --locked`: 185 passed, 0 failed, 0 ignored, including 7 new
  configuration tests.
- `cargo build --locked`: passed using a separate target directory.
- `cargo clippy --locked --all-targets`: passed; warnings are in existing
  code, with none in the new configuration module.
- Isolated executable smoke test: 10 checks passed, including actual config
  discovery and precedence, registration/invite enforcement, rejection of demo
  credentials on a normal disk DB, both TUI profile-directory overrides, and
  credential-free server output and parse errors.
- Source pattern scan: all 21 Rust/manifest files covered; no owner identifiers,
  personal home paths, fixed machine names, provider keys, private-key headers,
  or credential URLs. The sole email match is a reserved example-domain fixture.
- `git diff --check` and default-config ignore/example-trackability checks
  passed. New configuration/startup code passes rustfmt; remaining formatting
  differences in existing TUI/wake code match the baseline after fixture renames.
