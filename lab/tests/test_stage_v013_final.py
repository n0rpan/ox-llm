"""Model-free tests of the O-119 v0.1.3 stage generator: temp HOMEs and a recording fake systemctl only.

Nothing here reads or writes a real ~/.config, ~/.local or a real service manager: every live
file is a fixture under a temporary HOME, the engine binaries are fake bytes with a matching
freeze manifest, and the system drop-in roots are disabled (system_roots=())."""
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'tools'))
import install_staged_release as isr  # noqa: E402
import stage_controller_release as controller_release  # noqa: E402
import stage_v013_final as gen  # noqa: E402

ENGINE_REV = '6' * 40
OLD_SERVER = b'v012 oxe-server'
MOE = {'ornith-35b-a3b-local': ('a' * 64, '1073741824', '32768'), 'qwen3.6-35b-a3b-local': ('b' * 64, '536870912', '16384')}


def h(data):
    return hashlib.sha256(data).hexdigest()


# v0.2.0 (O-151) moved the client launcher out of the shipped tree; the v0.1.3 generator is history and
# is tested against the last lab commit that still has its sources (skipped in a shallow checkout).
V013_LAB_REV = '34ddd87e7b35a123603f1a0df1a242c5e5701603'


def have_git_head(rev='HEAD'):
    try:
        subprocess.run(['git', '-C', str(ROOT), 'rev-parse', '--verify', rev + '^{commit}'], check=True, capture_output=True)
        return True
    except (OSError, subprocess.CalledProcessError):
        return False


def tree(path):
    return {p.relative_to(path).as_posix(): (h(p.read_bytes()), p.stat().st_mode & 0o7777)
            for p in sorted(Path(path).rglob('*')) if p.is_file()}


