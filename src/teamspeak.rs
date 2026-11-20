//! TeamSpeak 3 voice outputs.
//!
//! Each output is a real voice client (tsclientlib). It joins a channel and sends
//! 48 kHz Opus music frames. PCM is copied off the station pump with `try_send`.
//! A slow or dead TeamSpeak socket never waits on that pump: the bot thread drops
//! frames and keeps its own clock. Silence is sent when the station has nothing to say.
//!
//! Server passwords, channel passwords, and identity private keys stay in SQLite
//! and in this process. They are not written to the log and not returned by the API.

use crate::auth;
use crate::encoder::{AudioFactory, RouterFactory};
use crate::engine::{Engine, PcmChunk};
use crate::error::Error;
use crate::store::{self, Output, Store};
use crate::tscmd::{self, Command, Vol};
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::Read;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tsclientlib::data::exts::{M2BClientEditExt, M2BClientMoveExt, M2BClientUpdateExt};
use tsclientlib::messages::OutMessageTrait;
use tsclientlib::{ChannelId, ClientId, ClientPermissionHint, ClientType, Connection, DisconnectOptions, Identity, OutCommandExt, Reason, StreamItem};
use tsproto_packets::packets::{AudioData, CodecType, OutAudio};

const FRAME: Duration = Duration::from_millis(20);
const SAMPLES: usize = 960;
const STEREO: usize = SAMPLES * 2;
const MAX_QUEUE: usize = 40;
const MAX_BOTS: usize = 32;
const DNS_TIMEOUT: Duration = Duration::from_secs(4);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
/// Stuck getaddrinfo calls are abandoned, not joined. Cap how many can pile up.
static DNS_INFLIGHT: AtomicUsize = AtomicUsize::new(0);
const OPUS_RATE: u32 = 48_000;

pub struct TeamSpeak {
    hub: Arc<Hub>,
}

struct Hub {
    store: Arc<Store>,
    engine: Arc<Engine>,
    media_root: PathBuf,
    stop: AtomicBool,
    bots: Mutex<HashMap<String, Arc<Worker>>>,
    master: AtomicU8,
}

struct Worker {
    stop: Arc<AtomicBool>,
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

struct Shared {
    id: String,
    cfg: RwLock<Cfg>,
    live: Mutex<Live>,
    play: Mutex<Play>,
    spam: Mutex<tscmd::Gate>,
    wake_mu: Mutex<()>,
    wake: Condvar,
    linked: AtomicBool,
    generation: AtomicU64,
    cmds: Mutex<VecDeque<VoiceCmd>>,
    master: AtomicU8,
}

enum VoiceCmd {
    Join { channel: u64, password: String },
    Kick { client: u16 },
    Mute { client: u16, on: bool },
    Move { client: u16, channel: u64 },
    Profile,
}

struct Cfg {
    station_id: String,
    mount_id: String,
    host: String,
    voice_port: u16,
    server_password: String,
    nickname: String,
    phonetic: String,
    away: String,
    channel_id: Option<u64>,
    channel_password: String,
    identity: String,
    volume: u8,
    enabled: bool,
    auto_reconnect: bool,
    prefix: String,
    commands: String,
    gate: String,
    gate_group: String,
}

impl Clone for Cfg {
    fn clone(&self) -> Self {
        Self {
            station_id: self.station_id.clone(),
            mount_id: self.mount_id.clone(),
            host: self.host.clone(),
            voice_port: self.voice_port,
            server_password: self.server_password.clone(),
            nickname: self.nickname.clone(),
            phonetic: self.phonetic.clone(),
            away: self.away.clone(),
            channel_id: self.channel_id,
            channel_password: self.channel_password.clone(),
            identity: self.identity.clone(),
            volume: self.volume,
            enabled: self.enabled,
            auto_reconnect: self.auto_reconnect,
            prefix: self.prefix.clone(),
            commands: self.commands.clone(),
            gate: self.gate.clone(),
            gate_group: self.gate_group.clone(),
        }
    }
}

#[derive(Clone)]
struct TreeChan {
    id: u64,
    parent: u64,
    name: String,
    clients: usize,
    locked: bool,
}

#[derive(Clone)]
struct Person {
    id: u16,
    name: String,
    muted: bool,
}

struct Live {
    status: &'static str,
    error: String,
    channel: String,
    channel_id: u64,
    now: String,
    station_name: String,
    tree: Vec<TreeChan>,
    people: Vec<Person>,
    log: VecDeque<String>,
    can_kick: bool,
    can_move: bool,
    can_mute: bool,
    perms_known: bool,
}

impl Default for Live {
    fn default() -> Self {
        Self {
            status: "disconnected",
            error: String::new(),
            channel: String::new(),
            channel_id: 0,
            now: String::new(),
            station_name: String::new(),
            tree: Vec::new(),
            people: Vec::new(),
            log: VecDeque::new(),
            can_kick: false,
            can_move: false,
            can_mute: false,
            perms_known: false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Radio,
    Queue,
    Paused,
    Stopped,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Radio => "radio",
            Mode::Queue => "queue",
            Mode::Paused => "paused",
            Mode::Stopped => "stopped",
        }
    }
}

#[derive(Clone)]
struct Track {
    media_id: String,
    title: String,
    artist: String,
    mime: String,
}

impl Track {
    fn label(&self) -> String {
        let title = if self.title.is_empty() { "Untitled".into() } else { self.title.clone() };
        if self.artist.is_empty() { title } else { format!("{} - {title}", self.artist) }
    }
}

struct Play {
    mode: Mode,
    queue: VecDeque<Track>,
    current: Option<Track>,
    skip: bool,
    volume: u8,
    repeat: String,
}

struct Decoder {
    reader: Box<dyn Read + Send>,
    cancel: Option<Box<dyn FnOnce() + Send>>,
}

impl Drop for Decoder {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel();
        }
    }
}

impl TeamSpeak {
    pub fn open(store: Arc<Store>, engine: Arc<Engine>, media_root: PathBuf) -> Arc<Self> {
        let master = store.setting("ts_master_volume").ok().flatten().and_then(|v| v.parse::<u8>().ok()).unwrap_or(100).min(100);
        let hub = Arc::new(Hub {
            store: Arc::clone(&store),
            engine,
            media_root,
            stop: AtomicBool::new(false),
            bots: Mutex::new(HashMap::new()),
            master: AtomicU8::new(master),
        });
        let ts = Arc::new(Self { hub });
        if let Ok(rows) = store.list_outputs(None) {
            for row in rows {
                ts.hub.spawn(row);
            }
        }
        ts
    }

    pub fn begin_shutdown(&self) {
        self.hub.stop.store(true, Ordering::SeqCst);
        let bots: Vec<_> = self.hub.bots.lock().unwrap_or_else(|e| e.into_inner()).values().cloned().collect();
        for bot in bots {
            bot.stop.store(true, Ordering::SeqCst);
            bot.shared.wake.notify_all();
        }
    }

    pub fn shutdown(&self) {
        self.begin_shutdown();
        let bots: Vec<_> = self.hub.bots.lock().unwrap_or_else(|e| e.into_inner()).drain().map(|(_, b)| b).collect();
        for bot in bots {
            if let Some(handle) = bot.thread.lock().unwrap_or_else(|e| e.into_inner()).take() {
                // Detach the join. A bot stuck in DNS must not keep the process from exiting on its own clock,
                // and it must not run on the thread that asked for shutdown.
                thread::spawn(move || {
                    let _ = crate::join_timeout(handle, Duration::from_secs(2));
                });
            }
        }
    }

    pub fn list_json(&self, only_user: Option<&str>) -> Result<Value, Error> {
        let mut rows = self.hub.store.list_outputs(None)?;
        if let Some(user_id) = only_user {
            rows.retain(|row| row.user_id == user_id);
        }
        if rows.len() > MAX_BOTS * 8 {
            return Err(Error::Invalid("too many bots".into()));
        }
        let snaps: Vec<_> = {
            let bots = self.hub.bots.lock().unwrap_or_else(|e| e.into_inner());
            rows.into_iter().map(|row| {
                let snap = bots.get(&row.id).map(|bot| bot.shared.snapshot());
                (row, snap)
            }).collect()
        };
        let out = snaps.iter().map(|(row, snap)| public_json(&self.hub.store, row, snap.as_ref())).collect::<Vec<_>>();
        Ok(json!({
            "master_volume": self.hub.master.load(Ordering::SeqCst),
            "bots": out,
        }))
    }

    pub fn station_board(&self, station_id: &str, only_user: Option<&str>) -> Result<Value, Error> {
        let station = self.hub.store.station(station_id)?.ok_or(Error::NotFound)?;
        let mut rows = self.hub.store.list_outputs(None)?;
        if let Some(user_id) = only_user {
            rows.retain(|row| row.user_id == user_id);
        }
        let snaps: Vec<_> = {
            let bots = self.hub.bots.lock().unwrap_or_else(|e| e.into_inner());
            rows.iter().map(|row| {
                let status = bots.get(&row.id).map(|bot| bot.shared.snapshot().status).unwrap_or("disconnected");
                (row.id.clone(), row.nickname.clone(), row.host.clone(), row.station_id.clone(), status)
            }).collect()
        };
        let chips = snaps.into_iter().map(|(id, nickname, host, station_id, status)| {
            json!({
                "id": id,
                "nickname": nickname,
                "host": host,
                "status": status,
                "feeding": station_id == station.id,
            })
        }).collect::<Vec<_>>();
        Ok(json!({ "radio": station.radio_enabled, "bots": chips }))
    }

