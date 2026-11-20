//! Pull an HTTP or HTTPS audio stream into a mount.
//!
//! Only `http` and `https` are accepted. `file:` and every other scheme are
//! rejected. Link-local, loopback, and private addresses are rejected unless
//! the config flag allows them. Paths under `/api` and `/admin`, and the
//! control listener itself, are never requested.

use crate::error::Error;
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Policy {
    pub allow_private: bool,
    pub deny: Vec<SocketAddr>,
}

impl Default for Policy {
    fn default() -> Self {
        Self { allow_private: false, deny: Vec::new() }
    }
}

pub struct Pull {
    pub content_type: String,
    pub metaint: Option<usize>,
    pub reader: Box<dyn Read + Send>,
    pub shutdown: Option<Box<dyn Fn() + Send>>,
}

pub fn validate(url: &str, policy: &Policy) -> Result<(), Error> {
    let parts = parse_url(url)?;
    let addrs = resolve(&parts)?;
    ensure_targets(&parts, &addrs, policy)?;
    Ok(())
}

pub fn open(url: &str, policy: &Policy) -> Result<Pull, Error> {
    open_depth(url, policy, 0)
}

fn open_depth(url: &str, policy: &Policy, depth: u8) -> Result<Pull, Error> {
    if depth > 3 {
        return Err(Error::Invalid("relay redirected too many times".into()));
    }
    let parts = parse_url(url)?;
    let addrs = resolve(&parts)?;
    let allowed = ensure_targets(&parts, &addrs, policy)?;
    let mut last = io::Error::new(io::ErrorKind::ConnectionRefused, "relay connect failed");
    for addr in allowed {
        match dial(&parts, addr) {
            Ok(conn) => {
                let pull = read_response(conn, &parts, policy, depth)?;
                return Ok(pull);
            }
            Err(err) => last = err,
        }
    }
    Err(Error::Invalid(format!("relay connect failed: {last}")))
}

struct Parts {
    https: bool,
    host: String,
    port: u16,
    path: String,
    origin: String,
}

enum Wire {
    Plain(TcpStream),
    Tls(rustls::StreamOwned<rustls::ClientConnection, TcpStream>),
}

impl Read for Wire {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Wire::Plain(s) => s.read(buf),
            Wire::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Wire {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Wire::Plain(s) => s.write(buf),
            Wire::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Wire::Plain(s) => s.flush(),
            Wire::Tls(s) => s.flush(),
        }
    }
}

struct Conn {
    io: Wire,
    shutdown: Option<Box<dyn Fn() + Send>>,
}

fn parse_url(raw: &str) -> Result<Parts, Error> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(Error::Invalid("relay url is empty".into()));
    }
    if raw.len() > 2048 {
        return Err(Error::Invalid("relay url is too long".into()));
    }
    if raw.chars().any(|c| c.is_control() || c == ' ' || c == '\\') {
        return Err(Error::Invalid("relay url must be http or https".into()));
    }
    let lower = raw.to_ascii_lowercase();
    let (https, rest) = if let Some(rest) = lower_strip(&raw, &lower, "https://") {
        (true, rest)
    } else if let Some(rest) = lower_strip(&raw, &lower, "http://") {
        (false, rest)
    } else {
        return Err(Error::Invalid("relay url must be http or https".into()));
    };
    if rest.is_empty() || rest.contains('@') {
        return Err(Error::Invalid("relay url must be http or https without user info".into()));
    }
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, "/".to_string()),
    };
    if authority.is_empty() {
        return Err(Error::Invalid("relay url is missing a host".into()));
    }
    let (host, port) = split_host_port(authority, if https { 443 } else { 80 })?;
    let path_only = path.split('?').next().unwrap_or(path.as_str());
    if is_admin_path(path_only) {
        return Err(Error::Invalid("relay url must not target the admin API".into()));
    }
    let origin = format!("{}://{authority}", if https { "https" } else { "http" });
    Ok(Parts { https, host, port, path, origin })
}

fn lower_strip<'a>(raw: &'a str, lower: &str, prefix: &str) -> Option<&'a str> {
    if lower.starts_with(prefix) { Some(&raw[prefix.len()..]) } else { None }
}

fn split_host_port(authority: &str, default_port: u16) -> Result<(String, u16), Error> {
    if let Some(rest) = authority.strip_prefix('[') {
        let Some((host, tail)) = rest.split_once(']') else {
            return Err(Error::Invalid("relay url has a bad host".into()));
        };
        if host.is_empty() {
            return Err(Error::Invalid("relay url is missing a host".into()));
        }
        let port = if let Some(p) = tail.strip_prefix(':') {
            parse_port(p)?
        } else if tail.is_empty() {
            default_port
        } else {
            return Err(Error::Invalid("relay url has a bad host".into()));
        };
        return Ok((host.to_string(), port));
    }
    if let Some((host, port)) = authority.rsplit_once(':') {
        if !port.is_empty() && port.bytes().all(|c| c.is_ascii_digit()) && !host.contains(':') {
            return Ok((host.to_string(), parse_port(port)?));
        }
    }
    Ok((authority.to_string(), default_port))
}

