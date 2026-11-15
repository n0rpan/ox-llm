"""v0.1.4 CORE-ROBUST (a): the admission queue with early SSE headers (V014-REPLAN section 3a).

On by default since v0.2.0 (A5; gates G-Q / G-Q-EG1 verified in the release batch). OXC_QUEUE=0
turns it off: admission is then v0.1.3's FIFO loop in ctl_lease.begin_queued, unchanged. One behaviour for every client; no client detection.

On (a local inference request; a shared LAN request keeps v0.1.3's rule, see below):
- A free slot admits at once: no byte differs from v0.1.3 (the wait callback is never called).
- Busy slot, streaming request (`"stream": true`): the client gets `200` and SSE headers at once,
  then the comment `: queued pos=<n> wait_s=<s>` at once and every KEEPALIVE_S seconds while it
  waits, then the engine's body unchanged (its own headers are not sent again). SSE comments are not
  data. v0.2.0 F10: one format with the engine's own queued line (`: queued [pos=<n>] wait_s=<s>`,
  key=value fields, unknown keys ignored): pos = place in this queue (1 = next), wait_s = whole
  seconds since the request first waited, rounded down. The engine sends no pos (its worker is a
  lock, not an ordered queue). Contract: engine/docs/lab-notes/SERVER.md, "Long prefills".
- Busy slot, non-streaming request: waits without early headers, bounded by QUEUE_WAIT_SECONDS
  (503 after it, as in v0.1.3): a JSON reply has no comment channel.
- No queue timeout for a streaming waiter while its client stays connected. A waiter whose
  client went away (socket EOF, seen within ~0.5 s) leaves the queue.
- Never 429. Above the cap (PER_PRINCIPAL waiters of one principal or TOTAL waiters in all):
  503 with `Retry-After: 30`, before anything else is sent.
- A failure after early headers (admission error, engine refused, engine error status, engine
  gone before its body) is one SSE `error` event, then the end of the stream, no `[DONE]`; the
  connection is closed. A failure after the engine's body started is v0.1.3's (closed, incomplete).
- Order: round-robin across lanes (principal + session key), FIFO inside one lane: the next turn
  goes to the waiting lane that was served least recently (never served = first; ties: the
  earliest arrival). `X-Oxe-Priority: background` (a client-neutral hint, never forwarded) puts a
  request behind every interactive waiter.
- Pre-emption (v0.2.0 EG1): a running STREAMING background request that has sent its client no
  data byte yet (SSE comment lines are not data) is cancelled at the engine when an interactive
  waiter arrives (the engine connection is shut down, as for a client that went away). The
  controller keeps the client's stream open, re-queues the request at the FRONT of the background
  class (its lane keeps its round-robin turn; the cap does not apply again) and re-issues it with
  the same body and headers (Idempotency-Key included) when its turn comes. The client sees only
  `: queued pos=<n> wait_s=<s>` comments, then the answer of the re-issued request. From the first data byte on
  the request is never cut (it is bounded by its own `max_tokens`). A non-streaming background
  request is not pre-empted (no comment channel; its wait is bounded). The check and the first
  data write are serialised (`BackgroundRun`), so a request is either cut before any data byte or
  runs to its end, never both. The ledger row counts the cuts (`preempted`).
- LAN share (O-102 M4) is unchanged: a shared request never waits among local ones, a local
  arrival cancels a running shared request, and a shared request waits at most SLOT_WAIT.
"""
import json
import os
import select
import socket
import threading
import time
if __package__:
    from .ctl_env import Unavailable
    from .ctl_seams import ctl
    from . import lan_share, openai_api
else:
    from ctl_env import Unavailable
    from ctl_seams import ctl
    import lan_share
    import openai_api

ENV = 'OXC_QUEUE'
PER_PRINCIPAL = 8
TOTAL = 16
KEEPALIVE_S = 15.0
RETRY_AFTER_S = 30
POLL_S = 0.5
PRIORITY_HEADER = 'X-Oxe-Priority'
SERVED_LANES = 256  # least-recently-served bookkeeping, bounded


OFF_VALUES = ('0', 'off', 'false', 'no')


# Called (queue lock released) whenever a local request is waiting for the slot, about every POLL_S:
# ctl_replay registers yield_cut_entries here, so a cut generation kept for a possible retry never makes
# a waiting request wait for it (the cut client's cancel reaches the engine within POLL_S).
SLOT_WANTED_HOOKS = []


def _slot_wanted():
    for hook in list(SLOT_WANTED_HOOKS):
        try:
            hook()
        except Exception:
            pass


def enabled():
    """On by default since v0.2.0 (A5); OXC_QUEUE=0 (or off/false/no) turns it off."""
    return os.environ.get(ENV, '1').strip().lower() not in OFF_VALUES


