"""One-time copy of a pre-rename config directory (~/.config/local-ai/llm) to the new one
(~/.config/oxc/llm). Plain copy: owner-only mode kept, symlinks never followed, the key file is never
parsed or printed, nothing in the legacy directory is deleted."""
import json
import os
import sys
import shutil
import stat
import tempfile
from pathlib import Path

FILES = ('runtime.json', 'cloud-secrets.json')


def config_home():
    return Path(os.environ.get('XDG_CONFIG_HOME') or Path.home() / '.config')


def legacy_dir():
    return config_home() / 'local-ai/llm'


def default_dir():
    return config_home() / 'oxc/llm'


def _copy(source, target):
    """Copy one regular file (never a symlink) to a new 0600 file; return False when skipped."""
    try:
        fd = os.open(source, os.O_RDONLY | os.O_NOFOLLOW)
    except OSError:
        return False
    with os.fdopen(fd, 'rb') as src:
        if not stat.S_ISREG(os.fstat(src.fileno()).st_mode):
            return False
        data = src.read()
    out = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(out, 'wb') as dst:
        dst.write(data)
    return True


def migrate(new=None, legacy=None):
    """Copy when `new` does not exist and `legacy` does. Returns the copied file names (maybe empty)."""
    new, legacy = Path(new or default_dir()), Path(legacy or legacy_dir())
    if os.path.lexists(new) or not legacy.is_dir() or legacy.is_symlink():
        return []
    new.parent.mkdir(parents=True, exist_ok=True)
    stage = Path(tempfile.mkdtemp(prefix='.migrate-', dir=new.parent))
    try:
        copied = [name for name in FILES if _copy(legacy / name, stage / name)]
        os.rename(stage, new)
    except BaseException:
        shutil.rmtree(stage, ignore_errors=True)
        raise
    runtime = new / 'runtime.json'
    if 'runtime.json' in copied:
        try:
            value = json.loads(runtime.read_text())
            cloud = value.get('cloud') if isinstance(value, dict) else None
            secrets = cloud.get('secrets_file') if isinstance(cloud, dict) else None
            if isinstance(secrets, str) and Path(secrets).parent == legacy and Path(secrets).name in copied:
                cloud['secrets_file'] = str(new / Path(secrets).name)
                runtime.write_text(json.dumps(value, indent=2) + '\n')
        except (OSError, ValueError):
            pass
    if copied:
        print(f'oxc: copied {", ".join(copied)} from {legacy} to {new} (the old files were left in place)', file=sys.stderr)
    return copied
