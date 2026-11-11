"""Minimal MCP StreamableHTTP client for the Alexandria memory server.

Sync (httpx), stdlib-only besides httpx (already present in Hermes' venv). Speaks just
enough of the MCP Streamable HTTP transport for tool use: initialize handshake with
session id, tools/list, tools/call. JSON and SSE response bodies are both accepted.

Deliberately not using the `mcp` Python SDK: Hermes' venv ships mcp 2.3.0 whose client
API differs from the widely-documented 1.x surface, and a memory provider needs ~150
lines of protocol at most.
"""

from __future__ import annotations

import itertools
import json
import logging
import threading
import uuid
from typing import Any, Optional

import httpx

logger = logging.getLogger(__name__)

# Newest version we offer; the server's negotiated answer is echoed back per spec.
PROTOCOL_VERSION = "2025-06-18"


class AlexandriaError(RuntimeError):
    """Transport or protocol failure (server unreachable, bad JSON-RPC, session lost)."""


class AlexandriaToolError(RuntimeError):
    """The tool executed and the server reported an application-level error."""

    def __init__(self, message: str, payload: Any = None):
        super().__init__(message)
        self.payload = payload


class AlexandriaClient:
    """One MCP session against ``url`` (e.g. ``http://127.0.0.1:3000/mcp``).

    Thread-safe: a lock serializes the request-id counter and the handshake; httpx
    handles socket-level concurrency. The session is established lazily and
    re-established once automatically if the server drops it (404/410).
    """

    def __init__(self, url: str, *, timeout: float = 10.0, client_name: str = "hermes-alexandria"):
        self.url = url
        self.timeout = timeout
        self._client_name = client_name
        self._http = httpx.Client(timeout=timeout)
        self._lock = threading.RLock()
        self._req_id = itertools.count(1)
        self._session_id: Optional[str] = None
        self._protocol_version = PROTOCOL_VERSION
        self.server_info: dict[str, Any] = {}

    # -- transport ----------------------------------------------------------

    def _headers(self) -> dict[str, str]:
        headers = {
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
        }
        if self._session_id:
            headers["Mcp-Session-Id"] = self._session_id
            headers["MCP-Protocol-Version"] = self._protocol_version
        return headers

    @staticmethod
    def _decode_body(resp: httpx.Response) -> Optional[dict[str, Any]]:
        """Return the JSON-RPC message from a response body (plain JSON or SSE)."""
        body = resp.text
        if not body.strip():
            return None
        ctype = resp.headers.get("content-type", "")
        if "text/event-stream" in ctype:
            # SSE: the JSON-RPC message rides a `data:` line (possibly several events).
            for line in body.splitlines():
                if line.startswith("data:"):
                    chunk = line[5:].strip()
                    if not chunk or chunk == "[DONE]":
                        continue
                    try:
                        msg = json.loads(chunk)
                    except json.JSONDecodeError:
                        continue
                    if isinstance(msg, dict) and ("result" in msg or "error" in msg):
                        return msg
            return None
        try:
            msg = json.loads(body)
        except json.JSONDecodeError as exc:
            raise AlexandriaError(f"non-JSON response from {resp.request.url}: {body[:200]!r}") from exc
        return msg if isinstance(msg, dict) else None

    def _post(self, message: dict[str, Any], *, want_reply: bool) -> Optional[dict[str, Any]]:
        try:
            resp = self._http.post(self.url, headers=self._headers(), json=message)
        except httpx.HTTPError as exc:
            raise AlexandriaError(f"alexandria unreachable at {self.url}: {exc}") from exc
        if resp.status_code in (202, 204) and not want_reply:
            return None
        if resp.status_code in (404, 410):
            raise _SessionLost(f"session rejected ({resp.status_code})")
        if resp.status_code >= 400:
            raise AlexandriaError(f"HTTP {resp.status_code} from {self.url}: {resp.text[:200]!r}")
        msg = self._decode_body(resp)
        if want_reply and msg is None:
            raise AlexandriaError("no JSON-RPC reply in response body")
        return msg

    # -- session ------------------------------------------------------------

    def initialize(self) -> None:
        with self._lock:
            req = {
                "jsonrpc": "2.0",
                "id": str(next(self._req_id)),
                "method": "initialize",
                "params": {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": self._client_name, "version": "0.1.0"},
                },
            }
            try:
                resp = self._http.post(self.url, headers=self._headers(), json=req)
            except httpx.HTTPError as exc:
                raise AlexandriaError(f"alexandria unreachable at {self.url}: {exc}") from exc
            if resp.status_code >= 400:
                raise AlexandriaError(f"initialize failed: HTTP {resp.status_code} {resp.text[:200]!r}")
            msg = self._decode_body(resp) or {}
            if "error" in msg:
                raise AlexandriaError(f"initialize error: {msg['error']}")
            result = msg.get("result", {})
            self._session_id = resp.headers.get("mcp-session-id") or None
            self._protocol_version = result.get("protocolVersion", PROTOCOL_VERSION)
            self.server_info = result.get("serverInfo", {})
            try:
                self._post({"jsonrpc": "2.0", "method": "notifications/initialized"}, want_reply=False)
            except AlexandriaError as exc:
                logger.debug("alexandria initialized-notification not acknowledged: %s", exc)
            logger.info("alexandria session %s established (%s)", self._session_id, self.server_info)

    def ensure_ready(self) -> None:
        with self._lock:
            if self._session_id is None:
                self.initialize()

    # -- tools --------------------------------------------------------------

    def _rpc(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        self.ensure_ready()
        with self._lock:
            req = {"jsonrpc": "2.0", "id": str(next(self._req_id)), "method": method, "params": params}
            try:
                msg = self._post(req, want_reply=True)
            except _SessionLost:
                # Server restarted or session expired: re-handshake once, then retry.
                self._session_id = None
                self.initialize()
                req["id"] = str(next(self._req_id))
                msg = self._post(req, want_reply=True)
        if msg and "error" in msg:
            raise AlexandriaError(f"{method} failed: {msg['error']}")
        return (msg or {}).get("result", {})

    def list_tools(self) -> list[dict[str, Any]]:
        return self._rpc("tools/list", {}).get("tools", [])

    def call_tool(self, name: str, arguments: dict[str, Any]) -> Any:
        """Call a tool and return the payload parsed from its text content.

        Raises AlexandriaToolError when the tool reports isError; the parsed payload is
        attached to the exception when it is JSON.
        """
        result = self._rpc("tools/call", {"name": name, "arguments": arguments})
        texts: list[str] = []
        for block in result.get("content", []):
            if isinstance(block, dict) and block.get("type") == "text":
                texts.append(block.get("text", ""))
        raw = "\n".join(texts).strip()
        payload: Any = raw
        if raw:
            try:
                payload = json.loads(raw)
            except json.JSONDecodeError:
                pass
        if result.get("isError"):
            message = raw or f"tool {name} failed"
            if isinstance(payload, dict):
                message = str(payload.get("error") or payload.get("message") or message)
            raise AlexandriaToolError(message, payload)
        return payload

    def close(self) -> None:
        with self._lock:
            if self._session_id:
                try:
                    self._http.delete(self.url, headers=self._headers())
                except Exception:  # noqa: BLE001 - best effort
                    pass
                self._session_id = None
            self._http.close()


class _SessionLost(AlexandriaError):
    """Internal: the server no longer knows this session; triggers one re-handshake."""
