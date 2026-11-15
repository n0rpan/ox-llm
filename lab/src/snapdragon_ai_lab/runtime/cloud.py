"""Cloud models through the controller (Wave D: D1/D2).

A remote model is a catalog entry of kind ``remote`` (or ``remote/openrouter`` /
``remote/anthropic`` / ``remote/openai``) under ``cloud.models`` in ``runtime.json``.  This module is the
metadata and privacy layer: it reads the catalog, the owner-only secrets file at request
time, the per-day ledger spend and the upstream HTTP connection.  It never logs a key and
never returns one to a caller that renders status.
"""
import http.client
import json
import math
import os
from pathlib import Path
import re
import ssl
import threading
import urllib.parse

if __package__:
    from .ctl_seams import ctl
else:
    from ctl_seams import ctl

PROVIDERS = ('openrouter', 'anthropic', 'openai')
KINDS = {'remote': None, 'remote/openrouter': 'openrouter', 'remote/anthropic': 'anthropic',
         'remote/openai': 'openai'}
ID_RE = re.compile(r'[A-Za-z0-9][A-Za-z0-9._:/-]{0,127}')
ENV_OFFLINE = 'OXC_OFFLINE'
LEGACY_OFFLINE = 'LOCAL_AI_OFFLINE'
DEFAULT_TIMEOUT = 1800.0
MAX_SECRET_BYTES = 64 * 1024
MAX_ERROR_BYTES = 1024 * 1024
COST_EPSILON = 1e-12
_TRUE = ('1', 'true', 'yes')

# Admission reservations are per local day and per process: a request reserves its worst-case
# cost here before it is forwarded, so concurrent requests cannot all spend the same budget.
_budget_lock = threading.Lock()
_reserved = {}
_ledger_failures = set()



class CloudError(Exception):
    """A remote request the controller refuses.  The message is safe to show a client."""

    def __init__(self, message, status=503, code='cloud_unavailable', param='model'):
        super().__init__(message)
        self.status = status
        self.code = code
        self.param = param


def truthy(value):
    return isinstance(value, str) and value.strip().lower() in _TRUE


def settings(value):
    raw = value.get('cloud') if isinstance(value, dict) else None
    return raw if isinstance(raw, dict) else {}


def offline(environ=None):
    environ = os.environ if environ is None else environ
    return truthy(environ.get(ENV_OFFLINE)) or truthy(environ.get(LEGACY_OFFLINE))


def enabled(value):
    return settings(value).get('enabled') is True


def provider_of(raw):
    """The provider for a valid catalog entry kind, else None."""
    if not isinstance(raw, dict):
        return None
    kind = raw.get('kind', 'remote')
    if kind in KINDS:
        provider = raw.get('provider') or KINDS[kind]
    else:
        provider = None
    return provider if provider in PROVIDERS else None


def _positive_number(raw, key, default=None):
    value = raw.get(key, default)
    return value if type(value) in (int, float) and math.isfinite(value) and value > 0 else None


def _pricing(raw):
    price = raw.get('pricing')
    if isinstance(price, dict):
        def one(*names):
            for name in names:
                value = price.get(name)
                if type(value) in (int, float) and math.isfinite(value) and value >= 0:
                    return float(value)
            return None
        input_price = one('input_per_million', 'input')
        output_price = one('output_per_million', 'output')
        if input_price is not None and output_price is not None:
            return {'input_per_million': input_price, 'output_per_million': output_price}
    if raw.get('free') is True:
        return {'input_per_million': 0.0, 'output_per_million': 0.0}
    return None  # Unknown pricing is never silently free.


def _base_url(raw):
    value = raw.get('base_url') if isinstance(raw, dict) else None
    if not isinstance(value, str):
        return None
    try:
        parts = urllib.parse.urlsplit(value)
    except ValueError:
        return None
    if parts.scheme not in ('http', 'https') or not parts.hostname or parts.username or parts.password:
        return None
    if parts.query or parts.fragment:
        return None
    return value.rstrip('/')


