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
if name == 'pgrep':
    raise SystemExit(1)
if name == 'systemctl' and 'is-active' in sys.argv:
    # Controller active; unrelated desktop/image/download units inactive.
    raise SystemExit(0 if 'local-ai-chat.service' in sys.argv else 3)
