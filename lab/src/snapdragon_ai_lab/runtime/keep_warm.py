"""O-160 smart keep-warm: when the reaper may unload an idle model.

The model stays loaded while the machine has room for it. An idle model (no request running or
queued) is unloaded only
  (a) under memory pressure: PSI `some avg10` of /proc/pressure/memory at or above a threshold, or
      MemAvailable below a floor, seen on PRESSURE_CONFIRM_PASSES reaper passes in a row and only
      after a short grace since the last request (a request's own pressure must decay first), or
  (b) after a long idle (default 2 h; LOCAL_LLM_KEEP_WARM_IDLE_SECONDS=600 restores the old 600 s
      unload, `off` disables the idle unload; pressure still unloads).

The legacy LOCAL_LLM_IDLE_SECONDS is IGNORED from v0.2.0: installed units carry
`LOCAL_LLM_IDLE_SECONDS=600` (the old default, written by the v0.1.x stage tool), so honouring it
would keep every install at the old 600 s unload. The controller logs one line when it is set.

Settings, lowest precedence first: built-in defaults, environment, then the runtime JSON
`keep_warm` block (top level, or a profile's own, which replaces it for that model). A Flash
profile's `oxe_flash.idle_unload_seconds` still sets its own idle time.
  env LOCAL_LLM_KEEP_WARM_IDLE_SECONDS    JSON idle_unload_seconds    7200 (off / null = never)
  env LOCAL_LLM_PRESSURE_PSI_SOME_AVG10   JSON psi_some_avg10         10.0 percent (off / null = no PSI check)
  env LOCAL_LLM_PRESSURE_MIN_AVAILABLE_MIB JSON min_available_mib     1024 MiB (off / null = no MemAvailable check)
  env LOCAL_LLM_PRESSURE_GRACE_SECONDS    JSON pressure_grace_seconds 60

A kernel without PSI (no /proc/pressure/memory) falls back to MemAvailable only. The memory
source is an object with psi_some_avg10() and available_bytes() (each None when unknown), so
tests inject a fake one; nothing here raises on a missing or malformed /proc file.
"""
import collections
import math
import os
from pathlib import Path

DEFAULT_IDLE_SECONDS = 7200.0
DEFAULT_PSI_SOME_AVG10 = 10.0
DEFAULT_MIN_AVAILABLE_MIB = 1024
DEFAULT_GRACE_SECONDS = 60.0
PRESSURE_CONFIRM_PASSES = 3  # the reaper runs once a second: about 3 s of sustained pressure
OFF_WORDS = ('off', 'never', 'none', 'disabled')
MIB = 1024 * 1024

IDLE_ENV = 'LOCAL_LLM_KEEP_WARM_IDLE_SECONDS'
LEGACY_IDLE_ENV = 'LOCAL_LLM_IDLE_SECONDS'  # ignored since v0.2.0 (see the module doc)
PSI_ENV = 'LOCAL_LLM_PRESSURE_PSI_SOME_AVG10'
AVAILABLE_ENV = 'LOCAL_LLM_PRESSURE_MIN_AVAILABLE_MIB'
GRACE_ENV = 'LOCAL_LLM_PRESSURE_GRACE_SECONDS'
JSON_KEY = 'keep_warm'


class ProcMemorySource:
    """The host's memory signals; None for any value that cannot be read."""

    def __init__(self, pressure_path='/proc/pressure/memory', meminfo_path='/proc/meminfo'):
        self.pressure_path = Path(pressure_path)
        self.meminfo_path = Path(meminfo_path)

    def psi_some_avg10(self):
        try:
            text = self.pressure_path.read_text()
        except (OSError, ValueError):
            return None  # no PSI on this kernel (or not readable): MemAvailable only
        return parse_psi_some_avg10(text)

    def available_bytes(self):
        try:
            for line in self.meminfo_path.read_text().splitlines():
                if line.startswith('MemAvailable:'):
                    return int(line.split()[1]) * 1024
        except (OSError, ValueError, IndexError):
            pass
        return None


def parse_psi_some_avg10(text):
    """`some avg10=1.23 avg60=... total=...` -> 1.23; None when absent or malformed."""
    for line in (text or '').splitlines():
        fields = line.split()
        if fields[:1] == ['some']:
            for field in fields[1:]:
                name, _, value = field.partition('=')
                if name == 'avg10':
                    try:
                        number = float(value)
                    except ValueError:
                        return None
                    return number if math.isfinite(number) and number >= 0 else None
    return None


def _number(value, minimum):
    """A finite number >= minimum, else None (bools are not numbers here)."""
    if isinstance(value, bool) or not isinstance(value, (int, float, str)):
        return None
    try:
        number = float(value)
    except ValueError:
        return None
    return number if math.isfinite(number) and number >= minimum else None


_UNSET = object()


def _setting(raw, minimum, default):
    """One setting from an env string or JSON value: OFF/null -> None (disabled); a valid number;
    anything else -> `default` (an invalid value never disables a safety unload)."""
    if raw is _UNSET:
        return default
    if raw is None or (isinstance(raw, str) and raw.strip().lower() in OFF_WORDS):
        return None
    number = _number(raw, minimum)
    return default if number is None else number


def env_settings(environ=None):
    environ = os.environ if environ is None else environ

    def get(name):
        return environ[name] if name in environ else _UNSET
    idle = _setting(get(IDLE_ENV), 1e-9, DEFAULT_IDLE_SECONDS)       # 0 is invalid (audit N8)
    psi = _setting(get(PSI_ENV), 1e-9, DEFAULT_PSI_SOME_AVG10)
    available = _setting(get(AVAILABLE_ENV), 1e-9, DEFAULT_MIN_AVAILABLE_MIB)
    grace = _setting(get(GRACE_ENV), 0, DEFAULT_GRACE_SECONDS)
    return {'idle_unload_seconds': idle, 'psi_some_avg10': psi, 'min_available_mib': available,
            'pressure_grace_seconds': DEFAULT_GRACE_SECONDS if grace is None else grace}


