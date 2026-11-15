"""Daily health digest (OEM-DONE §2(d)): numbers only, from the organic request-ledger rows.

Reads OXC_LEDGER_DIR/requests-YYYYMMDD.jsonl (request_ledger.py) and the engine diagnostic log's
`engine_died` events, keeps every row whose `origin` is not 'lab'/'gate' (organic and LAN-share
'remote' traffic is real use), and reduces them to counts, rates, percentiles and allowlisted enum
labels. Prompt text, bodies, session ids, hashes of content and paths never enter a digest: every
output value is built here from numbers or from a fixed enum set (see `_enum`).

Files (owner-only, $OXC_DIGEST_DIR, default $XDG_STATE_HOME/oxc/digest, i.e.
~/.local/state/oxc/digest):
  digest-YYYYMMDD.json   one day's digest (schema local-ai-digest/1)
  status.json            the widget's one status line + soak summary (schema local-ai-digest-status/1)

Alert thresholds are OEM-DONE §2(d) [estimates; recalibrate after 7 days]. Soak pass: 7 consecutive
green days (idle days, with no organic request, neither count nor break the run), >= 300 organic
requests and >= 10 history rewrites over those 7 days; 3 green days is the ship bar.

Client-neutral (V014-REPLAN O-133/O-135): only generic, structural ledger labels are read
(`history_change`, `history_rewrite`, `fold_suspect`, `rewrite_class`, `replay`, `queue_wait_ms`,
the client's own H5 `turn_kind`); no client's wording or swallow label is known here.

Installed versions (v0.2.0): `installed_versions()` reads the version line of ox-llm's own binaries
(today: the coding agent at ~/.local/bin/oxa, `--version`, 5 s cap); only a version-shaped token is
kept (else 'unknown'; 'absent' when not installed). The CLI passes it for today's and yesterday's digest.
"""
import datetime as _dt
import json
import math
import os
from pathlib import Path
import re
import stat
import subprocess
import tempfile
import time

SCHEMA = 'local-ai-digest/1'
STATUS_SCHEMA = 'local-ai-digest-status/1'
EXCLUDED_ORIGINS = frozenset({'lab', 'gate'})
ORIGINS = ('organic', 'remote')
MAX_FILE_BYTES = 256 * 1024 * 1024
MAX_LINE_BYTES = 1024 * 1024
STALE_SECONDS = 36 * 3600

# Depth bands by context tokens at the request (cached + new).
BANDS = ((0, 8192, '0-8k'), (8192, 32768, '8-32k'), (32768, 65536, '32-64k'),
         (65536, 98304, '64-96k'), (98304, float('inf'), '96k+'))
BAND_NAMES = tuple(b[2] for b in BANDS)
MIN_BAND_SAMPLES = 5      # a band's p50 joins the trend check only with this many samples
MIN_TRAILING_DAYS = 3     # and only against at least this many earlier days

# Thresholds (OEM-DONE §2(d)).
UNKNOWN_MISS_RATE = 0.01   # unknown cache misses > 1% of eligible turns
PREFIX_HIT_FLOOR = 0.90    # prefix hits < 90% of eligible turns
RESTORE_P95_MS = 5000.0    # restore time p95 > 5 s
TOKS_DROP = 0.10           # decode / append-prefill p50 per band 10% below the trailing 7-day median
TTFT_RISE = 1.5            # TTFT p95 on cache-hit turns, H5 compaction wall median: 1.5x trailing median
STORE_FULL = 0.90          # store > 90% of budget
TRAILING_DAYS = 7
SOAK_DAYS, SOAK_REQUESTS, SOAK_REWRITES, SHIP_DAYS = 7, 300, 10, 3
SOAK_LOOKBACK = 30

# Enum allowlists (anything else becomes 'other').
ERRORS = ('backend_died', 'client_disconnected', 'start_failed', 'model_busy', 'busy', 'unavailable',
          'bad_request', 'local_controller_error', 'preempted')
