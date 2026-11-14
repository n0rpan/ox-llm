#!/usr/bin/env python3
"""v0.1.3 install stage (O-119): generate from the CURRENT live state, never install.

The stage is computed against the files that are live NOW (the v0.1.2 production
install: staged/latest-v012-production's installed_sha256), not against v0.1.2's own
predecessor preimages. Five guarded payloads, bound by stage_rollback_guard and
installed by tools/install_staged_release.py (both are copied into <stage>/tools):

  unit           local-ai-chat.service  WorkingDirectory/ExecStart -> the v0.1.3 release; every
                                        other line byte-identical (watchdog env, image NPU, limits)
  runtime        runtime.json           both MoE oxe profiles re-pinned to the frozen candidate
                                        oxe-server (server_binary/server_sha256/validation only);
                                        every setting unchanged: 8 decode (0-7) / 12 prefill (0-11)
                                        threads, ctx 131072, prefill attention pinned
                                        `--pf-attn tiled` (the one oxe_args change), no fused attention, no RSS proof (max mode stays
                                        unavailable), LAN share off; Flash profile byte-identical
  cli            ~/.local/bin/local-ai              from --lab-rev
  launcher       ~/.local/bin/launch_local_plan.py  from --lab-rev (fast default, deep explicit,
                                                    MoE overlay header/chunk timeouts 3600 s)
  studio_dropin  prompt-studio-app.service.d/10-imgship.conf
                                        WorkingDirectory/ExecStart -> the same v0.1.3 release (the
                                        split studio_* modules); the `ExecStart=` reset, OXI_BIN,
                                        SD_CLI_PROBE and CPUQuota lines byte-identical

Unchanged but checked (`preconditions`, re-verified by --precheck before install):
opencode.json, prompt-studio-app.service (Restart=on-failure, the drop-in's ExecStart wins),
local-ai-chat.service.d/model-profiles.conf, images/runtime-settings.json and engine.json.
Every drop-in directory systemd reads for the two units is scanned; an unknown drop-in, or
a known one that sets ExecStart/WorkingDirectory/Restart where it must not, refuses.

rollback/rollback.py is the one-step v0.1.2 rollback: it restores the five files with their
captured bytes AND modes, stops local-ai-chat + prompt-studio-app, daemon-reloads, and starts only
the services that were active before the install (service-state.json, written by record-services
or `install --apply`; without it, those active at rollback time).

Subcommands (each prints what it did; none starts or stops a real service):
  generate     write a fresh stage (the generator never calls systemctl, writes only --out)
  verify       recheck a written stage's seal
  precheck     seal + preconditions + drop-in scan + installer dry run, against the live files
  record-services  write service-state.json once (systemctl --user is-active; read-only query)
  install      precheck + installer dry run; --apply also records the service state first
  materialize  copy runtime-source/ to release_dir and engine/ to engine_dir (new
               content-addressed dirs; identical existing dirs are a no-op, others refuse)
  rehearse     full generate + precheck + materialize + install + rollback in a PRIVATE HOME
               (mirrored copies of the live files, a recording fake systemctl, private release
               roots); nothing outside --workdir is written, the real user manager is never called
"""
import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import py_compile
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import time

TOOLS = Path(__file__).resolve().parent
sys.dont_write_bytecode = True  # the copy in <stage>/tools must not add files to a sealed stage
sys.path.insert(0, str(TOOLS))
import stage_rollback_guard as guard  # noqa: E402
# stage_controller_release (and through it the lab sources) is imported inside generate():
# the copy bundled in <stage>/tools needs only the guard and the installer.

RUNTIME_SRC = 'src/snapdragon_ai_lab/runtime'
# The v0.1.3 controller has no v0.2.0 modules: its sources (--lab-rev) are checked against the
# required set minus the modules v0.2.0 added (the v0.2.0 stage uses the full set).
V020_MODULES = ('openai_conformance.py', 'openai_responses.py', 'keep_warm.py')
# v0.1.4 D6: the OpenCode launcher lives at the edge (legacy adapter); still installed as
# ~/.local/bin/launch_local_plan.py and shipped in runtime-source/ beside the runtime.
LAUNCHER_SRC = 'src/snapdragon_ai_lab/adapters/opencode'
LAUNCHER = 'launch_local_plan.py'
KEYS = ('unit', 'runtime', 'cli', 'launcher', 'studio_dropin')
STAGED = {'unit': 'local-ai-chat.service', 'runtime': 'runtime.json', 'cli': 'local-ai',
          'launcher': 'launch_local_plan.py', 'studio_dropin': '10-imgship.conf'}
PRECONDITIONS = ('opencode', 'studio_unit', 'chat_dropin', 'image_settings', 'image_engine')
BASE_KEYS = ('unit', 'runtime', 'opencode', 'cli', 'launcher')  # what the v0.1.2 stage installed
CHAT, STUDIO = 'local-ai-chat.service', 'prompt-studio-app.service'
SERVICES = [CHAT, STUDIO]
REL = {  # live path of every key, relative to HOME
    'unit': '.config/systemd/user/' + CHAT,
    'runtime': '.config/local-ai/llm/runtime.json',
    'cli': '.local/bin/local-ai',
    'launcher': '.local/bin/launch_local_plan.py',
    'studio_dropin': '.config/systemd/user/' + STUDIO + '.d/10-imgship.conf',
    'opencode': '.config/opencode/opencode.json',
    'studio_unit': '.config/systemd/user/' + STUDIO,
    'chat_dropin': '.config/systemd/user/' + CHAT + '.d/model-profiles.conf',
    'image_settings': '.config/local-ai/images/runtime-settings.json',
    'image_engine': '.config/local-ai/images/engine.json',
}
EXPECTED_DROPINS = {CHAT: ['model-profiles.conf'], STUDIO: ['10-imgship.conf']}
RELEASES = '.local/share/ai-stack/runtime-releases'
ENGINES = '.local/share/ai-stack/releases'
MOE_MODELS = ('ornith-35b-a3b-local', 'qwen3.6-35b-a3b-local')
FLASH = 'qwen3.8-flash-next-local'
FAST_INPUT, DEEP_INPUT, MOE_CONTEXT, MOE_OUTPUT = 65536, 120000, 131072, 16384
TIMEOUT_MS = 3_600_000
CACHE = {'disk_budget_bytes': 64000000000, 'ttl_secs': 604800}
WATCHDOG = 'Environment=LOCAL_AI_IMAGE_WATCHDOG_KILL_CLIENT=1'
STUDIO_FILES = ('studio', 'studio_page.py', 'studio_pixels.py', 'studio_context.py', 'studio_worker.py',
                'studio_plan_generate.py', 'studio_plan_edit.py', 'studio_oxi.py', 'image_slot.py', 'image_npu.py',
                'image_npu_worker.py', 'image_size_gate.py')
