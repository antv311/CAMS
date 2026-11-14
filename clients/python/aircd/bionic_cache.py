"""Prompt-prefix caching hints and KV-cache pre-warming for Bionic.

Local OpenAI-compatible servers (LM Studio, llama.cpp, vLLM, Ollama) keep the
KV cache of the previous request and reuse it when the next prompt starts with
the same tokens. A Bionic run resends the same system prompt and tool schemas
every turn, so those turns only pay for the new tokens as long as the prefix
stays byte-identical and the server is told to keep it. This module:

* adds per-server cache hints to each chat request (:func:`apply_cache_hints`):
  ``cache_prompt`` for llama.cpp-style servers, ``prompt_cache_key`` for
  OpenAI-style routing, ``cache_control`` breakpoints for Anthropic-style
  gateways, and LM Studio's ``ttl`` so an idle model (and its cache) stays loaded;
* derives a stable cache key from the prompt prefix (:func:`cache_key_for`), so
  every run in the same workspace with the same tools shares one cache entry;
* reads how many prompt tokens the server served from cache
  (:func:`cached_tokens`), from ``usage.prompt_tokens_details.cached_tokens``
  (OpenAI, vLLM), ``usage.cache_read_input_tokens`` (Anthropic-style) or
  llama.cpp's ``timings.cache_n``;
* pre-fills the cache with the system prompt and tools before the first real
  prompt (:func:`prewarm`), so that prompt only pays for its own tokens.

Hints never change the stored conversation: :func:`apply_cache_hints` returns a
new payload and leaves the ``messages`` list it was given untouched.
"""

from __future__ import annotations

import copy
import hashlib
import ipaddress
import json
import time
import urllib.parse
from typing import Any, Callable, Optional

#: ``--prompt-cache`` values. ``auto`` picks hints from the base URL.
PROMPT_CACHE_MODES = ("auto", "off", "llamacpp", "openai", "anthropic")

#: Hosts that get OpenAI's ``prompt_cache_key`` in ``auto`` mode. Other remote
#: servers get no extra fields, since strict servers reject unknown ones.
_OPENAI_HOSTS = ("api.openai.com",)

#: Content of the throwaway user turn :func:`prewarm` sends after the system
#: prompt. Chat templates need a user turn; only the prefix before it is reused.
PREWARM_USER_MESSAGE = "Ready?"


def is_local_base_url(base_url: str) -> bool:
    """True for loopback, private-network, link-local and ``.local`` / ``localhost`` hosts."""
    host = (urllib.parse.urlsplit(base_url).hostname or "").strip("[]").lower()
    if not host:
        return False
    if host == "localhost" or host.endswith((".localhost", ".local", ".lan", ".internal")):
        return True
    try:
        addr = ipaddress.ip_address(host)
    except ValueError:
        return False
    return addr.is_loopback or addr.is_private or addr.is_link_local


def resolve_mode(mode: str, base_url: str) -> str:
    """Turn ``auto`` into the concrete hint set for ``base_url``: ``local``, ``openai`` or ``off``."""
    mode = (mode or "auto").strip().lower()
    if mode != "auto":
        return mode
    if is_local_base_url(base_url):
        return "local"
    host = (urllib.parse.urlsplit(base_url).hostname or "").lower()
    if host in _OPENAI_HOSTS:
        return "openai"
    return "off"


def _canonical(value: Any) -> str:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def prefix_fingerprint(system_prompt: str, tool_schemas: list[dict[str, Any]]) -> str:
    """Short hash of the part of every request that should never change within a session."""
    digest = hashlib.sha256()
    digest.update(system_prompt.encode("utf-8"))
    digest.update(b"\0")
    digest.update(_canonical(tool_schemas).encode("utf-8"))
    return digest.hexdigest()[:16]


def cache_key_for(model: str, system_prompt: str, tool_schemas: list[dict[str, Any]]) -> str:
    """``prompt_cache_key`` shared by every request with this model, system prompt and tools."""
    digest = hashlib.sha256()
    digest.update(model.encode("utf-8"))
    digest.update(b"\0")
    digest.update(prefix_fingerprint(system_prompt, tool_schemas).encode("ascii"))
    return "bionic-" + digest.hexdigest()[:24]


def _with_cache_control(message: dict[str, Any]) -> dict[str, Any]:
    """Copy of ``message`` whose last text part carries an ephemeral ``cache_control`` breakpoint."""
    content = message.get("content")
    if isinstance(content, str):
        if not content:
            return message
        parts: list[Any] = [{"type": "text", "text": content}]
    elif isinstance(content, list) and content:
        parts = copy.deepcopy(content)
    else:
        return message
    for part in reversed(parts):
        if isinstance(part, dict) and part.get("type") == "text":
            part["cache_control"] = {"type": "ephemeral"}
            break
    else:
        return message
    return {**message, "content": parts}


