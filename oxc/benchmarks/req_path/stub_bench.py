#!/usr/bin/env python3
"""v0.2.0 opt L6: model-free request-path bench (items #8 / #12 pricing). MIT.

Starts `oxe-server --engine stub` (tokenizer + chat template only, no weights; the tokenizer is read
from a GGUF header) on a temporary port with a private HOME/XDG tree, then sends chat requests whose
bodies are about `--tokens` tokens (default 100000) and measures what the request path costs without
any model compute:

- `cold`: a fresh body each repetition (no prefix reuse, the whole body is parsed, rendered, tokenised);
- `append_echo_on` / `append_echo_off`: the previous turn's messages plus the assistant reply with its
  reasoning dropped by the client and one short user message; ECHO (restore of the dropped reasoning)
  on or off, so the difference prices the ECHO match.

Per request: client wall time, time to response headers, and the engine's own `timings` object (the
`render_ms / tokenize_ms / restore_ms / ckpt_ms / queue_ms` fields exist only after the v0.2.0 REQ-PATH
item; absent keys are recorded as null, never invented). Overhead = wall - (prompt_ms + predicted_ms)
when the engine reports those; with the stub the prefill/decode terms are ~0, so overhead ~ wall.

Median of n >= 5 per arm (`--n`, smaller only with `--allow-small` for smoke runs). Prints one JSON
receipt (stdout, or `--out`). Model-free: no weights, no lease, no production port (8093 refused).

usage: stub_bench.py --server-bin engine/target/release/oxe-server --model <tiny.gguf> [--tokens 100000]
       [--n 5] [--port 18795] [--out receipt.json]
"""
import argparse
import hashlib
import http.client
import json
import os
from pathlib import Path
import random
import shutil
import statistics
import subprocess
import sys
import tempfile
import time

SCHEMA = 'v020-req-path-stub-bench/v1'
ENGINE_TIMING_KEYS = ('render_ms', 'tokenize_ms', 'restore_ms', 'ckpt_ms', 'queue_ms', 'echo_ms', 'parse_ms')
DEFAULT_MODEL = '/var/lib/local-ai-models/tiny/Qwen3.5-2B-Q4_K_M-qwen35moe-8of16.gguf'
REFUSED_PORTS = (8093,)
STUB_REPLY = '<think>Plan the answer briefly before replying.</think>Hello from the oxe stub engine.'
_WORDS = ('alpha bravo cedar delta ember fjord garnet harbor iris juniper kestrel lantern meadow nickel '
          'orchid pebble quartz rivet saffron timber umber velvet willow xenon yarrow zephyr').split()


# ---- pure helpers (unit-tested on synthetic data) -------------------------------------------------

def median(values):
    vals = [float(v) for v in values if v is not None]
    if not vals:
        return None
    return float(statistics.median(vals))


def engine_timings(resp_json):
    """The engine's `timings` object as {key: number|None} for the known request-path keys."""
    t = resp_json.get('timings') if isinstance(resp_json, dict) else None
    if not isinstance(t, dict):
        return {}
    out = {k: t.get(k) for k in ('prompt_ms', 'predicted_ms', 'prompt_n', 'cache_n', 'predicted_n')}
    for k in ENGINE_TIMING_KEYS:
        out[k] = t.get(k) if isinstance(t.get(k), (int, float)) else None
    return out


def overhead_ms(wall_ms, timings):
    """Client wall minus the engine's prefill and decode terms (what the request path itself costs)."""
    parts = [timings.get('prompt_ms'), timings.get('predicted_ms')]
    if any(p is None for p in parts):
        return wall_ms
    return max(0.0, wall_ms - sum(float(p) for p in parts))


def summarize(samples):
    """samples: [{'wall_ms', 'ttfb_ms', 'timings': {...}}]. Returns medians and the raw series."""
    walls = [s['wall_ms'] for s in samples]
    over = [overhead_ms(s['wall_ms'], s.get('timings', {})) for s in samples]
    keys = {}
    for s in samples:
        for k, v in s.get('timings', {}).items():
            if v is not None:
                keys.setdefault(k, []).append(float(v))
    return {
        'n': len(samples),
        'median_wall_ms': median(walls),
        'min_wall_ms': min(walls) if walls else None,
        'max_wall_ms': max(walls) if walls else None,
        'median_overhead_ms': median(over),
        'median_ttfb_ms': median([s.get('ttfb_ms') for s in samples]),
        'median_engine_ms': {k: median(v) for k, v in sorted(keys.items())},
        'engine_keys_seen': sorted(k for k in keys if k in ENGINE_TIMING_KEYS),
        'wall_ms_series': [round(w, 3) for w in walls],
    }


