#!/usr/bin/env python3
"""Model-free rehearsal of the interim adapters: every decision with positive AND negative
controls, so none of them can pass vacuously (V020-INTERIM-OVERNIGHT-PLAN, I0). MIT.

  rehearse_interim.py --out DIR [--with-sysfs]     writes DIR/rehearsal.json; exit 0 = all behaved

--with-sysfs adds a READ-ONLY thermal inventory of this host (types + paths; no load).
"""
import argparse
import json
import math
from pathlib import Path
import random
import sys
import tempfile

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import keepwarm_fixture as kw  # noqa: E402
import soak_slope as ss  # noqa: E402
import thermal_aa as th  # noqa: E402
import tool_counters as tc  # noqa: E402

GIB = 1024 ** 3
MIB = 1024 ** 2
TOOLS = [{'type': 'function', 'function': {'name': 'read', 'parameters': {
    'type': 'object', 'properties': {'path': {'type': 'string'}, 'limit': {'type': 'integer'}}, 'required': ['path']}}},
    {'type': 'function', 'function': {'name': 'bash', 'parameters': {
        'type': 'object', 'properties': {'command': {'type': 'string'}, 'mode': {'type': 'string', 'enum': ['fg', 'bg']}},
        'required': ['command']}}}]


def call(cid, name, args):
    return {'id': cid, 'type': 'function', 'function': {'name': name, 'arguments': args if isinstance(args, str) else json.dumps(args)}}


def wire(*responses):
    return [{'request_tools': TOOLS, 'response': r} for r in responses]


def counters_controls():
    out = []
    valid = wire({'tool_calls': [call('a', 'read', {'path': 'x'})]}, {'tool_calls': [call('b', 'bash', {'command': 'ls'})]})
    r = tc.count(valid, {'a': True, 'b': True})
    out.append(('counters.valid', r['valid'] == 2 and r['malformed'] == 0 and r['repaired'] == 0, r))
    bad = wire({'tool_calls': [call('c', 'read', '{"path": ')]}, {'tool_calls': [call('d', 'grep', {'q': 1})]},
               {'tool_calls': [call('e', 'read', {'limit': 3})]}, {'tool_calls': [call('f', 'read', {'path': 7})]},
               {'tool_calls': [call('g', 'bash', {'command': 'ls', 'mode': 'sideways'})]}, {'tool_calls': [call('h', 'read', '[1]')]})
    r = tc.count(bad, {})
    want = {'args_not_json': 1, 'unknown_tool': 1, 'missing_required': 1, 'wrong_type': 1, 'bad_enum': 1, 'args_not_object': 1}
    out.append(('counters.malformed_reasons', r['malformed'] == 6 and r['valid'] == 0 and r['malformed_reasons'] == want, r))
    rep = wire({'tool_calls': [call('k', 'read', '{"path": "x",}')]}, {'content': 'done'})
    r = tc.count(rep, tc.oxa_outcomes([{'type': 'tool_execution_end', 'toolCallId': 'k', 'toolName': 'read', 'isError': False}]))
    out.append(('counters.repaired', r['malformed'] == 1 and r['repaired'] == 1, r))
    fb = wire({'content': 'I will call <tool_call>\n<function=read>\n<parameter=path>x</parameter></function></tool_call>'},
              {'content': 'ok'})
    r = tc.count(fb, {})
    out.append(('counters.text_fallback', r['text_fallback'] == 1 and r['malformed'] == 1 and r['repair_turns'] == 0, r))
    err = wire({'tool_calls': [call('m', 'read', '{"path": ')]}, {'tool_calls': [call('n', 'read', {'path': 'x'})]})
    r = tc.count(err, {'m': False, 'n': True})
    out.append(('counters.repair_turn_after_error_result', r['repair_turns'] == 1 and r['repaired'] == 0, r))
    r = tc.count(rep, {'k': True})
    out.append(('counters.repaired_call_no_repair_turn', r['repaired'] == 1 and r['repair_turns'] == 0, r))
    r = tc.count(rep, tc.oxa_outcomes([]))
    out.append(('counters.empty_client_stream_is_null', r['repaired'] is None and r['repair_turns'] is None, r))
    r = tc.count(valid, None)
    out.append(('counters.no_client_source_is_null', r['repaired'] is None and r['legit_tool_failures'] is None, r))
    r = tc.count(None)
    out.append(('counters.no_wire_is_null', r['attempts'] is None and r['malformed'] is None, r))
    oc = tc.opencode_outcomes([{'part': {'type': 'tool', 'callID': 'a', 'state': {'status': 'completed'}}},
                               {'part': {'type': 'tool', 'callID': 'b', 'state': {'status': 'error'}}}])
    r = tc.count(valid, oc)
    out.append(('counters.legit_failure_not_malformed', r['legit_tool_failures'] == 1 and r['malformed'] == 0, r))
    r = tc.count(valid, tc.oxa_outcomes([{'type': 'tool_execution_end', 'toolCallId': 'a', 'toolName': 'read'}]))
    out.append(('counters.missing_isError_is_null', r['legit_tool_failures'] is None, r))
    return out


