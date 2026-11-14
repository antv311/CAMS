"""User-defined tool plugins for the Bionic agent runner (``airc-bionic``).

A plugin is a Python file in a tools directory (``<workspace>/.aircd/tools/``
by default, plus any ``--tools-dir`` / ``BIONIC_TOOLS_DIR`` entries). Each file
declares its tools in a module-level ``TOOLS`` list (or a single ``TOOL``
dict)::

    def count_words(args, ctx):
        text = ctx.resolve(args["path"]).read_text(encoding="utf-8")
        return {"words": len(text.split())}

    TOOLS = [
        {
            "name": "count_words",
            "description": "Count whitespace-separated words in a workspace file.",
            "parameters": {
                "type": "object",
                "properties": {"path": {"type": "string", "description": "File to count."}},
                "required": ["path"],
                "additionalProperties": False,
            },
            "handler": count_words,
            "mutating": False,
        }
    ]

Definitions are validated strictly when the file loads: unknown keys, bad
names, names that collide with built-in or other plugin tools, and
``parameters`` schemas that use unsupported JSON Schema keywords all fail the
whole load with a :class:`PluginError` naming the file. Arguments the model
sends are validated against ``parameters`` before the handler runs; invalid
calls return an error to the model and the handler is not called.
"""

from __future__ import annotations

import hashlib
import importlib.util
import inspect
import json
import math
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Optional

from aircd.bionic import BionicError, resolve_path

DEFAULT_TOOLS_SUBDIR = Path(".aircd") / "tools"

#: OpenAI function names: letters, digits, ``_`` and ``-``, at most 64 chars.
TOOL_NAME_RE = re.compile(r"^[A-Za-z0-9_-]{1,64}$")

_DEFINITION_KEYS = frozenset({"name", "description", "parameters", "handler", "mutating"})
_REQUIRED_KEYS = ("name", "description", "parameters", "handler")
_JSON_TYPES = ("string", "integer", "number", "boolean", "array", "object", "null")

# JSON Schema keywords accepted in ``parameters``, grouped by the types they apply to.
_COMMON_KEYWORDS = frozenset({"type", "description", "title", "enum", "default"})
_KEYWORDS_BY_TYPE: dict[str, frozenset[str]] = {
    "string": frozenset({"minLength", "maxLength", "pattern"}),
    "integer": frozenset({"minimum", "maximum"}),
    "number": frozenset({"minimum", "maximum"}),
    "boolean": frozenset(),
    "null": frozenset(),
    "array": frozenset({"items", "minItems", "maxItems"}),
    "object": frozenset({"properties", "required", "additionalProperties"}),
}
_MAX_SCHEMA_DEPTH = 16


class PluginError(BionicError):
    """A tool plugin failed to load or declares an invalid tool."""


class ToolArgumentError(ValueError):
    """Tool-call arguments do not match the tool's ``parameters`` schema."""


@dataclass
class ToolContext:
    """Runtime information handed to a plugin handler as its second argument."""

    cwd: str
    workspace_root: str
    command_timeout: float
    max_output_chars: int
    log: Optional[Callable[[str], None]] = None

    def resolve(self, path: str) -> Path:
        """Resolve ``path`` against ``cwd`` inside the workspace jail.

        Raises :class:`aircd.bionic.WorkspaceViolation` (reported to the model
        as a denied call) when the path leaves the workspace root.
        """
        return resolve_path(self.cwd, path, self.workspace_root)


@dataclass
class PluginTool:
    """One validated plugin tool."""

    name: str
    description: str
    parameters: dict[str, Any]
    handler: Callable[[dict[str, Any], ToolContext], Any]
    mutating: bool
    source: str

    def schema(self) -> dict[str, Any]:
        """The OpenAI ``tools`` entry sent to the model."""
        return {
            "type": "function",
            "function": {"name": self.name, "description": self.description, "parameters": self.parameters},
        }


