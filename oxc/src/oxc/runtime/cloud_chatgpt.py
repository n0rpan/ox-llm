"""Experimental ChatGPT subscription provider (#252), opt-in.

UNDOCUMENTED UPSTREAM: this signs in with the public PKCE client of OpenAI's own command-line
agent and calls the ChatGPT subscription Responses backend.  OpenAI does not document this
route; it is tolerated and may break without notice.  The official "Sign in with ChatGPT"
program (#247) replaces it.

Nothing here is on by default: catalog entries of kind ``remote/chatgpt`` are listed only while
``cloud.enabled`` is true AND a sign-in exists (``oxc login chatgpt``).  The tokens live in an
owner-only file next to the cloud keys (``chatgpt-auth.json``), are never logged or printed, are
refreshed by one refresher at a time (process lock + ``flock``) and are removed by
``oxc logout chatgpt``.  Requests cost 0 USD (cost class ``subscription``); the daily USD budget
does not apply, optional ``cloud.chatgpt.daily_requests`` / ``daily_tokens`` caps do.

This module is standalone (stdlib only) so the ``oxc`` CLI can load it by file path.
"""
import argparse
import base64
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import secrets as _random
import sys
import threading
import time
import urllib.parse
import urllib.request

PROVIDER = 'chatgpt'
ALIAS_PREFIX = 'chatgpt/'
TOKEN_FILE = 'chatgpt-auth.json'
LOCK_FILE = 'chatgpt-auth.lock'
CLIENT_ID = 'app_EMoamEEZ73f0CkXaXp7hrann'  # the published public PKCE client (#252)
AUTH_URL = 'https://auth.openai.com'
REDIRECT_PORT = 1455
REDIRECT_PATH = '/auth/callback'
SCOPE = 'openid profile email offline_access'
REFRESH_MARGIN = 120  # seconds before expiry when a token is refreshed
MAX_BYTES = 64 * 1024
AUTH_CLAIM = 'https://api.openai.com/auth'
DEFAULT_INSTRUCTIONS = 'You are a helpful assistant.'

_refresh_lock = threading.Lock()
_limits_lock = threading.Lock()
_limits = {}
_LIMIT_HEADER = re.compile(r'x-[a-z0-9]+-(primary|secondary)-(used-percent|reset-after-seconds|window-minutes)')


class LoginError(Exception):
    pass


# ---------------------------------------------------------------- settings and paths
def _settings(value):
    cloud = value.get('cloud') if isinstance(value, dict) else None
    raw = cloud.get(PROVIDER) if isinstance(cloud, dict) else None
    return raw if isinstance(raw, dict) else {}


def auth_url(value):
    raw = _settings(value).get('auth_url')
    return raw.rstrip('/') if isinstance(raw, str) and raw.startswith(('https://', 'http://127.0.0.1')) else AUTH_URL


def token_path(secrets_file):
    """The token file next to the cloud keys file."""
    return Path(secrets_file).parent / TOKEN_FILE


def signed_in(secrets_file):
    try:
        return os.path.isfile(token_path(secrets_file)) and not os.path.islink(token_path(secrets_file))
    except OSError:
        return False


def caps(value):
    """(daily_requests, daily_tokens); None = no cap."""
    raw = _settings(value)
    out = []
    for name in ('daily_requests', 'daily_tokens'):
        number = raw.get(name)
        out.append(number if type(number) is int and number > 0 else None)
    return tuple(out)


# ---------------------------------------------------------------- token file
def load(path):
    """The stored tokens, or None.  Refuses links, other owners and group/other access."""
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    except OSError:
        return None
    try:
        info = os.fstat(fd)
        if info.st_uid != os.getuid() or info.st_mode & 0o077 or info.st_size > MAX_BYTES:
            return None
        data = os.read(fd, MAX_BYTES + 1)
    except OSError:
        return None
    finally:
        os.close(fd)
    try:
        value = json.loads(data)
    except (ValueError, UnicodeDecodeError):
        return None
    return value if isinstance(value, dict) else None