REQUEUE = object()  # forward(): the background request was pre-empted; admit it again (EG1)


class QueueFull(Unavailable):
    """Above the waiter cap: 503 + Retry-After: 30."""


class ClientGone(Unavailable):
    """The waiting request's client went away; it left the queue (nothing is sent)."""


class Extras:
    """Per-request state of the robustness features (ProxyRequest.robust)."""
    __slots__ = ('waiter', 'replay_id', 'recorder', 'diag', 'diag_messages')

    def __init__(self):
        self.waiter = self.replay_id = self.recorder = self.diag = self.diag_messages = None


def client_gone(sock):
    """True when the client closed its socket (EOF), without consuming a pipelined byte.
    A TLS socket refuses MSG_PEEK; it is never judged gone here."""
    if sock is None or lan_share.is_tls(sock):
        return False
    try:
        readable, _, _ = select.select([sock], [], [], 0)
        if not readable:
            return False
        return not sock.recv(1, socket.MSG_PEEK | socket.MSG_DONTWAIT)
    except BlockingIOError:
        return False
    except (OSError, ValueError):
        return True


def queued_comment(position, waited_s):
    """F10: `: queued pos=<n> wait_s=<s>` (the engine's queued line has the same format, without pos)."""
    return b': queued pos=%d wait_s=%d\n\n' % (position, max(0, int(waited_s)))


class Waiter:
    """One local inference request's place in the queue (the queue's ticket)."""

    def __init__(self, handler, *, stream, session, principal='local', background=False):
        self.handler = handler
        self.stream = stream
        self.principal = principal
        self.lane = (principal, session if isinstance(session, str) else '')
        self.background = background
        self.seq = None
        self.early = False          # SSE headers were sent while waiting
        self.engine_started = False  # the engine's body follows the early headers
        self.failed = False          # the SSE error event was sent
        self.position = None         # first queue position (ledger)
        self.waited = False
        self.session_switch = None
        self._last_comment = None
        self.wait_t0 = None          # F10: first wait (wait_s of the `: queued` comment)
        self.run = None              # EG1: BackgroundRun while admitted (pre-emptible requests only)
        self.requeued = False        # EG1: waiting again after a pre-emption (front of its class)
        self.preemptions = 0
        self.prev_served = None      # EG1: its lane's round-robin mark before this admission

    @property
    def preemptible(self):
        return self.background and self.stream

    @property
    def deadline_s(self):
        return None if self.stream else ctl.QUEUE_WAIT_SECONDS

    def waiting(self, position):
        """Called by the queue (lock released) about every POLL_S while this request waits.
        False when the client is gone (the request leaves the queue)."""
        self.waited = True
        if self.position is None:
            self.position = position
        if self.wait_t0 is None:
            self.wait_t0 = time.monotonic()
        if client_gone(getattr(self.handler, 'connection', None)):
            return False
        if not self.stream:
            return True
        now = time.monotonic()
        try:
            if not self.early:
                send_early_headers(self.handler)
                self.early = True
            elif self._last_comment is not None and now - self._last_comment < KEEPALIVE_S:
                return True
            write_chunk(self.handler, queued_comment(position, now - self.wait_t0))
            self._last_comment = now
        except OSError:
            return False
        return True

    def active(self):
        """Early headers are out and the engine's body has not started: an error is an event."""
        return self.early and not self.engine_started

    def requeue(self, *, mid_comment=False):
        """EG1: the request was cut before its first data byte. When a response head is out (the
        engine's or the early one; `early`), the next engine head is dropped and an error is an SSE
        event; else the wait sends the early head as for any queued stream. The first `: queued`
        comment goes out at once."""
        self.engine_started = False
        self.requeued = True
        self.preemptions += 1
        self.run = None
        self._last_comment = None
        if mid_comment:  # end the engine's half-sent comment line before our own comment
            try:
                write_chunk(self.handler, b'\n')
            except OSError:
                pass


