# oxa-export assets (architect-owned, copied)

Copied unchanged from pi `packages/coding-agent/src/core/export-html/` @47e9d74 (MIT,
Copyright (c) 2025 Mario Zechner); `template.css` and `template.js` carry a one-line origin
comment, `template.html` does not (it must start with the doctype).

| File | Origin | Licence |
|---|---|---|
| `template.html`, `template.css`, `template.js` | pi | MIT |
| `vendor/marked.min.js` | marked v18.0.11 | MIT (header kept) |
| `vendor/marked-cjk-friendly.min.js` | marked-cjk-friendly v0.1.2 | MIT (header kept) |
| `vendor/highlight.min.js` | Highlight.js v11.9.0 | BSD-3-Clause (header kept) |

`template.js` reads pi's session JSON (header + entries, pi v3 names). E12 must feed it exactly
that shape; adapting `template.js` to oxa entry types (`shown`, `mode_change`, `usage`) is a
note to the architect, not an edit by the task.
