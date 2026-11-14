#!/usr/bin/env python3
"""OEM release-gate drills (OEM-DONE.md R2-R5, sec. 2(e)): kill -9, corrupt block, guard trip, swap x3.

Two modes, stdlib only:

  oem_drills.py hw <kill9|corrupt|guard|swap|all> [--controller URL] [--out DIR] ...
      The SHORT live drill against the installed controller (each < 5 min, hard cap 300 s).
      Every request carries `X-Local-AI-Origin: lab`, so nothing counts as organic usage and no
      lab failure becomes the user's `last_error`. Each drill checks its precondition, makes the
      fault, then asserts self-healing (the next request answers 200, status is sane, nothing is
      left half-dead) and a clear user-facing message (plain text, no traceback or path). One JSON
      receipt per drill goes to --out (default $XDG_STATE_HOME/local-ai/drills/<stamp>, 0700).
      Exit 0 only when every drill passes. Check results are PASS, FAIL or GAP: a GAP is a known
      controller limit (for example a raw guard code instead of a sentence) that is reported but
      does not fail the gate.

  oem_drills.py ci
      The model-free CI drills: tests/test_oem_drills.py through tools/run_lab_tests.sh.

Drills (hw):
  kill9    SIGKILL the engine child mid-decode of a streamed chat. Expect a truncated stream or a
           502 `backend_died`, an `engine_died` line in engine-stderr.log, then a 200 from a
           respawned engine.                                             ~1-2 min, unattended
  corrupt  Two turns with a fixed X-Session-Id, unload, flip one byte in the newest block that
           only this session's manifest references (--truncate: also cut a second one), then a
           third turn must answer 200. Reports whether the bad file was removed. Never touches a
           file another manifest references.                             ~2-3 min, unattended
  guard    Bounded memory pressure in a separate process (systemd-run --user --scope with
           MemoryMax when available, else a child with RLIMIT_AS) until the controller's memory
           guard stops the model or 90 s pass; freed at once. Expect a clean unload, a plain
           last_failure reason and a 200 afterwards. attended: recommended (it squeezes desktop
           memory); refused without --i-understand-memory-pressure.    ~1-2 min, attended
  oxa      The coding agent `local-ai code` launches: installed at ~/.local/bin/oxa (or --agent),
           executable, `--version` answers the release version (--agent-version, default 0.2.0),
           bubblewrap (`bwrap`) on PATH for its bash tool, the controller's /v1/models answers, and
           `oxa --list-models` pointed at the controller (OXA_ENGINE_URL, as `local-ai code` sets it)
           lists the selected model. Loads no model, sends no prompt.     ~10 s, unattended
  swap     Cycle ornith-35b-a3b-local and qwen3.6-35b-a3b-local x3 (select + 1-token chat each,
           fixed per-model session). Every chat 200, status model matches, and from cycle 2 on
           the returning model's session resumes (timings cache_n > 0; 'unknown' when the
           response has no timings).                                     ~3-5 min, unattended
"""
import argparse
import hashlib
import hmac
import http.client
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import threading
import time
from urllib.parse import urlsplit

REPO = Path(__file__).resolve().parents[1]
ORIGIN_HEADER, LAB_ORIGIN = 'X-Local-AI-Origin', 'lab'
DRILLS = ('oxa', 'kill9', 'corrupt', 'swap', 'guard')  # `all` runs them in this order, guard last
AGENT_VERSION = '0.2.0'
CAP_SECONDS = 300
GUARD_SECONDS = 90
MOE_MODELS = ('ornith-35b-a3b-local', 'qwen3.6-35b-a3b-local')
ATTENDED = {'oxa': 'no', 'kill9': 'no', 'corrupt': 'no', 'guard': 'recommended', 'swap': 'no'}
EXPECTED = {'oxa': '~10 s', 'kill9': '1-2 min', 'corrupt': '2-3 min', 'guard': '1-2 min', 'swap': '3-5 min'}
LOOPBACK = ('127.0.0.1', 'localhost', '::1')
GIB = 1024**3


class DrillTimeout(Exception):
    pass


class Refused(Exception):
    """A precondition does not hold; nothing was changed."""


# ---------------------------------------------------------------- plain-message checks

PATHLIKE = re.compile(r'(?:^|[\s(\'"=])(?:/home/|/tmp/|/usr/|/proc/|/run/|/var/|~/)|\.py\b|\.rs:\d')


def plain_message(text):
    """(ok, why): a short single-line text with no traceback and no file path."""
    if not isinstance(text, str) or not text.strip():
        return False, 'empty'
    if 'Traceback' in text or re.search(r'File "[^"]+", line \d+', text):
        return False, 'traceback'
    if PATHLIKE.search(text):
        return False, 'path'
    if '\n' in text.strip():
        return False, 'multi-line'
    if len(text) > 300:
        return False, 'too long'
    return True, ''


def is_sentence(text):
    """A plain message that reads as words, not a bare code like 'host_memory_pressure_sustained'."""
    ok, _ = plain_message(text)
    return ok and not re.fullmatch(r'[A-Za-z0-9_.:-]+', text.strip()) and len(text.split()) >= 3


# ---------------------------------------------------------------- receipts and deadlines

