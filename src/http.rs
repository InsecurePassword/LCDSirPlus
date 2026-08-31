//! Minimal loopback HTTP/1.1 GET client for the LibreHardwareMonitor
//! endpoint. Plain-text HTTP only (the caller validates loopback), bounded
//! response size, Content-Length and chunked transfer decoding.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

pub struct Url {
    pub host: String,
    pub port: u16,
    pub path: String,
}

/// Parse `http://host[:port]/path`. HTTPS is rejected (loopback JSON only).
pub fn parse_url(url: &str) -> Result<Url, String> {
    if url
        .bytes()
        .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err("URL contains whitespace or a control character".into());
    }
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| "only http:// loopback URLs are supported".to_string())?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if !path.starts_with('/') || path.contains('#') || authority.contains(['?', '#']) {
        return Err("invalid HTTP request target".into());
    }
    if authority.contains('@') {
        return Err("userinfo is not supported".into());
    }
    let parse_port = |port: &str| {
        if port.is_empty() {
            return Err("empty port".to_string());
        }
        let port = port
            .parse::<u16>()
            .map_err(|_| "invalid port".to_string())?;
        if port == 0 {
            Err("invalid port".to_string())
        } else {
            Ok(port)
        }
    };
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let end = bracketed.find(']').ok_or("invalid IPv6 authority")?;
        let host = &bracketed[..end];
        host.parse::<std::net::Ipv6Addr>()
            .map_err(|_| "invalid IPv6 authority")?;
        let suffix = &bracketed[end + 1..];
        let port = match suffix.strip_prefix(':') {
            Some(p) => parse_port(p)?,
            None if suffix.is_empty() => 80,
            None => return Err("invalid IPv6 authority".into()),
        };
        (host.to_string(), port)
    } else {
        if authority.contains(['[', ']']) || authority.matches(':').count() > 1 {
            return Err("invalid authority".into());
        }
        match authority.split_once(':') {
            Some((h, p)) => (h.to_string(), parse_port(p)?),
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

/// GET the URL body. `timeout` bounds the complete request.
pub fn get(url: &str, timeout: Duration) -> Result<Vec<u8>, String> {
    let parsed = parse_url(url)?;
    if !is_loopback_host(&parsed.host) {
        return Err("refusing non-loopback HTTP request".into());
    }
    let addr = if parsed.host.eq_ignore_ascii_case("localhost") {
        format!("127.0.0.1:{}", parsed.port)
    } else if parsed.host.contains(':') {
        format!("[{}]:{}", parsed.host, parsed.port)
    } else {
        format!("{}:{}", parsed.host, parsed.port)
    };
    let addr = addr
        .parse::<SocketAddr>()
        .map_err(|e| format!("invalid loopback address {}: {}", addr, e))?;
    let host_header = match (parsed.host.contains(':'), parsed.port) {
        (true, 80) => format!("[{}]", parsed.host),
        (true, port) => format!("[{}]:{}", parsed.host, port),
        (false, 80) => parsed.host.clone(),
        (false, port) => format!("{}:{}", parsed.host, port),
    };
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or("timeout exceeds supported duration")?;
    let mut stream = TcpStream::connect_timeout(&addr, remaining(deadline)?)
        .map_err(|e| format!("connect {}: {}", addr, e))?;

    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nAccept: application/json\r\nConnection: close\r\n\r\n",
        parsed.path, host_header
    );
    let mut sent = 0;
    while sent < request.len() {
        stream
            .set_write_timeout(Some(remaining(deadline)?))
            .map_err(|e| format!("write timeout: {}", e))?;
        match stream.write(&request.as_bytes()[sent..]) {
            Ok(0) => return Err("request write: connection closed".into()),
            Ok(n) => sent += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("request write: {}", e)),
        }
    }

    let mut raw = Vec::new();
    let mut chunk = [0u8; 16384];
    loop {
        stream
            .set_read_timeout(Some(remaining(deadline)?))
            .map_err(|e| format!("read timeout: {}", e))?;
        match stream.read(&mut chunk) {
            Ok(0) => {
                remaining(deadline)?;
                return parse_response(&raw);
            }
            Ok(n) => {
                raw.extend_from_slice(&chunk[..n]);
                if raw.len() > MAX_RESPONSE_BYTES {
                    return Err("response exceeds size limit".into());
                }
                if let Some(body) = try_parse_response(&raw, false)? {
                    return Ok(body);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("response read: {}", e)),
        }
    }
}

fn parse_response(raw: &[u8]) -> Result<Vec<u8>, String> {
    try_parse_response(raw, true)?.ok_or_else(|| "incomplete HTTP response".into())
}

fn try_parse_response(raw: &[u8], eof: bool) -> Result<Option<Vec<u8>>, String> {
    let Some(header_end) = find_header_end(raw) else {
        return if eof {
            Err("malformed HTTP response: no header terminator".into())
        } else {
            Ok(None)
        };
    };
    let head = std::str::from_utf8(&raw[..header_end])
        .map_err(|_| "malformed HTTP response: headers are not UTF-8")?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let (version, rest) = status_line.split_once(' ').unwrap_or(("", ""));
    let rest = rest.as_bytes();
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1")
        || rest.len() < 4
        || !rest[..3].iter().all(u8::is_ascii_digit)
        || rest[3] != b' '
        || !rest[4..]
            .iter()
            .all(|byte| *byte == b'\t' || *byte >= b' ' && *byte != 0x7f)
    {
        return Err(format!("malformed HTTP status: {}", status_line));
    }
    let code = &status_line[version.len() + 1..version.len() + 4];
    if code != "200" {
        return Err(format!("HTTP status: {}", status_line));
    }

    let mut chunked = false;
    let mut content_length = None;
    for line in lines {
        let (name, value) =
            parse_field(line).ok_or("malformed HTTP response: invalid header field")?;
        let value = value.trim_matches([' ', '\t']);
        if name.eq_ignore_ascii_case("transfer-encoding") {
            if chunked || !value.eq_ignore_ascii_case("chunked") {
                return Err("unsupported or duplicate Transfer-Encoding".into());
            }
            chunked = true;
        } else if name.eq_ignore_ascii_case("content-length") {
            for value in value.split(',') {
                let value = value.trim_matches([' ', '\t']);
                if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err("invalid Content-Length".into());
                }
                let len = value
                    .parse::<usize>()
                    .map_err(|_| "invalid Content-Length")?;
                if content_length.is_some_and(|current| current != len) {
                    return Err("conflicting Content-Length values".into());
                }
                content_length = Some(len);
            }
        }
    }
    if chunked && content_length.is_some() {
        return Err("conflicting Transfer-Encoding and Content-Length".into());
    }

    let body = &raw[header_end + 4..];
    if chunked {
        try_decode_chunked(body, eof)
    } else if let Some(len) = content_length {
        if body.len() < len {
            if eof {
                Err(format!("short body: {} of {} bytes", body.len(), len))
            } else {
                Ok(None)
            }
        } else if body.len() > len {
            Err("malformed HTTP response: bytes after Content-Length body".into())
        } else {
            Ok(Some(body.to_vec()))
        }
    } else if eof {
        Ok(Some(body.to_vec()))
    } else {
        Ok(None)
    }
}

