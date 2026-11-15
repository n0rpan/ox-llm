"""Recording OpenAI-compatible proxy for lab sessions (MIT): forwards every request to an upstream
server on 127.0.0.1 and records one JSONL row per chat request with sizes, hashes, usage/timing
numbers and wire tool-call verdicts only (no message text). Also the loopback listener-ownership
helpers (/proc based) used by the session drivers.

  rec = Recorder(upstream_port, 'requests.jsonl')
  ThreadingHTTPServer(('127.0.0.1', port), make_handler(rec))
"""
import hashlib
import http.client
import json
import os
from pathlib import Path
import socket
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler

sys.path.insert(0, str(Path(__file__).resolve().parent))
import tool_counters as tc_mod  # noqa: E402  (interim tool-call counters)


def h(x):
    return hashlib.sha256(json.dumps(x, sort_keys=True).encode()).hexdigest()[:16]


# ------------------------------------------------------------------ recording proxy

class Recorder:
    def __init__(self, upstream_port, path):
        self.up = upstream_port
        self.f = open(path, 'a')
        self.lock = threading.Lock()
        self.tag = {'client': None, 'task': None}
        self.rows = []

    def write(self, row, tag=None):
        with self.lock:
            row.update(tag if tag is not None else self.tag)
            self.rows.append(row)
            self.f.write(json.dumps(row) + '\n')
            self.f.flush()


class ClientEofWatch:
    """Detects the DOWNSTREAM client closing while the proxy streams a response, directly from
    the client socket (poll for RDHUP/HUP/EOF), instead of on the next upstream write (which made
    cancel/settle timing late by up to one heartbeat). On EOF it records the time and shuts the
    upstream socket, so upstream sees the cancel at once."""
    def __init__(self, client_sock, upstream_sock):
        import select as _select
        self.sel = _select
        self.sock = client_sock
        self.up = upstream_sock   # the socket itself: http.client drops conn.sock on Connection: close
        self.done = threading.Event()
        self.eof_t = None
        self.th = threading.Thread(target=self._run, daemon=True)
        self.th.start()

    def _run(self):
        sel = self.sel
        hup = sel.POLLHUP | sel.POLLERR | getattr(sel, 'POLLRDHUP', 0x2000)
        p = sel.poll()
        try:
            p.register(self.sock, sel.POLLIN | hup)
        except (OSError, ValueError):
            return
        while not self.done.is_set():
            ev = p.poll(50)
            if not ev:
                continue
            eof = any(e & hup for _, e in ev)
            if not eof:
                try:
                    eof = self.sock.recv(1, socket.MSG_PEEK | socket.MSG_DONTWAIT) == b''
                except BlockingIOError:
                    eof = False
                except OSError:
                    eof = True
            if eof:
                self.eof_t = time.time()
                try:
                    if self.up is not None:
                        self.up.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
                return
            time.sleep(0.02)   # unexpected client bytes after the request: do not spin

    def stop(self):
        self.done.set()
        self.th.join(1)


