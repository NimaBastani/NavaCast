//! Control API. Binds to localhost unless configured otherwise.
//! Public listener JSON lives under `/api/public` and is the only API a
//! public bind is allowed to serve.

use crate::auth;
use crate::config::Config;
use crate::engine::{self, Engine, MountSpec, RepeatMode};
use crate::error::{code_of, Error};
use crate::httputil::{header_safe, status_text};
use crate::icy::{self, Head};
use crate::ignore_sigpipe;
use crate::limit::{client_ip, FailWindow, Trusted};
use crate::media::{self, probe};
use crate::schedule;
use crate::store::{self, Media, Mount, Schedule, Station, Store};
use include_dir::{include_dir, Dir};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static DIST: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/web/dist");

pub struct ApiState {
    pub store: Arc<Store>,
    pub engine: Arc<Engine>,
    pub media_root: PathBuf,
    pub max_upload: u64,
    pub stream_public_url: String,
    pub http_public_url: String,
    pub allow_admin: bool,
    pub trusted: Trusted,
    pub fails: FailWindow,
}

pub struct ApiHandle {
    pub addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    conns: Arc<Mutex<Vec<(u64, TcpStream)>>>,
}

impl ApiHandle {
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

impl Drop for ApiHandle {
    fn drop(&mut self) { self.shutdown(); }
}

pub fn spawn(addr: &str, state: Arc<ApiState>) -> std::io::Result<ApiHandle> {
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
    let thread = thread::Builder::new().name("http-accept".into()).spawn(move || {
        while !stop_t.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((sock, peer)) => {
                    let _ = sock.set_nonblocking(false);
                    if stop_t.load(Ordering::SeqCst) { break; }
                    let id = { let mut n = next_t.lock().expect("id"); let id = *n; *n += 1; id };
                    if let Ok(c) = sock.try_clone() {
                        conns_t.lock().expect("conns").push((id, c));
                    }
                    let state = Arc::clone(&state);
                    let conns = Arc::clone(&conns_t);
                    thread::spawn(move || {
                        let _ = handle(sock, peer, state);
                        conns.lock().expect("conns").retain(|(i, _)| *i != id);
                    });
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(50)),
                Err(_) if stop_t.load(Ordering::SeqCst) => break,
                Err(_) => thread::sleep(Duration::from_millis(20)),
            }
        }
    })?;
    Ok(ApiHandle { addr: local, stop, thread: Some(thread), conns })
}

fn handle(sock: TcpStream, peer: SocketAddr, state: Arc<ApiState>) -> std::io::Result<()> {
    let _ = sock.set_nodelay(true);
    sock.set_read_timeout(Some(Duration::from_secs(15)))?;
    let mut reader = BufReader::new(sock);
    let bytes = match icy::read_head(&mut reader) {
        Ok(b) => b,
        Err(_) => return Ok(()),
    };
    let head = match icy::parse_head(&bytes) {
        Ok(h) => h,
        Err(_) => return write_json(reader.get_mut(), 400, json!({"error":"bad request","code":"invalid"})),
    };
    if head.method == "OPTIONS" {
        return write_json(reader.get_mut(), 204, json!({}));
    }
    let socket_ip = peer.ip().to_string();
    let ip = client_ip(&peer.to_string(), head.get("x-forwarded-for"), &state.trusted);
    let len = head.get("content-length").and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    let is_upload = head.method == "POST" && (head.path == "/api/media" || head.path.ends_with("/replace"));
    let cap = if is_upload { state.max_upload.saturating_add(4096) } else { 1 << 20 };
    if len > cap {
        return write_json(reader.get_mut(), 413, json!({"error":"too large","code":"too_large"}));
    }
    let mut body = vec![0u8; len as usize];
    if len > 0 {
        if reader.read_exact(&mut body).is_err() {
            return Ok(());
        }
    }
    match serve_static_or_api(&state, &head, &body, &ip, &socket_ip) {
        Ok(out) => write_raw(reader.get_mut(), out),
        Err(err) => {
            let (code_name, status) = code_of(&err);
            write_json(reader.get_mut(), status, json!({"error": err.to_string(), "code": code_name}))
        }
    }
}

fn write_raw(sock: &mut TcpStream, out: HttpOut) -> std::io::Result<()> {
    let text = format!(
        "HTTP/1.0 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nAccess-Control-Allow-Origin: *\r\nCache-Control: no-store\r\n\r\n",
        out.code, status_text(out.code), out.content_type, out.body.len()
    );
    sock.write_all(text.as_bytes())?;
    sock.write_all(&out.body)?;
    Ok(())
}

fn write_json(sock: &mut TcpStream, code: u16, value: Value) -> std::io::Result<()> {
    let body = if code == 204 { Vec::new() } else { serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec()) };
    let ctype = if code == 204 { "text/plain" } else { "application/json" };
    let text = format!(
        "HTTP/1.0 {code} {}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: Authorization, Content-Type\r\nAccess-Control-Allow-Methods: GET, POST, PATCH, DELETE, OPTIONS\r\nCache-Control: no-store\r\n\r\n",
        status_text(code),
        body.len()
    );
    sock.write_all(text.as_bytes())?;
    sock.write_all(&body)?;
    Ok(())
}

fn dispatch(state: &ApiState, head: &Head, body: &[u8], ip: &str, socket_ip: &str) -> Result<(u16, Value), Error> {
    let path = head.path.as_str();
    let segs: Vec<&str> = path.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    if segs.first().copied() == Some("api") {
        if !state.allow_admin && segs.get(1).copied() != Some("public") {
            return Err(Error::NotFound);
        }
        return dispatch_api(state, head.method.as_str(), &segs, head, body, ip, socket_ip);
    }
    if head.method == "GET" || head.method == "HEAD" {
        if let Some((bytes, mime)) = static_file(path) {
            return Ok((200, json!({"__file": true, "mime": mime, "b64": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes)})));
        }
    }
    Err(Error::NotFound)
}

fn static_file(path: &str) -> Option<(&'static [u8], &'static str)> {
    let rel = path.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    if rel.contains("..") {
        return None;
    }
    if let Some(file) = DIST.get_file(rel) {
        return Some((file.contents(), mime_of(rel)));
    }
    if !rel.contains('.') {
        DIST.get_file("index.html").map(|f| (f.contents(), "text/html; charset=utf-8"))
    } else {
        None
    }
}

fn mime_of(path: &str) -> &'static str {
    if path.ends_with(".html") { "text/html; charset=utf-8" }
    else if path.ends_with(".js") { "text/javascript; charset=utf-8" }
    else if path.ends_with(".css") { "text/css; charset=utf-8" }
    else if path.ends_with(".svg") { "image/svg+xml" }
    else if path.ends_with(".json") { "application/json" }
    else { "application/octet-stream" }
}

