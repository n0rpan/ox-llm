# oxa-export

HTML export of sessions with pi's export template (copied under MIT; see `assets/README.md`).
Used by `/export` (oxa-interactive), `--export` (oxa-cli) and RPC `export_html` (oxa-rpc).

| Module | Owner | Scope |
|---|---|---|
| `lib.rs`, `template.rs`, `assets/*` | architect (frozen) | crate surface, copied template |
| `html.rs` | E12 | `export_html`, `session_data_json`, `theme_css`, `base64_encode`, `write_export` |
| `ansi.rs` | E12 | `ansi_to_html`, `ansi_lines_to_html`, `strip_ansi_escapes`, `escape_html` |
| `names.rs` | E12 | `default_output_name`, `format_for` |

## Contracts
- `export_html` is pure and deterministic; it embeds the whole tree (all branches) as pi v3
  JSON, base64 in `{{SESSION_DATA}}`. Entry JSON comes from `oxa_session::encode_entry`.
- The crate does not depend on `oxa-tui`: the caller resolves theme colours into `ExportTheme`
  and pre-renders custom tool output into `RenderedTool` (via `Hooks::render` +
  `ansi_lines_to_html`).
- JSONL export is a plain copy of the session file by the caller; `format_for` only decides.
- No uploads. Nothing here touches the wire (W1-W6 not involved).

## Consumes
`oxa_proto::{Entry, OxaError, Value}`, `oxa_session::{SessionHeader, encode_entry}`.

## Tests
`tests/e12_*.rs`: fixture session -> HTML golden (placeholders all replaced, session data
round-trips through base64), ANSI fixtures recorded from pi, name derivation.
