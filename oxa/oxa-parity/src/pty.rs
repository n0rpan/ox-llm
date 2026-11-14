//! The pty harness: run the real `oxa` binary in a real pseudo-terminal, press keys, and assert
//! on the screen as a user sees it (Linux only).
//!
//! * [`PtySession::spawn`] opens a pty pair (`posix_openpt` + the slave opened with
//!   `O_CLOEXEC`, so a parallel test's child never inherits our fds), starts `oxa` with the slave
//!   as stdin/stdout/stderr in its own session (`setsid` + `TIOCSCTTY` in `pre_exec`; no fork of
//!   the multi-threaded test process outside `std::process::Command`), in `env.work()`, with only
//!   `env.envs(engine_url)` plus the [`TermProfile`]'s variables as its environment.
//! * A reader thread feeds every output byte to a `vt100` terminal emulator and to a raw log, and
//!   answers the terminal queries the binary sends (device attributes, kitty keyboard flags, cell
//!   size, cursor position, OSC 10/11 colours) the way the chosen [`TermProfile`] would.
//! * Input: [`PtySession::send_text`], [`PtySession::send_key`] (pi key ids, encoded as the
//!   terminal would: kitty CSI-u while the app has the kitty protocol pushed and the profile
//!   supports it, legacy otherwise, or forced with [`PtySession::send_key_as`]),
//!   [`PtySession::paste`], [`PtySession::resize`], [`PtySession::send_signal`].
//! * Waits are bounded and never sleep-as-assertion: [`PtySession::wait_for`] and friends return
//!   a [`ScreenDump`] (screen, cursor, modes, raw-log tail) on timeout; its `Debug` is the dump, so
//!   `.unwrap()` on a failed wait prints everything needed to diagnose the red test.
//! * Drop kills the child's process group (only that group), reaps it, stops and joins the
//!   reader and closes the master. Every test owns its own pty and `TestEnv`.
use crate::oxa_bin;
use oxa_fake::TestEnv;
use std::fmt;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Default bound of every wait: generous, because the machine runs niced parallel builds.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------------------------
// Terminal profiles
// ---------------------------------------------------------------------------------------------

/// What kind of terminal the harness pretends to be: its environment variables and how it
/// answers queries. Capability-dependent rows run once per profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermProfile {
    /// Short name for dumps (`xterm`, `kitty`, `tmux`, `silent`).
    pub name: &'static str,
    /// Environment the terminal sets (`TERM`, `COLORTERM`, `KITTY_WINDOW_ID`, `TMUX`, ...);
    /// overrides the `TestEnv`'s `TERM=dumb`.
    pub env: Vec<(String, String)>,
    /// Answer primary device attributes (`CSI c`) with this reply (`None` = stay silent).
    pub da_reply: Option<String>,
    /// The terminal supports the kitty keyboard protocol: it answers `CSI ? u` with the current
    /// flags, and `send_key` uses CSI-u while the app has flags pushed.
    pub kitty_keyboard: bool,
    /// With the kitty protocol's "report event types" flag on, follow every CSI-encoded key
    /// press with its release event, as a real kitty terminal does.
    pub kitty_release_events: bool,
    /// Cell size in pixels `(w, h)` answered to `CSI 16 t` (`None` = no answer).
    pub cell_px: Option<(u16, u16)>,
    /// Foreground / background colours answered to OSC 10 / OSC 11 queries, as 16-bit
    /// `rgb:rrrr/gggg/bbbb` components (`None` = no answer).
    pub fg_rgb: Option<(u16, u16, u16)>,
    pub bg_rgb: Option<(u16, u16, u16)>,
    /// Answer cursor position reports (`CSI 6 n`) from the emulated cursor.
    pub cursor_reports: bool,
}

impl TermProfile {
    /// A plain xterm-256color terminal: device attributes and cursor reports, no kitty
    /// protocol, dark colours.
    pub fn xterm() -> TermProfile {
        TermProfile {
            name: "xterm",
            env: vec![("TERM".into(), "xterm-256color".into())],
            da_reply: Some("\x1b[?62;22c".into()),
            kitty_keyboard: false,
            kitty_release_events: false,
            cell_px: None,
            fg_rgb: Some((0xd0d0, 0xd0d0, 0xd0d0)),
            bg_rgb: Some((0x1c1c, 0x1c1c, 0x1c1c)),
            cursor_reports: true,
        }
    }
    /// A kitty-protocol terminal (kitty itself: `TERM=xterm-kitty`, `KITTY_WINDOW_ID`,
    /// truecolor): confirms the keyboard protocol, reports cell size, sends key releases.
    pub fn kitty() -> TermProfile {
        TermProfile {
            name: "kitty",
            env: vec![
                ("TERM".into(), "xterm-kitty".into()),
                ("COLORTERM".into(), "truecolor".into()),
                ("KITTY_WINDOW_ID".into(), "1".into()),
            ],
            da_reply: Some("\x1b[?62;c".into()),
            kitty_keyboard: true,
            kitty_release_events: true,
            cell_px: Some((10, 20)),
            fg_rgb: Some((0xdddd, 0xdddd, 0xdddd)),
            bg_rgb: Some((0x0000, 0x0000, 0x0000)),
            cursor_reports: true,
        }
    }
    /// A tmux-like multiplexer (`TERM=tmux-256color`, `TMUX` set): device attributes only, no
    /// kitty protocol, no colour answers.
    pub fn tmux() -> TermProfile {
        TermProfile {
            name: "tmux",
            env: vec![
                ("TERM".into(), "tmux-256color".into()),
                ("TMUX".into(), "/tmp/tmux-0/default,1,0".into()),
            ],
            da_reply: Some("\x1b[?1;2c".into()),
            kitty_keyboard: false,
            kitty_release_events: false,
            cell_px: None,
            fg_rgb: None,
            bg_rgb: None,
            cursor_reports: true,
        }
    }
    /// A terminal that answers nothing (the binary's query timeouts are exercised).
    pub fn silent() -> TermProfile {
        TermProfile {
            name: "silent",
            env: vec![("TERM".into(), "xterm-256color".into())],
            da_reply: None,
            kitty_keyboard: false,
            kitty_release_events: false,
            cell_px: None,
            fg_rgb: None,
            bg_rgb: None,
            cursor_reports: false,
        }
    }
}

