"""Alexandria memory provider for Hermes Agent.

Semantic long-term memory via a running Alexandria MCP server (StreamableHTTP).
Recall is injected before substantive turns; only explicit tool calls store
memories. Turn and session hooks never capture or extract data.

Configure ``memory.alexandria.url`` in Hermes config, the bearer credential in
``ALEXANDRIA_API_TOKEN``, and optionally an issued project ID in
``ALEXANDRIA_PROJECT_ID``. Without a project, memory scope is Personal-only.
"""

from __future__ import annotations

import copy
import json
import logging
import os
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
from .tools import tool_schemas

logger = logging.getLogger(__name__)

DEFAULTS: dict[str, Any] = {
    "url": "http://127.0.0.1:3000/mcp",
    "timeout": 10.0,
    "recall_limit": 5,
    "min_similarity": 0.45,
    "cache_ttl": 60.0,
}

# These host contexts must not claim foreground reminder deliveries.
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


class AlexandriaMemoryProvider(MemoryProvider):
    """MemoryProvider backed by an Alexandria MCP server. All state is instance-bound
    (multiplexing contract): one instance per profile, bound at initialize()."""

    def __init__(
        self,
        *,
        config: Optional[dict[str, Any]] = None,
        client_factory: Optional[Callable[[str, float], Any]] = None,
    ):
        self._config_override = config
        self._api_token = os.environ.get("ALEXANDRIA_API_TOKEN", "").strip()
        self._project = os.environ.get("ALEXANDRIA_PROJECT_ID", "").strip() or None
        self._client_factory = client_factory or (
            lambda url, timeout: AlexandriaClient(url, timeout=timeout, api_token=self._api_token)
        )

        self._cfg: dict[str, Any] = dict(DEFAULTS)
        self._session_id = ""
        self._agent_context = "primary"
        self._client: Any = None
        self._client_lock = threading.Lock()
        self._transport_lock = threading.RLock()
        self._admission_lock = threading.Lock()
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
        """Config-only check; do not contact Alexandria to determine availability."""
        return bool(self._api_token)

    def unavailable_reason(self) -> str:
        return "" if self._api_token else "ALEXANDRIA_API_TOKEN is required"

    # -- lifecycle -------------------------------------------------------------

    def initialize(self, session_id: str, **kwargs) -> None:
        home = kwargs.get("hermes_home")
        self._bound_home = str(home) if home else ""
        self._session_id = session_id
        self._agent_context = str(kwargs.get("agent_context") or "primary")
        if not self._shutdown and self._worker is None:
            configured = self._config_override if self._config_override is not None else _load_plugin_config()
            self._cfg.update({key: configured[key] for key in DEFAULTS if key in configured})
            self._worker = spawn_context_thread(self._worker_loop, name="alexandria-sync")
            self._worker.start()

    def _get_client(self):
        with self._client_lock:
            if self._client is None:
                self._client = self._client_factory(self._cfg["url"], float(self._cfg["timeout"]))
            return self._client

    def _client_call(self, name: str, args: dict[str, Any]) -> Any:
        with self._transport_lock:
            if self._shutdown and threading.current_thread() is not self._worker:
                raise AlexandriaError("provider is shutting down")
            if name in ("retrieve_memories", "recall"):
                args = {**args, **self._memory_scope(project_scope="auto")}
            return self._get_client().call_tool(name, args)

    def _memory_scope(self, *, project_scope: str) -> dict[str, str]:
        if self._project:
            return {"scope": project_scope, "project": self._project}
        return {"scope": "personal"}

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
            "Use alexandria_store_memory explicitly for durable facts the user wants retained "
            "(preferences, conventions, gotchas, root causes); alexandria_search_memories / "
            "alexandria_recall to look things up beyond the injected context; "
            "alexandria_set_reminder / alexandria_list_reminders for follow-ups."
        )

    def prefetch(self, query: str, *, session_id: str = "") -> str:
        foreground = self._agent_context not in _NON_PRIMARY_CONTEXTS
        reminder_text = self._check_reminders() if foreground else ""
        if reminder_text is None:
            self._recall_cache.clear()
            self._last_recall_count = None
            return ""
        if is_trivial_prompt(query):
            self._last_recall_count = None
            return reminder_text
        now = time.time()
        key = session_id or self._session_id
        # Cached foreground recall needs a fresh successful authenticated request.
        # Non-foreground contexts must not consume reminders to obtain that proof.
        cached = self._recall_cache.get(key) if foreground else None
        if cached and now - cached[0] < float(self._cfg["cache_ttl"]):
            text, count = cached[1], cached[2]
        else:
            try:
                text, count = self._recall(query, key)
            except Exception as exc:  # noqa: BLE001 - failure must never break the turn
                logger.info("alexandria prefetch failed: %s", exc)
                self._recall_cache.pop(key, None)
                text, count = "", 0
            else:
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
                self._recall_cache.pop(sid, None)
                return
            self._recall_cache[sid] = (time.time(), text, count)

        self._enqueue(refresh)

    def recall_status(self) -> Optional[RecallStatus]:
        if self._last_recall_count is None:
            return None
        return RecallStatus(provider_label="alexandria", count=self._last_recall_count)

    def _recall(self, query: str, session_id: str) -> tuple[str, int]:
        """Non-consuming scoped retrieval, including for background refresh."""
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

    def _check_reminders(self) -> Optional[str]:
        try:
            args = {"project": self._project} if self._project else {}
            payload = self._client_call("check_reminders", args)
            if not isinstance(payload, dict) or not isinstance(payload.get("delivered"), list):
                return None
            due = payload["delivered"]
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
        except Exception as exc:  # noqa: BLE001 - failure does not authorize cached recall
            logger.info("alexandria check_reminders failed: %s", exc)
            return None

    # -- host lifecycle (no implicit memory writes) ----------------------------------

    def sync_turn(
        self,
        user_content: str,
        assistant_content: str,
        *,
        session_id: str = "",
        messages: Optional[list[dict[str, Any]]] = None,
        turn_author: Optional[dict[str, Any]] = None,
    ) -> None:
        """Do not persist, capture or extract anything from implicit host callbacks."""

    def on_session_switch(self, new_session_id: str, *, parent_session_id: str = "", reset: bool = False, rewound: bool = False, **kwargs) -> None:
        self._session_id = new_session_id
        if reset:
            self._drop_recall(new_session_id)

    def on_session_end(self, messages: list[dict[str, Any]]) -> None:
        self._drop_recall(self._session_id)

    def _drop_recall(self, session_id: str) -> None:
        try:
            self._run_ordered(lambda: self._recall_cache.pop(session_id, None))
        except AlexandriaError as exc:
            logger.info("alexandria recall invalidation deferred: %s", exc)

    # -- tools ---------------------------------------------------------------------------

    def get_tool_schemas(self) -> list[dict[str, Any]]:
        return tool_schemas()

    def handle_tool_call(self, tool_name: str, args: dict[str, Any], **kwargs) -> str:
        snapshot = copy.deepcopy(args)
        try:
            return self._run_ordered(lambda: self._handle_tool(tool_name, snapshot))
        except Exception as exc:  # noqa: BLE001 - an accepted write may still finish
            return json.dumps({"error": str(exc)})

    def _handle_tool(self, tool_name: str, args: dict[str, Any]) -> str:
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
                call_args.update(self._memory_scope(project_scope="project"))
                call_args["tags"] = list(args.get("tags") or [])
            elif name == "search_memories":
                name = "retrieve_memories"
                call_args.setdefault("limit", int(self._cfg["recall_limit"]))
            elif name == "list_reminders":
                call_args.setdefault("status", "pending")
            elif name == "check_reminders" and self._project:
                call_args.setdefault("project", self._project)
            payload = self._client_call(name, call_args)
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
        return [{"key": "url", "description": "Alexandria MCP endpoint", "default": DEFAULTS["url"]}]

    def save_config(self, values: dict[str, Any], hermes_home: str) -> None:
        from hermes_cli.config import load_config, save_config as write_config
        config = load_config()
        memory = config.setdefault("memory", {})
        block = memory.setdefault("alexandria", {})
        block.update({key: values[key] for key in DEFAULTS if key in values})
        write_config(config, merge_existing=True)


def register(ctx) -> None:
    """Register Alexandria as a memory provider plugin (one instance per load)."""
    ctx.register_memory_provider(AlexandriaMemoryProvider())
