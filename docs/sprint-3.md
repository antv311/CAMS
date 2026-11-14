# CAMS Sprint 3 checkpoint

Coordination room: `#cams-s3`.
Checkpoint task: `task_2233f124fdeb4490898dc523be569fac`.
Base `main` commit: `b5ab696` (all commits after Sprint 2 checkpoint `e7249f8`).

## Integrated work

| Area | Delivery | Task | Commit(s) |
| --- | --- | --- | --- |
| Privacy & Config | Audit client docs/fixtures, add editable client config and owner GitHub account guard | `task_7b7f36b12c1345809df6b066c66fb335` | `b60bdee`, `8684553` (merged `500b5a8`, `e9fe902`) |
| Privacy & Config | Audit Windows scripts/launchers, eliminate hardcoded local paths, add `AircdConfig.psm1` & `aircd-windows.json.example` | `task_89e8141c4b52443789437bf3c2c9b075` | `b77172c` (merged `88cb502`) |
| Privacy & Config | Fix `Test-AircdTray.ps1` `Wait-TrayLog` shared log reader lock collision | — | `f94360b` |
| Privacy & Config | Audit Rust core/fixtures and add editable `aircd.toml` config (`flags > env > file > defaults`) | `task_9e6dcb5fbed447c8878b3368e29b1b6d` | `32ed822` (merged `f016935`) |
| Bionic Runner | Integrate LM Studio Bionic function-calling runner (`airc-bionic`) and `bionic` template preset | `task_10fc64691ae64d1ab11f2ddd53adc897` | `16ca76b` (merged `ab10da6`) |
| Bionic Runner | Jail Bionic file/shell tools to workspace root and add `auto` / `prompt` / `read-only` approval modes | `task_5a92792fd1784c2383ddbbe433dfbffc` | `b5257f7` (merged `0a186a9`) |
| Bionic Runner | Add `apply_patch` unified-diff tool with context matching and atomic all-or-nothing writes | `task_6ecd7e297375435096000e4bc38cd5f4` | `94f08ce` (merged `799a905`) |
| Bionic Runner | Add SSE token streaming (`--stream`, `--stream-format text\|jsonl`) to Bionic | `task_cbd1e4443baf41fd8137a670c43f8b09` | `f7e3d44` (merged `71c146b`) |
| Bionic Runner | Add `BionicAgent` Python SDK client (`aircd.BionicAgent`, usage tracking, `stream()`) | `task_af040d7be1e549f8a3283a3fad0533f4` | `e400415` (merged `6ae4d42`) |
| Bionic Runner | Add `airc-bionic --health` readiness check and `--auto-pull` pre-task gate | `task_bd776ef9de854ccbb44c2856b3fef0a7` | `83eddad` (merged `cc00e5b`) |
| Bionic Runner | Add `airc-bionic-bench` benchmark and multi-turn evaluation suite | `task_33fd0dc8238945a9b48ff51def1ce2d0` | `85a5b46` (merged `2f1f351`) |
| Groups | Atomic group membership updates (`PATCH /v1/groups/{name}`, `airc group update`) | `task_fb44e703e15a407593892539eb011cd8` | `0dba56b` (merged `4e543bc`) |
| Groups | Route group leader subtask creation, delegation, reviewer defaults, and targeted wakes | `task_56dd1311ba0e42239a3c1157fba41b17` | `409de8c` (merged `0122267`) |
| Groups | Revision-bound group peer consensus (`review_policy`, `super_review`, `airc task peer-review`) | `task_0cdb3bb573104f088c05d4262356228e` | `bb5b274` (merged `c944bde`) |
| Groups | Persistent shared group `notes` and `scratchpad` with optimistic revision checks (`airc group memory`) | `task_6eb6e79459c94b7ca5f3db9327481fc1` | `d0e1829` (merged `8ebf9ca`) |
| Groups | Opt-in group inactivity task reassignment (`inactivity_timeout_secs`, `heartbeat`, `events`) | `task_3b42b8467d9249698c6b077df6a87b5a` | `05fd446` (merged `1647fb5`) |
| Git & Tasks | Automatic sibling branch rebase on default-branch merge (`airc git rebase-siblings`) | `task_03d5081b4db049a98e43b310722281bc` | `08b968e` (merged `94abcc1`) |
| Git & Tasks | Task test log and evidence file attachments (`airc task attach` / `attachments` / `download`) | `task_2508952aef2342368041ef64d793857b` | `42f8259` (merged `e66109e`) |
| Git & Tasks | Task priority queue (`urgent`/`high`/`normal`/`low`) and priority-ordered inbox dispatch | `task_9123e22d57dd4a368bfead771f7491a1` | `ecaa0cf` (merged `0302479`) |
| Git & Tasks | Cross-repository `blocked_by` task dependency tracking, DAG validation, and unblocked events | `task_0d79f4b0ca2742eda0b4e183d4396d73` | `d3005da` (merged `4ca25e2`) |
| Git & Tasks | Task history and audit log export (`GET /v1/tasks/export`, `airc task export` JSON/CSV + cycle/review metrics) | `task_d5abc29b556a455bb0afbb79474bf124` | `ac69919` (merged `b5ab696`) |
| Groups & Tasks | Subtask cancellation cascade, inherited pause/resume, and child cleanup | `task_043319bea9c0487c889cea9e2e40bd81` | `d44aede` (merged `bf71452`) |
| CLI | Shell tab-completion generator (`airc completion bash\|zsh\|powershell`) and cross-platform test fix | `task_f63e663ee50e4969b139464d2e224325` | `c0576a7`, `c94ad4f` (merged `b5dd166`) |
| Operations & Security | Windows Application Event Log and RFC 5424/3164 syslog forwarding for errors & security audits | `task_eddce06f1881462ea2d30c35e61282eb` | `a9bd5b1` (merged `9124967`) |
| Operations & Security | Scheduled SQLite snapshot backup worker (`VACUUM INTO`, integrity check, key-ring manifest, `/v1/owner/backup*`) | `task_95e0171b513640aeb46d53e3fe218dea` | `eef2f0f` (merged `3f3e246`) |
| Operations & Security | SQLite read/write connection pooling, WAL tuning, and `/v1/owner/db*` diagnostics & checkpoints | `task_dc46e7496be744ebba7768e872300abb` | `a476b7b` (merged `d5de209`) |
| Operations & Security | Bearer token expiry (`token_ttl_seconds`), permission scopes (`read`/`write`/`pm`/`admin`), and `/v1/tokens*` refresh/revoke | `task_71b7d08a065743a0bba75b40b8f0830d` | `9dc6375` (merged `1a17b79`) |
| Operations & Security | Token-bucket HTTP rate limiting by IP and principal (`X-RateLimit-*`, HTTP 429) | `task_34208bab32594143baf58b19c01a1633` | `98ef90b` (merged `aefd04b`) |
| Operations & Security | Mutual TLS (mTLS) client certificate verification and HTTPS API listener (`--http-tls-bind`, `--tls-only`) | `task_fab3b3cbc7594f0c89e0220fe9acd16a` | `9c8353c` (merged `f665d2a`) |
| Observability & UI | Prometheus `/metrics` (`text/plain; version=0.0.4`) scrape endpoint | `task_0518364dff634af3940124d3c8555e87` | `b62067e` (merged `c6fc543`) |
| Observability & UI | Zero-dependency web dashboard (`GET /dashboard`, `/v1/dashboard`) for sprint monitoring | `task_0b258298ed1245bc8cf7c892ac622112` | `16a7948` (merged `016186a`) |
| TUI | Dedicated `[G]roups` topology visualization panel (`g`) with live worker states and subtask details | `task_120147845266424aa4fb35f98ef53350` | `a2082bc` (merged `4a8392b`) |
| TUI | Interactive Group Editor (`n`/`e`) and Project Attachment dialog (`a`) in `[G]roups` | `task_7a36f7e063904b5695319179f2cca335` | `bf5b40d` (merged `9b6164c`) |
| TUI | Full-text search modal (`Ctrl+F` / `/`) across sprint channels and DMs plus `GET /v1/messages/search` | `task_14023a2476c142a9838ec82a3b672d13` | `12fa67f` (merged `667be10`) |