EXECUTABLE = ('local-ai', 'studio')
SEAL = 'SEAL.json'
SERVICE_STATE = 'service-state.json'  # written at install time (record-services), read by rollback.py
SEAL_SKIP = ('receipts', 'retired.json', 'rollback/receipts', SERVICE_STATE, SEAL)
PF_PIN = ['--pf-attn', 'tiled']  # explicit: the engine default (auto) would pick fa2 above n_ctx 131072
LIVE_LIMIT = 8 * 1024 ** 2
# keys under which the runtime payload may differ from the live runtime: nothing else changes
RUNTIME_REPIN = ('server_binary', 'server_sha256', 'validation', 'oxe_args')
SYSTEM_UNIT_ROOTS = ('/etc/systemd/user', '/run/systemd/user', '/usr/local/lib/systemd/user', '/usr/lib/systemd/user',
                     '/usr/local/share/systemd/user', '/usr/share/systemd/user')


class StageError(Exception):
    pass


def sha(data):
    return hashlib.sha256(data).hexdigest()


def git(repo, *args):
    return subprocess.run(['git', '-C', str(repo), *args], check=True, capture_output=True).stdout


# Pinned archived revisions (the v0.1.x lab commits) keep the source layout they were built with.
# The monorepo directory of the lab before the oxc rename (no slash here: the rename table keeps it).
LEGACY_TOP = 'lab'
LEGACY_LAYOUT = {'src/oxc/runtime': 'src/snapdragon_ai_lab/runtime',
                 'src/oxc/adapters/opencode': 'src/snapdragon_ai_lab/adapters/opencode'}


def export_tree(repo, rev, prefix, dest):
    """Files under `prefix` at commit `rev` (git objects only; the worktree is ignored). The lookup runs from
    the repository top, so it does not depend on the name of the checkout directory. A revision that predates
    the oxc rename keeps the old layout (under lab/): use it when the new prefix is absent there."""
    commit = git(repo, 'rev-parse', '--verify', rev + '^{commit}').decode().strip()
    top = Path(git(repo, 'rev-parse', '--show-toplevel').decode().strip())
    sub = git(repo, 'rev-parse', '--show-prefix').decode().strip()  # e.g. 'oxc/' in the renamed checkout
    candidates = [sub + prefix]
    if prefix in LEGACY_LAYOUT:
        candidates.append(LEGACY_TOP + '/' + LEGACY_LAYOUT[prefix])
    chosen = candidates[0]
    for cand in candidates:
        if subprocess.run(['git', '-C', str(top), 'cat-file', '-e', commit + ':' + cand],
                          capture_output=True).returncode == 0:
            chosen = cand
            break
    data = git(top, 'archive', '--format=tar', commit, '--', chosen)
    with tarfile.open(fileobj=io.BytesIO(data)) as tar:
        tar.extractall(dest, filter='data')
    return commit, Path(dest) / chosen


def capture_live(path):
    """sha256, mode and bytes of one live file; a bounded regular file owned by us, never followed."""
    path = Path(path)
    if not path.is_absolute():
        raise StageError('live path must be absolute: %s' % path)
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    except OSError as exc:
        raise StageError('live file unreadable: %s (%s)' % (path, exc.strerror)) from None
    with os.fdopen(fd, 'rb') as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_size > LIVE_LIMIT:
            raise StageError('live file must be a bounded regular file we own: %s' % path)
        data = stream.read(LIVE_LIMIT + 1)
    mode = info.st_mode & 0o7777
    if mode & ~0o777 or mode & 0o600 != 0o600:
        raise StageError('%s: unexpected live mode %04o' % (path, mode))
    return {'sha256': sha(data), 'mode': mode, 'bytes': data}


def dump_json(obj):
    return (json.dumps(obj, indent=2, ensure_ascii=False) + '\n').encode()


def same_format(raw):
    """The live JSON is indent-2, non-ASCII kept, trailing newline: keep that byte for byte."""
    if dump_json(json.loads(raw)) != raw:
        raise StageError('live JSON is not in the canonical indent-2 layout; refusing to reformat it')
    return json.loads(raw)


def arg_after(args, flag):
    hits = [i for i, a in enumerate(args) if a == flag]
    if len(hits) != 1 or hits[0] + 1 >= len(args):
        return None
    return args[hits[0] + 1]


def walk_keys(obj, path=()):
    if isinstance(obj, dict):
        for key, value in obj.items():
            yield path + (key,)
            yield from walk_keys(value, path + (key,))
    elif isinstance(obj, list):
        for value in obj:
            yield from walk_keys(value, path)


def json_diff(a, b, path=()):
    """Paths (tuples) where two JSON values differ; lists compare as whole values."""
    if isinstance(a, dict) and isinstance(b, dict):
        out = []
        for key in sorted(set(a) | set(b)):
            if key not in a or key not in b:
                out.append(path + (key,))
            else:
                out.extend(json_diff(a[key], b[key], path + (key,)))
        return out
    return [] if a == b else [path]


def service_lines(text):
    """[(section, line)] of a unit file; comments and blank lines dropped."""
    out, section = [], ''
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith(('#', ';')):
            continue
        if line.startswith('['):
            section = line
            continue
        out.append((section, line))
    return out


def effective_service(main_text, dropin_texts):
    """ExecStart / WorkingDirectory / Restart / Environment names after systemd's drop-in merge.

    Drop-ins apply in file-name order; an empty `ExecStart=` resets the list; a drop-in that
    adds an ExecStart without resetting gives a Type=simple service two commands (refused)."""
    result = {'ExecStart': [], 'WorkingDirectory': None, 'Restart': None, 'Environment': {}}
    for text in [main_text] + list(dropin_texts):
        for section, line in service_lines(text):
            if section != '[Service]' or '=' not in line:
                continue
            key, value = line.split('=', 1)
            if key == 'ExecStart':
                result['ExecStart'] = [] if value == '' else result['ExecStart'] + [value]
            elif key in ('WorkingDirectory', 'Restart'):
                result[key] = value
            elif key == 'Environment':
                for item in re.findall(r'"([^"]*)"|(\S+)', value):
                    assignment = item[0] or item[1]
                    if '=' in assignment:
                        name, val = assignment.split('=', 1)
                        result['Environment'][name] = val
    if len(result['ExecStart']) != 1:
        raise StageError('service would have %d ExecStart commands after the drop-in merge' % len(result['ExecStart']))
    result['ExecStart'] = result['ExecStart'][0]
    return result


def dropin_roots(home, system_roots):
    uid = os.getuid()
    roots = [Path(home) / '.config/systemd/user.control', Path('/run/user/%d/systemd/user.control' % uid),
             Path('/run/user/%d/systemd/transient' % uid), Path(home) / '.config/systemd/user',
             Path('/run/user/%d/systemd/user' % uid), Path(home) / '.local/share/systemd/user']
    if system_roots is None:
        system_roots = SYSTEM_UNIT_ROOTS
    else:
        roots = [r for r in roots if r.is_relative_to(home)]  # tests / rehearsal: the private home only
    return roots + [Path(r) for r in system_roots]


