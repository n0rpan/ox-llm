"""`oxc update` / `oxc rollback`: one step each, from a LOCAL staged release only.

Never fetches anything: the release must already be staged on disk (the O-119 stage format:
manifest.json, SEAL.json, the staged files, rollback/rollback.py and tools/install_staged_release.py,
optionally tools/stage_*_final.py with `precheck` and `materialize`).

update STAGE:
  1. check   seal (every staged byte as sealed), preimages (the live files are exactly what the stage
             expects to replace), the stage's own rollback (`rollback.py --check`), and the generator's
             `precheck` when the stage carries one. Any refusal: nothing changed (exit 2).
  2. record  `materialize` (new release dirs, content-addressed), then which services are active and
             which release the running controller runs (service-state.json, read by rollback.py).
  3. stop    stop the chat controller and Studio, and prove both are down (`is-active`). A stop that
             cannot be proven installs nothing: the services are put back as they were (exit 2).
  4. install the stage's installer `--apply` (atomic per file, restores itself on a partial failure;
             a refusal puts the services back as they were, exit 2).
  5. start   daemon-reload, start the chat controller (and Studio when it was running).
  6. ready   wait for /_local/status AND positive proof that the controller runs the staged release
             (its MainPID's cwd is the manifest's release_dir). Unknown identity is not ready. A stage
             without a release_dir (synthetic, identity-free) waits for /_local/status only.
  7. chat    one tiny chat (`X-Local-AI-Origin: lab`, so it never counts as use); the model is unloaded
             afterwards when none was loaded before.
  Any failure in 4-7 rolls back automatically with the stage's rollback.py (stop both, bytes and modes
  of the previous files, daemon-reload, start those active before), then proves the controller runs
  the previous release (the recorded one, or at least not the staged one) and says so (exit 1).
rollback:  runs the current stage's rollback.py (the release installed last): the same symmetric
           stop / restore / start, then the same identity-proven readiness wait. The previous release
           dirs were never removed.

State: $XDG_STATE_HOME/oxc/update/state.json (owner-only): the current stage and a short history.
Exit codes: 0 done, 1 update failed and the previous version is back, 2 refused (nothing changed),
3 needs attention (an automatic rollback did not finish; the message says what to run).
"""
import datetime as _dt
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

CHAT = os.environ.get('OXC_CHAT_URL', 'http://127.0.0.1:8092').rstrip('/')
CHAT_SERVICE, STUDIO_SERVICE = 'oxc-chat.service', 'prompt-studio-app.service'
SERVICES = (CHAT_SERVICE, STUDIO_SERVICE)
SERVICE_STATE = 'service-state.json'
SEAL_SKIP = ('receipts', 'retired.json', 'rollback/receipts', SERVICE_STATE, 'SEAL.json')
READY_SECONDS = float(os.environ.get('OXC_UPDATE_READY_SECONDS', '120'))
CHAT_SECONDS = float(os.environ.get('OXC_UPDATE_CHAT_SECONDS', '900'))
HISTORY = 20
PROC = '/proc'
UP_STATES = ('active', 'activating', 'reloading', 'deactivating')
LAB_HEADER = {'X-Local-AI-Origin': 'lab'}
ROLLBACK_EXIT = {3: 'the installed files are not the ones this release put there (a newer install or a hand edit)',
                 5: 'the saved copy of the previous version does not match its record',
                 7: 'this release was replaced by a newer one; roll that one back instead'}


class Refused(Exception):
    """Nothing was changed."""


class Failed(Exception):
    """Something was changed and has to be put back."""


def _xdg_state():
    value = os.environ.get('XDG_STATE_HOME') or ''
    return Path(value) if os.path.isabs(value) else Path.home() / '.local/state'


def state_path():
    return Path(os.environ.get('OXC_UPDATE_STATE') or _xdg_state() / 'oxc/update/state.json')


def stage_roots():
    """Where `oxc update` looks when no stage is named (OXC_STAGE_ROOTS, ':'-separated)."""
    value = os.environ.get('OXC_STAGE_ROOTS')
    if value:
        return [Path(p) for p in value.split(':') if p]
    data = os.environ.get('XDG_DATA_HOME') or ''
    data = Path(data) if os.path.isabs(data) else Path.home() / '.local/share'
    return [data / 'ai-stack/staged']


