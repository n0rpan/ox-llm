"""replay_driver lands every production rung window (32K/48K/64K/82K/111K) without overshoot, model-free.

The stub engine counts tokens with a density that differs between the fixed prefix (system +
tools: dense, like the Qwen tokenizer on JSON schemas) and the growing history (looser), which
is what made the average-ratio driver overshoot the 32K window on the tiny-real model.
A negative control re-runs with the marginal ratio disabled and must overshoot at least once.
"""
from pathlib import Path
import sys
import unittest

LAB = Path(__file__).resolve().parents[1]
sys.path[:0] = [str(LAB / 'benchmarks/fixtures'), str(LAB / 'benchmarks/gatepack')]
import fixture_lib as fx  # noqa: E402
import replay_driver as rd  # noqa: E402
import gate_replay as gr  # noqa: E402


def stub_engine(prefix_density=0.55, history_density=0.20):
    """stream_chat stand-in: prompt_tokens = dense prefix + looser history (chars x density)."""
    def chat(port, body, timeout=None):
        msgs = body['messages']
        head = sum(len(str(m.get('content') or '')) for m in msgs if m.get('role') == 'system')
        head += len(str(body.get('tools') or ''))
        rest = sum(len(str(m.get('content') or '')) + len(str(m.get('tool_calls') or '')) for m in msgs
                   if m.get('role') != 'system')
        tokens = int(head * prefix_density + rest * history_density) + 4 * len(msgs)
        return dict(status=200, done=True, error=None, finish='stop', usage={'prompt_tokens': tokens},
                    timings={'cache_n': 0}, message={'role': 'assistant', 'content': 'ok'})
    return chat


class LandingTests(unittest.TestCase):
    def grow_all(self, labels):
        session = rd.Session(1401, 'ornith-35b-a3b-local')
        got = {}
        for label in labels:
            target, ceiling = fx.RUNGS[label]
            r = session.grow(0, target, ceiling, 400)
            got[label] = r['measured']
        return got

    def test_every_production_window_is_landed(self):
        real = rd.stream_chat
        rd.stream_chat = stub_engine()
        try:
            got = self.grow_all(fx.RUNG_ORDER)
        finally:
            rd.stream_chat = real
        for label, measured in got.items():
            target, ceiling = fx.RUNGS[label]
            self.assertTrue(target <= measured <= ceiling, (label, measured))
        self.assertIn('82K', got)

    def test_82k_alone_after_32k_like_the_ornith_ladder(self):
        real = rd.stream_chat
        rd.stream_chat = stub_engine(prefix_density=0.6, history_density=0.25)
        try:
            got = self.grow_all(('32K', '64K', '82K'))
        finally:
            rd.stream_chat = real
        self.assertTrue(82000 <= got['82K'] <= 85000, got)

    def test_both_density_orientations_land(self):
        for dens in ((0.15, 0.40), (0.60, 0.20)):
            with self.subTest(dens=dens):
                real = rd.stream_chat
                rd.stream_chat = stub_engine(*dens)
                try:
                    got = self.grow_all(fx.RUNG_ORDER)
                finally:
                    rd.stream_chat = real
                for label, measured in got.items():
                    self.assertTrue(fx.RUNGS[label][0] <= measured <= fx.RUNGS[label][1], (dens, label, measured))

    def test_negative_control_average_ratio_overshoots(self):
        """A step-size ratio that underestimates the tokens a step adds (what the pre-fix average
        ratio did on the Qwen tokenizer) overshoots: the window check catches it."""
        class AverageOnly(rd.Session):
            mratio = property(lambda self: 0.12, lambda self, v: None)  # true marginal density ~0.40
        real = rd.stream_chat
        rd.stream_chat = stub_engine(0.15, 0.40)
        try:
            session = AverageOnly(1401, 'ornith-35b-a3b-local')
            with self.assertRaisesRegex(fx.FixtureError, 'rung_overshoot'):
                for label in fx.RUNG_ORDER:
                    session.grow(0, *fx.RUNGS[label], 400)
        finally:
            rd.stream_chat = real



class AppendSizeTests(unittest.TestCase):
    """G1's --append-tokens are real tokens of the model's tokenizer (tiny-real: a 4 chars/token
    guess made '28K' ~49K Qwen tokens and the 82K-rung request exceeded n_ctx 131072)."""
    class A:
        append_tokens = 28000

    def test_uses_the_ladder_session_ratio(self):
        s = rd.Session(1401, 'x')
        s.mratio = 1.92
        self.assertEqual(gr.append_chars(self.A, s), int(28000 * 4 / 1.92))
        s.mratio, s.ratio = None, 1.5
        self.assertEqual(gr.append_chars(self.A, s), int(28000 * 4 / 1.5))

    def test_82k_rung_plus_append_fits_a_131k_context(self):
        s = rd.Session(1401, 'x')
        s.mratio = 1.92
        tokens = gr.append_chars(self.A, s) / 4 * 1.92
        self.assertLess(85000 + tokens + 256, 131072)
        self.assertGreater(tokens, 27000)

    def test_ratio_is_clamped_and_defaults(self):
        self.assertEqual(gr.append_chars(self.A, None), 28000 * 4)
        s = rd.Session(1401, 'x')
        s.mratio = 100.0
        self.assertEqual(gr.append_chars(self.A, s), int(28000 * 4 / 5.0))


if __name__ == '__main__':
    unittest.main()
