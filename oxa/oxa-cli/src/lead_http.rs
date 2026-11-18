//! Registry-neutral HTTP adapter. No listener, identity policy or command is declared here.
//! The owner-approved caller binds the listener and supplies verified request authorization.
use crate::lead_page::Page;
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

/// One allowlisted chat-refusal fact: the response status plus fixed stage,
/// reason and remedy tokens. Owner identity, message text, token counts, URLs,
/// headers, keys and CSRF values never appear in a fact; call sites pass only
/// these constants, and no fact carries request or error bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefusalFact {
    pub status: u16,
    pub stage: Stage,
    pub reason: &'static str,
    pub fix: &'static str,
}

/// The actionable pipeline stage of a refused chat request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Identity,
    Origin,
    Csrf,
    Body,
    Limits,
    Busy,
    Recheck,
    Provider,
    Reply,
}
impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Identity => "identity",
            Self::Origin => "origin",
            Self::Csrf => "csrf",
            Self::Body => "body",
            Self::Limits => "limits",
            Self::Busy => "busy",
            Self::Recheck => "recheck",
            Self::Provider => "provider",
            Self::Reply => "reply",
        }
    }
}

/// The owner-approved caller's sink for refused chat requests.
pub type RefusalLog = std::sync::Arc<dyn Fn(RefusalFact) + Send + Sync>;

fn refuse(log: Option<&RefusalLog>, fact: RefusalFact) {
    if let Some(log) = log {
        log(fact);
    }
}

/// Every `/chat` path form; refusals on these paths are chat refusals.
pub(crate) fn chat_path(path: &str) -> bool {
    path == "/chat" || path.starts_with("/chat?")
}

/// Why an authenticated chat submission was not accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rejected {
    /// The submission's CSRF token did not match the served form.
    Csrf,
    /// The submission's shape or content was malformed.
    Body,
}
impl Rejected {
    fn fact(self) -> RefusalFact {
        match self {
            Self::Csrf => RefusalFact {
                status: 403,
                stage: Stage::Csrf,
                reason: "invalid",
                fix: "reload the served chat form and resubmit",
            },
            Self::Body => RefusalFact {
                status: 403,
                stage: Stage::Body,
                reason: "late_parse",
                fix: "send one well-formed urlencoded chat submission",
            },
        }
    }
}

