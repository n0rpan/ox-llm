# oxa-codemode

`codemode` (QuickJS scripts that call tools; only the script's output reaches the model) and
`tool_search` / `call_tool` (the W3-safe form of pi's deferred tools).

| Module | Task | Provides |
|---|---|---|
| `hooks.rs` | architect | `CodemodeHooks` (`impl Hooks`, implemented), `Shared`, `Catalog`, `Namespaces`, `Namespace`, `CodemodeConfig` |
| `tool.rs` | E6 | `CodemodeTool` (`impl Tool`), `ScriptOptions`, `parse_options_line`, `ScriptOutcome`, `format_outcome` |
| `api.rs` | E6 | `install_globals`, `ScriptSink`, `js_identifier`, `ts_declaration`, `load_store` |
| `pool.rs` | E6 | `WorkerPool` |
| `search.rs` | F4 | `rank`, `format_hits`, `ToolSearchTool`, `CallToolTool` |

## Cross-task contracts

- **W3**: the three tool specs are constant texts and never list tools; the catalog is read at
  run time through `Shared::catalog`. The prefix sha must be identical before and after any
  search or MCP connect (F4 acceptance).
- **`call_tool` is frozen**: `{name: string, args: object}`. A deferred tool is callable only
  after a `tool_search` in this session matched it (`Shared::matched`).
- All tool calls from scripts and from `call_tool` go through
  `Host::call(HostAction::CallTool { name, args })` -> `{text, isError}`.
- `CodemodeHooks::tools()` calls `CodemodeTool::new`, `ToolSearchTool::new`, `CallToolTool::new`
  (E6 / F4 bodies). Until F4 lands, enable only `codemode` in tests.
- Adaptations: JSON arguments `{"code": ..}`; no wall time in the result (W4); `image()` and
  `models.*` throw `unsupported_in_oxa`.
- The store persists as a `codemode-store` custom entry (`HostAction::AppendEntry`), reloaded
  on `attach` from the active branch, so it follows `/tree`, resume and fork.

## Acceptance

- E6: codemode.md examples; options line only on line 1; output cap + temp file; worker cap;
  `Promise.allSettled` over tools.
- F4: ranking incl. `$ref` properties; deferred tool callable only after a match; prefix sha
  unchanged across search.

## Used by

`oxa-cli` (wires `CodemodeHooks` into `Deps::hooks` with a catalog over built-ins, extensions
and `oxa-mcp`), `oxa-mcp` exposure modes (by convention only: no code dependency).
