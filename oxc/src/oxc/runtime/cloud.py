"""Cloud models through the controller (Wave D: D1/D2).

A remote model is a catalog entry of kind ``remote`` (or ``remote/openrouter`` /
``remote/anthropic`` / ``remote/openai``) under ``cloud.models`` in ``runtime.json``.  This module is the
metadata and privacy layer: it reads the catalog, the owner-only secrets file at request
time, the per-day ledger spend and the upstream HTTP connection.  It never logs a key and
never returns one to a caller that renders status.
"""
import errno
import http.client
from decimal import Decimal
import json
import math
import os
from pathlib import Path
import re
import ssl
import sys
import stat
import threading
import time
import urllib.parse

if __package__:
    from .ctl_seams import ctl
    from . import cloud_chatgpt, cloud_prices
else:
    from ctl_seams import ctl
    import cloud_chatgpt
    import cloud_prices

PROVIDERS = ('openrouter', 'anthropic', 'openai', 'chatgpt')
KINDS = {'remote': None, 'remote/openrouter': 'openrouter', 'remote/anthropic': 'anthropic',
         'remote/openai': 'openai', 'remote/chatgpt': 'chatgpt'}
ID_RE = re.compile(r'[A-Za-z0-9][A-Za-z0-9._:/-]{0,127}')
ENV_OFFLINE = 'OXC_OFFLINE'
LEGACY_OFFLINE = 'OXC_OFFLINE'
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
_unrecorded = {}  # ledger dir -> spend whose row could not be written



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


def share_with_lan(value):
    """The owner's explicit opt-in (default off) to let LAN-share guests use API-key cloud models."""
    return enabled(value) and settings(value).get('share_with_lan') is True


def guest_refusal(value, item):
    """None when a LAN-share guest may use this cloud model, else the 403 message.  The ChatGPT
    subscription belongs to one person's account and is never shared, opt-in or not."""
    if item['provider'] == 'chatgpt':
        return 'The ChatGPT subscription is never available to LAN-share requests.'
    if not share_with_lan(value):
        return ('Cloud models are not available to LAN-share requests. The owner can allow them '
                'with cloud.share_with_lan.')
    return None


def guest_models(value):
    """The cloud catalog a LAN-share guest may see and use."""
    return [item for item in models(value) if guest_refusal(value, item) is None]


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
        def rates(value):
            result = {}
            for key, aliases in (
                    ('input_per_million', ('input_per_million', 'input')),
                    ('output_per_million', ('output_per_million', 'output')),
                    ('cache_read_per_million', ('cache_read_per_million',)),
                    ('cache_write_per_million', ('cache_write_per_million',))):
                number = next((value[name] for name in aliases if name in value), None)
                if number is None and key.startswith('cache_'):
                    continue
                if type(number) not in (int, float) or not 0 <= number <= 1e12:
                    return None
                result[key] = float(number)
            return result
        if 'tiers' not in price:
            normalized = rates(price)
            if normalized is not None:
                return normalized
        else:
            tiers = price['tiers']
            if not isinstance(tiers, list) or not 1 <= len(tiers) <= 64:
                return None
            result, previous = [], 0
            for index, tier in enumerate(tiers):
                if not isinstance(tier, dict):
                    return None
                normalized = rates({**price, **tier})
                if normalized is None:
                    return None
                bound = tier.get('max_prompt_tokens')
                if index == len(tiers) - 1:
                    if bound is not None:
                        return None  # Require an explicit final catch-all price.
                elif type(bound) is not int or not previous < bound <= 100_000_000:
                    return None
                else:
                    previous = bound
                normalized['max_prompt_tokens'] = bound
                result.append(normalized)
            return {'tiers': result}
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
    if provider in ('openai', 'chatgpt') and output is None:
        return None
    if provider == 'chatgpt' and not alias.startswith(cloud_chatgpt.ALIAS_PREFIX):
        return None  # subscription models are always named chatgpt/<model>
    if base_url is None or not isinstance(upstream, str) or not upstream.strip() or context is None:
        return None
    price = _pricing(raw)
    if provider == 'chatgpt':
        price = {'input_per_million': 0.0, 'output_per_million': 0.0}  # cost class subscription
    if provider == 'openrouter' and (price is not None or raw.get('pricing') is None):
        price = cloud_prices.overlay(price, upstream.strip())
    return {'id': alias, 'kind': raw.get('kind', 'remote'), 'provider': provider,
            'base_url': base_url, 'upstream_model': upstream.strip(),
            'context_window': int(context), 'max_output': int(output) if output is not None else None,
            'pricing': price, 'cost_class': 'subscription' if provider == 'chatgpt' else 'usage',
            'display_name': raw.get('name') if isinstance(raw.get('name'), str) else alias}


