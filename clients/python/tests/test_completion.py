"""Tests for `airc completion bash|zsh|powershell`."""

from __future__ import annotations

import io
import os
import shutil
import subprocess
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

from aircd import cli
from aircd.completion import SHELLS, build_tree, render


def _bash() -> str | None:
    path = shutil.which("bash")
    # On Windows, System32\bash.exe is the WSL launcher, which cannot see temp paths.
    if path and "system32" in path.lower():
        return None
    return path


class CompletionTreeTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tree = build_tree(cli.build_parser())

    def test_every_subcommand_is_in_the_tree(self) -> None:
        def walk(parser, path):
            for action in parser._actions:
                if hasattr(action, "_name_parser_map"):
                    for name, child in action.choices.items():
                        self.assertIn(name, self.tree.subs[path], path)
                        walk(child, f"{path} {name}")

        walk(cli.build_parser(), "airc")
        self.assertIn("completion", self.tree.subs["airc"])
        self.assertIn("status", self.tree.subs["airc task"])

    def test_options_and_values(self) -> None:
        self.assertIn("--profile", self.tree.opts["airc"])
        self.assertIn("--kind", self.tree.opts["airc task attach"])
        self.assertEqual(self.tree.vals["airc task attach|--kind"], ["log", "evidence"])
        self.assertEqual(self.tree.vals["airc welcome|--format"], ["json", "md", "html"])
        # Free-text option: takes a value, no fixed choices.
        self.assertEqual(self.tree.vals["airc task status|--note"], [])
        # Flags take no value.
        self.assertNotIn("airc|--json", self.tree.vals)
        # Suppressed options stay hidden.
        self.assertNotIn("--hook", self.tree.opts["airc git rebase-siblings"])

    def test_positional_values_are_index_aware(self) -> None:
        self.assertEqual(self.tree.pos["airc completion@0"], list(SHELLS))
        self.assertEqual(self.tree.pos["airc task peer-review@1"], ["approve", "reject"])
        self.assertIn("review", self.tree.pos["airc task status@1"])
        self.assertNotIn("airc task status@0", self.tree.pos)


class CompletionCommandTest(unittest.TestCase):
    def test_each_shell_renders(self) -> None:
        for shell in SHELLS:
            buf = io.StringIO()
            with redirect_stdout(buf):
                self.assertEqual(cli.main(["completion", shell]), 0)
            script = buf.getvalue()
            self.assertNotIn("\r", script)
            self.assertIn("airc task status@1", script)
        self.assertIn("complete -o default -F _airc airc", render("bash", cli.build_parser()))
        self.assertTrue(render("zsh", cli.build_parser()).startswith("#compdef airc"))
        self.assertIn("Register-ArgumentCompleter -Native", render("powershell", cli.build_parser()))

    def test_unknown_shell_is_rejected(self) -> None:
        with self.assertRaises(ValueError):
            render("fish", cli.build_parser())
        with redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            cli.build_parser().parse_args(["completion", "fish"])


class LiveShellTest(unittest.TestCase):
    CASES = [
        (["airc", "ta"], "task"),
        (["airc", "task", "st"], "status"),
        (["airc", "task", "status", "abc", ""], "in_progress testing review changes_requested merged cancelled"),
        (["airc", "--profile", "me", "task", "attach", "x", "--kind", ""], "log evidence"),
        (["airc", "task", "attach", "x", "--note", "lo", "--k"], "--kind"),
        (["airc", "welcome", "--format", ""], "json md html"),
        (["airc", "--j"], "--json"),
        (["airc", "completion", "p"], "powershell"),
        (["airc", "task", "peer-review", "id", ""], "approve reject"),
    ]

    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def test_bash(self) -> None:
        bash = _bash()
        if not bash:
            self.skipTest("bash not available")
        script = self.dir / "airc.bash"
        script.write_bytes(render("bash", cli.build_parser()).encode())
        lines = [f"source '{script.as_posix()}'"]
        for words, _ in self.CASES:
            quoted = " ".join(f"'{w}'" for w in words)
            lines.append(
                f"COMP_WORDS=({quoted}); COMP_CWORD={len(words) - 1}; _airc; echo \"${{COMPREPLY[*]}}\""
            )
        runner = self.dir / "run.sh"
        runner.write_bytes(("\n".join(lines) + "\n").encode())
        out = subprocess.run([bash, runner.as_posix()], capture_output=True, text=True, timeout=60)
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertEqual(out.stdout.splitlines(), [want for _, want in self.CASES])

    def test_powershell(self) -> None:
        ps = shutil.which("pwsh") or (shutil.which("powershell") if os.name == "nt" else None)
        if not ps:
            self.skipTest("PowerShell not available")
        script = self.dir / "airc.ps1"
        script.write_text(render("powershell", cli.build_parser()), encoding="utf-8")
        lines = [f". '{script}'"]
        for words, _ in self.CASES:
            line = " ".join(words)
            lines.append(
                f"$r = TabExpansion2 -inputScript '{line}' -cursorColumn {len(line)}; "
                "(@($r.CompletionMatches | ForEach-Object CompletionText) -join ' ')"
            )
        runner = self.dir / "run.ps1"
        runner.write_text("\n".join(lines) + "\n", encoding="utf-8")
        out = subprocess.run(
            [ps, "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", str(runner)],
            capture_output=True, text=True, timeout=120,
        )
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertEqual(out.stdout.splitlines(), [want for _, want in self.CASES])


if __name__ == "__main__":
    unittest.main()
