//! Bounded nonblocking JSONL pipes. No reader threads or process ownership.
use serde_json::Value;
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::process::{ChildStderr, ChildStdin, ChildStdout};
const LINE_LIMIT: usize = 256 * 1024;
const INPUT_LIMIT: usize = 256 * 1024;

/// Transport for explicitly supplied owned child pipes. The caller remains
/// responsible for sandboxing, cancellation, descendant cleanup and reaping.
/// Reads/writes never wait for a full line or EOF, including when a descendant
/// retains a pipe. Diagnostic bytes count toward the lifetime output allowance
/// and are discarded rather than recorded. A transport error closes admission.
pub struct JsonPipes {
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
    stderr: ChildStderr,
    pending: VecDeque<u8>,
    line: Vec<u8>,
    bytes: usize,
    byte_limit: usize,
    stdout_eof: bool,
    stderr_eof: bool,
    failed: bool,
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::other(message)
}
impl JsonPipes {
    /// Takes ownership of the parent's pipe ends; never launches a process.
    pub fn new(
        stdin: ChildStdin,
        stdout: ChildStdout,
        stderr: ChildStderr,
        byte_limit: usize,
    ) -> io::Result<Self> {
        if byte_limit == 0 {
            return Err(invalid("worker output allowance must be positive"));
        }
        for fd in [stdin.as_raw_fd(), stdout.as_raw_fd(), stderr.as_raw_fd()] {
            nonblocking(fd)?;
        }
        Ok(Self {
            stdin: Some(stdin),
            stdout,
            stderr,
            pending: VecDeque::new(),
            line: Vec::new(),
            bytes: 0,
            byte_limit,
            stdout_eof: false,
            stderr_eof: false,
            failed: false,
        })
    }
    /// Observed stdout + stderr bytes, including the overflow probe byte.
    pub fn output_bytes(&self) -> usize {
        self.bytes
    }
    pub fn stdout_eof(&self) -> bool {
        self.stdout_eof
    }
    pub fn output_eof(&self) -> bool {
        self.stdout_eof && self.stderr_eof
    }
    /// Closes input immediately, discarding unsent requests. Reopening it is not
    /// supported. This does not cancel or reap the process.
    pub fn close_input(&mut self) {
        self.stdin.take();
        self.pending.clear();
    }
    /// Admit one JSON object without allocating an oversized serialization.
    /// Queue rejection poisons the transport just like an I/O/protocol error.
    pub fn queue(&mut self, value: &Value) -> io::Result<()> {
        if self.failed || self.stdin.is_none() {
            return Err(invalid("worker transport unavailable"));
        }
        let result = self.queue_inner(value);
        if result.is_err() {
            self.failed = true;
            self.close_input();
        }
        result
    }
    fn queue_inner(&mut self, value: &Value) -> io::Result<()> {
        if !value.is_object() {
            return Err(invalid("worker request must be an object"));
        }
        struct Bounded {
            bytes: Vec<u8>,
            remaining: usize,
        }
        impl Write for Bounded {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if bytes.len() > self.remaining {
                    return Err(invalid("worker input limit exceeded"));
                }
                self.bytes.extend_from_slice(bytes);
                self.remaining -= bytes.len();
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut buffer = Bounded {
            bytes: Vec::new(),
            remaining: INPUT_LIMIT.saturating_sub(self.pending.len()).saturating_sub(1),
        };
        serde_json::to_writer(&mut buffer, value)?;
        buffer.bytes.push(b'\n');
        if buffer.bytes.len() > INPUT_LIMIT.saturating_sub(self.pending.len()) {
            return Err(invalid("worker input limit exceeded"));
        }
        self.pending.extend(buffer.bytes);
        Ok(())
    }
    /// Services at most eight writes, eight stdout reads and eight stderr reads
    /// per call. A successful call can return no records without indicating EOF.
    pub fn poll(&mut self) -> io::Result<Vec<Value>> {
        if self.failed {
            return Err(invalid("worker transport unavailable"));
        }
        let result = self.poll_inner();
        if result.is_err() {
            self.failed = true;
            self.close_input();
        }
        result
    }
    /// Bound each poll's work too: an infinite emitter cannot starve other
    /// workers, controls, cancellation or the deadline checks.
    fn poll_inner(&mut self) -> io::Result<Vec<Value>> {
        if let Some(stdin) = self.stdin.as_mut() {
            for _ in 0..8 {
                if self.pending.is_empty() {
                    break;
                }
                let buffer = self.pending.make_contiguous();
                match stdin.write(buffer) {
                    Ok(0) => return Err(invalid("worker stdin closed")),
                    Ok(n) => {
                        self.pending.drain(..n);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => return Err(invalid("worker input failed")),
                }
            }
        }
        let mut records = Vec::new();
        let mut buffer = [0u8; 4096];
        for _ in 0..if self.stdout_eof { 0 } else { 8 } {
            let capacity = self
                .byte_limit
                .saturating_sub(self.bytes)
                .saturating_add(1)
                .min(buffer.len());
            match self.stdout.read(&mut buffer[..capacity]) {
                Ok(0) => {
                    self.stdout_eof = true;
                    if !self.line.is_empty() {
                        return Err(invalid("unterminated worker record"));
                    }
                    break;
                }
                Ok(n) => {
                    self.account(n)?;
                    for byte in &buffer[..n] {
                        if *byte == b'\n' {
                            if !self.line.is_empty() {
                                let record: Value = serde_json::from_slice(&self.line)
                                    .map_err(|_| invalid("malformed worker record"))?;
                                if !record.is_object() {
                                    return Err(invalid("worker record must be an object"));
                                }
                                records.push(record);
                                self.line.clear();
                            }
                        } else {
                            if self.line.len() == LINE_LIMIT {
                                return Err(invalid("worker record limit exceeded"));
                            }
                            self.line.push(*byte);
                        }
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(invalid("worker output failed")),
            }
        }
        for _ in 0..if self.stderr_eof { 0 } else { 8 } {
            let capacity = self
                .byte_limit
                .saturating_sub(self.bytes)
                .saturating_add(1)
                .min(buffer.len());
            match self.stderr.read(&mut buffer[..capacity]) {
                Ok(0) => {
                    self.stderr_eof = true;
                    break;
                }
                Ok(n) => self.account(n)?,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(invalid("worker diagnostic read failed")),
            }
        }
        Ok(records)
    }

    fn account(&mut self, n: usize) -> io::Result<()> {
        let exceeded = n > self.byte_limit.saturating_sub(self.bytes);
        self.bytes = self.bytes.saturating_add(n);
        if exceeded {
            return Err(invalid("worker output budget exceeded"));
        }
        Ok(())
    }
}
fn nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: live owned pipe descriptor; fcntl only changes its status flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: same owned descriptor and valid flag bits.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    struct Fixture {
        child: OwnedChild,
        pipes: JsonPipes,
    }
    impl Fixture {
        fn new(script: &str, limit: usize) -> Self {
            let mut child = Command::new("/usr/bin/python3")
                .args(["-I", "-S", "-c", script])
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("LANG", "C.UTF-8")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let pipes = JsonPipes::new(
                child.stdin.take().unwrap(),
                child.stdout.take().unwrap(),
                child.stderr.take().unwrap(),
                limit,
            )
            .unwrap();
            Self {
                child: OwnedChild(child),
                pipes,
            }
        }
        fn drain(&mut self) -> io::Result<Vec<Value>> {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut records = Vec::new();
            loop {
                records.extend(self.pipes.poll()?);
                if self.pipes.output_eof() {
                    return Ok(records);
                }
                assert!(Instant::now() < deadline, "owned pipe fixture did not settle");
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
    struct OwnedChild(Child);
    impl std::ops::Deref for OwnedChild {
        type Target = Child;
        fn deref(&self) -> &Child {
            &self.0
        }
    }
    impl std::ops::DerefMut for OwnedChild {
        fn deref_mut(&mut self) -> &mut Child {
            &mut self.0
        }
    }
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn json_roundtrip_and_explicit_input_close_preserve_record_boundaries() {
        let mut f = Fixture::new("import sys,json\nfor line in sys.stdin:\n v=json.loads(line); print(json.dumps({'echo':v}),flush=True); print('synthetic diagnostic',file=sys.stderr,flush=True)", 4096);
        f.pipes.queue(&json!({"id":1,"text":"synthetic\nλ"})).unwrap();
        f.pipes.queue(&json!({"id":2})).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut records = Vec::new();
        while records.len() < 2 {
            records.extend(f.pipes.poll().unwrap());
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            records,
            [
                json!({"echo":{"id":1,"text":"synthetic\nλ"}}),
                json!({"echo":{"id":2}})
            ]
        );
        f.pipes.close_input();
        assert!(f.pipes.queue(&json!({"id":3})).is_err());
        assert!(f.drain().unwrap().is_empty());
        assert!(f.pipes.stdout_eof());
        assert!(f.pipes.output_bytes() > records.iter().map(|v| v.to_string().len()).sum::<usize>());
        assert!(f.child.wait().unwrap().success());
    }

    #[test]
    fn fragments_blank_lines_and_final_eof_are_handled_without_reader_threads() {
        let mut f = Fixture::new(
            "import os,time\nos.write(1,b'\\n{\"part\":'); time.sleep(0.02); os.write(1,b'1}\\n\\n')",
            4096,
        );
        assert_eq!(f.drain().unwrap(), [json!({"part":1})]);
        assert!(f.pipes.output_eof());
        assert!(f.pipes.poll().unwrap().is_empty());
    }

    #[test]
    fn malformed_nonobject_unterminated_and_overlong_records_poison_transport() {
        for script in [
            "import os; os.write(1,b'not-json\\n')",
            "import os; os.write(1,b'[]\\n')",
            "import os; os.write(1,b'{\"incomplete\":true}')",
            "import os; os.write(1,b'x'*262145)",
        ] {
            let mut f = Fixture::new(script, 512 * 1024);
            assert!(f.drain().is_err(), "{script}");
            assert!(f.pipes.poll().is_err());
            assert!(f.pipes.queue(&json!({"must_not_send":1})).is_err());
            assert!(f.pipes.pending.is_empty());
        }
    }

    #[test]
    fn stderr_counts_toward_the_shared_lifetime_output_allowance() {
        let mut f = Fixture::new("import os; os.write(1,b'{}\\n'); os.write(2,b'diagnostic')", 13);
        assert_eq!(f.drain().unwrap(), [json!({})]);
        assert_eq!(f.pipes.output_bytes(), 13);
        let mut f = Fixture::new("import os; os.write(1,b'{}\\n'); os.write(2,b'diagnostic')", 12);
        assert!(f.drain().is_err());
        assert_eq!(f.pipes.output_bytes(), 13);
    }

    #[test]
    fn pending_input_is_bounded_before_any_write_and_rejection_discards_it() {
        let mut f = Fixture::new("import time; time.sleep(2)", 4096);
        f.pipes
            .queue(&json!({"large":"x".repeat(INPUT_LIMIT-30)}))
            .unwrap();
        assert!(f.pipes.queue(&json!({"more":"x".repeat(64)})).is_err());
        assert!(f.pipes.pending.is_empty());
        assert!(f.pipes.stdin.is_none());
        let mut f = Fixture::new("import time; time.sleep(2)", 4096);
        assert!(f
            .pipes
            .queue(&json!({"oversized":"x".repeat(INPUT_LIMIT)}))
            .is_err());
        assert!(f.pipes.pending.is_empty());
        let mut f = Fixture::new("import time; time.sleep(2)", 4096);
        assert!(f.pipes.queue(&json!([])).is_err());
    }

    #[test]
    fn retained_open_pipes_do_not_make_poll_or_drop_wait_for_eof() {
        let mut f = Fixture::new("import time; time.sleep(2)", 4096);
        let started = Instant::now();
        assert!(f.pipes.poll().unwrap().is_empty());
        assert!(!f.pipes.stdout_eof());
        assert!(started.elapsed() < Duration::from_secs(1));
        let Fixture { mut child, pipes } = f;
        let started = Instant::now();
        drop(pipes);
        assert!(started.elapsed() < Duration::from_secs(1));
        let _ = child.kill();
        child.wait().unwrap();
    }

    #[test]
    fn infinite_emitter_work_is_bounded_per_poll() {
        let mut f = Fixture::new("import os\nwhile True: os.write(1,b'{}\\n'*4096)", 1024 * 1024);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let before = f.pipes.output_bytes();
            let started = Instant::now();
            let records = f.pipes.poll().unwrap();
            assert!(f.pipes.output_bytes() - before <= 8 * 4096);
            assert!(started.elapsed() < Duration::from_secs(1));
            if !records.is_empty() {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    #[test]
    fn overflow_probe_is_counted_and_reads_only_one_byte_beyond_allowance() {
        let mut f = Fixture::new("import os; os.write(1,b'x'*5000)", 12);
        assert!(f.drain().is_err());
        assert_eq!(f.pipes.output_bytes(), 13);
    }
}