_PREFIXES = ('openai/', 'openai_compatible/', 'local/')


def local_aliases(value):
    """The local profile and base aliases of a runtime config (none on a cloud-only install)."""
    if not isinstance(value, dict) or value.get('cloud_only') is True:
        return set()
    names = {'qwen-local'}
    if isinstance(value.get('model_alias'), str):
        names.add(value['model_alias'])
    if isinstance(value.get('profiles'), dict):
        names.update(name for name in value['profiles'] if isinstance(name, str))
    return names


def _collision_forms(alias):
    forms = {alias}
    for prefix in _PREFIXES:
        if alias.startswith(prefix):
            forms.add(alias[len(prefix):])
        forms.add(prefix + alias)
    return forms


def collides(alias, value):
    """The local alias a cloud alias would shadow (itself or by a provider prefix), else None."""
    local = local_aliases(value)
    hit = sorted(_collision_forms(alias) & local)
    return hit[0] if hit else None


_warned = set()


def rejected(value):
    """{alias: reason} for the cloud aliases skipped because they collide with a local model."""
    models = settings(value).get('models')
    out = {}
    if isinstance(models, dict):
        for alias, raw in models.items():
            if isinstance(alias, str) and entry(alias, raw) is not None:
                hit = collides(alias, value)
                if hit is not None:
                    out[alias] = 'collides with the local model %s' % hit
    return out


def entries(value, include_disabled=False):
    """{alias: entry} for the valid catalog rows; [] while cloud is off unless asked."""
    if not include_disabled and not enabled(value):
        return {}
    models = settings(value).get('models')
    if not isinstance(models, dict):
        return {}
    out = {}
    signed_in = None
    for alias, raw in models.items():
        item = entry(alias, raw)
        if item is None:
            continue
        if item['provider'] == 'chatgpt':
            if signed_in is None:
                try:
                    signed_in = cloud_chatgpt.signed_in(secrets_file(value))
                except CloudError:
                    signed_in = False
            if not signed_in:
                continue  # opt-in: listed only after `oxc login chatgpt`
        if collides(alias, value) is not None:
            if alias not in _warned:
                _warned.add(alias)
                print('oxc: cloud model %s skipped: it collides with the local model %s'
                      % (alias, collides(alias, value)), file=sys.stderr, flush=True)
            continue
        out[alias] = item
    return out


def _name_candidates(name):
    if not isinstance(name, str):
        return []
    candidates = [name]
    for prefix in ('openai/', 'openai_compatible/'):  # never `local/`: that names a local model
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


def _cache_rates(item, price):
    base = price['input_per_million']
    # Without an explicit rate, do not assume a read discount. Anthropic's longest
    # supported write TTL costs 2x input; retain that bound when TTL pricing is absent.
    anthropic = (item['provider'] == 'anthropic'
                 or item.get('upstream_model', '').startswith('anthropic/'))
    write = price.get('cache_write_per_million', base * (2 if anthropic else 1))
    # Anthropic documents cache hits at 0.1x the base input price.
    read = price.get('cache_read_per_million',
                     base * 0.1 if item['provider'] == 'anthropic' else base)
    return read, write