@unittest.skipUnless(have_git_head(V013_LAB_REV), 'needs the lab git history (git archive of V013_LAB_REV)')
class V013StageTest(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name)
        self.home = self.root / 'home'
        self.calls = self.root / 'systemctl.log'
        self.systemctl = self.root / 'fake-systemctl'
        self.active = self.root / 'active-units'  # fake is-active: units listed here are active
        self.active.write_text('local-ai-chat.service\nprompt-studio-app.service\n')
        self.systemctl.write_text(
            '#!/bin/sh\necho "$@" >> %s\nif [ "$2" = is-active ]; then shift 2; for u in "$@"; do\n'
            '  if grep -qx "$u" %s; then echo active; else echo inactive; fi; done; fi\n' % (self.calls, self.active))
        self.systemctl.chmod(0o700)
        self.paths = gen.live_paths(self.home)
        self.write_live()
        self.base = self.make_base(self.root / 'staged' / 'v012-final')
        self.engine = self.make_engine(self.root / 'staged' / 'v013-artifacts')
        self.out = self.root / 'staged' / 'v013-final'

    # -- fixtures -------------------------------------------------------------------------
    def runtime(self, extra_args=(), extra_env=None):
        engine_dir = str(self.home / gen.ENGINES / 'oxe-v012-final-old')
        profiles = {}
        for alias, (model_sha, ram, mtp) in MOE.items():
            env = {'OXE_ATTN_TILED_MIN_KV': '0', 'OXE_VL': '15', 'LOCAL_AI_ENGINE_LEDGER': '1', **(extra_env or {})}
            profiles[alias] = {
                'display_name': 'Ornith 1.5 35B A3B' if alias.startswith('ornith') else 'Qwen 3.6 35B MoE',
                'sha256': model_sha, 'context_size': 16384, 'threads': 4,
                'memlat_overrides': {'oxe': {
                    'server_binary': engine_dir + '/oxe-server', 'server_sha256': h(OLD_SERVER),
                    'context_size': 131072, 'threads': 8,
                    'oxe_args': ['--engine', 'oxe', '--ctx-size', '{context}', '--oxe-cpus', '0-7',
                                 '--oxe-prefill-cpus', '0-11', '--ctx-checkpoints', '8', '--session-ram-bytes', ram,
                                 '--park-disk-min-bytes', '536870912', '--mtp-max-ctx', mtp, *extra_args],
                    'environment': env,
                    'validation': {'kind': 'v012-final-staged-unqualified', 'qualified': False}}}}
        profiles[gen.FLASH] = {'display_name': 'Flash', 'memlat_overrides': {'oxe_flash': {
            'cache_pref': '18', 'prefix_store': {'enabled': True}, 'sidecar_path': '/x/experts-r4.bin',
            'server_sha256': 'f' * 64,
            'oxe_args': ['--flash-sidecar', '/x/experts-r4.bin', '--flash-psi-some', '0', '--flash-psi-full', '0']}}}
        return {'model_alias': 'ornith-35b-a3b-local', 'selected_model': 'ornith-35b-a3b-local',
                'profiles': profiles, 'cache': dict(gen.CACHE)}

    def opencode(self):
        moe = {'limit': {'context': 131072, 'input': 65536, 'output': 16384}}
        return {'$schema': 'https://example.invalid/config.json', 'providers': {'local-qwen': {
            'settings': {'headerTimeout': 1800000, 'chunkTimeout': 1800000},
            'models': {'qwen-local': {'limit': {'context': 16384, 'input': 12288, 'output': 16384}},
                       gen.FLASH: {'limit': {'context': 16384, 'input': 12288, 'output': 2048}},
                       'ornith-35b-a3b-local': dict(moe), 'qwen3.6-35b-a3b-local': dict(moe)}}}}

    def chat_unit(self):
        release = self.home / gen.RELEASES / 'v012-final-9e1fceca421a'
        return '\n'.join([
            '[Unit]', 'Description=Private local AI runtime (controller.py)', '[Service]', gen.WATCHDOG,
            'Environment=LOCAL_AI_IMAGE_NPU=1', 'Type=simple', 'WorkingDirectory=%s' % release,
            'ExecStart=/usr/bin/python3 "%s/controller.py"' % release,
            'Environment="LOCAL_AI_LEASE=/x/inference.lock"', 'Environment="LOCAL_LLM_CONFIG=/x/runtime.json"',
            'Environment=PYTHONDONTWRITEBYTECODE=1', 'Restart=on-failure', 'MemoryMax=25G', '[Install]',
            'WantedBy=default.target']) + '\n'

    def studio_unit(self):
        release = self.home / gen.RELEASES / 'studio-handoff-a2930ab1976cdf969e7d'
        return '\n'.join([
            '[Unit]', 'Description=Private local AI runtime (studio)', '[Service]', 'Type=simple',
            'WorkingDirectory=%s' % release, 'ExecStart=/usr/bin/python3 "%s/studio"' % release,
            'Environment="LOCAL_AI_LEASE=/x/inference.lock"', 'Environment="LOCAL_IMAGE_CONFIG_DIR=/x/images"',
            'Restart=on-failure', 'RestartSec=5', 'MemoryMax=22G', '[Install]', 'WantedBy=default.target']) + '\n'

    def studio_dropin(self):
        release = self.home / gen.RELEASES / 'studio-npu-w1-7de44783af15'
        return '\n'.join(['[Service]', 'WorkingDirectory=%s' % release, 'ExecStart=',
                          'ExecStart=/usr/bin/python3 "%s/studio"' % release,
                          'Environment="OXI_BIN=/x/releases/oxi-fc267f50a3e0/oxi"',
                          'Environment="SD_CLI_PROBE=/x/releases/sd-probe-4a47a2659438/sd-cli"',
                          'CPUQuota=1200%']) + '\n'

    def write_live(self, runtime=None):
        content = {
            'unit': self.chat_unit().encode(), 'runtime': gen.dump_json(runtime or self.runtime()),
            'cli': b'#!/usr/bin/python3\n# v012 cli\n', 'launcher': b'#!/usr/bin/python3\n# v012 launcher\n',
            'studio_dropin': self.studio_dropin().encode(), 'opencode': gen.dump_json(self.opencode()),
            'studio_unit': self.studio_unit().encode(),
            'chat_dropin': b'[Service]\nMemoryHigh=infinity\nMemoryMax=28G\nCPUQuota=1200%\n',
            'image_settings': b'{"oxi": {"npu": true}}\n', 'image_engine': b'{"engine": "auto"}'}
        for key, data in content.items():
            path = self.paths[key]
            path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            path.write_bytes(data)
            path.chmod(0o700 if key in ('cli', 'launcher') else 0o600)

    def make_base(self, base):
        """The installed v0.1.2 stage: its installed_sha256 are the current live files."""
        (base / 'rollback').mkdir(parents=True)
        staged = {'unit': 'local-ai-chat.service', 'runtime': 'runtime.json', 'opencode': 'opencode.json',
                  'cli': 'local-ai', 'launcher': 'launch_local_plan.py'}
        installed, modes = {}, {}
        for key, name in staged.items():
            data = self.paths[key].read_bytes()
            (base / name).write_bytes(data)
            (base / 'rollback' / (key + '.original')).write_bytes(b'v011 ' + key.encode())
            installed[key] = h(data)
            modes[key] = '0700' if key in ('cli', 'launcher') else '0600'
        manifest = {'installed': False, 'paths': {k: str(self.paths[k]) for k in staged}, 'staged_files': staged,
                    'preimage_sha256': {k: h(b'v011 ' + k.encode()) for k in staged}, 'installed_sha256': installed,
                    'preimage_mode': modes, 'server_sha256': h(OLD_SERVER), 'engine_source': '5' * 40,
                    'engine_dir': str(self.home / gen.ENGINES / 'oxe-v012-final-old')}
        (base / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
        return base

    def make_engine(self, d, server=b'v013 oxe-server', oxe=b'v013 oxe cli'):
        (d / 'default').mkdir(parents=True)
        (d / 'default' / 'oxe-server').write_bytes(server)
        (d / 'default' / 'oxe').write_bytes(oxe)
        (d / 'manifest.json').write_text(json.dumps({'passed': True, 'source': ENGINE_REV, 'jobs': [
            {'kind': 'default', 'returncode': 0, 'command': ['cargo', 'build'], 'artifacts': {
                'oxe-server': {'sha256': h(server)}, 'oxe': {'sha256': h(oxe)}}}]}))
        return d

    def generate(self, **kw):
        args = dict(base=self.base, out=self.out, lab_repo=ROOT, lab_rev=V013_LAB_REV, engine_rev=ENGINE_REV,
                    engine_artifacts=self.engine, home=self.home, systemctl=str(self.systemctl), system_roots=())
        args.update(kw)
        return gen.generate(**args)

    def live_state(self):
        return {k: (p.read_bytes(), p.stat().st_mode & 0o7777) for k, p in self.paths.items()}

    def rollback(self, *args):
        return subprocess.run([sys.executable, '-I', str(self.out / 'rollback' / 'rollback.py'), *args],
                              capture_output=True, text=True, timeout=60)

    # -- tests ----------------------------------------------------------------------------
    def test_generates_guarded_stage_from_current_live_state(self):
        base_before, live_before = tree(self.base), self.live_state()
        m = self.generate()
        self.assertEqual(tree(self.base), base_before, 'base stage must stay immutable')
        self.assertEqual(self.live_state(), live_before, 'live files are only read')
        self.assertFalse(self.calls.exists(), 'the generator never calls systemctl')
        self.assertEqual(set(m['paths']), set(gen.KEYS))
        for key in gen.KEYS:  # preimages are the CURRENT live bytes, not v0.1.2's own predecessors
            self.assertEqual(m['preimage_sha256'][key], h(live_before[key][0]))
            self.assertEqual((self.out / 'rollback' / (key + '.original')).read_bytes(), live_before[key][0])
        self.assertEqual(m['preimage_mode'], {'unit': '0600', 'runtime': '0600', 'cli': '0700', 'launcher': '0700',
                                              'studio_dropin': '0600'})
        self.assertEqual(set(m['preconditions']), set(gen.PRECONDITIONS))
        head = subprocess.run(['git', '-C', str(ROOT), 'rev-parse', V013_LAB_REV], check=True, capture_output=True,
                              text=True).stdout.strip()
        self.assertEqual(m['lab_source'], head)
        # the pinned revision predates the oxc rename: read its files through export_tree (legacy layout)
        with tempfile.TemporaryDirectory() as pinned:
            _, runtime_dir = gen.export_tree(ROOT, V013_LAB_REV, gen.RUNTIME_SRC, pinned)
            _, launcher_dir = gen.export_tree(ROOT, V013_LAB_REV, gen.LAUNCHER_SRC, pinned)
            # the runtime-source keeps each file under the name it has in that revision (legacy CLI name)
            cases = ((controller_release.LEGACY_CLI, Path(runtime_dir) / controller_release.LEGACY_CLI, 'cli'),
                     ('launch_local_plan.py', Path(launcher_dir) / 'launch_local_plan.py', 'launcher'))
            for name, source, staged in cases:
                src = source.read_bytes()
                self.assertEqual((self.out / gen.STAGED[staged]).read_bytes(), src)
                self.assertEqual((self.out / 'runtime-source' / name).read_bytes(), src)
        # runtime: only the engine re-pin differs from live; Flash byte-identical
        live_rt = json.loads(live_before['runtime'][0])
        rt = json.loads((self.out / 'runtime.json').read_text())
        diff = {'.'.join(p[:5]) for p in gen.json_diff(live_rt, rt)}
        self.assertEqual(diff, {'profiles.%s.memlat_overrides.oxe.%s' % (a, k) for a in gen.MOE_MODELS
                                for k in ('server_binary', 'server_sha256', 'validation', 'oxe_args')})
        for alias in gen.MOE_MODELS:
            oxe = rt['profiles'][alias]['memlat_overrides']['oxe']
            live_args = live_rt['profiles'][alias]['memlat_overrides']['oxe']['oxe_args']
            self.assertEqual(oxe['oxe_args'], live_args + ['--pf-attn', 'tiled'])
            self.assertEqual(oxe['server_binary'], m['engine_dir'] + '/oxe-server')
            self.assertEqual(oxe['server_sha256'], h(b'v013 oxe-server'))
            self.assertEqual(oxe['validation']['engine_source'], ENGINE_REV)
        self.assertEqual(rt['profiles'][gen.FLASH], live_rt['profiles'][gen.FLASH])
        gate = json.loads((self.out / 'runtime.gate.json').read_text())
        self.assertEqual(gate['profiles']['ornith-35b-a3b-local']['memlat_overrides']['oxe']['server_binary'],
                         str(self.out / 'engine' / 'oxe-server'))
        # unit and Studio drop-in: only WorkingDirectory/ExecStart move to the one v0.1.3 release
        unit = (self.out / 'local-ai-chat.service').read_text()
        self.assertIn('ExecStart=/usr/bin/python3 "%s/controller.py"' % m['release_dir'], unit)
        self.assertEqual(unit.count(gen.WATCHDOG), 1)
        dropin = (self.out / '10-imgship.conf').read_text().splitlines()
        self.assertIn('ExecStart=', dropin)
        self.assertIn('ExecStart=/usr/bin/python3 "%s/studio"' % m['release_dir'], dropin)
        self.assertIn('Environment="OXI_BIN=/x/releases/oxi-fc267f50a3e0/oxi"', dropin)
        self.assertEqual(m['studio']['restart'], 'on-failure')
        self.assertIn('studio-npu-w1-7de44783af15', m['studio']['previous_exec'])
        for name in gen.STUDIO_FILES:
            self.assertTrue((self.out / 'runtime-source' / name).is_file(), name)
        # provenance and engine
        self.assertEqual(m['engine_source'], ENGINE_REV)
        self.assertEqual(m['engine_freeze']['freeze_manifest_sha256'], h((self.engine / 'manifest.json').read_bytes()))
        self.assertEqual((self.out / 'engine' / 'oxe-server').read_bytes(), b'v013 oxe-server')
        self.assertEqual(m['replaced_engine']['server_sha256'], h(OLD_SERVER))
        self.assertTrue(m['release_dir'].startswith(str(self.home / gen.RELEASES / 'v013-final-')))
        for path in self.out.rglob('*'):
            if path.is_file():
                self.assertFalse(path.stat().st_mode & 0o222, path)
        bundled = subprocess.run([sys.executable, '-I', str(self.out / 'tools' / 'stage_v013_final.py'),
                                  'verify', str(self.out)], capture_output=True, text=True, timeout=60)
        self.assertEqual(bundled.returncode, 0, bundled.stderr)
        overlay = json.loads((self.out / 'opencode-mode-overlay.deep.json').read_text())
        self.assertEqual(overlay['providers']['local-qwen']['settings']['chunkTimeout'], gen.TIMEOUT_MS)

    def test_install_then_one_step_rollback_restores_bytes_modes_and_both_services(self):
        before = self.live_state()
        self.generate()
        self.assertEqual(gen.precheck(self.out, system_roots=(), out=io.StringIO()), 0)
        self.assertEqual(self.live_state(), before)
        gen.materialize(self.out, out=io.StringIO())
        self.assertEqual(gen.install(self.out, True, system_roots=(), out=io.StringIO()), 0)
        self.assertEqual(json.loads((self.out / gen.SERVICE_STATE).read_text())['active'],
                         {'local-ai-chat.service': True, 'prompt-studio-app.service': True})
        self.assertEqual(gen.verify(self.out), len(json.loads((self.out / gen.SEAL).read_text())['files']))
        for key in gen.KEYS:
            self.assertEqual(self.paths[key].read_bytes(), (self.out / gen.STAGED[key]).read_bytes())
            self.assertEqual(self.paths[key].stat().st_mode & 0o7777, before[key][1])
        for key in gen.PRECONDITIONS:
            self.assertEqual(self.live_state()[key], before[key])
        self.assertEqual(self.calls.read_text().splitlines(),
                         ['--user is-active local-ai-chat.service prompt-studio-app.service'],
                         'install only queries the service state')
        self.calls.unlink()
        self.paths['launcher'].chmod(0o600)  # a hand chmod is undone by the exact-mode rollback
        self.assertEqual(self.rollback('--check').returncode, 0)
        result = self.rollback()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.live_state(), before)
        self.assertEqual(self.calls.read_text().splitlines(), [
            '--user stop local-ai-chat.service prompt-studio-app.service', '--user daemon-reload',
            '--user start local-ai-chat.service prompt-studio-app.service'])

    def test_rollback_restores_prior_active_state(self):
        self.active.write_text('local-ai-chat.service\n')  # Studio was stopped before the install
        self.generate()
        gen.install(self.out, True, system_roots=(), out=io.StringIO())
        self.assertEqual(json.loads((self.out / gen.SERVICE_STATE).read_text())['states'],
                         {'local-ai-chat.service': 'active', 'prompt-studio-app.service': 'inactive'})
        self.active.write_text('local-ai-chat.service\nprompt-studio-app.service\n')  # started later by hand
        check = self.rollback('--check')
        self.assertIn('would start local-ai-chat.service [active before install', check.stdout)
        self.calls.unlink()
        result = self.rollback()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls.read_text().splitlines(), [
            '--user stop local-ai-chat.service prompt-studio-app.service', '--user daemon-reload',
            '--user start local-ai-chat.service'])

    def test_rollback_without_record_uses_current_state_and_rejects_bad_record(self):
        self.generate()
        isr.run(self.out, True, out=io.StringIO())  # installed without the wrapper: no record
        self.active.write_text('prompt-studio-app.service\n')
        result = self.rollback()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls.read_text().splitlines()[-1], '--user start prompt-studio-app.service')
        self.calls.unlink()
        isr_out = io.StringIO()
        self.assertEqual(isr.run(self.out, True, out=isr_out), 0)
        (self.out / gen.SERVICE_STATE).write_text('{"active": {"local-ai-chat.service": "yes"}}')
        result = self.rollback()
        self.assertEqual(result.returncode, 5)
        self.assertNotIn('stop', self.calls.read_text() if self.calls.exists() else '')

    def test_record_services_records_once(self):
        self.generate()
        first = gen.record_services(self.out, out=io.StringIO())
        self.active.write_text('')
        self.assertEqual(gen.record_services(self.out, out=io.StringIO())['active'], first['active'])

    def test_rehearsal_in_private_home(self):
        live_before = self.live_state()
        work = self.root / 'rehearsal'
        receipt = gen.rehearse(base=self.base, workdir=work, lab_repo=ROOT, lab_rev=V013_LAB_REV, engine_rev=ENGINE_REV,
                               engine_artifacts=self.engine, source_home=self.home, system_roots=(), out=io.StringIO())
        self.assertTrue(receipt['passed'], receipt['problems'])
        self.assertEqual(self.live_state(), live_before, 'the source HOME is only read')
        self.assertFalse(self.calls.exists())
        self.assertEqual(receipt['systemctl_calls'][0], '--user is-active local-ai-chat.service prompt-studio-app.service')
        self.assertEqual(len(receipt['systemctl_calls']), 4)
        self.assertTrue(json.loads((work / 'REHEARSAL.json').read_text())['passed'])
        self.assertTrue(Path(receipt['stage']).is_relative_to(work))
        m = json.loads((work / 'stage' / 'manifest.json').read_text())
        self.assertTrue(m['release_dir'].startswith(str(work / 'home')))
        self.assertTrue(Path(m['release_dir'], 'studio').is_file())

    def test_refuses_live_drift_from_the_v012_install(self):
        self.paths['runtime'].write_bytes(self.paths['runtime'].read_bytes().replace(b'"18"', b'"14"'))
        with self.assertRaisesRegex(gen.StageError, 'runtime: live sha256 .* is not what'):
            self.generate()
        self.assertFalse(self.out.exists())

    def test_refuses_setting_changes_and_bad_engine(self):
        for n, (extra_args, extra_env, pattern) in enumerate((
                (('--pf-attn', 'fa2'), None, 'prefill attention'),
                ((), {'OXE_ATTN': 'fused'}, 'fused attention'),
                ((), {'OXE_PF_ATTN': 'fa2x'}, 'prefill attention'),
                (('--pf-attn-auto-min-kv', '8192'), None, 'prefill attention'))):
            self.write_live(self.runtime(extra_args, extra_env))
            self.base = self.make_base(self.root / 'staged' / ('bad-%d' % n))
            with self.assertRaisesRegex(gen.StageError, pattern):
                self.generate()
            self.assertFalse(self.out.exists())
        self.write_live(self.runtime(('--pf-attn', 'tiled')))  # an existing tiled pin is kept, not doubled
        self.base = self.make_base(self.root / 'staged' / 'pinned')
        m = self.generate()
        rt = json.loads((self.out / 'runtime.json').read_text())
        self.assertEqual(rt['profiles']['ornith-35b-a3b-local']['memlat_overrides']['oxe']['oxe_args'].count('--pf-attn'), 1)
        self.out = self.root / 'staged' / 'v013-final-2'
        self.write_live()
        self.base = self.make_base(self.root / 'staged' / 'clean')
        bad = self.make_engine(self.root / 'bad-engine')
        (bad / 'default' / 'oxe-server').chmod(0o600)
        (bad / 'default' / 'oxe-server').write_bytes(b'tampered')
        with self.assertRaisesRegex(gen.StageError, 'not the frozen'):
            self.generate(engine_artifacts=bad)
        with self.assertRaisesRegex(gen.StageError, 'engine-rev'):
            self.generate(engine_rev='6940301')
        with self.assertRaisesRegex(gen.StageError, 'passed must be true and source'):
            self.generate(engine_rev='7' * 40)
        with self.assertRaisesRegex(gen.StageError, 'live configuration'):
            self.generate(out=self.home / '.config' / 'stage')

    def test_dropin_conflicts_refuse(self):
        extra = self.home / '.config/systemd/user/prompt-studio-app.service.d/20-local.conf'
        extra.write_text('[Service]\nExecStart=\nExecStart=/bin/true\n')
        with self.assertRaisesRegex(gen.StageError, 'unexpected drop-in'):
            self.generate()
        extra.unlink()
        self.paths['chat_dropin'].write_text('[Service]\nWorkingDirectory=/tmp\n')
        self.base = self.make_base(self.root / 'staged' / 'b2')
        with self.assertRaisesRegex(gen.StageError, 'chat drop-in'):
            self.generate()
        self.assertFalse(self.out.exists())

    def test_precheck_and_rollback_guard_drift(self):
        self.generate()
        extra = self.home / '.config/systemd/user/service.d/10-all.conf'
        extra.parent.mkdir()
        extra.write_text('[Service]\nNice=0\n')
        with self.assertRaisesRegex(gen.StageError, 'drop-in scan differs'):
            gen.precheck(self.out, system_roots=(), out=io.StringIO())
        extra.unlink()
        extra.parent.rmdir()
        self.paths['opencode'].write_bytes(self.paths['opencode'].read_bytes() + b' ')
        with self.assertRaisesRegex(gen.StageError, 'opencode: .* changed since staging'):
            gen.precheck(self.out, system_roots=(), out=io.StringIO())
        self.assertEqual(isr.run(self.out, True, out=io.StringIO()), 0)
        self.paths['studio_dropin'].write_text('[Service]\nCPUQuota=100%\n')  # edited by hand after install
        result = self.rollback()
        self.assertEqual(result.returncode, 3, result.stderr)
        self.assertIn('studio_dropin', result.stderr)
        self.assertFalse(self.calls.exists(), 'a stale rollback never touches a service')

    def test_materialize_is_idempotent_and_refuses_foreign_content(self):
        m = self.generate()
        self.assertEqual(gen.materialize(self.out, out=io.StringIO()), {'release_dir': 'created', 'engine_dir': 'created'})
        self.assertEqual(gen.materialize(self.out, out=io.StringIO()), {'release_dir': 'present', 'engine_dir': 'present'})
        self.assertEqual(Path(m['release_dir'], 'studio').stat().st_mode & 0o777, 0o700)
        self.assertEqual(Path(m['release_dir'], 'controller.py').stat().st_mode & 0o777, 0o600)
        Path(m['engine_dir'], 'oxe').write_bytes(b'other')
        with self.assertRaisesRegex(gen.StageError, 'different content'):
            gen.materialize(self.out, out=io.StringIO())


class EffectiveServiceTest(unittest.TestCase):
    def test_reset_and_double_execstart(self):
        main = '[Service]\nExecStart=/a\nRestart=on-failure\nEnvironment="A=1" B=2\n'
        eff = gen.effective_service(main, ['[Service]\nExecStart=\nExecStart=/b\n'])
        self.assertEqual((eff['ExecStart'], eff['Restart'], eff['Environment']), ('/b', 'on-failure', {'A': '1', 'B': '2'}))
        with self.assertRaisesRegex(gen.StageError, '2 ExecStart'):
            gen.effective_service(main, ['[Service]\nExecStart=/b\n'])

    def test_dropin_rewrite_requires_reset(self):
        with self.assertRaisesRegex(gen.StageError, 'reset'):
            gen.rewrite_studio_dropin('[Service]\nWorkingDirectory=/a\nExecStart=/usr/bin/python3 "/a/studio"\n', '/r')


if __name__ == '__main__':
    unittest.main()