def scan_dropins(home, system_roots=None):
    """Every drop-in and unit fragment systemd could read for the chat and Studio units.

    Returns {unit: [{path, sha256}]} for drop-ins (*.d/*.conf incl. the type-wide service.d) and
    a list of fragments with the unit's name outside ~/.config/systemd/user (shadowing)."""
    found = {CHAT: [], STUDIO: []}
    fragments = []
    for root in dropin_roots(home, system_roots):
        for unit in (CHAT, STUDIO):
            for dname in (unit + '.d', 'service.d'):
                d = root / dname
                if d.is_dir():
                    for conf in sorted(d.glob('*.conf')):
                        found[unit].append({'path': str(conf), 'sha256': sha(conf.read_bytes()) if conf.is_file() else None})
            frag = root / unit
            if (frag.exists() or frag.is_symlink()) and root != Path(home) / '.config/systemd/user':
                fragments.append(str(frag))
    return {'dropins': found, 'other_fragments': sorted(fragments)}


def check_dropin_scan(scan, home):
    errors = []
    user_dir = Path(home) / '.config/systemd/user'
    for unit, expected in EXPECTED_DROPINS.items():
        paths = [Path(item['path']) for item in scan['dropins'][unit]]
        want = [user_dir / (unit + '.d') / name for name in expected]
        extra = [str(p) for p in paths if p not in want]
        if extra:
            errors.append('%s: unexpected drop-in(s) %s (review for conflicts before staging)' % (unit, ', '.join(extra)))
        missing = [str(p) for p in want if p not in paths]
        if missing:
            errors.append('%s: expected drop-in missing: %s' % (unit, ', '.join(missing)))
    if scan['other_fragments']:
        errors.append('unit fragments outside ~/.config/systemd/user: ' + ', '.join(scan['other_fragments']))
    return errors


# ---------------------------------------------------------------------------------- checks
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
        if args.count('--pf-attn') != 1 or arg_after(args, '--pf-attn') != 'tiled' or \
                any(a.startswith('--pf-attn') and a != '--pf-attn' for a in args) or \
                any(k.startswith('OXE_PF_ATTN') for k in env):
            errors.append(alias + ': prefill attention must be pinned tiled (exactly one --pf-attn tiled; '
                          'no --pf-attn-auto-*, no OXE_PF_ATTN* env; no fa2/fa2x)')
        live_oxe = (((live_runtime.get('profiles') or {}).get(alias) or {}).get('memlat_overrides') or {}).get('oxe') or {}
        live_args = live_oxe.get('oxe_args') or []
        if oxe.get('oxe_args') not in (live_args, live_args + PF_PIN):
            errors.append(alias + ': oxe_args may differ from live only by the appended --pf-attn tiled pin')
        if env.get('OXE_ATTN_TILED_MIN_KV') != '0':
            errors.append(alias + ': OXE_ATTN_TILED_MIN_KV must stay "0" (tiled decode attention)')
        if any(k == 'OXE_ATTN' or k.startswith('OXE_ATTN_FUSED') for k in env):
            errors.append(alias + ': fused attention must stay off (no OXE_ATTN / OXE_ATTN_FUSED_*)')
        if 'rss_proof' in json.dumps(profile):
            errors.append(alias + ': rss_proof present (max mode must stay unavailable; context cap 131072)')
        if '(beta)' in str(profile.get('display_name', '')):
            errors.append(alias + ': display_name carries "(beta)"')
    flash = (((profiles.get(FLASH) or {}).get('memlat_overrides') or {}).get('oxe_flash')) or {}
    fargs = flash.get('oxe_args') or []
    if flash.get('cache_pref') != '18':
        errors.append('flash: cache_pref must be "18" (18 GiB)')
    if (flash.get('prefix_store') or {}).get('enabled') is not True:
        errors.append('flash: prefix store must be on')
    if arg_after(fargs, '--flash-psi-some') != '0' or arg_after(fargs, '--flash-psi-full') != '0':
        errors.append('flash: PSI throttling must stay off')
    if not flash.get('sidecar_path') or arg_after(fargs, '--flash-sidecar') != flash.get('sidecar_path'):
        errors.append('flash: sidecar must be bound')
    if profiles.get(FLASH) != (live_runtime.get('profiles') or {}).get(FLASH):
        errors.append('flash: profile differs from the live one (it must be carried byte-identical)')
    for keys in walk_keys(runtime):
        if 'oxi' in keys and keys[-1] == 'text_encoder':
            errors.append('runtime: oxi.text_encoder present (the native text encoder stays inert)')
    allowed = {('profiles', a, 'memlat_overrides', 'oxe', k) for a in MOE_MODELS for k in RUNTIME_REPIN}
    extra = [p for p in json_diff(live_runtime, runtime) if p[:5] not in allowed]
    if extra:
        errors.append('runtime: settings changed beyond the engine re-pin: ' + ', '.join('.'.join(p) for p in extra))
    return errors


def check_unit(text, live_text, release_dir):
    errors = []
    lines = text.splitlines()
    if lines.count(WATCHDOG) != 1 or sum('LOCAL_AI_IMAGE_WATCHDOG_KILL_CLIENT' in l for l in lines) != 1:
        errors.append('unit: exactly one ' + WATCHDOG)
    if 'Restart=on-failure' not in lines:
        errors.append('unit: Restart=on-failure missing')
    for name in ('LOCAL_LLM_CONFIG', 'LOCAL_AI_LEASE'):
        if not any(l.startswith(('Environment="%s=' % name, 'Environment=%s=' % name)) for l in lines):
            errors.append('unit: %s must be set explicitly' % name)
    for name in ('LOCAL_AI_TOOL_OUTPUT_CAP', 'LOCAL_LLM_BACKEND_READ_TIMEOUT'):
        if any(name in l for l in lines):
            errors.append('unit: %s must not be set (cap off; backend read timeout default 3600 s)' % name)
    if 'WorkingDirectory=' + release_dir not in lines or \
            'ExecStart=/usr/bin/python3 "%s/controller.py"' % release_dir not in lines:
        errors.append('unit: not pointed at the staged release')
    live = live_text.splitlines()
    if len(live) != len(lines) or [i for i, (a, b) in enumerate(zip(live, lines))
                                    if a != b and not a.startswith(('WorkingDirectory=', 'ExecStart='))]:
        errors.append('unit: lines other than WorkingDirectory/ExecStart changed')
    return errors


def rewrite_studio_dropin(text, release_dir):
    """Re-point WorkingDirectory and the real ExecStart; keep the `ExecStart=` reset and the rest."""
    lines, section, done, reset = [], '', [], False
    for line in text.splitlines():
        if line.startswith('['):
            section = line
        if section == '[Service]' and line == 'ExecStart=':
            if done.count('exec') or reset:
                raise StageError('studio drop-in: unexpected ExecStart reset layout')
            reset = True
        elif section == '[Service]' and line.startswith('ExecStart='):
            if not reset or 'exec' in done:
                raise StageError('studio drop-in: the command must follow exactly one `ExecStart=` reset')
            line = 'ExecStart=/usr/bin/python3 "%s/studio"' % release_dir
            done.append('exec')
        elif section == '[Service]' and line.startswith('WorkingDirectory='):
            if 'cwd' in done:
                raise StageError('studio drop-in: two WorkingDirectory lines')
            line = 'WorkingDirectory=' + release_dir
            done.append('cwd')
        lines.append(line)
    if sorted(done) != ['cwd', 'exec']:
        raise StageError('studio drop-in: expected one WorkingDirectory and one ExecStart after the reset')
    return '\n'.join(lines) + '\n'


