//! Registry-neutral HTTP adapter. No listener, identity policy or command is declared here.
//! The owner-approved caller binds the listener and supplies verified request authorization.
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

struct Slot(Arc<AtomicUsize>);
impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Serve a caller-bound loopback listener with at most sixteen live requests.
/// Listener configuration, feature selection and verified identity remain with the
/// owner-reviewed caller. This transport has no command or configuration declaration.
pub fn serve(
    listener: TcpListener,
    authorize: impl Fn(&TcpStream, &Request<'_>) -> bool + Send + Sync + 'static,
    render: impl Fn(bool) -> String + Send + Sync + 'static,
) -> std::io::Result<()> {
    if !listener.local_addr()?.ip().is_loopback() {
        return Err(std::io::ErrorKind::PermissionDenied.into());
    }
    let (authorize, render) = (Arc::new(authorize), Arc::new(render));
    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let stream = stream?;
        if active.fetch_add(1, Ordering::SeqCst) >= 16 {
            active.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        let (authorize, render, slot) = (authorize.clone(), render.clone(), Slot(active.clone()));
        std::thread::spawn(move || {
            let _slot = slot;
            let _ = connection(stream, |s, r| authorize(s, r), |fragment| render(fragment));
        });
    }
    Ok(())
}

/// Parsed routing/identity input, deliberately without Debug or raw header serialization.
pub struct Request<'a> {
    pub method: &'a str,
    pub path: &'a str,
    /// The single raw Host authority. This is input to policy, never expected/proven authority.
    pub authority: &'a str,
    pub forwarded_login: Option<&'a str>,
}

/// Handle one accepted connection, authorizing before any fact read or page rendering.
/// The callback receives the actual stream so the identity provider can bind OS/proxy
/// provenance to this request. Neither an address nor a header is a proof by itself.
pub fn connection(
    mut stream: TcpStream,
    authorize: impl FnOnce(&TcpStream, &Request<'_>) -> bool,
    render: impl FnOnce(bool) -> String,
) -> std::io::Result<()> {
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let start = Instant::now();
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") && bytes.len() < 8192 {
        let left = Duration::from_secs(5)
            .checked_sub(start.elapsed())
            .filter(|d| !d.is_zero())
            .ok_or(std::io::ErrorKind::TimedOut)?;
        stream.set_read_timeout(Some(left))?;
        let mut byte = [0];
        if stream.read(&mut byte)? == 0 {
            return Ok(());
        }
        bytes.push(byte[0]);
    }
    let allowed = request(&bytes).and_then(|r| {
        if r.method != "GET" {
            return Err("405 Method Not Allowed");
        }
        if !authorize(&stream, &r) {
            return Err("403 Forbidden");
        }
        Ok(r.path)
    });
    let (status, kind, body) = match allowed {
        Ok("/") => ("200 OK", "text/html", render(false)),
        Ok("/events") => (
            "200 OK",
            "text/event-stream",
            format!("retry: 5000\nevent: snapshot\ndata: {}\n\n", render(true)),
        ),
        Ok("/style.css") => ("200 OK", "text/css", crate::lead_page::STYLE.into()),
        Ok("/live.js") => ("200 OK", "text/javascript", crate::lead_page::SCRIPT.into()),
        Ok(_) => ("404 Not Found", "text/plain", "Not found".into()),
        Err(status) => (status, "text/plain", "Request refused".into()),
    };
    write!(stream, "HTTP/1.1 {status}\r\nContent-Type: {kind}; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'\r\nReferrer-Policy: no-referrer\r\n\r\n{body}", body.len())
}