def entry(alias, raw):
    """The normalized entry for one catalog row, or None when the row is unusable."""
    provider = provider_of(raw)
    if provider is None or not isinstance(alias, str) or not ID_RE.fullmatch(alias):
        return None
    base_url = _base_url({**raw, 'base_url': raw.get('base_url', 'https://api.openai.com/v1')}
                         if provider == 'openai' else raw)
    upstream = raw.get('upstream_model') or raw.get('upstream')
    context = _positive_number(raw, 'context_window')
    output = _positive_number(raw, 'max_output')
    if provider == 'openai' and output is None:
        return None
    if base_url is None or not isinstance(upstream, str) or not upstream.strip() or context is None:
        return None
    return {'id': alias, 'kind': raw.get('kind', 'remote'), 'provider': provider,
            'base_url': base_url, 'upstream_model': upstream.strip(),
            'context_window': int(context), 'max_output': int(output) if output is not None else None,
            'pricing': _pricing(raw), 'display_name': raw.get('name') if isinstance(raw.get('name'), str) else alias}


def entries(value, include_disabled=False):
    """{alias: entry} for the valid catalog rows; [] while cloud is off unless asked."""
    if not include_disabled and not enabled(value):
        return {}
    models = settings(value).get('models')
    if not isinstance(models, dict):
        return {}
    out = {}
    for alias, raw in models.items():
        item = entry(alias, raw)
        if item is not None:
            out[alias] = item
    return out


def _name_candidates(name):
    if not isinstance(name, str):
        return []
    candidates = [name]
    for prefix in ('openai/', 'openai_compatible/', 'local/'):
        if name.startswith(prefix):
            candidates.append(name[len(prefix):])
    return candidates


def model(value, name, include_disabled=False):
    """The catalog entry named by a request, or None (unknown or cloud off)."""
    table = entries(value, include_disabled=include_disabled)
    for candidate in _name_candidates(name):
        if candidate in table:
            return table[candidate]
    return None


def models(value):
    return sorted(entries(value).values(), key=lambda item: item['id'])


def pricing(entry_value):
    return entry_value.get('pricing')


def cost(entry_value, prompt_tokens, completion_tokens):
    """USD for one usage record; numbers only, bounded to a small finite value."""
    price = pricing(entry_value)
    if price is None:
        return None
    prompt = prompt_tokens if type(prompt_tokens) is int and prompt_tokens >= 0 else 0
    completion = completion_tokens if type(completion_tokens) is int and completion_tokens >= 0 else 0
    if prompt > 100_000_000 or completion > 10_000_000:
        return None
    return round((prompt * price['input_per_million'] + completion * price['output_per_million']) / 1_000_000.0, 8)


def _cap(value):
    cap = settings(value).get('daily_budget_usd')
    return float(cap) if type(cap) in (int, float) and math.isfinite(cap) and cap > 0 else None


def budget(value):
    """The daily USD cap (None = not configured, which fails closed) and today's spend.

    ``spend_today`` raises :class:`CloudError` when the ledger cannot be read or is corrupt:
    an accounting failure must never look like zero spend.
    """
    return _cap(value), spend_today(value)


def _ledger_dir():
    try:
        return ctl.request_ledger.ledger_dir()
    except Exception:
        return None


def _ledger_error():
    return CloudError('The request ledger is unreadable; cloud calls are blocked until it is repaired.',
                      503, 'cloud_ledger_unavailable')


def _read_spend(path):
    total = 0.0
    try:
        stream = open(path, encoding='utf-8')
    except FileNotFoundError:
        return 0.0
    except (OSError, UnicodeError):
        raise _ledger_error() from None
    try:
        with stream:
            for line in stream:
                if not line.strip():
                    continue
                try:
                    row = json.loads(line)
                except ValueError:
                    raise _ledger_error() from None
                if not isinstance(row, dict):
                    raise _ledger_error() from None
                if row.get('remote') is not True:
                    continue
                usd = row.get('cost_usd')
                if type(usd) not in (int, float) or not math.isfinite(usd) or usd < 0:
                    # A remote row without a settled cost under-counts the cap: fail closed.
                    raise _ledger_error() from None
                total += usd
    except CloudError:
        raise
    except (OSError, UnicodeError):
        raise _ledger_error() from None
    return round(total, 8)


def spend_today(value=None, day=None):
    """Today's remote spend from the request ledger.

    Missing today's file is zero spend.  An unreadable file, a malformed row or a remote row
    without a cost raises :class:`CloudError` (fail closed) instead of returning 0.0.
    """
    directory = _ledger_dir()
    if directory is None:
        raise _ledger_error()
    import time
    day = day or time.strftime('%Y%m%d')
    path = Path(directory) / ('requests-%s.jsonl' % day)
    # Read under the ledger's own append lock so a row written by a concurrent finish is
    # either absent or complete; never parse a half-appended line.
    lock = getattr(ctl.request_ledger, 'LEDGER_LOCK', None)
    if lock is None:
        return _read_spend(path)
    with lock:
        return _read_spend(path)