impl Default for TermProfile {
    fn default() -> Self {
        TermProfile::xterm()
    }
}

/// Options of [`PtySession::spawn_with`].
#[derive(Debug, Clone)]
pub struct PtyOpts {
    /// `(rows, cols)`.
    pub size: (u16, u16),
    pub profile: TermProfile,
    /// Extra environment, applied last (after the profile).
    pub env: Vec<(String, String)>,
    /// Scrollback rows kept by the emulator (see [`Screen::scrollback_text`]).
    pub scrollback: usize,
}

impl Default for PtyOpts {
    fn default() -> Self {
        PtyOpts {
            size: (24, 80),
            profile: TermProfile::xterm(),
            env: Vec::new(),
            scrollback: 1000,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Screen snapshots
// ---------------------------------------------------------------------------------------------

/// Colour of a cell (re-exported from the emulator).
pub use vt100::Color;

/// The attributes of one cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellInfo {
    pub text: String,
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

/// Terminal modes as the emulator saw the binary set them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Modes {
    pub alt_screen: bool,
    pub bracketed_paste: bool,
    pub cursor_hidden: bool,
    /// Mouse reporting of any kind is on.
    pub mouse: bool,
    pub focus_reporting: bool,
    /// Synchronized output (`CSI ? 2026 h`) is currently open.
    pub synchronized_output: bool,
    /// The kitty keyboard flags stack of the screen in use (`CSI > n u` pushes, `CSI < n u`
    /// pops); top is last. The main and the alternate screen each have their own stack.
    pub kitty_stack: Vec<u32>,
}

impl Modes {
    /// The kitty keyboard flags in effect (0 = legacy).
    pub fn kitty_flags(&self) -> u32 {
        self.kitty_stack.last().copied().unwrap_or(0)
    }
}

/// An owned snapshot of the screen as a user sees it.
#[derive(Clone)]
pub struct Screen {
    screen: vt100::Screen,
    modes: Modes,
    title: String,
    raw_len: usize,
    exited: bool,
}

impl Screen {
    /// `(rows, cols)`.
    pub fn size(&self) -> (u16, u16) {
        self.screen.size()
    }
    /// The visible screen, rows joined by `\n`, trailing spaces trimmed per row.
    pub fn text(&self) -> String {
        self.rows().join("\n")
    }
    /// Every visible row, trailing spaces trimmed.
    pub fn rows(&self) -> Vec<String> {
        let (_, cols) = self.screen.size();
        self.screen
            .rows(0, cols)
            .map(|r| r.trim_end().to_string())
            .collect()
    }
    /// Visible row `n` (0-based), trailing spaces trimmed; empty when out of range.
    pub fn row(&self, n: u16) -> String {
        self.rows().get(n as usize).cloned().unwrap_or_default()
    }
    /// `true` when `needle` occurs on one visible row.
    pub fn contains(&self, needle: &str) -> bool {
        self.rows().iter().any(|r| r.contains(needle))
    }
    /// The first visible row containing `needle`.
    pub fn find_row(&self, needle: &str) -> Option<u16> {
        self.rows()
            .iter()
            .position(|r| r.contains(needle))
            .map(|i| i as u16)
    }
    /// The column (0-based, in cells) where `needle` starts on row `row`.
    pub fn find_col(&self, row: u16, needle: &str) -> Option<u16> {
        let (_, cols) = self.screen.size();
        let mut line = String::new();
        let mut starts = Vec::new();
        for c in 0..cols {
            if let Some(cell) = self.screen.cell(row, c) {
                if cell.is_wide_continuation() {
                    continue;
                }
                starts.push((line.len(), c));
                let t = cell.contents();
                line.push_str(if t.is_empty() { " " } else { t });
            }
        }
        let at = line.find(needle)?;
        starts.iter().find(|(b, _)| *b == at).map(|(_, c)| *c)
    }
    /// Cursor `(row, col)`, 0-based.
    pub fn cursor(&self) -> (u16, u16) {
        self.screen.cursor_position()
    }
    pub fn cursor_visible(&self) -> bool {
        !self.screen.hide_cursor()
    }
    /// The cell at `(row, col)`, 0-based.
    pub fn cell(&self, row: u16, col: u16) -> Option<CellInfo> {
        self.screen.cell(row, col).map(|c| CellInfo {
            text: c.contents().to_string(),
            fg: c.fgcolor(),
            bg: c.bgcolor(),
            bold: c.bold(),
            dim: c.dim(),
            italic: c.italic(),
            underline: c.underline(),
            inverse: c.inverse(),
        })
    }
    /// The cell where `needle` starts on the first row that contains it.
    pub fn cell_of(&self, needle: &str) -> Option<CellInfo> {
        let row = self.find_row(needle)?;
        let col = self.find_col(row, needle)?;
        self.cell(row, col)
    }
    pub fn modes(&self) -> &Modes {
        &self.modes
    }
    /// The last window title set with OSC 0/2.
    pub fn title(&self) -> &str {
        &self.title
    }
    /// Length of the raw output log when the snapshot was taken (a mark for `raw_since`).
    pub fn raw_len(&self) -> usize {
        self.raw_len
    }
    /// The child has exited and the output is drained.
    pub fn exited(&self) -> bool {
        self.exited
    }
    /// Scrollback plus visible screen, oldest first (what scrolling up would show).
    pub fn scrollback_text(&self) -> String {
        let mut s = self.screen.clone();
        let (_, cols) = s.size();
        let mut out: Vec<String> = Vec::new();
        // Walk the scrollback from the oldest row: vt100 shows `offset` rows of scrollback at
        // the top of the visible grid.
        s.set_scrollback(usize::MAX);
        let depth = s.scrollback();
        for off in (1..=depth).rev() {
            s.set_scrollback(off);
            if let Some(first) = s.rows(0, cols).next() {
                out.push(first.trim_end().to_string());
            }
        }
        s.set_scrollback(0);
        out.extend(s.rows(0, cols).map(|r| r.trim_end().to_string()));
        out.join("\n")
    }
    /// The underlying emulator screen, for anything not covered above.
    pub fn vt(&self) -> &vt100::Screen {
        &self.screen
    }
}

impl fmt::Debug for Screen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (rows, cols) = self.size();
        let (cr, cc) = self.cursor();
        writeln!(
            f,
            "screen {rows}x{cols}, cursor ({cr},{cc}), modes {:?}",
            self.modes
        )?;
        writeln!(f, "+{}+", "-".repeat(cols as usize))?;
        for (i, r) in self.rows().iter().enumerate() {
            writeln!(f, "|{r:<w$}| {i}", w = cols as usize)?;
        }
        write!(f, "+{}+", "-".repeat(cols as usize))
    }
}