class Deadline:
    def __init__(self, seconds):
        self.seconds = seconds
        self.end = time.monotonic() + seconds

    def remaining(self):
        return self.end - time.monotonic()

    def check(self):
        if self.remaining() <= 0:
            raise DrillTimeout(f'time cap of {self.seconds:.0f} s reached')

    def timeout(self, wanted):
        self.check()
        return max(0.5, min(wanted, self.remaining()))

    def sleep(self, seconds):
        self.check()
        time.sleep(max(0.0, min(seconds, self.remaining())))


class Receipt:
    def __init__(self, drill, controller, cap):
        self.value = {'drill': drill, 'attended': ATTENDED[drill], 'expected_wall': EXPECTED[drill],
                      'controller': controller, 'origin': LAB_ORIGIN, 'cap_seconds': cap,
                      'started_at': time.strftime('%Y-%m-%dT%H:%M:%S%z'), 'result': None,
                      'checks': [], 'observations': {}, 'error': None}
        self.started = time.monotonic()

    def check(self, name, ok, detail='', gap=False):
        """Record one assertion; gap=True turns a failure into GAP (known limit, not fatal)."""
        status = 'PASS' if ok else ('GAP' if gap else 'FAIL')
        self.value['checks'].append({'name': name, 'status': status, 'detail': str(detail)[:400]})
        return ok

    def observe(self, key, value):
        self.value['observations'][key] = value

    def fail(self, error):
        self.value['error'] = str(error)[:400]

    def finish(self):
        self.value['wall_seconds'] = round(time.monotonic() - self.started, 1)
        self.value['finished_at'] = time.strftime('%Y-%m-%dT%H:%M:%S%z')
        failed = self.value['error'] is not None or any(c['status'] == 'FAIL' for c in self.value['checks'])
        self.value['result'] = 'FAIL' if failed or not self.value['checks'] else 'PASS'
        return self.value

    @property
    def passed(self):
        return self.value['result'] == 'PASS'


def default_out():
    state = os.environ.get('XDG_STATE_HOME') or str(Path.home() / '.local/state')
    return Path(state) / 'local-ai/drills' / time.strftime('%Y%m%d-%H%M%S')


def private_dir(path):
    path = Path(path)
    path.mkdir(parents=True, exist_ok=True, mode=0o700)
    os.chmod(path, 0o700)
    return path


def write_receipt(out, value):
    path = Path(out) / f"{value['drill']}.json"
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, 'w') as f:
        json.dump(value, f, indent=2, sort_keys=True)
        f.write('\n')
    return path


# ---------------------------------------------------------------- the controller client

class Client:
    """Loopback HTTP to the controller; every request carries the lab origin header."""

    def __init__(self, base, deadline):
        parts = urlsplit(base)
        if parts.scheme != 'http' or parts.hostname not in LOOPBACK:
            raise Refused('the drills only talk to a loopback controller (http://127.0.0.1:PORT)')
        self.host, self.port = parts.hostname, parts.port or 80
        self.deadline = deadline

    def connection(self, timeout):
        return http.client.HTTPConnection(self.host, self.port, timeout=self.deadline.timeout(timeout))

    def headers(self, extra=None):
        return {'Content-Type': 'application/json', ORIGIN_HEADER: LAB_ORIGIN, **(extra or {})}

    def request(self, method, path, payload=None, headers=None, timeout=60):
        """(status, body bytes); (None, b'') when the connection fails."""
        conn = self.connection(timeout)
        try:
            body = None if payload is None else json.dumps(payload).encode()
            conn.request(method, path, body=body, headers=self.headers(headers))
            response = conn.getresponse()
            return response.status, response.read()
        except (OSError, http.client.HTTPException) as error:
            return None, str(type(error).__name__).encode()
        finally:
            conn.close()

    def json(self, method, path, payload=None, headers=None, timeout=60):
        status, data = self.request(method, path, payload, headers, timeout)
        try:
            value = json.loads(data) if data else None
        except ValueError:
            value = None
        return status, value

    def status(self):
        code, value = self.json('GET', '/_local/status', timeout=10)
        return value if code == 200 and isinstance(value, dict) else None

    def chat(self, model, messages, max_tokens=8, session=None, timeout=240):
        payload = {'model': model, 'messages': messages, 'max_tokens': max_tokens,
                   'temperature': 0, 'stream': False}
        return self.json('POST', '/v1/chat/completions', payload,
                         {'X-Session-Id': session} if session else None, timeout)

    def stream(self, model, messages, max_tokens, timeout=240):
        """Open a streamed chat; returns (connection, response)."""
        conn = self.connection(timeout)
        payload = {'model': model, 'messages': messages, 'max_tokens': max_tokens,
                   'temperature': 0, 'stream': True}
        conn.request('POST', '/v1/chat/completions', body=json.dumps(payload).encode(), headers=self.headers())
        return conn, conn.getresponse()


def content_of(value):
    try:
        return value['choices'][0]['message'].get('content') or ''
    except (KeyError, IndexError, TypeError, AttributeError):
        return ''


def cache_n(value):
    """Cached prompt tokens of a chat response: timings.cache_n, else usage cached_tokens, else None."""
    if not isinstance(value, dict):
        return None
    timings = value.get('timings')
    if isinstance(timings, dict) and type(timings.get('cache_n')) is int:
        return timings['cache_n']
    details = (value.get('usage') or {}).get('prompt_tokens_details') if isinstance(value.get('usage'), dict) else None
    if isinstance(details, dict) and type(details.get('cached_tokens')) is int:
        return details['cached_tokens']
    return None


