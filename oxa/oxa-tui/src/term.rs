//! A12: the terminal abstraction, the real terminal, raw mode and restore-on-panic. Owner: task A12. Signatures below are frozen; bodies are yours.
// ported from pi packages/tui/src/terminal.ts
use crate::{flush_pending, parse_input, parse_osc_color, InputEvent, TermCaps, TermMode, TerminalColors};
use oxa_proto::{codes, OxaError};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Once, OnceLock};
use std::time::{Duration, Instant};

/// The terminal as the renderer and the app see it. Implemented by [`ProcessTerminal`] and by
/// [`crate::TestTerminal`].
pub trait Terminal {
    /// Queue bytes for output. Never blocks on a slow terminal longer than the OS write.
    fn write(&mut self, bytes: &[u8]);
    /// Flush queued output. The renderer calls it once per frame.
    fn flush(&mut self);
    /// `(cols, rows)`, always at least `(1, 1)`.
    fn size(&self) -> (u16, u16);
    /// Wait up to `timeout_ms` for input and return every complete event, in order. Returns
    /// an empty vector on timeout. Applies the ESC timeout itself. A size change is reported
    /// as `InputEvent::Resize` exactly once per change.
    fn poll(&mut self, timeout_ms: u64) -> Vec<InputEvent>;
    fn caps(&self) -> TermCaps;
    /// Switch a mode. Idempotent. Every mode that was switched on is switched off on restore.
    fn set_mode(&mut self, mode: TermMode, on: bool);
    /// Leave raw mode and all modes (for Ctrl+Z and for an external editor); the screen
    /// content is the caller's business.
    fn suspend(&mut self);
    /// Undo [`Terminal::suspend`]: raw mode and the previously active modes are back.
    fn resume(&mut self);
    /// The terminal's default foreground / background as it answered the OSC 10 / OSC 11
    /// queries at start (the `system` theme derives from them). `None` = not reported.
    fn colors(&self) -> Option<TerminalColors> {
        None
    }
    /// The terminal answered the kitty graphics query at start (`gfx_query`). `false` = no
    /// reply within the query timeout, or an error reply.
    fn graphics(&self) -> bool {
        false
    }
}

/// Options of [`ProcessTerminal::open`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermOpts {
    /// Ask for the kitty keyboard protocol (flags 1|2|4|8) and fall back to legacy parsing
    /// when the terminal does not confirm it.
    pub kitty_keyboard: bool,
    pub bracketed_paste: bool,
    pub focus_reporting: bool,
    /// Milliseconds to wait for capability replies at start (0 = do not query).
    pub query_timeout_ms: u64,
    /// Capabilities decided by the caller (`detect_caps`, C14). Query replies refine
    /// `kitty_keyboard` and `cell_px`.
    pub caps: TermCaps,
}

impl Default for TermOpts {
    fn default() -> Self {
        TermOpts {
            kitty_keyboard: true,
            bracketed_paste: true,
            focus_reporting: false,
            query_timeout_ms: 100,
            caps: TermCaps::BASIC,
        }
    }
}