def store(path, tokens):
    """Write the tokens atomically, mode 0600; never through a symbolic link."""
    path = Path(path)
    if os.path.islink(path):
        raise OSError(errno.ELOOP, 'refusing to write through a symbolic link')
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    tmp = path.with_name('.%s.%d.tmp' % (path.name, os.getpid()))
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
    try:
        os.write(fd, json.dumps(tokens).encode())
        os.fsync(fd)
    finally:
        os.close(fd)
    os.replace(tmp, path)


def remove(path):
    for name in (Path(path), Path(path).with_name(LOCK_FILE)):
        try:
            os.unlink(name)
        except FileNotFoundError:
            pass


def _claims(token):
    try:
        part = token.split('.')[1]
        return json.loads(base64.urlsafe_b64decode(part + '=' * (-len(part) % 4)))
    except (AttributeError, IndexError, ValueError, UnicodeDecodeError):
        return {}


def account_of(*tokens):
    for token in tokens:
        claim = _claims(token).get(AUTH_CLAIM) if isinstance(token, str) else None
        if isinstance(claim, dict) and isinstance(claim.get('chatgpt_account_id'), str):
            return claim['chatgpt_account_id']
    return None


def _expiry(access, expires_in=None):
    if type(expires_in) in (int, float) and expires_in > 0:
        return time.time() + expires_in
    exp = _claims(access).get('exp')
    return float(exp) if type(exp) in (int, float) else time.time() + 3600


def _record(reply, previous=None):
    access, refresh = reply.get('access_token'), reply.get('refresh_token') or (previous or {}).get('refresh_token')
    if not isinstance(access, str) or not access or not isinstance(refresh, str) or not refresh:
        raise LoginError('the sign-in server returned no usable tokens')
    account = account_of(reply.get('id_token'), access) or (previous or {}).get('account_id')
    if not isinstance(account, str) or not account:
        raise LoginError('the sign-in did not name a ChatGPT account')
    return {'access_token': access, 'refresh_token': refresh, 'account_id': account,
            'expires_at': _expiry(access, reply.get('expires_in'))}


def _post_form(url, form, timeout=30):
    request = urllib.request.Request(url, data=urllib.parse.urlencode(form).encode(), method='POST',
                                     headers={'Content-Type': 'application/x-www-form-urlencoded',
                                              'Accept': 'application/json'})
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            value = json.loads(response.read(MAX_BYTES))
    except Exception:
        # Never echo the reply: it may carry tokens.
        raise LoginError('the sign-in server refused the request') from None
    if not isinstance(value, dict):
        raise LoginError('the sign-in server sent an invalid reply')
    return value


def credentials(secrets_file, value):
    """(access_token, account_id), refreshing first when it is about to expire.

    One refresher at a time: a process lock plus ``flock`` on a sibling lock file; after the
    lock the file is read again, so a waiter uses the token another refresher just stored.
    Raises LoginError (no token text in the message)."""
    path = token_path(secrets_file)
    tokens = load(path)
    if tokens is None:
        raise LoginError('not signed in (run `oxc login chatgpt`)')
    if _fresh(tokens):
        return tokens['access_token'], tokens['account_id']
    with _refresh_lock:
        fd = os.open(path.with_name(LOCK_FILE), os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX)
            tokens = load(path)
            if tokens is None:
                raise LoginError('not signed in (run `oxc login chatgpt`)')
            if not _fresh(tokens):
                if not isinstance(tokens.get('refresh_token'), str):
                    raise LoginError('the sign-in expired (run `oxc login chatgpt`)')
                reply = _post_form(auth_url(value) + '/oauth/token', {
                    'grant_type': 'refresh_token', 'refresh_token': tokens['refresh_token'],
                    'client_id': CLIENT_ID, 'scope': 'openid profile email'})
                tokens = _record(reply, tokens)
                store(path, tokens)
        finally:
            os.close(fd)
    return tokens['access_token'], tokens['account_id']


