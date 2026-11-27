//! A minimal HTTP/1.1 GET against xkrd's loopback RPC.
//!
//! xkrd's RPC only listens on `127.0.0.1` here and answers small JSON documents, so a blocking
//! `TcpStream` with `Connection: close` is enough and keeps an HTTP client crate out of the
//! dependency tree. xkrd checks the `Host` header (it must name a loopback address) and needs
//! no `Origin` for non-browser clients.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

use serde_json::Value;

/// Largest response accepted (`/status` is a few KB).
const MAX_RESPONSE: u64 = 4 * 1024 * 1024;

/// `GET http://127.0.0.1:<port><path>` and parse the body as JSON.
pub fn get_json(port: u16, path: &str, timeout: Duration) -> Result<Value, String> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, timeout).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(timeout))
        .and_then(|()| stream.set_write_timeout(Some(timeout)))
        .map_err(|e| e.to_string())?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    )
    .map_err(|e| e.to_string())?;

    let mut raw = Vec::new();
    stream
        .take(MAX_RESPONSE)
        .read_to_end(&mut raw)
        .map_err(|e| e.to_string())?;

    let split = find(&raw, b"\r\n\r\n").ok_or("malformed HTTP response")?;
    let head = std::str::from_utf8(&raw[..split]).map_err(|_| "non-UTF-8 HTTP header")?;
    let body = &raw[split + 4..];

    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or("malformed HTTP status line")?;
    let chunked = lines.any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    let body = if chunked {
        dechunk(body)?
    } else {
        body.to_vec()
    };

    if code != 200 {
        return Err(format!(
            "HTTP {code}: {}",
            String::from_utf8_lossy(&body)
                .chars()
                .take(200)
                .collect::<String>()
        ));
    }
    serde_json::from_slice(&body).map_err(|e| format!("bad JSON from {path}: {e}"))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Decodes a `Transfer-Encoding: chunked` body (extensions and trailers ignored).
fn dechunk(mut body: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let eol = find(body, b"\r\n").ok_or("truncated chunk header")?;
        let size_text = std::str::from_utf8(&body[..eol]).map_err(|_| "bad chunk header")?;
        let size_text = size_text.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_text, 16).map_err(|_| "bad chunk size")?;
        body = &body[eol + 2..];
        if size == 0 {
            return Ok(out);
        }
        if body.len() < size + 2 {
            return Err("truncated chunk".into());
        }
        out.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::dechunk;

    #[test]
    fn dechunks() {
        let body = b"4\r\n{\"a\"\r\n3;ext=1\r\n:1}\r\n0\r\n\r\n";
        assert_eq!(dechunk(body).unwrap(), b"{\"a\":1}");
    }
}
