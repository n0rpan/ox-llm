"""Allowlisted, credential-free environments for standalone conformance peers."""
import os
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PRIMARY = 'ornith-35b-a3b-local'
SECONDARY = 'qwen3.6-35b-a3b-local'


def isolated_env(home):
    env = {'HOME': str(home), 'PATH': '/usr/bin:/bin', 'LANG': 'C.UTF-8',
           'LC_ALL': 'C.UTF-8', 'TERM': 'dumb', 'COLUMNS': '80',
           'PYTHONPATH': str(ROOT / 'oxc/src')}
    for key, subdir in (('XDG_CONFIG_HOME', 'config'), ('XDG_STATE_HOME', 'state'),
                        ('XDG_DATA_HOME', 'data'), ('XDG_CACHE_HOME', 'cache'),
                        ('XDG_RUNTIME_DIR', 'run')):
        path = home / subdir
        path.mkdir(mode=0o700, exist_ok=True)
        env[key] = str(path)
    return env
