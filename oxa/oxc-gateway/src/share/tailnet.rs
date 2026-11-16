//! Explicit Linux tailscaled LocalAPI access. No subprocess, discovery worker,
//! credential lookup, environment fallback, TCP connection or cached identity.
use serde_json::Value;
use std::{fmt, future::Future, net::IpAddr, path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    sync::Semaphore,
};

const MAX_BODY: usize = 65536;
const MAX_HEAD: usize = 8192;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Invalid,
    Unavailable,
    Busy,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Invalid => "Invalid tailnet configuration or response.",
            Self::Unavailable => "Tailnet identity is unavailable.",
            Self::Busy => "Tailnet identity checks are busy.",
        })
    }
}
impl std::error::Error for Error {}

/// Construct only when sharing is explicitly enabled. The owner supplies the
/// trusted daemon socket; tests supply their own private synthetic socket.
/// Construction performs no I/O and starts no tasks. Debug never reveals paths.
pub struct LocalApi {
    socket: PathBuf,
    deadline: Duration,
    slots: Semaphore,
}
impl fmt::Debug for LocalApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TailnetLocalApi([private])")
    }
}
impl LocalApi {
    pub fn new(socket: PathBuf, deadline: Duration) -> Result<Self, Error> {
        if !socket.is_absolute() || deadline.is_zero() || deadline > Duration::from_secs(30) {
            return Err(Error::Invalid);
        }
        Ok(Self {
            socket,
            deadline,
            slots: Semaphore::new(8),
        })
    }

    /// Only addresses assigned by a running daemon with a real TUN interface.
    /// Absence never produces loopback, wildcard or another network's address.
    pub async fn addresses(&self) -> Result<Vec<IpAddr>, Error> {
        self.bounded(async { addresses(&self.status().await?) }).await
    }

    /// Check the transport peer freshly against this node's current owner.
    /// Tagged, shared, unknown and mismatched nodes fail closed. No headers or
    /// user-provided node names participate in the identity decision.
    pub async fn same_owner(&self, peer: IpAddr) -> Result<bool, Error> {
        if !usable(peer) {
            return Ok(false);
        }
        self.bounded(async {
            let status = self.status().await?;
            addresses(&status)?;
            let owner = status["Self"]["UserID"]
                .as_u64()
                .filter(|n| *n > 0)
                .ok_or(Error::Invalid)?;
            if !untagged(&status["Self"]["Tags"]) {
                return Ok(false);
            }
            // Numeric IpAddr spellings contain no query delimiters. Encode the
            // IPv6 colon to match the daemon client's addr query convention.
            let who = self
                .get(&format!(
                    "/localapi/v0/whois?addr={}",
                    peer.to_string().replace(':', "%3A")
                ))
                .await?;
            let node = &who["Node"];
            if node["User"].as_u64() != Some(owner)
                || who["UserProfile"]["ID"].as_u64() != Some(owner)
                || !untagged(&node["Tags"])
                || !node.get("Sharer").is_none_or(|v| v.as_u64() == Some(0))
            {
                return Ok(false);
            }
            let prefixes = node["Addresses"]
                .as_array()
                .filter(|v| v.len() <= 8)
                .ok_or(Error::Invalid)?;
            Ok(prefixes.iter().any(|v| {
                let Some((ip, bits)) = v.as_str().and_then(|s| s.split_once('/')) else {
                    return false;
                };
                ip.parse::<IpAddr>().ok() == Some(peer) && bits == if peer.is_ipv4() { "32" } else { "128" }
            }))
        })
        .await
    }

    async fn bounded<T>(&self, work: impl Future<Output = Result<T, Error>>) -> Result<T, Error> {
        let _slot = self.slots.try_acquire().map_err(|_| Error::Busy)?;
        // Covers connect, request, response and both calls for a peer check.
        // Dropping a timeout closes the stream; no detached work remains.
        tokio::time::timeout(self.deadline, work)
            .await
            .map_err(|_| Error::Unavailable)?
    }
    async fn status(&self) -> Result<Value, Error> {
        self.get("/localapi/v0/status?peers=false").await
    }
    async fn get(&self, path: &str) -> Result<Value, Error> {
        let mut stream = UnixStream::connect(&self.socket)
            .await
            .map_err(|_| Error::Unavailable)?;
        // HTTP/1.0 + close avoids chunked encoding and connection pooling. No
        // auth is read: Linux LocalAPI authorization is the Unix socket itself.
        stream
            .write_all(
                format!("GET {path} HTTP/1.0\r\nHost: local-tailscaled.sock\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .map_err(|_| Error::Unavailable)?;
        let mut bytes = Vec::new();
        stream
            .take((MAX_HEAD + MAX_BODY + 1) as u64)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| Error::Unavailable)?;
        response(&bytes)
    }
}

fn usable(ip: IpAddr) -> bool {
    !ip.is_unspecified() && !ip.is_multicast() && !ip.is_loopback()
}
fn untagged(tags: &Value) -> bool {
    tags.is_null() || tags.as_array().is_some_and(Vec::is_empty)
}
fn addresses(status: &Value) -> Result<Vec<IpAddr>, Error> {
    if status["BackendState"] != "Running" || status["TUN"] != true {
        return Err(Error::Unavailable);
    }
    let values = status["TailscaleIPs"]
        .as_array()
        .filter(|v| !v.is_empty() && v.len() <= 8)
        .ok_or(Error::Invalid)?;
    let mut ips = values
        .iter()
        .map(|v| {
            v.as_str()
                .and_then(|s| s.parse().ok())
                .filter(|ip| usable(*ip))
                .ok_or(Error::Invalid)
        })
        .collect::<Result<Vec<_>, _>>()?;
    ips.sort_unstable();
    ips.dedup();
    Ok(ips)
}
fn response(bytes: &[u8]) -> Result<Value, Error> {
    let end = bytes
        .windows(4)
        .position(|v| v == b"\r\n\r\n")
        .filter(|n| *n + 4 <= MAX_HEAD)
        .ok_or(Error::Invalid)?;
    let head = std::str::from_utf8(&bytes[..end]).map_err(|_| Error::Invalid)?;
    let body = &bytes[end + 4..];
    if body.len() > MAX_BODY {
        return Err(Error::Invalid);
    }
    let mut lines = head.split("\r\n");
    let mut status = lines.next().ok_or(Error::Invalid)?.splitn(3, ' ');
    if !matches!(status.next(), Some("HTTP/1.0" | "HTTP/1.1")) || status.next() != Some("200") {
        return Err(Error::Unavailable);
    }
    let mut length = None;
    for (n, line) in lines.enumerate() {
        if n >= 64 {
            return Err(Error::Invalid);
        }
        let (name, value) = line.split_once(':').ok_or(Error::Invalid)?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
            || !value.bytes().all(|b| b == b'\t' || (32..=126).contains(&b))
            || name.eq_ignore_ascii_case("transfer-encoding")
            || (name.eq_ignore_ascii_case("content-encoding")
                && !value.trim().eq_ignore_ascii_case("identity"))
        {
            return Err(Error::Invalid);
        }
        if name.eq_ignore_ascii_case("content-length") {
            let value = value.trim();
            if length.is_some() || value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err(Error::Invalid);
            }
            length = Some(value.parse::<usize>().map_err(|_| Error::Invalid)?);
        }
    }
    if length.is_some_and(|n| n != body.len()) {
        return Err(Error::Invalid);
    }
    serde_json::from_slice(body).map_err(|_| Error::Invalid)
}
