"""NativeHandle: a bare oxe-server on a private store root, for rehearsing the fixture library and
the from-rung gates on a real engine WITHOUT the staged controller (T1.5 tiny-real model). MIT.

Same interface as FakeHandle / ScopedController (start, load, unload, chat, stop, restart, port,
url, ledger_dir, alias). Differences, on purpose:
- no controller, no systemd scope, no inference lease, no runtime.json: the engine binds
  127.0.0.1 on a temporary port, HOME / XDG roots are private, the store is `<store_root>/state`;
- a small recording proxy on `port` (engine on `port + 1`) writes controller-shaped ledger rows
  (`requests-<day>.jsonl`: t, path, status, cache_n, prompt_n, predicted_n, cached_tokens,
  resume_src) from the engine's own timings. It has NO `miss_class` (a controller computation):
  the gate's `unknown_miss` check is vacuous here and only the staged controller exercises it.
- load() starts the engine (weights from a shared private weight cache), unload() is SIGTERM with
  the exit proven (the engine writes its head snapshot on SIGTERM: a quiescent store).
"""
import http.client
import json
import os
from pathlib import Path
import signal
import subprocess
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from handles import http_json

REFUSED_PORTS = (8092, 8093, 8094)


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


class NativeHandle:
    rehearsal = False
    native = True

    def __init__(self, oxe_server, model_path, ledger_dir, alias='tiny', port=18797, weight_cache=None,
                 ctx=131072, extra_args=()):
        if port in REFUSED_PORTS or port + 1 in REFUSED_PORTS:
            raise ValueError('production port refused')
        self.server = str(oxe_server)
        self.model_path = str(model_path)
        self.alias = alias
        self.port = port
        self.engine_port = port + 1
        self.url = f'http://127.0.0.1:{port}'
        self.ledger_dir = Path(ledger_dir)
        self.ledger_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
        self.weight_cache = Path(weight_cache) if weight_cache else self.ledger_dir / 'weights'
        self.weight_cache.mkdir(mode=0o700, parents=True, exist_ok=True)
        self.ctx = ctx
        self.extra_args = list(extra_args)
        self.proc = None
        self.store_root = None
        self.proxy = None
        self.events = []

    # ------------------------------------------------------------------ proxy
    def _start_proxy(self):
        handle = self

        class H(BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.1'

            def log_message(self, *a):
                pass

            def _fwd(self, method):
                n = int(self.headers.get('Content-Length') or 0)
                body = self.rfile.read(n) if n else b''
                t0 = time.time()
                hdrs = {k: v for k, v in self.headers.items() if k.lower() not in ('host', 'content-length', 'connection')}
                buf, status, logged = b'', 502, False
                try:
                    c = http.client.HTTPConnection('127.0.0.1', handle.engine_port, timeout=7200)
                    c.request(method, self.path, body=body, headers=hdrs)
                    r = c.getresponse()
                    status = r.status
                    self.send_response(r.status)
                    for k, v in r.getheaders():
                        if k.lower() not in ('transfer-encoding', 'connection', 'content-length'):
                            self.send_header(k, v)
                    self.send_header('Transfer-Encoding', 'chunked')
                    self.send_header('Connection', 'close')
                    self.end_headers()
                    while True:
                        chunk = r.read1(65536)
                        if not chunk:
                            break
                        buf += chunk
                        if (not logged and self.path.endswith('/chat/completions')
                                and (buf.startswith(b'data: [DONE]') or b'\ndata: [DONE]' in buf)):
                            # the row exists before the client sees the end of the stream (a client
                            # that reads the ledger right after [DONE] must find it, as with the controller)
                            handle._ledger(t0, self.path, status, buf)
                            logged = True
                        self.wfile.write(b'%x\r\n%s\r\n' % (len(chunk), chunk))
                        self.wfile.flush()
                    self.wfile.write(b'0\r\n\r\n')
                except Exception:
                    try:
                        self.send_error(502)
                    except Exception:
                        pass
                if self.path.endswith('/chat/completions') and not logged:
                    handle._ledger(t0, self.path, status, buf)
                self.close_connection = True

            def do_POST(self):
                self._fwd('POST')

            def do_GET(self):
                self._fwd('GET')

        self.proxy = ThreadingHTTPServer(('127.0.0.1', self.port), H)
        threading.Thread(target=self.proxy.serve_forever, daemon=True).start()

    def _ledger(self, t0, path, status, buf):
        timings, usage = {}, {}
        objs = []
        text = buf.decode('utf-8', 'replace')
        if text.lstrip().startswith('{'):
            try:
                objs.append(json.loads(text))
            except ValueError:
                pass
        else:
            for line in text.split('\n'):
                line = line.strip()
                if line.startswith('data:') and line[5:].strip() not in ('', '[DONE]'):
                    try:
                        objs.append(json.loads(line[5:]))
                    except ValueError:
                        pass
        for o in objs:
            if isinstance(o, dict):
                timings.update(o.get('timings') or {})
                usage = o.get('usage') or usage
        row = dict(t=t0, model=self.alias, path=path, status=status, cache_n=timings.get('cache_n'),
                   prompt_n=timings.get('prompt_n'), predicted_n=timings.get('predicted_n'),
                   cached_tokens=(usage.get('prompt_tokens_details') or {}).get('cached_tokens'),
                   resume_src=timings.get('resume_src'), origin='lab', source='native-handle-proxy')
        if status >= 400:
            row['error'] = text[:300]  # the engine's own error body (no prompt text in it)
        with open(self.ledger_dir / 'requests-{}.jsonl'.format(time.strftime('%Y%m%d')), 'a') as f:
            f.write(json.dumps(row) + '\n')

    # ------------------------------------------------------------------ lifecycle
    def start(self, store_root):
        self.store_root = Path(store_root)
        (self.store_root / 'state').mkdir(mode=0o700, parents=True, exist_ok=True)
        if self.proxy is None:
            self._start_proxy()
        self.events.append(('start', str(self.store_root)))

    def load(self):
        if self.proc is not None and self.proc.poll() is None:
            return
        home = self.store_root
        env = {k: v for k, v in os.environ.items() if not k.startswith(('OXE_', 'LOCAL_AI', 'LOCAL_LLM', 'XDG_', 'BRAVE_'))}
        env.update(HOME=str(home), XDG_CACHE_HOME=str(self.weight_cache), XDG_RUNTIME_DIR=str(home / '.run'),
                   XDG_STATE_HOME=str(home / '.local-state'))
        (home / '.run').mkdir(mode=0o700, exist_ok=True)
        if port_in_use(self.engine_port):
            raise RuntimeError('native_engine_port_in_use:%d (never touch an unrelated listener)' % self.engine_port)
        log = open(self.ledger_dir / 'engine.log', 'ab')
        argv = ['nice', '-n', '10', self.server, '--engine', 'oxe', '--model', self.model_path, '--alias', self.alias,
                '--host', '127.0.0.1', '--port', str(self.engine_port), '--ctx-size', str(self.ctx),
                '--state-cache', str(home / 'state'), '--state-ttl-secs', str(10 * 365 * 86400),
                '--spec-type', 'draft-mtp', '--spec-draft-n-max', '2', '--reasoning-format', 'deepseek'] + self.extra_args
        self.proc = subprocess.Popen(argv, env=env, stdout=log, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL,
                                     start_new_session=True)
        until = time.monotonic() + 300
        while True:
            if self.proc.poll() is not None:
                raise RuntimeError('native_engine_exited_at_load')
            try:
                # ready only when the listener on engine_port is OUR process (nice execs the server)
                if http_json(self.engine_port, 'GET', '/health', timeout=2)[0] == 200 and pid_listens(self.proc.pid, self.engine_port):
                    break
            except OSError:
                pass
            if time.monotonic() > until:
                raise RuntimeError('native_engine_not_ready')
            time.sleep(.5)
        self.events.append(('load',))

    def unload(self):
        if self.proc is not None and self.proc.poll() is None:
            os.killpg(self.proc.pid, signal.SIGTERM)
            try:
                self.proc.wait(timeout=120)
            except subprocess.TimeoutExpired:
                os.killpg(self.proc.pid, signal.SIGKILL)
                self.proc.wait(timeout=30)
                raise RuntimeError('native_engine_needed_sigkill')
        self.proc = None
        self.events.append(('unload',))
        return True

    def chat(self, body, timeout=3600):
        return http_json(self.port, 'POST', '/v1/chat/completions', body, timeout=timeout)

    def stop(self):
        try:
            self.unload()
        finally:
            if self.proxy is not None:
                self.proxy.shutdown()
                self.proxy.server_close()
                self.proxy = None
            self.events.append(('stop',))

    def restart(self, store_root):
        self.stop()
        self.start(store_root)

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.stop()
