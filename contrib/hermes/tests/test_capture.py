"""Capture opt-outs, confirmed persistence and mirror removal regressions."""
import json
import sys
import types
import unittest
from unittest.mock import patch

from _bootstrap import bootstrap_hermes
bootstrap_hermes()
from alexandria import AlexandriaMemoryProvider
from alexandria.client import AlexandriaError
from alexandria.detectors import SessionDedupBuffer, track_tool_store
from test_contracts import RecordingClient


def tool_messages(call_id, error=False):
    content = {'error': 'Connection refused while reading deployment manifest from the production cluster'} if error else {'result': 'Fixed service route, deployment succeeded'}
    return [{'role': 'assistant', 'tool_calls': [{'id': call_id, 'function': {'name': 'terminal', 'arguments': '{}'}}]},
            {'role': 'tool', 'tool_call_id': call_id, 'content': json.dumps(content)}]


class CaptureContracts(unittest.TestCase):
    def provider(self, **cfg):
        client = RecordingClient()
        provider = AlexandriaMemoryProvider(config=cfg, client_factory=lambda *_: client)
        provider.initialize('session')
        self.addCleanup(provider.shutdown)
        return provider, client

    def test_setup_false_round_trips_to_boolean(self):
        captured = {}
        host = types.ModuleType('hermes_cli.config')
        host.load_config = lambda: captured
        host.save_config = lambda *args, **kwargs: None
        parent = types.ModuleType('hermes_cli')
        with patch.dict(sys.modules, {'hermes_cli': parent, 'hermes_cli.config': host}):
            p, _ = self.provider()
            p.save_config({'auto_store': 'false', 'extract': 'false'}, '/unused')
        self.assertIs(captured['memory']['alexandria']['auto_store'], False)
        p, _ = self.provider(**captured['memory']['alexandria'])
        self.assertIs(p._cfg['extract'], False)
        fields = {s['key']: s for s in p.get_config_schema()}
        self.assertEqual(fields['auto_store']['type'], 'boolean')

    def test_malformed_boolean_is_rejected(self):
        for key in ('auto_store', 'extract', 'store_session'):
            with self.subTest(key=key), self.assertRaises(ValueError):
                self.provider(**{key: 'perhaps'})

    def test_auto_store_false_blocks_error_pairs(self):
        for value in (False, 'false'):
            with self.subTest(value=value):
                p, c = self.provider(auto_store=value, extract=False)
                messages = tool_messages('err', True) + tool_messages('ok')
                p._handle_turn('session', 'please deploy', messages)
                p.on_session_end([])
                self.assertFalse([a for n, a in c.calls if n == 'store_memory'])

    def test_extract_flag_is_independent(self):
        p, c = self.provider(auto_store=False, extract=True)
        with patch('alexandria.run_extraction', return_value=[{'content': 'extracted fact', 'tags': []}]) as extract:
            p.on_session_end([{'role': 'user', 'content': 'details'}])
        extract.assert_called_once()
        self.assertTrue([a for n, a in c.calls if n == 'store_memory'])

    def test_explicit_store_works_with_auto_capture_disabled(self):
        p, c = self.provider(auto_store=False, extract=False)
        self.assertIn('result', json.loads(p.handle_tool_call('alexandria_store_memory', {'content': 'explicit fact'})))
        self.assertEqual(c.calls[-1][0], 'store_memory')

    def test_failed_store_releases_dedup_reservation(self):
        p, c = self.provider()
        original = c.call_tool
        failed = True
        def call(name, args):
            if name == 'store_memory' and failed:
                raise AlexandriaError('offline')
            return original(name, args)
        c.call_tool = call
        text = 'Always run the complete regression suite.'
        p._handle_turn('session', text, [])
        self.assertEqual(p._state('session').buffer.get_all_stored_contents(), [])
        failed = False
        p._handle_turn('session', text, [])
        self.assertEqual(len([a for n, a in c.calls if n == 'store_memory']), 1)

    def test_failed_fact_not_in_extraction_exclusions(self):
        p, c = self.provider()
        c.call_tool = lambda *_: (_ for _ in ()).throw(AlexandriaError('offline'))
        p._handle_turn('session', 'Always run the regression suite.', [])
        with patch('alexandria.run_extraction', return_value=[]) as extract:
            p.on_session_end([])
        self.assertEqual(extract.call_args.args[1], [])

    def test_foreign_store_does_not_populate_local_dedup(self):
        for name in ('mcp__other__store_memory', 'mcp_other_store_memory', 'other_update_memory', 'store_memory'):
            buffer = SessionDedupBuffer()
            self.assertFalse(track_tool_store(name, 'not in Alexandria', False, buffer), name)
            self.assertEqual(buffer.get_all_stored_contents(), [])

    def test_builtin_memory_write_has_no_alexandria_side_effect(self):
        p, c = self.provider()
        for action in ('add', 'replace', 'remove'):
            p.on_memory_write(action, 'user', 'local entry')
        self.assertEqual(c.calls, [])