# 'compaction' is the pre-v0.1.4 name of 'history-rewrite' (old ledger rows).
MISS_CLASSES = ('append-ok', 'retry', 'history-edit', 'reasoning-drop', 'tool-rerender', 'history-rewrite',
                'compaction', 'template-drift', 'unknown')
REWRITE_CLASSES = ('identical', 'ws-trailing', 'ws-inner', 'args-reserialised', 'reasoning-dropped',
                   'id-changed', 'other')
REPLAYS = ('cut-served', 'cut-done', 'cut-aborted')
REWRITE_CHANGES = ('edited', 'truncated')  # ledger history_change values that are a history rewrite
COMPLETENESS = ('complete', 'closed-after-done', 'incomplete', 'errored')
GUARD_GROUPS = {
    'oom': ('cgroup_oom', 'host_oom_kill'),
    'psi': ('host_memory_pressure_sustained', 'host_memory_pressure_full_sustained'),
    'memory_floor': ('host_available_below_floor', 'host_available_emergency', 'host_memory_and_swap_exhausted'),
    'swap': ('own_swap_nonzero', 'own_swap_limit_not_zero'),
    'pids': ('cgroup_pids_limit',),
}
_GUARD_OF = {reason: group for group, reasons in GUARD_GROUPS.items() for reason in reasons}
_REASON = re.compile(r'[a-z][a-z0-9_]{0,47}')
# Latency/throughput series: one per model alias (and context mode when the row has one), so a model
# or mode switch never reads as an engine regression. Aliases are local profile names; anything else
# becomes 'other' (and a row without a model 'unknown').
_ALIAS = re.compile(r'[a-z0-9][a-z0-9._-]{0,63}')
MODES = ('fast', 'deep', 'max')


def series_of(row):
    model = row.get('model')
    model = 'unknown' if model is None else model if isinstance(model, str) and _ALIAS.fullmatch(model) else 'other'
    mode = row.get('mode')
    return model + '/' + mode if mode in MODES else model


def _new_series():
    return {'requests': 0, 'ttft_hit': [], 'compaction_wall': [],
            'prefill': {b: [] for b in BAND_NAMES}, 'decode': {b: [] for b in BAND_NAMES}}


# --------------------------------------------------------------------------- paths
def _xdg_state():
    value = os.environ.get('XDG_STATE_HOME') or ''
    return Path(value) if os.path.isabs(value) else Path.home() / '.local/state'


def digest_dir():
    return Path(os.environ.get('OXC_DIGEST_DIR') or _xdg_state() / 'oxc/digest')


def ledger_dir():
    # Same default as request_ledger.ledger_dir (it is not XDG-relative).
    return Path(os.environ.get('OXC_LEDGER_DIR', str(Path.home() / '.local/state/oxc/ledger')))


def engine_log_path():
    return Path(os.environ.get('OXC_ENGINE_LOG') or _xdg_state() / 'oxc/engine-stderr.log')


# --------------------------------------------------------------------------- small helpers
def _num(v):
    return v if type(v) in (int, float) and math.isfinite(v) else None


def _enum(value, allowed):
    if value is None:
        return None
    return value if isinstance(value, str) and value in allowed else 'other'


def percentile(values, q):
    """Nearest-rank percentile (q in 0..100) of finite numbers, rounded; None when empty."""
    values = sorted(v for v in values if _num(v) is not None)
    if not values:
        return None
    rank = max(1, math.ceil(q / 100 * len(values)))
    return round(values[rank - 1], 2)


def median(values):
    values = sorted(v for v in values if _num(v) is not None)
    if not values:
        return None
    mid = len(values) // 2
    return round(values[mid] if len(values) % 2 else (values[mid - 1] + values[mid]) / 2, 2)


def band_of(ctx):
    for low, high, name in BANDS:
        if low <= ctx < high:
            return name
    return None


def parse_day(text, today=None):
    today = today or _dt.date.today()
    if text in (None, '', 'today'):
        return today
    if text == 'yesterday':
        return today - _dt.timedelta(days=1)
    try:
        return _dt.date.fromisoformat(text)
    except (TypeError, ValueError):
        raise ValueError('Use a day like 2026-11-13, today or yesterday.') from None


