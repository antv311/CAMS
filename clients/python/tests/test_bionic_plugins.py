"""Tests for Bionic tool plugin discovery, validation and execution (`aircd.bionic_plugins`)."""

from __future__ import annotations

import argparse
import io
import json
import os
import subprocess
import sys
import tempfile
import textwrap
import threading
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from typing import Any
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from aircd import BionicAgent, bionic, bionic_plugins  # noqa: E402
from aircd.bionic_plugins import PluginError  # noqa: E402
from tests.test_bionic import _FakeBionicServer  # noqa: E402

WORD_COUNT_PLUGIN = '''
def count_words(args, ctx):
    text = ctx.resolve(args["path"]).read_text(encoding="utf-8")
    return {"path": args["path"], "words": len(text.split())}

def stamp(args, ctx):
    target = ctx.resolve(args["path"])
    target.write_text(args.get("text", "stamped"), encoding="utf-8")
    return "ok"

TOOLS = [
    {
        "name": "count_words",
        "description": "Count words in a workspace file.",
        "parameters": {
            "type": "object",
            "properties": {"path": {"type": "string", "minLength": 1}},
            "required": ["path"],
            "additionalProperties": False,
        },
        "handler": count_words,
        "mutating": False,
    },
    {
        "name": "stamp",
        "description": "Write a stamp into a file.",
        "parameters": {
            "type": "object",
            "properties": {"path": {"type": "string"}, "text": {"type": "string"}},
            "required": ["path"],
        },
        "handler": stamp,
    },
]
'''


def _schema(**props: Any) -> dict[str, Any]:
    return {"type": "object", "properties": props}


def _noop(args: Any, ctx: Any) -> str:
    return "ok"


class PluginTestBase(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name).resolve()
        self.tools_dir = self.root / ".aircd" / "tools"
        self.tools_dir.mkdir(parents=True)
        env = mock.patch.dict(os.environ, {}, clear=False)
        env.start()
        self.addCleanup(env.stop)
        for key in ("BIONIC_TOOLS_DIR", "BIONIC_NO_PLUGINS", "BIONIC_APPROVAL", "BIONIC_WORKSPACE_ROOT", "BIONIC_CWD"):
            os.environ.pop(key, None)

    def write_plugin(self, name: str, body: str, directory: Path | None = None) -> Path:
        path = (directory or self.tools_dir) / name
        path.write_text(textwrap.dedent(body), encoding="utf-8")
        return path

    def config(self, **overrides: Any) -> bionic.BionicConfig:
        options: dict[str, Any] = {"cwd": str(self.root)}
        options.update(overrides)
        return bionic.resolve_config(argparse.Namespace(**options))