def cost(entry_value, prompt_tokens, completion_tokens, *, cache_read=0, cache_write=0):
    """Price validated actual usage, selecting an inclusive prompt-size tier."""
    price = pricing(entry_value)
    if price is None:
        return None
    prompt = prompt_tokens if type(prompt_tokens) is int and prompt_tokens >= 0 else 0
    completion = completion_tokens if type(completion_tokens) is int and completion_tokens >= 0 else 0
    if prompt > 100_000_000 or completion > 10_000_000:
        return None
    if 'tiers' in price:
        price = next(tier for tier in price['tiers']
                     if tier['max_prompt_tokens'] is None or prompt <= tier['max_prompt_tokens'])
    read_rate, write_rate = _cache_rates(entry_value, price)
    read_rate, write_rate = Decimal(str(read_rate)), Decimal(str(write_rate))
    input_rate = Decimal(str(price['input_per_million']))
    output_rate = Decimal(str(price['output_per_million']))
    valid_cache = (type(cache_read) is int and type(cache_write) is int
                   and cache_read >= 0 and cache_write >= 0 and cache_read + cache_write <= prompt)
    if valid_cache:
        input_usd = ((prompt - cache_read - cache_write) * input_rate
                     + cache_read * read_rate + cache_write * write_rate)
    else:
        input_usd = prompt * max(input_rate, read_rate, write_rate)
    return float((input_usd + completion * output_rate) / Decimal(1_000_000))


def _reservation_cost(item, prompt, completion):
    price = pricing(item)
    if price is None or prompt > 100_000_000 or completion > 10_000_000:
        return None
    tiers = price.get('tiers', [price])
    input_bound = max(max(tier['input_per_million'], *_cache_rates(item, tier)) for tier in tiers)
    output_bound = max(tier['output_per_million'] for tier in tiers)
    return (prompt * input_bound + completion * output_bound) / 1_000_000.0


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


_spend_cache = {}  # path -> (inode, offset, total, tail bytes before offset)


def _row_cost(line):
    try:
        row = json.loads(line)
    except ValueError:
        raise _ledger_error() from None
    if not isinstance(row, dict):
        raise _ledger_error()
    if row.get('remote') is not True:
        return 0.0
    usd = row.get('cost_usd')
    if type(usd) not in (int, float) or not math.isfinite(usd) or usd < 0:
        # A remote row without a settled cost under-counts the cap: fail closed.
        raise _ledger_error()
    return usd


_spend_lock = threading.Lock()


def _read_spend(path):
    with _spend_lock:
        return _read_spend_locked(path)


def _read_spend_locked(path):
    """Sum remote cost.  Complete lines already summed are not re-read: the ledger is
    append-only, so only the bytes after the cached offset are parsed."""
    key = str(path)
    try:
        with open(path, 'rb') as stream:
            info = os.fstat(stream.fileno())
            ino, offset, total, tail = _spend_cache.get(key, (None, 0, 0.0, b''))
            if ino != info.st_ino or offset > info.st_size:
                offset, total, tail = 0, 0.0, b''
            elif offset:
                stream.seek(max(0, offset - len(tail)))
                if stream.read(len(tail)) != tail:  # rewritten, not appended
                    offset, total, tail = 0, 0.0, b''
            stream.seek(offset)
            data = stream.read()
    except FileNotFoundError:
        _spend_cache.pop(key, None)
        return 0.0
    except OSError:
        raise _ledger_error() from None
    consumed = data.rfind(b'\n') + 1
    try:
        for line in data[:consumed].splitlines():
            if line.strip():
                total += _row_cost(line.decode('utf-8'))
        # Bytes after the last newline are a row still being appended: not counted yet,
        # picked up by the next read once its newline is written.
    except UnicodeError:
        raise _ledger_error() from None
    if consumed:
        _spend_cache[key] = (info.st_ino, offset + consumed, total, data[max(0, consumed - 64):consumed])
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


def mark_ledger_failure(cost=None):
    """A cloud ledger row could not be written: refuse further cloud calls for this directory.

    ``cost`` is the spend that row would have recorded; it is written back when the latch clears."""
    directory = _ledger_dir()
    if directory is not None:
        with _budget_lock:
            _ledger_failures.add(str(directory))
            if type(cost) in (int, float) and math.isfinite(cost) and cost > 0:
                _unrecorded[str(directory)] = _unrecorded.get(str(directory), 0.0) + cost


