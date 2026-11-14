"""v0.2.0 opt L6 rig: parsers and math on synthetic data (model-free; runs in ci-lab).

The benches themselves need a built oxe-server and run by hand or in a lane; these tests cover the
pure helpers, the proxy on a fake upstream, the RPC tee on a fake binary, and the token harness
driven by a fake counter.
"""
import http.client
import http.server
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'lab' / 'benchmarks' / 'req_path'))
sys.path.insert(0, str(ROOT / 'lab' / 'benchmarks' / 'tokens'))
sys.path.insert(0, str(ROOT / 'tools' / 'gates'))
import stub_bench as sb  # noqa: E402
import token_harness as th  # noqa: E402
import hop_probe as hp  # noqa: E402


class StubBenchMath(unittest.TestCase):
    def test_median_even_odd_and_empty(self):
        self.assertEqual(sb.median([3, 1, 2]), 2.0)
        self.assertEqual(sb.median([4, 1, 2, 3]), 2.5)
        self.assertIsNone(sb.median([]))
        self.assertIsNone(sb.median([None]))

    def test_engine_timings_keeps_known_keys_and_nulls_absent(self):
        t = sb.engine_timings({'timings': {'prompt_ms': 5.0, 'predicted_ms': 2.0, 'cache_n': 7,
                                           'render_ms': 1.5, 'queue_ms': 0.25, 'bogus': 9}})
        self.assertEqual(t['prompt_ms'], 5.0)
        self.assertEqual(t['render_ms'], 1.5)
        self.assertEqual(t['queue_ms'], 0.25)
        self.assertIsNone(t['tokenize_ms'])
        self.assertNotIn('bogus', t)
        self.assertEqual(sb.engine_timings({}), {})

    def test_overhead_subtracts_engine_terms_or_falls_back_to_wall(self):
        self.assertEqual(sb.overhead_ms(100.0, {'prompt_ms': 30.0, 'predicted_ms': 20.0}), 50.0)
        self.assertEqual(sb.overhead_ms(100.0, {}), 100.0)
        self.assertEqual(sb.overhead_ms(10.0, {'prompt_ms': 30.0, 'predicted_ms': 20.0}), 0.0)

    def test_summarize_uses_medians_and_reports_engine_keys(self):
        samples = [
            {'wall_ms': 10.0, 'ttfb_ms': 9.0, 'timings': {'prompt_ms': 2.0, 'predicted_ms': 1.0, 'queue_ms': 0.1}},
            {'wall_ms': 30.0, 'ttfb_ms': 29.0, 'timings': {'prompt_ms': 2.0, 'predicted_ms': 1.0, 'queue_ms': 0.3}},
            {'wall_ms': 20.0, 'ttfb_ms': 19.0, 'timings': {'prompt_ms': 2.0, 'predicted_ms': 1.0, 'queue_ms': 0.2}},
        ]
        r = sb.summarize(samples)
        self.assertEqual(r['n'], 3)
        self.assertEqual(r['median_wall_ms'], 20.0)
        self.assertEqual(r['median_overhead_ms'], 17.0)
        self.assertEqual(r['engine_keys_seen'], ['queue_ms'])
        self.assertAlmostEqual(r['median_engine_ms']['queue_ms'], 0.2)
        self.assertEqual(r['wall_ms_series'], [10.0, 30.0, 20.0])

    def test_text_builder_is_deterministic_and_sized(self):
        a = sb.build_user_text('s', 500, 'x')
        b = sb.build_user_text('s', 500, 'x')
        self.assertEqual(a, b)
        self.assertNotEqual(a, sb.build_user_text('s', 500, 'y'))
        self.assertEqual(len(sb.make_text(__import__('random').Random(1), 300)), 300)

    def test_bench_refuses_small_n_and_production_port(self):
        with self.assertRaises(SystemExit):
            sb.main(['--n', '3', '--server-bin', __file__, '--model', __file__])
        with self.assertRaises(SystemExit):
            sb.main(['--port', '8093', '--server-bin', __file__, '--model', __file__])