def sha_file(path):
    h = hashlib.sha256()
    try:
        with open(path, 'rb') as stream:
            for chunk in iter(lambda: stream.read(1 << 20), b''):
                h.update(chunk)
    except OSError:
        return None
    return h.hexdigest()


# --------------------------------------------------------------------------- stage facts
def manifest(stage):
    try:
        value = json.loads((Path(stage) / 'manifest.json').read_text())
    except FileNotFoundError:
        raise Refused(f'{stage} is not a staged release (no manifest.json).') from None
    except (OSError, ValueError):
        raise Refused(f'The release manifest in {stage} cannot be read.') from None
    if not isinstance(value, dict) or not isinstance(value.get('paths'), dict) or not value['paths']:
        raise Refused(f'The release manifest in {stage} has no file list.')
    return value


def release_name(stage, m=None):
    m = m or manifest(stage)
    return str(m.get('release') or Path(stage).name)


def seal_check(stage):
    stage = Path(stage)
    try:
        recorded = json.loads((stage / 'SEAL.json').read_text())['files']
    except (OSError, ValueError, KeyError, TypeError):
        raise Refused('The release is not sealed (SEAL.json missing or unreadable); stage it again.') from None
    actual = {}
    for path in sorted(stage.rglob('*')):
        rel = path.relative_to(stage).as_posix()
        if path.is_file() and '__pycache__' not in rel.split('/') and \
                not any(rel == s or rel.startswith(s + '/') for s in SEAL_SKIP):
            actual[rel] = sha_file(path)
    bad = sorted(set(recorded) ^ set(actual)) + sorted(k for k in recorded if k in actual and recorded[k] != actual[k])
    if bad:
        raise Refused('The staged release changed after it was sealed (%s); stage it again.' % ', '.join(bad[:3]))
    return len(actual)


ABSENT = 'absent'  # a manifest hash for a file that does not exist (one a stage creates or removes, v0.2.0)


def live_state(path):
    """sha256 of a live file; ABSENT when nothing is at the path; None when unreadable."""
    return ABSENT if not os.path.lexists(path) else sha_file(path)


def live_matches(m, which):
    """True when every live file is at the manifest's `which` hashes (preimage_sha256 / installed_sha256)."""
    want = m.get(which) or {}
    return bool(want) and all(live_state(path) == want.get(key) for key, path in m['paths'].items())


AGENT = '.local/bin/oxa'  # ox-llm's coding agent, installed beside oxc (O-152 identity refresh)


def refresh_agent_identity(home=None, chat=CHAT, run=subprocess.run, timeout=30, agent=None):
    """Best effort, never raises: `oxa identity --refresh` (writes ~/.oxa/agent/identity.md from the
    controller's models and limits, rewriting only when the bytes change); `--offline` when the controller
    does not answer. Returns 'absent', 'refreshed', 'refreshed-offline' or 'failed'."""
    agent = Path(agent) if agent else Path(home or Path.home()) / AGENT
    if not agent.is_file():
        return 'absent'
    env = {**os.environ, 'OXA_ENGINE_URL': chat}
    if home is not None:
        env['HOME'] = str(home)
    for args, result in (([], 'refreshed'), (['--offline'], 'refreshed-offline')):
        try:
            done = run([str(agent), 'identity', '--refresh', *args], capture_output=True, text=True, timeout=timeout,
                       stdin=subprocess.DEVNULL, env=env, check=False)
            if done.returncode == 0:
                return result
        except (OSError, subprocess.SubprocessError, ValueError):
            pass
    return 'failed'


def generator(stage):
    tools = sorted((Path(stage) / 'tools').glob('stage_*_final.py'))
    return tools[-1] if tools else None


def systemctl_of(m):
    value = (m.get('rollback') or {}).get('systemctl') if isinstance(m.get('rollback'), dict) else None
    return value or os.environ.get('OXC_SYSTEMCTL') or 'systemctl'


def _run(args, timeout=600):
    try:
        done = subprocess.run(args, capture_output=True, text=True, timeout=timeout, stdin=subprocess.DEVNULL)
    except (OSError, subprocess.SubprocessError) as error:
        return 127, '', type(error).__name__
    return done.returncode, done.stdout, done.stderr


