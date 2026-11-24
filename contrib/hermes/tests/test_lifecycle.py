"""Deterministic lifecycle ordering: events, not sleeps, establish the race."""
import json
import os
import threading
import unittest
from unittest.mock import patch

from _bootstrap import bootstrap_hermes
bootstrap_hermes()
from alexandria import AlexandriaMemoryProvider
from mock_server import MOCK_API_TOKEN
from test_contracts import RecordingClient


class LifecycleContracts(unittest.TestCase):
    def setUp(self):
        self.client = RecordingClient()
        self.client.closed = False
        self.client.close = lambda: setattr(self.client, 'closed', True)
        with patch.dict(os.environ, {'ALEXANDRIA_API_TOKEN': MOCK_API_TOKEN, 'ALEXANDRIA_PROJECT_ID': ''}):
            self.provider = AlexandriaMemoryProvider(config={'timeout': 0.02}, client_factory=lambda *_: self.client)
        self.provider.initialize('old')
        self.addCleanup(self.provider.shutdown)

    def drain(self):
        done = threading.Event()
        self.provider._enqueue(done.set)
        self.assertTrue(done.wait(2))

    def pause_worker(self):
        started, release = threading.Event(), threading.Event()
        self.addCleanup(release.set)
        self.provider._enqueue(lambda: (started.set(), release.wait(2)))
        self.assertTrue(started.wait(1))
        return release

    def stores(self):
        return [a for n, a in self.client.calls if n == 'store_memory']

    def test_accepted_explicit_write_may_finish_after_queued_deadline(self):
        release = self.pause_worker()
        reply = json.loads(self.provider.handle_tool_call('alexandria_store_memory', {'content': 'a deliberate fact'}))
        self.assertIn('outcome unknown', reply['error'])
        self.assertEqual(self.stores(), [])
        release.set()
        self.drain()
        self.assertEqual(self.stores(), [{'content': 'a deliberate fact', 'scope': 'personal', 'tags': []}])

    def test_background_prefetch_session_does_not_drift(self):
        release = self.pause_worker()
        self.provider.queue_prefetch('our deployment policy')
        self.provider.on_session_switch('new')
        release.set()
        self.drain()
        self.assertIn('old', self.provider._recall_cache)
        self.assertNotIn('new', self.provider._recall_cache)

    def test_session_end_invalidation_runs_after_queued_prefetch(self):
        release = self.pause_worker()
        self.provider.queue_prefetch('our deployment policy')
        self.provider.on_session_end([])  # bounded wait expires while worker paused
        release.set()
        self.drain()
        self.assertEqual([n for n, _ in self.client.calls], ['retrieve_memories'])
        self.assertNotIn('old', self.provider._recall_cache)

    def test_rewound_and_compressed_transcripts_do_not_send_session_writes(self):
        self.provider.sync_turn('hello', 'ok', messages=[{'role': 'user', 'content': 'old'}] * 20)
        self.provider.on_session_switch('old', rewound=True)
        self.provider.sync_turn('Always use the small fixture.', 'ok', messages=[])
        self.provider.on_session_end([{'role': 'user', 'content': 'local transcript'}])
        self.assertEqual(self.client.calls, [])

    def test_shutdown_does_not_close_active_client(self):
        self.provider._get_client()
        release = self.pause_worker()
        self.provider.shutdown()
        closed_while_active = self.client.closed
        release.set()
        self.provider._worker.join(2)
        self.assertFalse(closed_while_active)
        self.assertTrue(self.client.closed)

    def test_end_or_shutdown_from_worker_does_not_self_join(self):
        errors, done = [], threading.Event()
        def inside_worker():
            try:
                self.provider.on_session_end([])
                self.provider.shutdown()
            except Exception as exc:
                errors.append(exc)
            finally:
                done.set()
        self.provider._enqueue(inside_worker)
        self.assertTrue(done.wait(1))
        self.assertEqual(errors, [])
