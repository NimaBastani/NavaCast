//! HTTP request heads and ICY metadata.
//! [`parse_head`] does not panic on hostile input.

use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};

pub const MAX_HEAD: usize = 16 * 1024;
/// Largest ICY metadata block accepted from a source or a relay. The length byte is one octet, so the protocol max is 4080; anything above this cap is rejected.
pub const MAX_ICY_META: usize = 1024;
pub const MAX_LINE: usize = 4 * 1024;
pub const MAX_HEADERS: usize = 64;
pub const MAX_HEADER_VAL: usize = 4 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadError {
    Empty,
    TooLarge,
    BadLine,
    BadHead,
    Nul,
    Io(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    pub method: String,
    pub path: String,
    pub query: String,
    pub protocol: String,
    pub headers: BTreeMap<String, String>,
}

impl Head {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(|s| s.as_str())
    }
}

pub fn parse_head(data: &[u8]) -> Result<Head, HeadError> {
    if data.is_empty() {
        return Err(HeadError::Empty);
    }
    if data.len() > MAX_HEAD {
        return Err(HeadError::TooLarge);
    }
    if data.contains(&0) {
        return Err(HeadError::Nul);
    }
    let lines = split_lines(data)?;
    if lines.len() < 2 || lines.last().map(|s| s.as_str()) != Some("") {
        return Err(HeadError::BadLine);
    }
    let (method, path, protocol) = parse_request_line(&lines[0])?;
    if lines.len() - 2 > MAX_HEADERS {
        return Err(HeadError::TooLarge);
    }
    let mut headers = BTreeMap::new();
    for line in &lines[1..lines.len() - 1] {
        if line.is_empty() {
            return Err(HeadError::BadHead);
        }
        let (name, value) = parse_header(line)?;
        headers.insert(name, value);
    }
    let (path, query) = split_query(&path);
    Ok(Head { method, path, query, protocol, headers })
}

fn split_lines(data: &[u8]) -> Result<Vec<String>, HeadError> {
    let mut out = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let (line, next) = match rest.iter().position(|b| *b == b'\n') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, &[][..]),
        };
        if line.len() > MAX_LINE {
            return Err(HeadError::TooLarge);
        }
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.contains(&b'\r') {
            return Err(HeadError::BadHead);
        }
        let s = std::str::from_utf8(line).map_err(|_| HeadError::BadHead)?;
        out.push(s.to_string());
        rest = next;
    }
    Ok(out)
}

fn parse_request_line(line: &str) -> Result<(String, String, String), HeadError> {
    if line.is_empty() || line.len() > MAX_LINE {
        return Err(HeadError::BadLine);
    }
    let mut parts = line.splitn(3, ' ');
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    let proto = parts.next().unwrap_or("");
    if method.is_empty() || path.is_empty() {
        return Err(HeadError::BadLine);
    }
    if !method.bytes().all(|c| c.is_ascii_alphabetic()) || method.len() > 16 {
        return Err(HeadError::BadLine);
    }
    if !valid_path(path) {
        return Err(HeadError::BadLine);
    }
    let protocol = if proto.is_empty() {
        "ICE/1.0".to_string()
    } else if valid_proto(proto) {
        proto.to_string()
    } else {
        return Err(HeadError::BadLine);
    };
    Ok((method.to_ascii_uppercase(), path.to_string(), protocol))
}

fn valid_path(path: &str) -> bool {
    if path.len() > 2048 || !path.starts_with('/') || path.contains("..") || path.contains('\\') || path.contains(' ') {
        return false;
    }
    !path.bytes().any(|c| c == 0 || c == b'\r' || c == b'\n')
}

fn valid_proto(s: &str) -> bool {
    !s.is_empty() && s.len() <= 16 && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'/' || c == b'.')
}

fn parse_header(line: &str) -> Result<(String, String), HeadError> {
    if line.len() > MAX_LINE {
        return Err(HeadError::TooLarge);
    }
    let Some((name, value)) = line.split_once(':') else {
        return Err(HeadError::BadHead);
    };
    let name = name.trim();
    if name.is_empty() || name.len() > 128 || !valid_token(name) {
        return Err(HeadError::BadHead);
    }
    let value = value.trim();
    if value.len() > MAX_HEADER_VAL {
        return Err(HeadError::TooLarge);
    }
    Ok((name.to_ascii_lowercase(), value.to_string()))
}

fn valid_token(s: &str) -> bool {
    s.bytes().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(c, b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~')
    })
}

fn split_query(path: &str) -> (String, String) {
    match path.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (path.to_string(), String::new()),
    }
}