fn remaining(deadline: Instant) -> Result<Duration, String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err("request timed out".into())
    } else {
        Ok(remaining)
    }
}

fn find_header_end(raw: &[u8]) -> Option<usize> {
    raw.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_field(line: &str) -> Option<(&str, &str)> {
    let (name, value) = line.split_once(':')?;
    let valid_name = !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte));
    let valid_value = value
        .bytes()
        .all(|byte| byte == b'\t' || byte >= b' ' && byte != 0x7f);
    (valid_name && valid_value).then_some((name, value))
}

#[cfg(test)]
fn decode_chunked(body: &[u8]) -> Result<Vec<u8>, String> {
    try_decode_chunked(body, true)?.ok_or_else(|| "incomplete chunked body".into())
}

fn try_decode_chunked(body: &[u8], eof: bool) -> Result<Option<Vec<u8>>, String> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    loop {
        let Some(line_end) = body[pos..].windows(2).position(|w| w == b"\r\n") else {
            return if eof {
                Err("malformed chunked body: no chunk size".into())
            } else {
                Ok(None)
            };
        };
        let line_end = line_end + pos;
        let size_text = &body[pos..line_end];
        if size_text.is_empty() || !size_text.iter().all(u8::is_ascii_hexdigit) {
            return Err(format!("invalid chunk size {:?}", size_text));
        }
        let size_text = std::str::from_utf8(size_text).expect("ASCII hex is UTF-8");
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| format!("invalid chunk size {:?}", size_text))?;
        pos = line_end + 2;
        if size == 0 {
            loop {
                let Some(trailer_end) = body[pos..].windows(2).position(|w| w == b"\r\n") else {
                    return if eof {
                        Err("malformed chunked body: truncated trailer".into())
                    } else {
                        Ok(None)
                    };
                };
                let trailer_end = trailer_end + pos;
                let trailer = std::str::from_utf8(&body[pos..trailer_end])
                    .map_err(|_| "malformed chunked body: invalid trailer")?;
                pos = trailer_end + 2;
                if trailer.is_empty() {
                    return if pos == body.len() {
                        Ok(Some(out))
                    } else {
                        Err("malformed chunked body: bytes after trailer".into())
                    };
                }
                if parse_field(trailer).is_none() {
                    return Err("malformed chunked body: invalid trailer".into());
                }
            }
        }
        let data_end = pos
            .checked_add(size)
            .ok_or("malformed chunked body: chunk size overflow")?;
        let chunk_end = data_end
            .checked_add(2)
            .ok_or("malformed chunked body: chunk size overflow")?;
        if chunk_end > body.len() {
            return if eof {
                Err("malformed chunked body: truncated chunk".into())
            } else {
                Ok(None)
            };
        }
        if body.get(data_end..chunk_end) != Some(b"\r\n") {
            return Err("malformed chunked body: invalid chunk terminator".into());
        }
        if out
            .len()
            .checked_add(size)
            .is_none_or(|len| len > MAX_RESPONSE_BYTES)
        {
            return Err("chunked response exceeds size limit".into());
        }
        out.extend_from_slice(&body[pos..data_end]);
        pos = chunk_end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture_request(listener: std::net::TcpListener) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut chunk = [0; 1024];
            while find_header_end(&request).is_none() {
                let read = stream.read(&mut chunk).unwrap();
                assert_ne!(read, 0, "client closed before completing request");
                request.extend_from_slice(&chunk[..read]);
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
            request
        })
    }

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
        let u = parse_url("http://[::1]:8085/data.json").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("::1", 8085));
        for url in [
            "http://user:pass@localhost:8080/x",
            "http://::1/data.json",
            "http://localhost:/data.json",
            "http://localhost:80:90/data.json",
            "http://[::1]:/data.json",
            "http://[::1]:80:90/data.json",
            "http://[localhost]/data.json",
            "http://localhost]/data.json",
            "http://localhost/data json",
            "http://localhost/data\tjson",
            "http://localhost/data.json\r\nX-Injected: yes",
            "http://localhost/data.json#fragment",
            "http://localhost?query",
        ] {
            assert!(parse_url(url).is_err(), "accepted {url:?}");
        }
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
        assert_eq!(decode_chunked(b"0\r\n\r\n").unwrap(), Vec::<u8>::new());
        assert_eq!(
            decode_chunked(b"1\r\nA\r\n0\r\nX-Checksum: ok\r\n\r\n").unwrap(),
            b"A"
        );
        assert!(decode_chunked(b"zz\r\n").is_err());
        assert!(decode_chunked(b"10\r\nshort").is_err());
        assert!(decode_chunked(b"1\r\nA").is_err());
        assert!(decode_chunked(b"1\r\nAxx0\r\n\r\n").is_err());
        assert!(decode_chunked(b"0\r\n").is_err());
        assert!(decode_chunked(b"0\r\nnot-a-field\r\n\r\n").is_err());
        assert!(decode_chunked(b"0\r\n\r\ntrailing").is_err());
        for body in [
            &b" 1\r\nA\r\n0\r\n\r\n"[..],
            &b"1 \r\nA\r\n0\r\n\r\n"[..],
            &b"1;extension=value\r\nA\r\n0\r\n\r\n"[..],
            &b"\t1\r\nA\r\n0\r\n\r\n"[..],
        ] {
            assert!(decode_chunked(body).is_err(), "accepted {body:?}");
        }

        let overflow = format!("{:x}\r\n", usize::MAX);
        assert!(decode_chunked(overflow.as_bytes()).is_err());
    }

    #[test]
    fn total_deadline_stops_drip_response() {
        use std::net::TcpListener;
        use std::thread;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
                .unwrap();
            for _ in 0..50 {
                thread::sleep(Duration::from_millis(40));
                if stream.write_all(b"x").is_err() {
                    break;
                }
            }
        });

        let timeout = Duration::from_millis(250);
        let started = Instant::now();
        let result = get(&format!("http://{addr}/data.json"), timeout);
        let elapsed = started.elapsed();
        server.join().unwrap();

        assert!(result.is_err(), "drip response unexpectedly succeeded");
        assert!(elapsed < Duration::from_secs(1), "elapsed: {elapsed:?}");
    }

    #[test]
    fn request_includes_ipv4_nondefault_port_in_host() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = capture_request(listener);

        assert_eq!(
            get(
                &format!("http://127.0.0.1:{port}/data.json"),
                Duration::from_secs(1)
            )
            .unwrap(),
            Vec::<u8>::new()
        );
        assert_eq!(
            server.join().unwrap(),
            format!(
                "GET /data.json HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
            )
            .as_bytes()
        );
    }

    #[test]
    fn request_brackets_ipv6_and_includes_nondefault_port() {
        let Ok(listener) = std::net::TcpListener::bind("[::1]:0") else {
            return;
        };
        let port = listener.local_addr().unwrap().port();
        let server = capture_request(listener);

        assert_eq!(
            get(
                &format!("http://[::1]:{port}/data.json"),
                Duration::from_secs(1)
            )
            .unwrap(),
            Vec::<u8>::new()
        );
        assert_eq!(
            server.join().unwrap(),
            format!(
                "GET /data.json HTTP/1.1\r\nHost: [::1]:{port}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
            )
            .as_bytes()
        );
    }

    #[test]
    fn framed_keepalive_responses_finish_without_eof() {
        use std::net::TcpListener;
        use std::thread;

        for (response, expected) in [
            (
                &b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello"[..],
                &b"hello"[..],
            ),
            (
                &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\nX-Trailer: ok\r\n\r\n"[..],
                &b"hello"[..],
            ),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream.write_all(response).unwrap();
                thread::sleep(Duration::from_millis(700));
            });

            let timeout = Duration::from_millis(500);
            let started = Instant::now();
            let body = get(&format!("http://{addr}/data.json"), timeout).unwrap();
            let elapsed = started.elapsed();
            server.join().unwrap();

            assert_eq!(body, expected);
            assert!(elapsed < timeout, "waited for EOF: {elapsed:?}");
        }
    }

    #[test]
    fn incomplete_content_length_response_fails() {
        use std::net::TcpListener;
        use std::thread;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhi")
                .unwrap();
            thread::sleep(Duration::from_millis(400));
        });

        let result = get(
            &format!("http://{addr}/data.json"),
            Duration::from_millis(150),
        );
        server.join().unwrap();

        assert!(
            result.is_err(),
            "incomplete response unexpectedly succeeded"
        );
    }

    #[test]
    fn response_status_and_framing() {
        assert_eq!(
            parse_response(b"HTTP/1.1 200 \r\nContent-Length: 0\r\n\r\n").unwrap(),
            Vec::<u8>::new()
        );
        for response in [
            &b"HTTP/1.1 200\r\n\r\n"[..],
            &b"HTTP/1.1  200 OK\r\n\r\n"[..],
            &b"HTTP/1.1\t200 OK\r\n\r\n"[..],
            &b"HTTP/1.1 20 OK\r\n\r\n"[..],
            &b"HTTP/1.1 2000 OK\r\n\r\n"[..],
            &b"HTTP/1.1 +200 OK\r\n\r\n"[..],
        ] {
            assert!(parse_response(response).is_err(), "accepted {response:?}");
        }
        assert_eq!(
            parse_response(b"HTTP/1.1 200 OK\r\nX-Transfer-Encoding: chunked\r\n\r\nplain")
                .unwrap(),
            b"plain"
        );
        assert!(parse_response(
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\nab"
        )
        .is_err());
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: nope\r\n\r\nbody").is_err());
        assert!(parse_response(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 0\r\n\r\n0\r\n\r\n"
        )
        .is_err());
        assert!(parse_response(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked, gzip\r\n\r\n0\r\n\r\n"
        )
        .is_err());
        assert!(parse_response(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n"
        )
        .is_err());
        assert_eq!(
            parse_response(
                b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\nhello"
            )
            .unwrap(),
            b"hello"
        );
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\nab").is_err());
        assert_eq!(
            parse_response(b"HTTP/1.0 200 OK\r\nConnection: close\r\n\r\nclose body").unwrap(),
            b"close body"
        );
        assert_eq!(
            parse_response(
                b"HTTP/1.1 200 OK\r\nTrAnSfEr-EnCoDiNg: ChUnKeD\r\n\r\n1\r\nA\r\n0\r\n\r\n"
            )
            .unwrap(),
            b"A"
        );
    }
}
