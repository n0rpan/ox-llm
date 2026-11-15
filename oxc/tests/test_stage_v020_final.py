"""Model-free tests of the v0.2.0 (ox-llm) stage generator: temp HOMEs and a recording fake systemctl only.

Nothing here reads or writes a real ~/.config, ~/.local, ~/.local/bin or a real service manager (P56):
every live file is a fixture under a temporary HOME, the engine binaries are fake bytes with a matching
freeze manifest, oxa is a tiny script stand-in (require_elf=False) and the system drop-in roots are off."""
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
sys.path.insert(0, str(Path(__file__).resolve().parent))
import stage_v020_final as gen  # noqa: E402
import stage_layout_fixture as v013t  # noqa: E402  (shared synthetic layouts, no legacy generator)

have_git_head, h, tree, ENGINE_REV = v013t.have_git_head, v013t.h, v013t.tree, v013t.ENGINE_REV

OLD_SERVER = b'v013 oxe-server'


def fake_oxa(path, version='0.2.0'):
    path.write_text('#!/bin/sh\nif [ "$1" = --version ]; then echo "oxa %s"; exit 0; fi\n'
                    'if [ "$1" = identity ]; then echo "$*" > "$HOME/identity-call"; exit 0; fi\nexit 2\n' % version)
    path.chmod(0o700)
    return path