def check_studio(dropin, live_dropin, studio_unit, release_dir):
    errors = []
    old = effective_service(studio_unit, [live_dropin])
    new = effective_service(studio_unit, [dropin])
    if new['ExecStart'] != '/usr/bin/python3 "%s/studio"' % release_dir or new['WorkingDirectory'] != release_dir:
        errors.append('studio: effective ExecStart/WorkingDirectory is not the staged release')
    if new['Restart'] != 'on-failure':
        errors.append('studio: Restart=on-failure must stay in effect')
    if not new['Environment'].get('OXI_BIN') or new['Environment'] != old['Environment']:
        errors.append('studio: environment (qualified OXI_BIN / SD_CLI_PROBE / config dir) must be unchanged')
    for name in ('LOCAL_AI_TOOL_OUTPUT_CAP', 'OXI_TEXT_ENCODER'):
        if name in new['Environment']:
            errors.append('studio: %s must not be set' % name)
    live, lines = live_dropin.splitlines(), dropin.splitlines()
    if len(live) != len(lines) or [i for i, (a, b) in enumerate(zip(live, lines))
                                    if a != b and not a.startswith(('WorkingDirectory=', 'ExecStart=/'))]:
        errors.append('studio drop-in: lines other than WorkingDirectory/ExecStart changed')
    for section, line in service_lines(dropin):
        if line.startswith('Restart'):
            errors.append('studio drop-in: must not override Restart')
    return errors, old, new


def check_chat_dropin(text):
    bad = [l for s, l in service_lines(text) if l.split('=', 1)[0] in ('ExecStart', 'WorkingDirectory', 'Restart')
           or 'LOCAL_AI_TOOL_OUTPUT_CAP' in l]
    return ['chat drop-in model-profiles.conf sets %s (conflicts with the staged unit)' % ', '.join(bad)] if bad else []


def check_opencode(opencode):
    errors = []
    if 'compaction' in opencode:
        errors.append('opencode: a global compaction block would leak keep to Flash/16K models')
    if 'mcp' in opencode:
        errors.append('opencode: an mcp block in the global config (tool-output cap must stay off)')
    models = (((opencode.get('providers') or {}).get('local-qwen') or {}).get('models')) or {}
    for name in MOE_MODELS:
        if (models.get(name) or {}).get('limit') != {'context': MOE_CONTEXT, 'input': FAST_INPUT, 'output': MOE_OUTPUT}:
            errors.append('opencode: %s limit must be context %d / input %d / output %d (fast default)'
                          % (name, MOE_CONTEXT, FAST_INPUT, MOE_OUTPUT))
    return errors


def launcher_facts(source_dir):
    """Constants and both MoE overlays, computed by the launcher module itself (isolated child)."""
    code = ('import json,sys; sys.path.insert(0, sys.argv[1]); import launch_local_plan as L; '
            'print(json.dumps({"default": L.DEFAULT_OPENCODE_MODE, "inputs": L.OPENCODE_MODE_INPUT, '
            '"contexts": L.OPENCODE_MODE_CONTEXT, "keep": L.MOE_KEEP_TOKENS, '
            '"fast": L.apply_moe_preset({}, "fast"), "deep": L.apply_moe_preset({}, "deep")}))')
    out = subprocess.run([sys.executable, '-I', '-B', '-c', code, str(source_dir)], check=True,
                         capture_output=True, text=True, env={'PATH': os.environ.get('PATH', '/usr/bin')})
    facts = json.loads(out.stdout)
    errors = []
    if facts['default'] != 'fast':
        errors.append('launcher: DEFAULT_OPENCODE_MODE must be fast (O-105)')
    if facts['inputs'].get('fast') != FAST_INPUT or facts['inputs'].get('deep') != DEEP_INPUT:
        errors.append('launcher: fast %d / deep %d inputs expected' % (FAST_INPUT, DEEP_INPUT))
    if facts['contexts'].get('fast') != MOE_CONTEXT or facts['contexts'].get('deep') != MOE_CONTEXT:
        errors.append('launcher: fast/deep context must be %d' % MOE_CONTEXT)
    for name in ('fast', 'deep'):
        overlay = facts[name]
        provider = overlay['providers']['local-qwen']
        limit = provider['models']['ornith-35b-a3b-local']['limit']
        if limit != {'context': MOE_CONTEXT, 'input': facts['inputs'][name], 'output': MOE_OUTPUT}:
            errors.append('launcher overlay %s: unexpected MoE limit' % name)
        if overlay.get('compaction', {}).get('keep') != {'tokens': 8000}:
            errors.append('launcher overlay %s: keep must be 8000 (overlay only)' % name)
        if not provider['models'][FLASH].get('disabled'):
            errors.append('launcher overlay %s does not isolate Flash' % name)
        settings = provider.get('settings') or {}
        if settings.get('headerTimeout') != TIMEOUT_MS or settings.get('chunkTimeout') != TIMEOUT_MS:
            errors.append('launcher overlay %s: header/chunk timeout must be %d ms (long append)' % (name, TIMEOUT_MS))
        if 'mcp' in overlay:
            errors.append('launcher overlay %s registers an MCP server (tool-output cap must stay off)' % name)
    return facts, errors


# ---------------------------------------------------------------------------------- seal
def seal(stage):
    stage = Path(stage)
    out = {}
    for path in sorted(stage.rglob('*')):
        rel = path.relative_to(stage).as_posix()
        if path.is_file() and '__pycache__' not in rel.split('/') and \
                not any(rel == s or rel.startswith(s + '/') for s in SEAL_SKIP):
            out[rel] = sha(path.read_bytes())
    return out


def verify(stage):
    stage = Path(stage)
    recorded = json.loads((stage / SEAL).read_text())
    actual = seal(stage)
    bad = sorted(set(recorded['files']) ^ set(actual)) + sorted(
        k for k in recorded['files'] if k in actual and recorded['files'][k] != actual[k])
    if bad:
        raise StageError('stage seal mismatch: ' + ', '.join(bad))
    return len(actual)


# ---------------------------------------------------------------------------------- generate
def live_paths(home):
    return {k: Path(home) / rel for k, rel in REL.items()}


