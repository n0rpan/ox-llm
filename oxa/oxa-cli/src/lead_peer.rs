//! Linux loopback TCP provenance. Inspect the kernel's reverse connection, not HTTP headers.
//! A missing/ambiguous/non-established peer never supplies identity. No UIDs enter diagnostics.
use std::{
    io::Read,
    net::{IpAddr, SocketAddr, TcpStream},
};

type Result<T> = std::result::Result<T, &'static str>;

/// The peer table ceiling; a larger table fails the proof instead of being truncated.
const MAX_TABLE: u64 = 2_097_152;
const TABLE_V4: &str = "/proc/self/net/tcp";
const TABLE_V6: &str = "/proc/self/net/tcp6";
/// No established reverse socket in the tables consulted. A caller may retry or refuse.
const UNAVAILABLE: &str = "peer ownership unavailable";

/// One kernel table and the peer's expected reverse tuple in it.
type Plan = (&'static str, SocketAddr, SocketAddr);

fn address(value: SocketAddr) -> String {
    let ip = match value.ip() {
        IpAddr::V4(ip) => format!("{:08X}", u32::from_ne_bytes(ip.octets())),
        IpAddr::V6(ip) => ip.octets().chunks_exact(4).map(hex_word).collect(),
    };
    format!("{ip}:{:04X}", value.port())
}

fn hex_word(bytes: &[u8]) -> String {
    let bytes: [u8; 4] = bytes.try_into().expect("four address bytes");
    format!("{:08X}", u32::from_ne_bytes(bytes))
}

/// Loopback includes the IPv4-mapped `::ffff:127.x.y.z` a dual-stack listener reports.
fn is_local(address: IpAddr) -> bool {
    match address {
        IpAddr::V6(ip) => ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()),
        IpAddr::V4(ip) => ip.is_loopback(),
    }
}

/// The tables that can hold the peer's own reverse socket, in the order worth trying. A mapped
/// IPv4 pair comes from either an IPv4 client (the IPv4 table) or an IPv6 client (the IPv6 table).
fn plans(local: SocketAddr, peer: SocketAddr) -> Vec<Plan> {
    match (local, peer) {
        (SocketAddr::V4(_), SocketAddr::V4(_)) => vec![(TABLE_V4, local, peer)],
        (SocketAddr::V6(local6), SocketAddr::V6(peer6)) => {
            let mapped = (local6.ip().to_ipv4_mapped(), peer6.ip().to_ipv4_mapped());
            match mapped {
                (Some(local4), Some(peer4)) => {
                    let v4_local = SocketAddr::from((local4, local6.port()));
                    let v4_peer = SocketAddr::from((peer4, peer6.port()));
                    vec![(TABLE_V4, v4_local, v4_peer), (TABLE_V6, local, peer)]
                }
                _ => vec![(TABLE_V6, local, peer)],
            }
        }
        _ => Vec::new(),
    }
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
    uid.ok_or(UNAVAILABLE)
}

/// A bounded read; the table is streamed once and never parsed outside this module.
fn read(path: &str) -> Result<String> {
    let file = std::fs::File::open(path).map_err(|_| UNAVAILABLE)?;
    let mut table = String::new();
    file.take(MAX_TABLE + 1)
        .read_to_string(&mut table)
        .map_err(|_| "kernel peer table unreadable")?;
    if table.len() as u64 > MAX_TABLE {
        return Err("kernel peer table exceeds limit");
    }
    Ok(table)
}