/// A failed wait: what was waited for, the screen, and the tail of the raw log.
pub struct ScreenDump {
    pub what: String,
    pub profile: &'static str,
    pub screen: Box<Screen>,
    /// The last bytes of the raw output (up to 4 KiB), escaped.
    pub raw_tail: String,
    pub exit: Option<ExitStatus>,
}

impl fmt::Display for ScreenDump {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "pty wait failed: {} (profile {})", self.what, self.profile)?;
        if let Some(s) = self.exit {
            writeln!(f, "child exited: {s}")?;
        }
        writeln!(f, "{:?}", self.screen)?;
        write!(f, "raw tail: {}", self.raw_tail)
    }
}

impl fmt::Debug for ScreenDump {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for ScreenDump {}

/// Bytes as a readable string: printable ASCII as is, ESC as `\e`, other controls as `\xNN`.
pub fn escape_bytes(bytes: &[u8]) -> String {
    let mut s = String::new();
    for &b in bytes {
        match b {
            0x1b => s.push_str("\\e"),
            b'\n' => s.push_str("\\n\n"),
            b'\r' => s.push_str("\\r"),
            0x20..=0x7e => s.push(b as char),
            _ => s.push_str(&format!("\\x{b:02x}")),
        }
    }
    s
}

// ---------------------------------------------------------------------------------------------
// Emulator state and the query answerer
// ---------------------------------------------------------------------------------------------

struct Answerer {
    profile: TermProfile,
    /// `modes.kitty_stack` is the main screen's kitty keyboard stack.
    modes: Modes,
    /// The alternate screen's own kitty keyboard stack: the kitty protocol keeps one stack per
    /// screen ("separate stacks for the main and alternate screens"; kitty and foot do), so a
    /// push made on the main screen does not apply in the alternate screen.
    alt_kitty_stack: Vec<u32>,
    title: String,
    replies: Vec<u8>,
}

fn first(p: &[&[u16]], i: usize) -> Option<u16> {
    p.get(i).and_then(|x| x.first().copied())
}

fn rgb_reply(code: &str, (r, g, b): (u16, u16, u16)) -> String {
    format!("\x1b]{code};rgb:{r:04x}/{g:04x}/{b:04x}\x1b\\")
}

impl Answerer {
    /// The kitty keyboard stack of the screen in use (main or alternate).
    fn kitty_stack(&mut self, screen: &vt100::Screen) -> &mut Vec<u32> {
        if screen.alternate_screen() {
            &mut self.alt_kitty_stack
        } else {
            &mut self.modes.kitty_stack
        }
    }
}

impl vt100::Callbacks for Answerer {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = String::from_utf8_lossy(title).into_owned();
    }
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        _i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        match (i1, c) {
            (None, 'c') if first(params, 0).unwrap_or(0) == 0 => {
                if let Some(r) = &self.profile.da_reply {
                    self.replies.extend_from_slice(r.as_bytes());
                }
            }
            (Some(b'>'), 'c') => {
                if self.profile.da_reply.is_some() {
                    self.replies.extend_from_slice(b"\x1b[>1;10;0c");
                }
            }
            (None, 'n') if first(params, 0) == Some(6) && self.profile.cursor_reports => {
                let (r, col) = screen.cursor_position();
                self.replies
                    .extend_from_slice(format!("\x1b[{};{}R", r + 1, col + 1).as_bytes());
            }
            (None, 't') if first(params, 0) == Some(16) => {
                if let Some((w, h)) = self.profile.cell_px {
                    self.replies
                        .extend_from_slice(format!("\x1b[6;{h};{w}t").as_bytes());
                }
            }
            (None, 't') if first(params, 0) == Some(14) => {
                if let Some((w, h)) = self.profile.cell_px {
                    let (rows, cols) = screen.size();
                    let (ph, pw) = (u32::from(h) * u32::from(rows), u32::from(w) * u32::from(cols));
                    self.replies
                        .extend_from_slice(format!("\x1b[4;{ph};{pw}t").as_bytes());
                }
            }
            (Some(b'?'), 'u') => {
                if self.profile.kitty_keyboard {
                    let flags = self.kitty_stack(screen).last().copied().unwrap_or(0);
                    self.replies
                        .extend_from_slice(format!("\x1b[?{flags}u").as_bytes());
                }
            }
            (Some(b'>'), 'u') => {
                if self.profile.kitty_keyboard {
                    self.kitty_stack(screen)
                        .push(u32::from(first(params, 0).unwrap_or(0)));
                }
            }
            (Some(b'<'), 'u') => {
                let n = first(params, 0).unwrap_or(1).max(1);
                for _ in 0..n {
                    self.kitty_stack(screen).pop();
                }
            }
            (Some(b'='), 'u') => {
                if self.profile.kitty_keyboard {
                    let flags = u32::from(first(params, 0).unwrap_or(0));
                    let mode = first(params, 1).unwrap_or(1);
                    let stack = self.kitty_stack(screen);
                    let cur = stack.last().copied().unwrap_or(0);
                    let new = match mode {
                        2 => cur | flags,
                        3 => cur & !flags,
                        _ => flags,
                    };
                    match stack.last_mut() {
                        Some(top) => *top = new,
                        None => stack.push(new),
                    }
                }
            }
            (Some(b'?'), 'h') | (Some(b'?'), 'l') => {
                let on = c == 'h';
                for p in params {
                    match p.first().copied() {
                        Some(1004) => self.modes.focus_reporting = on,
                        Some(2026) => self.modes.synchronized_output = on,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        match params {
            [b"10", b"?"] => {
                if let Some(c) = self.profile.fg_rgb {
                    self.replies.extend_from_slice(rgb_reply("10", c).as_bytes());
                }
            }
            [b"11", b"?"] => {
                if let Some(c) = self.profile.bg_rgb {
                    self.replies.extend_from_slice(rgb_reply("11", c).as_bytes());
                }
            }
            _ => {}
        }
    }
}

struct State {
    parser: vt100::Parser<Answerer>,
    /// The last complete frame while a synchronized update (`CSI ? 2026 h` .. `l`) is open:
    /// snapshots show it instead of the half-drawn screen. `None` = no update open.
    committed: Option<vt100::Screen>,
    raw: Vec<u8>,
    last_output: Instant,
    /// The reader saw end of file (every slave fd is closed).
    eof: bool,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    stop: AtomicBool,
    /// The write side of the master (a dup of the read side), shared by the test thread and
    /// the reader thread's query answers.
    writer: Mutex<File>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn write(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut w = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        w.write_all(bytes)?;
        w.flush()
    }
}

fn snapshot(st: &State) -> Screen {
    let cb = st.parser.callbacks();
    // Frame-atomic: an open synchronized update is not shown until it ends.
    let screen = match &st.committed {
        Some(frame) => frame.clone(),
        None => st.parser.screen().clone(),
    };
    let mut modes = cb.modes.clone();
    modes.alt_screen = screen.alternate_screen();
    if modes.alt_screen {
        modes.kitty_stack = cb.alt_kitty_stack.clone();
    }
    modes.bracketed_paste = screen.bracketed_paste();
    modes.cursor_hidden = screen.hide_cursor();
    modes.mouse = screen.mouse_protocol_mode() != vt100::MouseProtocolMode::None;
    Screen {
        screen,
        modes,
        title: cb.title.clone(),
        raw_len: st.raw.len(),
        exited: st.eof,
    }
}

/// Feed output to the parser in pieces that end at each `CSI ? 2026 l` (end of a synchronized
/// update), keeping the last complete frame in `committed` while an update is open.
fn feed_frames(st: &mut State, bytes: &[u8]) {
    const END: &[u8] = b"\x1b[?2026l";
    let mut rest = bytes;
    while !rest.is_empty() {
        let cut = rest
            .windows(END.len())
            .position(|w| w == END)
            .map(|i| i + END.len())
            .unwrap_or(rest.len());
        let (piece, tail) = rest.split_at(cut);
        let was_open = st.parser.callbacks().modes.synchronized_output;
        let before = (!was_open && st.committed.is_none()).then(|| st.parser.screen().clone());
        st.parser.process(piece);
        if st.parser.callbacks().modes.synchronized_output {
            if st.committed.is_none() {
                st.committed = before;
            }
        } else {
            st.committed = None;
        }
        rest = tail;
    }
}

/// The screen model without a process: feed `chunks` (output as it would arrive from the
/// application, chunk by chunk) through the same frame-atomic path as a live session and call
/// `seen` with the snapshot after each chunk (what a `wait_for` predicate would see then).
pub fn replay_output(rows: u16, cols: u16, chunks: &[&[u8]], mut seen: impl FnMut(&Screen)) {
    let answerer = Answerer {
        profile: TermProfile::silent(),
        modes: Modes::default(),
        alt_kitty_stack: Vec::new(),
        title: String::new(),
        replies: Vec::new(),
    };
    let mut st = State {
        parser: vt100::Parser::new_with_callbacks(rows, cols, 0, answerer),
        committed: None,
        raw: Vec::new(),
        last_output: Instant::now(),
        eof: false,
    };
    for chunk in chunks {
        st.raw.extend_from_slice(chunk);
        feed_frames(&mut st, chunk);
        seen(&snapshot(&st));
    }
}

fn reader_loop(shared: Arc<Shared>, mut master: File) {
    let fd = master.as_raw_fd();
    let mut buf = vec![0u8; 16384];
    loop {
        if shared.stop.load(Ordering::SeqCst) {
            break;
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd for the duration of the call.
        let r = unsafe { libc::poll(&mut pfd, 1, 50) };
        if r < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if r == 0 {
            continue;
        }
        match master.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let replies = {
                    let mut st = shared.lock();
                    st.raw.extend_from_slice(&buf[..n]);
                    feed_frames(&mut st, &buf[..n]);
                    st.last_output = Instant::now();
                    std::mem::take(&mut st.parser.callbacks_mut().replies)
                };
                if !replies.is_empty() {
                    let _ = shared.write(&replies);
                }
                shared.changed.notify_all();
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            // EIO: the last slave fd closed (the child and its group are gone).
            Err(_) => break,
        }
    }
    shared.lock().eof = true;
    shared.changed.notify_all();
}

// ---------------------------------------------------------------------------------------------
// Key encoding
// ---------------------------------------------------------------------------------------------

/// How [`PtySession::send_key_as`] encodes a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEncoding {
    /// What the profile's terminal would send now: kitty CSI-u with the flags the app pushed when
    /// the profile supports the protocol and the app enabled it, legacy otherwise.
    Auto,
    /// xterm legacy bytes (no kitty protocol).
    Legacy,
    /// Kitty CSI-u with the given flags (`7` = what oxa pushes, as pi: disambiguate, event
    /// types, alternate keys).
    Kitty(u32),
}

struct ParsedKey {
    ctrl: bool,
    shift: bool,
    alt: bool,
    sup: bool,
    name: String,
}

fn parse_key(id: &str) -> ParsedKey {
    let norm = oxa_proto::KeyId::parse(id).unwrap_or_else(|e| panic!("invalid key id {id:?}: {e:?}"));
    ParsedKey {
        ctrl: norm.has_mod("ctrl"),
        shift: norm.has_mod("shift"),
        alt: norm.has_mod("alt"),
        sup: norm.has_mod("super"),
        name: norm.key().to_string(),
    }
}

/// `CSI <n> ; <m> <fin>` with xterm's elision of default parameters.
fn csi_mod(n: u32, m: u32, fin: char, event: &str) -> Vec<u8> {
    if m == 1 && event.is_empty() {
        if n == 1 {
            format!("\x1b[{fin}")
        } else {
            format!("\x1b[{n}{fin}")
        }
    } else {
        format!("\x1b[{n};{m}{event}{fin}")
    }
    .into_bytes()
}

/// Letter-final keys: `(final, n)`; tilde keys: `('~', n)`.
fn named_csi(name: &str) -> Option<(char, u32)> {
    Some(match name {
        "up" => ('A', 1),
        "down" => ('B', 1),
        "right" => ('C', 1),
        "left" => ('D', 1),
        "home" => ('H', 1),
        "end" => ('F', 1),
        "f1" => ('P', 1),
        "f2" => ('Q', 1),
        "f3" => ('R', 1),
        "f4" => ('S', 1),
        "insert" => ('~', 2),
        "delete" => ('~', 3),
        "pageup" => ('~', 5),
        "pagedown" => ('~', 6),
        "f5" => ('~', 15),
        "f6" => ('~', 17),
        "f7" => ('~', 18),
        "f8" => ('~', 19),
        "f9" => ('~', 20),
        "f10" => ('~', 21),
        "f11" => ('~', 23),
        "f12" => ('~', 24),
        _ => return None,
    })
}

fn single_char(name: &str) -> Option<char> {
    let mut it = name.chars();
    let c = it.next()?;
    it.next().is_none().then_some(c)
}

/// The bytes a legacy xterm sends for the pi key id `id`, or `None` when such a terminal cannot
/// send that key (`shift+enter`, `ctrl+shift+a`, any `super+`): drive those with a kitty profile.
pub fn encode_legacy(id: &str) -> Option<Vec<u8>> {
    let k = parse_key(id);
    if k.sup {
        return None;
    }
    let m = 1 + u32::from(k.shift) + 2 * u32::from(k.alt) + 4 * u32::from(k.ctrl);
    if let Some((fin, n)) = named_csi(&k.name) {
        if m == 1 && matches!(fin, 'P' | 'Q' | 'R' | 'S') {
            return Some(vec![0x1b, b'O', fin as u8]);
        }
        return Some(csi_mod(n, m, fin, ""));
    }
    let base: Vec<u8> = match k.name.as_str() {
        "enter" if !k.ctrl && !k.shift => b"\r".to_vec(),
        "tab" if !k.ctrl && k.shift && !k.alt => return Some(b"\x1b[Z".to_vec()),
        "tab" if !k.ctrl && !k.shift => b"\t".to_vec(),
        "backspace" if !k.shift => {
            if k.ctrl {
                vec![0x08]
            } else {
                vec![0x7f]
            }
        }
        "escape" if !k.ctrl && !k.shift => vec![0x1b],
        "space" if !k.shift => {
            if k.ctrl {
                vec![0]
            } else {
                vec![b' ']
            }
        }
        "+" if !k.ctrl && !k.shift => b"+".to_vec(),
        name => {
            let c = single_char(name)?;
            if k.ctrl {
                if k.shift {
                    return None;
                }
                match c {
                    'a'..='z' => vec![c as u8 - b'a' + 1],
                    '[' => vec![0x1b],
                    '\\' => vec![0x1c],
                    ']' => vec![0x1d],
                    '^' => vec![0x1e],
                    '_' | '-' => vec![0x1f],
                    '@' | '2' => vec![0],
                    _ => return None,
                }
            } else if k.shift {
                // Only letters have a shifted form that is a key id here; `!` is its own id.
                if !c.is_alphabetic() {
                    return None;
                }
                c.to_uppercase().collect::<String>().into_bytes()
            } else {
                c.to_string().into_bytes()
            }
        }
    };
    if k.alt {
        let mut out = vec![0x1b];
        out.extend(base);
        Some(out)
    } else {
        Some(base)
    }
}

/// The bytes a kitty-protocol terminal sends for `id` with the progressive enhancement `flags`
/// pushed (1 disambiguate, 2 event types, 4 alternate keys, 8 all keys as escape codes, 16
/// associated text). `release` appends the key's release event when flag 2 is on. `flags == 0`
/// is the legacy encoding.
pub fn encode_kitty(id: &str, flags: u32, release: bool) -> Option<Vec<u8>> {
    if flags == 0 {
        return encode_legacy(id);
    }
    let k = parse_key(id);
    let m = 1 + u32::from(k.shift) + 2 * u32::from(k.alt) + 4 * u32::from(k.ctrl) + 8 * u32::from(k.sup);
    let events = flags & 2 != 0;
    let all = flags & 8 != 0;
    let mut out = Vec::new();
    if let Some((fin, n)) = named_csi(&k.name) {
        out.extend(csi_mod(n, m, fin, ""));
        if release && events {
            out.extend(csi_mod(n, m, fin, ":3"));
        }
        return Some(out);
    }
    let (code, shifted): (u32, Option<u32>) = match k.name.as_str() {
        "enter" => (13, None),
        "tab" => (9, None),
        "backspace" => (127, None),
        "escape" => (27, None),
        "space" => (32, None),
        name => {
            let c = single_char(name)?;
            let lower = c.to_lowercase().next().unwrap_or(c);
            let up = c.to_uppercase().next().unwrap_or(c);
            let shifted = (k.shift && up != lower).then_some(up as u32);
            (lower as u32, shifted)
        }
    };
    let plain_text_key = !matches!(code, 13 | 9 | 127 | 27) && !k.ctrl && !k.alt && !k.sup;
    // Without "all keys as escape codes", text-producing keys and unmodified Enter/Tab/Backspace
    // keep their legacy bytes.
    if !all {
        if plain_text_key {
            let s = match shifted {
                Some(u) => char::from_u32(u).map(|c| c.to_string()),
                None => char::from_u32(code).map(|c| c.to_string()),
            }?;
            return Some(s.into_bytes());
        }
        if m == 1 && matches!(code, 13 | 9 | 127) {
            return encode_legacy(id);
        }
    }
    let key = match shifted {
        Some(s) if flags & 4 != 0 => format!("{code}:{s}"),
        _ => code.to_string(),
    };
    let press = if m == 1 {
        format!("\x1b[{key}u")
    } else {
        format!("\x1b[{key};{m}u")
    };
    out.extend(press.into_bytes());
    if release && events {
        out.extend(format!("\x1b[{key};{m}:3u").into_bytes());
    }
    Some(out)
}

// ---------------------------------------------------------------------------------------------
// The session
// ---------------------------------------------------------------------------------------------

/// The real `oxa` binary running in a pseudo-terminal.
pub struct PtySession {
    shared: Arc<Shared>,
    reader: Option<JoinHandle<()>>,
    child: Child,
    pid: libc::pid_t,
    /// The master fd (kept for ioctls; the reader owns a dup for reading).
    master: OwnedFd,
    profile: TermProfile,
    exit: Option<ExitStatus>,
}

fn cvt(r: libc::c_int, what: &str) -> std::io::Result<libc::c_int> {
    if r < 0 {
        let e = std::io::Error::last_os_error();
        Err(std::io::Error::new(e.kind(), format!("{what}: {e}")))
    } else {
        Ok(r)
    }
}

/// Open a pty pair with both fds close-on-exec: `(master, slave)`.
fn open_pty(rows: u16, cols: u16) -> std::io::Result<(OwnedFd, OwnedFd)> {
    // SAFETY: plain libc calls; every returned fd is checked and owned immediately.
    unsafe {
        let m = cvt(
            libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC),
            "posix_openpt",
        )?;
        let master = OwnedFd::from_raw_fd(m);
        cvt(libc::grantpt(m), "grantpt")?;
        cvt(libc::unlockpt(m), "unlockpt")?;
        let mut name = [0 as libc::c_char; 128];
        if libc::ptsname_r(m, name.as_mut_ptr(), name.len()) != 0 {
            return Err(std::io::Error::other("ptsname_r failed"));
        }
        let s = cvt(
            libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC),
            "open pty slave",
        )?;
        let slave = OwnedFd::from_raw_fd(s);
        set_winsize(m, rows, cols)?;
        Ok((master, slave))
    }
}