pub fn basic_auth(value: &str) -> Option<(String, String)> {
    let value = value.trim();
    let enc = value.get(6..)?.trim();
    if value.len() < 6 || !value[..6].eq_ignore_ascii_case("basic ") || enc.len() > 1024 {
        return None;
    }
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
    use base64::Engine;
    let raw = STANDARD.decode(enc).or_else(|_| STANDARD_NO_PAD.decode(enc)).ok()?;
    if raw.len() > 512 {
        return None;
    }
    let i = raw.iter().position(|b| *b == b':')?;
    let user = String::from_utf8(raw[..i].to_vec()).ok()?;
    let pass = String::from_utf8(raw[i + 1..].to_vec()).ok()?;
    Some((user, pass))
}

pub fn read_head<R: BufRead>(r: &mut R) -> Result<Vec<u8>, HeadError> {
    let mut buf = Vec::new();
    loop {
        if buf.len() > MAX_HEAD {
            return Err(HeadError::TooLarge);
        }
        let mut line = Vec::new();
        let n = r.read_until(b'\n', &mut line).map_err(|e| HeadError::Io(e.to_string()))?;
        if n == 0 {
            if buf.is_empty() {
                return Err(HeadError::Empty);
            }
            return Err(HeadError::BadLine);
        }
        if line.len() > MAX_LINE || buf.len() + line.len() > MAX_HEAD {
            return Err(HeadError::TooLarge);
        }
        if line.contains(&0) {
            return Err(HeadError::Nul);
        }
        let blank = {
            let t = line.strip_suffix(b"\n").unwrap_or(&line);
            let t = t.strip_suffix(b"\r").unwrap_or(t);
            t.is_empty()
        };
        buf.extend_from_slice(&line);
        if blank {
            return Ok(buf);
        }
    }
}

pub fn format_meta(title: &str, url: &str) -> Vec<u8> {
    let s = format!("StreamTitle='{}';StreamUrl='{}';", sanitize(title), sanitize(url));
    let pad = (16 - (s.len() % 16)) % 16;
    let mut out = s.into_bytes();
    out.extend(std::iter::repeat(0).take(pad));
    out
}

fn sanitize(s: &str) -> String {
    s.chars().filter(|c| *c != '\r' && *c != '\n' && *c != '\'').take(512).collect()
}

pub fn parse_meta(block: &[u8]) -> (String, String) {
    let block = if block.len() > 16 * 255 { &block[..16 * 255] } else { block };
    let end = block.iter().rposition(|b| *b != 0).map(|i| i + 1).unwrap_or(0);
    let s = String::from_utf8_lossy(&block[..end]);
    (field(&s, "StreamTitle"), field(&s, "StreamUrl"))
}

fn field(s: &str, name: &str) -> String {
    let key = format!("{name}='");
    let Some(i) = s.find(&key) else { return String::new() };
    let rest = &s[i + key.len()..];
    match rest.find('\'') {
        Some(j) => rest[..j].to_string(),
        None => rest.to_string(),
    }
}

pub struct MetaWriter<W> {
    inner: W,
    meta_int: usize,
    sent: usize,
    enabled: bool,
    title: Box<dyn FnMut() -> (String, String) + Send>,
}

impl<W: Write> MetaWriter<W> {
    pub fn new(inner: W, meta_int: usize, enabled: bool, title: impl FnMut() -> (String, String) + Send + 'static) -> Self {
        Self {
            inner,
            meta_int: meta_int.max(1),
            sent: 0,
            enabled,
            title: Box::new(title),
        }
    }

    pub fn write_audio(&mut self, mut data: &[u8]) -> io::Result<()> {
        if !self.enabled {
            return self.inner.write_all(data);
        }
        while !data.is_empty() {
            let space = self.meta_int - self.sent;
            let n = data.len().min(space);
            self.inner.write_all(&data[..n])?;
            data = &data[n..];
            self.sent += n;
            if self.sent == self.meta_int {
                self.write_block()?;
                self.sent = 0;
            }
        }
        Ok(())
    }

    fn write_block(&mut self) -> io::Result<()> {
        let (title, url) = (self.title)();
        let payload = format_meta(&title, &url);
        let blocks = (payload.len() / 16).min(255) as u8;
        let payload = &payload[..blocks as usize * 16];
        self.inner.write_all(&[blocks])?;
        self.inner.write_all(payload)?;
        Ok(())
    }
}

pub struct Stripper<R> {
    inner: R,
    interval: usize,
    left_audio: usize,
    reading_len: bool,
    meta_need: usize,
    meta_buf: Vec<u8>,
    on_meta: Box<dyn FnMut(String, String) + Send>,
}

impl<R: io::Read> Stripper<R> {
    pub fn new(inner: R, interval: usize, on_meta: impl FnMut(String, String) + Send + 'static) -> Self {
        let interval = interval.clamp(1, 65536);
        Self {
            inner,
            interval,
            left_audio: interval,
            reading_len: false,
            meta_need: 0,
            meta_buf: Vec::new(),
            on_meta: Box::new(on_meta),
        }
    }
}

