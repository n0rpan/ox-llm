"""v0.1.4 `oxc update` / `oxc rollback` against a synthetic staged release in a private HOME.

The stage uses the real O-119 machinery (tools/install_staged_release.py, stage_rollback_guard.py and its
rendered rollback.py), a recording fake systemctl and a fake controller on a loopback port. Nothing
outside the temp dir is read or written (P56): no real units, ~/.config/oxc or ~/.local/bin.
"""
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import io
import json
import os
from pathlib import Path
import shutil
import stat
import sys
import subprocess
import tempfile
import threading
import unittest
from unittest.mock import patch

from oxc.runtime import release_update as ru

TOOLS = Path(__file__).resolve().parents[1] / 'tools'
sys.path.insert(0, str(TOOLS))
import stage_rollback_guard as guard  # noqa: E402

# A stateful fake: units start active (or as written in {units}/<unit>); stop/start change that state,
# unless {stuck} exists (a stop that "succeeds" but leaves the unit up). `show` prints {pid} (default
# MainPID=0, i.e. identity unknown); a verb in {fail} always fails, one in {fail}.once fails once; each start of the chat unit re-points {proc}/4242/cwd to the next
# line of {cwds} when that file has lines (the release the newly started controller runs).
FAKE_SYSTEMCTL = '''#!/bin/sh
printf '%s\\n' "$*" >> "{log}"
if [ -f "{fail}" ] && grep -q "^$2$" "{fail}"; then exit 1; fi
if [ -f "{fail}.once" ] && grep -q "^$2$" "{fail}.once"; then rm -f "{fail}.once"; exit 1; fi
cmd=$2; shift 2
mkdir -p "{units}"
case "$cmd" in
  is-active) rc=0; for u in "$@"; do s=$(cat "{units}/$u" 2>/dev/null || echo active); echo "$s"; [ "$s" = active ] || rc=3; done; exit $rc ;;
  stop) [ -f "{stuck}" ] || for u in "$@"; do echo inactive > "{units}/$u"; done ;;
  start|restart) for u in "$@"; do echo active > "{units}/$u";
      if [ "$u" = oxc-chat.service ] && [ -s "{cwds}" ]; then
        mkdir -p "{proc}/4242"; ln -sfn "$(head -n 1 "{cwds}")" "{proc}/4242/cwd"
        tail -n +2 "{cwds}" > "{cwds}.next"; mv "{cwds}.next" "{cwds}"; fi; done ;;
  show) cat "{pid}" 2>/dev/null || echo MainPID=0 ;;
esac
exit 0
'''


def sha(data):
    return hashlib.sha256(data).hexdigest()


class FakeController:
    """status: 200 (or `status_code`); chat: 200 with a reply unless `chat_code` says otherwise."""

    def __init__(self):
        self.status_code, self.chat_code, self.calls = 200, 200, []
        outer = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def reply(self, code, value):
                body = json.dumps(value).encode()
                self.send_response(code)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_GET(self):
                outer.calls.append(('GET', self.path, dict(self.headers)))
                self.reply(outer.status_code, {'state': 'unloaded', 'model': 'stub-local'})

            def do_POST(self):
                length = int(self.headers.get('Content-Length') or 0)
                self.rfile.read(length)
                outer.calls.append(('POST', self.path, dict(self.headers)))
                if self.path == '/v1/chat/completions':
                    if outer.chat_code != 200:
                        return self.reply(outer.chat_code, {'error': {'message': 'no'}})
                    return self.reply(200, {'choices': [{'message': {'content': 'ready'}, 'finish_reason': 'stop'}]})
                self.reply(200, {})

        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.url = 'http://127.0.0.1:%d' % self.server.server_address[1]
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()


