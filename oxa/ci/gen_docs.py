#!/usr/bin/env python3
"""F11: generate the PARITY-derived doc pages from PARITY.md (stdlib only, deterministic).

Writes:
  docs/differences.md            the A and O rows of every PARITY section
  docs/pages/engine-support.md   every ENG row, and every A/O row outside ENG whose note names O-144

Usage: python3 ci/gen_docs.py            (write both files)
       python3 ci/gen_docs.py --check    (exit 1 when either file differs from the generated text)

Output rewrites (the behaviour text describes pi's surface; oxa uses its own names):
  PI_ -> OXA_ (env prefix), ~/.pi -> ~/.oxa, .pi/ -> .oxa/, a leading command "`pi " -> "`oxa "
  internal ids are removed from the text (not from the id column, which is the row's key):
  decision ids (O-<digits>), the pi-parity tag [P], work-package ids (W3, H6, EG5, F12, X3 ...)
  and the ids of other rows cited in a note (ENG-010 ...). The first list below names the kinds.
"""
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PARITY = os.path.join(ROOT, "PARITY.md")
DIFF = os.path.join(ROOT, "docs", "differences.md")
ENGINE = os.path.join(ROOT, "docs", "pages", "engine-support.md")

SPLIT = re.compile(r"(?<!\\)\|")

# Internal ids that a user cannot use. Each pattern is removed from the note and behaviour text.
# ISO-8601 and SHA-256 are real names, not row ids: the lookahead keeps them.
INTERNAL_IDS = [
    re.compile(r"\s*\[P\]"),                                     # pi-parity tag
    re.compile(r"\s*\(\s*O-\d+\)"),                             # decision id in brackets
    re.compile(r",\s*\bO-\d+"),
    re.compile(r"\bO-\d+[:;]?\s*"),
    re.compile(r"\b(?:W|H|F|EG|X)\d+(?:-(?:W|H|F|EG|X)\d+)?\b"),  # work-package ids, ranges
    re.compile(r"\b(?!ISO-|SHA-|UTF-|HTTP-)[A-Z]{2,6}-\d{2,4}[a-z]?\b"),  # row ids cited in a note
]


def tidy(text):
    """Close the gaps that removing an id leaves: empty brackets, stray punctuation."""
    text = re.sub(r"\(\s*\)|\[\s*\]", "", text)
    text = re.sub(r"[;,:]\s*(?=\))", "", text)                 # "rebase,)" -> "rebase)"
    text = re.sub(r"([;,:])(?:\s*[;,:])+", r"\1", text)         # ";:" -> ";"
    text = re.sub(r"\s+([,;:)\]])", r"\1", text)              # no space before punctuation
    text = re.sub(r"\s+\.(?=\s|$)", ".", text)                  # "x ." -> "x." (not ".gitignore")
    text = re.sub(r"([(\[])\s+", r"\1", text)
    text = re.sub(r"\s*[+/]\s*(?=[;,)]|$)", "", text)           # a dangling "+" or "/"
    text = re.sub(r"^[;,:+]\s*", "", text)                       # a leading separator
    text = re.sub(r"[ \t]{2,}", " ", text)
    return text.strip()


def rewrite(text):
    text = re.sub(r"\bPI_", "OXA_", text)
    text = text.replace("~/.pi", "~/.oxa")
    text = text.replace(".pi/", ".oxa/")
    text = text.replace("`pi ", "`oxa ")
    for pattern in INTERNAL_IDS:
        text = pattern.sub("", text)
    return tidy(text)


def cell(text):
    """A table cell: the escaped pipes of PARITY.md stay escaped in the output."""
    return rewrite(text).replace("|", "\\|")


def parse():
    """(section title, [row dict]) pairs, in PARITY order. Rows: id, behaviour, verdict, note."""
    sections = []
    with open(PARITY, encoding="utf-8") as f:
        lines = f.read().splitlines()
    for line in lines:
        if line.startswith("## "):
            sections.append((line[3:].strip(), []))
            continue
        if not line.startswith("|") or not sections:
            continue
        body = line.strip()
        if body.endswith("|"):
            body = body[:-1]
        body = body[1:]
        cells = [c.replace("\\|", "|").strip() for c in SPLIT.split(body)]
        if len(cells) != 5 or not re.match(r"^[A-Z]+-\d+$", cells[0]):
            continue
        sections[-1][1].append(
            {"id": cells[0], "behaviour": cells[1], "verdict": cells[2], "note": cells[3]}
        )
    return sections


