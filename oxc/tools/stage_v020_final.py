#!/usr/bin/env python3
"""v0.2.0 (ox-llm) install stage: generate from the CURRENT live state, never install.

The stage is computed against the files that are live NOW (the v0.1.3 install: --base is the
installed v0.1.3 stage, whose installed_sha256 must match the live files). Six guarded keys, bound
by stage_rollback_guard and installed by tools/install_staged_release.py (both are copied into
<stage>/tools, with this file and stage_v013_final.py, whose helpers it reuses):

  unit           oxc-chat.service  WorkingDirectory/ExecStart -> the v0.2.0 release, and
                                        LOCAL_LLM_IDLE_SECONDS=600 replaced by
                                        LOCAL_LLM_KEEP_WARM_IDLE_SECONDS=7200 (O-160); every other
                                        line byte-identical
  runtime        runtime.json           both MoE oxe profiles re-pinned to the frozen candidate
                                        oxe-server (server_binary/server_sha256/validation) and the
                                        prefill attention set per model (O-141): Ornith `--pf-attn pfg`,
                                        Qwen 3.6 stays `--pf-attn tiled` (the engine's `auto` would pick
                                        fa2 above n_ctx 131072, so both stay explicit); every other
                                        setting unchanged; Flash profile byte-identical
  cli            ~/.local/bin/oxc  from --lab-rev (launches oxa; `uninstall`)
  studio_dropin  prompt-studio-app.service.d/10-imgship.conf -> the same v0.2.0 release
  oxa            ~/.local/bin/oxa       NEW file (preimage absent; rollback removes it): the coding
                                        agent, from --oxa-binary (until oxa/ is imported and built by
                                        the release workflow); its `--version` must report --oxa-version
  launcher       ~/.local/bin/launch_local_plan.py  REMOVED (O-151: no client launcher ships;
                                        rollback puts the v0.1.3 bytes back)

Unchanged but checked (`preconditions`, re-verified by --precheck before install): the Studio unit,
oxc-chat.service.d/model-profiles.conf, images/runtime-settings.json and engine.json. A client's
own configuration is never read as a precondition and never changed; a client config an older install
wrote (the v0.1.2 stage's client config) gets a one-time note in the manifest and at install.

Subcommands: generate, verify, precheck, record-services, install, materialize, rehearse (as in
stage_v013_final.py; rehearse runs everything in a PRIVATE HOME with a recording fake systemctl).
"""
import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import py_compile
import re
import subprocess
import sys
import tempfile
import time

TOOLS = Path(__file__).resolve().parent
sys.dont_write_bytecode = True
sys.path.insert(0, str(TOOLS))
import stage_rollback_guard as guard  # noqa: E402
import stage_v013_final as v013  # noqa: E402  (shared helpers: capture, unit/Studio rewrite, seal, install)

RELEASE = '0.2.0'
LABEL = 'v020-final'
StageError = v013.StageError
sha, dump_json, same_format, arg_after, json_diff = v013.sha, v013.dump_json, v013.same_format, v013.arg_after, v013.json_diff
RUNTIME_SRC = v013.RUNTIME_SRC
KEYS = ('unit', 'runtime', 'cli', 'studio_dropin', 'oxa', 'launcher')
CREATED = ('oxa',)      # absent before the stage
REMOVED = ('launcher',)  # absent after the stage
STAGED = {'unit': 'oxc-chat.service', 'runtime': 'runtime.json', 'cli': 'oxc',
          'studio_dropin': '10-imgship.conf', 'oxa': 'oxa', 'launcher': 'launch_local_plan.py.removed'}
PRECONDITIONS = ('studio_unit', 'chat_dropin', 'image_settings', 'image_engine')
BASE_KEYS = ('unit', 'runtime', 'cli', 'launcher', 'studio_dropin')  # what the v0.1.3 stage installed
CHAT, STUDIO, SERVICES = v013.CHAT, v013.STUDIO, v013.SERVICES
REL = {k: v013.REL[k] for k in ('unit', 'runtime', 'cli', 'launcher', 'studio_dropin', 'studio_unit',
                                 'chat_dropin', 'image_settings', 'image_engine')}