def _fresh(tokens):
    expires = tokens.get('expires_at')
    return (isinstance(tokens.get('access_token'), str) and isinstance(tokens.get('account_id'), str)
            and type(expires) in (int, float) and expires - REFRESH_MARGIN > time.time())


# ---------------------------------------------------------------- requests
def adapt(body):
    """The backend's constraints on top of the shared Responses translation: instructions from
    the system/developer messages, no output cap field, always streamed, never stored."""
    inputs, instructions = [], []
    for part in body.get('input') or []:
        if isinstance(part, dict) and part.get('role') in ('system', 'developer'):
            content = part.get('content')
            if isinstance(content, list):
                content = ''.join(block.get('text', '') for block in content if isinstance(block, dict))
            instructions.append(content or '')
        else:
            inputs.append(part)
    body['input'] = inputs
    body['instructions'] = '\n\n'.join(text for text in instructions if text) or DEFAULT_INSTRUCTIONS
    body.pop('max_output_tokens', None)
    body['stream'] = True
    body['store'] = False
    return body


def observe(headers):
    """Remember the allowance headers of the last reply (used percent, reset, window)."""
    seen = {}
    for name, raw in headers:
        match = _LIMIT_HEADER.fullmatch(name.lower())
        if not match:
            continue
        try:
            number = float(raw)
        except (TypeError, ValueError):
            continue
        window, field = match.groups()
        slot = seen.setdefault(window, {})
        if field == 'used-percent':
            slot['used_percent'] = number
        elif field == 'window-minutes':
            slot['window_minutes'] = int(number)
        else:
            slot['resets_at'] = int(time.time() + number)
    if seen:
        with _limits_lock:
            _limits.clear()
            _limits.update(seen)
            _limits['observed_at'] = int(time.time())


def status(secrets_file):
    with _limits_lock:
        limits = json.loads(json.dumps(_limits))
    return {'experimental': True, 'signed_in': signed_in(secrets_file) if secrets_file else False,
            'limits': limits}


# ---------------------------------------------------------------- sign-in (CLI)
class Flow:
    """One PKCE authorization: verifier, challenge, state and the token exchange."""

    def __init__(self, base=AUTH_URL, port=REDIRECT_PORT):
        self.base = base.rstrip('/')
        self.redirect = 'http://localhost:%d%s' % (port, REDIRECT_PATH)
        self.port = port
        self.verifier = _random.token_urlsafe(64)
        self.state = None

    def authorize_url(self):
        if self.state is None:
            self.state = _random.token_urlsafe(32)
        challenge = base64.urlsafe_b64encode(hashlib.sha256(self.verifier.encode()).digest()).rstrip(b'=').decode()
        query = {'response_type': 'code', 'client_id': CLIENT_ID, 'redirect_uri': self.redirect,
                 'scope': SCOPE, 'code_challenge': challenge, 'code_challenge_method': 'S256',
                 'state': self.state, 'id_token_add_organizations': 'true'}
        return self.base + '/oauth/authorize?' + urllib.parse.urlencode(query)

    def code_from(self, redirected):
        query = dict(urllib.parse.parse_qsl(urllib.parse.urlsplit(redirected.strip()).query))
        if not self.state or query.get('state') != self.state:
            raise LoginError('the sign-in reply does not match this login (state mismatch)')
        if not query.get('code'):
            raise LoginError('the sign-in reply has no authorization code')
        return query['code']

    def exchange(self, code):
        return _record(_post_form(self.base + '/oauth/token', {
            'grant_type': 'authorization_code', 'code': code, 'redirect_uri': self.redirect,
            'client_id': CLIENT_ID, 'code_verifier': self.verifier}))

    def wait_loopback(self, timeout=300):
        """Serve one redirect on 127.0.0.1; returns the redirected URL or None (port busy)."""
        from http.server import BaseHTTPRequestHandler, HTTPServer
        got = []

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_GET(self):
                got.append(self.path)
                body = b'Signed in. You can close this tab.'
                self.send_response(200)
                self.send_header('Content-Type', 'text/plain')
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        try:
            server = HTTPServer(('127.0.0.1', self.port), Handler)
        except OSError:
            return None
        server.timeout = timeout
        try:
            deadline = time.monotonic() + timeout
            while not any(p.startswith(REDIRECT_PATH) for p in got) and time.monotonic() < deadline:
                server.handle_request()
        finally:
            server.server_close()
        path = next((p for p in got if p.startswith(REDIRECT_PATH)), None)
        return None if path is None else 'http://localhost' + path