/// The process's controlling terminal (stdin/stdout), in raw mode from `open` until drop.
///
/// * Raw mode through `libc::termios` on fd 0; the saved termios is restored on drop, on
///   `suspend` and by the panic hook.
/// * Resize through `SIGWINCH` (a flag set by the handler, read by `poll`).
/// * `poll` reads fd 0 with `libc::poll`, feeds [`crate::parse_input`], and applies the ESC
///   timeout with [`crate::flush_pending`].
/// * `SIGTERM`/`SIGHUP` restore the terminal before the process exits.
pub struct ProcessTerminal {
    /// Bytes written but not yet flushed.
    out: Vec<u8>,
    caps: TermCaps,
    /// Modes currently switched on, in the order they were switched on. `CursorVisible` is
    /// never in it: the cursor is visible on entry, its state is `cursor_hidden`.
    active: Vec<TermMode>,
    /// The cursor was hidden (`set_mode(CursorVisible, false)`). Suspend and drop show it
    /// again (the entry state), resume hides it again.
    cursor_hidden: bool,
    /// Input bytes not yet parsed (a partial sequence waiting for its tail).
    pending: Vec<u8>,
    paste_scan: PasteScan,
    /// Events produced while querying at start, delivered by the first `poll`.
    queued: Vec<InputEvent>,
    last_size: (u16, u16),
    /// When the unparsed tail of `pending` started waiting (the ESC timeout runs from here,
    /// across `poll` calls shorter than the timeout).
    pending_since: Option<Instant>,
    suspended: bool,
    /// OSC 10 / OSC 11 answers to the start-up colour query.
    colors: TerminalColors,
    /// The kitty graphics query was answered with `OK`.
    graphics: bool,
}

impl ProcessTerminal {
    /// Enter raw mode and switch on the requested modes. Errors: `invalid` when stdin or stdout
    /// is not a terminal; `io` when termios calls fail.
    pub fn open(opts: TermOpts) -> Result<ProcessTerminal, OxaError> {
        if !stdio_is_tty() {
            return Err(
                OxaError::new(codes::INVALID, "stdin and stdout must be a terminal")
                    .with_hint("run oxa in an interactive terminal, or use print mode"),
            );
        }
        let current = get_attr()?;
        let original: libc::termios = *SAVED.get_or_init(|| current);
        let mut raw = original;
        make_raw(&mut raw);
        set_attr(&raw)?;
        install_signals();
        install_panic_hook();
        let mut term = ProcessTerminal {
            out: Vec::new(),
            caps: opts.caps,
            active: Vec::new(),
            cursor_hidden: false,
            pending: Vec::new(),
            paste_scan: PasteScan::default(),
            queued: Vec::new(),
            last_size: (0, 0),
            pending_since: None,
            colors: TerminalColors::default(),
            graphics: false,
            suspended: false,
        };
        term.last_size = term.size();
        if opts.bracketed_paste {
            term.set_mode(TermMode::BracketedPaste, true);
        }
        if opts.focus_reporting {
            term.set_mode(TermMode::FocusReporting, true);
        }
        if opts.kitty_keyboard {
            term.set_mode(TermMode::KittyKeyboard, true);
        }
        if opts.query_timeout_ms > 0 {
            term.query_caps(opts.query_timeout_ms);
        }
        term.flush();
        Ok(term)
    }

