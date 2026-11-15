"""v0.1.4 CORE-ROBUST (b): duplicate-request replay after a cut stream (V014-REPLAN section 3b).

On by default since v0.2.0 (A5; gate G-R verified in the release batch). OXC_REPLAY=0 turns it
off: a vanished client then cancels the engine as in v0.1.3 (ClientDisconnect shuts the engine connection down).

Eligible request: a local (never LAN-shared) POST to an inference path with `"stream": true`,
temperature exactly 0 after the engine defaults, a body, and an EXPLICIT session: a generic session
header (X-Session-Id / Session-Id) or else an Idempotency-Key. The ledger's first-user fallback is
never enough (two new sessions can share it): without either header the request is not replayable.
Key: HMAC-SHA256(per-process random salt; 'oxe-replay-v1', method, path, principal, explicit session,
engine identity (model alias + engine launch number), context mode, and either SHA-256 of the RAW
body or the client's `Idempotency-Key` (1-255 visible ASCII)). The SHA-256 of the raw body is
compared on attach in both cases: a non-identical body is never replayed, Idempotency-Key or not.

Limits (exact):
- Armed only by a CUT: the client went away before the engine's final chunk (the disconnect
  watcher saw EOF, or a write to it failed). A delivered answer is never replayed: when the client
  stays to the end the entry is deleted.
- On a cut the controller keeps reading the engine into RAM (never disk) for GRACE_S = 15 s.
  An identical retry in that time attaches: the recorded status and headers, the buffer from byte
  0, then the live tail; the engine runs to its end. No attach within 15 s of the cut: the engine
  is cancelled as in v0.1.3 and the entry dropped.
- An attached retry that leaves while the generation still runs (and no other reader is left)
  starts a new GRACE_S; then the engine is cancelled as above. Never unbounded.
- A cut while the engine is still in prefill (even when the first response head cannot be sent)
  records the engine's head and body: the engine keeps running into the buffer.
- A finished generation stays attachable until 15 s after the cut, or, once someone attached,
  until 15 s after the engine finished.
- MAX_BYTES = 8 MiB of engine body per entry (more: the replay is disarmed; a cut entry is then
  cancelled), MAX_ENTRIES = 4 entries at once, MAX_ATTACH = 4 attaches per entry.
- An identical retry that arrives while the first client still looks connected waits up to
  LIVE_WAIT_S = 2 s for the cut; otherwise it is an ordinary request (concurrent duplicates and
  benchmark repeats are computed, never shared).
- Another request of the same principal and session that is not identical cancels a cut entry at
  once (it gets the slot). v0.2.0: ANY request that has to wait for the slot cancels every cut entry
  nobody is attached to (within ctl_queue.POLL_S): a client's cancel never makes another request
  wait out the grace (a code client's abort through oxc settled in 4-26 s before, 0.2 s straight to oxe).
- Never across principals or sessions (both are in the key and checked on attach), never for a
  non-200 engine status, never for a shared (LAN) request, non-stream or temperature > 0.
Only the HMAC key and SHA-256 hashes are held besides the engine bytes; nothing is written.
"""
import hashlib
import hmac
import json
import os
import re
import secrets
import socket
import threading
import time
if __package__:
    from .ctl_proxy_prep import ANSWERED
    from . import ctl_queue, request_ledger
else:
    from ctl_proxy_prep import ANSWERED
    import ctl_queue
    import request_ledger

ENV = 'OXC_REPLAY'
GRACE_S = 15.0
LIVE_WAIT_S = 2.0
MAX_BYTES = 8 * 1024 * 1024
MAX_ENTRIES = 4
MAX_ATTACH = 4
IDEMPOTENCY_HEADER = 'Idempotency-Key'
_IDEM = re.compile(r'[\x21-\x7e]{1,255}')
REPLAY_PATHS = ('/v1/chat/completions', '/v1/completions', '/chat/completions', '/completion')
_SALT = secrets.token_bytes(32)  # per process: an entry never outlives the controller


OFF_VALUES = ('0', 'off', 'false', 'no')


def enabled():
    """On by default since v0.2.0 (A5); OXC_REPLAY=0 (or off/false/no) turns it off."""
    return os.environ.get(ENV, '1').strip().lower() not in OFF_VALUES


def _mac(*fields):
    return hmac.new(_SALT, json.dumps(fields, separators=(',', ':')).encode(), hashlib.sha256).hexdigest()


class Identity:
    __slots__ = ('key', 'body_hash', 'lane', 'session', 'idem')

    def __init__(self, key, body_hash, lane, session=None, idem=None):
        self.key, self.body_hash, self.lane = key, body_hash, lane
        self.session, self.idem = session, idem


def _header(handler, name):
    headers = handler.headers
    value = headers.get(name) if headers is not None else None
    return value if isinstance(value, str) else None


