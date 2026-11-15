"""`doctor` additions (read-only, starts nothing): which engine each runtime.json profile really uses on this
machine and why (the Rust engine applies by CPU + RAM capability, not by boot type), and whether the
controller service gives the memory guard what it needs (MemorySwapMax=0). Never raises."""
import json
import os
from pathlib import Path
from .runtime import profile_policy

UNIT = 'oxc-chat.service'


def runtime_config_path(home=None):
    if os.environ.get('LOCAL_LLM_CONFIG'):
        return Path(os.environ['LOCAL_LLM_CONFIG'])
    home = Path(home) if home is not None else Path.home()
    base = Path(os.environ['XDG_CONFIG_HOME']) if os.environ.get('XDG_CONFIG_HOME') else home / '.config'
    return base / 'oxc/llm/runtime.json'


def engine_profiles(config_path=None):
    path = Path(config_path) if config_path is not None else runtime_config_path()
    try:
        value = json.loads(path.read_text())
        profiles = value.get('profiles') or {}
    except (OSError, ValueError, AttributeError):
        return {'config': str(path), 'readable': False, 'profiles': {}}
    out = {}
    for alias, profile in profiles.items():
        if not isinstance(profile, dict):
            continue
        try:
            effective = profile_policy.resolve({**profile, 'model_alias': alias})
            engine = effective.get('backend_kind') or 'llama'
        except (ValueError, TypeError, KeyError) as error:
            out[alias] = {'engine': None, 'error': str(error)}
            continue
        rust = profile_policy.rust_status(profile)
        row = {'engine': {'oxe': 'rust'}.get(engine, engine), 'rust_configured': rust['configured']}
        if rust['configured'] and engine != 'oxe':
            row['why_not_rust'] = rust['reason'] or (
                'switched off (%s=0)' % (profile_policy.OXE_FLASH_ENABLE_ENV if profile.get('backend_kind') ==
                                         profile_policy.FLASH_KIND else profile_policy.OXE_ENABLE_ENV))
        out[alias] = row
    return {'config': str(path), 'readable': True, 'profiles': out,
            'rust_host': profile_policy.rust_host_reason() or 'ok'}


def unit_memory_setup(home=None):
    """MemorySwapMax=0 in the controller unit (+ its drop-ins), as written; the running controller's own
    cgroup is in `oxc status` memory_guard_setup."""
    home = Path(home) if home is not None else Path.home()
    unit = home / '.config/systemd/user' / UNIT
    texts = []
    for p in [unit, *sorted((home / '.config/systemd/user' / (UNIT + '.d')).glob('*.conf'))]:
        try:
            texts.append(p.read_text())
        except OSError:
            pass
    if not texts:
        return {'unit': str(unit), 'present': False, 'memory_swap_max': None, 'ok': None}
    value = None
    for text in texts:
        for line in text.splitlines():
            if line.startswith('MemorySwapMax='):
                value = line.split('=', 1)[1].strip()
    ok = value == '0'
    return {'unit': str(unit), 'present': True, 'memory_swap_max': value, 'ok': ok,
            'fix': None if ok else 'add MemorySwapMax=0 under [Service] (systemctl --user edit oxc-chat) and restart it: '
                                   'the memory guard starts no model without it'}


def report():
    return {'engines': engine_profiles(), 'controller_unit': unit_memory_setup()}
