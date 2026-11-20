//! Public stream port. Sources use SOURCE or PUT with the Icecast source
//! protocol. Listeners receive the encoded stream or HLS.
//! Admin routes are rejected here.

use crate::engine::{Engine, SourceMeta};
use crate::error::Error;
use crate::httputil::{header_safe, status_text};
use crate::icy::{self, Head};
use crate::ignore_sigpipe;
use std::io::{BufReader, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub struct StreamHandle {
    pub addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    conns: Arc<Mutex<Vec<(u64, TcpStream)>>>,
}

impl StreamHandle {
    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let conns: Vec<_> = std::mem::take(&mut *self.conns.lock().expect("conns"));
        for (_, sock) in conns {
            let _ = sock.shutdown(Shutdown::Both);
        }
        if let Some(t) = self.thread.take() {
            let _ = crate::join_timeout(t, Duration::from_secs(2));
        }
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) { self.shutdown(); }
}

pub fn spawn(addr: &str, engine: Arc<Engine>, source_timeout: Duration) -> std::io::Result<StreamHandle> {
    ignore_sigpipe();
    let listener = TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    let local = listener.local_addr()?;
    let stop = Arc::new(AtomicBool::new(false));
    let conns = Arc::new(Mutex::new(Vec::new()));
    let next = Arc::new(Mutex::new(1u64));
    let stop_t = Arc::clone(&stop);
    let conns_t = Arc::clone(&conns);
    let next_t = Arc::clone(&next);
    let thread = thread::Builder::new().name("stream-accept".into()).spawn(move || {
        accept_loop(listener, engine, source_timeout, stop_t, conns_t, next_t);
    })?;
    Ok(StreamHandle { addr: local, stop, thread: Some(thread), conns })
}