    pub fn set_master(&self, volume: u8) -> Result<Value, Error> {
        let volume = volume.min(100);
        self.hub.store.set_setting("ts_master_volume", &volume.to_string())?;
        self.hub.master.store(volume, Ordering::SeqCst);
        let bots = self.hub.bots.lock().unwrap_or_else(|e| e.into_inner());
        for bot in bots.values() {
            bot.shared.master.store(volume, Ordering::SeqCst);
        }
        Ok(json!({ "master_volume": volume }))
    }

    pub fn set_feed(&self, station_id: &str, bot_id: &str, feed: bool, only_user: Option<&str>) -> Result<Value, Error> {
        let station = self.hub.store.station(station_id)?.ok_or(Error::NotFound)?;
        let mut row = self.hub.store.output(bot_id)?.ok_or(Error::NotFound)?;
        if feed {
            let mounts = self.hub.store.list_mounts(Some(&station.id))?;
            let mount = mounts.into_iter().next();
            row.station_id = station.id;
            row.mount_id = mount.map(|m| m.id).unwrap_or_default();
        } else if row.station_id == station_id {
            row.station_id.clear();
            row.mount_id.clear();
        }
        self.hub.store.save_output(&row)?;
        if let Some(bot) = self.hub.bots.lock().unwrap_or_else(|e| e.into_inner()).get(bot_id).cloned() {
            bot.shared.replace_cfg(&row, false);
            remember_station(&self.hub.store, &bot.shared, &row.station_id);
        }
        self.station_board(station_id, only_user)
    }

    pub fn set_radio(&self, station_id: &str, on: bool, only_user: Option<&str>) -> Result<Value, Error> {
        let station = self.hub.store.station(station_id)?.ok_or(Error::NotFound)?;
        self.hub.store.set_radio(&station.id, on)?;
        for mount in self.hub.store.list_mounts(Some(&station.id))? {
            self.hub.engine.set_path_public(&mount.path, on);
        }
        self.station_board(station_id, only_user)
    }

    pub fn create(&self, body: &Value, user_id: &str) -> Result<Value, Error> {
        if self.hub.store.list_outputs(None)?.len() >= MAX_BOTS {
            return Err(Error::Invalid("this server already has 32 bots".into()));
        }
        let mut row = blank_output();
        row.user_id = user_id.to_string();
        apply_body(&self.hub.store, &mut row, body, true)?;
        self.hub.store.insert_output(&row)?;
        self.hub.spawn(row.clone());
        self.hub.remember(&row);
        Ok(self.view(&row.id)?)
    }

    pub fn update(&self, id: &str, body: &Value) -> Result<Value, Error> {
        let mut row = self.hub.store.output(id)?.ok_or(Error::NotFound)?;
        let previous = row.clone();
        apply_body(&self.hub.store, &mut row, body, false)?;
        let reconnect = connect_changed(&previous, &row);
        self.hub.store.save_output(&row)?;
        if let Some(bot) = self.hub.bots.lock().unwrap_or_else(|e| e.into_inner()).get(id).cloned() {
            bot.shared.replace_cfg(&row, reconnect);
            remember_station(&self.hub.store, &bot.shared, &row.station_id);
        } else {
            self.hub.spawn(row.clone());
            self.hub.remember(&row);
        }
        self.view(id)
    }

    pub fn delete(&self, id: &str) -> Result<(), Error> {
        let bot = self.hub.bots.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
        if let Some(bot) = bot {
            bot.stop.store(true, Ordering::SeqCst);
            bot.shared.wake.notify_all();
            if let Some(handle) = bot.thread.lock().unwrap_or_else(|e| e.into_inner()).take() {
                // The desk asked to delete. Do not wait on this bot's socket.
                thread::spawn(move || {
                    let _ = crate::join_timeout(handle, Duration::from_secs(2));
                });
            }
        }
        self.hub.store.delete_output(id)
    }

    /// Mark this account's bots disconnected and wake their threads. Does not join.
    pub fn disconnect_user(&self, user_id: &str) -> Result<(), Error> {
        let rows: Vec<_> = self.hub.store.list_outputs(None)?.into_iter().filter(|row| row.user_id == user_id).collect();
        for row in rows {
            if self.action(&row.id, "disconnect", &json!({})).is_err() {
                let mut parked = row;
                parked.enabled = false;
                let _ = self.hub.store.save_output(&parked);
            }
        }
        Ok(())
    }

    pub fn action(&self, id: &str, action: &str, body: &Value) -> Result<Value, Error> {
        let bot = self.hub.bots.lock().unwrap_or_else(|e| e.into_inner()).get(id).cloned().ok_or(Error::NotFound)?;
        match action {
            "connect" => {
                let mut row = self.hub.store.output(id)?.ok_or(Error::NotFound)?;
                row.enabled = true;
                self.hub.store.save_output(&row)?;
                bot.shared.replace_cfg(&row, false);
                // Lock `live` only to copy the channel. set_status locks it again,
                // and a second lock on the same thread never returns.
                let channel = bot.shared.live.lock().unwrap_or_else(|e| e.into_inner()).channel.clone();
                bot.shared.set_status("connecting", "", channel);
            }
            "disconnect" => {
                let mut row = self.hub.store.output(id)?.ok_or(Error::NotFound)?;
                row.enabled = false;
                self.hub.store.save_output(&row)?;
                bot.shared.replace_cfg(&row, false);
                bot.shared.set_status("disconnected", "", String::new());
                bot.shared.linked.store(false, Ordering::SeqCst);
            }
            "play" => bot.shared.enqueue(&self.hub, body)?,
            "pause" => bot.shared.pause(),
            "skip" => bot.shared.skip(),
            "stop" => bot.shared.stop_queue(),
            "clear" => bot.shared.clear_queue(),
            "radio" => bot.shared.back_to_radio(),
            "volume" => {
                let volume = body.get("volume").and_then(|v| v.as_u64()).ok_or_else(|| Error::Invalid("volume is required".into()))?;
                if volume > 100 {
                    return Err(Error::Invalid("volume must be 0 to 100".into()));
                }
                bot.shared.set_volume(&self.hub.store, volume as u8)?;
            }
            "join" => {
                let channel = body.get("channel_id").and_then(|v| v.as_u64()).ok_or_else(|| Error::Invalid("channel_id is required".into()))?;
                let password = body.get("password").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if password.chars().any(|c| c.is_control()) || password.len() > 128 {
                    return Err(Error::Invalid("channel password is invalid".into()));
                }
                let mut row = self.hub.store.output(id)?.ok_or(Error::NotFound)?;
                row.default_channel_id = Some(channel as i64);
                if !password.is_empty() {
                    row.channel_password = password.clone();
                }
                self.hub.store.save_output(&row)?;
                bot.shared.replace_cfg(&row, false);
                let stored = if password.is_empty() { row.channel_password } else { password };
                if !bot.shared.push_cmd(VoiceCmd::Join { channel, password: stored }) {
                    return Err(Error::Invalid("the bot is busy".into()));
                }
            }
            "kick" => {
                let client = small_id(body, "client_id")?;
                if !bot.shared.push_cmd(VoiceCmd::Kick { client }) {
                    return Err(Error::Invalid("the bot is busy".into()));
                }
            }
            "mute" => {
                let client = small_id(body, "client_id")?;
                let on = body.get("muted").and_then(|v| v.as_bool()).unwrap_or(true);
                if !bot.shared.push_cmd(VoiceCmd::Mute { client, on }) {
                    return Err(Error::Invalid("the bot is busy".into()));
                }
            }
            "move" => {
                let client = small_id(body, "client_id")?;
                let channel = body.get("channel_id").and_then(|v| v.as_u64()).ok_or_else(|| Error::Invalid("channel_id is required".into()))?;
                if !bot.shared.push_cmd(VoiceCmd::Move { client, channel }) {
                    return Err(Error::Invalid("the bot is busy".into()));
                }
            }
            "reorder" => {
                let order = body.get("order").and_then(|v| v.as_array()).ok_or_else(|| Error::Invalid("order is required".into()))?;
                let ids: Vec<String> = order.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect();
                bot.shared.reorder(&ids);
            }
            "repeat" => {
                let mode = body.get("repeat").and_then(|v| v.as_str()).unwrap_or("off");
                let mode = normalize_repeat(mode);
                let mut row = self.hub.store.output(id)?.ok_or(Error::NotFound)?;
                row.repeat_mode = mode;
                self.hub.store.save_output(&row)?;
                bot.shared.replace_cfg(&row, false);
            }
            _ => return Err(Error::NotFound),
        }
        self.view(id)
    }

    fn view(&self, id: &str) -> Result<Value, Error> {
        let row = self.hub.store.output(id)?.ok_or(Error::NotFound)?;
        let snap = self.hub.bots.lock().unwrap_or_else(|e| e.into_inner()).get(id).map(|bot| bot.shared.snapshot());
        Ok(public_json(&self.hub.store, &row, snap.as_ref()))
    }
}

impl Drop for TeamSpeak {
    fn drop(&mut self) { self.shutdown(); }
}

fn remember_station(store: &Store, shared: &Shared, station_id: &str) {
    let name = store.station(station_id).ok().flatten().map(|s| s.name).unwrap_or_default();
    shared.live.lock().unwrap_or_else(|e| e.into_inner()).station_name = name;
}

