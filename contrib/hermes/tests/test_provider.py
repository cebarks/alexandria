"""Provider lifecycle tests against the mock Alexandria server, with a fake LLM for
the extraction pass. Covers: recall injection, heuristic capture via sync_turn,
tool-event tracking, error->resolution pairing, session end, tool dispatch, and the
non-primary context write gate."""

import json
import tempfile
import threading
import time
import unittest
from _bootstrap import bootstrap_hermes

bootstrap_hermes()

from alexandria import AlexandriaMemoryProvider  # noqa: E402
from alexandria.client import AlexandriaClient  # noqa: E402
from mock_server import make_server  # noqa: E402


class FakeLlm:
    """PluginLlm stand-in returning canned structured output."""

    def __init__(self, memories):
        self._memories = memories
        self.calls = []

    def complete_structured(self, **kwargs):
        self.calls.append(kwargs)

        class R:
            def __init__(self, parsed):
                self.parsed = parsed
                self.text = json.dumps(parsed)
        return R({"memories": self._memories})


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
        self.home = tempfile.mkdtemp(prefix="alex-home-")
        self.llm = FakeLlm([{"content": "The staging box is named den-staging-02.", "tags": ["infra"]}])
        self.provider = AlexandriaMemoryProvider(
            config={"url": self.url, "timeout": 5.0, "min_similarity": 0.05},
            client_factory=lambda url, timeout: AlexandriaClient(url, timeout=timeout),
            llm=self.llm,
        )
        self.provider.initialize("sess-1", hermes_home=self.home, platform="cli", agent_context="primary")
        self.addCleanup(self.provider.shutdown)

    def wait_for_store(self, needle, timeout=3.0):
        deadline = time.time() + timeout
        while time.time() < deadline:
            if any(needle in m["content"] for m in self.mock.memories):
                return True
            time.sleep(0.02)
        return False

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

    def test_prefetch_failure_falls_back_to_cache(self):
        self.provider._get_client().call_tool("store_memory", {"content": "cached fact about kafka"})
        first = self.provider.prefetch("kafka", session_id="sess-1")
        self.assertIn("kafka", first)
        # Break the client; next prefetch (cache expired) must return the cached text.
        self.provider._recall_cache["sess-1"] = (0.0, first, 1)
        self.provider._client.close()
        self.provider._client = AlexandriaClient("http://127.0.0.1:1/mcp", timeout=1)
        self.provider._recall_cache["sess-1"] = (0.0, first, 1)
        again = self.provider.prefetch("kafka again", session_id="sess-1")
        self.assertEqual(again, first)

    # -- capture ----------------------------------------------------------------

    def test_sync_turn_stores_detected_correction(self):
        messages = [
            {"role": "user", "content": "Actually, the deploy target is den-prod-01."},
            {"role": "assistant", "content": "Got it."},
        ]
        self.provider.sync_turn("Actually, the deploy target is den-prod-01.", "Got it.",
                                session_id="sess-1", messages=messages)
        self.assertTrue(self.wait_for_store("User correction: the deploy target is den-prod-01"),
                        f"stored: {self.stored_contents()}")

    def test_sync_turn_records_agent_store_into_dedup_buffer(self):
        messages = [
            {"role": "assistant", "content": None, "tool_calls": [
                {"id": "c1", "function": {"name": "alexandria_store_memory",
                                          "arguments": json.dumps({"content": "agent stored fact"})}}]},
            {"role": "tool", "tool_call_id": "c1", "content": json.dumps({"result": {"id": "x"}})},
        ]
        self.provider.sync_turn("hello there", "I stored it.", session_id="sess-1", messages=messages)
        time.sleep(0.2)  # let the ordered worker drain
        st = self.provider._state("sess-1")
        self.assertIn("agent stored fact", st.buffer.get_all_stored_contents())

    def test_error_then_success_pairs_at_session_end(self):
        err_msg = json.dumps({"error": "Error: connection refused while fetching the pod manifest from cluster"})
        messages = [
            {"role": "assistant", "content": None, "tool_calls": [
                {"id": "c1", "function": {"name": "terminal", "arguments": "{}"}}]},
            {"role": "tool", "tool_call_id": "c1", "content": err_msg},
        ]
        self.provider.sync_turn("deploy it please", "failed", session_id="sess-1", messages=messages)
        success = [
            {"role": "assistant", "content": None, "tool_calls": [
                {"id": "c2", "function": {"name": "terminal", "arguments": "{}"}}]},
            {"role": "tool", "tool_call_id": "c2", "content": json.dumps({"result": "manifest applied, rollout ok"})},
        ]
        self.provider.sync_turn("try again", "done", session_id="sess-1", messages=messages + success)
        time.sleep(0.2)
        self.provider.on_session_end([{"role": "user", "content": "x" * 150}])
        contents = self.stored_contents()
        self.assertTrue(any("Error with terminal:" in c and "Resolution:" in c for c in contents), contents)

    def test_session_end_runs_extraction_and_finalizes(self):
        long_conv = [
            {"role": "user", "content": "How do we deploy staging? " * 5},
            {"role": "assistant", "content": "We deploy staging with just deploy. " * 5},
        ]
        self.provider.on_session_end(long_conv)
        contents = self.stored_contents()
        self.assertTrue(any("den-staging-02" in c for c in contents), contents)
        stored = [c for c in self.mock.calls if c[0] == "store_memory"]
        self.assertTrue(any("extracted" in (a.get("tags") or []) for _, a in stored))
        self.assertTrue(any(c[0] == "finalize_session" for c in self.mock.calls))

    def test_non_primary_context_skips_auto_writes(self):
        self.provider._agent_context = "subagent"
        messages = [{"role": "user", "content": "Actually, the port is 9090."}]
        self.provider.sync_turn("Actually, the port is 9090.", "ok", session_id="sess-2", messages=messages)
        time.sleep(0.2)
        self.assertFalse(any("User correction" in c for c in self.stored_contents()))

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

    def test_session_switch_reset_clears_state(self):
        self.provider.sync_turn("Always run just lint.", "ok", session_id="sess-3",
                                messages=[{"role": "user", "content": "Always run just lint."}])
        time.sleep(0.2)
        self.assertIn("sess-3", self.provider._states)
        self.provider.on_session_switch("sess-3", reset=True)
        self.assertNotIn("sess-3", self.provider._states)


if __name__ == "__main__":
    unittest.main(verbosity=2)
