"""local-ai uninstall: remove exactly what an ox-llm install recorded, keep everything else.

Stdlib only; `local-ai` loads this file with importlib from the runtime directory, so no
package-relative imports. Default is a DRY RUN that prints the plan; --apply acts.

Ownership (V020-SEAM-REVIEW): a file is removed only when its exact path is listed in an install
record AND its current sha256 is one that record says ox-llm installed there. There is no prefix,
glob or directory sweep. Install records:
  - the staged releases `local-ai update` installed (update state: current + history, each stage's
    manifest.json: `paths` x `installed_sha256`, the release_dir files from `runtime_source_sha256`,
    the engine_dir binaries from `server_sha256` / `oxe_sha256`);
  - a bootstrap install journal (`files`: path + sha256 under its `target_home`);
  - any extra record passed with --record (a stage manifest.json or a bootstrap journal).
Anything not recorded, or changed since it was installed, is reported and kept. A directory is
removed only when it is a recorded release/engine dir or a unit drop-in dir and is EMPTY afterwards.

Services: before any file is touched, every ox-llm unit (the chat controller, Studio, and any recorded
unit) must be POSITIVELY stopped: `systemctl --user is-active` must print `inactive` or `failed` for
each. A failed stop, an unknown state or a systemctl that cannot be asked aborts (exit 2) with every
file intact; units this run stopped are started again.

Model weights are kept by default. --remove-models deletes only the exact model files runtime.json
names (model_path / sidecar_path / draft_model_path / mtp_path) inside home, owned by this user; for a
file outside home it prints a quoted command for the user. Never a directory, never sudo.

Never touches any coding client's own install or configuration (decision O-151; the core names no
client). Exit 0 ok, 1 partial failure after the services were stopped, 2 refused (nothing removed).
"""
import argparse
import hashlib
import json
import os
import shlex
import subprocess
import sys
from pathlib import Path

CONTROLLER_UNIT = 'local-ai-chat.service'
NAMED_UNITS = (CONTROLLER_UNIT, 'prompt-studio-app.service')
STOPPED = ('inactive', 'failed')  # is-active output of a unit that is not running
ABSENT = 'absent'
MODEL_KEYS = ('model_path', 'sidecar_path', 'draft_model_path', 'mtp_path')
# Where ox-llm puts things: reported when something there is left (never swept).
PLACES = ('.local/bin/local-ai', '.local/bin/oxa', '.local/bin/launch_local_plan.py', '.config/local-ai',
          '.local/state/local-ai', '.local/share/ai-stack/runtime', '.local/share/ai-stack/runtime-releases',
          '.local/share/ai-stack/releases', '.config/omarchy/plugins/snapdragon.local-ai')
ENDPOINT = 'http://127.0.0.1:8092/v1'
CLIENT_NOTE = ('Client configurations outside ~/.config/local-ai are yours and are left in place; a client set up '
               'for ' + ENDPOINT + ' stops working until ox-llm is installed again.')
LIMIT = 8 * 1024 ** 2  # record files are small JSON


class Refused(Exception):
    pass


def check_home(home):
    if home is None:
        raise Refused('no home directory given')
    text = str(home)
    if not os.path.isabs(text):
        raise Refused('home must be an absolute path: ' + text)
    path = Path(os.path.normpath(text))
    if path == Path('/') or path.resolve() == Path('/'):
        raise Refused('refusing to work with home = /')
    if not path.is_dir():
        raise Refused('home is not a directory: ' + text)
    return path


def inside(path, root):
    try:
        Path(path).relative_to(root)
        return True
    except ValueError:
        return False


def exists(path):
    return os.path.lexists(path)


def sha_file(path):
    h = hashlib.sha256()
    with open(path, 'rb') as stream:
        for chunk in iter(lambda: stream.read(1 << 20), b''):
            h.update(chunk)
    return h.hexdigest()


def read_json(path):
    try:
        path = Path(path)
        if path.is_symlink() or not path.is_file() or path.stat().st_size > LIMIT:
            return None
        value = json.loads(path.read_text())
        return value if isinstance(value, dict) else None
    except (OSError, ValueError, UnicodeError):
        return None


# ------------------------------------------------------------------------------- install records
def update_state(home):
    return read_json(home / '.local/state/local-ai/update/state.json') or {}


