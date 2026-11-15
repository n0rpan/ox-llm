#!/usr/bin/env python3
"""Interim thermal start condition + A/A noise control (V020-INTERIM-OVERNIGHT-PLAN). MIT.

Sensors (bound by TYPE + PATH, never by zone index alone): every readable thermal zone whose type
starts with `cpu` or `cpuss`; the SoC value is their max. A zone typed like a skin sensor
(`skin`, `*-skin`, `skin_therm`) is used if present; battery, PMIC and SSD zones are never skin.
Missing SoC telemetry blocks timing (INCOMPLETE).

Start rule: max SoC <= 60 C AND within +/-2 C of the locked cool reference, held for 30 s
(1 s samples); with a real skin sensor also <= 40 C and within +/-2 C of its reference.
A/A rule: 3 pairs, median(A2/A1) in [0.98, 1.02] and every pair's relative spread
|A2-A1|/mean <= 5 %; one complete retry, the original kept; a repeated miss = no speed claim.

  thermal_aa.py inventory                     read-only sensor inventory (JSON)
  thermal_aa.py sample --seconds N --out F    1 s samples of the bound sensors (JSON lines)
"""
import json
import os
from pathlib import Path
import statistics
import sys
import time

SOC_MAX_C = 60.0
SKIN_MAX_C = 40.0
BAND_C = 2.0
STABLE_S = 30
AA_RANGE = (0.98, 1.02)
AA_SPREAD = 0.05
MAX_GAP_S = 1.5   # 1 s sampling with jitter


def _num(v):
    import math
    return isinstance(v, (int, float)) and not isinstance(v, bool) and math.isfinite(v)


NOT_SKIN = ('battery', 'batt', 'pmic', 'pm8', 'pmk', 'pmc', 'ssd', 'nvme', 'charger', 'usb')


def inventory(root='/sys/class/thermal'):
    zones = []
    for z in sorted(Path(root).glob('thermal_zone*')):
        try:
            typ = (z / 'type').read_text().strip()
            temp = int((z / 'temp').read_text().strip()) / 1000.0
        except (OSError, ValueError):
            continue
        zones.append(dict(path=str(z / 'temp'), type=typ, c=temp))
    soc = [z for z in zones if z['type'].lower().startswith(('cpu', 'cpuss'))]
    skin = [z for z in zones if 'skin' in z['type'].lower() and not any(n in z['type'].lower() for n in NOT_SKIN)]
    return dict(soc=[(z['type'], z['path']) for z in soc], skin=[(z['type'], z['path']) for z in skin],
                skin_reason=None if skin else 'no named skin sensor exposed (battery/PMIC/SSD are not skin)',
                zones=len(zones))


def read_bound(binding):
    """One sample {soc, skin} from the bound (type, path) pairs; a type change = unbound (None)."""
    def val(pairs):
        vals = []
        for typ, path in pairs:
            try:
                if Path(path).with_name('type').read_text().strip() != typ:
                    return None
                vals.append(int(Path(path).read_text().strip()) / 1000.0)
            except (OSError, ValueError):
                return None
        return max(vals) if vals else None
    return dict(t=time.time(), soc=val(binding['soc']), skin=val(binding['skin']) if binding.get('skin') else None)