fn parse_port(port: &str) -> Result<u16, Error> {
    port.parse::<u16>().map_err(|_| Error::Invalid("relay url has a bad port".into())).and_then(|p| {
        if p == 0 { Err(Error::Invalid("relay url has a bad port".into())) } else { Ok(p) }
    })
}

fn is_admin_path(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p == "/api" || p.starts_with("/api/") || p == "/admin" || p.starts_with("/admin/")
}

fn resolve(parts: &Parts) -> Result<Vec<SocketAddr>, Error> {
    let hostport = if parts.host.contains(':') {
        format!("[{}]:{}", parts.host, parts.port)
    } else {
        format!("{}:{}", parts.host, parts.port)
    };
    let addrs: Vec<SocketAddr> = hostport.to_socket_addrs()
        .map_err(|_| Error::Invalid(format!("relay host {} did not resolve", parts.host)))?
        .collect();
    if addrs.is_empty() {
        return Err(Error::Invalid(format!("relay host {} did not resolve", parts.host)));
    }
    Ok(addrs)
}

fn ensure_targets(parts: &Parts, addrs: &[SocketAddr], policy: &Policy) -> Result<Vec<SocketAddr>, Error> {
    let mut allowed = Vec::new();
    for addr in addrs {
        if !policy.allow_private && is_blocked_ip(addr.ip()) {
            return Err(Error::Invalid("relay address is private, link-local, or loopback".into()));
        }
        if policy.deny.iter().any(|d| d.ip() == addr.ip() && d.port() == addr.port()) {
            return Err(Error::Invalid("relay url must not target the admin API".into()));
        }
        allowed.push(*addr);
    }
    let _ = parts;
    if allowed.is_empty() {
        return Err(Error::Invalid("relay address was rejected".into()));
    }
    Ok(allowed)
}

pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            v.is_private()
                || v.is_loopback()
                || v.is_link_local()
                || v.is_broadcast()
                || v.is_unspecified()
                || v.is_documentation()
                || v.is_multicast()
                || is_cgnat(v)
                || v.octets()[0] == 0
        }
        IpAddr::V6(v) => {
            if let Some(v4) = v.to_ipv4_mapped() {
                return is_blocked_ip(IpAddr::V4(v4));
            }
            v.is_loopback() || v.is_unspecified() || v.is_multicast() || is_ula(v) || is_v6_link_local(v)
        }
    }
}

fn is_cgnat(v: Ipv4Addr) -> bool {
    let o = v.octets();
    o[0] == 100 && (o[1] & 0xc0) == 0x40
}

fn is_ula(v: Ipv6Addr) -> bool {
    (v.segments()[0] & 0xfe00) == 0xfc00
}

fn is_v6_link_local(v: Ipv6Addr) -> bool {
    (v.segments()[0] & 0xffc0) == 0xfe80
}

fn dial(parts: &Parts, addr: SocketAddr) -> io::Result<Conn> {
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    tcp.set_read_timeout(Some(Duration::from_secs(5)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(5)))?;
    let _ = tcp.set_nodelay(true);
    let killer = tcp.try_clone().ok();
    let shutdown = killer.map(|sock| {
        let f: Box<dyn Fn() + Send> = Box::new(move || {
            let _ = sock.shutdown(Shutdown::Both);
        });
        f
    });
    let io = if parts.https {
        Wire::Tls(tls_wrap(tcp, &parts.host)?)
    } else {
        Wire::Plain(tcp)
    };
    Ok(Conn { io, shutdown })
}

fn tls_wrap(tcp: TcpStream, host: &str) -> io::Result<rustls::StreamOwned<rustls::ClientConnection, TcpStream>> {
    install_ring();
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "relay tls name rejected"))?;
    let conn = rustls::ClientConnection::new(Arc::new(config), name)
        .map_err(|err| io::Error::other(err.to_string()))?;
    Ok(rustls::StreamOwned::new(conn, tcp))
}

fn install_ring() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

