#!/usr/bin/env python3
"""Negative controls for the fixture library on a REAL (tiny-real) ladder (O-158 rehearsal). MIT.

Each control copies a complete ladder (or one rung) to a private directory, breaks one thing and
runs the library's own verification; the control PASSES only when verification FAILS with the
expected problem. Online controls restart the engine through NativeHandle (bare oxe-server on a
temporary port; no controller, no lease).

  fixture_negatives.py --ladder L [--bench-mtp FILE] --native-server BIN --native-model GGUF
                       [--native-weight-cache DIR] --out DIR

  missing_block      a referenced KV block deleted              -> offline FAIL (store_missing_referenced_files)
  identity_changed   rung.json cache identity edited            -> offline FAIL (identity_differs_from_rung_json)
  depth_changed      rung.json token hash / count edited        -> offline FAIL (token_hash_mismatch)
  bench_mtp_kind     an `oxe bench --state-file` (kind=bench-mtp) file in place of a snapshot
                                                                -> offline FAIL (bad_state_file: magic)
  resume_loss        snapshot payloads corrupted, headers intact (offline-valid) -> online FAIL (not restored)
  control_intact     the untouched copy                          -> online PASS (positive control)
Writes OUT/negatives.json; exit 0 when every control behaved as required.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile

HERE = Path(__file__).resolve()
LAB = HERE.parents[2]
sys.path[:0] = [str(LAB / 'benchmarks/fixtures'), str(LAB / 'benchmarks/gatepack')]
import fixture_lib as fx  # noqa: E402
import ladder as lad  # noqa: E402
from native_handle import NativeHandle  # noqa: E402


def copy_ladder(src, dst):
    shutil.copytree(src, dst, copy_function=shutil.copy2, symlinks=True)
    for lock in dst.rglob('.lock'):
        lock.unlink()
    return dst


def rung_model_dir(rung):
    engine = fx.store_binding(rung / 'store')
    return rung / 'store' / 'state' / engine['model_key']


def first_rung(ladder):
    return ladder / 'rungs' / fx.read_json(ladder / 'ladder.json')['rungs'][0]


def offline(rung):
    ok, problems = fx.verify_offline(rung)
    return ok, problems


def main(argv=None):
    ap = argparse.ArgumentParser()
    ap.add_argument('--ladder', type=Path, required=True)
    ap.add_argument('--bench-mtp', type=Path, help='a kind=bench-mtp state file (oxe bench --state-file)')
    ap.add_argument('--native-server', type=Path, required=True)
    ap.add_argument('--native-model', type=Path, required=True)
    ap.add_argument('--native-weight-cache', type=Path)
    ap.add_argument('--native-port', type=int, default=18797)
    ap.add_argument('--out', type=Path, required=True)
    a = ap.parse_args(argv)
    a.out.mkdir(parents=True, exist_ok=True)
    work = Path(tempfile.mkdtemp(prefix='fx-neg-', dir=os.environ.get('TMPDIR')))
    results = {}

    def expect(name, ok, problems, want):
        hit = (not ok) and any(want in p for p in problems)
        results[name] = dict(control='negative', verdict_ok=ok, problems=problems, expected=want,
                             behaved=hit)

    try:
        # ---- offline negatives
        lad1 = copy_ladder(a.ladder, work / 'missing_block')
        rung = first_rung(lad1)
        md = rung_model_dir(rung)
        block = next(p for p in sorted(md.rglob('*.kv')))
        block.unlink()
        expect('missing_block', *offline(rung), 'store_missing_referenced_files')

        lad2 = copy_ladder(a.ladder, work / 'identity_changed')
        rung = first_rung(lad2)
        meta = fx.read_json(rung / 'rung.json')
        meta['binding']['engine']['cache_identity'] = 'f' * 64
        fx.write_json(rung / 'rung.json', meta)
        expect('identity_changed', *offline(rung), 'identity_differs_from_rung_json')

        lad3 = copy_ladder(a.ladder, work / 'depth_changed')
        rung = first_rung(lad3)
        meta = fx.read_json(rung / 'rung.json')
        meta['token']['token_hash'] = '0' * 64
        meta['token']['n_tokens'] = meta['token']['n_tokens'] - 256
        fx.write_json(rung / 'rung.json', meta)
        expect('depth_changed', *offline(rung), 'token_hash_mismatch')

        if a.bench_mtp:
            lad4 = copy_ladder(a.ladder, work / 'bench_mtp_kind')
            rung = first_rung(lad4)
            md = rung_model_dir(rung)
            snap = next(p for p in sorted(md.rglob('*.st')))
            os.unlink(snap)
            shutil.copy(a.bench_mtp, snap)  # same name, bench-mtp bytes: never converted, never accepted
            expect('bench_mtp_kind', *offline(rung), 'bad_state_file')
        else:
            results['bench_mtp_kind'] = dict(control='negative', behaved=False, problems=['not_run_no_bench_mtp_file'])

        # ---- online: resume loss (snapshots gone) and the intact positive control
        handle = NativeHandle(a.native_server, a.native_model, work / 'ledger', alias=fx.read_json(a.ladder / 'ladder.json')['alias'],
                              port=a.native_port, weight_cache=a.native_weight_cache)
        lad5 = copy_ladder(a.ladder, work / 'resume_loss')
        rung = first_rung(lad5)
        for st in rung_model_dir(rung).rglob('*.st'):
            # headers and lengths intact (offline-valid), payload corrupted: the engine's Check64
            # must refuse the snapshots at restore, so the canary cannot be restored
            size = st.stat().st_size
            with open(st, 'r+b') as f:
                for off in range(fx.HDR + 4096, size, max(4096, (size - fx.HDR) // 64)):
                    f.seek(off)
                    b = f.read(1)
                    f.seek(off)
                    f.write(bytes([b[0] ^ 0xFF]))
        off_ok, off_probs = offline(rung)
        results['resume_loss_offline_valid'] = dict(control='precondition', verdict_ok=off_ok, problems=off_probs, behaved=off_ok)
        r = lad.verify(handle, lad5, work / 'scratch5', labels=[rung.name], online=True, log=lambda *_: None)
        probs = r['summary']['problems']
        results['resume_loss'] = dict(control='negative', verdict_ok=r['ok'], problems=probs,
                                      behaved=not r['ok'] and bool(probs))
        lad6 = copy_ladder(a.ladder, work / 'control_intact')
        r = lad.verify(handle, lad6, work / 'scratch6', online=True, log=lambda *_: None)
        results['control_intact'] = dict(control='positive', verdict_ok=r['ok'], problems=r['summary']['problems'],
                                         rungs={k: dict(cache_n=v.get('cache_n'), prompt_tokens=v.get('prompt_tokens'),
                                                        resume_src=v.get('resume_src')) for k, v in r['rungs'].items()},
                                         behaved=r['ok'])
    finally:
        shutil.rmtree(work, ignore_errors=True)
    ok = all(v.get('behaved') for v in results.values())
    (a.out / 'negatives.json').write_text(json.dumps(dict(ladder=str(a.ladder), all_behaved=ok, controls=results), indent=1))
    for k, v in results.items():
        print('{:16s} {:8s} behaved={} problems={}'.format(k, v.get('control', ''), v.get('behaved'), (v.get('problems') or [])[:3]))
    print('FIXTURE NEGATIVES: ' + ('PASS' if ok else 'FAIL'))
    return 0 if ok else 1


if __name__ == '__main__':
    sys.exit(main())