    /// Ask the terminal for its kitty keyboard state, cell size, and (as the end marker) its
    /// primary device attributes. Replies refine `caps`; other input is kept for the next poll.
    fn query_caps(&mut self, timeout_ms: u64) {
        // Kitty flags, cell size, then the OSC 10/11 colours; `COLOR_QUERY` ends with the device
        // attributes request that closes the wait.
        self.write(b"\x1b[?u\x1b[16t");
        // The kitty graphics query: answered before the device attributes, or not at all.
        self.write(crate::gfx_query().as_bytes());
        self.write(crate::COLOR_QUERY.as_bytes());
        self.flush();
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let mut events = Vec::new();
            let alive = self.wait(left.as_millis().min(u64::MAX as u128) as u64, &mut events);
            let mut done = false;
            for ev in events {
                match ev {
                    InputEvent::KittyFlags(flags) => self.caps.kitty_keyboard = flags != 0,
                    InputEvent::CellSize { w, h } => self.caps.cell_px = Some((w, h)),
                    InputEvent::OscReply { code: 10, data } => self.colors.fg = parse_osc_color(&data),
                    InputEvent::OscReply { code: 11, data } => self.colors.bg = parse_osc_color(&data),
                    InputEvent::Apc(body) => {
                        if let Some(ok) = crate::parse_gfx_reply(&body) {
                            self.graphics = ok;
                        }
                    }
                    InputEvent::DeviceAttributes(_) => done = true,
                    other => self.queued.push(other),
                }
            }
            if done || !alive {
                break;
            }
        }
    }

    /// Read and parse input until at least one event exists, `timeout_ms` passes, or stdin
    /// fails. Returns `false` when stdin is closed or unreadable.
    fn wait(&mut self, timeout_ms: u64, out: &mut Vec<InputEvent>) -> bool {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let paste_open = self.paste_scan.open(&self.pending);
            if !self.pending.is_empty() && !paste_open {
                let (events, used) = parse_input(&self.pending);
                self.pending.drain(..used);
                if used > 0 || self.pending.is_empty() {
                    self.pending_since = None;
                }
                if !events.is_empty() {
                    out.extend(events);
                    return true;
                }
            }
            let now = Instant::now();
            let remaining = deadline.saturating_duration_since(now);
            // A lone ESC (or an unfinished sequence) waits only the ESC timeout, unless a paste
            // is open: that waits for its end marker. The timeout counts from when the tail
            // began waiting, so polls shorter than it (16 ms while animating) still flush it.
            let esc_limited = !self.pending.is_empty() && !paste_open;
            if esc_limited {
                let since = *self.pending_since.get_or_insert(now);
                let esc_left =
                    (since + Duration::from_millis(crate::ESC_TIMEOUT_MS)).saturating_duration_since(now);
                if esc_left.is_zero() {
                    out.extend(flush_pending(&self.pending));
                    self.pending.clear();
                    self.pending_since = None;
                    return true;
                }
            }
            if paste_open && self.paste_scan.stall_left(now).is_some_and(|d| d.is_zero()) {
                out.extend(flush_pending(&self.pending));
                self.pending.clear();
                self.pending_since = None;
                return true;
            }
            let esc_left = self.pending_since.map(|since| {
                (since + Duration::from_millis(crate::ESC_TIMEOUT_MS)).saturating_duration_since(now)
            });
            let mut wait = match esc_left {
                Some(left) if esc_limited && left < remaining => left,
                _ => remaining,
            };
            if paste_open {
                if let Some(left) = self.paste_scan.stall_left(now) {
                    wait = wait.min(left);
                }
            }
            match poll_stdin(wait) {
                Polled::Ready => match read_stdin() {
                    Some(bytes) => self.pending.extend_from_slice(&bytes),
                    None => {
                        out.extend(flush_pending(&self.pending));
                        self.pending.clear();
                        return false;
                    }
                },
                Polled::Interrupted => {
                    self.resize_into(out);
                    if !out.is_empty() {
                        return true;
                    }
                }
                Polled::Timeout => {
                    if esc_limited && esc_left.is_some_and(|left| left < remaining) {
                        out.extend(flush_pending(&self.pending));
                        self.pending.clear();
                        self.pending_since = None;
                    }
                    return true;
                }
                Polled::Error => return false,
            }
            if Instant::now() >= deadline && self.pending.is_empty() {
                return true;
            }
        }
    }

    /// Append a `Resize` event when the size changed since the last check.
    fn resize_into(&mut self, out: &mut Vec<InputEvent>) {
        let size = self.size();
        if size != self.last_size {
            self.last_size = size;
            out.push(InputEvent::Resize {
                cols: size.0,
                rows: size.1,
            });
        }
    }
}

