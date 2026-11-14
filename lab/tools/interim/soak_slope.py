#!/usr/bin/env python3
"""Interim soak memory-growth analyser + same-PID / matched-anchor checks (V020-INTERIM plan). MIT.

Input: JSON-lines samples {t, anchor, pids: {component: pid}, rss: {component: bytes},
pss: {component: bytes}} every 5 s during the soak; `anchor` is the matched context/cache-
occupancy key of a fixed-workload cycle point (null between anchors).

Per anchor occurrence: the median of its samples (>= the anchor's samples over ~20 s). Slope per
hour by Theil-Sen over the anchor medians, for the whole stack and for controller+oxa:
  stack RSS <= 512 MiB/h AND PSS <= 256 MiB/h; controller+oxa RSS <= 128 AND PSS <= 64 MiB/h;
  endpoint growth <= max(64 MiB, RSS limit x span h) and <= max(32 MiB, PSS limit x span h).
INCOMPLETE (never PASS, never bridged): < 5 anchors, span < 30 min, missing PSS for a component,
a PID change of any component (restart/unload epoch), unmatched anchors (not every anchor
occurrence carries the same anchor key set).
"""
import json
import statistics
import sys

MIB = 1024 * 1024
LIMITS = dict(stack_rss=512, stack_pss=256, ctl_rss=128, ctl_pss=64)   # MiB per hour
ENDPOINT_MIN = dict(rss=64, pss=32)                                      # MiB
MIN_ANCHORS = 5
MIN_SPAN_S = 30 * 60
CTL_COMPONENTS = ('controller', 'oxa')
REQUIRED_COMPONENTS = ('engine', 'controller', 'oxa')   # every observation must carry all three
SAMPLE_S = 5.0          # soak sampling cadence
ANCHOR_DWELL_S = 20.0   # median over the samples of a ~20 s dwell at each matched anchor
MIN_ANCHOR_SAMPLES = 5  # the sealed 20 s dwell at 5 s: samples at +0/+5/+10/+15/+20
MAX_ANCHOR_GAP_S = 1.5 * SAMPLE_S


def theil_sen(points):
    """Median of pairwise slopes (units per second). points: [(t, v)]."""
    slopes = [(v2 - v1) / (t2 - t1) for i, (t1, v1) in enumerate(points) for (t2, v2) in points[i + 1:] if t2 != t1]
    return statistics.median(slopes) if slopes else None


def same_pid(samples):
    """(ok, detail): every component keeps ONE pid over all samples (no restart/unload epoch),
    and every sample names the SAME component set (a vanished pid key fails closed)."""
    seen = {}
    comps = None
    for s in samples:
        keys = set((s.get('pids') or {}))
        if comps is None:
            comps = keys
        elif keys != comps:
            return False, 'component set changed: %s vs %s' % (sorted(comps), sorted(keys))
        for comp, pid in (s.get('pids') or {}).items():
            if pid is None:
                return False, 'pid missing for %s' % comp
            seen.setdefault(comp, set()).add(pid)
    changed = {c: sorted(p) for c, p in seen.items() if len(p) > 1}
    if not seen:
        return False, 'no pids'
    return (not changed), ('same pids' if not changed else 'pid change: %s' % changed)


def anchor_medians(samples):
    """[(t_mid, anchor, {component: rss}, {component: pss})] per contiguous anchor occurrence."""
    runs, cur = [], None
    for s in samples:
        a = s.get('anchor')
        if a is None:
            cur = None
            continue
        if cur is None or cur['anchor'] != a:
            cur = dict(anchor=a, rows=[])
            runs.append(cur)
        cur['rows'].append(s)
    out = []
    for r in runs:
        comps = set().union(*(set(x.get('rss') or {}) for x in r['rows']))
        rss = {c: statistics.median([x['rss'][c] for x in r['rows'] if c in (x.get('rss') or {})]) for c in comps}
        pss = {}
        for c in comps:
            vals = [(x.get('pss') or {}).get(c) for x in r['rows']]
            # fail closed: ANY anchor sample without PSS for a component makes that anchor's PSS None
            pss[c] = None if any(v is None for v in vals) else statistics.median(vals)
        if any(set(x.get('rss') or {}) != comps for x in r['rows']):
            rss = {c: None for c in comps}   # a sample missing a component's RSS: unmeasured
        out.append((statistics.median([x['t'] for x in r['rows']]), r['anchor'], rss, pss))
    return out


def observation_problems(samples, required=REQUIRED_COMPONENTS):
    """Fail-closed validation at ONE boundary: every anchor sample carries every required
    component's pid, RSS and PSS as finite numbers, timestamps are finite and strictly increasing,
    and every anchor occurrence dwells >= ANCHOR_DWELL_S (observed span) with >= MIN_ANCHOR_SAMPLES
    samples and no gap above MAX_ANCHOR_GAP_S."""
    import math
    probs = []
    ts = [s.get('t') for s in samples]
    if any(not isinstance(t, (int, float)) or isinstance(t, bool) or not math.isfinite(t) for t in ts):
        return ['bad timestamp']
    if any(b <= a for a, b in zip(ts, ts[1:])):
        probs.append('timestamps not strictly increasing')
    for s in samples:
        if s.get('anchor') is None:
            continue
        if set(s.get('rss') or {}) != set(s.get('pss') or {}):
            probs.append('rss and pss name different components')
        for kind in ('pids', 'rss', 'pss'):
            got = s.get(kind) or {}
            missing = [c for c in required if c not in got or got[c] is None]
            if missing:
                probs.append('%s missing for %s' % (kind, ','.join(missing)))
            elif kind != 'pids' and any(isinstance(got[c], bool) or not isinstance(got[c], (int, float))
                                        or not math.isfinite(got[c]) for c in required):
                probs.append('%s not finite' % kind)
    runs, cur = [], None
    for s in samples:
        a = s.get('anchor')
        if a is None:
            cur = None
            continue
        if cur is None or cur['anchor'] != a:
            cur = dict(anchor=a, t=[])
            runs.append(cur)
        cur['t'].append(s['t'])
    for r in runs:
        t = r['t']
        gaps = [b - a for a, b in zip(t, t[1:])]
        if len(t) < MIN_ANCHOR_SAMPLES or t[-1] - t[0] < ANCHOR_DWELL_S or (gaps and max(gaps) > MAX_ANCHOR_GAP_S):
            probs.append('anchor dwell short or gapped (%d samples over %.0f s)' % (len(t), t[-1] - t[0]))
            break
    return sorted(set(probs))