def mark_ledger_failure(value=None):
    """A cloud ledger row could not be written: refuse further cloud calls for this directory."""
    directory = _ledger_dir()
    if directory is not None:
        _ledger_failures.add(str(directory))


def _check_ledger_health():
    directory = _ledger_dir()
    if directory is not None and str(directory) in _ledger_failures:
        raise _ledger_error()


def _max_tokens(payload, item):
    """The validated catalog-bounded output limit shared by translation and admission."""
    fields = ('max_tokens', 'max_completion_tokens', 'n_predict')
    if item.get('provider') == 'openai':
        fields += ('max_output_tokens',)
    maximum = item.get('max_output') or 4096
    for field in fields:
        if isinstance(payload, dict) and field in payload:
            value = payload[field]
            if type(value) is not int or value <= 0:
                raise CloudError('The output token limit must be a positive integer.',
                                 400, 'invalid_value', field)
            return min(value, maximum)
    return maximum


def estimate_prompt_tokens(payload):
    """A deterministic upper-ish estimate for admission: UTF-8 bytes of the prompt / 4."""
    if not isinstance(payload, dict):
        return 1
    material = {'messages': payload.get('messages', payload.get('input')), 'tools': payload.get('tools'),
                'system': payload.get('system')}
    try:
        text = json.dumps(material, ensure_ascii=False, sort_keys=True, separators=(',', ':'))
    except (TypeError, ValueError):
        text = str(material)
    return max(1, (len(text.encode('utf-8', 'replace')) + 3) // 4)


def worst_case_cost(item, payload):
    """Reserve the catalog input window; byte estimates cannot bound tokenization or images."""
    prompt = item['context_window']
    usd = cost(item, prompt, _max_tokens(payload, item))
    # Routing prices may exceed the catalog. This headroom is additional to the
    # full-window reservation; it is a cushion, not a universal provider-price bound.
    if usd is not None and item['provider'] == 'openrouter':
        usd *= 1.25
    return math.inf if usd is None else usd


class BudgetHold:
    """One admission reservation.  ``settle`` is idempotent and must always run."""

    def __init__(self, day, reserved):
        self.day = day
        self.reserved = reserved
        self.settled = False

    def settle(self):
        with _budget_lock:
            if self.settled:
                return
            remaining = _reserved.get(self.day, 0.0) - self.reserved
            if remaining > 0:
                _reserved[self.day] = remaining
            else:
                _reserved.pop(self.day, None)
            self.settled = True


def validate_remote_request(payload, item):
    try:
        json.dumps(payload, ensure_ascii=False, allow_nan=False).encode('utf-8')
    except (TypeError, ValueError, UnicodeError):
        raise CloudError('Invalid cloud request encoding.', 400, 'invalid_request') from None
    n = payload.get('n') if isinstance(payload, dict) else None
    if n is not None and (type(n) is not int or n != 1):
        raise CloudError('Cloud requests support one completion.', 400, 'invalid_value', 'n')
    if pricing(item) is None:
        raise CloudError('Cloud models need catalog pricing or explicit free: true.',
                         400, 'cloud_pricing_unavailable', 'pricing')


def reserve(value, item, payload):
    """Atomically reserve the worst-case cost before forwarding, or raise a clear CloudError."""
    validate_remote_request(payload, item)
    cap = _cap(value)
    if cap is None:
        raise CloudError('Cloud models need a per-day USD budget (`cloud.daily_budget_usd`).',
                         403, 'budget_not_configured')
    if not getattr(ctl.request_ledger, 'enabled', lambda: True)():
        raise CloudError('The request ledger is disabled; cloud calls are blocked until it is enabled.',
                         503, 'cloud_ledger_unavailable')
    _check_ledger_health()
    day = spend_day()
    worst = worst_case_cost(item, payload)
    with _budget_lock:
        spent = spend_today(value, day=day)
        already = _reserved.get(day, 0.0)
        if spent + already + worst > cap + COST_EPSILON:
            raise CloudError('The daily cloud budget is exhausted.', 429, 'budget_exceeded')
        _reserved[day] = already + worst
    return BudgetHold(day, worst)


def spend_day():
    import time
    return time.strftime('%Y%m%d')


def conservative_cost(row, hold, failed=False):
    """Fill a ledger row from a reservation when usage or cost is missing.

    A failed request (or a reply that never reported usage) is charged the reserved
    worst case, never zero.  The reservation is released only by the caller after the row has
    been written, so a crash cannot make spend disappear.
    """
    if hold is None:
        return row
    missing = (failed or row.get('prompt_tokens') is None or row.get('completion_tokens') is None
               or row.get('cost_usd') is None)
    if missing:
        row['cost_usd'] = max(hold.reserved, row.get('cost_usd') or 0)
        row['usage_missing'] = True
        row['settled'] = 'reserved'
    else:
        row['settled'] = 'usage'
    return row



def secrets_file(value):
    """The configured 0600 secrets file; it must be absolute and outside this repository."""
    raw = settings(value).get('secrets_file')
    path = Path(raw) if isinstance(raw, str) and raw else Path(ctl.CONFIG).parent / 'cloud-secrets.json'
    if not path.is_absolute():
        raise CloudError('Cloud credentials are not configured.', 503, 'cloud_credentials_unavailable')
    try:
        resolved = path.resolve()
        for parent in resolved.parents:
            # Check the configured path, independently of source/versioned/flat module layout.
            # Worktree .git files count, as do exported product source trees without Git metadata.
            if ((parent / '.git').exists() or ((parent / 'AGENTS.md').is_file()
                    and (parent / 'lab').is_dir() and (parent / 'engine').is_dir())):
                raise CloudError('Cloud credentials are not configured.', 503, 'cloud_credentials_unavailable')
    except (OSError, RuntimeError, ValueError):
        raise CloudError('Cloud credentials are not configured.', 503, 'cloud_credentials_unavailable') from None
    return path


def _read_secrets(value):
    path = secrets_file(value)
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    except OSError:
        raise CloudError('Cloud credentials are not configured.', 503, 'cloud_credentials_unavailable') from None
    try:
        info = os.fstat(fd)
        import stat
        if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid()
                or info.st_mode & 0o077 or info.st_size > MAX_SECRET_BYTES):
            raise CloudError('Cloud credentials are not configured.', 503, 'cloud_credentials_unavailable')
        data = os.read(fd, MAX_SECRET_BYTES + 1)
    except OSError:
        raise CloudError('Cloud credentials are not configured.', 503, 'cloud_credentials_unavailable') from None
    finally:
        os.close(fd)
    try:
        parsed = json.loads(data)
    except (ValueError, UnicodeDecodeError):
        raise CloudError('Cloud credentials are not configured.', 503, 'cloud_credentials_unavailable') from None
    return parsed if isinstance(parsed, dict) else {}