fn request(bytes: &[u8]) -> Result<Request<'_>, &'static str> {
    let text = std::str::from_utf8(bytes).map_err(|_| "400 Bad Request")?;
    if !text.ends_with("\r\n\r\n") {
        return Err("431 Request Header Fields Too Large");
    }
    let mut lines = text[..text.len() - 4].split("\r\n");
    let first: Vec<_> = lines.next().unwrap_or_default().split(' ').collect();
    if first.len() != 3 || first[2] != "HTTP/1.1" {
        return Err("400 Bad Request");
    }
    let mut login = None;
    let mut authority = None;
    for line in lines {
        let (name, value) = line.split_once(':').ok_or("400 Bad Request")?;
        if name.is_empty()
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || value.bytes().any(|b| b.is_ascii_control() && b != b'\t')
        {
            return Err("400 Bad Request");
        }
        if name.eq_ignore_ascii_case("tailscale-user-login") && login.replace(value.trim()).is_some() {
            return Err("403 Forbidden");
        }
        if name.eq_ignore_ascii_case("host") && authority.replace(value.trim()).is_some() {
            return Err("400 Bad Request");
        }
        if name.eq_ignore_ascii_case("transfer-encoding")
            || (name.eq_ignore_ascii_case("content-length") && value.trim() != "0")
        {
            return Err("400 Bad Request");
        }
    }
    Ok(Request {
        method: first[0],
        path: first[1],
        authority: authority.filter(|s| !s.is_empty()).ok_or("400 Bad Request")?,
        forwarded_login: login,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    fn exchange(request: &str, permitted: bool) -> (String, usize) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        let reads = Arc::new(AtomicUsize::new(0));
        let count = reads.clone();
        let handler = std::thread::spawn(move || {
            connection(
                server,
                |_, _| permitted,
                |fragment| {
                    count.fetch_add(1, Ordering::SeqCst);
                    let board = oxa_work::board::Board::default();
                    let view = crate::lead_page::View {
                        board: Some(&board),
                        ..Default::default()
                    };
                    if fragment {
                        crate::lead_page::content(&view, 100)
                    } else {
                        crate::lead_page::document(&view, 100)
                    }
                },
            )
            .unwrap()
        });
        client.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        client.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        handler.join().unwrap();
        (response, reads.load(Ordering::SeqCst))
    }
    #[test]
    fn refusals_never_read_facts_or_reflect_input() {
        for (request, permitted, status) in [
            ("GET / HTTP/1.1\r\nHost: example.invalid\r\nTailscale-User-Login: synthetic-private-marker\r\n\r\n", false, "403"),
            ("POST / HTTP/1.1\r\nHost: example.invalid\r\n\r\n", true, "405"),
            ("GET / HTTP/1.1\r\nHost: example.invalid\r\nTailscale-User-Login: a\r\ntailscale-user-login: b\r\n\r\n", true, "403"),
            ("GET / HTTP/1.1\r\nHost: example.invalid\r\nContent-Length: 1\r\n\r\n", true, "400"),
            ("GET / HTTP/1.1\r\n\r\n", true, "400"),
            ("GET / HTTP/1.1\r\nHost: example.invalid\r\nHOST: other.invalid\r\n\r\n", true, "400"),
            ("GET / HTTP/1.1\r\nHost: \r\n\r\n", true, "400"),
        ] {
            let (reply, reads) = exchange(request, permitted);
            assert!(reply.starts_with(&format!("HTTP/1.1 {status}")));
            assert_eq!(reads, 0);
            assert!(!reply.contains("synthetic-private-marker"));
        }
    }
    #[test]
    fn shared_renderer_and_sse_read_one_snapshot_after_authorization() {
        for path in ["/", "/events", "/style.css", "/live.js", "/unknown"] {
            let (reply, reads) = exchange(
                &format!("GET {path} HTTP/1.1\r\nHost: example.invalid\r\n\r\n"),
                true,
            );
            assert_eq!(reads, usize::from(matches!(path, "/" | "/events")));
            assert!(reply.contains("Cache-Control: no-store"));
            if path == "/events" {
                assert!(reply.contains("Content-Type: text/event-stream"));
                assert!(reply.contains("event: snapshot\ndata: <main>"));
            }
            if path == "/" {
                assert!(reply.contains("No open PRs in this snapshot"));
            }
            if path == "/unknown" {
                assert!(reply.starts_with("HTTP/1.1 404"));
            }
        }
    }
}
