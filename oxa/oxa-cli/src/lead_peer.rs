//! Linux loopback TCP provenance. Inspect the kernel's reverse connection, not HTTP headers.
//! A missing/ambiguous/non-established peer never supplies identity. No UIDs enter diagnostics.
use std::{
    io::Read,
    net::{IpAddr, SocketAddr, TcpStream},
};

type Result<T> = std::result::Result<T, &'static str>;
const MAX_TABLE: u64 = 2_097_152;

fn address(value: SocketAddr) -> String {
    let ip = match value.ip() {
        IpAddr::V4(ip) => format!("{:08X}", u32::from_ne_bytes(ip.octets())),
        IpAddr::V6(ip) => ip
            .octets()
            .chunks_exact(4)
            .map(|word| {
                format!(
                    "{:08X}",
                    u32::from_ne_bytes(word.try_into().expect("four address bytes"))
                )
            })
            .collect(),
    };
    format!("{ip}:{:04X}", value.port())
}

fn lookup(table: &str, local: SocketAddr, peer: SocketAddr) -> Result<u32> {
    let (local, peer) = (address(local), address(peer));
    let mut uid = None;
    for line in table.lines().skip(1) {
        let fields: Vec<_> = line.split_whitespace().collect();
        // TIME_WAIT records have no owning process and may report uid 0. Never trust them.
        if fields.get(1) != Some(&peer.as_str())
            || fields.get(2) != Some(&local.as_str())
            || fields.get(3) != Some(&"01")
        {
            continue;
        }
        let found = fields
            .get(7)
            .and_then(|value| value.parse().ok())
            .ok_or("invalid peer ownership")?;
        if uid.replace(found).is_some() {
            return Err("ambiguous peer ownership");
        }
    }
    uid.ok_or("peer ownership unavailable")
}

/// Bind the proof to an accepted loopback stream's full tuple in this network namespace.
/// The caller still applies the configured provider/allowlist; this alone grants no access.
pub fn owner_uid(stream: &TcpStream) -> Result<u32> {
    let local = stream.local_addr().map_err(|_| "local endpoint unavailable")?;
    let peer = stream.peer_addr().map_err(|_| "peer endpoint unavailable")?;
    if !local.ip().is_loopback() || !peer.ip().is_loopback() {
        return Err("peer is not local");
    }
    let path = if local.is_ipv4() {
        "/proc/self/net/tcp"
    } else {
        "/proc/self/net/tcp6"
    };
    let file = std::fs::File::open(path).map_err(|_| "kernel peer table unavailable")?;
    let mut table = String::new();
    file.take(MAX_TABLE + 1)
        .read_to_string(&mut table)
        .map_err(|_| "kernel peer table unreadable")?;
    if table.len() as u64 > MAX_TABLE {
        return Err("kernel peer table exceeds limit");
    }
    lookup(&table, local, peer)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ownership_requires_the_reverse_tuple_and_established_state() {
        let local: SocketAddr = "127.0.0.1:8743".parse().unwrap();
        let peer: SocketAddr = "127.0.0.1:12500".parse().unwrap();
        let row = |state, uid| {
            format!(
                "header\n0: {} {} {state} 0:0 0:0 0 {uid} 0 123\n",
                address(peer),
                address(local)
            )
        };
        assert_eq!(lookup(&row("01", 2001), local, peer), Ok(2001));
        assert!(lookup(&row("06", 0), local, peer).is_err());
        assert!(lookup(&row("01", 2001), peer, local).is_err());
        let duplicate = format!("{}{}", row("01", 2001), row("01", 2002));
        assert!(lookup(&duplicate, local, peer).is_err());
        assert!(lookup("header\nmalformed private synthetic detail", local, peer).is_err());
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn proves_real_loopback_clients_for_both_address_families() {
        use std::os::unix::fs::MetadataExt;
        let uid = std::fs::metadata("/proc/self").unwrap().uid();
        for bind in ["127.0.0.1:0", "[::1]:0"] {
            let listener = std::net::TcpListener::bind(bind).unwrap();
            let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (server, _) = listener.accept().unwrap();
            assert_eq!(owner_uid(&server), Ok(uid));
            drop(client);
        }
    }
}
