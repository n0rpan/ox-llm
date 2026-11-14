"""compare_logits.py is fail-closed: synthetic dump_ref dirs only (no model, no oracle binary)."""
from array import array
import importlib.util
import io
import json
import math
from pathlib import Path
import random
import tempfile
import unittest
from contextlib import redirect_stdout

SPEC = importlib.util.spec_from_file_location(
    'compare_logits', Path(__file__).resolve().parents[1] / 'tools/tiny_real/compare_logits.py')
cmp = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(cmp)

NV = 64
TOKENS = [5, 6, 7]
GEN = [9, 10, 11, 12]
N_EVAL = len(TOKENS) + len(GEN) - 1


def logits(rng, n=N_EVAL, noise=0.0, base=None):
    base = base or [[rng.uniform(-5, 5) for _ in range(NV)] for _ in range(n)]
    return base, [[v + rng.uniform(-noise, noise) for v in row] for row in base]


def write(d, rows, tokens=TOKENS, gen=GEN, n_eval=None, extra=None):
    d.mkdir(parents=True)
    m = {'n_vocab': NV, 'n_eval': len(rows) if n_eval is None else n_eval, 'tokens': tokens, 'generated': gen,
         'logits': True, 'last_only': False}
    m.update(extra or {})
    (d / 'manifest.json').write_text(json.dumps(m))
    a = array('f')
    for r in rows:
        a.extend(r)
    with open(d / 'logits.f32', 'wb') as f:
        a.tofile(f)
    return d


class CompareTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)
        self.rng = random.Random(7)

    def tearDown(self):
        self.tmp.cleanup()

    def run_main(self, a, b, *args):
        buf = io.StringIO()
        with redirect_stdout(buf):
            rc = cmp.main([str(a), str(b), '--json', *args])
        return rc, json.loads(buf.getvalue())

    # ---- positive controls

    def test_identical_dumps_pass_bitwise(self):
        base, _ = logits(self.rng)
        a, b = write(self.dir / 'a', base), write(self.dir / 'b', base)
        rc, r = self.run_main(a, b, '--max-mean-kld', '1e-9')
        self.assertEqual(rc, 0)
        self.assertEqual((r['positions'], r['bitwise_positions'], r['top1_agree']), (N_EVAL, N_EVAL, 1.0))

    def test_small_noise_passes_and_kld_bound_is_enforced(self):
        base, noisy = logits(self.rng, noise=1e-3)
        a, b = write(self.dir / 'a', base), write(self.dir / 'b', noisy)
        rc, r = self.run_main(a, b, '--max-mean-kld', '1e-3')
        self.assertEqual(rc, 0)
        self.assertGreater(r['mean_kld'], 0)
        rc, _ = self.run_main(a, b, '--max-mean-kld', str(r['mean_kld'] / 2))
        self.assertEqual(rc, 1)

    def test_divergent_greedy_compares_only_the_common_prefix(self):
        base, _ = logits(self.rng)
        a = write(self.dir / 'a', base)
        b = write(self.dir / 'b', base, gen=[9, 10, 99, 98])
        rc, r = self.run_main(a, b, '--max-mean-kld', '1e-9')
        self.assertEqual(rc, 0)
        self.assertEqual((r['positions'], r['gen_common_prefix']), (len(TOKENS) + 2, 2))

    # ---- negative controls

    def assertFails(self, a, b, msg):
        rc, r = self.run_main(a, b, '--max-mean-kld', '1e9')
        self.assertEqual(rc, 1)
        self.assertFalse(r['pass'])
        self.assertRegex(r['error'], msg)

    def test_truncated_logits_file_fails(self):
        base, _ = logits(self.rng)
        a = write(self.dir / 'a', base)
        b = write(self.dir / 'b', base[:1], n_eval=N_EVAL)  # manifest claims N_EVAL, file holds one row
        self.assertFails(a, b, 'bytes')

    def test_short_n_eval_fails(self):
        base, _ = logits(self.rng)
        a = write(self.dir / 'a', base)
        b = write(self.dir / 'b', base[:1])  # consistent file + manifest, but 1 of 6 positions
        self.assertFails(a, b, 'n_eval')

    def test_nan_and_inf_fail_on_either_side(self):
        for bad in (math.nan, math.inf, -math.inf):
            for side in ('a', 'b'):
                with self.subTest(bad=bad, side=side):
                    base, _ = logits(self.rng)
                    rows = [list(r) for r in base]
                    rows[-1][3] = bad  # even outside the compared prefix
                    d = self.dir / '{}-{}'.format(side, bad)
                    good = write(d / 'good', base)
                    broken = write(d / 'bad', rows)
                    a, b = (broken, good) if side == 'a' else (good, broken)
                    self.assertFails(a, b, 'non-finite')

    def test_unequal_lengths_fail_even_with_a_bad_tail(self):
        base, _ = logits(self.rng)
        longer = [list(r) for r in base] + [[1.0] * NV, [1.0] * (NV - 1) + [math.nan]]
        a = write(self.dir / 'a', base)
        b = write(self.dir / 'b', longer, gen=GEN + [13, 14])  # consistent, 2 extra rows, NaN in the tail
        self.assertFails(a, b, 'shapes differ')
        self.assertFails(b, a, 'shapes differ')
        c = write(self.dir / 'c', longer, gen=GEN + [13, 14])
        self.assertFails(b, c, 'non-finite')  # equal shapes: the tail is scanned

    def test_all_nan_rows_fail(self):
        base, _ = logits(self.rng)
        a = write(self.dir / 'a', base)
        b = write(self.dir / 'b', [[math.nan] * NV for _ in base])
        self.assertFails(a, b, 'non-finite')

    def test_empty_or_missing_fails(self):
        a = write(self.dir / 'a', [], tokens=[], gen=[])
        b = write(self.dir / 'b', [], tokens=[], gen=[])
        self.assertFails(a, b, 'no prompt tokens')
        base, _ = logits(self.rng)
        c = write(self.dir / 'c', base)
        (c / 'logits.f32').unlink()
        self.assertFails(c, write(self.dir / 'd', base), 'logits.f32')

    def test_last_only_or_shape_mismatch_fails(self):
        base, _ = logits(self.rng)
        a = write(self.dir / 'a', base)
        self.assertFails(a, write(self.dir / 'b', base, extra={'last_only': True}), 'per-position')
        self.assertFails(a, write(self.dir / 'c', base, extra={'n_vocab': NV * 2}), 'bytes|vocab')
        self.assertFails(a, write(self.dir / 'd', base, tokens=[5, 6, 8]), 'prompt tokens')


if __name__ == '__main__':
    unittest.main()