def _read_lines(path):
    """Lines of one private regular file (no symlinks), bounded; [] when absent or unreadable."""
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    except OSError:
        return []
    with os.fdopen(fd, 'rb') as handle:
        info = os.fstat(handle.fileno())
        if not stat.S_ISREG(info.st_mode):
            return []
        data = handle.read(MAX_FILE_BYTES)
    return [line for line in data.splitlines() if 0 < len(line) <= MAX_LINE_BYTES]


def ledger_rows(day, directory=None):
    rows = []
    for line in _read_lines((directory or ledger_dir()) / f'requests-{day:%Y%m%d}.jsonl'):
        try:
            row = json.loads(line)
        except (ValueError, RecursionError):
            continue
        if isinstance(row, dict):
            rows.append(row)
    return rows


def engine_events(day, path=None):
    """`engine_died` events of `day` (local time) that carry a timestamp `t`; plus the number of
    undated ones in the file (older controllers wrote none), so a reader knows they exist."""
    start = time.mktime(day.timetuple())
    end = start + 86400
    events, undated = [], 0
    for line in _read_lines(path or engine_log_path()):
        if not line.startswith(b'{'):
            continue
        try:
            event = json.loads(line)
        except (ValueError, RecursionError):
            continue
        if not isinstance(event, dict) or event.get('event') != 'engine_died':
            continue
        t = _num(event.get('t'))
        if t is None:
            undated += 1
        elif start <= t < end:
            events.append(event)
    return events, undated


# --------------------------------------------------------------------------- the digest
def _rate(part, whole):
    return round(part / whole, 4) if whole else None


def _toks(count, ms):
    count, ms = _num(count), _num(ms)
    return count * 1000.0 / ms if count is not None and ms and ms > 0 and count >= 8 else None