def series(start, values, step=1.0):
    return [dict(t=start + i * step, soc=v, skin=None) for i, v in enumerate(values)]


def thermal_controls():
    ref = dict(soc=48.0, skin=None)
    out = []
    v, why = th.start_ok(series(0, [48.5] * 32), ref)
    out.append(('thermal.start_pass', v == 'PASS', why))
    v, why = th.start_ok(series(0, [61.0] * 32), dict(soc=60.0))
    out.append(('thermal.too_hot_waits', v == 'WAIT', why))
    v, why = th.start_ok(series(0, [48.0] * 20 + [50.5] * 12), ref)
    out.append(('thermal.outside_band_waits', v == 'WAIT', why))
    v, why = th.start_ok(series(0, [48.0] * 10), ref)
    out.append(('thermal.not_sustained_waits', v == 'WAIT', why))
    v, why = th.start_ok(series(0, [48.0] * 20 + [None] + [48.0] * 11), ref)
    out.append(('thermal.missing_soc_incomplete', v == 'INCOMPLETE', why))
    v, why = th.start_ok(series(0, [48.0] * 32), None)
    out.append(('thermal.no_reference_incomplete', v == 'INCOMPLETE', why))
    v, why = th.start_ok(series(0, [48.0] * 32), dict(soc=48.0, skin=35.0), has_skin=True)
    out.append(('thermal.skin_unreadable_incomplete', v == 'INCOMPLETE', why))
    good = [(10.0, 10.05), (10.0, 9.98), (10.2, 10.25)]
    r = th.aa_with_retry(good)
    out.append(('aa.pass', r['verdict'] == 'PASS' and r['speed_claim_allowed'], r))
    r = th.aa_with_retry([(10.0, 10.4), (10.0, 10.5), (10.0, 10.3)])
    out.append(('aa.median_out_fails', r['verdict'] == 'FAIL' and not r['speed_claim_allowed'], r))
    r = th.aa_with_retry([(10.0, 10.0), (10.0, 10.0), (10.0, 11.0)])
    out.append(('aa.spread_out_fails', r['verdict'] == 'FAIL', r))
    r = th.aa_with_retry([(10.0, 10.4), (10.0, 10.5), (10.0, 10.3)], good)
    out.append(('aa.retry_pass_keeps_original', r['verdict'] == 'PASS_AFTER_RETRY' and r['original']['verdict'] == 'FAIL', r))
    r = th.aa_with_retry([(10.0, 10.4), (10.0, 10.5), (10.0, 10.3)], [(10.0, 10.6), (10.0, 10.5), (10.0, 10.4)])
    out.append(('aa.repeated_miss_no_claim', r['verdict'] == 'FAIL' and not r['speed_claim_allowed'], r))
    r = th.aa_with_retry([(10.0, None), (10.0, 10.0), (10.0, 10.0)])
    out.append(('aa.incomplete_pair', r['verdict'] == 'INCOMPLETE', r))
    return out


