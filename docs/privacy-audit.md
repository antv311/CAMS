# Git privacy audit

Audit date: 2026-11-12. Task: `task_f7b62c71a18b438190d6713635b4aefb`.
Repository baseline: `c16aece` (`origin/main`).

## Findings

- A personal Windows user/project path remains in the `src/http.rs` test fixture
  in published history: 7 blob versions across 9 commits, including `a88c294`
  and `eda4c67`. These commits are ancestors of `origin/main`. The literal value
  is deliberately omitted here. Commit `38855e1` replaced it with a generic path,
  so it is absent from the current tracked files. Removing it from the tip did
  not remove it from history.
- The fork author's commits use a GitHub noreply email. Upstream history retains
  its contributor's name and non-noreply email as author attribution. Current
  source email matches are test fixtures. The public repository name/account also
  occurs in current source and documentation.
- No real credentials, private keys, phone numbers, or SSN-shaped values were
  identified by the scans. Token/password literals inspected in code were demo
  values, test fixtures, environment expressions, or generated-value prefixes.
  No database, environment file, saved profile, key, or log file was found among
  historical tracked filenames. Pattern scans are not a guarantee that every
  kind of personal information has been detected.
- Ignore rules previously covered database files but missed local hub state,
  owner keys, token profiles, SQLite sidecars, environment files, TLS keys, and
  logs. These could otherwise have been staged accidentally. No untracked or
  ignored local files existed in this checkout at the start of the audit.

## Preventative change

The root `.gitignore` now covers `.aircd/`, `aircd-owner.key`, `profiles.json`,
SQLite journals/WAL/SHM files, `.env` variants, TLS key containers, and logs.
Example environment templates remain trackable. These rules prevent ordinary
`git add` from including matching untracked files; they do not remove files
already in history, and `git add -f` can override them.

## Verification

- Fetched all origin branches and tags. Inspected all 19 remote branches,
  89 reachable commits, and all 229 blob objects in the local object database
  (577 objects total); no binary blobs or submodules were present.
- Supplemental scan covered user-home paths, email addresses, credential URLs,
  provider API keys, private keys, JWTs, token/password literals, IPv4 addresses,
  phone patterns, SSN patterns, and known local identity strings. Commit metadata
  was also inspected. All distinct candidate values were reviewed locally.
- Gitleaks v8.30.1 was downloaded from its official release with its published
  SHA256 checksum verified. `gitleaks git . --log-opts="--all --full-history"
  --redact --ignore-gitleaks-allow` reported no leaks (79 commits reported by
  that scanner; the object/metadata scan additionally covers all 89 commits).
- `gitleaks dir . --redact --ignore-gitleaks-allow` reported no leaks in the
  working tree. Reports were stored outside the repository with secrets redacted.
- Exported all 229 historical blob versions outside the repository and scanned
  them independently with `gitleaks dir`: no leaks in 5.61 MB of blob content.
- `git check-ignore --no-index` passed 30 sensitive-path cases and 12 source or
  example-template controls. All 47 baseline tracked paths remain trackable.
  `git diff --check` passed. No application code changed, so application tests
  were not needed for this ignore-rule and documentation change.

## Approved history cleanup

On 2026-11-12 the owner authorized rewriting the published history and
force-pushing it under task `task_ac358a96fabf4c649024f27d2f993053` (hub message
126). The cleanup replaces only the accidental test-fixture path with a generic
path. It preserves contributor attribution, commit messages, merge topology,
and the merged privacy-ignore and worker-template storage changes.

See [history cleanup and clone recovery](history-cleanup.md) for the procedure,
verification requirements, and instructions for existing clones. The original
findings above describe the pre-cleanup history; old commit hashes may no longer
be reachable from published branches after the rewrite.

## Client, fixture and documentation audit

Audit date: 2026-11-13. Task: `task_7b7f36b12c1345809df6b066c66fb335`.
Scope: tracked files under `clients/` (client, CLI, tests, examples), `docs/`
(including `docs/templates/`), `scripts/`, `README.md` and the root `.cmd` files.

- No credentials, user-home paths, machine names or real email addresses were
  found. Fixture emails use `example.local`. Tokens are labelled demo values
  such as `agent-token` or `test-token-1`. Paths in examples are placeholders
  such as `C:\path\to\repo`.
- Removed personal identifiers from documentation. The README used the
  owner's personal nick and a live agent account as example nicks; these are
  now `owner`, `gemini-flash` and `pm-agent`. `docs/sprint-1.md` named the
  owner and now says "the owner's message 472". The public repository link
  and upstream attribution are kept on purpose.
- Config: `airc` already reads hub URLs and tokens from an editable
  `profiles.json` found in `AIRCD_CONFIG_DIR`, `%LOCALAPPDATA%\aircd`,
  `$XDG_CONFIG_HOME/aircd` or `~/.config/aircd`. Flags and `AIRCD_*`
  environment variables override it. The README now documents this
  ("Client configuration"). The demo scripts take `AIRCD_HOST` and
  `AIRCD_PORT` instead of a fixed `localhost:6667`.
- Regression guard: `clients/python/tests/test_config_privacy.py` tests
  config-directory resolution and override precedence. It also scans the
  tracked files above for user-home paths and non-reserved email domains. The
  current machine's user and host names are read at run time, so no personal
  value is stored in the test.