def make_handler(rec):
    class H(BaseHTTPRequestHandler):
        protocol_version = 'HTTP/1.1'

        def log_message(self, *a):
            pass

        def _forward(self, method):
            n = int(self.headers.get('Content-Length') or 0)
            body = self.rfile.read(n) if n else b''
            t0 = time.time()
            tag = dict(rec.tag)  # attributed at request ENTRY (a late request keeps its own task)
            row = {'method': method, 'path': self.path, 't': round(t0, 3)}
            req = None
            if self.path.endswith('/chat/completions') and body:
                try:
                    req = json.loads(body)
                except ValueError:
                    req = None
            if req is not None:
                msgs = req.get('messages') or []
                sys_txt = ''.join(m.get('content') if isinstance(m.get('content'), str) else json.dumps(m.get('content'))
                                  for m in msgs if m.get('role') in ('system', 'developer'))
                tools = req.get('tools') or []
                row.update({
                    'chat': True, 'stream': bool(req.get('stream')), 'max_tokens': req.get('max_tokens') or req.get('max_completion_tokens'),
                    'n_messages': len(msgs), 'roles': ''.join((m.get('role') or '?')[0] for m in msgs),
                    'system_bytes': len(sys_txt.encode()), 'system_hash': h(sys_txt), 'tools_n': len(tools),
                    'tools_bytes': len(json.dumps(tools).encode()), 'tools_hash': h(tools),
                    'msg_hashes': [h(m) for m in msgs],
                    'tool_msg_bytes': [len(json.dumps(m.get('content')).encode()) for m in msgs if m.get('role') == 'tool'],
                    'last_user_bytes': next((len(json.dumps(m.get('content')).encode()) for m in reversed(msgs) if m.get('role') == 'user'), 0),
                    'session_hdr': self.headers.get('X-Session-Id') or self.headers.get('X-Oxe-Session'),
                    'turn_kind': self.headers.get('X-Oxe-Turn-Kind'), 'priority': self.headers.get('X-Oxe-Priority'),
                    'body_bytes': len(body), 'temperature': req.get('temperature'),
                    'tool_choice': req.get('tool_choice'), 'has_reasoning_echo': any('reasoning_content' in m for m in msgs),
                })
                if getattr(rec, 'first_sys', None) is None and sys_txt and tools:
                    rec.first_sys = (sys_txt, tools)  # the first main (tool-bearing) request
            hdrs = {k: v for k, v in self.headers.items() if k.lower() not in ('host', 'content-length', 'connection')}
            watch = None
            buf = b''
            first = None
            last, max_gap = None, 0.0   # stall metric: longest silence between chunks
            try:
                c = http.client.HTTPConnection('127.0.0.1', rec.up, timeout=1800)
                c.connect()
                # the client-disconnect watch is armed BEFORE the request goes upstream, so a cancel
                # while waiting for upstream headers (long prefill) closes upstream at once too
                watch = ClientEofWatch(self.connection, c.sock)
                c.request(method, self.path, body=body, headers=hdrs)
                r = c.getresponse()
                # the upstream status is evidence as soon as headers arrive: recorded BEFORE any
                # downstream write, so a client that closes early (broken pipe) cannot lose it
                row['status'] = r.status
                self.send_response(r.status)
                for k, v in r.getheaders():
                    if k.lower() not in ('transfer-encoding', 'connection', 'content-length'):
                        self.send_header(k, v)
                self.send_header('Transfer-Encoding', 'chunked')
                self.send_header('Connection', 'close')
                self.end_headers()
                self.wfile.flush()
                while True:
                    try:
                        chunk = r.read1(65536) if hasattr(r, 'read1') else r.read(65536)
                    except OSError:
                        if watch.eof_t is not None:
                            break
                        raise
                    if not chunk or watch.eof_t is not None:
                        break
                    now = time.time()
                    if first is None:
                        first = now
                    if last is not None:
                        max_gap = max(max_gap, now - last)
                    last = now
                    buf += chunk
                    self.wfile.write(b'%x\r\n%s\r\n' % (len(chunk), chunk))
                    self.wfile.flush()
                watch.stop()
                if watch.eof_t is not None:
                    raise ConnectionResetError('client_eof')
                self.wfile.write(b'0\r\n\r\n')
                row['ttfb_s'] = round((first or time.time()) - t0, 3)
                row['max_chunk_gap_s'] = round(max_gap, 3)
                if row.get('chat'):
                    row.update(parse_usage(buf, row.get('stream'), (req or {}).get('tools')))
            except Exception as e:
                if watch is not None and watch.eof_t is not None:
                    # client went away in ANY phase (headers wait or streaming): not an upstream failure
                    row['client_dropped'] = 'client_eof'
                    row['client_eof_s'] = round(watch.eof_t - t0, 3)   # cancel time seen on the client socket
                    self._salvage(row, buf, first, max_gap, t0, req)
                elif isinstance(e, (BrokenPipeError, ConnectionResetError)):
                    row.setdefault('client_dropped', type(e).__name__)
                    self._salvage(row, buf, first, max_gap, t0, req)
                else:  # upstream failure
                    self._upstream_error(row, e)
            finally:
                if watch is not None:
                    watch.stop()
            row['dur_s'] = round(time.time() - t0, 3)
            if row.get('chat') or (row.get('status') or 0) >= 400 or 'proxy_error' in row:
                rec.write(row, tag)
            self.close_connection = True

        @staticmethod
        def _salvage(row, buf, first, max_gap, t0, req):
            """An SSE client may close right after the semantic `data: [DONE]` frame, before the
            HTTP EOF. The relayed upstream bytes are then complete evidence: parse them ONLY when a
            whole [DONE] frame (terminated by a blank line) was relayed. A partial frame, an early
            cancel or a non-stream body stays unmeasured (wire_complete False, no usage)."""
            if not (row.get('chat') and row.get('stream')):
                return
            row['wire_complete'] = sse_done_complete(buf)
            if row['wire_complete']:
                row['ttfb_s'] = round((first or time.time()) - t0, 3)
                row['max_chunk_gap_s'] = round(max_gap, 3)
                row.update(parse_usage(buf, True, (req or {}).get('tools')))
                row['metrics_source'] = 'complete_upstream_sse'

        def _upstream_error(self, row, e):
            row['proxy_error'] = '{}: {}'.format(type(e).__name__, str(e)[:120])
            try:
                self.send_error(502)
            except Exception:
                pass

        def do_POST(self):
            self._forward('POST')

        def do_GET(self):
            self._forward('GET')
    return H