fn dispatch_api(state: &ApiState, method: &str, segs: &[&str], head: &Head, body: &[u8], ip: &str, socket_ip: &str) -> Result<(u16, Value), Error> {
    match (method, segs) {
        ("POST", ["api", "login"]) => login(state, body, ip),
        ("GET", ["api", "health"]) => { let _ = require_user(state, head)?; Ok((200, health(state, true))) }
        ("GET", ["api", "public", "health"]) => Ok((200, json!({"ok": true}))),
        ("GET", ["api", "public", "stations", slug]) => public_station(state, slug, head, socket_ip),
        ("GET", ["api", "stations"]) => { let _ = require_user(state, head)?; Ok((200, json!(state.store.list_stations()?.iter().map(station_json).collect::<Vec<_>>()))) }
        ("POST", ["api", "stations"]) => { let u = require_user(state, head)?; create_station(state, &u.username, body) }
        ("DELETE", ["api", "stations", id]) => { let u = require_user(state, head)?; delete_station(state, &u.username, id) }
        ("GET", ["api", "mounts"]) => { let _ = require_user(state, head)?; list_mounts(state, head, socket_ip) }
        ("POST", ["api", "mounts"]) => { let u = require_user(state, head)?; create_mount(state, &u.username, body) }
        ("PATCH", ["api", "mounts", id]) => { let u = require_user(state, head)?; update_mount(state, &u.username, id, body) }
        ("DELETE", ["api", "mounts", id]) => { let u = require_user(state, head)?; delete_mount(state, &u.username, id) }
        ("POST", ["api", "mounts", id, "disconnect"]) => { let u = require_user(state, head)?; disconnect(state, &u.username, id) }
        ("POST", ["api", "mounts", id, "metadata"]) => { let _ = require_user(state, head)?; set_meta(state, id, body) }
        ("POST", ["api", "mounts", id, "password"]) => { let u = require_user(state, head)?; rotate_source_password(state, &u.username, id, body) }
        ("GET", ["api", "mounts", id, "queue"]) => { let _ = require_user(state, head)?; queue_get(state, id) }
        ("POST", ["api", "mounts", id, "queue", action]) => { let u = require_user(state, head)?; queue_post(state, &u.username, id, action, body) }
        ("POST", ["api", "mounts", id, "seek"]) => { let u = require_user(state, head)?; seek_mount(state, &u.username, id, body) }
        ("POST", ["api", "mounts", id, "jump"]) => { let u = require_user(state, head)?; jump_mount(state, &u.username, id, body) }
        ("POST", ["api", "mounts", id, "play-next"]) => { let u = require_user(state, head)?; play_next_mount(state, &u.username, id, body) }
        ("GET", ["api", "mounts", id, "stats"]) => { let _ = require_user(state, head)?; mount_stats(state, id) }
        ("GET", ["api", "mounts", id, "listeners"]) => { let _ = require_user(state, head)?; listeners_get(state, id) }
        ("POST", ["api", "mounts", id, "listeners", "kick"]) => { let u = require_user(state, head)?; kick_all(state, &u.username, id) }
        ("POST", ["api", "mounts", id, "listeners", lid, "kick"]) => { let u = require_user(state, head)?; kick_one(state, &u.username, id, lid) }
        ("GET", ["api", "media"]) => { let _ = require_user(state, head)?; list_media(state, head) }
        ("POST", ["api", "media"]) => { let u = require_user(state, head)?; upload(state, &u.username, head, body) }
        ("POST", ["api", "media", "import"]) => { let u = require_user(state, head)?; import_media(state, &u.username, body) }
        ("PATCH", ["api", "media", id]) => { let u = require_user(state, head)?; tag_media(state, &u.username, id, body) }
        ("POST", ["api", "media", id, "replace"]) => { let u = require_user(state, head)?; replace_media(state, &u.username, id, head, body) }
        ("DELETE", ["api", "media", id]) => { let u = require_user(state, head)?; delete_media(state, &u.username, id) }
        ("GET", ["api", "playlists"]) => { let _ = require_user(state, head)?; list_playlists(state, head) }
        ("POST", ["api", "playlists"]) => { let u = require_user(state, head)?; create_playlist(state, &u.username, body) }
        ("POST", ["api", "playlists", id, "items"]) => { let u = require_user(state, head)?; add_item(state, &u.username, id, body) }
        ("POST", ["api", "playlists", id, "reorder"]) => { let u = require_user(state, head)?; reorder_playlist(state, &u.username, id, body) }
        ("DELETE", ["api", "playlists", id, "items", item]) => { let u = require_user(state, head)?; delete_playlist_item(state, &u.username, id, item) }
        ("DELETE", ["api", "playlists", id]) => { let u = require_user(state, head)?; delete_playlist(state, &u.username, id) }
        ("GET", ["api", "schedules"]) => { let _ = require_user(state, head)?; list_schedules(state, head) }
        ("POST", ["api", "schedules"]) => { let u = require_user(state, head)?; add_schedule(state, &u.username, body) }
        ("PATCH", ["api", "schedules", id]) => { let u = require_user(state, head)?; update_schedule(state, &u.username, id, body) }
        ("GET", ["api", "live"]) => { let _ = require_user(state, head)?; Ok((200, live_json(&state.engine.status(), &request_stream_base(state, head, socket_ip)))) }
        ("GET", ["api", "audit"]) => { let _ = require_user(state, head)?; list_audit(state, head) }
        ("GET", ["api", "users"]) => { let _ = require_user(state, head)?; list_users(state) }
        ("POST", ["api", "users"]) => { let u = require_user(state, head)?; create_user(state, &u.username, body) }
        ("POST", ["api", "tokens", "rotate"]) => rotate(state, head),
        _ => Err(Error::NotFound),
    }
}

fn login(state: &ApiState, body: &[u8], ip: &str) -> Result<(u16, Value), Error> {
    if !state.fails.allow(ip) {
        return Err(Error::Limit);
    }
    #[derive(Deserialize)]
    struct Req { username: String, password: String }
    let req: Req = parse_json(body)?;
    let user = state.store.user_by_name(&req.username)?;
    let ok = user.as_ref().map(|u| auth::verify_password(&u.password_hash, &req.password)).unwrap_or(false);
    if !ok {
        state.fails.fail(ip);
        let _ = state.store.audit("", "login_fail", &req.username);
        return Err(Error::Unauthorized);
    }
    let user = user.unwrap();
    let token = auth::new_token()?;
    state.store.create_token(&user.id, "login", &auth::hash_token(&token))?;
    let _ = state.store.audit(&user.username, "login", "");
    Ok((200, json!({"token": token, "username": user.username})))
}

fn require_user(state: &ApiState, head: &Head) -> Result<store::User, Error> {
    let header = head.get("authorization").unwrap_or("");
    let token = header.strip_prefix("Bearer ").or_else(|| header.strip_prefix("bearer ")).unwrap_or("").trim();
    if token.is_empty() {
        return Err(Error::Unauthorized);
    }
    state.store.user_by_token(&auth::hash_token(token))?.ok_or(Error::Unauthorized)
}

fn health(state: &ApiState, full: bool) -> Value {
    let rows = state.engine.status();
    let listeners: usize = rows.iter().map(|r| r.listeners).sum();
    if !full {
        return json!({"ok": true});
    }
    json!({
        "ok": true,
        "ffmpeg": crate::encoder::ffmpeg_available(),
        "native": true,
        "opus": true,
        "encoders": crate::encoder::available_encoders(),
        "pcm": true,
        "stream_public_url": state.stream_public_url,
        "http_public_url": state.http_public_url,
        "mounts": rows.len(),
        "listeners": listeners,
        "uptime_sec": state.engine.uptime_secs(),
        "version": env!("CARGO_PKG_VERSION"),
    })
}

fn request_stream_base(state: &ApiState, head: &Head, socket_ip: &str) -> String {
    crate::limit::public_origin(socket_ip, head.get("x-forwarded-host"), head.get("x-forwarded-proto"), &state.stream_public_url, &state.trusted)
}

fn public_station(state: &ApiState, slug: &str, head: &Head, socket_ip: &str) -> Result<(u16, Value), Error> {
    let station = state.store.station_by_slug(slug)?.ok_or(Error::NotFound)?;
    let mounts = state.store.list_mounts(Some(&station.id))?;
    let live = state.engine.status();
    let mut out = Vec::new();
    for m in mounts {
        let st = live.iter().find(|s| s.path == m.path);
        let origin = request_stream_base(state, head, socket_ip);
        let base = format!("{}/{}", origin.trim_end_matches('/'), m.path.trim_start_matches('/'));
        out.push(json!({
            "path": m.path,
            "hls_url": format!("{base}/index.m3u8"),
            "icecast_url": base,
            "live": st.map(|s| s.live).unwrap_or(false),
            "listeners": st.map(|s| s.listeners).unwrap_or(0),
            "now": st.map(|s| json!({"title": s.now.title, "artist": s.now.artist, "source": s.now.source, "elapsed_ms": s.now.elapsed_ms, "duration_ms": s.now.duration_ms})).unwrap_or(json!({})),
            "history": st.map(|s| s.history.iter().rev().take(20).map(|h| json!({"title": h.title, "artist": h.artist, "source": h.source})).collect::<Vec<_>>()).unwrap_or_default(),
        }));
    }
    Ok((200, json!({
        "id": station.id, "name": station.name, "slug": station.slug,
        "description": station.description, "genre": station.genre, "mounts": out,
    })))
}