def _last_line(text):
    lines = [line.strip() for line in (text or '').splitlines() if line.strip()]
    line = lines[-1] if lines else ''
    for prefix in ('REFUSED: ', 'ROLLBACK REFUSED '):
        if line.startswith(prefix):
            line = line[len(prefix):]
    return line[:300]


def python_tool(path, *args, timeout=600):
    return _run([sys.executable, '-I', '-B', str(path), *map(str, args)], timeout=timeout)


# --------------------------------------------------------------------------- controller
def http(url, payload=None, timeout=5, headers=None):
    data = None if payload is None else json.dumps(payload).encode()
    request = urllib.request.Request(url, data=data, headers={**({'Content-Type': 'application/json'} if data else {}),
                                                              **(headers or {})})
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            body = response.read(1 << 20)
            status = response.status
    except urllib.error.HTTPError as error:
        with error:
            body, status = error.read(1 << 20), error.code
    except (OSError, ValueError):
        return None, None
    try:
        return status, json.loads(body)
    except ValueError:
        return status, None


def controller_release(systemctl, proc=None):
    """The directory the chat controller's main process runs in (its release dir), or None when it
    cannot be told (no MainPID, systemctl failed, /proc unreadable)."""
    if not systemctl:
        return None
    code, out, _ = _run([systemctl, '--user', 'show', CHAT_SERVICE, '-p', 'MainPID'], timeout=15)
    try:
        pid = int(dict(line.split('=', 1) for line in out.splitlines() if '=' in line).get('MainPID', '0'))
    except ValueError:
        pid = 0
    if code != 0 or pid <= 0:
        return None
    try:
        return os.readlink(f'{proc or PROC}/{pid}/cwd')
    except OSError:
        return None


def runs_release(m, systemctl, proc=None):
    """None when it cannot be told, else whether the chat controller's process runs m['release_dir']."""
    release = m.get('release_dir')
    if not release:
        return None
    cwd = controller_release(systemctl, proc)
    return None if cwd is None else cwd == str(release)


def identity(systemctl, expect=None, avoid=None):
    """True only on positive proof: the controller runs `expect`, or (no `expect` known) runs a
    release that is not `avoid`. None = cannot be told (never success); False = the wrong release.
    With neither given (an identity-free synthetic stage) there is nothing to prove: True."""
    if not expect and not avoid:
        return True
    cwd = controller_release(systemctl)
    if cwd is None:
        return None
    return cwd == str(expect) if expect else cwd != str(avoid)


def wait_ready(chat, seconds, systemctl=None, expect=None, avoid=None, sleep=time.sleep, clock=time.monotonic):
    """Wait until the controller answers its status AND is proven to run the expected release."""
    end = clock() + seconds
    last = 'the local AI service did not answer'
    while True:
        code, body = http(chat + '/_local/status', timeout=3)
        if code == 200 and isinstance(body, dict):
            proven = identity(systemctl, expect, avoid)
            if proven is True:
                return body
            last = ('the service runs another version than expected' if proven is False
                    else 'which version the service runs could not be confirmed')
        elif code is not None:
            last = f'the local AI service answered {code}'
        if clock() >= end:
            raise Failed(f'{last} within {int(seconds)} s')
        sleep(1)


def chat_check(chat, status, timeout):
    loaded = status.get('state') in ('ready', 'loaded') or bool(status.get('loaded_model'))
    model = status.get('model') or status.get('selected_alias')
    body = {'temperature': 0, 'max_tokens': 8, 'stream': False,
            'messages': [{'role': 'user', 'content': 'Reply with one word: ready'}]}
    if isinstance(model, str) and model:
        body['model'] = model
    t0 = time.monotonic()
    code, reply = http(chat + '/v1/chat/completions', body, timeout=timeout, headers=LAB_HEADER)
    wall = time.monotonic() - t0
    try:
        choice = (reply.get('choices') or [{}])[0]
        content = (choice.get('message') or {}).get('content') or ''
        finish = choice.get('finish_reason')
    except (AttributeError, IndexError, TypeError):
        content, finish = '', None
    if not loaded:
        http(chat + '/_local/unload', {}, timeout=120, headers=LAB_HEADER)
    if code != 200 or not (content.strip() or finish):
        raise Failed('the test chat did not answer' + (f' (HTTP {code})' if code else ''))
    return round(wall, 1)


