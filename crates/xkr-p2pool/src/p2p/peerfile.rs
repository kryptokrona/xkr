//! The saved peer list `p2pool_peers.txt` (`P2PServer::save_peer_list`,
//! `P2PServer::load_peer_list`, `parse_address_list`): one `ip:port` per line, IPv6 as
//! `[ip]:port`. The file is read and written in the same format as the C++ p2pool, so a data
//! directory of either can be used by the other.

use std::fmt::Write as _;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;

use super::message::PeerEntry;

/// `saved_peer_list_file_name`.
pub const PEER_FILE_NAME: &str = "p2pool_peers.txt";

/// Reads an address list: entries separated by commas or line breaks, each `ip:port` or
/// `[ip]:port`. Entries that are not an IP address with a non-zero port are skipped (host
/// names belong in the seed list, which is resolved separately).
#[must_use]
pub fn parse_peer_list(text: &str) -> Vec<PeerEntry> {
    text.split([',', '\n', '\r'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(parse_entry)
        .collect()
}

/// One `ip:port` or `[ip]:port` entry.
fn parse_entry(s: &str) -> Option<PeerEntry> {
    let addr: SocketAddr = s.parse().ok()?;
    if addr.port() == 0 {
        return None;
    }
    Some(PeerEntry::from_socket_addr(addr).normalized())
}

/// The text of the file: one address per line, IPv4 as `a.b.c.d:port`, IPv6 as `[ip]:port`.
#[must_use]
pub fn format_peer_list<'a>(entries: impl IntoIterator<Item = &'a PeerEntry>) -> String {
    let mut out = String::new();
    for e in entries {
        let addr = e.socket_addr();
        // writing to a String cannot fail
        let _ = match addr.ip() {
            IpAddr::V4(v4) => writeln!(out, "{v4}:{}", addr.port()),
            IpAddr::V6(v6) => writeln!(out, "[{v6}]:{}", addr.port()),
        };
    }
    out
}

/// Loads the file; a missing file is an empty list.
pub fn load(path: &Path) -> io::Result<Vec<PeerEntry>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(parse_peer_list(&text)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// Writes the file through a temporary file next to it, so a stop in the middle never leaves
/// half a list.
pub fn save<'a>(path: &Path, entries: impl IntoIterator<Item = &'a PeerEntry>) -> io::Result<()> {
    let text = format_peer_list(entries);
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_round_trip() {
        let text = "1.2.3.4:37889\n[2001:db8::1]:37890\n\n5.6.7.8:20211,9.9.9.9:0\nnot-an-ip:1\n";
        let list = parse_peer_list(text);
        assert_eq!(list.len(), 3);
        assert_eq!(
            format_peer_list(&list),
            "1.2.3.4:37889\n[2001:db8::1]:37890\n5.6.7.8:20211\n"
        );
        assert_eq!(parse_peer_list(&format_peer_list(&list)), list);
        assert!(!list[0].is_v6);
        assert!(list[1].is_v6);
    }

    #[test]
    fn mapped_ipv6_is_written_as_ipv4() {
        let list = parse_peer_list("[::ffff:1.2.3.4]:37889");
        assert_eq!(list.len(), 1);
        assert!(!list[0].is_v6);
        assert_eq!(format_peer_list(&list), "1.2.3.4:37889\n");
    }
}