/// Bind the proof to an accepted loopback stream's full tuple in this network namespace.
/// The caller still applies the configured provider/allowlist; this alone grants no access.
pub fn owner_uid(stream: &TcpStream) -> Result<u32> {
    let local = stream.local_addr().map_err(|_| "local endpoint unavailable")?;
    let peer = stream.peer_addr().map_err(|_| "peer endpoint unavailable")?;
    if !is_local(local.ip()) || !is_local(peer.ip()) {
        return Err("peer is not local");
    }
    let plans = plans(local, peer);
    if plans.is_empty() {
        return Err("peer address families differ");
    }
    for (path, local, peer) in plans {
        let table = read(path)?;
        match lookup(&table, local, peer) {
            Ok(uid) => return Ok(uid),
            // A wrong or ambiguous answer is never retried against another table.
            Err(error) if error == UNAVAILABLE => {}
            Err(error) => return Err(error),
        }
    }
    Err(UNAVAILABLE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(local: &str, peer: &str) -> (SocketAddr, SocketAddr) {
        (local.parse().unwrap(), peer.parse().unwrap())
    }

    #[test]
    fn ownership_requires_the_reverse_tuple_and_established_state() {
        let (local, peer) = pair("127.0.0.1:8743", "127.0.0.1:12500");
        let row = |state: &str, uid: u32| {
            let (client, server) = (address(peer), address(local));
            format!("header\n0: {client} {server} {state} 0:0 0:0 0 {uid} 0 123\n")
        };
        assert_eq!(lookup(&row("01", 2001), local, peer), Ok(2001));
        assert!(lookup(&row("06", 0), local, peer).is_err());
        assert!(lookup(&row("01", 2001), peer, local).is_err());
        let duplicate = format!("{}{}", row("01", 2001), row("01", 2002));
        assert!(lookup(&duplicate, local, peer).is_err());
        assert!(lookup("header\nmalformed private synthetic detail", local, peer).is_err());
    }

    #[test]
    fn loopback_includes_mapped_ipv4_and_nothing_else() {
        let cases = [
            ("127.0.0.1", true),
            ("127.9.9.9", true),
            ("192.168.1.5", false),
            ("::1", true),
            ("::2", false),
            ("::ffff:127.0.0.1", true),
            ("::ffff:10.0.0.7", false),
        ];
        for (text, local) in cases {
            let address: IpAddr = text.parse().unwrap();
            assert_eq!(is_local(address), local, "{text}");
        }
    }

    #[test]
    fn a_mapped_peer_is_sought_in_the_ipv4_table_first() {
        let (local, peer) = pair("[::ffff:127.0.0.1]:8743", "[::ffff:127.0.0.1]:12500");
        let (v4_local, v4_peer) = pair("127.0.0.1:8743", "127.0.0.1:12500");
        assert_eq!(
            plans(local, peer),
            vec![(TABLE_V4, v4_local, v4_peer), (TABLE_V6, local, peer)]
        );
        // An IPv4 client of a dual-stack listener owns a row in the IPv4 table only.
        let (client, server) = (address(v4_peer), address(v4_local));
        let row = format!("header\n0: {client} {server} 01 0:0 0:0 0 2001 0 77\n");
        assert_eq!(lookup(&row, v4_local, v4_peer), Ok(2001));
        assert_eq!(lookup(&row, local, peer), Err(UNAVAILABLE));
    }

    #[test]
    fn an_unmapped_pair_uses_its_own_family_table() {
        let (local, peer) = pair("127.0.0.1:8743", "127.0.0.1:12500");
        assert_eq!(plans(local, peer), vec![(TABLE_V4, local, peer)]);
        let (local, peer) = pair("[::1]:8743", "[::1]:12500");
        assert_eq!(plans(local, peer), vec![(TABLE_V6, local, peer)]);
        let (v4, v6) = pair("127.0.0.1:8743", "[::1]:12500");
        assert!(plans(v4, v6).is_empty());
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

    /// A dual-stack listener reports an IPv4 client as `::ffff:127.0.0.1`, and that client's own
    /// socket stays in the IPv4 table; the proof must still find it.
    #[cfg(target_os = "linux")]
    #[test]
    fn proves_an_ipv4_client_of_a_dual_stack_listener() {
        use std::os::unix::fs::MetadataExt;
        let bindv6only = std::fs::read_to_string("/proc/sys/net/ipv6/bindv6only").ok();
        assert_eq!(bindv6only.as_deref().map(str::trim), Some("0"));
        let uid = std::fs::metadata("/proc/self").unwrap().uid();
        let listener = std::net::TcpListener::bind("[::]:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let (server, peer) = listener.accept().unwrap();
        assert!(peer.ip().is_ipv6(), "the kernel reports the mapped IPv4 client");
        assert_eq!(owner_uid(&server), Ok(uid));
        drop(client);
    }
}