# --------------------------------------------------------------------------- state
def load_state():
    try:
        value = json.loads(state_path().read_text())
    except (OSError, ValueError):
        return {'current': None, 'history': []}
    if not isinstance(value, dict):
        return {'current': None, 'history': []}
    value.setdefault('current', None)
    value['history'] = [h for h in value.get('history') or [] if isinstance(h, dict)][-HISTORY:]
    return value


def save_state(value):
    path = state_path()
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    os.chmod(path.parent, 0o700)
    fd, tmp = tempfile.mkstemp(prefix='.state.', dir=path.parent)
    with os.fdopen(fd, 'w') as stream:
        stream.write(json.dumps(value, indent=1) + '\n')
    os.chmod(tmp, 0o600)
    os.replace(tmp, path)


def record(action, stage, result, **extra):
    value = load_state()
    entry = {'time': _dt.datetime.now().astimezone().isoformat(timespec='seconds'), 'action': action,
             'stage': str(stage) if stage else None, 'result': result, **extra}
    value['history'] = (value['history'] + [entry])[-HISTORY:]
    if action == 'update' and result == 'installed':
        value['current'] = {'stage': str(stage), 'since': entry['time']}
    elif result in ('rolled_back', 'auto_rolled_back'):
        value['current'] = None
    try:
        save_state(value)
    except OSError:
        pass
    return entry


# --------------------------------------------------------------------------- discovery
def _stages():
    for root in stage_roots():
        try:
            children = sorted(p for p in root.iterdir() if p.is_dir() and (p / 'manifest.json').is_file())
        except OSError:
            continue
        yield from children


def find_candidate():
    """The newest stage under the stage roots that is installable now (live == its preimages)."""
    found = []
    for stage in _stages():
        try:
            m = manifest(stage)
        except Refused:
            continue
        if (stage / 'retired.json').exists() or m.get('retired'):
            continue
        if live_matches(m, 'preimage_sha256'):
            found.append((stage.stat().st_mtime, stage))
    if not found:
        raise Refused('No staged release is ready to install. Stage one first, then run `oxc update <dir>`.')
    return sorted(found)[-1][1]


def find_current():
    """The installed stage: the recorded one if live still matches it, else a stage whose payload is live."""
    current = (load_state().get('current') or {}).get('stage')
    candidates = ([Path(current)] if current else []) + list(_stages())
    for stage in candidates:
        try:
            m = manifest(stage)
        except Refused:
            continue
        if (stage / 'rollback/rollback.py').is_file() and live_matches(m, 'installed_sha256'):
            return stage
    return None


# --------------------------------------------------------------------------- the two commands
def say(out, text):
    print(text, file=out, flush=True)


def record_services(stage, systemctl):
    """Write <stage>/service-state.json once (rollback.py reads `active`): which services run now and
    which release the chat controller runs (`chat_release`, None when it cannot be told)."""
    target = Path(stage) / SERVICE_STATE
    if target.exists():
        return
    code, out, _ = _run([systemctl, '--user', 'is-active', *SERVICES], timeout=60)
    states = out.split()
    if len(states) != len(SERVICES):
        raise Refused('Could not read which local AI services are running.')
    active = {s: st in ('active', 'activating', 'reloading') for s, st in zip(SERVICES, states)}
    value = {'time': time.strftime('%Y-%m-%dT%H:%M:%S%z'), 'states': dict(zip(SERVICES, states)), 'active': active,
             'chat_release': controller_release(systemctl) if active[CHAT_SERVICE] else None}
    fd = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o400)
    with os.fdopen(fd, 'w') as stream:
        stream.write(json.dumps(value, indent=2) + '\n')


def recorded_services(stage):
    try:
        value = json.loads((Path(stage) / SERVICE_STATE).read_text())
    except (OSError, ValueError):
        return {}
    return value if isinstance(value, dict) else {}


def forget_services(stage):
    """A refused update leaves no install-time record behind (a later attempt records afresh)."""
    try:
        (Path(stage) / SERVICE_STATE).unlink()
    except OSError:
        pass


def stop_services(systemctl):
    """Stop both services and prove they are down; raises Refused when that cannot be proven."""
    code, _, _ = _run([systemctl, '--user', 'stop', *SERVICES], timeout=180)
    if code != 0:
        raise Refused('the local AI service could not be stopped')
    _, out, _ = _run([systemctl, '--user', 'is-active', *SERVICES], timeout=60)
    states = out.split()
    if len(states) != len(SERVICES) or any(st in UP_STATES for st in states):
        raise Refused('the local AI service did not stop')


