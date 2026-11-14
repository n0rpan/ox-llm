#!/usr/bin/env python3
"""Interim I0-R real-use session (V020-INTERIM-BATCH-BINDINGS real_use_session): 30-45 min of
release-built oxa on Ornith working IN a disposable monorepo checkout. MIT.

  real_use_session.py --repo <monorepo> --candidate <sha> --base-url http://127.0.0.1:<controller>
                      --oxa-bin <release oxa> --model ornith-35b-a3b-local --out DIR
                      [--soft-tokens 32000] [--budget-min 45] [--keep-work]
  real_use_session.py --probe ...      toolchain + sandbox probe only (no model request)
  real_use_session.py --check-only --work DIR   rerun the independent checker on a finished clone

SEALED TASK (bound before results, TASK_VERSION below): a real-module mutation in the engine's
GGUF reader (engine/crates/gguf/src/lib.rs: the data-section offset rounds DOWN instead of up to
`general.alignment`). The crate's unchanged unit tests are the independent checker. The session
script (TASKS) covers: a large read with continuation, the bug fix with `cargo test` through oxa's
bash, a unittest run (the lab suite runner; pytest is not installed on the host), one subagent,
cancel mid-stream and resume in a NEW oxa process (--continue), a forced compaction
(oxa.compaction.softTokens), and a second generic client overlapping the cargo step.

Isolation: the checkout is a fresh `git clone --shared` of --repo at --candidate in a private
work dir (never the real tree); HOME/XDG/agent/session dirs are private; BRAVE_* is never
inherited and web_search is excluded; oxa runs inside an OUTER bwrap (/ read-only, /home hidden,
only the toolchain dirs re-bound, the work dir writable), and oxa's own bwrap runs every bash
tool call inside it (nested; the real home is never visible to model-written commands).

Metrics (numbers and verdicts only; raw RPC records stay in the private work dir): wall per
task, requests, tool-call counters (wire valid/malformed/text-fallback; client repaired /
legit failures / repair turns: null when unmeasured), C3 append pairs and misses (+ oxa cache_miss
causes), compactions, prefix epochs, stalls (chunk silence > 30 s), second-client statuses.

Verdict lines `REAL-USE <item>: PASS|FAIL|INCOMPLETE <why>`; overall PASS needs every item.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from http.server import ThreadingHTTPServer
import urllib.parse

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent / 'tiny_real')]
import tool_counters as tc  # noqa: E402
import oxa_tiny_loop as oxl  # noqa: E402
import recording_proxy as rp  # noqa: E402

TASK_VERSION = 2   # v2: t1 forbids non-read shortcuts (v1 arm: the model scripted around paging)
MUTATION = dict(path='engine/crates/gguf/src/lib.rs',
                fixed='    h.data_offset = (c.pos as u64).div_ceil(align) * align;\n',
                broken='    h.data_offset = (c.pos as u64) / align * align;\n')
PROTECTED = ('engine/crates/gguf/src/tests.rs', 'engine/crates/gguf/Cargo.toml', 'engine/Cargo.lock')
CHECK_CMD = ['cargo', 'test', '-p', 'gguf', '--offline', '--quiet']
TASKS = [
    ('t1-large-read', 'Read the whole file engine/crates/model/src/engine.rs. It is long: page through it with the '
     'read tool (offset/limit) until you reach the end. Use ONLY the read tool for this task: no bash, grep, '
     'scripts or subagents. Then name the five longest functions in it.'),
    ('t2-bug', 'The gguf crate (engine/crates/gguf) has a failing unit test. In the engine/ directory run '
     '`cargo test -p gguf --offline`, find the bug in engine/crates/gguf/src/lib.rs and fix it. Do not edit '
     'any test file. Re-run the tests until they pass and tell me what you changed.'),
    ('t3-unittest', 'Run the lab unit tests for the tiny-real converter: in the lab/ directory run '
     '`./tools/run_lab_tests.sh discover -s tests -p test_tiny_real_convert.py` and report how many tests ran '
     'and whether they passed.'),
    ('t4-subagent', 'Use the subagent tool with agent "helper" and the task "List the public functions defined in '
     'engine/crates/gguf/src/lib.rs, one per line." Then repeat its list.'),
    ('t5-cancel', 'Explain in detail, step by step, how engine/crates/gguf/src/lib.rs parses a GGUF header.'),
    ('t6-resume', 'Continue our session: in three sentences, what did your gguf fix change and why do the tests '
     'pass now?'),
]
HELPER = '---\nname: helper\ndescription: Reads files and answers short questions about them\ntools: read\n---\nAnswer briefly.\n'
SECOND_CLIENT = 'Reply with one short sentence: what is a GGUF file?'
STALL_S = 30.0
LARGE_READ = 'engine/crates/model/src/engine.rs'
READ_COVERAGE_MIN = 0.95
SECOND_CLIENT_TIMEOUT_S = 600


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def peak_context(recs):
    """Largest context oxa reported for an assistant turn (usage.totalTokens on message_end);
    None when no usage was reported."""
    vals = [((r.get('message') or {}).get('usage') or {}).get('totalTokens') for r in recs
            if r.get('type') == 'message_end' and (r.get('message') or {}).get('role') == 'assistant']
    vals = [x for x in vals if isinstance(x, int) and not isinstance(x, bool)]
    return max(vals) if vals else None


def task_seal():
    return hashlib.sha256(json.dumps(dict(v=TASK_VERSION, m=MUTATION, p=PROTECTED, c=CHECK_CMD, t=TASKS)).encode()).hexdigest()


# ------------------------------------------------------------------ workspace

def prepare(repo, candidate, work):
    mono = work / 'mono'
    # self-contained: objects COPIED into the clone (no alternates, no hard links into the source),
    # so git works inside the sandbox and nothing in the session can touch the source objects
    subprocess.run(['git', 'clone', '--quiet', '--no-hardlinks', '--no-checkout', str(repo), str(mono)], check=True)
    if (mono / '.git' / 'objects' / 'info' / 'alternates').exists():
        raise SystemExit('clone is not self-contained (alternates present)')
    subprocess.run(['git', '-C', str(mono), 'checkout', '--quiet', '--detach', candidate], check=True)
    target = mono / MUTATION['path']
    text = target.read_text()
    if text.count(MUTATION['fixed']) != 1:
        raise SystemExit('sealed mutation does not apply at %s (fixed line not found exactly once)' % candidate)
    target.write_text(text.replace(MUTATION['fixed'], MUTATION['broken']))
    big = mono / LARGE_READ
    seal = dict(candidate=candidate, task_seal=task_seal(), mutated_sha256=sha(target),
                protected={p: sha(mono / p) for p in PROTECTED},
                large_read_lines=len(big.read_text(errors='replace').splitlines()) if big.exists() else None)
    (work / 'seal.json').write_text(json.dumps(seal, indent=1))
    return mono, seal


def private_home(work):
    """HOME for oxa: toolchain entries are symlinks to the real ones (oxa binds TOOLCHAIN_READ /
    CACHE_WRITE relative to $HOME when they exist); nothing else of the real home."""
    home = work / 'home'
    real = Path(os.path.expanduser('~'))
    for rel in ('.rustup', '.cargo/bin', '.cargo/registry', '.cargo/git', '.cargo/config.toml', '.local/share/mise'):
        src = real / rel
        if src.exists():
            dst = home / rel
            dst.parent.mkdir(parents=True, exist_ok=True)
            if not dst.exists():
                dst.symlink_to(src)
    (home / 'run').mkdir(parents=True, exist_ok=True)
    os.chmod(home / 'run', 0o700)
    return home


def outer_bwrap(work, extra_ro=()):
    """/ read-only, /home hidden, the toolchain dirs and the work dir re-bound, private /tmp."""
    bwrap = shutil.which('bwrap')
    if not bwrap:
        raise SystemExit('bwrap missing: refusing to run model-written commands without a sandbox')
    real = Path(os.path.expanduser('~'))
    argv = [bwrap, '--die-with-parent', '--unshare-pid', '--unshare-ipc', '--unshare-uts',
            '--ro-bind', '/', '/', '--tmpfs', '/home', '--dev', '/dev', '--proc', '/proc', '--tmpfs', '/tmp']
    for rel in ('.rustup', '.cargo/bin', '.cargo/config.toml', '.local/share/mise'):
        if (real / rel).exists():
            argv += ['--ro-bind', str(real / rel), str(real / rel)]
    for rel in ('.cargo/registry', '.cargo/git'):
        if (real / rel).exists():
            argv += ['--bind', str(real / rel), str(real / rel)]
    for p in extra_ro:
        argv += ['--ro-bind', str(p), str(p)]
    argv += ['--bind', str(work), str(work)]
    return argv


def env_for(work, home, base_url):
    engine = base_url[:-3] if base_url.rstrip('/').endswith('/v1') else base_url
    env = oxl.client_env(os.environ, home, home / '.oxa' / 'agent', work / 'sessions', engine.rstrip('/'))
    env.update(CARGO_HOME=str(home / '.cargo'), RUSTUP_HOME=str(home / '.rustup'),
               CARGO_TARGET_DIR=str(work / 'target-session'), TMPDIR=str(work / 'tmp'))
    (work / 'tmp').mkdir(exist_ok=True)
    return env


# ------------------------------------------------------------------ checker (independent)

def check(work, timeout_s=1800):
    mono = work / 'mono'
    seal = json.loads((work / 'seal.json').read_text())
    out = {}
    changed = [p for p, h in seal['protected'].items() if not (mono / p).exists() or sha(mono / p) != h]
    out['tests_unchanged'] = (not changed, 'protected files unchanged' if not changed else 'changed: %s' % changed)
    text = (mono / MUTATION['path']).read_text()
    out['mutation_removed'] = (MUTATION['broken'] not in text, 'broken line gone' if MUTATION['broken'] not in text else 'broken line still present')
    diff = subprocess.run(['git', '-C', str(mono), 'diff', '--name-only'], capture_output=True, text=True).stdout.split()
    extra = [d for d in diff if d != MUTATION['path']]
    out['diff_scoped'] = (not extra, 'only %s changed' % MUTATION['path'] if not extra else 'also changed: %s' % extra)
    # the checker compiles and runs MODEL-EDITED code: never on the host, always inside the same
    # outer sandbox, with no network
    home = private_home(work)
    env = env_for(work, home, 'http://127.0.0.1:1')
    env['CARGO_TARGET_DIR'] = str(work / 'target-check')
    # no compiler wrapper for the independent checker: the bound ~/.cargo/config.toml names sccache,
    # whose server socket path overflows SUN_LEN under a long scratch path (and the shared sccache
    # must never be touched from the sandbox). An empty RUSTC_WRAPPER disables the config wrapper.
    env['RUSTC_WRAPPER'] = ''
    for k in [k for k in env if k.startswith('SCCACHE_') or k == 'CARGO_BUILD_RUSTC_WRAPPER']:
        del env[k]
    cmd = outer_bwrap(work) + ['--unshare-net', '--chdir', str(mono / 'engine'), 'nice', '-n', '19'] + CHECK_CMD
    try:
        r = subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=max(1.0, float(timeout_s)))
    except subprocess.TimeoutExpired:
        out['checker_tests'] = (False, 'checker exceeded the remaining arm budget (%.0f s)' % timeout_s)
        return out
    tail = (r.stdout + r.stderr).strip().splitlines()[-1:] or ['']
    out['checker_tests'] = (r.returncode == 0, 'cargo test -p gguf exit %d (%s)' % (r.returncode, tail[0][:120]))
    return out


# ------------------------------------------------------------------ session

class SecondClient:
    """A generic streamed client overlapping the main session. Owned and bounded: an absolute
    deadline per request (checked between reads; the socket read timeout is short), a stop event,
    the live connection closed by stop(); reap() stops and joins and reports survivors. [t0, t1]
    of each request are recorded to prove the overlap."""
    READ_TIMEOUT_S = 10   # legacy knob, no longer used for reads: one absolute deadline covers the response

    def __init__(self, base_url, rows, stop_after=3, deadline_s=SECOND_CLIENT_TIMEOUT_S):
        self.u = urllib.parse.urlparse(base_url)
        self.rows, self.stop_after, self.deadline_s = rows, stop_after, deadline_s
        self.stop_ev = threading.Event()
        self.conn = None
        self.sock = None
        self.lock = threading.Lock()
        self.thread = threading.Thread(target=self.run, daemon=True)

    def start(self):
        self.thread.start()
        return self

    def run(self):
        import http.client
        for _ in range(self.stop_after):
            if self.stop_ev.is_set():
                break
            t0 = time.time()
            row = dict(t0=t0)
            timer = None
            cut = {}
            try:
                # ONE absolute deadline for connect + headers + the whole body. Nothing polls the
                # buffered reader with a short timeout (a timed-out BufferedReader is poisoned);
                # instead a timer shuts the raw socket at the deadline, exactly as stop() does.
                remaining = max(0.1, self.deadline_s - (time.time() - t0))
                c = http.client.HTTPConnection(self.u.hostname, self.u.port, timeout=remaining)
                c.connect()
                sock = c.sock          # kept: http.client drops conn.sock on Connection: close
                with self.lock:
                    self.conn, self.sock = c, sock
                if self.stop_ev.is_set():
                    raise ConnectionAbortedError('stopped')

                def deadline_cut(sock=sock):
                    cut['deadline'] = True
                    self._shut(sock)
                timer = threading.Timer(max(0.0, self.deadline_s - (time.time() - t0)), deadline_cut)
                timer.daemon = True
                timer.start()
                sock.settimeout(None)  # blocking; the timer / stop() interrupt it
                c.request('POST', '/v1/chat/completions', json.dumps({
                    'model': self.rows['model'], 'stream': True, 'max_tokens': 64, 'temperature': 0,
                    'messages': [{'role': 'user', 'content': SECOND_CLIENT}]}).encode(),
                    {'Content-Type': 'application/json', 'X-Session-Id': 'interim-second-client', 'X-Local-AI-Origin': 'lab'})
                r = c.getresponse()
                row['status'] = r.status
                body = b''
                while True:
                    try:
                        chunk = r.read1(65536)
                    except (OSError, ValueError, http.client.HTTPException):
                        if self.stop_ev.is_set() or cut:
                            break
                        raise
                    if not chunk:
                        break
                    body += chunk
                row.update(status=r.status, done=b'[DONE]' in body, retry_after=r.getheader('Retry-After'))
                if not row['done'] and (self.stop_ev.is_set() or cut):
                    row['cut'] = 'stopped' if self.stop_ev.is_set() else 'deadline'
            except Exception as e:
                if self.stop_ev.is_set() or cut:
                    row.update(status=row.get('status'), done=False, cut='stopped' if self.stop_ev.is_set() else 'deadline')
                else:
                    row.update(status=None, error=type(e).__name__)
            finally:
                if timer is not None:
                    timer.cancel()
                with self.lock:
                    if self.conn is not None:
                        self.conn.close()
                    self.conn = self.sock = None
            row.update(t1=time.time(), wall_s=round(time.time() - t0, 1))
            self.rows['statuses'].append(row)
            self.stop_ev.wait(2)

    @staticmethod
    def _shut(sock):
        if sock is not None:
            try:
                import socket
                sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass

    def stop(self):
        self.stop_ev.set()
        with self.lock:
            sock = self.sock
        self._shut(sock)

    def reap(self, timeout=30):
        """Stop + join; returns True when nothing survives."""
        self.stop()
        self.thread.join(timeout)
        return not self.thread.is_alive()


def second_client(base_url, rows, stop_after=3):
    """Compatibility wrapper (synchronous run)."""
    sc = SecondClient(base_url, rows, stop_after)
    sc.run()


def session(a, work, mono, home):
    """Owns every client and process it starts: the proxy, the oxa process(es) and the second
    client are stopped and reaped on EVERY exit path (finally), survivors recorded and failed.
    Wall clock is enforced by a watchdog thread: per-task cap and the overall budget abort the
    running prompt even when no record arrives."""
    agent = home / '.oxa' / 'agent'
    (agent / 'agents').mkdir(parents=True, exist_ok=True)
    (agent / 'agents' / 'helper.md').write_text(HELPER)
    settings = {'compaction': {'enabled': True},
                'oxa': {'compaction': {'softTokens': a.soft_tokens}, 'subagent': {'enabled': True}}}
    (work / 'sessions').mkdir(exist_ok=True)
    up = urllib.parse.urlparse(a.base_url)
    if up.hostname not in ('127.0.0.1', 'localhost', '::1') or up.port in (8093, 8094):
        raise SystemExit('loopback controller endpoint only (never an engine port)')
    rec = rp.Recorder(up.port, work / 'requests.jsonl')
    proxy = ThreadingHTTPServer(('127.0.0.1', a.proxy_port), rp.make_handler(rec))
    threading.Thread(target=proxy.serve_forever, daemon=True).start()
    proxied = 'http://127.0.0.1:%d' % a.proxy_port
    env = env_for(work, home, proxied)
    (agent / 'settings.json').write_text(json.dumps(settings))
    argv = outer_bwrap(work, extra_ro=[Path(a.oxa_bin).resolve().parent]) + [
        str(Path(a.oxa_bin).resolve()), '--mode', 'rpc', '--model', a.model, '--exclude-tools', 'web_search',
        '--no-context-files', '--no-skills', '--no-prompt-templates', '--no-themes', '--no-mcp',
        '--permission-mode', 'workspace', '--approve']
    tasks, all_recs, second = [], [], dict(model=a.model, statuses=[], survivors=[])
    budget_end = time.time() + a.budget_min * 60
    clients = []
    state = dict(rpc=None, task_t0=None, tid=None, aborted_by=None)
    stop_watch = threading.Event()

    def watchdog():
        while not stop_watch.wait(1.0):
            r, t0 = state['rpc'], state['task_t0']
            if r is None or t0 is None or state['aborted_by']:
                continue
            now = time.time()
            if state['tid'] != 't5-cancel' and now - t0 > a.task_cap_s:
                state['aborted_by'] = 'task_cap'
            elif now > budget_end:
                state['aborted_by'] = 'budget'
            if state['aborted_by']:
                try:
                    r.send({'type': 'abort'})
                except Exception:
                    pass
    wd = threading.Thread(target=watchdog, daemon=True)
    wd.start()
    try:
        state['rpc'] = oxl.Rpc(argv, env, str(mono), str(work / 'oxa-rpc.jsonl'))
        for tid, text in TASKS:
            if time.time() > budget_end:
                tasks.append(dict(task=tid, error='not run: overall budget exhausted', wall_s=0.0))
                continue
            rec.tag['task'] = tid
            t0 = time.time()
            row = dict(task=tid)
            sc = None
            try:
                if tid == 't6-resume':          # resume in a NEW oxa process on the same session
                    state['rpc'].close()
                    state['rpc'] = oxl.Rpc(argv + ['--continue'], env, str(mono), str(work / 'oxa-rpc-resume.jsonl'))
                rpc = state['rpc']
                all_recs.extend(rpc.wait_idle())
                if tid == 't2-bug':
                    sc = SecondClient(a.base_url, second).start()
                    clients.append(sc)
                seen = {'sent': False}
                state.update(task_t0=t0, tid=tid, aborted_by=None)

                def hook(r, seen=seen, tid=tid, rpc=rpc):
                    if tid == 't5-cancel' and r.get('type') == 'message_update' and not seen['sent']:
                        seen['sent'] = True          # the sealed mid-stream cancel
                        rpc.send({'type': 'abort'})
                recs, resp = rpc.run_prompt(text, hook)
                recs += rpc.wait_idle()
                for r in recs:
                    r['_task'] = tid
                all_recs.extend(recs)
                row['error'] = None if resp is None or resp.get('success') else resp.get('error')
                row['aborted'] = any(x.get('type') == 'agent_settled' and x.get('aborted') for x in recs)
                row['tool_calls'] = [x.get('toolName') for x in recs if x.get('type') == 'tool_execution_start']
                row['compactions'] = sum(1 for x in recs if x.get('type') == 'compaction_end')
            except Exception as e:
                row['error'] = '%s: %s' % (type(e).__name__, str(e)[:200])
            finally:
                row['capped'] = state['aborted_by'] is not None
                if state['aborted_by']:
                    row['error'] = row.get('error') or 'aborted by %s' % state['aborted_by']
                state.update(task_t0=None, tid=None)
                if sc is not None and not sc.reap():
                    second['survivors'].append(tid)
                row['wall_s'] = round(time.time() - t0, 1)
                tasks.append(row)
    finally:
        stop_watch.set()
        for sc in clients:
            if not sc.reap():
                second['survivors'].append('final')
        try:
            if state['rpc'] is not None:
                state['rpc'].close()
        finally:
            proxy.shutdown()
            proxy.server_close()
            wd.join(5)
        second['unjoined'] = bool(second['survivors']) or any(sc.thread.is_alive() for sc in clients)
    return tasks, all_recs, rec.rows, second


def read_coverage(recs, total):
    """Fraction of LARGE_READ's lines returned by SUCCESSFUL, linked read calls (from the read
    tool's own continuation notes; a page without a note returned the rest of the file)."""
    import re
    if not total:
        return 0.0
    starts = {x.get('toolCallId'): x for x in recs if x.get('type') == 'tool_execution_start' and x.get('toolCallId')}
    covered = set()
    for e in recs:
        if e.get('type') != 'tool_execution_end' or e.get('toolName') != 'read' or e.get('isError') is not False:
            continue
        st = starts.get(e.get('toolCallId'))
        args = (st or {}).get('args') or {}
        if st is None or st.get('toolName') != 'read' or not str(args.get('path', '')).endswith(LARGE_READ):
            continue
        text = ''.join(c.get('text', '') for c in ((e.get('result') or {}).get('content') or []) if isinstance(c, dict))
        first = int(args.get('offset') or 1)
        m = re.search(r'\[Showing lines (\d+)-(\d+) of (\d+)', text)
        m2 = re.search(r'\[(\d+) more lines in file\. Use offset=(\d+)', text)
        if m:
            lo, hi = int(m.group(1)), int(m.group(2))
        elif m2:
            lo, hi = first, int(m2.group(2)) - 1
        else:
            # no footer: only the lines actually returned count, capped by the requested limit
            body = text.split('\n\n[')[0]
            returned = len(body.splitlines())
            lim = args.get('limit')
            n = min(returned, int(lim)) if isinstance(lim, (int, float)) and lim > 0 else returned
            lo, hi = first, first + n - 1
        covered.update(range(max(1, lo), min(hi, total) + 1))
    return len(covered) / total