@dataclass
class ToolRegistry:
    """Plugin tools loaded for one run, keyed by name, plus the directories scanned."""

    tools: dict[str, PluginTool] = field(default_factory=dict)
    dirs: list[str] = field(default_factory=list)

    def get(self, name: str) -> Optional[PluginTool]:
        return self.tools.get(name)

    def schemas(self, approval: str = "auto") -> list[dict[str, Any]]:
        """Tool schemas for an approval mode; ``read-only`` keeps only non-mutating tools."""
        return [t.schema() for t in self.tools.values() if approval != "read-only" or not t.mutating]

    def __len__(self) -> int:
        return len(self.tools)


# ---------------------------------------------------------------------------
# Definition validation
# ---------------------------------------------------------------------------


def _check_number(value: Any, where: str, *, integer: bool = False, minimum: Optional[float] = None) -> None:
    ok_type = isinstance(value, int) if integer else isinstance(value, (int, float))
    if isinstance(value, bool) or not ok_type or (isinstance(value, float) and not math.isfinite(value)):
        kind = "an integer" if integer else "a finite number"
        raise PluginError(f"{where} must be {kind}, got {value!r}")
    if minimum is not None and value < minimum:
        raise PluginError(f"{where} must be >= {minimum}, got {value!r}")


def validate_parameters_schema(schema: Any, where: str = "parameters", *, depth: int = 0) -> None:
    """Check that ``schema`` uses only the supported JSON Schema subset.

    Every (sub)schema must be an object with a single ``type`` string; keywords
    must belong to that type (``properties`` only on objects, ``items`` only on
    arrays, ...). Raises :class:`PluginError` describing the first problem.
    """
    if depth > _MAX_SCHEMA_DEPTH:
        raise PluginError(f"{where}: schema nests deeper than {_MAX_SCHEMA_DEPTH} levels")
    if not isinstance(schema, dict):
        raise PluginError(f"{where} must be a JSON Schema object, got {type(schema).__name__}")
    kind = schema.get("type")
    if kind not in _JSON_TYPES:
        raise PluginError(f"{where}.type must be one of {', '.join(_JSON_TYPES)}, got {kind!r}")
    allowed = _COMMON_KEYWORDS | _KEYWORDS_BY_TYPE[kind]
    unknown = sorted(str(k) for k in schema if k not in allowed)
    if unknown:
        raise PluginError(f"{where}: unsupported keyword(s) for type {kind!r}: {', '.join(unknown)}")

    for key in ("description", "title"):
        if key in schema and not isinstance(schema[key], str):
            raise PluginError(f"{where}.{key} must be a string")
    if "enum" in schema:
        enum = schema["enum"]
        if not isinstance(enum, list) or not enum:
            raise PluginError(f"{where}.enum must be a non-empty list")
        for i, item in enumerate(enum):
            if validate_value(item, {"type": kind}):
                raise PluginError(f"{where}.enum[{i}] {item!r} is not of type {kind!r}")
    for key in ("minLength", "maxLength", "minItems", "maxItems"):
        if key in schema:
            _check_number(schema[key], f"{where}.{key}", integer=True, minimum=0)
    for key in ("minimum", "maximum"):
        if key in schema:
            _check_number(schema[key], f"{where}.{key}", integer=kind == "integer")
    for lo, hi in (("minLength", "maxLength"), ("minItems", "maxItems"), ("minimum", "maximum")):
        if lo in schema and hi in schema and schema[lo] > schema[hi]:
            raise PluginError(f"{where}: {lo} {schema[lo]} is greater than {hi} {schema[hi]}")
    if "pattern" in schema:
        if not isinstance(schema["pattern"], str):
            raise PluginError(f"{where}.pattern must be a string")
        try:
            re.compile(schema["pattern"])
        except re.error as err:
            raise PluginError(f"{where}.pattern is not a valid regular expression: {err}") from err

    if kind == "array" and "items" in schema:
        validate_parameters_schema(schema["items"], f"{where}.items", depth=depth + 1)
    if kind == "object":
        props = schema.get("properties", {})
        if not isinstance(props, dict):
            raise PluginError(f"{where}.properties must be an object")
        for prop_name, prop_schema in props.items():
            if not isinstance(prop_name, str) or not prop_name:
                raise PluginError(f"{where}.properties has an empty or non-string key {prop_name!r}")
            validate_parameters_schema(prop_schema, f"{where}.properties.{prop_name}", depth=depth + 1)
        required = schema.get("required", [])
        if not isinstance(required, list) or not all(isinstance(r, str) for r in required):
            raise PluginError(f"{where}.required must be a list of property names")
        if len(set(required)) != len(required):
            raise PluginError(f"{where}.required lists a property more than once")
        missing = [r for r in required if r not in props]
        if missing:
            raise PluginError(f"{where}.required names undeclared properties: {', '.join(missing)}")
        if "additionalProperties" in schema and not isinstance(schema["additionalProperties"], bool):
            raise PluginError(f"{where}.additionalProperties must be true or false")

    if "default" in schema:
        problems = validate_value(schema["default"], schema)
        if problems:
            raise PluginError(f"{where}.default is invalid: {problems[0]}")


