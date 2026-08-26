//! Minimal loopback HTTP/1.1 GET client for the LibreHardwareMonitor
//! endpoint. Plain-text HTTP only (the caller validates loopback), bounded
//! response size, Content-Length and chunked transfer decoding.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

pub struct Url {
    pub host: String,
    pub port: u16,
    pub path: String,
}

/// Parse `http://host[:port]/path`. HTTPS is rejected (loopback JSON only).
pub fn parse_url(url: &str) -> Result<Url, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| "only http:// loopback URLs are supported".to_string())?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let end = bracketed.find(']').ok_or("invalid IPv6 authority")?;
        let host = &bracketed[..end];
        let suffix = &bracketed[end + 1..];
        let port = match suffix.strip_prefix(':') {
            Some(p) => p.parse::<u16>().map_err(|_| "invalid port".to_string())?,
            None if suffix.is_empty() => 80,
            None => return Err("invalid IPv6 authority".into()),
        };
        (host.to_string(), port)
    } else if authority.matches(':').count() > 1 {
        (authority.to_string(), 80)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (
                h.to_string(),
                p.parse::<u16>().map_err(|_| "invalid port".to_string())?,
            ),
            None => (authority.to_string(), 80),
        }
    };
    if host.is_empty() {
        return Err("empty host".into());
    }
    Ok(Url {
        host,
        port,
        path: path.to_string(),
    })
}

pub fn is_loopback_host(host: &str) -> bool {
    matches!(
        host.to_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "::1"
    )
}

/// GET the URL body. `timeout` bounds connect and each read.
pub fn get(url: &str, timeout: Duration) -> Result<Vec<u8>, String> {
    let parsed = parse_url(url)?;
    if !is_loopback_host(&parsed.host) {
        return Err("refusing non-loopback HTTP request".into());
    }
    let addr = if parsed.host.contains(':') {
        format!("[{}]:{}", parsed.host, parsed.port)
    } else {
        format!("{}:{}", parsed.host, parsed.port)
    };
    let mut stream = TcpStream::connect(&addr).map_err(|e| format!("connect {}: {}", addr, e))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| format!("read timeout: {}", e))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| format!("write timeout: {}", e))?;

    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nAccept: application/json\r\nConnection: close\r\n\r\n",
        parsed.path, parsed.host
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("request write: {}", e))?;

    let mut raw = Vec::new();
    let mut chunk = [0u8; 16384];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&chunk[..n]);
                if raw.len() > MAX_RESPONSE_BYTES {
                    return Err("response exceeds size limit".into());
                }
            }
            Err(e) => {
                if raw.is_empty() {
                    return Err(format!("response read: {}", e));
                }
                break; // partial body after Connection: close is acceptable
            }
        }
    }

    let header_end =
        find_header_end(&raw).ok_or("malformed HTTP response: no header terminator")?;
    let headers = String::from_utf8_lossy(&raw[..header_end]).to_lowercase();
    let body = raw[header_end + 4..].to_vec();

    let status_line = headers.lines().next().unwrap_or("");
    if !status_line.contains("200") {
        return Err(format!("HTTP status: {}", status_line));
    }

    if headers.contains("transfer-encoding: chunked") {
        decode_chunked(&body)
    } else if let Some(len) = content_length(&headers) {
        if body.len() < len {
            Err(format!("short body: {} of {} bytes", body.len(), len))
        } else {
            Ok(body[..len].to_vec())
        }
    } else {
        Ok(body)
    }
}

fn find_header_end(raw: &[u8]) -> Option<usize> {
    raw.windows(4).position(|w| w == b"\r\n\r\n")
}

fn content_length(headers: &str) -> Option<usize> {
    headers.lines().find_map(|line| {
        let value = line.strip_prefix("content-length:")?;
        value.trim().parse::<usize>().ok()
    })
}

fn decode_chunked(body: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    loop {
        let line_end = body[pos..]
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or("malformed chunked body: no chunk size")?
            + pos;
        let size_text = String::from_utf8_lossy(&body[pos..line_end]);
        let size_text = size_text.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| format!("invalid chunk size {:?}", size_text))?;
        pos = line_end + 2;
        if size == 0 {
            return Ok(out);
        }
        if pos + size > body.len() {
            return Err("malformed chunked body: truncated chunk".into());
        }
        out.extend_from_slice(&body[pos..pos + size]);
        pos += size + 2; // skip chunk + CRLF
        if out.len() > MAX_RESPONSE_BYTES {
            return Err("chunked response exceeds size limit".into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_parsing() {
        let u = parse_url("http://127.0.0.1:8085/data.json").unwrap();
        assert_eq!(
            (u.host.as_str(), u.port, u.path.as_str()),
            ("127.0.0.1", 8085, "/data.json")
        );
        let u = parse_url("http://localhost/data.json").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("localhost", 80));
        assert!(parse_url("https://127.0.0.1/x").is_err());
        let u = parse_url("http://user:pass@host:8080/x").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("host", 8080)); // caller validates loopback
        let u = parse_url("http://[::1]:8085/data.json").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("::1", 8085));
    }

    #[test]
    fn loopback_check() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("LOCALHOST"));
        assert!(is_loopback_host("::1"));
        assert!(!is_loopback_host("192.168.1.5"));
        assert!(!is_loopback_host("example.com"));
    }

    #[test]
    fn chunked_decoding() {
        let body = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        assert_eq!(decode_chunked(body).unwrap(), b"Wikipedia".to_vec());
        assert!(decode_chunked(b"zz\r\n").is_err());
        assert!(decode_chunked(b"10\r\nshort").is_err());
    }

    #[test]
    fn content_length_extraction() {
        assert_eq!(content_length("content-length: 42\r\nother: x"), Some(42));
        assert_eq!(content_length("no length here"), None);
    }
}
