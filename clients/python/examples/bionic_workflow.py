"""Drive a Bionic (OpenAI-compatible) agent from Python with ``aircd.BionicAgent``.

Usage (LM Studio Bionic running on http://localhost:1235/v1):

    python examples/bionic_workflow.py path/to/repo

Set BIONIC_BASE_URL / BIONIC_MODEL to point at another server or model.
"""

from __future__ import annotations

import sys

from aircd import BionicAgent, BionicError


def main() -> int:
    repo = sys.argv[1] if len(sys.argv) > 1 else "."
    try:
        agent = BionicAgent(model="auto", cwd=repo, approval="read-only", keep_session=True)
        print(f"model: {agent.model}", file=sys.stderr)

        # Step 1: stream a survey of the repo, printing tokens live.
        for event in agent.stream("List the top-level files and say what this project is."):
            if event["type"] == "token":
                print(event["text"], end="", flush=True)
            elif event["type"] == "tool_call":
                print(f"\n[tool] {event['name']}", file=sys.stderr)
        print()

        # Step 2: follow up in the same conversation without streaming.
        result = agent.run("In three bullet points, what should a new contributor read first?")
        print(result.content)
        print(f"session_id: {agent.session_id} ({result.turns} turns, {result.tool_calls} tool calls)", file=sys.stderr)
    except BionicError as err:
        print(f"error: {err}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
