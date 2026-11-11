"""Alexandria memory provider for Hermes Agent.

Semantic long-term memory via a running Alexandria MCP server (StreamableHTTP).
Recall is injected automatically before each turn; capture runs the ported pi-companion
heuristics (corrections, preferences, error→resolution pairs) during the session and an
LLM extraction pass at session end, sharing one session-scoped dedup buffer.

Config lives in ``memory.alexandria`` (config.yaml):

    memory:
      provider: alexandria
      alexandria:
        url: "http://127.0.0.1:3000/mcp"
        recall_limit: 5
        min_similarity: 0.45
        auto_store: true      # heuristic capture during sessions
        extract: true         # LLM extraction pass at session end
        store_session: true   # group memories under the Hermes session id
"""

from __future__ import annotations

import json
import logging
import queue
import threading
import time
from typing import Any, Callable, Optional

from agent.memory_provider import (
    MemoryProvider,
    RecallStatus,
    is_trivial_prompt,
    spawn_context_thread,
)

from .client import AlexandriaClient, AlexandriaError, AlexandriaToolError
from .detectors import (
    ErrorTracker,
    SessionDedupBuffer,
    detect_correction,
    detect_preference,
    track_tool_store,
)
from .extraction import run_extraction

logger = logging.getLogger(__name__)

DEFAULTS: dict[str, Any] = {
    "url": "http://127.0.0.1:3000/mcp",
    "timeout": 10.0,
    "recall_limit": 5,
    "min_similarity": 0.45,
    "auto_store": True,
    "extract": True,
    "store_session": True,
    "cache_ttl": 60.0,
}

# agent_context values that must not trigger automatic writes (cron runs and
# delegate_task children pollute the corpus with derived context).
_NON_PRIMARY_CONTEXTS = {"cron", "subagent", "flush"}

_TOOL_PREFIX = "alexandria_"


def _load_plugin_config() -> dict[str, Any]:
    try:
        from hermes_cli.config import load_config
        memory_config = load_config().get("memory", {})
    except Exception:
        return {}
    block = memory_config.get("alexandria", {}) if isinstance(memory_config, dict) else {}
    return dict(block) if isinstance(block, dict) else {}


def _extract_text(content: Any) -> str:
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "\n".join(
            b.get("text", "") for b in content if isinstance(b, dict) and b.get("type") == "text"
        )
    return ""


def _classify_tool_result(content: str) -> tuple[bool, str]:
    """(is_error, error_text) for a tool-result message body. Hermes provider tools
    return JSON strings; an ``{"error": ...}`` payload is the failure shape."""
    stripped = content.strip()
    if not stripped:
        return False, ""
    try:
        data = json.loads(stripped)
    except json.JSONDecodeError:
        return stripped.lower().startswith("error"), stripped
    if isinstance(data, dict) and data.get("error"):
        return True, str(data["error"])
    return False, stripped


def _iter_tool_events(messages: list[dict[str, Any]]):
    """Yield ``(tool_name, args, result_text, is_error, error_text)`` for each tool
    result in an OpenAI-style message slice."""
    calls: dict[str, tuple[str, dict[str, Any]]] = {}
    for msg in messages:
        if not isinstance(msg, dict) or msg.get("role") != "assistant":
            continue
        for tc in msg.get("tool_calls") or []:
            if not isinstance(tc, dict):
                continue
            tc_id = str(tc.get("id") or "")
            if not tc_id:
                continue
            fn = tc.get("function") or {}
            try:
                args = json.loads(fn.get("arguments") or "{}")
            except (json.JSONDecodeError, TypeError):
                args = {}
            calls[tc_id] = (str(fn.get("name", "")), args if isinstance(args, dict) else {})
    for msg in messages:
        if not isinstance(msg, dict) or msg.get("role") != "tool":
            continue
        name, args = calls.get(str(msg.get("tool_call_id") or ""), ("unknown", {}))
        text = _extract_text(msg.get("content"))
        is_error, error_text = _classify_tool_result(text)
        yield name, args, text, is_error, error_text