REL['oxa'] = '.local/bin/oxa'
OLD_CLIENT_CONFIG = '.config/opencode/opencode.json'  # written by the v0.1.2 stage; never touched here
RELEASES, ENGINES = v013.RELEASES, v013.ENGINES
MOE_MODELS, FLASH, MOE_CONTEXT, CACHE, WATCHDOG = v013.MOE_MODELS, v013.FLASH, v013.MOE_CONTEXT, v013.CACHE, v013.WATCHDOG
ORNITH, QWEN = 'ornith-35b-a3b-local', 'qwen3.6-35b-a3b-local'
PF_ATTN = {ORNITH: 'pfg', QWEN: 'tiled'}  # O-141: PFG for Ornith only; never `auto` (fa2 above 131K)
STUDIO_FILES = v013.STUDIO_FILES
EXECUTABLE = v013.EXECUTABLE
SEAL, SERVICE_STATE = v013.SEAL, v013.SERVICE_STATE
RUNTIME_REPIN = ('server_binary', 'server_sha256', 'validation', 'oxe_args')
OXA_LIMIT = 256 * 1024 ** 2
VERSION_RX = re.compile(r'\b(\d+\.\d+\.\d+(?:-[0-9A-Za-z.]+)?)\b')
OXA_ENDPOINT = 'http://127.0.0.1:8092'
MIGRATION_NOTE = ('ox-llm v0.2.0 no longer configures or launches a third-party client (O-151). %s was written by '
                  'the v0.1.2 install and is left in place untouched; any OpenAI-compatible client can keep using '
                  + OXA_ENDPOINT + '/v1, see oxc/docs/OTHER-CLIENTS.md. `oxc code` now opens oxa.')


def live_paths(home):
    return {k: Path(home) / rel for k, rel in REL.items()}


def with_pf_attn(args, value):
    """oxe_args with exactly one `--pf-attn <value>` (replacing the live one, else appended)."""
    args = [str(a) for a in args]
    hits = [i for i, a in enumerate(args) if a == '--pf-attn']
    if len(hits) > 1 or any(a.startswith('--pf-attn') and a != '--pf-attn' for a in args):
        raise StageError('oxe_args carry several or inline --pf-attn flags; refusing to guess')
    if hits:
        if hits[0] + 1 >= len(args):
            raise StageError('oxe_args end with a bare --pf-attn')
        return args[:hits[0] + 1] + [value] + args[hits[0] + 2:]
    return args + ['--pf-attn', value]


def check_runtime(runtime, live_runtime, server_sha, server_path):
    errors = []
    if runtime.get('cache') != CACHE:
        errors.append('runtime cache must be %s' % CACHE)
    if runtime.get('lan_share', 'off') != 'off' or 'lan_share_options' in runtime:
        errors.append('runtime: LAN share must stay off (no lan_share / lan_share_options)')
    profiles = runtime.get('profiles') or {}
    for alias in MOE_MODELS:
        profile = profiles.get(alias) or {}
        oxe = ((profile.get('memlat_overrides') or {}).get('oxe')) or {}
        args = [str(a) for a in oxe.get('oxe_args') or []]
        env = oxe.get('environment') or {}
        if oxe.get('server_sha256') != server_sha or oxe.get('server_binary') != server_path:
            errors.append(alias + ': oxe server is not the staged candidate')
        if oxe.get('context_size') != MOE_CONTEXT or arg_after(args, '--ctx-size') != '{context}':
            errors.append(alias + ': oxe context must be %d' % MOE_CONTEXT)
        if oxe.get('threads') != 8 or arg_after(args, '--oxe-cpus') != '0-7':
            errors.append(alias + ': decode must be 8 threads on cpus 0-7')
        if arg_after(args, '--oxe-prefill-cpus') != '0-11':
            errors.append(alias + ': prefill must be 12 threads on cpus 0-11')
        want = PF_ATTN[alias]
        if args.count('--pf-attn') != 1 or arg_after(args, '--pf-attn') != want or \
                any(a.startswith('--pf-attn') and a != '--pf-attn' for a in args) or \
                any(k.startswith('OXE_PF_ATTN') for k in env):
            errors.append('%s: prefill attention must be pinned %s (exactly one --pf-attn %s; no '
                          '--pf-attn-auto-*, no OXE_PF_ATTN* env; never auto/fa2/fa2x)' % (alias, want, want))
        live_oxe = (((live_runtime.get('profiles') or {}).get(alias) or {}).get('memlat_overrides') or {}).get('oxe') or {}
        live_args = [str(a) for a in live_oxe.get('oxe_args') or []]
        try:
            expected_args = with_pf_attn(live_args, want)
        except StageError as exc:
            errors.append('%s: %s' % (alias, exc))
            expected_args = None
        if expected_args is not None and args != expected_args:
            errors.append(alias + ': oxe_args may differ from live only by the --pf-attn value')
        if env.get('OXE_ATTN_TILED_MIN_KV') != '0':
            errors.append(alias + ': OXE_ATTN_TILED_MIN_KV must stay "0" (tiled decode attention)')
        if any(k == 'OXE_ATTN' or k.startswith('OXE_ATTN_FUSED') for k in env):
            errors.append(alias + ': fused attention must stay off (no OXE_ATTN / OXE_ATTN_FUSED_*)')
        if 'rss_proof' in json.dumps(profile):
            errors.append(alias + ': rss_proof present (max mode must stay unavailable; context cap 131072)')
    if profiles.get(FLASH) != (live_runtime.get('profiles') or {}).get(FLASH):
        errors.append('flash: profile differs from the live one (it must be carried byte-identical)')
    allowed = {('profiles', a, 'memlat_overrides', 'oxe', k) for a in MOE_MODELS for k in RUNTIME_REPIN}
    extra = [p for p in json_diff(live_runtime, runtime) if p[:5] not in allowed]
    if extra:
        errors.append('runtime: settings changed beyond the engine re-pin: ' + ', '.join('.'.join(p) for p in extra))
    return errors


