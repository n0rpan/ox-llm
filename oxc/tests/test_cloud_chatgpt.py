"""#252: the experimental, opt-in ChatGPT subscription provider (`chatgpt/<model>`).

Fake auth server and fake backend on loopback only; no real sign-in, no real API call.
"""
import base64
import json
import os
import stat
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch
import urllib.parse

from oxc.runtime import cloud, cloud_chatgpt
from mock_cloud import _Base
from test_cloud_remote import CloudCase

MODEL = 'chatgpt/gpt-sub'
ACCESS = 'synthetic-access-token-0123456789abcdef'
REFRESH = 'synthetic-refresh-token-fedcba9876543210'
ACCOUNT = 'acct-synthetic-1'


def jwt(claims):
    def part(value):
        return base64.urlsafe_b64encode(json.dumps(value).encode()).rstrip(b'=').decode()
    return part({'alg': 'none'}) + '.' + part(claims) + '.sig'


def id_token(account=ACCOUNT):
    return jwt({'https://api.openai.com/auth': {'chatgpt_account_id': account}})


class FakeBackend(_Base):
    """The subscription Responses backend: SSE only, rate-limit headers on every reply."""

    def __init__(self):
        super().__init__()
        self.output = None  # None = a plain text answer
        self.start()

    def handle(self, handler, method):
        body = json.loads(handler.rfile.read(int(handler.headers.get('Content-Length', '0'))))
        self.requests.append({'path': handler.path, 'body': body,
                              'headers': {k.lower(): v for k, v in handler.headers.items()}})
        if not body.get('stream'):
            return self._json(handler, 400, {'detail': 'Stream must be set to true'})
        done = {'id': 'resp_sub', 'status': 'completed', 'output': self.output or [
            {'type': 'message', 'id': 'msg', 'role': 'assistant', 'status': 'completed',
             'content': [{'type': 'output_text', 'text': 'Done.'}]}],
            'usage': {'input_tokens': 7, 'output_tokens': 3}}
        events = [{'type': 'response.created', 'response': {'id': 'resp_sub'}},
                  {'type': 'response.output_text.delta', 'output_index': 0, 'delta': 'Done.'},
                  {'type': 'response.completed', 'response': done}]
        handler.send_response(200)
        handler.send_header('Content-Type', 'text/event-stream')
        handler.send_header('x-sub-primary-used-percent', '42.5')
        handler.send_header('x-sub-primary-reset-after-seconds', '600')
        handler.send_header('x-sub-primary-window-minutes', '300')
        handler.send_header('Connection', 'close')
        handler.end_headers()
        for event in events:
            handler.wfile.write(('event: %s\ndata: %s\n\n' % (event['type'], json.dumps(event))).encode())
        handler.close_connection = True


class FakeAuth:
    """The token endpoint: authorization_code and refresh_token grants."""

    def __init__(self):
        self.calls = []
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_POST(self):
                raw = self.rfile.read(int(self.headers.get('Content-Length', '0'))).decode()
                form = dict(urllib.parse.parse_qsl(raw))
                owner.calls.append({'path': self.path, 'form': form})
                time.sleep(0.05)  # widen the race window for the single-refresher test
                value = {'access_token': 'new-access-%d' % len(owner.calls),
                         'refresh_token': 'new-refresh-%d' % len(owner.calls),
                         'id_token': id_token(), 'expires_in': 3600}
                data = json.dumps(value).encode()
                self.send_response(200)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(data)))
                self.end_headers()
                self.wfile.write(data)

        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.base = 'http://127.0.0.1:%d' % self.server.server_port
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()


CLIENT = 'oxllm-test-client'  # synthetic stand-in for the client registered for ox-llm


def models():
    return {MODEL: {'kind': 'remote/chatgpt', 'upstream_model': 'gpt-sub',
                    'context_window': 200000, 'max_output': 4096}}


class Base(CloudCase):
    def setUp(self):
        super().setUp()
        self.backend = FakeBackend()
        self.addCleanup(self.backend.close)
        self.auth = FakeAuth()
        self.addCleanup(self.auth.close)
        self.tokens = self.tmp / 'chatgpt-auth.json'

    def sign_in(self, expires_in=3600, access=ACCESS):
        cloud_chatgpt.store(self.tokens, {'access_token': access, 'refresh_token': REFRESH,
                                          'account_id': ACCOUNT, 'expires_at': time.time() + expires_in})

    def start(self, budget=None, extra=None, **kwargs):
        kwargs.setdefault('models', models())
        kwargs.setdefault('mock', self.backend)
        stack = CloudCase.start(self, budget=budget, base_suffix='/api', **kwargs)
        value = json.loads(stack.config_path.read_text())
        value['cloud']['chatgpt'] = {'auth_url': self.auth.base, 'client_id': CLIENT, **(extra or {})}
        if budget is None:
            value['cloud'].pop('daily_budget_usd', None)
        stack.config_path.write_text(json.dumps(value))
        return stack