impl Terminal for ProcessTerminal {
    fn write(&mut self, bytes: &[u8]) {
        self.out.extend_from_slice(bytes);
    }
    fn colors(&self) -> Option<TerminalColors> {
        (self.colors != TerminalColors::default()).then_some(self.colors)
    }
    fn graphics(&self) -> bool {
        self.graphics
    }
    fn flush(&mut self) {
        if !self.out.is_empty() {
            write_fd(1, &self.out);
            self.out.clear();
        }
    }
    fn size(&self) -> (u16, u16) {
        term_size()
    }
    fn poll(&mut self, timeout_ms: u64) -> Vec<InputEvent> {
        let mut out = std::mem::take(&mut self.queued);
        self.resize_into(&mut out);
        if out.is_empty() {
            self.wait(timeout_ms, &mut out);
        }
        out
    }
    fn caps(&self) -> TermCaps {
        self.caps
    }
    fn set_mode(&mut self, mode: TermMode, on: bool) {
        if mode == TermMode::CursorVisible {
            if self.cursor_hidden != on {
                return;
            }
            self.cursor_hidden = !on;
            if !self.suspended {
                self.write(mode_sequence(mode, on).as_bytes());
            }
            return;
        }
        let Some(pos) = (match (self.active.iter().position(|m| *m == mode), on) {
            (None, true) => Some(self.active.len()),
            (Some(at), false) => Some(at),
            _ => None,
        }) else {
            return;
        };
        let bytes = switch_sequence(&self.active[..pos], mode, on);
        if on {
            self.active.push(mode);
        } else {
            self.active.retain(|m| *m != mode);
        }
        if !self.suspended {
            self.write(bytes.as_bytes());
        }
        ALT_KITTY.store(alt_kitty(&self.active), Ordering::SeqCst);
    }
    fn suspend(&mut self) {
        if self.suspended {
            return;
        }
        let bytes = leave_sequence(&self.active, self.cursor_hidden);
        self.out.extend_from_slice(&bytes);
        self.flush();
        restore_termios();
        self.suspended = true;
        ALT_KITTY.store(false, Ordering::SeqCst);
    }
    fn resume(&mut self) {
        if !self.suspended {
            return;
        }
        if let Some(saved) = SAVED.get() {
            let mut raw = *saved;
            make_raw(&mut raw);
            let _ = set_attr(&raw);
        }
        self.suspended = false;
        for (i, mode) in self.active.iter().enumerate() {
            let bytes = switch_sequence(&self.active[..i], *mode, true);
            self.out.extend_from_slice(bytes.as_bytes());
        }
        ALT_KITTY.store(alt_kitty(&self.active), Ordering::SeqCst);
        if self.cursor_hidden {
            self.out
                .extend_from_slice(mode_sequence(TermMode::CursorVisible, false).as_bytes());
        }
        self.flush();
    }
}

impl Drop for ProcessTerminal {
    /// Restores termios, switches every active mode off and shows the cursor again (the
    /// terminal's entry state). Must not panic.
    fn drop(&mut self) {
        let mut bytes = std::mem::take(&mut self.out);
        if !self.suspended {
            bytes.extend_from_slice(&leave_sequence(&self.active, self.cursor_hidden));
        }
        if !bytes.is_empty() {
            write_fd(1, &bytes);
        }
        restore_termios();
    }
}

/// The bytes that leave the program's terminal state: every mode of `active` switched off in
/// reverse order, then the cursor shown when it was hidden (`CursorVisible` is never switched
/// "off" on the way out: the cursor is visible on entry).
pub fn leave_sequence(active: &[TermMode], cursor_hidden: bool) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, mode) in active.iter().enumerate().rev() {
        if *mode != TermMode::CursorVisible {
            out.extend_from_slice(switch_sequence(&active[..i], *mode, false).as_bytes());
        }
    }
    if cursor_hidden {
        out.extend_from_slice(mode_sequence(TermMode::CursorVisible, true).as_bytes());
    }
    out
}