import re as _re
CARGO_TEST = _re.compile(r'^cargo\s+test\b')
UNIT_TEST = _re.compile(r'^(\./tools/run_lab_tests\.sh|tools/run_lab_tests\.sh|python3?\s+(-I\s+)?-m\s+(unittest|pytest)|pytest)\b')
CARGO_OK = _re.compile(r'test result: ok\.')
UNIT_OK = _re.compile(r'Ran \d+ tests? in [0-9.]+s\s*\n\s*\nOK\b|^OK\b', _re.M)
_PREFIX = _re.compile(r'^((cd\s+\S+)|([A-Z_][A-Z0-9_]*=\S*)|(nice(\s+-n\s+-?\d+)?)|(timeout\s+\S+)|(env))\s+')


def invokes(cmd, pattern):
    """True when one `&&` / `;` segment of the shell line RUNS the command (after cd / VAR=x / nice /
    timeout / env prefixes); an echoed or quoted mention does not count."""
    for seg in _re.split(r'&&|;|\|\|', cmd):
        seg = seg.strip()
        while True:
            m = _PREFIX.match(seg)
            if not m or m.group(2):          # a bare `cd dir` segment is its own segment
                if m and m.group(2):
                    seg = seg[m.end():]
                    continue
                break
            seg = seg[m.end():]
        if pattern.match(seg):
            return True
    return False


