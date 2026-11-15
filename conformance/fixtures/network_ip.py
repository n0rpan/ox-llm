#!/usr/bin/env python3
"""Scripted iproute2 metadata; test traffic stays on isolated loopback aliases."""
import json
import os
from pathlib import Path
import sys

state = json.loads(Path(os.environ['FAKE_SHARE_NETWORK']).read_text())
words = sys.argv[1:]
if words == ['-j', 'addr', 'show']:
    value = state['addresses']
elif words == ['-j', '-4', 'route', 'show', 'default']:
    value = state['routes']
elif words[:3] == ['-j', 'neigh', 'show']:
    value = state.get('neighbors', [])
else:
    raise SystemExit(2)
print(json.dumps(value))