def load_engine(artifacts_dir, engine_rev):
    """The frozen candidate binaries: the freeze manifest passed, its source is engine_rev, sha256s match."""
    artifacts_dir = Path(artifacts_dir)
    raw = (artifacts_dir / 'manifest.json').read_bytes()
    freeze = json.loads(raw)
    if freeze.get('passed') is not True or freeze.get('source') != engine_rev:
        raise StageError('engine freeze manifest: passed must be true and source must be --engine-rev')
    jobs = [j for j in freeze.get('jobs') or [] if j.get('kind') == 'default' and j.get('returncode') == 0]
    if len(jobs) != 1:
        raise StageError('engine freeze manifest: exactly one successful default build expected')
    out = {}
    for name in ('oxe-server', 'oxe'):
        want = jobs[0]['artifacts'][name]['sha256']
        data = (artifacts_dir / 'default' / name).read_bytes()
        if sha(data) != want:
            raise StageError('engine %s: sha256 %s is not the frozen %s' % (name, sha(data)[:12], want[:12]))
        out[name] = data
    return out, {'freeze_dir': str(artifacts_dir), 'freeze_manifest_sha256': sha(raw), 'source': engine_rev,
                 'build_command': jobs[0].get('command'), 'freeze_finished': freeze.get('finished')}


