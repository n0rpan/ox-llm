#!/usr/bin/env python3
"""Real-weights oracle: oxe against llama.cpp on a small dense Qwen3.5 GGUF.

  oracle_tiny.py --dump-ref BIN --oxe BIN --model FILE --out DIR [--threads 4] [--gen 48]

llama.cpp's dump_ref (engine/tools/oracle) runs a fixed chat prompt one token per llama_decode and
continues greedily, writing the logits of every evaluated position. `oxe verify` replays the same
tokens through oxe's decode path and compares every logit row. Fails when the top-1 agreement is
below --min-top1, the mean KL divergence above --max-mean-kld, or the largest absolute logit
difference above --max-abs-diff times the largest reference logit, and warns when a logit row is
not bit-exact. Writes report.json and the markdown summary. Standard library only.

Measured on the first run (llama.cpp b11898, Q8_0, 4 threads, hosted ARM64): 81 of 81 logit rows
bit-exact, greedy continuation identical. The bounds leave room for a last-bit change of a kernel,
not for a different computation.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import time

# Thinking off, as the e2e job runs the model (Qwen3.5 chat template, generation prompt).
PROMPT = (
    '<|im_start|>system\nYou are a helpful assistant.<|im_end|>\n'
    '<|im_start|>user\nName three primary colours and say one thing about each.<|im_end|>\n'
    '<|im_start|>assistant\n<think>\n\n</think>\n\n'
)


def run(cmd, **kw):
    print('+', ' '.join(str(c) for c in cmd), flush=True)
    t0 = time.monotonic()
    p = subprocess.run(cmd, check=True, **kw)
    return p, time.monotonic() - t0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--dump-ref', required=True)
    ap.add_argument('--oxe', required=True)
    ap.add_argument('--model', required=True)
    ap.add_argument('--out', required=True)
    ap.add_argument('--threads', type=int, default=4)
    ap.add_argument('--gen', type=int, default=48)
    ap.add_argument('--min-top1', type=float, default=0.99)
    ap.add_argument('--max-mean-kld', type=float, default=1e-4)
    ap.add_argument('--max-abs-diff', type=float, default=1e-3)
    a = ap.parse_args()
    out = Path(a.out)
    ref = out / 'ref'
    out.mkdir(parents=True, exist_ok=True)
    _, t_ref = run([a.dump_ref, '--model', a.model, '--out', ref, '--prompt', PROMPT,
                    '--logits', '--gen', str(a.gen), '--threads', str(a.threads), '--ctx', '1024'])
    cache = out / 'cache'
    cache.mkdir(mode=0o700, exist_ok=True)
    p, t_oxe = run([a.oxe, 'verify', '--model', a.model, '--ref', ref, '--threads', str(a.threads),
                    '--ctx', '1024', '--cache-dir', cache], stdout=subprocess.PIPE, text=True)
    v = json.loads(p.stdout)
    rel = v['max_abs_logit_diff'] / max(v['max_abs_ref_logit'], 1e-30)
    checks = [
        ('positions compared', v['positions_with_logits'] > 0, v['positions_with_logits'], '> 0'),
        ('top-1 agreement', v['top1_agreement'] >= a.min_top1, v['top1_agreement'], f'>= {a.min_top1}'),
        ('mean KL divergence', v['mean_kld'] <= a.max_mean_kld, v['mean_kld'], f'<= {a.max_mean_kld}'),
        ('max |logit diff| / max |ref logit|', rel <= a.max_abs_diff, rel, f'<= {a.max_abs_diff}'),
    ]
    report = {'verify': v, 'relative_max_abs_diff': rel, 'dump_ref_seconds': t_ref,
              'oxe_verify_seconds': t_oxe, 'checks': [
                  {'name': n, 'ok': ok, 'value': val, 'bound': b} for n, ok, val, b in checks]}
    (out / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
    lines = ['## oxe vs llama.cpp (Qwen3.5-0.8B)', '',
             f'{v["prompt_tokens"]} prompt tokens + {v["generated_ref"]} greedy tokens; '
             f'greedy identical: {v["greedy_identical"]} (first divergence: {v["first_divergence"]}); '
             f'bit-exact logit rows: {v["bit_exact_logit_rows"]} of {v["positions_with_logits"]}; '
             f'max KLD {v["max_kld"]:.3g}', '',
             '| check | value | bound | |', '|---|---|---|---|']
    lines += [f'| {n} | {val:.4g} | {b} | {"ok" if ok else "FAIL"} |' for n, ok, val, b in checks]
    md = '\n'.join(lines) + '\n'
    print(md)
    summary = os.environ.get('GITHUB_STEP_SUMMARY')
    if summary:
        with open(summary, 'a') as f:
            f.write(md)
    if v['bit_exact_logit_rows'] != v['positions_with_logits']:
        print(f"::warning::oracle: {v['bit_exact_logit_rows']} of {v['positions_with_logits']} logit rows "
              'bit-exact with llama.cpp (all were on the first run)', flush=True)
    bad = [n for n, ok, _, _ in checks if not ok]
    for n in bad:
        print(f'::error::oracle: {n} out of bounds', flush=True)
    return 1 if bad else 0


if __name__ == '__main__':
    sys.exit(main())