@unittest.skipUnless(have_git_head(), 'needs the lab git checkout (git archive of HEAD)')
class V020StageTest(unittest.TestCase):
    # fixtures borrowed from the v0.1.3 test (same live layout), then moved to the v0.1.3 installed state
    runtime = v013t.StageLayout.runtime
    chat_unit = v013t.StageLayout.chat_unit
    studio_unit = v013t.StageLayout.studio_unit
    studio_dropin = v013t.StageLayout.studio_dropin
    make_engine = v013t.StageLayout.make_engine

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name)
        self.home = self.root / 'home'
        self.calls = self.root / 'systemctl.log'
        self.systemctl = self.root / 'fake-systemctl'
        self.active = self.root / 'active-units'
        self.active.write_text('oxc-chat.service\nprompt-studio-app.service\n')
        self.systemctl.write_text(
            '#!/bin/sh\necho "$@" >> %s\nif [ "$2" = is-active ]; then shift 2; for u in "$@"; do\n'
            '  if grep -qx "$u" %s; then echo active; else echo inactive; fi; done; fi\n' % (self.calls, self.active))
        self.systemctl.chmod(0o700)
        self.paths = gen.live_paths(self.home)
        self.write_live()
        self.base = self.make_base(self.root / 'staged' / 'v013-final')
        self.engine = self.make_engine(self.root / 'staged' / 'v020-artifacts', server=b'v020 oxe-server',
                                       oxe=b'v020 oxe cli')
        self.oxa = fake_oxa(self.root / 'oxa')
        self.out = self.root / 'staged' / 'v020-final'

    def write_live(self, runtime=None, client_config=True):
        rt = runtime or self.runtime(extra_args=('--pf-attn', 'tiled'))
        for alias in rt['profiles']:
            oxe = rt['profiles'][alias].get('memlat_overrides', {}).get('oxe')
            if oxe:
                oxe['server_sha256'] = h(OLD_SERVER)
        content = {
            'unit': self.chat_unit().encode(), 'runtime': gen.dump_json(rt),
            'cli': b'#!/usr/bin/python3\n# v013 cli\n', 'launcher': b'#!/usr/bin/python3\n# v013 launcher\n',
            'studio_dropin': self.studio_dropin().encode(), 'studio_unit': self.studio_unit().encode(),
            'chat_dropin': b'[Service]\nMemoryHigh=infinity\nMemoryMax=28G\nCPUQuota=1200%\n',
            'image_settings': b'{"oxi": {"npu": true}}\n', 'image_engine': b'{"engine": "auto"}'}
        for key, data in content.items():
            path = self.paths[key]
            path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            path.write_bytes(data)
            path.chmod(0o700 if key in ('cli', 'launcher') else 0o600)
        if client_config:  # what the v0.1.2 stage wrote: never a precondition, never changed
            cfg = self.home / gen.OLD_CLIENT_CONFIG
            cfg.parent.mkdir(parents=True, exist_ok=True)
            cfg.write_text('{"providers": {"local-qwen": {}}}\n')

    def make_base(self, base):
        """The installed v0.1.3 stage: its installed_sha256 are the current live files."""
        (base / 'rollback').mkdir(parents=True)
        installed, modes = {}, {}
        for key in gen.BASE_KEYS:
            installed[key] = h(self.paths[key].read_bytes())
            modes[key] = '0700' if key in ('cli', 'launcher') else '0600'
        manifest = {'installed': False, 'paths': {k: str(self.paths[k]) for k in gen.BASE_KEYS},
                    'installed_sha256': installed, 'preimage_mode': modes, 'server_sha256': h(OLD_SERVER),
                    'engine_source': '5' * 40}
        (base / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
        return base

    def generate(self, **kw):
        args = dict(base=self.base, out=self.out, lab_repo=ROOT, lab_rev='HEAD', engine_rev=ENGINE_REV,
                    engine_artifacts=self.engine, oxa_binary=self.oxa, oxa_source='test', home=self.home,
                    systemctl=str(self.systemctl), system_roots=(), oxa_require_elf=False)
        args.update(kw)
        return gen.generate(**args)

    def state(self):
        return {k: ((p.read_bytes(), p.stat().st_mode & 0o7777) if os.path.lexists(p) else None)
                for k, p in self.paths.items()}

    # -- tests ----------------------------------------------------------------------------
    def test_generates_pfg_for_ornith_only_oxa_created_launcher_removed(self):
        before, base_before = self.state(), tree(self.base)
        m = self.generate()
        self.assertEqual(self.state(), before, 'live files are only read')
        self.assertEqual(tree(self.base), base_before)
        self.assertFalse(self.calls.exists(), 'the generator never calls systemctl')
        self.assertEqual(set(m['paths']), set(gen.KEYS))
        self.assertEqual(m['preimage_sha256']['oxa'], 'absent')
        self.assertEqual(m['installed_sha256']['launcher'], 'absent')
        self.assertEqual(m['installed_sha256']['oxa'], h(self.oxa.read_bytes()))
        self.assertEqual(m['installed_mode'], {'oxa': '0700'})
        self.assertFalse((self.out / 'rollback' / 'oxa.original').exists())
        self.assertEqual((self.out / 'rollback' / 'launcher.original').read_bytes(), before['launcher'][0])
        self.assertEqual(m['version'], '0.2.0')
        self.assertEqual(m['oxa']['version'], '0.2.0')
        unit = (self.out / 'oxc-chat.service').read_text().splitlines()
        self.assertEqual(unit.count(gen.KEEP_WARM_LINE), 1)
        self.assertFalse([l for l in unit if 'LOCAL_LLM_IDLE_SECONDS=' in l])
        self.assertEqual(m['keep_warm']['unit_env'], gen.KEEP_WARM_LINE)
        rt = json.loads((self.out / 'runtime.json').read_text())
        live_rt = json.loads(before['runtime'][0])
        ornith = rt['profiles']['ornith-35b-a3b-local']['memlat_overrides']['oxe']['oxe_args']
        qwen = rt['profiles']['qwen3.6-35b-a3b-local']['memlat_overrides']['oxe']['oxe_args']
        self.assertEqual(gen.arg_after(ornith, '--pf-attn'), 'pfg')
        self.assertEqual(gen.arg_after(qwen, '--pf-attn'), 'tiled')
        self.assertEqual(ornith.count('--pf-attn'), 1)
        live_ornith = live_rt['profiles']['ornith-35b-a3b-local']['memlat_overrides']['oxe']['oxe_args']
        self.assertEqual([a for a in ornith if a != 'pfg'], [a for a in live_ornith if a != 'tiled'])
        self.assertEqual(qwen, live_rt['profiles']['qwen3.6-35b-a3b-local']['memlat_overrides']['oxe']['oxe_args'])
        self.assertEqual(rt['profiles'][gen.FLASH], live_rt['profiles'][gen.FLASH])
        self.assertEqual(m['engine_pins']['prefill_attention'], {'ornith-35b-a3b-local': 'pfg',
                                                                 'qwen3.6-35b-a3b-local': 'tiled'})
        self.assertEqual(len(m['migration_notes']), 1)
        self.assertIn('left in place untouched', m['migration_notes'][0])
        self.assertNotIn(gen.OLD_CLIENT_CONFIG, json.dumps(m['preconditions']))
        for name in ('oxc', 'uninstall.py'):
            src = subprocess.run(['git', '-C', str(ROOT), 'show', 'HEAD:./%s/%s' % (gen.RUNTIME_SRC, name)],
                                 check=True, capture_output=True).stdout
            self.assertEqual((self.out / 'runtime-source' / name).read_bytes(), src)
        bundled = subprocess.run([sys.executable, '-I', str(self.out / 'tools' / 'stage_v020_final.py'),
                                  'verify', str(self.out)], capture_output=True, text=True, timeout=60)
        self.assertEqual(bundled.returncode, 0, bundled.stderr)

    def test_install_then_rollback_restores_bytes_modes_and_absence(self):
        before = self.state()
        self.generate()
        self.assertEqual(gen.precheck(self.out, system_roots=(), out=io.StringIO()), 0)
        gen.materialize(self.out, out=io.StringIO())
        log = io.StringIO()
        self.assertEqual(gen.install(self.out, True, system_roots=(), out=log), 0)
        self.assertIn('note: ox-llm v0.2.0 no longer configures or launches a third-party client', log.getvalue())
        self.assertIn('oxa identity: refreshed-offline', log.getvalue())  # O-152, in the scratch HOME
        self.assertEqual((self.home / 'identity-call').read_text(), 'identity --refresh --offline\n')
        (self.home / 'identity-call').unlink()
        self.assertEqual(self.paths['oxa'].read_bytes(), self.oxa.read_bytes())
        self.assertEqual(self.paths['oxa'].stat().st_mode & 0o7777, 0o700)
        self.assertFalse(self.paths['launcher'].exists())
        self.assertEqual(self.paths['cli'].stat().st_mode & 0o7777, 0o700)
        self.assertTrue((self.home / gen.OLD_CLIENT_CONFIG).is_file(), 'client config is never touched')
        check = subprocess.run([sys.executable, '-I', str(self.out / 'rollback/rollback.py'), '--check'],
                               capture_output=True, text=True, timeout=60)
        self.assertEqual(check.returncode, 0, check.stderr)
        roll = subprocess.run([sys.executable, '-I', str(self.out / 'rollback/rollback.py')],
                              capture_output=True, text=True, timeout=60)
        self.assertEqual(roll.returncode, 0, roll.stderr)
        self.assertEqual(self.state(), before, 'rollback restores bytes, modes and the absence of oxa')
        again = subprocess.run([sys.executable, '-I', str(self.out / 'rollback/rollback.py')],
                               capture_output=True, text=True, timeout=60)
        self.assertEqual(again.returncode, 0, again.stderr)
        self.assertIn('Nothing to roll back', again.stdout)

    def test_refuses_existing_oxa_wrong_version_and_auto_or_extra_pf_attn(self):
        self.paths['oxa'].write_text('mine')
        with self.assertRaisesRegex(gen.StageError, 'already exists'):
            self.generate()
        self.paths['oxa'].unlink()
        with self.assertRaisesRegex(gen.StageError, 'not the release version 0.2.0'):
            self.generate(oxa_binary=fake_oxa(self.root / 'old-oxa', '0.0.0'))
        with self.assertRaisesRegex(gen.StageError, 'ELF'):
            self.generate(oxa_require_elf=True)
        self.assertFalse(self.out.exists())
        with self.assertRaises(gen.StageError):
            gen.with_pf_attn(['--pf-attn', 'tiled', '--pf-attn', 'fa2'], 'pfg')
        self.assertEqual(gen.with_pf_attn(['--x', '1'], 'pfg'), ['--x', '1', '--pf-attn', 'pfg'])
        rt = json.loads(self.paths['runtime'].read_text())
        good = json.loads(json.dumps(rt))
        for alias, value in gen.PF_ATTN.items():
            oxe = good['profiles'][alias]['memlat_overrides']['oxe']
            oxe['oxe_args'] = gen.with_pf_attn(oxe['oxe_args'], value)
            oxe['server_sha256'], oxe['server_binary'] = 's', '/p'
        self.assertEqual(gen.check_runtime(good, rt, 's', '/p'), [])
        bad = json.loads(json.dumps(good))
        bad['profiles']['qwen3.6-35b-a3b-local']['memlat_overrides']['oxe']['oxe_args'] = gen.with_pf_attn(
            rt['profiles']['qwen3.6-35b-a3b-local']['memlat_overrides']['oxe']['oxe_args'], 'pfg')
        self.assertTrue(any('qwen3.6' in e and 'tiled' in e for e in gen.check_runtime(bad, rt, 's', '/p')))
        auto = json.loads(json.dumps(good))
        auto['profiles']['ornith-35b-a3b-local']['memlat_overrides']['oxe']['oxe_args'] = gen.with_pf_attn(
            rt['profiles']['ornith-35b-a3b-local']['memlat_overrides']['oxe']['oxe_args'], 'auto')
        self.assertTrue(any('ornith' in e for e in gen.check_runtime(auto, rt, 's', '/p')))

    def test_keep_warm_replaces_the_pinned_legacy_idle_line(self):
        unit = self.chat_unit().replace('Environment=PYTHONDONTWRITEBYTECODE=1',
                                        'Environment="LOCAL_LLM_IDLE_SECONDS=600"\nEnvironment=PYTHONDONTWRITEBYTECODE=1')
        out = gen.set_keep_warm(unit).splitlines()
        self.assertEqual(len(out), len(unit.splitlines()))
        self.assertEqual(out[unit.splitlines().index('Environment="LOCAL_LLM_IDLE_SECONDS=600"')], gen.KEEP_WARM_LINE)
        self.assertEqual(gen.check_unit('\n'.join(out), unit, '/nowhere')[-1:],
                         ['unit: not pointed at the staged release'])  # only the release pointer differs here
        with self.assertRaises(gen.StageError):
            gen.set_keep_warm(unit.replace('Environment=PYTHONDONTWRITEBYTECODE=1', gen.KEEP_WARM_LINE))
        with self.assertRaises(gen.StageError):
            gen.set_keep_warm(unit + 'Environment="LOCAL_LLM_IDLE_SECONDS=900"\n')

    def test_live_drift_refuses_before_writing(self):
        self.paths['cli'].write_bytes(b'#!/usr/bin/python3\n# edited by hand\n')
        with self.assertRaisesRegex(gen.StageError, 'cli: live sha256'):
            self.generate()
        self.assertFalse(self.out.exists())

    def test_no_migration_note_without_an_old_client_config(self):
        (self.home / gen.OLD_CLIENT_CONFIG).unlink()
        self.assertEqual(self.generate()['migration_notes'], [])


if __name__ == '__main__':
    unittest.main()