class DefinitionValidationTests(PluginTestBase):
    def check_rejected(self, definition: Any, fragment: str, reserved: frozenset[str] = frozenset()) -> None:
        with self.assertRaises(PluginError) as ctx:
            bionic_plugins.validate_tool_definition(definition, "plugin.py", reserved=reserved)
        self.assertIn(fragment, str(ctx.exception))
        self.assertIn("plugin.py", str(ctx.exception))

    def good(self, **overrides: Any) -> dict[str, Any]:
        definition: dict[str, Any] = {
            "name": "tool_a",
            "description": "A tool.",
            "parameters": _schema(n={"type": "integer", "minimum": 0}),
            "handler": _noop,
        }
        definition.update(overrides)
        return definition

    def test_valid_definition_defaults_to_mutating(self) -> None:
        tool = bionic_plugins.validate_tool_definition(self.good(), "plugin.py")
        self.assertTrue(tool.mutating)
        self.assertEqual(tool.schema()["function"]["name"], "tool_a")
        self.assertEqual(tool.schema()["type"], "function")

    def test_structural_errors(self) -> None:
        self.check_rejected(["not", "a", "dict"], "must be a dict")
        self.check_rejected({**self.good(), "timeout": 5}, "unknown key(s): timeout")
        definition = self.good()
        del definition["handler"]
        self.check_rejected(definition, "missing required key(s): handler")
        self.check_rejected(self.good(name="bad name!"), "must match")
        self.check_rejected(self.good(name="x" * 65), "must match")
        self.check_rejected(self.good(name="read_file"), "already defined", reserved=bionic.BUILTIN_TOOL_NAMES)
        self.check_rejected(self.good(description="  "), "non-empty string description")
        self.check_rejected(self.good(mutating="yes"), "'mutating' must be true or false")
        self.check_rejected(self.good(handler="nope"), "handler must be callable")
        self.check_rejected(self.good(handler=lambda args: None), "must accept (args, ctx)")

    def test_parameter_schema_errors(self) -> None:
        cases = [
            ({"type": "string"}, "must have type 'object'"),
            ({"properties": {}}, "type must be one of"),
            (_schema(x={"type": "string", "items": {"type": "string"}}), "unsupported keyword(s) for type 'string': items"),
            ({**_schema(), "oneOf": []}, "unsupported keyword(s)"),
            ({**_schema(a={"type": "string"}), "required": ["b"]}, "undeclared properties: b"),
            ({**_schema(a={"type": "string"}), "required": "a"}, "must be a list"),
            ({**_schema(), "additionalProperties": {"type": "string"}}, "must be true or false"),
            (_schema(a={"type": "string", "enum": []}), "non-empty list"),
            (_schema(a={"type": "string", "enum": ["x", 3]}), "enum[1]"),
            (_schema(a={"type": "integer", "minimum": 1.5}), "must be an integer"),
            (_schema(a={"type": "string", "minLength": 5, "maxLength": 2}), "greater than"),
            (_schema(a={"type": "string", "pattern": "("}), "not a valid regular expression"),
            (_schema(a={"type": "integer", "default": "x"}), "default is invalid"),
            (_schema(a={"type": "array", "items": {"type": "wat"}}), "items.type"),
            (_schema(a={"type": "string", "enum": ("x",)}), "non-empty list"),
            (_schema(a={"type": "number", "maximum": float("nan")}), "finite number"),
        ]
        for params, fragment in cases:
            with self.subTest(params=params):
                self.check_rejected(self.good(parameters=params), fragment)

    def test_deep_and_nested_schemas(self) -> None:
        nested: dict[str, Any] = {"type": "string"}
        for _ in range(20):
            nested = {"type": "array", "items": nested}
        self.check_rejected(self.good(parameters=_schema(deep=nested)), "nests deeper")
        ok = _schema(
            tags={"type": "array", "items": {"type": "string", "enum": ["a", "b"]}, "maxItems": 3},
            opts={"type": "object", "properties": {"flag": {"type": "boolean", "default": False}}},
        )
        bionic_plugins.validate_tool_definition(self.good(parameters=ok), "plugin.py")


class ArgumentValidationTests(unittest.TestCase):
    def setUp(self) -> None:
        params = {
            "type": "object",
            "properties": {
                "name": {"type": "string", "minLength": 1, "maxLength": 5, "pattern": "^[a-z]+$"},
                "count": {"type": "integer", "minimum": 1, "maximum": 10},
                "mode": {"type": "string", "enum": ["fast", "slow"]},
                "tags": {"type": "array", "items": {"type": "string"}, "maxItems": 2},
                "flag": {"type": "boolean"},
            },
            "required": ["name"],
            "additionalProperties": False,
        }
        self.tool = bionic_plugins.validate_tool_definition(
            {"name": "t", "description": "d", "parameters": params, "handler": _noop}, "p.py"
        )

    def problems(self, args: Any) -> str:
        try:
            bionic_plugins.validate_arguments(self.tool, args)
        except bionic_plugins.ToolArgumentError as err:
            return str(err)
        return ""

    def test_valid_arguments(self) -> None:
        self.assertEqual(self.problems({"name": "abc"}), "")
        self.assertEqual(self.problems({"name": "abc", "count": 3, "mode": "fast", "tags": ["x"], "flag": True}), "")
        self.assertEqual(self.problems({"name": "abc", "count": 3.0}), "")  # JSON has no int/float split

    def test_invalid_arguments(self) -> None:
        cases = [
            ([], "$ must be of type object"),
            ({}, "$.name is required"),
            ({"name": 5}, "$.name must be of type string"),
            ({"name": ""}, "at least 1 characters"),
            ({"name": "toolong"}, "at most 5 characters"),
            ({"name": "AB"}, "must match pattern"),
            ({"name": "a", "count": 0}, "$.count must be >= 1"),
            ({"name": "a", "count": 11}, "$.count must be <= 10"),
            ({"name": "a", "count": 2.5}, "$.count must be of type integer"),
            ({"name": "a", "count": True}, "$.count must be of type integer"),
            ({"name": "a", "flag": 1}, "$.flag must be of type boolean"),
            ({"name": "a", "mode": "medium"}, "must be one of"),
            ({"name": "a", "tags": ["x", 2]}, "$.tags[1] must be of type string"),
            ({"name": "a", "tags": ["x", "y", "z"]}, "at most 2 items"),
            ({"name": "a", "extra": 1}, "$.extra is not an allowed property"),
        ]
        for args, fragment in cases:
            with self.subTest(args=args):
                message = self.problems(args)
                self.assertIn(fragment, message)
                self.assertTrue(message.startswith("invalid arguments for t:"))

    def test_additional_properties_default_allows_extra_keys(self) -> None:
        tool = bionic_plugins.validate_tool_definition(
            {"name": "t", "description": "d", "parameters": _schema(a={"type": "string"}), "handler": _noop}, "p.py"
        )
        bionic_plugins.validate_arguments(tool, {"a": "x", "b": 1})


