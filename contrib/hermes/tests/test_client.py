"""Client tests against the mock Alexandria MCP server (real HTTP, real protocol)."""

import threading
import unittest

from _bootstrap import bootstrap_hermes

bootstrap_hermes()

from alexandria.client import AlexandriaClient, AlexandriaError, AlexandriaToolError  # noqa: E402
from mock_server import make_server  # noqa: E402


class ClientTestCase(unittest.TestCase):
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
        self.client = AlexandriaClient(self.url, timeout=5)
        self.addCleanup(self.client.close)


class TestHandshake(ClientTestCase):
    def test_lazy_initialize_on_first_call(self):
        self.assertIsNone(self.client._session_id)
        payload = self.client.call_tool("store_memory", {"content": "fact one"})
        self.assertIn("id", payload)
        self.assertIsNotNone(self.client._session_id)
        self.assertEqual(self.client.server_info.get("name"), "mock-alexandria")

    def test_list_tools(self):
        tools = self.client.list_tools()
        names = {t["name"] for t in tools}
        self.assertIn("store_memory", names)
        self.assertIn("retrieve_memories", names)
        self.assertEqual(len(tools), 13)


class TestTools(ClientTestCase):
    def test_store_and_retrieve(self):
        self.client.call_tool("store_memory", {"content": "the deploy target is den-prod-01", "tags": ["infra"]})
        out = self.client.call_tool("retrieve_memories", {"query": "deploy target", "limit": 5, "min_similarity": 0.1})
        self.assertEqual(len(out["results"]), 1)
        self.assertIn("den-prod-01", out["results"][0]["content"])

    def test_tool_error_surfaces_as_typed_exception(self):
        with self.assertRaises(AlexandriaToolError) as ctx:
            self.client.call_tool("delete_memory", {"id": "nope"})
        self.assertIn("not found", str(ctx.exception))

    def test_session_loss_triggers_one_rehandshake(self):
        self.client.call_tool("store_memory", {"content": "before restart"})
        sid = self.client._session_id
        # Simulate server restart: the server forgets every session.
        self.server.RequestHandlerClass.sessions.clear()
        out = self.client.call_tool("retrieve_memories", {"query": "before restart"})
        self.assertNotEqual(self.client._session_id, sid)
        self.assertEqual(len(out["results"]), 1)

    def test_unreachable_server_raises_transport_error(self):
        client = AlexandriaClient("http://127.0.0.1:1/mcp", timeout=1)
        self.addCleanup(client.close)
        with self.assertRaises(AlexandriaError):
            client.call_tool("store_memory", {"content": "x"})


if __name__ == "__main__":
    unittest.main(verbosity=2)