def error_message(value):
    if isinstance(value, dict):
        error = value.get('error')
        if isinstance(error, dict):
            return error.get('message')
        if isinstance(error, str):
            return error
    return None


def wait_for(predicate, deadline, seconds, poll=0.5):
    end = time.monotonic() + seconds
    while True:
        result = predicate()
        if result:
            return result
        if time.monotonic() >= end:
            return result
        deadline.sleep(poll)


def status_sane(receipt, status, name='status sane'):
    """No stop pending, not stopping, nothing running, and any user-facing error plain."""
    if status is None:
        return receipt.check(name, False, 'no /_local/status answer')
    problems = []
    if status.get('stop_pending'):
        problems.append('stop_pending set')
    if status.get('state') not in ('ready', 'unloaded'):
        problems.append(f"state {status.get('state')!r}")
    if status.get('active_requests'):
        problems.append('active requests left')
    error = status.get('last_error')
    if error is not None and not plain_message(error)[0]:
        problems.append('last_error is not plain: ' + plain_message(error)[1])
    return receipt.check(name, not problems, '; '.join(problems) or f"state {status.get('state')}")


def cli_status():
    """`local-ai status --json` as the user sees it (only last_failure/has_error), when installed."""
    exe = shutil.which('local-ai')
    if exe is None:
        return None
    try:
        done = subprocess.run([exe, 'status', '--json'], capture_output=True, text=True, timeout=15)
        value = json.loads(done.stdout)
    except (OSError, subprocess.SubprocessError, ValueError):
        return None
    return {k: value.get(k) for k in ('last_failure', 'has_error', 'state') if isinstance(value, dict)}


# ---------------------------------------------------------------- processes and ports

def listening_inodes(port):
    found = set()
    for name in ('/proc/net/tcp', '/proc/net/tcp6'):
        try:
            lines = Path(name).read_text().splitlines()[1:]
        except OSError:
            continue
        for line in lines:
            fields = line.split()
            if len(fields) > 9 and fields[3] == '0A' and int(fields[1].rsplit(':', 1)[1], 16) == port:
                found.add(fields[9])
    return found


def socket_owners(inodes):
    if not inodes:
        return set()
    wanted = {f'socket:[{i}]' for i in inodes}
    owners = set()
    for proc in Path('/proc').iterdir():
        if not proc.name.isdigit():
            continue
        try:
            for fd in (proc / 'fd').iterdir():
                if os.readlink(fd) in wanted:
                    owners.add(int(proc.name))
                    break
        except OSError:
            continue
    return owners


def parent_pid(pid):
    try:
        text = Path(f'/proc/{pid}/stat').read_text()
        return int(text.rsplit(')', 1)[1].split()[1])
    except (OSError, ValueError, IndexError):
        return None


def alive(pid):
    try:
        state = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()[0]
        return state != 'Z'
    except (OSError, IndexError):
        return False


def engine_pid(controller_port, backend_ports):
    """(pid, port) of the engine listening on a backend port, a descendant of the controller."""
    listening = {port: listening_inodes(port) for port in backend_ports}
    if not any(listening.values()):
        return None, None
    controllers = socket_owners(listening_inodes(controller_port))
    for port in backend_ports:
        for pid in sorted(socket_owners(listening[port])):
            if pid in controllers:
                continue
            ancestor = pid
            for _ in range(6):
                ancestor = parent_pid(ancestor)
                if ancestor is None or ancestor <= 1:
                    break
                if ancestor in controllers:
                    return pid, port
    return None, None


def meminfo():
    values = {}
    for line in Path('/proc/meminfo').read_text().splitlines():
        key, _, rest = line.partition(':')
        values[key] = int(rest.split()[0]) * 1024
    return values


# ---------------------------------------------------------------- shared drill steps

def warm(client, receipt, model, name='warm chat answers 200'):
    code, value = client.chat(model, [{'role': 'user', 'content': 'Reply with the single word: ready.'}], 4)
    receipt.check(name, code == 200, f'HTTP {code}')
    return code == 200


def precondition(client, receipt, args):
    status = client.status()
    if status is None:
        raise Refused('the controller does not answer /_local/status')
    if status.get('stop_pending') or status.get('state') == 'stopping':
        raise Refused('the controller is still stopping an engine; retry later')
    if status.get('active_requests'):
        raise Refused('a request is running; the drills need an idle controller')
    model = args.model or status.get('model') or status.get('selected_model')
    if not isinstance(model, str):
        raise Refused('no model is selected')
    receipt.observe('model', model)
    return status, model


def engine_log_lines(path, offset):
    try:
        with open(path, 'rb') as f:
            f.seek(offset)
            data = f.read(4 * 1024**2)
    except OSError:
        return []
    events = []
    for line in data.decode('utf-8', 'replace').splitlines():
        try:
            value = json.loads(line)
        except ValueError:
            continue
        if isinstance(value, dict):
            events.append(value)
    return events


def log_size(path):
    try:
        return Path(path).stat().st_size
    except OSError:
        return 0


# ---------------------------------------------------------------- kill9

