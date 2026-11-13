# CAMS Sprint 2 checkpoint

Coordination room: `#cams-s2`.
Checkpoint task: `task_1fb414872f194471ac461e6d1ae85e67`.
Base `main` commit: `b164cc8`.

## Integrated work

| Issue | Delivery | Task | Commit(s) |
| --- | --- | --- | --- |
| #12 | Automated task worktree lifecycle and `task.repo` validation against project `git_repos` | `task_0fc4326035b84bd9865e0a029056c7b8` | `abf1f24` (merged `ddafa47`) |
| #13 | Unique token labels for repeated `airc login --rotate` | `task_d5faedc0c0b64196b26683292be06fe2` | `9eef792` |
| #14 | Atomic token reissue and IRC session invalidation | `task_30662416b057453ba313fa5e2fc7e7d8` | `2a5ff68` (merged `932e284`) |
| #15 | DM wake loop prevention between automated accounts | `task_543c921157344e878f5eef4b40457b12` | `6f08b8a` (merged `04ce7fd`) |
| #19 | TUI direct messages pane and private conversation view | `task_7fd8cb4d42264ce7bead451457a94830` | `9119447` (merged `97fb7f2`) |
| #21 | TUI home screen key-hint/tab colors and sharpened people-pane dots | `task_57c838e3093644868d8a994952bb66d4` | `054d2ac` (merged `d715f56`) |
| #22 | Enforce test evidence and subtask review workflow | `task_14e8d26b84a14f04a8ddab47fad1a377` | `ef11ef0` (merged `8837a59`) |
| #22 | Python groups/subtasks client, `airc group`, subtask flags, and `--report` handoff guard | `task_8d8103743f6e4833b7ca34eacd432f2b` | `e8b2cfd`, `3b1f4f3` |
| #22 | Subtask hierarchy and reviewer display in TUI | `task_702c6c21162b4fe5bc910c70b650186f` | `ab4da86` |
| #23 | Groups SQLite storage and owner-guarded HTTP endpoints | `task_e7fa88d159ad4d82bd93d7c82f47bbb5` | `4c8ffa4` (merged `fc08949`) |
| #23 | Apply stored group role prompts at wake time | `task_a3acf2738f5843f7af5d9778967b9fab` | `dd01375` (merged `b164cc8`) |
| #24 | Windows startup tray server controls | `task_cfe3002fc528437880bfc05568084510` | `f17ae00` |
| #24 | Offer to start the hub from the TUI when unreachable | `task_bec8c53eb3b949f3890b9b7d0aeab605` | `fb581f4` |
| #24 | Windows service and scheduled task installer for autonomous hub execution | `task_36c2b15db4534557a5d2be3a98a8e054` | `acce9e9` (merged `5a4154a`) |
| - | LM Studio client template and LM Link model discovery | `task_b6dce9b611c04b6685bfe94d8859db5e` | `fff72f5` |
| - | Launch newest Windows hub build (`Get-AircdExecutable.ps1`) | `task_9117a0995b0f4b10a973bf1cae6b493d` | `b6fbb9c` |
| - | TUI chat input wrapping and cursor movement | `task_b2eed71aface413f9709ac0902ad52bd` | `afec076` |

This checkpoint branch also resolves the stop-inspection race in `scripts/AircdService.psm1` (`Get-AircdServiceServer` / `Stop-AircdServiceInstance`) when `aircd.exe` is exiting after a stop signal, and hardens `finally` process termination and temporary directory cleanup in `scripts/Test-AircdService.ps1`.

## Verification

On `docs/sprint-2-checkpoint` (rebased onto `b164cc8`):

- `cargo test --locked`: **170 passed**, 0 failed, 0 ignored (54.57s).
- `cargo build --release --locked`: passed (1m 22s), producing `target\release\aircd.exe`.
- PowerShell Windows integration checks against `target\release\aircd.exe`:
  - `powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\Test-AircdService.ps1 -AircdExe target\release\aircd.exe`: **3/3 consecutive runs passed**.
  - `powershell.exe -NoProfile -STA -ExecutionPolicy Bypass -File scripts\Test-AircdTray.ps1 -AircdExe target\release\aircd.exe`: **passed**.
  - `powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\Test-AircdWorktree.ps1`: **passed**.
  - `powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\Test-WindowsLaunchers.ps1`: **passed**.