fn set_winsize(fd: libc::c_int, rows: u16, cols: u16) -> std::io::Result<()> {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: a valid winsize for TIOCSWINSZ.
    cvt(unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) }, "TIOCSWINSZ").map(|_| ())
}

impl PtySession {
    /// Start `oxa <args>` in `env.work()` on a `rows x cols` pty with the default profile
    /// ([`TermProfile::xterm`]). Panics when the pty or the process cannot be started (build the
    /// binaries first: `cargo build -p oxa-cli -p oxa-fake`).
    pub fn spawn(env: &TestEnv, engine_url: &str, args: &[&str], size: (u16, u16)) -> PtySession {
        PtySession::spawn_with(
            env,
            engine_url,
            args,
            PtyOpts {
                size,
                ..PtyOpts::default()
            },
        )
    }

    /// [`PtySession::spawn`] with a profile, extra environment and scrollback.
    pub fn spawn_with(env: &TestEnv, engine_url: &str, args: &[&str], opts: PtyOpts) -> PtySession {
        let (rows, cols) = opts.size;
        let (master, slave) = open_pty(rows, cols).expect("open a pty");
        let io = |fd: &OwnedFd| Stdio::from(fd.try_clone().expect("dup pty slave"));
        let mut cmd = Command::new(oxa_bin());
        cmd.args(args)
            .current_dir(env.work())
            .env_clear()
            .envs(env.envs(engine_url))
            // The PARITY rows describe pi's interface: under both profiles they drive pi's
            // look (`OXA_SKIN=pi`), so a row checks the same capability with the profile's
            // formats and defaults. OXA's own look has its own pty tests (`oxa_skin.rs`),
            // which set `OXA_SKIN=oxa` through `PtyOpts::env` (applied last).
            .env("OXA_SKIN", "pi")
            .envs(crate::profile_env())
            .envs(opts.profile.env.iter().cloned())
            .envs(opts.env.iter().cloned())
            .stdin(io(&slave))
            .stdout(io(&slave))
            .stderr(io(&slave));
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().unwrap_or_else(|e| {
            panic!(
                "spawn {} in a pty: {e} (build it first: cargo build -p oxa-cli -p oxa-fake)",
                oxa_bin().display()
            )
        });
        // The parent's slave copies go away here, so the reader sees EOF once the child's group
        // has closed the terminal.
        drop(cmd);
        drop(slave);
        let pid = child.id() as libc::pid_t;
        let read_side = File::from(master.try_clone().expect("dup pty master"));
        let write_side = File::from(master.try_clone().expect("dup pty master"));
        let answerer = Answerer {
            profile: opts.profile.clone(),
            modes: Modes::default(),
            alt_kitty_stack: Vec::new(),
            title: String::new(),
            replies: Vec::new(),
        };
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                parser: vt100::Parser::new_with_callbacks(rows, cols, opts.scrollback, answerer),
                committed: None,
                raw: Vec::new(),
                last_output: Instant::now(),
                eof: false,
            }),
            changed: Condvar::new(),
            stop: AtomicBool::new(false),
            writer: Mutex::new(write_side),
        });
        let sh = Arc::clone(&shared);
        let reader = std::thread::Builder::new()
            .name(format!("pty-reader-{pid}"))
            .spawn(move || reader_loop(sh, read_side))
            .expect("spawn pty reader");
        PtySession {
            shared,
            reader: Some(reader),
            child,
            pid,
            master,
            profile: opts.profile,
            exit: None,
        }
    }

    /// The child's pid (also its process group and session id).
    pub fn pid(&self) -> u32 {
        self.pid as u32
    }
    pub fn profile(&self) -> &TermProfile {
        &self.profile
    }

    // ---- screen --------------------------------------------------------------------------

    /// A snapshot of the screen now.
    pub fn screen(&self) -> Screen {
        snapshot(&self.shared.lock())
    }
    pub fn screen_text(&self) -> String {
        self.screen().text()
    }
    pub fn row(&self, n: u16) -> String {
        self.screen().row(n)
    }
    pub fn contains(&self, needle: &str) -> bool {
        self.screen().contains(needle)
    }
    pub fn cursor(&self) -> (u16, u16) {
        self.screen().cursor()
    }
    pub fn cell(&self, row: u16, col: u16) -> Option<CellInfo> {
        self.screen().cell(row, col)
    }
    pub fn modes(&self) -> Modes {
        self.screen().modes
    }
    /// Every byte the binary wrote to the terminal so far.
    pub fn raw(&self) -> Vec<u8> {
        self.shared.lock().raw.clone()
    }
    /// A mark for [`PtySession::raw_since`]: the current length of the raw log.
    pub fn mark(&self) -> usize {
        self.shared.lock().raw.len()
    }
    /// The bytes written since `mark`.
    pub fn raw_since(&self, mark: usize) -> Vec<u8> {
        let st = self.shared.lock();
        st.raw[mark.min(st.raw.len())..].to_vec()
    }
    /// The raw log as (lossy) text, for `contains` checks on escape sequences.
    pub fn raw_string(&self) -> String {
        String::from_utf8_lossy(&self.raw()).into_owned()
    }
    /// `true` when the slave terminal is in raw mode (no echo, no canonical input): the TUI has
    /// taken the terminal. Read from the master with `tcgetattr`.
    pub fn raw_mode(&self) -> bool {
        // SAFETY: tcgetattr into a zeroed termios on our own fd.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(self.master.as_raw_fd(), &mut t) != 0 {
                return false;
            }
            t.c_lflag & (libc::ECHO | libc::ICANON) == 0
        }
    }

    // ---- input ---------------------------------------------------------------------------

    /// Write raw bytes to the terminal input.
    pub fn send_bytes(&self, bytes: &[u8]) {
        self.shared
            .write(bytes)
            .unwrap_or_else(|e| panic!("write to pty: {e}\n{}", self.dump("write")));
    }
    /// Type `text` as plain characters (no key encoding, no paste brackets).
    pub fn send_text(&self, text: &str) {
        self.send_bytes(text.as_bytes());
    }
    /// Press the key `id` (pi notation: `enter`, `ctrl+c`, `shift+tab`, `alt+up`, `f5`, `a`,
    /// `shift+a`) as the profile's terminal would send it now ([`KeyEncoding::Auto`]). Panics
    /// when the terminal cannot send that key (e.g. `shift+enter` on a legacy terminal).
    pub fn send_key(&self, id: &str) {
        self.send_key_as(id, KeyEncoding::Auto);
    }
    /// Press `id` with an explicit encoding.
    pub fn send_key_as(&self, id: &str, enc: KeyEncoding) {
        let bytes = self.key_bytes(id, enc).unwrap_or_else(|| {
            panic!(
                "key {id:?} has no {enc:?} encoding on profile {} (kitty flags {})",
                self.profile.name,
                self.modes().kitty_flags()
            )
        });
        self.send_bytes(&bytes);
    }
    /// The bytes [`PtySession::send_key_as`] would send.
    pub fn key_bytes(&self, id: &str, enc: KeyEncoding) -> Option<Vec<u8>> {
        match enc {
            KeyEncoding::Legacy => encode_legacy(id),
            KeyEncoding::Kitty(flags) => encode_kitty(id, flags, self.profile.kitty_release_events),
            KeyEncoding::Auto => {
                let flags = if self.profile.kitty_keyboard {
                    self.modes().kitty_flags()
                } else {
                    0
                };
                encode_kitty(id, flags, self.profile.kitty_release_events)
            }
        }
    }
    /// Paste `text` as the terminal would: inside `ESC[200~ ... ESC[201~` when the binary has
    /// bracketed paste on, as plain bytes otherwise. Newlines are sent as `\r` (what terminals do).
    pub fn paste(&self, text: &str) {
        let body = text.replace("\r\n", "\r").replace('\n', "\r");
        if self.modes().bracketed_paste {
            let mut b = b"\x1b[200~".to_vec();
            b.extend_from_slice(body.as_bytes());
            b.extend_from_slice(b"\x1b[201~");
            self.send_bytes(&b);
        } else {
            self.send_bytes(body.as_bytes());
        }
    }
    /// Change the window size: `TIOCSWINSZ` on the master (the kernel sends `SIGWINCH` to the
    /// terminal's foreground process group) and resize the emulator.
    pub fn resize(&self, rows: u16, cols: u16) {
        {
            let mut st = self.shared.lock();
            st.parser.screen_mut().set_size(rows, cols);
        }
        set_winsize(self.master.as_raw_fd(), rows, cols).expect("resize pty");
    }
    /// Send `sig` (`libc::SIGINT`, `SIGTERM`, `SIGTSTP`, ...) to the child process only.
    pub fn send_signal(&self, sig: libc::c_int) {
        if self.exit.is_none() {
            // SAFETY: kill on our own, not yet reaped, child.
            unsafe {
                libc::kill(self.pid, sig);
            }
        }
    }

    // ---- waiting -------------------------------------------------------------------------

    fn dump(&self, what: &str) -> ScreenDump {
        let st = self.shared.lock();
        let from = st.raw.len().saturating_sub(4096);
        ScreenDump {
            what: what.to_string(),
            profile: self.profile.name,
            screen: Box::new(snapshot(&st)),
            raw_tail: escape_bytes(&st.raw[from..]),
            exit: self.exit,
        }
    }

    /// Wait until `pred(screen)` holds; re-checked after every output chunk. On timeout the
    /// error carries the full dump.
    pub fn wait_for(&self, pred: impl Fn(&Screen) -> bool, timeout: Duration) -> Result<Screen, ScreenDump> {
        self.wait_for_named("condition", pred, timeout)
    }

    /// [`PtySession::wait_for`] with a description for the dump.
    pub fn wait_for_named(
        &self,
        what: &str,
        pred: impl Fn(&Screen) -> bool,
        timeout: Duration,
    ) -> Result<Screen, ScreenDump> {
        let deadline = Instant::now() + timeout;
        let mut st = self.shared.lock();
        loop {
            let snap = snapshot(&st);
            if pred(&snap) {
                return Ok(snap);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                drop(st);
                return Err(self.dump(&format!("{what} within {timeout:?}")));
            }
            // A wake-up at least every 100 ms: predicates may depend on time-free state only,
            // but the eof flag and resize are set outside the reader's notify.
            st = self
                .shared
                .changed
                .wait_timeout(st, left.min(Duration::from_millis(100)))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Wait until `needle` is visible on one row ([`DEFAULT_TIMEOUT`]).
    pub fn wait_for_text(&self, needle: &str) -> Result<Screen, ScreenDump> {
        self.wait_for_named(
            &format!("text {needle:?}"),
            |s| s.contains(needle),
            DEFAULT_TIMEOUT,
        )
    }

    /// Wait until `needle` is no longer visible ([`DEFAULT_TIMEOUT`]).
    pub fn wait_for_text_gone(&self, needle: &str) -> Result<Screen, ScreenDump> {
        self.wait_for_named(
            &format!("text {needle:?} gone"),
            |s| !s.contains(needle),
            DEFAULT_TIMEOUT,
        )
    }

    /// Wait until the raw log after `mark` contains `needle` ([`DEFAULT_TIMEOUT`]).
    pub fn wait_for_raw(&self, mark: usize, needle: &[u8]) -> Result<(), ScreenDump> {
        let deadline = Instant::now() + DEFAULT_TIMEOUT;
        let mut st = self.shared.lock();
        loop {
            let tail = &st.raw[mark.min(st.raw.len())..];
            if tail.windows(needle.len().max(1)).any(|w| w == needle) {
                return Ok(());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                drop(st);
                return Err(self.dump(&format!("raw output {} after mark {mark}", escape_bytes(needle))));
            }
            st = self
                .shared
                .changed
                .wait_timeout(st, left.min(Duration::from_millis(100)))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Bytes written to the terminal input that the application has not read yet (the slave
    /// side's input queue, `FIONREAD`). 0 when the slave cannot be opened (the child is gone).
    pub fn pending_input(&self) -> usize {
        let mut name = [0 as libc::c_char; 128];
        // SAFETY: plain libc calls on a name buffer we own and a fd we close at once.
        unsafe {
            if libc::ptsname_r(self.master.as_raw_fd(), name.as_mut_ptr(), name.len()) != 0 {
                return 0;
            }
            let fd = libc::open(
                name.as_ptr(),
                libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK | libc::O_CLOEXEC,
            );
            if fd < 0 {
                return 0;
            }
            let mut n: libc::c_int = 0;
            let r = libc::ioctl(fd, libc::FIONREAD, &mut n);
            libc::close(fd);
            if r < 0 {
                0
            } else {
                n.max(0) as usize
            }
        }
    }

    /// Wait until the application has read every input byte sent before this call (the pty's
    /// input queue is empty) and has then written nothing for `quiet_ms`: the quiet period
    /// starts no earlier than the moment the input was drained, so "send a key, settle, assert
    /// nothing happened" cannot pass before the key was even read. At most `timeout`.
    pub fn wait_stable(&self, quiet_ms: u64, timeout: Duration) -> Result<Screen, ScreenDump> {
        let quiet = Duration::from_millis(quiet_ms);
        let deadline = Instant::now() + timeout;
        // 1. The input is drained (polled: the reader is not woken by the child's reads).
        while self.pending_input() > 0 {
            if self.shared.lock().eof {
                break;
            }
            if Instant::now() >= deadline {
                return Err(self.dump(&format!("input read by the application within {timeout:?}")));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let drained = Instant::now();
        // 2. Quiet for `quiet_ms`, counted from the later of the drain and the last output.
        let mut st = self.shared.lock();
        loop {
            let since = st.last_output.elapsed().min(drained.elapsed());
            if since >= quiet || st.eof {
                return Ok(snapshot(&st));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                drop(st);
                return Err(self.dump(&format!("output quiet for {quiet_ms} ms within {timeout:?}")));
            }
            st = self
                .shared
                .changed
                .wait_timeout(st, (quiet - since).min(left))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Content-based readiness: wait (bounded by `timeout`) until the application has read all
    /// input sent so far AND `pred` holds on a complete frame, then require that `pred` still
    /// holds after `quiet_ms` without output. The predicate is the done-signal; quiet time is
    /// only a secondary condition (a frame that is still being redrawn under load cannot end the
    /// wait, and a late redraw that breaks the predicate restarts it). On timeout the error
    /// carries the full dump.
    pub fn wait_steady(
        &self,
        what: &str,
        pred: impl Fn(&Screen) -> bool,
        quiet_ms: u64,
        timeout: Duration,
    ) -> Result<Screen, ScreenDump> {
        let quiet = Duration::from_millis(quiet_ms);
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            self.wait_for_named(what, &pred, left)?;
            let left = deadline.saturating_duration_since(Instant::now());
            let snap = self.wait_stable(quiet_ms, left)?;
            if pred(&snap) {
                return Ok(snap);
            }
            if Instant::now() >= deadline {
                return Err(self.dump(&format!("{what} (steady for {quiet:?}) within {timeout:?}")));
            }
        }
    }

    /// Wait until the TUI owns the terminal (raw mode) and has drawn something, then until the
    /// first frame settles (200 ms quiet). Call before typing: input sent earlier is echoed by
    /// the cooked terminal.
    pub fn wait_ready(&self) -> Result<Screen, ScreenDump> {
        let deadline = Instant::now() + DEFAULT_TIMEOUT;
        loop {
            if self.raw_mode() && !self.screen_text().trim().is_empty() {
                break;
            }
            if self.shared.lock().eof || Instant::now() >= deadline {
                return Err(self.dump("raw mode and a first frame"));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.wait_stable(200, deadline.saturating_duration_since(Instant::now()))
    }

    /// Wait for the child to exit (killing nothing), drain its remaining output and return its
    /// status. On timeout the error carries the dump; the child keeps running (Drop kills it).
    pub fn wait_exit(&mut self, timeout: Duration) -> Result<ExitStatus, ScreenDump> {
        if let Some(s) = self.exit {
            return Ok(s);
        }
        let deadline = Instant::now() + timeout;
        loop {
            if self.exited_unreaped() {
                break;
            }
            if Instant::now() >= deadline {
                return Err(self.dump(&format!("process exit within {timeout:?}")));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let status = self.reap();
        // Drain: the reader sees EOF once every slave fd is closed (the group is gone).
        let drain = Instant::now() + Duration::from_secs(5);
        let mut st = self.shared.lock();
        while !st.eof && Instant::now() < drain {
            st = self
                .shared
                .changed
                .wait_timeout(st, Duration::from_millis(50))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        Ok(status)
    }

    /// The exit status, once [`PtySession::wait_exit`] has seen the child exit.
    pub fn exit_status(&self) -> Option<ExitStatus> {
        self.exit
    }

    /// `true` when the child has exited but is not yet reaped (`waitid` with `WNOWAIT`), so its
    /// pid, and with it the process-group id, cannot be reused yet.
    fn exited_unreaped(&self) -> bool {
        // SAFETY: waitid on our own child with a zeroed siginfo.
        unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            let r = libc::waitid(
                libc::P_PID,
                self.pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            );
            r == 0 && info.si_pid() == self.pid
        }
    }

    /// Kill what is left of the child's process group (helpers it started), then reap the child.
    /// Called only while the child is a zombie or alive, so the group id is still ours.
    fn reap(&mut self) -> ExitStatus {
        // SAFETY: the group led by our unreaped child.
        unsafe {
            libc::kill(-self.pid, libc::SIGKILL);
        }
        let status = self.child.wait().expect("reap oxa");
        self.exit = Some(status);
        status
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        if self.exit.is_none() {
            // Kills the whole group (the child and anything it started), then reaps.
            let _ = self.reap();
        }
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
    }
}