class TokenHarnessMath(unittest.TestCase):
    def test_common_prefix(self):
        self.assertEqual(th.common_prefix_len([1, 2, 3, 4], [1, 2, 9]), 2)
        self.assertEqual(th.common_prefix_len([], [1]), 0)
        self.assertEqual(th.common_prefix_len([5], [5]), 1)

    def test_turn_metrics_first_turn_is_all_appended(self):
        m = th.turn_metrics(None, [1, 2, 3])
        self.assertEqual((m['prompt_tokens'], m['reused_tokens'], m['appended_tokens']), (3, 0, 3))

    def test_turn_metrics_reuse_and_append(self):
        prev = [1, 2, 3, 4, 5]
        cur = [1, 2, 3, 4, 5, 6, 7]
        m = th.turn_metrics(prev, cur)
        self.assertEqual((m['reused_tokens'], m['appended_tokens']), (5, 2))
        edited = [1, 2, 9, 4, 5, 6]
        m2 = th.turn_metrics(prev, edited)
        self.assertEqual((m2['reused_tokens'], m2['appended_tokens']), (2, 4))

    def test_median_and_summarize_turns(self):
        turns = [
            {'prompt_tokens': 100, 'reused_tokens': 0, 'appended_tokens': 100, 'output_tokens': 10,
             'prompt_bytes': 300, 'tool_schema_bytes': 50, 'tool_schema_tokens': 20,
             'new_tool_output_bytes': 0, 'new_tool_output_tokens': 0, 'prev_state_present': False},
            {'prompt_tokens': 150, 'reused_tokens': 120, 'appended_tokens': 30, 'output_tokens': 20,
             'prompt_bytes': 450, 'tool_schema_bytes': 50, 'tool_schema_tokens': 20,
             'new_tool_output_bytes': 400, 'new_tool_output_tokens': 90, 'prev_state_present': True},
            {'prompt_tokens': 200, 'reused_tokens': 150, 'appended_tokens': 50, 'output_tokens': 30,
             'prompt_bytes': 600, 'tool_schema_bytes': 50, 'tool_schema_tokens': 20,
             'new_tool_output_bytes': 100, 'new_tool_output_tokens': 25, 'prev_state_present': True},
        ]
        s = th.summarize_turns(turns)
        self.assertEqual(s['turns'], 3)
        self.assertEqual(s['totals']['appended_tokens'], 180)
        self.assertEqual(s['totals']['output_tokens'], 60)
        self.assertEqual(s['medians']['prompt_tokens'], 150)
        self.assertAlmostEqual(s['median_reuse_fraction'], (120 / 150 + 150 / 200) / 2)

    def test_golden_check_reports_first_difference(self):
        self.assertEqual(th.golden_check('abc', 'abc'), {'match': True})
        r = th.golden_check('abcd', 'abXd')
        self.assertFalse(r['match'])
        self.assertEqual(r['first_diff_char'], 2)

    def test_tool_messages_and_reply_text(self):
        msgs = [{'role': 'user', 'content': 'x'}, {'role': 'assistant', 'content': 'y'},
                {'role': 'tool', 'content': 'old'}, {'role': 'tool', 'content': 'new'}]
        self.assertEqual([m['content'] for m in th.new_tool_messages(msgs[:3], msgs)], ['new'])
        self.assertEqual(len(th.tool_messages(msgs)), 2)
        self.assertEqual(th.reply_text({'content': 'hi', 'tool_calls': [{'id': 'a'}]}), 'hi[{"id":"a"}]')
        self.assertEqual(th.reply_text(None), '')

    def test_count_transcript_with_fake_counter(self):
        class Fake:
            def render(self, body):
                prompt = ''.join(m['content'] for m in body['messages'])
                return prompt, [ord(c) for c in prompt]

            def tokenize(self, text):
                return [ord(c) for c in text]

        lines = [
            {'body': {'messages': [{'role': 'user', 'content': 'ab'}], 'tools': []},
             'reply': {'content': 'R'}, 'usage': {'completion_tokens': 4}},
            {'body': {'messages': [{'role': 'user', 'content': 'ab'}, {'role': 'assistant', 'content': 'R'},
                                   {'role': 'tool', 'content': 'out'}], 'tools': []},
             'reply': {'content': 'Z'}},
        ]
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / 't.jsonl'
            p.write_text('\n'.join(json.dumps(x) for x in lines) + '\n')
            r = th.count_transcript(Fake(), str(p))
        t0, t1 = r['turns']
        self.assertEqual(t0['output_tokens'], 4)
        self.assertEqual(t0['output_tokens_source'], 'usage')
        self.assertEqual(t1['reused_tokens'], 2 + 1)
        self.assertEqual(t1['new_tool_outputs'], 1)
        self.assertEqual(t1['new_tool_output_bytes'], 3)
        self.assertEqual(t1['output_tokens_source'], 'tokenized')