def make_text(rng, chars):
    """Deterministic filler prose of about `chars` characters (lowercase words, sentence breaks)."""
    out, n = [], 0
    while n < chars:
        sent = ' '.join(rng.choice(_WORDS) for _ in range(rng.randint(6, 14))) + '. '
        out.append(sent)
        n += len(sent)
    return ''.join(out)[:chars]


def build_user_text(seed, chars, tag):
    rng = random.Random('req-path:%s:%s' % (seed, tag))
    return 'Benchmark document %s (seed %s). Summarise it in one sentence.\n%s' % (tag, seed, make_text(rng, chars))


# ---- HTTP + server --------------------------------------------------------------------------------

def post_json(port, path, obj, timeout=900):
    conn = http.client.HTTPConnection('127.0.0.1', port, timeout=timeout)
    try:
        body = json.dumps(obj).encode()
        t0 = time.perf_counter()
        conn.request('POST', path, body=body, headers={'Content-Type': 'application/json',
                                                        'Content-Length': str(len(body))})
        resp = conn.getresponse()
        ttfb = (time.perf_counter() - t0) * 1e3
        data = resp.read()
        wall = (time.perf_counter() - t0) * 1e3
        return resp.status, json.loads(data.decode() or 'null'), wall, ttfb
    finally:
        conn.close()


def wait_health(port, proc, timeout=120):
    t_end = time.time() + timeout
    while time.time() < t_end:
        if proc.poll() is not None:
            raise RuntimeError('oxe-server exited at start (code %s)' % proc.returncode)
        try:
            conn = http.client.HTTPConnection('127.0.0.1', port, timeout=2)
            conn.request('GET', '/health')
            if conn.getresponse().status == 200:
                return
        except OSError:
            pass
        time.sleep(0.5)
    raise RuntimeError('oxe-server not healthy in %d s' % timeout)


def start_server(server_bin, model, port, echo, ctx_size, env, log_path):
    cmd = [server_bin, '--engine', 'stub', '--model', model, '--alias', 'bench', '--host', '127.0.0.1',
           '--port', str(port), '--ctx-size', str(ctx_size), '--reasoning', 'on', '--reasoning-format',
           'deepseek', '--echo', echo, '--stub-reply', STUB_REPLY]
    log = open(log_path, 'w')
    proc = subprocess.Popen(cmd, stdout=log, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL,
                            env=env, start_new_session=True)
    return proc, log


def stop_server(proc, log):
    if proc.poll() is None:
        proc.terminate()
        try:
            proc.wait(timeout=60)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
    log.close()


def count_tokens(port, messages, tools=None):
    body = {'messages': messages, 'return_tokens': True}
    if tools:
        body['tools'] = tools
    status, j, _, _ = post_json(port, '/apply-template', body)
    if status != 200 or not isinstance(j, dict):
        raise RuntimeError('/apply-template failed: %s' % status)
    return len(j['tokens'])


def calibrate_chars_per_token(port, seed):
    sample = build_user_text(seed, 40000, 'calib')
    status, j, _, _ = post_json(port, '/tokenize', {'content': sample, 'add_special': False})
    if status != 200 or not j.get('tokens'):
        raise RuntimeError('/tokenize failed: %s' % status)
    return len(sample) / len(j['tokens'])


def build_body(port, seed, target_tokens, tag, ratio):
    """A one-user-message conversation of about target_tokens tokens (the template adds a few)."""
    chars = int(target_tokens * ratio)
    for _ in range(4):
        msgs = [{'role': 'system', 'content': 'You are a coding agent.\nBe brief.'},
                {'role': 'user', 'content': build_user_text(seed, chars, tag)}]
        n = count_tokens(port, msgs)
        if abs(n - target_tokens) <= target_tokens * 0.01:
            return msgs, n
        chars = int(chars * target_tokens / n)
    return msgs, n


def timed_request(port, model, msgs):
    req = {'model': model, 'messages': msgs, 'stream': False, 'max_tokens': 8, 'temperature': 0}
    status, j, wall, ttfb = post_json(port, '/v1/chat/completions', req)
    if status != 200:
        raise RuntimeError('chat request HTTP %s' % status)
    return j, wall, ttfb


def run_cold(port, model, seed, tokens, reps, ratio):
    """Fresh body per repetition: nothing to reuse, the whole body goes through the request path."""
    samples, body_tokens = [], []
    for i in range(reps):
        msgs, n = build_body(port, '%s-cold-%d' % (seed, i), tokens, 'cold-%d' % i, ratio)
        body_tokens.append(n)
        j, wall, ttfb = timed_request(port, model, msgs)
        samples.append({'wall_ms': wall, 'ttfb_ms': ttfb, 'timings': engine_timings(j)})
    return summarize(samples), body_tokens