impl Hub {
    fn remember(&self, row: &Output) {
        if let Some(bot) = self.bots.lock().unwrap_or_else(|e| e.into_inner()).get(&row.id).cloned() {
            remember_station(&self.store, &bot.shared, &row.station_id);
        }
    }

    fn spawn(self: &Arc<Self>, row: Output) {
        if self.stop.load(Ordering::SeqCst) {
            return;
        }
        let shared = Arc::new(Shared::from_row(&row));
        shared.master.store(self.master.load(Ordering::SeqCst), Ordering::SeqCst);
        let stop = Arc::new(AtomicBool::new(false));
        let worker = Arc::new(Worker {
            stop: Arc::clone(&stop),
            shared: Arc::clone(&shared),
            thread: Mutex::new(None),
        });
        remember_station(&self.store, &shared, &row.station_id);
        let hub = Arc::clone(self);
        let handle = thread::Builder::new()
            .name(format!("ts-{}", &row.id[..8.min(row.id.len())]))
            .spawn(move || bot_thread(hub, shared, stop))
            .expect("spawn teamspeak");
        *worker.thread.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
        self.bots.lock().unwrap_or_else(|e| e.into_inner()).insert(row.id, worker);
    }
}

fn bot_thread(hub: Arc<Hub>, shared: Arc<Shared>, stop: Arc<AtomicBool>) {
    // This thread is the only place a bot may resolve DNS, open a socket, or
    // wait on a handshake. Keep it quieter than the desk and the mount pump
    // so a long identity search cannot starve them.
    soften_bot_thread();
    let mut backoff = Duration::from_secs(1);
    while !stop.load(Ordering::SeqCst) && !hub.stop.load(Ordering::SeqCst) {
        if hub.store.output(&shared.id).ok().flatten().is_none() {
            break;
        }
        let cfg = shared.cfg.read().unwrap_or_else(|e| e.into_inner()).clone();
        if !cfg.enabled {
            shared.set_status("disconnected", "", String::new());
            shared.linked.store(false, Ordering::SeqCst);
            shared.park(&stop, Duration::from_millis(400));
            continue;
        }
        shared.set_status("connecting", "", String::new());
        let gen = shared.generation.load(Ordering::SeqCst);
        let ran = catch_unwind(AssertUnwindSafe(|| run_session(&hub, &shared, &stop, gen)));
        match ran {
            Ok(Ok(())) => backoff = Duration::from_secs(1),
            Ok(Err(err)) => {
                if stop.load(Ordering::SeqCst) || hub.stop.load(Ordering::SeqCst) {
                    break;
                }
                let cfg = shared.cfg.read().unwrap_or_else(|e| e.into_inner()).clone();
                let safe = redact(&err, &cfg);
                let shown = public_failure(&safe);
                if !cfg.auto_reconnect {
                    if let Ok(Some(mut row)) = hub.store.output(&shared.id) {
                        row.enabled = false;
                        let _ = hub.store.save_output(&row);
                        shared.replace_cfg(&row, false);
                    }
                    // Stay on the error until the user connects or disconnects.
                    // The next loop would otherwise paint this as merely disconnected.
                    shared.set_status("error", shown, String::new());
                    shared.note(if shown == "failed password" { "failed password" } else { "dropped" });
                    eprintln!("navad: teamspeak {} {}: {safe}", clip_log(&cfg.nickname), clip_log(&cfg.host));
                    shared.park(&stop, Duration::from_secs(3600));
                    continue;
                }
                if cfg.enabled {
                    shared.set_status("error", shown, String::new());
                    shared.note(if shown == "failed password" { "failed password" } else { "dropped" });
                    eprintln!("navad: teamspeak {} {}: {safe}", clip_log(&cfg.nickname), clip_log(&cfg.host));
                    shared.park(&stop, backoff);
                    backoff = (backoff * 2).min(Duration::from_secs(20));
                }
            }
            Err(_) => {
                shared.linked.store(false, Ordering::SeqCst);
                shared.set_status("error", "the bot stopped", String::new());
                shared.note("dropped");
                eprintln!("navad: teamspeak bot stopped");
                shared.park(&stop, Duration::from_secs(2));
            }
        }
        shared.linked.store(false, Ordering::SeqCst);
    }
    shared.linked.store(false, Ordering::SeqCst);
    shared.set_status("disconnected", "", String::new());
}

fn run_session(hub: &Hub, shared: &Arc<Shared>, stop: &Arc<AtomicBool>, gen: u64) -> Result<(), String> {
    let cfg = shared.cfg.read().unwrap_or_else(|e| e.into_inner()).clone();
    // DNS runs on its own short-lived thread and is abandoned on timeout.
    // Nothing above this point holds the database or the mount map.
    let addr = resolve_with_timeout(&cfg.host, cfg.voice_port)?;
    if stop.load(Ordering::SeqCst) || shared.generation.load(Ordering::SeqCst) != gen {
        return Ok(());
    }
    let identity = load_identity(&hub.store, shared)?;
    if stop.load(Ordering::SeqCst) || shared.generation.load(Ordering::SeqCst) != gen {
        return Ok(());
    }
    // A SocketAddr skips tsclientlib's resolver (system DNS, SRV, and TSDNS).
    let mut opts = Connection::build(addr)
        .name(cfg.nickname.clone())
        .identity(identity)
        .logger(quiet_logger())
        .log_commands(false)
        .log_packets(false)
        .log_udp_packets(false)
        .input_muted(false)
        .output_muted(false)
        .input_hardware_enabled(true)
        .output_hardware_enabled(true);
        if let Some(id) = cfg.channel_id {
        opts = opts.channel_id(ChannelId(id));
    }
    if !cfg.channel_password.is_empty() {
        opts = opts.channel_password(cfg.channel_password.clone());
    }
    if !cfg.server_password.is_empty() {
        opts = opts.password(cfg.server_password.clone());
    }
    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    let audio_stop = Arc::new(AtomicBool::new(false));
    let audio_flag = Arc::clone(&audio_stop);
    let audio_shared = Arc::clone(shared);
    let engine = Arc::clone(&hub.engine);
    let root = hub.media_root.clone();
    let audio = thread::Builder::new().name("ts-audio".into()).spawn(move || {
        audio_thread(audio_shared, engine, root, tx, audio_flag);
    }).map_err(|_| "could not start the audio thread".to_string())?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "could not start the voice runtime".to_string());
            let result = match runtime {
        Ok(runtime) => runtime.block_on(async {
            let mut con = opts.connect().map_err(|err| err.to_string())?;
            drive(hub, &mut con, rx, shared, stop, gen).await
        }),
        Err(err) => Err(err),
    };
    audio_stop.store(true, Ordering::SeqCst);
    shared.linked.store(false, Ordering::SeqCst);
    let _ = crate::join_timeout(audio, Duration::from_secs(2));
    let cfg = shared.cfg.read().unwrap_or_else(|e| e.into_inner()).clone();
    result.map_err(|err| redact(&err, &cfg))
}

async fn drive(
    hub: &Hub,
    con: &mut Connection,
    mut audio_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    shared: &Shared,
    stop: &AtomicBool,
    gen: u64,
) -> Result<(), String> {
    let started = Instant::now();
    let mut seq = 0u16;
    let mut in_channel = false;
    loop {
        if stop.load(Ordering::SeqCst) || shared.generation.load(Ordering::SeqCst) != gen {
            break;
        }
        let cfg = shared.cfg.read().unwrap_or_else(|e| e.into_inner()).clone();
        if !cfg.enabled {
            break;
        }
        if !in_channel && started.elapsed() > CONNECT_TIMEOUT {
            return Err("timed out connecting".into());
        }
        let tick = {
            let events = con.events();
            tokio::pin!(events);
            let wait = tokio::time::sleep(Duration::from_millis(10));
            tokio::pin!(wait);
            tokio::select! {
                ev = events.next() => Tick::Event(ev),
                _ = &mut wait => Tick::Pulse,
            }
        };
        match tick {
            Tick::Event(None) => return Err("disconnected".into()),
            Tick::Event(Some(Err(err))) => return Err(err.to_string()),
            Tick::Event(Some(Ok(item))) => {
                if let Some(err) = on_event(hub, con, shared, item, &cfg) {
                    return Err(err);
                }
                if !in_channel && shared.live.lock().unwrap_or_else(|e| e.into_inner()).status == "in_channel" {
                    in_channel = true;
                }
            }
            Tick::Pulse => {}
        }
        while let Ok(data) = audio_rx.try_recv() {
            let packet = OutAudio::new(&AudioData::C2S {
                id: seq,
                codec: CodecType::OpusMusic,
                data: &data,
            });
            seq = seq.wrapping_add(1);
            if con.send_audio(packet).is_err() {
                shared.note("dropped");
            }
        }
        if in_channel {
            apply_voice(con, shared);
            publish_description(con, shared);
        }
    }
    shared.note("dropped");
    let _ = con.disconnect(DisconnectOptions::new());
    let _ = tokio::time::timeout(Duration::from_millis(400), con.events().next()).await;
    Ok(())
}

enum Tick {
    Event(Option<Result<StreamItem, tsclientlib::Error>>),
    Pulse,
}