def secret(provider, value):
    """Read one provider key at request time.  The key is never put in an exception."""
    data = _read_secrets(value)
    raw = data.get(provider)
    if isinstance(raw, dict):
        raw = raw.get('api_key')
    if (not isinstance(raw, str) or not raw or len(raw) > 8192
            or any(not 33 <= ord(char) <= 126 for char in raw)):
        raise CloudError('Cloud credentials are not configured.', 503, 'cloud_credentials_unavailable')
    return raw


def configured(value):
    """{provider: bool} for status; never the key or its path."""
    out = {}
    for provider in PROVIDERS:
        try:
            secret(provider, value)
            out[provider] = True
        except CloudError:
            out[provider] = False
    return out


def redact(text, key):
    """Remove a known key from a body/error string before it can reach a client or a log."""
    if not key:
        return text
    if isinstance(text, bytes):
        return text.replace(key.encode('utf-8'), b'[redacted]')
    if isinstance(text, str):
        return text.replace(key, '[redacted]')
    return text


def redact_value(value, key):
    """Redact decoded JSON too, including escaped strings and metadata."""
    if isinstance(value, str):
        return redact(value, key)
    if isinstance(value, list):
        return [redact_value(item, key) for item in value]
    if isinstance(value, dict):
        return {redact(name, key): redact_value(item, key) for name, item in value.items()}
    return value


