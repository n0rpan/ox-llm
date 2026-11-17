//! Bounded raw HTTP capture for the existing synthetic loopback peers.
use std::{
    collections::BTreeMap,
    io::{self, Read},
    net::TcpStream,
    time::Duration,
};

/// Method, target, headers and body bytes; callers retain decoding and health policy.
type RawRequest = (String, String, BTreeMap<String, String>, Vec<u8>);

pub(super) fn read_request(stream: &mut TcpStream, deadline: Duration) -> io::Result<RawRequest> {
    stream.set_read_timeout(Some(deadline))?;
    stream.set_write_timeout(Some(deadline))?;
    let mut bytes = vec![];
    let end = loop {
        if let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
            break end + 4;
        }
        if bytes.len() >= 32 * 1024 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let mut next = [0; 1024];
        let n = stream.read(&mut next)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        bytes.extend_from_slice(&next[..n]);
    };
    let head = std::str::from_utf8(&bytes[..end]).map_err(|_| io::ErrorKind::InvalidData)?;
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split_whitespace();
    let method = first.next().ok_or(io::ErrorKind::InvalidData)?.to_owned();
    let target = first.next().ok_or(io::ErrorKind::InvalidData)?.to_owned();
    let mut headers = BTreeMap::new();
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').ok_or(io::ErrorKind::InvalidData)?;
        headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
    }
    let length: usize = headers
        .get("content-length")
        .map(String::as_str)
        .unwrap_or("0")
        .parse()
        .map_err(|_| io::ErrorKind::InvalidData)?;
    if length > 1024 * 1024 || headers.contains_key("transfer-encoding") {
        return Err(io::ErrorKind::InvalidData.into());
    }
    while bytes.len() < end + length {
        let mut next = [0; 4096];
        let n = stream.read(&mut next)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        bytes.extend_from_slice(&next[..n]);
    }
    Ok((method, target, headers, bytes[end..end + length].to_vec()))
}