fn on_event(hub: &Hub, con: &mut Connection, shared: &Shared, item: StreamItem, cfg: &Cfg) -> Option<String> {
    match item {
        StreamItem::BookEvents(events) => {
            let channel = channel_name(con);
            let first = !shared.linked.load(Ordering::SeqCst);
            shared.set_status("in_channel", "", channel.clone());
            shared.linked.store(true, Ordering::SeqCst);
            capture(con, shared);
            if first {
                shared.note("connected");
                if !channel.is_empty() {
                    shared.note(&format!("joined {channel}"));
                }
            }
            let _ = cfg;
            for event in events {
                if let tsclientlib::events::Event::Message { target, invoker, message } = event {
                    if target == tsclientlib::MessageTarget::Channel {
                        on_chat(hub, con, shared, &invoker, &message);
                    }
                }
            }
        }
        StreamItem::AudioChange(tsclientlib::AudioEvent::CanSendAudio(false)) => {
            let channel = shared.live.lock().unwrap_or_else(|e| e.into_inner()).channel.clone();
            shared.set_status("in_channel", "the server is dropping audio (mute or talk power)", channel);
        }
        StreamItem::AudioChange(tsclientlib::AudioEvent::CanSendAudio(true)) => {
            let channel = shared.live.lock().unwrap_or_else(|e| e.into_inner()).channel.clone();
            shared.set_status("in_channel", "", channel);
        }
        StreamItem::DisconnectedTemporarily(_) => {
            shared.linked.store(false, Ordering::SeqCst);
            shared.note("dropped");
            if !cfg.auto_reconnect {
                return Some("dropped".into());
            }
            shared.set_status("connecting", "reconnecting", String::new());
        }
        StreamItem::IdentityLevelIncreasing(_) | StreamItem::IdentityLevelIncreased => {}
        _ => {}
    }
    let _ = cfg;
    None
}

fn on_chat(hub: &Hub, con: &mut Connection, shared: &Shared, invoker: &tsclientlib::Invoker, message: &str) {
    let own = con.get_state().ok().map(|book| book.own_client);
    if own == Some(invoker.id) {
        return;
    }
    let cfg = shared.cfg.read().unwrap_or_else(|e| e.into_inner()).clone();
    if !speaker_allowed(con, &cfg, invoker) {
        return;
    }
    let enabled = enabled_commands(&cfg.commands);
    let Some(cmd) = tscmd::parse_with(message, &cfg.prefix, &enabled) else { return };
    let key = format!("{}", invoker.id.0);
    if !shared.spam.lock().unwrap_or_else(|e| e.into_inner()).allow(&key) {
        return;
    }
    let reply = shared.command_store(hub, cmd);
    if let Some(text) = reply {
        say(con, &text);
    }
}

fn speaker_allowed(con: &Connection, cfg: &Cfg, invoker: &tsclientlib::Invoker) -> bool {
    if cfg.gate != "group" {
        return true;
    }
    let want = cfg.gate_group.trim();
    if want.is_empty() {
        return false;
    }
    let Ok(book) = con.get_state() else { return false };
    let Some(client) = book.clients.get(&invoker.id) else { return false };
    client.server_groups.iter().any(|id| {
        book.server_groups.get(id).is_some_and(|g| g.name.eq_ignore_ascii_case(want))
    })
}

fn say(con: &mut Connection, text: &str) {
    let text = clip(text, 800);
    let packet = match con.get_state() {
        Ok(book) => book.send_message(tsclientlib::MessageTarget::Channel, &text),
        Err(_) => return,
    };
    let _ = packet.send(con);
}

fn channel_name(con: &Connection) -> String {
    let Ok(book) = con.get_state() else { return String::new() };
    let Some(client) = book.clients.get(&book.own_client) else { return String::new() };
    book.channels.get(&client.channel).map(|ch| ch.name.clone()).unwrap_or_else(|| format!("{}", client.channel.0))
}

fn capture(con: &Connection, shared: &Shared) {
    let Ok(book) = con.get_state() else { return };
    let own = book.clients.get(&book.own_client);
    let channel_id = own.map(|c| c.channel.0).unwrap_or(0);
    let hints = own.and_then(|c| c.permission_hints);
    let mut tree = Vec::new();
    for ch in book.channels.values() {
        let clients = book.clients.values().filter(|c| c.channel == ch.id && matches!(c.client_type, ClientType::Normal)).count();
        tree.push(TreeChan {
            id: ch.id.0,
            parent: ch.parent.0,
            name: ch.name.clone(),
            clients,
            locked: ch.has_password == Some(true),
        });
    }
    tree.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    let people = book.clients.values().filter(|c| c.channel.0 == channel_id && c.id != book.own_client && matches!(c.client_type, ClientType::Normal)).map(|c| Person {
        id: c.id.0,
        name: c.name.clone(),
        muted: c.input_muted || !c.talk_power_granted,
    }).collect::<Vec<_>>();
    let mut live = shared.live.lock().unwrap_or_else(|e| e.into_inner());
    live.channel_id = channel_id;
    if let Some(name) = book.channels.get(&ChannelId(channel_id)).map(|ch| ch.name.clone()) {
        live.channel = name;
    }
    live.tree = tree;
    live.people = people;
    live.perms_known = hints.is_some();
    live.can_kick = hints.is_some_and(|h| h.contains(ClientPermissionHint::KICK_CHANNEL));
    live.can_move = hints.is_some_and(|h| h.contains(ClientPermissionHint::MOVE_CLIENT));
    live.can_mute = live.can_move;
}

fn apply_voice(con: &mut Connection, shared: &Shared) {
    let mut batch = Vec::new();
    {
        let mut q = shared.cmds.lock().unwrap_or_else(|e| e.into_inner());
        while batch.len() < 4 {
            let Some(cmd) = q.pop_front() else { break };
            batch.push(cmd);
        }
    }
    for cmd in batch {
        let joined = matches!(cmd, VoiceCmd::Join { .. });
        let result = match &cmd {
            VoiceCmd::Join { channel, password } => move_client(con, None, *channel, password),
            VoiceCmd::Move { client, channel } => move_client(con, Some(*client), *channel, ""),
            VoiceCmd::Kick { client } => with_client(con, *client, |c| c.kick(Reason::KickChannel).to_packet()),
            VoiceCmd::Mute { client, on } => with_client(con, *client, |c| c.edit().set_talk_power_granted(!on).to_packet()),
            VoiceCmd::Profile => send_profile(con, shared),
        };
        match result {
            Ok(()) if joined => {
                let name = channel_name(con);
                shared.note(&format!("joined {name}"));
            }
            Ok(()) => {}
            Err(err) => {
                let cfg = shared.cfg.read().unwrap_or_else(|e| e.into_inner()).clone();
                let safe = redact(&err, &cfg);
                if joined {
                    shared.note(if !cfg.channel_password.is_empty() { "failed password" } else { "could not join the channel" });
                }
                let channel = shared.live.lock().unwrap_or_else(|e| e.into_inner()).channel.clone();
                shared.set_status("in_channel", &clip(&safe, 160), channel);
            }
        }
    }
    capture(con, shared);
}

fn move_client(con: &mut Connection, client: Option<u16>, channel: u64, password: &str) -> Result<(), String> {
    let packet = {
        let book = con.get_state().map_err(|e| e.to_string())?;
        let id = client.map(ClientId).unwrap_or(book.own_client);
        let Some(who) = book.clients.get(&id) else { return Err("client is gone".into()) };
        let part = who.client_move(ChannelId(channel));
        let part = if password.is_empty() { part } else { part.set_password(password) };
        part.to_packet()
    };
    packet.send(con).map_err(|e| e.to_string())
}

fn with_client(con: &mut Connection, client: u16, build: impl FnOnce(&tsclientlib::data::Client) -> tsproto_packets::packets::OutCommand) -> Result<(), String> {
    let packet = {
        let book = con.get_state().map_err(|e| e.to_string())?;
        let Some(who) = book.clients.get(&ClientId(client)) else { return Err("client is gone".into()) };
        build(who)
    };
    packet.send(con).map_err(|e| e.to_string())
}

fn send_profile(con: &mut Connection, shared: &Shared) -> Result<(), String> {
    let cfg = shared.cfg.read().unwrap_or_else(|e| e.into_inner()).clone();
    let packet = {
        let book = con.get_state().map_err(|e| e.to_string())?;
        let away = if cfg.away.is_empty() { None } else { Some(cfg.away.as_str()) };
        book.client_update().set_name(&cfg.nickname).set_phonetic_name(&cfg.phonetic).set_away(away).to_packet()
    };
    packet.send(con).map_err(|e| e.to_string())
}

fn publish_description(con: &mut Connection, shared: &Shared) {
    let now = shared.live.lock().unwrap_or_else(|e| e.into_inner()).now.clone();
    if now.is_empty() {
        return;
    }
    static LAST: std::sync::OnceLock<Mutex<HashMap<String, (String, Instant)>>> = std::sync::OnceLock::new();
    let map = LAST.get_or_init(|| Mutex::new(HashMap::new()));
    let send_now = {
        let mut g = map.lock().unwrap_or_else(|e| e.into_inner());
        let slot = g.entry(shared.id.clone()).or_insert_with(|| (String::new(), Instant::now() - Duration::from_secs(10)));
        let same = slot.0 == now;
        let age = slot.1.elapsed();
        !(same && age < Duration::from_secs(15) || age < Duration::from_secs(2))
    };
    if !send_now {
        return;
    }
    let text = clip(&now, 180);
    let packet = {
        let Ok(book) = con.get_state() else { return };
        let Some(who) = book.clients.get(&book.own_client) else { return };
        who.edit().set_description(&text).to_packet()
    };
    if packet.send(con).is_ok() {
        let mut g = map.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = g.get_mut(&shared.id) {
            slot.0 = now;
            slot.1 = Instant::now();
        }
    }
}