/// The escape sequence that switches `mode` on or off (`AltScreen` on = `ESC[?1049h`, ...).
/// `KittyKeyboard` on pushes pi's flags 1|2|4 (`ESC[>7u`: disambiguate, event types, alternate
/// keys; pi terminal.ts `DESIRED_KITTY_KEYBOARD_PROTOCOL_FLAGS`), off pops (`ESC[<u`).
pub fn mode_sequence(mode: TermMode, on: bool) -> &'static str {
    match (mode, on) {
        (TermMode::AltScreen, true) => "\x1b[?1049h",
        (TermMode::AltScreen, false) => "\x1b[?1049l",
        (TermMode::Mouse, true) => "\x1b[?1000h\x1b[?1002h\x1b[?1006h",
        (TermMode::Mouse, false) => "\x1b[?1006l\x1b[?1002l\x1b[?1000l",
        (TermMode::BracketedPaste, true) => "\x1b[?2004h",
        (TermMode::BracketedPaste, false) => "\x1b[?2004l",
        (TermMode::KittyKeyboard, true) => "\x1b[>7u",
        (TermMode::KittyKeyboard, false) => "\x1b[<u",
        (TermMode::FocusReporting, true) => "\x1b[?1004h",
        (TermMode::FocusReporting, false) => "\x1b[?1004l",
        (TermMode::CursorVisible, true) => "\x1b[?25h",
        (TermMode::CursorVisible, false) => "\x1b[?25l",
    }
}

/// The bytes that switch `mode` on or off when `earlier` are the modes switched on before it.
/// [`mode_sequence`], except for the alternate screen while the kitty keyboard protocol is
/// on: the protocol keeps a separate flags stack per screen (kitty, foot), so a push made on
/// the main screen does not reach the alternate one, where every shift chord would arrive as
/// its legacy byte (ctrl+shift+f as ctrl+f). Entering pushes the same flags there; leaving pops
/// them first, so the main screen's stack is untouched.
pub fn switch_sequence(earlier: &[TermMode], mode: TermMode, on: bool) -> String {
    let alt = mode_sequence(mode, on);
    if mode == TermMode::AltScreen && earlier.contains(&TermMode::KittyKeyboard) {
        let kitty = mode_sequence(TermMode::KittyKeyboard, on);
        if on {
            format!("{alt}{kitty}")
        } else {
            format!("{kitty}{alt}")
        }
    } else {
        alt.to_string()
    }
}

/// `true` when `active` has the kitty flags pushed on the alternate screen as well (the
/// protocol was on before the alternate screen was entered).
fn alt_kitty(active: &[TermMode]) -> bool {
    match (
        active.iter().position(|m| *m == TermMode::KittyKeyboard),
        active.iter().position(|m| *m == TermMode::AltScreen),
    ) {
        (Some(k), Some(a)) => k < a,
        _ => false,
    }
}

/// The bytes that put any terminal back into a sane state: every [`TermMode`] off except
/// `CursorVisible` (on), SGR reset. Written by the panic hook and on exit.
pub fn restore_sequence() -> &'static str {
    "\x1b[?1049l\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[?2004l\x1b[<u\x1b[?1004l\x1b[?25h\x1b[0m"
}

/// Install a panic hook that restores termios and writes [`restore_sequence`] to the terminal
/// **before** the previous hook prints the panic message. Idempotent (a second call does
/// nothing). Acceptance (plan A12): a panic restores the terminal.
pub fn install_panic_hook() {
    if PANIC_HOOK.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = PANIC_OWNER.set(std::thread::current().id());
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // A1/A2 (release builds unwind): a panic that a boundary contains (an extension, a
        // tool, the session worker) leaves the terminal alone; so does one on a thread other
        // than the one that drives the terminal, since the UI keeps running.
        if restores_on_panic() {
            emergency_restore();
        }
        previous(info);
    }));
}

/// `true` when a panic on the current thread must restore the terminal: the panic is not
/// contained ([`oxa_proto::panic::is_contained`]) and this thread drives the terminal (the one
/// that called [`install_panic_hook`] first).
pub fn restores_on_panic() -> bool {
    !oxa_proto::panic::is_contained()
        && PANIC_OWNER
            .get()
            .is_none_or(|t| *t == std::thread::current().id())
}