class _SessionState:
    __slots__ = ("buffer", "errors", "seen_msgs")

    def __init__(self) -> None:
        self.buffer = SessionDedupBuffer()
        self.errors = ErrorTracker()
        self.seen_msgs = 0


class AlexandriaMemoryProvider(MemoryProvider):
    """MemoryProvider backed by an Alexandria MCP server. All state is instance-bound
    (multiplexing contract): one instance per profile, bound at initialize()."""

    def __init__(
        self,
        *,
        config: Optional[dict[str, Any]] = None,
        client_factory: Optional[Callable[[str, float], Any]] = None,
        llm: Any = None,
    ):
        self._config_override = config
        self._client_factory = client_factory or (lambda url, timeout: AlexandriaClient(url, timeout=timeout))
        self._llm = llm  # PluginLlm-compatible; None -> constructed lazily in extraction

        self._cfg: dict[str, Any] = dict(DEFAULTS)
        self._session_id = ""
        self._agent_context = "primary"
        self._client: Any = None
        self._client_lock = threading.Lock()
        self._states: dict[str, _SessionState] = {}
        self._states_lock = threading.Lock()
        self._recall_cache: dict[str, tuple[float, str, int]] = {}
        self._last_recall_count: Optional[int] = None
        self._jobs: queue.Queue = queue.Queue()
        self._worker: Optional[threading.Thread] = None
        self._shutdown = False

    # -- identity / availability ---------------------------------------------

    @property
    def name(self) -> str:
        return "alexandria"

    def is_available(self) -> bool:
        """Config-only check (no network per contract). The URL always resolves via
        defaults, so activation is gated by ``memory.provider`` alone."""
        return True

    def unavailable_reason(self) -> str:
        return ""

    # -- lifecycle -------------------------------------------------------------

    def initialize(self, session_id: str, **kwargs) -> None:
        home = kwargs.get("hermes_home")
        self._bound_home = str(home) if home else ""
        self._session_id = session_id
        self._agent_context = str(kwargs.get("agent_context") or "primary")
        cfg = dict(DEFAULTS)
        cfg.update(self._config_override if self._config_override is not None else _load_plugin_config())
        self._cfg = cfg
        if not self._shutdown and self._worker is None:
            self._worker = spawn_context_thread(self._worker_loop, name="alexandria-sync")
            self._worker.start()

    def _writes_allowed(self) -> bool:
        return self._agent_context not in _NON_PRIMARY_CONTEXTS

    def _state(self, session_id: str) -> _SessionState:
        key = session_id or self._session_id
        with self._states_lock:
            st = self._states.get(key)
            if st is None:
                st = self._states[key] = _SessionState()
            return st

    def _get_client(self):
        with self._client_lock:
            if self._client is None:
                self._client = self._client_factory(self._cfg["url"], float(self._cfg["timeout"]))
            return self._client

    def _worker_loop(self) -> None:
        while True:
            job = self._jobs.get()
            if job is None:
                return
            try:
                job()
            except Exception as exc:  # noqa: BLE001 - background job failures are logged, never fatal
                logger.warning("alexandria background job failed: %s", exc)

    def _enqueue(self, job: Callable[[], None]) -> None:
        if self._worker is not None and not self._shutdown:
            self._jobs.put(job)

    def shutdown(self) -> None:
        self._shutdown = True
        self._jobs.put(None)
        if self._worker is not None:
            self._worker.join(timeout=5)
        with self._client_lock:
            if self._client is not None:
                try:
                    self._client.close()
                except Exception:  # noqa: BLE001
                    pass
                self._client = None

    # -- system prompt + recall --------------------------------------------------

    def system_prompt_block(self) -> str:
        return (
            "# Alexandria memory\n"
            "Active (semantic long-term memory). Relevant memories are auto-injected each turn. "
            "Use alexandria_store_memory proactively for durable facts the user states or corrects "
            "(preferences, conventions, gotchas, root causes); alexandria_search_memories / "
            "alexandria_recall to look things up beyond the injected context; "
            "alexandria_set_reminder / alexandria_list_reminders for follow-ups."
        )

    def prefetch(self, query: str, *, session_id: str = "") -> str:
        if is_trivial_prompt(query):
            self._last_recall_count = None
            return ""
        now = time.time()
        key = session_id or self._session_id
        cached = self._recall_cache.get(key)
        if cached and now - cached[0] < float(self._cfg["cache_ttl"]):
            self._last_recall_count = cached[2]
            return cached[1]
        try:
            text, count = self._recall(query, session_id)
        except Exception as exc:  # noqa: BLE001 - recall failure must never break the turn
            logger.info("alexandria prefetch failed: %s", exc)
            text, count = (cached[1], cached[2]) if cached else ("", 0)
        self._recall_cache[key] = (now, text, count)
        self._last_recall_count = count
        return text

    def queue_prefetch(self, query: str, *, session_id: str = "") -> None:
        if is_trivial_prompt(query):
            return

        def refresh() -> None:
            try:
                text, count = self._recall(query, session_id)
            except Exception:  # noqa: BLE001
                return
            self._recall_cache[session_id or self._session_id] = (time.time(), text, count)

        self._enqueue(refresh)

    def recall_status(self) -> Optional[RecallStatus]:
        if self._last_recall_count is None:
            return None
        return RecallStatus(provider_label="alexandria", count=self._last_recall_count)

    def _recall(self, query: str, session_id: str) -> tuple[str, int]:
        """Live retrieve + reminder delivery. Returns (injection text, memory count)."""
        client = self._get_client()
        args: dict[str, Any] = {
            "query": query,
            "limit": int(self._cfg["recall_limit"]),
            "min_similarity": float(self._cfg["min_similarity"]),
        }
        sid = session_id or self._session_id
        if self._cfg["store_session"] and sid:
            args["session_id"] = sid
        payload = client.call_tool("retrieve_memories", args)
        lines: list[str] = []
        count = 0
        results = payload.get("results") if isinstance(payload, dict) else payload
        for item in results or []:
            if isinstance(item, dict):
                content = str(item.get("content", ""))
                score = item.get("score", item.get("similarity"))
            else:
                content, score = str(item), None
            if not content:
                continue
            count += 1
            prefix = f"({score:.2f}) " if isinstance(score, (int, float)) else ""
            lines.append(f"- {prefix}{content}")
        due = payload.get("due_reminders") if isinstance(payload, dict) else None
        try:
            delivered = client.call_tool("check_reminders", {})
            if isinstance(delivered, dict):
                due = (delivered.get("reminders") or delivered.get("delivered") or due or [])
            if due:
                lines.append("")
                lines.append("Due reminders:")
                for r in due:
                    lines.append(f"- {r.get('content') if isinstance(r, dict) else r}")
        except (AlexandriaError, AlexandriaToolError) as exc:
            logger.debug("alexandria check_reminders skipped: %s", exc)
        if not lines:
            return "", 0
        body = "\n".join(lines)
        return f"## Alexandria memory (semantic recall)\n{body}", count

    # -- per-turn capture ----------------------------------------------------------

    def sync_turn(
        self,
        user_content: str,
        assistant_content: str,
        *,
        session_id: str = "",
        messages: Optional[list[dict[str, Any]]] = None,
        turn_author: Optional[dict[str, Any]] = None,
    ) -> None:
        # turn_author is accepted for ABC compatibility; capture is content-based.
        snapshot = list(messages) if messages else []
        self._enqueue(lambda: self._handle_turn(session_id, user_content, snapshot))

    def _handle_turn(self, session_id: str, user_text: str, messages: list[dict[str, Any]]) -> None:
        st = self._state(session_id)
        if self._cfg["auto_store"] and self._writes_allowed():
            for detected in (detect_correction(user_text, st.buffer), detect_preference(user_text, st.buffer)):
                if detected:
                    self._store(detected, session_id)
        new_messages = messages[st.seen_msgs:]
        st.seen_msgs = max(st.seen_msgs, len(messages))
        for name, args, text, is_error, error_text in _iter_tool_events(new_messages):
            if is_error:
                st.errors.record_error(name, error_text)
                continue
            if track_tool_store(name, args.get("content"), False, st.buffer):
                continue
            st.errors.record_success(name, text)

    def _store(self, memory: dict[str, Any], session_id: str, extra_tags: Optional[list[str]] = None) -> bool:
        tags = list(memory.get("tags") or [])
        if extra_tags:
            tags.extend(extra_tags)
        args: dict[str, Any] = {"content": memory["content"], "tags": tags}
        sid = session_id or self._session_id
        if self._cfg["store_session"] and sid:
            args["session_id"] = sid
        try:
            self._get_client().call_tool("store_memory", args)
            return True
        except (AlexandriaError, AlexandriaToolError) as exc:
            logger.info("alexandria store failed (%s): %s", memory["content"][:60], exc)
            return False

    # -- session boundaries -----------------------------------------------------------

    def on_session_switch(self, new_session_id: str, *, parent_session_id: str = "", reset: bool = False, rewound: bool = False, **kwargs) -> None:
        self._session_id = new_session_id
        if reset:
            with self._states_lock:
                self._states.pop(new_session_id, None)
            self._recall_cache.pop(new_session_id, None)

    def on_session_end(self, messages: list[dict[str, Any]]) -> None:
        sid = self._session_id
        st = self._state(sid)
        if not self._writes_allowed():
            return
        try:
            for memory in st.errors.flush():
                self._store(memory, sid)
        except Exception as exc:  # noqa: BLE001
            logger.info("alexandria error-flush failed: %s", exc)
        if self._cfg["extract"]:
            memories = run_extraction(
                messages,
                st.buffer.get_all_stored_contents(),
                llm=self._llm,
                timeout=float(self._cfg["timeout"]) * 3,
            )
            for memory in memories:
                self._store(memory, sid, extra_tags=["extracted", "source:llm"])
            if memories and sid:
                try:
                    self._get_client().call_tool(
                        "finalize_session",
                        {"session_id": sid, "summary": memories[0]["content"][:500], "tags": ["hermes"]},
                    )
                except (AlexandriaError, AlexandriaToolError) as exc:
                    logger.debug("alexandria finalize_session skipped: %s", exc)
        with self._states_lock:
            self._states.pop(sid, None)
        self._recall_cache.pop(sid, None)

    def on_memory_write(self, action: str, target: str, content: str, metadata: Optional[dict[str, Any]] = None) -> None:
        if action == "remove" or not self._writes_allowed():
            return
        self._store(
            {"content": f"Built-in memory ({action} {target}): {content}", "tags": ["builtin-mirror", f"mirror:{target}"]},
            "",
        )

    # -- tools ---------------------------------------------------------------------------

    def get_tool_schemas(self) -> list[dict[str, Any]]:
        return [
            {
                "name": f"{_TOOL_PREFIX}store_memory",
                "description": (
                    "Store a durable fact in long-term memory (call proactively for preferences, "
                    "conventions, corrections, root causes, and non-obvious gotchas worth keeping "
                    "across sessions)."
                ),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "content": {"type": "string", "description": "Standalone statement, understandable without this conversation"},
                        "tags": {"type": "array", "items": {"type": "string"}, "description": "Optional tags"},
                    },
                    "required": ["content"],
                },
            },
            {
                "name": f"{_TOOL_PREFIX}search_memories",
                "description": "Semantic search over long-term memory. Use when the auto-injected recall is not enough.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"},
                        "limit": {"type": "integer", "description": "Max results (default from config)"},
                    },
                    "required": ["query"],
                },
            },
            {
                "name": f"{_TOOL_PREFIX}recall",
                "description": "Progressive two-phase recall: broad cluster scan then focused narrowing. Use for 'what do you know about X' questions.",
                "parameters": {
                    "type": "object",
                    "properties": {"query": {"type": "string"}},
                    "required": ["query"],
                },
            },
            {
                "name": f"{_TOOL_PREFIX}set_reminder",
                "description": "Schedule a reminder (one-shot due_at ISO-8601, or recurring cron/pattern). Delivered into context when due.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "content": {"type": "string"},
                        "due_at": {"type": "string", "description": "ISO-8601 datetime for one-shot reminders"},
                        "cron": {"type": "string", "description": "Cron expression for recurring reminders"},
                    },
                    "required": ["content"],
                },
            },
            {
                "name": f"{_TOOL_PREFIX}list_reminders",
                "description": "List scheduled reminders, oldest-due first.",
                "parameters": {"type": "object", "properties": {"status": {"type": "string", "description": "e.g. scheduled, all"}}},
            },
        ]

    def handle_tool_call(self, tool_name: str, args: dict[str, Any], **kwargs) -> str:
        try:
            if tool_name == f"{_TOOL_PREFIX}store_memory":
                payload = self._get_client().call_tool(
                    "store_memory",
                    {"content": args["content"], "tags": args.get("tags") or ["agent-stored"]},
                )
            elif tool_name == f"{_TOOL_PREFIX}search_memories":
                call_args: dict[str, Any] = {"query": args["query"], "limit": int(args.get("limit") or self._cfg["recall_limit"])}
                if args.get("min_similarity") is not None:
                    call_args["min_similarity"] = float(args["min_similarity"])
                payload = self._get_client().call_tool("retrieve_memories", call_args)
            elif tool_name == f"{_TOOL_PREFIX}recall":
                payload = self._get_client().call_tool("recall", {"query": args["query"]})
            elif tool_name == f"{_TOOL_PREFIX}set_reminder":
                rem_args = {"content": args["content"]}
                for key in ("due_at", "cron", "pattern", "project"):
                    if args.get(key) is not None:
                        rem_args[key] = args[key]
                payload = self._get_client().call_tool("set_reminder", rem_args)
            elif tool_name == f"{_TOOL_PREFIX}list_reminders":
                payload = self._get_client().call_tool("list_reminders", {"status": args.get("status", "scheduled")})
            else:
                return json.dumps({"error": f"unknown tool {tool_name}"})
            return json.dumps({"result": payload}, default=str)
        except AlexandriaToolError as exc:
            return json.dumps({"error": str(exc)})
        except AlexandriaError as exc:
            return json.dumps({"error": f"alexandria unavailable: {exc}"})
        except Exception as exc:  # noqa: BLE001 - tool handlers must return JSON
            return json.dumps({"error": f"alexandria tool error: {exc}"})

    # -- setup UX ---------------------------------------------------------------------------

    def get_config_schema(self) -> list[dict[str, Any]]:
        return [
            {"key": "url", "description": "Alexandria MCP endpoint", "default": DEFAULTS["url"]},
            {"key": "auto_store", "description": "Auto-capture corrections/preferences during sessions",
             "default": "true", "choices": ["true", "false"]},
            {"key": "extract", "description": "LLM extraction pass at session end",
             "default": "true", "choices": ["true", "false"]},
        ]

    def save_config(self, values: dict[str, Any], hermes_home: str) -> None:
        from hermes_cli.config import load_config, save_config as write_config
        config = load_config()
        memory = config.setdefault("memory", {})
        block = memory.setdefault("alexandria", {})
        block.update(values)
        write_config(config, merge_existing=True)


def register(ctx) -> None:
    """Register Alexandria as a memory provider plugin (one instance per load)."""
    ctx.register_memory_provider(AlexandriaMemoryProvider())