KEEP_WARM_LINE = 'Environment="LOCAL_LLM_KEEP_WARM_IDLE_SECONDS=7200"'  # O-160 (keep_warm.py, L3)
LEGACY_IDLE = 'LOCAL_LLM_IDLE_SECONDS'  # pinned 600 by the v0.1.x stage tool; ignored by the v0.2.0 controller


def set_keep_warm(text):
    """The unit with the legacy idle line replaced by the keep-warm line (inserted after the last
    [Service] Environment= line when there is no legacy line). Nothing else changes."""
    lines = text.splitlines()
    legacy = [i for i, l in enumerate(lines) if l.startswith('Environment') and LEGACY_IDLE + '=' in l]
    if any('LOCAL_LLM_KEEP_WARM_IDLE_SECONDS' in l for l in lines):
        raise StageError('unit: a LOCAL_LLM_KEEP_WARM_IDLE_SECONDS line is already there; restage by hand')
    if len(legacy) > 1:
        raise StageError('unit: several LOCAL_LLM_IDLE_SECONDS lines')
    if legacy:
        lines[legacy[0]] = KEEP_WARM_LINE
    else:
        section, last = '', None
        for i, line in enumerate(lines):
            if line.startswith('['):
                section = line
            elif section == '[Service]' and line.startswith('Environment'):
                last = i
        if last is None:
            raise StageError('unit: no [Service] Environment= line to place the keep-warm setting after')
        lines.insert(last + 1, KEEP_WARM_LINE)
    return '\n'.join(lines) + '\n'


def check_unit(text, live_text, release_dir):
    errors = []
    lines = text.splitlines()
    if lines.count(WATCHDOG) != 1:
        errors.append('unit: exactly one ' + WATCHDOG)
    if 'Restart=on-failure' not in lines:
        errors.append('unit: Restart=on-failure missing')
    for name in ('LOCAL_LLM_CONFIG', 'OXC_LEASE'):
        if not any(l.startswith(('Environment="%s=' % name, 'Environment=%s=' % name)) for l in lines):
            errors.append('unit: %s must be set explicitly' % name)
    if 'WorkingDirectory=' + release_dir not in lines or \
            'ExecStart=/usr/bin/python3 "%s/controller.py"' % release_dir not in lines:
        errors.append('unit: not pointed at the staged release')
    if lines.count(KEEP_WARM_LINE) != 1 or any(LEGACY_IDLE + '=' in l for l in lines):
        errors.append('unit: exactly one keep-warm line (7200 s) and no legacy LOCAL_LLM_IDLE_SECONDS (O-160)')
    live = [l for l in live_text.splitlines() if LEGACY_IDLE + '=' not in l]
    new = [l for l in lines if l != KEEP_WARM_LINE]
    if len(live) != len(new) or [i for i, (a, b) in enumerate(zip(live, new))
                                  if a != b and not a.startswith(('WorkingDirectory=', 'ExecStart='))]:
        errors.append('unit: lines other than WorkingDirectory/ExecStart and the keep-warm setting changed')
    return errors


def load_oxa(path, want_version, source, require_elf=True):
    """The oxa binary: a bounded regular ELF file whose `--version` reports want_version (require_elf=False:
    model-free tests stage a script stand-in)."""
    path = Path(path)
    if not path.is_absolute() or path.is_symlink() or not path.is_file():
        raise StageError('--oxa-binary must be an absolute path to a regular file (no symlink)')
    data = path.read_bytes()
    if len(data) > OXA_LIMIT or (require_elf and not data.startswith(b'\x7fELF')):
        raise StageError('--oxa-binary is not a bounded ELF executable')
    with tempfile.TemporaryDirectory(prefix='v020-oxa-') as tmp:  # run the exact bytes we stage
        probe = Path(tmp) / 'oxa'
        probe.write_bytes(data)
        probe.chmod(0o700)
        try:
            done = subprocess.run([str(probe), '--version'], capture_output=True, text=True, timeout=10,
                                  stdin=subprocess.DEVNULL, check=False,
                                  env={'PATH': '/usr/bin:/bin', 'HOME': tmp, 'XDG_CONFIG_HOME': tmp})
        except (OSError, subprocess.SubprocessError) as exc:
            raise StageError('oxa --version did not run: %s' % type(exc).__name__) from None
    match = VERSION_RX.search(done.stdout or '')
    version = match.group(1) if match and done.returncode == 0 else None
    if version != want_version:
        raise StageError('oxa --version reports %r, not the release version %s' % (version, want_version))
    return data, {'sha256': sha(data), 'version': version, 'source': source, 'bytes': len(data),
                  'origin': 'built oxa binary (--oxa-binary); the release workflow builds it from oxa/ once imported'}