impl<R: io::Read> io::Read for Stripper<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let mut written = 0;
        while written < out.len() {
            if self.left_audio > 0 {
                let want = (out.len() - written).min(self.left_audio);
                let n = self.inner.read(&mut out[written..written + want])?;
                if n == 0 {
                    return Ok(written);
                }
                written += n;
                self.left_audio -= n;
                if self.left_audio == 0 {
                    self.reading_len = true;
                }
                continue;
            }
            if self.reading_len {
                let mut b = [0u8; 1];
                let n = self.inner.read(&mut b)?;
                if n == 0 {
                    return Ok(written);
                }
                let bytes = b[0] as usize * 16;
                if bytes > MAX_ICY_META {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "icy metadata is too large"));
                }
                self.meta_need = bytes;
                self.reading_len = false;
                self.meta_buf.clear();
                if self.meta_need == 0 {
                    self.left_audio = self.interval;
                }
                continue;
            }
            let want = self.meta_need.saturating_sub(self.meta_buf.len());
            if want == 0 {
                self.left_audio = self.interval;
                continue;
            }
            let mut tmp = vec![0u8; want];
            let n = self.inner.read(&mut tmp)?;
            if n == 0 {
                return Ok(written);
            }
            self.meta_buf.extend_from_slice(&tmp[..n]);
            if self.meta_buf.len() >= self.meta_need {
                let (t, u) = parse_meta(&self.meta_buf);
                (self.on_meta)(t, u);
                self.left_audio = self.interval;
                self.meta_buf.clear();
                self.meta_need = 0;
            }
        }
        Ok(written)
    }
}