class UpdateCase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.home = self.root / 'home'
        self.live = {'runtime': self.home / '.config/oxc/llm/runtime.json', 'cli': self.home / '.local/bin/oxc'}
        self.old = {'runtime': b'{"old": true}\n', 'cli': b'#!/bin/sh\necho old\n'}
        self.new = {'runtime': b'{"new": true}\n', 'cli': b'#!/bin/sh\necho new\n'}
        self.modes = {'runtime': 0o600, 'cli': 0o700}
        self.extra_keys()
        for key, path in self.live.items():
            path.parent.mkdir(parents=True, exist_ok=True)
            if self.old[key] is not None:  # None: absent before the stage (a file it creates)
                path.write_bytes(self.old[key])
                os.chmod(path, self.modes[key])
        self.log, self.fail = self.root / 'systemctl.log', self.root / 'systemctl.fail'
        self.systemctl = self.root / 'systemctl'
        self.units, self.stuck, self.pidfile = self.root / 'units', self.root / 'systemctl.stuck', self.root / 'mainpid'
        self.proc, self.cwds = self.root / 'proc', self.root / 'cwds'
        self.systemctl.write_text(FAKE_SYSTEMCTL.format(log=self.log, fail=self.fail, units=self.units, stuck=self.stuck,
                                                        pid=self.pidfile, proc=self.proc, cwds=self.cwds))
        self.systemctl.chmod(0o700)
        self.controller = FakeController()
        self.stages = self.root / 'staged'
        self.env = patch.dict(os.environ, {'OXC_UPDATE_STATE': str(self.root / 'state/update/state.json'),
                                           'OXC_STAGE_ROOTS': str(self.stages)})
        self.env.start()
        self.proc_patch = patch.object(ru, 'PROC', str(self.proc))
        self.proc_patch.start()
        self.stage = self.make_stage('v0.1.4-test')

    def extra_keys(self):
        """Subclasses add keys (None bytes = absent on that side)."""

    def tearDown(self):
        self.proc_patch.stop()
        self.env.stop()
        self.controller.close()
        self.tmp.cleanup()

    def make_stage(self, name, release_dir=None):
        stage = self.stages / name
        (stage / 'rollback').mkdir(parents=True)
        (stage / 'tools').mkdir()
        staged = {key: key + '.staged' for key in self.live}
        staged.update({'runtime': 'runtime.json', 'cli': 'oxc'})
        for key in self.live:
            if self.new[key] is not None:
                (stage / staged[key]).write_bytes(self.new[key])
            if self.old[key] is not None:
                (stage / 'rollback' / (key + '.original')).write_bytes(self.old[key])
        modes = {k: m for k, m in self.modes.items() if self.old[k] is not None}
        (stage / 'rollback/rollback.py').write_bytes(guard.render_rollback(
            {k: str(p) for k, p in self.live.items()}, self.new, self.old, staged_files=staged,
            systemctl=str(self.systemctl), services=list(ru.SERVICES), modes=modes,
            service_state=ru.SERVICE_STATE))
        for tool in ('install_staged_release.py', 'stage_rollback_guard.py'):
            shutil.copy(TOOLS / tool, stage / 'tools' / tool)
        manifest = {**guard.manifest_binding({k: str(p) for k, p in self.live.items()}, self.new, self.old,
                                             staged, modes=modes),
                    'installed_mode': {k: '%04o' % self.modes[k] for k in self.live if self.old[k] is None},
                    'staged_files': staged, 'release': name,
                    'rollback': {'script': 'rollback/rollback.py', 'systemctl': str(self.systemctl)}}
        if release_dir:
            manifest['release_dir'] = release_dir
        (stage / 'manifest.json').write_text(json.dumps(manifest))
        files = {p.relative_to(stage).as_posix(): sha(p.read_bytes()) for p in sorted(stage.rglob('*')) if p.is_file()}
        (stage / 'SEAL.json').write_text(json.dumps({'files': files}))
        return stage

    def live_bytes(self):
        return {k: (p.read_bytes(), stat.S_IMODE(p.stat().st_mode)) if p.exists() else (None, self.modes[k])
                for k, p in self.live.items()}

    def running(self, release, then=()):
        """The controller runs `release` now; its next starts run the releases in `then`."""
        (self.proc / '4242').mkdir(parents=True, exist_ok=True)
        link = self.proc / '4242/cwd'
        if link.is_symlink():
            link.unlink()
        link.symlink_to(release)
        self.pidfile.write_text('MainPID=4242\n')
        self.cwds.write_text(''.join(r + '\n' for r in then))

    def calls(self):
        return self.log.read_text().splitlines() if self.log.exists() else []

    def update(self, **kw):
        out = io.StringIO()
        code = ru.update(kw.pop('stage', self.stage), chat=self.controller.url, out=out, ready_seconds=kw.pop('ready', 5),
                         chat_seconds=10, **kw)
        return code, out.getvalue()

    def rollback(self, **kw):
        out = io.StringIO()
        return ru.rollback(chat=self.controller.url, out=out, ready_seconds=kw.pop('ready', 5), **kw), out.getvalue()