def migration_notes(home):
    path = Path(home) / OLD_CLIENT_CONFIG
    try:
        text = path.read_text()
    except (OSError, UnicodeError):
        return []
    return [MIGRATION_NOTE % path] if 'local-qwen' in text else []


def generate(*, base, out, lab_repo, lab_rev, engine_rev, engine_artifacts, oxa_binary, oxa_source=None,
             oxa_version=RELEASE, home=None, systemctl='systemctl', forbidden_roots=None, system_roots=None,
             expect=None, label=LABEL, oxa_require_elf=True):
    import stage_controller_release as controller_release
    home = Path(home) if home else Path.home()
    base, out = Path(base), Path(out)
    if not out.is_absolute() or out.exists() or out.is_symlink():
        raise StageError('--out must be a fresh absolute path')
    for live in forbidden_roots if forbidden_roots is not None else (home / '.config', home / '.local'):
        if out == live or out.is_relative_to(live):
            raise StageError('stage must not be under live configuration: %s' % live)
    if out.is_relative_to(base.resolve()) or out.is_relative_to(base):
        raise StageError('stage must not be inside the base stage')
    if not re.fullmatch(r'[0-9a-f]{40}', engine_rev or ''):
        raise StageError('--engine-rev must be a full 40-hex commit id')
    manifest_raw = (base / 'manifest.json').read_bytes()
    bm = json.loads(manifest_raw)

    # 1. live state, read once
    paths = live_paths(home)
    live = {k: v013.capture_live(p) for k, p in paths.items() if k not in CREATED}
    errors = []
    for key in CREATED:
        if os.path.lexists(paths[key]):
            errors.append('%s: %s already exists; this stage creates it (remove it or restage)' % (key, paths[key]))
        if not paths[key].parent.is_dir():
            errors.append('%s: directory %s is missing' % (key, paths[key].parent))
    for key in BASE_KEYS:
        if live[key]['sha256'] != (bm.get('installed_sha256') or {}).get(key):
            errors.append('%s: live sha256 %s is not what %s installed; restage from the actual live state'
                          % (key, live[key]['sha256'][:12], base.name))
        elif '%04o' % live[key]['mode'] != ((bm.get('preimage_mode') or {}).get(key) or '%04o' % live[key]['mode']):
            errors.append('%s: live mode %04o differs from the v0.1.3 install mode' % (key, live[key]['mode']))
    for key, want in (expect or {}).items():
        if key not in live or live[key]['sha256'] != want:
            errors.append('%s: live sha256 is not the expected %s' % (key, want[:12]))
    scan = v013.scan_dropins(home, system_roots)
    errors += v013.check_dropin_scan(scan, home)
    errors += v013.check_chat_dropin(live['chat_dropin']['bytes'].decode())
    if errors:
        raise StageError('; '.join(errors))

    # 2. sources at --lab-rev (git objects only), the frozen engine, the oxa binary
    with tempfile.TemporaryDirectory(prefix='v020-final-src-') as tmp:
        lab_commit, source_dir = v013.export_tree(lab_repo, lab_rev, RUNTIME_SRC, tmp)
        sources = controller_release.release_sources(source_dir)
    missing = [n for n in STUDIO_FILES + ('uninstall.py', 'health_digest.py', 'release_update.py') if n not in sources]
    if missing:
        errors.append('release lacks: ' + ', '.join(missing))
    engine, engine_prov = v013.load_engine(engine_artifacts, engine_rev)
    oxa, oxa_prov = load_oxa(oxa_binary, oxa_version, oxa_source, oxa_require_elf)
    server_sha, oxe_sha = sha(engine['oxe-server']), sha(engine['oxe'])
    engine_dir = str(home / ENGINES / ('oxe-%s-%s' % (label, server_sha[:12])))
    source_sha = {name: sha(data) for name, data in sources.items()}
    release_id = sha(json.dumps(source_sha, sort_keys=True).encode())[:12]
    release_dir = str(home / RELEASES / ('%s-%s' % (label, release_id)))

    # 3. payloads
    live_runtime = same_format(live['runtime']['bytes'])

    def repin(obj, server_path):
        obj = json.loads(json.dumps(obj))
        for alias in MOE_MODELS:
            oxe_cfg = obj['profiles'][alias]['memlat_overrides']['oxe']
            oxe_cfg['oxe_args'] = with_pf_attn(oxe_cfg['oxe_args'], PF_ATTN[alias])
            oxe_cfg['server_binary'] = server_path
            oxe_cfg['server_sha256'] = server_sha
            oxe_cfg['validation'] = {'kind': 'v020-final-staged-unqualified', 'engine_source': engine_rev,
                                     'server_sha256': server_sha, 'qualified': False,
                                     'prefill_attention': PF_ATTN[alias], 'predecessor': oxe_cfg.get('validation')}
        return obj
    try:
        runtime = repin(live_runtime, engine_dir + '/oxe-server')
        gate = repin(live_runtime, str(out / 'engine' / 'oxe-server'))
    except (KeyError, TypeError) as exc:
        raise StageError('runtime: MoE oxe profile missing (%s)' % exc) from None
    errors += check_runtime(runtime, live_runtime, server_sha, engine_dir + '/oxe-server')
    errors += check_runtime(gate, live_runtime, server_sha, str(out / 'engine' / 'oxe-server'))
    unit_live = live['unit']['bytes'].decode()
    unit = set_keep_warm(controller_release.base.rewrite_unit(unit_live, release_dir))
    errors += check_unit(unit, unit_live, release_dir)
    studio_unit = live['studio_unit']['bytes'].decode()
    dropin_live = live['studio_dropin']['bytes'].decode()
    dropin = v013.rewrite_studio_dropin(dropin_live, release_dir)
    studio_errors, studio_old, studio_new = v013.check_studio(dropin, dropin_live, studio_unit, release_dir)
    errors += studio_errors
    chat_eff = v013.effective_service(unit, [live['chat_dropin']['bytes'].decode()])
    if chat_eff['ExecStart'] != '/usr/bin/python3 "%s/controller.py"' % release_dir or chat_eff['Restart'] != 'on-failure':
        errors.append('chat: effective ExecStart/Restart after the drop-in merge is not the staged release')
    if errors:
        raise StageError('; '.join(errors))
    payload = {'unit': unit.encode(), 'runtime': dump_json(runtime), 'cli': controller_release.cli_source(sources),
               'studio_dropin': dropin.encode(), 'oxa': oxa, 'launcher': None}
    preimages = {k: (None if k in CREATED else live[k]['bytes']) for k in KEYS}
    modes = {k: live[k]['mode'] for k in KEYS if k not in CREATED}
    for key in KEYS:
        if payload[key] == preimages[key]:
            raise StageError('%s: payload equals the live preimage; nothing to install' % key)
    notes = migration_notes(home)

    # 4. write the stage (fresh dir, O_EXCL everywhere)
    old_umask = os.umask(0o077)
    try:
        out.mkdir(mode=0o700)

        def put(name, data, mode=0o600):
            target = out / name
            target.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            fd = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, mode)
            with os.fdopen(fd, 'wb') as stream:
                stream.write(data)
            os.chmod(target, mode)
        for key in KEYS:
            if payload[key] is not None:
                put(STAGED[key], payload[key], 0o700 if key in ('cli', 'oxa') else 0o600)
            if preimages[key] is not None:
                put('rollback/%s.original' % key, preimages[key])
        put('runtime.gate.json', dump_json(gate))
        put('engine/oxe-server', engine['oxe-server'], 0o700)
        put('engine/oxe', engine['oxe'], 0o700)
        for name, data in sources.items():
            put('runtime-source/' + name, data, 0o700 if name in EXECUTABLE else 0o600)
        for name in ('install_staged_release.py', 'stage_rollback_guard.py', 'stage_v013_final.py',
                     'stage_v020_final.py'):
            put('tools/' + name, (TOOLS / name).read_bytes(), 0o600 if name == 'stage_rollback_guard.py' else 0o700)
        doc = ('One-step v0.1.3 rollback (v0.2.0 ox-llm): restore the exact v0.1.3 unit, runtime, cli, Studio '
               'drop-in and launcher (bytes and modes), remove oxa, then restart the controller and Studio.')
        message = ('Restored v0.1.3: unit, runtime, cli, Studio drop-in and launcher (modes kept); oxa removed; '
                   'controller and Studio restarted; no model loaded. The v0.2.0 release and engine dirs were left in place.')
        put('rollback/rollback.py', guard.render_rollback(
            {k: paths[k] for k in KEYS}, payload, preimages, staged_files=STAGED, doc=doc, message=message,
            systemctl=systemctl, services=SERVICES, modes=modes, service_state=SERVICE_STATE), 0o700)
        try:
            gen_commit = v013.git(TOOLS.parent, 'rev-parse', 'HEAD').decode().strip()
            gen_dirty = bool(v013.git(TOOLS.parent, 'status', '--porcelain', '--', 'tools').strip())
        except (subprocess.CalledProcessError, OSError):
            gen_commit, gen_dirty = None, None
        tools = '%s/tools/' % out
        manifest = {
            **guard.manifest_binding({k: paths[k] for k in KEYS}, payload, preimages, STAGED, modes=modes),
            'installed_mode': {k: '0700' for k in CREATED},
            'installed': False, 'qualified': False, 'models_started': False,
            'release': 'v%s (ox-llm)' % RELEASE, 'version': RELEASE, 'product': 'ox-llm',
            'stage': str(out), 'home': str(home), 'created': time.strftime('%Y-%m-%dT%H:%M:%S%z'),
            'preimage_basis': 'CURRENT live files (== %s installed_sha256), captured at generation' % base.name,
            'base_stage': str(base), 'base_stage_resolved': str(base.resolve()), 'base_manifest_sha256': sha(manifest_raw),
            'preconditions': {k: {'path': str(paths[k]), 'sha256': live[k]['sha256'], 'mode': '%04o' % live[k]['mode']}
                              for k in PRECONDITIONS},
            'dropin_scan': scan,
            'lab_source': lab_commit, 'runtime_source_sha256': source_sha, 'release_dir': release_dir,
            'release_id': release_id,
            'engine_source': engine_rev, 'engine_freeze': engine_prov, 'engine_dir': engine_dir,
            'server_sha256': server_sha, 'oxe_sha256': oxe_sha,
            'engine_artifacts': {'oxe-server': {'sha256': server_sha, 'source': engine_rev, 'origin': 'frozen candidate'},
                                 'oxe': {'sha256': oxe_sha, 'source': engine_rev, 'origin': 'frozen candidate'}},
            'oxa': {**oxa_prov, 'path': str(paths['oxa']), 'endpoint': OXA_ENDPOINT,
                    'launched_by': 'oxc code / plan (OXA_ENGINE_URL=%s)' % OXA_ENDPOINT},
            'replaced_engine': {'server_sha256': bm.get('server_sha256'), 'engine_dir': bm.get('engine_dir'),
                                'source': bm.get('engine_source')},
            'engine_pins': {'profiles': list(MOE_MODELS), 'decode': '8 threads, --oxe-cpus 0-7',
                            'prefill': '12 threads, --oxe-prefill-cpus 0-11', 'context_cap': MOE_CONTEXT,
                            'prefill_attention': dict(PF_ATTN), 'prefill_attention_note':
                                'O-141: pfg for Ornith only; Qwen stays tiled; never auto (fa2 above 131072)',
                            'flash': 'profile byte-identical to live',
                            'runtime_diff_vs_live': ['profiles.%s.memlat_overrides.oxe.%s' % (a, k)
                                                     for a in MOE_MODELS for k in RUNTIME_REPIN]},
            'keep_warm': {'unit_env': KEEP_WARM_LINE, 'replaced': LEGACY_IDLE,
                          'module_in_release': 'keep_warm.py' in sources,
                          'note': 'O-160; the controller ignores LOCAL_LLM_IDLE_SECONDS from v0.2.0'},
            'clients': {'removed': ['launcher (~/.local/bin/launch_local_plan.py)'],
                        'client_configs': 'never read as preconditions, never changed (O-151)'},
            'migration_notes': notes,
            'studio': {'release_dir': release_dir, 'previous_exec': studio_old['ExecStart'],
                       'exec': studio_new['ExecStart'], 'restart': studio_new['Restart'],
                       'modules': [n for n in STUDIO_FILES if n in sources]},
            'payload_vs_live': {k: ('created' if k in CREATED else 'removed' if k in REMOVED else 'changed') for k in KEYS},
            'rollback': {'script': 'rollback/rollback.py', 'one_step_to': 'v0.1.3 (%s)' % base.resolve().name,
                         'restores': list(KEYS), 'removes': list(CREATED), 'puts_back': list(REMOVED),
                         'modes': {k: '%04o' % m for k, m in modes.items()},
                         'services': SERVICES, 'systemctl': str(systemctl),
                         'service_state': {'file': SERVICE_STATE},
                         'leaves_in_place': [release_dir, engine_dir]},
            'commands': [
                'python3 -I %sstage_v020_final.py precheck %s' % (tools, out),
                'python3 -I %sstage_v020_final.py materialize %s' % (tools, out),
                'python3 -I %sstage_v020_final.py install %s            (precheck + dry run)' % (tools, out),
                'oxc update %s   (or install --apply + restart, root only)' % out,
                'rollback: oxc rollback, or %s/rollback/rollback.py --check, then without --check' % out],
            'generator': {'path': 'tools/stage_v020_final.py', 'commit': gen_commit, 'tools_dirty': gen_dirty,
                          'sha256': sha((TOOLS / 'stage_v020_final.py').read_bytes())},
        }
        put('manifest.json', dump_json(manifest))
        put(SEAL, dump_json({'note': 'sha256 of every stage file at generation; receipts excluded',
                             'files': v013.seal(out)}), 0o400)
        for path in out.rglob('*'):
            if path.is_file() and not path.is_symlink():
                os.chmod(path, 0o500 if path.stat().st_mode & 0o100 else 0o400)
    finally:
        os.umask(old_umask)
    return manifest