/// Serve a caller-bound loopback listener with at most sixteen live requests.
/// Listener configuration, feature selection and verified identity remain with the
/// owner-reviewed caller. This transport has no command or configuration declaration.
pub fn serve(
    listener: TcpListener,
    authorize: impl Fn(&TcpStream, &Request<'_>) -> bool + Send + Sync + 'static,
    render: impl Fn(Page, bool) -> String + Send + Sync + 'static,
) -> std::io::Result<()> {
    serve_fixed(listener, authorize, render, None, None, None)
}

#[derive(Clone)]
pub(crate) struct FixedForm {
    pub csrf: String,
    pub scheme: &'static str,
}

pub(crate) enum OwnerSubmission {
    Message(String),
    Confirm(String),
}

pub(crate) type OwnerPost = dyn Fn(OwnerSubmission) -> Result<(), &'static str> + Send + Sync;

#[derive(Clone)]
pub(crate) struct OwnerForm {
    pub csrf: String,
    pub post: Arc<OwnerPost>,
}

pub(crate) fn serve_fixed(
    listener: TcpListener,
    authorize: impl Fn(&TcpStream, &Request<'_>) -> bool + Send + Sync + 'static,
    render: impl Fn(Page, bool) -> String + Send + Sync + 'static,
    form: Option<FixedForm>,
    owner: Option<OwnerForm>,
    refusals: Option<RefusalLog>,
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
        let form = form.clone();
        let owner = owner.clone();
        let refusals = refusals.clone();
        std::thread::spawn(move || {
            let _slot = slot;
            let _ = connection_fixed(
                stream,
                |s, r| authorize(s, r),
                |page, fragment| render(page, fragment),
                form.as_ref(),
                owner.as_ref(),
                refusals.as_ref(),
            );
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
    stream: TcpStream,
    authorize: impl FnOnce(&TcpStream, &Request<'_>) -> bool,
    render: impl FnOnce(Page, bool) -> String,
) -> std::io::Result<()> {
    connection_fixed(stream, authorize, render, None, None, None)
}

fn connection_fixed(
    mut stream: TcpStream,
    authorize: impl FnOnce(&TcpStream, &Request<'_>) -> bool,
    render: impl FnOnce(Page, bool) -> String,
    form: Option<&FixedForm>,
    owner: Option<&OwnerForm>,
    refusals: Option<&RefusalLog>,
) -> std::io::Result<()> {
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
    let allowed = request(&bytes, form.is_some(), owner.is_some())
        .map_err(|status| {
            // Framing failures precede identity and any fact read: one fixed
            // early-parse line, with no request bytes.
            refuse(
                refusals,
                RefusalFact {
                    status: status[..3].parse().unwrap_or(400),
                    stage: Stage::Body,
                    reason: "early_parse",
                    fix: "send well-formed HTTP request framing",
                },
            );
            status
        })
        .and_then(|(r, fields)| {
            let post = r.method == "POST" && r.path == "/chat" && (form.is_some() || owner.is_some());
            if r.method != "GET" && !post {
                if chat_path(r.path) {
                    refuse(
                        refusals,
                        RefusalFact {
                            status: 405,
                            stage: Stage::Body,
                            reason: "method",
                            fix: "chat serves GET pages and POST submissions only",
                        },
                    );
                }
                return Err("405 Method Not Allowed");
            }
            // An identity refusal is reported by the caller's own authorization closure.
            if !authorize(&stream, &r) {
                return Err("403 Forbidden");
            }
            let page = if post {
                let scheme = form.map_or("https", |form| form.scheme);
                if fields.origin != Some(format!("{scheme}://{}", r.authority).as_str())
                    || fields.kind != Some("application/x-www-form-urlencoded")
                    || (form.is_none() && fields.fetch_site != Some("same-origin"))
                {
                    refuse(
                        refusals,
                        RefusalFact {
                            status: 403,
                            stage: Stage::Origin,
                            reason: "cross_origin",
                            fix: "send the chat form from its served origin",
                        },
                    );
                    return Err("403 Forbidden");
                }
                let mut body = vec![0; fields.length];
                let read = (|| {
                    for byte in &mut body {
                        let left = Duration::from_secs(5)
                            .checked_sub(start.elapsed())
                            .filter(|d| !d.is_zero())
                            .ok_or("400 Bad Request")?;
                        stream
                            .set_read_timeout(Some(left))
                            .map_err(|_| "400 Bad Request")?;
                        stream
                            .read_exact(std::slice::from_mut(byte))
                            .map_err(|_| "400 Bad Request")?;
                    }
                    Ok(())
                })();
                if let Err(status) = read {
                    refuse(
                        refusals,
                        RefusalFact {
                            status: 400,
                            stage: Stage::Body,
                            reason: "late_parse",
                            fix: "send one well-formed urlencoded chat submission",
                        },
                    );
                    return Err(status);
                }
                // A fixed form that does not authenticate is re-examined by the owner form
                // below when one is installed; only the final refusal is recorded.
                let mut form_rejection: Option<Rejected> = None;
                let fixed = match form {
                    Some(form) if body.len() <= 256 => match fixed_target(&body, &form.csrf) {
                        Ok(target) => Some(target),
                        Err(rejected) => {
                            form_rejection = Some(rejected);
                            None
                        }
                    },
                    _ => None,
                };
                if let Some(target) = fixed {
                    Some(target)
                } else {
                    let owner = owner.ok_or_else(|| {
                        refuse(
                            refusals,
                            form_rejection.map(Rejected::fact).unwrap_or(RefusalFact {
                                status: 403,
                                stage: Stage::Csrf,
                                reason: "invalid",
                                fix: "reload the served chat form and resubmit",
                            }),
                        );
                        "403 Forbidden"
                    })?;
                    if scheme != "https" || fields.fetch_site != Some("same-origin") {
                        refuse(
                            refusals,
                            RefusalFact {
                                status: 403,
                                stage: Stage::Origin,
                                reason: "cross_origin",
                                fix: "send the chat form from its served origin",
                            },
                        );
                        return Err("403 Forbidden");
                    }
                    let submission = owner_submission(&body, &owner.csrf).map_err(|rejected| {
                        refuse(refusals, rejected.fact());
                        "403 Forbidden"
                    })?;
                    (owner.post)(submission)?;
                    Some("/chat".into())
                }
            } else {
                None
            };
            Ok((r.path, page))
        });
    let location = match &allowed {
        Ok((_, Some(target))) => format!("Location: {target}\r\n"),
        _ => String::new(),
    };
    let (status, kind, body) = match allowed {
        Ok((_, Some(_))) => ("303 See Other", "text/plain", String::new()),
        Ok(("/events", None)) => (
            "200 OK",
            "text/event-stream",
            format!(
                "retry: 5000\nevent: snapshot\ndata: {}\n\n",
                render(Page::Home, true)
            ),
        ),
        Ok(("/style.css", None)) => ("200 OK", "text/css", crate::lead_page::STYLE.into()),
        Ok(("/live.js", None)) => ("200 OK", "text/javascript", crate::lead_page::SCRIPT.into()),
        Ok((path, None)) => match Page::request(path) {
            Some((page, fragment)) => ("200 OK", "text/html", render(page, fragment)),
            None => ("404 Not Found", "text/plain", "Not found".into()),
        },
        Err(status) => (status, "text/plain", "Request refused".into()),
    };
    let has_form = form.is_some() || owner.is_some();
    let form_action = if has_form { "'self'" } else { "'none'" };
    let referrer = if has_form { "same-origin" } else { "no-referrer" };
    let headers = format!("HTTP/1.1 {status}\r\n{location}Content-Type: {kind}; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action {form_action}\r\nReferrer-Policy: {referrer}\r\n\r\n", body.len());
    write_response(
        &mut stream,
        &[headers.as_bytes(), body.as_bytes()],
        Duration::from_secs(5),
    )
}

fn write_response(stream: &mut TcpStream, parts: &[&[u8]], budget: Duration) -> std::io::Result<()> {
    let start = Instant::now();
    for part in parts {
        let mut remaining = *part;
        while !remaining.is_empty() {
            let left = budget
                .checked_sub(start.elapsed())
                .filter(|d| !d.is_zero())
                .ok_or(std::io::ErrorKind::TimedOut)?;
            stream.set_write_timeout(Some(left))?;
            match stream.write(remaining) {
                Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
                Ok(n) => remaining = &remaining[n..],
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::Interrupted
                            | std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => return Err(e),
            }
        }
    }
    Ok(())
}

#[derive(Default)]
struct Fields<'a> {
    origin: Option<&'a str>,
    fetch_site: Option<&'a str>,
    kind: Option<&'a str>,
    length: usize,
}

fn fixed_target(bytes: &[u8], expected: &str) -> Result<String, Rejected> {
    let (mut csrf, mut prompt) = (None, None);
    for field in std::str::from_utf8(bytes).map_err(|_| Rejected::Body)?.split('&') {
        let (key, value) = field.split_once('=').ok_or(Rejected::Body)?;
        match key {
            "csrf" if csrf.replace(value).is_none() => {}
            "prompt" if prompt.replace(value).is_none() => {}
            _ => return Err(Rejected::Body),
        }
    }
    if csrf != Some(expected) {
        return Err(Rejected::Csrf);
    }
    let target = format!("/chat?prompt={}", prompt.ok_or(Rejected::Body)?);
    let (Page::Chat(_), false) = Page::request(&target).ok_or(Rejected::Body)? else {
        return Err(Rejected::Body);
    };
    Ok(target)
}

fn owner_submission(bytes: &[u8], expected: &str) -> Result<OwnerSubmission, Rejected> {
    let (mut csrf, mut input) = (None, None);
    for field in std::str::from_utf8(bytes).map_err(|_| Rejected::Body)?.split('&') {
        let (key, value) = field.split_once('=').ok_or(Rejected::Body)?;
        match key {
            "csrf" if csrf.replace(value).is_none() => {}
            "message" | "proposal_id" if input.replace((key, value)).is_none() => {}
            _ => return Err(Rejected::Body),
        }
    }
    if csrf != Some(expected) {
        return Err(Rejected::Csrf);
    }
    match input.ok_or(Rejected::Body)? {
        ("message", value) => decode_message(value)
            .map(OwnerSubmission::Message)
            .ok_or(Rejected::Body),
        ("proposal_id", value)
            if value.len() == 43
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')) =>
        {
            Ok(OwnerSubmission::Confirm(value.to_owned()))
        }
        _ => Err(Rejected::Body),
    }
}

fn decode_message(value: &str) -> Option<String> {
    let mut encoded = value.bytes();
    let mut decoded = Vec::new();
    while let Some(byte) = encoded.next() {
        decoded.push(match byte {
            b'+' => b' ',
            b'%' => {
                let digit = |byte| char::from(byte).to_digit(16).map(|digit| digit as u8);
                digit(encoded.next()?)? * 16 + digit(encoded.next()?)?
            }
            byte => byte,
        });
    }
    let message = String::from_utf8(decoded).ok()?;
    (!message.trim().is_empty()
        && !message
            .chars()
            .any(|ch| ch.is_control() && !matches!(ch, '\t' | '\r' | '\n')))
    .then_some(message)
}

fn request(bytes: &[u8], fixed: bool, owner: bool) -> Result<(Request<'_>, Fields<'_>), &'static str> {
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
    let (mut fields, mut length) = (Fields::default(), None);
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
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err("400 Bad Request");
        }
        if name.eq_ignore_ascii_case("content-length") && length.replace(value.trim()).is_some()
            || name.eq_ignore_ascii_case("origin") && fields.origin.replace(value.trim()).is_some()
            || name.eq_ignore_ascii_case("content-type") && fields.kind.replace(value.trim()).is_some()
            || name.eq_ignore_ascii_case("sec-fetch-site")
                && fields.fetch_site.replace(value.trim()).is_some()
        {
            return Err("400 Bad Request");
        }
    }
    fields.length = length.unwrap_or("0").parse().map_err(|_| "400 Bad Request")?;
    if length.is_some_and(|text| text != fields.length.to_string())
        || fields.length > if owner { 4096 } else { 256 }
        || fields.length != 0 && !((fixed || owner) && first[0] == "POST" && first[1] == "/chat")
    {
        return Err("400 Bad Request");
    }
    Ok((
        Request {
            method: first[0],
            path: first[1],
            authority: authority.filter(|s| !s.is_empty()).ok_or("400 Bad Request")?,
            forwarded_login: login,
        },
        fields,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    #[test]
    fn owner_chat_refuses_bad_identity_origin_fetch_site_and_framing_before_dispatch() {
        let body = "csrf=fixture&message=hello+Lead";
        let origin_fact = || RefusalFact {
            status: 403,
            stage: Stage::Origin,
            reason: "cross_origin",
            fix: "send the chat form from its served origin",
        };
        let framing_fact = || RefusalFact {
            status: 400,
            stage: Stage::Body,
            reason: "early_parse",
            fix: "send well-formed HTTP request framing",
        };
        for (allowed, origin, site, framing, expected, logged) in [
            (false, "https://example.invalid", "same-origin", "", "403", None),
            (
                true,
                "https://other.invalid",
                "same-origin",
                "",
                "403",
                Some(origin_fact()),
            ),
            (
                true,
                "https://example.invalid",
                "same-site",
                "",
                "403",
                Some(origin_fact()),
            ),
            (
                true,
                "https://example.invalid",
                "",
                "",
                "403",
                Some(origin_fact()),
            ),
            (
                true,
                "https://example.invalid",
                "same-origin",
                "Transfer-Encoding: chunked\r\n",
                "400",
                Some(framing_fact()),
            ),
            (
                true,
                "https://example.invalid",
                "same-origin",
                "Content-Length: 4097\r\n",
                "400",
                Some(framing_fact()),
            ),
            (true, "https://example.invalid", "same-origin", "", "303", None),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (server, _) = listener.accept().unwrap();
            let count = Arc::new(AtomicUsize::new(0));
            let invoked = count.clone();
            let facts: Arc<std::sync::Mutex<Vec<RefusalFact>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
            let record = facts.clone();
            let handler = std::thread::spawn(move || {
                let owner = OwnerForm {
                    csrf: "fixture".into(),
                    post: Arc::new(move |submission| {
                        assert!(
                            matches!(submission, OwnerSubmission::Message(message) if message == "hello Lead")
                        );
                        invoked.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }),
                };
                let refusals: RefusalLog = Arc::new(move |fact| record.lock().unwrap().push(fact));
                connection_fixed(
                    server,
                    |_, _| allowed,
                    |_, _| panic!("unexpected render"),
                    None,
                    Some(&owner),
                    Some(&refusals),
                )
                .unwrap();
            });
            let length = if framing.starts_with("Content-Length:") {
                String::new()
            } else {
                format!("Content-Length: {}\r\n", body.len())
            };
            // Refused headers never reach the body reader. Do not queue an unread payload
            // that would turn the fixture's TCP close into a reset.
            let payload = if expected == "303" { body } else { "" };
            client.set_read_timeout(Some(Duration::from_secs(6))).unwrap();
            write!(client, "POST /chat HTTP/1.1\r\nHost: example.invalid\r\nOrigin: {origin}\r\nSec-Fetch-Site: {site}\r\nContent-Type: application/x-www-form-urlencoded\r\n{framing}{length}\r\n{payload}").unwrap();
            let mut reply = String::new();
            client.read_to_string(&mut reply).unwrap();
            handler.join().unwrap();
            assert!(
                reply.starts_with(&format!("HTTP/1.1 {expected}")),
                "authorized={allowed}, origin={origin}, fetch_site={site:?}, framing={framing:?}: {reply}"
            );
            assert_eq!(count.load(Ordering::SeqCst), usize::from(expected == "303"));
            assert_eq!(reply.contains("Location: /chat\r\n"), expected == "303");
            // The transport reports only its own chat refusals: identity belongs to the
            // caller's authorization; framing failures carry one fixed early-parse line.
            let facts = facts.lock().unwrap();
            assert_eq!(
                facts.as_slice(),
                logged.as_slice(),
                "authorized={allowed}, origin={origin}, fetch_site={site:?}"
            );
        }
    }

    #[test]
    fn refused_owner_posts_report_allowlisted_stages_without_payload_bytes() {
        for (body, expected) in [
            (
                "csrf=wrong&message=hello+Lead",
                RefusalFact {
                    status: 403,
                    stage: Stage::Csrf,
                    reason: "invalid",
                    fix: "reload the served chat form and resubmit",
                },
            ),
            (
                "csrf=fixture&message=%FF",
                RefusalFact {
                    status: 403,
                    stage: Stage::Body,
                    reason: "late_parse",
                    fix: "send one well-formed urlencoded chat submission",
                },
            ),
            (
                "csrf=fixture&message=",
                RefusalFact {
                    status: 403,
                    stage: Stage::Body,
                    reason: "late_parse",
                    fix: "send one well-formed urlencoded chat submission",
                },
            ),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (server, _) = listener.accept().unwrap();
            let facts: Arc<std::sync::Mutex<Vec<RefusalFact>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
            let record = facts.clone();
            let handler = std::thread::spawn(move || {
                let owner = OwnerForm {
                    csrf: "fixture".into(),
                    post: Arc::new(|_| panic!("refused submission reached the owner callback")),
                };
                let refusals: RefusalLog = Arc::new(move |fact| record.lock().unwrap().push(fact));
                connection_fixed(
                    server,
                    |_, _| true,
                    |_, _| panic!("unexpected render"),
                    None,
                    Some(&owner),
                    Some(&refusals),
                )
                .unwrap();
            });
            client.set_read_timeout(Some(Duration::from_secs(6))).unwrap();
            write!(client, "POST /chat HTTP/1.1\r\nHost: example.invalid\r\nOrigin: https://example.invalid\r\nSec-Fetch-Site: same-origin\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
            let mut reply = String::new();
            client.read_to_string(&mut reply).unwrap();
            handler.join().unwrap();
            assert!(reply.starts_with("HTTP/1.1 403"), "{body}: {reply}");
            let facts = facts.lock().unwrap();
            assert_eq!(facts.as_slice(), [expected], "{body}");
        }
    }

    #[test]
    fn owner_confirmation_requires_the_proposal_id_as_the_only_submission() {
        let id = "a".repeat(43);
        let allowed = format!("csrf=fixture&proposal_id={id}");
        assert!(matches!(
            owner_submission(allowed.as_bytes(), "fixture"),
            Ok(OwnerSubmission::Confirm(received)) if received == id
        ));
        for refused in [
            format!("{allowed}&message=also+run"),
            format!("{allowed}&proposal_id={id}"),
            "csrf=fixture&proposal_id=short".into(),
        ] {
            assert!(
                matches!(
                    owner_submission(refused.as_bytes(), "fixture"),
                    Err(Rejected::Body)
                ),
                "{refused}"
            );
        }
        assert!(matches!(
            owner_submission(allowed.as_bytes(), "different"),
            Err(Rejected::Csrf)
        ));
    }

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
                |page, fragment| {
                    count.fetch_add(1, Ordering::SeqCst);
                    let board = oxa_work::board::Board::default();
                    let view = crate::lead_page::View {
                        board: Some(&board),
                        ..Default::default()
                    };
                    if fragment {
                        crate::lead_page::content_for(page, &view, 100)
                    } else {
                        crate::lead_page::document_for(page, &view, 100)
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

    #[test]
    fn slow_reader_cannot_reset_the_total_response_write_budget() {
        use std::sync::atomic::AtomicBool;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        client.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let finished = stop.clone();
        let reader = std::thread::spawn(move || {
            while !finished.load(Ordering::SeqCst) {
                if matches!(client.read(&mut [0]), Ok(0)) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(30));
            }
        });
        let payload = vec![b'x'; 16 * 1024 * 1024];
        let began = Instant::now();
        let result = write_response(&mut server, &[&payload], Duration::from_secs(1));
        let elapsed = began.elapsed();
        stop.store(true, Ordering::SeqCst);
        drop(server);
        reader.join().unwrap();
        assert!(result.is_err());
        assert!(
            elapsed < Duration::from_millis(1750),
            "partial writes renewed the one-second budget: {elapsed:?}"
        );
    }
}