def result_text(e):
    return ''.join(c.get('text', '') for c in ((e.get('result') or {}).get('content') or []) if isinstance(c, dict))


def tool_outcomes(recs):
    """Per task, what tools SUCCEEDED: a start only counts with its matching end (same
    toolCallId) carrying isError == False; for bash also details.exit == 0 when reported. Command
    lines are reduced to booleans (no text in the receipt)."""
    starts = {x.get('toolCallId'): x for x in recs if x.get('type') == 'tool_execution_start' and x.get('toolCallId')}
    ok_reads = ok_cargo = ok_unit = 0
    for e in recs:
        if e.get('type') != 'tool_execution_end' or e.get('isError') is not False:
            continue
        st = starts.get(e.get('toolCallId'))
        if st is None or st.get('toolName') != e.get('toolName'):
            continue
        if e.get('toolName') == 'read':
            ok_reads += 1
        elif e.get('toolName') == 'bash':
            details = ((e.get('result') or {}).get('details') or {})
            if details.get('exit') != 0:          # oxa's bash reports the exit code; absent = unproven
                continue
            cmd = str((st.get('args') or {}).get('command', ''))
            out = result_text(e)
            ok_cargo += invokes(cmd, CARGO_TEST) and CARGO_OK.search(out) is not None and 'FAILED' not in out
            ok_unit += invokes(cmd, UNIT_TEST) and UNIT_OK.search(out) is not None and 'FAILED' not in out
    return dict(ok_reads=ok_reads, ran_cargo_test=ok_cargo > 0, ran_unittest=ok_unit > 0)