def compute(day, rows, events=(), undated_events=0, store=None, generated_at=None, versions=None):
    """The day's digest from raw ledger rows (any origin; filtered here) and engine events."""
    by_origin = {}
    kept = []
    for row in rows:
        origin = row.get('origin') if isinstance(row.get('origin'), str) else 'organic'
        if origin in EXCLUDED_ORIGINS:
            by_origin['excluded'] = by_origin.get('excluded', 0) + 1
            continue
        key = origin if origin in ORIGINS else 'other'
        by_origin[key] = by_origin.get(key, 0) + 1
        kept.append(row)
    kept.sort(key=lambda r: _num(r.get('t')) or 0)

    status_counts = {'2xx': 0, '4xx': 0, '5xx': 0, 'other': 0}
    errors, completeness, miss, rewrite_classes, replays = {}, {}, {}, {}, {}
    retries = duplicates = eligible = prefix_hits = c3_miss = unknown = 0
    cached_sum = ctx_sum = 0
    rewrites = fold_suspects = client_compactions = queued = session_switches = 0
    stream_cut = backend_died = 0
    ttft, ttft_hit, restore_ms, compaction_wall, queue_wait = [], [], [], [], []
    series = {}

    for row in kept:
        ser = series.setdefault(series_of(row), _new_series())
        ser['requests'] += 1
        status = _num(row.get('status'))
        bucket = ('2xx' if status is not None and 200 <= status < 300 else '4xx' if status is not None and 400 <= status < 500
                  else '5xx' if status is not None and 500 <= status < 600 else 'other')
        status_counts[bucket] += 1
        error = _enum(row.get('error'), ERRORS)
        if error is not None:
            errors[error] = errors.get(error, 0) + 1
            backend_died += error == 'backend_died'
        done = _enum(row.get('completeness'), COMPLETENESS)
        if done is not None:
            completeness[done] = completeness.get(done, 0) + 1
            if done == 'incomplete' and error != 'client_disconnected':
                stream_cut += 1
        cls = _enum(row.get('miss_class'), MISS_CLASSES)
        if cls is not None:
            miss[cls] = miss.get(cls, 0) + 1
            retries += cls == 'retry'
            unknown += cls == 'unknown'
        duplicates += row.get('history_change') == 'same'
        cache, ctx = _num(row.get('cached_tokens', row.get('cache_n'))), _num(row.get('ctx_tokens'))
        if ctx is None and cache is not None and _num(row.get('prompt_n')) is not None:
            ctx = cache + row['prompt_n']
        if cache is not None and ctx:
            cached_sum += cache
            ctx_sum += ctx
        # C3 eligible: an append turn whose previous reply came back unchanged.
        if row.get('history_change') == 'append' and row.get('previous_reply') == 'present':
            eligible += 1
            hit = cls in (None, 'append-ok') and (cache or 0) > 0
            prefix_hits += hit
            c3_miss += cls not in (None, 'append-ok')
        # Structural labels only (core diagnostics): a history that is not an append of the session's
        # previous one, a suspected client-side fold, and how the previous reply came back.
        rewrites += row.get('history_rewrite') is True or row.get('history_change') in REWRITE_CHANGES
        fold_suspects += row.get('fold_suspect') is True
        rewrite = _enum(row.get('rewrite_class'), REWRITE_CLASSES)
        if rewrite is not None:
            rewrite_classes[rewrite] = rewrite_classes.get(rewrite, 0) + 1
        replay = _enum(row.get('replay'), REPLAYS)
        if replay is not None:
            replays[replay] = replays.get(replay, 0) + 1
        if row.get('turn_kind') == 'compaction':  # the client's own H5 label, never guessed
            client_compactions += 1
            if _num(row.get('wall_ms')) is not None:
                compaction_wall.append(row['wall_ms'])
                ser['compaction_wall'].append(row['wall_ms'])
        wait = _num(row.get('queue_wait_ms'))
        if wait is not None:
            queue_wait.append(wait)
        queued += _num(row.get('queue_pos')) is not None
        session_switches += row.get('session_switch') is True
        answered = status is not None and 200 <= status < 300
        if answered and _num(row.get('ttfb_ms')) is not None:
            ttft.append(row['ttfb_ms'])
            if (cache or 0) > 0 and cls in (None, 'append-ok'):
                ttft_hit.append(row['ttfb_ms'])
                ser['ttft_hit'].append(row['ttfb_ms'])
        if _num(row.get('resume_ms')) is not None and row.get('resume_src') in ('ram', 'disk'):
            restore_ms.append(row['resume_ms'])
        if answered and ctx:
            band = band_of(ctx)
            p = _toks(row.get('prompt_n'), row.get('prompt_ms'))
            if p is not None and (cache or 0) > 0:  # append prefill: the new tokens after a reused prefix
                ser['prefill'][band].append(p)
            d = _toks(row.get('predicted_n'), row.get('predicted_ms'))
            if d is not None:
                ser['decode'][band].append(d)

    guard = {group: 0 for group in GUARD_GROUPS}
    guard['other'] = 0
    deaths = 0
    for event in events:
        deaths += 1
        reason = event.get('guard_failure')
        if isinstance(reason, str) and reason:
            reason = reason if _REASON.fullmatch(reason) else 'other'
            guard[_GUARD_OF.get(reason, 'other')] += 1
    guard_trips = sum(guard.values())

    requests = len(kept)
    digest = {
        'schema': SCHEMA,
        'day': day.isoformat(),
        'generated_at': int(generated_at if generated_at is not None else time.time()),
        'partial': day >= _dt.date.today(),
        'requests': requests,
        'by_origin': by_origin,
        'status': status_counts,
        'retries': retries,
        'duplicate_bodies': duplicates,
        'cache': {
            'token_hit_rate': _rate(cached_sum, ctx_sum),
            'eligible_turns': eligible,
            'prefix_hits': prefix_hits,
            'prefix_hit_rate': _rate(prefix_hits, eligible),
            'c3_misses': c3_miss,
            'unknown_misses': unknown,
            'unknown_miss_rate': _rate(unknown, eligible),
            'miss_classes': miss,
        },
        'history': {
            'history_rewrites': rewrites,
            'fold_suspects': fold_suspects,
            'rewrite_classes': rewrite_classes,
            'client_compactions': client_compactions,
            'compaction_wall_ms_median': median(compaction_wall),
        },
        'queue': {
            'queued': queued,
            'session_switches': session_switches,
            'wait_ms': {'p50': percentile(queue_wait, 50), 'p95': percentile(queue_wait, 95), 'n': len(queue_wait)},
            'replays': replays,
        },
        'guard': {'trips': guard_trips, 'by_group': guard, 'engine_deaths': deaths,
                  'undated_engine_events': undated_events},
        'latency': {
            'ttft_ms': {'p50': percentile(ttft, 50), 'p95': percentile(ttft, 95), 'n': len(ttft)},
            'ttft_hit_ms': {'p50': percentile(ttft_hit, 50), 'p95': percentile(ttft_hit, 95), 'n': len(ttft_hit)},
            'restore_ms': {'p50': percentile(restore_ms, 50), 'p95': percentile(restore_ms, 95), 'n': len(restore_ms)},
        },
        # Per model (and mode): the only place throughput lives, and the only basis of trend alerts.
        'series': {key: {
            'requests': v['requests'],
            'ttft_hit_ms': {'p50': percentile(v['ttft_hit'], 50), 'p95': percentile(v['ttft_hit'], 95), 'n': len(v['ttft_hit'])},
            'append_prefill_tok_s': {b: {'p50': percentile(x, 50), 'n': len(x)} for b, x in v['prefill'].items()},
            'decode_tok_s': {b: {'p50': percentile(x, 50), 'n': len(x)} for b, x in v['decode'].items()},
            'compaction_wall_ms_median': median(v['compaction_wall']),
        } for key, v in sorted(series.items())},
        'errors': {'by_label': errors, 'http_5xx': status_counts['5xx'], 'stream_cut': stream_cut,
                   'backend_died': backend_died, 'completeness': completeness},
        'store': _store(store),
        'versions': _versions(versions),
        'alerts': [],
        'trend_basis_days': 0,
    }
    return digest


