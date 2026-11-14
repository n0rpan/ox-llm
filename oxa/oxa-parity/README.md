# oxa-parity (test-only)

The PARITY suite (plan sec. 6.3). `PARITY.md` at the workspace root is the row list; `tests/<area>.rs` holds one test per row. Wave G (G1-G12) fills the tests; W0 provides the helpers in `src/lib.rs` and the empty area files.

| File | Task |
|---|---|
| `tests/tools.rs` | G1 |
| `tests/cli.rs` | G2 |
| `tests/sessions.rs` | G3 |
| `tests/tree.rs` | G4 |
| `tests/compaction.rs` | G5 |
| `tests/json.rs` | G6 |
| `tests/rpc.rs` | G7 |
| `tests/tui.rs` | G8 |
| `tests/keys_settings.rs` | G9 |
| `tests/resources.rs` | G10 |
| `tests/mcp.rs` | G11 |
| `tests/extensions.rs` | G12 |

Ship gate: 100% of P and A rows green, no unclassified row.

## The pty harness (`oxa_parity::pty`, wave H)

Interactive rows (keybindings, the tree navigator, the session picker, display settings, the
escape sequences the TUI writes) drive the real `oxa` in a real pseudo-terminal and assert on the
screen as a user sees it. `tests/pty_smoke.rs` proves the harness itself.

* `PtySession::spawn(&env, &server.url(), &args, (rows, cols))` starts `oxa` on a pty with the
  `xterm` profile; `spawn_with(.., PtyOpts { size, profile, env, scrollback })` picks another
  `TermProfile` (`xterm`, `kitty`, `tmux`, `silent`, or a custom one). The profile sets `TERM`
  and friends and answers the binary's queries (device attributes, kitty keyboard flags, cell
  size, cursor position, OSC 10/11 colours), so capability rows run both ways.
* `wait_ready()` before typing (raw mode on and a first frame settled). Then `send_text`,
  `send_key("ctrl+shift+up")` (pi key ids; kitty CSI-u while the app has the kitty protocol
  pushed on a kitty profile, legacy otherwise), `send_key_as(id, KeyEncoding::Legacy |
  Kitty(flags))`, `paste(text)` (bracketed when the app enabled it), `resize(rows, cols)`,
  `send_signal(sig)`, `send_bytes`.
* Assert on `screen()` (`text()`, `row(n)`, `rows()`, `contains`, `find_row`, `cursor()`,
  `cell(r, c)` / `cell_of(needle)` with bold/dim/italic/underline/inverse and fg/bg colours,
  `modes()`, `title()`, `scrollback_text()`), and on the raw output (`raw()`, `mark()` +
  `raw_since(mark)`, `wait_for_raw(mark, bytes)`) for escape-sequence rows.
* Never sleep as an assertion: `wait_for(|s| ..., timeout)`, `wait_for_text`,
  `wait_for_text_gone`, `wait_stable(quiet_ms, timeout)`, `wait_exit(timeout)`. A failed wait
  returns a `ScreenDump` whose `Debug` is the whole screen, the modes and the raw-log tail, so
  `.unwrap()` makes a red test diagnosable. `pty::DEFAULT_TIMEOUT` is 30 s.
* Drop kills the child's process group and joins the reader. Keep the `PtySession` declared
  before the `FakeServer` and the `TestEnv` (struct fields drop in order) so the child goes first.

```rust
use oxa_fake::{parse_script, FakeOpts, FakeServer, TestEnv};
use oxa_parity::pty::{PtyOpts, PtySession, TermProfile, DEFAULT_TIMEOUT};

#[test]
fn key_001() {
    let env = TestEnv::new();
    let server = FakeServer::start(parse_script(r#"{"text":"hi"}"#).unwrap(), FakeOpts::default()).unwrap();
    let mut pty = PtySession::spawn_with(&env, &server.url(), &[], PtyOpts {
        profile: TermProfile::kitty(),
        ..PtyOpts::default()
    });
    pty.wait_ready().unwrap();
    pty.send_text("hello");
    pty.send_key("enter");
    pty.wait_for_text("hi").unwrap();
    let s = pty.screen();
    assert!(s.cell_of("hello").is_some_and(|c| !c.inverse));
    pty.send_key("ctrl+d");
    assert!(pty.wait_exit(DEFAULT_TIMEOUT).unwrap().success());
}
```

The two product bugs the harness found first (the boot preload left the TUI busy, so the first
prompt became steering; the exit left the cursor hidden) are fixed in wave H; their repros are the
last tests of `tests/pty_smoke.rs`. The boot preload runs in pty tests as it does for users.