class BackgroundRun:
    """EG1: one admission of a pre-emptible background request. `cancel` (an interactive arrival)
    and `commit` (the first data byte, before it is written) are serialised: whichever comes first
    wins. A cancel shuts the engine connection down (now, or when it is attached)."""

    def __init__(self):
        self._lock = threading.Lock()
        self._sock = None
        self.cancelled = False
        self.committed = False
        self.at_line_start = True   # SSE scanner state (comment lines are not data)
        self.in_comment = False
        self.after_cr = False

    def attach(self, sock):
        with self._lock:
            self._sock = sock
            cancelled = self.cancelled
        if cancelled:
            _shutdown(sock)

    def cancel(self):
        """True when the request was cut (it had sent no data byte)."""
        with self._lock:
            if self.committed or self.cancelled:
                return False
            self.cancelled = True
            sock = self._sock
        if sock is not None:
            _shutdown(sock)
        return True

    def commit(self):
        """The first data byte is about to be written: False when the request was already cut."""
        with self._lock:
            if self.cancelled:
                return False
            self.committed = True
            return True

    def has_data(self, chunk):
        """True when this engine body chunk holds a byte of a non-comment SSE line. Lines starting
        with ':' and blank lines are not data. A line ends at CRLF, LF or a lone CR (the SSE
        grammar), also when the pair is split across chunks. State carries across chunks."""
        for byte in chunk:
            if self.after_cr:
                self.after_cr = False
                if byte == 10:  # the LF of a CRLF
                    continue
            if byte == 13 or byte == 10:
                self.in_comment = False
                self.at_line_start = True
                self.after_cr = byte == 13
                continue
            if self.in_comment:
                continue
            if self.at_line_start and byte == 58:  # ':'
                self.in_comment = True
                self.at_line_start = False
                continue
            return True
        return False

    def gate(self, chunk):
        """forward(): True when the chunk may be written (no data, or data and committed now);
        False when the request was cut (drop it and re-queue)."""
        if self.committed:
            return True
        if not self.has_data(chunk):
            return not self.cancelled
        return self.commit()


def _shutdown(sock):
    try:
        sock.shutdown(socket.SHUT_RDWR)
    except OSError:
        pass


def run_of(st):
    """The BackgroundRun of a proxied request, else None."""
    waiter = st.robust.waiter if getattr(st, 'robust', None) is not None else None
    return waiter.run if waiter is not None else None


def released(manager):
    """Manager.end(inference=True): the running request ended; nothing is pre-emptible now."""
    manager.queue_background_run = None


def waiter_for(handler, st):
    """The Waiter of a local inference request when the queue is on, else None."""
    if not enabled() or st.remote or not st.inference:
        return None
    stream = handler.command == 'POST' and isinstance(st.payload, dict) and st.payload.get('stream') is True
    session = getattr(st.led, 'session', None) if st.led is not None else None
    hint = handler.headers.get(PRIORITY_HEADER) if handler.headers is not None else None
    background = isinstance(hint, str) and hint.strip().lower() == 'background'
    return Waiter(handler, stream=stream, session=session, background=background)


def write_chunk(handler, data):
    handler.wfile.write(('%x\r\n' % len(data)).encode() + data + b'\r\n')
    handler.wfile.flush()


def send_early_headers(handler):
    """The SSE response head, in the order forward() sends an engine's: Content-Type, CORS,
    Cache-Control, chunked."""
    handler.send_response(200)
    handler.send_header('Content-Type', 'text/event-stream')
    for name, value in handler.cors_reply_headers():
        handler.send_header(name, value)
    handler.send_header('Cache-Control', 'no-store')
    handler.send_header('Transfer-Encoding', 'chunked')
    handler.end_headers()


def error_event(code, body):
    """`event: error` + the OpenAI error object of a reply that could not be sent as a status."""
    try:
        value = json.loads(body)
    except (ValueError, TypeError):
        value = None
    if not (isinstance(value, dict) and 'error' in value):
        value = {'error': {'message': 'Local backend error.', 'type': 'local_backend_error'}}
    # v0.2.0 S3: the OpenAI shape with a string code (an engine's integer code is mapped)
    error = dict(openai_api.normalize_error(code if code >= 400 else 500, value)['error'])
    return b'event: error\ndata: ' + json.dumps({'error': error}).encode() + b'\n\n'


def fail(handler, waiter, code, body):
    """Instead of a status reply after early headers: one SSE error event, the end of the
    chunked body (no [DONE]), and the connection closes. A second failure sends nothing."""
    handler.close_connection = True
    if waiter.failed:
        return
    waiter.failed = True
    try:
        write_chunk(handler, error_event(code, body))
        handler.wfile.write(b'0\r\n\r\n')
        handler.wfile.flush()
    except OSError:
        pass


def reply_full(handler):
    """503 + Retry-After: 30 (the cap); the OpenAI error shape, never 429."""
    body = json.dumps(openai_api.normalize_error(503, {'error': {
        'message': 'Too many requests are waiting for the local model; retry in %d s.' % RETRY_AFTER_S,
        'type': 'local_backend_busy', 'code': 'queue_full'}})).encode()
    handler.send_response(503)
    handler.send_header('Content-Type', 'application/json')
    handler.send_header('Content-Length', str(len(body)))
    handler.send_header('Cache-Control', 'no-store')
    handler.send_header('Retry-After', str(RETRY_AFTER_S))
    for name, value in handler.cors_reply_headers():
        handler.send_header(name, value)
    handler.end_headers()
    try:
        handler.wfile.write(body)
    except OSError:
        handler.close_connection = True