def records_from_stage(manifest_path):
    """[(path, sha256)] a stage manifest says ox-llm installed, and [dirs] it created."""
    m = read_json(manifest_path)
    if not m:
        return [], []
    files, dirs = [], []
    paths, installed = m.get('paths') or {}, m.get('installed_sha256') or {}
    if isinstance(paths, dict) and isinstance(installed, dict):
        for key, path in paths.items():
            want = installed.get(key)
            if isinstance(path, str) and os.path.isabs(path) and isinstance(want, str) and want != ABSENT:
                files.append((Path(path), want))
    release = m.get('release_dir')
    if isinstance(release, str) and os.path.isabs(release) and isinstance(m.get('runtime_source_sha256'), dict):
        for name, want in m['runtime_source_sha256'].items():
            if isinstance(name, str) and '/' not in name and name not in ('', '.', '..') and isinstance(want, str):
                files.append((Path(release) / name, want))
        dirs.append(Path(release))
    engine = m.get('engine_dir')
    if isinstance(engine, str) and os.path.isabs(engine):
        for name, key in (('oxe-server', 'server_sha256'), ('oxe', 'oxe_sha256')):
            if isinstance(m.get(key), str):
                files.append((Path(engine) / name, m[key]))
        dirs.append(Path(engine))
    return files, dirs


def records_from_journal(journal_path):
    j = read_json(journal_path)
    if not j or not j.get('installed') or not isinstance(j.get('target_home'), str):
        return [], []
    target = Path(j['target_home'])
    files = [(target / row['path'], row['sha256']) for row in j.get('files') or []
             if isinstance(row, dict) and isinstance(row.get('path'), str) and isinstance(row.get('sha256'), str)
             and not os.path.isabs(row['path']) and '..' not in Path(row['path']).parts]
    return files, []


def install_records(home, extra=()):
    """{path: {sha256, ...}} and recorded dirs, from every install record found or given."""
    sources = []
    state = update_state(home)
    for entry in [state.get('current')] + list(state.get('history') or []):
        if isinstance(entry, dict) and isinstance(entry.get('stage'), str):
            if entry.get('action', 'update') == 'update' and entry.get('result', 'installed') == 'installed':
                sources.append(Path(entry['stage']) / 'manifest.json')
    owned, dirs, used = {}, [], []
    for source in list(dict.fromkeys(sources)) + [Path(p) for p in extra]:
        doc = read_json(source)
        if doc is None:
            continue
        files, more = records_from_journal(source) if doc.get('kind') == 'sandbox-source-install' \
            else records_from_stage(source)
        if files or more:
            used.append(str(source))
        for path, want in files:
            owned.setdefault(Path(os.path.normpath(path)), set()).add(want)
        dirs += [Path(os.path.normpath(d)) for d in more if Path(os.path.normpath(d)) not in dirs]
    return owned, dirs, used


# ------------------------------------------------------------------------------- safety
def vet(home, path):
    """None when `path` may be removed, else the reason. Exact regular files (or symlinks, which are
    unlinked, never followed) inside home, owned by this user, with no parent leaving home."""
    real_home = home.resolve()
    if not inside(path, home):
        return 'outside home'
    if not inside(path.parent.resolve(), real_home):
        return 'a parent directory is a symlink that leaves home'
    try:
        info = path.lstat()
    except OSError:
        return None
    if info.st_uid != os.getuid():
        return 'not owned by the current user'
    if not path.is_symlink() and not path.is_file():
        return 'not a regular file'
    return None


def vet_dir(home, folder):
    """None when the empty directory `folder` may be removed, else the reason: a real directory (no
    symlink), owned by this user, that resolves inside home and whose parent resolves inside home."""
    real_home = home.resolve()
    if not inside(folder, home):
        return 'outside home'
    try:
        info = folder.lstat()
    except OSError:
        return 'gone'
    if folder.is_symlink() or not folder.is_dir():
        return 'not a real directory'
    if not inside(folder.parent.resolve(), real_home) or not inside(folder.resolve(), real_home):
        return 'resolves outside home (a parent is a symlink that leaves home)'
    if info.st_uid != os.getuid():
        return 'not owned by the current user'
    return None


def owned_now(path, wants):
    """True when the live file is one of the recorded installs (exact sha256, regular file)."""
    try:
        if path.is_symlink() or not path.is_file():
            return False
        return sha_file(path) in wants
    except OSError:
        return False


# ------------------------------------------------------------------------------- models
def model_files(home):
    """Exact model files runtime.json names (never their directories)."""
    found = []
    value = read_json(home / '.config/local-ai/llm/runtime.json')

    def walk(item):
        if isinstance(item, dict):
            for key, child in item.items():
                if key in MODEL_KEYS and isinstance(child, str) and os.path.isabs(child):
                    path = Path(os.path.normpath(child))
                    if path not in found:
                        found.append(path)
                else:
                    walk(child)
        elif isinstance(item, list):
            for child in item:
                walk(child)
    walk(value or {})
    return found


