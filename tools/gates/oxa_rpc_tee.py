#!/usr/bin/env python3
"""T1.5 hop probe: run the real oxa with its stdin/stdout piped through, and keep a copy of its
stdout (the RPC event stream) in $HOP_RPC_OUT. Used only with HOP_PROBE=1: tiny_real.sh points
OXA_BIN at this file and OXA_REAL at the real binary. Argv and stdin pass through unchanged; signals
are forwarded to the child, and the exit status is the child's. MIT.
"""
import os
import signal
import subprocess
import sys


def main():
    real = os.environ.get('OXA_REAL')
    out_path = os.environ.get('HOP_RPC_OUT')
    if not real or not out_path:
        sys.stderr.write('oxa_rpc_tee: OXA_REAL and HOP_RPC_OUT must be set\n')
        return 2
    child = subprocess.Popen([real] + sys.argv[1:], stdin=sys.stdin.fileno(), stdout=subprocess.PIPE)

    def forward(signum, _frame):
        if child.poll() is None:
            child.send_signal(signum)

    for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        signal.signal(sig, forward)
    with open(out_path, 'ab') as log:
        for line in child.stdout:
            log.write(line)
            log.flush()
            sys.stdout.buffer.write(line)
            sys.stdout.buffer.flush()
    return child.wait()


if __name__ == '__main__':
    sys.exit(main())