# ------------------------------------------------------------------ the queue (Manager side)
def _lane(ticket):
    lane = getattr(ticket, 'lane', None)
    return lane if lane is not None else ('?', id(ticket))  # a v0.1.3 ticket (a shared request)


def _served(manager):
    served = getattr(manager, 'queue_lane_served', None)
    if served is None:
        served = manager.queue_lane_served = {}
        manager.queue_serve_count = 0
    return served


def _pick(tickets, served):
    """The next ticket: interactive before background; the least recently served lane; FIFO
    inside a lane (the deque is in arrival order)."""
    local = [t for t in tickets if isinstance(t, Waiter)] or list(tickets)  # a shared ticket never goes first
    pool = [t for t in local if not t.background] if local and isinstance(local[0], Waiter) else local
    pool = pool or local
    if pool and isinstance(pool[0], Waiter) and pool[0].background:
        front = [t for t in pool if t.requeued]  # EG1: a pre-empted request goes first in its class
        if front:
            return front[0]
    best, best_key, heads = None, None, set()
    for index, ticket in enumerate(pool):
        lane = _lane(ticket)
        if lane in heads:
            continue
        heads.add(lane)
        key = (served.get(lane, -1), index)
        if best_key is None or key < best_key:
            best, best_key = ticket, key
    return best


def order(manager):
    """The admission order of the current waiters (positions for `: queued pos=<n>`)."""
    served = dict(_served(manager))
    count = getattr(manager, 'queue_serve_count', 0)
    left, out = list(manager.queue_waiters), []
    while left:
        ticket = _pick(left, served)
        out.append(ticket)
        left.remove(ticket)
        count += 1
        served[_lane(ticket)] = count
    return out


def over_cap(manager, waiter):
    waiters = manager.queue_waiters
    mine = sum(1 for t in waiters if getattr(t, 'principal', None) == waiter.principal)
    return len(waiters) >= TOTAL or mine >= PER_PRINCIPAL


def wait_turn(manager, waiter, *, preempt=None):
    """Wait (queue_cv held on return: no; returns after queue_reserved is set) for this local
    request's turn. Raises QueueFull, ClientGone or Unavailable (a non-stream wait's bound)."""
    cv = manager.queue_cv
    with cv:
        if not waiter.requeued and over_cap(manager, waiter):
            raise QueueFull('Too many requests are waiting for the local model.')
        manager.queue_waiters.append(waiter)
        running = getattr(manager, 'remote_preempt', None)
        if running is not None:
            running.cancel()  # O-102 M4: local use first, unchanged
        background = getattr(manager, 'queue_background_run', None)
        if background is not None and not waiter.background:
            background.cancel()  # EG1: no-op once the background request sent a data byte
        cv.notify_all()
        limit = waiter.deadline_s
        deadline = None if limit is None else time.monotonic() + limit
        try:
            while not (_pick(manager.queue_waiters, _served(manager)) is waiter
                       and not manager.queue_reserved and manager.inferences == 0):
                if deadline is not None and time.monotonic() >= deadline:
                    raise Unavailable('A language-model request is already running and the wait '
                                      f'for a turn exceeded {limit:.0f} s; retry when it finishes.')
                position = order(manager).index(waiter) + 1
                cv.release()
                try:
                    _slot_wanted()
                    alive = waiter.waiting(position)
                finally:
                    cv.acquire()
                if not alive:
                    raise ClientGone('The client left the queue.')
                left = POLL_S if deadline is None else max(0.0, min(POLL_S, deadline - time.monotonic()))
                cv.wait(left)
            manager.queue_reserved = True
            served = _served(manager)
            manager.queue_serve_count = getattr(manager, 'queue_serve_count', 0) + 1
            if not waiter.requeued:
                waiter.prev_served = served.get(waiter.lane)
            served.pop(waiter.lane, None)
            served[waiter.lane] = manager.queue_serve_count
            while len(served) > SERVED_LANES:
                served.pop(next(iter(served)))
            last = getattr(manager, 'queue_last_lane', None)
            waiter.session_switch = last is not None and last != waiter.lane
            manager.queue_last_lane = waiter.lane
            if preempt is not None:
                manager.remote_preempt = preempt
            if waiter.preemptible:
                waiter.run = manager.queue_background_run = BackgroundRun()
            waiter.requeued = False
        finally:
            manager.queue_waiters.remove(waiter)
            cv.notify_all()


def unserve(manager, waiter):
    """EG1: a pre-empted request gives its lane's round-robin mark back (its turn was not used)."""
    with manager.queue_cv:
        served = _served(manager)
        if served.get(waiter.lane) is not None:
            served.pop(waiter.lane, None)
            if waiter.prev_served is not None:
                served[waiter.lane] = waiter.prev_served