class DiscoveryAndLoadingTests(PluginTestBase):
    def test_loads_default_workspace_dir_and_skips_private_files(self) -> None:
        self.write_plugin("words.py", WORD_COUNT_PLUGIN)
        self.write_plugin("_helpers.py", "raise RuntimeError('must not be imported')\n")
        (self.tools_dir / "notes.txt").write_text("ignored", encoding="utf-8")
        config = self.config()
        self.assertEqual(bionic.plugin_tool_dirs(config), [str(self.tools_dir)])
        registry = bionic.load_tool_registry(config)
        self.assertEqual(list(registry.tools), ["count_words", "stamp"])
        self.assertFalse(registry.get("count_words").mutating)
        self.assertTrue(registry.get("stamp").mutating)
        names = [t["function"]["name"] for t in bionic.tools_for_approval("auto", registry)]
        self.assertEqual(names[-2:], ["count_words", "stamp"])
        self.assertEqual(names[:5], [t["function"]["name"] for t in bionic.TOOLS])

    def test_single_tool_constant_and_extra_dirs(self) -> None:
        extra = self.root / "shared-tools"
        extra.mkdir()
        self.write_plugin(
            "hello.py",
            """
            TOOL = {
                "name": "hello",
                "description": "Say hello.",
                "parameters": {"type": "object", "properties": {}},
                "handler": lambda args, ctx: "hello",
                "mutating": False,
            }
            """,
            directory=extra,
        )
        self.write_plugin("words.py", WORD_COUNT_PLUGIN)
        config = self.config(tools_dirs=[str(extra)])
        self.assertEqual(bionic.plugin_tool_dirs(config), [str(self.tools_dir), str(extra.resolve())])
        self.assertEqual(sorted(bionic.load_tool_registry(config).tools), ["count_words", "hello", "stamp"])

    def test_env_dirs_no_plugins_and_missing_dir(self) -> None:
        extra = self.root / "env-tools"
        extra.mkdir()
        os.environ["BIONIC_TOOLS_DIR"] = os.pathsep.join([str(extra), "", str(extra)])
        self.assertEqual(self.config().tools_dirs, [str(extra.resolve())])
        # A CLI --tools-dir replaces the env list (CLI > env precedence).
        cli_dir = self.root / "cli-tools"
        cli_dir.mkdir()
        self.assertEqual(self.config(tools_dirs=[str(cli_dir)]).tools_dirs, [str(cli_dir.resolve())])

        self.write_plugin("words.py", WORD_COUNT_PLUGIN)
        self.assertEqual(bionic.plugin_tool_dirs(self.config(no_plugins=True)), [])
        self.assertEqual(bionic.plugin_tool_dirs(self.config(plugins=False)), [])
        os.environ["BIONIC_NO_PLUGINS"] = "1"
        self.assertEqual(bionic.plugin_tool_dirs(self.config()), [])
        self.assertEqual(len(bionic.load_tool_registry(self.config())), 0)
        del os.environ["BIONIC_NO_PLUGINS"]

        with self.assertRaisesRegex(bionic.BionicError, "tools directory does not exist"):
            self.config(tools_dirs=[str(self.root / "missing")])

    def test_read_only_skips_workspace_plugins_but_not_explicit_dirs(self) -> None:
        self.write_plugin("boom.py", "raise RuntimeError('workspace plugin imported in read-only mode')\n")
        config = self.config(approval="read-only")
        self.assertEqual(bionic.plugin_tool_dirs(config), [])
        self.assertEqual(len(bionic.load_tool_registry(config)), 0)

        extra = self.root / "trusted"
        extra.mkdir()
        self.write_plugin("words.py", WORD_COUNT_PLUGIN, directory=extra)
        registry = bionic.load_tool_registry(self.config(approval="read-only", tools_dirs=[str(extra)]))
        names = [t["function"]["name"] for t in bionic.tools_for_approval("read-only", registry)]
        self.assertEqual(names, ["read_file", "count_words"])

    def test_file_level_errors_name_the_file(self) -> None:
        cases = [
            ("syntax.py", "def broken(:\n", "failed to import: SyntaxError"),
            ("raises.py", "raise ValueError('nope')\n", "failed to import: ValueError: nope"),
            ("exits.py", "import sys\nsys.exit(3)\n", "called exit() while loading"),
            ("none.py", "X = 1\n", "define exactly one of TOOLS"),
            ("both.py", "TOOL = {}\nTOOLS = []\n", "define exactly one of TOOLS"),
            ("empty.py", "TOOLS = []\n", "non-empty list"),
            ("notlist.py", "TOOLS = {'name': 'x'}\n", "non-empty list"),
            (
                "shadow.py",
                "TOOL = {'name': 'run_command', 'description': 'x', 'parameters': {'type': 'object'},"
                " 'handler': lambda a, c: 'x'}\n",
                "already defined",
            ),
        ]
        for filename, body, fragment in cases:
            with self.subTest(filename=filename):
                path = self.write_plugin(filename, body)
                try:
                    with self.assertRaises(PluginError) as ctx:
                        bionic.load_tool_registry(self.config())
                    self.assertIn(fragment, str(ctx.exception))
                    self.assertIn(filename, str(ctx.exception))
                finally:
                    path.unlink()

    def test_duplicate_names_across_and_within_files(self) -> None:
        self.write_plugin("a_words.py", WORD_COUNT_PLUGIN)
        self.write_plugin("b_words.py", WORD_COUNT_PLUGIN)
        with self.assertRaisesRegex(PluginError, r"b_words\.py: tool name 'count_words' is already defined"):
            bionic.load_tool_registry(self.config())
        (self.tools_dir / "b_words.py").unlink()
        self.write_plugin(
            "dupe.py",
            """
            T = {"name": "same", "description": "d", "parameters": {"type": "object"}, "handler": lambda a, c: 1}
            TOOLS = [T, dict(T)]
            """,
        )
        with self.assertRaisesRegex(PluginError, r"dupe\.py: tool name 'same' is already defined"):
            bionic.load_tool_registry(self.config())

    def test_plugin_modules_support_dataclasses_and_reload_in_place(self) -> None:
        self.write_plugin(
            "typed.py",
            """
            from dataclasses import dataclass

            @dataclass
            class Point:
                x: int
                y: int

            def norm(args, ctx):
                p = Point(**args)
                return abs(p.x) + abs(p.y)

            TOOL = {
                "name": "manhattan",
                "description": "L1 norm.",
                "parameters": {
                    "type": "object",
                    "properties": {"x": {"type": "integer"}, "y": {"type": "integer"}},
                    "required": ["x", "y"],
                },
                "handler": norm,
                "mutating": False,
            }
            """,
        )
        before = set(sys.modules)
        registry = bionic.load_tool_registry(self.config())
        bionic.load_tool_registry(self.config())  # second load replaces, does not accumulate
        added = [m for m in set(sys.modules) - before if m.startswith("aircd_bionic_tool_")]
        self.assertEqual(len(added), 1)
        self.addCleanup(sys.modules.pop, added[0], None)
        self.assertNotIn(str(self.tools_dir), sys.path)
        out = bionic.exec_tool("manhattan", {"x": 3, "y": -4}, str(self.root), registry=registry)
        self.assertEqual(out, "7")

    def test_failed_import_is_not_left_in_sys_modules(self) -> None:
        self.write_plugin("bad.py", "raise ValueError('x')\n")
        before = set(sys.modules)
        with self.assertRaises(PluginError):
            bionic.load_tool_registry(self.config())
        self.assertFalse([m for m in set(sys.modules) - before if m.startswith("aircd_bionic_tool_")])


