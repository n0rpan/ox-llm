#!/usr/bin/env python3
"""Render the pending Markdown fragments for the GitHub Release notes."""

import argparse
import re
import sys
from pathlib import Path


FRAGMENT_NAME = re.compile(r"[a-z0-9]+(?:-[a-z0-9]+)*\.md")
BASELINE = "legacy-unreleased.md"


def render(directory: Path) -> str:
    entries = []
    baseline = ""
    for path in sorted(directory.iterdir(), key=lambda p: p.name):
        if path.name == "README.md":
            continue
        if not FRAGMENT_NAME.fullmatch(path.name) or path.is_symlink() or not path.is_file():
            raise ValueError(f"invalid changelog fragment: {path.name}")
        text = path.read_text(encoding="utf-8").strip("\n")
        if not text.strip():
            raise ValueError(f"empty changelog fragment: {path.name}")
        if path.name == BASELINE:
            baseline = text
        else:
            entries.append(text)

    if not entries and not baseline:
        return ""
    sections = ["## Changelog"]
    if entries:
        sections.append("### Pull requests")
        sections.extend(entries)
    if baseline:
        sections.append(baseline)
    return "\n\n".join(sections) + "\n"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path, help="path to the changelog.d directory")
    args = parser.parse_args()
    try:
        sys.stdout.write(render(args.directory))
    except (OSError, ValueError) as exc:
        parser.exit(1, f"changelog: {exc}\n")


if __name__ == "__main__":
    main()