def soak(span_min=60, anchors=8, slope_mib_h=0.0, ctl_slope_mib_h=0.0, noise_mib=0.0, pss=True, pid_change=False,
         anchor_key=lambda i: 'ctx-A', seed=7):
    rnd = random.Random(seed)
    rows = []
    t0 = 0.0
    every = span_min * 60 / (anchors - 1)
    for i in range(anchors):
        for j in range(5):   # 5 samples at 5 s: the sealed 20 s dwell (+0..+20)
            t = t0 + i * every + j * 5
            h = t / 3600
            eng = 18 * GIB + slope_mib_h * h * MIB + rnd.gauss(0, noise_mib) * MIB
            ctl = 300 * MIB + ctl_slope_mib_h * h * MIB + rnd.gauss(0, noise_mib / 4) * MIB
            oxa = 120 * MIB
            pid = 100 + (1 if pid_change and i >= anchors // 2 else 0)
            rows.append(dict(t=t, anchor=anchor_key(i), pids=dict(engine=pid, controller=200, oxa=300),
                             rss=dict(engine=eng, controller=ctl, oxa=oxa),
                             pss=dict(engine=eng * 0.9 if pss or i < 2 else None, controller=ctl * 0.9, oxa=oxa * 0.9)))
        last = rows[-1]   # between anchors: a non-anchor sample (workload running)
        rows.append(dict(t=last['t'] + 5, anchor=None, pids=last['pids'], rss=last['rss'], pss=last['pss']))
    return rows


def soak_controls():
    out = []
    r = ss.analyse(soak())
    out.append(('soak.flat_pass', r['verdict'] == 'PASS', r))
    r = ss.analyse(soak(noise_mib=40, slope_mib_h=50))
    out.append(('soak.noisy_pass', r['verdict'] == 'PASS', r))
    r = ss.analyse(soak(slope_mib_h=1100))
    out.append(('soak.growing_1GiB_h_fails', r['verdict'] == 'FAIL', r))
    r = ss.analyse(soak(ctl_slope_mib_h=200))
    out.append(('soak.controller_oxa_growth_fails', r['verdict'] == 'FAIL' and any('ctl_' in x for x in r['reasons']), r))
    r = ss.analyse(soak(pss=False))
    out.append(('soak.missing_pss_incomplete', r['verdict'] == 'INCOMPLETE', r))
    r = ss.analyse(soak(pid_change=True))
    out.append(('soak.restart_incomplete', r['verdict'] == 'INCOMPLETE', r))
    r = ss.analyse(soak(anchors=4))
    out.append(('soak.too_few_anchors_incomplete', r['verdict'] == 'INCOMPLETE', r))
    r = ss.analyse(soak(span_min=20))
    out.append(('soak.short_span_incomplete', r['verdict'] == 'INCOMPLETE', r))
    r = ss.analyse(soak(anchor_key=lambda i: 'ctx-A' if i % 2 else 'ctx-B'))
    out.append(('soak.unmatched_anchors_incomplete', r['verdict'] == 'INCOMPLETE', r))
    idle = [dict(t=float(t), pids=dict(engine=4242), loaded=True, reloads=0) for t in range(0, 615, 5)]
    v, why = ss.keepwarm_idle(idle)
    out.append(('keepwarm.same_pid_pass', v == 'PASS', why))
    v, why = ss.keepwarm_idle([dict(r, pids=dict(engine=4243)) if r['t'] > 300 else r for r in idle])
    out.append(('keepwarm.pid_change_fails', v == 'FAIL', why))
    v, why = ss.keepwarm_idle([dict(r, reloads=1) if r['t'] > 300 else r for r in idle])
    out.append(('keepwarm.reload_fails', v == 'FAIL', why))
    v, why = ss.keepwarm_idle(idle[:60])
    out.append(('keepwarm.short_incomplete', v == 'INCOMPLETE', why))
    return out


def keepwarm_policy_controls():
    out = []
    settings = dict(long_idle_s=7200, mem_floor_bytes=int(1.5 * GIB), psi_full_max=2.0)
    r = kw.run(kw.reference_decide, settings)
    out.append(('policy.reference_pass', r['verdict'] == 'PASS', r['verdict']))
    r = kw.run(lambda i, s: 'keep', settings)
    out.append(('policy.never_unload_fails', r['verdict'] == 'FAIL', [x['scenario'] for x in r['rows'] if not x['ok']]))
    r = kw.run(lambda i, s: 'unload_idle' if i['idle_s'] >= 600 else 'keep', settings)
    out.append(('policy.old_600s_rule_fails', r['verdict'] == 'FAIL', [x['scenario'] for x in r['rows'] if not x['ok']]))
    r = kw.run(lambda i, s: kw.reference_decide(dict(i, active_requests=0, queued_requests=0), s), settings)
    out.append(('policy.unload_while_busy_fails', r['verdict'] == 'FAIL', [x['scenario'] for x in r['rows'] if not x['ok']]))
    with tempfile.TemporaryDirectory() as d:
        p = Path(d) / 's.json'
        p.write_text(json.dumps(settings))
        rc = kw.main(['--policy', 'no_such_module:decide', '--settings', str(p), '--out', str(Path(d) / 'r.json')])
        out.append(('policy.missing_policy_blocked', rc == 3 and json.loads((Path(d) / 'r.json').read_text())['verdict'] == 'BLOCKED', rc))
        p.write_text(json.dumps(dict(long_idle_s=7200)))
        rc = kw.main(['--reference', '--settings', str(p), '--out', str(Path(d) / 'r2.json')])
        out.append(('policy.missing_thresholds_blocked', rc == 3, rc))
    return out


def rehearse(with_sysfs=False):
    controls = counters_controls() + thermal_controls() + soak_controls() + keepwarm_policy_controls()
    rows = [dict(control=n, behaved=bool(ok), detail=d) for n, ok, d in controls]
    receipt = dict(all_behaved=all(r['behaved'] for r in rows), controls=rows)
    if with_sysfs:
        receipt['thermal_inventory'] = th.inventory()
    return receipt


def main(argv=None):
    ap = argparse.ArgumentParser()
    ap.add_argument('--out', required=True)
    ap.add_argument('--with-sysfs', action='store_true')
    a = ap.parse_args(argv)
    r = rehearse(a.with_sysfs)
    Path(a.out).mkdir(parents=True, exist_ok=True)
    (Path(a.out) / 'rehearsal.json').write_text(json.dumps(r, indent=1, default=str))
    for row in r['controls']:
        print('{:44s} {}'.format(row['control'], 'ok' if row['behaved'] else 'MISBEHAVED'))
    print('INTERIM ADAPTER REHEARSAL: ' + ('PASS' if r['all_behaved'] else 'FAIL'))
    return 0 if r['all_behaved'] else 1


if __name__ == '__main__':
    sys.exit(main())
