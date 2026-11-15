# Findings: fuzzing oxa (Task 61, part A)

The fix is in a separate commit (`fix(oxa): ...`) so the test can be reviewed without it. The
reproducer uses only synthetic text; no real terminal is involved.

## OXA-1 — Terminal control sequences in untrusted model output (high)

**Class:** terminal escape injection from model/tool text (not a sandbox escape). A model, an
MCP server or a tool result can contain raw C0/C1 bytes. The TUI renders assistant text,
thinking and tool output through `oxa_tui::render_markdown`, whose contract is "printable text
plus SGR/OSC 8". The renderer copied the bytes through: `hello \x1b[2Jworld` reached the
terminal as a clear-screen CSI, and an autolink such as
`<https://example.invalid/\x1b]52;c;...\x07>` reached the OSC payload because the autolink path
did not filter control characters (the normal `[text](url)` path filtered only the URL).

**Reproducer:** `tests/smoke.rs::markdown_does_not_pass_control_characters_through`, over
`corpus/markdown-control.md`. Before the fix the rendered line contains the raw ESC/BEL; the
test fails. The `markdown` fuzz target asserts the same property for `Theme::plain()`.

**Root cause:** `render_markdown` normalises CR/TAB but never rejects the remaining C0/C1
characters, and `push_link` interpolates an autolink URL verbatim. `paint()` only adds theme
styles; it does not sanitise literal text.

**Fix:** in `render_markdown`, after the line-ending normalisation, drop every control character
except `\n` (the block parser still needs line breaks). Theme-generated SGR/OSC is added after
this point, so links and highlighting keep working. This is a boundary check for all callers,
not a per-view patch.

**Threat model note:** the payload here is a test marker, not a weaponised sequence. The class
matters because generated text is untrusted and a terminal may implement clipboard/title/
notification OSC commands.

## Coverage, no additional crash found

3,000 libFuzzer runs per target (SSE, reply assembly, repair, Markdown, session, MCP SSE, RPC,
paths) found no panic outside OXA-1. The SSE target also checks split invariance at every byte
boundary; the session/MCP/RPC targets only require that malformed input becomes an error, never
a panic. The smoke test keeps those checks in normal CI.