def ledger_latched():
    """True while a failed ledger append blocks cloud admission for the current directory."""
    directory = _ledger_dir()
    return directory is not None and str(directory) in _ledger_failures


def _latch_error():
    return CloudError('A cloud request could not be written to the request ledger, so cloud calls are '
                      'blocked. Repair the ledger directory (free disk space, fix its permissions); '
                      'the block clears by itself on the next request once the ledger can be '
                      'written and read again.', 503, 'cloud_ledger_unavailable')


def _check_ledger_health():
    """Fail closed while latched; one cheap append+read probe per request can clear the latch."""
    directory = _ledger_dir()
    if directory is None or str(directory) not in _ledger_failures:
        return
    try:
        with _budget_lock:
            lost = _unrecorded.get(str(directory), 0.0)
        written = ctl.request_ledger.write({'remote': True, 'remote_provider': 'ledger_probe',
                                            'cost_usd': lost, 'settled': 'recovered' if lost else 'probe'})
        if written is True:
            spend_today()
            with _budget_lock:
                # Subtract only what was written; a failure recorded meanwhile stays latched.
                left = _unrecorded.get(str(directory), 0.0) - lost
                if left > 1e-12:
                    _unrecorded[str(directory)] = left
                else:
                    _unrecorded.pop(str(directory), None)
                    _ledger_failures.discard(str(directory))
            if str(directory) not in _ledger_failures:
                return
    except (CloudError, OSError):
        pass  # a probe that cannot write or read keeps the latch (fail closed)
    raise _latch_error()


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


_TEXT_PART_TYPES = frozenset(('text', 'input_text', 'output_text', 'tool_use', 'tool_result', 'thinking',
                              'redacted_thinking', 'function', 'function_call', 'function_call_output',
                              'message', 'reasoning', 'refusal'))
PROMPT_OVERHEAD_TOKENS = 2048  # provider-added framing (tool preambles, role markers)


def _text_only(value):
    """False when a message carries a non-text part (image, file, audio, ...) of unknown token cost."""
    if isinstance(value, dict):
        kind = value.get('type')
        if kind is not None and not isinstance(kind, str):
            return False
        if isinstance(kind, str) and kind not in _TEXT_PART_TYPES:
            return False
        if any(key in value for key in ('image_url', 'image', 'file', 'input_audio', 'source')):
            return False
        return all(_text_only(item) for key, item in value.items() if key != 'cache_control')
    if isinstance(value, list):
        return all(_text_only(item) for item in value)
    return True


def estimate_prompt_tokens(payload):
    """An upper bound on prompt tokens for text-only requests: one token per UTF-8 byte of the
    serialized messages, tools and system prompt, plus a fixed framing allowance.  Returns None
    when the request carries non-text content (the caller then reserves the full window)."""
    if not isinstance(payload, dict):
        return None
    conversation = {'messages': payload.get('messages', payload.get('input')),
                    'system': payload.get('system')}
    if not _text_only(conversation):
        return None
    material = {**conversation, 'tools': payload.get('tools'), 'tool_choice': payload.get('tool_choice'),
                'response_format': payload.get('response_format'), 'text': payload.get('text'),
                'instructions': payload.get('instructions')}
    try:
        text = json.dumps(material, ensure_ascii=False, sort_keys=True, separators=(',', ':'))
    except (TypeError, ValueError):
        return None
    return len(text.encode('utf-8', 'replace')) + PROMPT_OVERHEAD_TOKENS


def worst_case_cost(item, payload):
    """Reserve a request-derived prompt bound (the full window for non-text content), capped at
    the catalog window, plus the full output limit."""
    prompt = item['context_window']
    estimate = estimate_prompt_tokens(payload)
    if estimate is not None:
        prompt = min(prompt, estimate)
    usd = _reservation_cost(item, prompt, _max_tokens(payload, item))
    # Routing prices may exceed the catalog. This headroom is a cushion, not a universal bound.
    if usd is not None and item['provider'] == 'openrouter':
        usd *= 1.25
    return math.inf if usd is None else usd