def validate_tool_definition(
    definition: Any, source: str, *, reserved: frozenset[str] | set[str] = frozenset()
) -> PluginTool:
    """Validate one ``TOOLS`` entry from ``source`` and return it as a :class:`PluginTool`."""
    if not isinstance(definition, dict):
        raise PluginError(f"{source}: tool definition must be a dict, got {type(definition).__name__}")
    label = definition.get("name") if isinstance(definition.get("name"), str) else "<unnamed>"
    unknown = sorted(str(k) for k in definition if k not in _DEFINITION_KEYS)
    if unknown:
        raise PluginError(f"{source}: tool {label!r} has unknown key(s): {', '.join(unknown)}")
    missing = [k for k in _REQUIRED_KEYS if k not in definition]
    if missing:
        raise PluginError(f"{source}: tool {label!r} is missing required key(s): {', '.join(missing)}")

    name = definition["name"]
    if not isinstance(name, str) or not TOOL_NAME_RE.match(name):
        raise PluginError(f"{source}: tool name {name!r} must match {TOOL_NAME_RE.pattern}")
    if name in reserved:
        raise PluginError(f"{source}: tool name {name!r} is already defined (built-in or another plugin)")
    description = definition["description"]
    if not isinstance(description, str) or not description.strip():
        raise PluginError(f"{source}: tool {name!r} needs a non-empty string description")
    mutating = definition.get("mutating", True)
    if not isinstance(mutating, bool):
        raise PluginError(f"{source}: tool {name!r} 'mutating' must be true or false")

    parameters = definition["parameters"]
    validate_parameters_schema(parameters, f"{source}: tool {name!r} parameters")
    if parameters.get("type") != "object":
        raise PluginError(f"{source}: tool {name!r} parameters must have type 'object'")
    try:
        # Must survive the round trip to the model unchanged (no tuples, sets, NaN, ...).
        if json.loads(json.dumps(parameters, allow_nan=False)) != parameters:
            raise ValueError("schema changes when serialized to JSON")
    except (TypeError, ValueError) as err:
        raise PluginError(f"{source}: tool {name!r} parameters are not plain JSON: {err}") from err

    handler = definition["handler"]
    if not callable(handler):
        raise PluginError(f"{source}: tool {name!r} handler must be callable")
    try:
        inspect.signature(handler).bind(None, None)
    except TypeError as err:
        raise PluginError(f"{source}: tool {name!r} handler must accept (args, ctx): {err}") from err
    except ValueError:
        pass  # builtins without an introspectable signature; trust them

    return PluginTool(
        name=name,
        description=description.strip(),
        parameters=json.loads(json.dumps(parameters)),
        handler=handler,
        mutating=mutating,
        source=source,
    )


