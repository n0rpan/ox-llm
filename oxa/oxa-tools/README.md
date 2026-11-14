# oxa-tools

The seven built-in tools and `shape` (W4). Depends on `oxa-proto` (the sandbox is reached through `ToolCtx`, not through `oxa-sandbox`). Used by `oxa-core`.

| Module | Task | Provides |
|---|---|---|
| `lib.rs` | architect (frozen) | `builtin_tools`, `spec_of`, `resolve_path` |
| `descriptions.rs` | architect (frozen; all tool text) | `ToolText` consts, `limits`, `errors` |
| `shape.rs` | A5 | `shape`, `strip_ansi` |
| `read.rs`, `write.rs` | A6 | `ReadTool`, `WriteTool` |
| `edit.rs`, `diff.rs` | A7 | `EditTool`, `apply_edits`, `unified_diff` |
| `grep.rs`, `find.rs`, `ls.rs` | A8 | `GrepTool`, `FindTool`, `LsTool` |
| `bash.rs` | A9 (with `oxa-sandbox`) | `BashTool` |

Contracts
- W4: deterministic bytes; sorted; no timings; no ANSI. The loop calls `shape` on every result; tools apply pi's own limits first.
- Tool text (names, descriptions, schemas, error texts) is only in `descriptions.rs`. Changing a word there changes the frozen prefix: architect only.
- `Tool::access` is pure and cheap and returns absolute normalised paths (`resolve_path`).
- Tests use temp directories from `oxa_fake::TestEnv`; no test touches the real home.
