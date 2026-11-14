//! E5: exec hooks (`settings.json` `hooks: {event: command}`). Owner: task E5. Signatures below are frozen; bodies are yours.
use crate::guard;
use oxa_proto::{
    CancelToken, CommandSpec, ExecSpec, GuardClass, HookPoint, HookReply, Level, OxaError, Sandbox, Tool, Ui,
    Value,
};
use std::path::PathBuf;
use std::sync::Arc;

/// Default timeout of one exec hook in seconds.
pub const EXEC_HOOK_TIMEOUT_S: u32 = 30;

/// One configured exec hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecHookDecl {
    pub point: HookPoint,
    /// Run as `bash -c <command>` in the sandbox, cwd = session cwd. The hook may write inside
    /// the session's working directory (as `bash` in the workspace mode: a hook that logs its
    /// payload to a file, EXT-091); everything else is read-only, and there is no network.
    pub command: String,
    pub timeout_s: u32,
}

/// Parse the settings `hooks` value: an object `{<event name>: <command> | {command, timeout?}
/// | [<either>, ..]}`. Unknown event names and malformed entries are returned as warnings and
/// skipped (settings never fail). Declarations keep the object's order.
pub fn parse_exec_hooks(hooks: &Value) -> (Vec<ExecHookDecl>, Vec<String>) {
    let mut decls = Vec::new();
    let mut warnings = Vec::new();
    let map = match hooks {
        Value::Object(map) => map,
        Value::Null => return (decls, warnings),
        _ => {
            warnings
                .push("settings \"hooks\" must be an object of event name to command; ignored".to_string());
            return (decls, warnings);
        }
    };
    for (name, entry) in map {
        let Some(point) = HookPoint::parse(name) else {
            let hint = match nearest_event(name) {
                Some(known) => format!(" (did you mean \"{known}\"?)"),
                None => String::new(),
            };
            warnings.push(format!("unknown hook event \"{name}\"{hint}; skipped"));
            continue;
        };
        let items: Vec<&Value> = match entry {
            Value::Array(items) => items.iter().collect(),
            other => vec![other],
        };
        for item in items {
            match parse_one(item) {
                Ok((command, timeout_s)) => decls.push(ExecHookDecl {
                    point,
                    command,
                    timeout_s,
                }),
                Err(why) => warnings.push(format!("hook \"{name}\": {why}; skipped")),
            }
        }
    }
    (decls, warnings)
}

/// One entry of a hook event: a command string or `{command, timeout?}`.
fn parse_one(v: &Value) -> Result<(String, u32), String> {
    match v {
        Value::String(command) => non_empty(command).map(|c| (c, EXEC_HOOK_TIMEOUT_S)),
        Value::Object(map) => {
            let command = match map.get("command") {
                Some(Value::String(c)) => non_empty(c)?,
                _ => return Err("expected a \"command\" string".to_string()),
            };
            let timeout_s = match map.get("timeout") {
                None => EXEC_HOOK_TIMEOUT_S,
                Some(t) => t
                    .as_u64()
                    .filter(|n| *n > 0 && *n <= u64::from(u32::MAX))
                    .map(|n| n as u32)
                    .ok_or_else(|| "\"timeout\" must be a positive whole number of seconds".to_string())?,
            };
            Ok((command, timeout_s))
        }
        _ => Err("expected a command string or {\"command\": ..., \"timeout\": ...}".to_string()),
    }
}

fn non_empty(command: &str) -> Result<String, String> {
    if command.trim().is_empty() {
        Err("the command is empty".to_string())
    } else {
        Ok(command.to_string())
    }
}