def import_login(source):
    """Read an existing CLI login file once (never written).  Accepts {tokens: {...}}."""
    try:
        with open(source, 'rb') as stream:
            value = json.loads(stream.read(MAX_BYTES))
    except (OSError, ValueError):
        raise LoginError('the import file cannot be read') from None
    tokens = value.get('tokens') if isinstance(value, dict) else None
    if not isinstance(tokens, dict):
        raise LoginError('the import file holds no ChatGPT login')
    reply = {'access_token': tokens.get('access_token'), 'refresh_token': tokens.get('refresh_token'),
             'id_token': tokens.get('id_token')}
    record = _record(reply, {'account_id': tokens.get('account_id')})
    return record


def _config_secrets(config):
    try:
        value = json.loads(Path(config).read_text())
    except (OSError, ValueError):
        value = {}
    value = value if isinstance(value, dict) else {}
    cloud = value.get('cloud') if isinstance(value.get('cloud'), dict) else {}
    raw = cloud.get('secrets_file')
    secrets_file = Path(raw) if isinstance(raw, str) and raw else Path(config).parent / 'cloud-secrets.json'
    return value, secrets_file


NOTICE = ('EXPERIMENTAL: this uses an undocumented ChatGPT subscription route that OpenAI tolerates '
          'but does not support; it may stop working without notice.')


def main(argv, flow=None):
    parser = argparse.ArgumentParser(prog='oxc', description=NOTICE)
    parser.add_argument('action', choices=('login', 'logout'))
    parser.add_argument('provider', nargs='?', default=PROVIDER, choices=(PROVIDER,))
    parser.add_argument('--paste', action='store_true', help='headless: paste the redirected URL instead of a loopback listener')
    parser.add_argument('--import', dest='source', help='read an existing CLI login file once (never written)')
    parser.add_argument('--config', default=None, help='runtime.json (default: the controller\'s)')
    args = parser.parse_args(argv)
    config = args.config or os.environ.get('LOCAL_LLM_CONFIG') or str(
        Path(os.environ.get('XDG_CONFIG_HOME') or Path.home() / '.config') / 'oxc/llm/runtime.json')
    value, secrets_file = _config_secrets(config)
    path = token_path(secrets_file)
    if args.action == 'logout':
        remove(path)
        print('Signed out of ChatGPT; the token file was removed.')
        return 0
    print(NOTICE, file=sys.stderr)
    try:
        if args.source:
            record = import_login(args.source)
        else:
            flow = flow or Flow(auth_url(value))
            url = flow.authorize_url()
            redirected = None
            if not args.paste:
                print('Open this URL in a browser to sign in:\n  ' + url, file=sys.stderr)
                redirected = flow.wait_loopback()
            if redirected is None:
                print('Open this URL in any browser, sign in, then paste the full address of the page '
                      'it redirects to (it may fail to load; that is fine):\n  ' + url, file=sys.stderr)
                redirected = input('Redirected URL: ')
            record = flow.exchange(flow.code_from(redirected))
        store(path, record)
    except LoginError as error:
        print('oxc: sign-in failed: %s' % error, file=sys.stderr)
        return 1
    except OSError as error:
        print('oxc: cannot store the sign-in: %s' % (error.strerror or type(error).__name__), file=sys.stderr)
        return 1
    print('Signed in to ChatGPT (account %s). Models: catalog entries of kind remote/chatgpt, named chatgpt/<model>;'
          ' cloud.enabled must be true.' % record['account_id'])
    return 0


if __name__ == '__main__':
    raise SystemExit(main(sys.argv[1:]))