/// `true` when both stdin and stdout are terminals (`libc::isatty`).
pub fn stdio_is_tty() -> bool {
    unsafe { libc::isatty(0) == 1 && libc::isatty(1) == 1 }
}

// ---------------------------------------------------------------------------------------------
// Internals (private).
// ---------------------------------------------------------------------------------------------

/// The termios of stdin as it was before the first `open`. Set once; read by the restore paths
/// and by signal handlers, so it must never be mutated after it is set.
static SAVED: OnceLock<libc::termios> = OnceLock::new();
static PANIC_HOOK: AtomicBool = AtomicBool::new(false);
static PANIC_OWNER: OnceLock<std::thread::ThreadId> = OnceLock::new();
static SIGNALS: Once = Once::new();
/// The terminal has kitty keyboard flags pushed on the alternate screen (for the emergency
/// restore of a panic or a signal, which cannot see the terminal's mode list).
static ALT_KITTY: AtomicBool = AtomicBool::new(false);

enum Polled {
    Ready,
    Interrupted,
    Timeout,
    Error,
}

fn io_err(context: &str) -> OxaError {
    OxaError::io(context, &std::io::Error::last_os_error())
}

fn get_attr() -> Result<libc::termios, OxaError> {
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(0, &mut t) } != 0 {
        return Err(io_err("reading the terminal settings"));
    }
    Ok(t)
}

fn set_attr(t: &libc::termios) -> Result<(), OxaError> {
    if unsafe { libc::tcsetattr(0, libc::TCSANOW, t) } != 0 {
        return Err(io_err("switching the terminal to raw mode"));
    }
    Ok(())
}

/// Raw mode in the libuv style: no echo, no line buffering, no signal keys (ctrl+c arrives as
/// a byte), no input translation; output processing (`\n` to `\r\n`) stays on.
fn make_raw(t: &mut libc::termios) {
    t.c_iflag &= !(libc::IGNBRK
        | libc::BRKINT
        | libc::PARMRK
        | libc::ISTRIP
        | libc::INLCR
        | libc::IGNCR
        | libc::ICRNL
        | libc::IXON);
    t.c_oflag |= libc::OPOST | libc::ONLCR;
    t.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG | libc::IEXTEN);
    t.c_cflag &= !(libc::CSIZE | libc::PARENB);
    t.c_cflag |= libc::CS8;
    t.c_cc[libc::VMIN] = 1;
    t.c_cc[libc::VTIME] = 0;
}

/// Write all of `bytes` to `fd`, retrying on EINTR and giving up on any other error.
fn write_fd(fd: libc::c_int, bytes: &[u8]) {
    let mut off = 0;
    while off < bytes.len() {
        let rest = &bytes[off..];
        let n = unsafe { libc::write(fd, rest.as_ptr().cast(), rest.len()) };
        if n < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if n == 0 {
            break;
        }
        off += n as usize;
    }
}

/// Put the saved termios back on stdin. Errors are ignored: this runs in drop and in handlers.
fn restore_termios() {
    if let Some(saved) = SAVED.get() {
        let _ = unsafe { libc::tcsetattr(0, libc::TCSANOW, saved) };
    }
}

/// Restore everything a terminal can be left with: the modes, the screen attributes and termios.
fn emergency_restore() {
    // The alternate screen's own kitty push is popped before it is left (`switch_sequence`).
    if ALT_KITTY.load(Ordering::SeqCst) {
        write_fd(1, mode_sequence(TermMode::KittyKeyboard, false).as_bytes());
    }
    write_fd(1, restore_sequence().as_bytes());
    restore_termios();
}

extern "C" fn on_winch(_sig: libc::c_int) {
    // Only interrupts `poll`; the size is compared in `resize_into`.
}