fn create_station(state: &ApiState, actor: &str, body: &[u8]) -> Result<(u16, Value), Error> {
    #[derive(Deserialize)]
    struct Req { name: String, slug: String, #[serde(default)] description: String, #[serde(default)] genre: String }
    let req: Req = parse_json(body)?;
    if !valid_slug(&req.slug) || req.name.trim().is_empty() || req.name.len() > 80 {
        return Err(Error::Invalid("invalid station".into()));
    }
    let s = state.store.create_station(req.name.trim(), &req.slug, &req.description, &req.genre)?;
    state.store.audit(actor, "station_create", &s.slug)?;
    Ok((201, station_json(&s)))
}

fn delete_station(state: &ApiState, actor: &str, id: &str) -> Result<(u16, Value), Error> {
    let mounts = state.store.list_mounts(Some(id))?;
    for m in &mounts {
        state.engine.remove_path(&m.path);
    }
    state.store.delete_station(id)?;
    state.store.audit(actor, "station_delete", id)?;
    Ok((200, json!({"ok": true})))
}

fn list_mounts(state: &ApiState, head: &Head, socket_ip: &str) -> Result<(u16, Value), Error> {
    let station = query_param(&head.query, "station");
    let mounts = state.store.list_mounts(station.as_deref())?;
    let live = state.engine.status();
    let base = request_stream_base(state, head, socket_ip);
    let rows: Vec<_> = mounts.iter().map(|m| mount_json(m, live.iter().find(|s| s.id == m.id || s.path == m.path), &base)).collect();
    Ok((200, json!(rows)))
}

fn create_mount(state: &ApiState, actor: &str, body: &[u8]) -> Result<(u16, Value), Error> {
    #[derive(Deserialize)]
    struct Req {
        station_id: String,
        path: String,
        source_password: String,
        #[serde(default = "default_user")] source_user: String,
        #[serde(default = "default_br")] bitrate: u32,
        #[serde(default = "default_listeners")] max_listeners: usize,
        #[serde(default = "default_maxbr")] max_bitrate: u32,
        #[serde(default)] allow_takeover: bool,
        #[serde(default)] fallback_playlist_id: Option<String>,
        #[serde(default = "default_fade")] crossfade_ms: u64,
        #[serde(default = "default_true")] autodj: bool,
        #[serde(default)] shuffle: bool,
        #[serde(default = "default_repeat")] repeat: Value,
        #[serde(default)] jingle_every: u32,
        #[serde(default)] jingle_media_id: Option<String>,
        #[serde(default)] intro_media_id: Option<String>,
        #[serde(default)] gain_db: f64,
        #[serde(default = "default_rate")] sample_rate: u32,
        #[serde(default = "default_channels")] channels: u16,
        #[serde(default = "default_buffer")] buffer_ms: u64,
        #[serde(default = "default_hls")] hls_segment_ms: u64,
        #[serde(default)] encoder: String,
        #[serde(default)] relay_url: String,
    }
    let req: Req = parse_json(body)?;
    let repeat = parse_repeat(&req.repeat)?;
    let encoder = if req.encoder.trim().is_empty() {
        crate::encoder::default_encoder().to_string()
    } else {
        crate::encoder::normalize_encoder(req.encoder.trim())?.to_string()
    };
    let relay_url = normalize_relay(state, &req.relay_url)?;
    if !valid_mount(&req.path) || req.source_password.len() < 4 {
        return Err(Error::Invalid("invalid mount".into()));
    }
    if state.store.station(&req.station_id)?.is_none() {
        return Err(Error::NotFound);
    }
    let hash = auth::hash_password(&req.source_password)?;
    let jingle_media_id = check_bed(state, &req.station_id, req.jingle_media_id)?;
    let intro_media_id = check_bed(state, &req.station_id, req.intro_media_id)?;
    let mount = Mount {
        id: auth::new_id(),
        station_id: req.station_id,
        path: req.path,
        source_user: if req.source_user.is_empty() { "source".into() } else { req.source_user },
        source_password_hash: hash,
        format: "mp3".into(),
        bitrate: engine::clamp_bitrate(req.bitrate) as i64,
        max_listeners: req.max_listeners as i64,
        max_bitrate: req.max_bitrate as i64,
        allow_takeover: req.allow_takeover,
        fallback_playlist_id: empty_opt(req.fallback_playlist_id),
        created_at: store::now_ms(),
        crossfade_ms: engine::clamp_crossfade_ms(req.crossfade_ms) as i64,
        autodj: req.autodj,
        shuffle: req.shuffle,
        repeat,
        jingle_every: req.jingle_every.min(1_000) as i64,
        jingle_media_id,
        intro_media_id,
        gain_db: engine::clamp_gain_db(req.gain_db),
        sample_rate: engine::clamp_sample_rate(req.sample_rate) as i64,
        channels: engine::clamp_channels(req.channels) as i64,
        buffer_ms: engine::clamp_buffer_ms(req.buffer_ms) as i64,
        hls_segment_ms: engine::clamp_hls_segment_ms(req.hls_segment_ms) as i64,
        encoder,
        relay_url,
    };
    state.store.create_mount(&mount)?;
    sync_mount(state, &mount)?;
    state.store.audit(actor, "mount_create", &format!("{} encoder={}", mount.path, mount.encoder))?;
    if !mount.relay_url.is_empty() {
        state.store.audit(actor, "relay_set", &format!("{} {}", mount.path, mount.relay_url))?;
    }
    Ok((201, mount_json(&mount, None, &state.stream_public_url)))
}

fn update_mount(state: &ApiState, actor: &str, id: &str, body: &[u8]) -> Result<(u16, Value), Error> {
    #[derive(Deserialize)]
    struct Req {
        path: Option<String>,
        source_password: Option<String>,
        bitrate: Option<u32>,
        max_listeners: Option<usize>,
        max_bitrate: Option<u32>,
        allow_takeover: Option<bool>,
        fallback_playlist_id: Option<String>,
        crossfade_ms: Option<u64>,
        autodj: Option<bool>,
        shuffle: Option<bool>,
        repeat: Option<Value>,
        jingle_every: Option<u32>,
        jingle_media_id: Option<String>,
        intro_media_id: Option<String>,
        gain_db: Option<f64>,
        sample_rate: Option<u32>,
        channels: Option<u16>,
        buffer_ms: Option<u64>,
        hls_segment_ms: Option<u64>,
        encoder: Option<String>,
        relay_url: Option<String>,
    }
    let req: Req = parse_json(body)?;
    let mut mount = state.store.mount(id)?.ok_or(Error::NotFound)?;
    let old_path = mount.path.clone();
    let old_encoder = mount.encoder.clone();
    let old_relay = mount.relay_url.clone();
    if let Some(path) = req.path {
        if !valid_mount(&path) { return Err(Error::Invalid("invalid path".into())); }
        mount.path = path;
    }
    let rotating = req.source_password.is_some();
    if let Some(pw) = req.source_password {
        if pw.len() < 4 { return Err(Error::Invalid("password too short".into())); }
        mount.source_password_hash = auth::hash_password(&pw)?;
    }
    if let Some(b) = req.bitrate { mount.bitrate = engine::clamp_bitrate(b) as i64; }
    if let Some(n) = req.max_listeners { mount.max_listeners = n as i64; }
    if let Some(b) = req.max_bitrate { mount.max_bitrate = b as i64; }
    if let Some(t) = req.allow_takeover { mount.allow_takeover = t; }
    if let Some(p) = req.fallback_playlist_id { mount.fallback_playlist_id = if p.is_empty() { None } else { Some(p) }; }
    if let Some(ms) = req.crossfade_ms { mount.crossfade_ms = engine::clamp_crossfade_ms(ms) as i64; }
    if let Some(v) = req.autodj { mount.autodj = v; }
    if let Some(v) = req.shuffle { mount.shuffle = v; }
    if let Some(v) = req.repeat { mount.repeat = parse_repeat(&v)?; }
    if let Some(v) = req.gain_db { mount.gain_db = engine::clamp_gain_db(v); }
    if let Some(v) = req.sample_rate { mount.sample_rate = engine::clamp_sample_rate(v) as i64; }
    if let Some(v) = req.channels { mount.channels = engine::clamp_channels(v) as i64; }
    if let Some(v) = req.buffer_ms { mount.buffer_ms = engine::clamp_buffer_ms(v) as i64; }
    if let Some(v) = req.hls_segment_ms { mount.hls_segment_ms = engine::clamp_hls_segment_ms(v) as i64; }
    if let Some(v) = req.encoder { mount.encoder = crate::encoder::normalize_encoder(v.trim())?.to_string(); }
    if let Some(v) = req.relay_url { mount.relay_url = normalize_relay(state, &v)?; }
    mount.format = "mp3".into();
    if let Some(n) = req.jingle_every { mount.jingle_every = n.min(1_000) as i64; }
    if let Some(id) = req.jingle_media_id { mount.jingle_media_id = check_bed(state, &mount.station_id, Some(id))?; }
    if let Some(id) = req.intro_media_id { mount.intro_media_id = check_bed(state, &mount.station_id, Some(id))?; }
    if rotating {
        state.store.audit(actor, "source_password_rotate", &mount.path)?;
    }
    if old_path != mount.path {
        state.engine.remove_path(&old_path);
    }
    state.store.update_mount(&mount)?;
    sync_mount(state, &mount)?;
    state.store.audit(actor, "mount_update", &mount.path)?;
    if old_encoder != mount.encoder {
        state.store.audit(actor, "encoder_set", &format!("{} {}", mount.path, mount.encoder))?;
    }
    if old_relay != mount.relay_url {
        state.store.audit(actor, "relay_set", &format!("{} {}", mount.path, mount.relay_url))?;
    }
    Ok((200, mount_json(&mount, None, &state.stream_public_url)))
}

fn normalize_relay(state: &ApiState, raw: &str) -> Result<String, Error> {
    let url = raw.trim();
    if url.is_empty() {
        return Ok(String::new());
    }
    crate::relay::validate(url, &state.engine.relay_policy())?;
    Ok(url.to_string())
}

fn delete_mount(state: &ApiState, actor: &str, id: &str) -> Result<(u16, Value), Error> {
    let mount = state.store.mount(id)?.ok_or(Error::NotFound)?;
    state.engine.remove_path(&mount.path);
    state.store.delete_mount(id)?;
    state.store.audit(actor, "mount_delete", &mount.path)?;
    Ok((200, json!({"ok": true})))
}

fn disconnect(state: &ApiState, actor: &str, id: &str) -> Result<(u16, Value), Error> {
    let mount = state.store.mount(id)?.ok_or(Error::NotFound)?;
    state.engine.disconnect(&mount.path)?;
    state.store.audit(actor, "mount_disconnect", &mount.path)?;
    Ok((200, json!({"ok": true})))
}

fn set_meta(state: &ApiState, id: &str, body: &[u8]) -> Result<(u16, Value), Error> {
    #[derive(Deserialize)]
    struct Req { #[serde(default)] title: String, #[serde(default)] artist: String }
    let req: Req = parse_json(body)?;
    let mount = state.store.mount(id)?.ok_or(Error::NotFound)?;
    state.engine.set_metadata(&mount.path, &req.title, &req.artist)?;
    Ok((200, json!({"ok": true})))
}

fn rotate_source_password(state: &ApiState, actor: &str, id: &str, body: &[u8]) -> Result<(u16, Value), Error> {
    #[derive(Deserialize)]
    struct Req { password: String }
    let req: Req = parse_json(body)?;
    if req.password.len() < 4 { return Err(Error::Invalid("password too short".into())); }
    let mut mount = state.store.mount(id)?.ok_or(Error::NotFound)?;
    mount.source_password_hash = auth::hash_password(&req.password)?;
    state.store.update_mount(&mount)?;
    sync_mount(state, &mount)?;
    state.store.audit(actor, "source_password_rotate", &mount.path)?;
    Ok((200, json!({"ok": true})))
}

fn queue_get(state: &ApiState, id: &str) -> Result<(u16, Value), Error> {
    let mount = state.store.mount(id)?.ok_or(Error::NotFound)?;
    let snap = state.engine.queue(&mount.path)?;
    let live = state.engine.status().into_iter().find(|s| s.id == mount.id);
    Ok((200, queue_json(&snap, live.as_ref())))
}

fn queue_post(state: &ApiState, actor: &str, id: &str, action: &str, body: &[u8]) -> Result<(u16, Value), Error> {
    let mount = state.store.mount(id)?.ok_or(Error::NotFound)?;
    let path = mount.path.as_str();
    match action {
        "skip" => state.engine.skip(path)?,
        "previous" => state.engine.previous(path)?,
        "next" => {
            #[derive(Deserialize, Default)]
            struct Req { #[serde(default)] media_id: String }
            let req: Req = if body.is_empty() { Req::default() } else { parse_json(body)? };
            state.engine.queue_next(path, &req.media_id)?;
        }
        "move" => {
            #[derive(Deserialize)]
            struct Req { from: usize, to: usize }
            let req: Req = parse_json(body)?;
            state.engine.queue_move(path, req.from, req.to)?;
        }
        "remove" => {
            #[derive(Deserialize)]
            struct Req { index: usize }
            let req: Req = parse_json(body)?;
            state.engine.queue_remove(path, req.index)?;
        }
        "clear" => state.engine.queue_clear(path)?,
        _ => return Err(Error::NotFound),
    }
    state.store.audit(actor, &format!("queue_{action}"), path)?;
    queue_get(state, id)
}

fn seek_mount(state: &ApiState, actor: &str, id: &str, body: &[u8]) -> Result<(u16, Value), Error> {
    #[derive(Deserialize)]
    struct Req { position_ms: u64 }
    let req: Req = parse_json(body)?;
    let mount = state.store.mount(id)?.ok_or(Error::NotFound)?;
    let position_ms = state.engine.seek(&mount.path, req.position_ms)?;
    state.store.audit(actor, "seek", &format!("{} {position_ms}", mount.path))?;
    Ok((200, json!({"position_ms": position_ms})))
}

fn jump_mount(state: &ApiState, actor: &str, id: &str, body: &[u8]) -> Result<(u16, Value), Error> {
    #[derive(Deserialize)]
    struct Req { media_id: String }
    let req: Req = parse_json(body)?;
    if req.media_id.is_empty() {
        return Err(Error::Invalid("media_id required".into()));
    }
    let mount = state.store.mount(id)?.ok_or(Error::NotFound)?;
    state.engine.jump(&mount.path, &req.media_id)?;
    state.store.audit(actor, "jump", &format!("{} {}", mount.path, req.media_id))?;
    Ok((200, json!({"ok": true})))
}

fn play_next_mount(state: &ApiState, actor: &str, id: &str, body: &[u8]) -> Result<(u16, Value), Error> {
    #[derive(Deserialize)]
    struct Req { media_id: String }
    let req: Req = parse_json(body)?;
    if req.media_id.is_empty() {
        return Err(Error::Invalid("media_id required".into()));
    }
    let mount = state.store.mount(id)?.ok_or(Error::NotFound)?;
    state.engine.play_next(&mount.path, &req.media_id)?;
    state.store.audit(actor, "play_next", &format!("{} {}", mount.path, req.media_id))?;
    Ok((200, json!({"ok": true})))
}

fn mount_stats(state: &ApiState, id: &str) -> Result<(u16, Value), Error> {
    let mount = state.store.mount(id)?.ok_or(Error::NotFound)?;
    let live = state.engine.status().into_iter().find(|s| s.id == mount.id || s.path == mount.path);
    let listeners = live.as_ref().map(|s| s.listeners).unwrap_or(0);
    let seen = live.as_ref().map(|s| s.listeners.max(s.peak_listeners)).unwrap_or(0) as i64;
    let peak = state.store.note_peak(&mount.id, seen)?;
    let tracks = state.store.top_tracks(&mount.id, 10)?;
    Ok((200, json!({
        "listeners": listeners,
        "peak_listeners": peak,
        "tracks": tracks.iter().map(|t| json!({
            "media_id": t.media_id,
            "title": t.title,
            "artist": t.artist,
            "plays": t.plays,
        })).collect::<Vec<_>>(),
    })))
}

fn listeners_get(state: &ApiState, id: &str) -> Result<(u16, Value), Error> {
    let mount = state.store.mount(id)?.ok_or(Error::NotFound)?;
    let live = state.engine.status().into_iter().find(|s| s.path == mount.path);
    let rows = live.map(|s| s.listener_rows).unwrap_or_default();
    Ok((200, json!(rows.iter().map(listener_json).collect::<Vec<_>>())))
}

fn kick_one(state: &ApiState, actor: &str, id: &str, lid: &str) -> Result<(u16, Value), Error> {
    let mount = state.store.mount(id)?.ok_or(Error::NotFound)?;
    let lid: u64 = lid.parse().map_err(|_| Error::Invalid("bad listener".into()))?;
    state.engine.kick(&mount.path, lid)?;
    state.store.audit(actor, "listener_kick", &format!("{} {lid}", mount.path))?;
    Ok((200, json!({"ok": true})))
}

fn kick_all(state: &ApiState, actor: &str, id: &str) -> Result<(u16, Value), Error> {
    let mount = state.store.mount(id)?.ok_or(Error::NotFound)?;
    let n = state.engine.kick_all(&mount.path)?;
    state.store.audit(actor, "listener_kick_all", &format!("{} {n}", mount.path))?;
    Ok((200, json!({"ok": true, "kicked": n})))
}

fn upload(state: &ApiState, actor: &str, head: &Head, body: &[u8]) -> Result<(u16, Value), Error> {
    if body.len() as u64 > state.max_upload + 65_536 {
        return Err(Error::TooLarge);
    }
    let ctype = head.get("content-type").unwrap_or("");
    let parts = parse_multipart(body, ctype)?;
    let station_id = parts.iter().find(|p| p.name == "station_id").map(|p| String::from_utf8_lossy(&p.data).trim().to_string()).unwrap_or_default();
    let file = parts.iter().find(|p| p.name == "file").ok_or_else(|| Error::Invalid("file required".into()))?;
    if file.data.len() as u64 > state.max_upload {
        return Err(Error::TooLarge);
    }
    if state.store.station(&station_id)?.is_none() {
        return Err(Error::NotFound);
    }
    let kind = probe(&file.data)?;
    let tags = media::read_id3(&file.data);
    let id = auth::new_id();
    std::fs::create_dir_all(&state.media_root)?;
    let path = media::media_path(&state.media_root, &id)?;
    std::fs::write(&path, &file.data)?;
    let filename = media::display_name(file.filename.as_deref().unwrap_or("track"));
    let title = if tags.title.is_empty() { filename.clone() } else { tags.title };
    let row = Media {
        id: id.clone(), station_id, filename, mime: kind.mime.into(), bytes: file.data.len() as i64,
        title, artist: tags.artist, created_at: store::now_ms(),
    };
    if let Err(err) = state.store.create_media(&row) {
        let _ = std::fs::remove_file(&path);
        return Err(err);
    }
    state.store.audit(actor, "upload", &id)?;
    Ok((201, media_json(&row)))
}

fn import_media(state: &ApiState, actor: &str, body: &[u8]) -> Result<(u16, Value), Error> {
    #[derive(Deserialize)]
    struct Req { station_id: String, path: String }
    let req: Req = parse_json(body)?;
    let path = req.path.trim();
    if path.is_empty() {
        return Err(Error::Invalid("folder path required".into()));
    }
    let report = media::import_folder(&state.store, &state.media_root, &req.station_id, Path::new(path), state.max_upload, actor)?;
    Ok((200, json!({"imported": report.imported, "skipped": report.skipped, "failed": report.failed})))
}

fn list_media(state: &ApiState, head: &Head) -> Result<(u16, Value), Error> {
    let station = query_param(&head.query, "station").ok_or_else(|| Error::Invalid("station required".into()))?;
    let q = query_param(&head.query, "q").unwrap_or_default();
    let rows = state.store.search_media(&station, &q)?;
    Ok((200, json!(rows.iter().map(media_json).collect::<Vec<_>>())))
}

fn tag_media(state: &ApiState, actor: &str, id: &str, body: &[u8]) -> Result<(u16, Value), Error> {
    #[derive(Deserialize)]
    struct Req { #[serde(default)] title: String, #[serde(default)] artist: String }
    let req: Req = parse_json(body)?;
    if !media::valid_id(id) { return Err(Error::NotFound); }
    if state.store.media(id)?.is_none() { return Err(Error::NotFound); }
    let title = req.title.trim().to_string();
    let artist = req.artist.trim().to_string();
    state.store.update_media_tags(id, &title, &artist)?;
    state.store.audit(actor, "media_tag", id)?;
    let row = state.store.media(id)?.ok_or(Error::NotFound)?;
    Ok((200, media_json(&row)))
}

fn replace_media(state: &ApiState, actor: &str, id: &str, head: &Head, body: &[u8]) -> Result<(u16, Value), Error> {
    if !media::valid_id(id) { return Err(Error::NotFound); }
    let existing = state.store.media(id)?.ok_or(Error::NotFound)?;
    if body.len() as u64 > state.max_upload + 65_536 { return Err(Error::TooLarge); }
    let ctype = head.get("content-type").unwrap_or("");
    let parts = parse_multipart(body, ctype)?;
    let file = parts.iter().find(|p| p.name == "file").ok_or_else(|| Error::Invalid("file required".into()))?;
    if file.data.len() as u64 > state.max_upload { return Err(Error::TooLarge); }
    let kind = probe(&file.data)?;
    let tags = media::read_id3(&file.data);
    let path = media::media_path(&state.media_root, id)?;
    let tmp = path.with_extension("replacing");
    std::fs::write(&tmp, &file.data)?;
    if let Err(err) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err.into());
    }
    let filename = media::display_name(file.filename.as_deref().unwrap_or(&existing.filename));
    let title = if tags.title.is_empty() { filename.clone() } else { tags.title };
    state.store.replace_media(id, &filename, kind.mime, file.data.len() as i64, &title, &tags.artist)?;
    state.store.audit(actor, "media_replace", id)?;
    let row = state.store.media(id)?.ok_or(Error::NotFound)?;
    Ok((200, media_json(&row)))
}

fn delete_media(state: &ApiState, actor: &str, id: &str) -> Result<(u16, Value), Error> {
    if !media::valid_id(id) {
        return Err(Error::NotFound);
    }
    let _ = state.store.media(id)?.ok_or(Error::NotFound)?;
    state.store.delete_media(id)?;
    if let Ok(path) = media::media_path(&state.media_root, id) {
        let _ = std::fs::remove_file(path);
    }
    state.store.audit(actor, "media_delete", id)?;
    Ok((200, json!({"ok": true})))
}

fn list_playlists(state: &ApiState, head: &Head) -> Result<(u16, Value), Error> {
    let station = query_param(&head.query, "station").ok_or_else(|| Error::Invalid("station required".into()))?;
    let lists = state.store.list_playlists(&station)?;
    let mut out = Vec::new();
    for pl in lists {
        let items = state.store.playlist_items(&pl.id)?;
        out.push(json!({
            "id": pl.id, "station_id": pl.station_id, "name": pl.name,
            "items": items.iter().map(|i| json!({"id": i.id, "media_id": i.media_id, "title": i.title, "artist": i.artist, "filename": i.filename, "position": i.position})).collect::<Vec<_>>(),
        }));
    }
    Ok((200, json!(out)))
}

fn create_playlist(state: &ApiState, actor: &str, body: &[u8]) -> Result<(u16, Value), Error> {
    #[derive(Deserialize)]
    struct Req { station_id: String, name: String }
    let req: Req = parse_json(body)?;
    if req.name.trim().is_empty() { return Err(Error::Invalid("name required".into())); }
    if state.store.station(&req.station_id)?.is_none() { return Err(Error::NotFound); }
    let pl = state.store.create_playlist(&req.station_id, req.name.trim())?;
    state.store.audit(actor, "playlist_create", &pl.id)?;
    Ok((201, json!({"id": pl.id, "station_id": pl.station_id, "name": pl.name, "items": []})))
}

fn add_item(state: &ApiState, actor: &str, playlist_id: &str, body: &[u8]) -> Result<(u16, Value), Error> {
    #[derive(Deserialize)]
    struct Req { media_id: String }
    let req: Req = parse_json(body)?;
    if !media::valid_id(&req.media_id) { return Err(Error::Invalid("invalid media id".into())); }
    state.store.add_playlist_item(playlist_id, &req.media_id)?;
    state.store.audit(actor, "playlist_add", playlist_id)?;
    Ok((200, json!({"ok": true})))
}

fn reorder_playlist(state: &ApiState, actor: &str, id: &str, body: &[u8]) -> Result<(u16, Value), Error> {
    #[derive(Deserialize)]
    struct Req { from: usize, to: usize }
    let req: Req = parse_json(body)?;
    state.store.reorder_playlist(id, req.from, req.to)?;
    state.store.audit(actor, "playlist_reorder", id)?;
    Ok((200, json!({"ok": true})))
}

fn delete_playlist_item(state: &ApiState, actor: &str, playlist_id: &str, item_id: &str) -> Result<(u16, Value), Error> {
    state.store.delete_playlist_item(playlist_id, item_id)?;
    state.store.audit(actor, "playlist_remove", playlist_id)?;
    Ok((200, json!({"ok": true})))
}

fn delete_playlist(state: &ApiState, actor: &str, id: &str) -> Result<(u16, Value), Error> {
    state.store.delete_playlist(id)?;
    state.store.audit(actor, "playlist_delete", id)?;
    Ok((200, json!({"ok": true})))
}

fn list_schedules(state: &ApiState, head: &Head) -> Result<(u16, Value), Error> {
    let mount = query_param(&head.query, "mount").ok_or_else(|| Error::Invalid("mount required".into()))?;
    let rows = state.store.list_schedules(&mount)?;
    Ok((200, json!(rows.iter().map(schedule_json).collect::<Vec<_>>())))
}

fn add_schedule(state: &ApiState, actor: &str, body: &[u8]) -> Result<(u16, Value), Error> {
    #[derive(Deserialize)]
    struct Req { mount_id: String, playlist_id: String, days: Vec<String>, start: String, end: String, #[serde(default = "one")] priority: i32 }
    fn one() -> i32 { 1 }
    let req: Req = parse_json(body)?;
    let mask = schedule::parse_days(&req.days).ok_or_else(|| Error::Invalid("days".into()))?;
    let start = schedule::parse_hhmm(&req.start).ok_or_else(|| Error::Invalid("start".into()))?;
    let end = schedule::parse_hhmm(&req.end).ok_or_else(|| Error::Invalid("end".into()))?;
    let row = Schedule {
        id: auth::new_id(), mount_id: req.mount_id, playlist_id: req.playlist_id,
        days_mask: mask as i64, start_min: start as i64, end_min: end as i64, priority: req.priority as i64,
        enabled: true,
    };
    state.store.add_schedule(&row)?;
    state.store.audit(actor, "schedule_create", &row.id)?;
    Ok((201, schedule_json(&row)))
}

fn update_schedule(state: &ApiState, actor: &str, id: &str, body: &[u8]) -> Result<(u16, Value), Error> {
    #[derive(Deserialize)]
    struct Req { enabled: Option<bool>, priority: Option<i64> }
    let req: Req = parse_json(body)?;
    if req.enabled.is_none() && req.priority.is_none() {
        return Err(Error::Invalid("enabled or priority required".into()));
    }
    let row = state.store.update_schedule(id, req.enabled, req.priority)?;
    state.store.audit(actor, "schedule_update", id)?;
    Ok((200, schedule_json(&row)))
}

fn list_audit(state: &ApiState, head: &Head) -> Result<(u16, Value), Error> {
    let limit = query_param(&head.query, "limit").and_then(|s| s.parse().ok()).unwrap_or(100);
    let rows = state.store.list_audit(limit)?;
    Ok((200, json!(rows.iter().map(|a| json!({"id": a.id, "at": a.at, "actor": a.actor, "action": a.action, "detail": a.detail})).collect::<Vec<_>>())))
}

fn list_users(state: &ApiState) -> Result<(u16, Value), Error> {
    let users = state.store.list_users()?;
    Ok((200, json!(users.iter().map(|u| json!({"id": u.id, "username": u.username, "role": u.role})).collect::<Vec<_>>())))
}

fn create_user(state: &ApiState, actor: &str, body: &[u8]) -> Result<(u16, Value), Error> {
    #[derive(Deserialize)]
    struct Req { username: String, password: String }
    let req: Req = parse_json(body)?;
    if !valid_user(&req.username) || req.password.len() < 8 {
        return Err(Error::Invalid("invalid user".into()));
    }
    let hash = auth::hash_password(&req.password)?;
    let user = state.store.create_user(&req.username, &hash, "admin")?;
    state.store.audit(actor, "user_create", &user.username)?;
    Ok((201, json!({"id": user.id, "username": user.username, "role": user.role})))
}

fn rotate(state: &ApiState, head: &Head) -> Result<(u16, Value), Error> {
    let user = require_user(state, head)?;
    state.store.revoke_user_tokens(&user.id)?;
    let token = auth::new_token()?;
    state.store.create_token(&user.id, "rotate", &auth::hash_token(&token))?;
    state.store.audit(&user.username, "token_rotate", "")?;
    Ok((200, json!({"token": token})))
}

fn sync_mount(state: &ApiState, mount: &Mount) -> Result<(), Error> {
    let station = state.store.station(&mount.station_id)?.ok_or(Error::NotFound)?;
    state.engine.upsert(MountSpec {
        id: mount.id.clone(),
        station_id: mount.station_id.clone(),
        station_name: station.name,
        genre: station.genre,
        path: mount.path.clone(),
        source_user: mount.source_user.clone(),
        pass_hash: mount.source_password_hash.clone(),
        format: mount.format.clone(),
        bitrate: mount.bitrate.max(32_000) as u32,
        max_listeners: mount.max_listeners.max(0) as usize,
        max_bitrate: mount.max_bitrate.max(0) as u32,
        allow_takeover: mount.allow_takeover,
        crossfade_ms: mount.crossfade_ms.max(0) as u64,
        autodj: mount.autodj,
        shuffle: mount.shuffle,
        repeat: RepeatMode::parse(&mount.repeat).unwrap_or(RepeatMode::All),
        jingle_every: mount.jingle_every.max(0) as u32,
        jingle_media_id: mount.jingle_media_id.clone(),
        intro_media_id: mount.intro_media_id.clone(),
        gain_db: mount.gain_db,
        sample_rate: mount.sample_rate.max(0) as u32,
        channels: mount.channels.max(0) as u16,
        buffer_ms: mount.buffer_ms.max(0) as u64,
        hls_segment_ms: mount.hls_segment_ms.max(0) as u64,
        encoder: if mount.encoder.is_empty() { crate::encoder::default_encoder().into() } else { mount.encoder.clone() },
        relay_url: mount.relay_url.clone(),
    });
    Ok(())
}

pub fn sync_all(state: &ApiState) -> Result<(), Error> {
    for mount in state.store.list_mounts(None)? {
        sync_mount(state, &mount)?;
    }
    Ok(())
}

fn live_json(rows: &[crate::engine::MountStatus], base: &str) -> Value {
    Value::Array(rows.iter().map(|s| {
        let urls = public_mount_urls(base, &s.path);
        json!({
        "id": s.id,
        "station_id": s.station_id,
        "station_name": s.station_name,
        "path": s.path,
        "live": s.live,
        "listeners": s.listeners,
        "bitrate": s.bitrate,
        "now": now_json(&s.now),
        "history": s.history.iter().map(|h| json!({"title": h.title, "artist": h.artist, "source": h.source, "media_id": h.media_id})).collect::<Vec<_>>(),
        "listener_rows": s.listener_rows.iter().map(listener_json).collect::<Vec<_>>(),
        "peak_listeners": s.peak_listeners,
        "bytes_sent": s.bytes_sent,
        "uptime_sec": s.uptime_sec,
        "mode": s.mode,
        "playing": s.playing,
        "position_ms": s.position_ms,
        "duration_ms": s.duration_ms,
        "shuffle": s.shuffle,
        "repeat": s.repeat,
        "gain_db": s.gain_db,
        "sample_rate": s.sample_rate,
        "channels": s.channels,
        "crossfade_ms": s.crossfade_ms,
        "buffer_ms": s.buffer_ms,
        "hls_segment_ms": s.hls_segment_ms,
        "autodj": s.autodj,
        "allow_takeover": s.allow_takeover,
        "format": s.format,
        "encoder": s.encoder,
        "relay_url": s.relay_url,
        "stream_url": urls.0,
        "hls_url": urls.1,
        })
    }).collect())
}

fn public_mount_urls(base: &str, path: &str) -> (String, String) {
    let base = base.trim_end_matches('/');
    if base.is_empty() {
        return (String::new(), String::new());
    }
    let stream = format!("{base}{}", path);
    (stream.clone(), format!("{stream}/index.m3u8"))
}

fn station_json(s: &Station) -> Value {
    json!({"id": s.id, "name": s.name, "slug": s.slug, "description": s.description, "genre": s.genre})
}

fn mount_json(m: &Mount, live: Option<&crate::engine::MountStatus>, base: &str) -> Value {
    let urls = public_mount_urls(base, &m.path);
    json!({
        "id": m.id, "station_id": m.station_id, "path": m.path, "source_user": m.source_user,
        "format": m.format, "bitrate": m.bitrate, "max_listeners": m.max_listeners,
        "max_bitrate": m.max_bitrate, "allow_takeover": m.allow_takeover,
        "fallback_playlist_id": m.fallback_playlist_id,
        "crossfade_ms": m.crossfade_ms,
        "autodj": m.autodj,
        "shuffle": m.shuffle,
        "repeat": m.repeat,
        "gain_db": m.gain_db,
        "sample_rate": m.sample_rate,
        "channels": m.channels,
        "buffer_ms": m.buffer_ms,
        "hls_segment_ms": m.hls_segment_ms,
        "position_ms": live.map(|s| s.position_ms).unwrap_or(0),
        "duration_ms": live.map(|s| s.duration_ms).unwrap_or(0),
        "jingle_every": m.jingle_every,
        "jingle_media_id": m.jingle_media_id,
        "intro_media_id": m.intro_media_id,
        "live": live.map(|s| s.live).unwrap_or(false),
        "listeners": live.map(|s| s.listeners).unwrap_or(0),
        "peak_listeners": live.map(|s| s.peak_listeners).unwrap_or(0),
        "bytes_sent": live.map(|s| s.bytes_sent).unwrap_or(0),
        "uptime_sec": live.map(|s| s.uptime_sec).unwrap_or(0),
        "mode": live.map(|s| s.mode.clone()).unwrap_or_else(|| if m.autodj { "autodj".into() } else { "idle".into() }),
        "playing": live.map(|s| s.playing).unwrap_or(false),
        "now": live.map(|s| now_json(&s.now)).unwrap_or(json!(null)),
        "listener_rows": live.map(|s| s.listener_rows.iter().map(listener_json).collect::<Vec<_>>()).unwrap_or_default(),
        "encoder": if m.encoder.is_empty() { "ffmpeg" } else { m.encoder.as_str() },
        "relay_url": m.relay_url,
        "stream_url": urls.0,
        "hls_url": urls.1,
    })
}

fn media_json(m: &Media) -> Value {
    json!({"id": m.id, "station_id": m.station_id, "filename": m.filename, "mime": m.mime, "bytes": m.bytes, "title": m.title, "artist": m.artist})
}

fn schedule_json(s: &Schedule) -> Value {
    json!({
        "id": s.id, "mount_id": s.mount_id, "playlist_id": s.playlist_id, "days_mask": s.days_mask,
        "start_min": s.start_min, "end_min": s.end_min, "priority": s.priority, "enabled": s.enabled,
    })
}

fn now_json(n: &crate::engine::NowPlaying) -> Value {
    json!({
        "title": n.title, "artist": n.artist, "source": n.source,
        "elapsed_ms": n.elapsed_ms, "position_ms": n.position_ms, "duration_ms": n.duration_ms, "media_id": n.media_id,
    })
}

fn listener_json(l: &crate::engine::ListenerRow) -> Value {
    json!({
        "id": l.id, "ip": l.ip, "connected_ms": l.connected_ms,
        "connected_sec": l.connected_ms / 1000, "bytes": l.bytes, "user_agent": l.user_agent,
    })
}

fn queue_json(snap: &crate::engine::QueueSnapshot, live: Option<&crate::engine::MountStatus>) -> Value {
    let item = |q: &crate::engine::QueueItem| json!({
        "index": q.index, "media_id": q.media_id, "title": q.title, "artist": q.artist, "duration_ms": q.duration_ms,
    });
    if live.map(|s| !s.playing).unwrap_or(false) {
        return json!({
            "now": null,
            "elapsed_ms": 0,
            "duration_ms": 0,
            "upcoming": snap.upcoming.iter().map(item).collect::<Vec<_>>(),
            "history": snap.history.iter().map(item).collect::<Vec<_>>(),
        });
    }
    let (elapsed, duration, now) = if let Some(live) = live {
        if live.live {
            (live.now.elapsed_ms, live.now.duration_ms, json!({"title": live.now.title, "artist": live.now.artist, "source": "live", "media_id": live.now.media_id, "duration_ms": 0}))
        } else if let Some(now) = &snap.now {
            (snap.elapsed_ms, snap.duration_ms, item(now))
        } else {
            (live.now.elapsed_ms, live.now.duration_ms, now_json(&live.now))
        }
    } else if let Some(now) = &snap.now {
        (snap.elapsed_ms, snap.duration_ms, item(now))
    } else {
        (0, 0, json!(null))
    };
    json!({
        "now": now,
        "elapsed_ms": elapsed,
        "duration_ms": duration,
        "upcoming": snap.upcoming.iter().map(item).collect::<Vec<_>>(),
        "history": snap.history.iter().map(item).collect::<Vec<_>>(),
    })
}

fn default_true() -> bool { true }
fn default_fade() -> u64 { 250 }
fn default_repeat() -> Value { Value::String("all".into()) }
fn default_rate() -> u32 { 44_100 }
fn default_channels() -> u16 { 2 }
fn default_buffer() -> u64 { 250 }
fn default_hls() -> u64 { 1_000 }

fn parse_repeat(v: &Value) -> Result<String, Error> {
    match v {
        Value::Bool(true) => Ok("all".into()),
        Value::Bool(false) => Ok("off".into()),
        Value::String(s) if matches!(s.as_str(), "off" | "one" | "all") => Ok(s.clone()),
        _ => Err(Error::Invalid("repeat must be off, one, or all".into())),
    }
}
fn empty_opt(v: Option<String>) -> Option<String> { v.filter(|s| !s.is_empty()) }

fn check_bed(state: &ApiState, station_id: &str, id: Option<String>) -> Result<Option<String>, Error> {
    let Some(id) = id.filter(|s| !s.is_empty()) else { return Ok(None) };
    if !media::valid_id(&id) { return Err(Error::Invalid("invalid media id".into())); }
    let row = state.store.media(&id)?.ok_or(Error::NotFound)?;
    if row.station_id != station_id { return Err(Error::Invalid("media is on another station".into())); }
    Ok(Some(id))
}

fn parse_json<T: for<'a> Deserialize<'a>>(body: &[u8]) -> Result<T, Error> {
    serde_json::from_slice(body).map_err(|_| Error::Invalid("invalid json".into()))
}

fn query_param(q: &str, key: &str) -> Option<String> {
    for pair in q.split('&') {
        let (k, v) = pair.split_once('=')?;
        if k == key {
            return Some(urlencoding_decode(v));
        }
    }
    None
}

fn urlencoding_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' { out.push(b' '); } else { out.push(bytes[i]); }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn valid_slug(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty() && b.len() <= 40
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

fn valid_mount(s: &str) -> bool {
    s.starts_with('/') && s.len() > 1 && s.len() <= 64 && !s.contains("..") && !s.contains("//")
        && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'/' || c == b'_' || c == b'-')
}

fn valid_user(s: &str) -> bool {
    let b = s.as_bytes();
    (3..=32).contains(&b.len()) && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

fn default_user() -> String { "source".into() }
fn default_br() -> u32 { 128_000 }
fn default_listeners() -> usize { 100 }
fn default_maxbr() -> u32 { 320_000 }

struct Part { name: String, filename: Option<String>, data: Vec<u8> }

fn parse_multipart(body: &[u8], content_type: &str) -> Result<Vec<Part>, Error> {
    let boundary = content_type.split(';').find_map(|p| {
        let p = p.trim();
        p.strip_prefix("boundary=").map(|b| b.trim_matches('"').to_string())
    }).ok_or_else(|| Error::Invalid("multipart boundary".into()))?;
    let marker = format!("--{boundary}");
    let text_marker = marker.as_bytes();
    let mut parts = Vec::new();
    let mut rest = body;
    while let Some(idx) = find_sub(rest, text_marker) {
        rest = &rest[idx + text_marker.len()..];
        if rest.starts_with(b"--") { break; }
        if rest.starts_with(b"\r\n") { rest = &rest[2..]; }
        let Some(sep) = find_sub(rest, b"\r\n\r\n") else { break };
        let header = &rest[..sep];
        rest = &rest[sep + 4..];
        let end = find_sub(rest, text_marker).unwrap_or(rest.len());
        let mut data = &rest[..end];
        if data.ends_with(b"\r\n") { data = &data[..data.len() - 2]; }
        rest = &rest[end..];
        let header_s = String::from_utf8_lossy(header);
        let mut name = String::new();
        let mut filename = None;
        for line in header_s.lines() {
            let lower = line.to_ascii_lowercase();
            if lower.starts_with("content-disposition:") {
                name = disp_param(line, "name").unwrap_or_default();
                filename = disp_param(line, "filename");
            }
        }
        if !name.is_empty() {
            parts.push(Part { name, filename, data: data.to_vec() });
        }
    }
    if parts.is_empty() { return Err(Error::Invalid("empty upload".into())); }
    Ok(parts)
}

fn disp_param(line: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=\"");
    let i = line.find(&needle)?;
    let rest = &line[i + needle.len()..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

pub fn bootstrap(store: &Store, username: Option<String>, password: Option<String>) -> Result<Option<(String, String, String)>, Error> {
    if store.count_users()? > 0 {
        return Ok(None);
    }
    let username = username.unwrap_or_else(|| "admin".into());
    let password = match password {
        Some(p) => p,
        None => auth::random_password()?,
    };
    if password.len() < 8 || !valid_user(&username) {
        return Err(Error::Invalid("NAVA_ADMIN_USER / NAVA_ADMIN_PASSWORD are not usable".into()));
    }
    let hash = auth::hash_password(&password)?;
    let user = store.create_user(&username, &hash, "admin")?;
    let token = auth::new_token()?;
    store.create_token(&user.id, "bootstrap", &auth::hash_token(&token))?;
    store.audit(&username, "bootstrap", "")?;
    Ok(Some((username, password, token)))
}

pub fn state_from_config(cfg: &Config, store: Arc<Store>, engine: Arc<Engine>, allow_admin: bool) -> ApiState {
    ApiState {
        store,
        engine,
        media_root: PathBuf::from(&cfg.data_dir).join("media"),
        max_upload: cfg.max_upload_bytes,
        stream_public_url: cfg.stream_public_url.clone(),
        http_public_url: cfg.http_public_url.clone(),
        allow_admin,
        trusted: Trusted::parse(&cfg.trusted_proxies),
        fails: FailWindow::new(8, Duration::from_secs(60)),
    }
}

/// Dashboard files are served as bytes. `/api` is JSON.
pub fn serve_static_or_api(state: &ApiState, head: &Head, body: &[u8], ip: &str, socket_ip: &str) -> Result<HttpOut, Error> {
    let path = head.path.as_str();
    if !path.starts_with("/api") {
        if head.method != "GET" && head.method != "HEAD" {
            return Err(Error::NotFound);
        }
        let (bytes, mime) = static_file(path).ok_or(Error::NotFound)?;
        return Ok(HttpOut { code: 200, content_type: mime.to_string(), body: if head.method == "HEAD" { Vec::new() } else { bytes.to_vec() } });
    }
    let (code, value) = dispatch(state, head, body, ip, socket_ip)?;
    let bytes = if code == 204 { Vec::new() } else { serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec()) };
    Ok(HttpOut { code, content_type: "application/json".into(), body: bytes })
}

pub struct HttpOut {
    pub code: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

pub fn epoch_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[allow(dead_code)]
fn _header_touch() { let _ = header_safe; }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::IdentityFactory;
    use crate::engine::{EngineOpts, NoTracks};
    use std::io::Write;

    fn boot(max_upload: u64) -> (ApiHandle, Arc<Store>, String, PathBuf) {
        let dir = std::env::temp_dir().join(format!("radio-api-{}-{}", std::process::id(), max_upload));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("media")).unwrap();
        let store = Arc::new(Store::open(&dir.join("r.db")).unwrap());
        let hash = auth::hash_with("password123", auth::HashParams::fast()).unwrap();
        let user = store.create_user("admin", &hash, "admin").unwrap();
        let token = auth::new_token().unwrap();
        store.create_token(&user.id, "test", &auth::hash_token(&token)).unwrap();
        let opts = EngineOpts::production(dir.join("media"), 16_000, 8, Duration::from_millis(20));
        let engine = Arc::new(Engine::new(Arc::new(IdentityFactory), Arc::new(NoTracks), opts));
        let state = Arc::new(ApiState {
            store: Arc::clone(&store),
            engine,
            media_root: dir.join("media"),
            max_upload,
            stream_public_url: "http://127.0.0.1:8000".into(),
            http_public_url: "http://127.0.0.1:8088".into(),
            allow_admin: true,
            trusted: Trusted::parse(&["127.0.0.1".into()]),
            fails: FailWindow::new(8, Duration::from_secs(60)),
        });
        let handle = spawn("127.0.0.1:0", state).unwrap();
        (handle, store, token, dir)
    }

    fn call(addr: SocketAddr, req: &str, body: &[u8]) -> (u16, Vec<u8>) {
        let mut sock = TcpStream::connect(addr).unwrap();
        sock.write_all(req.as_bytes()).unwrap();
        if !body.is_empty() { sock.write_all(body).unwrap(); }
        sock.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 2048];
        loop {
            match sock.read(&mut tmp) {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        }
        let text = String::from_utf8_lossy(&buf);
        let code = text.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        (code, buf)
    }

    #[test]
    fn auth_upload_limits_and_ui() {
        let (handle, store, token, dir) = boot(128);
        let addr = handle.addr;

        let (code, _) = call(addr, "GET /api/stations HTTP/1.0\r\n\r\n", b"");
        assert_eq!(code, 401);

        let bad_login = br#"{"username":"admin","password":"nope"}"#;
        let req = format!("POST /api/login HTTP/1.0\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", bad_login.len());
        let (code, body) = call(addr, &req, bad_login);
        assert_eq!(code, 401, "{}", String::from_utf8_lossy(&body));
        let audits = store.list_audit(20).unwrap();
        assert!(audits.iter().any(|a| a.action == "login_fail"));

        let (code, body) = call(addr, "GET / HTTP/1.0\r\n\r\n", b"");
        assert_eq!(code, 200);
        assert!(String::from_utf8_lossy(&body).contains("id=\"root\"") || String::from_utf8_lossy(&body).contains("id=\\\"root\\\"") || body.windows(8).any(|w| w == b"id=\"root\""));

        let station_body = br#"{"name":"Night","slug":"night","genre":"jazz"}"#;
        let req = format!("POST /api/stations HTTP/1.0\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", station_body.len());
        let (code, body) = call(addr, &req, station_body);
        assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));

        // Oversize upload
        let big = vec![b'A'; 20_000];
        let req = format!("POST /api/media HTTP/1.0\r\nAuthorization: Bearer {token}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n", big.len());
        let (code, _) = call(addr, &req, &big);
        assert_eq!(code, 413);

        // Bad type that is small enough
        let (handle2, store2, token2, dir2) = boot(50_000);
        let payload = b"this is text!!";
        let _raw = multipart_body("station", "note.txt", payload);
        // station id is invalid; probe fails first if we send a real station. Create station via SQL-less API.
        let sb = br#"{"name":"N","slug":"n"}"#;
        let req = format!("POST /api/stations HTTP/1.0\r\nAuthorization: Bearer {token2}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", sb.len());
        let (code, body) = call(handle2.addr, &req, sb);
        assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
        let v: Value = serde_json::from_slice(&body_of(&body)).unwrap();
        let sid = v["id"].as_str().unwrap().to_string();
        let raw = multipart_body(&sid, "note.txt", payload);
        let req = format!(
            "POST /api/media HTTP/1.0\r\nAuthorization: Bearer {token2}\r\nContent-Type: multipart/form-data; boundary=----r\r\nContent-Length: {}\r\n\r\n",
            raw.len()
        );
        let (code, body) = call(handle2.addr, &req, &raw);
        assert_eq!(code, 415, "{}", String::from_utf8_lossy(&body));

        let mut mp3 = b"ID3\x04\x00\x00\x00\x00\x00\x00".to_vec();
        mp3.extend_from_slice(&[0u8; 16]);
        let raw = multipart_body(&sid, "../../etc/passwd.mp3", &mp3);
        let req = format!(
            "POST /api/media HTTP/1.0\r\nAuthorization: Bearer {token2}\r\nContent-Type: multipart/form-data; boundary=----r\r\nContent-Length: {}\r\n\r\n",
            raw.len()
        );
        let (code, body) = call(handle2.addr, &req, &raw);
        assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
        let v: Value = serde_json::from_slice(&body_of(&body)).unwrap();
        let id = v["id"].as_str().unwrap();
        assert!(media::valid_id(id));
        assert_eq!(v["filename"], "passwd.mp3");
        let stored = dir2.join("media").join(id);
        assert!(stored.is_file());
        // Traversal id is not a route to the file.
        let (code, _) = call(handle2.addr, "GET /api/media/../secret HTTP/1.0\r\nAuthorization: Bearer x\r\n\r\n", b"");
        assert!(code == 400 || code == 401 || code == 404);
        let audits = store2.list_audit(20).unwrap();
        assert!(audits.iter().any(|a| a.action == "upload"));
        let _ = (store, dir);
    }

    #[test]
    fn stream_settings_are_clamped_on_patch_and_live() {
        let (handle, store, token, _dir) = boot(77);
        let addr = handle.addr;
        let sb = br#"{"name":"Night","slug":"night"}"#;
        let req = format!("POST /api/stations HTTP/1.0\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", sb.len());
        let (code, body) = call(addr, &req, sb);
        assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
        let station: Value = serde_json::from_slice(&body_of(&body)).unwrap();
        let sid = station["id"].as_str().unwrap();
        let create = format!(r#"{{"station_id":"{sid}","path":"/live","source_password":"secret"}}"#);
        let req = format!("POST /api/mounts HTTP/1.0\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", create.len());
        let (code, body) = call(addr, &req, create.as_bytes());
        assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
        let mount: Value = serde_json::from_slice(&body_of(&body)).unwrap();
        assert_eq!(mount["repeat"], "all");
        assert_eq!(mount["buffer_ms"], 250);
        assert_eq!(mount["hls_segment_ms"], 1000);
        assert_eq!(mount["format"], "mp3");
        let id = mount["id"].as_str().unwrap();

        let bad = br#"{"repeat":"nope"}"#;
        let req = format!("PATCH /api/mounts/{id} HTTP/1.0\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", bad.len());
        let (code, _) = call(addr, &req, bad);
        assert_eq!(code, 400);

        let patch = br#"{"buffer_ms":0,"hls_segment_ms":9,"bitrate":1000,"gain_db":90,"sample_rate":12345,"channels":5,"repeat":"one","crossfade_ms":999999,"shuffle":true}"#;
        let req = format!("PATCH /api/mounts/{id} HTTP/1.0\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", patch.len());
        let (code, body) = call(addr, &req, patch);
        assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));
        let mount: Value = serde_json::from_slice(&body_of(&body)).unwrap();
        assert_eq!(mount["buffer_ms"], 50);
        assert_eq!(mount["hls_segment_ms"], 250);
        assert_eq!(mount["bitrate"], 32_000);
        assert_eq!(mount["gain_db"], 24.0);
        assert_eq!(mount["sample_rate"], 11_025);
        assert_eq!(mount["channels"], 2);
        assert_eq!(mount["repeat"], "one");
        assert_eq!(mount["crossfade_ms"], 10_000);
        assert_eq!(mount["shuffle"], true);

        let req = format!("GET /api/live HTTP/1.0\r\nAuthorization: Bearer {token}\r\n\r\n");
        let (code, body) = call(addr, &req, b"");
        assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));
        let live: Value = serde_json::from_slice(&body_of(&body)).unwrap();
        let row = live.as_array().unwrap().iter().find(|r| r["id"] == id).unwrap();
        assert_eq!(row["buffer_ms"], 50);
        assert_eq!(row["hls_segment_ms"], 250);
        assert_eq!(row["repeat"], "one");
        assert_eq!(row["gain_db"], 24.0);
        assert!(row["playing"].is_boolean());
        assert!(row["position_ms"].is_number());
        assert!(row["duration_ms"].is_number());
        assert_eq!(row["format"], "mp3");
        assert!(store.list_audit(20).unwrap().iter().any(|a| a.action == "mount_update"));
    }

    #[test]
    fn admin_login_is_rate_limited() {
        let (handle, _store, _token, _dir) = boot(96);
        let addr = handle.addr;
        let bad = br#"{"username":"admin","password":"nope"}"#;
        let req = format!("POST /api/login HTTP/1.0\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", bad.len());
        for _ in 0..8 {
            let (code, body) = call(addr, &req, bad);
            assert_eq!(code, 401, "{}", String::from_utf8_lossy(&body));
        }
        let (code, body) = call(addr, &req, bad);
        assert_eq!(code, 429, "{}", String::from_utf8_lossy(&body));
    }

    #[test]
    fn folder_import_api_rejects_traversal_and_reads_a_folder() {
        let (handle, store, token, dir) = boot(8192);
        let addr = handle.addr;
        let denied = br#"{"station_id":"x","path":"/tmp"}"#;
        let req = format!("POST /api/media/import HTTP/1.0\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", denied.len());
        let (code, _) = call(addr, &req, denied);
        assert_eq!(code, 401);

        let sb = br#"{"name":"Night","slug":"night"}"#;
        let req = format!("POST /api/stations HTTP/1.0\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", sb.len());
        let (code, body) = call(addr, &req, sb);
        assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
        let station: Value = serde_json::from_slice(&body_of(&body)).unwrap();
        let sid = station["id"].as_str().unwrap();

        let folder = dir.join("drop");
        std::fs::create_dir_all(folder.join("album")).unwrap();
        std::fs::write(folder.join("song.mp3"), b"ID3\x04\x00\x00\x00\x00\x00\x00SONG!!!!").unwrap();
        std::fs::write(folder.join("album").join("tune.mp3"), b"ID3\x04\x00\x00\x00\x00\x00\x00TUNE!!!!").unwrap();
        std::fs::write(folder.join("notes.txt"), b"hello world!!").unwrap();
        let outside = dir.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.mp3"), b"ID3\x04\x00\x00\x00\x00\x00\x00OUTSIDE!!").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, folder.join("via")).unwrap();

        let escaped = folder.join("..").join("outside");
        let payload = serde_json::json!({"station_id": sid, "path": escaped}).to_string();
        let req = format!("POST /api/media/import HTTP/1.0\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", payload.len());
        let (code, body) = call(addr, &req, payload.as_bytes());
        assert_eq!(code, 400, "{}", String::from_utf8_lossy(&body));
        assert!(store.search_media(sid, "").unwrap().is_empty());

        let payload = serde_json::json!({"station_id": sid, "path": folder}).to_string();
        let req = format!("POST /api/media/import HTTP/1.0\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", payload.len());
        let (code, body) = call(addr, &req, payload.as_bytes());
        assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));
        let report: Value = serde_json::from_slice(&body_of(&body)).unwrap();
        assert_eq!(report["imported"], 2);
        assert_eq!(report["skipped"], 1);
        #[cfg(unix)]
        assert_eq!(report["failed"], 1);
        let names: Vec<_> = store.search_media(sid, "").unwrap().into_iter().map(|m| m.filename).collect();
        assert!(names.contains(&"song.mp3".to_string()));
        assert!(names.contains(&"tune.mp3".to_string()));
        assert!(!names.iter().any(|n| n == "secret.mp3"));
        assert!(store.list_audit(20).unwrap().iter().any(|a| a.action == "import" && a.detail.contains("imported=2")));
    }

    fn multipart_body(station: &str, filename: &str, file: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"------r\r\nContent-Disposition: form-data; name=\"station_id\"\r\n\r\n");
        out.extend_from_slice(station.as_bytes());
        out.extend_from_slice(b"\r\n------r\r\nContent-Disposition: form-data; name=\"file\"; filename=\"");
        out.extend_from_slice(filename.as_bytes());
        out.extend_from_slice(b"\"\r\nContent-Type: application/octet-stream\r\n\r\n");
        out.extend_from_slice(file);
        out.extend_from_slice(b"\r\n------r--\r\n");
        out
    }

    fn body_of(buf: &[u8]) -> Vec<u8> {
        match buf.windows(4).position(|w| w == b"\r\n\r\n") {
            Some(i) => buf[i + 4..].to_vec(),
            None => buf.to_vec(),
        }
    }
}
