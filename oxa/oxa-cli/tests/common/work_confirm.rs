//! Real controlling-terminal approvals for publication integration tests.
use std::{
    io::Write,
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};

pub fn output(mut command: Command) -> Output {
    if !command
        .get_args()
        .any(|a| a == "--publish" || a == "--report-problems")
    {
        return command.output().unwrap();
    }
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let tmp = command
        .get_envs()
        .find_map(|(k, v)| (k == "TMPDIR").then_some(v).flatten())
        .unwrap();
    let prefix = std::path::Path::new(tmp).join(format!(
        "approval-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let stdout = prefix.with_extension("out");
    let stderr = prefix.with_extension("err");
    let quote = |s: &std::ffi::OsStr| format!("'{}'", s.to_string_lossy().replace('\'', "'\\''"));
    let words = std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(quote)
        .collect::<Vec<_>>()
        .join(" ");
    let shell = format!(
        "{words} >{} 2>{}",
        quote(stdout.as_os_str()),
        quote(stderr.as_os_str())
    );
    let mut terminal = Command::new("script");
    terminal.args(["-qefc", &shell, "/dev/null"]).env_clear();
    for (key, value) in command.get_envs() {
        if let Some(value) = value {
            terminal.env(key, value);
        }
    }
    if let Some(cwd) = command.get_current_dir() {
        terminal.current_dir(cwd);
    }
    let mut child = terminal
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("script provides a real controlling terminal");
    child
        .stdin
        .take()
        .unwrap()
        .write_all("yes\n".repeat(32).as_bytes())
        .unwrap();
    let mut out = child.wait_with_output().unwrap();
    out.stdout = std::fs::read(&stdout).unwrap_or_default();
    out.stderr.extend(std::fs::read(&stderr).unwrap_or_default());
    let _ = std::fs::remove_file(stdout);
    let _ = std::fs::remove_file(stderr);
    out
}
