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

import copy
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
from .tools import tool_schemas

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


def _config_bool(value: Any, key: str) -> bool:
    if isinstance(value, bool):
        return value
    if isinstance(value, str) and value.strip().lower() in ("true", "false"):
        return value.strip().lower() == "true"
    raise ValueError(f"memory.alexandria.{key} must be a boolean")


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
    """Classify provider errors and Hermes terminal envelopes.

    Nonzero terminal exits cannot resolve a prior failure. Informational exits
    carry no error text; other failures still pass ErrorTracker's signal filter.
    """
    stripped = content.strip()
    if not stripped:
        return False, ""
    try:
        data = json.loads(stripped)
    except json.JSONDecodeError:
        return stripped.lower().startswith("error"), stripped
    if isinstance(data, dict):
        if data.get("error"):
            return True, str(data["error"])
        code = data.get("exit_code")
        if isinstance(code, int) and not isinstance(code, bool) and "output" in data:
            output = str(data.get("output") or "")
            if code != 0:
                meaning = str(data.get("exit_code_meaning") or "").lower()
                return True, "" if "not an error" in meaning else output
            return False, output
    return False, stripped


def _iter_tool_events(messages: list[dict[str, Any]]):
    """Yield ``(identity, tool_name, args, result_text, is_error, error_text)`` for each tool
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
        identity = (str(msg.get("tool_call_id") or ""), text)
        yield identity, name, args, text, is_error, error_text


class _SessionState:
    __slots__ = ("buffer", "errors", "seen_events", "linked_write")

    def __init__(self) -> None:
        self.buffer = SessionDedupBuffer()
        self.errors = ErrorTracker()
        self.seen_events: set[tuple[str, str]] = set()
        self.linked_write = False


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
        self._transport_lock = threading.RLock()
        self._admission_lock = threading.Lock()
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
        for key in ("auto_store", "extract", "store_session"):
            cfg[key] = _config_bool(cfg[key], key)
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

    def _client_call(self, name: str, args: dict[str, Any]) -> Any:
        with self._transport_lock:
            if self._shutdown and threading.current_thread() is not self._worker:
                raise AlexandriaError("provider is shutting down")
            return self._get_client().call_tool(name, args)

    def _close_client(self) -> None:
        with self._transport_lock, self._client_lock:
            if self._client is not None:
                try:
                    self._client.close()
                except Exception as exc:  # noqa: BLE001 - best-effort teardown
                    logger.debug("alexandria close failed: %s", exc)
                self._client = None

    def _worker_loop(self) -> None:
        try:
            while True:
                job = self._jobs.get()
                try:
                    if job is None:
                        return
                    job()
                except Exception as exc:  # noqa: BLE001 - never fatal to the host
                    logger.warning("alexandria background job failed: %s", exc)
                finally:
                    self._jobs.task_done()
        finally:
            self._close_client()

    def _enqueue(self, job: Callable[[], Any]) -> bool:
        with self._admission_lock:
            if self._worker is None or self._shutdown:
                return False
            self._jobs.put(job)
            return True

    def _run_ordered(self, job: Callable[[], Any]) -> Any:
        if threading.current_thread() is self._worker:
            return job()
        done = threading.Event()
        result: list[Any] = []
        errors: list[Exception] = []

        def run() -> None:
            try:
                result.append(job())
            except Exception as exc:
                errors.append(exc)
            finally:
                done.set()

        if not self._enqueue(run):
            raise AlexandriaError("provider worker is unavailable")
        if not done.wait(max(0.01, float(self._cfg["timeout"])) * 4):
            raise AlexandriaError("operation still queued or running; outcome unknown, do not blindly retry")
        if errors:
            raise errors[0]
        return result[0]

    def shutdown(self) -> None:
        with self._admission_lock:
            if not self._shutdown:
                self._shutdown = True
                self._jobs.put(None)
        if self._worker is None:
            self._close_client()
        elif threading.current_thread() is not self._worker:
            self._worker.join(timeout=min(5.0, max(0.01, float(self._cfg["timeout"])) * 4))
            if self._worker.is_alive():
                logger.warning("alexandria shutdown timed out; worker retains client until accepted work completes")

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
        reminder_text = self._check_reminders() if self._writes_allowed() else ""
        if is_trivial_prompt(query):
            self._last_recall_count = None
            return reminder_text
        now = time.time()
        key = session_id or self._session_id
        cached = self._recall_cache.get(key)
        if cached and now - cached[0] < float(self._cfg["cache_ttl"]):
            text, count = cached[1], cached[2]
        else:
            try:
                text, count = self._recall(query, key)
            except Exception as exc:  # noqa: BLE001 - failure must never break the turn
                logger.info("alexandria prefetch failed: %s", exc)
                text, count = (cached[1], cached[2]) if cached else ("", 0)
            self._recall_cache[key] = (now, text, count)
        self._last_recall_count = count
        return "\n\n".join(part for part in (text, reminder_text) if part)

    def queue_prefetch(self, query: str, *, session_id: str = "") -> None:
        if is_trivial_prompt(query):
            return

        sid = session_id or self._session_id

        def refresh() -> None:
            try:
                text, count = self._recall(query, sid)
            except Exception:  # noqa: BLE001
                return
            self._recall_cache[sid] = (time.time(), text, count)

        self._enqueue(refresh)

    def recall_status(self) -> Optional[RecallStatus]:
        if self._last_recall_count is None:
            return None
        return RecallStatus(provider_label="alexandria", count=self._last_recall_count)

    def _recall(self, query: str, session_id: str) -> tuple[str, int]:
        """Non-consuming global retrieval, safe for background refresh and caching."""
        args: dict[str, Any] = {
            "query": query,
            "limit": int(self._cfg["recall_limit"]),
        }
        payload = self._client_call("retrieve_memories", args)
        lines: list[str] = []
        count = 0
        for item in self._filtered_results(payload):
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
        if not lines:
            return "", 0
        body = "\n".join(lines)
        return f"## Alexandria memory (semantic recall)\n{body}", count

    def _filtered_results(self, payload: Any) -> list[dict[str, Any]]:
        results = payload.get("results", []) if isinstance(payload, dict) else payload
        floor = float(self._cfg["min_similarity"])
        return [item for item in results or [] if isinstance(item, dict)
                and isinstance(item.get("similarity", item.get("score")), (int, float))
                and item.get("similarity", item.get("score")) >= floor]

    def _check_reminders(self) -> str:
        try:
            payload = self._client_call("check_reminders", {})
            due = payload.get("delivered", []) if isinstance(payload, dict) else []
            lines = []
            for reminder in due:
                message = reminder.get("message") if isinstance(reminder, dict) else None
                if message:
                    context = []
                    if reminder.get("recurring"):
                        context.append("recurring")
                    if reminder.get("escalated"):
                        context.append("escalated")
                    missed = reminder.get("missed_occurrences", 0)
                    if missed:
                        capped = "+" if reminder.get("missed_occurrences_saturated") else ""
                        context.append(f"missed: {missed}{capped}")
                    suffix = f" ({', '.join(context)})" if context else ""
                    target = reminder.get("target", "global")
                    lines.append(f"- [{target}] {message}{suffix}")
                    if reminder.get("note"):
                        lines.append(f"  Note: {reminder['note']}")
            return "## Due reminders\n" + "\n".join(lines) if lines else ""
        except Exception as exc:  # noqa: BLE001 - foreground recall must fail open
            logger.info("alexandria check_reminders failed: %s", exc)
            return ""

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
        snapshot = copy.deepcopy(messages or [])
        sid = session_id or self._session_id
        self._enqueue(lambda: self._handle_turn(sid, user_content, snapshot))

    def _handle_turn(self, session_id: str, user_text: str, messages: list[dict[str, Any]]) -> None:
        st = self._state(session_id)
        if self._cfg["auto_store"] and self._writes_allowed():
            for detected in (detect_correction(user_text, st.buffer), detect_preference(user_text, st.buffer)):
                if detected and not self._store(detected, session_id):
                    st.buffer.discard_heuristic_store(detected["content"])
        for identity, name, args, text, is_error, error_text in _iter_tool_events(messages):
            if identity in st.seen_events:
                continue
            st.seen_events.add(identity)
            track_tool_store(name, args.get("content"), is_error, st.buffer)
            if not self._cfg["auto_store"] or not self._writes_allowed():
                continue
            if is_error:
                st.errors.record_error(name, error_text)
                continue
            st.errors.record_success(name, error_text)

    def _store(self, memory: dict[str, Any], session_id: str, extra_tags: Optional[list[str]] = None) -> bool:
        tags = list(memory.get("tags") or [])
        if extra_tags:
            tags.extend(extra_tags)
        args = self._store_args({"content": memory["content"], "tags": tags}, session_id)
        try:
            self._client_call("store_memory", args)
            st = self._state(session_id)
            st.buffer.add_tool_store(memory["content"])
            st.linked_write = st.linked_write or "session_id" in args
            return True
        except (AlexandriaError, AlexandriaToolError) as exc:
            logger.info("alexandria store failed (%s): %s", memory["content"][:60], exc)
            return False

    def _store_args(self, memory: dict[str, Any], session_id: str) -> dict[str, Any]:
        args = {"content": memory["content"], "tags": list(memory.get("tags") or [])}
        sid = session_id or self._session_id
        if self._cfg["store_session"] and sid:
            args["session_id"] = sid
        return args

    # -- session boundaries -----------------------------------------------------------

    def on_session_switch(self, new_session_id: str, *, parent_session_id: str = "", reset: bool = False, rewound: bool = False, **kwargs) -> None:
        self._session_id = new_session_id
        if reset:
            def clear() -> None:
                with self._states_lock:
                    self._states.pop(new_session_id, None)
                self._recall_cache.pop(new_session_id, None)
            try:
                self._run_ordered(clear)
            except AlexandriaError as exc:
                logger.info("alexandria reset deferred: %s", exc)
        # Stable event identities survive a shortened/rewound transcript. Keep
        # confirmed dedup so replaying an existing tool result cannot store it twice.

    def on_session_end(self, messages: list[dict[str, Any]]) -> None:
        sid = self._session_id
        snapshot = copy.deepcopy(messages)
        try:
            self._run_ordered(lambda: self._finish_session(sid, snapshot))
        except Exception as exc:  # noqa: BLE001 - worker retains unfinished state
            logger.info("alexandria session end incomplete: %s", exc)

    def _finish_session(self, sid: str, messages: list[dict[str, Any]]) -> None:
        st = self._state(sid)
        failed = False
        if self._writes_allowed():
            if self._cfg["auto_store"]:
                for memory in st.errors.pending_resolutions():
                    if self._store(memory, sid):
                        st.errors.acknowledge(memory)
                    else:
                        failed = True
            if self._cfg["extract"]:
                memories = run_extraction(
                    messages, st.buffer.get_all_stored_contents(), llm=self._llm,
                    timeout=float(self._cfg["timeout"]) * 3,
                )
                for memory in memories:
                    if not st.buffer.contains(memory["content"]):
                        failed = not self._store(memory, sid, extra_tags=["extracted", "source:llm"]) or failed
                if memories and sid and self._cfg["store_session"] and st.linked_write:
                    try:
                        self._client_call("finalize_session", {
                            "session_id": sid, "summary": memories[0]["content"][:500], "tags": ["hermes"],
                        })
                    except (AlexandriaError, AlexandriaToolError) as exc:
                        logger.debug("alexandria finalize_session skipped: %s", exc)
        if not failed:
            with self._states_lock:
                self._states.pop(sid, None)
        self._recall_cache.pop(sid, None)

    # -- tools ---------------------------------------------------------------------------

    def get_tool_schemas(self) -> list[dict[str, Any]]:
        return tool_schemas()

    def handle_tool_call(self, tool_name: str, args: dict[str, Any], **kwargs) -> str:
        sid = self._session_id
        snapshot = copy.deepcopy(args)
        try:
            return self._run_ordered(lambda: self._handle_tool(tool_name, snapshot, sid))
        except Exception as exc:  # noqa: BLE001 - an accepted write may still finish
            return json.dumps({"error": str(exc)})

    def _handle_tool(self, tool_name: str, args: dict[str, Any], sid: str) -> str:
        schemas = {schema["name"]: schema["parameters"] for schema in self.get_tool_schemas()}
        schema = schemas.get(tool_name)
        if schema is None:
            return json.dumps({"error": f"unknown tool {tool_name}"})
        try:
            missing = set(schema["required"]) - args.keys()
            unknown = args.keys() - schema["properties"].keys()
            if missing or unknown:
                raise ValueError(f"invalid arguments: missing={sorted(missing)}, unknown={sorted(unknown)}")
            name = tool_name[len(_TOOL_PREFIX):]
            call_args = dict(args)
            if name == "store_memory":
                call_args = self._store_args(args, sid)
            elif name == "search_memories":
                name = "retrieve_memories"
                call_args.setdefault("limit", int(self._cfg["recall_limit"]))
            elif name == "list_reminders":
                call_args.setdefault("status", "pending")
            elif name == "set_reminder" and self._cfg["store_session"] and sid:
                call_args["session_id"] = sid
            payload = self._client_call(name, call_args)
            if name in ("store_memory", "update_memory") and isinstance(args.get("content"), str):
                st = self._state(sid)
                st.buffer.add_tool_store(args["content"])
                if name == "store_memory" and "session_id" in call_args:
                    st.linked_write = True
            if name == "retrieve_memories" and isinstance(payload, dict):
                payload = {**payload, "results": self._filtered_results(payload)}
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
            {"key": "auto_store", "description": "Auto-capture corrections, preferences and error resolutions",
             "type": "boolean", "default": True},
            {"key": "extract", "description": "LLM extraction pass at session end",
             "type": "boolean", "default": True},
        ]

    def save_config(self, values: dict[str, Any], hermes_home: str) -> None:
        from hermes_cli.config import load_config, save_config as write_config
        config = load_config()
        memory = config.setdefault("memory", {})
        block = memory.setdefault("alexandria", {})
        normalized = dict(values)
        for key in ("auto_store", "extract", "store_session"):
            if key in normalized:
                normalized[key] = _config_bool(normalized[key], key)
        block.update(normalized)
        write_config(config, merge_existing=True)


def register(ctx) -> None:
    """Register Alexandria as a memory provider plugin (one instance per load)."""
    ctx.register_memory_provider(AlexandriaMemoryProvider())
