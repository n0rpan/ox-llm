"""v0.1.4 CORE-ROBUST (c): structural turn diagnostics, labels only (V014-REPLAN sections 2 and 3c).

Behind LOCAL_AI_TURN_DIAG=1 (default off). Nothing here changes a request, a reply or the history;
the controller never normalises history to gain a cache hit (the engine's ECHO stays the only
exact repair). Every value written is an enum or a boolean; what is held in memory between turns
is SHA-256 hashes, a polynomial rolling hash and lengths, never text.

Ledger fields (request_ledger rows):
- `history_rewrite` (bool): the request's history is not an append of the session's previous one
  (ledger `history_change` edited | truncated).
- `rewrite_class`: how the client sent back the session's previous relayed reply R (the message at
  R's position), from hashes of R's normal forms:
    identical | ws-trailing | ws-inner | args-reserialised | reasoning-dropped | id-changed | other
  identical = same content, tool names and arguments, same call ids, reasoning kept (or none);
  id-changed / reasoning-dropped = content and calls identical but the call ids / the reasoning
  differ; ws-trailing = equal after stripping trailing whitespace per line; ws-inner = equal after
  collapsing every whitespace run; args-reserialised = content equal and arguments equal as
  canonical JSON; other = none of these. Absent when no reply of the session is known or the
  message at R's position is not an assistant message.
- `fold_suspect` (True only; written by request_ledger.label_turn through cache_turn's hook
  `turn['fold_suspect']`, or on the row for a request without turn labels) and `fold_basis`
  (session | recent): at a turn start, i.e. the session's last relayed step ended `stop` with no
  tool calls and reply text T (stripped, >= FOLD_MIN bytes), a request arrives whose history is not
  an append (history_change initial | edited | truncated | same), ends in a user message with nothing after it,
  and that message's text contains T (rolling-hash window over the UTF-8 bytes, SHA-256 confirm).
  For a history the ledger has never seen (`initial`, e.g. a session key that changed with the
  rewrite) T is the principal's most recent such reply for the same model (`recent`).
"""
import gzip
import hashlib
import io
import json
import os
import re
import threading
from collections import OrderedDict

ENV = 'LOCAL_AI_TURN_DIAG'
FOLD_MIN = 32               # bytes; shorter replies are too common to name a fold
HAY_MAX = 2 * 1024 * 1024   # bytes of the last user message scanned for T
REPLY_MAX = 4 * 1024 * 1024
SESSIONS = 256
REWRITE_CLASSES = ('identical', 'ws-trailing', 'ws-inner', 'args-reserialised', 'reasoning-dropped',
                   'id-changed', 'other')
_MOD = (1 << 61) - 1
_BASE = 257
_WS = re.compile(r'\s+')
_lock = threading.Lock()
_sessions = OrderedDict()   # (principal, session, model) -> state of the last relayed turn
_recent = OrderedDict()     # (principal, model) -> fold record of the last stop reply


def enabled():
    return os.environ.get(ENV) == '1'


def _h(*parts):
    return hashlib.sha256(json.dumps(parts, ensure_ascii=False, separators=(',', ':')).encode('utf-8', 'replace')).hexdigest()


def rolling(data):
    h = 0
    for byte in data:
        h = (h * _BASE + byte) % _MOD
    return h


def fold_record(text):
    data = text.strip().encode('utf-8', 'replace')
    if len(data) < FOLD_MIN:
        return None
    return (len(data), rolling(data), hashlib.sha256(data).hexdigest())


def contains(haystack, record):
    """Does `haystack` (bytes) contain the text of `record`? Rolling hash, SHA-256 confirm."""
    length, target, digest = record
    n = len(haystack)
    if n < length:
        return False
    h = rolling(haystack[:length])
    top = pow(_BASE, length - 1, _MOD)
    for i in range(n - length + 1):
        if h == target and hashlib.sha256(haystack[i:i + length]).hexdigest() == digest:
            return True
        if i + length < n:
            h = ((h - haystack[i] * top) * _BASE + haystack[i + length]) % _MOD
    return False