fn read_response(mut conn: Conn, parts: &Parts, policy: &Policy, depth: u8) -> Result<Pull, Error> {
    let req = format!(
        "GET {} HTTP/1.0\r\nHost: {}\r\nUser-Agent: navad\r\nIcy-MetaData: 1\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        parts.path,
        host_header(parts),
    );
    conn.io.write_all(req.as_bytes()).map_err(|e| Error::Invalid(format!("relay request failed: {e}")))?;
    let (status, headers, rest) = read_head(&mut conn.io).map_err(|e| Error::Invalid(format!("relay response failed: {e}")))?;
    if matches!(status, 301 | 302 | 303 | 307 | 308) {
        let loc = header(&headers, "location").ok_or_else(|| Error::Invalid("relay redirect is missing a location".into()))?;
        let next = resolve_redirect(&parts.origin, &loc)?;
        return open_depth(&next, policy, depth + 1);
    }
    if status != 200 && status != 206 {
        return Err(Error::Invalid(format!("relay responded {status}")));
    }
    let content_type = header(&headers, "content-type").unwrap_or_else(|| "audio/mpeg".into());
    let metaint = header(&headers, "icy-metaint").and_then(|s| s.parse().ok()).filter(|n| (16..=65_536).contains(n));
    Ok(Pull {
        content_type,
        metaint,
        reader: Box::new(Prefixed { rest, pos: 0, io: conn.io }),
        shutdown: conn.shutdown,
    })
}

/// Bytes already pulled past the response head stay in `rest`. The cap is the header, not the body.
struct Prefixed {
    rest: Vec<u8>,
    pos: usize,
    io: Wire,
}

impl Read for Prefixed {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos < self.rest.len() {
            let n = (self.rest.len() - self.pos).min(buf.len());
            buf[..n].copy_from_slice(&self.rest[self.pos..self.pos + n]);
            self.pos += n;
            if self.pos == self.rest.len() {
                self.rest.clear();
                self.pos = 0;
            }
            return Ok(n);
        }
        match &mut self.io {
            Wire::Plain(s) => s.read(buf),
            Wire::Tls(s) => s.read(buf),
        }
    }
}

fn host_header(parts: &Parts) -> String {
    let default = if parts.https { 443 } else { 80 };
    if parts.port == default {
        if parts.host.contains(':') { format!("[{}]", parts.host) } else { parts.host.clone() }
    } else if parts.host.contains(':') {
        format!("[{}]:{}", parts.host, parts.port)
    } else {
        format!("{}:{}", parts.host, parts.port)
    }
}

fn resolve_redirect(origin: &str, location: &str) -> Result<String, Error> {
    let location = location.trim();
    if location.to_ascii_lowercase().starts_with("http://") || location.to_ascii_lowercase().starts_with("https://") {
        return Ok(location.to_string());
    }
    if let Some(path) = location.strip_prefix('/') {
        return Ok(format!("{origin}/{path}"));
    }
    Err(Error::Invalid("relay redirect is not an http(s) url".into()))
}

const MAX_RELAY_HEAD: usize = 8 * 1024;

fn header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

fn read_head(reader: &mut dyn Read) -> io::Result<(u16, HashMap<String, String>, Vec<u8>)> {
    let mut buf = Vec::with_capacity(512);
    let mut tmp = [0u8; 1024];
    loop {
        if let Some(end) = header_end(&buf) {
            let rest = buf.split_off(end);
            return parse_head(&buf, rest);
        }
        if buf.len() >= MAX_RELAY_HEAD {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "relay response headers are too large"));
        }
        let n = reader.read(&mut tmp)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "relay headers were incomplete"));
        }
        let room = MAX_RELAY_HEAD - buf.len();
        let take = n.min(room);
        buf.extend_from_slice(&tmp[..take]);
        if let Some(end) = header_end(&buf) {
            let mut rest = buf.split_off(end);
            if take < n {
                rest.extend_from_slice(&tmp[take..n]);
            }
            return parse_head(&buf, rest);
        }
        if take < n {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "relay response headers are too large"));
        }
    }
}

fn parse_head(buf: &[u8], rest: Vec<u8>) -> io::Result<(u16, HashMap<String, String>, Vec<u8>)> {
    let text = String::from_utf8_lossy(buf);
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status = status_code(status_line).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "relay status line"))?;
    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if headers.len() >= 64 || line.len() > 4 * 1024 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "relay response headers are too large"));
        }
        if let Some((k, v)) = line.split_once(':') {
            let value = v.trim();
            if value.len() > 4 * 1024 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "relay response headers are too large"));
            }
            headers.insert(k.trim().to_ascii_lowercase(), value.to_string());
        }
    }
    Ok((status, headers, rest))
}

fn status_code(line: &str) -> Option<u16> {
    let mut parts = line.split_whitespace();
    let proto = parts.next()?;
    if !(proto.starts_with("HTTP/") || proto.eq_ignore_ascii_case("ICY")) {
        return None;
    }
    parts.next()?.parse().ok()
}

