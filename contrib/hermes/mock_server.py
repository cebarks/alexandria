"""Hermetic MCP protocol double, not an embedding or scheduling reference.

Request fields and response shapes follow the Rust MCP tools. Ranking is simple
word overlap; reminder due times are UTC ISO strings (no recurring calendar engine).
Usage: python mock_server.py [port].
"""
from __future__ import annotations

import json
import re
import threading
import uuid
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

NAMES = ('store_memory', 'retrieve_memories', 'recall', 'update_memory', 'delete_memory',
         'import_document', 'get_session', 'list_sessions', 'finalize_session',
         'set_reminder', 'check_reminders', 'list_reminders', 'cancel_reminder')
TOOLS = [{'name': name, 'description': name, 'inputSchema': {'type': 'object'}} for name in NAMES]
REQUIRED = {'store_memory': ('content',), 'retrieve_memories': ('query',), 'recall': ('query',),
            'update_memory': ('id',), 'delete_memory': ('id',), 'set_reminder': ('message',),
            'cancel_reminder': ('id',), 'get_session': ('session_id',), 'finalize_session': ('session_id',)}


class MockAlexandria:
    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.memories: list[dict] = []
        self.sessions: dict[str, list[str]] = {}
        self.reminders: list[dict] = []
        self.calls: list[tuple[str, dict]] = []

    def handle(self, name: str, args: dict) -> dict:
        with self.lock:
            self.calls.append((name, dict(args)))
            for key in REQUIRED.get(name, ()):
                if key not in args:
                    raise ValueError(f'missing required field {key}')
            return self._dispatch(name, args)

    def _retrieve(self, args: dict) -> dict:
        query_words = set(re.findall(r'\w+', args['query'].lower()))
        scored = []
        for mem in self.memories:
            if mem.get('deleted') or ('session_id' in args and mem.get('session_id') != args['session_id']):
                continue
            words = set(re.findall(r'\w+', mem['content'].lower()))
            score = len(query_words & words) / max(len(query_words), 1)
            scored.append({'id': mem['id'], 'content': mem['content'], 'similarity': round(score, 3)})
        scored.sort(key=lambda m: m['similarity'], reverse=True)
        return {'results': scored[:int(args.get('limit', 5))], 'due_reminders': []}

    def _dispatch(self, name: str, args: dict) -> dict:
        if name == 'store_memory':
            mem = {'id': 'fact:' + uuid.uuid4().hex[:8], 'content': args['content'],
                   'tags': args.get('tags', []), 'session_id': args.get('session_id')}
            self.memories.append(mem)
            if mem['session_id']:
                self.sessions.setdefault(mem['session_id'], []).append(mem['id'])
            return {'id': mem['id'], 'truncated': False}
        if name == 'retrieve_memories':
            return self._retrieve(args)
        if name == 'recall':
            return {'scope_handle': 'mock-scope', 'results': self._retrieve(args)['results']}
        if name in ('update_memory', 'delete_memory'):
            mem = next((m for m in self.memories if m['id'] == args['id']), None)
            if mem is None:
                raise ValueError('memory not found')
            if name == 'delete_memory':
                mem['deleted'] = True
            else:
                mem.update({k: v for k, v in args.items() if k in ('content', 'tags', 'confidence')})
            return {'id': mem['id'], 'status': 'ok'}
        if name == 'get_session':
            return {'session_id': args['session_id'],
                    'memories': [m for m in self.memories if m.get('session_id') == args['session_id'] and not m.get('deleted')]}
        if name == 'finalize_session':
            if args['session_id'] not in self.sessions:
                raise ValueError('session not found')
            return {'finalized': args['session_id']}
        if name == 'set_reminder':
            if sum(k in args for k in ('due_at', 'pattern', 'cron')) != 1:
                raise ValueError('supply exactly one schedule')
            rem = {'id': 'reminder:' + uuid.uuid4().hex[:8], 'message': args['message'],
                   'status': 'pending', 'next_due_at': args.get('due_at'),
                   'target_project': args.get('target_project'), 'recurring': 'due_at' not in args}
            self.reminders.append(rem)
            return {'id': rem['id'], 'next_due_at': rem['next_due_at'], 'next_fire_preview': []}
        if name == 'check_reminders':
            due = []
            for rem in self.reminders:
                target = rem.get('target_project')
                date = rem.get('next_due_at')
                if rem['status'] != 'pending' or not date or (target and target != args.get('project')):
                    continue
                if datetime.fromisoformat(date.replace('Z', '+00:00')) > datetime.now(timezone.utc):
                    continue
                rem['status'] = 'delivered'
                due.append({'id': rem['id'], 'message': rem['message'], 'recurring': rem['recurring'],
                            'target': 'project:' + target if target else 'global'})
            return {'count': len(due), 'delivered': due}
        if name == 'list_reminders':
            status = args.get('status', 'pending')
            rows = [r for r in self.reminders if status == 'all' or r['status'] == status]
            if 'target_project' in args:
                rows = [r for r in rows if r.get('target_project') == args['target_project']]
            start, limit = int(args.get('offset', 0)), int(args.get('limit', 50))
            return {'reminders': rows[start:start + limit], 'truncated': start + limit < len(rows)}
        if name == 'cancel_reminder':
            rem = next((r for r in self.reminders if r['id'] == args['id']), None)
            if rem is None:
                raise ValueError('reminder not found')
            rem['status'] = 'cancelled'
            return {'id': rem['id'], 'status': 'cancelled'}
        if name == 'list_sessions':
            return {'sessions': list(self.sessions)}
        raise NotImplementedError(f'mock alexandria does not implement {name}')