# ---------------------------------------------------------------------------
# Argument validation
# ---------------------------------------------------------------------------


def _type_ok(value: Any, kind: str) -> bool:
    if kind == "string":
        return isinstance(value, str)
    if kind == "integer":
        return (isinstance(value, int) and not isinstance(value, bool)) or (
            isinstance(value, float) and value.is_integer()
        )
    if kind == "number":
        return isinstance(value, (int, float)) and not isinstance(value, bool)
    if kind == "boolean":
        return isinstance(value, bool)
    if kind == "array":
        return isinstance(value, list)
    if kind == "object":
        return isinstance(value, dict)
    return value is None


def validate_value(value: Any, schema: dict[str, Any], path: str = "$") -> list[str]:
    """Return human-readable problems with ``value`` against a supported ``schema`` (empty = valid)."""
    kind = schema.get("type")
    if kind is not None and not _type_ok(value, kind):
        return [f"{path} must be of type {kind}, got {type(value).__name__}"]
    problems: list[str] = []
    if "enum" in schema and value not in schema["enum"]:
        problems.append(f"{path} must be one of {json.dumps(schema['enum'])}")
    if isinstance(value, str):
        if "minLength" in schema and len(value) < schema["minLength"]:
            problems.append(f"{path} must be at least {schema['minLength']} characters")
        if "maxLength" in schema and len(value) > schema["maxLength"]:
            problems.append(f"{path} must be at most {schema['maxLength']} characters")
        if "pattern" in schema and not re.search(schema["pattern"], value):
            problems.append(f"{path} must match pattern {schema['pattern']!r}")
    if kind in ("integer", "number"):
        if "minimum" in schema and value < schema["minimum"]:
            problems.append(f"{path} must be >= {schema['minimum']}")
        if "maximum" in schema and value > schema["maximum"]:
            problems.append(f"{path} must be <= {schema['maximum']}")
    if isinstance(value, list):
        if "minItems" in schema and len(value) < schema["minItems"]:
            problems.append(f"{path} must have at least {schema['minItems']} items")
        if "maxItems" in schema and len(value) > schema["maxItems"]:
            problems.append(f"{path} must have at most {schema['maxItems']} items")
        if isinstance(schema.get("items"), dict):
            for i, item in enumerate(value):
                problems.extend(validate_value(item, schema["items"], f"{path}[{i}]"))
    if isinstance(value, dict) and kind == "object":
        props = schema.get("properties", {})
        for req in schema.get("required", []):
            if req not in value:
                problems.append(f"{path}.{req} is required")
        for key, item in value.items():
            if key in props:
                problems.extend(validate_value(item, props[key], f"{path}.{key}"))
            elif schema.get("additionalProperties") is False:
                problems.append(f"{path}.{key} is not an allowed property")
    return problems


def validate_arguments(tool: PluginTool, args: Any) -> None:
    """Raise :class:`ToolArgumentError` unless ``args`` satisfies ``tool.parameters``."""
    problems = validate_value(args, tool.parameters)
    if problems:
        shown = "; ".join(problems[:5]) + (f" (+{len(problems) - 5} more)" if len(problems) > 5 else "")
        raise ToolArgumentError(f"invalid arguments for {tool.name}: {shown}")


# ---------------------------------------------------------------------------
# Discovery and loading
# ---------------------------------------------------------------------------


def discover_tool_files(dirs: list[str]) -> list[Path]:
    """List plugin files (``*.py`` not starting with ``_`` or ``.``) in ``dirs``, sorted per directory."""
    files: list[Path] = []
    seen: set[Path] = set()
    for raw in dirs:
        directory = Path(raw)
        for path in sorted(directory.glob("*.py"), key=lambda p: p.name):
            if path.name.startswith(("_", ".")) or not path.is_file():
                continue
            real = path.resolve()
            if real not in seen:
                seen.add(real)
                files.append(path)
    return files