def generate(*, base, out, lab_repo, lab_rev, engine_rev, engine_artifacts, home=None, systemctl='systemctl',
             forbidden_roots=None, system_roots=None, expect=None, label='v013-final'):
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

    # 1. live state, read once: the five keys v0.1.2 installed must be live with exactly those bytes
    paths = live_paths(home)
    live = {k: capture_live(p) for k, p in paths.items()}
    errors = []
    for key in BASE_KEYS:
        if live[key]['sha256'] != bm['installed_sha256'][key]:
            errors.append('%s: live sha256 %s is not what %s installed (%s); restage from the actual live state'
                          % (key, live[key]['sha256'][:12], base.name, bm['installed_sha256'][key][:12]))
        elif (base / bm['staged_files'][key]).read_bytes() != live[key]['bytes']:
            errors.append('%s: base staged payload differs from the live bytes' % key)
        elif '%04o' % live[key]['mode'] != (bm.get('preimage_mode') or {}).get(key, '%04o' % live[key]['mode']):
            errors.append('%s: live mode %04o differs from the v0.1.2 install mode' % (key, live[key]['mode']))
    for key, want in (expect or {}).items():
        if key not in live or live[key]['sha256'] != want:
            errors.append('%s: live sha256 is not the expected %s' % (key, want[:12]))
    scan = scan_dropins(home, system_roots)
    errors += check_dropin_scan(scan, home)
    errors += check_chat_dropin(live['chat_dropin']['bytes'].decode())
    if errors:
        raise StageError('; '.join(errors))

    # 2. sources at --lab-rev (git objects only) and the frozen engine
    with tempfile.TemporaryDirectory(prefix='v013-final-src-') as tmp:
        lab_commit, source_dir = export_tree(lab_repo, lab_rev, RUNTIME_SRC, tmp)
        _, launcher_dir = export_tree(lab_repo, lab_commit, LAUNCHER_SRC, tmp)
        sources = controller_release.release_sources(
            source_dir, tuple(n for n in controller_release.REQUIRED if n not in V020_MODULES))
        if LAUNCHER in sources:
            raise StageError('%s is in both %s and %s at %s' % (LAUNCHER, RUNTIME_SRC, LAUNCHER_SRC, lab_commit[:12]))
        sources[LAUNCHER] = (launcher_dir / LAUNCHER).read_bytes()
        facts, errors = launcher_facts(launcher_dir)
    missing = [n for n in STUDIO_FILES if n not in sources]
    if missing:
        errors.append('release lacks the Studio modules: ' + ', '.join(missing))
    engine, engine_prov = load_engine(engine_artifacts, engine_rev)
    server_sha, oxe_sha = sha(engine['oxe-server']), sha(engine['oxe'])
    engine_dir = str(home / ENGINES / ('oxe-%s-%s' % (label, server_sha[:12])))
    source_sha = {name: sha(data) for name, data in sources.items()}
    release_id = sha(json.dumps(source_sha, sort_keys=True).encode())[:12]
    release_dir = str(home / RELEASES / ('%s-%s' % (label, release_id)))

    # 3. payloads
    live_runtime = same_format(live['runtime']['bytes'])
    old_server = {a: live_runtime['profiles'][a]['memlat_overrides']['oxe']['server_sha256'] for a in MOE_MODELS}
    if set(old_server.values()) != {bm['server_sha256']}:
        errors.append('runtime: live MoE servers are not the v0.1.2 server %s' % bm['server_sha256'][:12])

    def repin(obj, server_path):
        obj = json.loads(json.dumps(obj))
        for alias in MOE_MODELS:
            oxe_cfg = obj['profiles'][alias]['memlat_overrides']['oxe']
            if '--pf-attn' not in oxe_cfg['oxe_args']:
                oxe_cfg['oxe_args'] = oxe_cfg['oxe_args'] + PF_PIN
            oxe_cfg['server_binary'] = server_path
            oxe_cfg['server_sha256'] = server_sha
            oxe_cfg['validation'] = {'kind': 'v013-final-staged-unqualified', 'engine_source': engine_rev,
                                     'server_sha256': server_sha, 'qualified': False,
                                     'predecessor': oxe_cfg.get('validation')}
        return obj
    runtime = repin(live_runtime, engine_dir + '/oxe-server')
    gate = repin(live_runtime, str(out / 'engine' / 'oxe-server'))
    errors += check_runtime(runtime, live_runtime, server_sha, engine_dir + '/oxe-server')
    errors += check_runtime(gate, live_runtime, server_sha, str(out / 'engine' / 'oxe-server'))
    unit_live = live['unit']['bytes'].decode()
    unit = controller_release.base.rewrite_unit(unit_live, release_dir)
    errors += check_unit(unit, unit_live, release_dir)
    studio_unit = live['studio_unit']['bytes'].decode()
    dropin_live = live['studio_dropin']['bytes'].decode()
    dropin = rewrite_studio_dropin(dropin_live, release_dir)
    studio_errors, studio_old, studio_new = check_studio(dropin, dropin_live, studio_unit, release_dir)
    errors += studio_errors
    chat_eff = effective_service(unit, [live['chat_dropin']['bytes'].decode()])
    if chat_eff['ExecStart'] != '/usr/bin/python3 "%s/controller.py"' % release_dir or chat_eff['Restart'] != 'on-failure':
        errors.append('chat: effective ExecStart/Restart after the drop-in merge is not the staged release')
    errors += check_opencode(same_format(live['opencode']['bytes']))
    if errors:
        raise StageError('; '.join(errors))
    payload = {'unit': unit.encode(), 'runtime': dump_json(runtime), 'cli': controller_release.cli_source(sources),
               'launcher': sources[LAUNCHER], 'studio_dropin': dropin.encode()}
    preimages = {k: live[k]['bytes'] for k in KEYS}
    modes = {k: live[k]['mode'] for k in KEYS}
    for key in KEYS:
        if payload[key] == preimages[key]:
            raise StageError('%s: payload equals the live preimage; nothing to install' % key)

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
            put(STAGED[key], payload[key], 0o700 if key in ('cli', 'launcher') else 0o600)
            put('rollback/%s.original' % key, preimages[key])
        put('runtime.gate.json', dump_json(gate))
        put('opencode-mode-overlay.fast.json', dump_json(facts['fast']))
        put('opencode-mode-overlay.deep.json', dump_json(facts['deep']))
        put('engine/oxe-server', engine['oxe-server'], 0o700)
        put('engine/oxe', engine['oxe'], 0o700)
        for name, data in sources.items():
            put('runtime-source/' + name, data, 0o700 if name in EXECUTABLE else 0o600)
        for name in ('install_staged_release.py', 'stage_rollback_guard.py', 'stage_v013_final.py'):
            put('tools/' + name, (TOOLS / name).read_bytes(), 0o700 if name != 'stage_rollback_guard.py' else 0o600)
        doc = ('One-step v0.1.2 rollback (O-119): restore the exact v0.1.2 unit, runtime, cli, launcher and '
               'Studio drop-in (bytes and modes), then restart the controller and Studio.')
        message = ('Restored v0.1.2: unit, runtime, cli, launcher and the Studio drop-in (modes kept); controller '
                   'and Studio restarted; no model loaded. The v0.1.3 release and engine dirs were left in place.')
        put('rollback/rollback.py', guard.render_rollback(paths_for(paths), payload, preimages, staged_files=STAGED,
                                                          doc=doc, message=message, systemctl=systemctl,
                                                          services=SERVICES, modes=modes,
                                                          service_state=SERVICE_STATE), 0o700)
        probe = ('import json,sys; sys.path.insert(0, sys.argv[1]); import oxe_runtime as R; '
                 'print(json.dumps({"state_cache": bool(R.engine_has_state_cache(sys.argv[2])), '
                 '"state_preload": bool(R.engine_has_state_preload(sys.argv[2]))}))')
        try:  # the staged release's own probe on the staged binary, in an isolated child
            capabilities = json.loads(subprocess.run(
                [sys.executable, '-I', '-B', '-c', probe, str(out / 'runtime-source'), str(out / 'engine/oxe-server')],
                check=True, capture_output=True, text=True, timeout=120,
                env={'PATH': os.environ.get('PATH', '/usr/bin'), 'HOME': str(home)}).stdout)
        except (subprocess.SubprocessError, ValueError) as exc:  # recorded, never silently true
            capabilities = {'probe_error': type(exc).__name__}
        put('CAPABILITIES.json', dump_json(capabilities))
        try:
            gen_commit = git(TOOLS.parent, 'rev-parse', 'HEAD').decode().strip()
            gen_dirty = bool(git(TOOLS.parent, 'status', '--porcelain', '--', 'tools').strip())
        except (subprocess.CalledProcessError, OSError):
            gen_commit, gen_dirty = None, None
        tools = '%s/tools/' % out
        manifest = {
            **guard.manifest_binding(paths_for(paths), payload, preimages, STAGED, modes=modes),
            'installed': False, 'qualified': False, 'models_started': False, 'release': 'v0.1.3 (O-119)',
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
            'replaced_engine': {'server_sha256': bm['server_sha256'], 'engine_dir': bm.get('engine_dir'),
                                'source': bm.get('engine_source')},
            'engine_pins': {'profiles': list(MOE_MODELS), 'decode': '8 threads, --oxe-cpus 0-7',
                            'prefill': '12 threads, --oxe-prefill-cpus 0-11', 'context_cap': MOE_CONTEXT,
                            'prefill_attention': 'pinned: --pf-attn tiled appended to oxe_args (engine default auto '
                                                 'is tiled at n_ctx <= 131072 but fa2 above it); no fa2/fa2x',
                            'decode_attention': 'tiled (OXE_ATTN_TILED_MIN_KV=0); fused off (no OXE_ATTN*)',
                            'max_mode': 'unavailable (no rss_proof)', 'tool_output_cap': 'off', 'lan_share': 'off',
                            'flash': 'profile byte-identical to live (cache_pref 18, prefix store on, PSI off)',
                            'runtime_diff_vs_live': ['profiles.%s.memlat_overrides.oxe.%s' % (a, k)
                                                     for a in MOE_MODELS for k in RUNTIME_REPIN],
                            'oxe_args_change': 'live args + %s' % PF_PIN},
            'opencode': {'global_config': 'unchanged (precondition)', 'canonical_moe_input': FAST_INPUT,
                         'deep_override_input': DEEP_INPUT, 'launcher_default_mode': facts['default'],
                         'overlay_timeouts_ms': TIMEOUT_MS, 'moe_keep_tokens_launch_overlay_only': facts['keep']},
            'studio': {'release_dir': release_dir, 'previous_exec': studio_old['ExecStart'],
                       'previous_workdir': studio_old['WorkingDirectory'], 'exec': studio_new['ExecStart'],
                       'restart': studio_new['Restart'], 'oxi_bin': studio_new['Environment'].get('OXI_BIN'),
                       'sd_cli_probe': studio_new['Environment'].get('SD_CLI_PROBE'),
                       'image_config_unchanged': ['image_settings', 'image_engine'],
                       'modules': [n for n in STUDIO_FILES if n in sources],
                       'native_text_encoder': 'inert (no oxi.text_encoder; no TE)'},
            'payload_vs_live': {k: 'changed' for k in KEYS},
            'rollback': {'script': 'rollback/rollback.py', 'one_step_to': 'v0.1.2 (%s)' % base.resolve().name,
                         'restores': list(KEYS), 'modes': {k: '%04o' % m for k, m in modes.items()},
                         'services': SERVICES, 'systemctl': str(systemctl),
                         'service_state': {'file': SERVICE_STATE, 'recorded': 'at install (record-services, or '
                                           'install --apply when not yet recorded): systemctl --user is-active',
                                           'restart_rule': 'stop both; start only those recorded active; without '
                                           'the record, those active at rollback time'},
                         'leaves_in_place': [release_dir, engine_dir]},
            'commands': [
                'python3 -I %sstage_v013_final.py rehearse --base %s --workdir <fresh private dir> '
                '--lab-repo <lab> --lab-rev %s --engine-rev %s --engine-artifacts %s'
                % (tools, base, lab_commit, engine_rev, engine_prov['freeze_dir']),
                'python3 -I %sstage_v013_final.py precheck %s' % (tools, out),
                'python3 -I %sstage_v013_final.py materialize %s' % (tools, out),
                'python3 -I %sstage_v013_final.py record-services %s   (BEFORE stopping anything)' % (tools, out),
                'python3 -I %sstage_v013_final.py install %s            (precheck + dry run)' % (tools, out),
                'python3 -I %sstage_v013_final.py install %s --apply' % (tools, out),
                'systemctl --user daemon-reload && systemctl --user restart %s %s   (root only)' % (CHAT, STUDIO),
                'rollback: %s/rollback/rollback.py --check, then without --check' % out],
            'generator': {'path': 'tools/stage_v013_final.py', 'commit': gen_commit, 'tools_dirty': gen_dirty,
                          'sha256': sha((TOOLS / 'stage_v013_final.py').read_bytes())},
        }
        put('manifest.json', dump_json(manifest))
        put(SEAL, dump_json({'note': 'sha256 of every stage file at generation; receipts excluded',
                             'files': seal(out)}), 0o400)
        for path in out.rglob('*'):  # immutable payload: read-only files, writable dirs for receipts only
            if path.is_file() and not path.is_symlink():
                os.chmod(path, 0o500 if path.stat().st_mode & 0o100 else 0o400)
    finally:
        os.umask(old_umask)
    return manifest


