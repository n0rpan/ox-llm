# Example extensions (ports from pi)

Twelve of pi's example extensions, ported to oxa's JavaScript extension API. They are the
acceptance tests of the extension surface: if a port needs an API change, that is a note to the
architect, not an edit of `oxa-ext`.

Source: `earendil-works/pi` @47e9d74, `packages/coding-agent/examples/extensions/` (MIT; see
`NOTICE`). Each ported file starts with `// ported from pi <path>`.

| Port (directory, entry `index.js`) | Task | pi source |
|---|---|---|
| `plan-mode/` | F9 | `examples/extensions/plan-mode/{index.ts,utils.ts}` |
| `permission-gate/` | F9 | `examples/extensions/permission-gate.ts` |
| `custom-compaction/` | F9 | `examples/extensions/custom-compaction.ts` |
| `todo/` | F9 | `examples/extensions/todo.ts` |
| `status-line/` | F9 | `examples/extensions/status-line.ts` |
| `tools/` | F10 | `examples/extensions/tools.ts` |
| `commands/` | F10 | `examples/extensions/commands.ts` |
| `input-transform/` | F10 | `examples/extensions/input-transform.ts` |
| `question/` | F10 | `examples/extensions/question.ts` |
| `git-checkpoint/` | F10 | `examples/extensions/git-checkpoint.ts` |
| `trigger-compact/` | F10 | `examples/extensions/trigger-compact.ts` |
| `protected-paths/` | F10 | `examples/extensions/protected-paths.ts` |

## Porting rules

- One directory per port, entry `index.js` (ES module, `export default function (oxa) { .. }`),
  plus a `session.fake.json` script for the fake engine and nothing else.
- Mechanical changes only: `pi` -> `oxa`, types removed, `@earendil-works/pi-tui` imports ->
  `oxa:tui`, Node APIs -> host functions (`oxa.exec`, `ctx.fs`, `ctx.fetch`). No npm imports.
- A port that changes context must stay append-safe (W-guard). `custom-compaction` returns its
  summary from `session_before_compact`; the summary still lands as the W5 head.
- `plan-mode` is kept as a port even though oxa has a built-in `plan` permission mode: it tests
  `setActiveTools`, widgets and `before_agent_start`.
- pi behaviour that oxa omits (`registerProvider`, virtual models) is not used by any of the
  twelve.

## Acceptance rule (F9, F10)

Each port loads without a load error (`ExtensionHost::load`) and passes its scripted session:
`oxa --mode json -e examples/extensions/<name>` against `oxa-fake` with the port's
`session.fake.json` produces the expected event sequence, and the fake's invariant checker
(W1-W6) stays green for the whole session. Tests live in
`oxa-parity/tests/extensions.rs` (G12) and in each task's own acceptance test.