TASK_CAP_DEFAULT_S = 900
BUDGET_DEFAULT_MIN = 45


def verdicts(tasks, recs, rows, second, chk, seal=None, task_cap_s=TASK_CAP_DEFAULT_S, budget_min=BUDGET_DEFAULT_MIN,
             arm_wall_s=None, soft_tokens=None):
    """Every item from OBSERVED evidence: task rows (errors/caps/abort), the RPC records tagged by
    task (tool success = linked end isError False [+ exit 0]), the proxy rows and the second-client
    observations. Flags in a task row are never trusted on their own."""
    v = {}
    by = {t['task']: t for t in tasks}
    per = {t['task']: tool_outcomes([r for r in recs if r.get('_task') == t['task']]) for t in tasks}
    for t in tasks:
        t.update(per[t['task']])
    cover = read_coverage([r for r in recs if r.get('_task') == 't1-large-read'], (seal or {}).get('large_read_lines'))

    def ok_task(tid):
        t = by.get(tid) or {}
        return bool(t) and not t.get('error') and not t.get('capped')
    v['large_read'] = (ok_task('t1-large-read') and (by['t1-large-read'].get('ok_reads') or 0) >= 2 and cover >= READ_COVERAGE_MIN,
                       '%d successful reads, %.0f%% of %s covered' % ((by.get('t1-large-read') or {}).get('ok_reads') or 0,
                                                                      100 * cover, LARGE_READ))
    v['cargo_via_oxa'] = (ok_task('t2-bug') and bool((by.get('t2-bug') or {}).get('ran_cargo_test')),
                          'a `cargo test` bash call ended isError=false, exit 0' if (by.get('t2-bug') or {}).get('ran_cargo_test')
                          else 'no successful `cargo test` bash call')
    v['unittest_via_oxa'] = (ok_task('t3-unittest') and bool((by.get('t3-unittest') or {}).get('ran_unittest')),
                             'the unit-test runner ended isError=false, exit 0' if (by.get('t3-unittest') or {}).get('ran_unittest')
                             else 'no successful unit-test run')
    sub = [x for x in recs if x.get('type') == 'tool_execution_end' and x.get('toolName') == 'subagent'
           and x.get('_task') == 't4-subagent']
    v['subagent'] = (bool(sub) and all(x.get('isError') is False for x in sub), '%d subagent calls' % len(sub))
    v['cancel'] = (bool((by.get('t5-cancel') or {}).get('aborted')), 'aborted run settled')
    v['resume'] = (ok_task('t6-resume'), 'new oxa process --continue answered')
    comps = sum(t.get('compactions', 0) for t in tasks)
    peak = peak_context(recs)
    soft = soft_tokens
    if comps >= 1:
        v['compaction'] = (True, '%d compactions (peak context %s tokens)' % (comps, peak))
    elif isinstance(peak, int) and isinstance(soft, int) and peak < soft:
        v['compaction'] = (False, '0 compactions: workload never reached softTokens (peak context %d < %d): fixture' % (peak, soft))
    elif isinstance(peak, int) and isinstance(soft, int):
        v['compaction'] = (False, '0 compactions although peak context %d >= softTokens %d: oxa finding' % (peak, soft))
    else:
        v['compaction'] = (False, '0 compactions (peak context %s, softTokens %s)' % (peak, soft))
    chat = [r for r in rows if r.get('chat') and r.get('session_hdr') != 'interim-second-client']
    done_main = [r for r in chat if r.get('status') == 200 and r.get('usage_seen')]
    v['main_requests'] = (len(done_main) >= len(TASKS) - 1, '%d completed oxa requests observed' % len(done_main))
    st = second.get('statuses') or []
    codes = [s.get('status') for s in st]
    completed = [s for s in st if s.get('status') == 200 and s.get('done') is True]
    overlap = any(s.get('t0') is not None and s.get('t1') is not None and r.get('t') is not None and r.get('dur_s') is not None
                  and s['t0'] < r['t'] + r['dur_s'] and r['t'] < s['t1'] for s in completed for r in done_main)
    storm = sum(1 for c in codes if c not in (200, 503))
    v['second_client'] = (bool(completed) and storm == 0 and codes.count(503) <= 3 and overlap and not second.get('unjoined'),
                          'statuses %s, completed %d, overlap %s%s' % (codes, len(completed), overlap,
                                                                        ', a request outlived its task' if second.get('unjoined') else ''))
    main5xx = sum(1 for r in chat if (r.get('status') or 0) >= 500)
    v['no_error_storm'] = (bool(chat) and main5xx == 0, '%d 5xx on the oxa session (%d requests)' % (main5xx, len(chat)))
    walls = [t.get('wall_s') for t in tasks]
    over = [t['task'] for t in tasks if not isinstance(t.get('wall_s'), (int, float)) or t['wall_s'] > task_cap_s
            or t.get('capped') or 'budget' in str(t.get('error') or '')]
    total = sum(w for w in walls if isinstance(w, (int, float)))
    arm = arm_wall_s if isinstance(arm_wall_s, (int, float)) else total
    in_budget = len(tasks) == len(TASKS) and not over and total <= budget_min * 60 and arm <= budget_min * 60
    v['budget'] = (in_budget,
                   'all %d tasks within %d s each; arm wall incl. checker %.1f of %d min' % (len(TASKS), task_cap_s, arm / 60, budget_min)
                   if in_budget else 'over budget / capped / not run: %s; tasks %.1f min, arm wall incl. checker %.1f of %d min'
                   % (over or ('missing tasks' if len(tasks) != len(TASKS) else 'none'), total / 60, arm / 60, budget_min))
    for k, (ok, why) in chk.items():
        v['checker_' + k] = (ok, why)
    return v