extern "C" fn on_terminate(sig: libc::c_int) {
    emergency_restore();
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

fn install_signals() {
    SIGNALS.call_once(|| unsafe {
        libc::signal(
            libc::SIGWINCH,
            on_winch as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            on_terminate as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGHUP,
            on_terminate as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
    });
}

fn term_size() -> (u16, u16) {
    for fd in [1, 0] {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        let ok = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws as *mut libc::winsize) } == 0;
        if ok && ws.ws_col > 0 && ws.ws_row > 0 {
            return (ws.ws_col, ws.ws_row);
        }
    }
    (80, 24)
}

/// Remembers how much of an open paste was already searched for the end marker, so each new
/// chunk costs only its own length (a large paste arrives in many small reads).
#[derive(Default)]
struct PasteScan {
    scanned: usize,
    /// When the open paste last grew (None while no paste is open).
    grew: Option<Instant>,
}

/// An open paste that receives no byte for this long is flushed as text (lost end marker).
const PASTE_STALL: Duration = Duration::from_secs(5);

impl PasteScan {
    /// True while `buf` starts a paste whose end marker has not arrived yet.
    fn open(&mut self, buf: &[u8]) -> bool {
        if !buf.starts_with(PASTE_START) {
            self.scanned = 0;
            self.grew = None;
            return false;
        }
        let from = self
            .scanned
            .saturating_sub(PASTE_END.len() - 1)
            .max(PASTE_START.len())
            .min(buf.len());
        if buf[from..].windows(PASTE_END.len()).any(|w| w == PASTE_END) {
            self.scanned = 0;
            self.grew = None;
            return false;
        }
        if buf.len() != self.scanned || self.grew.is_none() {
            self.grew = Some(Instant::now());
        }
        self.scanned = buf.len();
        true
    }

    /// Time left before the open paste counts as unterminated.
    fn stall_left(&self, now: Instant) -> Option<Duration> {
        self.grew
            .map(|t| (t + PASTE_STALL).saturating_duration_since(now))
    }
}

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

fn poll_stdin(wait: Duration) -> Polled {
    let ms = wait.as_millis().min(i32::MAX as u128) as libc::c_int;
    let mut pfd = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    let r = unsafe { libc::poll(&mut pfd, 1, ms) };
    if r > 0 {
        Polled::Ready
    } else if r == 0 {
        Polled::Timeout
    } else if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
        Polled::Interrupted
    } else {
        Polled::Error
    }
}

/// Read up to one buffer of stdin. `None` on end of file or a read error; `Some(empty)` when
/// the read was interrupted and should be retried.
fn read_stdin() -> Option<Vec<u8>> {
    let mut buf = vec![0u8; 4096];
    let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
    if n > 0 {
        buf.truncate(n as usize);
        Some(buf)
    } else if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
        Some(Vec::new())
    } else {
        None
    }
}

#[cfg(test)]
mod paste_scan_tests {
    use super::*;

    #[test]
    fn stalled_paste_runs_out_of_time() {
        let mut s = PasteScan::default();
        let buf = b"\x1b[200~abc".to_vec();
        assert!(s.open(&buf));
        assert!(s.stall_left(Instant::now()).unwrap() > Duration::from_secs(4));
        assert!(s.stall_left(Instant::now() + PASTE_STALL).unwrap().is_zero());
    }

    #[test]
    fn marker_split_across_chunks_is_found() {
        let mut s = PasteScan::default();
        let mut buf = b"\x1b[200~abc\x1b[20".to_vec();
        assert!(s.open(&buf));
        buf.extend_from_slice(b"1~");
        assert!(!s.open(&buf));
    }

    #[test]
    fn large_paste_in_small_chunks_is_linear() {
        let mut s = PasteScan::default();
        let mut buf = PASTE_START.to_vec();
        let t = std::time::Instant::now();
        for _ in 0..4096 {
            buf.extend_from_slice(&[b'x'; 4096]);
            assert!(s.open(&buf));
        }
        assert!(t.elapsed().as_millis() < 500, "{:?}", t.elapsed());
    }
}
