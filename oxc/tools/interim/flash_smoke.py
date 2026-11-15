#!/usr/bin/env python3
"""Interim Flash smoke (V020-INTERIM-BATCH-BINDINGS flash_smoke): load Flash through the NEW
controller, one short chat, one cache append, unload. <= 15 min. MIT.

  flash_smoke.py --base-url http://127.0.0.1:<controller port> --out DIR
                 [--alias qwen3.8-flash-next-local] [--api-key-file F] [--no-controller]

Steps (each prints `FLASH SMOKE <step>: PASS|FAIL <why>`; the run is PASS only if all pass):
  S1 load      POST /_local/load {"model": alias} -> 200 {"state": "ready"} (wall recorded); then
               GET /_local/status names the alias as the loaded model when it reports one
  S2 chat      streamed, greedy, max_tokens 256: the answer's content parses as the JSON object
               {"ok": true, "n": 3}; finish_reason stop; usage present
  S3 append    the same conversation + the assistant reply VERBATIM (content + reasoning_content)
               + a new user turn: cached >= prev prompt + prev completion - 1 (C3, the sealed
               formula), the answer parses as {"ok": true, "n": 4}
  S4 unload    POST /_local/unload {"model": alias} -> 200 {"state": "unloaded"}; status then no
               longer names the alias as loaded
Through the controller only (its admission, guards, lease and Flash profile are untouched; this
tool never passes a flag or profile). --no-controller skips S1/S4 (rehearsal against a bare
engine). Loopback only; no prompt text is written, only sizes, numbers and verdicts.
"""
import argparse
import http.client
import json
from pathlib import Path
import re
import sys
import time
import urllib.parse

ALIAS = 'qwen3.8-flash-next-local'
SYSTEM = 'You answer with exactly one JSON object and nothing else.'
ASK1 = 'Reply with the JSON object {"ok": true, "n": 3} and nothing else.'
ASK2 = 'Now reply with the JSON object {"ok": true, "n": 4} and nothing else.'


class Client:
    def __init__(self, base, key=None, timeout=900):
        u = urllib.parse.urlparse(base)
        if u.hostname not in ('127.0.0.1', 'localhost', '::1'):
            raise SystemExit('loopback endpoints only')
        self.host, self.port, self.timeout = u.hostname, u.port, timeout
        self.headers = {'Content-Type': 'application/json', 'X-Local-AI-Origin': 'lab', 'X-Session-Id': 'interim-flash-smoke'}
        if key:
            self.headers['Authorization'] = 'Bearer ' + key

    def call(self, method, path, body=None):
        c = http.client.HTTPConnection(self.host, self.port, timeout=self.timeout)
        try:
            c.request(method, path, None if body is None else json.dumps(body).encode(), self.headers)
            r = c.getresponse()
            raw = r.read(16 * 1024 * 1024)
            try:
                return r.status, json.loads(raw or b'null')
            except ValueError:
                return r.status, None
        finally:
            c.close()

    def stream(self, body):
        body = dict(body, stream=True, stream_options={'include_usage': True})
        c = http.client.HTTPConnection(self.host, self.port, timeout=self.timeout)
        out = dict(status=None, done=False, finish=None, content='', reasoning='', usage=None, timings={})
        try:
            c.request('POST', '/v1/chat/completions', json.dumps(body).encode(), self.headers)
            r = c.getresponse()
            out['status'] = r.status
            if r.status != 200:
                r.read()
                return out
            for raw in r:
                line = raw.decode('utf-8', 'replace').strip()
                if not line.startswith('data:'):
                    continue
                data = line[5:].strip()
                if data == '[DONE]':
                    out['done'] = True
                    break
                try:
                    o = json.loads(data)
                except ValueError:
                    continue
                if o.get('usage'):
                    out['usage'] = o['usage']
                if isinstance(o.get('timings'), dict):
                    out['timings'].update(o['timings'])
                for ch in o.get('choices') or []:
                    d = ch.get('delta') or {}
                    out['content'] += d.get('content') or ''
                    out['reasoning'] += d.get('reasoning_content') or ''
                    if ch.get('finish_reason'):
                        out['finish'] = ch['finish_reason']
        finally:
            c.close()
        return out


def _ints(*vals):
    return all(isinstance(v, int) and not isinstance(v, bool) and v >= 0 for v in vals)


def parse_answer(content, n):
    text = re.sub(r'<think>.*?</think>', '', content or '', flags=re.S).strip()
    m = re.search(r'\{.*\}', text, re.S)
    if not m:
        return False, 'no JSON object in the answer'
    try:
        obj = json.loads(m.group(0))
    except ValueError:
        return False, 'answer JSON does not parse'
    ok = obj.get('ok') is True and obj.get('n') == n
    return ok, 'valid' if ok else 'JSON parsed but not {"ok": true, "n": %d}' % n


LOADED_STATES = ('ready', 'running', 'loaded')


def loaded_alias(status):
    """The alias the controller reports as LOADED, else None. The lab controller's /_local/status
    has top-level `state` + `model` (the configured alias, present even when idle), so the alias
    counts only while `state` is a loaded state."""
    if not isinstance(status, dict):
        return None
    if status.get('state') not in LOADED_STATES:   # missing state = not proven loaded
        return None
    for key in ('model', 'loaded_model', 'model_alias'):
        if isinstance(status.get(key), str):
            return status[key]
    chat = status.get('chat') or status.get('engine') or {}
    if isinstance(chat, dict):
        for key in ('model', 'loaded_model', 'model_alias'):
            if isinstance(chat.get(key), str):
                return chat[key]
    return None