class Update(UpdateCase):
    def test_happy_path_installs_restarts_waits_and_chats_as_lab(self):
        code, text = self.update()
        self.assertEqual(code, 0, text)
        self.assertEqual(self.live_bytes(), {k: (self.new[k], self.modes[k]) for k in self.live})
        self.assertIn('Updated to v0.1.4-test', text)
        self.assertIn('oxc rollback', text)
        calls = self.calls()
        self.assertIn('--user daemon-reload', calls)
        self.assertIn('--user start oxc-chat.service prompt-studio-app.service', calls)
        # U1: both services stopped and proven down (is-active right after) before the restart
        stop = calls.index('--user stop oxc-chat.service prompt-studio-app.service')
        self.assertEqual(calls[stop + 1], '--user is-active oxc-chat.service prompt-studio-app.service')
        self.assertLess(stop, calls.index('--user daemon-reload'))
        chats = [c for c in self.controller.calls if c[1] == '/v1/chat/completions']
        self.assertEqual(len(chats), 1)
        self.assertEqual({k.lower(): v for k, v in chats[0][2].items()}.get('x-local-ai-origin'), 'lab')
        self.assertTrue(any(c[1] == '/_local/unload' for c in self.controller.calls))  # nothing was loaded before
        state = json.loads((self.root / 'state/update/state.json').read_text())
        self.assertEqual(state['current']['stage'], str(self.stage))
        self.assertEqual(stat.S_IMODE((self.root / 'state/update/state.json').stat().st_mode), 0o600)

    def test_failed_chat_rolls_back_automatically(self):
        self.controller.chat_code = 500
        code, text = self.update()
        self.assertEqual(code, 1, text)
        self.assertEqual(self.live_bytes(), {k: (self.old[k], self.modes[k]) for k in self.live})
        self.assertIn('did not pass its check', text)
        self.assertIn('previous version is back and working', text)
        self.assertIn('--user stop oxc-chat.service prompt-studio-app.service', self.calls())
        self.assertIsNone(json.loads((self.root / 'state/update/state.json').read_text())['current'])

    def test_failed_restart_rolls_back(self):
        Path(str(self.fail) + '.once').write_text('start\n')
        code, text = self.update()
        self.assertEqual(code, 1, text)
        self.assertEqual(self.live_bytes(), {k: (self.old[k], self.modes[k]) for k in self.live})

    def test_not_ready_after_rollback_needs_attention(self):
        self.controller.status_code = 503
        code, text = self.update(ready=1)
        self.assertEqual(code, 3, text)
        self.assertEqual(self.live_bytes(), {k: (self.old[k], self.modes[k]) for k in self.live})
        self.assertIn('oxc status', text)

    def test_tampered_stage_is_refused_without_changes(self):
        (self.stage / 'runtime.json').write_bytes(b'{"evil": 1}\n')
        before = self.live_bytes()
        code, text = self.update()
        self.assertEqual(code, 2, text)
        self.assertIn('Nothing was changed', text)
        self.assertIn('changed after it was sealed', text)
        self.assertEqual(self.live_bytes(), before)
        self.assertEqual(self.calls(), [])
        self.assertEqual(self.controller.calls, [])

    def test_live_file_changed_is_refused(self):
        self.live['runtime'].write_bytes(b'{"hand": "edit"}\n')
        before = self.live_bytes()
        code, text = self.update()
        self.assertEqual(code, 2, text)
        self.assertEqual(self.live_bytes(), before)
        self.assertNotIn('--user stop', ' '.join(self.calls()))

    def test_already_installed_is_refused(self):
        self.assertEqual(self.update()[0], 0)
        code, text = self.update()
        self.assertEqual(code, 2)
        self.assertIn('already installed', text)

    def test_default_stage_is_the_installable_one(self):
        self.assertEqual(ru.find_candidate(), self.stage)
        code, text = self.update(stage=None)
        self.assertEqual(code, 0, text)
        with self.assertRaises(ru.Refused):
            ru.find_candidate()  # live is now the new release: nothing left to install

    def test_no_chat_skips_the_chat(self):
        code, _ = self.update(chat_check_enabled=False)
        self.assertEqual(code, 0)
        self.assertFalse(any(c[1] == '/v1/chat/completions' for c in self.controller.calls))