VERSION = re.compile(r'\b(\d{1,4}\.\d{1,4}\.\d{1,6}(?:-[0-9A-Za-z.]{1,32})?)\b')
AGENT_BINARY = '.local/bin/oxa'  # ox-llm's own coding agent (installed beside oxc)


def installed_versions(home=None, run=subprocess.run, timeout=5):
    """{'oxa': version | 'unknown' | 'absent'}: never raises, never more than one short child."""
    path = Path(home or Path.home()) / AGENT_BINARY
    if not path.is_file():
        return {'oxa': 'absent'}
    try:
        done = run([str(path), '--version'], capture_output=True, text=True, timeout=timeout,
                   stdin=subprocess.DEVNULL, check=False)
        match = VERSION.search((done.stdout or '') + ' ' + (done.stderr or '')) if done.returncode == 0 else None
    except (OSError, subprocess.SubprocessError, ValueError):
        match = None
    return {'oxa': match.group(1) if match else 'unknown'}


def _versions(versions):
    if not isinstance(versions, dict):
        return None
    out = {}
    for key in ('oxa',):
        value = versions.get(key)
        out[key] = value if isinstance(value, str) and (value in ('absent', 'unknown') or VERSION.fullmatch(value)) \
            else 'unknown'
    return out


def _store(store):
    if not isinstance(store, dict):
        return None
    used, budget = _num(store.get('used_bytes')), _num(store.get('budget_bytes'))
    out = {'used_bytes': used, 'budget_bytes': budget,
           'used_fraction': round(used / budget, 4) if used is not None and budget else None}
    young = _num(store.get('young_evictions'))
    if young is not None:
        out['young_evictions'] = young
    return out


def _alert(alerts, alert_id, value, threshold):
    alerts.append({'id': alert_id, 'value': value, 'threshold': threshold})