def run(cl, alias, controller=True):
    steps, rec = [], {}

    def step(name, ok, why, **numbers):
        steps.append(dict(step=name, verdict='PASS' if ok else 'FAIL', why=why, **numbers))
        print('FLASH SMOKE %s: %s %s' % (name, 'PASS' if ok else 'FAIL', why), flush=True)
        return ok

    if controller:
        t0 = time.time()
        st, body = cl.call('POST', '/_local/load', {'model': alias})
        wall = round(time.time() - t0, 1)
        ok = st == 200 and isinstance(body, dict) and body.get('state') == 'ready'
        st2, status = cl.call('GET', '/_local/status')
        named = loaded_alias(status)
        if not ok:
            why = 'load answered %s %s' % (st, (body or {}).get('error') if isinstance(body, dict) else '')
        elif st2 != 200 or not isinstance(status, dict) or 'state' not in status:
            # fail closed: missing / partial status never confirms a load
            ok, why = False, 'status missing or partial after load (HTTP %s)' % st2
        elif named != alias:
            ok, why = False, 'status names %r loaded, not %r (state %r)' % (named, alias, status.get('state'))
        else:
            why = 'ready in %.1f s' % wall
        if not step('S1-load', ok, why, load_wall_s=wall, status_model=named):
            return steps
    msgs = [{'role': 'system', 'content': SYSTEM}, {'role': 'user', 'content': ASK1}]
    base = {'model': alias, 'temperature': 0, 'max_tokens': 256}
    r1 = cl.stream(dict(base, messages=msgs))
    good, why = parse_answer(r1['content'], 3)
    u1 = r1['usage'] or {}
    ok = (r1['status'] == 200 and r1['done'] and r1['finish'] == 'stop' and good
          and _ints(u1.get('prompt_tokens'), u1.get('completion_tokens')))
    step('S2-chat', ok, why if r1['status'] == 200 else 'HTTP %s' % r1['status'], prompt=u1.get('prompt_tokens'),
         completion=u1.get('completion_tokens'), finish=r1['finish'])
    if ok:
        assistant = {'role': 'assistant', 'content': r1['content']}
        if r1['reasoning']:
            assistant['reasoning_content'] = r1['reasoning']
        r2 = cl.stream(dict(base, messages=msgs + [assistant, {'role': 'user', 'content': ASK2}]))
        u2 = r2['usage'] or {}
        cached = (u2.get('prompt_tokens_details') or {}).get('cached_tokens', r2['timings'].get('cache_n'))
        need = (u1.get('prompt_tokens') or 0) + (u1.get('completion_tokens') or 0) - 1
        good2, why2 = parse_answer(r2['content'], 4)
        c3 = _ints(cached) and cached >= need
        ok2 = (r2['status'] == 200 and r2['done'] and r2['finish'] == 'stop' and good2 and c3
               and _ints(u2.get('prompt_tokens'), u2.get('completion_tokens')))
        step('S3-append', ok2, ('%s; cached %s vs C3 %d' % (why2, cached, need)) if r2['status'] == 200 else 'HTTP %s' % r2['status'],
             prompt=u2.get('prompt_tokens'), cached=cached, c3_need=need, resume_src=r2['timings'].get('resume_src'))
    if controller:
        st, body = cl.call('POST', '/_local/unload', {'model': alias})
        st2, status = cl.call('GET', '/_local/status')
        named = loaded_alias(status)
        status_ok = st2 == 200 and isinstance(status, dict) and isinstance(status.get('state'), str)
        ok = st == 200 and isinstance(body, dict) and body.get('state') == 'unloaded' and status_ok and named != alias
        why = ('unloaded' if ok else 'status missing or partial after unload (HTTP %s)' % st2 if not status_ok
               else 'unload answered %s, status model %r' % (st, named))
        step('S4-unload', ok, why)
    return steps


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    ap.add_argument('--base-url', required=True)
    ap.add_argument('--alias', default=ALIAS)
    ap.add_argument('--out', required=True)
    ap.add_argument('--api-key-file')
    ap.add_argument('--no-controller', action='store_true', help='rehearsal against a bare engine: S1/S4 skipped')
    ap.add_argument('--timeout', type=int, default=900)
    a = ap.parse_args(argv)
    key = Path(a.api_key_file).read_text().split('\n')[0].strip() if a.api_key_file else None
    t0 = time.time()
    steps = run(Client(a.base_url, key, a.timeout), a.alias, controller=not a.no_controller)
    want = 2 if a.no_controller else 4
    verdict = 'PASS' if len(steps) == want and all(s['verdict'] == 'PASS' for s in steps) else 'FAIL'
    Path(a.out).mkdir(parents=True, exist_ok=True)
    (Path(a.out) / 'flash-smoke.json').write_text(json.dumps(dict(verdict=verdict, alias=a.alias, controller=not a.no_controller,
                                                                  wall_s=round(time.time() - t0, 1), steps=steps), indent=1))
    print('FLASH SMOKE: %s (%d/%d steps)' % (verdict, sum(s['verdict'] == 'PASS' for s in steps), want))
    return 0 if verdict == 'PASS' else 1


if __name__ == '__main__':
    sys.exit(main())