def analyse(samples, required=REQUIRED_COMPONENTS):
    res = dict(verdict=None, reasons=[], slopes_mib_h={}, endpoints_mib={}, anchors=0, span_min=None)
    res['reasons'] += ['INCOMPLETE:' + p for p in observation_problems(samples, required)]
    ok_pid, why = same_pid(samples)
    if not ok_pid:
        res['reasons'].append('INCOMPLETE:' + why)
    meds = anchor_medians(samples)
    res['anchors'] = len(meds)
    if len(meds) < MIN_ANCHORS:
        res['reasons'].append('INCOMPLETE:fewer than %d anchors' % MIN_ANCHORS)
    if meds:
        span = meds[-1][0] - meds[0][0]
        res['span_min'] = round(span / 60, 1)
        if span < MIN_SPAN_S:
            res['reasons'].append('INCOMPLETE:anchor span < 30 min')
        keys = {m[1] for m in meds}
        comps = [set(m[2]) for m in meds]
        if len(keys) != 1 or any(c != comps[0] for c in comps):
            res['reasons'].append('INCOMPLETE:unmatched anchors or components')
        if any(v is None for m in meds for v in m[3].values()):
            res['reasons'].append('INCOMPLETE:missing PSS')
        if any(v is None for m in meds for v in m[2].values()):
            res['reasons'].append('INCOMPLETE:missing RSS')
    if res['reasons']:
        res['verdict'] = 'INCOMPLETE'
        return res
    span_h = (meds[-1][0] - meds[0][0]) / 3600
    groups = dict(stack=None, ctl=CTL_COMPONENTS)
    fails = []
    for g, comps in groups.items():
        for kind in ('rss', 'pss'):
            idx = 2 if kind == 'rss' else 3
            pts = [(m[0], sum(v for c, v in m[idx].items() if comps is None or c in comps)) for m in meds]
            slope = theil_sen(pts) * 3600 / MIB
            end = (pts[-1][1] - pts[0][1]) / MIB
            limit = LIMITS['%s_%s' % (g, kind)]
            res['slopes_mib_h']['%s_%s' % (g, kind)] = round(slope, 2)
            res['endpoints_mib']['%s_%s' % (g, kind)] = round(end, 2)
            if slope > limit:
                fails.append('%s_%s slope %.1f > %d MiB/h' % (g, kind, slope, limit))
            if end > max(ENDPOINT_MIN[kind], limit * span_h):
                fails.append('%s_%s endpoint %.1f MiB > bound' % (g, kind, end))
    res['verdict'] = 'FAIL' if fails else 'PASS'
    res['reasons'] = fails
    return res


KEEPWARM_MAX_GAP_S = 30


def keepwarm_idle(samples, idle_s=610, max_gap_s=KEEPWARM_MAX_GAP_S):
    """Smart keep-warm idle observation: over >= idle_s of idle, the SAME resident engine pid and no
    reload. samples: [{t, pids: {engine: pid}, loaded: bool, reloads: int}], at most max_gap_s apart.
    Fail closed: a missing / non-integer reload counter or `loaded` flag, or a sampling gap, is
    INCOMPLETE (an unobserved interval can hide an unload + reload)."""
    if not samples:
        return 'INCOMPLETE', 'no samples'
    if any(type(s.get('reloads')) is not int or type(s.get('loaded')) is not bool for s in samples):
        return 'INCOMPLETE', 'reload counter or loaded flag missing on a sample'
    gaps = [b['t'] - a['t'] for a, b in zip(samples, samples[1:])]
    if gaps and max(gaps) > max_gap_s:
        return 'INCOMPLETE', 'sampling gap %.0f s > %d s' % (max(gaps), max_gap_s)
    span = samples[-1]['t'] - samples[0]['t']
    if span < idle_s:
        return 'INCOMPLETE', 'observed %.0f s < %d s' % (span, idle_s)
    pids = {(s.get('pids') or {}).get('engine') for s in samples}
    if None in pids:
        return 'INCOMPLETE', 'engine pid missing'
    if len(pids) != 1:
        return 'FAIL', 'engine pid changed %s' % sorted(pids)
    if not all(s.get('loaded') for s in samples):
        return 'FAIL', 'model unloaded during idle'
    if len({s.get('reloads') for s in samples}) != 1:
        return 'FAIL', 'reload counter moved'
    return 'PASS', 'same pid, resident, no reload over %.0f s' % span


def main(argv=None):
    path = (sys.argv[1:] if argv is None else argv)[0]
    with open(path) as f:
        samples = [json.loads(l) for l in f if l.strip()]
    print(json.dumps(analyse(samples), indent=1))
    return 0


if __name__ == '__main__':
    sys.exit(main())
