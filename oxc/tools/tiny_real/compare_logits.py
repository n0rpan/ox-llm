#!/usr/bin/env python3
"""Compare two dump_ref output dirs (engine/tools/oracle/dump_ref.cpp: manifest.json + logits.f32).

Fail-closed: both dumps must be complete (n_eval == prompt + generated - 1, logits.f32 exactly
n_eval x n_vocab f32), every logit finite, at least the whole prompt comparable; otherwise exit 1.
Per evaluated position with identical inputs in both runs (prompt + common greedy prefix): cosine of the logit vectors, max |diff|, top-1 equal, top-k set equal;
plus the greedy continuations. Standard library only (streams the f32 file row by row).
Also the KL divergence KL(A || B) per position (mean / max). Exit 0 when --max-mean-kld is given
and met, or (without it) when every position meets --min-cos, top-1 agrees on >= --min-top1 of
positions and the greedy continuations agree on >= --min-gen-prefix tokens; else 1.

  compare_logits.py DIR_A DIR_B [--topk 5] [--min-cos 0.9999] [--min-top1 1.0] [--min-gen-prefix 8] [--json]
"""
import argparse
from array import array
import json
import math
from pathlib import Path
import sys


class CompareError(Exception):
    """The dumps cannot be compared (truncated, inconsistent or non-finite): always a FAIL."""


def load_dump(d):
    """Validate one dump_ref dir; returns (manifest, n_eval). Fail-closed on every inconsistency."""
    d = Path(d)
    try:
        m = json.loads((d / 'manifest.json').read_text())
    except (OSError, ValueError) as e:
        raise CompareError('{}: unreadable manifest ({})'.format(d, e))
    toks, gen, nv, n_eval = m.get('tokens'), m.get('generated', []), m.get('n_vocab'), m.get('n_eval')
    if not isinstance(toks, list) or not toks or not isinstance(gen, list):
        raise CompareError('{}: no prompt tokens'.format(d))
    if not isinstance(nv, int) or nv < 2 or not isinstance(n_eval, int):
        raise CompareError('{}: bad n_vocab / n_eval'.format(d))
    if not m.get('logits') or m.get('last_only'):
        raise CompareError('{}: dump has no per-position logits (--logits, not --last-only)'.format(d))
    # dump_ref evaluates the prompt, then each generated token except the last
    want = len(toks) + max(len(gen) - 1, 0)
    if n_eval != want:
        raise CompareError('{}: n_eval {} != {} (prompt {} + generated {} - 1)'.format(d, n_eval, want, len(toks), len(gen)))
    try:
        size = (d / 'logits.f32').stat().st_size
    except OSError as e:
        raise CompareError('{}: no logits.f32 ({})'.format(d, e))
    if size != n_eval * nv * 4:
        raise CompareError('{}: logits.f32 has {} bytes, expected {} x {} x 4 = {}'.format(d, size, n_eval, nv, n_eval * nv * 4))
    return m, n_eval


def rows(path, n_vocab, n):
    """Exactly n rows of n_vocab f32 (the size was checked)."""
    with open(path, 'rb') as f:
        for _ in range(n):
            a = array('f')
            a.fromfile(f, n_vocab)  # EOFError here = file changed under us: propagates as a failure
            yield a


def finite(a):
    return all(math.isfinite(x) for x in a)


def topk(a, k):
    return sorted(range(len(a)), key=a.__getitem__, reverse=True)[:k]


def kl(a, b):
    """KL(P_a || P_b) of the softmaxes, in nats (a = reference)."""
    ma, mb = max(a), max(b)
    za = sum(math.exp(x - ma) for x in a)
    zb = sum(math.exp(y - mb) for y in b)
    la, lb = ma + math.log(za), mb + math.log(zb)
    return max(0.0, sum(math.exp(x - la) * ((x - la) - (y - lb)) for x, y in zip(a, b)))