- Python `unittest` suites (with `PYTHONPATH=clients/python` and `AIRCD_BIN` pointing to `target\release\aircd.exe`): **47 passed**, 0 failed across all 5 `unittest` modules:
  - `test_cli_e2e.py`: **5 passed** (14.693s).
  - `test_groups.py`: **26 passed** (14.178s).
  - `test_login.py`: **2 passed** (1.125s).
  - `test_templates.py`: **12 passed** (15.907s).
  - `test_worktree.py`: **2 passed** (2.518s).
- `pytest` is absent from the system Python (`No module named pytest`), so the pytest-based modules (`test_daemon.py`, `test_delivery.py`, `test_integration.py`, `test_protocol.py`) were not run.
- `git diff --check`: passed.

## Operational instructions

### Release binary build and deployment

1. Build the release binary from the repository root:

   ```powershell
   cargo build --release --locked
   ```

   This produces `target\release\aircd.exe`. Both `scripts\Get-AircdExecutable.ps1` and the Windows service host (`HostRunner.ResolveExecutable`) automatically select the newest modified binary between `target\release\aircd.exe` and `target\debug\aircd.exe`, with `target\release\aircd.exe` winning timestamp ties.

2. Roll out the new binary to an already-installed hub without disturbing the SQLite database or `aircd-owner.key`:
   - If using the autonomous service or scheduled task:

     ```powershell
     .\scripts\Install-AircdService.ps1 -Restart
     ```

   - If using the Windows tray controller: right-click the **C** tray icon and select **Restart Server**.
   - Any already-open `aircd tui` session keeps its existing executable until closed and re-opened (or re-launched via **Launch TUI**).

### Autonomous service / scheduled task auto-start (`Install-AircdService.ps1`)

Install and start the hub so it runs independently of any terminal or assistant session:

```powershell
.\scripts\Install-AircdService.ps1 -StartNow
```

- **Default (`-Mode Auto`)**:
  - From a normal PowerShell prompt: registers a hidden `AtLogOn` Scheduled Task (`CAMS-Aircd`) in the current user's context (survives closing terminals/assistant sessions and restarts on sign-in).
  - From an elevated Administrator PowerShell prompt: registers an `AtStartup` Scheduled Task with `LogonType S4U` (starts at boot before sign-in and survives user logout without storing a password, while keeping user profile and agent CLI paths).
- **Windows Service (`-Mode Service`, Administrator prompt required)**: installs a native Windows Service (`CAMS-Aircd`) with automatic startup and SCM failure recovery (`restart/5000` reset after 60s).
- **Lifecycle management**: use `-Status` (or `-Status -Json`), `-Start`, `-Stop`, `-Restart`, and `-Uninstall`. Uninstalling removes the service/task and `service\` host files while preserving the SQLite database, `aircd-owner.key`, and logs (`aircd.service.log`, `aircd.stdout.log`, `aircd.stderr.log` in `%LOCALAPPDATA%\aircd\` by default).

### Windows tray auto-start (`Install-AircdTray.ps1`)

Install and start the system tray controller for interactive desktop sign-in:

```powershell
.\scripts\Install-AircdTray.ps1 -StartNow
```

- Creates `CAMS tray.lnk` in the user's Windows Startup folder to run `scripts\Start-AircdTray.ps1` hidden at sign-in (and migrates any matching checkout `CAMS hub.lnk` shortcut with a `.before-tray` backup).
- Right-click the **C** tray icon for **Restart Server**, **Launch TUI**, **Stop Server**, logs, server details, or **Exit Tray (keep server running)**; double-click launches `aircd tui`.
- To remove sign-in startup: run `.\scripts\Install-AircdTray.ps1 -Uninstall`, then choose **Exit Tray** from the tray menu.
- Defaults bind `127.0.0.1:6667` (IRC), `127.0.0.1:6680` (HTTP), and `%LOCALAPPDATA%\aircd\aircd.sqlite3`. Pass `-Bind`, `-HttpBind`, `-DbPath`, or `-Profile` if using custom hub settings.
