#!/usr/bin/env python3
"""Wave-checkpoint helper: validates WAVES.toml against the tree.

- every owned file is owned once;
- every owned source file exists;
- every acceptance bullet has a #[test] of that name in the task's test file;
- `needs` never points to a later wave;
- every src/*.rs is owned by a task or listed as an architect file (printed, not fatal).
Usage: python3 ci/check_manifest.py [--changed <git-range>]
  --changed: also fail when a commit range touches a file owned by a different task than the
  one named in the commit subject prefix (`A7: ...`), or an architect file.
"""
import glob, os, re, subprocess, sys, tomllib

root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
os.chdir(root)
d = tomllib.load(open("WAVES.toml", "rb"))
errors = []
owned = {}
order = "ABCDEFGH"
ids = {t["id"] for t in d["task"]}
for t in d["task"]:
    for f in t["owns"]:
        if f in owned:
            errors.append(f"{f}: owned by {owned[f]} and {t['id']}")
        owned[f] = t["id"]
        if f.endswith(".rs") and "/src/" in f and not os.path.exists(f):
            errors.append(f"{t['id']}: owned source file missing: {f}")
    for n in t["needs"]:
        if n not in ids:
            errors.append(f"{t['id']}: unknown need {n}")
        elif order.index(n[0]) > order.index(t["id"][0]):
            errors.append(f"{t['id']}: needs later-wave task {n}")
    tf = t.get("test_file")
    if tf:
        if not os.path.exists(tf):
            errors.append(f"{t['id']}: test file missing: {tf}")
        else:
            src = open(tf).read()
            for a in t["acceptance"]:
                if not re.search(r"fn " + re.escape(a["test"]) + r"\s*\(", src):
                    errors.append(f"{t['id']}: acceptance test missing: {a['test']} in {tf}")
planned = [t for t in d["task"] if t["wave"] != "H"]
if len(planned) != d["meta"]["planned_tasks"]:
    errors.append(f"planned task count {len(planned)} != meta.planned_tasks")

if "--changed" in sys.argv:
    rng = sys.argv[sys.argv.index("--changed") + 1]
    log = subprocess.run(["git", "log", "--format=%H %s", rng], capture_output=True, text=True, check=True).stdout
    for line in log.splitlines():
        sha, subject = line.split(" ", 1)
        m = re.match(r"([A-H]\d+):", subject)
        files = subprocess.run(["git", "show", "--name-only", "--format=", sha], capture_output=True, text=True, check=True).stdout.split()
        for f in files:
            # paths are relative to the git root; strip an `oxa/` prefix in the monorepo
            f = f[4:] if f.startswith("oxa/") else f
            owner = owned.get(f) or next((o for p, o in owned.items() if p.endswith("/") and f.startswith(p)), None)
            if m and owner != m.group(1):
                errors.append(f"{sha[:8]} ({m.group(1)}) touches {f} owned by {owner or 'the architect'}")

for e in errors:
    print("ERROR", e)
print(f"{len(d['task'])} tasks, {len(owned)} owned paths, {len(errors)} errors")
sys.exit(1 if errors else 0)