def _import_plugin(path: Path) -> Any:
    digest = hashlib.sha1(str(path.resolve()).encode("utf-8")).hexdigest()[:12]
    module_name = f"aircd_bionic_tool_{re.sub(r'[^A-Za-z0-9_]', '_', path.stem)}_{digest}"
    spec = importlib.util.spec_from_file_location(module_name, path)
    if spec is None or spec.loader is None:
        raise PluginError(f"{path}: cannot be imported as a Python module")
    module = importlib.util.module_from_spec(spec)
    # Registered (under a name unique to the file path) so decorators such as
    # @dataclass that look the module up by name work; reloading replaces it.
    sys.modules[module_name] = module
    try:
        spec.loader.exec_module(module)
    except BaseException as err:
        sys.modules.pop(module_name, None)
        if isinstance(err, SystemExit):
            raise PluginError(f"{path}: plugin called exit() while loading ({err.code!r})") from err
        if isinstance(err, Exception):
            raise PluginError(f"{path}: failed to import: {type(err).__name__}: {err}") from err
        raise
    return module


def load_plugin_file(path: Path, *, reserved: frozenset[str] | set[str] = frozenset()) -> list[PluginTool]:
    """Import one plugin file and validate every tool it declares."""
    module = _import_plugin(path)
    has_tools, has_tool = hasattr(module, "TOOLS"), hasattr(module, "TOOL")
    if has_tools == has_tool:
        raise PluginError(f"{path}: define exactly one of TOOLS (a list) or TOOL (a dict)")
    definitions = [module.TOOL] if has_tool else module.TOOLS
    if not isinstance(definitions, (list, tuple)) or not definitions:
        raise PluginError(f"{path}: TOOLS must be a non-empty list of tool definitions")
    taken = set(reserved)
    tools: list[PluginTool] = []
    for definition in definitions:
        tool = validate_tool_definition(definition, str(path), reserved=taken)
        taken.add(tool.name)
        tools.append(tool)
    return tools


def load_registry(dirs: list[str], *, builtin_names: frozenset[str] | set[str] = frozenset()) -> ToolRegistry:
    """Load and validate every plugin in ``dirs``; any invalid plugin fails the whole load."""
    registry = ToolRegistry(dirs=[str(d) for d in dirs])
    taken = set(builtin_names)
    for path in discover_tool_files(registry.dirs):
        for tool in load_plugin_file(path, reserved=taken):
            taken.add(tool.name)
            registry.tools[tool.name] = tool
    return registry


# ---------------------------------------------------------------------------
# Execution
# ---------------------------------------------------------------------------


def run_plugin_tool(tool: PluginTool, args: Any, ctx: ToolContext) -> str:
    """Validate ``args`` and run ``tool``'s handler, serializing its result for the model.

    A ``str`` result is returned as-is; anything else is JSON-encoded. Results
    longer than ``ctx.max_output_chars`` are truncated. Handler exceptions
    become ``{"error": ...}``; :class:`~aircd.bionic.WorkspaceViolation` is
    re-raised so the caller reports a denied call.
    """
    from aircd.bionic import WorkspaceViolation

    try:
        validate_arguments(tool, args)
    except ToolArgumentError as err:
        return json.dumps({"error": str(err)})
    if ctx.log:
        ctx.log(f"[tool] {tool.name} (plugin {Path(tool.source).name})")
    try:
        result = tool.handler(args, ctx)
    except WorkspaceViolation:
        raise
    except (Exception, SystemExit) as err:
        return json.dumps({"error": f"{tool.name} failed: {type(err).__name__}: {err}"})
    if isinstance(result, str):
        text = result
    else:
        try:
            text = json.dumps(result)
        except (TypeError, ValueError) as err:
            return json.dumps({"error": f"{tool.name} returned a value that is not JSON-serializable: {err}"})
    limit = ctx.max_output_chars
    if len(text) > limit:
        return text[:limit] + f"\n...[truncated at {limit} chars]"
    return text
