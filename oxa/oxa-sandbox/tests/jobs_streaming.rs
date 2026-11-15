//! Background jobs and `run_streaming` against the REAL process executor (unsandboxed, so the
//! test needs no bwrap).
use oxa_proto::{CancelToken, ExecResult, ExecSpec, OxaError, Sandbox};
use oxa_sandbox::{run_unsandboxed, run_unsandboxed_capture};
use oxa_tools::{JobRegistry, JobStatus, BUFFER_CAP, READ_CAP};
use std::sync::Arc;
use std::time::{Duration, Instant};

const FLOOD: &str = "head -c 8000000 /dev/zero | tr '\\0' x";

fn spec(command: &str) -> ExecSpec {
    ExecSpec {
        command: command.to_string(),
        cwd: std::env::temp_dir(),
        writable: true,
        net: false,
        timeout_s: None,
        env: Vec::new(),
    }
}

/// The real executor without bwrap; `run` captures, `run_streaming` does not.
struct Plain;

impl Sandbox for Plain {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
        run_unsandboxed(s, c, live)
    }
    fn run_streaming(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
        run_unsandboxed_capture(s, c, live, false)
    }
}

#[test]
fn streaming_keeps_no_output_but_live_sees_every_byte() {
    let mut seen = 0usize;
    let r = run_unsandboxed_capture(&spec(FLOOD), &CancelToken::new(), &mut |c| seen += c.len(), false);
    assert!(r.output.is_empty());
    assert_eq!(seen, 8_000_000);
    assert_eq!(r.exit, Some(0));

    let mut seen = 0usize;
    let r = Plain.run_streaming(&spec(FLOOD), &CancelToken::new(), &mut |c| seen += c.len());
    assert!(r.output.is_empty());
    assert_eq!((seen, r.exit), (8_000_000, Some(0)));
}

#[test]
fn plain_run_still_returns_the_output() {
    let mut seen = 0usize;
    let r = run_unsandboxed(&spec(FLOOD), &CancelToken::new(), &mut |c| seen += c.len());
    assert_eq!(r.output.len(), 8_000_000);
    assert_eq!((seen, r.exit), (8_000_000, Some(0)));
}

#[test]
fn flooding_job_stays_within_the_caps() {
    let reg = JobRegistry::new();
    reg.set_sandbox(Arc::new(Plain));
    let id = reg.spawn(spec(FLOOD)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(reg.buffered_len(&id).unwrap() <= BUFFER_CAP);
        let read = reg.read(&id).unwrap();
        assert!(read.output.len() <= READ_CAP);
        if read.status == JobStatus::Exited(0) {
            assert_eq!(read.output.len(), READ_CAP);
            assert_eq!(reg.buffered_len(&id), Some(BUFFER_CAP));
            break;
        }
        assert!(Instant::now() < deadline, "job did not finish");
        std::thread::sleep(Duration::from_millis(10));
    }
}
