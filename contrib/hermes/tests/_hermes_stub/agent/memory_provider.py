"""Minimal stand-in for Hermes' ``agent.memory_provider``, used only when a Hermes
source tree is not on disk (CI). It mirrors the real ABC's surface — same abstract
methods, same hook signatures — so the companion's subclass contract is still
exercised. Local runs against an installed Hermes import the real module instead
(see the path setup at the top of each test file); if the two ever drift, the local
run is the one that catches it."""

import re
import threading
from abc import ABC, abstractmethod
from dataclasses import dataclass
from typing import Any, Callable, Optional

INDICATOR_GLYPH = "🧠"


@dataclass
class RecallStatus:
    provider_label: str
    count: int
    glyph: str = INDICATOR_GLYPH


TRIVIAL_PROMPT_RE = re.compile(
    r'^(yes|no|ok|okay|sure|thanks|thank you|y|n|yep|nope|yeah|nah|'
    r'hi|hey|hello|yo|sup|'
    r'continue|go ahead|do it|proceed|got it|cool|nice|great|done|next|lgtm|k)'
    r'[\s!?.:;,"' + "'" + r'~\u2018\u2019\u201c\u201d\u2014\u2013\u2026()\[\]{}<>*&^%$#@!+=`\u00a0]*$',
    re.IGNORECASE,
)


def is_trivial_prompt(text: Optional[str]) -> bool:
    stripped = (text or "").strip()
    if not stripped or stripped.startswith("/"):
        return True
    return bool(TRIVIAL_PROMPT_RE.match(stripped))


def spawn_context_thread(target: Callable[..., Any], *, name: str, daemon: bool = True,
                         args: tuple = (), kwargs: Optional[dict[str, Any]] = None) -> threading.Thread:
    # The real helper binds the spawner's contextvars; a plain thread is behaviourally
    # equivalent wherever no profile contextvars exist (every CI/test process).
    return threading.Thread(target=target, args=args, kwargs=kwargs or {}, name=name, daemon=daemon)


class MemoryProvider(ABC):
    """Mirror of Hermes' provider ABC — abstract set and every hook the companion overrides."""

    @property
    @abstractmethod
    def name(self) -> str: ...

    @abstractmethod
    def is_available(self) -> bool: ...

    @abstractmethod
    def initialize(self, session_id: str, **kwargs) -> None: ...

    def system_prompt_block(self) -> str:
        return ""

    def prefetch(self, query: str, *, session_id: str = "") -> str:
        return ""

    def queue_prefetch(self, query: str, *, session_id: str = "") -> None: ...

    def recall_status(self) -> Optional[RecallStatus]:
        return None

    def sync_turn(
        self, user_content: str, assistant_content: str, *,
        session_id: str = "", messages: Optional[list[dict[str, Any]]] = None,
        turn_author: Optional[dict[str, Any]] = None,
    ) -> None: ...

    @abstractmethod
    def get_tool_schemas(self) -> list[dict[str, Any]]: ...

    def handle_tool_call(self, tool_name: str, args: dict[str, Any], **kwargs) -> str:
        raise NotImplementedError(f"Provider {self.name} does not handle tool {tool_name}")

    def shutdown(self) -> None: ...

    def on_session_end(self, messages: list[dict[str, Any]]) -> None: ...

    def on_session_switch(self, session_id: str, *, reset: bool = False, **kwargs) -> None: ...

    def on_pre_compress(self, messages: list[dict[str, Any]]) -> str:
        return ""

    def get_config_schema(self) -> list[dict[str, Any]]:
        return []

    def save_config(self, values: dict[str, Any], hermes_home: str) -> None: ...

    def on_memory_write(self, action: str, target: str, content: str,
                        metadata: Optional[dict[str, Any]] = None) -> None: ...