fn audio_thread(
    shared: Arc<Shared>,
    engine: Arc<Engine>,
    root: PathBuf,
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    stop: Arc<AtomicBool>,
) {
    let mut enc = match opus::Encoder::new(OPUS_RATE, opus::Channels::Stereo, opus::Application::Audio) {
        Ok(enc) => enc,
        Err(err) => {
            eprintln!("navad: teamspeak opus encoder failed: {err}");
            return;
        }
    };
    let _ = enc.set_bitrate(opus::Bitrate::Bits(128_000));
    let mut radio = RadioIn::default();
    let mut decoder: Option<Decoder> = None;
    let mut failures = 0u8;
    let mut heard = String::new();
    let mut fade = 0u8;
    let mut next = Instant::now();
    let mut packet = [0u8; 1275];
    let mut shown = Instant::now();
    while !stop.load(Ordering::SeqCst) {
        if !shared.linked.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(20));
            next = Instant::now();
            continue;
        }
        let mount = shared.cfg.read().unwrap_or_else(|e| e.into_inner()).mount_id.clone();
        let mode = shared.play.lock().unwrap_or_else(|e| e.into_inner()).mode;
        let volume = shared.play.lock().unwrap_or_else(|e| e.into_inner()).volume;
        let master = shared.master.load(Ordering::SeqCst);
        if mount != heard {
            heard = mount.clone();
            fade = 12;
            radio = RadioIn::default();
        }
        let mut frame = [0i16; STEREO];
        match mode {
            Mode::Radio => {
                decoder = None;
                failures = 0;
                frame = radio.pull(&engine, &mount);
                if shown.elapsed() > Duration::from_secs(1) {
                    shown = Instant::now();
                    let title = engine.program(&mount).map(|np| {
                        if np.title.is_empty() { "silence".into() } else { np.label() }
                    }).unwrap_or_else(|| "silence".into());
                    let station = shared.live.lock().unwrap_or_else(|e| e.into_inner()).station_name.clone();
                    let now = if station.is_empty() { title } else { format!("{station}: {title}") };
                    shared.set_now(&now, Mode::Radio);
                }
            }
            Mode::Paused => {
                radio.drop_sub();
                shared.set_now_mode(Mode::Paused);
            }
            Mode::Stopped => {
                radio.drop_sub();
                decoder = None;
                shared.set_now("Stopped", Mode::Stopped);
            }
            Mode::Queue => {
                radio.drop_sub();
                if shared.take_skip() {
                    decoder = None;
                    shared.finish_current();
                }
                if decoder.is_none() {
                    match shared.next_track() {
                        Some(track) => match open_track(&root, &track) {
                            Some(dec) => {
                                let label = track.label();
                                shared.set_current(track);
                                shared.set_now(&label, Mode::Queue);
                                decoder = Some(dec);
                                failures = 0;
                            }
                            None => {
                                failures = failures.saturating_add(1);
                                shared.finish_current();
                                if failures >= 8 {
                                    shared.stop_queue();
                                }
                            }
                        },
                        None => {
                            decoder = None;
                            shared.stop_queue();
                            shared.set_now("Stopped", Mode::Stopped);
                        }
                    }
                }
                if let Some(dec) = decoder.as_mut() {
                    match read_frame(dec) {
                        Frame::Audio(buf) => frame = buf,
                        Frame::End => {
                            decoder = None;
                            shared.advance_after_track();
                        }
                        Frame::Err => {
                            decoder = None;
                            shared.finish_current();
                            failures = failures.saturating_add(1);
                        }
                    }
                }
            }
        }
        if fade > 0 {
            let gain = ((12 - fade) as u8).saturating_mul(8);
            apply_volume(&mut frame, gain.min(100));
            fade -= 1;
        }
        apply_volume(&mut frame, volume);
        apply_volume(&mut frame, master);
        match enc.encode(&frame, &mut packet) {
            Ok(n) if n > 0 => {
                let _ = tx.try_send(packet[..n].to_vec());
            }
            _ => {}
        }
        next += FRAME;
        let now = Instant::now();
        if next > now {
            thread::sleep(next - now);
        } else {
            next = now;
        }
    }
}

enum Frame {
    Audio([i16; STEREO]),
    End,
    Err,
}

fn read_frame(dec: &mut Decoder) -> Frame {
    let mut raw = [0u8; STEREO * 2];
    let mut filled = 0;
    while filled < raw.len() {
        match dec.reader.read(&mut raw[filled..]) {
            Ok(0) => {
                if filled == 0 {
                    return Frame::End;
                }
                raw[filled..].fill(0);
                break;
            }
            Ok(n) => filled += n,
            Err(_) => return Frame::Err,
        }
    }
    let mut frame = [0i16; STEREO];
    for (i, slot) in frame.iter_mut().enumerate() {
        let b = i * 2;
        *slot = i16::from_le_bytes([raw[b], raw[b + 1]]);
    }
    Frame::Audio(frame)
}

fn open_track(root: &Path, track: &Track) -> Option<Decoder> {
    let path = root.join(&track.media_id);
    let factory = RouterFactory;
    let mime = if track.mime.is_empty() { "audio/mpeg" } else { track.mime.as_str() };
    let file = File::open(&path).ok()?;
    if let Ok(opened) = factory.decode_kind("native", Box::new(file), mime, OPUS_RATE, 2) {
        return Some(Decoder { reader: opened.reader, cancel: opened.cancel });
    }
    let file = File::open(&path).ok()?;
    let opened = factory.decode_kind("ffmpeg", Box::new(file), mime, OPUS_RATE, 2).ok()?;
    Some(Decoder { reader: opened.reader, cancel: opened.cancel })
}

#[derive(Default)]
struct RadioIn {
    rx: Option<Receiver<PcmChunk>>,
    pcm: Vec<i16>,
    tail: Vec<i16>,
    phase: f64,
}

impl RadioIn {
    fn drop_sub(&mut self) {
        self.rx = None;
    }

    fn pull(&mut self, engine: &Engine, mount_id: &str) -> [i16; STEREO] {
        if self.rx.is_none() && !mount_id.is_empty() {
            self.rx = engine.subscribe_pcm(mount_id);
        }
        let mut incoming = Vec::new();
        let mut dead = false;
        if let Some(rx) = &self.rx {
            loop {
                match rx.try_recv() {
                    Ok(chunk) => incoming.push(chunk),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        dead = true;
                        break;
                    }
                }
            }
        }
        if dead {
            self.rx = None;
        }
        for chunk in incoming {
            self.push(chunk);
        }
        if self.pcm.len() >= STEREO {
            let mut frame = [0i16; STEREO];
            frame.copy_from_slice(&self.pcm[..STEREO]);
            self.pcm.drain(..STEREO);
            frame
        } else {
            [0i16; STEREO]
        }
    }

    fn push(&mut self, chunk: PcmChunk) {
        let ch = chunk.channels.max(1) as usize;
        let mut samples = Vec::with_capacity(chunk.pcm.len() / 2);
        let mut i = 0;
        while i + 1 < chunk.pcm.len() {
            samples.push(i16::from_le_bytes([chunk.pcm[i], chunk.pcm[i + 1]]));
            i += 2;
        }
        let mut joined = Vec::with_capacity(self.tail.len() + samples.len());
        joined.append(&mut self.tail);
        joined.extend_from_slice(&samples);
        let frames = joined.len() / ch;
        if frames < 2 {
            self.tail = joined;
            return;
        }
        let step = chunk.rate.max(1) as f64 / OPUS_RATE as f64;
        let mut pos = self.phase;
        while pos + 1.0 < frames as f64 {
            let at = pos.floor() as usize;
            let frac = (pos - at as f64) as f32;
            let sample = |c: usize| {
                let a = joined[at * ch + c] as f32;
                let b = joined[(at + 1) * ch + c] as f32;
                a + (b - a) * frac
            };
            let left = sample(0);
            let right = if ch > 1 { sample(1) } else { left };
            self.pcm.push(left.clamp(-32768.0, 32767.0) as i16);
            self.pcm.push(right.clamp(-32768.0, 32767.0) as i16);
            pos += step;
        }
        let keep = pos.floor() as usize;
        self.phase = pos - keep as f64;
        self.tail = joined[keep * ch..].to_vec();
        let cap = OPUS_RATE as usize;
        if self.pcm.len() > cap {
            let extra = self.pcm.len() - cap;
            self.pcm.drain(..extra);
        }
    }
}

fn apply_volume(frame: &mut [i16], volume: u8) {
    if volume >= 100 {
        return;
    }
    let gain = volume as i32;
    for sample in frame.iter_mut() {
        *sample = ((*sample as i32) * gain / 100).clamp(-32768, 32767) as i16;
    }
}

impl Shared {
    fn from_row(row: &Output) -> Self {
        let volume = row.volume.clamp(0, 100) as u8;
        Self {
            id: row.id.clone(),
            cfg: RwLock::new(cfg_from(row)),
            live: Mutex::new(Live::default()),
            play: Mutex::new(Play {
                mode: Mode::Radio,
                queue: VecDeque::new(),
                current: None,
                skip: false,
                volume,
                repeat: normalize_repeat(&row.repeat_mode),
            }),
            spam: Mutex::new(tscmd::Gate::new()),
            wake_mu: Mutex::new(()),
            wake: Condvar::new(),
            linked: AtomicBool::new(false),
            generation: AtomicU64::new(1),
            cmds: Mutex::new(VecDeque::new()),
            master: AtomicU8::new(100),
        }
    }