class Handler(BaseHTTPRequestHandler):
    mock: MockAlexandria
    sessions: set[str]

    def log_message(self, format: str, *args: object) -> None:
        pass

    def _send(self, code: int, body: bytes = b'', ctype: str = 'application/json', extra: dict | None = None) -> None:
        self.send_response(code)
        self.send_header('Content-Type', ctype)
        self.send_header('Content-Length', str(len(body)))
        for k, v in (extra or {}).items():
            self.send_header(k, v)
        self.end_headers()
        if body:
            self.wfile.write(body)

    def do_DELETE(self) -> None:
        self.sessions.discard(self.headers.get('Mcp-Session-Id', ''))
        self._send(204)

    def do_POST(self) -> None:
        try:
            msg = json.loads(self.rfile.read(int(self.headers.get('Content-Length', 0))))
        except (json.JSONDecodeError, ValueError):
            self._send(400)
            return
        method = msg.get('method')
        if method == 'initialize':
            sid = str(uuid.uuid4())
            self.sessions.add(sid)
            result = {'protocolVersion': '2025-06-18', 'capabilities': {'tools': {}},
                      'serverInfo': {'name': 'mock-alexandria', 'version': '0'}}
            self._send(200, json.dumps({'jsonrpc': '2.0', 'id': msg.get('id'), 'result': result}).encode(),
                       extra={'Mcp-Session-Id': sid})
            return
        if self.headers.get('Mcp-Session-Id') not in self.sessions:
            self._send(404)
            return
        if method == 'notifications/initialized':
            self._send(202)
            return
        if method == 'tools/list':
            result = {'tools': TOOLS}
        elif method == 'tools/call':
            params = msg.get('params', {})
            try:
                payload = self.mock.handle(params.get('name', ''), params.get('arguments', {}))
                result = {'content': [{'type': 'text', 'text': json.dumps(payload)}], 'isError': False}
            except Exception as exc:
                result = {'content': [{'type': 'text', 'text': json.dumps({'error': str(exc)})}], 'isError': True}
        else:
            self._send(200, json.dumps({'jsonrpc': '2.0', 'id': msg.get('id'),
                                       'error': {'code': -32601, 'message': f'no method {method}'}}).encode())
            return
        self._send(200, json.dumps({'jsonrpc': '2.0', 'id': msg.get('id'), 'result': result}).encode())


def make_server(port: int = 0) -> tuple[ThreadingHTTPServer, MockAlexandria]:
    mock = MockAlexandria()
    handler = type('IsolatedHandler', (Handler,), {'mock': mock, 'sessions': set()})
    return ThreadingHTTPServer(('127.0.0.1', port), handler), mock


if __name__ == '__main__':
    import sys
    httpd, _ = make_server(int(sys.argv[1]) if len(sys.argv) > 1 else 3900)
    print(f'mock alexandria MCP on http://127.0.0.1:{httpd.server_address[1]}/mcp', flush=True)
    httpd.serve_forever()
