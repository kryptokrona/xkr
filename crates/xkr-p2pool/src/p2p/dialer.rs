//! Outgoing TCP: turning seed and `--addpeers` entries into addresses and opening connections
//! (`P2PServer::connect_to_peers`, `load_peer_list` with `load_from_seed_nodes`,
//! `TCPServer::connect_to_peer`).
//!
//! Entries are `host:port`, `ip:port`, `[ipv6]:port` or a bare host or IP (then the sidechain's
//! default port). Host names are resolved through the system resolver (address records).

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use tokio::net::TcpStream;

/// How long an outgoing connection may take to open.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Splits an entry into host and port; `default_port` when the entry has none.
/// `None` for an empty entry or a port outside 1..65535.
#[must_use]
pub fn split_host_port(entry: &str, default_port: u16) -> Option<(String, u16)> {
    let entry = entry.trim();
    if entry.is_empty() {
        return None;
    }
    if let Ok(addr) = entry.parse::<SocketAddr>() {
        return (addr.port() != 0).then(|| (addr.ip().to_string(), addr.port()));
    }
    if let Ok(ip) = entry.parse::<IpAddr>() {
        return Some((ip.to_string(), default_port));
    }
    if let Some(inner) = entry.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        return inner
            .parse::<IpAddr>()
            .ok()
            .map(|ip| (ip.to_string(), default_port));
    }
    match entry.rsplit_once(':') {
        Some((host, port)) => {
            let port = port.parse::<u16>().ok().filter(|p| *p != 0)?;
            (!host.is_empty()).then(|| (host.to_owned(), port))
        }
        None => Some((entry.to_owned(), default_port)),
    }
}

/// Resolves entries to socket addresses, in order, without duplicates. Entries that do not
/// resolve are skipped.
pub async fn resolve(entries: &[String], default_port: u16) -> Vec<SocketAddr> {
    let mut out: Vec<SocketAddr> = Vec::new();
    for entry in entries {
        let Some((host, port)) = split_host_port(entry, default_port) else {
            continue;
        };
        if let Ok(ip) = host.parse::<IpAddr>() {
            let addr = SocketAddr::new(ip, port);
            if !out.contains(&addr) {
                out.push(addr);
            }
            continue;
        }
        if let Ok(found) = tokio::net::lookup_host((host.as_str(), port)).await {
            for addr in found {
                if !out.contains(&addr) {
                    out.push(addr);
                }
            }
        }
    }
    out
}

/// Opens a TCP connection within [`CONNECT_TIMEOUT`], with Nagle's algorithm off.
pub async fn dial(addr: SocketAddr) -> io::Result<TcpStream> {
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connect timed out"))??;
    stream.set_nodelay(true)?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_and_port() {
        let s = |e: &str| split_host_port(e, 37889);
        assert_eq!(s("1.2.3.4:20211"), Some(("1.2.3.4".into(), 20211)));
        assert_eq!(s("1.2.3.4"), Some(("1.2.3.4".into(), 37889)));
        assert_eq!(s("[2001:db8::1]:5"), Some(("2001:db8::1".into(), 5)));
        assert_eq!(s("[2001:db8::1]"), Some(("2001:db8::1".into(), 37889)));
        assert_eq!(s("2001:db8::1"), Some(("2001:db8::1".into(), 37889)));
        assert_eq!(
            s("deploy.cloud.cbh.kth.se:20211"),
            Some(("deploy.cloud.cbh.kth.se".into(), 20211))
        );
        assert_eq!(s("seed.example"), Some(("seed.example".into(), 37889)));
        assert_eq!(s("seed.example:0"), None);
        assert_eq!(s("1.2.3.4:0"), None);
        assert_eq!(s(" "), None);
    }
}
