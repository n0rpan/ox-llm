#!/usr/bin/env python3
"""T1.5 hop-breakdown probe (v0.2.0 opt L6, item #8 / section 5 "Budget"). MIT.

Optional. Runs only when `tools/gates/tiny_real.sh` is invoked with HOP_PROBE=1. It adds no compute:

- `proxy`: a transparent logging HTTP proxy in front of the temporary oxe-server. It forwards every
  request and streams the reply back unchanged. Per request it writes one metadata line to
  `requests.jsonl` (times, sizes, the engine's own `timings`, the speculative counters). Nothing from
  the content goes there. With `--transcript` it also appends `transcript.jsonl` (request body and
  reply text) for lab/benchmarks/tokens/token_harness.py; that file holds gate-run content and is kept
  only in the temporary directory unless the gate is told to copy it.
- `summarize`: joins the proxy records, the oxa RPC events (written by `oxa_rpc_tee.py`) and the
  server's own `oxe-server: ... in N ms` log lines into one receipt: per-hop medians of prefill
  (engine prompt_ms), decode (predicted_ms), engine overhead, client + tool time (gap between one
  response's end and the next request's start), tool execution (sum of oxa `tool_execution_end`
  duration_ms) and client-side time = gaps - tools.

Times are wall clock on this machine; the probe never touches production (no port 8093, no lease).
The gate removes its temporary tree at exit; the receipt is printed to stdout or `--out`.

usage: hop_probe.py proxy --listen PORT --upstream PORT --dir DIR [--transcript]
       hop_probe.py summarize --dir DIR [--server-log LOG ...] [--rpc FILE] [--out FILE]
"""
import argparse
import http.client
import http.server
import json
import os
from pathlib import Path
import re
import statistics
import sys
import threading
import time

SCHEMA = 'v020-t15-hop-probe/v1'
HOP_BY_HOP = {'connection', 'keep-alive', 'proxy-connection', 'transfer-encoding', 'upgrade', 'te', 'trailer',
              'content-length', 'host'}
MAX_CAPTURE = 64 << 20
LOG_PATTERNS = {
    'resume': re.compile(r'state cache: resumed (\d+) tokens .*? in ([\d.]+) ms'),
    'preload': re.compile(r'state cache: preloaded (\d+) tokens .*? in ([\d.]+) ms'),
    'load': re.compile(r'oxe-server: (.*?): loaded (\d+) tokens in ([\d.]+) ms'),
    'session_park': re.compile(r'parked a session of (\d+) tokens .*? in ([\d.]+) ms'),
}


# ---- pure helpers (unit-tested on synthetic data) -------------------------------------------------

def timings_from_text(text):
    """The last `timings` object in a JSON body or an SSE stream (None when absent)."""
    try:
        whole = json.loads(text)
        if isinstance(whole, dict) and isinstance(whole.get('timings'), dict):
            return whole['timings']
    except ValueError:
        pass
    found = None
    for obj in iter_json_objects(text):
        if isinstance(obj, dict) and isinstance(obj.get('timings'), dict):
            found = obj['timings']
    return found


def iter_json_objects(text):
    for line in text.splitlines():
        s = line[5:].strip() if line.startswith('data:') else line.strip()
        if not s or s == '[DONE]' or not s.startswith('{'):
            continue
        try:
            yield json.loads(s)
        except ValueError:
            continue


def reply_from_text(text):
    """Reply content, tool calls and usage from a chat completion body or SSE stream."""
    content, reasoning, tools, usage, streamed = [], [], {}, None, False
    whole = None
    try:
        whole = json.loads(text)
    except ValueError:
        whole = None
    if isinstance(whole, dict) and 'choices' in whole:
        msg = (whole.get('choices') or [{}])[0].get('message') or {}
        content.append(msg.get('content') or '')
        if isinstance(msg.get('reasoning_content'), str):
            reasoning.append(msg['reasoning_content'])
        for i, tc in enumerate(msg.get('tool_calls') or []):
            tools[i] = tc
        usage = whole.get('usage')
    else:
        streamed = True
        for obj in iter_json_objects(text):
            if not isinstance(obj, dict):
                continue
            if obj.get('usage'):
                usage = obj['usage']
            for ch in obj.get('choices') or []:
                delta = ch.get('delta') or {}
                if delta.get('content'):
                    content.append(delta['content'])
                if isinstance(delta.get('reasoning_content'), str):
                    reasoning.append(delta['reasoning_content'])
                for tc in delta.get('tool_calls') or []:
                    slot = tools.setdefault(tc.get('index', len(tools)), {'type': 'function', 'function': {'name': '', 'arguments': ''}})
                    if tc.get('id'):
                        slot['id'] = tc['id']
                    fn = tc.get('function') or {}
                    slot['function']['name'] += fn.get('name') or ''
                    slot['function']['arguments'] += fn.get('arguments') or ''
    reply = {'content': ''.join(content)}
    if reasoning:   # kept, never dropped: the generated state includes the reasoning tokens
        reply['reasoning_content'] = ''.join(reasoning)
    if tools:
        reply['tool_calls'] = [tools[k] for k in sorted(tools)]
    return {'reply': reply, 'usage': usage or {}, 'streamed': streamed}