def drill_kill9(client, receipt, args, deadline):
    _, model = precondition(client, receipt, args)
    if not warm(client, receipt, model):
        return
    pid, port = engine_pid(client.port, args.backend_ports)
    if not receipt.check('engine pid found (backend port owner, child of the controller)', pid is not None,
                         f'pid {pid} on port {port}'):
        return
    log = Path(args.engine_log)
    offset = log_size(log)
    first_chunk, result = threading.Event(), {}

    def read_stream():
        conn = None
        try:
            conn, response = client.stream(model, [{'role': 'user', 'content':
                                           'Count from 1 to 300 in words, separated by commas.'}], 64)
            result['status'] = response.status
            body = b''
            try:
                while chunk := response.read1(65536):
                    body += chunk
                    if b'"content"' in body:
                        first_chunk.set()
                result['complete'] = True
            except (http.client.IncompleteRead, OSError, http.client.HTTPException) as error:
                result['complete'] = False
                result['read_error'] = type(error).__name__
            result['body'] = body[-4096:]
        except (OSError, http.client.HTTPException) as error:
            result['error'] = type(error).__name__
        finally:
            first_chunk.set()
            if conn is not None:
                conn.close()

    reader = threading.Thread(target=read_stream, daemon=True)
    reader.start()
    first_chunk.wait(deadline.timeout(150))
    if result.get('complete') or 'error' in result:
        receipt.check('fault landed mid-decode', False, f'stream ended before the kill: {result}')
        return
    os.kill(pid, signal.SIGKILL)
    killed_at = time.monotonic()
    receipt.observe('killed_pid', pid)
    reader.join(deadline.timeout(60))
    body = result.get('body', b'')
    done = b'[DONE]' in body
    status = result.get('status')
    receipt.observe('stream', {'status': status, 'saw_done': done, 'complete': result.get('complete'),
                               'read_error': result.get('read_error')})
    message = None
    if status == 502:
        try:
            value = json.loads(body)
            message = error_message(value)
            kind = (value.get('error') or {}).get('type')
        except (ValueError, AttributeError):
            kind = None
        receipt.check('crash reported: 502 backend_died', kind == 'backend_died', f'type {kind}')
    else:
        receipt.check('crash reported: stream ends incomplete (no [DONE])', status == 200 and not done,
                      f'HTTP {status}, [DONE] seen: {done}')
    died = wait_for(lambda: [e for e in engine_log_lines(log, offset)
                             if e.get('event') == 'engine_died' and e.get('pid') == pid], deadline, 20)
    receipt.check('engine_died line in engine-stderr.log', bool(died),
                  f"exit_code {died[0].get('exit_code')}" if died else f'nothing new in {log.name}')
    receipt.check('killed engine is gone (no zombie)', wait_for(lambda: not alive(pid), deadline, 15), f'pid {pid}')
    status_value = wait_for(lambda: (lambda s: s if s and not s.get('active_requests')
                                     and s.get('state') != 'stopping' else None)(client.status()), deadline, 30)
    receipt.check('controller released the request and the engine', status_value is not None)
    code, value = client.chat(model, [{'role': 'user', 'content': 'Reply with the single word: back.'}], 4)
    receipt.check('next chat answers 200 (respawn)', code == 200 and bool(content_of(value)),
                  f'HTTP {code}, {round(time.monotonic() - killed_at, 1)} s after the kill')
    new_pid, _ = engine_pid(client.port, args.backend_ports)
    receipt.check('a new engine process answers', new_pid is not None and new_pid != pid, f'new pid {new_pid}')
    after = client.status()
    status_sane(receipt, after)
    if message is not None:
        ok, why = plain_message(message)
        receipt.check('user-facing message is plain', ok, f'{message!r} {why}')
    else:
        # The stream had already started: the controller closes it without an error event and
        # records no last_failure for a crash, so the user sees a cut-off answer only.
        receipt.check('user-facing message for a crash mid-stream', False,
                      'no message: the stream is cut without an SSE error event and last_failure stays empty',
                      gap=True)
    receipt.observe('cli_status', cli_status())


# ---------------------------------------------------------------- corrupt

def runtime_config():
    path = os.environ.get('LOCAL_LLM_CONFIG') or str(
        Path(os.environ.get('XDG_CONFIG_HOME') or Path.home() / '.config') / 'local-ai/llm/runtime.json')
    try:
        value = json.loads(Path(path).read_text())
        return value if isinstance(value, dict) else None
    except (OSError, ValueError):
        return None


def state_cache_module():
    sys.path.insert(0, str(REPO / 'src'))
    from snapdragon_ai_lab.runtime import state_cache
    return state_cache


def session_hint(sid):
    """The manifest `hint` for an X-Session-Id: hint_hash of the ledger's keyed session hash
    (request_ledger.session_key, cache_turn). None when the install salt is not readable."""
    ledger = Path(os.environ.get('LOCAL_AI_LEDGER_DIR') or Path.home() / '.local/state/local-ai/ledger')
    try:
        salt = (ledger / '.salt').read_bytes()
    except OSError:
        return None
    if len(salt) != 32:
        return None
    key = hmac.new(salt, ('h:' + sid).encode(), hashlib.sha256).hexdigest()[:16]
    return hashlib.sha256(key.encode()).hexdigest()[:32]


def manifest_stamps(sc, root):
    out = {}
    for key, model in sc.scan(root).items():
        for rel, (size, mtime) in model['files'].items():
            if rel.startswith('sessions/'):
                out[(key, rel)] = (size, mtime)
    return out