def refresh_identity(agent, home, timeout=30):
    """O-152, best effort, never raises: `oxa identity --refresh --offline` right after the files are in
    place (the controller is restarted afterwards; `oxc update` refreshes online once it answers)."""
    agent = Path(agent)
    if not agent.is_file():
        return 'absent'
    try:
        done = subprocess.run([str(agent), 'identity', '--refresh', '--offline'], capture_output=True, text=True,
                              timeout=timeout, stdin=subprocess.DEVNULL, check=False,
                              env={**os.environ, 'HOME': str(home), 'OXA_ENGINE_URL': OXA_ENDPOINT})
    except (OSError, subprocess.SubprocessError):
        return 'failed'
    return 'refreshed-offline' if done.returncode == 0 else 'failed'


def install(stage, apply, system_roots=None, out=None):
    out = out or sys.stdout
    m = json.loads((Path(stage) / 'manifest.json').read_text())
    for note in m.get('migration_notes') or []:
        print('note: ' + note, file=out)
    code = v013.install(stage, apply, system_roots=system_roots, out=out)
    if apply and code == 0:
        print('oxa identity: %s' % refresh_identity(m['paths']['oxa'], m['home']), file=out)
    return code


verify, precheck, materialize, record_services = v013.verify, v013.precheck, v013.materialize, v013.record_services


