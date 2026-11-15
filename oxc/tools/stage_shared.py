"""Shared integrity/capture/install helpers for the retained stage command; bodies preserved verbatim."""

import hashlib

import io

import json

import os

from pathlib import Path

import re

import shutil

import stat

import subprocess

import sys
sys.dont_write_bytecode = True

import tarfile

import tempfile

import time

RUNTIME_SRC = 'src/oxc/runtime'

CHAT, STUDIO = 'oxc-chat.service', 'prompt-studio-app.service'

SERVICES = [CHAT, STUDIO]

REL = {  # live path of every key, relative to HOME
    'unit': '.config/systemd/user/' + CHAT,
    'runtime': '.config/oxc/llm/runtime.json',
    'cli': '.local/bin/oxc',
    'launcher': '.local/bin/launch_local_plan.py',
    'studio_dropin': '.config/systemd/user/' + STUDIO + '.d/10-imgship.conf',
    'opencode': '.config/opencode/opencode.json',
    'studio_unit': '.config/systemd/user/' + STUDIO,
    'chat_dropin': '.config/systemd/user/' + CHAT + '.d/model-profiles.conf',
    'image_settings': '.config/oxc/images/runtime-settings.json',
    'image_engine': '.config/oxc/images/engine.json',
}

EXPECTED_DROPINS = {CHAT: ['model-profiles.conf'], STUDIO: ['10-imgship.conf']}

RELEASES = '.local/share/ai-stack/runtime-releases'

ENGINES = '.local/share/ai-stack/releases'

MOE_MODELS = ('ornith-35b-a3b-local', 'qwen3.6-35b-a3b-local')

FLASH = 'qwen3.8-flash-next-local'

FAST_INPUT, DEEP_INPUT, MOE_CONTEXT, MOE_OUTPUT = 65536, 120000, 131072, 16384

CACHE = {'disk_budget_bytes': 64000000000, 'ttl_secs': 604800}

WATCHDOG = 'Environment=OXC_IMAGE_WATCHDOG_KILL_CLIENT=1'

STUDIO_FILES = ('studio', 'studio_page.py', 'studio_pixels.py', 'studio_context.py', 'studio_worker.py',
                'studio_plan_generate.py', 'studio_plan_edit.py', 'studio_oxi.py', 'image_slot.py', 'image_npu.py',
                'image_npu_worker.py', 'image_size_gate.py')

EXECUTABLE = ('oxc', 'studio')

SEAL = 'SEAL.json'

SERVICE_STATE = 'service-state.json'

SEAL_SKIP = ('receipts', 'retired.json', 'rollback/receipts', SERVICE_STATE, SEAL)

LIVE_LIMIT = 8 * 1024 ** 2

SYSTEM_UNIT_ROOTS = ('/etc/systemd/user', '/run/systemd/user', '/usr/local/lib/systemd/user', '/usr/lib/systemd/user',
                     '/usr/local/share/systemd/user', '/usr/share/systemd/user')

class StageError(Exception):
    pass

def sha(data):
    return hashlib.sha256(data).hexdigest()

def git(repo, *args):
    return subprocess.run(['git', '-C', str(repo), *args], check=True, capture_output=True).stdout

LEGACY_TOP = 'lab'

LEGACY_LAYOUT = {'src/oxc/runtime': 'src/snapdragon_ai_lab/runtime',
                 'src/oxc/adapters/opencode': 'src/snapdragon_ai_lab/adapters/opencode'}

def export_tree(repo, rev, prefix, dest):
    """Files under `prefix` at commit `rev` (git objects only; the worktree is ignored). The lookup runs from
    the repository top, so it does not depend on the name of the checkout directory. A revision that predates
    the oxc rename keeps the old layout (under oxc/): use it when the new prefix is absent there."""
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
    for name in ('OXC_TOOL_OUTPUT_CAP', 'OXI_TEXT_ENCODER'):
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
           or 'OXC_TOOL_OUTPUT_CAP' in l]
    return ['chat drop-in model-profiles.conf sets %s (conflicts with the staged unit)' % ', '.join(bad)] if bad else []

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

FAKE_SYSTEMCTL = ('#!/bin/sh\n# rehearsal systemctl: records the call, never contacts a service manager\n'
                  'printf "%%s\\n" "$*" >> %s\n'
                  'if [ "$2" = is-active ]; then shift 2; for u in "$@"; do echo active; done; fi\nexit 0\n')
