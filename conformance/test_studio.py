"""Model-free installed Studio wire contracts; all traffic targets loopback."""
import hashlib
import http.client
import json
import os
import re
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import time
import unittest
import urllib.parse

from harness import ROOT, free_port, isolated_env

GOLDENS = Path(__file__).with_name('goldens') / 'studio.json'
CASES = ('page', 'activity', 'models', 'missing', 'missing-job', 'bad-host',
         'missing-host', 'duplicate-host', 'get-origin', 'post-missing',
         'post-origin', 'post-site', 'post-duplicate-origin', 'post-duplicate-site',
         'post-same-origin', 'post-bad-length', 'post-negative-length',
         'post-oversized', 'engine-invalid', 'engine-invalid-json', 'engine',
         'models-populated', 'models-settings', 'engine-configured', 'engine-ready',
         'engine-bad-gate', 'engine-set', 'estimate-q8', 'estimate-q4', 'estimate-edit',
         'estimate-nonaligned', 'estimate-bad-size', 'estimate-bad-count', 'estimate-klein-missing',
         'estimate-klein-ready', 'estimate-klein-gated', 'estimate-forced-sdcpp',
         'estimate-cpu-profile', 'estimate-cpu-bad-hash', 'estimate-calibrated', 'estimate-unicode',
         'upload-png', 'upload-webp', 'upload-bad-type', 'upload-no-boundary', 'upload-empty',
         'upload-oversized', 'file-png', 'file-webp', 'file-outside', 'file-symlink-inside',
         'file-symlink-outside', 'file-missing', 'file-upload-root-symlink', 'upload-nonutf8-name',
         'models-invalid-setup', 'models-invalid-benchmarks', 'models-invalid-dimensions', 'file-directory')


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
        ready_cases = ('engine-ready', 'engine-bad-gate', 'estimate-klein-ready',
                       'estimate-klein-gated', 'estimate-forced-sdcpp')
        if name == 'models-populated' or name in ready_cases:
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
        if name == 'models-invalid-setup':
            (project / 'q8-status.json').write_text('[]\n')
        elif name == 'models-invalid-benchmarks':
            (project / 'benchmarks').mkdir(mode=0o700)
            (project / 'benchmarks/results.json').write_text('{"synthetic":1}\n')
        elif name == 'models-invalid-dimensions':
            (project / 'cpu-profile.json').write_text('{"validated":true,"dimensions":7}\n')
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
        if name in ('engine-configured', 'estimate-forced-sdcpp'):
            (project / 'engine.json').write_text('{"engine":"sdcpp"}\n')
        if name in ready_cases:
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
            if name in ('estimate-klein-ready', 'estimate-klein-gated', 'estimate-forced-sdcpp'):
                gate = json.loads((gates / '768x768.json').read_text())
                gate['oxi_sha256'] = hashlib.sha256(binary.read_bytes()).hexdigest()
                (gates / '768x768.json').write_text(json.dumps(gate))
        if name in ('estimate-cpu-profile', 'estimate-cpu-bad-hash'):
            binary = home / 'stack/releases/synthetic-cpu/sd-cli'
            binary.parent.mkdir(mode=0o700, parents=True)
            binary.write_bytes(b'synthetic metadata executable\n')
            binary.chmod(0o700)
            (project / 'cpu-profile.json').write_text(json.dumps({'enabled': True, 'validated': True,
                'models': ['q8'], 'dimensions': [[512, 512]], 'binary': str(binary),
                'sha256': hashlib.sha256(binary.read_bytes()).hexdigest() if name == 'estimate-cpu-profile' else '0' * 64}))
        if name == 'estimate-calibrated':
            (project / 'upscale.json').write_text(json.dumps({'estimate': {'calibrated': True,
                'unoptimized_factor': 2, 'q8': {'fixed_s': 1, 's_per_mpx_step': 3, 'pixel_exponent': 1}}}))
        file_path = None
        if name.startswith('file-'):
            pictures = home / 'pictures'
            pictures.mkdir(mode=0o700)
            file_path = pictures / ('synthetic.webp' if name == 'file-webp' else 'synthetic.png')
            if name != 'file-missing':
                file_path.write_bytes(b'synthetic image file bytes\n')
            if name in ('file-outside', 'file-symlink-outside'):
                outside = home / 'synthetic-private.png'
                outside.write_bytes(b'synthetic unrelated file bytes\n')
                if name == 'file-outside':
                    file_path = outside
                else:
                    file_path.unlink()
                    file_path.symlink_to(outside)
            elif name == 'file-symlink-inside':
                link = pictures / 'synthetic-link.jpg'
                link.symlink_to(file_path)
                file_path = link
            elif name == 'file-upload-root-symlink':
                external = home / 'synthetic-uploads'
                external.mkdir(mode=0o700)
                (pictures / '_uploads').symlink_to(external, target_is_directory=True)
                file_path = external / 'synthetic.png'
                file_path.write_bytes(b'synthetic image file bytes\n')
            elif name == 'file-directory':
                file_path.unlink()
                file_path.mkdir(mode=0o700)
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
                if name.startswith('upload-'):
                    method, path = 'POST', '/upload'
                    extension = 'webp' if name == 'upload-webp' else 'exe' if name == 'upload-bad-type' else 'PNG'
                    body = (b'--synthetic-boundary\r\nContent-Disposition: form-data; name="file"; filename="synthetic.' +
                            extension.encode() + b'"\r\n\r\nsynthetic upload bytes\r\n\r\n--synthetic-boundary--\r\n')
                    if name == 'upload-empty':
                        body = b'--synthetic-boundary--\r\n'
                    elif name == 'upload-nonutf8-name':
                        body = body.replace(b'synthetic.PNG', b'synthetic-\xff.PNG')
                    if name != 'upload-no-boundary':
                        headers += [('Content-Type', 'multipart/form-data; boundary="synthetic-boundary"')]
                    headers += [('Content-Length', str(25 * 1024 * 1024 + 1) if name == 'upload-oversized' else str(len(body)))]
                    if name == 'upload-oversized':
                        body = None
                elif name.startswith('file-'):
                    path = '/file?' + urllib.parse.urlencode({'path': str(file_path)})
                elif name.startswith('estimate-'):
                    query = {
                        'estimate-q8': '', 'estimate-q4': 'model=q4&width=512&height=768&variations=2',
                        'estimate-edit': 'edit=1&strength=0.55&width=512&height=512',
                        'estimate-nonaligned': 'width=257&height=259',
                        'estimate-bad-size': 'width=200', 'estimate-bad-count': 'variations=5',
                        'estimate-klein-missing': 'model=klein-q8',
                        'estimate-klein-ready': 'model=klein-q8&variations=2',
                        'estimate-klein-gated': 'model=klein-q8&width=768&height=768',
                        'estimate-forced-sdcpp': 'model=klein-q8',
                        'estimate-cpu-profile': 'width=512&height=512',
                        'estimate-cpu-bad-hash': 'width=512&height=512',
                        'estimate-calibrated': 'width=512&height=512',
                        'estimate-unicode': 'width=' + urllib.parse.quote('٥١٢') + '&height=512',
                    }[name]
                    path = '/estimate?' + query
                elif name.startswith('models'):
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
                    try:
                        response = connection.getresponse()
                    except http.client.RemoteDisconnected:
                        return {'closed_without_response': True}
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
                    if name in ('upload-png', 'upload-webp', 'upload-nonutf8-name') and response.status == 200:
                        uploaded = Path(value['body']['path'])
                        expected_suffix = '.webp' if name == 'upload-webp' else '.png'
                        if uploaded.parent != home / 'pictures/_uploads' or not re.fullmatch(
                                r'[0-9a-f]{12}4[0-9a-f]{3}[89ab][0-9a-f]{15}' + re.escape(expected_suffix), uploaded.name):
                            raise AssertionError('Upload name is not a private UUID-v4 file')
                        if uploaded.read_bytes() != b'synthetic upload bytes\r\n':
                            raise AssertionError('Upload contents changed')
                        value['uploaded_mode'] = uploaded.stat().st_mode & 0o777
                        value['body']['path'] = str(uploaded.parent / ('<upload:v4>' + expected_suffix))
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
