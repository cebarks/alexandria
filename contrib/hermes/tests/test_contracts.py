"""Real-shaped MCP payload regressions for provider behavior, not ranking."""
import json
import threading
import unittest
from _bootstrap import bootstrap_hermes

bootstrap_hermes()
from alexandria import AlexandriaMemoryProvider
from alexandria.client import AlexandriaClient, AlexandriaToolError
from mock_server import make_server


class RecordingClient:
    def __init__(self):
        self.calls = []
        self.results = []
        self.deliveries = []
        self.sample = []

    def call_tool(self, name, args):
        self.calls.append((name, dict(args)))
        if name == 'retrieve_memories':
            return {'results': self.results, 'due_reminders': self.sample}
        if name == 'check_reminders':
            due, self.deliveries = self.deliveries, []
            return {'delivered': due, 'count': len(due)}
        return {'id': 'fact:test'}

    def close(self):
        pass


class RecallContracts(unittest.TestCase):
    def setUp(self):
        self.client = RecordingClient()
        self.provider = AlexandriaMemoryProvider(client_factory=lambda *_: self.client)
        self.provider.initialize('new-session')
        self.addCleanup(self.provider.shutdown)

    def barrier(self):
        done = threading.Event()
        self.provider._enqueue(done.set)
        self.assertTrue(done.wait(2))

    def test_auto_recall_includes_previous_session(self):
        self.provider.prefetch('our deployment policy')
        args = next(a for n, a in self.client.calls if n == 'retrieve_memories')
        self.assertNotIn('session_id', args)
        self.assertNotIn('min_similarity', args)

    def test_native_store_is_session_attributed(self):
        self.provider.handle_tool_call('alexandria_store_memory', {'content': 'fact'})
        self.assertEqual(self.client.calls[-1][1].get('session_id'), 'new-session')

    def test_client_floor_filters_low_similarity(self):
        self.client.results = [{'content': 'too weak', 'similarity': 0.1},
                               {'content': 'relevant', 'similarity': 0.9}]
        text = self.provider.prefetch('what is relevant')
        self.assertNotIn('too weak', text)
        self.assertIn('relevant', text)
        self.assertEqual(self.provider.recall_status().count, 1)

    def test_reminder_delivery_uses_message(self):
        self.client.deliveries = [{'message': 'Pay the bill', 'target': 'global', 'recurring': False}]
        self.assertIn('Pay the bill', self.provider.prefetch('any reminders due'))

    def test_background_refresh_never_consumes(self):
        self.client.deliveries = [{'message': 'Not yet delivered'}]
        self.provider.queue_prefetch('our deployment policy')
        self.barrier()
        self.assertNotIn('check_reminders', [n for n, _ in self.client.calls])
        self.assertTrue(self.client.deliveries)

    def test_cached_recall_checks_reminders_fresh(self):
        self.provider.prefetch('our deployment policy')
        self.client.deliveries = [{'message': 'Newly due'}]
        self.assertIn('Newly due', self.provider.prefetch('our deployment policy'))
        self.assertEqual(sum(n == 'retrieve_memories' for n, _ in self.client.calls), 1)

    def test_direct_trivial_prefetch_can_deliver_without_host_dispatch(self):
        # Hermes currently skips this hook on trivial prompts; this is provider-only.
        self.client.deliveries = [{'message': 'Still due'}]
        self.assertIn('Still due', self.provider.prefetch('thanks'))
        self.assertNotIn('retrieve_memories', [n for n, _ in self.client.calls])

    def test_consumed_reminder_is_not_replayed_from_cache(self):
        self.client.deliveries = [{'message': 'Only once'}]
        self.provider.prefetch('our deployment policy')
        self.assertNotIn('Only once', self.provider.prefetch('our deployment policy'))

    def test_retrieval_due_sample_is_not_a_delivery(self):
        self.client.sample = [{'message': 'Other project pending', 'target': 'project:elsewhere'}]
        self.assertNotIn('Other project pending', self.provider.prefetch('our deployment policy'))

    def test_native_tool_schemas_and_dispatch_cover_nine_tools(self):
        cases = {
            'store_memory': {'content': 'fact'},
            'search_memories': {'query': 'fact', 'limit': 2},
            'recall': {'query': 'fact', 'scope_handle': 'scope:1'},
            'update_memory': {'id': 'fact:1', 'content': 'new fact'},
            'delete_memory': {'id': 'fact:1'},
            'set_reminder': {'message': 'task', 'due_at': '2027-01-01T00:00:00Z', 'target_project': 'repo'},
            'check_reminders': {'project': 'repo'},
            'list_reminders': {},
            'cancel_reminder': {'id': 'reminder:1'},
        }
        self.assertEqual({s['name'] for s in self.provider.get_tool_schemas()},
                         {'alexandria_' + n for n in cases})
        for name, args in cases.items():
            with self.subTest(tool=name):
                out = json.loads(self.provider.handle_tool_call('alexandria_' + name, args))
                self.assertNotIn('error', out)
                wire, sent = self.client.calls[-1]
                self.assertEqual(wire, 'retrieve_memories' if name == 'search_memories' else name)
                if name == 'set_reminder':
                    self.assertEqual(sent['message'], 'task')
                    self.assertEqual(sent['target_project'], 'repo')
                if name == 'list_reminders':
                    self.assertEqual(sent.get('status'), 'pending')
                if name == 'recall':
                    self.assertEqual(sent.get('scope_handle'), 'scope:1')


class MockContracts(unittest.TestCase):
    def setUp(self):
        self.server, self.mock = make_server()
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.client = AlexandriaClient(f'http://127.0.0.1:{self.server.server_address[1]}/mcp', timeout=0.3)
        self.addCleanup(self.client.close)
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)

    def test_recall_completes_over_http(self):
        self.client.call_tool('store_memory', {'content': 'deploy policy'})
        try:
            result = self.client.call_tool('recall', {'query': 'deploy policy'})
        except Exception as exc:
            self.fail(f'recall must complete, not fail or deadlock: {exc}')
        self.assertIsInstance(result, dict)

    def test_retrieve_respects_session_linkage(self):
        self.client.call_tool('store_memory', {'content': 'deploy old', 'session_id': 'old'})
        self.client.call_tool('store_memory', {'content': 'deploy new', 'session_id': 'new'})
        out = self.client.call_tool('retrieve_memories', {'query': 'deploy', 'session_id': 'new'})
        self.assertEqual([m['content'] for m in out['results']], ['deploy new'])

    def test_set_reminder_requires_message(self):
        with self.assertRaises(AlexandriaToolError):
            self.client.call_tool('set_reminder', {'content': 'wrong field'})

    def test_pending_reminder_listing_matches_server_contract(self):
        self.client.call_tool('set_reminder', {'message': 'task', 'due_at': '2027-01-01T00:00:00Z'})
        out = self.client.call_tool('list_reminders', {'status': 'pending'})
        self.assertIn('message', out['reminders'][0])
        self.assertEqual(out['reminders'][0]['message'], 'task')
        self.assertEqual(out['reminders'][0]['status'], 'pending')
