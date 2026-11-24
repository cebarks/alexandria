import json
import os
import subprocess
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

HOOKS = Path(__file__).resolve().parents[1] / "hooks"
TOKEN = "test-only-bearer"


class MockMcpHandler(BaseHTTPRequestHandler):
    calls = []
    session = "local-fixture-session"

    def log_message(self, *_args):
        pass

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        body = json.loads(self.rfile.read(length))
        self.calls.append({
            "method": body.get("method"),
            "tool": body.get("params", {}).get("name"),
            "args": body.get("params", {}).get("arguments", {}),
            "authorization": self.headers.get("Authorization"),
        })
        if body.get("method") == "initialize":
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Mcp-Session-Id", self.session)
            self.end_headers()
            response = {"jsonrpc": "2.0", "id": body["id"], "result": {"protocolVersion": "2025-03-26", "capabilities": {}}}
        elif body.get("method") == "tools/call":
            tool = body["params"]["name"]
            if tool == "resolve_project":
                text = json.dumps({"status": "ok", "project": {"id": "project:fixture"}})
            elif tool == "retrieve_memories":
                text = json.dumps({"results": [{"id": "fact:fixture", "similarity": 0.99, "tags": [], "content": "fixture memory"}]})
            else:
                text = json.dumps({"status": "ok"})
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            response = {"jsonrpc": "2.0", "id": body["id"], "result": {"content": [{"type": "text", "text": text}]}}
        else:
            self.send_response(202)
            self.end_headers()
            return
        self.wfile.write(("data: " + json.dumps(response) + "\n\n").encode())

    def do_DELETE(self):
        self.calls.append({"method": "DELETE", "authorization": self.headers.get("Authorization")})
        self.send_response(200)
        self.end_headers()


class ClaudeHookTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), MockMcpHandler)
        cls.thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.thread.start()
        cls.url = f"http://127.0.0.1:{cls.server.server_port}/mcp"

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()
        cls.thread.join()

    def setUp(self):
        MockMcpHandler.calls = []
        self.temp = tempfile.TemporaryDirectory()
        self.env = os.environ.copy()
        self.env.update({
            "HOME": self.temp.name,
            "XDG_STATE_HOME": str(Path(self.temp.name) / "state"),
            "ALEXANDRIA_URL": self.url,
            "ALEXANDRIA_API_TOKEN": TOKEN,
            "ALEXANDRIA_PROJECT_ID": "project:fixture",
            "ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY": "0.0",
        })
        for key in ("CLAUDE_CODE_ENTRYPOINT", "ALEXANDRIA_AUTO_STORE", "ALEXANDRIA_HOOK_CHILD"):
            self.env.pop(key, None)

    def tearDown(self):
        self.temp.cleanup()

    def run_hook(self, name, payload, extra_env=None):
        env = self.env.copy()
        if extra_env:
            env.update(extra_env)
        return subprocess.run(
            ["bash", str(HOOKS / name)],
            input=payload,
            text=True,
            capture_output=True,
            env=env,
            check=True,
            timeout=10,
        )

    def test_prompt_with_error_receipt_preference_and_correction_only_recalls(self):
        prompt = "No, use the corrected receipt path; always prefer this choice."
        result = self.run_hook("alexandria-recall.sh", json.dumps({"session_id": "s1", "prompt": prompt}))
        self.assertIn("fixture memory", result.stdout)
        tools = [call["tool"] for call in MockMcpHandler.calls if call.get("method") == "tools/call"]
        self.assertEqual(tools, ["resolve_project", "retrieve_memories"])
        self.assertNotIn("store_memory", tools)
        self.assertTrue(all(call["authorization"] == f"Bearer {TOKEN}" for call in MockMcpHandler.calls))
        recall = next(call for call in MockMcpHandler.calls if call.get("tool") == "retrieve_memories")
        self.assertEqual(recall["args"]["scope"], "auto")
        self.assertEqual(recall["args"]["project"], "project:fixture")

    def test_no_token_fails_closed_without_unscoped_recall(self):
        result = self.run_hook(
            "alexandria-recall.sh",
            json.dumps({"session_id": "s1", "prompt": "remember this"}),
            {"ALEXANDRIA_API_TOKEN": ""},
        )
        self.assertIn("ALEXANDRIA_API_TOKEN", result.stdout)
        self.assertFalse(any(call.get("method") == "tools/call" for call in MockMcpHandler.calls))

    def test_obsolete_stop_hook_is_a_noop(self):
        result = self.run_hook(
            "alexandria-extract.sh",
            json.dumps({"session_id": "s1", "transcript_path": "/missing/transcript"}),
            {"ALEXANDRIA_AUTO_STORE": "on", "ALEXANDRIA_EXTRACT_MODEL": "old/model"},
        )
        self.assertIn("obsolete and ignored", result.stderr)
        self.assertFalse(MockMcpHandler.calls)

    def test_explicit_scoped_store_uses_authenticated_mcp_path(self):
        args = {"content": "user-approved fact", "scope": "project", "context_project": "project:fixture"}
        result = subprocess.run(
            ["bash", str(HOOKS / "alexandria-recall.sh"), "store_memory", json.dumps(args)],
            text=True,
            capture_output=True,
            env=self.env,
            check=True,
            timeout=10,
        )
        self.assertIn('"status": "ok"', result.stdout)
        store = next(call for call in MockMcpHandler.calls if call.get("tool") == "store_memory")
        self.assertEqual(store["authorization"], f"Bearer {TOKEN}")
        self.assertEqual(store["args"]["scope"], "project")
        self.assertEqual(store["args"]["context_project"], "project:fixture")

    def test_explicit_store_attribution_preserves_scope_and_project(self):
        payload = {
            "session_id": "s1",
            "tool_name": "mcp__alexandria__store_memory",
            "tool_input": {"content": "explicit user-approved memory", "scope": "personal", "context_project": "project:fixture"},
        }
        result = self.run_hook("alexandria-session.sh", json.dumps(payload))
        update = json.loads(result.stdout)["hookSpecificOutput"]["updatedInput"]
        self.assertEqual(update["scope"], "personal")
        self.assertEqual(update["context_project"], "project:fixture")
        self.assertEqual(update["session_id"], "s1")
        self.assertEqual(update["agent_id"], "claude-code")


if __name__ == "__main__":
    unittest.main()
