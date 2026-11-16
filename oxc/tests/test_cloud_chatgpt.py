"""#252: the experimental, opt-in ChatGPT subscription provider (`chatgpt/<model>`).

Fake auth server and fake backend on loopback only; no real sign-in, no real API call.
"""
import base64
import io
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
        self.events = None  # None = a text delta, then response.completed
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
        events = self.events or [{'type': 'response.created', 'response': {'id': 'resp_sub'}},
                  {'type': 'response.output_text.delta', 'output_index': 0, 'delta': 'Done.'},
                  {'type': 'response.completed', 'response': done}]
        self.events = None
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


def multi_tool_events(completed_output, deltas='full'):
    """A subscription turn with three parallel tool calls, as the backend streams it: each
    call is announced with empty arguments, filled by deltas and closed by output_item.done;
    with ``store: false`` the final response.completed carries ``completed_output``.
    ``deltas``: ``full`` = all arguments as deltas plus function_call_arguments.done;
    ``partial`` = only the first half as a delta, then .done; ``none`` = no argument events
    at all, the arguments arrive only in output_item.done (#450)."""
    reasoning = {'type': 'reasoning', 'id': 'rs_1', 'summary': [], 'encrypted_content': 'opaque-synthetic'}
    calls = [{'type': 'function_call', 'id': 'fc_%d' % n, 'call_id': 'call_%d' % n, 'name': name,
              'arguments': args, 'status': 'completed'}
             for n, (name, args) in enumerate([('bash', '{"command":"ls -la"}'),
                                               ('read', '{"path":"a.txt"}'),
                                               ('read', '{"path":"b.txt"}')], 1)]
    events = [{'type': 'response.created', 'response': {'id': 'resp_sub'}},
              {'type': 'response.output_item.added', 'output_index': 0, 'item': dict(reasoning, encrypted_content=None)},
              {'type': 'response.output_item.done', 'output_index': 0, 'item': reasoning}]
    for index, call in enumerate(calls, 1):
        events.append({'type': 'response.output_item.added', 'output_index': index,
                       'item': dict(call, arguments='', status='in_progress')})
        half = len(call['arguments']) // 2
        pieces = {'full': (call['arguments'][:half], call['arguments'][half:]),
                  'partial': (call['arguments'][:half],), 'none': ()}[deltas]
        for piece in pieces:
            events.append({'type': 'response.function_call_arguments.delta', 'output_index': index,
                           'item_id': call['id'], 'delta': piece})
        if deltas != 'none':
            events.append({'type': 'response.function_call_arguments.done', 'output_index': index,
                           'item_id': call['id'], 'arguments': call['arguments']})
        events.append({'type': 'response.output_item.done', 'output_index': index, 'item': call})
    output = [reasoning] + calls if completed_output else []
    events.append({'type': 'response.completed', 'response': {
        'id': 'resp_sub', 'status': 'completed', 'output': output,
        'usage': {'input_tokens': 7, 'output_tokens': 3}}})
    return events, reasoning, calls