def settings(config, base=None):
    """Effective settings for a loaded model's config: `base` (env) overlaid with the config's
    `keep_warm` block, then a Flash profile's own oxe_flash.idle_unload_seconds."""
    out = dict(base if base is not None else env_settings())
    config = config if isinstance(config, dict) else {}
    block = config.get(JSON_KEY)
    if isinstance(block, dict):
        for key, minimum in (('idle_unload_seconds', 1e-9), ('psi_some_avg10', 1e-9),
                             ('min_available_mib', 1e-9), ('pressure_grace_seconds', 0)):
            if key in block:
                value = _setting(block[key], minimum, out[key])
                out[key] = out[key] if value is None and key == 'pressure_grace_seconds' else value
    flash = config.get('oxe_flash')
    if (config.get('backend_kind') == 'oxe' and isinstance(flash, dict)
            and _number(flash.get('idle_unload_seconds'), 1e-9) is not None):
        out['idle_unload_seconds'] = float(flash['idle_unload_seconds'])
    return out


Decision = collections.namedtuple('Decision', 'action reason streak')


def decide(now, last_use, mem_available, psi, config=None, *, streak=0, base=None):
    """THE keep-warm decision, pure (no I/O, no clock): the controller's reaper calls it through Policy,
    and fixtures call it directly:  from snapdragon_ai_lab.runtime.keep_warm import decide
    (flat release layout: `import keep_warm`).

    now, last_use   monotonic seconds; idle = now - last_use (a preload or a finished request sets last_use)
    mem_available   MemAvailable in bytes, or None when unknown
    psi             PSI `some avg10` percent of /proc/pressure/memory, or None (no PSI on this kernel)
    config          the loaded model's resolved config (its `keep_warm` block / Flash idle time), or None
    streak          consecutive earlier passes that saw pressure (pass back Decision.streak each call)
    base            env settings (env_settings()); None reads the process environment
    Returns Decision(action='keep'|'unload', reason=None|'idle'|'pressure_psi'|'pressure_available', streak).
    The caller guarantees no request runs, waits or is being admitted."""
    current = settings(config or {}, base)
    idle_for = now - last_use
    if current['idle_unload_seconds'] is not None and idle_for >= current['idle_unload_seconds']:
        return Decision('unload', 'idle', 0)
    if idle_for < current['pressure_grace_seconds']:
        return Decision('keep', None, 0)
    reason = None
    if current['psi_some_avg10'] is not None and psi is not None and psi >= current['psi_some_avg10']:
        reason = 'pressure_psi'
    elif (current['min_available_mib'] is not None and mem_available is not None
          and mem_available < current['min_available_mib'] * MIB):
        reason = 'pressure_available'
    if reason is None:
        return Decision('keep', None, 0)
    if streak + 1 < PRESSURE_CONFIRM_PASSES:
        return Decision('keep', None, streak + 1)
    return Decision('unload', reason, 0)


class Policy:
    """The reaper's unload decision. Not thread safe: the reaper calls it under Manager.mu."""

    def __init__(self, source=None):
        self.source = source if source is not None else ProcMemorySource()
        self.streak = 0
        self.last_unload = None  # {'reason', 'idle_s', 'psi_some_avg10', 'available_mib'}
        self.last_sample = None

    def reset(self):
        self.streak = 0

    def pressure(self, current):
        """(reason, psi, available_bytes): reason is 'pressure_psi', 'pressure_available' or None."""
        psi = available = None
        if current['psi_some_avg10'] is not None:
            psi = self.source.psi_some_avg10()
        if current['min_available_mib'] is not None:
            available = self.source.available_bytes()
        self.last_sample = {'psi_some_avg10': psi,
                            'available_mib': None if available is None else int(available // MIB)}
        if psi is not None and psi >= current['psi_some_avg10']:
            return 'pressure_psi', psi, available
        if available is not None and available < current['min_available_mib'] * MIB:
            return 'pressure_available', psi, available
        return None, psi, available

    def decide(self, config, idle_for, base=None):
        """The unload reason for a model idle for `idle_for` seconds, else None (the module-level
        decide() with this policy's memory source and pressure streak). The caller has already
        checked that no request runs or waits."""
        current = settings(config, base)
        psi = available = None
        idle_due = current['idle_unload_seconds'] is not None and idle_for >= current['idle_unload_seconds']
        if not idle_due and idle_for >= current['pressure_grace_seconds']:
            if current['psi_some_avg10'] is not None:
                psi = self.source.psi_some_avg10()
            if current['min_available_mib'] is not None:
                available = self.source.available_bytes()
            self.last_sample = {'psi_some_avg10': psi,
                                'available_mib': None if available is None else int(available // MIB)}
        result = decide(idle_for, 0.0, available, psi, config, streak=self.streak, base=base)
        self.streak = result.streak
        if result.action != 'unload':
            return None
        self.last_unload = {'reason': result.reason, 'idle_s': round(idle_for, 1)}
        if result.reason != 'idle':
            self.last_unload.update({'psi_some_avg10': psi,
                                     'available_mib': None if available is None else int(available // MIB)})
        return result.reason

    def status(self, config, base=None):
        current = settings(config, base)
        return {**current, 'last_unload': dict(self.last_unload) if self.last_unload else None,
                'last_sample': dict(self.last_sample) if self.last_sample else None}
