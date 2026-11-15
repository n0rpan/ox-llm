"""Shared unit rewrite helpers; bodies preserved verbatim."""

RUNTIME_FILES = ('local-ai', 'studio')

def rewrite_unit(text, release_dir):
    """Re-point WorkingDirectory/ExecStart only; the unit keeps its one MemoryMax line as is."""
    if len(unit_limits(text)) != 1:
        raise ValueError('unit_must_have_exactly_one_memory_limit')
    lines, done = [], set()
    for line in text.splitlines():
        if line.startswith('WorkingDirectory='):
            line = 'WorkingDirectory=' + release_dir; done.add('cwd')
        elif line.startswith('ExecStart='):
            line = 'ExecStart=/usr/bin/python3 "%s/controller.py"' % release_dir; done.add('exec')
        lines.append(line)
    if done != {'cwd', 'exec'}:
        raise ValueError('unexpected_unit_layout')
    return '\n'.join(lines) + '\n'

def unit_limits(text):
    return [line for line in text.splitlines() if line.startswith('MemoryMax=')]
