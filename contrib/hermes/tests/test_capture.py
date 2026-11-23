"""Settings and explicit-only persistence regressions."""
import json
import os
import sys
import types
import unittest
from unittest.mock import patch

from _bootstrap import bootstrap_hermes
bootstrap_hermes()
from alexandria import AlexandriaMemoryProvider
from alexandria.client import AlexandriaError
from alexandria.detectors import SessionDedupBuffer, track_tool_store
from mock_server import MOCK_API_TOKEN
from test_contracts import RecordingClient


class CaptureContracts(unittest.TestCase):
    def provider(self, **cfg):
        client = RecordingClient()
        with patch.dict(os.environ, {'ALEXANDRIA_API_TOKEN': MOCK_API_TOKEN, 'ALEXANDRIA_PROJECT_ID': ''}):
            provider = AlexandriaMemoryProvider(config=cfg, client_factory=lambda *_: client)
        provider.initialize('session')
        self.addCleanup(provider.shutdown)
        return provider, client

    def test_setup_does_not_save_credentials_or_retired_capture_flags(self):
        captured = {}
        host = types.ModuleType('hermes_cli.config')
        host.load_config = lambda: captured
        host.save_config = lambda *args, **kwargs: None
        parent = types.ModuleType('hermes_cli')
        with patch.dict(sys.modules, {'hermes_cli': parent, 'hermes_cli.config': host}):
            p, _ = self.provider()
            url = 'https://alexandria.example/mcp'
            p.save_config({'url': url, 'auto_store': True, 'extract': True, 'api_token': MOCK_API_TOKEN}, '/unused')
        self.assertEqual(captured['memory']['alexandria'], {'url': url})
        self.assertEqual({field['key'] for field in p.get_config_schema()}, {'url'})

    def test_legacy_capture_values_are_ignored(self):
        flags = {'auto_store': 'perhaps', 'extract': True, 'store_session': True}
        p, c = self.provider(**flags)
        self.assertTrue(flags.keys().isdisjoint(p._cfg))
        p.sync_turn('Always keep this test in memory.', 'okay')
        p.on_session_end([])
        self.assertEqual(c.calls, [])

    def test_yaml_cannot_supply_a_missing_environment_bearer(self):
        with patch.dict(os.environ, {'ALEXANDRIA_API_TOKEN': ''}), patch('alexandria.client.httpx.Client') as transport:
            p = AlexandriaMemoryProvider(config={'api_token': MOCK_API_TOKEN})
            p.initialize('session')
            self.addCleanup(p.shutdown)
            self.assertFalse(p.is_available())
            reply = json.loads(p.handle_tool_call('alexandria_store_memory', {'content': 'fixture fact'}))
            self.assertIn('ALEXANDRIA_API_TOKEN', reply['error'])
            transport.assert_not_called()

    def test_explicit_store_works_with_auto_capture_disabled(self):
        p, c = self.provider(auto_store=False, extract=False)
        self.assertIn('result', json.loads(p.handle_tool_call('alexandria_store_memory', {'content': 'explicit fact'})))
        self.assertEqual(c.calls[-1][0], 'store_memory')

    def test_session_end_does_not_retry_a_rejected_explicit_store(self):
        p, c = self.provider()
        args = {'content': 'Always run the complete regression suite.'}
        with patch.object(c, 'call_tool', side_effect=AlexandriaError('fixture refused')) as calls:
            self.assertIn('error', json.loads(p.handle_tool_call('alexandria_store_memory', args)))
            p.on_session_end([])
            calls.assert_called_once_with('store_memory', {**args, 'scope': 'personal', 'tags': []})
        self.assertIn('result', json.loads(p.handle_tool_call('alexandria_store_memory', args)))
        self.assertEqual(len(c.calls), 1)

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