    fn push_cmd(&self, cmd: VoiceCmd) -> bool {
        let mut q = self.cmds.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() >= 32 {
            return false;
        }
        q.push_back(cmd);
        true
    }

    fn note(&self, line: &str) {
        let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        let stamp = store::now_unix();
        live.log.push_back(format!("{stamp} {line}"));
        while live.log.len() > 40 {
            live.log.pop_front();
        }
    }

    fn replace_cfg(&self, row: &Output, reconnect: bool) {
        let volume = row.volume.clamp(0, 100) as u8;
        let mut cfg = cfg_from(row);
        let current = self.cfg.read().unwrap_or_else(|e| e.into_inner()).identity.clone();
        if cfg.identity.is_empty() && !current.is_empty() {
            cfg.identity = current;
        }
        *self.cfg.write().unwrap_or_else(|e| e.into_inner()) = cfg;
        {
            let mut play = self.play.lock().unwrap_or_else(|e| e.into_inner());
            play.volume = volume;
            play.repeat = normalize_repeat(&row.repeat_mode);
        }
        if reconnect {
            self.generation.fetch_add(1, Ordering::SeqCst);
            self.linked.store(false, Ordering::SeqCst);
        } else if self.linked.load(Ordering::SeqCst) {
            self.push_cmd(VoiceCmd::Profile);
        }
        self.wake.notify_all();
    }

    fn park(&self, stop: &AtomicBool, dur: Duration) {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let guard = self.wake_mu.lock().unwrap_or_else(|e| e.into_inner());
        let _ = self.wake.wait_timeout(guard, dur);
    }

    fn set_status(&self, status: &'static str, error: &str, channel: String) {
        let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        live.status = status;
        live.error = clip(error, 180);
        live.channel = clip(&channel, 120);
    }

    fn set_now(&self, now: &str, mode: Mode) {
        self.live.lock().unwrap_or_else(|e| e.into_inner()).now = clip(now, 180);
        let _ = mode;
    }

    fn set_now_mode(&self, mode: Mode) {
        let _ = mode;
    }

    fn snapshot(&self) -> Snap {
        let live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        let play = self.play.lock().unwrap_or_else(|e| e.into_inner());
        Snap {
            status: live.status,
            error: live.error.clone(),
            channel: live.channel.clone(),
            channel_id: live.channel_id,
            now: live.now.clone(),
            station_name: live.station_name.clone(),
            mode: play.mode,
            volume: play.volume,
            repeat: play.repeat.clone(),
            queue: play.queue.iter().cloned().collect(),
            current: play.current.clone(),
            tree: live.tree.clone(),
            people: live.people.clone(),
            log: live.log.iter().cloned().collect(),
            can_kick: live.can_kick,
            can_move: live.can_move,
            can_mute: live.can_mute,
            perms_known: live.perms_known,
        }
    }

    fn command(&self, cmd: Command) -> Option<String> {
        match cmd {
            Command::Play(_) => Some(self.play_query()),
            Command::Queue => Some(self.queue_text()),
            Command::Skip => {
                self.skip();
                Some("Skipped.".into())
            }
            Command::Stop => {
                self.stop_queue();
                Some("Stopped.".into())
            }
            Command::Np => Some(self.now_text()),
            Command::Vol(Vol::Show) => Some(format!("Volume is {}.", self.play.lock().unwrap_or_else(|e| e.into_inner()).volume)),
            Command::Vol(Vol::Set(n)) => Some(format!("Volume set to {n}.")),
            Command::Vol(Vol::Invalid) => Some("Usage: !vol 0-100".into()),
            Command::Radio => {
                self.back_to_radio();
                Some("Back on the station.".into())
            }
            Command::Help => {
                let prefix = self.cfg.read().unwrap_or_else(|e| e.into_inner()).prefix.clone();
                Some(format!("{prefix}play <title>, {prefix}queue, {prefix}skip, {prefix}stop, {prefix}np, {prefix}vol <0-100>, {prefix}radio, {prefix}help"))
            }
        }
    }

    fn play_query(&self) -> String {
        let mut play = self.play.lock().unwrap_or_else(|e| e.into_inner());
        if play.mode == Mode::Paused {
            play.mode = Mode::Queue;
            return "Resumed.".into();
        }
        if play.current.is_some() || !play.queue.is_empty() {
            play.mode = Mode::Queue;
            return "Playing the queue.".into();
        }
        "Usage: !play <title>".into()
    }

    fn enqueue(&self, hub: &Hub, body: &Value) -> Result<(), Error> {
        if let Some(id) = body.get("media_id").and_then(|v| v.as_str()) {
            let cfg = self.cfg.read().unwrap_or_else(|e| e.into_inner()).clone();
            let media = hub.store.media(id)?.ok_or(Error::NotFound)?;
            if media.station_id != cfg.station_id {
                return Err(Error::NotFound);
            }
            self.push_track(track_from(&media), true)?;
            return Ok(());
        }
        let query = body.get("query").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        if query.is_empty() {
            self.play_query();
            return Ok(());
        }
        let found = self.find(hub, &query).ok_or_else(|| Error::NotFound)?;
        self.push_track(found, true)?;
        Ok(())
    }

    fn command_store(&self, hub: &Hub, cmd: Command) -> Option<String> {
        match cmd {
            Command::Play(query) if !query.is_empty() => {
                let Some(track) = self.find(hub, &query) else {
                    return Some("No matching track in the library.".into());
                };
                let label = track.label();
                if self.push_track(track, false).is_err() {
                    return Some("Queue is full.".into());
                }
                Some(format!("Queued {label}"))
            }
            Command::Vol(Vol::Set(n)) => {
                if self.set_volume(&hub.store, n).is_err() {
                    return Some("Volume did not save.".into());
                }
                Some(format!("Volume set to {n}."))
            }
            other => self.command(other),
        }
    }

    fn find(&self, hub: &Hub, query: &str) -> Option<Track> {
        let station = self.cfg.read().unwrap_or_else(|e| e.into_inner()).station_id.clone();
        let q = query.to_lowercase();
        let rows = hub.store.list_media(&station).ok()?;
        let mut best: Option<(u8, store::Media)> = None;
        for row in rows {
            let title = row.title.to_lowercase();
            let blob = format!("{} {} {}", row.title, row.artist, row.filename).to_lowercase();
            if !blob.contains(&q) && row.id != query {
                continue;
            }
            let rank = if row.id == query || title == q { 0 } else if title.starts_with(&q) { 1 } else { 2 };
            if best.as_ref().map(|(r, _)| rank < *r).unwrap_or(true) {
                best = Some((rank, row));
            }
        }
        best.map(|(_, row)| track_from(&row))
    }

    fn push_track(&self, track: Track, strict: bool) -> Result<(), Error> {
        let mut play = self.play.lock().unwrap_or_else(|e| e.into_inner());
        if play.queue.len() >= MAX_QUEUE {
            return Err(Error::Invalid("queue is full".into()));
        }
        play.queue.push_back(track);
        if play.mode != Mode::Paused {
            play.mode = Mode::Queue;
        }
        let _ = strict;
        Ok(())
    }

    fn pause(&self) {
        let mut play = self.play.lock().unwrap_or_else(|e| e.into_inner());
        if play.mode == Mode::Queue {
            play.mode = Mode::Paused;
        }
    }

    fn skip(&self) {
        let mut play = self.play.lock().unwrap_or_else(|e| e.into_inner());
        if play.mode == Mode::Radio {
            return;
        }
        play.skip = true;
        if play.mode == Mode::Paused || play.mode == Mode::Stopped {
            play.current = None;
            play.mode = if play.queue.is_empty() { Mode::Stopped } else { Mode::Queue };
        }
    }

    fn take_skip(&self) -> bool {
        let mut play = self.play.lock().unwrap_or_else(|e| e.into_inner());
        let skip = play.skip;
        play.skip = false;
        skip
    }

    fn stop_queue(&self) {
        let mut play = self.play.lock().unwrap_or_else(|e| e.into_inner());
        play.mode = Mode::Stopped;
    }

    fn clear_queue(&self) {
        let mut play = self.play.lock().unwrap_or_else(|e| e.into_inner());
        play.queue.clear();
        play.current = None;
        play.skip = true;
        play.mode = Mode::Stopped;
    }

    fn back_to_radio(&self) {
        let mut play = self.play.lock().unwrap_or_else(|e| e.into_inner());
        play.mode = Mode::Radio;
    }

    fn finish_current(&self) {
        let mut play = self.play.lock().unwrap_or_else(|e| e.into_inner());
        play.current = None;
    }

    fn advance_after_track(&self) {
        let mut play = self.play.lock().unwrap_or_else(|e| e.into_inner());
        match play.repeat.as_str() {
            "one" => {}
            "all" => {
                if let Some(cur) = play.current.take() {
                    play.queue.push_back(cur);
                }
            }
            _ => play.current = None,
        }
    }

    fn reorder(&self, ids: &[String]) {
        let mut play = self.play.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = VecDeque::new();
        for id in ids {
            if let Some(pos) = play.queue.iter().position(|t| t.media_id == *id) {
                let track = play.queue.remove(pos).unwrap();
                next.push_back(track);
            }
        }
        for track in play.queue.drain(..) {
            next.push_back(track);
        }
        play.queue = next;
    }

    fn set_current(&self, track: Track) {
        self.play.lock().unwrap_or_else(|e| e.into_inner()).current = Some(track);
    }