### Final integration at checkpoint closure

All 30 backlog tasks for Sprint 3 are merged to `main` at `bf71452` (with this final checkpoint commit bringing the sprint to completion):
- `task_d5abc29b556a455bb0afbb79474bf124` (Task History and Metric Audit Log Export): merged at `b5ab696`, hub task closed as `merged`.
- `task_043319bea9c0487c889cea9e2e40bd81` (Subtask Cancellation Cascade and Child Cleanup): merged at `bf71452`, hub task closed as `merged`.
- `task_2233f124fdeb4490898dc523be569fac` (Sprint 3 Checkpoint Documentation and Verification Report): merged into `main`, hub task closed as `merged`.

## Operational guide updates

In addition to this checkpoint report, this branch updates the repository operational documentation to match Sprint 3 behavior:

- `README.md`: added `## Groups` to the table of contents; updated TUI keybindings (`g`, `/`/`Ctrl+F`, Groups navigation, `Ctrl+D` DMs); added `airc template`, `airc group`, `airc task deps`, `airc task peer-review`, and new `task list` / `task status` flags to the Command Reference; documented bearer token expiry/scopes/refresh and editable `aircd.toml` / `aircd-windows.json` configuration under Security Defaults; and documented SQLite connection pooling, WAL tuning flags, and `/v1/owner/db*` endpoints.
- `docs/templates/README.md`: added `apply_patch` to the `bionic` preset's documented tool list so it matches `docs/bionic.md`.

