#!/usr/bin/env python3
"""Split the lab unit suite (tests/**/test*.py, as discover finds them) into N balanced CI shards and run one.

usage: ci_test_shard.py N              print every shard with its estimated seconds
       ci_test_shard.py list N K       print the test modules of shard K (0-based)
       ci_test_shard.py run N K        run shard K verbosely (call it through
                                       LAB_TEST_SHARD=K/N tools/run_lab_tests.sh for the isolated HOME)

Shards are whole test modules, loaded exactly as `unittest discover -s tests` loads them (tests/ on
sys.path, loadTestsFromModule, the load_tests protocol), so tests a module imports from a helper
module (e.g. openai_api_stack.py) still run with it. Balancing: longest-processing-time greedy over
per-module seconds measured on the x64 helper (ci-lab run 43474664355, 2026-11-13); unlisted modules
weigh DEFAULT_S. Every module lands in exactly one shard (checked on every call), so a new test file
is never silently skipped; the weights only affect balance, never coverage.
"""
import importlib
import sys
import unittest
from pathlib import Path

TESTS = Path(__file__).resolve().parent.parent / "tests"
DEFAULT_S = 2.0
WEIGHTS = {
    "test_panel_v013_states": 150, "test_v014_core_robust": 91, "test_v013_proxy_contract": 86,
    "test_release_scrub": 42, "test_bootstrap_install": 28, "test_release_update": 22,
    "test_lan_share": 18, "test_oem_drills": 11, "test_v013_image_watchdog": 9,
    "test_state_cache": 8, "test_closed_loop_session": 8, "test_openai_api": 7,
    "test_controller_security": 7, "test_v014_fixtures": 7,
}


def test_modules() -> list[str]:
    """Dotted names of every module `discover -s tests` loads: test*.py in tests/ and in its package
    subdirectories (an __init__.py at every level), e.g. adapters.test_long_append."""
    out = []
    for p in TESTS.rglob("test*.py"):
        rel = p.relative_to(TESTS)
        pkgs = [TESTS.joinpath(*rel.parts[:i]) for i in range(1, len(rel.parts))]
        if all((d / "__init__.py").is_file() for d in pkgs):
            out.append(".".join(rel.with_suffix("").parts))
    return sorted(out)


def shards(n: int) -> list[list[str]]:
    if n < 1:
        raise SystemExit("N must be >= 1")
    mods = test_modules()
    if not mods:
        raise SystemExit(f"no test modules under {TESTS}")
    out: list[list[str]] = [[] for _ in range(n)]
    load = [0.0] * n
    for m in sorted(mods, key=lambda m: (-WEIGHTS.get(m, DEFAULT_S), m)):
        i = min(range(n), key=lambda i: (load[i], i))
        out[i].append(m)
        load[i] += WEIGHTS.get(m, DEFAULT_S)
    flat = sorted(m for s in out for m in s)
    if flat != mods:
        raise SystemExit("shard coverage check failed")
    return out


def run(mods: list[str]) -> int:
    # as under `python3 -m unittest discover -s tests` from lab/: lab/ first (not tools/), then tests/
    sys.path[0] = str(TESTS.parent)
    sys.path.insert(0, str(TESTS))
    loader = unittest.TestLoader()
    suite = unittest.TestSuite(loader.loadTestsFromModule(importlib.import_module(m)) for m in mods)
    if loader.errors:
        for e in loader.errors:
            print(e, file=sys.stderr)
        return 1
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    return 0 if result.wasSuccessful() else 1


def main() -> None:
    a = sys.argv[1:]
    if len(a) == 1:
        for k, s in enumerate(shards(int(a[0]))):
            print(f"shard {k}: {len(s)} modules, ~{sum(WEIGHTS.get(m, DEFAULT_S) for m in s):.0f} s")
    elif len(a) == 3 and a[0] in ("list", "run"):
        n, k = int(a[1]), int(a[2])
        if not 0 <= k < n:
            raise SystemExit("K must be in 0..N-1")
        mods = shards(n)[k]
        if a[0] == "list":
            print("\n".join(mods))
        else:
            sys.exit(run(mods))
    else:
        raise SystemExit(__doc__)


if __name__ == "__main__":
    main()