def explicit_session(handler, payload=None):
    """(session, idempotency key) from what the client sent explicitly: a generic session header
    (request_ledger.SESSION_HEADERS), else the body's `prompt_cache_key` (the same hint, v0.2.0 S7),
    or, without either, its Idempotency-Key. (None, None) when the client sent none of them: the
    ledger's first-user fallback can be equal for two new sessions."""
    idem = _header(handler, IDEMPOTENCY_HEADER)
    idem = idem if idem is not None and _IDEM.fullmatch(idem) else None
    for name in request_ledger.SESSION_HEADERS:
        value = _header(handler, name)
        if value is not None and 1 <= len(value) <= 200:
            return 'h:' + value, idem
    value = payload.get(request_ledger.PROMPT_CACHE_KEY) if isinstance(payload, dict) else None
    if isinstance(value, str) and 1 <= len(value) <= 200:
        return 'h:' + value, idem
    return ('i:' + idem, idem) if idem is not None else (None, None)


def identity(handler, st):
    """The replay identity of an eligible request, else None."""
    if not enabled() or not st.inference or st.remote or handler.command != 'POST':
        return None
    if st.path not in REPLAY_PATHS or not st.raw_body:
        return None
    payload = st.payload
    if not isinstance(payload, dict) or payload.get('stream') is not True:
        return None
    temperature = payload.get('temperature')
    if type(temperature) not in (int, float) or temperature != 0:
        return None
    session, idem = explicit_session(handler, payload)
    if session is None:
        return None  # the ledger's first-user fallback is not a unique session: never replay on it
    row = getattr(st.led, 'row', None) or {}
    target = st.target or {}
    body_hash = hashlib.sha256(st.raw_body).hexdigest()
    principal = 'local'
    basis = ['idem', idem] if idem is not None else ['body', body_hash]
    key = _mac('oxe-replay-v1', handler.command, st.path, principal, session, target.get('model_alias'),
               getattr(handler.manager, 'launches', None), row.get('mode'), *basis)
    return Identity(key, body_hash, _mac('lane', principal, session), session, idem)


class Entry:
    def __init__(self, ident):
        self.key, self.body_hash, self.lane = ident.key, ident.body_hash, ident.lane
        self.session, self.idem = ident.session, ident.idem
        self.cond = threading.Condition(threading.Lock())
        self.status = None
        self.headers = None
        self.chunks = []
        self.size = 0
        self.state = 'live'  # live | cut | done | aborted
        self.cut_at = None
        self.expires = None
        self.attached = 0
        self.served = 0  # attaches in all (the ledger label)
        self.upstream = None
        self.timer = None

    def attachable(self, now):
        return self.state in ('cut', 'done') and (self.expires is None or now < self.expires)


class Store:
    def __init__(self):
        self.lock = threading.Lock()
        self.entries = {}

    def purge(self, now):
        for key, entry in list(self.entries.items()):
            if entry.state == 'aborted' or (entry.expires is not None and now >= entry.expires
                                             and entry.state != 'live'):
                if entry.state == 'cut':
                    _abort(entry)
                self.entries.pop(key, None)

    def drop(self, entry):
        with self.lock:
            if self.entries.get(entry.key) is entry:
                del self.entries[entry.key]


STORE = Store()


def _arm_grace(entry):
    """(entry.cond held) A cut generation nobody reads is cancelled GRACE_S from now."""
    if entry.timer is not None:
        entry.timer.cancel()
    entry.expires = time.monotonic() + GRACE_S
    entry.timer = threading.Timer(GRACE_S, _grace_over, (entry,))
    entry.timer.daemon = True
    entry.timer.start()


def _grace_over(entry):
    if _abort_if_unattached(entry):
        STORE.drop(entry)


def _abort_if_unattached(entry):
    """Cancel a cut entry only if nobody is attached, as ONE transition under entry.cond (the lock
    try_attach takes to attach): an attach either wins first (then this returns False and the replay runs on)
    or finds the entry aborted and runs as a fresh request. The engine socket is closed outside the lock."""
    with entry.cond:
        if entry.state != 'cut' or entry.attached != 0:
            return False
        entry.state = 'aborted'
        if entry.timer is not None:
            entry.timer.cancel()
        entry.cond.notify_all()
    if entry.upstream is not None:
        try:
            entry.upstream.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
    return True


def _abort(entry):
    """Cancel the engine as v0.1.3 does for a vanished client, and wake the attached readers."""
    with entry.cond:
        if entry.state in ('done', 'aborted'):
            return
        entry.state = 'aborted'
        if entry.timer is not None:
            entry.timer.cancel()
        entry.cond.notify_all()
    if entry.upstream is not None:
        try:
            entry.upstream.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass


def cancel_explicit(session, idem=None):
    """POST /_local/cancel: the client deliberately cancelled. Every live or cut entry of this session (and
    idempotency key, when given) ends now: the engine socket is closed and the grace is over. Returns the
    number cancelled; an unknown request is 0 (idempotent)."""
    wanted = {'h:' + session}
    if idem:
        wanted.add('i:' + idem)
    with STORE.lock:
        found = [e for e in STORE.entries.values() if e.session in wanted and (idem is None or e.idem == idem)
                 and e.state in ('live', 'cut')]
    for entry in found:
        _abort(entry)
        STORE.drop(entry)
    return len(found)


def yield_cut_entries():
    """Another request waits for the slot: every cut generation nobody is attached to is cancelled now
    instead of running out its GRACE_S (an identical retry attaches before admission, so it never
    waits in the queue; a cut that then blocks a different request only costs that request time)."""
    with STORE.lock:  # candidates only; the decision is made per entry, atomically with attach
        candidates = [e for e in STORE.entries.values() if e.state == 'cut']
    for entry in candidates:
        if _abort_if_unattached(entry):
            STORE.drop(entry)  # removed only when the cancel actually won


ctl_queue.SLOT_WANTED_HOOKS.append(yield_cut_entries)


# ------------------------------------------------------------------ producer (the cut request)
class Guard:
    """Stands in for the engine socket in the disconnect watcher: a disconnect arms the replay
    instead of cancelling the engine; a request that is not armed is cancelled as before."""

    def __init__(self, recorder, sock):
        self.recorder, self.sock = recorder, sock

    def shutdown(self, how):
        if not self.recorder.cut():
            self.sock.shutdown(how)


class Recorder:
    def __init__(self, entry):
        self.entry = entry
        self.armed = True

    def cut(self):
        """The client is gone: True when the replay keeps the engine running for GRACE_S."""
        entry = self.entry
        with entry.cond:
            if not self.armed or entry.state != 'live':
                return entry.state == 'cut'
            entry.state = 'cut'
            entry.cut_at = time.monotonic()
            entry.expires = entry.cut_at + GRACE_S
            _arm_grace(entry)
            entry.cond.notify_all()
        return True

    def head(self, status, items):
        entry = self.entry
        with entry.cond:
            entry.status, entry.headers = status, list(items)
            entry.cond.notify_all()
        if status != 200:
            self.disarm()

    def disarm(self):
        """Not replayable after all (error status, too big): a cut entry is cancelled now."""
        if not self.armed:
            return
        self.armed = False
        entry = self.entry
        with entry.cond:
            was_cut = entry.state == 'cut'
        if was_cut:
            _abort(entry)
        STORE.drop(entry)

    def relay(self, handler, st, chunk, write):
        """Record one engine body chunk; write it to the client unless it is gone. True when the
        chunk was handled here (the caller does not write it)."""
        entry = self.entry
        if self.armed:
            with entry.cond:
                if entry.size + len(chunk) > MAX_BYTES:
                    over = True
                else:
                    over = False
                    entry.chunks.append(chunk)
                    entry.size += len(chunk)
                    entry.cond.notify_all()
            if over:
                self.disarm()
        if st.client_gone() or entry.state in ('cut', 'aborted'):
            if not self.armed or entry.state == 'aborted':
                raise ConnectionAbortedError('client gone')  # v0.1.3: a vanished client cancels
            return True
        try:
            write(handler, st, ('%x\r\n' % len(chunk)).encode() + chunk + b'\r\n')
        except OSError:
            st.client_write_failed = True
            if not self.armed or not self.cut():
                raise
        return True

    def done(self, st):
        """The engine finished. True when the client is gone (the final chunk is not written)."""
        entry = self.entry
        gone = st.client_gone() or entry.state == 'cut'
        with entry.cond:
            if entry.state == 'cut':
                entry.state = 'done'
                if entry.timer is not None:
                    entry.timer.cancel()
                if entry.attached:
                    entry.expires = time.monotonic() + GRACE_S
                entry.cond.notify_all()
        if not gone or not self.armed:
            STORE.drop(entry)  # delivered (or never armed): never replayed
        return gone

    def release(self, st):
        """finish(): an entry that did not end as 'done' is aborted; ledger label."""
        entry = self.entry
        with entry.cond:
            state, attached = entry.state, entry.served
        if state in ('live', 'cut'):
            if state == 'cut' and st.led is not None:
                _label(st, 'cut-aborted')
            _abort(entry)
            STORE.drop(entry)
        elif state == 'done' and self.armed and st.led is not None:
            _label(st, 'cut-served' if attached else 'cut-done')


def _label(st, value):
    try:
        st.led.row['replay'] = value
    except Exception:
        pass


