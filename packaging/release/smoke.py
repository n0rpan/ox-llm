#!/usr/bin/env python3
"""Exercise installed CLIs and the real loopback stub engine, without a model."""
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import tempfile
import time
import urllib.request
import urllib.error

def tokenizer_fixture(path):
    """GGUF metadata only: a synthetic byte vocabulary, no tensors or weights."""
    visible = list(range(33, 127)) + list(range(161, 173)) + list(range(174, 256))
    extra = {byte: 256 + index for index, byte in enumerate(b for b in range(256) if b not in visible)}
    tokens = [chr(byte if byte in visible else extra[byte]) for byte in range(256)]
    tokens += ['<|im_start|>', '<|im_end|>', '<|endoftext|>', '<think>', '</think>']
    def string(value):
        data = value.encode()
        return struct.pack('<Q', len(data)) + data
    rows = []
    for name, value in [('tokenizer.ggml.model', 'gpt2'), ('tokenizer.ggml.pre', 'qwen2'),
                        ('tokenizer.chat_template', "{% for message in messages %}<|im_start|>{{ message['role'] }}\n{{ message['content'] }}<|im_end|>\n{% endfor %}{% if add_generation_prompt %}<|im_start|>assistant\n{% endif %}")]:
        rows.append(string(name) + struct.pack('<I', 8) + string(value))
    rows.append(string('tokenizer.ggml.tokens') + struct.pack('<IIQ', 9, 8, len(tokens)) + b''.join(map(string, tokens)))
    rows.append(string('tokenizer.ggml.merges') + struct.pack('<IIQ', 9, 8, 0))
    rows.append(string('tokenizer.ggml.eos_token_id') + struct.pack('<II', 4, 257))
    path.write_bytes(b'GGUF' + struct.pack('<IQQ', 3, 0, len(rows)) + b''.join(rows))


with tempfile.TemporaryDirectory() as sandbox, tempfile.TemporaryDirectory() as fixtures:
    model = Path(fixtures) / 'tokenizer-only.gguf'
    tokenizer_fixture(model)
    env = {'PATH': os.defpath, 'HOME': sandbox, 'XDG_CONFIG_HOME': sandbox + '/config',
           'XDG_STATE_HOME': sandbox + '/state', 'XDG_CACHE_HOME': sandbox + '/cache',
           'XDG_DATA_HOME': sandbox + '/data', 'XDG_RUNTIME_DIR': sandbox + '/runtime',
           'PYTHONDONTWRITEBYTECODE': '1'}
    assert not Path('/usr/share/ox-llm/oxc').exists(), 'Python controller payload installed'
    assert not Path('/usr/bin/oxc-diagnostics').exists(), 'Python diagnostics wrapper installed'
    for retired in ('oxc', 'oxc-diagnostics', 'oxe'):
        assert not Path('/usr/bin', retired).exists(), f'retired command installed: {retired}'
    result = subprocess.run(['oxa', '--version'], env=env, text=True, capture_output=True, check=True)
    assert '0.2.0' in result.stdout, result.stdout  # component version; release-version decision pending
    print(result.stdout.strip())
    for binary in ('oxa', 'oxe-server'):
        subprocess.run([binary, '--help'], env=env, stdout=subprocess.DEVNULL, check=True)
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        port = sock.getsockname()[1]
    with tempfile.TemporaryFile() as log:
        process = subprocess.Popen(['oxe-server', '--engine', 'stub', '--model', str(model),
                                    '--alias', 'synthetic', '--stub-reply', 'ok', '--reasoning', 'off', '--host', '127.0.0.1',
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
            try:
                with opener.open(request, timeout=10) as response:
                    completion = json.load(response)
            except urllib.error.HTTPError as error:
                log.seek(0)
                raise AssertionError((error.read().decode(), log.read().decode())) from error
            assert completion['choices'], completion
        finally:
            process.terminate()
            process.wait(timeout=10)
    assert not list(Path(sandbox).iterdir()), 'smoke commands wrote unexpected user state'
print('installed model-free smoke passed')