def digest(path):
    try:
        return hashlib.sha256(Path(path).read_bytes()).hexdigest()[:16]
    except OSError:
        return None


def drill_corrupt(client, receipt, args, deadline):
    status, model = precondition(client, receipt, args)
    sc = state_cache_module()
    root = Path(args.state_root) if args.state_root else sc.root(runtime=runtime_config())
    models = sc.scan(root)
    if not any(rel.startswith('blocks/') for m in models.values() for rel in m['files']):
        raise Refused('the state-cache store has no blocks (is the disk state cache on for this model?)')
    sid = f'oem-drill-corrupt-{int(time.time())}'
    nonce = hashlib.sha256(sid.encode()).hexdigest()[:12]
    filler = '\n'.join(f'Drill note {i} ({nonce}): the harbour lamp at pier {i} is checked at dawn and dusk.'
                       for i in range(110))
    history = [{'role': 'user', 'content': f'{filler}\n\nRead the notes above. Reply with the single word: one.'}]
    before = manifest_stamps(sc, root)
    started = time.time()
    for turn, ask in ((1, None), (2, 'Reply with the single word: two.')):
        if ask:
            history += [{'role': 'user', 'content': ask}]
        code, value = client.chat(model, history, 8, sid)
        if not receipt.check(f'turn {turn} answers 200', code == 200, f'HTTP {code}'):
            return
        history += [{'role': 'assistant', 'content': content_of(value)}]
    # Unload so no engine writes the store while we flip a byte and the next turn reads from disk.
    code, value = client.json('POST', '/_local/unload', {}, timeout=60)
    receipt.check('unload answers 200', code == 200, f'HTTP {code} {error_message(value) or ""}')
    wait_for(lambda: (client.status() or {}).get('state') == 'unloaded', deadline, 30)
    models = sc.scan(root)
    hint = session_hint(sid)
    found = [(key, sid_) for key, m in models.items() for sid_, s in m['sessions'].items()
             if hint is not None and s['hint'] == hint]
    # forks of the drill conversation share its hint: take the newest, keep the others as ours
    candidates = sorted(found, key=lambda c: models[c[0]]['sessions'][c[1]]['atime'])[-1:]
    how = 'hint'
    if not candidates:
        how = 'changed manifest'
        changed = [(key, rel) for (key, rel), stamp in manifest_stamps(sc, root).items()
                   if before.get((key, rel)) != stamp and stamp[1] >= started - 1]
        candidates = [(key, rel[len('sessions/'):-len('.json')]) for key, rel in changed]
        candidates = [c for c in candidates if c[1] in models.get(c[0], {}).get('sessions', {})]
    receipt.observe('manifest_found_by', how)
    if len(candidates) != 1:
        raise Refused(f'expected exactly one manifest for the drill session, found {len(candidates)} '
                      '(another conversation may have written the store meanwhile)')
    key, manifest_sid = candidates[0]
    store = models[key]
    mine = store['sessions'][manifest_sid]['refs']
    ours = {s for k, s in found if k == key} | {manifest_sid}
    others = set().union(set(), *[s['refs'] for k, s in store['sessions'].items() if k not in ours])
    if store['opaque']:
        raise Refused('an unreadable manifest in this model may reference any block; nothing is touched')
    exclusive = sorted((rel for rel in mine if rel.startswith('blocks/') and rel not in others
                        and rel in store['files']), key=lambda rel: store['files'][rel][1], reverse=True)
    if not exclusive:
        raise Refused('no block belongs to the drill session alone (the prompt fits inside shared blocks)')
    targets = exclusive[:2] if args.truncate and len(exclusive) > 1 else exclusive[:1]
    record = []
    for i, rel in enumerate(targets):
        path = root / key / rel
        old = digest(path)
        data = bytearray(path.read_bytes())
        if i == 0:
            data[len(data) // 2] ^= 0xFF
            action = 'flip 1 byte'
        else:
            data = data[:len(data) // 2]
            action = 'truncate to half'
        with open(path, 'r+b') as f:
            f.write(data)
            f.truncate(len(data))
        record.append({'block': rel.rsplit('/', 1)[1][:16] + '...', 'action': action, 'sha_before': old,
                       'sha_after': digest(path), 'size': len(data)})
    receipt.observe('corrupted', record)
    receipt.observe('model_key', key)
    log = Path(args.engine_log)
    offset = log_size(log)
    history += [{'role': 'user', 'content': 'Reply with the single word: three.'}]
    code, value = client.chat(model, history, 8, sid)
    answer = content_of(value)
    receipt.check('turn 3 after the corruption answers 200 with an answer', code == 200 and bool(answer.strip()),
                  f'HTTP {code}, {len(answer)} chars, cache_n {cache_n(value)}')
    receipt.observe('turn3_cache_n', cache_n(value))
    receipt.observe('turn3_answer', answer[:80])
    fates = []
    for i, rel in enumerate(targets):
        path = root / key / rel
        now = digest(path)
        fate = 'removed' if now is None else ('still_corrupt' if now == record[i]['sha_after'] else 'rewritten')
        fates.append(fate)
    receipt.observe('bad_file_fate', fates)
    receipt.check('bad block dropped or rewritten', all(f != 'still_corrupt' for f in fates), ', '.join(fates),
                  gap=True)
    receipt.observe('engine_log_events', sorted({str(e.get('event')) for e in engine_log_lines(log, offset)}))
    after = client.status()
    status_sane(receipt, after)
    if after is not None:
        failure = after.get('last_failure')
        receipt.check('no failure recorded for the corrupted turn', not failure or failure.get('t', 0) < started,
                      json.dumps(failure)[:200])


# ---------------------------------------------------------------- guard

HOG = r'''
import os, sys, time
cap, seconds = int(sys.argv[1]), float(sys.argv[2])
parent = os.getppid()  # the drill (or systemd-run); a change means the drill went away
chunk = 256 * 1024**2
held, total, end = [], 0, time.monotonic() + seconds
while time.monotonic() < end and os.getppid() == parent:
    if total + chunk <= cap:
        block = bytearray(chunk)
        block[::4096] = b'\x01' * len(range(0, chunk, 4096))
        held.append(block)
        total += chunk
        print(total, flush=True)
        time.sleep(0.05)
    else:
        time.sleep(0.2)
'''


def start_hog(cap, seconds, use_scope=True):
    """The pressure child: systemd-run --user --scope -p MemoryMax=cap when present, always under
    RLIMIT_AS (cap + 512 MiB) and its own byte cap; exits when the drill process goes away."""
    import resource

    def limit():
        resource.setrlimit(resource.RLIMIT_AS, (cap + 512 * 1024**2, cap + 512 * 1024**2))
    command = [sys.executable, '-c', HOG, str(cap), str(max(1.0, seconds))]
    scope = shutil.which('systemd-run') if use_scope else None
    if scope:
        command = [scope, '--user', '--scope', '--quiet', '--collect', '-p', f'MemoryMax={cap + 256 * 1024**2}',
                   *command]
    return subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True,
                            preexec_fn=limit), bool(scope)


def stop_hog(hog):
    if hog is None or hog.poll() is not None:
        return
    try:
        hog.kill()
    except OSError:
        pass
    try:
        hog.wait(10)
    except subprocess.TimeoutExpired:
        pass


def drill_guard(client, receipt, args, deadline):
    _, model = precondition(client, receipt, args)
    if not warm(client, receipt, model):
        return
    pid, _ = engine_pid(client.port, args.backend_ports)
    receipt.observe('engine_pid', pid)
    mem = meminfo()
    available, swap_free = mem.get('MemAvailable', 0), mem.get('SwapFree', 0)
    # GUARD-2 stops at MemAvailable + SwapFree < 2 GiB (or sustained PSI full > 20 %) for 3 s;
    # aim at ~1 GiB left so the floor trips even if PSI stays calm.
    cap = max(GIB, available + swap_free - GIB)
    receipt.observe('memory_before', {'available_gib': round(available / GIB, 1), 'swap_free_gib': round(swap_free / GIB, 1),
                                      'hog_cap_gib': round(cap / GIB, 1)})
    started = int(time.time())
    hog = None
    tripped = None
    try:
        hog, scoped = start_hog(cap, min(GUARD_SECONDS, deadline.remaining() - 5), not args.no_scope)
        receipt.observe('pressure_process', 'systemd-run scope' if scoped else 'plain child (RLIMIT_AS)')
        end = time.monotonic() + GUARD_SECONDS
        while time.monotonic() < end and hog.poll() is None:
            status = client.status()
            failure = (status or {}).get('last_failure') or {}
            if failure.get('kind') == 'guard' and (failure.get('t') or 0) >= started:
                tripped = status
                break
            deadline.sleep(0.5)
    finally:
        stop_hog(hog)
    receipt.observe('hog_exit', hog.poll() if hog is not None else None)
    if not receipt.check('memory guard stopped the model within 90 s', tripped is not None,
                         'no guard stop recorded'):
        return
    wait_for(lambda: meminfo().get('MemAvailable', 0) > available * 0.6, deadline, 30)
    status = wait_for(lambda: (lambda s: s if s and s.get('state') == 'unloaded' else None)(client.status()),
                      deadline, 20)
    receipt.check('model unloaded cleanly (state unloaded, no stop pending)',
                  status is not None and not status.get('stop_pending'), json.dumps((status or {}).get('state')))
    if pid is not None:
        receipt.check('engine process is gone', wait_for(lambda: not alive(pid), deadline, 15), f'pid {pid}')
    reason = (tripped.get('last_failure') or {}).get('reason')
    receipt.observe('last_failure', tripped.get('last_failure'))
    receipt.observe('last_error', tripped.get('last_error'))
    ok, why = plain_message(reason)
    receipt.check('last_failure.reason is plain (no traceback, no path)', ok, f'{reason!r} {why}')
    receipt.check('last_failure.reason reads as a sentence', is_sentence(reason),
                  f'{reason!r} is a raw guard code' if ok else why, gap=True)
    receipt.check('lab-origin stop is not the user\'s last_error', tripped.get('last_error') is None,
                  repr(tripped.get('last_error')))
    code, value = client.chat(model, [{'role': 'user', 'content': 'Reply with the single word: back.'}], 4)
    receipt.check('next chat answers 200', code == 200 and bool(content_of(value)), f'HTTP {code}')
    status_sane(receipt, client.status())
    receipt.observe('cli_status', cli_status())


# ---------------------------------------------------------------- swap

def drill_swap(client, receipt, args, deadline):
    precondition(client, receipt, args)
    stamp = int(time.time())
    prompts = {}
    for alias in MOE_MODELS:
        nonce = hashlib.sha256(f'{alias}{stamp}'.encode()).hexdigest()[:12]
        filler = '\n'.join(f'Swap note {i} ({nonce}): shelf {i} holds the blue folders.' for i in range(70))
        prompts[alias] = [{'role': 'user', 'content': f'{filler}\n\nReply with OK.'}]
    rows, previous_pid = [], None
    for cycle in (1, 2, 3):
        for alias in MOE_MODELS:
            row = {'cycle': cycle, 'model': alias}
            for attempt in range(4):
                code, value = client.json('POST', '/_local/select', {'model': alias}, timeout=90)
                if code not in (409, 503):
                    break
                deadline.sleep(2)
            row['select'] = code
            if code != 200:
                message = error_message(value)
                receipt.check(f'cycle {cycle} select {alias}', False, f'HTTP {code} {message or ""}')
                if message is not None:
                    receipt.check('select error message is plain', plain_message(message)[0], repr(message))
                rows.append(row)
                continue
            code, value = client.chat(alias, prompts[alias], 1, f'oem-drill-swap-{alias}-{stamp}')
            row['chat'] = code
            row['cache_n'] = cache_n(value)
            status = client.status() or {}
            row['status_model'] = status.get('model')
            pid, _ = engine_pid(client.port, args.backend_ports)
            row['previous_engine_gone'] = None if previous_pid is None or pid is None else not alive(previous_pid)
            previous_pid = pid or previous_pid
            receipt.check(f'cycle {cycle} {alias}: chat 200', code == 200, f'HTTP {code}')
            receipt.check(f'cycle {cycle} {alias}: status model matches',
                          status.get('model') == alias and status.get('state') == 'ready',
                          f"{status.get('model')} {status.get('state')}")
            if row['previous_engine_gone'] is not None:
                receipt.check(f'cycle {cycle} {alias}: previous engine exited', row['previous_engine_gone'])
            if cycle >= 2:
                n = row['cache_n']
                if n is None:
                    receipt.check(f'cycle {cycle} {alias}: session resumes', False, 'unknown (no timings)', gap=True)
                else:
                    receipt.check(f'cycle {cycle} {alias}: session resumes', n > 0, f'cache_n {n}')
            rows.append(row)
    receipt.observe('cycles', rows)
    status_sane(receipt, client.status(), 'status sane after the swaps')


def agent_path(args):
    return Path(args.agent) if getattr(args, 'agent', None) else Path.home() / '.local/bin/oxa'


def run_agent(args, argv, env_extra=None, timeout=20):
    """(returncode, stdout, stderr) of the agent; (None, '', error name) when it cannot run."""
    env = {**os.environ, **(env_extra or {})}
    try:
        done = subprocess.run([str(agent_path(args)), *argv], capture_output=True, text=True, timeout=timeout,
                              stdin=subprocess.DEVNULL, env=env, check=False)
    except (OSError, subprocess.SubprocessError) as error:
        return None, '', type(error).__name__
    return done.returncode, done.stdout, done.stderr


def drill_oxa(client, receipt, args, deadline):
    path = agent_path(args)
    receipt.observe('agent', str(path))
    ok = path.is_file() and not path.is_symlink() and os.access(path, os.X_OK)
    receipt.check('oxa installed and executable', ok, str(path))
    if not ok:
        return
    code, out, err = run_agent(args, ['--version'], timeout=deadline.timeout(10))
    version = re.search(r'\b\d+\.\d+\.\d+\S*', out or '')
    receipt.observe('version', version.group(0) if version else None)
    receipt.check('oxa --version answers', code == 0 and version is not None, f'exit {code}')
    want = getattr(args, 'agent_version', None) or AGENT_VERSION
    receipt.check(f'oxa is version {want}', bool(version) and version.group(0) == want,
                  version.group(0) if version else 'none')
    bwrap = shutil.which('bwrap')
    receipt.check('bwrap present (oxa bash tool sandbox)', bwrap is not None, bwrap or 'not on PATH')
    status = client.status() or {}
    model = args.model or status.get('model') or status.get('selected_model')
    code, value = client.json('GET', '/v1/models', timeout=10)
    ids = [r.get('id') for r in (value or {}).get('data') or [] if isinstance(r, dict)] if code == 200 else []
    receipt.check('controller /v1/models answers', code == 200 and bool(ids), f'HTTP {code}, {len(ids)} models')
    url = 'http://%s:%d' % (client.host, client.port)
    code, out, err = run_agent(args, ['--list-models'], {'OXA_ENGINE_URL': url}, timeout=deadline.timeout(20))
    receipt.check('oxa --list-models against the controller', code == 0, f'exit {code}')
    if isinstance(model, str) and code == 0:
        receipt.check('oxa sees the selected model', model in (out or ''), model)
    message = (err or '').strip().splitlines()[-1] if code not in (0, None) and (err or '').strip() else None
    if message is not None:
        receipt.check('oxa error message is plain', plain_message(message)[0], repr(message[:200]))


RUNNERS = {'oxa': drill_oxa, 'kill9': drill_kill9, 'corrupt': drill_corrupt, 'guard': drill_guard, 'swap': drill_swap}


def run_drill(drill, args, out):
    cap = args.cap_seconds
    deadline = Deadline(cap)
    receipt = Receipt(drill, args.controller, cap)

    def on_alarm(signum, frame):
        raise DrillTimeout(f'hard time cap of {cap} s reached')
    old = signal.signal(signal.SIGALRM, on_alarm)
    signal.alarm(int(cap) + 5)
    try:
        client = Client(args.controller, deadline)
        RUNNERS[drill](client, receipt, args, deadline)
    except Refused as error:
        receipt.fail(f'refused: {error}')
    except DrillTimeout as error:
        receipt.fail(str(error))
    except Exception as error:  # a drill bug must still leave a receipt
        receipt.fail(f'{type(error).__name__}: {error}')
    finally:
        signal.alarm(0)
        signal.signal(signal.SIGALRM, old)
    value = receipt.finish()
    path = write_receipt(out, value)
    gaps = [c['name'] for c in value['checks'] if c['status'] == 'GAP']
    fails = [c['name'] for c in value['checks'] if c['status'] == 'FAIL']
    line = f"{value['result']} {drill} ({value['wall_seconds']} s)"
    if value['error']:
        line += f" - {value['error']}"
    if fails:
        line += ' - failed: ' + '; '.join(fails)
    if gaps:
        line += ' - gaps: ' + '; '.join(gaps)
    print(line, flush=True)
    print(f'  receipt: {path}', flush=True)
    return receipt.passed


def ports(text):
    try:
        values = tuple(int(p) for p in text.split(','))
    except ValueError:
        raise argparse.ArgumentTypeError('comma-separated port numbers') from None
    if not values or any(not 0 < p < 65536 for p in values):
        raise argparse.ArgumentTypeError('comma-separated port numbers')
    return values


def cap_seconds(text):
    try:
        value = float(text)
    except ValueError:
        raise argparse.ArgumentTypeError('seconds') from None
    if not 10 <= value <= CAP_SECONDS:
        raise argparse.ArgumentTypeError(f'between 10 and {CAP_SECONDS}')
    return value


def parser():
    top = argparse.ArgumentParser(prog='oem_drills.py', description=__doc__.split('\n\n')[0],
                                  formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = top.add_subparsers(dest='mode', required=True)
    epilog = '\n'.join(f'  {d:8} expected {EXPECTED[d]}, attended: {ATTENDED[d]}' for d in DRILLS)
    hw = sub.add_parser('hw', help='live hardware drills against the installed controller',
                        formatter_class=argparse.RawDescriptionHelpFormatter,
                        epilog='drills (hard cap 300 s each):\n' + epilog +
                        '\n\nguard is attended: recommended (it squeezes desktop memory) and needs'
                        '\n--i-understand-memory-pressure, also under `all`.')
    hw.add_argument('drill', choices=(*DRILLS, 'all'))
    hw.add_argument('--controller', default='http://127.0.0.1:8092', help='loopback controller URL')
    hw.add_argument('--out', help='receipt directory (default $XDG_STATE_HOME/local-ai/drills/<stamp>, 0700)')
    hw.add_argument('--model', help='model alias for kill9/corrupt/guard (default: the selected one)')
    hw.add_argument('--i-understand-memory-pressure', action='store_true', dest='memory_pressure_ok',
                    help='allow the guard drill (attended: recommended)')
    hw.add_argument('--engine-log', default=str(Path(os.environ.get('XDG_STATE_HOME') or Path.home() / '.local/state')
                                                / 'local-ai/engine-stderr.log'))
    hw.add_argument('--state-root', help='disk state-cache root (default: from runtime.json, as the controller)')
    hw.add_argument('--backend-ports', type=ports, default=(8093, 8094), help='engine ports (default 8093,8094)')
    hw.add_argument('--cap-seconds', type=cap_seconds, default=CAP_SECONDS, help='per-drill hard cap (<= 300)')
    hw.add_argument('--truncate', action='store_true', help='corrupt: also truncate a second block')
    hw.add_argument('--no-scope', action='store_true', help='guard: no systemd-run scope, plain child only')
    hw.add_argument('--agent', help='oxa: the coding agent binary (default ~/.local/bin/oxa)')
    hw.add_argument('--agent-version', default=AGENT_VERSION, help='oxa: expected version (default %(default)s)')
    sub.add_parser('ci', help='model-free CI drills (tests/test_oem_drills.py)')
    return top


def main(argv=None):
    args = parser().parse_args(argv)
    if args.mode == 'ci':
        return subprocess.call([str(REPO / 'tools/run_lab_tests.sh'), 'discover', '-s', 'tests',
                                '-p', 'test_oem_drills.py', '-v'], cwd=REPO)
    drills = DRILLS if args.drill == 'all' else (args.drill,)
    if 'guard' in drills and not args.memory_pressure_ok:
        print('refused: the guard drill squeezes desktop memory (attended: recommended); '
              'pass --i-understand-memory-pressure to run it.', file=sys.stderr)
        return 2
    try:
        Client(args.controller, Deadline(1))
    except Refused as error:
        print(f'refused: {error}', file=sys.stderr)
        return 2
    out = private_dir(args.out or default_out())
    results = [run_drill(drill, args, out) for drill in drills]
    print(f"{'PASS' if all(results) else 'FAIL'}: {sum(results)}/{len(results)} drills passed; receipts in {out}")
    return 0 if all(results) else 1


if __name__ == '__main__':
    sys.exit(main())