def apply_alerts(digest, trailing=()):
    """Fill digest['alerts'] (OEM-DONE §2(d)). `trailing`: earlier days' digests (up to 7)."""
    alerts = []
    errors, guard, cache = digest['errors'], digest['guard'], digest['cache']
    for alert_id, value in (('client_retries', digest['retries']), ('duplicate_prefill', digest['duplicate_bodies']),
                            ('http_5xx', errors['http_5xx']), ('stream_cut', errors['stream_cut']),
                            ('engine_death', max(errors['backend_died'], guard['engine_deaths'] - guard['trips'])),
                            ('guard_trip', guard['trips'] - guard['by_group']['oom'] - guard['by_group']['swap']),
                            ('scope_oom', guard['by_group']['oom']), ('swap', guard['by_group']['swap'])):
        if value > 0:
            _alert(alerts, alert_id, value, 0)
    if cache['eligible_turns']:
        if cache['unknown_miss_rate'] is not None and cache['unknown_miss_rate'] > UNKNOWN_MISS_RATE:
            _alert(alerts, 'unknown_cache_misses', cache['unknown_miss_rate'], UNKNOWN_MISS_RATE)
        if cache['prefix_hit_rate'] is not None and cache['prefix_hit_rate'] < PREFIX_HIT_FLOOR:
            _alert(alerts, 'prefix_hits_low', cache['prefix_hit_rate'], PREFIX_HIT_FLOOR)
    p95 = digest['latency']['restore_ms']['p95']
    if p95 is not None and p95 > RESTORE_P95_MS:
        _alert(alerts, 'restore_slow', p95, RESTORE_P95_MS)
    store = digest.get('store') or {}
    if store.get('used_fraction') is not None and store['used_fraction'] > STORE_FULL:
        _alert(alerts, 'store_full', store['used_fraction'], STORE_FULL)
    if (store.get('young_evictions') or 0) > 0:
        _alert(alerts, 'store_young_eviction', store['young_evictions'], 0)

    trailing = [d for d in trailing if isinstance(d, dict) and d.get('schema') == SCHEMA][:TRAILING_DAYS]
    digest['trend_basis_days'] = len(trailing)
    # Trends compare a series only with the same series (model alias, mode) on earlier days.
    for key, cur in (digest.get('series') or {}).items():
        before = [((d.get('series') or {}).get(key) or {}) for d in trailing]
        for metric, alert_id in (('decode_tok_s', 'decode_slower'), ('append_prefill_tok_s', 'prefill_slower')):
            for band in BAND_NAMES:
                today = cur[metric][band]
                if today['p50'] is None or today['n'] < MIN_BAND_SAMPLES:
                    continue
                past = [(b.get(metric) or {}).get(band) or {} for b in before]
                past = [p['p50'] for p in past if _num(p.get('p50')) is not None and (p.get('n') or 0) >= MIN_BAND_SAMPLES]
                base = median(past) if len(past) >= MIN_TRAILING_DAYS else None
                if base and today['p50'] < (1 - TOKS_DROP) * base:
                    _alert(alerts, f'{alert_id}:{key}:{band}', today['p50'], round((1 - TOKS_DROP) * base, 2))
        for value, past_values, alert_id in (
                (cur['ttft_hit_ms']['p95'], [(b.get('ttft_hit_ms') or {}).get('p95') for b in before], 'ttft_hit_slower'),
                (cur['compaction_wall_ms_median'], [b.get('compaction_wall_ms_median') for b in before],
                 'compaction_slower')):
            past = [v for v in past_values if _num(v) is not None]
            base = median(past) if len(past) >= MIN_TRAILING_DAYS else None
            if value is not None and base and value > TTFT_RISE * base:
                _alert(alerts, f'{alert_id}:{key}', value, round(TTFT_RISE * base, 2))
    digest['alerts'] = alerts
    digest['green'] = not alerts
    digest['idle'] = digest['requests'] == 0
    return digest


# --------------------------------------------------------------------------- storage
def _write_private(path, value):
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    os.chmod(path.parent, 0o700)
    fd, tmp = tempfile.mkstemp(prefix='.' + path.name + '.', dir=path.parent)
    try:
        with os.fdopen(fd, 'w') as stream:
            stream.write(json.dumps(value, indent=1, sort_keys=True) + '\n')
            stream.flush()
            os.fsync(stream.fileno())
        os.chmod(tmp, 0o600)
        os.replace(tmp, path)
    except BaseException:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise


def stored(day, directory=None):
    lines = _read_lines((directory or digest_dir()) / f'digest-{day:%Y%m%d}.json')
    try:
        value = json.loads(b'\n'.join(lines)) if lines else None
    except (ValueError, RecursionError):
        return None
    return value if isinstance(value, dict) and value.get('schema') == SCHEMA else None


class Digester:
    """Computes days on demand (stored complete days first, else from the ledger) with a cache."""

    def __init__(self, ledger=None, engine_log=None, directory=None, store=None, today=None, versions=None):
        self.ledger, self.engine_log, self.directory = ledger, engine_log, directory
        self.store, self.today = store, today or _dt.date.today()
        self.versions = versions
        self.cache = {}

    def raw(self, day):
        if day not in self.cache:
            value = stored(day, self.directory) if day < self.today else None
            if value is None:
                events, undated = engine_events(day, self.engine_log)
                recent = day >= self.today - _dt.timedelta(days=1)
                value = compute(day, ledger_rows(day, self.ledger), events, undated,
                                store=self.store if recent else None, versions=self.versions if recent else None)
                value['partial'] = day >= self.today
                apply_alerts(value, self.trailing(day))
            self.cache[day] = value
        return self.cache[day]

    def trailing(self, day):
        out = []
        for back in range(1, TRAILING_DAYS + 1):
            other = day - _dt.timedelta(days=back)
            value = stored(other, self.directory)
            if value is None:
                events, undated = engine_events(other, self.engine_log)
                rows = ledger_rows(other, self.ledger)
                if not rows:
                    continue
                value = compute(other, rows, events, undated)
            if value.get('requests'):
                out.append(value)
        return out

    def day(self, day):
        return self.raw(day)


def soak(digester, last_day):
    """Walk back from `last_day`: idle days are skipped, a day with an alert ends the run."""
    green, window = 0, []
    for back in range(SOAK_LOOKBACK):
        day = last_day - _dt.timedelta(days=back)
        value = digester.day(day)
        if value.get('requests', 0) == 0:
            continue
        if value.get('alerts'):
            break
        green += 1
        window.append(value)
        if green >= SOAK_DAYS:
            break
    requests = sum(v['requests'] for v in window)
    rewrites = sum(v['history']['history_rewrites'] for v in window)
    return {'green_days': green, 'days': [v['day'] for v in window], 'requests': requests,
            'history_rewrites': rewrites, 'ship_ready': green >= SHIP_DAYS,
            'pass': green >= SOAK_DAYS and requests >= SOAK_REQUESTS and rewrites >= SOAK_REWRITES,
            'needs': {'green_days': SOAK_DAYS, 'requests': SOAK_REQUESTS, 'history_rewrites': SOAK_REWRITES,
                      'ship_green_days': SHIP_DAYS}}


ALERT_TEXT = {
    'client_retries': 'client retries', 'duplicate_prefill': 'repeated requests', 'http_5xx': 'server errors',
    'stream_cut': 'answers cut off', 'engine_death': 'engine stopped unexpectedly', 'guard_trip': 'memory guard stopped a model',
    'scope_oom': 'out-of-memory stop', 'swap': 'model memory swapped', 'unknown_cache_misses': 'unexplained cache misses',
    'prefix_hits_low': 'low cache reuse',
    'restore_slow': 'slow session restore', 'store_full': 'cache storage nearly full',
    'store_young_eviction': 'recent conversation evicted', 'decode_slower': 'slower replies',
    'prefill_slower': 'slower prompt reading', 'ttft_hit_slower': 'slower first token', 'compaction_slower': 'slower compaction',
}