def paths_for(paths):
    return {k: paths[k] for k in KEYS}


# ---------------------------------------------------------------------------------- install helpers
def precheck(stage, system_roots=None, out=None):
    """Seal, unchanged preconditions, drop-in scan and the installer's dry run; writes nothing."""
    out = out or sys.stdout
    stage = Path(stage)
    count = verify(stage)
    m = json.loads((stage / 'manifest.json').read_text())
    errors = []
    for key, item in m['preconditions'].items():
        try:
            got = capture_live(item['path'])
        except StageError as exc:
            errors.append(str(exc))
            continue
        if got['sha256'] != item['sha256'] or '%04o' % got['mode'] != item['mode']:
            errors.append('%s: %s changed since staging (sha256/mode)' % (key, item['path']))
    scan = scan_dropins(Path(m['home']), system_roots)
    if scan != m['dropin_scan']:
        errors.append('drop-in scan differs from the one recorded at staging')
    if errors:
        raise StageError('; '.join(errors))
    import install_staged_release as isr
    try:
        isr.run(stage, False, out=out)
    except isr.RefuseError as exc:
        raise StageError('installer dry run refused: %s' % exc) from None
    print('precheck OK: seal %d files, %d preconditions unchanged, drop-ins as staged' % (count, len(m['preconditions'])),
          file=out)
    return 0


def _materialize_dir(target, files, out):
    """files: name -> (bytes, mode). Create `target` atomically, or accept an identical existing one."""
    target = Path(target)
    if target.exists() or target.is_symlink():
        if target.is_symlink() or not target.is_dir():
            raise StageError('%s exists and is not a directory' % target)
        present = {p.name for p in target.iterdir() if p.name != '__pycache__'}
        if present != set(files) or any(sha((target / n).read_bytes()) != sha(d) for n, (d, _) in files.items()):
            raise StageError('%s exists with different content; refusing to touch it' % target)
        print('%s: already present and identical' % target, file=out)
        return 'present'
    target.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    tmp = Path(tempfile.mkdtemp(prefix='.' + target.name + '.', dir=target.parent))
    try:
        for name, (data, mode) in files.items():
            fd = os.open(tmp / name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, mode)
            with os.fdopen(fd, 'wb') as stream:
                stream.write(data)
                stream.flush()
                os.fsync(stream.fileno())
            os.chmod(tmp / name, mode)
        os.chmod(tmp, 0o700)
        os.rename(tmp, target)
    except BaseException:
        shutil.rmtree(tmp, ignore_errors=True)
        raise
    print('%s: created (%d files)' % (target, len(files)), file=out)
    return 'created'


def materialize(stage, out=None):
    out = out or sys.stdout
    stage = Path(stage)
    verify(stage)
    m = json.loads((stage / 'manifest.json').read_text())
    src = {}
    for name, want in m['runtime_source_sha256'].items():
        data = (stage / 'runtime-source' / name).read_bytes()
        if sha(data) != want:
            raise StageError('runtime-source/%s does not match the manifest' % name)
        src[name] = (data, 0o700 if name in EXECUTABLE else 0o600)
    eng = {}
    for name, key in (('oxe-server', 'server_sha256'), ('oxe', 'oxe_sha256')):
        data = (stage / 'engine' / name).read_bytes()
        if sha(data) != m[key]:
            raise StageError('engine/%s does not match the manifest' % name)
        eng[name] = (data, 0o700)
    home = Path(m['home'])
    for target in (m['release_dir'], m['engine_dir']):
        if not Path(target).is_relative_to(home / '.local/share/ai-stack'):
            raise StageError('%s is outside the stage home' % target)
    return {'release_dir': _materialize_dir(m['release_dir'], src, out),
            'engine_dir': _materialize_dir(m['engine_dir'], eng, out)}


def record_services(stage, out=None):
    """Write <stage>/service-state.json once: which of the two services are active now (read-only query)."""
    out = out or sys.stdout
    stage = Path(stage)
    m = json.loads((stage / 'manifest.json').read_text())
    target = stage / SERVICE_STATE
    if target.exists():
        print('service state already recorded: %s' % target.read_text().strip(), file=out)
        return json.loads(target.read_text())
    result = subprocess.run([m['rollback']['systemctl'], '--user', 'is-active', *SERVICES],
                            capture_output=True, text=True, timeout=60)
    states = result.stdout.split()
    if len(states) != len(SERVICES):
        raise StageError('systemctl is-active gave %r for %s' % (result.stdout.strip(), SERVICES))
    record = {'time': time.strftime('%Y-%m-%dT%H:%M:%S%z'), 'states': dict(zip(SERVICES, states)),
              'active': {s: st in ('active', 'activating', 'reloading') for s, st in zip(SERVICES, states)}}
    fd = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o400)
    with os.fdopen(fd, 'w') as stream:
        stream.write(json.dumps(record, indent=2) + '\n')
    print('service state recorded: %s' % json.dumps(record['states']), file=out)
    return record


def install(stage, apply, system_roots=None, out=None):
    """precheck, then (with --apply) record the service state if not yet recorded, then install."""
    out = out or sys.stdout
    precheck(stage, system_roots=system_roots, out=out)
    if not apply:
        return 0
    record_services(stage, out=out)
    import install_staged_release as isr
    try:
        return isr.run(stage, True, out=out)
    except isr.RefuseError as exc:
        raise StageError('install refused: %s' % exc) from None


# ---------------------------------------------------------------------------------- rehearsal
FAKE_SYSTEMCTL = ('#!/bin/sh\n# rehearsal systemctl: records the call, never contacts a service manager\n'
                  'printf "%%s\\n" "$*" >> %s\n'
                  'if [ "$2" = is-active ]; then shift 2; for u in "$@"; do echo active; done; fi\nexit 0\n')


