"""Heuristic capture detectors, ported from Alexandria's pi companion
(``contrib/pi/extensions/alexandria/src/detectors/``). Regexes and gates are kept
1:1 with the TypeScript originals where the semantics transfer; the parts that are
pi-specific are replaced with documented Hermes equivalents (see TRANSIENT_PATTERNS).

A ``DetectedMemory`` is a plain dict ``{"content": str, "tags": [str, ...]}``.
"""

from __future__ import annotations

import re
import time
from typing import Any, Optional

DetectedMemory = dict[str, Any]

# ---------------------------------------------------------------------------
# Session-scoped dedup buffer (types.ts)
# ---------------------------------------------------------------------------


class SessionDedupBuffer:
    """Tracks what has been stored this session (heuristics + agent tool calls) so the
    LLM extraction pass does not re-extract known facts."""

    def __init__(self) -> None:
        self._normalized: set[str] = set()
        self._raw: list[str] = []

    @staticmethod
    def _normalize(s: str) -> str:
        return re.sub(r"\s+", " ", s).strip().lower()

    def add_heuristic_store(self, content: str) -> bool:
        """Record a heuristic-detected store. Returns False if already stored."""
        norm = self._normalize(content)
        if norm in self._normalized:
            return False
        self._normalized.add(norm)
        self._raw.append(content)
        return True

    def add_tool_store(self, content: str) -> None:
        """Record an agent-initiated store_memory call."""
        self._raw.append(content)

    def get_all_stored_contents(self) -> list[str]:
        return list(self._raw)


# ---------------------------------------------------------------------------
# Correction detector (correction.ts)
# ---------------------------------------------------------------------------

CORRECTION_PATTERNS = [
    re.compile(r"\bno[,.]?\s+(?:use|it\s+should\s+be|it'?s)\s+(.+)", re.I),
    re.compile(r"\bthat'?s\s+(?:wrong|incorrect|not\s+right)[,.]?\s*(.+)", re.I),
    re.compile(r"\bactually[,.]?\s+(.+)", re.I),
    re.compile(r"\bi\s+meant\s+(.+)", re.I),
    re.compile(r"\bnot\s+.{2,30}[,;]\s*(?:use|it'?s)\s+(.+)", re.I),
    re.compile(r"\bwrong\s*[—–-]\s*(.+)", re.I),
    re.compile(r"\bincorrect\s*[—–-]\s*(.+)", re.I),
]


def detect_correction(prompt: str, buffer: SessionDedupBuffer) -> Optional[DetectedMemory]:
    """Scan a user prompt for correction-shaped language. Fires when unambiguous;
    ambiguous cases are left to the LLM extraction pass."""
    trimmed = prompt.strip()
    if len(trimmed) < 8 or len(trimmed) > 500:
        return None
    for pattern in CORRECTION_PATTERNS:
        match = pattern.search(trimmed)
        if match and match.group(1):
            corrected_fact = re.sub(r"[.!]+$", "", match.group(1)).strip()
            # One word ("no, it's completed") is a state report, not a fact.
            if len(corrected_fact) < 5 or not re.search(r"\s", corrected_fact):
                continue
            content = f"User correction: {corrected_fact}"
            if not buffer.add_heuristic_store(content):
                return None
            return {"content": content, "tags": ["correction", "auto-detected", "source:regex"]}
    return None


# ---------------------------------------------------------------------------
# Preference detector (preference.ts)
# ---------------------------------------------------------------------------

# Every pattern captures from its trigger onward: an inverted preference is worse than
# a missing one. Negative triggers first so "don't always rebase" is stored whole.
PREFERENCE_PATTERNS = [
    re.compile(r"(?:^|[.!?;:,]\s*|\bplease\s+)((?:don'?t|do\s+not)\s+.+)", re.I),
    re.compile(r"\b(don'?t\s+ever\s+.+)", re.I),
    re.compile(r"\b(never\s+.+)", re.I),
    re.compile(r"\b(always\s+.+)", re.I),
    re.compile(r"\b(i\s+prefer\s+.+)", re.I),
    re.compile(r"\b(i\s+like\s+.+?\s+better\b.*)", re.I),
    re.compile(r"\b(default\s+to\s+.+)", re.I),
    re.compile(r"\b(make\s+sure\s+to\s+.+)", re.I),
    re.compile(r"\b(use\s+.+?\s+instead\s+of\s+.+)", re.I),
    re.compile(r"\bfrom\s+now\s+on[,.]?\s+(.+)", re.I),
    re.compile(r"\bgoing\s+forward[,.]?\s+(.+)", re.I),
]