def _text(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return ''.join(p.get('text', '') for p in content
                       if isinstance(p, dict) and isinstance(p.get('text'), str))
    return ''


def _canon(args):
    try:
        return json.dumps(json.loads(args), sort_keys=True, separators=(',', ':'), ensure_ascii=False)
    except (ValueError, TypeError):
        return args


def forms(content, calls, reasoning):
    """Hashes of the normal forms of one assistant turn. calls: [(id, name, arguments-string)]."""
    names = [c[1] or '' for c in calls]
    args = [c[2] or '' for c in calls]
    trailing = '\n'.join(line.rstrip() for line in content.split('\n')).rstrip()
    return {'exact': _h(content, names, args),
            'ws_trailing': _h(trailing, names, args),
            'ws_inner': _h(_WS.sub(' ', content).strip(), names, args),
            'args': _h(content, names, [_canon(a) for a in args]),
            'ids': _h([c[0] or '' for c in calls]) if any(c[0] for c in calls) else None,
            'reasoning': _h(reasoning) if reasoning else None}


def message_forms(message):
    calls = []
    for call in message.get('tool_calls') or []:
        if not isinstance(call, dict):
            continue
        fn = call.get('function') if isinstance(call.get('function'), dict) else {}
        args = fn.get('arguments')
        args = args if isinstance(args, str) else json.dumps(args) if args is not None else ''
        calls.append((call.get('id') if isinstance(call.get('id'), str) else None, fn.get('name'), args))
    reasoning = message.get('reasoning_content') or message.get('reasoning')
    return forms(_text(message.get('content')), calls, reasoning if isinstance(reasoning, str) else None)


def classify(reply, message):
    m = message_forms(message)
    if m['exact'] == reply['exact']:
        if reply['ids'] is not None and m['ids'] != reply['ids']:
            return 'id-changed'
        if reply['reasoning'] is not None and m['reasoning'] != reply['reasoning']:
            return 'reasoning-dropped'
        return 'identical'
    for form, name in (('ws_trailing', 'ws-trailing'), ('ws_inner', 'ws-inner'), ('args', 'args-reserialised')):
        if m[form] == reply[form]:
            return name
    return 'other'


class ReplyForms:
    """The relayed reply (SSE deltas or a JSON body), parsed for its forms; held until finish."""

    def __init__(self):
        self.buf, self.body, self.mode, self.broken, self.size = b'', b'', None, False, 0
        self.content, self.reasoning, self.calls, self.finish_reason = [], [], {}, None

    def feed(self, chunk):
        if self.broken:
            return
        try:
            if self.mode is None and chunk.strip():
                self.mode = 'json' if chunk.lstrip()[:1] == b'{' else 'sse'
            if self.mode == 'json':
                self.body += chunk
                self.broken = len(self.body) > REPLY_MAX
                return
            *lines, self.buf = (self.buf + chunk).split(b'\n')
            for line in lines:
                if line.startswith(b'data:') and line[5:].strip() not in (b'', b'[DONE]'):
                    self._choice(json.loads(line[5:]), 'delta')
            self.broken = self.size > REPLY_MAX or len(self.buf) > REPLY_MAX
        except (ValueError, AttributeError, TypeError, IndexError, KeyError):
            self.broken = True

    def _choice(self, obj, key):
        choices = obj.get('choices') if isinstance(obj, dict) else None
        if not choices:
            return
        choice = choices[0]
        if isinstance(choice.get('finish_reason'), str):
            self.finish_reason = choice['finish_reason']
        part = choice.get(key) or {}
        for field, out in (('content', self.content), ('reasoning_content', self.reasoning), ('reasoning', self.reasoning)):
            if isinstance(part.get(field), str):
                out.append(part[field])
                self.size += len(part[field])
        for index, call in enumerate(part.get('tool_calls') or []):
            slot = self.calls.setdefault(call.get('index', index) if key == 'delta' else index, ['', '', ''])
            if isinstance(call.get('id'), str):
                slot[0] = slot[0] or call['id']
            fn = call.get('function') or {}
            slot[1] += fn.get('name') or ''
            args = fn.get('arguments')
            slot[2] += args if isinstance(args, str) else json.dumps(args) if args is not None else ''
            self.size += len(slot[2])

    def result(self):
        """(forms, stop_no_tools, fold record) or None."""
        if self.broken:
            return None
        if self.mode == 'json':
            try:
                self._choice(json.loads(self.body), 'message')
            except (ValueError, AttributeError, TypeError, IndexError, KeyError):
                return None
        if self.finish_reason is None:
            return None  # not a complete reply
        content = ''.join(self.content)
        calls = [tuple(self.calls[k]) for k in sorted(self.calls)]
        reasoning = ''.join(self.reasoning) or None
        stop = self.finish_reason == 'stop' and not calls
        return forms(content, calls, reasoning), stop, (fold_record(content) if stop else None)


def _messages(handler, st):
    raw = st.raw_body
    if not raw:
        return None
    try:
        if (handler.headers.get('Content-Encoding') or '').lower() == 'gzip':
            with gzip.GzipFile(fileobj=io.BytesIO(raw)) as stream:
                raw = stream.read(16 * 1024 ** 2 + 1)
        payload = json.loads(raw)
    except (ValueError, OSError, EOFError):
        return None
    messages = payload.get('messages') if isinstance(payload, dict) else None
    return messages if isinstance(messages, list) else None


def _keys(st):
    session = getattr(st.led, 'session', None)
    model = (st.target or {}).get('model_alias')
    principal = 'remote' if st.remote else 'local'
    return ((principal, session, model) if isinstance(session, str) else None), (principal, model)


def start(handler, st):
    """open_entry, after the ledger Entry and before label_turn: the request's labels."""
    if not enabled() or not st.inference or st.led is None or st.robust is None:
        return
    try:
        messages = _messages(handler, st)
        if messages is None:
            return
        row = st.led.row
        change = row.get('history_change')
        key, recent_key = _keys(st)
        st.robust.diag = ReplyForms()
        st.robust.diag_messages = len(messages)
        if change in ('edited', 'truncated'):
            row['history_rewrite'] = True
        elif change in ('append', 'same', 'initial'):
            row['history_rewrite'] = False
        with _lock:
            state = _sessions.get(key) if key is not None else None
            recent = _recent.get(recent_key)
        if state is not None and state['n'] < len(messages):
            message = messages[state['n']]
            if isinstance(message, dict) and message.get('role') == 'assistant':
                row['rewrite_class'] = classify(state['forms'], message)
        last = messages[-1] if messages else None
        if change == 'append' or not (isinstance(last, dict) and last.get('role') == 'user'):
            return
        record, basis = (state['fold'], 'session') if state is not None else (None, None)
        if record is None and change == 'initial' and recent is not None:
            record, basis = recent, 'recent'
        if record is None:
            return
        haystack = _text(last.get('content')).encode('utf-8', 'replace')
        if len(haystack) > HAY_MAX:
            row['fold_basis'] = 'unscanned'
            return
        if contains(haystack, record):
            if isinstance(st.turn, dict):
                st.turn['fold_suspect'] = True  # cache_turn's hook; label_turn writes it
            else:
                row['fold_suspect'] = True
            row['fold_basis'] = basis
    except Exception:  # advisory: a diagnostic never fails a request
        pass


def feed(st, chunk):
    diag = st.robust.diag if st.robust is not None else None
    if diag is not None:
        diag.feed(chunk)


def finish(st):
    """After the reply: remember the relayed turn's hashes for the session's next request."""
    diag = st.robust.diag if st.robust is not None else None
    if diag is None or st.backend_status != 200:
        return
    try:
        result = diag.result()
        if result is None:
            return
        reply, stop, record = result
        key, recent_key = _keys(st)
        n = getattr(st.robust, 'diag_messages', None)
        with _lock:
            if key is not None and isinstance(n, int):
                _sessions[key] = {'n': n, 'forms': reply, 'fold': record}
                _sessions.move_to_end(key)
                while len(_sessions) > SESSIONS:
                    _sessions.popitem(last=False)
            if record is not None:
                _recent[recent_key] = record
                _recent.move_to_end(recent_key)
                while len(_recent) > SESSIONS:
                    _recent.popitem(last=False)
            elif recent_key in _recent and not stop:
                _recent.pop(recent_key)
    except Exception:
        pass
