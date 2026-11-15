#!/usr/bin/env python3
"""Smart keep-warm (O-160) POLICY input fixture for the interim batch. MIT.

The interim plan exercises pressure-driven unload ONLY through an isolated policy-input fixture:
synthetic inputs go to the candidate's keep-warm POLICY function; the real safety guards keep
reading the real host (no memory is ever allocated, no guard threshold is changed). If the
candidate exposes no injectable policy function, the subcase is BLOCKED (never a fake PASS).

Policy contract the fixture expects (the candidate binds it with --policy module:function):
    decide(inputs: dict, settings: dict) -> 'keep' | 'unload_pressure' | 'unload_idle'
inputs:   mem_available_bytes, mem_total_bytes, psi_some_avg10, psi_full_avg10 (percent),
          idle_s (since the last request), active_requests, queued_requests
settings: long_idle_s (default 7200), and the candidate's OWN pressure thresholds, read from its
          configuration by the runner (--settings JSON), never invented by this fixture

Scenarios (SCENARIOS below; expected decisions under the documented O-160 semantics):
  keep while free RAM allows; unload only on memory pressure (MemAvailable below the floor or
  PSI full above its threshold) when nothing is running, or after a long idle (2 h); never unload
  with an active or queued request.

  keepwarm_fixture.py --policy pkg.module:decide --settings settings.json [--out receipt.json]
  keepwarm_fixture.py --reference --settings settings.json   (the fixture's own reference policy:
                                                               proves the scenarios are non-vacuous)
"""
import argparse
import importlib
import json
from pathlib import Path
import sys

GIB = 1024 ** 3


def scenarios(s):
    """Scenario list derived from the candidate's thresholds s (floor_bytes, psi_full_max)."""
    floor, psi = s['mem_floor_bytes'], s['psi_full_max']
    base = dict(mem_total_bytes=30 * GIB, psi_some_avg10=0.0, psi_full_avg10=0.0, active_requests=0, queued_requests=0)
    rows = [
        ('roomy_short_idle', dict(base, mem_available_bytes=floor + 8 * GIB, idle_s=600), 'keep'),
        ('roomy_610s_idle', dict(base, mem_available_bytes=floor + 8 * GIB, idle_s=610), 'keep'),
        ('roomy_just_below_long_idle', dict(base, mem_available_bytes=floor + 8 * GIB, idle_s=s['long_idle_s'] - 1), 'keep'),
        ('long_idle', dict(base, mem_available_bytes=floor + 8 * GIB, idle_s=s['long_idle_s']), 'unload_idle'),
        ('mem_below_floor_idle', dict(base, mem_available_bytes=floor - 1, idle_s=30), 'unload_pressure'),
        ('psi_full_high_idle', dict(base, mem_available_bytes=floor + 4 * GIB, psi_full_avg10=psi + 1, idle_s=30), 'unload_pressure'),
        ('mem_below_floor_but_active', dict(base, mem_available_bytes=floor - 1, idle_s=0, active_requests=1), 'keep'),
        ('psi_high_but_queued', dict(base, mem_available_bytes=floor + 4 * GIB, psi_full_avg10=psi + 1, idle_s=5, queued_requests=1), 'keep'),
        ('long_idle_but_active', dict(base, mem_available_bytes=floor + 8 * GIB, idle_s=s['long_idle_s'] + 10, active_requests=1), 'keep'),
        ('at_floor_exactly', dict(base, mem_available_bytes=floor, idle_s=30), 'keep'),
    ]
    return rows


def reference_decide(inputs, settings):
    """The documented O-160 semantics (fixture self-check only, never the candidate)."""
    if inputs['active_requests'] or inputs['queued_requests']:
        return 'keep'
    if inputs['mem_available_bytes'] < settings['mem_floor_bytes'] or inputs['psi_full_avg10'] > settings['psi_full_max']:
        return 'unload_pressure'
    if inputs['idle_s'] >= settings['long_idle_s']:
        return 'unload_idle'
    return 'keep'


def run(decide, settings):
    rows, ok = [], True
    for name, inputs, want in scenarios(settings):
        try:
            got = decide(dict(inputs), dict(settings))
        except Exception as e:  # a crashing policy is a FAIL with its reason, never a skip
            got = 'error:%s' % type(e).__name__
        rows.append(dict(scenario=name, expected=want, got=got, ok=got == want))
        ok &= got == want
    return dict(verdict='PASS' if ok else 'FAIL', rows=rows)


def load_policy(spec):
    mod, _, fn = spec.partition(':')
    try:
        return getattr(importlib.import_module(mod), fn)
    except (ImportError, AttributeError) as e:
        return None


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    ap.add_argument('--policy', help='module:function of the candidate keep-warm policy')
    ap.add_argument('--reference', action='store_true')
    ap.add_argument('--settings', required=True, help='JSON: long_idle_s, mem_floor_bytes, psi_full_max (the candidate own values)')
    ap.add_argument('--out')
    a = ap.parse_args(argv)
    settings = json.loads(Path(a.settings).read_text())
    missing = [k for k in ('long_idle_s', 'mem_floor_bytes', 'psi_full_max') if k not in settings]
    if missing:
        receipt = dict(verdict='BLOCKED', reason='settings missing %s (must come from the candidate config)' % missing)
    elif a.reference:
        receipt = dict(run(reference_decide, settings), policy='fixture-reference')
    else:
        decide = load_policy(a.policy) if a.policy else None
        receipt = (dict(run(decide, settings), policy=a.policy) if decide else
                   dict(verdict='BLOCKED', reason='no injectable keep-warm policy function in the candidate (%s)' % a.policy))
    text = json.dumps(receipt, indent=1)
    if a.out:
        Path(a.out).write_text(text)
    print(text)
    return {'PASS': 0, 'FAIL': 1}.get(receipt['verdict'], 3)


if __name__ == '__main__':
    sys.exit(main())
