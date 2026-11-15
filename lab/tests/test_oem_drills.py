"""Install/rollback dispatch and a real HTTP loopback smoke check; no live service or model."""
from contextlib import redirect_stdout, redirect_stderr
import http.server
import importlib.util
import io
import json
import os
import subprocess
import sys
from pathlib import Path
import tempfile
import threading
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('oem_drills', ROOT / 'tools/oem_drills.py')
tool = importlib.util.module_from_spec(spec)
spec.loader.exec_module(tool)


class Smoke(unittest.TestCase):
    def setUp(self):
        self.requests = []
        self.state = 'ready'
        self.remote = False
        self.chat_status = 200
        self.answer = 'ready'
        self.models = True
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_GET(self):
                self.handle_request()

            def do_POST(self):
                self.handle_request()

            def handle_request(self):
                data = self.rfile.read(int(self.headers.get('Content-Length', 0)))
                owner.requests.append((self.command, self.path, json.loads(data) if data else None,
                                       self.headers.get('X-Local-AI-Origin')))
                status = 200
                if self.path == '/_local/status':
                    value = {'state': owner.state,
                             ('selected_alias' if owner.state == 'unloaded' else 'model'): 'synthetic-model'}
                elif self.path == '/v1/models':
                    value = {'data': [{'id': 'synthetic-model', 'remote': owner.remote}]} if owner.models else {}
                elif self.path == '/v1/chat/completions':
                    status = owner.chat_status
                    value = {'choices': [{'message': {'content': owner.answer}, 'finish_reason': 'stop'}]}
                else:
                    value = {'state': 'unloaded'}
                raw = json.dumps(value).encode()
                self.send_response(status)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)

        self.server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        self.url = 'http://127.0.0.1:%d' % self.server.server_port

    def run_smoke(self):
        output = io.StringIO()
        with redirect_stdout(output), redirect_stderr(output):
            code = tool.main(['--controller', self.url, 'smoke'])
        return code, output.getvalue()

    def test_local_smoke_marks_usage_and_preserves_loaded_model(self):
        code, output = self.run_smoke()
        self.assertEqual(code, 0, output)
        self.assertIn('PASS smoke', output)
        self.assertEqual([r[1] for r in self.requests], ['/_local/status', '/v1/models', '/v1/chat/completions'])
        self.assertTrue(all(r[3] == 'lab' for r in self.requests))
        self.assertEqual(self.requests[-1][2]['model'], 'synthetic-model')
        self.assertEqual(self.requests[-1][2]['max_tokens'], 8)

    def test_unloaded_smoke_allows_cold_start_and_unloads_even_on_chat_failure(self):
        for status in (200, 500):
            self.state, self.chat_status = 'unloaded', status
            with patch.object(tool.http.client, 'HTTPConnection',
                              wraps=http.client.HTTPConnection) as connection:
                code, _ = self.run_smoke()
            self.assertEqual(code, 0 if status == 200 else 1)
            self.assertEqual([call.kwargs['timeout'] for call in connection.call_args_list],
                             [10, 10, 180, 120])
            self.assertEqual(self.requests[-1][1], '/_local/unload')

    def test_remote_and_malformed_models_are_refused_before_chat(self):
        for remote, models in ((True, True), ('true', True), (False, False)):
            self.requests.clear()
            self.remote, self.models = remote, models
            self.assertEqual(self.run_smoke()[0], 1)
            self.assertFalse(any(r[0] == 'POST' for r in self.requests))

    def test_empty_reply_fails_and_backend_text_is_not_printed(self):
        self.answer = ''
        self.assertEqual(self.run_smoke()[0], 1)
        self.chat_status = 500
        self.answer = 'synthetic backend-private text'
        code, output = self.run_smoke()
        self.assertEqual(code, 1)
        self.assertNotIn(self.answer, output)

    def test_non_loopback_url_is_refused_before_any_network(self):
        import argparse
        for url in ('https://example.invalid', 'http://example.invalid', 'http://127.0.0.1/path',
                    'http://127.0.0.1?target=other'):
            with self.assertRaises(argparse.ArgumentTypeError):
                tool.controller_url(url)
        self.assertEqual(self.requests, [])


class Lifecycle(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory(prefix='oem-lifecycle-')
        self.addCleanup(directory.cleanup)
        root = Path(directory.name)
        env = {'LOCAL_AI_UPDATE_STATE': str(root / 'update/state.json'),
               'LOCAL_AI_STAGE_ROOTS': str(root / 'stages')}
        for name in ('HOME', 'XDG_STATE_HOME', 'XDG_DATA_HOME', 'XDG_CONFIG_HOME',
                     'XDG_CACHE_HOME', 'XDG_RUNTIME_DIR'):
            env[name] = str(root / name.lower())
        isolated = patch.dict(os.environ, env)
        isolated.start()
        self.addCleanup(isolated.stop)

    def test_missing_stage_and_rollback_keep_existing_refusal_guards(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(tool.release_update, '_run') as run:
            with redirect_stdout(io.StringIO()):
                self.assertEqual(tool.main(['install', str(Path(directory) / 'missing')]), 2)
                self.assertEqual(tool.main(['rollback']), 2)
            run.assert_not_called()


class SelectedFileIsolation(unittest.TestCase):
    def test_lifecycle_does_not_mutate_inherited_update_state_or_stages(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            state = root / 'inherited-update.json'
            sentinel = '{"current": null, "history": [], "sentinel": "owned-state"}\n'
            state.write_text(sentinel)
            stages = root / 'inherited-stages'
            stages.mkdir()
            marker = stages / 'sentinel'
            marker.write_text('owned-stage-root')
            env = dict(os.environ, LOCAL_AI_UPDATE_STATE=str(state), LOCAL_AI_STAGE_ROOTS=str(stages),
                       PYTHONPATH=str(ROOT / 'src') + os.pathsep + str(ROOT / 'tests'))
            for name in ('HOME', 'XDG_STATE_HOME', 'XDG_DATA_HOME', 'XDG_CONFIG_HOME',
                         'XDG_CACHE_HOME', 'XDG_RUNTIME_DIR'):
                env[name] = str(root / name.lower())
            result = subprocess.run([sys.executable, '-m', 'unittest',
                'test_oem_drills.Lifecycle.test_missing_stage_and_rollback_keep_existing_refusal_guards'],
                cwd=ROOT, env=env, capture_output=True, text=True, timeout=30)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertEqual(state.read_text(), sentinel)
            self.assertEqual(marker.read_text(), 'owned-stage-root')
            self.assertEqual(sorted(p.name for p in stages.iterdir()), ['sentinel'])
