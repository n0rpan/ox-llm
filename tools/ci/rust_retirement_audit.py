#!/usr/bin/env python3
"""Source-only O416 inventory; candidates require a keep/drop/port decision."""
import collections
import json
from pathlib import Path
import re
import subprocess
import sys
import tomllib


WORKSPACES = ("oxa", "engine", "oxc-rs")


def main():
    root = Path(__file__).resolve().parents[2]
    destination = Path(sys.argv[1]).resolve()
    if destination == root or root in destination.parents:
        raise SystemExit("audit artifacts must be outside the checkout")
    destination.mkdir(parents=True, exist_ok=True)
    tracked = subprocess.check_output(
        ["git", "ls-files", "-z"], cwd=root
    ).decode().split("\0")
    scopes = tuple(f"{workspace}/" for workspace in WORKSPACES)
    sources = {
        name: (root / name).read_text()
        for name in tracked
        if name.endswith(".rs") and name.startswith(scopes)
    }
    words = collections.Counter(
        word for text in sources.values() for word in re.findall(r"\b\w+\b", text)
    )
    public = []
    allowances = []
    declaration = re.compile(
        r"\bpub(?:\s*\([^)]*\))?\s+(?:(?:async|unsafe|const)\s+)*"
        r"(?:fn|struct|enum|trait|type|const|static|mod)\s+(\w+)"
    )
    for name, text in sources.items():
        for number, line in enumerate(text.splitlines(), 1):
            if re.search(r"\b(?:allow|expect)\s*\([^)]*\bdead_code\b", line):
                allowances.append({"path": name, "line": number, "source": line.strip()})
            for match in declaration.finditer(line):
                item = match.group(1)
                public.append({
                    "path": name, "line": number, "item": item,
                    "tracked_rust_mentions": words[item],
                    "single_mention_candidate": words[item] == 1,
                })
    manifests = {}
    for name in tracked:
        if name.endswith("Cargo.toml") and name.startswith(scopes):
            data = tomllib.loads((root / name).read_text())
            manifests[name] = {
                "package": data.get("package", {}).get("name"),
                "features": data.get("features", {}),
                "machete_exceptions": data.get("package", {}).get("metadata", {}).get("cargo-machete"),
            }
    metadata = {}
    for scope in WORKSPACES:
        result = subprocess.run(
            ["cargo", "metadata", "--no-deps", "--locked", "--format-version", "1"],
            cwd=root / scope, capture_output=True, text=True,
        )
        (destination / f"{scope}-metadata-stderr.txt").write_text(result.stderr)
        result.check_returncode()
        metadata[scope] = json.loads(result.stdout)
    report = {
        "head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip(),
        "qualification": "SOURCE CANDIDATES ONLY: macros, cfg, downstream users and generated code need review; no deletion approval",
        "metadata": metadata,
        "manifests_and_declared_features": manifests,
        "dead_code_allowances": allowances,
        "public_items": public,
    }
    (destination / "source-inventory.json").write_text(json.dumps(report, indent=2) + "\n")
    print(f"source files={len(sources)} public declarations={len(public)} dead-code allowances={len(allowances)}")
    print("Declared features and metadata are inventory, not proof that a feature is unused.")
    for scope in WORKSPACES:
        result = subprocess.run(
            ["cargo", "machete"], cwd=root / scope,
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
        )
        (destination / f"{scope}-machete.txt").write_text(result.stdout)
        (destination / f"{scope}-machete-exit.txt").write_text(f"{result.returncode}\n")
        # Pinned cargo-machete 0.9.1 can skip a failed manifest and still exit 0 or 1.
        manifest_error = any(line.startswith("error when handling ") for line in result.stdout.splitlines())
        if result.returncode not in (0, 1) or manifest_error:
            raise SystemExit(f"{scope}: incomplete cargo-machete scan; see {scope}-machete.txt")


if __name__ == "__main__":
    main()
