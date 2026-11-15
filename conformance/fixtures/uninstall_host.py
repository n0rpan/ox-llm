#!/usr/bin/env python3
"""Private scripted systemd peer; never starts or stops a real service."""
import json
import os
from pathlib import Path
import sys

home = Path(os.environ['FAKE_UNINSTALL_HOME'])
config = home / 'systemd-state.json'
state = json.loads(config.read_text())
words = sys.argv[1:]
with (home / 'host-commands.jsonl').open('a') as log:
    log.write(json.dumps(words) + '\n')
if not words or words[0] != '--user':
    raise SystemExit(99)
operation, units = words[1], words[2:]
code = 0
if operation == 'is-active':
    if state.get('short_reply'):
        print('synthetic unavailable')
        raise SystemExit(1)
    for unit in units:
        print(state['units'].get(unit, 'inactive'))
    code = 0 if all(state['units'].get(unit) == 'active' for unit in units) else 3
elif operation in ('stop', 'start'):
    if not (operation == 'stop' and state.get('stop_refused')):
        for unit in units:
            state['units'][unit] = 'inactive' if operation == 'stop' else 'active'
    if operation == 'stop' and state.get('change_on_stop'):
        Path(state['change_on_stop']).write_text('synthetic owner change after plan\n')
elif operation in ('disable', 'daemon-reload'):
    if state.get(operation + '_failed'):
        print('synthetic ' + operation + ' failure', file=sys.stderr)
        code = 1
else:
    raise SystemExit(99)
config.write_text(json.dumps(state))
raise SystemExit(code)