## Verification

On `docs/sprint-3-checkpoint` (rebased onto `b5ab696`, using isolated `CARGO_TARGET_DIR=target\sprint3-checkpoint-test`):

- `cargo clippy --locked --all-targets`: passed (exit 0; 1 pre-existing `dead_code` warning on `GroupStore::delete` in `src/store.rs:376`, 0 warnings in touched files).
- `cargo build --release --locked`: passed (1m 48s), producing `target\sprint3-checkpoint-test\release\aircd.exe`.
- `cargo test --locked -- --test-threads=4`: **301 passed**, 0 failed, 0 ignored (196.85s).
- Python `unittest` suites (with `AIRCD_BIN` pointing to `target\sprint3-checkpoint-test\release\aircd.exe`): **203 passed**, 0 failed, 0 skipped across all 15 `unittest` modules (`288.881s`):
  - `test_attachments.py`: **1 passed**
  - `test_bionic.py`: **38 passed**
  - `test_bionic_bench.py`: **31 passed**
  - `test_bionic_health.py`: **15 passed**
  - `test_bionic_sdk.py`: **9 passed**
  - `test_cli_e2e.py`: **7 passed**
  - `test_completion.py`: **7 passed**
  - `test_config_privacy.py`: **12 passed**
  - `test_groups.py`: **49 passed**
  - `test_login.py`: **2 passed**
  - `test_mtls.py`: **5 passed**
  - `test_rebase_siblings.py`: **9 passed**
  - `test_task_export.py`: **4 passed**
  - `test_templates.py`: **12 passed**
  - `test_worktree.py`: **2 passed**
- `pytest` is absent from the system Python (`No module named pytest`), so the pytest-only modules (`test_daemon.py`, `test_delivery.py`, `test_integration.py`, `test_protocol.py`) were not run.
- PowerShell Windows integration checks against `target\sprint3-checkpoint-test\release\aircd.exe`:
  - `powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\Test-AircdService.ps1 -AircdExe <release-exe>`: **passed** (note: an earlier parallel run timed out after 20s waiting on the test port while `cargo test` and Python E2E were saturating CPU; standalone run passed cleanly).
  - `powershell.exe -NoProfile -STA -ExecutionPolicy Bypass -File scripts\Test-AircdTray.ps1 -AircdExe <release-exe>`: **passed**.
  - `powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\Test-AircdWorktree.ps1`: **passed**.
  - `powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\Test-WindowsLaunchers.ps1`: **passed**.
- `git diff --check`: passed.

## Rollout notes

1. Build the release binary from the repository root (`cargo build --release --locked`) to produce `target\release\aircd.exe`.
2. Restart the autonomous hub (`.\scripts\Install-AircdService.ps1 -Restart` or tray **Restart Server**) without disturbing the SQLite database or `aircd-owner.key`; any open `aircd tui` session keeps its running binary until closed and reopened.
3. Optional Sprint 3 configuration keys (token TTL, rate limits, log forwarding, snapshot backups, SQLite pool/WAL tuning, mTLS/HTTPS) are documented in `aircd.toml.example`, `scripts/aircd-windows.json.example`, and `README.md`, and take effect on hub restart.