def bar_note(home):
    shell = home / '.config/omarchy/shell.json'
    try:
        listed = 'snapdragon.local-ai' in shell.read_text()
    except OSError:
        listed = False
    if not listed:
        return None
    return (str(shell) + ' still lists the snapdragon.local-ai widget in the bar layout; it is not edited. '
            'Remove that entry yourself, then restart the shell.')


# ------------------------------------------------------------------------------- plan
def plan(home, remove_models, extra_records=()):
    owned, dirs, used = install_records(home, extra_records)
    unit_root = home / '.config/systemd/user'
    remove, kept, refused, notes, commands = [], [], [], [], []
    for path in sorted(owned):
        if not exists(path):
            continue
        if not owned_now(path, owned[path]):
            kept.append((str(path), 'changed since ox-llm installed it (sha256 differs); kept'))
            continue
        reason = vet(home, path)
        if reason:
            refused.append((str(path), reason))
            continue
        remove.append(path)
    for place in PLACES:
        path = home / place
        if exists(path) and path not in remove:
            if path.is_dir() and not path.is_symlink():
                kept.append((str(path), 'directory left in place; only its recorded files are removed'))
            else:
                kept.append((str(path), 'not in an ox-llm install record; kept'))
    models = model_files(home)
    for path in models:
        if not exists(path):
            continue
        if not remove_models:
            kept.append((str(path), 'model weights (pass --remove-models to delete this file)'))
        elif inside(path, home):
            reason = vet(home, path)
            if reason:
                refused.append((str(path), reason))
            else:
                remove.append(path)
        else:
            kept.append((str(path), 'model file outside home; never removed by this tool'))
            commands.append('rm -- ' + shlex.quote(str(path)))
    # Units are computed from the COMPLETE remove list (recorded files AND model files): any file
    # removal at all means the controller and Studio (which map those files) must stop first.
    owned_units = sorted(p.name for p in remove if p.parent == unit_root and p.name.endswith(('.service', '.timer')))
    units = sorted(set(owned_units) | {name for name in NAMED_UNITS if exists(unit_root / name)}) if remove else []
    # Recorded directories and unit drop-in dirs: removed only if empty after the files go (vetted again
    # right before the rmdir).
    cleanup = [d for d in dirs if inside(d, home)] + [unit_root / (name + '.d') for name in owned_units]
    for folder in cleanup:
        reason = vet_dir(home, folder) if exists(folder) else None
        if reason:
            refused.append((str(folder), reason))
    if not used:
        notes.append('no ox-llm install record was found (update state, or pass --record); no program file is removed')
    notes.append(CLIENT_NOTE)
    note = bar_note(home)
    if note:
        notes.append(note)
    return {'units': units, 'disable': owned_units, 'remove': remove, 'cleanup': cleanup, 'kept': kept, 'notes': notes,
            'commands': commands, 'refused': refused, 'records': used}


# ------------------------------------------------------------------------------- services
def run(systemctl, *args):
    try:
        done = subprocess.run([systemctl, '--user', *args], stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE, text=True, timeout=60)
    except (OSError, subprocess.SubprocessError) as error:
        return None, '', str(error)
    return done.returncode, (done.stdout or '').strip(), (done.stderr or '').strip()


def states(systemctl, units):
    """{unit: state} from one is-active call; None when the answer cannot be trusted."""
    code, out, _ = run(systemctl, 'is-active', *units)
    lines = out.splitlines()
    if code is None or len(lines) != len(units):
        return None
    return dict(zip(units, (line.strip() for line in lines)))


def stop_all(systemctl, units):
    """Positively stop every unit or raise Refused (restarting what this call stopped)."""
    before = states(systemctl, units)
    if before is None:
        raise Refused('cannot ask systemctl whether the ox-llm services run; nothing removed')
    running = [u for u in units if before[u] not in STOPPED]
    if running:
        run(systemctl, 'stop', *running)
    after = states(systemctl, units)
    bad = [u for u in units if after is None or after.get(u) not in STOPPED]
    if bad:
        was_active = [u for u in running if before[u] == 'active']
        if was_active:
            run(systemctl, 'start', *was_active)
        shown = ', '.join('%s (%s)' % (u, 'unknown' if after is None else after.get(u) or 'unknown') for u in bad)
        raise Refused('could not prove these services stopped: ' + shown + '; nothing removed'
                      + ('; restarted ' + ', '.join(was_active) if was_active else ''))
    return running


