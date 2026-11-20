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
use crate::teamspeak::TeamSpeak;
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
    pub recordings_root: PathBuf,
    pub teamspeak: Arc<TeamSpeak>,
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
    let disposition = if out.disposition.is_empty() {
        String::new()
    } else {
        format!("Content-Disposition: {}\r\n", out.disposition)
    };
    let text = format!(
        "HTTP/1.0 {} {}\r\nContent-Type: {}\r\n{}Content-Length: {}\r\nConnection: close\r\nAccess-Control-Allow-Origin: *\r\nCache-Control: no-store\r\n\r\n",
        out.code, status_text(out.code), out.content_type, disposition, out.body.len()
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
        ("GET", ["api", "health"]) => { let u = require_user(state, head, ip)?; Ok((200, health(state, true, Some(&u)))) }
        ("GET", ["api", "public", "health"]) => Ok((200, json!({"ok": true}))),
        ("GET", ["api", "public", "stations", slug]) => public_station(state, slug, head, socket_ip),
        ("GET", ["api", "me"]) => { let u = require_user(state, head, ip)?; account_json(state, &u) }
        ("GET", ["api", "stations"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Stations)?; list_visible_stations(state, &u) }
        ("POST", ["api", "stations"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Stations)?; create_station(state, &u, body) }
        ("DELETE", ["api", "stations", id]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Stations)?; see_station(state, &u, id)?; delete_station(state, &u.username, id) }
        ("GET", ["api", "bots"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Bots)?; Ok((200, state.teamspeak.list_json(viewer(&u))?)) }
        ("POST", ["api", "bots"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Bots)?; create_output(state, &u, body) }
        ("POST", ["api", "bots", "master"]) => { let u = require_user(state, head, ip)?; owner_only(state, &u)?; set_master(state, &u.username, body) }
        ("PATCH", ["api", "bots", id]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Bots)?; see_bot(state, &u, id)?; update_output(state, &u, id, body) }
        ("DELETE", ["api", "bots", id]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Bots)?; see_bot(state, &u, id)?; delete_output(state, &u.username, id) }
        ("POST", ["api", "bots", id, action]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Bots)?; see_bot(state, &u, id)?; output_action(state, &u.username, id, action, body) }
        ("GET", ["api", "stations", id, "outputs"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Stations)?; see_station(state, &u, id)?; Ok((200, state.teamspeak.station_board(id, viewer(&u))?)) }
        ("POST", ["api", "stations", id, "outputs"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Stations)?; see_station(state, &u, id)?; station_outputs(state, &u, id, body) }
        ("GET", ["api", "mounts"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Mounts)?; list_mounts(state, &u, head, socket_ip) }
        ("POST", ["api", "mounts"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Mounts)?; create_mount(state, &u, body) }
        ("PATCH", ["api", "mounts", id]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Mounts)?; see_mount(state, &u, id)?; update_mount(state, &u, id, body) }
        ("DELETE", ["api", "mounts", id]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Mounts)?; see_mount(state, &u, id)?; delete_mount(state, &u.username, id) }
        ("POST", ["api", "mounts", id, "disconnect"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Mounts)?; see_mount(state, &u, id)?; disconnect(state, &u.username, id) }
        ("POST", ["api", "mounts", id, "metadata"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Mounts)?; see_mount(state, &u, id)?; set_meta(state, id, body) }
        ("POST", ["api", "mounts", id, "password"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Mounts)?; see_mount(state, &u, id)?; rotate_source_password(state, &u.username, id, body) }
        ("GET", ["api", "mounts", id, "queue"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Mounts)?; see_mount(state, &u, id)?; queue_get(state, id) }
        ("POST", ["api", "mounts", id, "queue", action]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Mounts)?; see_mount(state, &u, id)?; queue_post(state, &u.username, id, action, body) }
        ("POST", ["api", "mounts", id, "seek"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Mounts)?; see_mount(state, &u, id)?; seek_mount(state, &u.username, id, body) }
        ("POST", ["api", "mounts", id, "jump"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Mounts)?; see_mount(state, &u, id)?; jump_mount(state, &u.username, id, body) }
        ("POST", ["api", "mounts", id, "play-next"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Mounts)?; see_mount(state, &u, id)?; play_next_mount(state, &u.username, id, body) }
        ("POST", ["api", "mounts", id, "play"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Mounts)?; see_mount(state, &u, id)?; hold_mount(state, &u.username, id, false) }
        ("POST", ["api", "mounts", id, "pause"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Mounts)?; see_mount(state, &u, id)?; hold_mount(state, &u.username, id, true) }
        ("GET", ["api", "mounts", id, "listeners"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Listeners)?; see_mount(state, &u, id)?; listeners_get(state, id) }
        ("POST", ["api", "mounts", id, "listeners", "kick"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Listeners)?; see_mount(state, &u, id)?; kick_all(state, &u.username, id) }
        ("POST", ["api", "mounts", id, "listeners", lid, "kick"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Listeners)?; see_mount(state, &u, id)?; kick_one(state, &u.username, id, lid) }
        ("POST", ["api", "mounts", id, "record", "start"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Recordings)?; see_mount(state, &u, id)?; start_record(state, &u.username, id, body) }
        ("POST", ["api", "mounts", id, "record", "stop"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Recordings)?; see_mount(state, &u, id)?; stop_record(state, &u.username, id) }
        ("GET", ["api", "mounts", id, "recordings"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Recordings)?; see_mount(state, &u, id)?; list_recordings(state, id) }
        ("DELETE", ["api", "mounts", id, "recordings"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Recordings)?; see_mount(state, &u, id)?; delete_recordings(state, &u.username, id) }
        ("DELETE", ["api", "mounts", id, "recordings", rid]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Recordings)?; see_mount(state, &u, id)?; see_recording(state, &u, rid)?; delete_recording(state, &u.username, id, rid) }
        ("POST", ["api", "mounts", id, "recordings", rid, "jump"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Recordings)?; see_mount(state, &u, id)?; see_recording(state, &u, rid)?; replay_recording(state, &u.username, id, rid, true) }
        ("POST", ["api", "mounts", id, "recordings", rid, "queue"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Recordings)?; see_mount(state, &u, id)?; see_recording(state, &u, rid)?; replay_recording(state, &u.username, id, rid, false) }
        ("GET", ["api", "media"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Library)?; list_media(state, &u, head) }
        ("POST", ["api", "media"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Library)?; upload(state, &u, head, body) }
        ("POST", ["api", "media", "import"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Import)?; import_media(state, &u, body) }
        ("PATCH", ["api", "media", id]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Library)?; see_media(state, &u, id)?; tag_media(state, &u.username, id, body) }
        ("POST", ["api", "media", id, "replace"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Library)?; see_media(state, &u, id)?; replace_media(state, &u, id, head, body) }
        ("DELETE", ["api", "media", id]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Library)?; see_media(state, &u, id)?; delete_media(state, &u.username, id) }
        ("GET", ["api", "playlists"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Playlists)?; list_playlists(state, &u, head) }
        ("POST", ["api", "playlists"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Playlists)?; create_playlist(state, &u, body) }
        ("POST", ["api", "playlists", id, "items"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Playlists)?; see_playlist(state, &u, id)?; add_item(state, &u.username, id, body) }
        ("POST", ["api", "playlists", id, "reorder"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Playlists)?; see_playlist(state, &u, id)?; reorder_playlist(state, &u.username, id, body) }
        ("DELETE", ["api", "playlists", id, "items", item]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Playlists)?; see_playlist(state, &u, id)?; delete_playlist_item(state, &u.username, id, item) }
        ("DELETE", ["api", "playlists", id]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Playlists)?; see_playlist(state, &u, id)?; delete_playlist(state, &u.username, id) }
        ("GET", ["api", "schedules"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Schedules)?; list_schedules(state, &u, head) }
        ("POST", ["api", "schedules"]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Schedules)?; add_schedule(state, &u, body) }
        ("PATCH", ["api", "schedules", id]) => { let u = require_user(state, head, ip)?; permit(state, &u, Perm::Schedules)?; see_schedule(state, &u, id)?; update_schedule(state, &u.username, id, body) }
        ("GET", ["api", "live"]) => { let u = require_user(state, head, ip)?; live_for(state, &u, head, socket_ip) }
        ("GET", ["api", "audit"]) => { let u = require_user(state, head, ip)?; list_audit(state, &u, head) }
        ("GET", ["api", "users"]) => { let u = require_user(state, head, ip)?; owner_only(state, &u)?; list_users(state) }
        ("POST", ["api", "users"]) => { let u = require_user(state, head, ip)?; owner_only(state, &u)?; create_user(state, &u, body) }
        ("GET", ["api", "users", id]) => { let u = require_user(state, head, ip)?; owner_only(state, &u)?; user_live(state, id) }
        ("PATCH", ["api", "users", id]) => { let u = require_user(state, head, ip)?; owner_only(state, &u)?; update_user(state, &u, id, body) }
        ("DELETE", ["api", "users", id]) => { let u = require_user(state, head, ip)?; owner_only(state, &u)?; delete_account(state, &u.username, id) }
        ("GET", ["api", "tokens"]) => { let u = require_user(state, head, ip)?; owner_only(state, &u)?; list_tokens(state) }
        ("POST", ["api", "tokens"]) => { let u = require_user(state, head, ip)?; owner_only(state, &u)?; create_token(state, &u, body) }
        ("PATCH", ["api", "tokens", id]) => { let u = require_user(state, head, ip)?; owner_only(state, &u)?; update_token(state, &u, id, body) }
        ("DELETE", ["api", "tokens", id]) => { let u = require_user(state, head, ip)?; owner_only(state, &u)?; revoke_token(state, &u, id) }
        ("POST", ["api", "tokens", id, "rotate"]) => { let u = require_user(state, head, ip)?; owner_only(state, &u)?; rotate_token(state, &u, id) }
        ("POST", ["api", "tokens", "rotate"]) => rotate(state, head, ip),
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
    if user.disabled {
        let _ = state.store.audit(&user.username, "login_fail", "disabled");
        return Err(Error::Unauthorized);
    }
    let token = auth::new_token()?;
    state.store.create_token(&user.id, "login", &auth::hash_token(&token))?;
    let _ = state.store.audit(&user.username, "login", "");
    Ok((200, json!({"token": token, "username": user.username, "role": user.role, "owner": user.is_owner()})))
}

struct Actor {
    user: store::User,
    /// Session of the real owner. Skips permission switches.
    bypass: bool,
    /// May create users and tokens. An owner API token still cannot grant a switch it lacks.
    manage: bool,
    mask: Mask,
}

impl std::ops::Deref for Actor {
    type Target = store::User;
    fn deref(&self) -> &store::User { &self.user }
}

#[derive(Clone, Copy)]
struct Mask {
    stations: bool,
    mounts: bool,
    playlists: bool,
    library: bool,
    import: bool,
    recordings: bool,
    schedules: bool,
    bots: bool,
    listeners: bool,
    takeover: bool,
}

impl Mask {
    fn all() -> Self {
        Self { stations: true, mounts: true, playlists: true, library: true, import: true, recordings: true, schedules: true, bots: true, listeners: true, takeover: true }
    }
    fn none() -> Self {
        Self { stations: false, mounts: false, playlists: false, library: false, import: false, recordings: false, schedules: false, bots: false, listeners: false, takeover: false }
    }
    fn from_user(user: &store::User) -> Self {
        Self {
            stations: user.perm_stations, mounts: user.perm_mounts, playlists: user.perm_playlists,
            library: user.perm_library, import: user.perm_import, recordings: user.perm_recordings,
            schedules: user.perm_schedules, bots: user.perm_bots, listeners: user.perm_listeners, takeover: user.perm_takeover,
        }
    }
    fn from_token(row: &store::TokenRow) -> Self {
        Self {
            stations: row.perm_stations, mounts: row.perm_mounts, playlists: row.perm_playlists,
            library: row.perm_library, import: row.perm_import, recordings: row.perm_recordings,
            schedules: row.perm_schedules, bots: row.perm_bots, listeners: row.perm_listeners, takeover: row.perm_takeover,
        }
    }
    fn and(self, other: Self) -> Self {
        Self {
            stations: self.stations && other.stations, mounts: self.mounts && other.mounts,
            playlists: self.playlists && other.playlists, library: self.library && other.library,
            import: self.import && other.import, recordings: self.recordings && other.recordings,
            schedules: self.schedules && other.schedules, bots: self.bots && other.bots,
            listeners: self.listeners && other.listeners, takeover: self.takeover && other.takeover,
        }
    }
    fn get(self, perm: Perm) -> bool {
        match perm {
            Perm::Stations => self.stations, Perm::Mounts => self.mounts, Perm::Playlists => self.playlists,
            Perm::Library => self.library, Perm::Import => self.import, Perm::Recordings => self.recordings,
            Perm::Schedules => self.schedules, Perm::Bots => self.bots, Perm::Listeners => self.listeners,
            Perm::Takeover => self.takeover,
        }
    }
    fn covers(self, other: Self) -> bool {
        (!other.stations || self.stations) && (!other.mounts || self.mounts) && (!other.playlists || self.playlists)
            && (!other.library || self.library) && (!other.import || self.import) && (!other.recordings || self.recordings)
            && (!other.schedules || self.schedules) && (!other.bots || self.bots) && (!other.listeners || self.listeners)
            && (!other.takeover || self.takeover)
    }
    fn json(self) -> Value {
        json!({
            "stations": self.stations, "mounts": self.mounts, "playlists": self.playlists,
            "library": self.library, "import": self.import, "recordings": self.recordings,
            "schedules": self.schedules, "bots": self.bots, "listeners": self.listeners, "takeover": self.takeover,
        })
    }
}