def status_line(digest, soak_value=None, now=None):
    """The widget's one line: what the day looked like, in plain words."""
    now = time.time() if now is None else now
    alerts = digest.get('alerts') or []
    if now - (digest.get('generated_at') or 0) > STALE_SECONDS:
        state, line = 'stale', 'Health check has not run lately'
    elif digest.get('requests', 0) == 0:
        state, line = 'no_data', 'No use recorded yet'
    elif not alerts:
        state, line = 'healthy', 'Healthy'
    else:
        state = 'attention'
        line = '1 thing to look at' if len(alerts) == 1 else f'{len(alerts)} things to look at'
    value = {'schema': STATUS_SCHEMA, 'day': digest.get('day'), 'state': state, 'line': line,
             'alerts': [a['id'] for a in alerts],
             'alert_text': [ALERT_TEXT.get(a['id'].split(':', 1)[0], 'check the digest') for a in alerts],
             'requests': digest.get('requests', 0), 'generated_at': digest.get('generated_at')}
    if soak_value is not None:
        value['soak'] = {k: soak_value[k] for k in ('green_days', 'ship_ready', 'pass', 'requests', 'history_rewrites')}
    return value


def write_day(digester, day, directory=None):
    """Persist `day`'s digest and refresh status.json from it; returns (digest, status)."""
    directory = directory or digest_dir()
    value = digester.day(day)
    _write_private(directory / f'digest-{day:%Y%m%d}.json', value)
    soak_value = soak(digester, day)
    status = status_line(value, soak_value, now=value.get('generated_at'))
    _write_private(directory / 'status.json', status)
    return value, status


def read_status(directory=None, now=None):
    lines = _read_lines((directory or digest_dir()) / 'status.json')
    try:
        value = json.loads(b'\n'.join(lines)) if lines else None
    except (ValueError, RecursionError):
        value = None
    if not isinstance(value, dict) or value.get('schema') != STATUS_SCHEMA:
        return {'schema': STATUS_SCHEMA, 'day': None, 'state': 'no_data', 'line': 'No health check yet',
                'alerts': [], 'alert_text': [], 'requests': 0, 'generated_at': None}
    now = time.time() if now is None else now
    if now - (value.get('generated_at') or 0) > STALE_SECONDS:
        value = dict(value, state='stale', line='Health check has not run lately')
    return value


def text(digest):
    """Short human summary of one digest (numbers only)."""
    lat, cache, hist, queue = digest['latency'], digest['cache'], digest['history'], digest['queue']

    def fmt(v, unit=''):
        return '-' if v is None else (f'{v:.0f}{unit}' if isinstance(v, (int, float)) else str(v))
    hit = cache['token_hit_rate']
    lines = [f"Day {digest['day']}{' (so far)' if digest.get('partial') else ''}: {digest['requests']} requests, "
             + ('healthy' if not digest['alerts'] else f"{len(digest['alerts'])} to look at: "
                + ', '.join(ALERT_TEXT.get(a['id'].split(':', 1)[0], a['id']) for a in digest['alerts'])),
             f"  cache reuse {fmt(None if hit is None else 100 * hit, '%')} of tokens, {cache['c3_misses']} misses "
             f"in {cache['eligible_turns']} follow-up turns ({cache['unknown_misses']} unexplained)",
             f"  history rewrites {hist['history_rewrites']} (fold suspects {hist['fold_suspects']}, "
             f"client compactions {hist['client_compactions']})",
             f"  queued {queue['queued']} (wait p95 {fmt(queue['wait_ms']['p95'], ' ms')}), "
             f"replays {sum(queue['replays'].values())}",
             f"  first token p50 {fmt(lat['ttft_ms']['p50'], ' ms')}, p95 {fmt(lat['ttft_ms']['p95'], ' ms')}; "
             f"errors 5xx {digest['errors']['http_5xx']}, cut {digest['errors']['stream_cut']}, "
             f"guard stops {digest['guard']['trips']}"]
    if digest.get('versions'):
        lines.append('  installed: ' + ', '.join(f'{k} {v}' for k, v in sorted(digest['versions'].items())))
    for key, cur in (digest.get('series') or {}).items():
        decode = [f"{b} {cur['decode_tok_s'][b]['p50']:.1f}" for b in BAND_NAMES if cur['decode_tok_s'][b]['p50'] is not None]
        if decode:
            lines.append(f'  {key} reply speed tok/s by depth: ' + ', '.join(decode))
    return '\n'.join(lines)
