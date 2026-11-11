"""End-of-session LLM extraction pass, ported from the pi companion
(``src/extraction.ts`` + ``src/extraction-parse.ts``).

The LLM call goes through Hermes' plugin LLM facade (``agent.plugin_llm.PluginLlm``)
so it uses the host's active model and credentials. Everything fails open: a missing,
timed-out, or misbehaving model costs the extraction, never the session teardown.
"""

from __future__ import annotations

import json
import logging
import re
from typing import Any, Optional

logger = logging.getLogger(__name__)

EXTRACTION_PROMPT = """You are a memory extraction system. Given a conversation between a user and an AI coding assistant, extract durable facts worth remembering across sessions.

Extract:
- User preferences and conventions (tooling choices, style rules, workflow habits)
- Architectural/design decisions AND their rationale
- Bug root causes once resolved (not symptoms)
- Non-obvious gotchas, footguns, or platform/library quirks
- Corrections the user gave about something the assistant got wrong

Do NOT extract:
- Ephemeral task details (file paths being edited, current branch name, etc.)
- Things already in the "already stored" list below
- Common knowledge or well-documented behavior
- Incomplete work or open questions

Each extracted memory must be a standalone statement that makes sense without this conversation. No "as discussed above", no pronouns without antecedents.

Respond with JSON only:
{
  "memories": [
    {"content": "standalone statement", "tags": ["relevant", "tags"]},
    ...
  ]
}

If nothing is worth extracting, respond with: {"memories": []}"""

EXTRACTION_JSON_SCHEMA: dict[str, Any] = {
    "type": "object",
    "properties": {
        "memories": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "content": {"type": "string"},
                    "tags": {"type": "array", "items": {"type": "string"}},
                },
                "required": ["content"],
            },
        }
    },
    "required": ["memories"],
}

# Cap serialized conversation at ~16k tokens (~64k chars), keeping the tail.
MAX_SERIALIZED_CHARS = 64_000
MIN_SERIALIZED_CHARS = 100


def _extract_text(content: Any) -> str:
    """Plain text from an OpenAI-style message content (string or content blocks)."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = [b.get("text", "") for b in content
                 if isinstance(b, dict) and b.get("type") == "text"]
        return "\n".join(p for p in parts if p)
    return ""


def serialize_messages(messages: list[dict[str, Any]]) -> str:
    """Serialize an OpenAI-style message list for the extraction prompt. Keeps
    user/assistant text only; tool calls and tool results are dropped."""
    lines: list[str] = []
    turn = 0
    for msg in messages:
        if not isinstance(msg, dict):
            continue
        role = msg.get("role")
        if role == "user":
            text = _extract_text(msg.get("content"))
            if text:
                turn += 1
                lines.append(f"[Turn {turn} - User]: {text}")
        elif role == "assistant":
            text = _extract_text(msg.get("content"))
            if text:
                lines.append(f"[Turn {turn} - Assistant]: {text}")
    return "\n\n".join(lines)


def build_prompt(serialized: str, already_stored: list[str]) -> str:
    stored_block = "\n".join(f"- {c}" for c in already_stored) if already_stored else "(nothing stored yet this session)"
    return (
        f"{EXTRACTION_PROMPT}\n\n"
        f"Already stored this session (do not duplicate):\n<already_stored>\n{stored_block}\n</already_stored>\n\n"
        f"Conversation:\n<conversation>\n{serialized}\n</conversation>"
    )


def _normalize_memories(parsed: Any) -> list[dict[str, Any]]:
    """Shape-check a parsed ``{"memories": [...]}`` payload; bad shape yields []."""
    if not isinstance(parsed, dict) or not isinstance(parsed.get("memories"), list):
        return []
    out: list[dict[str, Any]] = []
    for m in parsed["memories"]:
        if isinstance(m, dict) and isinstance(m.get("content"), str) and m["content"]:
            tags = [t for t in m.get("tags", []) if isinstance(t, str)] if isinstance(m.get("tags"), list) else ["extracted"]
            out.append({"content": m["content"], "tags": tags or ["extracted"]})
    return out


def parse_extraction_response(response_text: str) -> list[dict[str, Any]]:
    """Parse model output into memories. Tolerates markdown fences; anything
    unparseable or wrongly shaped yields [] (fail open)."""
    if not response_text:
        return []
    json_text = re.sub(r"^```(?:json)?\s*\n?", "", response_text.strip(), flags=re.M)
    json_text = re.sub(r"\n?```\s*$", "", json_text.strip(), flags=re.M).strip()
    try:
        parsed = json.loads(json_text)
    except (json.JSONDecodeError, TypeError):
        return []
    return _normalize_memories(parsed)


def run_extraction(
    messages: list[dict[str, Any]],
    already_stored: list[str],
    *,
    llm: Any = None,
    timeout: float = 60.0,
) -> list[dict[str, Any]]:
    """Run the extraction pass. ``llm`` is a PluginLlm-compatible object (or None to
    skip). Returns a list of ``{"content", "tags"}`` dicts; never raises."""
    serialized = serialize_messages(messages)
    if len(serialized) < MIN_SERIALIZED_CHARS:
        return []
    if len(serialized) > MAX_SERIALIZED_CHARS:
        serialized = serialized[-MAX_SERIALIZED_CHARS:]
    prompt = build_prompt(serialized, already_stored)

    if llm is None:
        try:
            from agent.plugin_llm import PluginLlm
            llm = PluginLlm(plugin_id="alexandria")
        except Exception as exc:  # noqa: BLE001
            logger.info("alexandria extraction skipped (no llm): %s", exc)
            return []

    # Structured lane first; plain-chat lane as fallback.
    try:
        result = llm.complete_structured(
            instructions="Extract durable memories from the conversation as JSON per the schema.",
            input=[{"type": "text", "text": prompt}],
            json_schema=EXTRACTION_JSON_SCHEMA,
            purpose="alexandria.extraction",
            max_tokens=2000,
            timeout=timeout,
        )
        parsed = getattr(result, "parsed", None)
        if isinstance(parsed, dict) and isinstance(parsed.get("memories"), list):
            return _normalize_memories(parsed)
        text = getattr(result, "text", "") or ""
        if text:
            return parse_extraction_response(text)
    except Exception as exc:  # noqa: BLE001 - fail open by design
        logger.info("alexandria structured extraction failed, trying plain lane: %s", exc)

    try:
        result = llm.complete(
            messages=[{"role": "user", "content": prompt}],
            purpose="alexandria.extraction",
            max_tokens=2000,
            timeout=timeout,
        )
        return parse_extraction_response(getattr(result, "text", "") or "")
    except Exception as exc:  # noqa: BLE001
        logger.info("alexandria extraction skipped: %s", exc)
        return []