def metrics(tasks, recs, rows):
    chat = [r for r in rows if r.get('chat') and r.get('session_hdr') != 'interim-second-client']
    classified = [([tuple(x) for x in r.get('wire_calls') or []], bool(r.get('text_fallback'))) for r in chat if 'wire_calls' in r]
    counters = tc.tally(classified, tc.oxa_outcomes(recs))
    misses = [(x.get('expected'), x.get('cached'), x.get('cause')) for x in recs if x.get('type') == 'cache_miss']
    pairs = short = 0
    prev = None
    for r in chat:
        if prev is not None and r.get('prompt') is not None and prev.get('prompt') is not None:
            pairs += 1
            if (r.get('cached') or 0) < prev['prompt'] + (prev.get('completion') or 0) - 1:
                short += 1
        prev = r
    return dict(requests=len(chat), counters=counters, adjacent_pairs=pairs, adjacent_pairs_short=short,
                cache_miss_events=misses, compactions=sum(1 for x in recs if x.get('type') == 'compaction_end'),
                prefix_epochs=[(x.get('reason'), x.get('reprefillTokens')) for x in recs if x.get('type') == 'prefix_epoch'],
                stalls=sum(1 for r in chat if (r.get('max_chunk_gap_s') or 0) > STALL_S),
                max_chunk_gap_s=max([r.get('max_chunk_gap_s') or 0 for r in chat] or [0]),
                wall_s={t['task']: t.get('wall_s') for t in tasks},
                note='adjacent_pairs counts every consecutive oxa request (incl. compaction rewrites): '
                     'informational; oxa cache_miss events carry the cause')