def rehearse(*, base, workdir, lab_repo, lab_rev, engine_rev, engine_artifacts, oxa_binary, oxa_version=RELEASE,
             source_home=None, system_roots=None, out=None, oxa_require_elf=True):
    """Mirror the live files into a private HOME and run generate/precheck/materialize/install/rollback there."""
    out = out or sys.stdout
    workdir = Path(workdir)
    source_home = Path(source_home) if source_home else Path.home()
    if not workdir.is_absolute() or workdir.exists():
        raise StageError('--workdir must be a fresh absolute path')
    if workdir.is_relative_to(source_home / '.config') or workdir.is_relative_to(source_home / '.local'):
        raise StageError('--workdir must not be under live configuration')
    workdir.mkdir(mode=0o700)
    home = workdir / 'home'
    home.mkdir(mode=0o700)
    calls = workdir / 'systemctl.log'
    fake = workdir / 'systemctl'
    fake.write_text(v013.FAKE_SYSTEMCTL % json.dumps(str(calls)))
    fake.chmod(0o700)
    src_paths, dst_paths = live_paths(source_home), live_paths(home)
    for key, src in src_paths.items():
        if key in CREATED:
            dst_paths[key].parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            continue
        got = v013.capture_live(src)
        dst = dst_paths[key]
        dst.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        dst.write_bytes(got['bytes'])
        os.chmod(dst, got['mode'])

    def state():
        return {k: (p.read_bytes(), p.stat().st_mode & 0o7777) if p.exists() else None for k, p in dst_paths.items()}
    before = state()
    stage = workdir / 'stage'
    m = generate(base=base, out=stage, lab_repo=lab_repo, lab_rev=lab_rev, engine_rev=engine_rev,
                 engine_artifacts=engine_artifacts, oxa_binary=oxa_binary, oxa_version=oxa_version, home=home,
                 systemctl=str(fake), system_roots=system_roots, oxa_require_elf=oxa_require_elf)
    log = io.StringIO()
    precheck(stage, system_roots=system_roots, out=log)
    materialize(stage, out=log)
    install(stage, True, system_roots=system_roots, out=log)
    problems = []
    for key in KEYS:
        if key in REMOVED:
            if dst_paths[key].exists():
                problems.append('%s: not removed' % key)
        elif dst_paths[key].read_bytes() != (stage / STAGED[key]).read_bytes():
            problems.append('%s: not installed' % key)
    if dst_paths['oxa'].exists() and dst_paths['oxa'].stat().st_mode & 0o7777 != 0o700:
        problems.append('oxa: not installed 0700')
    for key in PRECONDITIONS:
        if state()[key] != before[key]:
            problems.append('%s: a precondition file was touched' % key)
    runtime = json.loads(dst_paths['runtime'].read_text())
    for alias in MOE_MODELS:
        args = runtime['profiles'][alias]['memlat_overrides']['oxe']['oxe_args']
        if arg_after(args, '--pf-attn') != PF_ATTN[alias]:
            problems.append('%s: --pf-attn is not %s' % (alias, PF_ATTN[alias]))
    for name, entry in (('chat', 'controller.py'), ('studio', 'studio')):
        target = Path(m['release_dir']) / entry
        if not target.is_file():
            problems.append('%s: release not materialized' % name)
        elif entry.endswith('.py'):
            with tempfile.TemporaryDirectory() as tmp:
                py_compile.compile(str(target), cfile=str(Path(tmp) / 'probe.pyc'), doraise=True)
    roll = subprocess.run([sys.executable, '-I', str(stage / 'rollback/rollback.py')], capture_output=True,
                          text=True, timeout=120)
    again = subprocess.run([sys.executable, '-I', str(stage / 'rollback/rollback.py')], capture_output=True,
                           text=True, timeout=120)
    if roll.returncode or again.returncode:
        problems.append('rollback failed: %s %s' % (roll.stderr.strip(), again.stderr.strip()))
    after = state()
    if after != before:
        problems.append('rollback did not restore every byte, mode and absence: '
                        + ', '.join(k for k in before if after[k] != before[k]))
    receipt = {'action': 'v020-rehearsal', 'time': time.strftime('%Y-%m-%dT%H:%M:%S%z'), 'workdir': str(workdir),
               'stage': str(stage), 'lab_source': m['lab_source'], 'engine_source': m['engine_source'],
               'server_sha256': m['server_sha256'], 'oxa': m['oxa'], 'release_id': m['release_id'],
               'systemctl_calls': calls.read_text().splitlines() if calls.exists() else [],
               'passed': not problems, 'problems': problems, 'real_service_manager_contacted': False}
    (workdir / 'REHEARSAL.json').write_text(json.dumps(receipt, indent=2) + '\n')
    print(log.getvalue(), end='', file=out)
    print('rehearsal %s: %s' % ('PASSED' if not problems else 'FAILED', workdir / 'REHEARSAL.json'), file=out)
    if problems:
        raise StageError('rehearsal failed: ' + '; '.join(problems))
    return receipt


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest='command', required=True)
    for name in ('generate', 'rehearse'):
        p = sub.add_parser(name)
        p.add_argument('--base', type=Path, required=True, help='the installed v0.1.3 stage')
        p.add_argument('--lab-repo', type=Path, required=True)
        p.add_argument('--lab-rev', required=True)
        p.add_argument('--engine-rev', required=True, help='full sha of the frozen engine build')
        p.add_argument('--engine-artifacts', type=Path, required=True, help='freeze dir (manifest.json + default/)')
        p.add_argument('--oxa-binary', type=Path, required=True, help='a built oxa binary (absolute path)')
        p.add_argument('--oxa-version', default=RELEASE, help='the version oxa --version must report (default %(default)s)')
        if name == 'generate':
            p.add_argument('--oxa-source', help='provenance of the oxa binary (commit / build id), recorded')
            p.add_argument('--out', type=Path, required=True)
            p.add_argument('--expect', action='append', default=[], metavar='KEY=SHA256')
            p.add_argument('--systemctl', default='systemctl', help='baked into rollback.py (rehearsal uses a fake)')
        else:
            p.add_argument('--workdir', type=Path, required=True)
    for name in ('verify', 'precheck', 'materialize', 'record-services', 'install'):
        p = sub.add_parser(name)
        p.add_argument('stage', type=Path)
        if name == 'install':
            p.add_argument('--apply', action='store_true', help='really install (default: precheck + dry run)')
    args = ap.parse_args(argv)
    try:
        if args.command == 'verify':
            print('seal OK: %d files' % verify(args.stage))
        elif args.command == 'precheck':
            precheck(args.stage)
        elif args.command == 'record-services':
            record_services(args.stage)
        elif args.command == 'install':
            install(args.stage, args.apply)
        elif args.command == 'materialize':
            print(json.dumps(materialize(args.stage)))
        elif args.command == 'rehearse':
            rehearse(base=args.base, workdir=args.workdir, lab_repo=args.lab_repo, lab_rev=args.lab_rev,
                     engine_rev=args.engine_rev, engine_artifacts=args.engine_artifacts,
                     oxa_binary=args.oxa_binary, oxa_version=args.oxa_version)
        else:
            m = generate(base=args.base, out=args.out, lab_repo=args.lab_repo, lab_rev=args.lab_rev,
                         engine_rev=args.engine_rev, engine_artifacts=args.engine_artifacts,
                         oxa_binary=args.oxa_binary, oxa_source=args.oxa_source, oxa_version=args.oxa_version,
                         systemctl=args.systemctl, expect=dict(item.split('=', 1) for item in args.expect))
            print(json.dumps({k: m[k] for k in ('stage', 'version', 'lab_source', 'engine_source', 'server_sha256',
                                                'release_dir', 'engine_dir', 'preimage_sha256', 'installed_sha256',
                                                'migration_notes')}, indent=2))
            print(json.dumps({'oxa': m['oxa']}, indent=2))
    except (StageError, guard.GuardError, ValueError, KeyError, OSError, subprocess.CalledProcessError) as exc:
        print('REFUSED: %s' % exc, file=sys.stderr)
        return 2
    return 0


if __name__ == '__main__':
    sys.exit(main())