class Subscription(Base):
    def test_not_listed_without_sign_in_and_not_while_cloud_is_off(self):
        self.start()
        self.assertNotIn(MODEL, [row['id'] for row in self.rows()])
        self.assertEqual(self.chat(model=MODEL)[0], 404)
        self.stack.close()
        self.sign_in()
        self.start(enabled=False)
        self.assertNotIn(MODEL, [row['id'] for row in self.rows()])
        self.assertEqual(self.backend.requests, [])

    def test_alias_must_carry_the_chatgpt_prefix(self):
        raw = models()[MODEL]
        self.assertIsNotNone(cloud.entry(MODEL, {**raw, 'base_url': 'https://example.invalid/api'}))
        self.assertIsNone(cloud.entry('gpt-sub', {**raw, 'base_url': 'https://example.invalid/api'}))

    def test_request_shape_subscription_ledger_and_no_usd_budget(self):
        self.sign_in()
        self.start()
        self.assertIn(MODEL, [row['id'] for row in self.rows()])
        status, body = self.call('POST', '/v1/chat/completions', {
            'model': MODEL, 'max_tokens': 100, 'messages': [
                {'role': 'system', 'content': 'Be brief.'}, {'role': 'user', 'content': 'hello'}]})
        self.assertEqual(status, 200, body)
        self.assertEqual(json.loads(body)['choices'][0]['message']['content'], 'Done.')
        sent = self.backend.requests[0]
        self.assertEqual(sent['path'], '/api/responses')
        self.assertEqual(sent['headers']['authorization'], 'Bearer ' + ACCESS)
        self.assertEqual(sent['headers']['chatgpt-account-id'], ACCOUNT)
        self.assertIs(sent['body']['store'], False)
        self.assertIs(sent['body']['stream'], True)
        self.assertEqual(sent['body']['instructions'], 'Be brief.')
        self.assertNotIn('max_output_tokens', sent['body'])
        self.assertEqual(sent['body']['input'], [{'role': 'user', 'content': 'hello'}])
        row = self.ledger_rows()[-1]
        self.assertEqual(row['remote_provider'], 'chatgpt')
        self.assertEqual(row['cost_class'], 'subscription')
        self.assertEqual(row['cost_usd'], 0)
        self.assertEqual((row['prompt_tokens'], row['completion_tokens']), (7, 3))
        ledger = self.ledger_text()
        self.assertNotIn(ACCESS, ledger)
        self.assertNotIn(REFRESH, ledger)

    def test_streamed_reply_and_allowance_in_status(self):
        self.sign_in()
        self.start()
        status, body = self.chat(model=MODEL, stream=True)
        self.assertEqual(status, 200, body)
        self.assertIn(b'Done.', body)
        self.assertIn(b'[DONE]', body)
        status, raw = self.call('GET', '/_local/status')
        self.assertEqual(status, 200)
        self.assertNotIn(ACCESS.encode(), raw)
        self.assertNotIn(REFRESH.encode(), raw)
        sub = json.loads(raw)['cloud']['subscription']
        self.assertTrue(sub['signed_in'])
        self.assertEqual(sub['limits']['primary']['used_percent'], 42.5)
        self.assertEqual(sub['limits']['primary']['window_minutes'], 300)
        self.assertGreater(sub['limits']['primary']['resets_at'], time.time())

    def test_daily_request_cap_refuses_before_upstream(self):
        self.sign_in()
        self.start(extra={'daily_requests': 1})
        self.assertEqual(self.chat(model=MODEL)[0], 200)
        status, body = self.chat(model=MODEL)
        self.assertEqual(status, 429, body)
        self.assertEqual(json.loads(body)['error']['code'], 'subscription_cap_exceeded')
        self.assertEqual(len(self.backend.requests), 1)

    def test_daily_token_cap_refuses(self):
        self.sign_in()
        self.start(extra={'daily_tokens': 5})
        self.assertEqual(self.chat(model=MODEL)[0], 200)  # 10 tokens recorded
        self.assertEqual(self.chat(model=MODEL)[0], 429)

    def test_expired_token_is_refreshed_once_and_stored_owner_only(self):
        self.sign_in(expires_in=-10)
        self.start()
        results = []
        threads = [threading.Thread(target=lambda: results.append(self.chat(model=MODEL)[0]))
                   for _ in range(3)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
        self.assertEqual(results, [200, 200, 200])
        refreshes = [c for c in self.auth.calls if c['form'].get('grant_type') == 'refresh_token']
        self.assertEqual(len(refreshes), 1, self.auth.calls)
        self.assertEqual(refreshes[0]['form']['refresh_token'], REFRESH)
        self.assertEqual(stat.S_IMODE(os.stat(self.tokens).st_mode), 0o600)
        saved = json.loads(self.tokens.read_text())
        self.assertEqual(saved['access_token'], 'new-access-1')
        self.assertEqual(saved['refresh_token'], 'new-refresh-1')
        self.assertTrue(all(r['headers']['authorization'] == 'Bearer new-access-1'
                            for r in self.backend.requests))

    def test_tool_continuation_survives_an_access_token_refresh(self):
        # Codex review: the continuation cache was keyed by the access token, so a routine
        # refresh between a tool call and its result made the loop fail with 409.
        self.sign_in()
        self.start()
        reasoning = {'type': 'reasoning', 'id': 'rs_1', 'summary': [],
                     'encrypted_content': 'opaque-synthetic'}
        call = {'type': 'function_call', 'id': 'fc_1', 'call_id': 'call_1', 'name': 'ls',
                'arguments': '{}'}
        self.backend.output = [reasoning, call]
        tools = [{'type': 'function', 'function': {'name': 'ls', 'parameters': {'type': 'object'}}}]
        messages = [{'role': 'user', 'content': 'list'}]
        status, body = self.call('POST', '/v1/chat/completions', {
            'model': MODEL, 'max_tokens': 100, 'messages': messages, 'tools': tools})
        self.assertEqual(status, 200, body)
        assistant = json.loads(body)['choices'][0]['message']
        self.sign_in(access='synthetic-rotated-access-token-0000')  # the token was refreshed
        self.backend.output = None
        messages += [assistant, {'role': 'tool', 'tool_call_id': 'call_1', 'content': 'a.txt'}]
        status, body = self.call('POST', '/v1/chat/completions', {
            'model': MODEL, 'max_tokens': 100, 'messages': messages, 'tools': tools})
        self.assertEqual(status, 200, body)
        sent = self.backend.requests[-1]
        self.assertEqual(sent['headers']['authorization'], 'Bearer synthetic-rotated-access-token-0000')
        self.assertIn(reasoning, sent['body']['input'])

    def test_credential_os_error_is_a_clean_503(self):
        # Review: an OSError while refreshing (here: the lock file is a link) escaped forward().
        self.sign_in(expires_in=-10)
        os.symlink(self.tmp / 'nowhere', self.tmp / cloud_chatgpt.LOCK_FILE)
        self.start()
        status, body = self.chat(model=MODEL)
        self.assertEqual(status, 503, body)
        self.assertEqual(json.loads(body)['error']['code'], 'cloud_credentials_unavailable')
        self.assertEqual(self.backend.requests, [])

    def test_lan_guest_is_refused_even_when_signed_in(self):
        self.sign_in()
        self.start()

        def handle(handler, inner):
            handler.share_client = 'guest'
            return inner()
        self.stack.server.share_listener = SimpleNamespace(
            controller=SimpleNamespace(opts={'max_tokens': 100}, handle=handle))
        self.assertNotIn(MODEL, [row['id'] for row in self.rows()])
        status, body = self.chat(model=MODEL)
        self.assertEqual(status, 403)
        self.assertEqual(json.loads(body)['error']['code'], 'cloud_not_shared')
        self.assertEqual(self.backend.requests, [])

    def test_offline_refuses(self):
        self.sign_in()
        self.start(offline=True)
        self.assertNotEqual(self.chat(model=MODEL)[0], 200)
        self.assertEqual(self.backend.requests, [])


class Login(Base):
    def test_pkce_paste_login_and_logout(self):
        config = self.tmp / 'runtime.json'
        config.write_text(json.dumps({'cloud': {'secrets_file': str(self.secrets),
                                                'chatgpt': {'auth_url': self.auth.base, 'client_id': CLIENT}}}))
        flow = cloud_chatgpt.Flow(CLIENT, self.auth.base)
        url = urllib.parse.urlsplit(flow.authorize_url())
        query = dict(urllib.parse.parse_qsl(url.query))
        self.assertEqual(query['code_challenge_method'], 'S256')
        self.assertEqual(query['client_id'], CLIENT)
        redirect = query['redirect_uri'] + '?code=c0de&state=' + query['state']
        with patch('builtins.input', return_value=redirect), patch('builtins.print'):
            self.assertEqual(cloud_chatgpt.main(['login', '--paste', '--config', str(config)], flow=flow), 0)
        exchange = self.auth.calls[-1]['form']
        self.assertEqual(exchange['grant_type'], 'authorization_code')
        self.assertEqual(exchange['code'], 'c0de')
        self.assertEqual(exchange['client_id'], CLIENT)
        self.assertTrue(exchange['code_verifier'])
        saved = json.loads(self.tokens.read_text())
        self.assertEqual(saved['account_id'], ACCOUNT)
        self.assertEqual(stat.S_IMODE(os.stat(self.tokens).st_mode), 0o600)
        # A wrong state is refused and changes nothing.
        flow = cloud_chatgpt.Flow(CLIENT, self.auth.base)
        flow.authorize_url()
        with patch('builtins.input', return_value='http://localhost/cb?code=x&state=forged'), \
                patch('builtins.print'):
            self.assertNotEqual(cloud_chatgpt.main(['login', '--paste', '--config', str(config)], flow=flow), 0)
        self.assertEqual(json.loads(self.tokens.read_text()), saved)
        # Logout removes the file.
        with patch('builtins.print'):
            self.assertEqual(cloud_chatgpt.main(['logout', '--config', str(config)]), 0)
        self.assertFalse(self.tokens.exists())

    def test_no_borrowed_client_identity(self):
        # Codex review P1: login used another app's OAuth client id.  oxc ships none; without
        # cloud.chatgpt.client_id login, refresh and importing another app's login all refuse.
        source_text = Path(cloud_chatgpt.__file__).read_text()
        self.assertNotIn('app_EMoam', source_text)
        self.assertFalse(hasattr(cloud_chatgpt, 'CLIENT_ID'))
        config = self.tmp / 'runtime.json'
        config.write_text(json.dumps({'cloud': {'secrets_file': str(self.secrets),
                                                'chatgpt': {'auth_url': self.auth.base}}}))
        with patch('builtins.input', side_effect=AssertionError('no prompt')), patch('builtins.print'):
            self.assertEqual(cloud_chatgpt.main(['login', '--paste', '--config', str(config)]), 1)
        with self.assertRaises(SystemExit), patch('sys.stderr'):
            cloud_chatgpt.main(['login', '--import', str(self.tmp / 'x.json'), '--config', str(config)])
        self.assertEqual(self.auth.calls, [])
        self.assertFalse(self.tokens.exists())
        self.sign_in(expires_in=-10)
        with self.assertRaisesRegex(cloud_chatgpt.LoginError, 'client_id'):
            cloud_chatgpt.credentials(self.secrets, {'cloud': {'chatgpt': {'auth_url': self.auth.base}}})
        self.assertEqual(self.auth.calls, [])

    def test_login_refuses_a_relative_or_in_repository_destination(self):
        # Codex review: login wrote tokens wherever cloud.secrets_file pointed, unlike the
        # controller, which refuses relative paths and paths inside a source checkout.
        self.addCleanup(lambda: __import__('shutil').rmtree('relative', ignore_errors=True))
        checkout = self.tmp / 'checkout'
        (checkout / '.git').mkdir(parents=True)
        for raw in ('relative/cloud-secrets.json', str(checkout / 'sub/cloud-secrets.json')):
            config = self.tmp / 'runtime.json'
            config.write_text(json.dumps({'cloud': {'secrets_file': raw, 'chatgpt': {'client_id': CLIENT}}}))
            with patch('builtins.input', side_effect=AssertionError('no prompt')), patch('builtins.print'):
                self.assertEqual(cloud_chatgpt.main(['login', '--paste', '--config', str(config)]), 1, raw)
        self.assertEqual(list(checkout.rglob(cloud_chatgpt.TOKEN_FILE)), [])
        self.assertFalse(Path('relative').exists())

    def test_logout_waits_for_a_running_refresh_and_keeps_the_lock(self):
        # Codex review: logout during a refresh let the refresher re-create the login.
        self.sign_in()
        lock = self.tokens.with_name(cloud_chatgpt.LOCK_FILE)
        fd = os.open(lock, os.O_RDWR | os.O_CREAT, 0o600)
        import fcntl
        fcntl.flock(fd, fcntl.LOCK_EX)  # a refresher holds the lock
        done = threading.Event()
        worker = threading.Thread(target=lambda: (cloud_chatgpt.remove(self.tokens), done.set()))
        worker.start()
        self.assertFalse(done.wait(0.3))
        cloud_chatgpt.store(self.tokens, {'access_token': 'refreshed'})  # refresher finishes
        fcntl.flock(fd, fcntl.LOCK_UN)
        os.close(fd)
        worker.join(5)
        self.assertTrue(done.is_set())
        self.assertFalse(self.tokens.exists())
        self.assertTrue(lock.exists())
        with self.assertRaises(cloud_chatgpt.LoginError):
            cloud_chatgpt.credentials(self.secrets, {})

    def test_store_refuses_a_symlink(self):
        target = self.tmp / 'elsewhere'
        target.write_text('{}')
        os.symlink(target, self.tokens)
        with self.assertRaises(OSError):
            cloud_chatgpt.store(self.tokens, {'access_token': ACCESS})
        self.assertEqual(target.read_text(), '{}')