def apply(home, value, systemctl):
    """Returns (exit code, removed, failed). Positive stop first, then exact owned files only."""
    removed, failed = [], []
    units = value['units']
    if units:
        stop_all(systemctl, units)
    if value.get('disable'):
        code, _, err = run(systemctl, 'disable', *value['disable'])
        if code != 0:
            failed.append(('systemctl disable', err or 'exit ' + str(code)))
    owned, _, _ = install_records(home, value.get('extra_records', ()))
    for path in value['remove']:
        if not exists(path):
            continue
        wants = owned.get(Path(os.path.normpath(path)))
        if wants is not None and not owned_now(path, wants):  # changed between plan and apply
            failed.append((str(path), 'changed since the plan; kept'))
            continue
        reason = vet(home, path)
        if reason:
            failed.append((str(path), reason))
            continue
        try:
            os.unlink(path)
            removed.append(str(path))
        except OSError as error:
            failed.append((str(path), str(error)))
    for folder in value['cleanup']:
        if not exists(folder):
            continue
        reason = vet_dir(home, folder)  # immediately before the rmdir
        if reason:
            if reason != 'gone':
                failed.append((str(folder), reason + '; directory kept'))
            continue
        try:
            if not any(folder.iterdir()):
                folder.rmdir()
                removed.append(str(folder))
        except OSError:
            pass
    if units:
        code, _, err = run(systemctl, 'daemon-reload')
        if code != 0:
            failed.append(('systemctl daemon-reload', err or 'exit ' + str(code)))
    return (1 if failed else 0), removed, failed


# ------------------------------------------------------------------------------- report / CLI
def report(value, out, as_json, applied, removed, failed, code, message=None):
    if as_json:
        body = {'applied': applied, 'exit': code, 'units': value.get('units', []),
                'remove' if not applied else 'removed': removed, 'records': value.get('records', []),
                'failed': [{'path': a, 'reason': b} for a, b in failed],
                'kept': [{'path': a, 'reason': b} for a, b in value.get('kept', [])],
                'refused': [{'path': a, 'reason': b} for a, b in value.get('refused', [])],
                'notes': value.get('notes', []), 'commands': value.get('commands', [])}
        if message:
            body['error'] = message
        print(json.dumps(body, indent=2), file=out)
        return
    if message:
        print('local-ai uninstall: refused: ' + message, file=out)
    print('Removed:' if applied else 'Planned (not removed):' if message else
          'Dry run (nothing changed; pass --apply to remove):', file=out)
    for item in removed:
        print('  - ' + item, file=out)
    if not removed:
        print('  (nothing)', file=out)
    if value.get('units') and not applied:
        print('Services that must stop first: ' + ', '.join(value['units']), file=out)
    for title, rows in (('Refused:', value.get('refused', [])), ('Failed:', failed), ('Kept:', value.get('kept', []))):
        if rows:
            print(title, file=out)
            for path, reason in rows:
                print('  - ' + path + ': ' + reason, file=out)
    if value.get('commands'):
        print('Run yourself to finish (this tool never deletes outside home or uses sudo):', file=out)
        for command in value['commands']:
            print('  ' + command, file=out)
    if value.get('notes'):
        print('Notes:', file=out)
        for note in value['notes']:
            print('  - ' + note, file=out)


def main(argv=None, *, home=None, systemctl='systemctl', out=None):
    out = out or sys.stdout
    parser = argparse.ArgumentParser(prog='local-ai uninstall',
                                     description='Remove ox-llm from this user account (dry run unless --apply).')
    parser.add_argument('--apply', action='store_true', help='actually remove (default: print the plan)')
    parser.add_argument('--remove-models', action='store_true', help='also delete the model files runtime.json names')
    parser.add_argument('--record', action='append', default=[], metavar='PATH',
                        help='an extra install record: a stage manifest.json or a bootstrap install journal')
    parser.add_argument('--json', action='store_true', help='machine-readable output')
    try:
        args = parser.parse_args(list(argv or []))
    except SystemExit as error:
        return 2 if error.code else 0
    value = {}
    try:
        root = check_home(home if home is not None else os.environ.get('HOME') or str(Path.home()))
        value = plan(root, args.remove_models, args.record)
        value['extra_records'] = args.record
        if value['refused']:
            raise Refused('unsafe paths in the plan (see Refused); nothing removed')
        if not args.apply:
            report(value, out, args.json, False, [str(p) for p in value['remove']], [], 0)
            return 0
        code, removed, failed = apply(root, value, systemctl)
    except Refused as error:
        report(value, out, args.json, False, [str(p) for p in value.get('remove', [])], [], 2, str(error))
        return 2
    report(value, out, args.json, True, removed, failed, code)
    return code


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
