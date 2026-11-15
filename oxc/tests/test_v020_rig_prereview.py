"""L6 rig pre-review fixes (PREREVIEW-l6-tiny.md defects 6/7): gap chain chat->chat with epoch
reset and upstream-error reset, reasoning_content and tool-call ids kept, tool messages counted by
identity after a compaction. Model-free (fake upstream on loopback)."""
import http.client
import http.server
import json
from pathlib import Path
import sys
import tempfile
import threading
import time
import unittest

ROOT = Path(__file__).resolve().parents[2]
sys.path[:0] = [str(ROOT / 'tools' / 'gates'), str(ROOT / 'oxc' / 'benchmarks' / 'tokens')]
import hop_probe as hp  # noqa: E402
import token_harness as th  # noqa: E402


class Upstream(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    fail = False

    def log_message(self, *a):
        return

    def _send(self, code, body):
        self.send_response(code)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        self._send(200, b'{"status":"ok"}')

    def do_POST(self):
        self.rfile.read(int(self.headers.get('Content-Length') or 0))
        if Upstream.fail:
            self._send(500, b'{"error":"boom"}')
        else:
            self._send(200, json.dumps({'choices': [{'message': {'content': 'hi', 'reasoning_content': 'think'}}]}).encode())


class GapChainTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix='hop-gap-')
        self.up = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Upstream)
        threading.Thread(target=self.up.serve_forever, daemon=True).start()
        self.proxy = http.server.ThreadingHTTPServer(('127.0.0.1', 0), hp.make_handler(hp.State(self.tmp, self.up.server_address[1], True)))
        threading.Thread(target=self.proxy.serve_forever, daemon=True).start()
        Upstream.fail = False
        self.completed = 0

    def tearDown(self):
        self.proxy.shutdown()
        self.up.shutdown()

    def call(self, method, path, body=None):
        c = http.client.HTTPConnection('127.0.0.1', self.proxy.server_address[1], timeout=10)
        c.request(method, path, body=json.dumps(body).encode() if body is not None else None)
        r = c.getresponse()
        r.read()
        c.close()
        if path != hp.EPOCH_PATH:
            # HTTP completion precedes asynchronous gap-state and ledger updates.
            self.completed += 1
            self.assertEqual(len(self.recs(self.completed)), self.completed)
        return r.status

    def recs(self, n):
        p = Path(self.tmp) / 'requests.jsonl'
        for _ in range(200):
            if p.exists() and p.read_text().count('\n') >= n:
                break
            time.sleep(0.02)
        return [json.loads(x) for x in p.read_text().splitlines()]

    def chat(self):
        return self.call('POST', '/v1/chat/completions', {'messages': []})

    def test_gap_only_between_chats_and_reset_by_epoch_and_errors(self):
        self.chat()                                   # 1 chat: no gap (first)
        self.call('GET', '/health')                   # 2 GET: no gap, does not end the chain
        time.sleep(0.15)
        self.chat()                                   # 3 chat: gap measured from chat 1 (>= 150 ms)
        self.assertEqual(self.call('POST', hp.EPOCH_PATH, {}), 204)   # restart marker (not recorded)
        self.chat()                                   # 4 chat: gap reset -> None
        Upstream.fail = True
        self.chat()                                   # 5 chat 500: gap from 4, then chain ends
        Upstream.fail = False
        self.chat()                                   # 6 chat: after an upstream error -> None
        r = self.recs(6)   # 5 chats + 1 GET (the epoch call is never recorded)
        r = sorted(r, key=lambda x: x['seq'])
        chats = [x for x in r if x['path'].endswith('/chat/completions')]
        gets = [x for x in r if x['method'] == 'GET']
        self.assertEqual(len(chats), 5)
        self.assertIsNone(chats[0]['gap_ms'])
        self.assertGreaterEqual(chats[1]['gap_ms'], 140)
        self.assertIsNone(chats[2]['gap_ms'])
        self.assertIsNotNone(chats[3]['gap_ms'])
        self.assertIsNone(chats[4]['gap_ms'])
        self.assertTrue(all(g['gap_ms'] is None for g in gets))
        self.assertFalse(any(x['path'] == hp.EPOCH_PATH for x in r))

    def test_transcript_keeps_reasoning(self):
        self.chat()
        p = Path(self.tmp) / 'transcript.jsonl'
        # the recorder creates the file before it writes the line: wait for a complete first line
        for _ in range(500):
            if p.exists() and p.read_text().endswith('\n'):
                break
            time.sleep(0.02)
        tr = json.loads(p.read_text().splitlines()[0])
        self.assertEqual(tr['reply']['reasoning_content'], 'think')


