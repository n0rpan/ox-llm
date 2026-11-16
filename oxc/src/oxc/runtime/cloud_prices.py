"""Public OpenRouter price metadata: no credentials, bounded replies, durable last-known values."""
import http.client
import json
import os
from pathlib import Path
import stat
import tempfile
import threading
import time
import urllib.parse

URL = 'https://openrouter.ai/api/v1/models'
MAX_BYTES = 4 * 1024 * 1024
DAY_SECONDS = 24 * 60 * 60
POLL_SECONDS = 60
_lock = threading.RLock()
_snapshots = {}


def cache_path():
    state = Path(os.environ.get('XDG_STATE_HOME') or Path.home() / '.local/state')
    return state / 'oxc' / 'openrouter-prices.json'


def _rates(raw):
    if not isinstance(raw, dict):
        return None
    result = {}
    for source, target in (('prompt', 'input_per_million'), ('completion', 'output_per_million'),
                           ('input_cache_read', 'cache_read_per_million'),
                           ('input_cache_write', 'cache_write_per_million')):
        value = raw.get(source)
        if value is None and source.startswith('input_cache_'):
            continue
        if type(value) not in (str, int, float):
            return None
        try:
            value = float(value)
        except (ValueError, OverflowError):
            return None
        if not 0 <= value <= 1e6:
            return None
        result[target] = value * 1_000_000
    return result


def _read(path):
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
        with os.fdopen(fd, 'rb') as stream:
            info = os.fstat(stream.fileno())
            if (not stat.S_ISREG(info.st_mode) or info.st_size > MAX_BYTES
                    or info.st_uid != os.getuid() or info.st_mode & 0o022):
                return {}
            value = json.loads(stream.read(MAX_BYTES + 1))
        rows = value.get('data') if isinstance(value, dict) else None
        if not isinstance(rows, list) or len(rows) > 20_000:
            return {}
        return {row['id']: row for row in rows if isinstance(row, dict)
                and isinstance(row.get('id'), str) and len(row['id']) <= 128
                and _rates(row.get('pricing')) is not None}
    except (OSError, ValueError, RecursionError):
        return {}


def _snapshot(path):
    with _lock:
        key = str(path)
        if key not in _snapshots:
            _snapshots[key] = _read(path)
        return _snapshots[key]


def price_for(model, path=None):
    row = _snapshot(path or cache_path()).get(model)
    return _rates(row.get('pricing')) if row else None


def _fetch(url):
    parts = urllib.parse.urlsplit(url)
    connection_type = http.client.HTTPSConnection if parts.scheme == 'https' else http.client.HTTPConnection
    connection = connection_type(parts.hostname, parts.port, timeout=10)
    try:
        # No auth, cookies, proxy credentials, redirects or caller-controlled headers.
        connection.request('GET', parts.path, headers={'Accept': 'application/json'})
        reply = connection.getresponse()
        if reply.status != 200:
            return None
        body = reply.read(MAX_BYTES + 1)
        return json.loads(body) if len(body) <= MAX_BYTES else None
    finally:
        connection.close()


def refresh(path=None, *, url=URL):
    path = Path(path or cache_path())
    try:
        value = _fetch(url)
        rows = value.get('data') if isinstance(value, dict) else None
        if not isinstance(rows, list) or len(rows) > 20_000:
            return False
        updates = {row['id']: {'id': row['id'], 'pricing': {
                       key: row['pricing'][key] for key in
                       ('prompt', 'completion', 'input_cache_read', 'input_cache_write')
                       if key in row['pricing']}}
                   for row in rows if isinstance(row, dict)
                   and isinstance(row.get('id'), str) and 0 < len(row['id']) <= 128
                   and _rates(row.get('pricing')) is not None}
        if not updates:
            return False
        with _lock:
            merged = {**_snapshot(path), **updates}
            body = json.dumps({'data': list(merged.values())}, allow_nan=False).encode()
            if len(body) > MAX_BYTES:
                return False
            path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            fd, temporary = tempfile.mkstemp(prefix='.prices-', dir=path.parent)
            try:
                with os.fdopen(fd, 'wb') as stream:
                    stream.write(body)
                    stream.flush()
                    os.fsync(stream.fileno())
                os.replace(temporary, path)
            finally:
                if os.path.exists(temporary):
                    os.unlink(temporary)
            _snapshots[str(path)] = merged
        return True
    except (OSError, ValueError, RecursionError, http.client.HTTPException):
        return False  # Keep last-known prices; unknown models still refuse admission.


def overlay(configured, model):
    current = price_for(model)
    if current is None:
        return configured
    if configured is None:
        return current
    def floor(tier, base):
        out = dict(tier)
        for field, rate in current.items():
            # The live list price is the short-context rate; a higher configured tier is never
            # lowered by it, so long prompts are still reserved at their own rate.
            if base:
                # A zero live price never lowers a configured rate (it would disable the budget cap).
                out[field] = rate if rate > 0 else out.get(field, rate)
            else:
                out[field] = max(out.get(field, rate), rate)
        return out
    if 'tiers' in configured:
        tiers = []
        for index, tier in enumerate(configured['tiers']):
            tier = floor(tier, index == 0)
            for field, rate in (tiers[-1].items() if tiers else ()):
                if field.endswith('_per_million'):  # longer prompts never cost less per token
                    tier[field] = max(tier.get(field, rate), rate)
            tiers.append(tier)
        return {'tiers': tiers}
    return floor(configured, True)


class Refresher:
    def __init__(self, allowed):
        self.allowed = allowed
        self.stop = threading.Event()
        self.thread = threading.Thread(target=self._run, name='public-model-prices', daemon=True)
        self.thread.start()

    RETRY_SECONDS = (60, 300, 1800, 3600)  # after a failure; hourly from then on

    def step(self, now, state):
        """One scheduling decision.  ``state`` = [next_refresh, failures]; returns nothing."""
        if not self.allowed() or now < state[0]:
            return
        if refresh():
            state[:] = [now + DAY_SECONDS, 0]
        else:
            delay = self.RETRY_SECONDS[min(state[1], len(self.RETRY_SECONDS) - 1)]
            state[:] = [now + delay, state[1] + 1]

    def _run(self):
        state = [0, 0]
        while not self.stop.is_set():
            self.step(time.monotonic(), state)
            if self.stop.wait(min(POLL_SECONDS, DAY_SECONDS)):
                break

    def close(self):
        self.stop.set()
        self.thread.join(timeout=11)
