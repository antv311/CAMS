# CAMS Sprint 1 checkpoint

Coordination room: `#cams-s1`. Owner authorization: the owner's message 472.
Final task: `task_9508106a796f45dca8006bc6c7bcf1ab`.

## Integrated work

| Delivery | Task | Earlier main commit |
| --- | --- | --- |
| SQLite client template storage | `task_fc008ce545e448739d5e4ba9f3dc54c3` | `f881f85` |
| Client template HTTP API | `task_bb5dd4bf0d63421385a941c84762458c` | `6677fc7` |
| Python template commands and client | `task_fe0d0c0088b9467eb1a3ebf0aeac306c` | `af70795` |
| Worker provisioning endpoint | `task_8997e9e7fe36461187cbc402281f4cdb` | `7926203` |
| Python worker spawn support | `task_d49e0b898d224660a1fff5bd198a1cf7` | `5a1aae6` |
| Scrollable TUI chat | `task_542b2b61ec4844eeb94a089f274365fa` | `85a8de7` |

This checkpoint adds a default wake limit of **30 runs per hour**, matching
the CLI help and README. Explicit nonnegative limits remain supported,
including `0` for unlimited runs; negative limits are rejected.

## Verification

On the final task branch, using `CARGO_TARGET_DIR=target/wake-limit-test`:

- `cargo test --locked`: **117 passed**, no failures or ignored tests.
- `cargo build --locked`: passed.
- Focused rate-limit test after formatting: **1 passed**.
- With `PYTHONPATH=clients/python` and `AIRCD_BIN` pointing to the newly built
  binary, `python -m unittest discover -s clients/python/tests -p test_templates.py -v`:
  **12 passed**, including real CLI default, explicit-limit and rejection checks.
- With the same environment, `python -m unittest discover -s clients/python/tests -p test_cli_e2e.py -v`:
  **3 passed**. Both suites use isolated test hubs.
- `git diff --check`: passed.
- Formatting comparison against the parent: the 9 existing `wake.rs` and 98
  existing `http.rs` formatting edits remain identical; no new formatting edits.

The broader pytest command could not start because pytest is absent from the
system Python. The two relevant unittest suites above completed without skips.

## Live configuration and rollout

The owner API listed six existing wake configurations. Five limits changed
from 12 to 30; one was already 30. A second API listing verified **6/6 at 30**
and every other configuration field unchanged, apart from update timestamps.
Launcher commands, environment, sessions, enabled state and other limits were
preserved. No agents were deliberately launched for this verification.

The live hub was left running its existing binary. It reads existing limits
from storage, so the six updated limits apply immediately. Restart the hub
with a binary built from this checkpoint to load the new creation default.
The source and merge commits are local; publication is a separate operation.

Hub review closure remains with the project manager or owner. The scroll
feature was already merged into Git while awaiting that administrative step.
