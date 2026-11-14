"""Cloud models through the controller (Wave D: D1/D2).

A remote model is a catalog entry of kind ``remote`` (or ``remote/openrouter`` /
``remote/anthropic``) under ``cloud.models`` in ``runtime.json``.  This module is the
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
import urllib.parse

if __package__:
    from .ctl_seams import ctl
else:
    from ctl_seams import ctl

PROVIDERS = ('openrouter', 'anthropic')
KINDS = {'remote': None, 'remote/openrouter': 'openrouter', 'remote/anthropic': 'anthropic'}
ID_RE = re.compile(r'[A-Za-z0-9][A-Za-z0-9._:/-]{0,127}')
ENV_OFFLINE = 'OXC_OFFLINE'
LEGACY_OFFLINE = 'LOCAL_AI_OFFLINE'
DEFAULT_TIMEOUT = 1800.0
MAX_SECRET_BYTES = 64 * 1024
MAX_ERROR_BYTES = 1024 * 1024
_TRUE = ('1', 'true', 'yes')


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
    """'openrouter' / 'anthropic' for a valid catalog entry kind, else None."""
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
    if not isinstance(price, dict):
        return {'input_per_million': 0.0, 'output_per_million': 0.0}
    def one(*names):
        for name in names:
            value = price.get(name)
            if type(value) in (int, float) and math.isfinite(value) and value >= 0:
                return float(value)
        return 0.0
    return {'input_per_million': one('input_per_million', 'input'),
            'output_per_million': one('output_per_million', 'output')}


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
    base_url = _base_url(raw)
    upstream = raw.get('upstream_model') or raw.get('upstream')
    context = _positive_number(raw, 'context_window')
    output = _positive_number(raw, 'max_output')
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
    return entry_value.get('pricing') or {'input_per_million': 0.0, 'output_per_million': 0.0}


def cost(entry_value, prompt_tokens, completion_tokens):
    """USD for one usage record; numbers only, bounded to a small finite value."""
    price = pricing(entry_value)
    prompt = prompt_tokens if type(prompt_tokens) is int and prompt_tokens >= 0 else 0
    completion = completion_tokens if type(completion_tokens) is int and completion_tokens >= 0 else 0
    if prompt > 100_000_000 or completion > 10_000_000:
        return None
    return round((prompt * price['input_per_million'] + completion * price['output_per_million']) / 1_000_000.0, 8)


def budget(value):
    """The daily USD cap (None = not configured, which fails closed) and today's spend."""
    cap = settings(value).get('daily_budget_usd')
    cap = float(cap) if type(cap) in (int, float) and math.isfinite(cap) and cap > 0 else None
    return cap, spend_today(value)


def _ledger_dir():
    try:
        return ctl.request_ledger.ledger_dir()
    except Exception:
        return None


def spend_today(value=None, day=None):
    """Today's remote spend from the request ledger, or 0.0 when it is unavailable."""
    directory = _ledger_dir()
    if directory is None:
        return 0.0
    import time
    day = day or time.strftime('%Y%m%d')
    path = Path(directory) / ('requests-%s.jsonl' % day)
    total = 0.0
    try:
        with open(path, encoding='utf-8') as stream:
            for line in stream:
                try:
                    row = json.loads(line)
                except ValueError:
                    continue
                if not isinstance(row, dict) or row.get('remote') is not True:
                    continue
                value = row.get('cost_usd')
                if type(value) in (int, float) and math.isfinite(value) and value >= 0:
                    total += value
    except OSError:
        return 0.0
    return round(total, 8)


def secrets_file(value):
    """The configured 0600 secrets file; it must be absolute and outside this repository."""
    raw = settings(value).get('secrets_file')
    path = Path(raw) if isinstance(raw, str) and raw else Path(ctl.CONFIG).parent / 'cloud-secrets.json'
    if not path.is_absolute():
        raise CloudError('Cloud credentials are not configured.', 503, 'cloud_credentials_unavailable')
    try:
        repo = Path(__file__).resolve().parents[4]
        if path.resolve().is_relative_to(repo):
            raise CloudError('Cloud credentials are not configured.', 503, 'cloud_credentials_unavailable')
    except (OSError, ValueError):
        pass
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
    if not isinstance(raw, str) or not raw.strip() or len(raw) > 8192:
        raise CloudError('Cloud credentials are not configured.', 503, 'cloud_credentials_unavailable')
    return raw.strip()


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
    """Remove a known key from an error/body string before it can reach a client or a log.
    Bytes stay bytes (an SSE or JSON body is relayed as it arrived, minus the key)."""
    was_bytes = isinstance(text, bytes)
    if was_bytes:
        text = text.decode('utf-8', 'replace')
    if not isinstance(text, str):
        return text
    if key:
        text = text.replace(key, '[redacted]')
    return text.encode('utf-8') if was_bytes else text


