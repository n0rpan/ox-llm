#!/usr/bin/env python3
"""Exercise installed CLIs and the real loopback stub engine, without a model."""
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import urllib.request

with tempfile.TemporaryDirectory() as sandbox:
    env = {'PATH': os.defpath, 'HOME': sandbox, 'XDG_CONFIG_HOME': sandbox + '/config',
           'XDG_STATE_HOME': sandbox + '/state', 'XDG_CACHE_HOME': sandbox + '/cache',
           'XDG_DATA_HOME': sandbox + '/data', 'XDG_RUNTIME_DIR': sandbox + '/runtime',
           'PYTHONDONTWRITEBYTECODE': '1'}
    for binary in ('oxc', 'oxa', 'oxe'):
        result = subprocess.run([binary, '--version'], env=env, text=True, capture_output=True, check=True)
        assert '0.2.0' in result.stdout, (binary, result.stdout)
        print(result.stdout.strip())
    for binary in ('oxc', 'oxa', 'oxe-server', 'snapdragon-ai-lab'):
        subprocess.run([binary, '--help'], env=env, stdout=subprocess.DEVNULL, check=True)
    # This validates argument parsing without loading model weights.
    subprocess.run(['oxe', 'gen', '--model', 'absent.gguf', '--tokens', '1', '--check-args'],
                   env=env, check=True)
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        port = sock.getsockname()[1]
    with tempfile.TemporaryFile() as log:
        process = subprocess.Popen(['oxe-server', '--engine', 'stub', '--host', '127.0.0.1',
                                    '--port', str(port)], env=env, stdout=log, stderr=log)
        try:
            opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
            deadline = time.monotonic() + 20
            while True:
                if process.poll() is not None:
                    log.seek(0)
                    raise AssertionError(log.read().decode())
                try:
                    with opener.open(f'http://127.0.0.1:{port}/v1/models', timeout=1) as response:
                        models = json.load(response)
                    break
                except OSError:
                    if time.monotonic() >= deadline:
                        raise
                    time.sleep(0.1)
            assert isinstance(models['data'], list) and models['data'], models
            request = urllib.request.Request(f'http://127.0.0.1:{port}/v1/chat/completions',
                data=json.dumps({'model': models['data'][0]['id'],
                    'messages': [{'role': 'user', 'content': 'hello'}], 'max_tokens': 4}).encode(),
                headers={'Content-Type': 'application/json'})
            with opener.open(request, timeout=10) as response:
                completion = json.load(response)
            assert completion['choices'], completion
        finally:
            process.terminate()
            process.wait(timeout=10)
    assert not list(Path(sandbox).iterdir()), 'smoke commands wrote unexpected user state'
print('installed model-free smoke passed')