fn accept_loop(
    listener: TcpListener,
    engine: Arc<Engine>,
    source_timeout: Duration,
    stop: Arc<AtomicBool>,
    conns: Arc<Mutex<Vec<(u64, TcpStream)>>>,
    next: Arc<Mutex<u64>>,
) {
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((sock, peer)) => {
                let _ = sock.set_nonblocking(false);
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let id = {
                    let mut n = next.lock().expect("id");
                    let id = *n;
                    *n += 1;
                    id
                };
                if let Ok(tracked) = sock.try_clone() {
                    conns.lock().expect("conns").push((id, tracked));
                }
                let engine = Arc::clone(&engine);
                let conns = Arc::clone(&conns);
                thread::spawn(move || {
                    let _ = handle(sock, peer, engine, source_timeout);
                    conns.lock().expect("conns").retain(|(i, _)| *i != id);
                });
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(50)),
            Err(_) if stop.load(Ordering::SeqCst) => break,
            Err(_) => thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn handle(sock: TcpStream, peer: SocketAddr, engine: Arc<Engine>, source_timeout: Duration) -> std::io::Result<()> {
    let _ = sock.set_nodelay(true);
    sock.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(sock);
    let head_bytes = match icy::read_head(&mut reader) {
        Ok(b) => b,
        Err(_) => return Ok(()),
    };
    let head = match icy::parse_head(&head_bytes) {
        Ok(h) => h,
        Err(_) => {
            let _ = write_simple(reader.get_mut(), 400, "bad request");
            return Ok(());
        }
    };
    if icy::is_admin_path(&head.path) || head.path.to_ascii_lowercase().contains("/api") {
        let _ = write_simple(reader.get_mut(), 404, "not found");
        return Ok(());
    }
    match head.method.as_str() {
        "SOURCE" | "PUT" => handle_source(reader, peer, head, engine, source_timeout),
        "GET" | "HEAD" => handle_get(reader, peer, head, engine),
        "OPTIONS" => {
            let _ = write_simple(reader.get_mut(), 204, "");
            Ok(())
        }
        _ => {
            let _ = write_simple(reader.get_mut(), 405, "method");
            Ok(())
        }
    }
}

fn handle_source(
    mut reader: BufReader<TcpStream>,
    peer: SocketAddr,
    head: Head,
    engine: Arc<Engine>,
    source_timeout: Duration,
) -> std::io::Result<()> {
    let (mount, _rest) = match resolve(&engine, &head.path) {
        Some((m, rest)) if rest.is_empty() => (m, rest),
        _ => {
            let _ = write_simple(reader.get_mut(), 404, "not found");
            return Ok(());
        }
    };
    let (user, pass) = icy::basic_auth(head.get("authorization").unwrap_or("")).unwrap_or_default();
    let meta = meta_from(&head);
    let kill = reader.get_ref().try_clone()?;
    let guard = match engine.open_source(&mount, &user, &pass, meta, Box::new(move || {
        let _ = kill.shutdown(Shutdown::Both);
    }), &peer.ip().to_string()) {
        Ok(g) => g,
        Err(Error::Limit) => {
            let _ = write_simple(reader.get_mut(), 429, "slow down");
            return Ok(());
        }
        Err(Error::Unauthorized) => {
            let _ = write_headers(reader.get_mut(), 401, &[("WWW-Authenticate", "Basic realm=\"NavaCast\"")], b"unauthorized");
            return Ok(());
        }
        Err(Error::Busy) => {
            let _ = write_simple(reader.get_mut(), 403, "mount busy");
            return Ok(());
        }
        Err(Error::Bitrate) => {
            let _ = write_simple(reader.get_mut(), 409, "bitrate");
            return Ok(());
        }
        Err(Error::NotFound) => {
            let _ = write_simple(reader.get_mut(), 404, "not found");
            return Ok(());
        }
        Err(_) => {
            let _ = write_simple(reader.get_mut(), 400, "bad request");
            return Ok(());
        }
    };
    if head.get("expect").unwrap_or("").eq_ignore_ascii_case("100-continue") {
        let _ = reader.get_mut().write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
    }
    write_stream_ok(reader.get_mut())?;
    reader.get_mut().set_read_timeout(Some(source_timeout))?;
    let _ = guard.run(Box::new(reader));
    Ok(())
}

fn handle_get(mut reader: BufReader<TcpStream>, peer: SocketAddr, head: Head, engine: Arc<Engine>) -> std::io::Result<()> {
    let Some((mount, rest)) = resolve(&engine, &head.path) else {
        let _ = write_simple(reader.get_mut(), 404, "not found");
        return Ok(());
    };
    if !engine.path_public(&mount) {
        let _ = write_simple(reader.get_mut(), 404, "not found");
        return Ok(());
    }
    if rest == "/index.m3u8" || rest == "/playlist.m3u8" {
        let base = engine.stream_base(&peer.ip().to_string(), head.get("x-forwarded-host"), head.get("x-forwarded-proto"));
        let Some(body) = engine.hls_playlist_for(&mount, &base) else {
            let _ = write_simple(reader.get_mut(), 404, "not found");
            return Ok(());
        };
        return write_headers(reader.get_mut(), 200, &[
            ("Content-Type", "application/vnd.apple.mpegurl"),
            ("Cache-Control", "no-cache"),
            ("Access-Control-Allow-Origin", "*"),
        ], body.as_bytes());
    }
    let seg_name = rest.strip_prefix("/seg/").or_else(|| {
        let name = rest.trim_start_matches('/');
        if name.starts_with("seg") && name.ends_with(".ts") { Some(name) } else { None }
    });
    if let Some(name) = seg_name {
        let Some(body) = engine.hls_segment(&mount, name) else {
            let _ = write_simple(reader.get_mut(), 404, "not found");
            return Ok(());
        };
        return write_headers(reader.get_mut(), 200, &[
            ("Content-Type", "video/mp2t"),
            ("Cache-Control", "public, max-age=60"),
            ("Access-Control-Allow-Origin", "*"),
        ], &body);
    }
    if !rest.is_empty() {
        let _ = write_simple(reader.get_mut(), 404, "not found");
        return Ok(());
    }
    let ip = peer.ip().to_string();
    if let Err(err) = engine.listener_allowed(&mount, &ip) {
        let code = if matches!(err, Error::Limit) { 503 } else { 404 };
        let _ = write_simple(reader.get_mut(), code, "unavailable");
        return Ok(());
    }
    let status = engine.status().into_iter().find(|s| s.path == mount);
    let name = status.as_ref().map(|s| s.station_name.clone()).unwrap_or_else(|| "NavaCast".into());
    let br = status.as_ref().map(|s| s.bitrate).unwrap_or(128_000);
    let encoder = status.as_ref().map(|s| s.encoder.as_str()).unwrap_or("native");
    let content_type = crate::encoder::content_type(encoder);
    let want_meta = head.get("icy-metadata").unwrap_or("0") == "1";
    let icy_name = header_safe(&name);
    let br_kbps = (br / 1000).max(1).to_string();
    let meta_int = engine.meta_int().to_string();
    let mut extra = format!(
        "Content-Type: {content_type}\r\nicy-name: {icy_name}\r\nicy-br: {br_kbps}\r\nicy-pub: 0\r\nServer: navad\r\nCache-Control: no-cache\r\nAccess-Control-Allow-Origin: *\r\n"
    );
    if want_meta {
        extra.push_str(&format!("icy-metaint: {meta_int}\r\n"));
    }
    let head_out = format!("HTTP/1.0 200 OK\r\n{extra}\r\n");
    reader.get_mut().write_all(head_out.as_bytes())?;
    if head.method == "HEAD" {
        return Ok(());
    }
    let _ = reader.get_mut().set_write_timeout(Some(Duration::from_secs(20)));
    let sock = reader.into_inner();
    let ua = head.get("user-agent").unwrap_or("");
    let _ = engine.serve_listener(&mount, &ip, ua, want_meta, sock);
    Ok(())
}

fn resolve(engine: &Engine, path: &str) -> Option<(String, String)> {
    let path = path.split('?').next().unwrap_or(path);
    for mount in engine.paths() {
        if path == mount {
            return Some((mount, String::new()));
        }
        if let Some(rest) = path.strip_prefix(&mount) {
            if rest.starts_with('/') {
                return Some((mount, rest.to_string()));
            }
        }
    }
    None
}

fn meta_from(head: &Head) -> SourceMeta {
    let name = head.get("ice-name").or_else(|| head.get("icy-name")).unwrap_or("").to_string();
    let url = head.get("ice-url").or_else(|| head.get("icy-url")).unwrap_or("").to_string();
    let declared = head.get("ice-bitrate").or_else(|| head.get("icy-br")).and_then(|s| s.parse::<u32>().ok());
    let bitrate_bps = declared.map(|v| if v < 10_000 { v.saturating_mul(1000) } else { v });
    let inline_metaint = head.get("icy-metaint").and_then(|s| s.parse::<usize>().ok()).filter(|n| (16..=65536).contains(n));
    SourceMeta {
        content_type: head.get("content-type").unwrap_or("").to_string(),
        name,
        url,
        bitrate_bps,
        inline_metaint,
    }
}

fn write_stream_ok(sock: &mut TcpStream) -> std::io::Result<()> {
    sock.write_all(b"HTTP/1.0 200 OK\r\nServer: navad\r\n\r\n")
}

fn write_simple(sock: &mut TcpStream, code: u16, body: &str) -> std::io::Result<()> {
    write_headers(sock, code, &[("Content-Type", "text/plain"), ("Access-Control-Allow-Origin", "*")], body.as_bytes())
}

fn write_headers(sock: &mut TcpStream, code: u16, headers: &[(&str, &str)], body: &[u8]) -> std::io::Result<()> {
    let mut out = format!("HTTP/1.0 {code} {}\r\nContent-Length: {}\r\nConnection: close\r\n", status_text(code), body.len());
    for (k, v) in headers {
        out.push_str(k);
        out.push_str(": ");
        out.push_str(&header_safe(v));
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    sock.write_all(out.as_bytes())?;
    sock.write_all(body)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth;
    use crate::encoder::{IdentityFactory, PassthroughFactory};
    use crate::engine::{Engine, EngineOpts, MountSpec, NoTracks, Track, TrackSource};
    use std::io::Read;
    use std::path::PathBuf;
    use std::sync::Arc;

    struct OneFile(PathBuf);
    impl TrackSource for OneFile {
        fn next(&self, _: &str) -> Option<Track> {
            Some(Track {
                title: "Auto".into(), artist: String::new(), path: self.0.clone(), mime: "audio/mpeg".into(),
                media_id: String::new(), duration_ms: 0,
            })
        }
    }

    fn engine_pcm(dir: PathBuf, max_listeners: usize, takeover: bool, max_bitrate: u32) -> (Arc<Engine>, String) {
        let opts = EngineOpts {
            sample_rate: 8_000,
            channels: 2,
            frame: Duration::from_millis(5),
            fade: Duration::from_millis(15),
            meta_int: 800,
            max_conns_per_ip: 8,
            media_root: dir,
            fast: true,
        };
        let engine = Arc::new(Engine::new(Arc::new(IdentityFactory), Arc::new(NoTracks), opts));
        let hash = auth::hash_with("secret-pass", auth::HashParams::fast()).unwrap();
        engine.upsert(MountSpec {
            id: "m1".into(), station_id: "s".into(), station_name: "Night".into(), genre: "jazz".into(),
            path: "/live".into(), source_user: "source".into(), pass_hash: hash, format: "mp3".into(),
            bitrate: 128_000, max_listeners, max_bitrate, allow_takeover: takeover,
            crossfade_ms: 15, autodj: true, shuffle: false, repeat: crate::engine::RepeatMode::All, jingle_every: 0,
            gain_db: 0.0, sample_rate: 0, channels: 0, buffer_ms: 0, hls_segment_ms: 0,
            encoder: "ffmpeg".into(), relay_url: String::new(),
            jingle_media_id: None, intro_media_id: None,
        });
        (engine, "secret-pass".into())
    }

    fn read_http(sock: &mut TcpStream) -> (u16, Vec<u8>) {
        sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        loop {
            match sock.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() > 16 {
                        // For short responses with content-length, stop once the body arrived.
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let header = String::from_utf8_lossy(&buf[..pos]).to_string();
                            let cl = header.lines().find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                if k.eq_ignore_ascii_case("content-length") { v.trim().parse::<usize>().ok() } else { None }
                            });
                            if let Some(cl) = cl {
                                if buf.len() >= pos + 4 + cl {
                                    break;
                                }
                            } else {
                                break;
                            }
                        }
                    }
                }
                Err(_) => break,
            }
        }
        let text = String::from_utf8_lossy(&buf);
        let code = text.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        (code, buf)
    }

    fn connect_listener(addr: SocketAddr) -> TcpStream {
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(b"GET /live HTTP/1.0\r\nIcy-MetaData: 1\r\n\r\n").unwrap();
        s
    }

    #[test]
    fn source_disconnect_does_not_drop_listener() {
        let dir = std::env::temp_dir().join(format!("radio-disc-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let (engine, pass) = engine_pcm(dir, 8, false, 320_000);
        let handle = spawn("127.0.0.1:0", Arc::clone(&engine), Duration::from_secs(3)).unwrap();
        let mut listener = connect_listener(handle.addr);
        listener.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut pre = [0u8; 64];
        let n = listener.read(&mut pre).unwrap();
        assert!(n > 0, "listener got nothing before source");

        let mut src = TcpStream::connect(handle.addr).unwrap();
        let basic = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, format!("source:{pass}"));
        let req = format!("SOURCE /live ICE/1.0\r\nAuthorization: Basic {basic}\r\nContent-Type: audio/L16\r\nIce-Name: DJ Set\r\n\r\n");
        src.write_all(req.as_bytes()).unwrap();
        src.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let (code, _) = read_http(&mut src);
        assert_eq!(code, 200, "source should be accepted");
        let mut tone = Vec::new();
        for _ in 0..(8_000 / 5) {
            tone.extend_from_slice(&20_000i16.to_le_bytes());
            tone.extend_from_slice(&20_000i16.to_le_bytes());
        }
        src.write_all(&tone).unwrap();

        let mut saw_live = false;
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            let mut buf = [0u8; 512];
            listener.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
            let n = match listener.read(&mut buf) {
                Ok(0) => panic!("listener closed while source was connected"),
                Ok(n) => n,
                Err(_) => continue,
            };
            for sample in buf[..n].chunks_exact(2) {
                let v = i16::from_le_bytes([sample[0], sample[1]]);
                if v.unsigned_abs() > 2_000 {
                    saw_live = true;
                    break;
                }
            }
            if saw_live { break; }
        }
        assert!(saw_live, "listener never heard the live source");
        drop(src);

        listener.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut after = [0u8; 128];
        let n = listener.read(&mut after).expect("listener dropped when the source disconnected");
        assert!(n > 0, "listener returned EOF when the source disconnected");
    }

    #[test]
    fn admin_routes_stay_off_the_stream_port() {
        let dir = std::env::temp_dir().join(format!("radio-admin-off-{}", uniq()));
        let _ = std::fs::create_dir_all(&dir);
        let (engine, _) = engine_pcm(dir, 4, false, 320_000);
        let handle = spawn("127.0.0.1:0", Arc::clone(&engine), Duration::from_secs(2)).unwrap();
        for path in ["/api/stations", "/api/health", "/api/mounts", "/admin", "/API/live"] {
            let mut s = TcpStream::connect(handle.addr).unwrap();
            s.write_all(format!("GET {path} HTTP/1.0\r\n\r\n").as_bytes()).unwrap();
            let (code, body) = read_http(&mut s);
            assert!(code == 404 || code == 400, "{path} -> {code}");
            let text = String::from_utf8_lossy(&body).to_ascii_lowercase();
            assert!(!text.contains("\"token\""), "{path} leaked an admin payload");
            assert!(!text.contains("fallback_playlist"), "{path} leaked mount config");
        }
    }

    #[test]
    fn admin_routes_absent_and_limits() {
        let dir = std::env::temp_dir().join(format!("radio-sec-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let (engine, pass) = engine_pcm(dir, 1, false, 8_000);
        let handle = spawn("127.0.0.1:0", Arc::clone(&engine), Duration::from_secs(2)).unwrap();

        for path in ["/api/health", "/admin/metadata", "/API/stations"] {
            let mut s = TcpStream::connect(handle.addr).unwrap();
            s.write_all(format!("GET {path} HTTP/1.0\r\n\r\n").as_bytes()).unwrap();
            let (code, body) = read_http(&mut s);
            assert!(code == 404 || code == 400, "{path} -> {code}");
            let text = String::from_utf8_lossy(&body);
            assert!(!text.contains("\"ok\""));
        }

        let mut first = connect_listener(handle.addr);
        let _ = read_http(&mut first);
        let mut second = connect_listener(handle.addr);
        let (code, _) = read_http(&mut second);
        assert_eq!(code, 503, "second listener should be refused");

        let basic = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, "source:nope");
        let mut bad = TcpStream::connect(handle.addr).unwrap();
        bad.write_all(format!("SOURCE /live ICE/1.0\r\nAuthorization: Basic {basic}\r\n\r\n").as_bytes()).unwrap();
        let (code, _) = read_http(&mut bad);
        assert_eq!(code, 401);

        let basic = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, format!("source:{pass}"));
        let mut held = TcpStream::connect(handle.addr).unwrap();
        held.write_all(format!("SOURCE /live ICE/1.0\r\nAuthorization: Basic {basic}\r\nicy-br: 8\r\nContent-Type: audio/L16\r\n\r\n").as_bytes()).unwrap();
        let (code, _) = read_http(&mut held);
        assert_eq!(code, 200);
        let mut other = TcpStream::connect(handle.addr).unwrap();
        other.write_all(format!("PUT /live HTTP/1.1\r\nAuthorization: Basic {basic}\r\nContent-Type: audio/L16\r\n\r\n").as_bytes()).unwrap();
        let (code, _) = read_http(&mut other);
        assert_eq!(code, 403, "second source rejected without takeover");

        // Over the configured bitrate: dump a large burst.
        let burst = vec![1u8; 400_000];
        let _ = held.write_all(&burst);
        // Listener from `first` is still the only allowed listener and must keep reading
        // after the source is dropped for bitrate.
        drop(held);
        first.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut buf = [0u8; 32];
        let n = first.read(&mut buf).expect("listener died after source bitrate drop");
        assert!(n > 0);
    }

    #[test]
    fn passthrough_takeover_keeps_listener() {
        let dir = std::env::temp_dir().join(format!("radio-pass-{}", uniq()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("0123456789abcdef0123456789abcdef");
        std::fs::write(&file, b"AUTO-BYTES-AUTO-BYTES-AUTO-BYTES-").unwrap();
        let opts = EngineOpts {
            sample_rate: 8_000, channels: 2, frame: Duration::from_millis(20),
            fade: Duration::ZERO, meta_int: 16000, max_conns_per_ip: 8,
            media_root: dir.clone(), fast: true,
        };
        let engine = Arc::new(Engine::new(Arc::new(PassthroughFactory), Arc::new(OneFile(file)), opts));
        let hash = auth::hash_with("secret-pass", auth::HashParams::fast()).unwrap();
        engine.upsert(MountSpec {
            id: "m".into(), station_id: "s".into(), station_name: "Night".into(), genre: String::new(),
            path: "/live".into(), source_user: "source".into(), pass_hash: hash.clone(),
            format: "mp3".into(), bitrate: 1_000_000, max_listeners: 4, max_bitrate: 0, allow_takeover: true,
            crossfade_ms: 0, autodj: true, shuffle: false, repeat: crate::engine::RepeatMode::All, jingle_every: 0,
            gain_db: 0.0, sample_rate: 0, channels: 0, buffer_ms: 0, hls_segment_ms: 0,
            encoder: "ffmpeg".into(), relay_url: String::new(),
            jingle_media_id: None, intro_media_id: None,
        });
        let handle = spawn("127.0.0.1:0", Arc::clone(&engine), Duration::from_secs(2)).unwrap();
        let mut listener = TcpStream::connect(handle.addr).unwrap();
        listener.write_all(b"GET /live HTTP/1.0\r\n\r\n").unwrap();
        listener.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut acc = Vec::new();
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(2) && !acc.windows(9).any(|w| w == b"AUTO-BYTE") {
            let mut buf = [0u8; 256];
            match listener.read(&mut buf) {
                Ok(0) => panic!("eof before autodj"),
                Ok(n) => acc.extend_from_slice(&buf[..n]),
                Err(_) => {}
            }
        }
        assert!(acc.windows(9).any(|w| w == b"AUTO-BYTE"), "missing autodj bytes");

        let basic = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, "source:secret-pass");
        let mut src = TcpStream::connect(handle.addr).unwrap();
        src.write_all(format!("SOURCE /live ICE/1.0\r\nAuthorization: Basic {basic}\r\nContent-Type: audio/mpeg\r\n\r\n").as_bytes()).unwrap();
        let (code, _) = read_http(&mut src);
        assert_eq!(code, 200);
        src.write_all(b"LIVE-BYTES-LIVE-BYTES-").unwrap();
        let start = std::time::Instant::now();
        let mut saw = false;
        while start.elapsed() < Duration::from_secs(2) {
            let mut buf = [0u8; 256];
            listener.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
            match listener.read(&mut buf) {
                Ok(0) => panic!("listener dropped on takeover"),
                Ok(n) => {
                    acc.extend_from_slice(&buf[..n]);
                    if acc.windows(9).any(|w| w == b"LIVE-BYTE") { saw = true; break; }
                }
                Err(_) => {}
            }
        }
        assert!(saw, "live bytes did not replace autodj");
        drop(src);
        let start = std::time::Instant::now();
        let mark = acc.len();
        while start.elapsed() < Duration::from_secs(2) {
            let mut buf = [0u8; 256];
            match listener.read(&mut buf) {
                Ok(0) => panic!("listener dropped when source disconnected"),
                Ok(n) => {
                    acc.extend_from_slice(&buf[..n]);
                    if acc[mark..].windows(9).any(|w| w == b"AUTO-BYTE") {
                        return;
                    }
                }
                Err(_) => {}
            }
        }
        panic!("autodj did not resume");
    }

    fn uniq() -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64
    }
}
