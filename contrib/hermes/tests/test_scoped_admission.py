"""Contracts for explicit Hermes writes and authenticated MCP transport."""

import json
import os
import unittest
from unittest.mock import patch

import httpx

from _bootstrap import bootstrap_hermes

bootstrap_hermes()

from alexandria import AlexandriaMemoryProvider  # noqa: E402
from alexandria.client import AlexandriaClient, AlexandriaError  # noqa: E402


TOKEN = "hermes-unit-test-token"
PROJECT = "project:hermes-fixture"
ENDPOINT = "https://alexandria.example/mcp"


class TransportAdmission(unittest.TestCase):
    def mock_client(self, *, reject_tools=False):
        requests = []

        def respond(req):
            requests.append(req)
            if req.method == "DELETE":
                return httpx.Response(204)
            message = json.loads(req.content)
            if "id" not in message:
                return httpx.Response(202)
            if reject_tools and message["method"] == "tools/list":
                return httpx.Response(401, text=f"private fixture diagnostic: {TOKEN}")
            return httpx.Response(
                200,
                headers={"Mcp-Session-Id": "fixture-session"},
                json={"jsonrpc": "2.0", "id": message["id"], "result": {}},
            )

        client = httpx.Client(transport=httpx.MockTransport(respond))
        self.addCleanup(client.close)
        return client, requests

    def test_bearer_is_present_during_initialization_call_and_close(self):
        http, requests = self.mock_client()
        with patch.dict(os.environ, {"ALEXANDRIA_API_TOKEN": TOKEN}), patch(
            "alexandria.client.httpx.Client", return_value=http
        ):
            client = AlexandriaClient(ENDPOINT)
            try:
                client.list_tools()
            finally:
                client.close()

        self.assertEqual([req.method for req in requests], ["POST", "POST", "POST", "DELETE"])
        self.assertEqual([req.headers.get("Authorization") for req in requests], [f"Bearer {TOKEN}"] * 4)

    def test_missing_bearer_makes_no_mcp_request(self):
        http, requests = self.mock_client()
        with patch.dict(os.environ, {"ALEXANDRIA_API_TOKEN": ""}), patch(
            "alexandria.client.httpx.Client", return_value=http
        ):
            with self.assertRaises(AlexandriaError):
                client = AlexandriaClient(ENDPOINT)
                try:
                    client.list_tools()
                finally:
                    client.close()
        self.assertEqual(requests, [])

    def test_remote_cleartext_makes_no_mcp_request(self):
        http, requests = self.mock_client()
        with patch.dict(os.environ, {"ALEXANDRIA_API_TOKEN": TOKEN}), patch(
            "alexandria.client.httpx.Client", return_value=http
        ):
            with self.assertRaises(AlexandriaError):
                client = AlexandriaClient("http://alexandria.example/mcp")
                try:
                    client.list_tools()
                finally:
                    client.close()
        self.assertEqual(requests, [])

    def test_rejected_bearer_is_not_retried_or_echoed(self):
        http, requests = self.mock_client(reject_tools=True)
        with patch.dict(os.environ, {"ALEXANDRIA_API_TOKEN": TOKEN}), patch(
            "alexandria.client.httpx.Client", return_value=http
        ):
            client = AlexandriaClient(ENDPOINT)
            try:
                with self.assertRaises(AlexandriaError) as denied:
                    client.list_tools()
            finally:
                client.close()
        self.assertIn("401", str(denied.exception))
        self.assertNotIn(TOKEN, str(denied.exception))
        self.assertEqual([r.method for r in requests], ["POST", "POST", "POST", "DELETE"])


class WriteAdmission(unittest.TestCase):
    def provider(self):
        calls = []

        class Recorder:
            def call_tool(self, name, args):
                calls.append((name, dict(args)))
                if name == "check_reminders":
                    return {"delivered": []}
                if name == "retrieve_memories":
                    return {"results": [{"content": "project fixture fact", "similarity": 0.99}]}
                return {"id": "fact:manual-fixture"}

            def close(self):
                pass

        provider = AlexandriaMemoryProvider(
            config={"url": ENDPOINT, "auto_store": True, "extract": True, "store_session": True},
            client_factory=lambda *_: Recorder(),
        )
        provider.initialize("session-fixture")
        self.addCleanup(provider.shutdown)
        return provider, calls

    def test_legacy_capture_flags_cannot_enable_automatic_writes(self):
        with patch.dict(os.environ, {"ALEXANDRIA_API_TOKEN": TOKEN, "ALEXANDRIA_PROJECT_ID": PROJECT}), patch(
            "alexandria.run_extraction", return_value=[{"content": "fixture extraction result"}], create=True
        ) as extraction:
            provider, calls = self.provider()
            provider.sync_turn("Actually, the fixture port is 9090.", "Got it.")
            provider.on_session_end([{"role": "user", "content": "fixture conversation"}])
            self.assertEqual(calls, [])
            extraction.assert_not_called()

    def test_explicit_store_uses_the_configured_project(self):
        with patch.dict(os.environ, {"ALEXANDRIA_API_TOKEN": TOKEN, "ALEXANDRIA_PROJECT_ID": PROJECT}):
            provider, calls = self.provider()
            reply = json.loads(provider.handle_tool_call("alexandria_store_memory", {"content": "fixture decision"}))
        self.assertEqual(reply, {"result": {"id": "fact:manual-fixture"}})
        self.assertEqual(calls, [("store_memory", {
            "content": "fixture decision", "tags": [], "scope": "project", "project": PROJECT,
        })])

    def test_project_is_used_for_recall_and_foreground_reminders(self):
        with patch.dict(os.environ, {"ALEXANDRIA_API_TOKEN": TOKEN, "ALEXANDRIA_PROJECT_ID": PROJECT}):
            provider, calls = self.provider()
            prompt = "Which operational settings should we use for this fixture?"
            self.assertIn("project fixture fact", provider.prefetch(prompt))
        self.assertEqual(calls, [
            ("check_reminders", {"project": PROJECT}),
            ("retrieve_memories", {"query": prompt, "limit": 5, "scope": "auto", "project": PROJECT}),
        ])

    def test_foreground_recall_never_replays_cache_after_a_rejection(self):
        denied = False

        class Recorder:
            def call_tool(self, name, args):
                if denied:
                    raise AlexandriaError("HTTP 401")
                if name == "check_reminders":
                    return {"delivered": []}
                return {"results": [{"content": "private fixture memory", "similarity": 0.95}]}

            def close(self):
                pass

        with patch.dict(os.environ, {"ALEXANDRIA_API_TOKEN": TOKEN, "ALEXANDRIA_PROJECT_ID": PROJECT}):
            provider = AlexandriaMemoryProvider(config={"url": ENDPOINT}, client_factory=lambda *_: Recorder())
            provider.initialize("session-fixture")
            self.addCleanup(provider.shutdown)
            prompt = "Which operational settings should we use for this fixture?"
            self.assertIn("private fixture memory", provider.prefetch(prompt))
            denied = True
            self.assertEqual(provider.prefetch(prompt), "")