fn apply_mask(user: &mut store::User, mask: Mask) {
    user.perm_stations = mask.stations;
    user.perm_mounts = mask.mounts;
    user.perm_playlists = mask.playlists;
    user.perm_library = mask.library;
    user.perm_import = mask.import;
    user.perm_recordings = mask.recordings;
    user.perm_schedules = mask.schedules;
    user.perm_bots = mask.bots;
    user.perm_listeners = mask.listeners;
    user.perm_takeover = mask.takeover;
}

fn require_user(state: &ApiState, head: &Head, ip: &str) -> Result<Actor, Error> {
    let header = head.get("authorization").unwrap_or("");
    let presented = header.strip_prefix("Bearer ").or_else(|| header.strip_prefix("bearer ")).unwrap_or("").trim();
    if presented.is_empty() {
        return Err(Error::Unauthorized);
    }
    if !state.fails.allow(ip) {
        return Err(Error::Limit);
    }
    let Some(hit) = state.store.authenticate_token(&auth::hash_token(presented))? else {
        state.fails.fail(ip);
        return Err(Error::Unauthorized);
    };
    if hit.account.disabled {
        state.fails.fail(ip);
        return Err(Error::Unauthorized);
    }
    let actor = if hit.token.kind != "api" {
        let mask = if hit.account.is_owner() { Mask::all() } else { Mask::from_user(&hit.account) };
        Actor { bypass: hit.account.is_owner(), manage: hit.account.is_owner(), mask, user: hit.account }
    } else if hit.token.scope == "owner" {
        if !hit.account.is_owner() {
            state.fails.fail(ip);
            return Err(Error::Unauthorized);
        }
        Actor { bypass: false, manage: true, mask: Mask::from_token(&hit.token), user: hit.account }
    } else {
        let Some(mut act) = hit.act_as else {
            state.fails.fail(ip);
            return Err(Error::Unauthorized);
        };
        if act.disabled || act.is_owner() {
            state.fails.fail(ip);
            return Err(Error::Unauthorized);
        }
        let mask = Mask::from_token(&hit.token).and(Mask::from_user(&act));
        apply_mask(&mut act, mask);
        Actor { bypass: false, manage: false, mask, user: act }
    };
    Ok(actor)
}

#[derive(Clone, Copy)]
enum Perm { Stations, Mounts, Playlists, Library, Import, Recordings, Schedules, Bots, Listeners, Takeover }

fn deny(state: &ApiState, actor: &Actor, reason: &str) -> Error {
    let _ = state.store.audit(&actor.user.username, "denied", reason);
    Error::Forbidden(reason.into())
}

fn owner_only(state: &ApiState, actor: &Actor) -> Result<(), Error> {
    if actor.manage { Ok(()) } else { Err(deny(state, actor, "only the owner can do that")) }
}

fn permit(state: &ApiState, actor: &Actor, perm: Perm) -> Result<(), Error> {
    if actor.bypass || actor.mask.get(perm) { return Ok(()); }
    let name = match perm {
        Perm::Stations => "stations",
        Perm::Mounts => "mounts",
        Perm::Playlists => "playlists",
        Perm::Library => "library upload",
        Perm::Import => "folder import",
        Perm::Recordings => "recordings",
        Perm::Schedules => "schedules",
        Perm::Bots => "teamspeak bots",
        Perm::Listeners => "listener list and kick",
        Perm::Takeover => "live DJ takeover",
    };
    Err(deny(state, actor, &format!("{name} permission is off")))
}

fn viewer(user: &store::User) -> Option<&str> {
    if user.is_owner() { None } else { Some(user.id.as_str()) }
}

fn owned_ids(state: &ApiState, user: &store::User) -> Result<Vec<String>, Error> {
    let rows = state.store.list_stations()?;
    Ok(rows.into_iter().filter(|s| user.is_owner() || s.user_id == user.id).map(|s| s.id).collect())
}

fn see_station(state: &ApiState, user: &store::User, id: &str) -> Result<store::Station, Error> {
    let station = state.store.station(id)?.ok_or(Error::NotFound)?;
    if user.is_owner() || station.user_id == user.id {
        Ok(station)
    } else {
        let _ = state.store.audit(&user.username, "denied", "not your station");
        Err(Error::Forbidden("not your station".into()))
    }
}

fn see_mount(state: &ApiState, user: &store::User, id: &str) -> Result<store::Mount, Error> {
    let mount = state.store.mount(id)?.ok_or(Error::NotFound)?;
    see_station(state, user, &mount.station_id)?;
    Ok(mount)
}

fn see_playlist(state: &ApiState, user: &store::User, id: &str) -> Result<store::Playlist, Error> {
    let playlist = state.store.playlist(id)?.ok_or(Error::NotFound)?;
    see_station(state, user, &playlist.station_id)?;
    Ok(playlist)
}

fn see_media(state: &ApiState, user: &store::User, id: &str) -> Result<store::Media, Error> {
    let row = state.store.media(id)?.ok_or(Error::NotFound)?;
    see_station(state, user, &row.station_id)?;
    Ok(row)
}

fn see_schedule(state: &ApiState, user: &store::User, id: &str) -> Result<store::Schedule, Error> {
    let row = state.store.schedule(id)?.ok_or(Error::NotFound)?;
    let mount = state.store.mount(&row.mount_id)?.ok_or(Error::NotFound)?;
    see_station(state, user, &mount.station_id)?;
    Ok(row)
}

fn see_bot(state: &ApiState, user: &store::User, id: &str) -> Result<store::Output, Error> {
    let row = state.store.output(id)?.ok_or(Error::NotFound)?;
    if user.is_owner() || row.user_id == user.id {
        Ok(row)
    } else {
        let _ = state.store.audit(&user.username, "denied", "not your bot");
        Err(Error::Forbidden("not your bot".into()))
    }
}

fn see_recording(state: &ApiState, user: &store::User, id: &str) -> Result<(), Error> {
    let row = state.store.recording(id)?.ok_or(Error::NotFound)?;
    see_mount(state, user, &row.mount_id)?;
    Ok(())
}

fn cap_room(state: &ApiState, owner_id: &str, kind: &str, add: i64) -> Result<(), Error> {
    if owner_id.is_empty() { return Ok(()); }
    let Some(user) = state.store.user_by_id(owner_id)? else { return Ok(()); };
    if user.is_owner() { return Ok(()); }
    let usage = state.store.usage(owner_id)?;
    let (max, used, msg) = match kind {
        "stations" => (user.max_stations, usage.stations, "station limit reached"),
        "mounts" => (user.max_mounts, usage.mounts, "mount limit reached"),
        "playlists" => (user.max_playlists, usage.playlists, "playlist limit reached"),
        "bots" => (user.max_bots, usage.bots, "teamspeak bot limit reached"),
        _ => return Ok(()),
    };
    if let Some(max) = max {
        if used >= max || used.saturating_add(add) > max {
            return Err(Error::Forbidden(format!("{msg}: {used} of {max}")));
        }
    }
    Ok(())
}

fn set_gates(state: &ApiState, user_id: &str, open: bool) -> Result<(), Error> {
    for station in state.store.list_stations()? {
        if station.user_id != user_id { continue; }
        for mount in state.store.list_mounts(Some(&station.id))? {
            state.engine.set_path_open(&mount.path, open);
        }
    }
    Ok(())
}