def compare(da, db, k=5):
    ma, na_eval = load_dump(da)
    mb, nb_eval = load_dump(db)
    if ma['tokens'] != mb['tokens'] or ma['n_vocab'] != mb['n_vocab']:
        raise CompareError('different prompt tokens or vocabularies')
    nv = ma['n_vocab']
    ga, gb = ma['generated'], mb['generated']
    # equal shapes first: zip below must cover every row of BOTH dumps
    if na_eval != nb_eval or len(ga) != len(gb):
        raise CompareError('dump shapes differ: n_eval {} vs {}, generated {} vs {}'.format(na_eval, nb_eval, len(ga), len(gb)))
    prefix = next((i for i, (x, y) in enumerate(zip(ga, gb)) if x != y), min(len(ga), len(gb)))
    # positions whose inputs are identical in both runs: the prompt plus the common greedy prefix
    # (the logits that chose the first differing token are still comparable; later ones are not)
    n_cmp = min(len(ma['tokens']) + prefix, na_eval, nb_eval)
    if n_cmp < len(ma['tokens']):
        raise CompareError('fewer comparable positions ({}) than prompt tokens ({})'.format(n_cmp, len(ma['tokens'])))
    pos = []
    checked = 0
    for i, (a, b) in enumerate(zip(rows(Path(da) / 'logits.f32', nv, na_eval), rows(Path(db) / 'logits.f32', nv, nb_eval))):
        # every row of both dumps must be finite, compared or not
        if not finite(a) or not finite(b):
            raise CompareError('non-finite logit at position {} ({})'.format(i, 'A' if not finite(a) else 'B'))
        checked += 1
        if i >= n_cmp:
            continue
        dot = sum(x * y for x, y in zip(a, b))
        na = math.sqrt(sum(x * x for x in a))
        nb = math.sqrt(sum(y * y for y in b))
        if na == 0.0 or nb == 0.0:
            raise CompareError('all-zero logit row at position {}'.format(i))
        ta, tb = topk(a, k), topk(b, k)
        kld = kl(a, b)
        p = {'pos': i, 'cos': dot / (na * nb), 'max_abs': max(abs(x - y) for x, y in zip(a, b)),
             'bitwise': a == b, 'top1': ta[0] == tb[0], 'topk_set': set(ta) == set(tb), 'topk_order': ta == tb, 'kld': kld}
        if not all(math.isfinite(p[x]) for x in ('cos', 'max_abs', 'kld')):
            raise CompareError('non-finite metric at position {}'.format(i))
        pos.append(p)
    if checked != na_eval:
        raise CompareError('scanned {} of {} rows'.format(checked, na_eval))
    if len(pos) != n_cmp or not pos:
        raise CompareError('compared {} positions, expected {}'.format(len(pos), n_cmp))
    return {'positions': len(pos), 'min_cos': min(p['cos'] for p in pos), 'max_abs': max(p['max_abs'] for p in pos),
            'mean_kld': sum(p['kld'] for p in pos) / len(pos), 'max_kld': max(p['kld'] for p in pos),
            'bitwise_positions': sum(p['bitwise'] for p in pos), 'top1_agree': sum(p['top1'] for p in pos) / len(pos),
            'topk_set_agree': sum(p['topk_set'] for p in pos) / len(pos), 'gen_len': [len(ga), len(gb)],
            'gen_common_prefix': prefix, 'gen_equal': ga == gb}


def main(argv=None):
    ap = argparse.ArgumentParser()
    ap.add_argument('a')
    ap.add_argument('b')
    ap.add_argument('--topk', type=int, default=5)
    ap.add_argument('--min-cos', type=float, default=0.9999)
    ap.add_argument('--min-top1', type=float, default=1.0)
    ap.add_argument('--min-gen-prefix', type=int, default=8)
    ap.add_argument('--max-mean-kld', type=float, default=None, help='pass bound on the mean KLD (replaces --min-cos/--min-top1)')
    ap.add_argument('--json', action='store_true')
    a = ap.parse_args(argv)
    try:
        r = compare(a.a, a.b, a.topk)
    except (CompareError, OSError, EOFError, ValueError) as e:
        print(json.dumps({'pass': False, 'error': str(e)}) if a.json else 'pass=False error={}'.format(e))
        return 1
    if a.max_mean_kld is not None:
        ok = r['mean_kld'] <= a.max_mean_kld
    else:
        ok = r['min_cos'] >= a.min_cos and r['top1_agree'] >= a.min_top1 and r['gen_common_prefix'] >= min(a.min_gen_prefix, max(r['gen_len']))
    r['pass'] = ok
    print(json.dumps(r) if a.json else ' '.join('{}={}'.format(k, v) for k, v in r.items()))
    return 0 if ok else 1


if __name__ == '__main__':
    sys.exit(main())
