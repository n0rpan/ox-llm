//! Bounded operator client; never discovers settings or starts an inference runtime.
use serde_json::{json, Value};
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

const REQUEST_LIMIT: usize = 32 * 1024;
const RESPONSE_LIMIT: usize = 1024 * 1024;
const HELP: &str = "Usage: oxa orchestrate-control --socket PATH status\n\
       oxa orchestrate-control --socket PATH events --after SEQUENCE\n\
       oxa orchestrate-control --socket PATH request < request.json\n\
Prints one JSON response. Mutations require an explicit id and current revision.\n\
A queued response is not admission; inspect events for the matching request_id.\n\
Exit: 0 response without error, 1 transport/protocol/server error, 2 invalid input.\n";

pub(crate) fn run(args: &[String]) -> i32 {
    if args == ["--help"] || args == ["-h"] {
        let _ = io::stdout().write_all(HELP.as_bytes());
        return 0;
    }
    let (path, request) = match parse(args, &mut io::stdin().lock()) {
        Ok(value) => value,
        Err(()) => {
            let _ = io::stderr().write_all(HELP.as_bytes());
            return 2;
        }
    };
    let response = match exchange(&path, &request) {
        Ok(value) => value,
        Err(_) => {
            // Do not echo socket paths, operator text or untrusted response bytes.
            let _ = io::stderr().write_all(
                b"oxa control: transport or protocol failure; inspect events before retrying a mutation\n",
            );
            return 1;
        }
    };
    let failed = response.get("error").is_some();
    if writeln!(io::stdout().lock(), "{response}").is_err() {
        return 1;
    }
    i32::from(failed)
}

fn parse(args: &[String], input: &mut impl Read) -> Result<(String, Value), ()> {
    if args.len() < 3 || args[0] != "--socket" || args[1].is_empty() {
        return Err(());
    }
    let id = format!("control-client-{}", std::process::id());
    let request = match args[2].as_str() {
        "status" if args.len() == 3 => json!({"id":id,"method":"status"}),
        "events" if args.len() == 5 && args[3] == "--after" => {
            let after = args[4].parse::<u64>().map_err(|_| ())?;
            json!({"id":id,"method":"events","after":after})
        }
        "request" if args.len() == 3 => {
            let mut bytes = Vec::new();
            input
                .take(REQUEST_LIMIT as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| ())?;
            if bytes.len() > REQUEST_LIMIT {
                return Err(());
            }
            serde_json::from_slice(&bytes).map_err(|_| ())?
        }
        _ => return Err(()),
    };
    let id = request.get("id").and_then(Value::as_str).ok_or(())?;
    if id.is_empty() || id.len() > 128 || id.chars().any(char::is_control) {
        return Err(());
    }
    // Reuse the server schema, including unknown-field rejection, before connecting.
    serde_json::from_value::<crate::control::Request>(request.clone()).map_err(|_| ())?;
    if request.to_string().len() + 1 > REQUEST_LIMIT {
        return Err(());
    }
    Ok((args[1].clone(), request))
}

fn exchange(path: &str, request: &Value) -> io::Result<Value> {
    let mut socket = UnixStream::connect(path)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    socket.set_write_timeout(Some(Duration::from_secs(3)))?;
    writeln!(socket, "{request}")?;
    let mut bytes = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "response deadline"));
        }
        socket.set_read_timeout(Some(remaining))?;
        let mut chunk = [0; 4096];
        let count = socket.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        if count > RESPONSE_LIMIT - bytes.len() {
            return Err(io::Error::other("response limit"));
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    let response: Value = serde_json::from_slice(&bytes)?;
    if !response.is_object() || response.get("id") != request.get("id") {
        return Err(io::Error::other("response identity"));
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn requests_reuse_the_server_schema_and_require_explicit_mutation_revision() {
        let base = args(&["--socket", "unused", "request"]);
        for invalid in [
            r#"{"id":"x","method":"pause"}"#,
            r#"{"id":"x","method":"status","unexpected":true}"#,
            r#"{"id":"x","method":"change_model","model":"other"}"#,
            r#"{"id":"x","method":"status"} {"id":"y","method":"status"}"#,
        ] {
            assert!(parse(&base, &mut invalid.as_bytes()).is_err());
        }
        let text = r#"{"id":"operator-1","method":"steer","revision":7,"text":"Keep the original budget"}"#;
        let (_, got) = parse(&base, &mut text.as_bytes()).unwrap();
        assert_eq!(got["revision"], 7);
        assert_eq!(got["id"], "operator-1");
        assert!(parse(&base, &mut vec![b' '; REQUEST_LIMIT + 1].as_slice()).is_err());
    }

    #[test]
    fn observation_commands_do_not_read_stdin() {
        struct NoRead;
        impl Read for NoRead {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                panic!("observation must not read stdin")
            }
        }
        let (_, status) = parse(&args(&["--socket", "unused", "status"]), &mut NoRead).unwrap();
        assert_eq!(status["method"], "status");
        let (_, events) = parse(
            &args(&["--socket", "unused", "events", "--after", "42"]),
            &mut NoRead,
        )
        .unwrap();
        assert_eq!(events["after"], 42);
    }

    #[test]
    fn actual_server_queues_one_identified_mutation_and_returns_queue_errors() {
        use std::os::unix::fs::PermissionsExt;
        let env = oxa_fake::TestEnv::new();
        let directory = env.root().join("client");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let server = crate::control::Server::start(&directory, tx).unwrap();
        let path = directory.join("session.sock");
        let path = path.to_str().unwrap();
        let request = json!({"id":"first","method":"pause","revision":9});
        assert_eq!(exchange(path, &request).unwrap()["queued"], true);
        let second = json!({"id":"second","method":"resume","revision":9});
        assert_eq!(exchange(path, &second).unwrap()["error"], "control_queue_full");
        assert!(
            matches!(rx.recv().unwrap(), crate::goal_session::Control::Identified {id, ..} if id == "first")
        );
        assert!(rx.try_recv().is_err());
        drop(server);
    }

    #[test]
    fn uncorrelated_malformed_and_oversized_server_replies_fail_without_replay() {
        use std::os::unix::net::UnixListener;
        for response in [
            b"{\"id\":\"another-request\"}\n".to_vec(),
            b"not json\n".to_vec(),
            vec![b' '; RESPONSE_LIMIT + 1],
        ] {
            let env = oxa_fake::TestEnv::new();
            let path = env.root().join("client.sock");
            let server = UnixListener::bind(&path).unwrap();
            let thread = std::thread::spawn(move || {
                let (mut stream, _) = server.accept().unwrap();
                let mut byte = [0];
                while stream.read_exact(&mut byte).is_ok() && byte[0] != b'\n' {}
                let _ = stream.write_all(&response);
            });
            assert!(exchange(path.to_str().unwrap(), &json!({"id":"request","method":"status"})).is_err());
            thread.join().unwrap();
        }
    }
}