def assemble(body):
    """The assistant message a streaming client (oxa) builds from the chat SSE."""
    message, calls = {'role': 'assistant', 'content': None}, {}
    for line in body.decode().splitlines():
        if not line.startswith('data: ') or line == 'data: [DONE]':
            continue
        for choice in json.loads(line[6:]).get('choices') or []:
            delta = choice.get('delta') or {}
            if delta.get('content'):
                message['content'] = (message['content'] or '') + delta['content']
            for part in delta.get('tool_calls') or []:
                slot = calls.setdefault(part['index'], {'id': None, 'type': 'function',
                                                        'function': {'name': '', 'arguments': ''}})
                slot['id'] = part.get('id') or slot['id']
                function = part.get('function') or {}
                slot['function']['name'] += function.get('name') or ''
                slot['function']['arguments'] += function.get('arguments') or ''
    if calls:
        message['tool_calls'] = [calls[index] for index in sorted(calls)]
    return message


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

    def test_shipped_catalog_lists_chatgpt_models_only_after_sign_in(self):
        shipped = json.loads((Path(cloud_chatgpt.__file__).resolve().parents[3] / 'docs' / 'cloud-models.json').read_text())
        mine = {alias: raw for alias, raw in shipped.items() if raw.get('kind') == 'remote/chatgpt'}
        self.assertTrue(mine)
        for alias, raw in mine.items():
            item = cloud.entry(alias, raw)
            self.assertEqual((item['provider'], item['base_url'], item['cost_class']),
                             ('chatgpt', 'https://chatgpt.com/backend-api/codex', 'subscription'), alias)
            self.assertEqual(cloud.path_for(item), '/backend-api/codex/responses')
        value = {'cloud': {'enabled': True, 'secrets_file': str(self.secrets), 'models': shipped}}
        self.assertFalse(set(mine) & set(cloud.entries(value)))
        self.assertTrue(set(shipped) - set(mine) <= set(cloud.entries(value)))
        self.sign_in()
        self.assertTrue(set(mine) <= set(cloud.entries(value)))

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
        self.assertEqual(sent['headers']['openai-beta'], 'responses=experimental')
        self.assertEqual(sent['headers']['originator'], 'oxc')
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

    def test_streamed_multi_tool_turn_continues(self):
        # #450: several tool calls in one streamed turn, then the continuation request.
        cases = [(c, s, d) for d in ('full', 'partial', 'none')
                 for c, s in ((True, True), (False, True), (False, False))]
        for completed_output, stream, deltas in cases:
            with self.subTest(completed_output=completed_output, stream=stream, deltas=deltas):
                self.sign_in()
                self.start()
                events, reasoning, calls = multi_tool_events(completed_output, deltas)
                self.backend.events = events
                tools = [{'type': 'function', 'function': {'name': n, 'parameters': {'type': 'object'}}}
                         for n in ('bash', 'read')]
                messages = [{'role': 'user', 'content': 'inspect'}]
                status, body = self.call('POST', '/v1/chat/completions', {
                    'model': MODEL, 'max_tokens': 100, 'messages': messages, 'tools': tools, 'stream': stream})
                self.assertEqual(status, 200, body)
                if stream:
                    self.assertIn(b'"finish_reason": "tool_calls"', body)
                    assistant = assemble(body)
                else:
                    choice = json.loads(body)['choices'][0]
                    self.assertEqual(choice['finish_reason'], 'tool_calls')
                    assistant = choice['message']
                self.assertEqual([(c['id'], c['function']['arguments']) for c in assistant['tool_calls']],
                                 [(c['call_id'], c['arguments']) for c in calls])
                self.sign_in(access='synthetic-rotated-access-token-0000')
                messages += [assistant] + [{'role': 'tool', 'tool_call_id': c['call_id'], 'content': 'ok'}
                                           for c in calls]
                status, body = self.call('POST', '/v1/chat/completions', {
                    'model': MODEL, 'max_tokens': 100, 'messages': messages, 'tools': tools, 'stream': True})
                self.assertEqual(status, 200, body)
                sent = self.backend.requests[-1]['body']['input']
                self.assertIn(reasoning, sent)
                for call in calls:
                    self.assertIn(call, sent)
                self.assertNotIn(b'synthetic-rotated-access-token', body)
                self.assertNotIn(b'synthetic-access-token', body)
                self.stack.close()
                self.stack = None

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

    def test_lan_guest_is_refused_the_subscription_even_with_the_share_opt_in(self):
        self.sign_in()
        self.start()
        value = json.loads(self.stack.config_path.read_text())
        value['cloud']['share_with_lan'] = True
        self.stack.config_path.write_text(json.dumps(value))

        def handle(handler, inner):
            handler.share_client = 'guest'
            return inner()
        self.stack.server.share_listener = SimpleNamespace(
            controller=SimpleNamespace(opts={'max_tokens': 100}, handle=handle))
        self.assertNotIn(MODEL, [row['id'] for row in self.rows()])
        status, body = self.chat(model=MODEL)
        self.assertEqual(status, 403)
        error = json.loads(body)['error']
        self.assertEqual(error['code'], 'cloud_not_shared')
        self.assertIn('subscription', error['message'])
        self.assertEqual(self.backend.requests, [])
        self.assertFalse(self.auth.calls)  # the account's tokens are never even refreshed for a guest

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

    def test_codex_route_is_the_built_in_default_with_pinned_sourced_constants(self):
        # Owner decision (#252): the Codex sign-in route ships its public client identifier the
        # way other open-source harnesses do.  Each constant is pinned here, sits next to a comment naming
        # its source file, and is documented with its source URL in docs/CHATGPT-SIGNIN.md.
        pinned = {'CLIENT_ID': 'app_EMoamEEZ73f0CkXaXp7hrann', 'AUTH_URL': 'https://auth.openai.com',
                  'REDIRECT_PORT': 1455, 'REDIRECT_PATH': '/auth/callback',
                  'SCOPE': 'openid profile email offline_access', 'ORIGINATOR': 'oxc'}
        source_lines = Path(cloud_chatgpt.__file__).read_text().splitlines()
        doc = (Path(cloud_chatgpt.__file__).resolve().parents[3] / 'docs' / 'CHATGPT-SIGNIN.md').read_text()
        for name, expected in pinned.items():
            self.assertEqual(getattr(cloud_chatgpt, name), expected, name)
            line = next(i for i, text in enumerate(source_lines) if text.startswith(name + ' = '))
            comment = ' '.join(source_lines[max(0, line - 3):line + 1])
            self.assertRegex(comment, r'# source: S1 packages/core/src/plugin/provider/openai\.ts L\d+', name)
        for url in ('https://github.com/sst/opencode/blob/6c5ef96af4928524e1daa24d4a2c0d9c279cdefb/'
                    'packages/core/src/plugin/provider/openai.ts#L18',
                    'https://github.com/badlogic/pi-mono/blob/463e17f8dc08a76a1e24e55db37f88de81b4b118/'
                    'packages/ai/src/auth/oauth/openai-codex.ts#L22'):
            self.assertIn(url, doc)
        self.assertIn(pinned['CLIENT_ID'], doc)
        # Without cloud.chatgpt.client_id the built-in route is used; the setting overrides it.
        self.assertEqual(cloud_chatgpt.client_id({}), pinned['CLIENT_ID'])
        self.assertEqual(cloud_chatgpt.client_id({'cloud': {'chatgpt': {'client_id': CLIENT}}}), CLIENT)
        query = dict(urllib.parse.parse_qsl(urllib.parse.urlsplit(
            cloud_chatgpt.Flow(cloud_chatgpt.client_id({})).authorize_url()).query))
        self.assertEqual(query['client_id'], pinned['CLIENT_ID'])
        self.assertEqual(query['redirect_uri'], 'http://localhost:1455/auth/callback')
        self.assertEqual((query['id_token_add_organizations'], query['originator']), ('true', 'oxc'))
        # The sources' UI flag is named after a third-party client; shipped code names none (O-151).
        self.assertNotIn('codex_cli_simplified_flow', query)
        # Login works out of the box (fake auth server) and says the route is experimental.
        config = self.tmp / 'runtime.json'
        config.write_text(json.dumps({'cloud': {'secrets_file': str(self.secrets),
                                                'chatgpt': {'auth_url': self.auth.base}}}))
        flow = cloud_chatgpt.Flow(cloud_chatgpt.client_id({}), self.auth.base)
        state_url = dict(urllib.parse.parse_qsl(urllib.parse.urlsplit(flow.authorize_url()).query))
        err = io.StringIO()
        with patch('builtins.input', return_value=state_url['redirect_uri'] + '?code=c&state=' + state_url['state']), \
                patch('sys.stderr', new=err), patch('builtins.print', side_effect=lambda *a, **k: err.write(' '.join(map(str, a)))):
            self.assertEqual(cloud_chatgpt.main(['login', '--paste', '--config', str(config)], flow=flow), 0)
        self.assertIn('EXPERIMENTAL', err.getvalue())
        self.assertIn('undocumented', err.getvalue())
        self.assertEqual(self.auth.calls[-1]['form']['client_id'], pinned['CLIENT_ID'])
        # Refresh uses the built-in client too and sends what the sources send (no scope).
        self.sign_in(expires_in=-10)
        cloud_chatgpt.credentials(self.secrets, {'cloud': {'chatgpt': {'auth_url': self.auth.base}}})
        form = self.auth.calls[-1]['form']
        self.assertEqual((form['grant_type'], form['client_id']), ('refresh_token', pinned['CLIENT_ID']))
        self.assertNotIn('scope', form)

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