class HopProbeParsers(unittest.TestCase):
    def test_timings_from_compact_json_pretty_json_and_sse(self):
        self.assertEqual(hp.timings_from_text('{"timings":{"prompt_ms":3}}'), {'prompt_ms': 3})
        pretty = json.dumps({'timings': {'predicted_ms': 7}}, indent=2)
        self.assertEqual(hp.timings_from_text(pretty), {'predicted_ms': 7})
        sse = 'data: {"choices":[{"delta":{"content":"a"}}]}\n\ndata: {"timings":{"cache_n":5}}\n\ndata: [DONE]\n'
        self.assertEqual(hp.timings_from_text(sse), {'cache_n': 5})
        self.assertIsNone(hp.timings_from_text('not json'))

    def test_reply_from_plain_and_streamed_bodies(self):
        plain = json.dumps({'choices': [{'message': {'content': 'hi', 'tool_calls': [
            {'id': 'c1', 'type': 'function', 'function': {'name': 'read', 'arguments': '{}'}}]}}],
            'usage': {'completion_tokens': 3}})
        r = hp.reply_from_text(plain)
        self.assertEqual(r['reply']['content'], 'hi')
        self.assertEqual(r['reply']['tool_calls'][0]['id'], 'c1')
        self.assertEqual(r['usage'], {'completion_tokens': 3})
        self.assertFalse(r['streamed'])
        sse = ('data: {"choices":[{"delta":{"content":"he"}}]}\n\n'
               'data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"rd","arguments":"{\\"a"}}]}}]}\n\n'
               'data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\\":1}"}}]}}]}\n\n'
               'data: {"choices":[{"delta":{"content":"llo"}}],"usage":{"completion_tokens":5}}\n\n'
               'data: [DONE]\n')
        s = hp.reply_from_text(sse)
        self.assertTrue(s['streamed'])
        self.assertEqual(s['reply']['content'], 'hello')
        self.assertEqual(s['reply']['tool_calls'][0]['function'], {'name': 'rd', 'arguments': '{"a":1}'})
        self.assertEqual(s['usage'], {'completion_tokens': 5})

    def test_server_log_lines(self):
        lines = ['oxe-server: state cache: resumed 512 tokens (2 blocks + 1 snapshot) in 12.5 ms',
                 'oxe-server: state cache: preloaded 900 tokens (3 blocks + 0 snapshot) in 40 ms',
                 'oxe-server: flash: loaded 77 tokens in 3.25 ms', 'unrelated line']
        r = hp.parse_server_log(lines)
        self.assertEqual(r['resume'], [{'tokens': 512, 'ms': 12.5}])
        self.assertEqual(r['preload'], [{'tokens': 900, 'ms': 40.0}])
        self.assertEqual(r['load'], [{'what': 'flash', 'tokens': 77, 'ms': 3.25}])

    def test_rpc_events_real_schema_fixture(self):
        """Fixture in lab/tests/fixtures/oxa_rpc/tool_events.jsonl, field names from oxa-rpc/src/events.rs
        (toolName, durationMs, isError, done/total/secs) and oxa-rpc tests c9_json_records_*.rs."""
        path = ROOT / 'lab' / 'tests' / 'fixtures' / 'oxa_rpc' / 'tool_events.jsonl'
        r = hp.parse_rpc_events(path.read_text().splitlines())
        self.assertEqual(len(r['tools']), 3)
        self.assertEqual([t['tool'] for t in r['tools']], ['read', 'bash', 'edit'])
        self.assertEqual(r['tool_timed'], 2)
        self.assertEqual(r['tool_ms_total'], 42.0)
        self.assertTrue(r['tools'][1]['is_error'])
        self.assertEqual(r['compactions'], 1)
        self.assertEqual(r['prefill_secs_total'], 2.5)
        self.assertEqual(r['events'], 10)

    def test_control_zero_tool_time_with_tool_events_is_flagged(self):
        """Control: tool events exist but carry no durationMs. Tool time must not read as 0 (which would
        move the whole gap into client time). The summary must say so and withhold the split."""
        lines = ['{"type":"tool_execution_end","toolName":"read","isError":false}']
        rpc = hp.parse_rpc_events(lines)
        self.assertEqual(rpc['tool_ms_total'], 0)
        s = hp.hop_summary([{'status': 200, 'wall_ms': 5.0, 'gap_ms': 40.0, 'timings': {}}], dict(rpc, events=1))
        self.assertIsNone(s['totals_ms']['client_only'])
        self.assertIsNone(s['totals_ms']['tool_execution'])
        self.assertTrue(any('TOOL-TIME-ZERO' in n for n in s['notes']))

    def test_old_snake_case_fields_are_not_read(self):
        """Guard against the earlier wrong schema: duration_ms/tool_name are not oxa's fields."""
        rpc = hp.parse_rpc_events(['{"type":"tool_execution_end","tool_name":"x","duration_ms":9}'])
        self.assertEqual(rpc['tool_ms_total'], 0)
        self.assertEqual(rpc['tool_timed'], 0)

    def test_hop_summary_splits_client_and_tool_time(self):
        recs = [
            {'status': 200, 'wall_ms': 1000.0, 'ttfb_ms': 900.0, 'gap_ms': None,
             'timings': {'prompt_ms': 600.0, 'predicted_ms': 300.0, 'prompt_n': 800, 'cache_n': 0,
                         'predicted_n': 20, 'draft_n': 10, 'draft_n_accepted': 7}},
            {'status': 200, 'wall_ms': 500.0, 'ttfb_ms': 400.0, 'gap_ms': 250.0,
             'timings': {'prompt_ms': 100.0, 'predicted_ms': 200.0, 'prompt_n': 40, 'cache_n': 800,
                         'predicted_n': 10}},
        ]
        rpc = hp.parse_rpc_events(['{"type":"tool_execution_end","toolName":"read","durationMs":100}'])
        s = hp.hop_summary(recs, dict(rpc, events=3), {'resume': []})
        self.assertEqual(s['requests'], 2)
        self.assertEqual(s['totals_ms']['client_only'], 150.0)
        self.assertEqual(s['totals_ms']['tool_execution'], 100.0)
        self.assertAlmostEqual(s['per_request'][0]['engine_overhead_ms'], 100.0)
        self.assertAlmostEqual(s['per_request'][0]['mtp_acceptance'], 0.7)
        self.assertEqual(s['medians']['prefill_ms'], 350.0)
        self.assertEqual(s['notes'], [])

    def test_hop_summary_without_rpc_says_so(self):
        s = hp.hop_summary([{'status': 200, 'wall_ms': 5.0, 'timings': {}}])
        self.assertIn('no oxa RPC events: tool and client split not measured', s['notes'])
        self.assertIsNone(s['per_request'][0]['prefill_ms'])


