#!/usr/bin/env python3
"""Synthetic sd-cli peer: records numeric/argv facts and emits a plain PNG.

No product imports, model reads or image inference. Prompt contents are verified
in memory and never written to the event receipt or printed.
"""
import json
import os
from pathlib import Path
import stat
import struct
import sys
import time
import zlib

args = sys.argv[1:]


def option(name, default=None):
    return args[args.index(name) + 1] if name in args else default


def chunk(kind, data):
    return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data))


prompt_file = option('--prompt-file')
if not prompt_file or not prompt_file.startswith('/proc/self/fd/'):
    raise SystemExit('synthetic peer requires an anonymous prompt descriptor')
descriptor = int(prompt_file.rsplit('/', 1)[1])
info = os.fstat(descriptor)
if not stat.S_ISREG(info.st_mode) or not os.readlink(prompt_file).startswith('/memfd:prompt-studio'):
    raise SystemExit('synthetic peer requires the prompt memfd')
with open(prompt_file, encoding='utf-8') as stream:
    prompt_ok = stream.read() == os.environ['FAKE_IMAGE_EXPECTED_PROMPT']
if not prompt_ok or os.environ['FAKE_IMAGE_EXPECTED_PROMPT'] in args:
    raise SystemExit('synthetic prompt handling mismatch')
safe_args = list(args)
safe_args[safe_args.index('--prompt-file') + 1] = '<validated:prompt-memfd>'
Path(os.environ['FAKE_IMAGE_EVENTS']).write_text(json.dumps({'argv': safe_args, 'prompt_memfd': True,
    'repack': os.environ.get('SD_CPU_REPACK_EXPERIMENT')}) + '\n')
print('loading tensors completed', flush=True)
mode = os.environ.get('FAKE_IMAGE_MODE', 'ok')
if mode == 'error':
    raise SystemExit(7)
if mode == 'missing':
    raise SystemExit(0)
if mode == 'delay':
    time.sleep(.2)
width, height = int(option('-W', '512')), int(option('-H', '512'))
header = struct.pack('>IIBBBBB', width, height, 8, 2, 0, 0, 0)
png = b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', header) + chunk(b'IDAT', zlib.compress((b'\0' + b'\0' * width * 3) * height)) + chunk(b'IEND', b'')
output = option('-o')
for index in range(int(option('-b', '1'))):
    path = Path(output.replace('%d', str(index)))
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    path.write_bytes(png)