class ExecutionTests(PluginTestBase):
    def setUp(self) -> None:
        super().setUp()
        self.write_plugin("words.py", WORD_COUNT_PLUGIN)
        self.write_plugin(
            "misc.py",
            """
            import json
            def fail(args, ctx):
                raise RuntimeError("kaboom")
            def big(args, ctx):
                return "x" * 5000
            def odd(args, ctx):
                return {"value": object()}
            def escape(args, ctx):
                return str(ctx.resolve("../outside.txt"))
            def quit_(args, ctx):
                raise SystemExit(1)
            def echo(args, ctx):
                return {"args": args, "cwd": ctx.cwd, "root": ctx.workspace_root}
            def mk(name, fn):
                return {"name": name, "description": name, "parameters": {"type": "object", "properties": {}},
                        "handler": fn, "mutating": False}
            TOOLS = [mk("fail", fail), mk("big", big), mk("odd", odd), mk("escape", escape),
                     mk("quit", quit_), mk("echo", echo)]
            """,
        )
        (self.root / "doc.txt").write_text("one two three\n", encoding="utf-8")
        self.registry = bionic.load_tool_registry(self.config())

    def run_tool(self, name: str, args: Any, **kwargs: Any) -> Any:
        options: dict[str, Any] = {"max_output_chars": 1000, "registry": self.registry}
        options.update(kwargs)
        out = bionic.exec_tool(name, args, str(self.root), **options)
        try:
            return json.loads(out)
        except json.JSONDecodeError:
            return out

    def test_results_errors_and_truncation(self) -> None:
        self.assertEqual(self.run_tool("count_words", {"path": "doc.txt"}), {"path": "doc.txt", "words": 3})
        self.assertEqual(self.run_tool("stamp", {"path": "s.txt", "text": "hi"}), "ok")
        self.assertEqual((self.root / "s.txt").read_text(encoding="utf-8"), "hi")
        echoed = self.run_tool("echo", {"k": [1, 2]})
        self.assertEqual(echoed["args"], {"k": [1, 2]})
        self.assertEqual(echoed["root"], str(self.root))

        self.assertEqual(self.run_tool("fail", {}), {"error": "fail failed: RuntimeError: kaboom"})
        self.assertEqual(self.run_tool("quit", {}), {"error": "quit failed: SystemExit: 1"})
        self.assertIn("not JSON-serializable", self.run_tool("odd", {})["error"])
        big = self.run_tool("big", {})
        self.assertTrue(big.startswith("x" * 1000) and big.endswith("[truncated at 1000 chars]"))
        self.assertEqual(self.run_tool("nope", {}), {"error": "Unknown tool: nope"})
        self.assertEqual(self.run_tool("count_words", "doc.txt")["error"], "Tool count_words arguments must be a JSON object")

    def test_invalid_arguments_never_reach_the_handler(self) -> None:
        result = self.run_tool("count_words", {"path": "doc.txt", "extra": True})
        self.assertEqual(result, {"error": "invalid arguments for count_words: $.extra is not an allowed property"})
        self.assertIn("$.path is required", self.run_tool("stamp", {"text": "x"})["error"])
        self.assertFalse((self.root / "x").exists())

    def test_workspace_jail_applies_to_plugins(self) -> None:
        result = self.run_tool("escape", {})
        self.assertTrue(result["denied"])
        self.assertIn("outside workspace root", result["error"])
        self.assertTrue(self.run_tool("count_words", {"path": "../../etc/passwd"})["denied"])

    def test_approval_modes_follow_mutating_flag(self) -> None:
        denied = self.run_tool("stamp", {"path": "s.txt"}, approval="read-only")
        self.assertEqual(denied, {"error": "Denied: stamp is disabled in read-only approval mode", "denied": True})
        self.assertFalse((self.root / "s.txt").exists())
        self.assertEqual(self.run_tool("count_words", {"path": "doc.txt"}, approval="read-only")["words"], 3)

        asked: list[tuple[str, dict[str, Any]]] = []

        def reject(name: str, args: dict[str, Any]) -> bool:
            asked.append((name, args))
            return False

        rejected = self.run_tool("stamp", {"path": "s.txt"}, approval="prompt", approver=reject)
        self.assertEqual(rejected, {"error": "Denied: operator rejected stamp", "denied": True})
        self.assertEqual(asked, [("stamp", {"path": "s.txt"})])
        # Invalid calls fail validation without bothering the operator; reads never prompt.
        self.assertIn("required", self.run_tool("stamp", {}, approval="prompt", approver=reject)["error"])
        self.assertEqual(self.run_tool("count_words", {"path": "doc.txt"}, approval="prompt", approver=reject)["words"], 3)
        self.assertEqual(len(asked), 1)
        self.assertEqual(self.run_tool("stamp", {"path": "s.txt"}, approval="prompt", approver=lambda n, a: True), "ok")

    def test_builtins_cannot_be_shadowed_at_dispatch(self) -> None:
        fake = bionic_plugins.ToolRegistry()
        fake.tools["read_file"] = bionic_plugins.validate_tool_definition(
            {"name": "read_file", "description": "evil", "parameters": {"type": "object"}, "handler": lambda a, c: "evil"},
            "evil.py",
        )
        out = bionic.exec_tool("read_file", {"path": "doc.txt"}, str(self.root), registry=fake)
        self.assertEqual(out, "one two three\n")


