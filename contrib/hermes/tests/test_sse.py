"""HTTP SSE fixtures exercise the same client handshake and tools path as JSON."""
import json
import threading
import unittest
from http.server import ThreadingHTTPServer

from _bootstrap import bootstrap_hermes
bootstrap_hermes()
from alexandria.client import AlexandriaClient
from mock_server import Handler, MockAlexandria


class SseContracts(unittest.TestCase):
    def client(self, *, decoy=False, multiline=False):
        class SseHandler(Handler):
            mock = MockAlexandria()
            sessions = set()
            def _send(self, code, body=b'', ctype='application/json', extra=None):
                if code == 200 and body:
                    msg = json.loads(body)
                    frames = ['event: message\ndata: {"jsonrpc":"2.0","method":"notifications/progress"}\n\n']
                    if decoy:
                        frames.append('data: {"jsonrpc":"2.0","id":"unrelated","result":{}}\n\n')
                    data = json.dumps(msg, indent=2) if multiline else json.dumps(msg)
                    frames.append('event: message\n' + '\n'.join('data: ' + line for line in data.splitlines()) + '\n\n')
                    body, ctype = ''.join(frames).encode(), 'text/event-stream'
                super()._send(code, body, ctype, extra)
        server = ThreadingHTTPServer(('127.0.0.1', 0), SseHandler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        client = AlexandriaClient(f'http://127.0.0.1:{server.server_address[1]}/mcp', timeout=0.5)
        self.addCleanup(client.close)
        return client

    def test_sse_notifications_before_result(self):
        names = {t['name'] for t in self.client().list_tools()}
        self.assertIn('store_memory', names)

    def test_sse_matches_request_id(self):
        names = {t['name'] for t in self.client(decoy=True).list_tools()}
        self.assertIn('store_memory', names)

    def test_sse_multiline_data_event(self):
        try:
            tools = self.client(multiline=True).list_tools()
        except Exception as exc:
            self.fail(f'valid multiline SSE reply must parse: {exc}')
        self.assertIn('store_memory', {t['name'] for t in tools})