def monotonic():
    """The clock for cancel accounting; tests replace it."""
    return time.monotonic()


def elapsed_since(hold):
    return 0.0 if hold is None else max(0.0, monotonic() - hold.started)


class BudgetHold:
    """One admission reservation.  ``settle`` is idempotent and must always run."""

    def __init__(self, day, reserved, output_tokens=0, started=None):
        self.day = day
        self.reserved = reserved
        self.output_tokens = output_tokens
        self.started = monotonic() if started is None else started
        self.settled = False
        self.accounted = False  # A completed accounting attempt, distinct from releasing admission.

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
    if item.get('cost_class') == 'subscription':
        return _reserve_subscription(value, item, payload)
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
    return BudgetHold(day, worst, _max_tokens(payload, item))


_sub_inflight = {}


def _subscription_use(day):
    """(requests, tokens) of today's subscription rows in the ledger (fail closed)."""
    directory = _ledger_dir()
    if directory is None:
        raise _ledger_error()
    path = Path(directory) / ('requests-%s.jsonl' % day)
    requests = tokens = 0
    lock = getattr(ctl.request_ledger, 'LEDGER_LOCK', None)
    try:
        if lock is not None:
            lock.acquire()
        try:
            stream = open(path, encoding='utf-8')
        except FileNotFoundError:
            return 0, 0
        with stream:
            for line in stream:
                if not line.strip():
                    continue
                row = json.loads(line)
                if not isinstance(row, dict):
                    raise ValueError
                if row.get('remote') is True and row.get('cost_class') == 'subscription':
                    requests += 1
                    tokens += sum(row.get(name) or 0 for name in ('prompt_tokens', 'completion_tokens')
                                  if type(row.get(name)) is int)
    except (OSError, UnicodeError, ValueError):
        raise _ledger_error() from None
    finally:
        if lock is not None:
            lock.release()
    return requests, tokens


class _SubscriptionHold(BudgetHold):
    def settle(self):
        with _budget_lock:
            if self.settled:
                return
            left = _sub_inflight.get(self.day, 0) - 1
            if left > 0:
                _sub_inflight[self.day] = left
            else:
                _sub_inflight.pop(self.day, None)
            self.settled = True


def _reserve_subscription(value, item, payload):
    """No USD budget for a subscription; optional daily request/token caps instead."""
    if not getattr(ctl.request_ledger, 'enabled', lambda: True)():
        raise CloudError('The request ledger is disabled; cloud calls are blocked until it is enabled.',
                         503, 'cloud_ledger_unavailable')
    _check_ledger_health()
    max_requests, max_tokens = cloud_chatgpt.caps(value)
    day = spend_day()
    with _budget_lock:
        requests, tokens = _subscription_use(day)
        inflight = _sub_inflight.get(day, 0)
        if ((max_requests is not None and requests + inflight >= max_requests)
                or (max_tokens is not None and tokens >= max_tokens)):
            raise CloudError('The daily subscription cap is reached.', 429, 'subscription_cap_exceeded')
        _sub_inflight[day] = inflight + 1
    return _SubscriptionHold(day, 0.0, _max_tokens(payload, item))


def spend_day():
    import time
    return time.strftime('%Y%m%d')


CLEAN_REFUSAL_STATUSES = frozenset((400, 401, 402, 403, 404, 405, 409, 413, 415, 422, 429))


def clean_refusal(status):
    """True for HTTP statuses a provider returns before generating anything.  5xx (including 529)
    can follow partial generation, so they keep the worst-case charge."""
    return status in CLEAN_REFUSAL_STATUSES