def parse_server_log(lines):
    out = {k: [] for k in LOG_PATTERNS}
    for line in lines:
        for key, pat in LOG_PATTERNS.items():
            m = pat.search(line)
            if not m:
                continue
            if key == 'load':
                out[key].append({'what': m.group(1), 'tokens': int(m.group(2)), 'ms': float(m.group(3))})
            elif key == 'resume' or key == 'preload' or key == 'session_park':
                out[key].append({'tokens': int(m.group(1)), 'ms': float(m.group(2))})
    return out


def parse_rpc_events(lines):
    """oxa RPC records (oxa-rpc/src/events.rs, pi camelCase): one JSON object per line with `type`.

    tool_execution_end: `toolName`, `isError`, and `durationMs` only when oxa measured it (absent
    otherwise, so a missing duration is reported, never read as 0). prefill (oxa's own event):
    `done`, `total`, `secs`. compaction_end: `reason`, `aborted`, `willRetry`; the RPC records
    carry no timestamps, so compaction time is not derived here (the count is).
    """
    tools, compactions, prefill_secs, n = [], 0, [], 0
    for line in lines:
        line = line.strip()
        if not line.startswith('{'):
            continue
        try:
            ev = json.loads(line)
        except ValueError:
            continue
        n += 1
        kind = ev.get('type')
        if kind == 'tool_execution_end':
            tools.append({'tool': ev.get('toolName'), 'duration_ms': ev.get('durationMs'),
                          'is_error': ev.get('isError')})
        elif kind == 'compaction_end':
            compactions += 1
        elif kind == 'prefill' and isinstance(ev.get('secs'), (int, float)):
            prefill_secs.append(float(ev['secs']))
    timed = [t['duration_ms'] for t in tools if isinstance(t.get('duration_ms'), (int, float))]
    return {'events': n, 'tools': tools, 'tool_timed': len(timed), 'tool_ms_total': sum(timed),
            'compactions': compactions, 'prefill_secs_total': sum(prefill_secs) if prefill_secs else None,
            'prefill_events': len(prefill_secs)}


def median(values):
    vals = [float(v) for v in values if v is not None]
    return float(statistics.median(vals)) if vals else None


