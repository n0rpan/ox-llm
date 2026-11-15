"""Model-free installed Studio wire contracts; all traffic targets loopback."""
import hashlib
import http.client
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import time
import unittest

from harness import ROOT, free_port, isolated_env

GOLDENS = Path(__file__).with_name('goldens') / 'studio.json'
CASES = ('page', 'activity', 'models', 'missing', 'missing-job', 'bad-host',
         'missing-host', 'duplicate-host', 'get-origin', 'post-missing',
         'post-origin', 'post-site', 'post-duplicate-origin', 'post-duplicate-site',
         'post-same-origin', 'post-bad-length', 'post-negative-length',
         'post-oversized', 'engine-invalid', 'engine-invalid-json', 'engine',
         'models-populated', 'models-settings', 'engine-configured', 'engine-ready',
         'engine-bad-gate', 'engine-set')


def capture(name):
    with tempfile.TemporaryDirectory(prefix='oxc-studio-conformance-') as directory:
        home = Path(directory)
        env = isolated_env(home)
        port = free_port()
        project = home / 'project'
        project.mkdir(mode=0o700)
        env.update(STUDIO_PORT=str(port), LOCAL_IMAGE_CONFIG_DIR=str(project),
                   SD_HOME=str(home / 'stack'), IMG_OUTDIR=str(home / 'pictures'),
                   LOCAL_AI_LEASE=str(home / 'inference.lock'))
        if name == 'models-populated' or name in ('engine-ready', 'engine-bad-gate'):
            models = home / 'stack/models/images'
            for filename in ('z_image_turbo-Q4_K.gguf', 'z_image_turbo-Q8_0.gguf',
                             'flux2-klein/flux-2-klein-4b-Q8_0.gguf',
                             'flux2-klein/Qwen3-4B-Q8_0.gguf', 'flux2-klein/flux2-vae.safetensors',
                             'upscale/RealESRGAN_x4plus.pth'):
                path = models / filename
                path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
                path.write_bytes(b'synthetic metadata fixture\n')
            with (models / 'z_image_turbo-Q4_K.gguf.part').open('wb') as partial:
                partial.truncate(9 * (1 << 19))
        if name == 'models-populated':
            for filename, value in (
                    ('q8-status.json', {'state': 'synthetic staged'}),
                    ('overnight-status.json', {'state': 'synthetic complete'}),
                    ('cpu-profile.json', {'validated': True, 'dimensions': [[1024, 1024]]}),
                    ('gpu-profile.json', {'enabled': True, 'validated': True, 'models': {'q8': {'max_width': 512}}})):
                (project / filename).write_text(json.dumps(value))
        if name == 'models-settings':
            (project / 'edit.json').write_text(json.dumps({'estimate': {'calibrated': True,
                'fixed_s': 20, 's_per_mpx_step': 40, 'pixel_exponent': 1, 'vae_encode_s_per_mpx': 2}}))
            (project / 'presets.json').write_text(json.dumps({
                'draft': {'model': 'q8', 'width': 512, 'height': 512, 'steps': 8, 'expected_seconds': 1.5},
                'final': {'model': 'q4', 'width': True, 'height': 1024, 'steps': 8},
                'draft512': {'model': 'klein-q4', 'width': 768, 'height': 512, 'steps': 4, 'expected_seconds': 60000}}))
            (project / 'upscale.json').write_text(json.dumps({'esrgan_scale': 2.0, 'refine_targets': [1024, 768, 1024],
                'refine_target': 1280, 'refine_strength': .5, 'esrgan_model': '../synthetic-outside.pth',
                'estimate': {'calibrated': True}}))
        if name == 'engine-configured':
            (project / 'engine.json').write_text('{"engine":"sdcpp"}\n')
        if name in ('engine-ready', 'engine-bad-gate'):
            binary = home / 'stack/releases/oxi/oxi'
            probe = home / 'stack/releases/sd-probe/sd-cli'
            for path in (binary, probe):
                path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
                path.write_bytes(b'synthetic metadata executable\n')
                path.chmod(0o700)
            gates = project / 'oxi-size-gates'
            gates.mkdir(mode=0o700)
            (gates / '768x768.json').write_text(json.dumps({'schema': 'oxi-size-gate/1', 'width': 768,
                'height': 768, 'verdict': 'PASS', 'seeds': [{'pass': True}],
                'oxi_settings': {'chunk1': 32, 'chunk2': '32', 'attn_prec': 'f32', 'vae_conv': 'f32'},
                'oxi_sha256': hashlib.sha256(binary.read_bytes()).hexdigest() if name == 'engine-ready' else '0' * 64}))
        command = shlex.split(os.environ['OXC_STUDIO_COMMAND']) if 'OXC_STUDIO_COMMAND' in os.environ else [
            sys.executable, str(ROOT / 'lab/src/snapdragon_ai_lab/runtime/studio')]
        with tempfile.TemporaryFile() as log:
            process = subprocess.Popen(command, env=env, cwd=home, stdin=subprocess.DEVNULL,
                stdout=log, stderr=log, umask=0o077)
            try:
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline and process.poll() is None:
                    connection = http.client.HTTPConnection('127.0.0.1', port, timeout=.5)
                    try:
                        connection.request('GET', '/activity')
                        response = connection.getresponse()
                        response.read()
                        break
                    except (ConnectionError, OSError):
                        time.sleep(.02)
                    finally:
                        connection.close()
                else:
                    raise AssertionError('Studio did not start')
                method, path, body = 'GET', '/', None
                headers = [('Host', '127.0.0.1:' + str(port))]
                if name.startswith('models'):
                    path = '/models'
                elif name.startswith('engine') and name not in ('engine-invalid', 'engine-invalid-json', 'engine-set'):
                    path = '/engine'
                elif name in ('activity', 'missing'):
                    path = '/' + name
                elif name == 'missing-job':
                    path = '/job?id=synthetic-missing'
                elif name == 'bad-host':
                    headers = [('Host', 'example.invalid')]
                elif name == 'missing-host':
                    headers = []
                elif name == 'duplicate-host':
                    headers *= 2
                elif name == 'get-origin':
                    headers += [('Origin', 'https://example.invalid')]
                elif name.startswith('post-'):
                    method, path = 'POST', '/missing'
                    if name == 'post-origin':
                        headers += [('Origin', 'https://example.invalid')]
                    elif name == 'post-site':
                        headers += [('Sec-Fetch-Site', 'cross-site')]
                    elif name == 'post-duplicate-origin':
                        headers += [('Origin', 'http://127.0.0.1:' + str(port))] * 2
                    elif name == 'post-duplicate-site':
                        headers += [('Sec-Fetch-Site', 'same-origin')] * 2
                    elif name == 'post-same-origin':
                        headers += [('Origin', 'http://127.0.0.1:' + str(port)), ('Sec-Fetch-Site', 'same-origin')]
                    elif name == 'post-bad-length':
                        headers += [('Content-Length', 'synthetic-invalid')]
                    elif name == 'post-negative-length':
                        headers += [('Content-Length', '-1')]
                    elif name == 'post-oversized':
                        headers += [('Content-Length', str((1 << 20) + 1))]
                elif name.startswith('engine-'):
                    method, path = 'POST', '/engine'
                    body = b'{"engine":"sdcpp"}' if name == 'engine-set' else (
                        b'{"engine":"synthetic-invalid"}' if name == 'engine-invalid' else b'[]')
                    headers += [('Content-Length', str(len(body)))]
                connection = http.client.HTTPConnection('127.0.0.1', port, timeout=5)
                try:
                    connection.putrequest(method, path, skip_host=True, skip_accept_encoding=True)
                    for header, value in headers:
                        connection.putheader(header, value)
                    connection.endheaders(body)
                    response = connection.getresponse()
                    raw = response.read()
                    value = {'status': response.status, 'http_version': response.version,
                             'content_type': response.getheader('Content-Type')}
                    if path == '/' and response.status == 200:
                        value['page_sha256'] = hashlib.sha256(raw).hexdigest()
                        value['page_bytes'] = len(raw)
                    else:
                        try:
                            value['body'] = json.loads(raw)
                        except ValueError:
                            value['body'] = raw.decode()
                    if name == 'engine-set' and (project / 'engine.json').exists():
                        value['settings_after'] = json.loads((project / 'engine.json').read_text())
                        value['settings_mode'] = (project / 'engine.json').stat().st_mode & 0o777
                finally:
                    connection.close()
                return json.loads(json.dumps(value).replace(str(home), '<HOME>'))
            finally:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)


class StudioContract(unittest.TestCase):
    def test_studio_goldens(self):
        expected = json.loads(GOLDENS.read_text())
        self.assertEqual(set(expected), set(CASES))
        for name in CASES:
            with self.subTest(case=name):
                self.assertEqual(capture(name), expected[name])


if __name__ == '__main__':
    if sys.argv[1:] == ['--record-python']:
        if 'OXC_STUDIO_COMMAND' in os.environ:
            raise SystemExit('Record goldens only from the Python reference')
        GOLDENS.write_text(json.dumps({name: capture(name) for name in CASES}, indent=2, sort_keys=True) + '\n')
    else:
        unittest.main()
