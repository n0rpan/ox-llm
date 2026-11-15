"""Uninstall contracts: exact install records, positive stops, and retained owner data."""
from environment import PYTHON
import hashlib
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import unittest

from harness import ROOT
from test_cli_runtime import CANONICAL_HOME, invoke_case

GOLDENS = Path(__file__).with_name('goldens') / 'uninstall.json'
CASES = ('journal-dry', 'journal-apply', 'journal-text', 'changed-file', 'unrecorded-file',
         'program-symlink', 'parent-escape', 'outside-record', 'directory-record',
         'models-kept', 'models-remove', 'model-symlink', 'model-outside', 'model-parent-escape',
         'stop-refused', 'stop-unavailable', 'disable-failed', 'reload-failed', 'change-after-plan',
         'stage-record', 'multiple-records', 'oversized-record', 'symlink-record', 'root-home',
         'relative-home', 'hardlink', 'bar-note')


def capture(name):
    home = CANONICAL_HOME
    binary_dir = home / 'bin'
    binary_dir.mkdir()
    executable = binary_dir / 'systemctl'
    source = (ROOT / 'conformance/fixtures/uninstall_host.py').read_text().split('\n', 1)[1]
    executable.write_text('#!' + PYTHON + '\n' + source)
    executable.chmod(0o700)
    program = home / '.local/bin/oxa'
    program.parent.mkdir(parents=True)
    program.write_text('synthetic installed program\n')
    unit = home / '.config/systemd/user/oxc-chat.service'
    unit.parent.mkdir(parents=True)
    unit.write_text('synthetic controller unit\n')
    studio = unit.with_name('prompt-studio-app.service')
    studio.write_text('synthetic owner studio unit\n')
    dropin = unit.with_name(unit.name + '.d')
    dropin.mkdir()
    marker = dropin / 'keep-owner.conf'
    marker.write_text('synthetic owner drop-in\n')
    unrelated = program.with_name('owner-file')
    unrelated.write_text('synthetic unrelated file\n')
    outside_dir = Path('/tmp/oxc-uninstall-outside')
    outside_dir.mkdir()
    outside = outside_dir / "model owner's copy.gguf"
    outside.write_text('synthetic outside owner data\n')
    record = home / 'install-record.json'
    recorded = [program, unit]
    observed = [program, unit, studio, marker, unrelated, outside]
    def sha(path):
        return hashlib.sha256(path.read_bytes()).hexdigest()
    journal = {'kind': 'sandbox-source-install', 'installed': True, 'target_home': str(home),
               'files': [{'path': str(path.relative_to(home)), 'sha256': sha(path)} for path in recorded]}
    record.write_text(json.dumps(journal))
    arguments = ['uninstall', '--record', str(record), '--json']
    if name != 'journal-dry':
        arguments.append('--apply')
    if name == 'journal-text':
        arguments.remove('--json')
    if name == 'changed-file':
        program.write_text('synthetic owner-edited program\n')
    if name == 'unrecorded-file':
        record.write_text('{}')
    if name in ('program-symlink', 'parent-escape'):
        program.unlink()
        if name == 'program-symlink':
            program.symlink_to(outside)
        else:
            escaped = home / 'escaped-parent'
            escaped.symlink_to(outside_dir, target_is_directory=True)
            program = escaped / outside.name
            journal['files'].append({'path': str(program.relative_to(home)), 'sha256': sha(outside)})
            record.write_text(json.dumps(journal))
            observed.append(program)
    if name == 'outside-record':
        record.write_text(json.dumps({'paths': {'outside': str(outside)}, 'installed_sha256': {'outside': sha(outside)}}))
    if name == 'directory-record':
        journal['files'].append({'path': str(program.parent.relative_to(home)), 'sha256': '0' * 64})
        record.write_text(json.dumps(journal))
    if name.startswith('model'):
        model = home / 'models/synthetic.gguf'
        model.parent.mkdir()
        model.write_text('synthetic owned model\n')
        observed.append(model)
        if name == 'model-symlink':
            model.unlink(); model.symlink_to(outside)
        if name == 'model-outside':
            model = outside
        if name == 'model-parent-escape':
            linked = home / 'linked-models'
            linked.symlink_to(outside_dir, target_is_directory=True)
            model = linked / outside.name
        config = home / '.config/oxc/llm/runtime.json'
        config.parent.mkdir(parents=True)
        config.write_text(json.dumps({'models': [{'model_path': str(model), 'sidecar_path': str(model)}]}))
        if name != 'models-kept':
            arguments.append('--remove-models')
    if name == 'stage-record':
        release = home / '.local/share/ai-stack/runtime-releases/synthetic'
        release.mkdir(parents=True)
        runtime = release / 'controller.txt'
        runtime.write_text('synthetic release source\n')
        engine = home / '.local/share/ai-stack/releases/synthetic'
        engine.mkdir(parents=True)
        server = engine / 'oxe-server'
        server.write_text('synthetic executor, never run\n')
        record.write_text(json.dumps({'paths': {'entry': str(program), 'unit': str(unit)},
                         'installed_sha256': {'entry': sha(program), 'unit': sha(unit)},
                         'release_dir': str(release), 'runtime_source_sha256': {'controller.txt': sha(runtime)},
                         'engine_dir': str(engine), 'server_sha256': sha(server)}))
        update = home / '.local/state/oxc/update/state.json'
        update.parent.mkdir(parents=True)
        stage = home / 'stage'
        stage.mkdir()
        (stage / 'manifest.json').write_text(record.read_text())
        update.write_text(json.dumps({'current': {'stage': str(stage)}, 'history': [{'stage': str(stage)}]}))
        arguments = ['uninstall', '--apply', '--json']
        observed += [runtime, server, release, engine]
    if name == 'multiple-records':
        extra = home / 'extra-record.json'
        extra.write_text(json.dumps({'paths': {'extra': str(unrelated)}, 'installed_sha256': {'extra': sha(unrelated)}}))
        arguments += ['--record', str(extra)]
    if name == 'oversized-record':
        with record.open('r+b') as stream:
            stream.truncate(8 * 1024**2 + 1)
    if name == 'symlink-record':
        target = home / 'record-target.json'
        record.rename(target)
        record.symlink_to(target)
    if name == 'hardlink':
        hardlink = home / 'retained-hardlink'
        os.link(program, hardlink)
        observed.append(hardlink)
    if name == 'bar-note':
        shell = home / '.config/omarchy/shell.json'
        shell.parent.mkdir(parents=True)
        shell.write_text('{"bar": ["snapdragon.local-ai", "synthetic-owner-widget"]}')
        observed.append(shell)
    state = {'units': {'oxc-chat.service': 'active', 'prompt-studio-app.service': 'active'}}
    for case, option in (('stop-refused', 'stop_refused'), ('stop-unavailable', 'short_reply'),
                         ('disable-failed', 'disable_failed'), ('reload-failed', 'daemon-reload_failed')):
        if name == case:
            state[option] = True
    if name == 'change-after-plan':
        state['change_on_stop'] = str(program)
    (home / 'systemd-state.json').write_text(json.dumps(state))
    entry = home / 'oxc'
    entry.symlink_to(ROOT / 'oxc/src/oxc/runtime/oxc')
    command = shlex.split(os.environ['OXC_COMMAND']) if 'OXC_COMMAND' in os.environ else [PYTHON, str(entry)]
    environment = {**os.environ, 'PATH': str(binary_dir) + ':/usr/bin:/bin', 'FAKE_UNINSTALL_HOME': str(home)}
    if name in ('root-home', 'relative-home'):
        environment['HOME'] = '/' if name == 'root-home' else 'relative'
    result = subprocess.run([*command, *arguments], cwd=home, env=environment,
                            capture_output=True, text=True, stdin=subprocess.DEVNULL, timeout=10)
    snapshots = {}
    for path in observed:
        if path.is_symlink():
            snapshots[str(path)] = {'symlink': str(path.readlink())}
        elif path.is_file():
            snapshots[str(path)] = {'sha256': sha(path)}
        else:
            snapshots[str(path)] = {'directory': path.is_dir(), 'exists': path.exists()}
    commands = home / 'host-commands.jsonl'
    output = {'exit': result.returncode, 'stdout': result.stdout, 'stderr': result.stderr,
              'host_commands': [json.loads(line) for line in commands.read_text().splitlines()] if commands.exists() else [],
              'files': snapshots}
    return json.loads(json.dumps(output).replace(str(home), '<HOME>'))


class UninstallContract(unittest.TestCase):
    def test_uninstall_goldens(self):
        expected = json.loads(GOLDENS.read_text())
        self.assertEqual(set(expected), set(CASES))
        for name in CASES:
            with self.subTest(case=name):
                self.assertEqual(invoke_case(name, __file__), expected[name])


if __name__ == '__main__':
    if sys.argv[1:] == ['--record-python']:
        if 'OXC_COMMAND' in os.environ or 'OXC_DAEMON_COMMAND' in os.environ:
            raise SystemExit('Goldens must be recorded from the Python reference')
        GOLDENS.write_text(json.dumps({name: invoke_case(name, __file__) for name in CASES}, indent=2, sort_keys=True) + '\n')
    elif sys.argv[1:2] == ['--case']:
        print(json.dumps(capture(sys.argv[2])))
    else:
        unittest.main()
