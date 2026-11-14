"""local-ai uninstall (V020-SEAM-REVIEW fixes): exact recorded files only, positive stop before any change.

Everything runs in a temp HOME with a fake systemctl that records calls and keeps per-unit state;
home= is always passed explicitly, so nothing here can reach the real ~/.config/local-ai,
~/.local/state/local-ai, user units, inference.lock or ~/.local/bin (P56).
"""
import hashlib
import io
import json
import os
from pathlib import Path
import tempfile
import unittest

from snapdragon_ai_lab.runtime import uninstall

# Per-unit state lives in $STATE/<unit> (missing file = inactive). MODE file:
#   (none)  stop makes units inactive;  sticky  stop fails and units stay active;
#   blind   stop "works" but is-active afterwards prints nothing (state unknown).
FAKE_SYSTEMCTL = """#!/bin/sh
echo "$*" >> "{log}"
mode=$(cat "{mode}" 2>/dev/null)
cmd=$2; shift 2
case "$cmd" in
  is-active)
    [ "$mode" = blind ] && [ -f "{stopped_once}" ] && exit 4
    rc=3
    for u in "$@"; do
      if [ -f "{state}/$u" ]; then cat "{state}/$u"; rc=0; else echo inactive; fi
    done
    exit $rc;;
  stop)
    touch "{stopped_once}"
    [ "$mode" = sticky ] && exit 1
    for u in "$@"; do rm -f "{state}/$u"; done; exit 0;;
  start)
    for u in "$@"; do echo active > "{state}/$u"; done; exit 0;;
esac
exit 0
"""


def h(data):
    return hashlib.sha256(data).hexdigest()


def snapshot(root):
    rows = {}
    for folder, dirs, files in os.walk(root, followlinks=False):
        for name in dirs + files:
            path = Path(folder) / name
            if path.is_symlink():
                rows[str(path)] = ('link', os.readlink(path))
            elif path.is_dir():
                rows[str(path)] = ('dir',)
            else:
                rows[str(path)] = ('file', path.lstat().st_mode, h(path.read_bytes()))
    return rows


class UninstallTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        base = Path(self.tmp.name)
        self.home, self.outside, self.tools = base / 'home', base / 'outside', base / 'tools'
        for path in (self.home, self.outside, self.tools, self.tools / 'state'):
            path.mkdir()
        self.log, self.mode = self.tools / 'calls.log', self.tools / 'mode'
        self.systemctl = self.tools / 'systemctl'
        self.systemctl.write_text(FAKE_SYSTEMCTL.format(log=self.log, mode=self.mode, state=self.tools / 'state',
                                                        stopped_once=self.tools / 'stopped-once'))
        self.systemctl.chmod(0o755)
        self.build()

    def put(self, rel, data, mode=0o600):
        path = self.home / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data if isinstance(data, bytes) else data.encode())
        path.chmod(mode)
        return path

    def active(self, *units):
        for unit in units:
            (self.tools / 'state' / unit).write_text('active\n')

    def build(self):
        """An installed ox-llm: the stage manifest + update state record exactly these files."""
        unit_dir = '.config/systemd/user/'
        release = self.home / '.local/share/ai-stack/runtime-releases/v020-final-abc'
        engine = self.home / '.local/share/ai-stack/releases/oxe-v020-final-def'
        self.files = {
            'unit': self.put(unit_dir + 'local-ai-chat.service', '[Service]\nExecStart=/x\n'),
            'cli': self.put('.local/bin/local-ai', '#!/bin/sh\n# cli\n', 0o700),
            'oxa': self.put('.local/bin/oxa', '#!/bin/sh\n# oxa\n', 0o700),
            'runtime': self.put('.config/local-ai/llm/runtime.json', json.dumps({'profiles': {
                'flash': {'sidecar_path': str(self.home / 'Documents/experts.bin')},
                'ornith': {'model_path': str(self.home / '.local/share/ai-stack/models/llm/ornith.gguf')},
                'qwen': {'model_path': str(self.outside / 'qwen weights.gguf')}}})),
        }
        self.release_files = {'controller.py': self.put(str(release.relative_to(self.home)) + '/controller.py', 'x = 1\n')}
        self.engine_files = {'oxe-server': self.put(str(engine.relative_to(self.home)) + '/oxe-server', 'server', 0o700),
                             'oxe': self.put(str(engine.relative_to(self.home)) + '/oxe', 'oxe', 0o700)}
        self.release, self.engine = release, engine
        stage = self.outside / 'stage'
        stage.mkdir()
        manifest = {'paths': {k: str(p) for k, p in self.files.items()} | {'launcher': str(self.home / '.local/bin/launch_local_plan.py')},
                    'installed_sha256': {k: h(p.read_bytes()) for k, p in self.files.items()} | {'launcher': 'absent'},
                    'release_dir': str(release), 'runtime_source_sha256': {n: h(p.read_bytes()) for n, p in self.release_files.items()},
                    'engine_dir': str(engine), 'server_sha256': h(b'server'), 'oxe_sha256': h(b'oxe')}
        (stage / 'manifest.json').write_text(json.dumps(manifest))
        self.put('.local/state/local-ai/update/state.json', json.dumps(
            {'current': {'stage': str(stage)}, 'history': [{'action': 'update', 'stage': str(stage), 'result': 'installed'}]}))
        # Things that are NOT recorded and must survive, including similar names.
        self.keep = [
            self.put('.local/bin/local-ai-tools', 'mine', 0o700),
            self.put('.local/bin/oxa.bak', 'mine', 0o700),
            self.put(unit_dir + 'local-ai-chat.service.bak', 'mine'),
            self.put(unit_dir + 'local-ai-mine.service', '[Service]\n'),
            self.put(unit_dir + 'prompt-studio-q8-setup.service', '[Service]\n'),
            self.put('.config/local-ai/llm/api-key.json', '{}'),
            self.put('.config/local-ai-notes.txt', 'mine'),
            self.put('.local/state/local-ai/ledger/requests-20261113.jsonl', '{}\n'),
            self.put('.local/share/ai-stack/runtime-releases/v020-final-abc-old/controller.py', 'x = 1\n'),
            self.put('.local/share/ai-stack/models/llm/ornith.gguf', b'weights'),
            self.put('.local/share/ai-stack/models/llm/other.gguf', b'weights'),
            self.put('Documents/experts.bin', b'experts'),
            self.put('Documents/unrelated.txt', 'precious'),
            self.put('.config/agent-client/client.json', '{"providers": {"local-qwen": {}}}'),
        ]
        (self.outside / 'qwen weights.gguf').write_bytes(b'w')

    def call(self, *args, home=None):
        out = io.StringIO()
        code = uninstall.main(list(args), home=str(home or self.home), systemctl=str(self.systemctl), out=out)
        return code, out.getvalue()

    def calls(self):
        return self.log.read_text().splitlines() if self.log.exists() else []

    # -- the review's repro cases -----------------------------------------------------------
    def test_remove_models_deletes_the_exact_sidecar_never_its_parent_or_siblings(self):
        code, text = self.call('--apply', '--remove-models')
        self.assertEqual(code, 0, text)
        self.assertFalse((self.home / 'Documents/experts.bin').exists())
        self.assertEqual((self.home / 'Documents/unrelated.txt').read_text(), 'precious')
        self.assertTrue((self.home / 'Documents').is_dir())
        self.assertFalse((self.home / '.local/share/ai-stack/models/llm/ornith.gguf').exists())
        self.assertTrue((self.home / '.local/share/ai-stack/models/llm/other.gguf').exists(), 'unnamed model kept')
        self.assertTrue((self.outside / 'qwen weights.gguf').exists())
        self.assertIn("rm -- '%s'" % (self.outside / 'qwen weights.gguf'), text)  # quoted manual command

    def test_failed_stop_aborts_before_any_file_change_and_restarts(self):
        self.active('local-ai-chat.service', 'prompt-studio-app.service')
        self.mode.write_text('sticky')
        before = snapshot(self.home)
        code, text = self.call('--apply')
        self.assertEqual(code, 2, text)
        self.assertIn('could not prove these services stopped', text)
        self.assertEqual(snapshot(self.home), before, 'nothing touched')
        self.assertNotIn('disable', ' '.join(self.calls()))

    def test_unknown_state_after_stop_aborts_with_files_intact(self):
        self.active('local-ai-chat.service')
        self.mode.write_text('blind')
        before = snapshot(self.home)
        code, text = self.call('--apply')
        self.assertEqual(code, 2, text)
        self.assertIn('unknown', text)
        self.assertEqual(snapshot(self.home), before)
        self.assertIn('--user start local-ai-chat.service', self.calls(), 'what was running is started again')

    def test_systemctl_missing_refuses_and_changes_nothing(self):
        before = snapshot(self.home)
        out = io.StringIO()
        code = uninstall.main(['--apply'], home=str(self.home), systemctl=str(self.tools / 'nope'), out=out)
        self.assertEqual(code, 2, out.getvalue())
        self.assertEqual(snapshot(self.home), before)

    # -- exact ownership --------------------------------------------------------------------
    def test_apply_removes_exactly_the_recorded_files_and_siblings_with_similar_names_survive(self):
        self.active('local-ai-chat.service')
        code, text = self.call('--apply')
        self.assertEqual(code, 0, text)
        for path in list(self.files.values()) + list(self.release_files.values()) + list(self.engine_files.values()):
            self.assertFalse(path.exists(), path)
        self.assertFalse(self.release.exists(), 'recorded release dir removed once empty')
        self.assertFalse(self.engine.exists())
        for path in self.keep:
            self.assertTrue(path.exists(), path)
        self.assertTrue((self.home / '.config/local-ai').is_dir(), 'config dir with unrecorded files stays')
        calls = self.calls()
        self.assertEqual(calls[0], '--user is-active local-ai-chat.service')
        self.assertIn('--user stop local-ai-chat.service', calls)
        self.assertLess(calls.index('--user stop local-ai-chat.service'), calls.index('--user disable local-ai-chat.service'))
        self.assertEqual(calls[-1], '--user daemon-reload')
        self.assertNotIn('local-ai-mine.service', ' '.join(calls), 'an unrecorded unit is never touched')

    def test_dry_run_is_the_default_and_changes_nothing(self):
        before = snapshot(self.home)
        code, text = self.call()
        self.assertEqual(code, 0, text)
        self.assertIn('Dry run', text)
        self.assertEqual(snapshot(self.home), before)
        self.assertEqual([c for c in self.calls() if 'stop' in c or 'disable' in c], [])

    def test_a_file_changed_since_install_is_kept(self):
        self.files['cli'].write_text('#!/bin/sh\n# edited by the user\n')
        code, text = self.call('--apply')
        self.assertEqual(code, 0, text)
        self.assertTrue(self.files['cli'].exists())
        self.assertIn('changed since ox-llm installed it', text)

    def test_without_an_install_record_nothing_is_removed(self):
        (self.home / '.local/state/local-ai/update/state.json').unlink()
        before = snapshot(self.home)
        code, text = self.call('--apply')
        self.assertEqual(code, 0, text)
        self.assertIn('no ox-llm install record was found', text)
        self.assertEqual(snapshot(self.home), before)
        self.assertEqual(self.calls(), [], 'no service is stopped when nothing is removed')

    def test_a_recorded_path_behind_a_symlink_that_leaves_home_is_refused(self):
        bin_dir = self.home / '.local/bin'
        moved = self.outside / 'bin'
        bin_dir.rename(moved)
        bin_dir.symlink_to(moved)
        before = snapshot(self.outside)
        code, text = self.call('--apply')
        self.assertEqual(code, 2, text)
        self.assertEqual(snapshot(self.outside), before)

    def test_bootstrap_journal_record(self):
        (self.home / '.local/state/local-ai/update/state.json').unlink()
        journal = self.outside / 'journal.json'
        journal.write_text(json.dumps({'kind': 'sandbox-source-install', 'installed': True, 'target_home': str(self.home),
                                       'files': [{'path': '.local/bin/oxa', 'sha256': h(self.files['oxa'].read_bytes())}]}))
        code, text = self.call('--apply', '--record', str(journal))
        self.assertEqual(code, 0, text)
        self.assertFalse(self.files['oxa'].exists())
        self.assertTrue(self.files['cli'].exists(), 'not in that record')
        self.assertTrue((self.home / '.local/bin/oxa.bak').exists())

    def test_models_kept_by_default_and_client_config_never_named(self):
        code, text = self.call('--apply')
        self.assertEqual(code, 0, text)
        self.assertTrue((self.home / 'Documents/experts.bin').exists())
        self.assertTrue((self.home / '.local/share/ai-stack/models/llm/ornith.gguf').exists())
        self.assertIn('Client configurations outside ~/.config/local-ai are yours', text)
        self.assertNotIn('.config/agent-client', text)

    # -- Codex review of 0bd707f (independent-controls / directory-boundary-control) -----------
    def test_model_only_removal_without_a_record_still_stops_the_controller_first(self):
        (self.home / '.local/state/local-ai/update/state.json').unlink()
        self.active('local-ai-chat.service')
        code, text = self.call('--remove-models')  # dry run: the plan must name the units
        self.assertEqual(code, 0, text)
        self.assertIn('Services that must stop first: local-ai-chat.service', text)
        self.mode.write_text('sticky')  # and an unproven stop keeps the weights
        before = snapshot(self.home)
        code, text = self.call('--apply', '--remove-models')
        self.assertEqual(code, 2, text)
        self.assertEqual(snapshot(self.home), before)
        self.mode.unlink()
        code, text = self.call('--apply', '--remove-models')
        self.assertEqual(code, 0, text)
        self.assertFalse((self.home / 'Documents/experts.bin').exists())
        self.assertTrue(self.files['cli'].exists(), 'no record: no program file removed')
        calls = self.calls()
        self.assertLess(calls.index('--user stop local-ai-chat.service'), len(calls))

    def test_an_empty_recorded_dir_behind_a_symlink_that_leaves_home_is_never_removed(self):
        share = self.home / '.local/share'
        moved = self.outside / 'share'
        share.rename(moved)
        share.symlink_to(moved)
        for path in list(self.release_files.values()) + list(self.engine_files.values()):
            (moved / path.relative_to(share)).unlink()  # the recorded dirs are empty, outside home
        outside_release = moved / self.release.relative_to(share)
        code, text = self.call('--apply')
        self.assertEqual(code, 2, text)
        self.assertIn('resolves outside home', text)
        self.assertTrue(outside_release.is_dir(), 'the outside directory survives')
        self.assertTrue(self.files['cli'].exists(), 'refused before any change')

    def test_cleanup_is_vetted_again_right_before_the_rmdir(self):
        value = uninstall.plan(self.home, False)
        share = self.home / '.local/share'
        moved = self.outside / 'share'
        share.rename(moved)
        share.symlink_to(moved)  # swapped after the plan
        for path in list(self.release_files.values()) + list(self.engine_files.values()):
            (moved / path.relative_to(share)).unlink()
        code, removed, failed = uninstall.apply(self.home, {**value, 'units': [], 'disable': [], 'remove': []},
                                                str(self.systemctl))
        self.assertEqual(code, 1)
        self.assertTrue((moved / self.release.relative_to(share)).is_dir())
        self.assertTrue(any('resolves outside home' in reason for _, reason in failed))

    def test_json_and_bad_homes(self):
        code, text = self.call('--apply', '--json')
        value = json.loads(text)
        self.assertEqual((code, value['applied']), (0, True))
        self.assertIn(str(self.files['oxa']), value['removed'])
        for bad in ('relative/home', '/'):
            out = io.StringIO()
            self.assertEqual(uninstall.main(['--apply'], home=bad, systemctl=str(self.systemctl), out=out), 2)
        self.assertEqual(uninstall.main(['--nope'], home=str(self.home), out=io.StringIO()), 2)


if __name__ == '__main__':
    unittest.main()