def run_append(port, model, seed, tokens, reps, ratio):
    """Unmeasured seed turn, then `reps` measured turns, each one appended to the previous conversation."""
    msgs, _ = build_body(port, '%s-app-seed' % seed, tokens, 'app-seed', ratio)
    j, _, _ = timed_request(port, model, msgs)
    samples = []
    for i in range(reps):
        msgs = append_messages(msgs, j, i)
        j, wall, ttfb = timed_request(port, model, msgs)
        samples.append({'wall_ms': wall, 'ttfb_ms': ttfb, 'timings': engine_timings(j)})
    return summarize(samples)


def append_messages(prev_msgs, resp_json, rep):
    """Client view of the next turn: reasoning dropped, reply kept, one new short user message."""
    reply = resp_json['choices'][0]['message']['content'] if resp_json else ''
    reply = reply.split('</think>')[-1].strip() or 'ok'
    return prev_msgs + [{'role': 'assistant', 'content': reply},
                        {'role': 'user', 'content': 'Continue with step %d in one line.' % rep}]


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    ap.add_argument('--server-bin', default=str(Path(__file__).resolve().parents[3] / 'engine/target/release/oxe-server'))
    ap.add_argument('--model', default=os.environ.get('TINY_GGUF', DEFAULT_MODEL))
    ap.add_argument('--tokens', type=int, default=100000)
    ap.add_argument('--ctx-size', type=int, default=131072)
    ap.add_argument('--n', type=int, default=5)
    ap.add_argument('--allow-small', action='store_true', help='smoke runs only: allow --n below 5')
    ap.add_argument('--port', type=int, default=18795)
    ap.add_argument('--seed', default='1')
    ap.add_argument('--out', default='')
    a = ap.parse_args(argv)

    if a.n < 5 and not a.allow_small:
        ap.error('--n must be >= 5 (median of n >= 5); use --allow-small for smoke runs')
    if a.port in REFUSED_PORTS:
        ap.error('port %d is production' % a.port)
    for p in (a.server_bin, a.model):
        if not os.path.exists(p):
            ap.error('missing: %s' % p)

    work = tempfile.mkdtemp(prefix='req-path-bench-')
    env = dict(os.environ, HOME=os.path.join(work, 'home'), XDG_CONFIG_HOME=os.path.join(work, 'cfg'),
               XDG_STATE_HOME=os.path.join(work, 'state'), XDG_RUNTIME_DIR=os.path.join(work, 'run'),
               XDG_CACHE_HOME=os.path.join(work, 'cache'))
    for d in ('home', 'cfg', 'state', 'run', 'cache'):
        os.makedirs(os.path.join(work, d), mode=0o700, exist_ok=True)
    env.pop('LOCAL_LLM_CONFIG', None)
    env.pop('OXE_API_KEY_FILE', None)
    arms = {}
    proc, log = None, None
    try:
        proc, log = start_server(a.server_bin, a.model, a.port, 'on', a.ctx_size, env, os.path.join(work, 'server.log'))
        wait_health(a.port, proc)
        ratio = calibrate_chars_per_token(a.port, a.seed)

        arms['cold'], body_tokens = run_cold(a.port, a.model, a.seed, a.tokens, a.n, ratio)

        for echo in ('on', 'off'):
            stop_server(proc, log)
            proc, log = start_server(a.server_bin, a.model, a.port, echo, a.ctx_size, env,
                                     os.path.join(work, 'server-echo-%s.log' % echo))
            wait_health(a.port, proc)
            arms['append_echo_%s' % echo] = run_append(a.port, a.model, a.seed, a.tokens, a.n, ratio)

        result = {
            'schema': SCHEMA,
            'engine': 'stub',
            'server_bin_sha256_prefix': hashlib.sha256(Path(a.server_bin).read_bytes()).hexdigest()[:16],
            'body': {'target_tokens': a.tokens, 'tokens_per_rep': body_tokens, 'chars_per_token': round(ratio, 4)},
            'n': a.n,
            'cpus': os.cpu_count(),
            'arms': arms,
            'overhead_definition': 'median client wall minus engine prompt_ms + predicted_ms (wall when absent)',
        }
        text = json.dumps(result, indent=2, sort_keys=True)
        if a.out:
            Path(a.out).write_text(text + '\n')
        print(text)
        return 0
    finally:
        if proc is not None:
            stop_server(proc, log)
        shutil.rmtree(work, ignore_errors=True)


if __name__ == '__main__':
    sys.exit(main())