def apply_cache_hints(
    payload: dict[str, Any],
    mode: str,
    *,
    base_url: str,
    cache_key: str = "",
    ttl: Optional[int] = None,
    slot: Optional[int] = None,
) -> dict[str, Any]:
    """Return ``payload`` with the prompt-cache hints for ``mode`` added.

    * ``off`` - unchanged (the same object is returned).
    * ``local`` (what ``auto`` means for a local server) - ``cache_prompt`` plus
      ``prompt_cache_key``; local servers ignore the fields they do not know.
    * ``llamacpp`` - ``cache_prompt`` (and ``id_slot`` when ``slot`` is set).
    * ``openai`` - ``prompt_cache_key``.
    * ``anthropic`` - ``cache_control`` breakpoints on the system message and on
      the newest user/tool message, so both the fixed prefix and the
      conversation so far are cached.

    ``ttl`` (seconds, LM Studio) is added in every mode but ``off`` and keeps a
    just-in-time loaded model, and with it the KV cache, resident between turns.
    """
    mode = resolve_mode(mode, base_url)
    if mode == "off":
        return payload
    out = dict(payload)
    if mode in ("local", "llamacpp"):
        out["cache_prompt"] = True
        if slot is not None:
            out["id_slot"] = int(slot)
    if mode in ("local", "openai") and cache_key:
        out["prompt_cache_key"] = cache_key
    if mode == "anthropic":
        messages = list(out.get("messages") or [])
        if messages and messages[0].get("role") == "system":
            messages[0] = _with_cache_control(messages[0])
        for idx in range(len(messages) - 1, 0, -1):
            if messages[idx].get("role") in ("user", "tool"):
                messages[idx] = _with_cache_control(messages[idx])
                break
        out["messages"] = messages
    if ttl is not None:
        out["ttl"] = int(ttl)
    return out


def _int(value: Any) -> Optional[int]:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return int(value)


def cached_tokens(usage: Any, timings: Any = None) -> Optional[int]:
    """Prompt tokens the server reused from its cache, or ``None`` when it does not say."""
    if isinstance(usage, dict):
        details = usage.get("prompt_tokens_details")
        if isinstance(details, dict):
            found = _int(details.get("cached_tokens"))
            if found is not None:
                return found
        for key in ("cached_tokens", "cache_read_input_tokens"):
            found = _int(usage.get(key))
            if found is not None:
                return found
    if isinstance(timings, dict):
        found = _int(timings.get("cache_n"))
        if found is not None:
            return found
    return None


def prewarm(
    config: Any,
    *,
    model: Optional[str] = None,
    log: Optional[Callable[[str], None]] = None,
) -> dict[str, Any]:
    """Fill the server's prompt cache with the system prompt and tool schemas of ``config``.

    Sends the same prefix :func:`aircd.bionic.run_agent` would, followed by a
    short user turn, with a one-token completion. Returns a report with ``ok``,
    ``model``, ``cache_key``, ``fingerprint``, ``mode``, ``seconds``,
    ``prompt_tokens`` and ``cached_tokens`` (``None`` when the server does not
    report it); ``ok`` is False with an ``error`` when the request failed.
    """
    from aircd import bionic

    registry = bionic.load_tool_registry(config)
    resolved = model or bionic.resolve_model(config)
    tool_schemas = bionic.tools_for_approval(config.approval, registry)
    system_prompt = bionic.system_prompt_for(config, tool_schemas)
    key = cache_key_for(resolved, system_prompt, tool_schemas)
    mode = resolve_mode(config.prompt_cache, config.base_url)
    report: dict[str, Any] = {
        "ok": False,
        "model": resolved,
        "mode": mode,
        "cache_key": key,
        "fingerprint": prefix_fingerprint(system_prompt, tool_schemas),
    }
    payload: dict[str, Any] = {
        "model": resolved,
        "messages": [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": PREWARM_USER_MESSAGE},
        ],
        "tools": tool_schemas,
        "tool_choice": "auto",
        "max_tokens": 1,
        "temperature": 0,
    }
    payload = apply_cache_hints(
        payload,
        config.prompt_cache,
        base_url=config.base_url,
        cache_key=key,
        ttl=config.cache_ttl,
        slot=config.cache_slot,
    )
    url = bionic.normalize_base_url(config.base_url) + "/chat/completions"
    started = time.monotonic()
    try:
        resp = bionic._http_json(url, method="POST", payload=payload, api_key=config.api_key, timeout=config.timeout)
    except bionic.BionicError as err:
        report["seconds"] = round(time.monotonic() - started, 3)
        report["error"] = str(err)
        if log:
            log(f"[cache] pre-warm failed: {err}")
        return report
    report["seconds"] = round(time.monotonic() - started, 3)
    usage = resp.get("usage") if isinstance(resp.get("usage"), dict) else {}
    report["ok"] = True
    report["prompt_tokens"] = _int(usage.get("prompt_tokens"))
    report["cached_tokens"] = cached_tokens(usage, resp.get("timings"))
    if log:
        log(
            f"[cache] pre-warmed {report['prompt_tokens'] or '?'} prompt tokens for {resolved} "
            f"in {report['seconds']}s (key {key}, mode {mode})"
        )
    return report