def probe_script(mono, work):
    """The probe's shell: every step's status is kept (no pipeline swallows the cargo exit)."""
    return ('cargo --version && git -C %s log -1 --format=%%H && '
            'echo "home entries: $(ls /home/*/ 2>/dev/null | wc -l)" && '
            'cargo test -p gguf --offline --quiet --no-run > %s/probe-build.log 2>&1; rc=$?; '
            'tail -1 %s/probe-build.log; exit $rc' % (mono, work, work))


def probe(a, work):
    """Toolchain + nested sandbox probe: cargo visible and the gguf crate builds offline inside the
    outer bwrap AND inside a nested `bwrap --unshare-all` (the shape of oxa's own sandbox)."""
    home = private_home(work)
    mono = work / 'mono'
    env = env_for(work, home, 'http://127.0.0.1:1')
    inner = ['bwrap', '--unshare-all', '--die-with-parent', '--ro-bind', '/', '/', '--dev', '/dev', '--proc', '/proc',
             '--tmpfs', '/tmp', '--bind', str(work), str(work), '--chdir', str(mono / 'engine')]
    cmd = outer_bwrap(work) + inner + ['sh', '-c', probe_script(mono, work)]
    r = subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=1800)
    return r.returncode, (r.stdout + r.stderr).strip().splitlines()


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    ap.add_argument('--repo', type=Path)
    ap.add_argument('--candidate')
    ap.add_argument('--base-url')
    ap.add_argument('--oxa-bin')
    ap.add_argument('--model', default='ornith-35b-a3b-local')
    ap.add_argument('--out', type=Path, required=True)
    ap.add_argument('--soft-tokens', type=int, default=32000)
    ap.add_argument('--budget-min', type=int, default=45)
    ap.add_argument('--task-cap-s', type=int, default=TASK_CAP_DEFAULT_S, help='abort a task that runs longer (recorded as capped = FAIL for that item)')
    ap.add_argument('--proxy-port', type=int, default=18799)
    ap.add_argument('--keep-work', action='store_true')
    ap.add_argument('--probe', action='store_true')
    ap.add_argument('--rehearse-checker', action='store_true', help='model-free: checker FAILS on the mutation, PASSES on the reference fix')
    ap.add_argument('--check-only', action='store_true')
    ap.add_argument('--work', type=Path)
    a = ap.parse_args(argv)
    a.out.mkdir(parents=True, exist_ok=True)
    if a.check_only:
        chk = check(a.work)
        for k, (ok, why) in chk.items():
            print('REAL-USE checker_%s: %s %s' % (k, 'PASS' if ok else 'FAIL', why))
        ok = all(x[0] for x in chk.values())
        (a.out / 'checker.json').write_text(json.dumps({k: dict(ok=v[0], why=v[1]) for k, v in chk.items()}, indent=1))
        print('REAL-USE CHECKER: ' + ('PASS' if ok else 'FAIL'))
        return 0 if ok else 1
    if a.proxy_port in (8092, 8093, 8094):
        raise SystemExit('production port refused')
    work = Path(tempfile.mkdtemp(prefix='real-use-', dir=os.environ.get('TMPDIR')))
    try:
        mono, seal = prepare(a.repo, a.candidate, work)
        if a.rehearse_checker:
            # negative: the untouched mutation must FAIL the checker; positive: the reference fix PASSES
            neg = check(work)
            target = mono / MUTATION['path']
            target.write_text(target.read_text().replace(MUTATION['broken'], MUTATION['fixed']))
            pos = check(work)
            rows = dict(negative={k: dict(ok=v[0], why=v[1]) for k, v in neg.items()},
                        positive={k: dict(ok=v[0], why=v[1]) for k, v in pos.items()})
            behaved = (not neg['checker_tests'][0] and not neg['mutation_removed'][0]
                       and all(v[0] for v in pos.values()))
            (a.out / 'checker-rehearsal.json').write_text(json.dumps(dict(behaved=behaved, seal=seal, **rows), indent=1))
            print('negative (mutation left):', {k: v[0] for k, v in neg.items()})
            print('positive (reference fix):', {k: v[0] for k, v in pos.items()})
            print('REAL-USE CHECKER REHEARSAL: ' + ('PASS' if behaved else 'FAIL'))
            return 0 if behaved else 1
        if a.probe:
            rc, lines = probe(a, work)
            print('\n'.join(lines[-4:]))
            print('REAL-USE PROBE: ' + ('PASS' if rc == 0 else 'FAIL exit %d' % rc))
            return 0 if rc == 0 else 1
        home = private_home(work)
        t0 = time.time()
        tasks, recs, rows, second = session(a, work, mono, home)
        remaining = a.budget_min * 60 - (time.time() - t0)
        chk = check(work, remaining)          # the independent checker counts against the arm budget
        wall = round(time.time() - t0, 1)
        v = verdicts(tasks, recs, rows, second, chk, seal, task_cap_s=a.task_cap_s, budget_min=a.budget_min,
                     arm_wall_s=wall, soft_tokens=a.soft_tokens)
        m = metrics(tasks, recs, rows)
        overall = all(ok for ok, _ in v.values())
        for k, (ok, why) in v.items():
            print('REAL-USE %s: %s %s' % (k, 'PASS' if ok else 'FAIL', why))
        print('REAL-USE wall: %.1f min (bound 30-45)' % (wall / 60))
        (a.out / 'real-use.json').write_text(json.dumps(dict(
            verdict='PASS' if overall else 'FAIL', seal=seal, wall_s=wall, tasks=tasks,
            verdicts={k: dict(ok=ok, why=why) for k, (ok, why) in v.items()}, metrics=m,
            second_client=second['statuses']), indent=1, default=str))
        print('REAL-USE: ' + ('PASS' if overall else 'FAIL'))
        return 0 if overall else 1
    finally:
        if a.keep_work:
            print('work dir kept: %s' % work)
        else:
            shutil.rmtree(work, ignore_errors=True)


if __name__ == '__main__':
    sys.exit(main())