class StreamRedactor:
    """Incremental redactor for a byte stream: a key split across chunks is still removed.

    Only the longest suffix that could be the start of the key is held, so ordinary text is
    never delayed just because the key is long.
    """

    def __init__(self, key):
        self.key = key.encode('utf-8') if isinstance(key, str) and key else b''
        self.buffer = b''

    def feed(self, chunk):
        if not self.key:
            return chunk
        data = (self.buffer + chunk).replace(self.key, b'[redacted]')
        hold = 0
        for size in range(min(len(self.key) - 1, len(data)), 0, -1):
            if data.endswith(self.key[:size]):
                hold = size
                break
        if hold:
            self.buffer = data[-hold:]
            return data[:-hold]
        self.buffer = b''
        return data

    def finish(self):
        if not self.key or not self.buffer:
            self.buffer = b''
            return b''
        data = self.buffer.replace(self.key, b'[redacted]')
        self.buffer = b''
        return data


class SSEDecoder:
    """Incremental Server-Sent Events parser (WHATWG event-stream grammar).

    Accepts LF, CRLF and lone-CR line endings, ignores comment lines (``:``), joins every
    ``data:`` field with ``\\n`` and dispatches one event on a blank line.  It buffers only a
    partial line plus the event being assembled, never multiple complete events.
    """

    def __init__(self):
        self.buffer = b''
        self._data = []
        self._event = None

    def feed(self, chunk):
        self.buffer += chunk
        return self._drain(final=False)

    def finish(self):
        out = self._drain(final=True)
        if self.buffer:
            line, self.buffer = self.buffer, b''
            event = self._line(line)
            if event is not None:
                out.append(event)
        event = self._dispatch()
        if event is not None:
            out.append(event)
        return out

    def _drain(self, final):
        out = []
        while True:
            cr = self.buffer.find(b'\r')
            lf = self.buffer.find(b'\n')
            if cr < 0 and lf < 0:
                break
            if cr >= 0 and (lf < 0 or cr < lf):
                if cr + 1 >= len(self.buffer) and not final:
                    break  # a trailing CR may be the first byte of a CRLF pair
                if self.buffer[cr + 1:cr + 2] == b'\n':
                    line, self.buffer = self.buffer[:cr], self.buffer[cr + 2:]
                else:
                    line, self.buffer = self.buffer[:cr], self.buffer[cr + 1:]
            else:
                line, self.buffer = self.buffer[:lf], self.buffer[lf + 1:]
            event = self._line(line)
            if event is not None:
                out.append(event)
        return out

    def _line(self, line):
        if line == b'':
            return self._dispatch()
        if line.startswith(b':'):
            return None  # comment / keep-alive
        colon = line.find(b':')
        if colon < 0:
            field, value = line, b''
        else:
            field, value = line[:colon], line[colon + 1:]
            if value.startswith(b' '):
                value = value[1:]
        if field == b'data':
            self._data.append(value)
        elif field == b'event':
            self._event = value.decode('utf-8', 'replace')
        return None

    def _dispatch(self):
        event = self._event
        self._event = None
        if not self._data:
            return None
        data = b'\n'.join(self._data)
        self._data = []
        return event, data


def capabilities(item):
    """The Wave D per-model capability block for a remote catalog entry."""
    return {'remote': True, 'remote_provider': item['provider'], 'echo': False,
            'cache_hints': False, 'cancel': item['provider'] != 'openai', 'prefill': False,
            'preserve_thinking': False, 'context_window': item['context_window']}


def row(item):
    """One `/v1/models` row for a remote model.  No key, path or upstream URL."""
    return {'id': item['id'], 'object': 'model', 'created': 0, 'owned_by': item['provider'],
            'name': item['display_name'], 'remote': True, 'provider': item['provider'],
            'context_length': item['context_window'],
            'context_window': item['context_window'],
            'max_output_tokens': item['max_output'],
            'capabilities': capabilities(item)}


def catalog_row(item):
    """One `/_local/models` chooser row for a remote model."""
    return {'id': item['id'], 'key': 'cloud', 'name': item['display_name'], 'engine': 'remote',
            'engine_status': 'remote', 'qualification': None, 'memory_gib': None,
            'battery_start_percent': None, 'prefix': 'off', 'selected': False, 'default': False,
            'state': 'unloaded', 'remote': True, 'provider': item['provider'],
            'context_window': item['context_window'], 'max_output': item['max_output'],
            'capabilities': capabilities(item)}