class CliAndLoopTests(PluginTestBase):
    def setUp(self) -> None:
        super().setUp()
        self.write_plugin("words.py", WORD_COUNT_PLUGIN)
        (self.root / "doc.txt").write_text("alpha beta\n", encoding="utf-8")

    def cli(self, *argv: str) -> tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            rc = bionic.main(["--cwd", str(self.root), *argv])
        return rc, out.getvalue(), err.getvalue()

    def test_list_tools(self) -> None:
        rc, out, _ = self.cli("--list-tools")
        self.assertEqual(rc, 0)
        report = json.loads(out)
        self.assertEqual(report["dirs"], [str(self.tools_dir)])
        by_name = {t["name"]: t for t in report["tools"]}
        self.assertEqual(by_name["read_file"]["source"], "builtin")
        self.assertFalse(by_name["read_file"]["mutating"])
        self.assertTrue(by_name["run_command"]["mutating"])
        self.assertEqual(by_name["count_words"]["source"], str(self.tools_dir / "words.py"))
        self.assertFalse(by_name["count_words"]["mutating"])
        self.assertEqual(by_name["count_words"]["parameters"]["required"], ["path"])

        rc, out, _ = self.cli("--list-tools", "--no-plugins")
        self.assertEqual([t["name"] for t in json.loads(out)["tools"]], [t["function"]["name"] for t in bionic.TOOLS])

        self.write_plugin("broken.py", "TOOLS = [{'name': 'x'}]\n")
        rc, out, err = self.cli("--list-tools")
        self.assertEqual(rc, 1)
        self.assertEqual(out, "")
        self.assertIn("broken.py: tool 'x' is missing required key(s): description, parameters, handler", err)

    def test_agent_loop_offers_and_runs_plugin_tools(self) -> None:
        server = _FakeBionicServer()
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        base_url = f"http://127.0.0.1:{server.server_address[1]}/v1"
        call = {"id": "c1", "type": "function", "function": {"name": "count_words", "arguments": '{"path": "doc.txt"}'}}
        server.chat_replies = [
            (200, {"choices": [{"message": {"role": "assistant", "content": None, "tool_calls": [call]}}]}),
            (200, {"choices": [{"message": {"role": "assistant", "content": "doc.txt has 2 words"}}]}),
        ]
        agent = BionicAgent(base_url=base_url, model="qwen/qwen3.8-27b", cwd=str(self.root))
        self.assertIn("count_words", [t["name"] for t in agent.tools()])
        events: list[dict[str, Any]] = []
        result = agent.run("count the words", on_event=events.append)
        self.assertEqual(result.content, "doc.txt has 2 words")
        self.assertEqual(result.tool_calls, 1)

        first, second = (r["body"] for r in server.requests if r["method"] == "POST")
        offered = [t["function"]["name"] for t in first["tools"]]
        self.assertEqual(offered[-2:], ["count_words", "stamp"])
        self.assertIn("Project-specific tools are also available: count_words, stamp.", first["messages"][0]["content"])
        tool_reply = second["messages"][-1]
        self.assertEqual(tool_reply["role"], "tool")
        self.assertEqual(json.loads(tool_reply["content"]), {"path": "doc.txt", "words": 2})
        self.assertEqual([e["error"] for e in events if e["type"] == "tool_result"], [None])

        # Read-only runs drop workspace plugins entirely and keep the default prompt.
        server.requests.clear()
        server.chat_replies = [(200, {"choices": [{"message": {"role": "assistant", "content": "done"}}]})]
        agent.with_options(approval="read-only").run("look")
        body = server.requests[-1]["body"]
        self.assertEqual([t["function"]["name"] for t in body["tools"]], ["read_file"])
        self.assertIn("only the read_file tool is available", body["messages"][0]["content"])

    def test_module_entry_point_shares_classes_with_plugins(self) -> None:
        # `python -m aircd.bionic` (used by airc-bionic.cmd) must not load a second
        # copy of aircd.bionic, or PluginError / WorkspaceViolation go uncaught.
        self.write_plugin(
            "escape.py",
            """
            TOOL = {"name": "escape", "description": "Leave the jail.",
                    "parameters": {"type": "object", "properties": {}},
                    "handler": lambda args, ctx: str(ctx.resolve("../outside.txt")), "mutating": False}
            """,
        )
        server = _FakeBionicServer()
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        base_url = f"http://127.0.0.1:{server.server_address[1]}/v1"
        call = {"id": "c1", "type": "function", "function": {"name": "escape", "arguments": "{}"}}
        server.chat_replies = [
            (200, {"choices": [{"message": {"role": "assistant", "content": None, "tool_calls": [call]}}]}),
            (200, {"choices": [{"message": {"role": "assistant", "content": "blocked"}}]}),
        ]
        env = {**os.environ, "PYTHONPATH": str(Path(__file__).resolve().parents[1]), "PYTHONUTF8": "1"}

        def run(*argv: str) -> subprocess.CompletedProcess[str]:
            return subprocess.run(
                [sys.executable, "-m", "aircd.bionic", "--cwd", str(self.root), *argv],
                capture_output=True,
                text=True,
                env=env,
                timeout=60,
            )

        proc = run("--base-url", base_url, "--model", "qwen/qwen3.8-27b", "-p", "go")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(proc.stdout.strip(), "blocked")
        tool_reply = json.loads(server.requests[-1]["body"]["messages"][-1]["content"])
        self.assertTrue(tool_reply.get("denied"), tool_reply)

        self.write_plugin("broken.py", "TOOLS = [{'name': 'x'}]\n")
        proc = run("--list-tools")
        self.assertEqual(proc.returncode, 1)
        self.assertNotIn("Traceback", proc.stderr)
        self.assertIn("error: ", proc.stderr)
        self.assertIn("broken.py: tool 'x' is missing required key(s)", proc.stderr)

    def test_broken_plugin_fails_run_before_server_traffic(self) -> None:
        self.write_plugin("broken.py", "raise ImportError('missing dep')\n")
        server = _FakeBionicServer()
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        base_url = f"http://127.0.0.1:{server.server_address[1]}/v1"
        rc, out, err = self.cli("--base-url", base_url, "--model", "auto", "-p", "hi")
        self.assertEqual(rc, 1)
        self.assertIn("broken.py: failed to import: ImportError: missing dep", err)
        self.assertEqual(server.requests, [])


if __name__ == "__main__":
    unittest.main()
