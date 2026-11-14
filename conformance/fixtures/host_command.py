#!/usr/bin/env python3
"""Synthetic systemd/desktop command peer; never contacts a real service."""
import json
import os
from pathlib import Path
import sys

name = Path(sys.argv[0]).name
row = json.dumps({'command': name, 'args': sys.argv[1:]}) + '\n'
fd = os.open(Path.home() / 'host-commands.jsonl', os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
os.write(fd, row.encode())
os.close(fd)
if name == 'omarchy-shell' and os.environ.get('OXC_PANEL_OUTPUT') == '1':
    print('Synthetic panel output.')
    print('Synthetic panel diagnostic.', file=sys.stderr)
    raise SystemExit(37)
if name == 'systemctl' and os.environ.get('OXC_STATEFUL_SYSTEMCTL') == '1':
    state_path = Path.home() / 'mock-services.json'
    states = json.loads(state_path.read_text()) if state_path.exists() else {}
    verb = sys.argv[2]
    units = [arg for arg in sys.argv[3:] if arg.endswith('.service')]
    failure = Path.home() / 'mock-fail-once'
    if failure.exists() and failure.read_text().strip() == verb:
        failure.unlink()
        raise SystemExit(1)
    if verb == 'is-active':
        values = [states.get(unit, 'active') for unit in units]
        print('\n'.join(values))
        raise SystemExit(0 if all(value == 'active' for value in values) else 3)
    if verb == 'show':
        print('MainPID=0')
    elif verb in ('stop', 'start', 'restart'):
        if not (verb == 'stop' and (Path.home() / 'mock-stuck-stop').exists()):
            for unit in units:
                states[unit] = 'inactive' if verb == 'stop' else 'active'
            state_path.write_text(json.dumps(states))
    raise SystemExit(0)
if name == 'pgrep':
    raise SystemExit(1)
if name == 'systemctl' and 'is-active' in sys.argv:
    # Controller active; unrelated desktop/image/download units inactive.
    raise SystemExit(0 if 'local-ai-chat.service' in sys.argv else 3)