def rehearse(*, base, workdir, lab_repo, lab_rev, engine_rev, engine_artifacts, source_home=None, system_roots=None,
             out=None):
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
    fake.write_text(FAKE_SYSTEMCTL % json.dumps(str(calls)))
    fake.chmod(0o700)
    src_paths, dst_paths = live_paths(source_home), live_paths(home)
    for key, src in src_paths.items():  # read-only copies with the live modes
        got = capture_live(src)
        dst = dst_paths[key]
        rel_parent = dst.parent.relative_to(home)
        cur = home
        for part in rel_parent.parts:
            cur = cur / part
            if not cur.exists():
                cur.mkdir(mode=0o700)
                live_dir = source_home / cur.relative_to(home)
                os.chmod(cur, (live_dir.stat().st_mode & 0o777) if live_dir.is_dir() else 0o700)
        dst.write_bytes(got['bytes'])
        os.chmod(dst, got['mode'])
    before = {k: (p.read_bytes(), p.stat().st_mode & 0o7777) for k, p in dst_paths.items()}
    stage = workdir / 'stage'
    m = generate(base=base, out=stage, lab_repo=lab_repo, lab_rev=lab_rev, engine_rev=engine_rev,
                 engine_artifacts=engine_artifacts, home=home, systemctl=str(fake), system_roots=system_roots)
    log = io.StringIO()
    precheck(stage, system_roots=system_roots, out=log)
    materialize(stage, out=log)
    materialize(stage, out=log)  # idempotent
    install(stage, True, system_roots=system_roots, out=log)
    steps = {'generate': True, 'precheck': True, 'materialize': True, 'install': True}
    problems = []
    for key in KEYS:
        if dst_paths[key].read_bytes() != (stage / STAGED[key]).read_bytes():
            problems.append('%s: not installed' % key)
        if dst_paths[key].stat().st_mode & 0o7777 != before[key][1]:
            problems.append('%s: mode changed by install' % key)
    for key in PRECONDITIONS:
        if (dst_paths[key].read_bytes(), dst_paths[key].stat().st_mode & 0o7777) != before[key]:
            problems.append('%s: a precondition file was touched' % key)
    chat = effective_service(dst_paths['unit'].read_text(), [dst_paths['chat_dropin'].read_text()])
    studio = effective_service(dst_paths['studio_unit'].read_text(), [dst_paths['studio_dropin'].read_text()])
    for name, eff, entry in (('chat', chat, 'controller.py'), ('studio', studio, 'studio')):
        target = Path(m['release_dir']) / entry
        if eff['ExecStart'] != '/usr/bin/python3 "%s"' % target or not target.is_file():
            problems.append('%s: effective ExecStart does not run the materialized release' % name)
        elif entry.endswith('.py') or entry == 'studio':
            with tempfile.TemporaryDirectory() as tmp:
                py_compile.compile(str(target), cfile=str(Path(tmp) / 'probe.pyc'), doraise=True)
        if eff['Restart'] != 'on-failure':
            problems.append('%s: Restart=on-failure not in effect' % name)
    runtime = json.loads(dst_paths['runtime'].read_text())
    for alias in MOE_MODELS:
        server = Path(runtime['profiles'][alias]['memlat_overrides']['oxe']['server_binary'])
        if not server.is_file() or sha(server.read_bytes()) != m['server_sha256']:
            problems.append('%s: runtime server_binary is not the materialized candidate' % alias)
    if (calls.read_text().splitlines() if calls.exists() else []) != ['--user is-active %s %s' % (CHAT, STUDIO)]:
        problems.append('install made service calls other than one is-active query')
    check = subprocess.run([sys.executable, '-I', str(stage / 'rollback/rollback.py'), '--check'],
                           capture_output=True, text=True, timeout=120)
    roll = subprocess.run([sys.executable, '-I', str(stage / 'rollback/rollback.py')],
                          capture_output=True, text=True, timeout=120)
    again = subprocess.run([sys.executable, '-I', str(stage / 'rollback/rollback.py')],
                           capture_output=True, text=True, timeout=120)
    steps['rollback'] = check.returncode == 0 and roll.returncode == 0 and again.returncode == 0
    if not steps['rollback']:
        problems.append('rollback failed: %s %s %s' % (check.stderr.strip(), roll.stderr.strip(), again.stderr.strip()))
    after = {k: (p.read_bytes(), p.stat().st_mode & 0o7777) for k, p in dst_paths.items()}
    if after != before:
        problems.append('rollback did not restore every byte and mode: '
                        + ', '.join(k for k in before if after[k] != before[k]))
    want_calls = ['--user is-active %s %s' % (CHAT, STUDIO), '--user stop %s %s' % (CHAT, STUDIO), '--user daemon-reload', '--user start %s %s' % (CHAT, STUDIO)]
    got_calls = calls.read_text().splitlines() if calls.exists() else []
    if got_calls != want_calls:
        problems.append('rollback service calls %r != %r' % (got_calls, want_calls))
    receipt = {'action': 'o119-rehearsal', 'time': time.strftime('%Y-%m-%dT%H:%M:%S%z'), 'workdir': str(workdir),
               'private_home': str(home), 'source_home': str(source_home), 'stage': str(stage),
               'lab_source': m['lab_source'], 'engine_source': m['engine_source'], 'server_sha256': m['server_sha256'],
               'release_id': m['release_id'], 'preimage_sha256': m['preimage_sha256'],
               'installed_sha256': m['installed_sha256'], 'systemctl_calls': got_calls, 'steps': steps,
               'second_rollback': again.stdout.strip(), 'passed': not problems, 'problems': problems,
               'real_service_manager_contacted': False}
    (workdir / 'REHEARSAL.json').write_text(json.dumps(receipt, indent=2) + '\n')
    print(log.getvalue(), end='', file=out)
    print('rehearsal %s: %s' % ('PASSED' if not problems else 'FAILED', workdir / 'REHEARSAL.json'), file=out)
    if problems:
        raise StageError('rehearsal failed: ' + '; '.join(problems))
    return receipt


# ---------------------------------------------------------------------------------- CLI
def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest='command', required=True)
    for name in ('generate', 'rehearse'):
        p = sub.add_parser(name)
        p.add_argument('--base', type=Path, required=True, help='the installed v0.1.2 stage (staged/latest-v012-production)')
        p.add_argument('--lab-repo', type=Path, required=True)
        p.add_argument('--lab-rev', required=True)
        p.add_argument('--engine-rev', required=True, help='full sha of the frozen engine build')
        p.add_argument('--engine-artifacts', type=Path, required=True, help='freeze dir (manifest.json + default/)')
        if name == 'generate':
            p.add_argument('--out', type=Path, required=True)
            p.add_argument('--expect', action='append', default=[], metavar='KEY=SHA256',
                           help='also require this live sha256 (e.g. studio_dropin=...)')
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
                     engine_rev=args.engine_rev, engine_artifacts=args.engine_artifacts)
        else:
            expect = dict(item.split('=', 1) for item in args.expect)
            m = generate(base=args.base, out=args.out, lab_repo=args.lab_repo, lab_rev=args.lab_rev,
                         engine_rev=args.engine_rev, engine_artifacts=args.engine_artifacts,
                         systemctl=args.systemctl, expect=expect)
            print(json.dumps({k: m[k] for k in ('stage', 'lab_source', 'engine_source', 'server_sha256', 'release_dir',
                                                'engine_dir', 'preimage_sha256', 'preimage_mode', 'installed_sha256')},
                             indent=2))
    except (StageError, guard.GuardError, ValueError, KeyError, OSError, subprocess.CalledProcessError) as exc:
        print('REFUSED: %s' % exc, file=sys.stderr)
        return 2
    return 0


if __name__ == '__main__':
    sys.exit(main())