class ProxyEndToEnd(unittest.TestCase):
    """The proxy in front of a fake upstream: a JSON reply and an SSE stream, both passed through."""

    @classmethod
    def setUpClass(cls):
        class Upstream(http.server.BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.1'

            def log_message(self, *a):
                return

            def do_POST(self):
                n = int(self.headers.get('Content-Length') or 0)
                raw = self.rfile.read(n)
                if json.loads(raw).get('stream'):
                    body = (b'data: {"choices":[{"delta":{"content":"ok"}}]}\n\n'
                            b'data: {"timings":{"prompt_ms":4,"predicted_ms":2},"choices":[]}\n\n'
                            b'data: [DONE]\n\n')
                    self.send_response(200)
                    self.send_header('Content-Type', 'text/event-stream')
                    self.send_header('Transfer-Encoding', 'chunked')
                    self.end_headers()
                    for i in range(0, len(body), 17):
                        piece = body[i:i + 17]
                        self.wfile.write(b'%x\r\n%s\r\n' % (len(piece), piece))
                    self.wfile.write(b'0\r\n\r\n')
                else:
                    body = json.dumps({'choices': [{'message': {'content': 'hi'}}],
                                       'timings': {'prompt_ms': 9}, 'usage': {'completion_tokens': 1}}).encode()
                    self.send_response(200)
                    self.send_header('Content-Type', 'application/json')
                    self.send_header('Content-Length', str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)

        cls.tmp = tempfile.mkdtemp(prefix='hop-proxy-test-')
        cls.up = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Upstream)
        threading.Thread(target=cls.up.serve_forever, daemon=True).start()
        state = hp.State(cls.tmp, cls.up.server_address[1], transcript=True)
        cls.proxy = http.server.ThreadingHTTPServer(('127.0.0.1', 0), hp.make_handler(state))
        cls.proxy.daemon_threads = True
        threading.Thread(target=cls.proxy.serve_forever, daemon=True).start()
        cls.port = cls.proxy.server_address[1]

    @classmethod
    def tearDownClass(cls):
        cls.proxy.shutdown()
        cls.up.shutdown()

    def post(self, path, obj):
        conn = http.client.HTTPConnection('127.0.0.1', self.port, timeout=10)
        conn.request('POST', path, body=json.dumps(obj).encode(), headers={'Content-Type': 'application/json'})
        r = conn.getresponse()
        data = r.read()
        conn.close()
        return r.status, data

    def test_json_and_stream_pass_through_and_are_recorded(self):
        body = {'messages': [{'role': 'user', 'content': 'hello'}]}
        status, data = self.post('/v1/chat/completions', body)
        self.assertEqual(status, 200)
        self.assertEqual(json.loads(data)['choices'][0]['message']['content'], 'hi')
        status, data = self.post('/v1/chat/completions', dict(body, stream=True))
        self.assertEqual(status, 200)
        self.assertIn(b'"content":"ok"', data)
        # Both metadata and transcript are appended after the response; await complete rows.
        import time
        deadline = time.monotonic() + 10
        for name in ('requests.jsonl', 'transcript.jsonl'):
            path = Path(self.tmp) / name
            while time.monotonic() < deadline:
                if path.exists() and path.read_text().count('\n') == 2:
                    break
                time.sleep(0.01)
            self.assertTrue(path.exists(), name)
            self.assertEqual(path.read_text().count('\n'), 2, name)
        recs = [json.loads(x) for x in (Path(self.tmp) / 'requests.jsonl').read_text().splitlines()]
        self.assertEqual(len(recs), 2)
        self.assertEqual(recs[0]['timings'], {'prompt_ms': 9})
        self.assertEqual(recs[1]['timings'], {'prompt_ms': 4, 'predicted_ms': 2})
        self.assertTrue(recs[1]['streamed'])
        self.assertIsNotNone(recs[1]['gap_ms'])
        self.assertNotIn('hello', (Path(self.tmp) / 'requests.jsonl').read_text(), 'metadata log holds no content')
        tr = [json.loads(x) for x in (Path(self.tmp) / 'transcript.jsonl').read_text().splitlines()]
        self.assertEqual(len(tr), 2)
        self.assertEqual(tr[0]['body'], body)
        self.assertEqual(tr[0]['reply']['content'], 'hi')
        self.assertEqual(tr[1]['reply']['content'], 'ok')