def table(header, rows):
    out = ["| " + " | ".join(header) + " |", "|" + "---|" * len(header)]
    for r in rows:
        out.append("| " + " | ".join(r) + " |")
    return out


def differences(sections):
    out = [
        "# Differences from pi 1.5.2",
        "",
        "oxa is a small local agent tool derived from the pi coding agent. This page lists where",
        "oxa differs: the rows that are adapted for the local engine, and the pi features that oxa",
        "does not have. Rows that behave as pi documents are not listed. The table is generated",
        "from `PARITY.md` by `ci/gen_docs.py`; do not edit it by hand.",
        "",
        "Each row has an id in the first column, of the form `AREA-NNN`. The id is the row's key in oxa's internal parity",
        "table, which lists each documented behaviour of the earlier agent with its verdict and the",
        "test that checks it. The ids are shown so that a row can be referred to; the text of the",
        "rows does not cite the internal work items.",
        "",
    ]
    for title, rows in sections:
        adapted = [r for r in rows if r["verdict"] == "A"]
        omitted = [r for r in rows if r["verdict"] == "O"]
        if not adapted and not omitted:
            continue
        out += ["## " + rewrite(title), ""]
        if adapted:
            out += ["### Adapted", ""]
            out += table(
                ["id", "behaviour", "how oxa differs"],
                [[r["id"], cell(r["behaviour"]), cell(r["note"])] for r in adapted],
            )
            out.append("")
        if omitted:
            out += ["### Not in oxa", ""]
            out += table(
                ["id", "behaviour", "reason"],
                [[r["id"], cell(r["behaviour"]), cell(r["note"])] for r in omitted],
            )
            out.append("")
    return "\n".join(out).rstrip("\n") + "\n"


def engine(sections):
    eng = []
    other = []
    for title, rows in sections:
        for r in rows:
            if r["id"].startswith("ENG-"):
                eng.append(r)
            elif r["verdict"] in ("A", "O") and "O-144" in r["note"]:
                other.append(r)
    out = [
        "# Engine support",
        "",
        "oxa talks to one engine slot: one model request is in flight at a time for the whole",
        "instance. A request that arrives while the slot is busy queues, and the screen shows its",
        "position. Subagents run one at a time, never in parallel, because they share the slot.",
        "The engine is text-only: image, audio and binary content never reach it, and MCP results",
        "of those kinds become a fixed placeholder line. Thinking can be turned on or off per",
        "request. Context window, compaction threshold and maximum output come from the engine,",
        "per model, as the engine reports its limits. The small Flash model has reduced features:",
        "a 16K context, no preserved thinking, and no subagent tool unless it is enabled for it.",
        "The table is generated from `PARITY.md` by `ci/gen_docs.py`. Each row has an id in the first",
        "column, of the form `AREA-NNN`: the row's key in oxa's internal parity table, shown so that a row can be referred",
        "to. The text of the rows does not cite the internal work items.",
        "",
        "## Engine rows",
        "",
    ]
    out += table(
        ["id", "behaviour", "note"],
        [[r["id"], cell(r["behaviour"]), cell(r["note"])] for r in eng],
    )
    out += ["", "## Other rows that depend on the one-slot engine", ""]
    out += table(
        ["id", "behaviour", "note"],
        [[r["id"], cell(r["behaviour"]), cell(r["note"])] for r in other],
    )
    return "\n".join(out).rstrip("\n") + "\n"


def main():
    check = "--check" in sys.argv[1:]
    sections = parse()
    outputs = {DIFF: differences(sections), ENGINE: engine(sections)}
    if check:
        bad = []
        for path, text in outputs.items():
            current = None
            if os.path.exists(path):
                with open(path, encoding="utf-8") as f:
                    current = f.read()
            if current != text:
                bad.append(os.path.relpath(path, ROOT))
        if bad:
            print("out of date: " + ", ".join(bad) + " (run: python3 ci/gen_docs.py)")
            return 1
        print("docs generated from PARITY.md are in sync")
        return 0
    os.makedirs(os.path.dirname(ENGINE), exist_ok=True)
    for path, text in outputs.items():
        with open(path, "w", encoding="utf-8", newline="\n") as f:
            f.write(text)
        print("wrote " + os.path.relpath(path, ROOT))
    return 0


if __name__ == "__main__":
    sys.exit(main())