def remote_payload(payload, item):
    """The upstream body for an OpenAI-compatible remote: the client's body with the upstream
    id and the mandatory OpenRouter privacy prefs.  D3 adds the normalisation on top."""
    if not isinstance(payload, dict):
        raise CloudError('Invalid cloud request.', 400, 'invalid_request')
    fields = ('messages', 'tools', 'tool_choice', 'parallel_tool_calls', 'stream',
              'response_format', 'temperature', 'top_p', 'frequency_penalty',
              'presence_penalty', 'logit_bias', 'logprobs', 'top_logprobs', 'stop', 'seed',
              'reasoning_effort', 'chat_template_kwargs')
    body = {name: payload[name] for name in fields if name in payload}
    if body.get('tools') is None:
        body.pop('tools', None)
    tools = body.get('tools', [])
    if not isinstance(tools, list) or any(
            not isinstance(tool, dict) or tool.get('type') != 'function'
            or set(tool) - {'type', 'function'} or not isinstance(tool.get('function'), dict)
            for tool in tools):
        raise CloudError('Only client function tools are supported.', 400, 'invalid_value', 'tools')
    body['model'] = item['upstream_model']
    field = 'max_completion_tokens' if 'max_completion_tokens' in payload else 'max_tokens'
    # Resolve alias precedence once; admission receives this normalized body.
    body[field] = _max_tokens(payload, item)
    for field in ('cache_prompt', 'n_predict', 'id_slot', 'slot',
                  'ignore_eos', 'typical_p', 'min_p', 'repeat_penalty', 'top_k'):
        body.pop(field, None)
    if item['provider'] == 'openrouter':
        body['provider'] = {'zdr': True, 'data_collection': 'deny'}
    return body


def path_for(item):
    if item['provider'] == 'openai':
        if __package__:
            from . import cloud_openai
        else:
            import cloud_openai
        return urllib.parse.urlsplit(item['base_url']).path.rstrip('/') + cloud_openai.endpoint(item)
    if item['provider'] == 'anthropic':
        return '/v1/messages'
    return urllib.parse.urlsplit(item['base_url']).path.rstrip('/') + '/chat/completions'


def connection(item, key):
    """(connection, path, headers) for the upstream call.  Raises CloudError only."""
    parts = urllib.parse.urlsplit(item['base_url'])
    try:
        if parts.scheme == 'https':
            connection = http.client.HTTPSConnection(parts.hostname, parts.port or 443,
                                                     timeout=min(DEFAULT_TIMEOUT, ctl.BACKEND_READ_TIMEOUT_SECONDS),
                                                     context=ssl.create_default_context())
        else:
            connection = http.client.HTTPConnection(parts.hostname, parts.port or 80,
                                                    timeout=min(DEFAULT_TIMEOUT, ctl.BACKEND_READ_TIMEOUT_SECONDS))
    except (OSError, ValueError):
        raise CloudError('The cloud provider is unavailable.', 503, 'cloud_unavailable') from None
    path = path_for(item)
    headers = {'Content-Type': 'application/json', 'Accept': 'application/json'}
    if item['provider'] == 'anthropic':
        headers.update({'x-api-key': key, 'anthropic-version': '2023-06-01'})
    else:
        headers['Authorization'] = 'Bearer ' + key
    return connection, path, headers


