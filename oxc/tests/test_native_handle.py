"""NativeHandle (tiny-real fixture rehearsal): ledger rows from engine timings, refused ports. No engine."""
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'benchmarks/fixtures'))
from native_handle import NativeHandle  # noqa: E402


class NativeHandleTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.h = NativeHandle('/nonexistent/oxe-server', '/nonexistent/m.gguf', Path(self.tmp.name) / 'ledger', port=18997)

    def tearDown(self):
        self.tmp.cleanup()

    def rows(self):
        out = []
        for p in sorted((Path(self.tmp.name) / 'ledger').glob('requests-*.jsonl')):
            out += [json.loads(l) for l in p.read_text().splitlines()]
        return out

    def test_stream_timings_become_a_ledger_row(self):
        sse = ('data: {"choices":[{"delta":{"content":"hi"}}]}\n\n'
               'data: {"choices":[],"usage":{"prompt_tokens":40,"prompt_tokens_details":{"cached_tokens":32}},'
               '"timings":{"cache_n":32,"prompt_n":8,"predicted_n":5,"resume_src":"live"}}\n\ndata: [DONE]\n\n')
        self.h._ledger(1.0, '/v1/chat/completions', 200, sse.encode())
        r = self.rows()[0]
        self.assertEqual((r['cache_n'], r['prompt_n'], r['predicted_n'], r['cached_tokens'], r['resume_src']), (32, 8, 5, 32, 'live'))
        self.assertNotIn('miss_class', r)  # a controller computation: never invented here

    def test_json_body_and_missing_timings(self):
        self.h._ledger(2.0, '/v1/chat/completions', 200, json.dumps({'timings': {'cache_n': 0, 'prompt_n': 9, 'predicted_n': 1}}).encode())
        self.h._ledger(3.0, '/v1/chat/completions', 500, b'oops')
        a, b = self.rows()
        self.assertEqual(a['prompt_n'], 9)
        self.assertEqual((b['status'], b['cache_n']), (500, None))

    def test_production_ports_refused(self):
        for port in (8092, 8093, 8091):
            with self.assertRaises(ValueError):
                NativeHandle('x', 'y', Path(self.tmp.name) / 'l2', port=port)


sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'benchmarks/gatepack'))
import fixture_lib as fx  # noqa: E402


class CanaryEarlyEndTests(unittest.TestCase):
    """Tiny-real rehearsal finding: an agent rung's greedy canary may end in a tool call."""
    def canary(self, n, finish):
        rows = [dict(token='t%d' % i, logprob=-0.1, top=[('a', -0.1)] * 4) for i in range(n)]
        return dict(rows=rows, want_positions=64, want_top=4, finish=finish)

    def test_tool_call_end_is_an_early_end(self):
        self.assertEqual(fx.canary_problems(self.canary(40, 'tool_calls')), [])
        self.assertIn('canary_wrong_length', fx.canary_problems(self.canary(5, 'tool_calls')))
        self.assertEqual(fx.canary_problems(self.canary(40, 'stop')), [])

    def test_short_without_a_model_end_stays_invalid(self):
        for finish in ('length', None, 'error'):
            self.assertIn('canary_wrong_length', fx.canary_problems(self.canary(40, finish)))
        self.assertIn('canary_wrong_length', fx.canary_problems(self.canary(65, 'tool_calls')))


class ClassLabelTests(unittest.TestCase):
    """Interim seal: never label a PFG ladder `tiled` (the binding still records pf_attn)."""
    def test_kernel_label_must_match_actual(self):
        fx.check_class_label('tiled', {'pf_attn': 'tiled'})
        fx.check_class_label('pfg-thinking', {'pf_attn': 'pfg'})
        fx.check_class_label('fa2x', {'pf_attn': 'fa2x.f8.c64'})
        for label, actual in (('tiled', 'pfg'), ('tiled-thinking', 'pfg'), ('pfg', 'tiled')):
            with self.assertRaisesRegex(fx.FixtureError, 'class_label_mismatch'):
                fx.check_class_label(label, {'pf_attn': actual})

    def test_free_labels_and_unknown_actual_pass(self):
        fx.check_class_label('ornith-candidate', {'pf_attn': 'pfg'})
        fx.check_class_label('tiled', {'pf_attn': 'default'})
        with self.assertRaisesRegex(fx.FixtureError, 'class_label_unverifiable'):
            fx.check_class_label('tiled', {})


if __name__ == '__main__':
    unittest.main()