class Order(UpdateCase):
    """Codex review U1: stop (and prove it) before apply; a stop that cannot be proven installs nothing."""

    def apply_spy(self):
        seen = []
        real = ru.python_tool

        def spy(path, *args, **kw):
            if '--apply' in args:
                seen.append(self.units_state())
            return real(path, *args, **kw)
        return seen, patch.object(ru, 'python_tool', spy)

    def units_state(self):
        return {u: (self.units / u).read_text().strip() if (self.units / u).exists() else 'active' for u in ru.SERVICES}

    def test_services_are_down_when_the_installer_runs(self):
        seen, spy = self.apply_spy()
        with spy:
            code, text = self.update()
        self.assertEqual(code, 0, text)
        self.assertEqual(seen, [{u: 'inactive' for u in ru.SERVICES}])
        self.assertEqual(self.units_state(), {u: 'active' for u in ru.SERVICES})

    def test_stop_failure_installs_nothing_and_restores_services(self):
        self.fail.write_text('stop\n')
        seen, spy = self.apply_spy()
        with spy:
            code, text = self.update()
        self.assertEqual(code, 2, text)
        self.assertEqual(seen, [])
        self.assertEqual(self.live_bytes(), {k: (self.old[k], self.modes[k]) for k in self.live})
        self.assertIn('--user start oxc-chat.service prompt-studio-app.service', self.calls())
        self.assertFalse((self.stage / ru.SERVICE_STATE).exists())  # a later attempt records afresh

    def test_unproven_stop_installs_nothing(self):
        self.stuck.write_text('')
        seen, spy = self.apply_spy()
        with spy:
            code, text = self.update()
        self.assertEqual(code, 2, text)
        self.assertIn('did not stop', text)
        self.assertEqual(seen, [])
        self.assertEqual(self.live_bytes(), {k: (self.old[k], self.modes[k]) for k in self.live})

    def test_inactive_studio_stays_stopped(self):
        self.units.mkdir()
        (self.units / ru.STUDIO_SERVICE).write_text('inactive\n')
        code, text = self.update()
        self.assertEqual(code, 0, text)
        self.assertIn('--user start oxc-chat.service', self.calls())
        self.assertEqual(self.units_state()[ru.STUDIO_SERVICE], 'inactive')

    def test_rollback_is_symmetric_stop_restore_start(self):
        self.controller.chat_code = 500
        code, text = self.update()
        self.assertEqual(code, 1, text)
        calls = self.calls()
        tail = calls[calls.index('--user start oxc-chat.service prompt-studio-app.service') + 1:]
        self.assertEqual(tail[:3], ['--user stop oxc-chat.service prompt-studio-app.service', '--user daemon-reload',
                                    '--user start oxc-chat.service prompt-studio-app.service'])


class Identity(UpdateCase):
    """Codex review U2: readiness needs positive proof of the staged release; unknown is not ready."""

    def setUp(self):
        super().setUp()
        self.stage = self.make_stage('v0.1.4-rel', release_dir='/rel/new')

    def test_exact_release_is_ready(self):
        self.running('/rel/old', then=['/rel/new'])
        code, text = self.update(chat_check_enabled=False)
        self.assertEqual(code, 0, text)
        self.assertEqual(self.live_bytes(), {k: (self.new[k], self.modes[k]) for k in self.live})
        self.assertEqual(json.loads((self.stage / ru.SERVICE_STATE).read_text())['chat_release'], '/rel/old')

    def test_unknown_identity_is_not_ready_and_rolls_back(self):
        # no MainPID: the identity cannot be told, even with --no-chat and a 200 status
        code, text = self.update(chat_check_enabled=False, ready=1)
        self.assertNotEqual(code, 0, text)
        self.assertIn('could not be confirmed', text)
        self.assertEqual(self.live_bytes(), {k: (self.old[k], self.modes[k]) for k in self.live})
        self.assertNotEqual((json.loads((self.root / 'state/update/state.json').read_text()).get('current')), {'stage': str(self.stage)})

    def test_wrong_release_is_not_ready_and_rolls_back_to_the_proven_old_one(self):
        self.running('/rel/old', then=['/rel/stale', '/rel/old'])
        code, text = self.update(chat_check_enabled=False, ready=1)
        self.assertEqual(code, 1, text)
        self.assertIn('another version', text)
        self.assertIn('previous version is back and working', text)
        self.assertEqual(self.live_bytes(), {k: (self.old[k], self.modes[k]) for k in self.live})

    def test_rollback_readiness_needs_the_old_release(self):
        self.running('/rel/old', then=['/rel/new', '/rel/new'])  # the rollback's start still runs the new one
        self.controller.chat_code = 500
        code, text = self.update(ready=1)
        self.assertEqual(code, 3, text)
        self.assertIn('oxc status', text)

    def test_manual_rollback_proves_the_old_release(self):
        self.running('/rel/old', then=['/rel/new', '/rel/old'])
        self.assertEqual(self.update(chat_check_enabled=False)[0], 0)
        code, text = self.rollback()
        self.assertEqual(code, 0, text)

    def test_manual_rollback_still_on_the_new_release_needs_attention(self):
        self.running('/rel/old', then=['/rel/new', '/rel/new'])
        self.assertEqual(self.update(chat_check_enabled=False)[0], 0)
        code, text = self.rollback(ready=1)
        self.assertEqual(code, 3, text)
        self.assertEqual(self.live_bytes(), {k: (self.old[k], self.modes[k]) for k in self.live})


