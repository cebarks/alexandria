"""Scoped provider behavior over the authenticated loopback MCP fixture."""

import json
import os
import threading
import unittest
from unittest.mock import patch
from _bootstrap import bootstrap_hermes

bootstrap_hermes()

from alexandria import AlexandriaMemoryProvider  # noqa: E402
from alexandria.client import AlexandriaError  # noqa: E402
from mock_server import MOCK_API_TOKEN, make_server  # noqa: E402


class ProviderTestCase(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server, cls.mock = make_server()
        cls.url = f"http://127.0.0.1:{cls.server.server_address[1]}/mcp"
        cls.thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.thread.start()

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()

    def setUp(self):
        self.mock.memories.clear()
        self.mock.calls.clear()
        with patch.dict(os.environ, {"ALEXANDRIA_API_TOKEN": MOCK_API_TOKEN, "ALEXANDRIA_PROJECT_ID": ""}):
            self.provider = AlexandriaMemoryProvider(config={"url": self.url, "timeout": 5.0, "min_similarity": 0.05})
        self.provider.initialize("sess-1", platform="cli", agent_context="primary")
        self.addCleanup(self.provider.shutdown)

    def stored_contents(self):
        return [m["content"] for m in self.mock.memories]

    # -- recall ---------------------------------------------------------------

    def test_trivial_prompts_skip_recall(self):
        self.assertEqual(self.provider.prefetch("thanks", session_id="sess-1"), "")
        self.assertEqual(self.provider.prefetch("/new", session_id="sess-1"), "")

    def test_prefetch_injects_relevant_memories(self):
        self.provider._get_client().call_tool("store_memory", {"content": "the deploy target is den-prod-01"})
        text = self.provider.prefetch("what is the deploy target", session_id="sess-1")
        self.assertIn("den-prod-01", text)
        self.assertIn("Alexandria memory", text)
        status = self.provider.recall_status()
        self.assertIsNotNone(status)
        self.assertEqual(status.count, 1)

    def test_prefetch_transport_failure_drops_cache(self):
        self.provider._get_client().call_tool("store_memory", {"content": "cached fact about kafka"})
        self.assertIn("kafka", self.provider.prefetch("kafka deployment", session_id="sess-1"))
        with patch.object(self.provider, "_client_call", side_effect=AlexandriaError("transport unavailable")):
            self.assertEqual(self.provider.prefetch("kafka deployment", session_id="sess-1"), "")
        self.assertNotIn("sess-1", self.provider._recall_cache)

    def test_turn_and_session_end_do_not_write_even_with_a_valid_bearer(self):
        user_text = "Actually, the deploy target is den-prod-01."
        messages = [{"role": "user", "content": user_text}, {"role": "assistant", "content": "Got it."}]
        self.provider.sync_turn(user_text, "Got it.", session_id="sess-1", messages=messages)
        self.provider.on_session_end(messages)
        self.assertEqual(self.mock.calls, [])
        self.assertEqual(self.stored_contents(), [])

    # -- tools / mirrors ---------------------------------------------------------

    def test_handle_tool_call_store_and_search(self):
        out = json.loads(self.provider.handle_tool_call("alexandria_store_memory",
                                                        {"content": "kafka runs on port 9093", "tags": ["infra"]}))
        self.assertIn("result", out)
        out = json.loads(self.provider.handle_tool_call("alexandria_search_memories", {"query": "kafka port"}))
        self.assertIn("kafka", json.dumps(out))

    def test_handle_tool_call_unknown_and_error_shapes(self):
        out = json.loads(self.provider.handle_tool_call("alexandria_nope", {}))
        self.assertIn("error", out)
        out = json.loads(self.provider.handle_tool_call("alexandria_store_memory", {"content": "x"}))
        self.assertIn("result", out)

    def test_builtin_memory_write_is_not_mirrored(self):
        for action in ("add", "replace", "remove"):
            self.provider.on_memory_write(action, "user", "local only")
        self.assertEqual(self.stored_contents(), [])

    def test_session_switch_reset_clears_recall_cache(self):
        self.provider.prefetch("our deployment policy", session_id="sess-3")
        self.assertIn("sess-3", self.provider._recall_cache)
        self.provider.on_session_switch("sess-3", reset=True)
        self.assertNotIn("sess-3", self.provider._recall_cache)


if __name__ == "__main__":
    unittest.main(verbosity=2)