def start_ok(samples, reference, has_skin=False):
    """Verdict for the 30 s pre-arm window. samples: [{t, soc, skin}] at ~1 s, newest last.
    reference: {soc, skin} locked once. Returns (verdict, why): PASS / WAIT / INCOMPLETE."""
    if reference is None or not _num(reference.get('soc')):
        return 'INCOMPLETE', 'no finite locked reference'
    if has_skin and not _num(reference.get('skin')):
        return 'INCOMPLETE', 'skin sensor bound but no finite skin reference'
    if not samples or any(not _num(s.get('soc')) or not _num(s.get('t')) for s in samples):
        return 'INCOMPLETE', 'missing or non-finite SoC telemetry / timestamp'
    if has_skin and any(not _num(s.get('skin')) for s in samples):
        return 'INCOMPLETE', 'skin sensor bound but a skin sample is missing or non-finite'
    ts = [s['t'] for s in samples]
    if any(b <= a for a, b in zip(ts, ts[1:])):
        return 'INCOMPLETE', 'timestamps not strictly increasing'
    if samples[-1]['t'] - samples[0]['t'] < STABLE_S:
        return 'WAIT', 'less than 30 s of samples'
    # the last 30 s must be continuously observed at ~1 s: the window starts at or before
    # (last - 30 s) and no gap inside it exceeds MAX_GAP_S
    window = [s for s in samples if s['t'] >= samples[-1]['t'] - STABLE_S - MAX_GAP_S]
    wt = [s['t'] for s in window]
    if wt[0] > samples[-1]['t'] - STABLE_S or any(b - a > MAX_GAP_S for a, b in zip(wt, wt[1:])):
        return 'INCOMPLETE', 'the 30 s window is not continuously sampled at 1 s'
    for s in window:
        if s['soc'] > SOC_MAX_C:
            return 'WAIT', 'SoC %.1f C > %.0f C' % (s['soc'], SOC_MAX_C)
        if abs(s['soc'] - reference['soc']) > BAND_C:
            return 'WAIT', 'SoC %.1f C outside reference %.1f +/- 2 C' % (s['soc'], reference['soc'])
        if has_skin:
            if s.get('skin') is None or reference.get('skin') is None:
                return 'INCOMPLETE', 'skin sensor bound but unreadable'
            if s['skin'] > SKIN_MAX_C or abs(s['skin'] - reference['skin']) > BAND_C:
                return 'WAIT', 'skin outside limit/band'
    return 'PASS', 'held 30 s within the band'


def aa_check(pairs):
    """pairs: [(a1_seconds, a2_seconds)] x 3. (verdict, detail)."""
    if len(pairs) != 3 or any(not _num(a) or not _num(b) or a <= 0 or b <= 0 for a, b in pairs):
        return 'INCOMPLETE', dict(reason='need 3 complete pairs')
    ratios = [b / a for a, b in pairs]
    spreads = [abs(b - a) / ((a + b) / 2) for a, b in pairs]
    med = statistics.median(ratios)
    ok = AA_RANGE[0] <= med <= AA_RANGE[1] and all(s <= AA_SPREAD for s in spreads)
    return ('PASS' if ok else 'FAIL'), dict(median_ratio=round(med, 4), spreads=[round(s, 4) for s in spreads])


def aa_with_retry(first, retry=None):
    """One complete retry allowed; the original is always reported."""
    v1, d1 = aa_check(first)
    if v1 == 'PASS' or retry is None:
        return dict(verdict=v1, original=d1, retry=None, speed_claim_allowed=v1 == 'PASS')
    v2, d2 = aa_check(retry)
    return dict(verdict='PASS_AFTER_RETRY' if v2 == 'PASS' else 'FAIL', original=dict(d1, verdict=v1),
                retry=dict(d2, verdict=v2), speed_claim_allowed=v2 == 'PASS')


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    if argv[:1] == ['inventory']:
        print(json.dumps(inventory(), indent=1))
        return 0
    if argv[:1] == ['sample']:
        import argparse
        ap = argparse.ArgumentParser()
        ap.add_argument('cmd')
        ap.add_argument('--seconds', type=int, default=30)
        ap.add_argument('--out', required=True)
        a = ap.parse_args(argv)
        inv = inventory()
        binding = dict(soc=inv['soc'], skin=inv['skin'])
        with open(a.out, 'a') as f:
            for _ in range(a.seconds):
                f.write(json.dumps(read_bound(binding)) + '\n')
                f.flush()
                time.sleep(1)
        return 0
    print(__doc__)
    return 2


if __name__ == '__main__':
    sys.exit(main())
