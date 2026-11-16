//! The existing stdout reader also observes progress. It never cancels a child.
//! The parent owns wall-time escalation, independently of status/checkpoint I/O here.
use super::{Observation, Result, LINE_CAP};
use crate::StatusReader;
use std::io::{Cursor, Read};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

pub(super) fn read(
    mut stdout: impl Read + AsRawFd,
    stop: UnixStream,
    root: &Path,
    status: Option<&StatusReader>,
    pid: u32,
    first_probe: Duration,
    observer: &mut dyn FnMut(Observation) -> Result<()>,
) -> Result<Option<String>> {
    let mut next_probe: Option<Instant> = None;
    let mut session = None;
    let mut first_line = Vec::new();
    let mut oversized = false;
    let mut header_read = false;
    let mut eof = false;
    loop {
        if next_probe.is_some_and(|at| Instant::now() >= at) {
            let next = match (status, session.as_deref()) {
                (Some(reader), Some(id)) => observe(reader, root, id, pid, observer)?,
                _ => Some(Duration::from_secs(900)),
            };
            next_probe = next.and_then(|delay| Instant::now().checked_add(delay));
        }
        let timeout = next_probe.map_or(-1, |at| {
            at.saturating_duration_since(Instant::now())
                .as_millis()
                .saturating_add(1)
                .min(i32::MAX as u128) as i32
        });
        let mut fds = [
            libc::pollfd {
                fd: if eof { -1 } else { stdout.as_raw_fd() },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stop.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: both descriptors remain owned for this call. The initialized array
        // has exactly the supplied length; poll does not retain its pointer.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err("worker progress wait failed".into());
        }
        if fds.iter().any(|fd| fd.revents & libc::POLLNVAL != 0) {
            return Err("worker progress descriptor is invalid".into());
        }
        let mut count = 0;
        if fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            let mut bytes = [0_u8; 8192];
            count = match stdout.read(&mut bytes) {
                Ok(count) => count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err("worker stdout read failed".into()),
            };
            eof = count == 0;
            if !header_read {
                let end = bytes[..count].iter().position(|byte| *byte == b'\n');
                let take = end.map_or(count, |index| index + 1);
                if !oversized && first_line.len() + take <= LINE_CAP {
                    first_line.extend_from_slice(&bytes[..take]);
                } else {
                    first_line.clear();
                    oversized = true;
                }
                if end.is_some() || eof {
                    session = super::read_session(Cursor::new(&first_line), &mut |id| {
                        observer(Observation::Session { id: id.into(), pid })
                    })?;
                    first_line.clear();
                    header_read = true;
                    if session.is_some() {
                        next_probe = status.and_then(|_| Instant::now().checked_add(first_probe));
                    }
                }
            }
        }
        // Drain available data before honoring closure: a fast child may have
        // exited before this reader first runs, with its header still in the pipe.
        if fds[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 && count == 0 {
            break;
        }
    }
    Ok(session)
}

fn observe(
    reader: &StatusReader,
    root: &Path,
    session: &str,
    pid: u32,
    observer: &mut dyn FnMut(Observation) -> Result<()>,
) -> Result<Option<Duration>> {
    let retry = Some(Duration::from_secs(900));
    let Ok(bytes) = reader(root, session) else {
        return Ok(retry);
    };
    let now = crate::state::now_ms();
    let Ok(stall) = crate::incidents::detect_stall(&bytes, session, pid, now) else {
        return Ok(retry);
    };
    if let Some(stall) = stall {
        observer(Observation::Stall(stall))?;
        return Ok(retry);
    }
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| "validated worker status could not be decoded")?;
    if value["phase"] == "finished" {
        return Ok(None);
    }
    Ok(Some(
        value["stall_deadline_ms"]
            .as_u64()
            .filter(|deadline| *deadline > now)
            .map(|deadline| Duration::from_millis(deadline - now))
            .unwrap_or(Duration::from_secs(900)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{mpsc, Arc};

    #[test]
    fn a_fast_childs_buffered_header_is_drained_after_parent_completion() {
        let (mut output, input) = UnixStream::pair().unwrap();
        let (done, stop) = UnixStream::pair().unwrap();
        output
            .write_all(b"{\"type\":\"session\",\"id\":\"saved\"}\n")
            .unwrap();
        drop(output);
        drop(done);
        let mut observations = Vec::new();
        let session = read(
            input,
            stop,
            Path::new("/tmp"),
            None,
            123,
            Duration::from_secs(900),
            &mut |event| {
                observations.push(event);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(session.as_deref(), Some("saved"));
        assert!(matches!(&observations[..], [Observation::Session { id, pid:123 }] if id == "saved"));
    }

    #[test]
    fn native_status_identity_is_checked_before_emitting_a_stall() {
        for expected in [123, 124] {
            let (mut output, input) = UnixStream::pair().unwrap();
            let (done, stop) = UnixStream::pair().unwrap();
            let (called, checked) = mpsc::channel();
            output
                .write_all(b"{\"type\":\"session\",\"id\":\"saved\"}\n")
                .unwrap();
            let closer = std::thread::spawn(move || {
                checked.recv_timeout(Duration::from_secs(5)).unwrap();
                drop(output);
                drop(done);
            });
            let reader: StatusReader = Arc::new(move |_, _| {
                called.send(()).unwrap();
                Ok(br#"{"type":"oxa_status","version":1,"session_id":"saved","pid":123,"phase":"model","last_progress_ms":100,"stall_timeout_ms":900000,"stall_deadline_ms":900100,"outcome":null,"stalled":true}"#.to_vec())
            });
            let mut stalls = 0;
            read(
                input,
                stop,
                Path::new("/tmp"),
                Some(&reader),
                expected,
                Duration::from_millis(1),
                &mut |event| {
                    if matches!(event, Observation::Stall(_)) {
                        stalls += 1;
                    }
                    Ok(())
                },
            )
            .unwrap();
            closer.join().unwrap();
            assert_eq!(stalls, usize::from(expected == 123));
        }
    }
}
