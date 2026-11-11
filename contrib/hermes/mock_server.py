"""Mock Alexandria MCP server (StreamableHTTP) for testing the Hermes provider without
the Rust build. Implements the transport subset the client speaks and an in-memory
store with naive word-overlap 'semantic' scoring. Not a reference implementation —
response shapes follow the README's documented tool semantics, defensively minimal.

Usage: python mock_server.py [port]  -> prints the URL. Also used by the test suite.
"""

from __future__ import annotations

import json
import re
import threading
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

TOOLS = [
    {"name": "store_memory", "description": "Store a memory.", "inputSchema": {"type": "object", "properties": {"content": {"type": "string"}}}},
    {"name": "retrieve_memories", "description": "Semantic search.", "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}}}},
    {"name": "recall", "description": "Progressive recall.", "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}}}},
    {"name": "update_memory", "description": "Update.", "inputSchema": {"type": "object"}},
    {"name": "delete_memory", "description": "Delete.", "inputSchema": {"type": "object"}},
    {"name": "import_document", "description": "Import.", "inputSchema": {"type": "object"}},
    {"name": "get_session", "description": "Get session.", "inputSchema": {"type": "object"}},
    {"name": "list_sessions", "description": "List sessions.", "inputSchema": {"type": "object"}},
    {"name": "finalize_session", "description": "Finalize session.", "inputSchema": {"type": "object"}},
    {"name": "set_reminder", "description": "Set reminder.", "inputSchema": {"type": "object"}},
    {"name": "check_reminders", "description": "Check reminders.", "inputSchema": {"type": "object"}},
    {"name": "list_reminders", "description": "List reminders.", "inputSchema": {"type": "object"}},
    {"name": "cancel_reminder", "description": "Cancel reminder.", "inputSchema": {"type": "object"}},
]


class MockAlexandria:
    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.memories: list[dict] = []
        self.sessions: dict[str, list[str]] = {}
        self.reminders: list[dict] = []
        self.calls: list[tuple[str, dict]] = []

    def handle(self, name: str, args: dict) -> dict:
        with self.lock:
            self.calls.append((name, args))
            if name == "store_memory":
                mem = {"id": str(uuid.uuid4())[:8], "content": args.get("content", ""),
                       "tags": args.get("tags", []), "session_id": args.get("session_id")}
                self.memories.append(mem)
                if mem["session_id"]:
                    self.sessions.setdefault(mem["session_id"], []).append(mem["id"])
                return {"id": mem["id"], "truncated": False}
            if name == "retrieve_memories":
                query_words = set(re.findall(r"\w+", str(args.get("query", "")).lower()))
                limit = int(args.get("limit", 5))
                scored = []
                for mem in self.memories:
                    words = set(re.findall(r"\w+", mem["content"].lower()))
                    score = len(query_words & words) / max(len(query_words), 1)
                    if score >= float(args.get("min_similarity", 0.0)):
                        scored.append({"id": mem["id"], "content": mem["content"], "score": round(score, 3)})
                scored.sort(key=lambda m: m["score"], reverse=True)
                return {"results": scored[:limit], "due_reminders": []}
            if name == "recall":
                return self.handle("retrieve_memories", args)
            if name == "finalize_session":
                self.sessions.setdefault(args.get("session_id", ""), [])
                return {"finalized": args.get("session_id")}
            if name == "set_reminder":
                rem = {"id": str(uuid.uuid4())[:8], "content": args.get("content", ""), "status": "scheduled"}
                self.reminders.append(rem)
                return {"reminder": rem, "next_fire_times": []}
            if name == "check_reminders":
                return {"reminders": []}
            if name == "list_reminders":
                return {"reminders": self.reminders}
            if name == "list_sessions":
                return {"sessions": list(self.sessions)}
            raise NotImplementedError(f"mock alexandria does not implement {name}")


class Handler(BaseHTTPRequestHandler):
    mock: MockAlexandria  # set by make_server
    sessions: set[str] = set()

    def log_message(self, format: str, *args: object) -> None:  # silence
        pass

    def _send(self, code: int, body: bytes = b"", ctype: str = "application/json", extra: dict | None = None) -> None:
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        for k, v in (extra or {}).items():
            self.send_header(k, v)
        self.end_headers()
        if body:
            self.wfile.write(body)

    def do_POST(self) -> None:  # noqa: N802
        length = int(self.headers.get("Content-Length", 0))
        try:
            msg = json.loads(self.rfile.read(length))
        except (json.JSONDecodeError, ValueError):
            self._send(400)
            return
        method = msg.get("method")
        if method == "initialize":
            sid = str(uuid.uuid4())
            Handler.sessions.add(sid)
            result = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                      "serverInfo": {"name": "mock-alexandria", "version": "0"}}
            self._send(200, json.dumps({"jsonrpc": "2.0", "id": msg.get("id"), "result": result}).encode(),
                       extra={"Mcp-Session-Id": sid})
            return
        if self.headers.get("Mcp-Session-Id") not in Handler.sessions:
            self._send(404, json.dumps({"error": "unknown session"}).encode())
            return
        if method == "notifications/initialized":
            self._send(202)
            return
        try:
            if method == "tools/list":
                result: dict = {"tools": TOOLS}
            elif method == "tools/call":
                params = msg.get("params", {})
                try:
                    payload = self.mock.handle(params.get("name", ""), params.get("arguments", {}))
                    content = [{"type": "text", "text": json.dumps(payload)}]
                    result = {"content": content, "isError": False}
                except Exception as exc:  # noqa: BLE001
                    content = [{"type": "text", "text": json.dumps({"error": str(exc)})}]
                    result = {"content": content, "isError": True}
            else:
                self._send(200, json.dumps({"jsonrpc": "2.0", "id": msg.get("id"),
                                            "error": {"code": -32601, "message": f"no method {method}"}}).encode())
                return
            self._send(200, json.dumps({"jsonrpc": "2.0", "id": msg.get("id"), "result": result}).encode())
        except Exception as exc:  # noqa: BLE001
            self._send(500, json.dumps({"error": str(exc)}).encode())


def make_server(port: int = 0) -> tuple[ThreadingHTTPServer, MockAlexandria]:
    mock = MockAlexandria()
    Handler.mock = mock
    Handler.sessions = set()
    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    return server, mock


if __name__ == "__main__":
    import sys
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 3900
    httpd, _ = make_server(port)
    print(f"mock alexandria MCP on http://127.0.0.1:{httpd.server_address[1]}/mcp", flush=True)
    httpd.serve_forever()
