"""Deterministic lifecycle ordering: events, not sleeps, establish the race."""
import json
import threading
import unittest
from unittest.mock import patch

from _bootstrap import bootstrap_hermes
bootstrap_hermes()
from alexandria import AlexandriaMemoryProvider
from alexandria.client import AlexandriaError
from test_capture import tool_messages
from test_contracts import RecordingClient


class LifecycleContracts(unittest.TestCase):
    def setUp(self):
        self.client = RecordingClient()
        self.client.closed = False
        self.client.close = lambda: setattr(self.client, 'closed', True)
        self.provider = AlexandriaMemoryProvider(config={'timeout': 0.02, 'extract': False},
                                                client_factory=lambda *_: self.client)
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

    def test_omitted_session_is_captured_before_queueing(self):
        release = self.pause_worker()
        self.provider.sync_turn('Always run the complete suite.', 'ok')
        self.provider.on_session_switch('new')
        release.set()
        self.drain()
        self.assertEqual(self.stores()[0]['session_id'], 'old')

    def test_background_prefetch_session_does_not_drift(self):
        release = self.pause_worker()
        self.provider.queue_prefetch('our deployment policy')
        self.provider.on_session_switch('new')
        release.set()
        self.drain()
        self.assertIn('old', self.provider._recall_cache)
        self.assertNotIn('new', self.provider._recall_cache)

    def test_end_waits_for_queued_final_turn(self):
        release = self.pause_worker()
        messages = tool_messages('err', True) + tool_messages('ok')
        self.provider.sync_turn('deploy please', 'ok', messages=messages)
        self.provider.on_session_end([])  # bounded wait expires while worker paused
        release.set()
        self.drain()
        self.assertTrue(any('Resolution:' in a['content'] for a in self.stores()))

    def test_rewind_processes_new_tool_events(self):
        old = [{'role': 'user', 'content': 'old'}] * 20
        self.provider.sync_turn('hello', 'ok', messages=old)
        self.drain()
        self.provider.on_session_switch('old', rewound=True)
        messages = tool_messages('new-error', True) + tool_messages('new-success')
        self.provider.sync_turn('hello', 'ok', messages=messages)
        self.provider.on_session_end([])
        self.assertTrue(any('Resolution:' in a['content'] for a in self.stores()))

    def test_compression_processes_shorter_transcript(self):
        self.provider._handle_turn('old', 'hello', [{'role': 'user', 'content': 'old'}] * 20)
        self.provider._handle_turn('old', 'hello', tool_messages('new-error', True) + tool_messages('new-success'))
        self.provider.on_session_end([])
        self.assertTrue(any('Resolution:' in a['content'] for a in self.stores()))

    def test_duplicate_snapshot_does_not_duplicate_tracking(self):
        st = self.provider._state('old')
        with patch.object(st.errors, 'record_success', wraps=st.errors.record_success) as success:
            messages = tool_messages('success')
            self.provider._handle_turn('old', 'hello', messages)
            self.provider._handle_turn('old', 'hello', messages)
            self.assertEqual(success.call_count, 1)

    def test_failed_error_flush_remains_pending(self):
        self.provider._handle_turn('old', 'hello', tool_messages('error', True) + tool_messages('success'))
        original = self.client.call_tool
        self.client.call_tool = lambda *_: (_ for _ in ()).throw(AlexandriaError('offline'))
        self.provider.on_session_end([])
        self.client.call_tool = original
        self.provider.on_session_end([])
        self.assertTrue(any('Resolution:' in a['content'] for a in self.stores()))

    def test_no_finalize_without_linked_success(self):
        self.provider._cfg['extract'] = True
        original = self.client.call_tool
        def failed_store(name, args):
            if name == 'store_memory':
                raise AlexandriaError('offline')
            return original(name, args)
        self.client.call_tool = failed_store
        with patch('alexandria.run_extraction', return_value=[{'content': 'fact', 'tags': []}]):
            self.provider.on_session_end([])
        self.assertNotIn('finalize_session', [n for n, _ in self.client.calls])

    def test_store_session_false_skips_finalization(self):
        self.provider._cfg.update(extract=True, store_session=False)
        with patch('alexandria.run_extraction', return_value=[{'content': 'fact', 'tags': []}]):
            self.provider.on_session_end([])
        self.assertNotIn('finalize_session', [n for n, _ in self.client.calls])

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
