# Windows Scripts & Service Launchers Privacy and Config Audit

Task: `task_89e8141c4b52443789437bf3c2c9b075`
Branch: `audit/hydrogen-scripts-config`

## Scope and Inspected Files

All root `.cmd` launchers (2 files) and every file in `scripts/` (18 files) were audited:

### Root `.cmd` launchers
1. `airc.cmd` — Resolves repository root dynamically via `%~dp0`, prepends `clients\python` to `PYTHONPATH`, and invokes `AIRCD_PYTHON` (if set) or `python -m aircd.cli %*`. No hardcoded personal data.
2. `aircd-server.cmd` — Resolves repository root via `%~dp0`, honors `AIRCD_EXECUTABLE` if set to an existing file, otherwise resolves the newest build via `scripts\Get-AircdExecutable.ps1`, with `cargo run` fallback. No hardcoded personal data.

### PowerShell modules and scripts (`scripts/`)
3. `scripts/AircdConfig.psm1` *(added)* — Centralizes dynamic data/config directory resolution and editable config file loading (`CLI > Environment > ConfigFile > Defaults`).
4. `scripts/aircd-windows.json.example` *(added)* — Editable JSON config template using `%LOCALAPPDATA%` placeholders and loopback defaults.
5. `scripts/AircdService.psm1` — Replaced hardcoded `"$env:LOCALAPPDATA\aircd\aircd.sqlite3"` parameter default with `Resolve-AircdWindowsConfig` (`CLI > Env > ConfigFile > DynamicDefault`). Verified C# `AircdServiceHost` source contains no personal paths, credentials, or fixed machine names.
6. `scripts/Install-AircdService.ps1` — Added `-ConfigPath` support and bound-parameter forwarding to `New-AircdServiceConfig`, plus env/file fallback for `-Mode` and `-Trigger`.
7. `scripts/AircdTray.psm1` — Updated `New-AircdTrayConfig` to resolve settings via `Resolve-AircdWindowsConfig` (`CLI > Env > ConfigFile > DynamicDefault`), added support for `OpenRegistration`, `RegisterKey`, `TlsCert`, `TlsKey`, `TlsBind`, `ExecutablePath`, and `StartupDirectory`, and added `Resolve-AircdTrayExecutable`.
8. `scripts/Start-AircdTray.ps1` — Added `-ConfigPath`, registration, TLS, and `-ExecutablePath` parameters and bound-parameter forwarding to `New-AircdTrayConfig`.
9. `scripts/Install-AircdTray.ps1` — Added `-ConfigPath`, registration, TLS, and `-ExecutablePath` parameters, dynamic PowerShell executable lookup via `[Environment]::GetFolderPath('System')`, and config-resolved `StartupDirectory`.
10. `scripts/Start-Aircd.ps1` — Integrated `AircdConfig.psm1` (`CLI > Env > ConfigFile > DynamicDefault`) with fallback when invoked standalone, added `-ConfigPath`, registration, TLS, and `-ExecutablePath` support, and made printed URL/IRC host output dynamic.
11. `scripts/Get-AircdExecutable.ps1` — Resolves `$RepoRoot` dynamically from `$PSScriptRoot` (or `-RepoRoot`) and picks the newest `target\release\aircd.exe` / `target\debug\aircd.exe`. No personal data.
12. `scripts/New-AircdWorktree.ps1` — Resolves repo root dynamically via `git rev-parse`. No personal data.
13. `scripts/Test-AircdService.ps1` — Uses temp directories (`[IO.Path]::GetTempPath()`), synthetic test fixtures (`secret-123`, `C:\trailing slash\`), isolated config/data env vars, and added editable config/env/CLI precedence assertions.
14. `scripts/Test-AircdTray.ps1` — Uses temp directories, synthetic test fixtures (`profile with spaces`, `C:\trailing slash\`), isolated config/data env vars, and added editable config/env/CLI precedence assertions.
15. `scripts/Test-AircdWorktree.ps1` — Uses temp directories and synthetic git config (`tester@example.local`, `Tester`). No personal data.
16. `scripts/Test-WindowsLaunchers.ps1` — Uses temp directories, isolated config/data env vars, and tests `aircd-server.cmd` (`AIRCD_EXECUTABLE`) and `Start-Aircd.ps1` JSON/TOML config + env + CLI precedence.

### POSIX shell scripts (`scripts/`)
17. `scripts/demo.sh` — Uses `REPO_ROOT`, `mktemp /tmp/...`, `localhost`, and seeded demo tokens. No personal data.
18. `scripts/e2e-claude.sh` — Uses `REPO_ROOT`, `mktemp /tmp/...`, `CLAUDE_MODEL`, and seeded demo tokens. No personal data.
19. `scripts/start-agent.sh` — Parameterized via `AIRCD_*` env vars and CLI flags. No personal data.
20. `scripts/start-human-irssi.sh` — Uses ephemeral `mktemp` home and `AIRCD_*` env vars / CLI flags. No personal data.
21. `scripts/start-server.sh` — Uses `REPO_ROOT/.aircd` and `AIRCD_DB` / `AIRCD_BIND` env vars / CLI flags. No personal data.
22. `scripts/start-workspace.sh` — Uses `REPO_ROOT/.aircd/workspace` and `AIRCD_*` env vars / CLI flags. No personal data.

## Hardcoded Personal Data Findings

- **Personal paths, usernames, credentials, fixed machine names**: None found across any of the 20 baseline `.cmd`, `.ps1`, `.psm1`, and `.sh` files.
- **Improvements made**:
  - Eliminated raw `"$env:LOCALAPPDATA\..."` string interpolation in `param(...)` defaults (which previously degraded to `\aircd\aircd.sqlite3` if `LOCALAPPDATA` was unset) in favor of `Get-AircdDefaultDataDir` (`AIRCD_DATA_DIR` -> `LOCALAPPDATA` -> `[Environment]::GetFolderPath('LocalApplicationData')` -> `USERPROFILE\AppData\Local` -> `$RepoRoot\.aircd`).
  - Added editable config file loading (`aircd-windows.json`, `aircd.json`, `aircd.toml`) supporting `%ENV_VAR%` expansion and config-relative paths.

## Editable Configuration & Precedence

Settings resolve in strict order:
1. **Explicit CLI parameters** (e.g., `-Bind`, `-HttpBind`, `-DbPath`, `-Profile`, `-Name`, `-ExecutablePath`)
2. **Environment variables** (`AIRCD_BIND`, `AIRCD_HTTP_BIND`, `AIRCD_DB` / `AIRCD_DB_PATH`, `AIRCD_OPEN_REGISTRATION`, `AIRCD_REGISTER_KEY`, `AIRCD_TLS_CERT`, `AIRCD_TLS_KEY`, `AIRCD_TLS_BIND`, `AIRCD_EXECUTABLE`, `AIRCD_SERVICE_NAME`, `AIRCD_TASK_PATH`, `AIRCD_SERVICE_MODE`, `AIRCD_SERVICE_TRIGGER`, `AIRCD_PROFILE`, `AIRCD_STARTUP_DIR`)
3. **Editable config file**, discovered from:
   - `-ConfigPath` (CLI)
   - `$env:AIRCD_WINDOWS_CONFIG` or `$env:AIRCD_CONFIG`
   - `<RepoRoot>\aircd-windows.json` or `<RepoRoot>\aircd.toml`
   - `<ConfigDir>\aircd-windows.json`, `<ConfigDir>\aircd.json`, or `<ConfigDir>\aircd.toml` (where `<ConfigDir>` resolves via `AIRCD_CONFIG_DIR` -> `XDG_CONFIG_HOME\aircd` -> `Get-AircdDefaultDataDir`)
4. **Built-in dynamic defaults**