class ReplyParserTests(unittest.TestCase):
    def test_stream_keeps_reasoning_and_tool_ids(self):
        sse = ('data: {"choices":[{"delta":{"reasoning_content":"th"}}]}\n\n'
               'data: {"choices":[{"delta":{"reasoning_content":"ink","content":"x"}}]}\n\n'
               'data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c9","function":{"name":"rd","arguments":"{}"}}]}}]}\n\n'
               'data: [DONE]\n\n')
        r = hp.reply_from_text(sse)['reply']
        self.assertEqual(r['reasoning_content'], 'think')
        self.assertEqual(r['tool_calls'][0]['id'], 'c9')
        self.assertNotIn('reasoning_content', hp.reply_from_text('data: {"choices":[{"delta":{"content":"a"}}]}\n\n')['reply'])


class ToolMessageIdentityTests(unittest.TestCase):
    def test_after_compaction_shrinks_the_list(self):
        t1 = {'role': 'tool', 'tool_call_id': 'a', 'content': 'old'}
        t2 = {'role': 'tool', 'tool_call_id': 'b', 'content': 'new'}
        prev = [{'role': 'system'}, {'role': 'user'}, {'role': 'assistant'}, t1, {'role': 'assistant'}, {'role': 'user'}]
        cur = [{'role': 'system'}, {'role': 'user', 'content': 'summary'}, t2]   # compacted: shorter than prev
        self.assertEqual(th.new_tool_messages(prev, cur), [t2])
        self.assertEqual(th.new_tool_messages(prev, prev + [t2]), [t2])
        self.assertEqual(th.new_tool_messages(prev, prev), [])
        self.assertEqual(th.new_tool_messages(None, [t1, t1]), [t1, t1])


class CodexL6Controls(unittest.TestCase):
    """Codex review of 776307a (S/v020-l6-776307a-review-20261113/controls.py) as regression tests."""
    def test_partially_timed_tools_withhold_client_only(self):
        rpc = hp.parse_rpc_events([json.dumps({'type': 'tool_execution_end', 'toolName': 'read', 'durationMs': 50}),
                                   json.dumps({'type': 'tool_execution_end', 'toolName': 'bash'})])
        s = hp.hop_summary([{'gap_ms': 200}], rpc)
        self.assertIsNone(s['totals_ms']['client_only'])
        self.assertIsNone(s['totals_ms']['tool_execution'])
        self.assertEqual(s['totals_ms']['tool_execution_known_partial'], 50)
        self.assertTrue(any('TOOL-TIME-PARTIAL' in n for n in s['notes']))

    def test_fully_timed_tools_keep_the_split(self):
        rpc = hp.parse_rpc_events([json.dumps({'type': 'tool_execution_end', 'toolName': 'read', 'durationMs': 50}),
                                   json.dumps({'type': 'tool_execution_end', 'toolName': 'bash', 'durationMs': 30})])
        s = hp.hop_summary([{'gap_ms': 200}], rpc)
        self.assertEqual(s['totals_ms']['client_only'], 120)
        self.assertIsNone(s['totals_ms']['tool_execution_known_partial'])

    def test_reconstruction_keeps_reasoning_and_reuse_is_labelled_estimate(self):
        wire = 'data: ' + json.dumps({'choices': [{'delta': {'reasoning_content': 'synthetic reasoning', 'content': 'answer',
                                                             'tool_calls': [{'index': 0, 'id': 'call_x', 'function': {'name': 'read', 'arguments': '{}'}}]}}]}) + '\n\ndata: [DONE]\n\n'
        reply = hp.reply_from_text(wire)['reply']
        text = th.reply_text(reply)
        self.assertIn('synthetic reasoning', text)
        self.assertIn('answer', text)
        self.assertTrue(th.turn_metrics([1, 2], [1, 2, 3])['reuse_estimate'])
        self.assertIn('estimate', th.summarize_turns([th.turn_metrics(None, [1])])['reuse_kind'])


if __name__ == '__main__':
    unittest.main()