def restore_services(stage, systemctl):
    """Start again the services that ran before (after a refusal); False when that failed."""
    active = recorded_services(stage).get('active') or {}
    start = [s for s in SERVICES if active.get(s)]
    if not start:
        return True
    code, _, _ = _run([systemctl, '--user', 'start', *start], timeout=180)
    return code == 0


def precheck(stage, m, out):
    say(out, f'Checking the staged release {release_name(stage, m)} ...')
    seal_check(stage)
    if live_matches(m, 'installed_sha256'):
        raise Refused('This release is already installed.')
    for name in ('tools/install_staged_release.py', 'rollback/rollback.py'):
        if not (Path(stage) / name).is_file():
            raise Refused(f'The staged release has no {name}; stage it again.')
    code, _, err = python_tool(Path(stage) / 'rollback/rollback.py', '--check', timeout=120)
    if code != 0:
        raise Refused('Its rollback would not work (%s).' % (ROLLBACK_EXIT.get(code) or _last_line(err) or f'exit {code}'))
    tool = generator(stage)
    if tool is not None:
        code, _, err = python_tool(tool, 'precheck', stage)
    else:
        code, _, err = python_tool(Path(stage) / 'tools/install_staged_release.py', stage, '--dry-run')
    if code != 0:
        raise Refused(_last_line(err) or 'the release does not fit the files installed now')


def update(stage=None, *, chat=CHAT, chat_check_enabled=True, out=None, ready_seconds=None, chat_seconds=None):
    out = out or sys.stdout
    ready_seconds = READY_SECONDS if ready_seconds is None else ready_seconds
    chat_seconds = CHAT_SECONDS if chat_seconds is None else chat_seconds
    try:
        stage = Path(stage).resolve() if stage else find_candidate()
        m = manifest(stage)
        precheck(stage, m, out)
    except Refused as error:
        say(out, f'Nothing was changed: {error}')
        record('update', stage, 'refused', reason=str(error)[:300])
        return 2
    systemctl = systemctl_of(m)
    name = release_name(stage, m)
    try:
        tool = generator(stage)
        if tool is not None and m.get('release_dir'):
            code, _, err = python_tool(tool, 'materialize', stage)
            if code != 0:
                raise Refused('the release files could not be unpacked (%s)' % (_last_line(err) or f'exit {code}'))
        record_services(stage, systemctl)
    except (Refused, OSError) as error:
        say(out, f'Nothing was changed: {error}')
        record('update', stage, 'refused', reason=str(error)[:300])
        return 2
    try:
        say(out, 'Stopping the local AI service ...')
        stop_services(systemctl)  # before the first file is replaced: no request sees a half-installed release
        say(out, f'Installing {name} ...')
        code, _, err = python_tool(Path(stage) / 'tools/install_staged_release.py', stage, '--apply')
        if code != 0:
            if live_matches(m, 'preimage_sha256'):
                raise Refused('the installer stopped and put every file back (%s)' % (_last_line(err) or f'exit {code}'))
            raise Failed('the installer stopped part-way')
    except Refused as error:
        back = restore_services(stage, systemctl)
        forget_services(stage)
        record('update', stage, 'refused', reason=str(error)[:300], services_restored=back)
        if not back:
            say(out, f'No file was changed ({error}), but the local AI service did not start again. '
                'Run `systemctl --user start oxc-chat.service`, then `oxc status`.')
            return 3
        say(out, f'Nothing was changed: {error}')
        return 2
    except (Failed, OSError) as error:
        return _auto_rollback(stage, m, chat, out, str(error), ready_seconds)
    try:
        say(out, 'Starting the local AI service ...')
        active = recorded_services(stage).get('active') or {}
        start = [CHAT_SERVICE] + ([STUDIO_SERVICE] if active.get(STUDIO_SERVICE) else [])
        for args in (['daemon-reload'], ['start', *start]):
            code, _, err = _run([systemctl, '--user', *args], timeout=120)
            if code != 0:
                raise Failed('the service did not start')
        say(out, 'Waiting for it to be ready ...')
        status = wait_ready(chat, ready_seconds, systemctl, expect=m.get('release_dir'))
        wall = None
        if chat_check_enabled:
            say(out, 'Checking that chat answers (this may load the model) ...')
            wall = chat_check(chat, status, chat_seconds)
    except (Failed, OSError, ValueError) as error:
        return _auto_rollback(stage, m, chat, out, str(error), ready_seconds)
    # O-152, best effort: never fails the update. The stage's own oxa path when it installs one.
    identity = refresh_agent_identity(chat=chat, agent=(m.get('paths') or {}).get('oxa'),
                                      home=m.get('home'))
    record('update', stage, 'installed', release=name, chat_seconds=wall, agent_identity=identity)
    say(out, f'Updated to {name}.' + (f' Chat answered in {wall} s.' if wall is not None else '')
        + ' If anything feels wrong: oxc rollback')
    return 0