def conservative_cost(row, hold, failed=False, refused=False):
    """Fill a ledger row from a reservation when usage or cost is missing.

    A failed request (or a reply that never reported usage) is charged the reserved
    worst case, never zero, unless ``refused``: a clean upstream refusal with no usage settles at 0.  The reservation is released only by the caller after the row has
    been written, so a crash cannot make spend disappear.
    """
    if hold is None:
        return row
    if refused and row.get('prompt_tokens') is None and row.get('completion_tokens') is None:
        # A clean upstream refusal (HTTP error status, nothing streamed, no usage) cost nothing.
        row['cost_usd'] = row.get('cost_usd') or 0.0
        row['settled'] = 'refused'
        return row
    missing = (failed or row.get('usage_missing') is True or row.get('prompt_tokens') is None or row.get('completion_tokens') is None
               or row.get('cost_usd') is None)
    if missing:
        row['cost_usd'] = max(hold.reserved, row.get('cost_usd') or 0)
        row['usage_missing'] = True
        row['settled'] = 'reserved'
    else:
        row['settled'] = 'usage'
    return row


GENERATION_TOKENS_PER_SECOND = 300  # generous upper bound on generation speed


def cancelled_cost(row, hold, item, request_bytes, streamed_bytes, elapsed=0.0, usage_final=False):
    """Settle a stream the client cancelled.

    Provider-reported usage wins only when it is FINAL (the stream's closing usage).  Start-of-stream
    usage (e.g. Anthropic message_start) only sets the prompt count and a floor for output.  Otherwise charge an estimate of what was actually sent and
    streamed (one token per byte is an upper bound for text), never above the reservation; with
    no known price the full reservation stays charged.  Fail-closed accounting is unchanged.
    """
    if hold is None:
        return row
    if (usage_final and row.get('prompt_tokens') is not None and row.get('completion_tokens') is not None
            and row.get('cost_usd') is not None):
        row['settled'] = 'usage'
        return row
    # Hidden reasoning tokens are never streamed: bound output by elapsed time, within the reservation.
    output = min(hold.output_tokens, max(streamed_bytes, row.get('completion_tokens') or 0,
                                         int(max(0.0, elapsed) * GENERATION_TOKENS_PER_SECOND)))
    estimate = cost(item, row['prompt_tokens'] if row.get('prompt_tokens') is not None else request_bytes, output)
    if estimate is None or not math.isfinite(estimate):
        estimate = hold.reserved
    row['cost_usd'] = min(hold.reserved, max(estimate, row.get('cost_usd') or 0))
    row['usage_missing'] = True
    row['settled'] = 'estimated'
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
                    and (parent / 'engine').is_dir())):
                _secrets_refusal('the key file is inside a repository')
    except (OSError, RuntimeError, ValueError):
        raise CloudError('Cloud credentials are not configured.', 503, 'cloud_credentials_unavailable') from None
    return path


def _secrets_refusal(reason):
    """A 503 the caller may show; it says *why* the file was refused without the key or content."""
    raise CloudError(
        'Cloud credentials are not configured: %s.' % reason, 503, 'cloud_credentials_unavailable')


def _read_secrets(value):
    path = secrets_file(value)
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    except OSError as exc:
        if exc.errno == errno.ELOOP:
            reason = 'the configured path is a symbolic link'
            _secrets_refusal(reason)
        if exc.errno in (errno.EACCES, errno.EPERM):
            raise CloudError('Cloud credentials are not configured: the file cannot be read.',
                              503, 'cloud_credentials_unavailable') from None
        # Missing file, dangling symlink, or a non-final path component that is a link.
        raise CloudError('Cloud credentials are not configured.', 503, 'cloud_credentials_unavailable') from None
    try:
        info = os.fstat(fd)
        mode = info.st_mode
        if not stat.S_ISREG(mode):
            _secrets_refusal('the configured path is not a regular file')
        if info.st_uid != os.getuid():
            _secrets_refusal('the file is owned by another user')
        if mode & 0o077:
            _secrets_refusal('the file is not mode 0600 (group/other permissions must be off)')
        if info.st_size > MAX_SECRET_BYTES:
            _secrets_refusal('the file is larger than %d bytes' % MAX_SECRET_BYTES)
        data = os.read(fd, MAX_SECRET_BYTES + 1)
    except OSError:
        raise CloudError('Cloud credentials are not configured.', 503, 'cloud_credentials_unavailable') from None
    finally:
        os.close(fd)
    if len(data) > MAX_SECRET_BYTES:
        _secrets_refusal('the file is larger than %d bytes' % MAX_SECRET_BYTES)
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
        if provider == 'chatgpt':
            try:
                out[provider] = cloud_chatgpt.signed_in(secrets_file(value))
            except CloudError:
                out[provider] = False
            continue
        try:
            secret(provider, value)
            out[provider] = True
        except CloudError:
            out[provider] = False
    return out


