#!/usr/bin/env python3
"""Split the lab unit suite (tests/**/test*.py, as discover finds them) into N balanced CI shards and run one.

usage: ci_test_shard.py N              print every shard with its estimated seconds
       ci_test_shard.py list N K       print the test modules of shard K (0-based)
       ci_test_shard.py run N K        run shard K verbosely (call it through
                                       LAB_TEST_SHARD=K/N tools/run_lab_tests.sh for the isolated HOME)

Shards are whole test modules, loaded exactly as `unittest discover -s tests` loads them (tests/ on
sys.path, loadTestsFromModule, the load_tests protocol), so tests a module imports from a helper
module (e.g. openai_api_stack.py) still run with it. Balancing: longest-processing-time greedy over
per-module seconds measured on the hosted x86_64 runner (checks run 43759275937, 2026-11-15; modules of
3 s or more; the 2026-11-13 weights had drifted, e.g. test_panel_v013_states 150 s -> under 3 s, the cloud
suites unlisted); unlisted modules weigh DEFAULT_S. `run` prints each module's measured seconds and the
shard total ("lab shard K/N: ..."), so the weights can be refreshed from any CI log. Every module lands
in exactly one shard (checked on every call), so a new test file is never silently skipped; the weights only affect balance, never coverage.
"""
import importlib
import sys
import time
import unittest
from pathlib import Path

TESTS = Path(__file__).resolve().parent.parent / "tests"
DEFAULT_S = 0.5  # the ~150 modules under 3 s took ~70 s together
WEIGHTS = {
    "test_v014_core_robust": 87, "test_v013_proxy_contract": 86, "test_cloud_remote": 43,
    "test_cloud_openai": 41, "test_cloud_anthropic": 25, "test_release_update": 23,
    "test_interim_sessions": 20, "test_lan_share": 18, "test_cloud_conformance": 16,
    "test_unload_after_reply": 12, "test_state_cache": 9, "test_v013_image_watchdog": 9,
    "test_openai_api": 7, "test_controller_security": 7, "test_release_scrub": 7,
    "test_panel_model": 7, "test_explore1_observer": 7, "test_image_slot": 7,
    "test_cloud_cancel": 7, "test_controller_behaviour": 6, "test_oxc_ui_data": 6,
    "test_cache_perfect_p1": 6, "test_controller_recovery": 5, "test_v013_modes": 5,
    "test_cloud_review_security": 5, "test_openai_conformance": 5, "test_v020_cancel_settle": 5,
    "test_cloud_prices": 5, "test_studio_crop_edit": 5, "test_flash_transport": 4,
    "test_cloud_residuals": 4, "test_run_synthetic_screen": 4, "test_v013_image_slot": 4,
    "test_studio_handoff": 4, "test_v020_preempt": 3, "test_cloud_only_no_engine": 3,
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


class _TimedResult(unittest.TextTestResult):
    """Verbose result that adds up wall seconds per test module (setUpClass time lands on the first test)."""
    seconds: dict[str, float] = {}
    _last = 0.0

    def startTest(self, test):
        type(self)._last = time.monotonic()
        super().startTest(test)

    def stopTest(self, test):
        super().stopTest(test)
        mod = type(test).__module__
        self.seconds[mod] = self.seconds.get(mod, 0.0) + time.monotonic() - self._last


def timing_report(label: str, seconds: dict[str, float], total: float) -> str:
    lines = [f"lab shard {label}: {total:.1f} s measured, {len(seconds)} modules timed"]
    lines += [f"  {s:7.1f} s  {m}" for m, s in sorted(seconds.items(), key=lambda x: (-x[1], x[0]))]
    return "\n".join(lines)


def run(mods: list[str], label: str = "") -> int:
    # as under `python3 -m unittest discover -s tests` from oxc/: oxc/ first (not tools/), then tests/
    sys.path[0] = str(TESTS.parent)
    sys.path.insert(0, str(TESTS))
    loader = unittest.TestLoader()
    suite = unittest.TestSuite(loader.loadTestsFromModule(importlib.import_module(m)) for m in mods)
    if loader.errors:
        for e in loader.errors:
            print(e, file=sys.stderr)
        return 1
    _TimedResult.seconds = {}
    t0 = time.monotonic()
    result = unittest.TextTestRunner(verbosity=2, resultclass=_TimedResult).run(suite)
    est = sum(WEIGHTS.get(m, DEFAULT_S) for m in mods)
    print(timing_report(f"{label} (estimated ~{est:.0f} s)", _TimedResult.seconds, time.monotonic() - t0),
          file=sys.stderr, flush=True)
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
            sys.exit(run(mods, f"{k}/{n}"))
    else:
        raise SystemExit(__doc__)


if __name__ == "__main__":
    main()