def _auto_rollback(stage, m, chat, out, why, ready_seconds):
    say(out, f'The new version did not pass its check ({why}). Putting the previous version back ...')
    code, _, err = python_tool(Path(stage) / 'rollback/rollback.py', timeout=600)
    if code != 0:
        record('update', stage, 'rollback_failed', reason=why[:300])
        say(out, 'The automatic rollback did not finish (%s). Run `oxc rollback`; if that also stops, '
            'run `%s` and share its output.' % (ROLLBACK_EXIT.get(code) or _last_line(err) or f'exit {code}',
                                                 Path(stage) / 'rollback/rollback.py --check'))
        return 3
    try:
        _ready_after_rollback(stage, m, chat, ready_seconds)
    except Failed as error:
        record('update', stage, 'auto_rolled_back', reason=why[:300], ready=False)
        say(out, f'The previous version is back, but the service is not answering yet ({error}). '
            'Give it a minute, then run `oxc status`.')
        return 3
    record('update', stage, 'auto_rolled_back', reason=why[:300])
    say(out, 'Your previous version is back and working. Nothing else changed.')
    return 1


def _ready_after_rollback(stage, m, chat, ready_seconds):
    """rollback.py starts the services that ran before; when the controller was one of them, prove it
    runs the previous release (the recorded one; else at least not the staged one)."""
    recorded = recorded_services(stage)
    active = recorded.get('active')
    if isinstance(active, dict) and not active.get(CHAT_SERVICE):
        return None  # it was not running before the install, and rollback.py leaves it stopped
    return wait_ready(chat, ready_seconds, systemctl_of(m), expect=recorded.get('chat_release'),
                      avoid=m.get('release_dir'))


def rollback(*, chat=CHAT, check_only=False, out=None, ready_seconds=None):
    out = out or sys.stdout
    ready_seconds = READY_SECONDS if ready_seconds is None else ready_seconds
    stage = find_current()
    if stage is None:
        say(out, 'Nothing to roll back: no installed release with a saved previous version was found.')
        return 2
    script = stage / 'rollback/rollback.py'
    code, stdout, err = python_tool(script, '--check', timeout=120)
    if code != 0:
        say(out, 'Nothing was changed: %s.' % (ROLLBACK_EXIT.get(code) or _last_line(err) or f'exit {code}'))
        return 2
    if check_only:
        say(out, f'Rollback is ready: it would put back the version before {release_name(stage)}.')
        return 0
    say(out, f'Putting back the version before {release_name(stage)} ...')
    code, stdout, err = python_tool(script, timeout=600)
    if code != 0:
        record('rollback', stage, 'failed', reason=(ROLLBACK_EXIT.get(code) or _last_line(err))[:300])
        say(out, 'The rollback stopped (%s). Run `oxc rollback` again; if it stops again, run `%s --check` '
            'and share its output.' % (ROLLBACK_EXIT.get(code) or _last_line(err) or f'exit {code}', script))
        return 3
    try:
        _ready_after_rollback(stage, manifest(stage), chat, ready_seconds)
    except (Failed, Refused) as error:
        record('rollback', stage, 'rolled_back', ready=False)
        say(out, f'The previous version is back, but the service is not answering yet ({error}). '
            'Give it a minute, then run `oxc status`.')
        return 3
    record('rollback', stage, 'rolled_back')
    say(out, 'The previous version is back and running. To install the newer one again: oxc update ' + str(stage))
    return 0