pub fn is_admin_path(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p == "/api" || p.starts_with("/api/") || p == "/admin" || p.starts_with("/admin/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn meta_roundtrip_and_interval() {
        let block = format_meta("Artist - Title", "http://example");
        assert_eq!(block.len() % 16, 0);
        let (t, u) = parse_meta(&block);
        assert_eq!(t, "Artist - Title");
        assert_eq!(u, "http://example");
        assert_eq!(parse_meta(b"StreamTitle='';").0, "");
        assert_eq!(parse_meta(b"nope").0, "");
        assert_eq!(parse_meta(b"StreamTitle='unterminated").0, "unterminated");
        assert!(!String::from_utf8_lossy(&format_meta("a'b", "")).contains("a'b"));

        let mut buf = Vec::new();
        let mut w = MetaWriter::new(&mut buf, 100, true, || ("Song".into(), String::new()));
        w.write_audio(&vec![b'A'; 250]).unwrap();
        assert!(buf.len() > 250);
        assert_eq!(&buf[..100], &vec![b'A'; 100]);
        let blocks = buf[100] as usize;
        let meta = &buf[101..101 + blocks * 16];
        assert_eq!(parse_meta(meta).0, "Song");

        buf.clear();
        let mut w = MetaWriter::new(&mut buf, 100, false, || (String::new(), String::new()));
        let payload = vec![b'A'; 250];
        w.write_audio(&payload).unwrap();
        assert_eq!(buf, payload);
    }

    #[test]
    fn stripper_removes_inline_meta() {
        let audio = vec![b'B'; 40];
        let meta = format_meta("Live", "");
        let mut body = Vec::new();
        body.extend_from_slice(&audio[..20]);
        body.push((meta.len() / 16) as u8);
        body.extend_from_slice(&meta);
        body.extend_from_slice(&audio[20..]);
        let title = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let slot = std::sync::Arc::clone(&title);
        let mut s = Stripper::new(Cursor::new(body), 20, move |t, _| *slot.lock().expect("title") = t);
        let mut out = Vec::new();
        io::Read::read_to_end(&mut s, &mut out).unwrap();
        assert_eq!(out, audio);
        assert_eq!(title.lock().expect("title").as_str(), "Live");
    }

    #[test]
    fn stripper_rejects_oversized_metadata() {
        let mut body = vec![0u8; 4];
        body.push(65);
        body.extend(std::iter::repeat(b'x').take(65 * 16));
        let mut s = Stripper::new(Cursor::new(body), 4, |_, _| {});
        let mut out = [0u8; 16];
        let err = io::Read::read(&mut s, &mut out).unwrap_err();
        assert!(err.to_string().contains("too large"), "{err}");
    }

    #[test]
    fn parse_head_exhaustive() {
        let ok = b"SOURCE /live ICE/1.0\r\nAuthorization: Basic c291cmNlOnNlY3JldA==\r\nContent-Type: audio/mpeg\r\nIce-Name: Night\r\nicy-br: 128\r\n\r\n";
        let h = parse_head(ok).unwrap();
        assert_eq!(h.method, "SOURCE");
        assert_eq!(h.path, "/live");
        assert_eq!(h.protocol, "ICE/1.0");
        assert_eq!(h.get("content-type"), Some("audio/mpeg"));
        assert_eq!(h.get("ice-name"), Some("Night"));
        assert_eq!(h.get("icy-br"), Some("128"));
        let (user, pass) = basic_auth(h.get("authorization").unwrap()).unwrap();
        assert_eq!(user, "source");
        assert_eq!(pass, "secret");

        let cases: &[(&[u8], Option<HeadError>)] = &[
            (b"", Some(HeadError::Empty)),
            (b"\x00", Some(HeadError::Nul)),
            (b"GET\r\n\r\n", Some(HeadError::BadLine)),
            (b"GET / HTTP/1.1\r\n\r\n", None),
            (b"PUT /mount HTTP/1.1\r\nExpect: 100-continue\r\n\r\n", None),
            (b"GET /live?x=1 HTTP/1.0\r\nIcy-MetaData: 1\r\n\r\n", None),
            (b"GET /../etc/passwd HTTP/1.0\r\n\r\n", Some(HeadError::BadLine)),
            (b"GET /live HTTP/1.0\r\nNoColon\r\n\r\n", Some(HeadError::BadHead)),
            (b"GET /live HTTP/1.0\r\n: missing\r\n\r\n", Some(HeadError::BadHead)),
            (b"GET /live HTTP/1.0\r\nBad Name: x\r\n\r\n", Some(HeadError::BadHead)),
            (b"SOURCE /live ICE/1.0\r\n\r\n", None),
            (b"get /live HTTP/1.1\r\n\r\n", None),
            (b"GET /live HTTP/1.1\r\nFoo: bar\r\nFoo: baz\r\n\r\n", None),
            (b"GET /live\n\n", None),
            (b"GET /live HTTP/1.1\r\nFoo: bar\r\n", Some(HeadError::BadLine)),
            (b"NOT A REQUEST\r\n\r\n", Some(HeadError::BadLine)),
            (b"GET live HTTP/1.0\r\n\r\n", Some(HeadError::BadLine)),
            (b"GET /live HTTP/1.0\r\nFoo:\r\n\r\n", None),
        ];
        for (input, want) in cases {
            let got = parse_head(input);
            match want {
                None => assert!(got.is_ok(), "input {input:?} -> {got:?}"),
                Some(err) => assert_eq!(got.unwrap_err(), *err, "input {input:?}"),
            }
        }
        let q = parse_head(b"GET /live?x=1 HTTP/1.0\r\nIcy-MetaData: 1\r\n\r\n").unwrap();
        assert_eq!(q.path, "/live");
        assert_eq!(q.query, "x=1");
        assert_eq!(q.get("icy-metadata"), Some("1"));
        let dup = parse_head(b"GET /live HTTP/1.1\r\nFoo: bar\r\nFoo: baz\r\n\r\n").unwrap();
        assert_eq!(dup.get("foo"), Some("baz"));
        let lower = parse_head(b"get /live HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(lower.method, "GET");

        let huge = format!("GET /live HTTP/1.0\r\nX: {}\r\n\r\n", "a".repeat(MAX_HEADER_VAL + 10));
        assert_eq!(parse_head(huge.as_bytes()).unwrap_err(), HeadError::TooLarge);
        assert!(basic_auth("").is_none());
        assert!(basic_auth("Bearer abc").is_none());
        assert!(basic_auth("Basic !!!").is_none());
        assert!(is_admin_path("/api/health"));
        assert!(is_admin_path("/ADMIN/stats"));
        assert!(!is_admin_path("/live"));
    }

    #[test]
    fn random_heads_do_not_panic() {
        let mut state = 0x1234_5678_9abc_def0u64;
        for _ in 0..3000 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let n = (state % 400) as usize;
            let mut data = vec![0u8; n];
            for b in &mut data {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                *b = (state >> 33) as u8;
            }
            let _ = parse_head(&data);
            let _ = parse_meta(&data);
            if let Ok(h) = parse_head(&data) {
                assert!(h.path.starts_with('/'));
                assert!(!h.path.contains(".."));
                assert!(!h.method.is_empty());
                let _ = basic_auth(h.get("authorization").unwrap_or(""));
            }
        }
    }

    #[test]
    fn read_head_leaves_body() {
        let mut c = Cursor::new(&b"PUT /x HTTP/1.0\r\n\r\nBODY"[..]);
        let buf = read_head(&mut c).unwrap();
        let h = parse_head(&buf).unwrap();
        assert_eq!(h.method, "PUT");
        assert_eq!(h.path, "/x");
        let mut rest = String::new();
        io::Read::read_to_string(&mut c, &mut rest).unwrap();
        assert_eq!(rest, "BODY");
    }
}
