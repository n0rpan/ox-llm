//! Owner-only HTTP behind Tailscale Serve. No TCP listener accepts forged identity headers.
use oxa_config::{Dirs, Env};
use oxa_proto::{codes, OxaError};
use std::{
    fs::{self, DirBuilder},
    io::{Read, Write},
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

fn invalid(reason: &str) -> OxaError {
    OxaError::new(codes::INVALID, reason)
}

pub(crate) fn run(args: &[String], dirs: &Dirs, env: &Env, out: &mut dyn Write) -> Result<i32, OxaError> {
    if args != ["--enable-web"] {
        return Err(invalid(
            "lead web is disabled; usage: oxa lead serve --enable-web",
        ));
    }
    if oxa_proto::offline() {
        return Err(invalid("lead web is unavailable in offline mode"));
    }
    let owner = env
        .get("OXA_LEAD_OWNER_LOGIN")
        .filter(|s| !s.is_empty() && s.len() <= 256 && s.bytes().all(|b| b.is_ascii_graphic() && b != b','))
        .ok_or_else(|| invalid("set OXA_LEAD_OWNER_LOGIN to the owner's exact Tailscale login"))?;
    fs::create_dir_all(&dirs.agent_dir).map_err(|_| invalid("cannot create agent directory"))?;
    let uid = fs::metadata(&dirs.home)
        .map_err(|_| invalid("cannot inspect owner home"))?
        .uid();
    let parent =
        fs::symlink_metadata(&dirs.agent_dir).map_err(|_| invalid("cannot inspect agent directory"))?;
    if !parent.is_dir() || parent.uid() != uid || parent.mode() & 0o022 != 0 {
        return Err(invalid("agent directory must be owner-controlled"));
    }
    let directory = dirs.agent_dir.join("lead-web");
    if let Err(error) = DirBuilder::new().mode(0o700).create(&directory) {
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(invalid("cannot create private lead web directory"));
        }
    }
    let meta = fs::symlink_metadata(&directory).map_err(|_| invalid("cannot inspect lead web directory"))?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o777 != 0o700 {
        return Err(invalid("lead web directory must be owner-only (0700)"));
    }
    // Never unlink an existing socket: another live server may own it.
    let socket = directory.join("http.sock");
    let listener = UnixListener::bind(&socket)
        .map_err(|_| invalid("cannot bind lead web socket; existing paths are preserved"))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))
        .map_err(|_| invalid("cannot protect lead web socket"))?;
    writeln!(out, "lead web: listening on private lead-web/http.sock")
        .map_err(|_| invalid("cannot report lead web startup"))?;
    out.flush()
        .map_err(|_| invalid("cannot report lead web startup"))?;
    let owner = Arc::new(owner.to_owned());
    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let stream = stream.map_err(|_| invalid("lead web accept failed"))?;
        if active.fetch_add(1, Ordering::SeqCst) >= 16 {
            active.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        let (owner, active) = (owner.clone(), active.clone());
        std::thread::spawn(move || {
            let _ = handle(stream, &owner);
            active.fetch_sub(1, Ordering::SeqCst);
        });
    }
    Ok(0)
}

fn handle(mut stream: UnixStream, owner: &str) -> std::io::Result<()> {
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let start = Instant::now();
    let mut bytes = Vec::new();
    // One bounded request, then close. Never parse a pipelined second request.
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
    let (status, body) = match request(&bytes, owner) {
        Ok("/") => ("200 OK", "<!doctype html><meta name=viewport content=\"width=device-width\"><title>oxa lead</title><h1>oxa lead</h1><p>Read-only views are not connected yet.</p>"),
        Ok(_) => ("404 Not Found", "Not found"),
        Err(status) => (status, "Request refused"),
    };
    write!(stream, "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'none'; frame-ancestors 'none'; base-uri 'none'\r\nReferrer-Policy: no-referrer\r\n\r\n{body}", body.len())
}

fn request<'a>(bytes: &'a [u8], owner: &str) -> Result<&'a str, &'static str> {
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
        if name.eq_ignore_ascii_case("transfer-encoding")
            || (name.eq_ignore_ascii_case("content-length") && value.trim() != "0")
        {
            return Err("400 Bad Request");
        }
    }
    if login != Some(owner) {
        return Err("403 Forbidden");
    }
    if first[0] != "GET" {
        return Err("405 Method Not Allowed");
    }
    Ok(first[1])
}