class Meter:
    """Usage/finish numbers from an upstream response; no prompt or reply text is kept."""

    def __init__(self):
        self.prompt = None
        self.completion = None
        self.cached = None
        self.reported_cost = None
        self.finish = None
        self._sse = SSEDecoder()

    def _object(self, value):
        if not isinstance(value, dict):
            return
        usage = value.get('usage')
        if isinstance(usage, dict):
            reported = usage.get('cost')
            if type(reported) in (int, float) and 0 <= reported <= 1e100:
                # Usage frames may repeat. Never let a later partial frame reduce a bill.
                self.reported_cost = max(self.reported_cost or 0, reported)
            for source, target in (('prompt_tokens', 'prompt'), ('input_tokens', 'prompt'),
                                   ('completion_tokens', 'completion'), ('output_tokens', 'completion')):
                number = usage.get(source)
                if type(number) is int and number >= 0:
                    setattr(self, target, number)
            if type(usage.get('cache_read_input_tokens')) is int:
                self.cached = usage['cache_read_input_tokens']
            details = usage.get('prompt_tokens_details')
            if isinstance(details, dict) and type(details.get('cached_tokens')) is int:
                self.cached = details['cached_tokens']
        choices = value.get('choices')
        if isinstance(choices, list) and choices and isinstance(choices[0], dict):
            if isinstance(choices[0].get('finish_reason'), str):
                self.finish = choices[0]['finish_reason']

    def feed(self, chunk, sse=False):
        if sse:
            for _, data in self._sse.feed(chunk):
                self._event(data)
        else:
            try:
                self._object(json.loads(chunk.decode('utf-8', 'replace') if isinstance(chunk, bytes) else chunk))
            except ValueError:
                pass

    def finish_sse(self):
        """Dispatch a final event that arrived without its blank line."""
        for _, data in self._sse.finish():
            self._event(data)

    def _event(self, data):
        if data in (b'', b'[DONE]'):
            return
        try:
            self._object(json.loads(data.decode('utf-8', 'replace')))
        except ValueError:
            pass

    def row(self, item, secret=None):
        prompt, completion = self.prompt, self.completion
        out = {'remote': True, 'remote_provider': item['provider'],
               'remote_model': item['upstream_model']}
        if prompt is not None:
            out['prompt_tokens'] = prompt
        if completion is not None:
            out['completion_tokens'] = completion
        if self.cached is not None:
            out['cached_tokens'] = self.cached
        if self.finish is not None:
            out['finish_reason'] = redact(self.finish, secret)
        usd = cost(item, prompt, completion)
        if usd is not None:
            out['cost_usd'] = usd
            out['catalog_cost_usd'] = usd
        if item['provider'] == 'openrouter' and self.reported_cost is not None:
            out['reported_cost_usd'] = self.reported_cost
            out['cost_usd'] = max(usd or 0, self.reported_cost)
        return out


# ---------------------------------------------------------------- D3 normalisation
REASONING_EFFORT = {'none': 'none', 'minimal': 'none', 'low': 'low', 'medium': 'medium',
                    'high': 'high', 'xhigh': 'high'}


def upstream_payload(payload, item, key=None):
    """The normalised upstream body: the remote catalog swap plus the OpenAI v1 mapping.

    Local-only fields (``chat_template_kwargs``, cache/slot controls) are removed; an OpenAI
    ``reasoning_effort`` becomes the upstream effort (``none``/``minimal`` = off, ``xhigh`` =
    ``high``); a local ``enable_thinking`` switch is mapped the same way.  A stream asks for
    the final usage chunk so the ledger and budget see the provider's own numbers.
    """
    if item['provider'] == 'openai':
        if __package__:
            from . import cloud_openai
        else:
            import cloud_openai
        return cloud_openai.request(payload, item, key)
    body = remote_payload(payload, item)
    effort = body.get('reasoning_effort')
    kwargs = body.pop('chat_template_kwargs', None)
    if effort is not None:
        if not isinstance(effort, str):
            raise CloudError('"reasoning_effort" must be a string.', 400, 'invalid_value', 'reasoning_effort')
        if effort in REASONING_EFFORT:
            body['reasoning_effort'] = REASONING_EFFORT[effort]
        else:
            body.pop('reasoning_effort', None)
    elif isinstance(kwargs, dict) and isinstance(kwargs.get('enable_thinking'), bool):
        body['reasoning_effort'] = 'high' if kwargs['enable_thinking'] else 'none'
    if body.get('stream') is True:
        body['stream_options'] = {'include_usage': True}
    return body


def _normalize_calls(calls):
    if not isinstance(calls, list):
        return
    for position, call in enumerate(calls):
        if not isinstance(call, dict):
            continue
        call.setdefault('type', 'function')
        if 'index' not in call:
            call['index'] = position
        function = call.get('function')
        if isinstance(function, dict):
            args = function.get('arguments')
            if isinstance(args, (dict, list)):
                function['arguments'] = json.dumps(args, ensure_ascii=False)
            elif args is None:
                function['arguments'] = ''


def _normalize_message(message):
    if not isinstance(message, dict):
        return
    if 'reasoning_content' not in message and isinstance(message.get('reasoning'), str):
        message['reasoning_content'] = message['reasoning']
    _normalize_calls(message.get('tool_calls'))