    fn next_track(&self) -> Option<Track> {
        let mut play = self.play.lock().unwrap_or_else(|e| e.into_inner());
        if play.current.is_some() {
            return play.current.clone();
        }
        let track = play.queue.pop_front()?;
        play.current = Some(track.clone());
        Some(track)
    }

    fn set_volume(&self, store: &Store, volume: u8) -> Result<(), Error> {
        self.set_volume_local(volume)?;
        store.set_output_volume(&self.id, i64::from(volume))?;
        Ok(())
    }

    fn set_volume_local(&self, volume: u8) -> Result<(), Error> {
        self.play.lock().unwrap_or_else(|e| e.into_inner()).volume = volume;
        self.cfg.write().unwrap_or_else(|e| e.into_inner()).volume = volume;
        Ok(())
    }

    fn queue_text(&self) -> String {
        let play = self.play.lock().unwrap_or_else(|e| e.into_inner());
        if play.queue.is_empty() && play.current.is_none() {
            return "Queue is empty.".into();
        }
        let mut lines = Vec::new();
        if let Some(cur) = &play.current {
            lines.push(format!("Now: {}", cur.label()));
        }
        for (i, track) in play.queue.iter().take(8).enumerate() {
            lines.push(format!("{}. {}", i + 1, track.label()));
        }
        lines.join(" | ")
    }

    fn now_text(&self) -> String {
        let live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        if live.now.is_empty() { "Nothing playing.".into() } else { live.now.clone() }
    }
}

struct Snap {
    status: &'static str,
    error: String,
    channel: String,
    channel_id: u64,
    now: String,
    station_name: String,
    mode: Mode,
    volume: u8,
    repeat: String,
    queue: Vec<Track>,
    current: Option<Track>,
    tree: Vec<TreeChan>,
    people: Vec<Person>,
    log: Vec<String>,
    can_kick: bool,
    can_move: bool,
    can_mute: bool,
    perms_known: bool,
}

fn cfg_from(row: &Output) -> Cfg {
    Cfg {
        station_id: row.station_id.clone(),
        mount_id: row.mount_id.clone(),
        host: row.host.clone(),
        voice_port: row.voice_port.clamp(1, 65535) as u16,
        server_password: row.server_password.clone(),
        nickname: row.nickname.clone(),
        phonetic: row.phonetic.clone(),
        away: row.away.clone(),
        channel_id: row.default_channel_id.and_then(|n| if n > 0 { Some(n as u64) } else { None }),
        channel_password: row.channel_password.clone(),
        identity: row.identity.clone(),
        volume: row.volume.clamp(0, 100) as u8,
        enabled: row.enabled,
        auto_reconnect: row.auto_reconnect,
        prefix: if row.prefix.is_empty() { "!".into() } else { row.prefix.clone() },
        commands: row.commands.clone(),
        gate: row.gate.clone(),
        gate_group: row.gate_group.clone(),
    }
}

fn normalize_repeat(mode: &str) -> String {
    match mode {
        "one" | "all" => mode.to_string(),
        _ => "off".into(),
    }
}

fn enabled_commands(raw: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for name in raw.split(',') {
        let name = name.trim();
        if tscmd::COMMANDS.contains(&name) {
            out.push(name);
        }
    }
    out
}

fn public_json(store: &Store, row: &Output, snap: Option<&Snap>) -> Value {
    let snap = snap.cloned();
    let status = snap.as_ref().map(|s| s.status).unwrap_or("disconnected");
    let volume = snap.as_ref().map(|s| s.volume).unwrap_or(row.volume.clamp(0, 100) as u8);
    let mut queue = Vec::new();
    if let Some(snap) = &snap {
        if let Some(cur) = &snap.current {
            queue.push(json!({"media_id": cur.media_id, "title": cur.title, "artist": cur.artist, "current": true}));
        }
        for track in &snap.queue {
            queue.push(json!({"media_id": track.media_id, "title": track.title, "artist": track.artist, "current": false}));
        }
    }
    let station_name = store.station(&row.station_id).ok().flatten().map(|s| s.name).unwrap_or_default();
    let reason = |ok: bool, known: bool| {
        if !known { "waiting for the server to send permissions" } else if ok { "" } else { "the bot does not have this permission" }
    };
    let known = snap.as_ref().map(|s| s.perms_known).unwrap_or(false);
    let can_kick = snap.as_ref().map(|s| s.can_kick).unwrap_or(false);
    let can_move = snap.as_ref().map(|s| s.can_move).unwrap_or(false);
    let can_mute = snap.as_ref().map(|s| s.can_mute).unwrap_or(false);
    let people = snap.as_ref().map(|s| s.people.iter().map(|p| json!({
        "id": p.id, "name": p.name, "muted": p.muted,
        "kick_ok": can_kick, "kick_reason": reason(can_kick, known),
        "move_ok": can_move, "move_reason": reason(can_move, known),
        "mute_ok": can_mute, "mute_reason": reason(can_mute, known),
    })).collect::<Vec<_>>()).unwrap_or_default();
    let tree = snap.as_ref().map(|s| s.tree.iter().map(|c| json!({
        "id": c.id, "parent": c.parent, "name": c.name, "clients": c.clients, "locked": c.locked,
    })).collect::<Vec<_>>()).unwrap_or_default();
    let commands: Vec<&str> = row.commands.split(',').map(str::trim).filter(|s| tscmd::COMMANDS.contains(s)).collect();
    json!({
        "id": row.id,
        "station_id": row.station_id,
        "station_name": station_name,
        "mount_id": row.mount_id,
        "kind": "teamspeak",
        "host": row.host,
        "voice_port": row.voice_port,
        "server_password_set": !row.server_password.is_empty(),
        "nickname": row.nickname,
        "phonetic": row.phonetic,
        "away": row.away,
        "default_channel_id": row.default_channel_id,
        "channel_password_set": !row.channel_password.is_empty(),
        "volume": volume,
        "enabled": row.enabled,
        "auto_reconnect": row.auto_reconnect,
        "prefix": row.prefix,
        "commands": commands,
        "gate": row.gate,
        "gate_group": row.gate_group,
        "repeat": snap.as_ref().map(|s| s.repeat.as_str()).unwrap_or(row.repeat_mode.as_str()),
        "identity_ready": !row.identity.is_empty(),
        "status": status,
        "error": snap.as_ref().map(|s| s.error.as_str()).unwrap_or(""),
        "current_channel": snap.as_ref().map(|s| s.channel.as_str()).unwrap_or(""),
        "current_channel_id": snap.as_ref().map(|s| s.channel_id).unwrap_or(0),
        "now_playing": snap.as_ref().map(|s| s.now.as_str()).unwrap_or(""),
        "mode": snap.as_ref().map(|s| s.mode.as_str()).unwrap_or("radio"),
        "queue": queue,
        "channels": tree,
        "people": people,
        "log": snap.as_ref().map(|s| s.log.clone()).unwrap_or_default(),
    })
}

impl Clone for Snap {
    fn clone(&self) -> Self {
        Self {
            status: self.status,
            error: self.error.clone(),
            channel: self.channel.clone(),
            channel_id: self.channel_id,
            now: self.now.clone(),
            station_name: self.station_name.clone(),
            mode: self.mode,
            volume: self.volume,
            repeat: self.repeat.clone(),
            queue: self.queue.clone(),
            current: self.current.clone(),
            tree: self.tree.clone(),
            people: self.people.clone(),
            log: self.log.clone(),
            can_kick: self.can_kick,
            can_move: self.can_move,
            can_mute: self.can_mute,
            perms_known: self.perms_known,
        }
    }
}

fn track_from(media: &store::Media) -> Track {
    let title = if media.title.is_empty() { media.filename.clone() } else { media.title.clone() };
    Track { media_id: media.id.clone(), title, artist: media.artist.clone(), mime: media.mime.clone() }
}

fn connect_changed(before: &Output, after: &Output) -> bool {
    before.host != after.host || before.voice_port != after.voice_port || before.server_password != after.server_password
}

fn blank_output() -> Output {
    Output {
        id: auth::new_id(),
        station_id: String::new(),
        mount_id: String::new(),
        host: String::new(),
        voice_port: 9987,
        server_password: String::new(),
        nickname: String::new(),
        phonetic: String::new(),
        away: String::new(),
        channel_password: String::new(),
        default_channel_id: None,
        identity: String::new(),
        volume: 80,
        enabled: false,
        auto_reconnect: true,
        prefix: "!".into(),
        commands: "play,queue,skip,stop,np,vol,radio,help".into(),
        gate: "everyone".into(),
        gate_group: String::new(),
        repeat_mode: "off".into(),
        created_at: store::now_ms(),
        user_id: String::new(),
    }
}

fn small_id(body: &Value, key: &str) -> Result<u16, Error> {
    let n = body.get(key).and_then(|v| v.as_u64()).ok_or_else(|| Error::Invalid(format!("{key} is required")))?;
    u16::try_from(n).map_err(|_| Error::Invalid(format!("{key} is invalid")))
}