class Rollback(UpdateCase):
    def test_rollback_after_update_then_nothing_left(self):
        self.assertEqual(self.update()[0], 0)
        code, text = self.rollback(check_only=True)
        self.assertEqual(code, 0, text)
        self.assertEqual(self.live_bytes(), {k: (self.new[k], self.modes[k]) for k in self.live})
        code, text = self.rollback()
        self.assertEqual(code, 0, text)
        self.assertIn('previous version is back', text)
        self.assertEqual(self.live_bytes(), {k: (self.old[k], self.modes[k]) for k in self.live})
        code, text = self.rollback()
        self.assertEqual(code, 2)
        self.assertIn('Nothing to roll back', text)

    def test_rollback_finds_the_installed_stage_without_state(self):
        self.assertEqual(self.update()[0], 0)
        (self.root / 'state/update/state.json').unlink()
        self.assertEqual(ru.find_current(), self.stage)
        self.assertEqual(self.rollback()[0], 0)

    def test_rollback_refuses_after_hand_edit(self):
        self.assertEqual(self.update()[0], 0)
        self.live['cli'].write_bytes(b'hand edit\n')
        code, text = self.rollback()
        self.assertEqual(code, 2, text)
        self.assertEqual(self.live['cli'].read_bytes(), b'hand edit\n')


class Pieces(unittest.TestCase):
    def test_runs_release_reads_proc_cwd(self):
        with tempfile.TemporaryDirectory() as tmp:
            proc = Path(tmp) / 'proc/42'
            proc.mkdir(parents=True)
            (proc / 'cwd').symlink_to('/rel/a')
            with patch.object(ru, '_run', return_value=(0, 'MainPID=42\n', '')):
                self.assertTrue(ru.runs_release({'release_dir': '/rel/a'}, 'x', proc=str(Path(tmp) / 'proc')))
                self.assertFalse(ru.runs_release({'release_dir': '/rel/b'}, 'x', proc=str(Path(tmp) / 'proc')))
            with patch.object(ru, '_run', return_value=(0, 'MainPID=0\n', '')):
                self.assertIsNone(ru.runs_release({'release_dir': '/rel/a'}, 'x'))
            self.assertIsNone(ru.runs_release({}, 'x'))

    def test_wait_ready_unknown_wrong_exact(self):
        clock = iter(range(0, 100)).__next__
        ok = (200, {'state': 'unloaded'})
        with patch.object(ru, 'http', return_value=ok):
            for cwd, ready in ((None, False), ('/rel/old', False), ('/rel/new', True)):
                with self.subTest(cwd=cwd), patch.object(ru, 'controller_release', return_value=cwd):
                    if ready:
                        self.assertEqual(ru.wait_ready('u', 3, 'x', expect='/rel/new', sleep=lambda s: None, clock=clock), ok[1])
                    else:
                        with self.assertRaises(ru.Failed):
                            ru.wait_ready('u', 3, 'x', expect='/rel/new', sleep=lambda s: None, clock=clock)
            # rollback side: no recorded old release, only "not the staged one" can be proven
            with patch.object(ru, 'controller_release', return_value=None), self.assertRaises(ru.Failed):
                ru.wait_ready('u', 3, 'x', avoid='/rel/new', sleep=lambda s: None, clock=clock)
            with patch.object(ru, 'controller_release', return_value='/rel/new'), self.assertRaises(ru.Failed):
                ru.wait_ready('u', 3, 'x', avoid='/rel/new', sleep=lambda s: None, clock=clock)
            with patch.object(ru, 'controller_release', return_value='/rel/old'):
                self.assertEqual(ru.wait_ready('u', 3, 'x', avoid='/rel/new', sleep=lambda s: None, clock=clock), ok[1])
            # an identity-free (synthetic) stage: nothing to prove
            self.assertEqual(ru.wait_ready('u', 3, None, sleep=lambda s: None, clock=clock), ok[1])

    def test_never_reaches_the_network_for_a_release(self):
        source = (Path(ru.__file__)).read_text()
        for word in ('https://', 'urlretrieve', 'git clone', 'curl', 'wget'):
            self.assertNotIn(word, source)