def start(st, sock):
    """forward(), engine connected: register the eligible request as live; the watcher gets the
    Guard instead of the engine socket. Returns the socket for the watcher."""
    ident = st.robust.replay_id if st.robust is not None else None
    if ident is None or sock is None:
        return sock
    entry = Entry(ident)
    entry.upstream = sock
    with STORE.lock:
        STORE.purge(time.monotonic())
        if len(STORE.entries) >= MAX_ENTRIES or ident.key in STORE.entries:
            return sock
        STORE.entries[ident.key] = entry
    st.robust.recorder = Recorder(entry)
    return Guard(st.robust.recorder, sock)


def recorder(st):
    return st.robust.recorder if st.robust is not None else None


# ------------------------------------------------------------------ consumer (the identical retry)
def try_attach(handler, st):
    """Before admission: serve an identical retry from a cut entry (ANSWERED), else None.
    Also cancels a cut entry of the same lane whose request is not identical."""
    if st.robust is None:
        return None
    ident = identity(handler, st)
    st.robust.replay_id = ident
    if ident is None:
        _cancel_lane(handler, st)  # an ineligible request of the same session also moves it on
        return None
    now = time.monotonic()
    stale = []
    with STORE.lock:
        STORE.purge(now)
        entry = STORE.entries.get(ident.key)
        if entry is not None and (entry.lane != ident.lane or
                                  not hmac.compare_digest(entry.body_hash, ident.body_hash)):
            entry = None
        for other in list(STORE.entries.values()):
            if other is not entry and other.lane == ident.lane and other.state != 'live':
                stale.append(other)
                STORE.entries.pop(other.key, None)
    for other in stale:
        _abort(other)  # the same session moved on: its cut generation gives the slot back now
    if entry is None:
        return None
    with entry.cond:
        if entry.state == 'live':
            entry.cond.wait_for(lambda: entry.state != 'live', LIVE_WAIT_S)
        if not entry.attachable(time.monotonic()) or entry.attached >= MAX_ATTACH:
            return None
        entry.attached += 1
        entry.served += 1
        entry.expires = None
        if entry.timer is not None and entry.state == 'cut':
            entry.timer.cancel()
    _serve(handler, st, entry)
    return ANSWERED


def _cancel_lane(handler, st):
    """Cancel the cut entries of this request's (local) session at once."""
    if not enabled() or not STORE.entries or not st.inference or st.remote:
        return
    session, _ = explicit_session(handler, st.payload)
    if session is None:
        return
    lane = _mac('lane', 'local', session)
    with STORE.lock:
        stale = [e for e in STORE.entries.values() if e.lane == lane and e.state != 'live']
        for entry in stale:
            STORE.entries.pop(entry.key, None)
    for entry in stale:
        _abort(entry)


def _serve(handler, st, entry):
    led = st.led
    status, error = 200, None
    try:
        with entry.cond:
            while entry.status is None and entry.state not in ('aborted',):
                entry.cond.wait(ctl_queue.POLL_S)
                if ctl_queue.client_gone(handler.connection):
                    raise ConnectionAbortedError('client gone')
            if entry.status is None:
                raise ConnectionAbortedError('engine gone')
            status, items = entry.status, list(entry.headers)
        handler.send_response(status)
        for name, value in items:
            handler.send_header(name, value)
        for name, value in handler.cors_reply_headers():
            handler.send_header(name, value)
        handler.send_header('Cache-Control', 'no-store')
        handler.send_header('Transfer-Encoding', 'chunked')
        handler.end_headers()
        sent = 0
        while True:
            with entry.cond:
                while len(entry.chunks) == sent and entry.state not in ('done', 'aborted'):
                    entry.cond.wait(ctl_queue.POLL_S)
                    if ctl_queue.client_gone(handler.connection):
                        raise ConnectionAbortedError('client gone')
                pending, state = entry.chunks[sent:], entry.state
            for chunk in pending:
                if led is not None:
                    led.observe(chunk)
                ctl_queue.write_chunk(handler, chunk)
            sent += len(pending)
            if pending:
                continue
            if state == 'done':
                handler.wfile.write(b'0\r\n\r\n')
                handler.wfile.flush()
                break
            error = 'replay_aborted'  # the engine stopped before its end: closed, incomplete
            handler.close_connection = True
            break
    except OSError:
        error = 'client_disconnected'
        handler.close_connection = True
    finally:
        with entry.cond:
            entry.attached -= 1
            if entry.state == 'done' and entry.expires is None:
                entry.expires = time.monotonic() + GRACE_S
            elif entry.state == 'cut' and entry.attached == 0:
                _arm_grace(entry)  # the last reader left: a new grace, never an unbounded generation
        if led is not None:
            _label(st, 'attached')
            try:
                led.finish(status, error)
            except Exception:
                pass