def normalize_response(value, requested_model):
    """A non-streamed upstream reply in the controller's OpenAI v1 shape."""
    if not isinstance(value, dict):
        return value
    value['model'] = requested_model
    message = None
    for choice in value.get('choices') or ():
        if not isinstance(choice, dict):
            continue
        _normalize_message(choice.get('message'))
        if message is None:
            message = choice.get('message')
    try:
        from .openai_conformance import complete_usage, reasoning_estimate
    except ImportError:
        from openai_conformance import complete_usage, reasoning_estimate
    if isinstance(value.get('usage'), dict):
        value['usage'] = complete_usage(
            value['usage'], reasoning_estimate(message, value['usage'].get('completion_tokens')),
            value.get('timings'))
    return value


class CloudStream:
    """Pass an OpenAI-compatible SSE stream through with the same normalisation as a local
    reply: tool-call deltas keep their index and string arguments, ``reasoning`` becomes
    ``reasoning_content``, the final usage block gets both detail objects, and the model id is
    the local alias the client asked for."""

    def __init__(self, requested_model, key=None):
        self.key = key
        self.fields = {}
        self.model = requested_model
        self.parser = SSEDecoder()
        self.reasoning = 0

    def _wire(self, name, data):
        prefix = b'event: ' + name.encode() + b'\n' if name else b''
        return prefix + b'data: ' + data + b'\n\n'

    def _fragment(self, choice, field, text, slot=None, final=False):
        token = (choice, field, slot)
        redactor = self.fields.setdefault(token, StreamRedactor(self.key))
        safe = redactor.feed(text.encode('utf-8'))
        if final:
            safe += redactor.finish()
        return safe.decode('utf-8')

    def _tails(self):
        out = b''
        for (index, field, slot), redactor in self.fields.items():
            tail = redactor.finish().decode('utf-8')
            if not tail:
                continue
            delta = ({'tool_calls': [{'index': slot, 'function': {field: tail}}]}
                     if slot is not None else {field: tail})
            value = {'model': self.model, 'object': 'chat.completion.chunk',
                     'choices': [{'index': index, 'delta': delta, 'finish_reason': None}]}
            out += self._wire(None, json.dumps(value, ensure_ascii=False).encode())
        return out

    def _event(self, name, data):
        if data == b'[DONE]':
            return self._tails() + self._wire(name, data)
        if data == b'':
            return b''
        try:
            value = json.loads(data.decode('utf-8', 'replace'))
        except (ValueError, UnicodeDecodeError):
            return self._wire(name, data)
        if not isinstance(value, dict):
            return self._wire(name, data)
        value['model'] = self.model
        tails = b''
        for choice in value.get('choices') or ():
            if not isinstance(choice, dict):
                continue
            delta = choice.get('delta')
            if isinstance(delta, dict):
                if 'reasoning_content' not in delta and isinstance(delta.get('reasoning'), str):
                    delta['reasoning_content'] = delta['reasoning']
                if isinstance(delta.get('reasoning_content'), str) and delta['reasoning_content']:
                    self.reasoning += 1
                _normalize_calls(delta.get('tool_calls'))
                if self.key:
                    index = choice.get('index', 0)
                    for field in ('content', 'reasoning_content', 'refusal'):
                        if isinstance(delta.get(field), str):
                            delta[field] = self._fragment(index, field, delta[field])
                    delta.pop('reasoning', None)
                    for call in delta.get('tool_calls') or []:
                        function = call.get('function') or {}
                        for field in ('arguments', 'name'):
                            if isinstance(function.get(field), str):
                                function[field] = self._fragment(index, field, function[field],
                                                                 call.get('index', 0))
            if choice.get('finish_reason') is not None:
                tails += self._tails()
            _normalize_message(choice.get('message'))
        if isinstance(value.get('usage'), dict):
            try:
                from .openai_conformance import complete_usage
            except ImportError:
                from openai_conformance import complete_usage
            value['usage'] = complete_usage(value['usage'], self.reasoning, value.get('timings'))
        if self.key:
            value = redact_value(value, self.key)
        return tails + self._wire(name, json.dumps(value, ensure_ascii=False).encode())

    def feed(self, chunk):
        out = b''
        for name, data in self.parser.feed(chunk):
            out += self._event(name, data)
        return out

    def finish(self):
        out = b''
        for name, data in self.parser.finish():
            out += self._event(name, data)
        return out