# A negation earlier in the same clause governs the match, and no capture keeps it, so
# such a match is never stored.
NEGATION = re.compile(r"\b(?:not|no|never|cannot|without)\b|n'?t\b", re.I)


def detect_preference(prompt: str, buffer: SessionDedupBuffer) -> Optional[DetectedMemory]:
    """Scan a user prompt for forward-looking preference/convention statements."""
    trimmed = prompt.strip()
    if len(trimmed) < 8 or len(trimmed) > 500:
        return None
    for pattern in PREFERENCE_PATTERNS:
        match = pattern.search(trimmed)
        if match and match.group(1):
            clause_so_far = re.split(r"[.!?;:,]", trimmed[: match.start(1)])[-1]
            if NEGATION.search(clause_so_far):
                continue
            statement = re.sub(r"[.!]+$", "", match.group(1)).strip()
            if len(statement) < 5:
                continue
            content = f"User preference: {statement}"
            if not buffer.add_heuristic_store(content):
                return None
            return {"content": content, "tags": ["preference", "auto-detected", "source:regex"]}
    return None


# ---------------------------------------------------------------------------
# Tool-store tracker (tool-tracker.ts)
# ---------------------------------------------------------------------------

STORE_TOOLS = {"store_memory", "update_memory"}

# Hermes names provider tools ``alexandria_*`` and natively-mounted MCP tools
# ``mcp_<server>_<tool>``; pi names ``mcp__<server>__<tool>``; adapter 2.x used
# ``<server>_<tool>``. All reduce to the bare name below, matched exactly — a suffix
# test would accept another server's ``auto_store_memory`` (pi's comment, kept).


def bare_tool_name(tool_name: str) -> str:
    name = tool_name
    if name.startswith("mcp_") and not name.startswith("mcp__"):
        name = name[len("mcp_"):]  # Hermes native MCP prefix: mcp_alexandria_store_memory
    sep = name.rfind("__")
    if sep >= 0:
        return name[sep + 2:]
    return re.sub(r"^[A-Za-z0-9-]+_", "", name)


def is_store_tool_call(tool_name: str) -> bool:
    return tool_name in STORE_TOOLS or bare_tool_name(tool_name) in STORE_TOOLS


def track_tool_store(tool_name: str, content: Any, is_error: bool, buffer: SessionDedupBuffer) -> bool:
    """If this tool result is a successful store_memory/update_memory call, record its
    content in the dedup buffer. Returns True if recorded."""
    if not is_store_tool_call(tool_name) or is_error:
        return False
    if not isinstance(content, str) or not content:
        return False
    buffer.add_tool_store(content)
    return True


# ---------------------------------------------------------------------------
# Error-resolution tracker (error-tracker.ts)
# ---------------------------------------------------------------------------

MAX_ERRORS = 5
MAX_TEXT_LENGTH = 200
MIN_ERROR_LENGTH = 30

# Errors that are not durable lessons. The pi original also carried pi-lens markers
# (`RETRYABLE —`, `PARTIAL APPLY —`); those are pi-lens's wording and are deliberately
# NOT copied — the Hermes port re-derives its transient list against Hermes' own
# tool-error vocabulary. Extend from real Hermes noise, not imagination.
TRANSIENT_PATTERNS = [
    re.compile(r"fatal: not a git repository"),
]

# Call-shape rejections: noise about how the agent addressed some other server's tool.
# NOT applied to alexandria's own tools — a rejected store_memory argument is exactly
# the "error -> what worked" lesson this module exists to write.
CALL_SHAPE_PATTERNS = [
    re.compile(r"Validation failed for tool"),
    re.compile(r"failed to deserialize parameters", re.I),
    re.compile(r"Failed to call tool: Missing"),
]