fn header(headers: &HashMap<String, String>, name: &str) -> Option<String> {
    headers.get(name).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_scheme_and_private_and_admin_are_rejected() {
        let closed = Policy::default();
        let open = Policy { allow_private: true, deny: vec!["127.0.0.1:8088".parse().unwrap()] };
        assert!(validate("file:///tmp/a.mp3", &closed).is_err());
        assert!(validate("ftp://example.com/a", &closed).is_err());
        assert!(validate("/etc/passwd", &closed).is_err());
        assert!(validate("http://10.1.2.3/live", &closed).is_err());
        assert!(validate("http://192.168.1.5/live", &closed).is_err());
        assert!(validate("http://127.0.0.1/live", &closed).is_err());
        assert!(validate("http://169.254.1.1/live", &closed).is_err());
        assert!(validate("http://[::1]/live", &closed).is_err());
        assert!(validate("http://127.0.0.1:9/api/health", &open).is_err());
        assert!(validate("http://127.0.0.1:9/admin", &open).is_err());
        assert!(validate("http://127.0.0.1:8088/live", &open).is_err());
        assert!(validate("http://93.184.216.34/live", &closed).is_ok());
        assert!(validate("https://93.184.216.34/stream", &closed).is_ok());
        assert!(validate("http://127.0.0.1:9/track.mp3", &open).is_ok());
    }

    fn local_http(body: Vec<u8>) -> std::net::SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            let mut n = 0;
            while n < 8 && std::time::Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut sock, _)) => {
                        let _ = sock.set_read_timeout(Some(Duration::from_millis(300)));
                        let mut buf = [0u8; 2048];
                        let _ = sock.read(&mut buf);
                        let _ = sock.write_all(&body);
                        let _ = sock.shutdown(std::net::Shutdown::Both);
                        n += 1;
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(10)),
                    Err(_) => break,
                }
            }
        });
        addr
    }

    #[test]
    fn relay_body_starts_after_the_headers() {
        let addr = local_http(b"HTTP/1.0 200 OK\r\nContent-Type: audio/mpeg\r\n\r\nBODYDATA".to_vec());
        let policy = Policy { allow_private: true, deny: Vec::new() };
        let mut pull = open(&format!("http://{addr}/live"), &policy).unwrap();
        let mut buf = String::new();
        pull.reader.read_to_string(&mut buf).unwrap();
        assert_eq!(buf, "BODYDATA");
        assert_eq!(pull.content_type, "audio/mpeg");
    }

    #[test]
    fn relay_rejects_oversized_headers_and_admin_redirects() {
        let huge = {
            let mut resp = b"HTTP/1.0 200 OK\r\nX-Big: ".to_vec();
            resp.extend(std::iter::repeat(b'a').take(9_000));
            resp.extend_from_slice(b"\r\n\r\n");
            resp
        };
        let addr = local_http(huge);
        let policy = Policy { allow_private: true, deny: Vec::new() };
        let err = match open(&format!("http://{addr}/live"), &policy) {
            Err(err) => err,
            Ok(_) => panic!("oversized relay headers were accepted"),
        };
        assert!(err.to_string().contains("too large"), "{err}");

        let admin: std::net::SocketAddr = "127.0.0.1:9".parse().unwrap();
        let hop = local_http(b"HTTP/1.0 302 Found\r\nLocation: http://127.0.0.1:9/live\r\nContent-Length: 0\r\n\r\n".to_vec());
        let policy = Policy { allow_private: true, deny: vec![admin] };
        let err = match open(&format!("http://{hop}/live"), &policy) {
            Err(err) => err,
            Ok(_) => panic!("redirect onto the admin port was accepted"),
        };
        assert!(err.to_string().contains("admin"), "{err}");

        let api = local_http(b"HTTP/1.0 302 Found\r\nLocation: /api/health\r\nContent-Length: 0\r\n\r\n".to_vec());
        let err = match open(&format!("http://{api}/live"), &policy) {
            Err(err) => err,
            Ok(_) => panic!("redirect onto /api was accepted"),
        };
        assert!(err.to_string().contains("admin"), "{err}");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let loop_addr = listener.local_addr().unwrap();
        let body = format!("HTTP/1.0 302 Found\r\nLocation: http://{loop_addr}/live\r\nContent-Length: 0\r\n\r\n");
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            let mut n = 0;
            while n < 8 && std::time::Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut sock, _)) => {
                        let _ = sock.set_read_timeout(Some(Duration::from_millis(300)));
                        let mut buf = [0u8; 1024];
                        let _ = sock.read(&mut buf);
                        let _ = sock.write_all(body.as_bytes());
                        let _ = sock.shutdown(std::net::Shutdown::Both);
                        n += 1;
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(10)),
                    Err(_) => break,
                }
            }
        });
        let err = match open(&format!("http://{loop_addr}/live"), &Policy { allow_private: true, deny: Vec::new() }) {
            Err(err) => err,
            Ok(_) => panic!("redirect loop was accepted"),
        };
        assert!(err.to_string().contains("too many"), "{err}");
    }
}