fn health(state: &ApiState, full: bool, user: Option<&store::User>) -> Value {
    let mut rows = state.engine.status();
    if let Some(user) = user {
        if !user.is_owner() {
            if let Ok(mine) = owned_ids(state, user) {
                rows.retain(|row| mine.iter().any(|id| id == &row.station_id));
            }
        }
    }
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

fn create_output(state: &ApiState, user: &store::User, body: &[u8]) -> Result<(u16, Value), Error> {
    let body = parse_json_value(body)?;
    if let Some(station_id) = body.get("station_id").and_then(|v| v.as_str()) {
        if !station_id.is_empty() {
            see_station(state, user, station_id)?;
        }
    }
    cap_room(state, &user.id, "bots", 1)?;
    let actor = user.username.as_str();
    let view = state.teamspeak.create(&body, &user.id)?;
    let nick = view.get("nickname").and_then(|v| v.as_str()).unwrap_or("");
    let host = view.get("host").and_then(|v| v.as_str()).unwrap_or("");
    state.store.audit(actor, "ts_output_create", &format!("{nick} {host}"))?;
    Ok((201, view))
}

fn set_master(state: &ApiState, actor: &str, body: &[u8]) -> Result<(u16, Value), Error> {
    let body = parse_json_value(body)?;
    let volume = body.get("volume").and_then(|v| v.as_u64()).ok_or_else(|| Error::Invalid("volume is required".into()))?;
    if volume > 100 {
        return Err(Error::Invalid("volume must be 0 to 100".into()));
    }
    let view = state.teamspeak.set_master(volume as u8)?;
    state.store.audit(actor, "ts_master", &volume.to_string())?;
    Ok((200, view))
}

fn station_outputs(state: &ApiState, user: &store::User, station_id: &str, body: &[u8]) -> Result<(u16, Value), Error> {
    let body = parse_json_value(body)?;
    let actor = user.username.as_str();
    if let Some(on) = body.get("radio").and_then(|v| v.as_bool()) {
        let view = state.teamspeak.set_radio(station_id, on, viewer(user))?;
        state.store.audit(actor, "ts_radio", if on { "on" } else { "off" })?;
        return Ok((200, view));
    }
    let bot_id = body.get("bot_id").and_then(|v| v.as_str()).ok_or_else(|| Error::Invalid("bot_id is required".into()))?;
    see_bot(state, user, bot_id)?;
    let feed = body.get("feed").and_then(|v| v.as_bool()).unwrap_or(true);
    let view = state.teamspeak.set_feed(station_id, bot_id, feed, viewer(user))?;
    state.store.audit(actor, "ts_feed", bot_id)?;
    Ok((200, view))
}

fn update_output(state: &ApiState, user: &store::User, id: &str, body: &[u8]) -> Result<(u16, Value), Error> {
    let body = parse_json_value(body)?;
    if let Some(station_id) = body.get("station_id").and_then(|v| v.as_str()) {
        if !station_id.is_empty() {
            see_station(state, user, station_id)?;
        }
    }
    let actor = user.username.as_str();
    let view = state.teamspeak.update(id, &body)?;
    let nick = view.get("nickname").and_then(|v| v.as_str()).unwrap_or("");
    state.store.audit(actor, "ts_output_update", nick)?;
    Ok((200, view))
}

fn delete_output(state: &ApiState, actor: &str, id: &str) -> Result<(u16, Value), Error> {
    let nick = state.store.output(id)?.map(|row| row.nickname).unwrap_or_default();
    state.teamspeak.delete(id)?;
    state.store.audit(actor, "ts_output_delete", &nick)?;
    Ok((200, json!({"ok": true})))
}

fn output_action(state: &ApiState, actor: &str, id: &str, action: &str, body: &[u8]) -> Result<(u16, Value), Error> {
    let body = if body.is_empty() { Value::Object(Default::default()) } else { parse_json_value(body)? };
    let view = state.teamspeak.action(id, action, &body)?;
    if matches!(action, "connect" | "disconnect") {
        state.store.audit(actor, "ts_output_connect", &format!("{action} {id}"))?;
    }
    Ok((200, view))
}

fn parse_json_value(body: &[u8]) -> Result<Value, Error> {
    if body.is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    serde_json::from_slice(body).map_err(|_| Error::Invalid("invalid json".into()))
}

fn list_visible_stations(state: &ApiState, user: &store::User) -> Result<(u16, Value), Error> {
    let rows = state.store.list_stations()?;
    let rows: Vec<_> = rows.into_iter().filter(|s| user.is_owner() || s.user_id == user.id).collect();
    Ok((200, json!(rows.iter().map(station_json).collect::<Vec<_>>())))
}

fn create_station(state: &ApiState, user: &store::User, body: &[u8]) -> Result<(u16, Value), Error> {
    #[derive(Deserialize)]
    struct Req { name: String, slug: String, #[serde(default)] description: String, #[serde(default)] genre: String }
    let req: Req = parse_json(body)?;
    if !valid_slug(&req.slug) || req.name.trim().is_empty() || req.name.len() > 80 {
        return Err(Error::Invalid("invalid station".into()));
    }
    cap_room(state, &user.id, "stations", 1)?;
    let actor = user.username.as_str();
    let s = state.store.create_station(&user.id, req.name.trim(), &req.slug, &req.description, &req.genre)?;
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

fn list_mounts(state: &ApiState, user: &store::User, head: &Head, socket_ip: &str) -> Result<(u16, Value), Error> {
    let station = query_param(&head.query, "station");
    if let Some(station_id) = station.as_deref() {
        see_station(state, user, station_id)?;
    }
    let mut mounts = state.store.list_mounts(station.as_deref())?;
    if !user.is_owner() {
        let mine = owned_ids(state, user)?;
        mounts.retain(|m| mine.iter().any(|id| id == &m.station_id));
    }
    let live = state.engine.status();
    let base = request_stream_base(state, head, socket_ip);
    let rows: Vec<_> = mounts.iter().map(|m| mount_json(m, live.iter().find(|s| s.id == m.id || s.path == m.path), &base)).collect();
    Ok((200, json!(rows)))
}

fn create_mount(state: &ApiState, user: &Actor, body: &[u8]) -> Result<(u16, Value), Error> {
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
    let station = see_station(state, user, &req.station_id)?;
    cap_room(state, &station.user_id, "mounts", 1)?;
    if req.allow_takeover {
        permit(state, user, Perm::Takeover)?;
    }
    let actor = user.username.as_str();
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
        record: false,
        record_max_ms: 3_600_000,
    };
    state.store.create_mount(&mount)?;
    sync_mount(state, &mount)?;
    state.store.audit(actor, "mount_create", &format!("{} encoder={}", mount.path, mount.encoder))?;
    if !mount.relay_url.is_empty() {
        state.store.audit(actor, "relay_set", &format!("{} {}", mount.path, mount.relay_url))?;
    }
    Ok((201, mount_json(&mount, None, &state.stream_public_url)))
}

fn update_mount(state: &ApiState, user: &Actor, id: &str, body: &[u8]) -> Result<(u16, Value), Error> {
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
    if req.allow_takeover == Some(true) {
        permit(state, user, Perm::Takeover)?;
    }
    let actor = user.username.as_str();
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
    let _ = finish_active(state, &mount, actor);
    remove_recording_files(state, &mount);
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

fn hold_mount(state: &ApiState, actor: &str, id: &str, paused: bool) -> Result<(u16, Value), Error> {
    let mount = state.store.mount(id)?.ok_or(Error::NotFound)?;
    state.engine.set_paused(&mount.path, paused)?;
    state.store.audit(actor, if paused { "pause" } else { "play" }, &mount.path)?;
    Ok((200, json!({"ok": true, "paused": paused})))
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

fn upload(state: &ApiState, user: &store::User, head: &Head, body: &[u8]) -> Result<(u16, Value), Error> {
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
    let station = see_station(state, user, &station_id)?;
    state.store.allow_track(&station.user_id, file.data.len() as i64, 1)?;
    let actor = user.username.as_str();
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

fn import_media(state: &ApiState, user: &store::User, body: &[u8]) -> Result<(u16, Value), Error> {
    #[derive(Deserialize)]
    struct Req { station_id: String, path: String }
    let req: Req = parse_json(body)?;
    let path = req.path.trim();
    if path.is_empty() {
        return Err(Error::Invalid("folder path required".into()));
    }
    see_station(state, user, &req.station_id)?;
    let report = media::import_folder(&state.store, &state.media_root, &req.station_id, Path::new(path), state.max_upload, &user.username)?;
    Ok((200, json!({"imported": report.imported, "skipped": report.skipped, "failed": report.failed})))
}

fn list_media(state: &ApiState, user: &store::User, head: &Head) -> Result<(u16, Value), Error> {
    let station = query_param(&head.query, "station").ok_or_else(|| Error::Invalid("station required".into()))?;
    see_station(state, user, &station)?;
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

fn replace_media(state: &ApiState, user: &store::User, id: &str, head: &Head, body: &[u8]) -> Result<(u16, Value), Error> {
    if !media::valid_id(id) { return Err(Error::NotFound); }
    let existing = state.store.media(id)?.ok_or(Error::NotFound)?;
    let actor = user.username.as_str();
    if body.len() as u64 > state.max_upload + 65_536 { return Err(Error::TooLarge); }
    let ctype = head.get("content-type").unwrap_or("");
    let parts = parse_multipart(body, ctype)?;
    let file = parts.iter().find(|p| p.name == "file").ok_or_else(|| Error::Invalid("file required".into()))?;
    if file.data.len() as u64 > state.max_upload { return Err(Error::TooLarge); }
    let kind = probe(&file.data)?;
    let tags = media::read_id3(&file.data);
    let delta = file.data.len() as i64 - existing.bytes;
    if delta > 0 {
        let owner = state.store.station(&existing.station_id)?.map(|s| s.user_id).unwrap_or_default();
        state.store.allow_track(&owner, delta, 0)?;
    }
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

fn list_playlists(state: &ApiState, user: &store::User, head: &Head) -> Result<(u16, Value), Error> {
    let station = query_param(&head.query, "station").ok_or_else(|| Error::Invalid("station required".into()))?;
    see_station(state, user, &station)?;
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

fn create_playlist(state: &ApiState, user: &store::User, body: &[u8]) -> Result<(u16, Value), Error> {
    #[derive(Deserialize)]
    struct Req { station_id: String, name: String }
    let req: Req = parse_json(body)?;
    if req.name.trim().is_empty() { return Err(Error::Invalid("name required".into())); }
    let station = see_station(state, user, &req.station_id)?;
    cap_room(state, &station.user_id, "playlists", 1)?;
    let actor = user.username.as_str();
    let pl = state.store.create_playlist(&req.station_id, req.name.trim())?;
    state.store.audit(actor, "playlist_create", &pl.id)?;
    Ok((201, json!({"id": pl.id, "station_id": pl.station_id, "name": pl.name, "items": []})))
}

fn add_item(state: &ApiState, actor: &str, playlist_id: &str, body: &[u8]) -> Result<(u16, Value), Error> {
    #[derive(Deserialize)]
    struct Req { media_id: String }
    let req: Req = parse_json(body)?;
    if !media::valid_id(&req.media_id) { return Err(Error::Invalid("invalid media id".into())); }
    let playlist = state.store.playlist(playlist_id)?.ok_or(Error::NotFound)?;
    let track = state.store.media(&req.media_id)?.ok_or(Error::NotFound)?;
    if track.station_id != playlist.station_id {
        return Err(Error::Forbidden("not your track".into()));
    }
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

fn list_schedules(state: &ApiState, user: &store::User, head: &Head) -> Result<(u16, Value), Error> {
    let mount = query_param(&head.query, "mount").ok_or_else(|| Error::Invalid("mount required".into()))?;
    see_mount(state, user, &mount)?;
    let rows = state.store.list_schedules(&mount)?;
    Ok((200, json!(rows.iter().map(schedule_json).collect::<Vec<_>>())))
}

fn add_schedule(state: &ApiState, user: &store::User, body: &[u8]) -> Result<(u16, Value), Error> {
    #[derive(Deserialize)]
    struct Req { mount_id: String, playlist_id: String, days: Vec<String>, start: String, end: String, #[serde(default = "one")] priority: i32 }
    fn one() -> i32 { 1 }
    let req: Req = parse_json(body)?;
    let mask = schedule::parse_days(&req.days).ok_or_else(|| Error::Invalid("days".into()))?;
    let start = schedule::parse_hhmm(&req.start).ok_or_else(|| Error::Invalid("start".into()))?;
    let end = schedule::parse_hhmm(&req.end).ok_or_else(|| Error::Invalid("end".into()))?;
    see_mount(state, user, &req.mount_id)?;
    let playlist = see_playlist(state, user, &req.playlist_id)?;
    let mount = state.store.mount(&req.mount_id)?.ok_or(Error::NotFound)?;
    if playlist.station_id != mount.station_id {
        return Err(Error::Invalid("playlist is on another station".into()));
    }
    let actor = user.username.as_str();
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

fn list_audit(state: &ApiState, user: &store::User, head: &Head) -> Result<(u16, Value), Error> {
    let limit = query_param(&head.query, "limit").and_then(|s| s.parse().ok()).unwrap_or(100);
    let mut rows = state.store.list_audit(limit)?;
    if !user.is_owner() {
        rows.retain(|row| row.actor == user.username);
    }
    Ok((200, json!(rows.iter().map(|a| json!({"id": a.id, "at": a.at, "actor": a.actor, "action": a.action, "detail": a.detail})).collect::<Vec<_>>())))
}

fn list_users(state: &ApiState) -> Result<(u16, Value), Error> {
    let users = state.store.list_users()?;
    let mut out = Vec::new();
    for user in users {
        let usage = state.store.usage(&user.id)?;
        out.push(user_json(&user, &usage));
    }
    Ok((200, json!(out)))
}

fn account_json(state: &ApiState, actor: &Actor) -> Result<(u16, Value), Error> {
    let usage = state.store.usage(&actor.user.id)?;
    let mut value = user_json(&actor.user, &usage);
    if !actor.bypass {
        if let Some(obj) = value.as_object_mut() {
            obj.insert("permissions".into(), actor.mask.json());
        }
    }
    if let Some(obj) = value.as_object_mut() {
        obj.insert("manage_tokens".into(), json!(actor.manage));
    }
    Ok((200, value))
}

fn user_json(user: &store::User, usage: &store::Usage) -> Value {
    json!({
        "id": user.id,
        "username": user.username,
        "role": user.role,
        "owner": user.is_owner(),
        "disabled": user.disabled,
        "permissions": {
            "stations": user.perm_stations,
            "mounts": user.perm_mounts,
            "playlists": user.perm_playlists,
            "library": user.perm_library,
            "import": user.perm_import,
            "recordings": user.perm_recordings,
            "schedules": user.perm_schedules,
            "bots": user.perm_bots,
            "listeners": user.perm_listeners,
            "takeover": user.perm_takeover,
        },
        "quotas": {
            "max_stations": user.max_stations,
            "max_mounts": user.max_mounts,
            "max_playlists": user.max_playlists,
            "max_tracks": user.max_tracks,
            "max_library_bytes": user.max_library_bytes,
            "max_bots": user.max_bots,
            "max_recordings": user.max_recordings,
            "max_recording_bytes": user.max_recording_bytes,
        },
        "usage": {
            "stations": usage.stations,
            "mounts": usage.mounts,
            "playlists": usage.playlists,
            "tracks": usage.tracks,
            "library_bytes": usage.library_bytes,
            "bots": usage.bots,
            "recordings": usage.recordings,
            "recording_bytes": usage.recording_bytes,
        }
    })
}

fn create_user(state: &ApiState, actor: &Actor, body: &[u8]) -> Result<(u16, Value), Error> {
    let value = parse_json_value(body)?;
    ensure_grant(state, actor, &value)?;
    let username = value.get("username").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let password = value.get("password").and_then(|v| v.as_str()).unwrap_or("");
    if !valid_user(&username) || password.len() < 8 {
        return Err(Error::Invalid("invalid user".into()));
    }
    let mut user = store::User::open_account();
    if !actor.bypass {
        apply_mask(&mut user, Mask::none());
    }
    user.username = username;
    apply_grants(&mut user, &value)?;
    let hash = auth::hash_password(password)?;
    let created = state.store.create_user(&user.username, &hash, "user")?;
    user.id = created.id;
    user.role = created.role;
    user.password_hash.clear();
    state.store.save_account(&user)?;
    state.store.audit(&actor.user.username, "user_create", &user.username)?;
    let usage = state.store.usage(&user.id)?;
    Ok((201, user_json(&user, &usage)))
}

fn update_user(state: &ApiState, actor: &Actor, id: &str, body: &[u8]) -> Result<(u16, Value), Error> {
    let value = parse_json_value(body)?;
    ensure_grant(state, actor, &value)?;
    let mut user = state.store.user_by_id(id)?.ok_or(Error::NotFound)?;
    if user.is_owner() {
        return Err(Error::Invalid("the owner account stays as it is".into()));
    }
    let was_disabled = user.disabled;
    apply_grants(&mut user, &value)?;
    state.store.save_account(&user)?;
    if let Some(password) = value.get("password").and_then(|v| v.as_str()) {
        if !password.is_empty() {
            if password.len() < 8 {
                return Err(Error::Invalid("password too short".into()));
            }
            state.store.set_password_hash(&user.id, &auth::hash_password(password)?)?;
            state.store.revoke_user_tokens(&user.id)?;
        }
    }
    if user.disabled && !was_disabled {
        state.store.revoke_user_tokens(&user.id)?;
        state.teamspeak.disconnect_user(&user.id)?;
        set_gates(state, &user.id, false)?;
    } else if !user.disabled && was_disabled {
        set_gates(state, &user.id, true)?;
    }
    state.store.audit("owner", "user_update", &user.username)?;
    let fresh = state.store.user_by_id(&user.id)?.ok_or(Error::NotFound)?;
    let usage = state.store.usage(&fresh.id)?;
    Ok((200, user_json(&fresh, &usage)))
}

fn delete_account(state: &ApiState, actor: &str, id: &str) -> Result<(u16, Value), Error> {
    let user = state.store.user_by_id(id)?.ok_or(Error::NotFound)?;
    if user.is_owner() {
        return Err(Error::Invalid("the owner cannot be deleted".into()));
    }
    let files = state.store.owned_files(&user.id)?;
    state.teamspeak.disconnect_user(&user.id)?;
    for bot_id in &files.bot_ids {
        let _ = state.teamspeak.delete(bot_id);
    }
    for path in &files.mount_paths {
        state.engine.remove_path(path);
    }
    state.store.delete_user(&user.id)?;
    for media_id in &files.media_ids {
        if let Ok(path) = media::media_path(&state.media_root, media_id) {
            let _ = std::fs::remove_file(path);
        }
    }
    for (mount_id, filename) in &files.recordings {
        if let Ok(dir) = crate::record::mount_dir(&state.recordings_root, mount_id) {
            let path = dir.join(filename);
            if path.starts_with(&dir) {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    state.store.audit(actor, "user_delete", &user.username)?;
    Ok((200, json!({"ok": true})))
}

fn user_live(state: &ApiState, id: &str) -> Result<(u16, Value), Error> {
    let user = state.store.user_by_id(id)?.ok_or(Error::NotFound)?;
    let usage = state.store.usage(&user.id)?;
    let stations: Vec<_> = state.store.list_stations()?.into_iter().filter(|s| s.user_id == user.id).collect();
    let live = state.engine.status();
    let mut mounts = Vec::new();
    for station in &stations {
        for mount in state.store.list_mounts(Some(&station.id))? {
            let row = live.iter().find(|row| row.path == mount.path);
            mounts.push(json!({
                "id": mount.id,
                "station_id": mount.station_id,
                "path": mount.path,
                "playing": row.map(|row| row.playing).unwrap_or(false),
                "live": row.map(|row| row.live).unwrap_or(false),
                "listeners": row.map(|row| row.listeners).unwrap_or(0),
            }));
        }
    }
    let playlists = state.store.playlists_for(&user.id)?;
    let bots = state.teamspeak.list_json(Some(&user.id))?;
    let recordings = state.store.recordings_for(&user.id)?;
    Ok((200, json!({
        "user": user_json(&user, &usage),
        "stations": stations.iter().map(station_json).collect::<Vec<_>>(),
        "mounts": mounts,
        "playlists": playlists.iter().map(|p| json!({"id": p.id, "station_id": p.station_id, "name": p.name})).collect::<Vec<_>>(),
        "bots": bots,
        "library": {"tracks": usage.tracks, "bytes": usage.library_bytes},
        "recordings": recordings.iter().map(|r| json!({
            "id": r.id, "mount_id": r.mount_id, "bytes": r.bytes, "filename": r.filename,
            "started_at": r.started_at, "ended_at": r.ended_at,
        })).collect::<Vec<_>>(),
    })))
}

fn apply_grants(user: &mut store::User, body: &Value) -> Result<(), Error> {
    if let Some(perms) = body.get("permissions").filter(|v| v.is_object()) {
        take_bool(perms, "stations", &mut user.perm_stations);
        take_bool(perms, "mounts", &mut user.perm_mounts);
        take_bool(perms, "playlists", &mut user.perm_playlists);
        take_bool(perms, "library", &mut user.perm_library);
        take_bool(perms, "import", &mut user.perm_import);
        take_bool(perms, "recordings", &mut user.perm_recordings);
        take_bool(perms, "schedules", &mut user.perm_schedules);
        take_bool(perms, "bots", &mut user.perm_bots);
        take_bool(perms, "listeners", &mut user.perm_listeners);
        take_bool(perms, "takeover", &mut user.perm_takeover);
    }
    if let Some(quotas) = body.get("quotas").filter(|v| v.is_object()) {
        user.max_stations = take_cap(quotas, "max_stations", user.max_stations)?;
        user.max_mounts = take_cap(quotas, "max_mounts", user.max_mounts)?;
        user.max_playlists = take_cap(quotas, "max_playlists", user.max_playlists)?;
        user.max_tracks = take_cap(quotas, "max_tracks", user.max_tracks)?;
        user.max_library_bytes = take_cap(quotas, "max_library_bytes", user.max_library_bytes)?;
        user.max_bots = take_cap(quotas, "max_bots", user.max_bots)?;
        user.max_recordings = take_cap(quotas, "max_recordings", user.max_recordings)?;
        user.max_recording_bytes = take_cap(quotas, "max_recording_bytes", user.max_recording_bytes)?;
    }
    if let Some(disabled) = body.get("disabled").and_then(|v| v.as_bool()) {
        user.disabled = disabled;
    }
    Ok(())
}

fn take_bool(obj: &Value, key: &str, slot: &mut bool) {
    if let Some(value) = obj.get(key).and_then(|v| v.as_bool()) {
        *slot = value;
    }
}

fn take_cap(obj: &Value, key: &str, current: Option<i64>) -> Result<Option<i64>, Error> {
    match obj.get(key) {
        None => Ok(current),
        Some(Value::Null) => Ok(None),
        Some(value) => {
            let parsed = if let Some(text) = value.as_str() {
                let text = text.trim();
                if text.is_empty() { return Ok(None); }
                text.parse::<i64>().map_err(|_| Error::Invalid(format!("{key} is not a number")))?
            } else {
                value.as_i64().ok_or_else(|| Error::Invalid(format!("{key} is not a number")))?
            };
            if parsed < 0 {
                return Err(Error::Invalid(format!("{key} cannot be negative")));
            }
            Ok(Some(parsed))
        }
    }
}

fn live_for(state: &ApiState, actor: &Actor, head: &Head, socket_ip: &str) -> Result<(u16, Value), Error> {
    let mut rows = state.engine.status();
    if !actor.user.is_owner() {
        let mine = owned_ids(state, actor)?;
        rows.retain(|row| mine.iter().any(|id| id == &row.station_id));
    }
    if !actor.bypass && !actor.mask.listeners {
        for row in &mut rows {
            row.listener_rows.clear();
        }
    }
    Ok((200, live_json(&rows, &request_stream_base(state, head, socket_ip))))
}

fn token_name(value: &Value) -> Result<String, Error> {
    let name = value.get("name").and_then(|v| v.as_str()).unwrap_or("").trim();
    if name.is_empty() || name.chars().count() > 40 || name.chars().any(|c| c.is_control()) {
        return Err(Error::Invalid("name the token".into()));
    }
    Ok(name.to_string())
}

fn token_prefix(secret: &str) -> String {
    secret.chars().take(12).collect()
}

fn mask_from_value(value: &Value) -> Mask {
    let Some(perms) = value.get("permissions").filter(|v| v.is_object()) else {
        return Mask::none();
    };
    let flag = |key: &str| perms.get(key).and_then(|v| v.as_bool()).unwrap_or(false);
    Mask {
        stations: flag("stations"), mounts: flag("mounts"), playlists: flag("playlists"),
        library: flag("library"), import: flag("import"), recordings: flag("recordings"),
        schedules: flag("schedules"), bots: flag("bots"), listeners: flag("listeners"), takeover: flag("takeover"),
    }
}

fn ensure_grant(state: &ApiState, actor: &Actor, body: &Value) -> Result<(), Error> {
    if actor.bypass || body.get("permissions").filter(|v| v.is_object()).is_none() {
        return Ok(());
    }
    let want = mask_from_value(body);
    if actor.mask.covers(want) {
        return Ok(());
    }
    Err(deny(state, actor, "cannot grant a permission this token does not have"))
}

fn take_expiry(value: &Value, present: bool) -> Result<Option<Option<i64>>, Error> {
    if !present {
        return Ok(None);
    }
    match value.get("expires_at") {
        None | Some(Value::Null) => Ok(Some(None)),
        Some(v) => {
            let n = if let Some(text) = v.as_str() {
                let text = text.trim();
                if text.is_empty() { return Ok(Some(None)); }
                text.parse::<i64>().map_err(|_| Error::Invalid("expiry is not a time".into()))?
            } else {
                v.as_i64().ok_or_else(|| Error::Invalid("expiry is not a time".into()))?
            };
            if n <= 0 {
                return Err(Error::Invalid("expiry is not a time".into()));
            }
            Ok(Some(Some(n)))
        }
    }
}

fn apply_token_mask(row: &mut store::TokenRow, mask: Mask) {
    row.perm_stations = mask.stations;
    row.perm_mounts = mask.mounts;
    row.perm_playlists = mask.playlists;
    row.perm_library = mask.library;
    row.perm_import = mask.import;
    row.perm_recordings = mask.recordings;
    row.perm_schedules = mask.schedules;
    row.perm_bots = mask.bots;
    row.perm_listeners = mask.listeners;
    row.perm_takeover = mask.takeover;
}

fn token_public(state: &ApiState, row: &store::TokenRow) -> Value {
    let linked = if row.scope == "user" { &row.act_as } else { &row.user_id };
    let username = state.store.user_by_id(linked).ok().flatten().map(|u| u.username).unwrap_or_default();
    json!({
        "id": row.id,
        "name": row.name,
        "prefix": row.prefix,
        "scope": row.scope,
        "user_id": linked,
        "username": username,
        "permissions": Mask::from_token(row).json(),
        "expires_at": row.expires_at,
        "last_used_at": row.last_used_at,
        "created_at": row.created_at,
    })
}

fn list_tokens(state: &ApiState) -> Result<(u16, Value), Error> {
    let rows = state.store.list_api_tokens()?;
    Ok((200, Value::Array(rows.iter().map(|row| token_public(state, row)).collect())))
}

fn create_token(state: &ApiState, actor: &Actor, body: &[u8]) -> Result<(u16, Value), Error> {
    let value = parse_json_value(body)?;
    let name = token_name(&value)?;
    let scope = value.get("scope").and_then(|v| v.as_str()).unwrap_or("owner");
    if scope != "owner" && scope != "user" {
        return Err(Error::Invalid("scope is owner or user".into()));
    }
    ensure_grant(state, actor, &value)?;
    let mask = if value.get("permissions").is_some() { mask_from_value(&value) } else { Mask::none() };
    let act_as = if scope == "user" {
        let id = value.get("user_id").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        let chosen = state.store.user_by_id(&id)?.ok_or_else(|| Error::Invalid("choose a user".into()))?;
        if chosen.is_owner() {
            return Err(Error::Invalid("an owner token is how you act as the owner".into()));
        }
        if chosen.disabled {
            return Err(Error::Invalid("that user is disabled".into()));
        }
        chosen.id
    } else {
        String::new()
    };
    let expires_at = take_expiry(&value, true)?.unwrap_or(None);
    let secret = auth::new_token()?;
    let mut row = store::TokenRow {
        id: auth::new_id(),
        user_id: actor.user.id.clone(),
        name,
        prefix: token_prefix(&secret),
        created_at: store::now_ms(),
        expires_at,
        last_used_at: None,
        kind: "api".into(),
        scope: scope.into(),
        act_as,
        perm_stations: false, perm_mounts: false, perm_playlists: false, perm_library: false, perm_import: false,
        perm_recordings: false, perm_schedules: false, perm_bots: false, perm_listeners: false, perm_takeover: false,
    };
    apply_token_mask(&mut row, mask);
    state.store.insert_api_token(&row, &auth::hash_token(&secret))?;
    state.store.audit(&actor.user.username, "token_create", &format!("{} {}", row.name, row.prefix))?;
    let mut view = token_public(state, &row);
    if let Some(obj) = view.as_object_mut() {
        obj.insert("token".into(), json!(secret));
    }
    Ok((201, view))
}

fn update_token(state: &ApiState, actor: &Actor, id: &str, body: &[u8]) -> Result<(u16, Value), Error> {
    let value = parse_json_value(body)?;
    let mut row = state.store.api_token(id)?.ok_or(Error::NotFound)?;
    if value.get("name").is_some() {
        row.name = token_name(&value)?;
    }
    if value.get("expires_at").is_some() {
        row.expires_at = take_expiry(&value, true)?.unwrap_or(None);
    }
    if value.get("permissions").is_some() {
        ensure_grant(state, actor, &value)?;
        apply_token_mask(&mut row, mask_from_value(&value));
    }
    state.store.save_api_token(&row)?;
    Ok((200, token_public(state, &row)))
}

fn revoke_token(state: &ApiState, actor: &Actor, id: &str) -> Result<(u16, Value), Error> {
    let row = state.store.api_token(id)?.ok_or(Error::NotFound)?;
    state.store.revoke_api_token(id)?;
    state.store.audit(&actor.user.username, "token_revoke", &format!("{} {}", row.name, row.prefix))?;
    Ok((200, json!({"ok": true})))
}

fn rotate_token(state: &ApiState, actor: &Actor, id: &str) -> Result<(u16, Value), Error> {
    let mut row = state.store.api_token(id)?.ok_or(Error::NotFound)?;
    let secret = auth::new_token()?;
    let prefix = token_prefix(&secret);
    state.store.rotate_api_token(id, &auth::hash_token(&secret), &prefix)?;
    row.prefix = prefix;
    state.store.audit(&actor.user.username, "token_rotate", &format!("{} {}", row.name, row.prefix))?;
    let mut view = token_public(state, &row);
    if let Some(obj) = view.as_object_mut() {
        obj.insert("token".into(), json!(secret));
    }
    Ok((200, view))
}

fn rotate(state: &ApiState, head: &Head, ip: &str) -> Result<(u16, Value), Error> {
    let actor = require_user(state, head, ip)?;
    state.store.revoke_session_tokens(&actor.user.id)?;
    let token = auth::new_token()?;
    state.store.create_token(&actor.user.id, "rotate", &auth::hash_token(&token))?;
    state.store.audit(&actor.user.username, "token_rotate", "")?;
    Ok((200, json!({"token": token})))
}

fn start_record(state: &ApiState, actor: &str, id: &str, body: &[u8]) -> Result<(u16, Value), Error> {
    #[derive(Deserialize, Default)]
    struct Req { #[serde(default)] max_duration_ms: u64 }
    let req: Req = if body.is_empty() { Req::default() } else { parse_json(body)? };
    let mount = state.store.mount(id)?.ok_or(Error::NotFound)?;
    let max_ms = if req.max_duration_ms == 0 { mount.record_max_ms.max(0) as u64 } else { req.max_duration_ms };
    begin_recording(state, &mount, max_ms, actor)
}

fn stop_record(state: &ApiState, actor: &str, id: &str) -> Result<(u16, Value), Error> {
    let mount = state.store.mount(id)?.ok_or(Error::NotFound)?;
    let fin = finish_active(state, &mount, actor)?;
    Ok((200, json!({
        "ok": true,
        "recording": false,
        "id": fin.as_ref().map(|f| f.id.clone()).unwrap_or_default(),
        "bytes": fin.as_ref().map(|f| f.bytes).unwrap_or(0),
        "duration_ms": fin.as_ref().map(|f| f.duration_ms).unwrap_or(0),
        "error": fin.as_ref().map(|f| f.error.clone()).unwrap_or_default(),
    })))
}

fn list_recordings(state: &ApiState, id: &str) -> Result<(u16, Value), Error> {
    let mount = state.store.mount(id)?.ok_or(Error::NotFound)?;
    let live = state.engine.recording_live(&mount.path);
    let rows = state.store.list_recordings(&mount.id)?;
    let out: Vec<_> = rows.iter().map(|r| recording_json(r, &mount.path, live.as_ref())).collect();
    Ok((200, Value::Array(out)))
}

fn delete_recording(state: &ApiState, actor: &str, mount_id: &str, rid: &str) -> Result<(u16, Value), Error> {
    if !media::valid_id(mount_id) || !media::valid_id(rid) {
        return Err(Error::NotFound);
    }
    let mount = state.store.mount(mount_id)?.ok_or(Error::NotFound)?;
    let rec = state.store.recording(rid)?.ok_or(Error::NotFound)?;
    if rec.mount_id != mount.id {
        return Err(Error::NotFound);
    }
    if state.engine.recording_live(&mount.path).is_some_and(|l| l.active && l.id == rec.id) {
        let _ = finish_active(state, &mount, actor)?;
    }
    unlink_recording(state, &mount.id, &rec);
    state.store.delete_recording_row(&rec.id)?;
    state.store.audit(actor, "record_delete", &format!("{} {}", mount.path, rec.id))?;
    Ok((200, json!({"ok": true})))
}

fn delete_recordings(state: &ApiState, actor: &str, id: &str) -> Result<(u16, Value), Error> {
    let mount = state.store.mount(id)?.ok_or(Error::NotFound)?;
    let _ = finish_active(state, &mount, actor)?;
    let rows = state.store.list_recordings(&mount.id)?;
    for rec in &rows {
        unlink_recording(state, &mount.id, rec);
    }
    state.store.delete_mount_recordings(&mount.id)?;
    state.store.audit(actor, "record_delete", &format!("{} all", mount.path))?;
    Ok((200, json!({"ok": true, "deleted": rows.len()})))
}

fn replay_recording(state: &ApiState, actor: &str, mount_id: &str, rid: &str, jump: bool) -> Result<(u16, Value), Error> {
    if !media::valid_id(mount_id) || !media::valid_id(rid) {
        return Err(Error::NotFound);
    }
    let mount = state.store.mount(mount_id)?.ok_or(Error::NotFound)?;
    let rec = state.store.recording(rid)?.ok_or(Error::NotFound)?;
    if rec.mount_id != mount.id {
        return Err(Error::NotFound);
    }
    let path = crate::record::resolve_download(&state.recordings_root, &mount.id, &rec.id, &rec.filename)?;
    let live = state.engine.recording_live(&mount.path).filter(|l| l.id == rec.id);
    let duration_ms = if rec.duration_ms > 0 {
        rec.duration_ms as u64
    } else {
        live.map(|l| l.duration_ms).unwrap_or(0)
    };
    let track = engine::Track {
        title: "Recording".into(),
        artist: String::new(),
        path,
        mime: crate::record::stream_mime(&rec.format).into(),
        media_id: rec.id.clone(),
        duration_ms,
    };
    state.engine.replay(&mount.path, track, jump)?;
    let action = if jump { "record_jump" } else { "record_queue" };
    state.store.audit(actor, action, &format!("{} {}", mount.path, rec.id))?;
    Ok((200, json!({"ok": true})))
}

fn begin_recording(state: &ApiState, mount: &Mount, max_ms: u64, actor: &str) -> Result<(u16, Value), Error> {
    if state.engine.recording_live(&mount.path).is_some_and(|l| l.active) {
        return Err(Error::Busy);
    }
    let owner = state.store.station(&mount.station_id)?.map(|s| s.user_id).unwrap_or_default();
    state.store.allow_recording(&owner)?;
    let max_ms = crate::record::clamp_max_ms(max_ms);
    let format = crate::record::stream_format(&mount.encoder);
    let id = auth::new_id();
    let filename = crate::record::file_name(&id, format)?;
    let dir = crate::record::mount_dir(&state.recordings_root, &mount.id)?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(&filename);
    if path.parent() != Some(dir.as_path()) {
        return Err(Error::NotFound);
    }
    let used = state.store.recording_bytes()? as u64;
    let budget = match state.engine.reserve_recording(&id, used) {
        Ok(budget) => budget,
        Err(err) => {
            state.engine.note_recording_error(&mount.path, "recording storage is full");
            return Err(err);
        }
    };
    let job = crate::record::Job {
        id: id.clone(),
        path: path.clone(),
        format: format.into(),
        max_duration: Duration::from_millis(max_ms),
        budget,
        fail_writes: false,
    };
    if let Err(err) = state.engine.start_recording(&mount.path, job) {
        state.engine.release_recording(&id);
        let _ = std::fs::remove_file(&path);
        if err.to_string().contains("disk write") {
            state.engine.note_recording_error(&mount.path, "disk write failed");
        }
        return Err(err);
    }
    let now = store::now_ms();
    if let Err(err) = state.store.insert_recording(&store::Recording {
        id: id.clone(),
        mount_id: mount.id.clone(),
        started_at: now,
        ended_at: None,
        duration_ms: 0,
        format: format.into(),
        bytes: 0,
        filename,
        error: String::new(),
    }) {
        let _ = state.engine.stop_recording(&mount.path);
        state.engine.release_recording(&id);
        return Err(err);
    }
    let mut saved = mount.clone();
    saved.record = true;
    saved.record_max_ms = max_ms as i64;
    state.store.update_mount(&saved)?;
    state.store.audit(actor, "record_start", &format!("{} {id}", mount.path))?;
    Ok((200, json!({
        "id": id,
        "mount_id": mount.id,
        "format": format,
        "started_at": now,
        "max_duration_ms": max_ms,
        "active": true,
    })))
}

fn finish_active(state: &ApiState, mount: &Mount, actor: &str) -> Result<Option<crate::record::Finished>, Error> {
    let fin = match state.engine.stop_recording(&mount.path) {
        Ok(fin) => Some(fin),
        Err(Error::NotFound) => None,
        Err(err) => return Err(err),
    };
    if let Some(fin) = &fin {
        state.store.finish_recording(&fin.id, store::now_ms(), fin.duration_ms as i64, fin.bytes as i64, &fin.error)?;
        state.engine.release_recording(&fin.id);
        state.store.audit(actor, "record_stop", &format!("{} {}", mount.path, fin.id))?;
    }
    if mount.record {
        let mut saved = mount.clone();
        saved.record = false;
        state.store.update_mount(&saved)?;
    }
    Ok(fin)
}

fn on_record_finished(state: &ApiState, path: &str, fin: crate::record::Finished) -> Result<(), Error> {
    let mounts = state.store.list_mounts(None)?;
    let Some(mount) = mounts.into_iter().find(|m| m.path == path) else {
        state.store.finish_recording(&fin.id, store::now_ms(), fin.duration_ms as i64, fin.bytes as i64, &fin.error)?;
        state.engine.release_recording(&fin.id);
        return Ok(());
    };
    state.store.finish_recording(&fin.id, store::now_ms(), fin.duration_ms as i64, fin.bytes as i64, &fin.error)?;
    state.engine.release_recording(&fin.id);
    state.store.audit("navad", "record_stop", &format!("{} {} {}", mount.path, fin.id, fin.error))?;
    if !fin.error.is_empty() {
        let mut saved = mount.clone();
        saved.record = false;
        state.store.update_mount(&saved)?;
        state.engine.note_recording_error(&mount.path, &fin.error);
        return Ok(());
    }
    if fin.rotate && mount.record {
        if let Err(err) = begin_recording(state, &mount, mount.record_max_ms.max(0) as u64, "navad") {
            let mut saved = mount.clone();
            saved.record = false;
            let _ = state.store.update_mount(&saved);
            state.engine.note_recording_error(&mount.path, &err.to_string());
            eprintln!("navad: recording {}: {err}", mount.path);
        }
    }
    Ok(())
}

pub fn watch_recordings(state: &Arc<ApiState>) {
    let hooked = Arc::clone(state);
    state.engine.set_record_hook(Arc::new(move |path, fin| {
        if let Err(err) = on_record_finished(&hooked, path, fin) {
            eprintln!("navad: recording: {err}");
        }
    }));
}

fn close_stale_recordings(state: &ApiState, mount: &Mount) -> Result<(), Error> {
    let live_id = state.engine.recording_live(&mount.path).filter(|l| l.active).map(|l| l.id);
    for rec in state.store.open_recordings(&mount.id)? {
        if live_id.as_deref() == Some(rec.id.as_str()) {
            continue;
        }
        let bytes = crate::record::resolve_download(&state.recordings_root, &mount.id, &rec.id, &rec.filename)
            .ok()
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| m.len() as i64)
            .unwrap_or(rec.bytes);
        state.store.finish_recording(&rec.id, store::now_ms(), rec.duration_ms, bytes, "interrupted")?;
        state.engine.release_recording(&rec.id);
    }
    Ok(())
}

fn unlink_recording(state: &ApiState, mount_id: &str, rec: &store::Recording) {
    if let Ok(path) = crate::record::resolve_download(&state.recordings_root, mount_id, &rec.id, &rec.filename) {
        let _ = std::fs::remove_file(path);
    }
}

fn remove_recording_files(state: &ApiState, mount: &Mount) {
    if let Ok(rows) = state.store.list_recordings(&mount.id) {
        for rec in &rows {
            unlink_recording(state, &mount.id, rec);
        }
    }
    if let Ok(dir) = crate::record::mount_dir(&state.recordings_root, &mount.id) {
        let _ = std::fs::remove_dir_all(dir);
    }
}

fn recording_json(r: &store::Recording, mount_path: &str, live: Option<&crate::record::Live>) -> Value {
    let active = live.is_some_and(|l| l.active && l.id == r.id);
    let bytes = if active { live.map(|l| l.bytes as i64).unwrap_or(r.bytes) } else { r.bytes };
    let duration_ms = if active { live.map(|l| l.duration_ms as i64).unwrap_or(r.duration_ms) } else { r.duration_ms };
    let error = if active { live.map(|l| l.error.clone()).unwrap_or_default() } else { r.error.clone() };
    json!({
        "id": r.id,
        "mount_id": r.mount_id,
        "mount": mount_path,
        "started_at": r.started_at,
        "ended_at": if active { Value::Null } else { r.ended_at.map(Value::from).unwrap_or(Value::Null) },
        "duration_ms": duration_ms,
        "format": r.format,
        "bytes": bytes,
        "size": bytes,
        "error": error,
        "active": active,
    })
}

fn download_recording(state: &ApiState, head: &Head, ip: &str, mount_id: &str, rid: &str) -> Result<HttpOut, Error> {
    let actor = require_user(state, head, ip)?;
    permit(state, &actor, Perm::Recordings)?;
    if !state.allow_admin || !media::valid_id(mount_id) || !media::valid_id(rid) {
        return Err(Error::NotFound);
    }
    let mount = see_mount(state, &actor, mount_id)?;
    let rec = state.store.recording(rid)?.ok_or(Error::NotFound)?;
    if rec.mount_id != mount.id {
        return Err(Error::NotFound);
    }
    let path = crate::record::resolve_download(&state.recordings_root, &mount.id, &rec.id, &rec.filename)?;
    let body = std::fs::read(&path)?;
    let name = crate::record::file_name(&rec.id, &rec.format).unwrap_or_else(|_| format!("{}.bin", rec.id));
    Ok(HttpOut {
        code: 200,
        content_type: crate::record::stream_mime(&rec.format).into(),
        body,
        disposition: format!("attachment; filename=\"{}\"", header_safe(&name)),
    })
}

fn recording_download_ids(path: &str) -> Option<(&str, &str)> {
    let segs: Vec<&str> = path.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    match segs.as_slice() {
        ["api", "mounts", id, "recordings", rid] => Some((*id, *rid)),
        _ => None,
    }
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
    let _ = close_stale_recordings(state, mount);
    if mount.record && !state.engine.recording_live(&mount.path).is_some_and(|l| l.active) {
        if let Err(err) = begin_recording(state, mount, crate::record::clamp_max_ms(mount.record_max_ms.max(0) as u64), "navad") {
            eprintln!("navad: recording {}: {err}", mount.path);
            if err.to_string().contains("full") || err.to_string().contains("disk write") {
                let mut saved = mount.clone();
                saved.record = false;
                let _ = state.store.update_mount(&saved);
            }
        }
    }
    Ok(())
}

pub fn sync_all(state: &ApiState) -> Result<(), Error> {
    for mount in state.store.list_mounts(None)? {
        sync_mount(state, &mount)?;
    }
    for station in state.store.list_stations()? {
        if !station.radio_enabled {
            for mount in state.store.list_mounts(Some(&station.id))? {
                state.engine.set_path_public(&mount.path, false);
            }
        }
    }
    for user in state.store.list_users()? {
        if user.disabled {
            set_gates(state, &user.id, false)?;
        }
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
        "recording": s.recording,
        "paused": s.paused,
        "recording_error": s.recording_error,
        "recording_id": s.recording_id,
        "recording_bytes": s.recording_bytes,
        "recording_ms": s.recording_ms,
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
    json!({"id": s.id, "name": s.name, "slug": s.slug, "description": s.description, "genre": s.genre, "radio": s.radio_enabled})
}

fn mount_json(m: &Mount, live: Option<&crate::engine::MountStatus>, base: &str) -> Value {
    let urls = public_mount_urls(base, &m.path);
    let mut value = json!({
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
    });
    let Some(obj) = value.as_object_mut() else { return value };
    obj.insert("record".into(), json!(m.record));
    obj.insert("record_max_ms".into(), json!(if m.record_max_ms <= 0 { 3_600_000 } else { m.record_max_ms }));
    obj.insert("recording".into(), json!(live.map(|s| s.recording).unwrap_or(false)));
    obj.insert("recording_error".into(), json!(live.map(|s| s.recording_error.clone()).unwrap_or_default()));
    obj.insert("recording_id".into(), json!(live.map(|s| s.recording_id.clone()).unwrap_or_default()));
    obj.insert("recording_bytes".into(), json!(live.map(|s| s.recording_bytes).unwrap_or(0)));
    obj.insert("recording_ms".into(), json!(live.map(|s| s.recording_ms).unwrap_or(0)));
    value
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
    let user = store.create_user(&username, &hash, "owner")?;
    store.claim_unowned(&user.id)?;
    let token = auth::new_token()?;
    store.create_token(&user.id, "bootstrap", &auth::hash_token(&token))?;
    store.audit(&username, "bootstrap", "")?;
    Ok(Some((username, password, token)))
}

pub fn state_from_config(cfg: &Config, store: Arc<Store>, engine: Arc<Engine>, allow_admin: bool) -> ApiState {
    let teamspeak = TeamSpeak::open(Arc::clone(&store), Arc::clone(&engine), PathBuf::from(&cfg.data_dir).join("media"));
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
        recordings_root: PathBuf::from(&cfg.data_dir).join("recordings"),
        teamspeak,
    }
}

/// Dashboard files are served as bytes. `/api` is JSON, except an authenticated recording download.
pub fn serve_static_or_api(state: &ApiState, head: &Head, body: &[u8], ip: &str, socket_ip: &str) -> Result<HttpOut, Error> {
    let path = head.path.as_str();
    if head.method == "GET" {
        if let Some((mount_id, rid)) = recording_download_ids(path) {
            return download_recording(state, head, ip, mount_id, rid);
        }
    }
    if !path.starts_with("/api") {
        if head.method != "GET" && head.method != "HEAD" {
            return Err(Error::NotFound);
        }
        let (bytes, mime) = static_file(path).ok_or(Error::NotFound)?;
        return Ok(HttpOut { code: 200, content_type: mime.to_string(), body: if head.method == "HEAD" { Vec::new() } else { bytes.to_vec() }, disposition: String::new() });
    }
    let (code, value) = dispatch(state, head, body, ip, socket_ip)?;
    let bytes = if code == 204 { Vec::new() } else { serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec()) };
    Ok(HttpOut { code, content_type: "application/json".into(), body: bytes, disposition: String::new() })
}

pub struct HttpOut {
    pub code: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    pub disposition: String,
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
        let recordings = dir.join("recordings");
        std::fs::create_dir_all(&recordings).unwrap();
        engine.set_recordings_root(recordings.clone());
        let teamspeak = TeamSpeak::open(Arc::clone(&store), Arc::clone(&engine), dir.join("media"));
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
            recordings_root: recordings,
            teamspeak,
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

    #[test]
    fn recording_download_rejects_another_mount() {
        let (handle, store, token, dir) = boot(424_242);
        let addr = handle.addr;
        let sb = br#"{"name":"Night","slug":"night"}"#;
        let req = format!("POST /api/stations HTTP/1.0\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", sb.len());
        let (code, body) = call(addr, &req, sb);
        assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
        let sid = serde_json::from_slice::<Value>(&body_of(&body)).unwrap()["id"].as_str().unwrap().to_string();
        let mut mounts = Vec::new();
        for path in ["/live", "/late"] {
            let create = format!(r#"{{"station_id":"{sid}","path":"{path}","source_password":"secret"}}"#);
            let req = format!("POST /api/mounts HTTP/1.0\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", create.len());
            let (code, body) = call(addr, &req, create.as_bytes());
            assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
            mounts.push(serde_json::from_slice::<Value>(&body_of(&body)).unwrap()["id"].as_str().unwrap().to_string());
        }
        let start = br#"{"max_duration_ms":60000}"#;
        let req = format!("POST /api/mounts/{}/record/start HTTP/1.0\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", mounts[0], start.len());
        let (code, body) = call(addr, &req, start);
        assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));
        let rec_id = serde_json::from_slice::<Value>(&body_of(&body)).unwrap()["id"].as_str().unwrap().to_string();
        let file = dir.join("recordings").join(&mounts[0]).join(format!("{rec_id}.mp3"));
        let began = std::time::Instant::now();
        while began.elapsed() < Duration::from_secs(4) && file.metadata().map(|m| m.len() == 0).unwrap_or(true) {
            std::thread::sleep(Duration::from_millis(30));
        }
        assert!(file.metadata().unwrap().len() > 0, "recording file is empty");
        let req = format!("POST /api/mounts/{}/record/stop HTTP/1.0\r\nAuthorization: Bearer {token}\r\nContent-Length: 2\r\n\r\n", mounts[0]);
        let (code, body) = call(addr, &req, b"{}");
        assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));
        let stopped = file.metadata().unwrap().len();
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(file.metadata().unwrap().len(), stopped, "stop left the file open");
        assert!(!std::fs::read(&file).unwrap().is_empty());

        let req = format!("GET /api/mounts/{}/recordings/{rec_id} HTTP/1.0\r\nAuthorization: Bearer {token}\r\n\r\n", mounts[0]);
        let (code, body) = call(addr, &req, b"");
        assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));
        assert!(body_of(&body).len() > 0);

        let secret = dir.join("recordings").join(&mounts[1]);
        std::fs::create_dir_all(&secret).unwrap();
        std::fs::write(secret.join("secret.mp3"), b"OTHER-MOUNT-SECRET").unwrap();
        let req = format!("GET /api/mounts/{}/recordings/{rec_id} HTTP/1.0\r\nAuthorization: Bearer {token}\r\n\r\n", mounts[1]);
        let (code, body) = call(addr, &req, b"");
        assert_eq!(code, 404, "{}", String::from_utf8_lossy(&body));
        assert!(!body.windows(b"OTHER-MOUNT-SECRET".len()).any(|w| w == b"OTHER-MOUNT-SECRET"));

        let req = format!("GET /api/mounts/{}/recordings/{rec_id} HTTP/1.0\r\n\r\n", mounts[0]);
        let (code, _) = call(addr, &req, b"");
        assert_eq!(code, 401);

        let audits = store.list_audit(40).unwrap();
        assert!(audits.iter().any(|a| a.action == "record_start"));
        assert!(audits.iter().any(|a| a.action == "record_stop"));
        let _ = store.delete_recording_row(&rec_id);
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

    #[test]
    fn teamspeak_output_keeps_secrets_out_of_the_response() {
        let (handle, store, token, _dir) = boot(64);
        let addr = handle.addr;
        let sb = br#"{"name":"Night","slug":"night"}"#;
        let req = format!("POST /api/stations HTTP/1.0\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", sb.len());
        let (code, body) = call(addr, &req, sb);
        assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
        let sid = serde_json::from_slice::<Value>(&body_of(&body)).unwrap()["id"].as_str().unwrap().to_string();
        let create = format!(r#"{{"station_id":"{sid}","path":"/live","source_password":"secret"}}"#);
        let req = format!("POST /api/mounts HTTP/1.0\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", create.len());
        let (code, body) = call(addr, &req, create.as_bytes());
        assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
        let mid = serde_json::from_slice::<Value>(&body_of(&body)).unwrap()["id"].as_str().unwrap().to_string();
        let secret = "s3cret-pass-xyz";
        let channel_secret = "channel-secret-xyz";
        let payload = serde_json::json!({
            "host": "voice.example.test",
            "voice_port": 9987,
            "server_password": secret,
            "nickname": "NavaCast",
            "channel_password": channel_secret,
            "volume": 40,
            "enabled": false
        }).to_string();
        let _ = mid;
        let req = format!("POST /api/bots HTTP/1.0\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", payload.len());
        let (code, body) = call(addr, &req, payload.as_bytes());
        assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
        let text = String::from_utf8_lossy(&body);
        assert!(!text.contains(secret));
        assert!(!text.contains(channel_secret));
        assert!(!text.contains("\"identity\""));
        assert!(!text.contains("server_password\""));
        let view: Value = serde_json::from_slice(&body_of(&body)).unwrap();
        assert_eq!(view["server_password_set"], true);
        assert_eq!(view["channel_password_set"], true);
        assert_eq!(view["identity_ready"], false);
        assert_eq!(view["status"], "disconnected");
        assert_eq!(view["volume"], 40);
        let id = view["id"].as_str().unwrap();
        let stored = store.output(id).unwrap().unwrap();
        assert_eq!(stored.server_password, secret);
        assert_eq!(stored.channel_password, channel_secret);
        assert!(stored.identity.is_empty());

        let patch = br#"{"nickname":"Booth"}"#;
        let req = format!("PATCH /api/bots/{id} HTTP/1.0\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", patch.len());
        let (code, body) = call(addr, &req, patch);
        assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));
        let text = String::from_utf8_lossy(&body);
        assert!(!text.contains(secret));
        assert_eq!(store.output(id).unwrap().unwrap().server_password, secret);
        assert_eq!(serde_json::from_slice::<Value>(&body_of(&body)).unwrap()["nickname"], "Booth");

        let feed = format!(r#"{{"bot_id":"{id}","feed":true}}"#);
        let req = format!("POST /api/stations/{sid}/outputs HTTP/1.0\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", feed.len());
        let (code, body) = call(addr, &req, feed.as_bytes());
        assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));
        let text = String::from_utf8_lossy(&body);
        assert!(!text.contains(secret));
        assert!(!text.contains(channel_secret));
        assert!(text.contains("\"feeding\":true") || text.contains("\"feeding\": true"));
        let req = format!("GET /api/bots HTTP/1.0\r\nAuthorization: Bearer {token}\r\n\r\n");
        let (code, body) = call(addr, &req, b"");
        assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));
        let text = String::from_utf8_lossy(&body);
        assert!(!text.contains(secret));
        assert!(!text.contains(channel_secret));
        assert!(!text.contains("\"identity\""));
        let other = br#"{"name":"Day","slug":"day"}"#;
        let req = format!("POST /api/stations HTTP/1.0\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", other.len());
        let (code, body) = call(addr, &req, other);
        assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
        let sid2 = serde_json::from_slice::<Value>(&body_of(&body)).unwrap()["id"].as_str().unwrap().to_string();
        let feed = format!(r#"{{"bot_id":"{id}","feed":true}}"#);
        let req = format!("POST /api/stations/{sid2}/outputs HTTP/1.0\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", feed.len());
        let (code, _) = call(addr, &req, feed.as_bytes());
        assert_eq!(code, 200);
        assert_eq!(store.output(id).unwrap().unwrap().station_id, sid2);
        assert_eq!(store.output(id).unwrap().unwrap().server_password, secret);
    }

    #[test]
    fn blackhole_teamspeak_connect_leaves_the_desk_and_the_pump_running() {
        // A real level-8 identity so this test measures the socket, not the hash search.
        const FIXTURE_IDENTITY: &str = r#"{"key":"E62P0TAe/fw7wbhc8OxnO7ebfR5ckiQ2KfznOjvzI/s=","counter":5,"max_counter":5}"#;
        let (handle, store, token, _dir) = boot(95);
        let addr = handle.addr;
        let limit = Duration::from_millis(800);
        let sb = br#"{"name":"Night","slug":"night"}"#;
        let req = format!("POST /api/stations HTTP/1.0\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", sb.len());
        let (code, body, elapsed) = call_within(addr, &req, sb, limit);
        assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
        assert!(elapsed < limit, "creating a station waited {elapsed:?}");
        let sid = serde_json::from_slice::<Value>(&body_of(&body)).unwrap()["id"].as_str().unwrap().to_string();
        let create = format!(r#"{{"station_id":"{sid}","path":"/live","source_password":"secret"}}"#);
        let req = format!("POST /api/mounts HTTP/1.0\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", create.len());
        let (code, body, _) = call_within(addr, &req, create.as_bytes(), Duration::from_secs(3));
        assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));

        let mut pumped = 0u64;
        let started = std::time::Instant::now();
        while started.elapsed() < Duration::from_secs(4) {
            let req = format!("GET /api/live HTTP/1.0\r\nAuthorization: Bearer {token}\r\n\r\n");
            let (code, body, _) = call_within(addr, &req, b"", Duration::from_secs(1));
            if code == 200 {
                let live: Value = serde_json::from_slice(&body_of(&body)).unwrap_or(Value::Null);
                pumped = live.as_array().and_then(|rows| rows.first()).and_then(|row| row.get("bytes_sent")).and_then(|v| v.as_u64()).unwrap_or(0);
                if pumped > 0 { break; }
            }
            thread::sleep(Duration::from_millis(40));
        }
        assert!(pumped > 0, "the mount pump did not start");

        let mut ids = Vec::new();
        for (nick, host) in [("North", "192.0.2.1"), ("South", "198.51.100.1")] {
            let payload = format!(r#"{{"nickname":"{nick}","host":"{host}","voice_port":9987,"server_password":"hidden-secret"}}"#);
            let req = format!("POST /api/bots HTTP/1.0\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", payload.len());
            let (code, body, elapsed) = call_within(addr, &req, payload.as_bytes(), limit);
            assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
            assert!(elapsed < limit, "creating {nick} waited {elapsed:?}");
            let text = String::from_utf8_lossy(&body);
            assert!(!text.contains("hidden-secret"));
            assert!(!text.contains("\"identity\""));
            let id = serde_json::from_slice::<Value>(&body_of(&body)).unwrap()["id"].as_str().unwrap().to_string();
            store.set_output_identity(&id, FIXTURE_IDENTITY).unwrap();
            ids.push(id);
        }
        for id in &ids {
            let req = format!("POST /api/bots/{id}/connect HTTP/1.0\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n");
            let (code, body, elapsed) = call_within(addr, &req, b"{}", limit);
            assert_eq!(code, 200, "elapsed {elapsed:?} {}", String::from_utf8_lossy(&body));
            assert!(elapsed < limit, "connect for {id} waited on the socket ({elapsed:?})");
            let text = String::from_utf8_lossy(&body);
            assert!(!text.contains("hidden-secret"));
            assert!(!text.contains(FIXTURE_IDENTITY));
            assert!(!text.contains("\"identity\""));
        }

        for path in ["/api/health", "/api/stations", "/api/live", "/api/bots"] {
            let req = format!("GET {path} HTTP/1.0\r\nAuthorization: Bearer {token}\r\n\r\n");
            let (code, body, elapsed) = call_within(addr, &req, b"", limit);
            assert_eq!(code, 200, "{path} {}", String::from_utf8_lossy(&body));
            assert!(elapsed < limit, "{path} stalled for {elapsed:?} while bots were connecting");
            let text = String::from_utf8_lossy(&body);
            assert!(!text.contains("hidden-secret"), "{path} echoed a password");
            assert!(!text.contains(FIXTURE_IDENTITY), "{path} echoed the identity");
        }
        thread::sleep(Duration::from_millis(250));
        let req = format!("GET /api/live HTTP/1.0\r\nAuthorization: Bearer {token}\r\n\r\n");
        let (code, body, elapsed) = call_within(addr, &req, b"", limit);
        assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));
        assert!(elapsed < limit, "live stalled for {elapsed:?}");
        let live: Value = serde_json::from_slice(&body_of(&body)).unwrap();
        let later = live.as_array().and_then(|rows| rows.first()).and_then(|row| row.get("bytes_sent")).and_then(|v| v.as_u64()).unwrap_or(0);
        assert!(later > pumped, "mount pump stalled at {pumped} bytes while TeamSpeak was connecting");
    }

    fn call_within(addr: SocketAddr, req: &str, body: &[u8], limit: Duration) -> (u16, Vec<u8>, Duration) {
        let started = std::time::Instant::now();
        let mut sock = match TcpStream::connect_timeout(&addr, limit) {
            Ok(sock) => sock,
            Err(_) => return (0, Vec::new(), started.elapsed()),
        };
        let _ = sock.set_read_timeout(Some(limit));
        let _ = sock.set_write_timeout(Some(limit));
        if sock.write_all(req.as_bytes()).is_err() {
            return (0, Vec::new(), started.elapsed());
        }
        if !body.is_empty() && sock.write_all(body).is_err() {
            return (0, Vec::new(), started.elapsed());
        }
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            if started.elapsed() > limit {
                break;
            }
            match sock.read(&mut tmp) {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        }
        let text = String::from_utf8_lossy(&buf);
        let code = text.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        (code, buf, started.elapsed())
    }

    fn body_of(buf: &[u8]) -> Vec<u8> {
        match buf.windows(4).position(|w| w == b"\r\n\r\n") {
            Some(i) => buf[i + 4..].to_vec(),
            None => buf.to_vec(),
        }
    }

    fn authed(addr: SocketAddr, token: &str, method: &str, path: &str, body: &str) -> (u16, Vec<u8>) {
        let req = format!(
            "{method} {path} HTTP/1.0\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        call(addr, &req, body.as_bytes())
    }

    #[test]
    fn accounts_are_isolated_capped_and_the_desk_stays_fast() {
        const FIXTURE_IDENTITY: &str = r#"{"key":"E62P0TAe/fw7wbhc8OxnO7ebfR5ckiQ2KfznOjvzI/s=","counter":5,"max_counter":5}"#;
        let (handle, store, owner, _dir) = boot(91);
        let addr = handle.addr;
        let perms = r#""permissions":{"stations":true,"mounts":true,"playlists":true,"library":true,"import":true,"recordings":true,"schedules":true,"bots":true,"listeners":true,"takeover":true}"#;

        let ada_body = format!(r#"{{"username":"ada","password":"password123",{perms},"quotas":{{"max_stations":1,"max_playlists":2,"max_bots":2}}}}"#);
        let (code, body) = authed(addr, &owner, "POST", "/api/users", &ada_body);
        assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
        let ada_id = serde_json::from_slice::<Value>(&body_of(&body)).unwrap()["id"].as_str().unwrap().to_string();

        let bob_body = format!(r#"{{"username":"bob","password":"password123",{perms}}}"#);
        let (code, body) = authed(addr, &owner, "POST", "/api/users", &bob_body);
        assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));

        let cam_body = format!(r#"{{"username":"cam","password":"password123","permissions":{{"stations":false,"mounts":true,"playlists":true,"library":true,"import":true,"recordings":true,"schedules":true,"bots":true,"listeners":true,"takeover":true}}}}"#);
        let (code, body) = authed(addr, &owner, "POST", "/api/users", &cam_body);
        assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));

        let login = |name: &str| {
            let payload = format!(r#"{{"username":"{name}","password":"password123"}}"#);
            let req = format!("POST /api/login HTTP/1.0\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", payload.len());
            let (code, body) = call(addr, &req, payload.as_bytes());
            assert_eq!(code, 200, "{name} {}", String::from_utf8_lossy(&body));
            serde_json::from_slice::<Value>(&body_of(&body)).unwrap()["token"].as_str().unwrap().to_string()
        };
        let ada = login("ada");
        let bob = login("bob");
        let cam = login("cam");

        let (code, _) = authed(addr, &ada, "PATCH", &format!("/api/users/{ada_id}"), r#"{"quotas":{"max_stations":9}}"#);
        assert_eq!(code, 403);

        let (code, body) = authed(addr, &ada, "POST", "/api/stations", r#"{"name":"Alpha","slug":"alpha"}"#);
        assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
        let alpha = serde_json::from_slice::<Value>(&body_of(&body)).unwrap()["id"].as_str().unwrap().to_string();
        let (code, body) = authed(addr, &bob, "POST", "/api/stations", r#"{"name":"Beta","slug":"beta"}"#);
        assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
        let beta = serde_json::from_slice::<Value>(&body_of(&body)).unwrap()["id"].as_str().unwrap().to_string();

        let (code, body) = authed(addr, &ada, "GET", "/api/stations", "");
        assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));
        let stations = String::from_utf8_lossy(&body);
        assert!(stations.contains("alpha"));
        assert!(!stations.contains("beta"), "ada's station list included bob's station");
        let (code, _) = authed(addr, &ada, "DELETE", &format!("/api/stations/{beta}"), "");
        assert_eq!(code, 403);

        let (code, body) = authed(addr, &ada, "POST", "/api/playlists", &format!(r#"{{"station_id":"{alpha}","name":"Ada list"}}"#));
        assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
        let (code, body) = authed(addr, &bob, "POST", "/api/playlists", &format!(r#"{{"station_id":"{beta}","name":"Bob list"}}"#));
        assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
        let bob_list = serde_json::from_slice::<Value>(&body_of(&body)).unwrap()["id"].as_str().unwrap().to_string();
        let (code, body) = authed(addr, &ada, "GET", &format!("/api/playlists?station={alpha}"), "");
        assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));
        assert!(!String::from_utf8_lossy(&body).contains("Bob list"));
        let (code, _) = authed(addr, &ada, "GET", &format!("/api/playlists?station={beta}"), "");
        assert_eq!(code, 403);
        let (code, _) = authed(addr, &ada, "DELETE", &format!("/api/playlists/{bob_list}"), "");
        assert_eq!(code, 403);

        let (code, body) = authed(addr, &ada, "POST", "/api/bots", r#"{"nickname":"AdaBot","host":"192.0.2.1","voice_port":9987}"#);
        assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
        let ada_bot = serde_json::from_slice::<Value>(&body_of(&body)).unwrap()["id"].as_str().unwrap().to_string();
        let (code, body) = authed(addr, &bob, "GET", "/api/bots", "");
        assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));
        assert!(!String::from_utf8_lossy(&body).contains("AdaBot"));
        let (code, _) = authed(addr, &bob, "PATCH", &format!("/api/bots/{ada_bot}"), r#"{"nickname":"Stolen"}"#);
        assert_eq!(code, 403);

        let (code, body) = authed(addr, &ada, "POST", "/api/stations", r#"{"name":"Extra","slug":"extra"}"#);
        assert_eq!(code, 403, "{}", String::from_utf8_lossy(&body));
        assert!(String::from_utf8_lossy(&body).contains("station limit reached"), "{}", String::from_utf8_lossy(&body));
        let (code, _) = authed(addr, &ada, "DELETE", &format!("/api/stations/{alpha}"), "");
        assert_eq!(code, 200);
        let (code, body) = authed(addr, &ada, "POST", "/api/stations", r#"{"name":"Alpha","slug":"alpha"}"#);
        assert_eq!(code, 201, "deleting a station frees the quota: {}", String::from_utf8_lossy(&body));

        let (code, body) = authed(addr, &cam, "POST", "/api/stations", r#"{"name":"Nope","slug":"nope"}"#);
        assert_eq!(code, 403, "{}", String::from_utf8_lossy(&body));
        assert!(String::from_utf8_lossy(&body).contains("stations permission is off"));

        let (code, body) = authed(addr, &owner, "GET", &format!("/api/users/{ada_id}"), "");
        assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));
        let live = String::from_utf8_lossy(&body);
        assert!(live.contains("\"stations\"") && live.contains("\"bots\"") && live.contains("\"library\""));

        let (code, _) = authed(addr, &owner, "PATCH", &format!("/api/users/{ada_id}"), r#"{"disabled":true}"#);
        assert_eq!(code, 200);
        let locked = r#"{"username":"ada","password":"password123"}"#;
        let req = format!("POST /api/login HTTP/1.0\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", locked.len());
        let (code, _) = call(addr, &req, locked.as_bytes());
        assert_eq!(code, 401);
        let (code, _) = authed(addr, &ada, "GET", "/api/stations", "");
        assert_eq!(code, 401);

        let sb = r#"{"name":"Desk","slug":"desk"}"#;
        let (code, body) = authed(addr, &owner, "POST", "/api/stations", sb);
        assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
        let sid = serde_json::from_slice::<Value>(&body_of(&body)).unwrap()["id"].as_str().unwrap().to_string();
        let mount = format!(r#"{{"station_id":"{sid}","path":"/live","source_password":"secret"}}"#);
        let (code, body) = authed(addr, &owner, "POST", "/api/mounts", &mount);
        assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
        let (code, body) = authed(addr, &owner, "POST", "/api/bots", r#"{"nickname":"Hole","host":"192.0.2.1","voice_port":9987,"server_password":"hidden-secret"}"#);
        assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
        let hole = serde_json::from_slice::<Value>(&body_of(&body)).unwrap()["id"].as_str().unwrap().to_string();
        store.set_output_identity(&hole, FIXTURE_IDENTITY).unwrap();
        let (code, _, elapsed) = call_within(
            addr,
            &format!("POST /api/bots/{hole}/connect HTTP/1.0\r\nAuthorization: Bearer {owner}\r\nContent-Length: 0\r\n\r\n"),
            b"",
            Duration::from_millis(800),
        );
        assert_eq!(code, 200, "connect did not return");
        assert!(elapsed < Duration::from_millis(800), "connect waited {elapsed:?}");
        let limit = Duration::from_millis(800);
        let (code, _, elapsed) = call_within(
            addr,
            &format!("GET /api/live HTTP/1.0\r\nAuthorization: Bearer {bob}\r\n\r\n"),
            b"",
            limit,
        );
        assert_eq!(code, 200);
        assert!(elapsed < limit, "desk GET waited on TeamSpeak: {elapsed:?}");
        let (code, _, elapsed) = call_within(
            addr,
            &format!("GET /api/health HTTP/1.0\r\nAuthorization: Bearer {owner}\r\n\r\n"),
            b"",
            limit,
        );
        assert_eq!(code, 200);
        assert!(elapsed < limit, "health GET waited on TeamSpeak: {elapsed:?}");
        let mut pumped = 0u64;
        let started = std::time::Instant::now();
        while started.elapsed() < Duration::from_secs(4) {
            let (code, body, elapsed) = call_within(
                addr,
                &format!("GET /api/live HTTP/1.0\r\nAuthorization: Bearer {owner}\r\n\r\n"),
                b"",
                limit,
            );
            assert!(elapsed < limit, "desk GET waited on TeamSpeak: {elapsed:?}");
            if code == 200 {
                let live: Value = serde_json::from_slice(&body_of(&body)).unwrap_or(Value::Null);
                pumped = live.as_array().and_then(|rows| rows.first()).and_then(|row| row.get("bytes_sent")).and_then(|v| v.as_u64()).unwrap_or(0);
                if pumped > 0 { break; }
            }
            thread::sleep(Duration::from_millis(40));
        }
        assert!(pumped > 0, "the mount pump stalled while a bot was connecting");
    }

    #[test]
    fn bearer_tokens_honor_scope_expiry_revoke_and_denials() {
        let (handle, store, owner, _dir) = boot(92);
        let addr = handle.addr;
        let ada = r#"{"username":"ada","password":"password123","permissions":{"stations":true,"mounts":false},"quotas":{"max_stations":1}}"#;
        let (code, body) = authed(addr, &owner, "POST", "/api/users", ada);
        assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
        let ada_id = serde_json::from_slice::<Value>(&body_of(&body)).unwrap()["id"].as_str().unwrap().to_string();

        let (code, body) = authed(addr, &owner, "POST", "/api/tokens", r#"{"name":"narrow","scope":"owner","permissions":{"stations":true}}"#);
        assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
        let made: Value = serde_json::from_slice(&body_of(&body)).unwrap();
        assert!(made.get("token_hash").is_none(), "create returned a hash");
        let narrow = made["token"].as_str().unwrap().to_string();
        let prefix = made["prefix"].as_str().unwrap().to_string();
        assert!(narrow.starts_with(&prefix));

        let (code, body) = authed(addr, &owner, "GET", "/api/tokens", "");
        assert_eq!(code, 200);
        let listed = String::from_utf8_lossy(&body_of(&body)).to_string();
        assert!(!listed.contains(&narrow), "list returned the secret");
        assert!(!listed.contains("token_hash"), "list returned a hash");
        assert!(listed.contains(&prefix));

        let (code, body) = authed(addr, &narrow, "POST", "/api/stations", r#"{"name":"Night","slug":"night"}"#);
        assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
        let (code, body) = authed(addr, &narrow, "POST", "/api/mounts", "{}");
        assert_eq!(code, 403, "{}", String::from_utf8_lossy(&body));
        assert!(String::from_utf8_lossy(&body).contains("mounts permission is off"));
        let (code, body) = authed(addr, &narrow, "POST", "/api/tokens", r#"{"name":"wider","scope":"owner","permissions":{"mounts":true}}"#);
        assert_eq!(code, 403, "{}", String::from_utf8_lossy(&body));
        assert!(String::from_utf8_lossy(&body).contains("cannot grant"));
        let (code, _) = authed(addr, &narrow, "POST", "/api/users", r#"{"username":"cam","password":"password123","permissions":{"mounts":true}}"#);
        assert_eq!(code, 403);
        let (code, _) = authed(addr, &narrow, "POST", "/api/users", r#"{"username":"cam","password":"password123","permissions":{"stations":true}}"#);
        assert_eq!(code, 201);

        let desk = format!(r#"{{"name":"ada desk","scope":"user","user_id":"{ada_id}","permissions":{{"stations":true,"mounts":true}}}}"#);
        let (code, body) = authed(addr, &owner, "POST", "/api/tokens", &desk);
        assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
        let ada_token = serde_json::from_slice::<Value>(&body_of(&body)).unwrap()["token"].as_str().unwrap().to_string();
        let (code, body) = authed(addr, &ada_token, "POST", "/api/stations", r#"{"name":"Ada","slug":"ada"}"#);
        assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
        let ada_station = serde_json::from_slice::<Value>(&body_of(&body)).unwrap()["id"].as_str().unwrap().to_string();
        let (code, body) = authed(addr, &ada_token, "POST", "/api/stations", r#"{"name":"Ada 2","slug":"ada2"}"#);
        assert_eq!(code, 403, "{}", String::from_utf8_lossy(&body));
        assert!(String::from_utf8_lossy(&body).contains("station limit"));
        let mount = format!(r#"{{"station_id":"{ada_station}","path":"/ada","source_password":"sourcepass"}}"#);
        let (code, body) = authed(addr, &ada_token, "POST", "/api/mounts", &mount);
        assert_eq!(code, 403, "{}", String::from_utf8_lossy(&body));
        let (code, _) = authed(addr, &ada_token, "GET", "/api/tokens", "");
        assert_eq!(code, 403);

        let login = br#"{"username":"ada","password":"password123"}"#;
        let login_req = format!("POST /api/login HTTP/1.0\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", login.len());
        let (login_code, login_body) = call(addr, &login_req, login);
        assert_eq!(login_code, 200, "{}", String::from_utf8_lossy(&login_body));
        let ada_session = serde_json::from_slice::<Value>(&body_of(&login_body)).unwrap()["token"].as_str().unwrap().to_string();
        let (code, _) = authed(addr, &ada_session, "GET", "/api/tokens", "");
        assert_eq!(code, 403);

        let (code, _) = authed(addr, &owner, "PATCH", &format!("/api/users/{ada_id}"), r#"{"disabled":true}"#);
        assert_eq!(code, 200);
        let (code, _) = authed(addr, &ada_token, "GET", "/api/stations", "");
        assert_eq!(code, 401);
        let (code, _) = authed(addr, &ada_session, "GET", "/api/stations", "");
        assert_eq!(code, 401);
        let (code, _) = authed(addr, &owner, "PATCH", &format!("/api/users/{ada_id}"), r#"{"disabled":false}"#);
        assert_eq!(code, 200);
        let (code, _) = authed(addr, &ada_token, "GET", "/api/stations", "");
        assert_eq!(code, 200);

        let ada_id_for_rotate = serde_json::from_slice::<Value>(&body_of(&authed(addr, &owner, "GET", "/api/tokens", "").1)).unwrap();
        let ada_row = ada_id_for_rotate.as_array().unwrap().iter().find(|row| row["name"] == "ada desk").unwrap();
        let ada_api_id = ada_row["id"].as_str().unwrap();
        let (code, body) = authed(addr, &owner, "POST", &format!("/api/tokens/{ada_api_id}/rotate"), "");
        assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));
        let rotated = serde_json::from_slice::<Value>(&body_of(&body)).unwrap()["token"].as_str().unwrap().to_string();
        assert!(!String::from_utf8_lossy(&body_of(&body)).contains("token_hash"));
        let (code, _) = authed(addr, &ada_token, "GET", "/api/stations", "");
        assert_eq!(code, 401);
        let (code, _) = authed(addr, &rotated, "GET", "/api/stations", "");
        assert_eq!(code, 200);

        let narrow_id = made["id"].as_str().unwrap();
        let (code, _) = authed(addr, &owner, "DELETE", &format!("/api/tokens/{narrow_id}"), "");
        assert_eq!(code, 200);
        let (code, _) = authed(addr, &narrow, "GET", "/api/stations", "");
        assert_eq!(code, 401);

        let (code, body) = authed(addr, &owner, "POST", "/api/tokens", r#"{"name":"spent","scope":"owner","expires_at":1,"permissions":{"stations":true}}"#);
        assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
        let spent = serde_json::from_slice::<Value>(&body_of(&body)).unwrap()["token"].as_str().unwrap().to_string();
        let (code, _) = authed(addr, &spent, "GET", "/api/stations", "");
        assert_eq!(code, 401);

        let (code, body) = authed(addr, &owner, "POST", "/api/stations", r#"{"name":"Desk","slug":"desk"}"#);
        assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
        let sid = serde_json::from_slice::<Value>(&body_of(&body)).unwrap()["id"].as_str().unwrap().to_string();
        let mount = format!(r#"{{"station_id":"{sid}","path":"/live","source_password":"sourcepass"}}"#);
        let (code, body) = authed(addr, &owner, "POST", "/api/mounts", &mount);
        assert_eq!(code, 201, "{}", String::from_utf8_lossy(&body));
        let mid = serde_json::from_slice::<Value>(&body_of(&body)).unwrap()["id"].as_str().unwrap().to_string();
        let (code, body) = authed(addr, &owner, "POST", &format!("/api/mounts/{mid}/pause"), "");
        assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));
        let (code, body) = authed(addr, &owner, "GET", "/api/live", "");
        assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));
        let live: Value = serde_json::from_slice(&body_of(&body)).unwrap();
        let row = live.as_array().unwrap().iter().find(|row| row["id"] == mid).unwrap();
        assert_eq!(row["paused"], true);
        let (code, _) = authed(addr, &owner, "POST", &format!("/api/mounts/{mid}/play"), "");
        assert_eq!(code, 200);
        let (code, _) = authed(addr, &rotated, "POST", &format!("/api/mounts/{mid}/listeners/kick"), "");
        assert_eq!(code, 403);

        let (code, body) = authed(addr, &owner, "POST", "/api/tokens/rotate", "");
        assert_eq!(code, 200, "{}", String::from_utf8_lossy(&body));
        let fresh = serde_json::from_slice::<Value>(&body_of(&body)).unwrap()["token"].as_str().unwrap().to_string();
        let (code, _) = authed(addr, &owner, "GET", "/api/me", "");
        assert_eq!(code, 401);
        let (code, _) = authed(addr, &fresh, "GET", "/api/me", "");
        assert_eq!(code, 200);
        let (code, _) = authed(addr, &rotated, "GET", "/api/stations", "");
        assert_eq!(code, 200);

        let audits = store.list_audit(80).unwrap();
        assert!(audits.iter().any(|a| a.action == "token_create"));
        assert!(audits.iter().any(|a| a.action == "token_revoke"));
        assert!(audits.iter().any(|a| a.action == "denied"));
        assert!(audits.iter().all(|a| a.detail.len() < 80), "audit detail looks like a secret");
    }

    #[test]
    fn bearer_token_attempts_are_rate_limited() {
        let (handle, _store, _token, _dir) = boot(93);
        let addr = handle.addr;
        let req = "GET /api/stations HTTP/1.0\r\nAuthorization: Bearer nv_not-a-real-token\r\n\r\n";
        for _ in 0..8 {
            let (code, _) = call(addr, req, b"");
            assert_eq!(code, 401);
        }
        let (code, _) = call(addr, req, b"");
        assert_eq!(code, 429);
    }
}