# A path lookup that found nothing is the routine answer to a wrong guess. Matched at
# the HEAD only: the same words inside another tool's failure are durable detail.
PATH_LOOKUP_FAILED = re.compile(r"^\s*(Error: )?Path not found:")

# Error text must contain at least one signal word to be worth tracking.
ERROR_SIGNAL_PATTERN = re.compile(
    r"\b(error|fail(ed|ure)?|exception|panic|denied|not found|timeout|refused|abort|crash|fatal|"
    r"invalid|cannot|couldn'?t|unable|unexpected|broken|missing|violation)\b",
    re.I,
)


def _references_alexandria(error_text: str) -> bool:
    return bool(re.search(r"mcp__alexandria__|mcp_alexandria_|alexandria_[a-z_]+", error_text))


def _is_transient(tool_name: str, error_text: str) -> bool:
    if PATH_LOOKUP_FAILED.search(error_text):
        return True
    if any(rx.search(error_text) for rx in TRANSIENT_PATTERNS):
        return True
    if "alexandria" in tool_name or _references_alexandria(error_text):
        return False
    return any(rx.search(error_text) for rx in CALL_SHAPE_PATTERNS)


def _normalize_error_text(text: str) -> str:
    # Whitespace collapse, and nothing else (see the pi comment: every payload-eliding
    # rule tried damaged real rows; coarser keys are tolerable, wrong ones are not).
    return re.sub(r"\s+", " ", text).strip()


class _ErrorRecord:
    __slots__ = ("tool_name", "error_text", "timestamp", "key")

    def __init__(self, tool_name: str, error_text: str, key: str):
        self.tool_name = tool_name
        self.error_text = error_text
        self.timestamp = time.time()
        self.key = key


class ErrorTracker:
    """Pairs tool errors with subsequent successes of the same tool; flush() at
    session end emits the paired resolutions. One memory per error class per
    session — unpaired classes release their claim at flush."""

    def __init__(self) -> None:
        self._errors: list[_ErrorRecord] = []
        self._resolutions: list[tuple[_ErrorRecord, str]] = []
        self._seen: set[str] = set()
        self._pending_release: list[str] = []

    def record_error(self, tool_name: str, text: str) -> None:
        raw = text[:MAX_TEXT_LENGTH].strip()
        # Length, signal and transience are all judged on the untouched text.
        if len(raw) < MIN_ERROR_LENGTH:
            return
        if not ERROR_SIGNAL_PATTERN.search(raw):
            return
        if _is_transient(tool_name, raw):
            return
        error_text = _normalize_error_text(raw)
        key = f"{tool_name}\u0000{error_text}"
        if key in self._seen:
            return
        self._seen.add(key)
        # Ring buffer — the evicted class's claim is released at the next flush, not
        # immediately (re-recording within a turn must not displace a still-pairable
        # neighbour's claim).
        if len(self._errors) >= MAX_ERRORS:
            evicted = self._errors.pop(0)
            self._pending_release.append(evicted.key)
        self._errors.append(_ErrorRecord(tool_name, error_text, key))

    def record_success(self, tool_name: str, text: str) -> None:
        # Not normalized: the resolution is the half that says what fixed it.
        success_text = text[:MAX_TEXT_LENGTH].strip()
        if not success_text:
            return
        for idx, err in enumerate(self._errors):
            if err.tool_name == tool_name:
                self._errors.pop(idx)
                self._resolutions.append((err, success_text))
                return

    def flush(self) -> list[DetectedMemory]:
        memories = [
            {
                "content": f"Error with {err.tool_name}: {err.error_text}\nResolution: {success}",
                "tags": ["error-resolution", "auto-detected", err.tool_name],
            }
            for err, success in self._resolutions
        ]
        # Unpaired errors taught us nothing: release their class claims so the same
        # error recurring next turn followed by a real fix is still storable. Classes
        # that DID produce a memory keep their claim — that is what stops the flood.
        for err in self._errors:
            self._seen.discard(err.key)
        for key in self._pending_release:
            self._seen.discard(key)
        self._pending_release = []
        self._resolutions = []
        self._errors = []
        return memories