def hop_summary(records, rpc=None, server_log=None):
    """records: proxy metadata dicts for chat requests in order (see Proxy.record)."""
    rpc = rpc or {'tools': [], 'compactions': 0, 'tool_timed': 0, 'tool_ms_total': 0.0}
    gaps = [r['gap_ms'] for r in records if r.get('gap_ms') is not None]
    tool_total = float(rpc.get('tool_ms_total') or 0.0)
    notes = []
    if rpc['tools'] and rpc.get('tool_timed', 0) < len(rpc['tools']):
        notes.append('%d of %d tool events carry no durationMs: tool time is partial' %
                     (len(rpc['tools']) - rpc.get('tool_timed', 0), len(rpc['tools'])))
    partial = bool(rpc['tools']) and rpc.get('tool_timed', 0) < len(rpc['tools'])
    if rpc['tools'] and tool_total == 0.0:
        # a 0 here while tool events exist would move all of the gap time into client_only: refuse it
        notes.append('TOOL-TIME-ZERO: tool events exist but no durationMs; client/tool split is not measured')
        client_total = None
    elif partial:
        # any untimed tool: the gap minus a PARTIAL tool sum is not client time (fail closed)
        notes.append('TOOL-TIME-PARTIAL: client/tool split withheld (client_only null); known tool time is a lower bound')
        client_total = None
    else:
        client_total = max(0.0, sum(gaps) - tool_total) if gaps else None
    per = []
    for r in records:
        t = r.get('timings') or {}
        prefill, decode = t.get('prompt_ms'), t.get('predicted_ms')
        overhead = None
        if r.get('wall_ms') is not None and prefill is not None and decode is not None:
            overhead = max(0.0, r['wall_ms'] - float(prefill) - float(decode))
        draft_n, acc = t.get('draft_n'), t.get('draft_n_accepted')
        per.append({'status': r.get('status'), 'prefill_ms': prefill, 'decode_ms': decode,
                    'prompt_n': t.get('prompt_n'), 'cache_n': t.get('cache_n'), 'predicted_n': t.get('predicted_n'),
                    'ttfb_ms': r.get('ttfb_ms'), 'wall_ms': r.get('wall_ms'), 'gap_ms': r.get('gap_ms'),
                    'engine_overhead_ms': overhead,
                    'mtp_acceptance': (acc / draft_n) if draft_n else None})
    return {
        'schema': SCHEMA,
        'requests': len(records),
        'medians': {
            'prefill_ms': median([p['prefill_ms'] for p in per]),
            'decode_ms': median([p['decode_ms'] for p in per]),
            'engine_overhead_ms': median([p['engine_overhead_ms'] for p in per]),
            'ttfb_ms': median([p['ttfb_ms'] for p in per]),
            'wall_ms': median([p['wall_ms'] for p in per]),
            'gap_ms': median(gaps),
            'mtp_acceptance': median([p['mtp_acceptance'] for p in per]),
        },
        'totals_ms': {
            'prefill': sum(float(p['prefill_ms'] or 0) for p in per),
            'decode': sum(float(p['decode_ms'] or 0) for p in per),
            'gaps_client_and_tool': sum(gaps) if gaps else None,
            'tool_execution': tool_total if rpc['tools'] and tool_total > 0 and not partial else None,
            'tool_execution_known_partial': tool_total if partial and tool_total > 0 else None,
            'client_only': client_total,
        },
        'tool_calls': len(rpc['tools']),
        'compactions': rpc.get('compactions', 0),
        'prefill_secs_total': rpc.get('prefill_secs_total'),
        'server_log': server_log or {},
        'per_request': per,
        'notes': notes + ([] if rpc.get('events') else ['no oxa RPC events: tool and client split not measured']),
    }


# ---- proxy -------------------------------------------------------------------------------------

EPOCH_PATH = '/_hop_probe/epoch'   # proxy-local: resets the gap chain, never forwarded


class State:
    def __init__(self, out_dir, upstream, transcript):
        self.out_dir = Path(out_dir)
        self.upstream = upstream
        self.transcript = transcript
        self.lock = threading.Lock()
        self.last_end = None
        self.seq = 0

    def write(self, name, obj):
        with self.lock:
            with open(self.out_dir / name, 'a') as f:
                f.write(json.dumps(obj, ensure_ascii=False) + '\n')