class AgentIdentity(unittest.TestCase):
    """O-152: `oxa identity --refresh` after install/update, best effort, `--offline` fallback (scratch HOME)."""

    def agent(self, home, script):
        path = Path(home) / '.local/bin/oxa'
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text('#!/bin/sh\necho "$OXA_ENGINE_URL $*" >> "$HOME/calls"\n' + script)
        path.chmod(0o700)
        return path

    def test_online_offline_failed_and_absent(self):
        with tempfile.TemporaryDirectory() as home:
            self.assertEqual(ru.refresh_agent_identity(home=home), 'absent')
            self.agent(home, 'exit 0\n')
            self.assertEqual(ru.refresh_agent_identity(home=home, chat='http://127.0.0.1:9'), 'refreshed')
            self.assertEqual((Path(home) / 'calls').read_text(), 'http://127.0.0.1:9 identity --refresh\n')
        with tempfile.TemporaryDirectory() as home:
            self.agent(home, 'case "$*" in *--offline*) exit 0;; esac\nexit 1\n')
            self.assertEqual(ru.refresh_agent_identity(home=home), 'refreshed-offline')
            self.assertEqual(len((Path(home) / 'calls').read_text().splitlines()), 2)
        with tempfile.TemporaryDirectory() as home:
            self.agent(home, 'exit 3\n')
            self.assertEqual(ru.refresh_agent_identity(home=home), 'failed')

    def test_a_hanging_agent_is_bounded(self):
        def run(args, **kwargs):
            raise subprocess.TimeoutExpired(args, kwargs['timeout'])
        with tempfile.TemporaryDirectory() as home:
            self.agent(home, 'exit 0\n')
            self.assertEqual(ru.refresh_agent_identity(home=home, run=run), 'failed')


class AbsentKeys(UpdateCase):
    """v0.2.0: a stage that CREATES a file (oxa) and REMOVES one (the retired launcher); update, the
    automatic rollback and `oxc rollback` restore bytes, modes and absence (temp HOME only, P56)."""

    def extra_keys(self):
        self.live['oxa'] = self.home / '.local/bin/oxa'
        self.old['oxa'], self.new['oxa'], self.modes['oxa'] = None, b'#!/bin/sh\necho oxa 0.2.0\n', 0o700
        self.live['launcher'] = self.home / '.local/bin/launch_local_plan.py'
        self.old['launcher'], self.new['launcher'], self.modes['launcher'] = b'#!/bin/sh\n# old launcher\n', None, 0o700

    def test_update_creates_and_removes_then_rollback_restores(self):
        before = self.live_bytes()
        self.assertEqual(before['oxa'][0], None)
        code, text = self.update()
        self.assertEqual(code, 0, text)
        state = json.loads((self.root / 'state/update/state.json').read_text())
        self.assertEqual(state['history'][-1]['agent_identity'], 'refreshed')  # O-152 best-effort refresh ran
        self.assertEqual(self.live_bytes(), {k: (self.new[k], self.modes[k]) for k in self.live})
        code, text = self.rollback()
        self.assertEqual(code, 0, text)
        self.assertEqual(self.live_bytes(), before)

    def test_failed_chat_rolls_back_the_new_file_too(self):
        before = self.live_bytes()
        self.controller.chat_code = 500
        code, text = self.update()
        self.assertEqual(code, 1, text)
        self.assertEqual(self.live_bytes(), before)
        self.assertFalse(self.live['oxa'].exists())

    def test_an_existing_file_where_the_stage_creates_one_is_refused(self):
        self.live['oxa'].write_bytes(b'mine')
        code, text = self.update()
        self.assertEqual(code, 2, text)
        self.assertEqual(self.live['oxa'].read_bytes(), b'mine')
        self.assertEqual(self.calls(), [])


if __name__ == '__main__':
    unittest.main()