def capabilities(item):
    """The Wave D per-model capability block for a remote catalog entry."""
    return {'remote': True, 'remote_provider': item['provider'], 'echo': False,
            'cache_hints': False, 'cancel': True, 'prefill': False,
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
    body = dict(payload)
    body['model'] = item['upstream_model']
    for field in ('cache_prompt', 'n_predict', 'id_slot', 'slot',
                  'ignore_eos', 'typical_p', 'min_p', 'repeat_penalty', 'top_k'):
        body.pop(field, None)
    if item['provider'] == 'openrouter':
        prefs = body.get('provider') if isinstance(body.get('provider'), dict) else {}
        body['provider'] = {**prefs, 'zdr': True, 'data_collection': 'deny'}
    return body


def path_for(item):
    if item['provider'] == 'anthropic':
        return '/v1/messages'
    return urllib.parse.urlsplit(item['base_url']).path.rstrip('/') + '/chat/completions'


def connection(item, secret_key):
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
        headers.update({'x-api-key': secret_key, 'anthropic-version': '2023-06-01'})
    else:
        headers['Authorization'] = 'Bearer ' + secret_key
    return connection, path, headers


class Meter:
    """Usage/finish numbers from an upstream response; no prompt or reply text is kept."""

    def __init__(self):
        self.prompt = None
        self.completion = None
        self.cached = None
        self.finish = None
        self._buffer = ''

    def _object(self, value):
        if not isinstance(value, dict):
            return
        usage = value.get('usage')
        if isinstance(usage, dict):
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
            self._buffer += chunk.decode('utf-8', 'replace') if isinstance(chunk, bytes) else chunk
            *lines, self._buffer = self._buffer.split('\n')
            for line in lines:
                line = line.strip()
                if not line.startswith('data:'):
                    continue
                data = line[5:].strip()
                if data in ('', '[DONE]'):
                    continue
                try:
                    self._object(json.loads(data))
                except ValueError:
                    continue
        else:
            try:
                self._object(json.loads(chunk.decode('utf-8', 'replace') if isinstance(chunk, bytes) else chunk))
            except ValueError:
                pass

    def row(self, item):
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
            out['finish_reason'] = self.finish
        usd = cost(item, prompt, completion)
        if usd is not None:
            out['cost_usd'] = usd
        return out


# ---------------------------------------------------------------- D3 normalisation
REASONING_EFFORT = {'none': 'none', 'minimal': 'none', 'low': 'low', 'medium': 'medium',
                    'high': 'high', 'xhigh': 'high'}


def upstream_payload(payload, item):
    """The normalised upstream body: the remote catalog swap plus the OpenAI v1 mapping.

    Local-only fields (``chat_template_kwargs``, cache/slot controls) are removed; an OpenAI
    ``reasoning_effort`` becomes the upstream effort (``none``/``minimal`` = off, ``xhigh`` =
    ``high``); a local ``enable_thinking`` switch is mapped the same way.  A stream asks for
    the final usage chunk so the ledger and budget see the provider's own numbers.
    """
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
        options = body.get('stream_options') if isinstance(body.get('stream_options'), dict) else {}
        body['stream_options'] = {**options, 'include_usage': True}
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

    def __init__(self, requested_model):
        self.model = requested_model
        self.buffer = b''
        self.reasoning = 0

    def _frame(self, raw):
        if not raw.startswith(b'data:'):
            return raw + b'\n\n'
        data = raw[5:].strip()
        if data in (b'', b'[DONE]'):
            return raw + b'\n\n'
        try:
            value = json.loads(data)
        except ValueError:
            return raw + b'\n\n'
        if not isinstance(value, dict):
            return raw + b'\n\n'
        value['model'] = self.model
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
            _normalize_message(choice.get('message'))
        if isinstance(value.get('usage'), dict):
            try:
                from .openai_conformance import complete_usage
            except ImportError:
                from openai_conformance import complete_usage
            value['usage'] = complete_usage(value['usage'], self.reasoning, value.get('timings'))
        return b'data: ' + json.dumps(value, ensure_ascii=False).encode() + b'\n\n'

    def feed(self, chunk):
        self.buffer += chunk
        out = b''
        while b'\n\n' in self.buffer:
            raw, self.buffer = self.buffer.split(b'\n\n', 1)
            out += self._frame(raw)
        return out

    def finish(self):
        out = self._frame(self.buffer) if self.buffer else b''
        self.buffer = b''
        return out