def make_handler(state):
    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = 'HTTP/1.1'
        server_version = 'hop-probe'

        def log_message(self, fmt, *args):  # quiet: the gate's logs stay clean
            return

        def _forward(self):
            t_recv = time.time()
            length = int(self.headers.get('Content-Length') or 0)
            body = self.rfile.read(length) if length else b''
            if self.path.split('?')[0] == EPOCH_PATH:
                # the launcher marks a server restart / client start: the next chat gap would
                # otherwise carry that wall time as "client + tool"
                with state.lock:
                    state.last_end = None
                self.send_response(204)
                self.send_header('Content-Length', '0')
                self.send_header('Connection', 'close')
                self.end_headers()
                self.close_connection = True
                return
            is_chat = self.command == 'POST' and self.path.split('?')[0].endswith('/chat/completions')
            with state.lock:
                # chat -> chat only: /health, GETs and other paths neither open nor close a gap
                gap = (t_recv - state.last_end) * 1e3 if (is_chat and state.last_end) else None
                state.seq += 1
                seq = state.seq
            headers = {k: v for k, v in self.headers.items() if k.lower() not in HOP_BY_HOP}
            conn = http.client.HTTPConnection('127.0.0.1', state.upstream, timeout=3600)
            status, captured, streamed_out, headers_sent = 502, bytes(), False, False
            try:
                conn.request(self.command, self.path, body=body or None, headers=headers)
                resp = conn.getresponse()
                t_head = time.time()
                status = resp.status
                self.send_response(status)
                for k, v in resp.getheaders():
                    if k.lower() not in HOP_BY_HOP:
                        self.send_header(k, v)
                clen = resp.getheader('Content-Length')
                if clen is not None:
                    data = resp.read()
                    captured = data[:MAX_CAPTURE]
                    self.send_header('Content-Length', str(len(data)))
                    self.send_header('Connection', 'close')
                    self.end_headers()
                    headers_sent = True
                    self.wfile.write(data)
                else:
                    streamed_out = True
                    self.send_header('Transfer-Encoding', 'chunked')
                    self.send_header('Connection', 'close')
                    self.end_headers()
                    headers_sent = True
                    while True:
                        chunk = resp.read1(65536)
                        if not chunk:
                            break
                        if len(captured) < MAX_CAPTURE:
                            captured += chunk[:MAX_CAPTURE - len(captured)]
                        self.wfile.write(b'%x\r\n%s\r\n' % (len(chunk), chunk))
                        self.wfile.flush()
                    self.wfile.write(b'0\r\n\r\n')
            except (OSError, http.client.HTTPException) as e:
                if not headers_sent:
                    try:
                        msg = json.dumps({'error': 'upstream: %s' % type(e).__name__}).encode()
                        self.send_response(502)
                        self.send_header('Content-Length', str(len(msg)))
                        self.send_header('Connection', 'close')
                        self.end_headers()
                        self.wfile.write(msg)
                    except OSError:
                        pass
                t_head = time.time()
            finally:
                conn.close()
                t_end = time.time()
                with state.lock:
                    if is_chat:
                        # an upstream failure ends the chain: the next gap is unmeasured, not inflated
                        state.last_end = t_end if status < 500 else None
                self.close_connection = True
            text = captured.decode('utf-8', 'replace')
            rec = {'seq': seq, 'method': self.command, 'path': self.path.split('?')[0], 'status': status,
                   'req_bytes': len(body), 'resp_bytes': None, 'gap_ms': gap,
                   'ttfb_ms': (t_head - t_recv) * 1e3, 'wall_ms': (t_end - t_recv) * 1e3,
                   'streamed': streamed_out, 'timings': timings_from_text(text)}
            state.write('requests.jsonl', rec)
            if state.transcript and self.command == 'POST' and rec['path'].endswith('/chat/completions') and status == 200:
                try:
                    req = json.loads(body.decode('utf-8'))
                except ValueError:
                    req = None
                if isinstance(req, dict):
                    got = reply_from_text(text)
                    state.write('transcript.jsonl', {'seq': seq, 'body': req, 'reply': got['reply'], 'usage': got['usage']})

        do_GET = do_POST = do_PUT = do_DELETE = _forward

    return Handler


def run_proxy(listen, upstream, out_dir, transcript):
    Path(out_dir).mkdir(parents=True, exist_ok=True, mode=0o700)
    state = State(out_dir, upstream, transcript)
    srv = http.server.ThreadingHTTPServer(('127.0.0.1', listen), make_handler(state))
    srv.daemon_threads = True
    print('hop-probe proxy 127.0.0.1:%d -> 127.0.0.1:%d' % (listen, upstream), flush=True)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        srv.server_close()


def cmd_summarize(a):
    recs = []
    p = Path(a.dir) / 'requests.jsonl'
    if p.exists():
        for line in p.read_text().splitlines():
            if line.strip():
                recs.append(json.loads(line))
    chat = [r for r in recs if r.get('method') == 'POST' and str(r.get('path', '')).endswith('/chat/completions')]
    rpc = None
    if a.rpc and Path(a.rpc).exists():
        rpc = parse_rpc_events(Path(a.rpc).read_text().splitlines())
    logs = {}
    for lp in a.server_log:
        if Path(lp).exists():
            for k, v in parse_server_log(Path(lp).read_text(errors='replace').splitlines()).items():
                logs.setdefault(k, []).extend(v)
    result = hop_summary(chat, rpc, logs)
    text = json.dumps(result, indent=2, sort_keys=True)
    if a.out:
        Path(a.out).write_text(text + '\n')
    print(text)
    return 0


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    sub = ap.add_subparsers(dest='cmd', required=True)
    pp = sub.add_parser('proxy')
    pp.add_argument('--listen', type=int, required=True)
    pp.add_argument('--upstream', type=int, required=True)
    pp.add_argument('--dir', required=True)
    pp.add_argument('--transcript', action='store_true')
    ps = sub.add_parser('summarize')
    ps.add_argument('--dir', required=True)
    ps.add_argument('--server-log', action='append', default=[])
    ps.add_argument('--rpc', default='')
    ps.add_argument('--out', default='')
    a = ap.parse_args(argv)
    if a.cmd == 'proxy':
        run_proxy(a.listen, a.upstream, a.dir, a.transcript)
        return 0
    return cmd_summarize(a)


if __name__ == '__main__':
    sys.exit(main())
