//! Trace actual execs without requiring a system-installed strace binary.
use std::{
    fs,
    io::{self, Read},
    os::unix::process::{CommandExt, ExitStatusExt},
    path::Path,
    process::{Command, Output, Stdio},
    thread,
};

fn ptrace(request: libc::c_uint, pid: libc::pid_t, data: usize) -> io::Result<()> {
    // SAFETY: these requests use a scalar data argument and no address argument.
    if unsafe { libc::ptrace(request, pid, std::ptr::null_mut::<libc::c_void>(), data) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub fn output(command: &mut Command, trace: &Path) -> io::Result<Output> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    // SAFETY: the child callback only makes the async-signal-safe ptrace syscall.
    unsafe {
        command.pre_exec(|| ptrace(libc::PTRACE_TRACEME, 0, 0));
    }
    let mut child = command.spawn()?;
    let root = child.id() as libc::pid_t;
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let err = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });
    let mut programs = String::new();
    let mut root_status = None;
    let mut first_stop = true;
    loop {
        let mut status = 0;
        // SAFETY: status is writable. __WNOTHREAD keeps parallel tests' children out
        // of this tracer; __WALL also waits for traced clone children.
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::__WALL | libc::__WNOTHREAD) };
        if pid == -1 {
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::ECHILD) => break,
                _ => return Err(error),
            }
        }
        if libc::WIFEXITED(status) || libc::WIFSIGNALED(status) {
            if pid == root {
                root_status = Some(status);
            }
            continue;
        }
        if !libc::WIFSTOPPED(status) {
            continue;
        }
        let signal = libc::WSTOPSIG(status);
        let event = status >> 16;
        let initial_stop = first_stop && pid == root;
        if initial_stop {
            first_stop = false;
            ptrace(
                libc::PTRACE_SETOPTIONS,
                pid,
                (libc::PTRACE_O_TRACEEXEC
                    | libc::PTRACE_O_TRACEFORK
                    | libc::PTRACE_O_TRACEVFORK
                    | libc::PTRACE_O_TRACECLONE
                    | libc::PTRACE_O_EXITKILL) as usize,
            )?;
            let path = fs::read_link(format!("/proc/{pid}/exe"))?;
            programs.push_str(&format!("execve(\"{}\")\n", path.display()));
        } else if event == libc::PTRACE_EVENT_EXEC {
            let path = fs::read_link(format!("/proc/{pid}/exe"))?;
            programs.push_str(&format!("execve(\"{}\")\n", path.display()));
        }
        // Suppress ptrace event traps and the automatic stop of a new tracee,
        // but deliver ordinary signals so tracing does not change behaviour.
        let deliver = if initial_stop || event != 0 || signal == libc::SIGSTOP {
            0
        } else {
            signal as usize
        };
        ptrace(libc::PTRACE_CONT, pid, deliver)?;
    }
    fs::write(trace, programs)?;
    Ok(Output {
        status: std::process::ExitStatus::from_raw(
            root_status.ok_or_else(|| io::Error::other("traced process had no exit status"))?,
        ),
        stdout: out.join().expect("stdout reader panicked")?,
        stderr: err.join().expect("stderr reader panicked")?,
    })
}

#[test]
fn follows_fork_and_exec_and_preserves_output_and_status() {
    let env = oxa_fake::TestEnv::new();
    let trace = env.work().join("exec.trace");
    let result = output(
        Command::new("/bin/sh").args([
            "-c",
            "/bin/true & wait; printf stdout; printf stderr >&2; exec /bin/sh -c 'exit 7'",
        ]),
        &trace,
    )
    .unwrap();
    assert_eq!(result.status.code(), Some(7));
    assert_eq!(result.stdout, b"stdout");
    assert_eq!(result.stderr, b"stderr");
    let trace = fs::read_to_string(trace).unwrap();
    for (path, count) in [("/bin/sh", 2), ("/bin/true", 1)] {
        let executable = fs::canonicalize(path).unwrap();
        assert_eq!(
            trace
                .matches(&format!("execve(\"{}\")", executable.display()))
                .count(),
            count,
            "{trace}"
        );
    }
}
