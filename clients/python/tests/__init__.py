import os

# Tests run in-process ``run_agent`` calls; when the suite itself runs inside a
# hub-woken agent, these would otherwise bill (or be capped by) that agent's
# real task on the live hub.
for _name in ("AIRCD_WAKE_TASK", "BIONIC_TASK_ID", "BIONIC_TOKEN_BUDGET", "BIONIC_NO_USAGE_REPORT"):
    os.environ.pop(_name, None)
