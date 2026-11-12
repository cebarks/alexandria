"""Host-shaped regressions from the independent whole-feature review."""
import json
import unittest

from _bootstrap import bootstrap_hermes
bootstrap_hermes()
from alexandria import AlexandriaMemoryProvider
from test_contracts import RecordingClient


def result(call_id, name, payload, **args):
    return [
        {'role': 'assistant', 'tool_calls': [{'id': call_id, 'function': {
            'name': name, 'arguments': json.dumps(args)}}]},
        {'role': 'tool', 'tool_call_id': call_id, 'content': json.dumps(payload)},
    ]


class ReviewRegressions(unittest.TestCase):
    def setUp(self):
        self.client = RecordingClient()
        self.provider = AlexandriaMemoryProvider(config={'extract': False}, client_factory=lambda *_: self.client)
        self.provider.initialize('session')
        self.addCleanup(self.provider.shutdown)

    def lessons(self, messages):
        self.provider.sync_turn('run the deployment', 'ok', messages=messages)
        self.provider.on_session_end([])
        return [a['content'] for n, a in self.client.calls if n == 'store_memory']

    def test_non_primary_prefetch_never_claims_reminders(self):
        for context in ('cron', 'flush', 'subagent'):
            with self.subTest(context=context):
                self.provider._agent_context = context
                self.client.calls.clear()
                self.provider.prefetch('any reminders due')
                self.assertNotIn('check_reminders', [n for n, _ in self.client.calls])
        self.provider.handle_tool_call('alexandria_check_reminders', {})
        self.assertIn('check_reminders', [n for n, _ in self.client.calls])

    def test_terminal_nonzero_then_zero_captures_real_resolution(self):
        messages = result('bad', 'terminal', {'output': 'Error: connection refused by the production service', 'exit_code': 1, 'error': None})
        messages += result('good', 'terminal', {'output': 'Successfully deployed after fixing the service port', 'exit_code': 0, 'error': None})
        lessons = self.lessons(messages)
        self.assertEqual(len(lessons), 1)
        self.assertIn('connection refused', lessons[0])
        self.assertIn('Successfully deployed', lessons[0])

    def test_terminal_still_failing_cannot_be_a_resolution(self):
        messages = result('bad', 'terminal', {'error': 'Error: connection refused by the production service'})
        messages += result('worse', 'terminal', {'output': 'Error: permission denied while deploying the service', 'exit_code': 1, 'error': None})
        self.assertEqual(self.lessons(messages), [])

    def test_informational_exit_is_neither_error_nor_resolution(self):
        messages = result('bad', 'terminal', {'error': 'Error: connection refused by the production service'})
        messages += result('info', 'terminal', {'output': 'Error: words in an expected diff are not failures', 'exit_code': 1, 'error': None,
                                                 'exit_code_meaning': 'Files differ (expected, not an error)'})
        st = self.provider._state('session')
        self.provider._handle_turn('session', 'hello', messages)
        self.assertEqual(len(st.errors._errors), 1)
        self.assertEqual(st.errors.pending_resolutions(), [])

    def test_successful_memory_tool_both_deduplicates_and_resolves(self):
        messages = result('bad', 'alexandria_store_memory', {'error': 'Error: required content field was missing from the memory API'})
        messages += result('good', 'alexandria_store_memory', {'id': 'fact:ok'}, content='confirmed fact')
        lessons = self.lessons(messages)
        self.assertEqual(len(lessons), 1)
        self.assertIn('required content', lessons[0])

    def test_delivery_preserves_note_and_saturated_missed_count(self):
        self.client.deliveries = [{'message': 'Check deployment', 'note': 'Use the runbook at /ops/recovery',
                                  'target': 'project:ops', 'recurring': True, 'escalated': True,
                                  'missed_occurrences': 10000, 'missed_occurrences_saturated': True}]
        text = self.provider.prefetch('any reminders due')
        self.assertIn('Use the runbook at /ops/recovery', text)
        self.assertIn('escalated', text)
        self.assertIn('10000+', text)
        self.assertNotIn('Check deployment', self.provider.prefetch('any reminders due'))