fn apply_body(store: &Store, row: &mut Output, body: &Value, create: bool) -> Result<(), Error> {
    if let Some(kind) = body.get("kind").and_then(|v| v.as_str()) {
        if kind != "teamspeak" {
            return Err(Error::Invalid("only teamspeak outputs are supported".into()));
        }
    }
    if let Some(station_id) = body.get("station_id").and_then(|v| v.as_str()) {
        if station_id.is_empty() {
            row.station_id.clear();
            row.mount_id.clear();
        } else {
            let station = store.station(station_id)?.ok_or(Error::NotFound)?;
            row.station_id = station.id;
            if row.mount_id.is_empty() || store.mount(&row.mount_id)?.map(|m| m.station_id != row.station_id).unwrap_or(true) {
                row.mount_id = store.list_mounts(Some(&row.station_id))?.into_iter().next().map(|m| m.id).unwrap_or_default();
            }
        }
    }
    let host = match body.get("host").and_then(|v| v.as_str()) {
        Some(host) => host.trim().to_string(),
        None if !create => row.host.clone(),
        None => return Err(Error::Invalid("host is required".into())),
    };
    if !valid_host(&host) {
        return Err(Error::Invalid("host is invalid".into()));
    }
    row.host = host;
    if body.get("voice_port").is_some() || create {
        let port = body.get("voice_port").and_then(|v| v.as_u64()).unwrap_or(9987);
        if !(1..=65535).contains(&port) {
            return Err(Error::Invalid("voice port must be 1 to 65535".into()));
        }
        row.voice_port = port as i64;
    }
    let nick = match body.get("nickname").and_then(|v| v.as_str()) {
        Some(nick) => nick,
        None if !create => row.nickname.as_str(),
        None => return Err(Error::Invalid("nickname is required".into())),
    };
    row.nickname = clean_nick(nick)?;
    if let Some(phonetic) = body.get("phonetic").and_then(|v| v.as_str()) {
        row.phonetic = clean_text(phonetic, 30)?;
    }
    if let Some(away) = body.get("away").and_then(|v| v.as_str()) {
        row.away = clean_text(away, 80)?;
    }
    row.server_password = take_secret(body, "server_password", &row.server_password)?;
    row.channel_password = take_secret(body, "channel_password", &row.channel_password)?;
    if body.get("default_channel_id").is_some() {
        row.default_channel_id = take_channel_id(body, "default_channel_id", row.default_channel_id)?;
    }
    if let Some(volume) = body.get("volume") {
        let volume = volume.as_u64().ok_or_else(|| Error::Invalid("volume must be 0 to 100".into()))?;
        if volume > 100 {
            return Err(Error::Invalid("volume must be 0 to 100".into()));
        }
        row.volume = volume as i64;
    }
    if let Some(enabled) = body.get("enabled") {
        row.enabled = enabled.as_bool().ok_or_else(|| Error::Invalid("enabled must be true or false".into()))?;
    }
    if let Some(on) = body.get("auto_reconnect") {
        row.auto_reconnect = on.as_bool().ok_or_else(|| Error::Invalid("auto_reconnect must be true or false".into()))?;
    }
    if let Some(prefix) = body.get("prefix").and_then(|v| v.as_str()) {
        row.prefix = clean_prefix(prefix)?;
    }
    if let Some(commands) = body.get("commands") {
        row.commands = clean_commands(commands)?;
    }
    if let Some(gate) = body.get("gate").and_then(|v| v.as_str()) {
        if gate != "everyone" && gate != "group" {
            return Err(Error::Invalid("gate must be everyone or group".into()));
        }
        row.gate = gate.to_string();
    }
    if let Some(group) = body.get("gate_group").and_then(|v| v.as_str()) {
        row.gate_group = clean_text(group, 40)?;
    }
    if let Some(mode) = body.get("repeat").and_then(|v| v.as_str()) {
        row.repeat_mode = normalize_repeat(mode);
    }
    Ok(())
}

fn clean_prefix(prefix: &str) -> Result<String, Error> {
    let prefix = prefix.trim();
    if prefix.is_empty() || prefix.chars().count() > 3 || prefix.chars().any(|c| c.is_whitespace() || c.is_control() || c.is_alphanumeric()) {
        return Err(Error::Invalid("prefix must be one to three marks".into()));
    }
    Ok(prefix.to_string())
}

fn clean_commands(value: &Value) -> Result<String, Error> {
    let names: Vec<String> = match value {
        Value::Array(items) => items.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect(),
        Value::String(s) => s.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
        _ => return Err(Error::Invalid("commands are invalid".into())),
    };
    let mut out = Vec::new();
    for name in names {
        if tscmd::COMMANDS.contains(&name.as_str()) && !out.contains(&name) {
            out.push(name);
        }
    }
    Ok(out.join(","))
}

fn take_secret(body: &Value, key: &str, current: &str) -> Result<String, Error> {
    match body.get(key) {
        None => Ok(current.to_string()),
        Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) => {
            if s.chars().count() > 128 {
                return Err(Error::Invalid("password is too long".into()));
            }
            if s.chars().any(|c| c.is_control()) {
                return Err(Error::Invalid("password is invalid".into()));
            }
            Ok(s.clone())
        }
        _ => Err(Error::Invalid("password is invalid".into())),
    }
}

fn take_channel_id(body: &Value, key: &str, current: Option<i64>) -> Result<Option<i64>, Error> {
    match body.get(key) {
        None => Ok(current),
        Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::Number(n)) => {
            let v = n.as_i64().ok_or_else(|| Error::Invalid("channel id is invalid".into()))?;
            if v <= 0 { Ok(None) } else { Ok(Some(v)) }
        }
        Some(Value::String(s)) => {
            let v: i64 = s.trim().parse().map_err(|_| Error::Invalid("channel id is invalid".into()))?;
            if v <= 0 { Ok(None) } else { Ok(Some(v)) }
        }
        _ => Err(Error::Invalid("channel id is invalid".into())),
    }
}

fn valid_host(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    !host.chars().any(|c| c.is_control() || matches!(c, ' ' | '/' | '\\' | '@' | '?' | '#' | ','))
}

fn clean_nick(nick: &str) -> Result<String, Error> {
    let nick: String = nick.chars().filter(|c| !c.is_control()).take(30).collect();
    let nick = nick.trim().to_string();
    if nick.is_empty() {
        return Err(Error::Invalid("nickname is required".into()));
    }
    Ok(nick)
}

fn clean_text(text: &str, max: usize) -> Result<String, Error> {
    if text.chars().any(|c| c.is_control()) {
        return Err(Error::Invalid("text is invalid".into()));
    }
    let text = text.trim();
    if text.chars().count() > max {
        return Err(Error::Invalid("text is too long".into()));
    }
    Ok(text.to_string())
}

fn load_identity(store: &Store, shared: &Shared) -> Result<Identity, String> {
    let stored = shared.cfg.read().unwrap_or_else(|e| e.into_inner()).identity.clone();
    if !stored.is_empty() {
        if let Ok(mut id) = serde_json::from_str::<Identity>(&stored) {
            if id.level() < 8 {
                id.upgrade_level(8);
                persist_identity(store, shared, &id)?;
            }
            return Ok(id);
        }
    }
    let id = Identity::create();
    persist_identity(store, shared, &id)?;
    Ok(id)
}

fn persist_identity(store: &Store, shared: &Shared, id: &Identity) -> Result<(), String> {
    let text = serde_json::to_string(id).map_err(|_| "identity could not be stored".to_string())?;
    store.set_output_identity(&shared.id, &text).map_err(|_| "identity could not be stored".to_string())?;
    shared.cfg.write().unwrap_or_else(|e| e.into_inner()).identity = text;
    Ok(())
}

fn quiet_logger() -> slog::Logger {
    slog::Logger::root(slog::Discard, slog::o!())
}

fn soften_bot_thread() {
    #[cfg(unix)]
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        libc::setpriority(libc::PRIO_PROCESS, tid, 5);
    }
}

/// Resolve on a thread this bot owns, then give up. A stuck `getaddrinfo` is
/// left behind and is not joined from the desk or the mount pump.
fn resolve_with_timeout(host: &str, port: u16) -> Result<SocketAddr, String> {
    let inflight = DNS_INFLIGHT.fetch_add(1, Ordering::SeqCst);
    if inflight >= 4 {
        DNS_INFLIGHT.fetch_sub(1, Ordering::SeqCst);
        return Err("timed out resolving the server".into());
    }
    let host = host.to_string();
    let (tx, rx) = mpsc::channel();
    let spawned = thread::Builder::new().name("ts-dns".into()).spawn(move || {
        let found = (host.as_str(), port).to_socket_addrs().ok().and_then(|mut it| it.next());
        let _ = tx.send(found);
        DNS_INFLIGHT.fetch_sub(1, Ordering::SeqCst);
    });
    if spawned.is_err() {
        DNS_INFLIGHT.fetch_sub(1, Ordering::SeqCst);
        return Err("could not resolve the server".into());
    }
    match rx.recv_timeout(DNS_TIMEOUT) {
        Ok(Some(addr)) => Ok(addr),
        Ok(None) => Err("could not resolve the server".into()),
        Err(_) => Err("timed out resolving the server".into()),
    }
}

fn public_failure(err: &str) -> &'static str {
    let low = err.to_lowercase();
    if low.contains("password") {
        "failed password"
    } else if low.contains("timed out") || low.contains("timeout") {
        "timed out connecting"
    } else {
        "could not connect"
    }
}

fn redact(err: &str, cfg: &Cfg) -> String {
    let mut text: String = err.chars().filter(|c| !c.is_control()).take(400).collect();
    for secret in [&cfg.server_password, &cfg.channel_password, &cfg.identity] {
        if secret.len() >= 4 {
            text = text.replace(secret.as_str(), "••••");
        }
    }
    clip(&text, 180)
}

fn clip(text: &str, max: usize) -> String {
    text.chars().filter(|c| !c.is_control()).take(max).collect()
}

fn clip_log(text: &str) -> String {
    clip(text, 60)
}