class RpcTee(unittest.TestCase):
    def test_tee_copies_events_and_passes_stdin_and_exit_code(self):
        with tempfile.TemporaryDirectory() as d:
            fake = Path(d) / 'fake_oxa.py'
            fake.write_text('#!/usr/bin/env python3\nimport sys\nfor line in sys.stdin:\n    print(\'{"type":"echo","line":\' + str(len(line)) + \'}\', flush=True)\nsys.exit(3)\n')
            fake.chmod(0o755)
            out = Path(d) / 'rpc.jsonl'
            env = dict(os.environ, OXA_REAL=str(fake), HOP_RPC_OUT=str(out))
            tee = str(ROOT / 'tools' / 'gates' / 'oxa_rpc_tee.py')
            p = subprocess.run([sys.executable, tee, '--mode', 'rpc'], input=b'abc\n', env=env,
                               stdout=subprocess.PIPE, timeout=60)
            self.assertEqual(p.returncode, 3)
            self.assertIn(b'"type":"echo"', p.stdout)
            self.assertEqual(out.read_text().count('"type":"echo"'), 1)


class LauncherPaths(unittest.TestCase):
    def test_gate_references_to_tools_gates_resolve(self):
        """Every $GATES/<file> the launcher uses exists in tools/gates (the earlier launcher pointed at
        lab/tools/tiny_real, where the probe files are not)."""
        import re
        text = (ROOT / 'tools' / 'gates' / 'tiny_real.sh').read_text()
        refs = set(re.findall(r'\$GATES/([A-Za-z0-9_.-]+)', text))
        self.assertTrue({'hop_probe.py', 'oxa_rpc_tee.py'} <= refs, refs)
        for name in refs:
            self.assertTrue((ROOT / 'tools' / 'gates' / name).is_file(), name)
        self.assertIn('GATES="$ROOT/tools/gates"', text)

    def test_probe_is_off_by_default(self):
        text = (ROOT / 'tools' / 'gates' / 'tiny_real.sh').read_text()
        self.assertIn('HOP_PROBE="${HOP_PROBE:-0}"', text)


if __name__ == '__main__':
    unittest.main()