def subscription_status(value):
    """The experimental subscription block for status: sign-in and allowance, never a token."""
    try:
        path = secrets_file(value)
    except CloudError:
        path = None
    return cloud_chatgpt.status(path)


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


def cache_pricing(item):
    """How cache tokens are priced: explicit catalog rates, provider default or conservative."""
    price = pricing(item) or {}
    levels = price['tiers'] if 'tiers' in price else [price]
    if all('cache_read_per_million' in level and 'cache_write_per_million' in level for level in levels):
        return 'explicit'
    return 'provider_default' if item['provider'] == 'anthropic' else 'conservative'


def capabilities(item):
    """The Wave D per-model capability block for a remote catalog entry."""
    return {'cache_pricing': cache_pricing(item), 'remote': True, 'remote_provider': item['provider'], 'echo': False,
            'cache_hints': False, 'cancel': item['provider'] not in ('openai', 'chatgpt'), 'prefill': False,
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
    if item['provider'] == 'chatgpt':
        return urllib.parse.urlsplit(item['base_url']).path.rstrip('/') + '/responses'
    if item['provider'] == 'openai':
        if __package__:
            from . import cloud_openai
        else:
            import cloud_openai
        return urllib.parse.urlsplit(item['base_url']).path.rstrip('/') + cloud_openai.endpoint(item)
    if item['provider'] == 'anthropic':
        return '/v1/messages'
    return urllib.parse.urlsplit(item['base_url']).path.rstrip('/') + '/chat/completions'


def connection(item, key, account=None):
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
    if item['provider'] == 'chatgpt':
        # source: docs/CHATGPT-SIGNIN.md (request headers row; S1 packages/core/src/plugin/provider/openai.ts L276-L280)
        headers.update({'chatgpt-account-id': account or '', 'OpenAI-Beta': 'responses=experimental',
                        'Accept': 'text/event-stream', 'originator': cloud_chatgpt.ORIGINATOR})
    return connection, path, headers


class Meter:
    """Usage/finish numbers from an upstream response; no prompt or reply text is kept."""

    def __init__(self):
        self.prompt = None
        self.completion = None
        self.cached = None
        self.cache_written = None
        self.cache_invalid = False
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
            details = usage.get('prompt_tokens_details')
            details = details if isinstance(details, dict) else {}
            for source, target in ((usage, 'cached'), (details, 'cached'),
                                   (usage, 'cache_written'), (details, 'cache_written')):
                key = ('cache_read_input_tokens' if source is usage else 'cached_tokens') if target == 'cached' else (
                    'cache_creation_input_tokens' if source is usage else 'cache_write_tokens')
                if key in source:
                    number = source[key]
                    if type(number) is int and number >= 0:
                        setattr(self, target, number)
                    else:
                        self.cache_invalid = True
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
        if item.get('cost_class') == 'subscription':
            out['cost_class'] = 'subscription'
        if prompt is not None:
            out['prompt_tokens'] = prompt
        if completion is not None:
            out['completion_tokens'] = completion
        if self.cached is not None:
            out['cached_tokens'] = self.cached
        if self.cache_written is not None:
            out['cache_write_tokens'] = self.cache_written
        if self.finish is not None:
            out['finish_reason'] = redact(self.finish, secret)
        invalid_cache = (self.cache_invalid or (prompt is not None
                         and (self.cached or 0) + (self.cache_written or 0) > prompt))
        if invalid_cache:
            out['usage_missing'] = True
        usd = None if invalid_cache else cost(
            item, prompt, completion, cache_read=self.cached or 0, cache_write=self.cache_written or 0)
        if usd is not None:
            out['cost_usd'] = usd
            out['catalog_cost_usd'] = usd
        if item['provider'] == 'openrouter' and self.reported_cost is not None:
            out['reported_cost_usd'] = self.reported_cost
            # The bill; catalog_cost_usd keeps the estimate.  A zero bill for a request that used
            # tokens is not believed: the catalog estimate stays the charge then.
            if self.reported_cost > 0 or not usd:
                out['cost_usd'] = self.reported_cost
        return out


# ---------------------------------------------------------------- D3 normalisation
REASONING_EFFORT = {'none': 'none', 'minimal': 'none', 'low': 'low', 'medium': 'medium',
                    'high': 'high', 'xhigh': 'high'}


def upstream_payload(payload, item, key=None, scope=None):
    """The normalised upstream body: the remote catalog swap plus the OpenAI v1 mapping.

    Local-only fields (``chat_template_kwargs``, cache/slot controls) are removed; an OpenAI
    ``reasoning_effort`` becomes the upstream effort (``none``/``minimal`` = off, ``xhigh`` =
    ``high``); a local ``enable_thinking`` switch is mapped the same way.  A stream asks for
    the final usage chunk so the ledger and budget see the provider's own numbers.
    """
    if item['provider'] in ('openai', 'chatgpt'):
        if __package__:
            from . import cloud_openai
        else:
            import cloud_openai
        body = cloud_openai.request(payload, item, key, scope)
        return cloud_chatgpt.adapt(body) if item['provider'] == 'chatgpt' else body
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
        self.names = {}  # (choice, slot) -> name pieces held until the call is complete
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

    def _tails(self, indices=None):
        out = b''
        for (index, slot), name in list(self.names.items()):
            if indices is not None and index not in indices:
                continue
            del self.names[(index, slot)]
            value = {'model': self.model, 'object': 'chat.completion.chunk',
                     'choices': [{'index': index, 'delta': {'tool_calls': [
                         {'index': slot, 'function': {'name': redact_value(name, self.key)}}]},
                         'finish_reason': None}]}
            out += self._wire(None, json.dumps(value, ensure_ascii=False).encode())
        for (index, field, slot), redactor in self.fields.items():
            if indices is not None and index not in indices:
                continue
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
                        # A tool name is held whole per call (redacting pieces would split `ls`
                        # into `l`,`s`; passing pieces would leak a key split across deltas) and
                        # sent redacted once the call has arguments, or at finish / [DONE].
                        slot = (index, call.get('index', 0))
                        if isinstance(function.get('name'), str):
                            self.names[slot] = self.names.get(slot, '') + function.pop('name')
                        if isinstance(function.get('arguments'), str):
                            # _normalize_calls fills a missing arguments with '': only real
                            # argument text proves the name is complete.
                            if function['arguments'] and slot in self.names:
                                function['name'] = redact_value(self.names.pop(slot), self.key)
                            function['arguments'] = self._fragment(index, 'arguments', function['arguments'],
                                                                   call.get('index', 0))
            _normalize_message(choice.get('message'))
        if isinstance(value.get('usage'), dict):
            try:
                from .openai_conformance import complete_usage
            except ImportError:
                from openai_conformance import complete_usage
            value['usage'] = complete_usage(value['usage'], self.reasoning, value.get('timings'))
        if self.key:
            value = redact_value(value, self.key)
        finished = [choice for choice in value.get('choices') or ()
                    if isinstance(choice, dict) and choice.get('finish_reason') is not None]
        tails = self._tails({choice.get('index', 0) for choice in finished}) if finished else b''
        if tails:
            # A final delta can itself leave a key-prefix suffix buffered. Preserve
            # current delta -> suffix -> finish, without flushing unfinished siblings.
            ending = {**value, 'choices': [
                {'index': choice.get('index', 0), 'delta': {},
                 'finish_reason': choice['finish_reason']} for choice in finished]}
            for choice in finished:
                choice['finish_reason'] = None
            value.pop('usage', None)  # Usage belongs to the single final notification.
            return (self._wire(name, json.dumps(value, ensure_ascii=False).encode()) + tails
                    + self._wire(name, json.dumps(ending, ensure_ascii=False).encode()))
        return self._wire(name, json.dumps(value, ensure_ascii=False).encode())

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