def assemble_message(objs):
    """The assistant message (content + tool calls) assembled from stream deltas or a body."""
    content, calls = [], {}
    for o in objs:
        for ch in o.get('choices') or []:
            d = ch.get('delta') or ch.get('message') or {}
            if isinstance(d.get('content'), str):
                content.append(d['content'])
            for i, tcall in enumerate(d.get('tool_calls') or []):
                slot = calls.setdefault(tcall.get('index', i), {'id': None, 'function': {'name': '', 'arguments': ''}})
                if tcall.get('id'):
                    slot['id'] = tcall['id']
                fn = tcall.get('function') or {}
                slot['function']['name'] += fn.get('name') or ''
                args = fn.get('arguments')
                slot['function']['arguments'] += args if isinstance(args, str) else (json.dumps(args) if args else '')
    return {'content': ''.join(content), 'tool_calls': [calls[k] for k in sorted(calls)]}


def sse_done_complete(buf):
    """True when `buf` holds a complete `data: [DONE]` SSE frame (the frame ended by a blank line)."""
    frames = buf.replace(b'\r\n', b'\n').split(b'\n\n')
    return any(f.strip() in (b'data: [DONE]', b'data:[DONE]') for f in frames[:-1])


def parse_usage(buf, stream, request_tools=None):
    out = {'finish': None}
    usage, timings, n_tool_calls, n_chunks = None, None, 0, 0
    objs = []
    if stream:
        for line in buf.split(b'\n'):
            line = line.strip()
            if line.startswith(b'data:') and line[5:].strip() not in (b'[DONE]', b''):
                try:
                    objs.append(json.loads(line[5:]))
                except ValueError:
                    pass
    else:
        try:
            objs.append(json.loads(buf))
        except ValueError:
            pass
    for o in objs:
        n_chunks += 1
        if o.get('usage'):
            usage = o['usage']
        if o.get('timings'):
            timings = o['timings']
        for ch in o.get('choices') or []:
            if ch.get('finish_reason'):
                out['finish'] = ch['finish_reason']
            d = ch.get('delta') or ch.get('message') or {}
            for tc in d.get('tool_calls') or []:
                if tc.get('id') or (tc.get('function') or {}).get('name'):
                    n_tool_calls += 1
        if o.get('error'):
            out['error'] = str(o['error'])[:200]
    if usage:
        out['prompt'] = usage.get('prompt_tokens')
        out['completion'] = usage.get('completion_tokens')
        out['cached'] = (usage.get('prompt_tokens_details') or {}).get('cached_tokens')
    if timings:
        out['cache_n'] = timings.get('cache_n')
        out['resume_src'] = timings.get('resume_src')
        if out.get('cached') is None:
            out['cached'] = timings.get('cache_n')
        if out.get('prompt') is None and timings.get('prompt_n') is not None:
            out['prompt'] = (timings.get('cache_n') or 0) + timings['prompt_n']
        if out.get('completion') is None:
            out['completion'] = timings.get('predicted_n')
    out['tool_calls_out'] = n_tool_calls
    # interim counters: classify the WIRE calls (before any client repair); keep verdicts only
    verdicts, fb = tc_mod.classify_response(request_tools or [], assemble_message(objs))
    out['wire_calls'] = [[cid, reason] for cid, reason in verdicts]
    out['text_fallback'] = fb
    out['usage_seen'] = usage is not None or timings is not None
    return out


# ------------------------------------------------------------------ listener ownership

def _listen_inodes(port):
    out = set()
    for f in ('/proc/net/tcp', '/proc/net/tcp6'):
        try:
            for line in open(f).read().splitlines()[1:]:
                parts = line.split()
                if parts[3] == '0A' and int(parts[1].rsplit(':', 1)[1], 16) == port:
                    out.add(parts[9])
        except (OSError, IndexError, ValueError):
            continue
    return out


def port_in_use(port):
    return bool(_listen_inodes(port))


def pid_listens(pid, port):
    """True when a LISTEN socket on `port` is held by `pid` (via /proc fd inodes)."""
    inodes = _listen_inodes(port)
    try:
        fds = os.listdir('/proc/%d/fd' % pid)
    except OSError:
        return False
    for fd in fds:
        try:
            link = os.readlink('/proc/%d/fd/%s' % (pid, fd))
        except OSError:
            continue
        if link.startswith('socket:[') and link[8:-1] in inodes:
            return True
    return False