/// The known event name closest to `name` (Levenshtein distance up to 2), for a typo hint.
fn nearest_event(name: &str) -> Option<&'static str> {
    HookPoint::ALL
        .iter()
        .map(|p| (edit_distance(name, p.as_str()), p.as_str()))
        .filter(|(d, _)| *d <= 2)
        .min_by_key(|(d, _)| *d)
        .map(|(_, s)| s)
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            cur.push((prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// The JSON written to a hook's stdin: `{"event": <name>, "cwd": <cwd>, "payload": <payload>}`
/// + `\n`, canonical key order.
pub fn hook_stdin(point: HookPoint, cwd: &std::path::Path, payload: &Value) -> Vec<u8> {
    let v = serde_json::json!({
        "event": point.as_str(),
        "cwd": cwd.to_string_lossy(),
        "payload": payload,
    });
    let mut bytes = serde_json::to_vec(&v).unwrap_or_else(|_| b"null".to_vec());
    bytes.push(b'\n');
    bytes
}

/// Map a hook's result to a reply.
///
/// * exit 0 and empty stdout -> `Continue`;
/// * exit 0 and a JSON object on stdout -> the same shapes as `crate::reply_from_js` for
///   `point`;
/// * exit 2 -> blocks where the point documents blocking: `Block(<stdout or "blocked by hook">)`
///   for `ToolCall` / `UserBash`, `Cancel(..)` for the `SessionBefore*` points; elsewhere
///   `Continue`;
/// * any other exit, a timeout, or unparsable stdout -> `Continue` (a failing hook never
///   blocks by accident); the second value is the warning to log.
pub fn reply_from_exit(
    point: HookPoint,
    exit: Option<i32>,
    timed_out: bool,
    stdout: &[u8],
) -> (HookReply, Option<String>) {
    let name = point.as_str();
    if timed_out {
        return (
            HookReply::Continue,
            Some(format!("{name} hook timed out and was killed; continuing")),
        );
    }
    match exit {
        Some(0) => {
            let text = String::from_utf8_lossy(stdout);
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return (HookReply::Continue, None);
            }
            match serde_json::from_str::<Value>(trimmed) {
                Ok(v @ Value::Object(_)) => (crate::reply_from_js(point, &v), None),
                _ => (
                    HookReply::Continue,
                    Some(format!(
                        "{name} hook printed output that is not a JSON object; ignored"
                    )),
                ),
            }
        }
        Some(2) => {
            let text = String::from_utf8_lossy(stdout).trim().to_string();
            let message = if text.is_empty() {
                "blocked by hook".to_string()
            } else {
                text
            };
            match point {
                HookPoint::ToolCall | HookPoint::UserBash => (HookReply::Block(message), None),
                HookPoint::SessionBeforeSwitch
                | HookPoint::SessionBeforeFork
                | HookPoint::SessionBeforeCompact
                | HookPoint::SessionBeforeTree => (HookReply::Cancel(message), None),
                _ => (HookReply::Continue, None),
            }
        }
        Some(code) => (
            HookReply::Continue,
            Some(format!("{name} hook exited with status {code}; ignored")),
        ),
        None => (
            HookReply::Continue,
            Some(format!("{name} hook was killed by a signal; ignored")),
        ),
    }
}

/// POSIX single-quote a string for `sh`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Payloads up to this size are piped in by `printf` inside the command line (one `bash -c`
/// argument; Linux caps a single argument at 128 KiB, and quoting can grow the text). Larger
/// payloads (a `context` hook on a long wire) go through `Sandbox::run_with_stdin`.
pub const EXEC_HOOK_INLINE_STDIN_MAX: usize = 64 * 1024;

/// The command handed to the sandbox for a small payload: the hook's JSON on its stdin, the
/// hook's stdout as the only output. The payload is piped in by `printf`; stderr is discarded
/// so that the output is exactly stdout (the reply is parsed from it).
fn sandboxed_command(command: &str, stdin: &[u8]) -> String {
    format!(
        "printf '%s' {} | (\n{}\n) 2>/dev/null",
        shell_quote(&String::from_utf8_lossy(stdin)),
        command
    )
}

/// The command for a payload that arrives on the real standard input.
fn stdin_command(command: &str) -> String {
    format!("(\n{command}\n) 2>/dev/null")
}

/// Exec hooks as a `Hooks` source. Replies are still subject to the W-guard: `fire` passes
/// each through `crate::guard::check` with `declared_rebase = false`.
pub struct ExecHooks {
    pub(crate) decls: Vec<ExecHookDecl>,
    pub(crate) sandbox: Arc<dyn Sandbox>,
    pub(crate) cwd: PathBuf,
}

impl ExecHooks {
    pub fn new(decls: Vec<ExecHookDecl>, sandbox: Arc<dyn Sandbox>, cwd: PathBuf) -> ExecHooks {
        ExecHooks { decls, sandbox, cwd }
    }
}

impl oxa_proto::Hooks for ExecHooks {
    /// Run the hooks declared for `p` in order; compose as documented on `HookReply`. Without a
    /// usable sandbox (`Sandbox::check` fails) hooks do not run and one warning is notified.
    ///
    /// A hook set has no wire of its own, so a `Rebase` from an exec hook at a prefix-checked
    /// point is refused (it cannot be verified as an append); the chain continues.
    fn fire(&self, p: HookPoint, payload: &Value, ui: &mut dyn Ui) -> HookReply {
        let decls: Vec<&ExecHookDecl> = self.decls.iter().filter(|d| d.point == p).collect();
        if decls.is_empty() {
            return HookReply::Continue;
        }
        if let Err(e) = self.sandbox.check() {
            ui.notify(
                &format!("exec hooks for {} skipped: {}", p.as_str(), e.message),
                Level::Warning,
            );
            return HookReply::Continue;
        }
        let stdin = hook_stdin(p, &self.cwd, payload);
        for decl in decls {
            let inline = stdin.len() <= EXEC_HOOK_INLINE_STDIN_MAX;
            let spec = ExecSpec {
                command: if inline {
                    sandboxed_command(&decl.command, &stdin)
                } else {
                    stdin_command(&decl.command)
                },
                cwd: self.cwd.clone(),
                // EXT-091: with a read-only bind of cwd, `cat > hook-out.txt` failed with
                // status 1 ("Read-only file system"); the stdin wiring was fine.
                writable: true,
                net: false,
                timeout_s: Some(decl.timeout_s),
                env: Vec::new(),
            };
            let cancel = CancelToken::new();
            let result = if inline {
                self.sandbox.run(&spec, &cancel, &mut |_chunk: &[u8]| {})
            } else {
                self.sandbox
                    .run_with_stdin(&spec, &stdin, &cancel, &mut |_chunk: &[u8]| {})
            };
            let (reply, warning) = reply_from_exit(p, result.exit, result.timed_out, &result.output);
            if let Some(w) = warning {
                ui.notify(&w, Level::Warning);
            }
            if reply == HookReply::Continue {
                continue;
            }
            let verdict =
                if matches!(p.guard(), GuardClass::PrefixChecked) && matches!(reply, HookReply::Rebase(_)) {
                    guard::GuardVerdict::Refused(OxaError::new(
                        oxa_proto::codes::GUARD_REFUSED,
                        format!("exec hook for {} cannot rebase the wire", p.as_str()),
                    ))
                } else {
                    guard::check(p, &[], reply, false)
                };
            match verdict {
                guard::GuardVerdict::Pass(r) => return r,
                guard::GuardVerdict::Ignored { reason } => ui.notify(
                    &format!("exec hook for {}: {reason}; ignored", p.as_str()),
                    Level::Warning,
                ),
                guard::GuardVerdict::Refused(e) => ui.notify(
                    &format!("exec hook for {}: {}; ignored", p.as_str(), e.message),
                    Level::Warning,
                ),
                guard::GuardVerdict::DeclaredRebase(_) => {}
            }
        }
        HookReply::Continue
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }
    fn commands(&self) -> Vec<CommandSpec> {
        Vec::new()
    }
    fn run_command(&self, name: &str, _args: &str, _ui: &mut dyn Ui) -> Result<(), OxaError> {
        Err(OxaError::new(
            oxa_proto::codes::NOT_FOUND,
            format!("unknown command: /{name}"),
        ))
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    /// A hook is declared for `p`.
    fn wants(&self, p: HookPoint) -> bool {
        self.decls.iter().any(|d| d.point == p)
    }
    fn name(&self) -> &str {
        "exec-hooks"
    }
}
