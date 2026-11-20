//! One mount, one encoded byte stream, many listeners.
//!
//! AutoDJ and the live source meet in a short PCM mixer (crossfade) when the
//! factory speaks PCM. Otherwise the mount switches pre-encoded bytes.
//! Listeners sit on the fanout and stay connected across the switch.

use crate::auth;
use crate::encoder::{AudioFactory, SILENCE_MP3};
use crate::error::Error;
use crate::hls::{self, Hls};
use crate::icy::MetaWriter;
use crate::limit::{FailWindow, IpCounter};
use crate::pcm::{self, Mixer, PcmQueue};
use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepeatMode {
    Off,
    One,
    All,
}

impl RepeatMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "off" => Some(Self::Off),
            "one" => Some(Self::One),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::One => "one",
            Self::All => "all",
        }
    }
}

/// Buffer and HLS windows stay short so a bad setting cannot stall the fanout.
pub fn clamp_bitrate(v: u32) -> u32 {
    v.clamp(32_000, 320_000)
}

pub fn clamp_sample_rate(v: u32) -> u32 {
    const RATES: [u32; 7] = [8_000, 11_025, 16_000, 22_050, 32_000, 44_100, 48_000];
    if v == 0 {
        return 44_100;
    }
    RATES.iter().copied().min_by_key(|r| r.abs_diff(v)).unwrap_or(44_100)
}

pub fn clamp_channels(v: u16) -> u16 {
    if v == 1 { 1 } else { 2 }
}

pub fn clamp_crossfade_ms(v: u64) -> u64 {
    v.min(10_000)
}

pub fn clamp_buffer_ms(v: u64) -> u64 {
    v.clamp(50, 1_000)
}

pub fn clamp_hls_segment_ms(v: u64) -> u64 {
    v.clamp(250, 2_000)
}

pub fn clamp_gain_db(v: f64) -> f64 {
    v.clamp(-24.0, 24.0)
}

pub fn buffer_bytes(sample_rate: u32, channels: u16, buffer_ms: u64) -> usize {
    let ms = if buffer_ms == 0 { 250 } else { clamp_buffer_ms(buffer_ms) };
    let sr = sample_rate.max(8_000) as u64;
    let ch = channels.max(1) as u64;
    (sr * ch * 2 * ms / 1_000).max(ch * 2) as usize
}

#[derive(Clone, Debug)]
pub struct MountSpec {
    pub id: String,
    pub station_id: String,
    pub station_name: String,
    pub genre: String,
    pub path: String,
    pub source_user: String,
    pub pass_hash: String,
    pub format: String,
    pub bitrate: u32,
    pub max_listeners: usize,
    pub max_bitrate: u32,
    pub allow_takeover: bool,
    pub crossfade_ms: u64,
    pub autodj: bool,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub jingle_every: u32,
    pub jingle_media_id: Option<String>,
    pub intro_media_id: Option<String>,
    pub gain_db: f64,
    /// 0 keeps the engine default from startup.
    pub sample_rate: u32,
    /// 0 keeps the engine default from startup.
    pub channels: u16,
    /// 0 means the low default (250 ms).
    pub buffer_ms: u64,
    /// 0 means the low default (1 s).
    pub hls_segment_ms: u64,
    /// `ffmpeg`, `native`, or `opus`.
    pub encoder: String,
    /// Empty disables the relay. http or https only.
    pub relay_url: String,
}

#[derive(Clone, Debug)]
pub struct NowPlaying {
    pub title: String,
    pub artist: String,
    pub source: String,
    pub url: String,
    pub elapsed_ms: u64,
    pub position_ms: u64,
    pub duration_ms: u64,
    pub media_id: String,
}

impl NowPlaying {
    pub fn new(title: impl Into<String>, artist: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            artist: artist.into(),
            source: source.into(),
            url: String::new(),
            elapsed_ms: 0,
            position_ms: 0,
            duration_ms: 0,
            media_id: String::new(),
        }
    }
    fn autodj() -> Self { Self::new("AutoDJ", "", "autodj") }
    pub fn label(&self) -> String {
        if self.artist.is_empty() { self.title.clone() } else { format!("{} - {}", self.artist, self.title) }
    }
}

#[derive(Clone, Debug)]
pub struct Track {
    pub title: String,
    pub artist: String,
    pub path: PathBuf,
    pub mime: String,
    pub media_id: String,
    pub duration_ms: u64,
}

#[derive(Clone, Debug, Default)]
pub struct PlayHead {
    pub elapsed_ms: u64,
    pub duration_ms: u64,
    pub title: String,
    pub artist: String,
    pub media_id: String,
}

#[derive(Clone, Debug, Default)]
pub struct QueueItem {
    pub index: usize,
    pub media_id: String,
    pub title: String,
    pub artist: String,
    pub duration_ms: u64,
}

#[derive(Clone, Debug, Default)]
pub struct QueueSnapshot {
    pub now: Option<QueueItem>,
    pub elapsed_ms: u64,
    pub duration_ms: u64,
    pub upcoming: Vec<QueueItem>,
    pub history: Vec<QueueItem>,
}

pub trait TrackSource: Send + Sync {
    fn next(&self, mount_id: &str) -> Option<Track>;
    fn play_generation(&self, _mount_id: &str) -> u64 { 0 }
    fn play_head(&self, _mount_id: &str) -> Option<PlayHead> { None }
    fn queue_snapshot(&self, _mount_id: &str) -> Result<QueueSnapshot, Error> { Ok(QueueSnapshot::default()) }
    fn skip(&self, _mount_id: &str) -> Result<(), Error> { Err(Error::NotFound) }
    fn previous(&self, _mount_id: &str) -> Result<(), Error> { Err(Error::NotFound) }
    fn queue_next(&self, _mount_id: &str, _media_id: &str) -> Result<(), Error> { Err(Error::NotFound) }
    fn queue_move(&self, _mount_id: &str, _from: usize, _to: usize) -> Result<(), Error> { Err(Error::NotFound) }
    fn queue_remove(&self, _mount_id: &str, _index: usize) -> Result<(), Error> { Err(Error::NotFound) }
    fn queue_clear(&self, _mount_id: &str) -> Result<(), Error> { Err(Error::NotFound) }
    fn seek(&self, _mount_id: &str, _position_ms: u64) -> Result<u64, Error> { Err(Error::NotFound) }
    fn jump(&self, _mount_id: &str, _media_id: &str) -> Result<(), Error> { Err(Error::NotFound) }
    fn play_next(&self, _mount_id: &str, _media_id: &str) -> Result<(), Error> { Err(Error::NotFound) }
    /// Queue or jump a file that is not a library row. Recordings use this.
    fn play_file(&self, _mount_id: &str, _track: Track, _jump: bool) -> Result<(), Error> { Err(Error::NotFound) }
    fn seek_generation(&self, _mount_id: &str) -> u64 { 0 }
    fn seek_offset_ms(&self, _mount_id: &str) -> u64 { 0 }
}

pub struct NoTracks;
impl TrackSource for NoTracks {
    fn next(&self, _: &str) -> Option<Track> { None }
}

#[derive(Clone, Debug)]
pub struct SourceMeta {
    pub content_type: String,
    pub name: String,
    pub url: String,
    pub bitrate_bps: Option<u32>,
    pub inline_metaint: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct ListenerRow {
    pub id: u64,
    pub ip: String,
    pub connected_ms: u64,
    pub bytes: u64,
    pub user_agent: String,
}

#[derive(Clone, Debug)]
pub struct MountStatus {
    pub id: String,
    pub station_id: String,
    pub station_name: String,
    pub path: String,
    pub live: bool,
    pub listeners: usize,
    pub bitrate: u32,
    pub now: NowPlaying,
    pub history: Vec<NowPlaying>,
    pub listener_rows: Vec<ListenerRow>,
    pub peak_listeners: usize,
    pub bytes_sent: u64,
    pub uptime_sec: u64,
    pub mode: String,
    /// True only while encoded bytes are actually reaching the fanout.
    pub playing: bool,
    pub position_ms: u64,
    pub duration_ms: u64,
    pub gain_db: f64,
    pub sample_rate: u32,
    pub channels: u16,
    pub crossfade_ms: u64,
    pub buffer_ms: u64,
    pub hls_segment_ms: u64,
    pub autodj: bool,
    pub allow_takeover: bool,
    pub shuffle: bool,
    pub repeat: String,
    pub format: String,
    pub encoder: String,
    pub relay_url: String,
    pub recording: bool,
    pub recording_error: String,
    pub recording_id: String,
    pub recording_bytes: u64,
    pub recording_ms: u64,
}

#[derive(Clone)]
pub struct EngineOpts {
    pub sample_rate: u32,
    pub channels: u16,
    pub frame: Duration,
    pub fade: Duration,
    pub meta_int: usize,
    pub max_conns_per_ip: usize,
    pub media_root: PathBuf,
    pub fast: bool,
}

impl EngineOpts {
    pub fn production(media_root: PathBuf, meta_int: usize, max_ip: usize, fade: Duration) -> Self {
        Self {
            sample_rate: 44_100,
            channels: 2,
            frame: Duration::from_millis(20),
            fade,
            meta_int,
            max_conns_per_ip: max_ip.max(1),
            media_root,
            fast: false,
        }
    }
}

pub struct Engine {
    mounts: Mutex<HashMap<String, Arc<Runtime>>>,
    ips: IpCounter,
    factory: Arc<dyn AudioFactory>,
    tracks: Arc<dyn TrackSource>,
    opts: EngineOpts,
    dummy: String,
    started: Instant,
    halt: Arc<AtomicBool>,
    stream_public_url: RwLock<String>,
    trusted: RwLock<crate::limit::Trusted>,
    relay_allow_private: Arc<AtomicBool>,
    deny_relay: Arc<Mutex<Vec<std::net::SocketAddr>>>,
    source_fails: FailWindow,
    recordings_root: RwLock<PathBuf>,
    record_hook: RwLock<Option<crate::record::FinishHook>>,
    recording_budgets: Mutex<HashMap<String, u64>>,
    max_recording_bytes: AtomicU64,
}

impl Engine {
    pub fn new(factory: Arc<dyn AudioFactory>, tracks: Arc<dyn TrackSource>, opts: EngineOpts) -> Self {
        let dummy = auth::hash_with("timing-pad", auth::HashParams::fast()).unwrap_or_default();
        Self {
            mounts: Mutex::new(HashMap::new()),
            ips: IpCounter::new(opts.max_conns_per_ip),
            factory,
            tracks,
            opts,
            dummy,
            started: Instant::now(),
            halt: Arc::new(AtomicBool::new(false)),
            stream_public_url: RwLock::new(String::new()),
            trusted: RwLock::new(crate::limit::Trusted::default()),
            relay_allow_private: Arc::new(AtomicBool::new(false)),
            deny_relay: Arc::new(Mutex::new(Vec::new())),
            source_fails: FailWindow::new(8, Duration::from_secs(60)),
            recordings_root: RwLock::new(PathBuf::new()),
            record_hook: RwLock::new(None),
            recording_budgets: Mutex::new(HashMap::new()),
            max_recording_bytes: AtomicU64::new(crate::record::DEFAULT_MAX_BYTES),
        }
    }

    pub fn set_recordings_root(&self, path: PathBuf) {
        *self.recordings_root.write().expect("recordings") = path.clone();
        for rt in self.mounts.lock().expect("mounts").values() {
            *rt.record_root.write().unwrap_or_else(|e| e.into_inner()) = path.clone();
        }
    }

    pub fn set_recording_cap(&self, bytes: u64) {
        let bytes = if bytes == 0 { crate::record::DEFAULT_MAX_BYTES } else { bytes };
        self.max_recording_bytes.store(bytes, Ordering::SeqCst);
    }

    pub fn set_record_hook(&self, hook: crate::record::FinishHook) {
        *self.record_hook.write().expect("record hook") = Some(Arc::clone(&hook));
        for rt in self.mounts.lock().expect("mounts").values() {
            *rt.record_hook.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&hook));
        }
    }

    /// Bytes this file may still grow, after files already on disk and other open recordings.
    pub fn reserve_recording(&self, id: &str, used_on_disk: u64) -> Result<u64, Error> {
        let cap = self.max_recording_bytes.load(Ordering::SeqCst);
        let mut guard = self.recording_budgets.lock().unwrap_or_else(|e| e.into_inner());
        let reserved: u64 = guard.values().copied().sum();
        let budget = cap.saturating_sub(used_on_disk.saturating_add(reserved));
        if budget < 4096 {
            return Err(Error::Invalid("recording storage is full".into()));
        }
        guard.insert(id.to_string(), budget);
        Ok(budget)
    }

    pub fn release_recording(&self, id: &str) {
        self.recording_budgets.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
    }

    pub fn set_stream_public(&self, url: String) {
        *self.stream_public_url.write().expect("public url") = url.trim_end_matches('/').to_string();
    }

    pub fn set_trusted(&self, trusted: crate::limit::Trusted) {
        *self.trusted.write().expect("trusted") = trusted;
    }

    pub fn set_relay_policy(&self, allow_private: bool, deny: Vec<std::net::SocketAddr>) {
        self.relay_allow_private.store(allow_private, Ordering::SeqCst);
        *self.deny_relay.lock().expect("deny") = deny;
    }

    pub fn relay_policy(&self) -> crate::relay::Policy {
        crate::relay::Policy {
            allow_private: self.relay_allow_private.load(Ordering::SeqCst),
            deny: self.deny_relay.lock().expect("deny").clone(),
        }
    }

    pub fn stream_base(&self, peer: &str, forwarded_host: Option<&str>, forwarded_proto: Option<&str>) -> String {
        let configured = self.stream_public_url.read().expect("public url").clone();
        let trusted = self.trusted.read().expect("trusted").clone();
        crate::limit::public_origin(peer, forwarded_host, forwarded_proto, &configured, &trusted)
    }

    /// Set the stop flag, close queues, and kill encoder children. Does not join.
    pub fn begin_shutdown(&self) {
        self.halt.store(true, Ordering::SeqCst);
        let runtimes: Vec<_> = self.mounts.lock().expect("mounts").values().cloned().collect();
        for rt in runtimes {
            rt.arm_stop();
        }
        crate::encoder::kill_all();
    }

    pub fn uptime_secs(&self) -> u64 { self.started.elapsed().as_secs() }
    pub fn meta_int(&self) -> usize { self.opts.meta_int }

    /// Checks listener and per-IP caps without taking a slot.
    pub fn listener_allowed(&self, path: &str, ip: &str) -> Result<(), Error> {
        let rt = self.mount(path).ok_or(Error::NotFound)?;
        if !rt.public_stream.load(Ordering::SeqCst) {
            return Err(Error::NotFound);
        }
        if !self.ips.would_allow(ip) || !rt.fan.has_room() {
            return Err(Error::Limit);
        }
        Ok(())
    }

    /// Public Icecast and HLS for one mount. Turning it off drops listeners.
    /// The mixer thread keeps running so a TeamSpeak bot can still tap PCM.
    pub fn set_path_public(&self, path: &str, on: bool) {
        let Some(rt) = self.mount(path) else { return };
        rt.public_stream.store(on, Ordering::SeqCst);
        if !on {
            rt.fan.kick_all();
        }
    }

    pub fn path_public(&self, path: &str) -> bool {
        self.mount(path).is_some_and(|rt| rt.public_stream.load(Ordering::SeqCst))
    }

    pub fn upsert(&self, spec: MountSpec) {
        let mut g = self.mounts.lock().expect("mounts");
        if let Some(existing) = g.get(&spec.path) {
            if existing.id() == spec.id {
                let old = existing.spec.read().expect("spec").clone();
                let (sr, ch, _, _) = resolve_format(&spec, existing.sample_rate, existing.channels);
                existing.fan.set_max(spec.max_listeners);
                existing.mixer.set_channels(ch);
                existing.mixer.set_fade(pcm::fade_samples(sr, Duration::from_millis(clamp_crossfade_ms(spec.crossfade_ms))));
                let buf = buffer_bytes(sr, ch, spec.buffer_ms);
                existing.auto_q.set_max(buf);
                existing.live_q.set_max(buf);
                let hls_ms = if spec.hls_segment_ms == 0 { 1_000 } else { clamp_hls_segment_ms(spec.hls_segment_ms) };
                existing.hls.lock().unwrap_or_else(|e| e.into_inner()).set_target(Duration::from_millis(hls_ms));
                let restart = old.bitrate != spec.bitrate || old.sample_rate != spec.sample_rate || old.channels != spec.channels || old.encoder != spec.encoder;
                *existing.spec.write().expect("spec") = spec;
                if restart {
                    existing.audio_run.fetch_add(1, Ordering::SeqCst);
                }
                return;
            }
        }
        let root = self.recordings_root.read().expect("recordings").clone();
        let hook = self.record_hook.read().expect("record hook").clone();
        let rt = Runtime::start(
            spec,
            &self.factory,
            &self.tracks,
            &self.opts,
            Arc::clone(&self.halt),
            Arc::clone(&self.relay_allow_private),
            Arc::clone(&self.deny_relay),
            root,
            hook,
        );
        g.insert(rt.path(), rt);
    }

    pub fn remove_path(&self, path: &str) {
        let rt = self.mounts.lock().expect("mounts").remove(path);
        if let Some(rt) = rt {
            rt.stop_join();
        }
    }

    pub fn shutdown(&self) {
        self.begin_shutdown();
        let all: Vec<_> = self.mounts.lock().expect("mounts").drain().map(|(_, rt)| rt).collect();
        for rt in all {
            rt.stop_join();
        }
    }

    pub fn paths(&self) -> Vec<String> {
        let mut v: Vec<_> = self.mounts.lock().expect("mounts").keys().cloned().collect();
        v.sort_by_key(|p| std::cmp::Reverse(p.len()));
        v
    }

    fn mount(&self, path: &str) -> Option<Arc<Runtime>> {
        self.mounts.lock().expect("mounts").get(path).cloned()
    }

    pub fn status(&self) -> Vec<MountStatus> {
        let arcs: Vec<_> = self.mounts.lock().expect("mounts").values().cloned().collect();
        let mut rows: Vec<_> = arcs.iter().map(|rt| rt.status()).collect();
        rows.sort_by(|a, b| a.path.cmp(&b.path));
        rows
    }

    /// PCM of one mount, for a voice output. The receiver is bounded.
    /// A full or dead subscriber is removed inside the pump and never blocks it.
    pub fn subscribe_pcm(&self, mount_id: &str) -> Option<Receiver<PcmChunk>> {
        let mounts = self.mounts.lock().unwrap_or_else(|e| e.into_inner());
        let rt = mounts.values().find(|rt| rt.id() == mount_id)?;
        Some(rt.tap.subscribe())
    }

    /// What that mount is putting on the air right now.
    pub fn program(&self, mount_id: &str) -> Option<NowPlaying> {
        let rt = {
            let mounts = self.mounts.lock().unwrap_or_else(|e| e.into_inner());
            mounts.values().find(|rt| rt.id() == mount_id).cloned()
        };
        // Drop the mount map before reading status. A TeamSpeak thread must not
        // hold that map across anything the desk or the pump also locks.
        rt.map(|rt| rt.status().now)
    }

    pub fn disconnect(&self, path: &str) -> Result<(), Error> {
        let rt = self.mount(path).ok_or(Error::NotFound)?;
        if let Some(shut) = rt.slot.lock().expect("slot").shutdown.take() {
            shut();
        }
        Ok(())
    }

    pub fn set_metadata(&self, path: &str, title: &str, artist: &str) -> Result<(), Error> {
        if title.chars().count() > 512 || artist.chars().count() > 512 {
            return Err(Error::Invalid("icy metadata is too large".into()));
        }
        let rt = self.mount(path).ok_or(Error::NotFound)?;
        rt.set_now(NowPlaying::new(title, artist, "live"));
        Ok(())
    }

    fn mount_id(&self, path: &str) -> Result<String, Error> {
        Ok(self.mount(path).ok_or(Error::NotFound)?.id())
    }

    pub fn queue(&self, path: &str) -> Result<QueueSnapshot, Error> {
        let id = self.mount_id(path)?;
        self.tracks.queue_snapshot(&id)
    }

    pub fn skip(&self, path: &str) -> Result<(), Error> {
        let rt = self.mount(path).ok_or(Error::NotFound)?;
        self.tracks.skip(&rt.id())?;
        rt.auto_q.drain();
        Ok(())
    }

    pub fn previous(&self, path: &str) -> Result<(), Error> {
        let rt = self.mount(path).ok_or(Error::NotFound)?;
        self.tracks.previous(&rt.id())?;
        rt.auto_q.drain();
        Ok(())
    }

    pub fn queue_next(&self, path: &str, media_id: &str) -> Result<(), Error> {
        let id = self.mount_id(path)?;
        self.tracks.queue_next(&id, media_id)
    }

    pub fn queue_move(&self, path: &str, from: usize, to: usize) -> Result<(), Error> {
        let id = self.mount_id(path)?;
        self.tracks.queue_move(&id, from, to)
    }

    pub fn queue_remove(&self, path: &str, index: usize) -> Result<(), Error> {
        let id = self.mount_id(path)?;
        self.tracks.queue_remove(&id, index)
    }

    pub fn queue_clear(&self, path: &str) -> Result<(), Error> {
        let id = self.mount_id(path)?;
        self.tracks.queue_clear(&id)
    }

    pub fn seek(&self, path: &str, position_ms: u64) -> Result<u64, Error> {
        let rt = self.mount(path).ok_or(Error::NotFound)?;
        if rt.slot.lock().unwrap_or_else(|e| e.into_inner()).active {
            return Err(Error::Invalid("cannot seek a live source".into()));
        }
        let pos = self.tracks.seek(&rt.id(), position_ms)?;
        rt.auto_q.drain();
        Ok(pos)
    }

    pub fn jump(&self, path: &str, media_id: &str) -> Result<(), Error> {
        let rt = self.mount(path).ok_or(Error::NotFound)?;
        self.tracks.jump(&rt.id(), media_id)?;
        rt.auto_q.drain();
        Ok(())
    }

    pub fn play_next(&self, path: &str, media_id: &str) -> Result<(), Error> {
        let id = self.mount_id(path)?;
        self.tracks.play_next(&id, media_id)
    }

    pub fn start_recording(&self, path: &str, job: crate::record::Job) -> Result<(), Error> {
        let rt = self.mount(path).ok_or(Error::NotFound)?;
        rt.capture.start(job).map_err(|e| {
            if e == "already recording" {
                Error::Busy
            } else if e.contains("full") {
                Error::Invalid(e)
            } else {
                Error::Other(e)
            }
        })
    }

    pub fn stop_recording(&self, path: &str) -> Result<crate::record::Finished, Error> {
        let rt = self.mount(path).ok_or(Error::NotFound)?;
        rt.capture.stop().ok_or(Error::NotFound)
    }

    pub fn recording_live(&self, path: &str) -> Option<crate::record::Live> {
        self.mount(path).and_then(|rt| rt.capture.live())
    }

    pub fn fail_recording(&self, path: &str) -> Result<(), Error> {
        let rt = self.mount(path).ok_or(Error::NotFound)?;
        rt.capture.fail_writes();
        Ok(())
    }

    pub fn note_recording_error(&self, path: &str, error: &str) {
        if let Some(rt) = self.mount(path) {
            rt.capture.note_error(error);
        }
    }

    /// Queue or jump a recording the same way a library track is queued or jumped.
    pub fn replay(&self, path: &str, track: Track, jump: bool) -> Result<(), Error> {
        let rt = self.mount(path).ok_or(Error::NotFound)?;
        let root = rt.record_root.read().unwrap_or_else(|e| e.into_inner()).clone();
        open_media(&rt.media_root, &root, &track.path).map_err(|_| Error::NotFound)?;
        self.tracks.play_file(&rt.id(), track, jump)?;
        if jump {
            rt.auto_q.drain();
        }
        Ok(())
    }

    pub fn kick(&self, path: &str, listener_id: u64) -> Result<(), Error> {
        let rt = self.mount(path).ok_or(Error::NotFound)?;
        if rt.fan.kick(listener_id) { Ok(()) } else { Err(Error::NotFound) }
    }

    pub fn kick_all(&self, path: &str) -> Result<usize, Error> {
        let rt = self.mount(path).ok_or(Error::NotFound)?;
        Ok(rt.fan.kick_all())
    }

    pub fn open_source(
        &self,
        path: &str,
        user: &str,
        pass: &str,
        meta: SourceMeta,
        shutdown: Box<dyn Fn() + Send>,
        ip: &str,
    ) -> Result<SourceGuard, Error> {
        let ip = if ip.is_empty() { "unknown" } else { ip };
        if !self.source_fails.allow(ip) {
            return Err(Error::Limit);
        }
        let Some(rt) = self.mount(path) else {
            let _ = auth::verify_password(&self.dummy, pass);
            self.source_fails.fail(ip);
            return Err(Error::NotFound);
        };
        let spec = rt.spec.read().expect("spec").clone();
        if user != spec.source_user {
            let _ = auth::verify_password(&self.dummy, pass);
            self.source_fails.fail(ip);
            return Err(Error::Unauthorized);
        }
        if !auth::verify_password(&spec.pass_hash, pass) {
            self.source_fails.fail(ip);
            return Err(Error::Unauthorized);
        }
        if let Some(br) = meta.bitrate_bps {
            if spec.max_bitrate > 0 && br > spec.max_bitrate {
                return Err(Error::Bitrate);
            }
        }
        let gen = rt.slot.lock().expect("slot").begin(shutdown, spec.allow_takeover)?;
        {
            let _guard = rt.live_lock.lock().expect("live");
            rt.live_gen.store(gen, Ordering::SeqCst);
            rt.live_q.drain();
        }
        let title = if meta.name.is_empty() { "Live".into() } else { meta.name.clone() };
        let mut now = NowPlaying::new(title, "DJ", "live");
        now.url = meta.url.clone();
        rt.set_now(now);
        *rt.live_since.lock().expect("since") = Some(Instant::now());
        Ok(SourceGuard { rt, gen, meta, ended: false })
    }

    pub fn serve_listener(&self, path: &str, ip: &str, user_agent: &str, want_meta: bool, writer: impl Write) -> Result<(), Error> {
        if !self.ips.acquire(ip) {
            return Err(Error::Limit);
        }
        let result = self.write_listener(path, ip, user_agent, want_meta, writer);
        self.ips.release(ip);
        result
    }

    fn write_listener(&self, path: &str, ip: &str, user_agent: &str, want_meta: bool, writer: impl Write) -> Result<(), Error> {
        let rt = self.mount(path).ok_or(Error::NotFound)?;
        let sub = rt.fan.subscribe(ip, user_agent)?;
        let now = Arc::clone(&rt.now);
        let mut icy = MetaWriter::new(writer, self.opts.meta_int, want_meta, move || {
            let g = now.lock().expect("now");
            (g.label(), g.url.clone())
        });
        if !sub.ring.is_empty() {
            if let Err(e) = icy.write_audio(&sub.ring) {
                rt.fan.remove(sub.id);
                return client_result(e);
            }
        }
        loop {
            match sub.rx.recv_timeout(Duration::from_millis(250)) {
                Ok(chunk) => {
                    sub.bytes.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                    if let Err(e) = icy.write_audio(&chunk) {
                        rt.fan.remove(sub.id);
                        return client_result(e);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    if rt.stop.load(Ordering::SeqCst) {
                        break;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        rt.fan.remove(sub.id);
        Ok(())
    }

    pub fn hls_playlist(&self, path: &str) -> Option<String> {
        let base = self.stream_public_url.read().expect("public url").clone();
        self.hls_playlist_for(path, &base)
    }

    pub fn hls_playlist_for(&self, path: &str, base: &str) -> Option<String> {
        self.mount(path).map(|rt| rt.hls.lock().expect("hls").playlist_at(base, path))
    }

    pub fn hls_segment(&self, path: &str, name: &str) -> Option<Vec<u8>> {
        let stem = name.strip_prefix("seg")?.strip_suffix(".ts")?;
        if stem.is_empty() || !stem.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        self.mount(path)?.hls.lock().expect("hls").segment(name)
    }
}

impl Drop for Engine {
    fn drop(&mut self) { self.shutdown(); }
}

fn client_result(err: io::Error) -> Result<(), Error> {
    match err.kind() {
        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted | io::ErrorKind::TimedOut => Ok(()),
        _ => Err(Error::Io(err)),
    }
}

pub struct SourceGuard {
    rt: Arc<Runtime>,
    gen: u64,
    meta: SourceMeta,
    ended: bool,
}

impl SourceGuard {
    pub fn run(mut self, reader: Box<dyn Read + Send>) -> io::Result<()> {
        let result = if self.rt.pcm {
            self.run_pcm(reader)
        } else {
            self.run_packet(reader)
        };
        self.finish();
        result
    }

    fn run_pcm(&self, reader: Box<dyn Read + Send>) -> io::Result<()> {
        let max_br = self.rt.spec.read().expect("spec").max_bitrate;
        let limited = Box::new(Limited::new(reader, max_br));
        let stream: Box<dyn Read + Send> = if let Some(interval) = self.meta.inline_metaint {
            let rt = Arc::clone(&self.rt);
            Box::new(crate::icy::Stripper::new(limited, interval, move |title, url| {
                    if !title.is_empty() {
                        let mut now = NowPlaying::new(title, "DJ", "live");
                        now.url = url;
                        rt.set_now(now);
                    }
            }))
        } else {
            limited
        };
        let kind = self.rt.spec.read().expect("spec").encoder.clone();
        let mut pcm = self.rt.factory.decode_kind(&kind, stream, &self.meta.content_type, self.rt.sample_rate, self.rt.channels)?.reader;
        let mut buf = vec![0u8; 8192];
        loop {
            if self.rt.stop.load(Ordering::SeqCst) {
                break;
            }
            let n = match pcm.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    if e.kind() == io::ErrorKind::Other && e.to_string().contains("bitrate") {
                        eprintln!("navad: source over bitrate on {}", self.rt.path());
                    }
                    break;
                }
            };
            let mine = {
                let _guard = self.rt.live_lock.lock().expect("live");
                if self.rt.live_gen.load(Ordering::SeqCst) != self.gen {
                    break;
                }
                true
            };
            if !mine {
                break;
            }
            {
                let _guard = self.rt.live_lock.lock().unwrap_or_else(|e| e.into_inner());
                if self.rt.live_gen.load(Ordering::SeqCst) != self.gen {
                    break;
                }
                match self.rt.live_q.write_timeout(&buf[..n], Duration::from_millis(400)) {
                    Ok(()) => {}
                    Err(err) if err.kind() == io::ErrorKind::TimedOut => continue,
                    Err(_) => break,
                }
            }
            self.rt.mixer.note_live();
        }
        Ok(())
    }

    fn run_packet(&self, reader: Box<dyn Read + Send>) -> io::Result<()> {
        let max_br = self.rt.spec.read().expect("spec").max_bitrate;
        let reader = Box::new(Limited::new(reader, max_br));
        let (tx, rx) = mpsc::channel();
        self.rt.set_live(LiveSession { reader, done: tx });
        let _ = rx.recv();
        Ok(())
    }

    fn finish(&mut self) {
        if self.ended {
            return;
        }
        self.ended = true;
        let current = self.rt.slot.lock().expect("slot").end(self.gen);
        if current {
            self.rt.mixer.to_auto();
            self.rt.set_now(NowPlaying::autodj());
            *self.rt.live_since.lock().expect("since") = None;
            let _guard = self.rt.live_lock.lock().expect("live");
            self.rt.live_q.drain();
        }
    }
}

impl Drop for SourceGuard {
    fn drop(&mut self) { self.finish(); }
}

struct Limited {
    inner: Box<dyn Read + Send>,
    total: u64,
    start: Instant,
    max_bps: u64,
}

impl Limited {
    fn new(inner: Box<dyn Read + Send>, max_bitrate: u32) -> Self {
        Self { inner, total: 0, start: Instant::now(), max_bps: (max_bitrate / 8) as u64 }
    }
}

impl Read for Limited {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n == 0 || self.max_bps == 0 {
            return Ok(n);
        }
        self.total += n as u64;
        let elapsed = self.start.elapsed().as_secs_f64();
        if elapsed >= 0.5 {
            let allowed = (elapsed * self.max_bps as f64) as u64 + 256 * 1024;
            if self.total > allowed {
                return Err(io::Error::new(io::ErrorKind::Other, "bitrate exceeded"));
            }
        }
        Ok(n)
    }
}

struct Runtime {
    spec: RwLock<MountSpec>,
    stop: AtomicBool,
    pcm: bool,
    sample_rate: u32,
    channels: u16,
    frame: Duration,
    media_root: PathBuf,
    fast: bool,
    factory: Arc<dyn AudioFactory>,
    tracks: Arc<dyn TrackSource>,
    auto_q: Arc<PcmQueue>,
    live_q: Arc<PcmQueue>,
    mixer: Mixer,
    fan: Fanout,
    hls: Mutex<Hls>,
    now: Arc<Mutex<NowPlaying>>,
    history: Mutex<VecDeque<NowPlaying>>,
    slot: Mutex<SourceSlot>,
    live_lock: Mutex<()>,
    live_gen: AtomicU64,
    live_session: Mutex<Option<LiveSession>>,
    live_since: Mutex<Option<Instant>>,
    live_cv: Condvar,
    audio_run: AtomicU64,
    worker: Mutex<Option<JoinHandle<()>>>,
    halt: Arc<AtomicBool>,
    relay_live: AtomicBool,
    relay_shut: Mutex<Option<Box<dyn Fn() + Send>>>,
    relay_allow_private: Arc<AtomicBool>,
    deny_relay: Arc<Mutex<Vec<std::net::SocketAddr>>>,
    capture: crate::record::Slot,
    record_root: RwLock<PathBuf>,
    record_hook: RwLock<Option<crate::record::FinishHook>>,
    tap: PcmTap,
    public_stream: AtomicBool,
}

struct LiveSession {
    reader: Box<dyn Read + Send>,
    done: mpsc::Sender<()>,
}

struct SourceSlot {
    gen: u64,
    active: bool,
    shutdown: Option<Box<dyn Fn() + Send>>,
}

impl SourceSlot {
    fn begin(&mut self, shutdown: Box<dyn Fn() + Send>, takeover: bool) -> Result<u64, Error> {
        if self.active && !takeover {
            return Err(Error::Busy);
        }
        if let Some(shut) = self.shutdown.take() {
            shut();
        }
        self.gen = self.gen.wrapping_add(1).max(1);
        self.active = true;
        self.shutdown = Some(shutdown);
        Ok(self.gen)
    }

    fn end(&mut self, gen: u64) -> bool {
        if self.active && self.gen == gen {
            self.active = false;
            self.shutdown = None;
            true
        } else {
            false
        }
    }
}

impl Runtime {
    fn start(
        spec: MountSpec,
        factory: &Arc<dyn AudioFactory>,
        tracks: &Arc<dyn TrackSource>,
        opts: &EngineOpts,
        halt: Arc<AtomicBool>,
        relay_allow_private: Arc<AtomicBool>,
        deny_relay: Arc<Mutex<Vec<std::net::SocketAddr>>>,
        record_root: PathBuf,
        record_hook: Option<crate::record::FinishHook>,
    ) -> Arc<Self> {
        let (sr, ch, _, _) = resolve_format(&spec, opts.sample_rate, opts.channels);
        let cap = buffer_bytes(sr, ch, spec.buffer_ms);
        let auto_q = Arc::new(PcmQueue::new(cap));
        let live_q = Arc::new(PcmQueue::new(cap));
        let fade = pcm::fade_samples(sr, Duration::from_millis(clamp_crossfade_ms(spec.crossfade_ms)));
        let mixer = Mixer::new(Arc::clone(&auto_q), Arc::clone(&live_q), ch, fade);
        let max_listeners = spec.max_listeners;
        let hls_ms = if spec.hls_segment_ms == 0 { 1_000 } else { clamp_hls_segment_ms(spec.hls_segment_ms) };
        let rt = Arc::new(Self {
            spec: RwLock::new(spec),
            stop: AtomicBool::new(false),
            pcm: factory.pcm(),
            sample_rate: opts.sample_rate,
            channels: opts.channels,
            frame: opts.frame,
            media_root: opts.media_root.clone(),
            fast: opts.fast,
            factory: Arc::clone(factory),
            tracks: Arc::clone(tracks),
            auto_q,
            live_q,
            mixer,
            fan: Fanout::new(max_listeners, 64 * 1024),
            hls: Mutex::new(Hls::new(Duration::from_millis(hls_ms), 6)),
            now: Arc::new(Mutex::new(NowPlaying::autodj())),
            history: Mutex::new(VecDeque::new()),
            slot: Mutex::new(SourceSlot { gen: 0, active: false, shutdown: None }),
            live_lock: Mutex::new(()),
            live_gen: AtomicU64::new(0),
            live_session: Mutex::new(None),
            live_since: Mutex::new(None),
            live_cv: Condvar::new(),
            audio_run: AtomicU64::new(0),
            worker: Mutex::new(None),
            halt,
            relay_live: AtomicBool::new(false),
            relay_shut: Mutex::new(None),
            relay_allow_private,
            deny_relay,
            capture: crate::record::Slot::new(),
            record_root: RwLock::new(record_root),
            record_hook: RwLock::new(record_hook),
            tap: PcmTap::new(),
            public_stream: AtomicBool::new(true),
        });
        let worker = Arc::clone(&rt);
        let handle = thread::Builder::new()
            .name(format!("mount{}", rt.path()))
            .spawn(move || supervise(worker))
            .expect("spawn mount");
        *rt.worker.lock().expect("worker") = Some(handle);
        rt
    }

    fn relay_policy(&self) -> crate::relay::Policy {
        crate::relay::Policy {
            allow_private: self.relay_allow_private.load(Ordering::SeqCst),
            deny: self.deny_relay.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        }
    }

    fn path(&self) -> String { self.spec.read().expect("spec").path.clone() }
    fn id(&self) -> String { self.spec.read().expect("spec").id.clone() }

    fn set_now(&self, np: NowPlaying) {
        let mut cur = self.now.lock().expect("now");
        let changed = cur.title != np.title || cur.artist != np.artist || cur.source != np.source;
        *cur = np.clone();
        drop(cur);
        if changed {
            let mut hist = self.history.lock().expect("hist");
            hist.push_back(np);
            while hist.len() > 30 {
                hist.pop_front();
            }
        }
    }

    fn set_live(&self, session: LiveSession) {
        let mut g = self.live_session.lock().expect("live session");
        if let Some(prev) = g.replace(session) {
            let _ = prev.done.send(());
        }
        self.live_cv.notify_all();
    }

    fn status(&self) -> MountStatus {
        self.capture_bytes(&[]);
        let spec = self.spec.read().unwrap_or_else(|e| e.into_inner()).clone();
        let source_on = self.slot.lock().unwrap_or_else(|e| e.into_inner()).active;
        let playing = self.fan.audible(AUDIBLE_WINDOW);
        let mut now = self.now.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if source_on {
            now.elapsed_ms = self.live_since.lock().unwrap_or_else(|e| e.into_inner()).map(|t| t.elapsed().as_millis() as u64).unwrap_or(0);
            now.duration_ms = 0;
            now.source = "live".into();
        } else if self.relay_live.load(Ordering::SeqCst) {
            if now.source != "relay" {
                now = NowPlaying::new("Relay", "", "relay");
            }
        } else if let Some(head) = self.tracks.play_head(&spec.id) {
            now.elapsed_ms = head.elapsed_ms;
            now.duration_ms = head.duration_ms;
            if !head.media_id.is_empty() {
                now.media_id = head.media_id;
            }
            if (now.title.is_empty() || now.title == "AutoDJ") && !head.title.is_empty() {
                now.title = head.title;
                now.artist = head.artist;
                now.source = "autodj".into();
            }
        }
        now.position_ms = now.elapsed_ms;
        let (sample_rate, channels, _, _) = resolve_format(&spec, self.sample_rate, self.channels);
        let buffer_ms = if spec.buffer_ms == 0 { 250 } else { clamp_buffer_ms(spec.buffer_ms) };
        let hls_segment_ms = if spec.hls_segment_ms == 0 { 1_000 } else { clamp_hls_segment_ms(spec.hls_segment_ms) };
        let gain_db = clamp_gain_db(spec.gain_db);
        let crossfade_ms = clamp_crossfade_ms(spec.crossfade_ms);
        let repeat = spec.repeat.as_str().to_string();
        let autodj = spec.autodj;
        let allow_takeover = spec.allow_takeover;
        let shuffle = spec.shuffle;
        let format = if spec.format.is_empty() { "mp3".into() } else { "mp3".into() };
        let position_ms = now.position_ms;
        let duration_ms = now.duration_ms;
        if !playing {
            now = NowPlaying::new("", "", "idle");
        }
        let history: Vec<_> = self.history.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect();
        let listener_rows = self.fan.list();
        let mode = if !playing {
            "idle"
        } else if source_on {
            "source"
        } else if self.relay_live.load(Ordering::SeqCst) {
            "relay"
        } else if spec.autodj {
            "autodj"
        } else {
            "idle"
        };
        let rec = self.capture.live();
        let rec_active = rec.as_ref().is_some_and(|r| r.active);
        let rec_error = rec.as_ref().map(|r| r.error.clone()).unwrap_or_default();
        let rec_id = rec.as_ref().map(|r| r.id.clone()).unwrap_or_default();
        let rec_bytes = rec.as_ref().map(|r| r.bytes).unwrap_or(0);
        let rec_ms = rec.as_ref().map(|r| r.duration_ms).unwrap_or(0);
        MountStatus {
            id: spec.id,
            station_id: spec.station_id,
            station_name: spec.station_name,
            path: spec.path,
            live: source_on && playing,
            listeners: listener_rows.len(),
            bitrate: spec.bitrate,
            now,
            history,
            listener_rows,
            peak_listeners: self.fan.peak(),
            bytes_sent: self.fan.bytes_sent(),
            uptime_sec: self.fan.started.elapsed().as_secs(),
            mode: mode.into(),
            playing,
            position_ms: if playing { position_ms } else { 0 },
            duration_ms: if playing { duration_ms } else { 0 },
            gain_db,
            sample_rate,
            channels,
            crossfade_ms,
            buffer_ms,
            hls_segment_ms,
            autodj,
            allow_takeover,
            shuffle,
            repeat,
            format,
            encoder: spec.encoder,
            relay_url: spec.relay_url,
            recording: rec_active,
            recording_error: rec_error,
            recording_id: rec_id,
            recording_bytes: rec_bytes,
            recording_ms: rec_ms,
        }
    }

    fn arm_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.auto_q.close();
        self.live_q.close();
        if let Some(shut) = self.slot.lock().expect("slot").shutdown.take() {
            shut();
        }
        if let Some(shut) = self.relay_shut.lock().expect("relay").take() {
            shut();
        }
        if let Some(session) = self.live_session.lock().expect("live session").take() {
            let _ = session.done.send(());
        }
        self.live_cv.notify_all();
        if let Some(fin) = self.capture.stop() {
            self.notify_record(fin);
        }
    }

    /// Same encoded bytes the fanout just accepted. A write error stays here.
    fn capture_bytes(&self, chunk: &[u8]) {
        let finished = panic::catch_unwind(AssertUnwindSafe(|| self.capture.write(chunk)));
        let Ok(finished) = finished else { return };
        if let Some(fin) = finished {
            self.notify_record(fin);
        }
    }

    fn notify_record(&self, fin: crate::record::Finished) {
        let hook = self.record_hook.read().unwrap_or_else(|e| e.into_inner()).clone();
        let path = self.path();
        if let Some(hook) = hook {
            let _ = panic::catch_unwind(AssertUnwindSafe(|| hook(&path, fin)));
        }
    }

    fn publish(&self, chunk: &[u8], dur: Duration) {
        if chunk.is_empty() {
            self.capture_bytes(&[]);
            return;
        }
        let owned = chunk.to_vec();
        self.fan.broadcast(owned.clone());
        self.capture_bytes(&owned);
        let mut hls = self.hls.lock().expect("hls");
        hls.push(&owned, dur);
    }

    fn stop_join(&self) {
        self.arm_stop();
        crate::encoder::kill_all();
        if let Some(handle) = self.worker.lock().expect("worker").take() {
            let _ = crate::join_timeout(handle, Duration::from_secs(4));
        }
        self.fan.close();
    }
}

fn supervise(rt: Arc<Runtime>) {
    if !rt.pcm {
        let _ = panic::catch_unwind(AssertUnwindSafe(|| run_packet(&rt)));
        return;
    }
    while !rt.stop.load(Ordering::SeqCst) && !rt.halt.load(Ordering::SeqCst) {
        let started = Instant::now();
        let rt_run = Arc::clone(&rt);
        let crashed = panic::catch_unwind(AssertUnwindSafe(|| run_pcm(&rt_run))).is_err();
        if rt.stop.load(Ordering::SeqCst) {
            break;
        }
        if crashed {
            eprintln!("navad: {} pump panicked; restarting", rt.path());
            rt.auto_q.reopen();
        }
        // An encoder that dies before the clock ticks would otherwise spin this thread.
        if started.elapsed() < Duration::from_millis(50) {
            thread::sleep(Duration::from_millis(200));
        }
    }
}

fn audio_format(rt: &Runtime) -> (u32, u16, usize, u32) {
    let spec = rt.spec.read().unwrap_or_else(|e| e.into_inner());
    let (sr, ch, _br, _gain) = resolve_format(&spec, rt.sample_rate, rt.channels);
    let frame = pcm::frame_bytes(sr, ch, rt.frame).max((ch.max(1) as usize) * 2);
    let bitrate = clamp_bitrate(spec.bitrate.max(32_000));
    (sr, ch, frame, bitrate)
}

fn resolve_format(spec: &MountSpec, fallback_rate: u32, fallback_ch: u16) -> (u32, u16, u32, f64) {
    let sr = if spec.sample_rate == 0 { fallback_rate.max(8_000) } else { clamp_sample_rate(spec.sample_rate) };
    let ch = if spec.channels == 0 { fallback_ch.max(1) } else { clamp_channels(spec.channels) };
    (sr, ch, clamp_bitrate(spec.bitrate.max(32_000)), clamp_gain_db(spec.gain_db))
}

fn apply_gain(pcm: &mut [u8], gain_db: f64) {
    if gain_db.abs() < 0.05 {
        return;
    }
    let g = 10f64.powf(gain_db / 20.0) as f32;
    for frame in pcm.chunks_exact_mut(2) {
        let s = i16::from_le_bytes([frame[0], frame[1]]) as f32 * g;
        let s = s.clamp(-32768.0, 32767.0) as i16;
        frame.copy_from_slice(&s.to_le_bytes());
    }
}

fn run_pcm(rt: &Arc<Runtime>) {
    let (sr, ch, _frame_bytes, bitrate) = audio_format(rt);
    let kind = rt.spec.read().unwrap_or_else(|e| e.into_inner()).encoder.clone();
    let mut ends = match rt.factory.encode_with(&kind, sr, ch, bitrate) {
        Ok(ends) => ends,
        Err(err) => {
            eprintln!("navad: encoder {}: {err}", rt.path());
            thread::sleep(Duration::from_millis(200));
            return;
        }
    };
    let Some(input) = ends.input.take() else {
        eprintln!("navad: encoder {} returned no input", rt.path());
        thread::sleep(Duration::from_millis(200));
        return;
    };
    let Some(output) = ends.output.take() else {
        eprintln!("navad: encoder {} returned no output", rt.path());
        thread::sleep(Duration::from_millis(200));
        return;
    };
    let run_id = rt.audio_run.fetch_add(1, Ordering::SeqCst) + 1;
    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(8);
    let failed = Arc::new(AtomicBool::new(false));
    let failed_feeder = Arc::clone(&failed);
    let feeder = thread::Builder::new().name("rd-feed".into()).spawn(move || {
        let mut input = input;
        let _ = panic::catch_unwind(AssertUnwindSafe(|| {
            while let Ok(buf) = rx.recv() {
                if input.write_all(&buf).is_err() {
                    break;
                }
            }
        }));
        failed_feeder.store(true, Ordering::SeqCst);
    }).ok();
    let pump_alive = Arc::new(AtomicBool::new(true));
    let alive_flag = Arc::clone(&pump_alive);
    let out_bytes = Arc::new(AtomicU64::new(0));
    let out_for_pump = Arc::clone(&out_bytes);
    let pump_rt = Arc::clone(rt);
    let pump = thread::Builder::new().name("rd-pump".into()).spawn(move || {
        let _mark = AliveGuard(alive_flag);
        let _ = panic::catch_unwind(AssertUnwindSafe(|| pump_encoded(pump_rt, output, out_for_pump)));
    }).ok();
    let auto_rt = Arc::clone(rt);
    let autodj = thread::Builder::new().name("rd-auto".into()).spawn(move || {
        let _ = panic::catch_unwind(AssertUnwindSafe(|| autodj_loop(auto_rt, run_id)));
    }).ok();
    let mut session = PcmSession {
        rt,
        tx: Some(tx),
        child: ends.child.take(),
        feeder,
        pump,
        autodj,
        failed,
        pump_alive,
        out_bytes,
        run_id,
    };
    drop(ends);
    if session.feeder.is_none() || session.pump.is_none() || session.autodj.is_none() {
        eprintln!("navad: {} could not start the pump", rt.path());
        return;
    }
    session.run();
}

struct AliveGuard(Arc<AtomicBool>);
impl Drop for AliveGuard {
    fn drop(&mut self) { self.0.store(false, Ordering::SeqCst); }
}

struct PcmSession<'a> {
    rt: &'a Runtime,
    tx: Option<SyncSender<Vec<u8>>>,
        child: Option<crate::encoder::SharedChild>,
    feeder: Option<JoinHandle<()>>,
    pump: Option<JoinHandle<()>>,
    autodj: Option<JoinHandle<()>>,
    failed: Arc<AtomicBool>,
    pump_alive: Arc<AtomicBool>,
    out_bytes: Arc<AtomicU64>,
    run_id: u64,
}

impl PcmSession<'_> {
    fn run(&mut self) {
        let rt = self.rt;
        let (_, _, frame_bytes, _) = audio_format(rt);
        let mut pcm_buf = vec![0u8; frame_bytes.max(1)];
        let mut next = Instant::now();
        let mut last_out = 0u64;
        let mut last_out_at = Instant::now();
        let mut sent = 0u64;
        loop {
            if rt.stop.load(Ordering::SeqCst) || rt.audio_run.load(Ordering::SeqCst) != self.run_id {
                break;
            }
            if !self.pump_alive.load(Ordering::SeqCst) || self.failed.load(Ordering::SeqCst) {
                eprintln!("navad: encoder on {} exited; restarting the pump", rt.path());
                break;
            }
            let produced = self.out_bytes.load(Ordering::SeqCst);
            if produced != last_out {
                last_out = produced;
                last_out_at = Instant::now();
            } else if sent > 0 && last_out_at.elapsed() > Duration::from_secs(4) {
                eprintln!("navad: encoder on {} produced no bytes; restarting the pump", rt.path());
                break;
            }
            let spec = rt.spec.read().unwrap_or_else(|e| e.into_inner()).clone();
            let (sr, ch, _, gain) = resolve_format(&spec, rt.sample_rate, rt.channels);
            rt.mixer.set_channels(ch);
            rt.mixer.set_fade(pcm::fade_samples(sr, Duration::from_millis(clamp_crossfade_ms(spec.crossfade_ms))));
            rt.mixer.fill(&mut pcm_buf);
            apply_gain(&mut pcm_buf, gain);
            // TeamSpeak bots take a copy here. try_send drops a slow bot; the pump does not wait.
            rt.tap.publish(&pcm_buf, sr, ch);
            let Some(tx) = self.tx.as_ref() else { break };
            if !send_frame(tx, &pcm_buf, rt) {
                if !rt.stop.load(Ordering::SeqCst) {
                    eprintln!("navad: encoder on {} stopped accepting audio; restarting the pump", rt.path());
                }
                break;
            }
            sent = sent.saturating_add(1);
            rt.hls.lock().unwrap_or_else(|e| e.into_inner()).add_time(rt.frame);
            next += rt.frame;
            let now = Instant::now();
            if next > now {
                thread::sleep(next - now);
            } else {
                next = now;
            }
        }
    }
}

impl Drop for PcmSession<'_> {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(child) = self.child.take() {
            child.shutdown();
        }
        if let Some(handle) = self.feeder.take() {
            join_within(handle, Duration::from_secs(2));
        }
        if let Some(handle) = self.pump.take() {
            join_within(handle, Duration::from_secs(2));
        }
        self.rt.audio_run.fetch_add(1, Ordering::SeqCst);
        self.rt.auto_q.close();
        if let Some(handle) = self.autodj.take() {
            join_within(handle, Duration::from_secs(2));
        }
        if !self.rt.stop.load(Ordering::SeqCst) {
            self.rt.auto_q.reopen();
        }
    }
}

fn send_frame(tx: &SyncSender<Vec<u8>>, frame: &[u8], rt: &Runtime) -> bool {
    if frame.is_empty() {
        return true;
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut pending = frame.to_vec();
    loop {
        if rt.stop.load(Ordering::SeqCst) {
            return false;
        }
        match tx.try_send(pending) {
            Ok(()) => return true,
            Err(TrySendError::Disconnected(_)) => return false,
            Err(TrySendError::Full(back)) => {
                pending = back;
                if Instant::now() >= deadline {
                    return false;
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

fn join_within(handle: JoinHandle<()>, timeout: Duration) {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = handle.join();
        let _ = tx.send(());
    });
    if rx.recv_timeout(timeout).is_err() {
        eprintln!("navad: encoder thread did not exit; leaving it detached");
    }
}

fn pump_encoded(rt: Arc<Runtime>, mut output: Box<dyn Read + Send>, out_bytes: Arc<AtomicU64>) {
    let mut buf = [0u8; 1024];
    loop {
        if rt.stop.load(Ordering::SeqCst) {
            break;
        }
        match output.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                out_bytes.fetch_add(n as u64, Ordering::SeqCst);
                let chunk = buf[..n].to_vec();
                rt.fan.broadcast(chunk.clone());
                rt.capture_bytes(&chunk);
                rt.hls.lock().unwrap_or_else(|e| e.into_inner()).add_bytes(&chunk);
            }
            Err(_) => break,
        }
    }
}

fn autodj_loop(rt: Arc<Runtime>, run_id: u64) {
    let mut failures = 0u32;
    let mut backoff = Duration::from_millis(300);
    let mut next_retry = Instant::now();
    while !rt.stop.load(Ordering::SeqCst) && rt.audio_run.load(Ordering::SeqCst) == run_id {
        let url = rt.spec.read().unwrap_or_else(|e| e.into_inner()).relay_url.trim().to_string();
        let try_relay = !url.is_empty() && (failures < 3 || Instant::now() >= next_retry);
        if try_relay {
            if failures >= 3 {
                failures = 0;
                backoff = Duration::from_millis(300);
            }
            let pulled = panic::catch_unwind(AssertUnwindSafe(|| relay_pull(&rt, &url, run_id)));
            match pulled {
                Ok(Ok(())) => {
                    failures = 0;
                    backoff = Duration::from_millis(300);
                    continue;
                }
                Ok(Err(err)) => {
                    failures = failures.saturating_add(1);
                    rt.relay_live.store(false, Ordering::SeqCst);
                    eprintln!("navad: relay {url} on {}: {err}", rt.path());
                    if failures < 3 {
                        if !sleep_stop(&rt, backoff) {
                            return;
                        }
                        backoff = (backoff * 2).min(Duration::from_secs(8));
                        continue;
                    }
                    next_retry = Instant::now() + Duration::from_secs(10);
                    eprintln!("navad: relay on {} stayed down; falling back to autodj", rt.path());
                }
                Err(_) => {
                    failures = 3;
                    rt.relay_live.store(false, Ordering::SeqCst);
                    next_retry = Instant::now() + Duration::from_secs(10);
                    eprintln!("navad: relay on {} failed; falling back to autodj", rt.path());
                }
            }
        } else if url.is_empty() {
            rt.relay_live.store(false, Ordering::SeqCst);
            failures = 0;
        }
        let step = panic::catch_unwind(AssertUnwindSafe(|| autodj_once(&rt, run_id)));
        match step {
            Ok(AutoStep::Stop) => return,
            Ok(AutoStep::Continue) => {}
            Err(_) => {
                eprintln!("navad: autodj on {} hit a bad track; continuing", rt.path());
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn sleep_stop(rt: &Runtime, total: Duration) -> bool {
    let end = Instant::now() + total;
    while Instant::now() < end {
        if rt.stop.load(Ordering::SeqCst) {
            return false;
        }
        let slice = Duration::from_millis(50).min(end.saturating_duration_since(Instant::now()));
        thread::sleep(slice);
    }
    !rt.stop.load(Ordering::SeqCst)
}

fn relay_pull(rt: &Arc<Runtime>, url: &str, run_id: u64) -> io::Result<()> {
    if rt.stop.load(Ordering::SeqCst) || rt.audio_run.load(Ordering::SeqCst) != run_id {
        return Ok(());
    }
    let pull = crate::relay::open(url, &rt.relay_policy()).map_err(|err| io::Error::new(io::ErrorKind::Other, err.to_string()))?;
    if let Some(shut) = pull.shutdown {
        *rt.relay_shut.lock().unwrap_or_else(|e| e.into_inner()) = Some(shut);
    }
    let result = relay_body(rt, url, run_id, pull.reader, pull.content_type, pull.metaint);
    *rt.relay_shut.lock().unwrap_or_else(|e| e.into_inner()) = None;
    result
}

fn relay_body(rt: &Arc<Runtime>, url: &str, run_id: u64, reader: Box<dyn Read + Send>, content_type: String, metaint: Option<usize>) -> io::Result<()> {
    let max_br = {
        let spec = rt.spec.read().unwrap_or_else(|e| e.into_inner());
        if spec.max_bitrate == 0 { 320_000 } else { spec.max_bitrate }
    };
    let limited = Box::new(Limited::new(reader, max_br));
    let stream: Box<dyn Read + Send> = if let Some(interval) = metaint {
        let rt_meta = Arc::clone(rt);
        Box::new(crate::icy::Stripper::new(limited, interval, move |title, page| {
            if !title.is_empty() {
                let mut now = NowPlaying::new(title, "", "relay");
                now.url = page;
                rt_meta.set_now(now);
            }
        }))
    } else {
        limited
    };
    let (sr, ch, _, _) = audio_format(rt);
    let kind = rt.spec.read().unwrap_or_else(|e| e.into_inner()).encoder.clone();
    let decoded = rt.factory.decode_kind(&kind, stream, &content_type, sr, ch)?;
    let mut session = DecodeSession::start(decoded);
    loop {
        if rt.stop.load(Ordering::SeqCst) || rt.audio_run.load(Ordering::SeqCst) != run_id {
            return Ok(());
        }
        if rt.spec.read().unwrap_or_else(|e| e.into_inner()).relay_url.trim() != url {
            return Ok(());
        }
        match session.recv_timeout(Duration::from_millis(200)) {
            Ok(Some(bytes)) => {
                if bytes.is_empty() {
                    continue;
                }
                match rt.auto_q.write_timeout(&bytes, Duration::from_millis(200)) {
                    Ok(()) => rt.relay_live.store(true, Ordering::SeqCst),
                    Err(err) if err.kind() == io::ErrorKind::TimedOut => {}
                    Err(err) if err.kind() == io::ErrorKind::BrokenPipe => return Ok(()),
                    Err(err) => return Err(err),
                }
            }
            Ok(None) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "relay ended")),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "relay decoder exited"));
            }
        }
    }
}

enum AutoStep { Continue, Stop }

fn autodj_once(rt: &Arc<Runtime>, run_id: u64) -> AutoStep {
    if rt.stop.load(Ordering::SeqCst) || rt.audio_run.load(Ordering::SeqCst) != run_id {
        return AutoStep::Stop;
    }
    let mount_id = rt.id();
    if !rt.spec.read().unwrap_or_else(|e| e.into_inner()).autodj {
        thread::sleep(Duration::from_millis(40));
        return AutoStep::Continue;
    }
    let token = rt.tracks.play_generation(&mount_id);
    match rt.tracks.next(&mount_id) {
        Some(track) => {
            let mut now = NowPlaying::new(track.title.clone(), track.artist.clone(), "autodj");
            now.duration_ms = track.duration_ms;
            now.media_id = track.media_id.clone();
            rt.set_now(now);
            match play_track(rt, &mount_id, &track, token, run_id) {
                Ok(()) => AutoStep::Continue,
                Err(err) if err.kind() == io::ErrorKind::BrokenPipe => AutoStep::Stop,
                Err(err) => {
                    eprintln!("navad: skipping {} on {}: {err}", track.title, rt.path());
                    AutoStep::Continue
                }
            }
        }
        None => {
            for _ in 0..4 {
                if rt.stop.load(Ordering::SeqCst) || rt.audio_run.load(Ordering::SeqCst) != run_id {
                    return AutoStep::Stop;
                }
                thread::sleep(Duration::from_millis(50));
            }
            AutoStep::Continue
        }
    }
}

fn still_playing(rt: &Runtime, mount_id: &str, token: u64, run_id: u64) -> bool {
    !rt.stop.load(Ordering::SeqCst)
        && rt.audio_run.load(Ordering::SeqCst) == run_id
        && rt.tracks.play_generation(mount_id) == token
        && rt.spec.read().unwrap_or_else(|e| e.into_inner()).autodj
}

fn play_track(rt: &Runtime, mount_id: &str, track: &Track, token: u64, run_id: u64) -> io::Result<()> {
    loop {
        if !still_playing(rt, mount_id, token, run_id) {
            return Ok(());
        }
        let seek_gen = rt.tracks.seek_generation(mount_id);
        let (sr, ch, _, _) = audio_format(rt);
        let mut skip = pcm_skip_bytes(rt.tracks.seek_offset_ms(mount_id), sr, ch);
        let record_root = rt.record_root.read().unwrap_or_else(|e| e.into_inner()).clone();
        let file = open_media(&rt.media_root, &record_root, &track.path)?;
        let kind = rt.spec.read().unwrap_or_else(|e| e.into_inner()).encoder.clone();
        let decoded = rt.factory.decode_kind(&kind, Box::new(file), &track.mime, sr, ch)?;
        let mut session = DecodeSession::start(decoded);
        loop {
            if !still_playing(rt, mount_id, token, run_id) {
                return Ok(());
            }
            if rt.tracks.seek_generation(mount_id) != seek_gen {
                break;
            }
            match session.recv_timeout(Duration::from_millis(200)) {
                Ok(Some(bytes)) => {
                    let play = if skip == 0 {
                        bytes
                    } else if bytes.len() <= skip {
                        skip -= bytes.len();
                        continue;
                    } else {
                        let rest = bytes[skip..].to_vec();
                        skip = 0;
                        rest
                    };
                    if play.is_empty() {
                        continue;
                    }
                    match rt.auto_q.write_timeout(&play, Duration::from_millis(400)) {
                        Ok(()) => {}
                        Err(err) if err.kind() == io::ErrorKind::TimedOut => {
                            if !still_playing(rt, mount_id, token, run_id) {
                                return Ok(());
                            }
                        }
                        Err(err) => return Err(err),
                    }
                }
                Ok(None) => return Ok(()),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "decoder exited"));
                }
            }
        }
        drop(session);
    }
}

fn pcm_skip_bytes(ms: u64, sample_rate: u32, channels: u16) -> usize {
    if ms == 0 {
        return 0;
    }
    let frame = (channels.max(1) as usize) * 2;
    let bytes = (sample_rate as u64)
        .saturating_mul(channels.max(1) as u64)
        .saturating_mul(2)
        .saturating_mul(ms)
        / 1_000;
    let bytes = bytes as usize;
    bytes - (bytes % frame)
}

struct DecodeSession {
    cancel: Option<Box<dyn FnOnce() + Send>>,
    rx: Option<Receiver<Option<Vec<u8>>>>,
    join: Option<JoinHandle<()>>,
}

impl DecodeSession {
    fn start(decoded: crate::encoder::CancellableDecode) -> Self {
        let crate::encoder::CancellableDecode { reader, cancel } = decoded;
        let (tx, rx) = mpsc::sync_channel(2);
        let join = thread::spawn(move || {
            let _ = panic::catch_unwind(AssertUnwindSafe(|| {
                let mut reader = reader;
                let mut buf = vec![0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => {
                            let _ = tx.send(None);
                            break;
                        }
                        Ok(n) => {
                            if tx.send(Some(buf[..n].to_vec())).is_err() {
                                break;
                            }
                        }
                        Err(_) => {
                            let _ = tx.send(None);
                            break;
                        }
                    }
                }
            }));
        });
        Self { cancel, rx: Some(rx), join: Some(join) }
    }

    fn recv_timeout(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, RecvTimeoutError> {
        let Some(rx) = self.rx.as_ref() else {
            return Err(RecvTimeoutError::Disconnected);
        };
        rx.recv_timeout(timeout)
    }
}

impl Drop for DecodeSession {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel();
        }
        self.rx.take();
        if let Some(handle) = self.join.take() {
            join_within(handle, Duration::from_secs(2));
        }
    }
}

fn open_media(root: &Path, record_root: &Path, path: &Path) -> io::Result<File> {
    let path = path.canonicalize()?;
    if let Ok(root) = root.canonicalize() {
        if path.starts_with(&root) {
            return File::open(path);
        }
    }
    if !record_root.as_os_str().is_empty() {
        if let Ok(root) = record_root.canonicalize() {
            if path.starts_with(&root) {
                return File::open(path);
            }
        }
    }
    Err(io::Error::new(io::ErrorKind::PermissionDenied, "media path rejected"))
}

fn run_packet(rt: &Arc<Runtime>) {
    let mut cursor = PacketAuto { file: None, silence_at: 0, armed: String::new(), gen: 0 };
    while !rt.stop.load(Ordering::SeqCst) {
        if let Some(session) = rt.live_session.lock().expect("live session").take() {
            play_live(rt, session);
            continue;
        }
        let bitrate = rt.spec.read().expect("spec").bitrate.max(8_000);
        let chunk = next_auto_chunk(rt, &mut cursor);
        if chunk.is_empty() {
            let guard = rt.live_session.lock().expect("live session");
            if guard.is_some() {
                continue;
            }
            let (_guard, _) = rt.live_cv.wait_timeout(guard, Duration::from_millis(200)).expect("cv");
            continue;
        }
        rt.publish(&chunk, hls::bytes_duration(chunk.len(), bitrate));
        if rt.fast {
            thread::sleep(Duration::from_millis(2));
        } else {
            thread::sleep(hls::bytes_duration(chunk.len(), bitrate));
        }
    }
}

fn play_live(rt: &Runtime, mut session: LiveSession) {
    let mut buf = [0u8; 4096];
    loop {
        if rt.stop.load(Ordering::SeqCst) {
            break;
        }
        match session.reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let bitrate = rt.spec.read().expect("spec").bitrate.max(8_000);
                rt.publish(&buf[..n], hls::bytes_duration(n, bitrate));
            }
        }
    }
    let _ = session.done.send(());
}

struct PacketAuto {
    file: Option<BufReader<File>>,
    silence_at: usize,
    armed: String,
    gen: u64,
}

fn next_auto_chunk(rt: &Runtime, cursor: &mut PacketAuto) -> Vec<u8> {
    let gen = rt.tracks.play_generation(&rt.id());
    if cursor.gen != gen {
        cursor.file = None;
        cursor.armed.clear();
        cursor.gen = gen;
    }
    if !rt.spec.read().expect("spec").autodj {
        cursor.file = None;
        return Vec::new();
    }
    if cursor.file.is_none() {
        if let Some(track) = rt.tracks.next(&rt.id()) {
            if cursor.armed != track.path.display().to_string() {
                cursor.armed = track.path.display().to_string();
                let mut now = NowPlaying::new(track.title, track.artist, "autodj");
                now.duration_ms = track.duration_ms;
                now.media_id = track.media_id;
                rt.set_now(now);
            }
            let record_root = rt.record_root.read().unwrap_or_else(|e| e.into_inner()).clone();
            if let Ok(file) = open_media(&rt.media_root, &record_root, &track.path) {
                cursor.file = Some(BufReader::new(file));
            }
        }
    }
    if let Some(file) = cursor.file.as_mut() {
        let mut buf = vec![0u8; 4096];
        match file.read(&mut buf) {
            Ok(0) => cursor.file = None,
            Ok(n) => {
                buf.truncate(n);
                return buf;
            }
            Err(_) => cursor.file = None,
        }
    }
    if SILENCE_MP3.is_empty() {
        return Vec::new();
    }
    let n = 2048.min(SILENCE_MP3.len());
    if cursor.silence_at + n > SILENCE_MP3.len() {
        cursor.silence_at = 0;
    }
    let end = (cursor.silence_at + n).min(SILENCE_MP3.len());
    let out = SILENCE_MP3[cursor.silence_at..end].to_vec();
    cursor.silence_at = if end >= SILENCE_MP3.len() { 0 } else { end };
    out
}

struct Sub {
    tx: SyncSender<Vec<u8>>,
    ip: String,
    user_agent: String,
    connected: Instant,
    bytes: Arc<AtomicU64>,
}

struct FanInner {
    subs: HashMap<u64, Sub>,
    next: u64,
    ring: Vec<u8>,
    ring_max: usize,
    max: usize,
    closed: bool,
    bytes_sent: u64,
    peak: usize,
    last_audio: Option<Instant>,
}

const AUDIBLE_WINDOW: Duration = Duration::from_millis(1500);

/// One mixer frame, interleaved little-endian s16, before the Icecast encoder.
#[derive(Clone)]
pub struct PcmChunk {
    pub rate: u32,
    pub channels: u16,
    pub pcm: Vec<u8>,
}

const TAP_CAP: usize = 24;

struct PcmTap {
    subs: Mutex<Vec<SyncSender<PcmChunk>>>,
}

impl PcmTap {
    fn new() -> Self {
        Self { subs: Mutex::new(Vec::new()) }
    }

    fn subscribe(&self) -> Receiver<PcmChunk> {
        let (tx, rx) = mpsc::sync_channel(TAP_CAP);
        self.subs.lock().unwrap_or_else(|e| e.into_inner()).push(tx);
        rx
    }

    fn publish(&self, pcm: &[u8], rate: u32, channels: u16) {
        let mut g = self.subs.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_empty() || pcm.is_empty() {
            return;
        }
        let frame = PcmChunk { rate, channels, pcm: pcm.to_vec() };
        let mut i = 0;
        while i < g.len() {
            match g[i].try_send(frame.clone()) {
                Ok(()) => i += 1,
                Err(_) => {
                    g.swap_remove(i);
                }
            }
        }
    }
}

struct Fanout {
    inner: Mutex<FanInner>,
    started: Instant,
}

struct Subscription {
    id: u64,
    rx: Receiver<Vec<u8>>,
    ring: Vec<u8>,
    bytes: Arc<AtomicU64>,
}

impl Fanout {
    fn new(max: usize, ring_max: usize) -> Self {
        Self {
            inner: Mutex::new(FanInner {
                subs: HashMap::new(),
                next: 0,
                ring: Vec::new(),
                ring_max,
                max,
                closed: false,
                bytes_sent: 0,
                peak: 0,
                last_audio: None,
            }),
            started: Instant::now(),
        }
    }

    fn set_max(&self, max: usize) {
        self.inner.lock().expect("fan").max = max;
    }

    fn has_room(&self) -> bool {
        let g = self.inner.lock().expect("fan");
        !g.closed && (g.max == 0 || g.subs.len() < g.max)
    }

    fn subscribe(&self, ip: &str, user_agent: &str) -> Result<Subscription, Error> {
        let (tx, rx) = mpsc::sync_channel(24);
        let bytes = Arc::new(AtomicU64::new(0));
        let mut g = self.inner.lock().expect("fan");
        if g.closed {
            return Err(Error::NotFound);
        }
        if g.max > 0 && g.subs.len() >= g.max {
            return Err(Error::Limit);
        }
        g.next += 1;
        let id = g.next;
        let ring = g.ring.clone();
        g.subs.insert(id, Sub {
            tx,
            ip: ip.into(),
            user_agent: clean_agent(user_agent),
            connected: Instant::now(),
            bytes: Arc::clone(&bytes),
        });
        if g.subs.len() > g.peak {
            g.peak = g.subs.len();
        }
        Ok(Subscription { id, rx, ring, bytes })
    }

    fn remove(&self, id: u64) {
        self.inner.lock().expect("fan").subs.remove(&id);
    }

    fn broadcast(&self, chunk: Vec<u8>) {
        let mut g = self.inner.lock().expect("fan");
        if g.closed {
            return;
        }
        g.bytes_sent = g.bytes_sent.saturating_add(chunk.len() as u64);
        if !chunk.is_empty() {
            g.last_audio = Some(Instant::now());
        }
        g.ring.extend_from_slice(&chunk);
        if g.ring.len() > g.ring_max {
            let extra = g.ring.len() - g.ring_max;
            g.ring.drain(..extra);
        }
        let mut dead = Vec::new();
        for (id, sub) in g.subs.iter() {
            if sub.tx.try_send(chunk.clone()).is_err() {
                dead.push(*id);
            }
        }
        for id in dead {
            g.subs.remove(&id);
        }
    }

    fn close(&self) {
        let mut g = self.inner.lock().expect("fan");
        g.closed = true;
        g.subs.clear();
    }

    fn list(&self) -> Vec<ListenerRow> {
        let g = self.inner.lock().expect("fan");
        let mut rows: Vec<_> = g.subs.iter().map(|(id, sub)| ListenerRow {
            id: *id,
            ip: mask_ip(&sub.ip),
            connected_ms: sub.connected.elapsed().as_millis() as u64,
            bytes: sub.bytes.load(Ordering::Relaxed),
            user_agent: sub.user_agent.clone(),
        }).collect();
        rows.sort_by_key(|r| r.id);
        rows
    }

    fn kick(&self, id: u64) -> bool {
        self.inner.lock().expect("fan").subs.remove(&id).is_some()
    }

    fn kick_all(&self) -> usize {
        let mut g = self.inner.lock().expect("fan");
        let n = g.subs.len();
        g.subs.clear();
        n
    }

    fn bytes_sent(&self) -> u64 { self.inner.lock().unwrap_or_else(|e| e.into_inner()).bytes_sent }
    fn peak(&self) -> usize { self.inner.lock().unwrap_or_else(|e| e.into_inner()).peak }

    fn audible(&self, window: Duration) -> bool {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.last_audio.is_some_and(|t| t.elapsed() <= window)
    }
}

pub fn mask_ip(ip: &str) -> String {
    let ip = ip.split('%').next().unwrap_or(ip);
    let parts: Vec<&str> = ip.split('.').collect();
    if parts.len() == 4 && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit())) {
        return format!("{}.{}.{}.xxx", parts[0], parts[1], parts[2]);
    }
    if ip.contains(':') {
        let groups: Vec<&str> = ip.split(':').filter(|g| !g.is_empty()).collect();
        if groups.len() >= 2 {
            let head = groups.iter().take(2).cloned().collect::<Vec<_>>().join(":");
            return format!("{head}:xxxx");
        }
    }
    "xxx".into()
}

fn clean_agent(ua: &str) -> String {
    ua.chars().filter(|c| !c.is_control()).take(160).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::IdentityFactory;
    use std::io::{Read, Write};

    #[test]
    fn pcm_tap_drops_a_full_subscriber_without_blocking() {
        let tap = PcmTap::new();
        let rx = tap.subscribe();
        let frame = [0u8, 1, 2, 3];
        for _ in 0..TAP_CAP {
            tap.publish(&frame, 48_000, 2);
        }
        let started = Instant::now();
        tap.publish(&frame, 48_000, 2);
        assert!(started.elapsed() < Duration::from_millis(50));
        assert_eq!(tap.subs.lock().unwrap().len(), 0);
        assert!(rx.try_recv().is_ok());
        let kept = tap.subscribe();
        tap.publish(&[9, 0], 44_100, 1);
        let got = kept.recv_timeout(Duration::from_millis(50)).unwrap();
        assert_eq!(got.pcm, vec![9, 0]);
        assert_eq!(got.rate, 44_100);
        assert_eq!(got.channels, 1);
    }

    #[test]
    fn two_listeners_get_the_same_bytes() {
        let fan = Fanout::new(8, 1024);
        let a = fan.subscribe("a", "ua").unwrap();
        let b = fan.subscribe("b", "ua").unwrap();
        fan.broadcast(b"abc".to_vec());
        fan.broadcast(b"def".to_vec());
        assert_eq!(a.rx.recv().unwrap(), b"abc");
        assert_eq!(b.rx.recv().unwrap(), b"abc");
        assert_eq!(a.rx.recv().unwrap(), b"def");
        assert_eq!(b.rx.recv().unwrap(), b"def");
    }

    #[test]
    fn listener_cap() {
        let fan = Fanout::new(1, 1024);
        let _a = fan.subscribe("a", "ua").unwrap();
        assert!(matches!(fan.subscribe("b", "ua"), Err(Error::Limit)));
    }

    #[test]
    fn engine_builds_with_identity_factory() {
        let dir = std::env::temp_dir().join(format!("radio-eng-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let opts = EngineOpts {
            sample_rate: 8_000,
            channels: 2,
            frame: Duration::from_millis(5),
            fade: Duration::from_millis(20),
            meta_int: 800,
            max_conns_per_ip: 8,
            media_root: dir,
            fast: true,
        };
        let engine = Engine::new(Arc::new(IdentityFactory), Arc::new(NoTracks), opts);
        let hash = auth::hash_with("secret-pass", auth::HashParams::fast()).unwrap();
        engine.upsert(MountSpec {
            id: "m1".into(),
            station_id: "s1".into(),
            station_name: "Night".into(),
            genre: "jazz".into(),
            path: "/live".into(),
            source_user: "source".into(),
            pass_hash: hash,
            format: "mp3".into(),
            bitrate: 128_000,
            max_listeners: 4,
            max_bitrate: 320_000,
            allow_takeover: false,
            crossfade_ms: 20,
            autodj: true,
            shuffle: false,
            repeat: RepeatMode::All,
            jingle_every: 0,
            jingle_media_id: None,
            intro_media_id: None,
            gain_db: 0.0,
            sample_rate: 0,
            channels: 0,
            buffer_ms: 0,
            hls_segment_ms: 0,
            encoder: "ffmpeg".into(),
            relay_url: String::new(),
        });
        let start = Instant::now();
        let rows = loop {
            let rows = engine.status();
            if rows[0].playing && rows[0].bytes_sent > 0 {
                break rows;
            }
            if start.elapsed() > Duration::from_secs(2) {
                panic!("mount looked live before any audio reached the fanout");
            }
            thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(rows[0].mode, "autodj");
        assert!(rows[0].playing);
        assert_eq!(mask_ip("203.0.113.44"), "203.0.113.xxx");
        assert_eq!(mask_ip("2001:db8::1"), "2001:db8:xxxx");
        assert_eq!(engine.paths(), vec!["/live".to_string()]);
        engine.shutdown();
    }

    #[test]
    fn kick_releases_the_listener() {
        let dir = std::env::temp_dir().join(format!("radio-kick-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let opts = EngineOpts {
            sample_rate: 8_000,
            channels: 2,
            frame: Duration::from_millis(5),
            fade: Duration::from_millis(0),
            meta_int: 800,
            max_conns_per_ip: 8,
            media_root: dir,
            fast: true,
        };
        let engine = Arc::new(Engine::new(Arc::new(IdentityFactory), Arc::new(NoTracks), opts));
        let hash = auth::hash_with("secret-pass", auth::HashParams::fast()).unwrap();
        engine.upsert(MountSpec {
            id: "m1".into(),
            station_id: "s1".into(),
            station_name: "Night".into(),
            genre: "jazz".into(),
            path: "/live".into(),
            source_user: "source".into(),
            pass_hash: hash,
            format: "mp3".into(),
            bitrate: 128_000,
            max_listeners: 4,
            max_bitrate: 0,
            allow_takeover: false,
            crossfade_ms: 0,
            autodj: true,
            shuffle: false,
            repeat: RepeatMode::All,
            jingle_every: 0,
            jingle_media_id: None,
            intro_media_id: None,
            gain_db: 0.0,
            sample_rate: 0,
            channels: 0,
            buffer_ms: 0,
            hls_segment_ms: 0,
            encoder: "ffmpeg".into(),
            relay_url: String::new(),
        });
        let (tx, rx) = mpsc::channel();
        let eng = Arc::clone(&engine);
        thread::spawn(move || {
            let _ = eng.serve_listener("/live", "203.0.113.10", "RadioTest/9\nbad", false, std::io::sink());
            let _ = tx.send(());
        });
        let start = Instant::now();
        let id = loop {
            let rows = engine.status();
            if let Some(row) = rows.first().and_then(|s| s.listener_rows.first()) {
                assert_eq!(row.ip, "203.0.113.xxx");
                assert_eq!(row.user_agent, "RadioTest/9bad");
                break row.id;
            }
            if start.elapsed() > Duration::from_secs(3) {
                panic!("listener did not register");
            }
            thread::sleep(Duration::from_millis(20));
        };
        engine.kick("/live", id).unwrap();
        rx.recv_timeout(Duration::from_secs(2)).expect("kick did not release the listener");
        assert_eq!(engine.status()[0].listeners, 0);
        engine.shutdown();
    }

    #[test]
    fn exited_encoder_does_not_look_live_with_a_silent_fanout() {
        let dir = std::env::temp_dir().join(format!("radio-exit-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let opts = EngineOpts {
            sample_rate: 8_000,
            channels: 2,
            frame: Duration::from_millis(5),
            fade: Duration::from_millis(0),
            meta_int: 800,
            max_conns_per_ip: 4,
            media_root: dir,
            fast: true,
        };
        let engine = Engine::new(Arc::new(ExitOnce { hits: std::sync::atomic::AtomicU32::new(0) }), Arc::new(NoTracks), opts);
        let hash = auth::hash_with("secret-pass", auth::HashParams::fast()).unwrap();
        engine.upsert(test_mount(hash));
        let start = Instant::now();
        let mut recovered = false;
        while start.elapsed() < Duration::from_secs(3) {
            let st = &engine.status()[0];
            if st.bytes_sent == 0 {
                assert!(!st.playing, "playing with a silent fanout");
                assert!(!st.live, "live with a silent fanout");
                assert_eq!(st.mode, "idle");
                assert!(st.now.title.is_empty(), "now playing while the fanout is silent");
            } else if st.playing && st.mode == "autodj" && !st.live {
                recovered = true;
                break;
            }
            thread::sleep(Duration::from_millis(15));
        }
        assert!(recovered, "pump did not restart after the encoder exited");
        engine.shutdown();
    }

    #[test]
    fn failed_encoder_does_not_look_live() {
        let dir = std::env::temp_dir().join(format!("radio-fail-enc-{}", uniq_test()));
        let _ = std::fs::create_dir_all(&dir);
        let opts = EngineOpts {
            sample_rate: 8_000,
            channels: 2,
            frame: Duration::from_millis(20),
            fade: Duration::ZERO,
            meta_int: 800,
            max_conns_per_ip: 4,
            media_root: dir,
            fast: true,
        };
        let engine = Engine::new(Arc::new(FailEncode), Arc::new(NoTracks), opts);
        let hash = auth::hash_with("secret-pass", auth::HashParams::fast()).unwrap();
        engine.upsert(test_mount(hash));
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(700) {
            let st = &engine.status()[0];
            assert!(!st.playing, "a failed encoder looked live");
            assert!(!st.live, "a failed encoder looked live");
            assert_eq!(st.bytes_sent, 0);
            thread::sleep(Duration::from_millis(40));
        }
        let err = engine.set_metadata("/live", &"t".repeat(600), "a").unwrap_err();
        assert!(err.to_string().contains("too large"), "{err}");
        engine.shutdown();
    }

    #[test]
    fn opus_mount_plays_without_ffmpeg() {
        let dir = std::env::temp_dir().join(format!("radio-opus-{}", uniq_test()));
        let _ = std::fs::create_dir_all(&dir);
        let opts = EngineOpts {
            sample_rate: 44_100,
            channels: 2,
            frame: Duration::from_millis(20),
            fade: Duration::ZERO,
            meta_int: 16_000,
            max_conns_per_ip: 4,
            media_root: dir,
            fast: true,
        };
        let engine = Engine::new(Arc::new(crate::encoder::RouterFactory), Arc::new(NoTracks), opts);
        let hash = auth::hash_with("secret-pass", auth::HashParams::fast()).unwrap();
        let mut spec = test_mount(hash);
        spec.encoder = "opus".into();
        engine.upsert(spec);
        let start = Instant::now();
        loop {
            let st = &engine.status()[0];
            if st.playing && st.bytes_sent > 0 && st.encoder == "opus" {
                break;
            }
            if start.elapsed() > Duration::from_secs(3) {
                panic!("opus encoder did not reach listeners: playing={} bytes={}", st.playing, st.bytes_sent);
            }
            thread::sleep(Duration::from_millis(20));
        }
        engine.shutdown();
    }

    #[test]
    fn source_login_is_rate_limited() {
        let dir = std::env::temp_dir().join(format!("radio-src-limit-{}", uniq_test()));
        let _ = std::fs::create_dir_all(&dir);
        let opts = EngineOpts {
            sample_rate: 8_000,
            channels: 2,
            frame: Duration::from_millis(20),
            fade: Duration::ZERO,
            meta_int: 800,
            max_conns_per_ip: 4,
            media_root: dir,
            fast: true,
        };
        let engine = Engine::new(Arc::new(IdentityFactory), Arc::new(NoTracks), opts);
        let hash = auth::hash_with("secret-pass", auth::HashParams::fast()).unwrap();
        engine.upsert(test_mount(hash));
        let meta = SourceMeta {
            content_type: "audio/mpeg".into(),
            name: String::new(),
            url: String::new(),
            bitrate_bps: None,
            inline_metaint: None,
        };
        for _ in 0..8 {
            let err = match engine.open_source("/live", "source", "wrong", meta.clone(), Box::new(|| {}), "198.51.100.9") {
                Err(err) => err,
                Ok(_) => panic!("wrong source password was accepted"),
            };
            assert_eq!(err, Error::Unauthorized);
        }
        let err = match engine.open_source("/live", "source", "wrong", meta, Box::new(|| {}), "198.51.100.9") {
            Err(err) => err,
            Ok(_) => panic!("rate limit did not trip"),
        };
        assert_eq!(err, Error::Limit);
        engine.shutdown();
    }

    #[test]
    fn skip_unblocks_a_stuck_decoder() {
        let dir = std::env::temp_dir().join(format!("radio-skip-stuck-{}", uniq_test()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("track");
        std::fs::write(&file, b"track-bytes").unwrap();
        let tracks = Arc::new(TwoTracks {
            path: file,
            n: std::sync::atomic::AtomicU32::new(0),
            gen: AtomicU64::new(1),
        });
        let opts = EngineOpts {
            sample_rate: 8_000,
            channels: 2,
            frame: Duration::from_millis(5),
            fade: Duration::from_millis(0),
            meta_int: 800,
            max_conns_per_ip: 4,
            media_root: dir,
            fast: true,
        };
        let engine = Arc::new(Engine::new(
            Arc::new(StuckThen { calls: std::sync::atomic::AtomicU32::new(0) }),
            tracks,
            opts,
        ));
        let hash = auth::hash_with("secret-pass", auth::HashParams::fast()).unwrap();
        engine.upsert(test_mount(hash));
        let start = Instant::now();
        loop {
            let rows = engine.status();
            if rows[0].playing && rows[0].now.title == "First" {
                break;
            }
            if start.elapsed() > Duration::from_secs(2) {
                panic!("stuck track was reported playing as {:?}", rows[0].now.title);
            }
            thread::sleep(Duration::from_millis(15));
        }
        engine.skip("/live").unwrap();
        let start = Instant::now();
        loop {
            let rows = engine.status();
            if rows[0].playing && rows[0].now.title == "Second" {
                break;
            }
            if start.elapsed() > Duration::from_secs(2) {
                panic!("skip left the mount on {:?}", rows[0].now.title);
            }
            thread::sleep(Duration::from_millis(15));
        }
        engine.shutdown();
    }

    fn uniq_test() -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64
    }

    fn test_mount(hash: String) -> MountSpec {
        MountSpec {
            id: "m1".into(),
            station_id: "s1".into(),
            station_name: "Night".into(),
            genre: "jazz".into(),
            path: "/live".into(),
            source_user: "source".into(),
            pass_hash: hash,
            format: "mp3".into(),
            bitrate: 128_000,
            max_listeners: 4,
            max_bitrate: 320_000,
            allow_takeover: false,
            crossfade_ms: 0,
            autodj: true,
            shuffle: false,
            repeat: RepeatMode::All,
            jingle_every: 0,
            jingle_media_id: None,
            intro_media_id: None,
            gain_db: 0.0,
            sample_rate: 0,
            channels: 0,
            buffer_ms: 0,
            hls_segment_ms: 0,
            encoder: "ffmpeg".into(),
            relay_url: String::new(),
        }
    }

    struct FailEncode;

    impl AudioFactory for FailEncode {
        fn pcm(&self) -> bool { true }
        fn encode(&self, _: u32, _: u16, _: u32) -> std::io::Result<crate::encoder::EncodeEnds> {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "encoder opus failed to start"))
        }
        fn decode(&self, input: Box<dyn std::io::Read + Send>, _: &str, _: u32, _: u16) -> std::io::Result<Box<dyn std::io::Read + Send>> {
            Ok(input)
        }
    }

    struct ExitOnce {
        hits: std::sync::atomic::AtomicU32,
    }

    impl AudioFactory for ExitOnce {
        fn pcm(&self) -> bool { true }
        fn encode(&self, sample_rate: u32, channels: u16, bitrate: u32) -> std::io::Result<crate::encoder::EncodeEnds> {
            if self.hits.fetch_add(1, Ordering::SeqCst) == 0 {
                return Ok(crate::encoder::EncodeEnds {
                    input: Some(Box::new(Discard)),
                    output: Some(Box::new(EofRead)),
                    child: None,
                });
            }
            IdentityFactory.encode(sample_rate, channels, bitrate)
        }
        fn decode(&self, input: Box<dyn std::io::Read + Send>, _: &str, _: u32, _: u16) -> std::io::Result<Box<dyn std::io::Read + Send>> {
            Ok(input)
        }
    }

    struct Discard;
    impl std::io::Write for Discard {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> { Ok(buf.len()) }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    struct EofRead;
    impl std::io::Read for EofRead {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> { Ok(0) }
    }

    struct StuckThen {
        calls: std::sync::atomic::AtomicU32,
    }

    impl AudioFactory for StuckThen {
        fn pcm(&self) -> bool { true }
        fn encode(&self, sample_rate: u32, channels: u16, bitrate: u32) -> std::io::Result<crate::encoder::EncodeEnds> {
            IdentityFactory.encode(sample_rate, channels, bitrate)
        }
        fn decode(&self, input: Box<dyn std::io::Read + Send>, mime: &str, sample_rate: u32, channels: u16) -> std::io::Result<Box<dyn std::io::Read + Send>> {
            Ok(self.decode_cancellable(input, mime, sample_rate, channels)?.reader)
        }
        fn decode_cancellable(
            &self,
            input: Box<dyn std::io::Read + Send>,
            _: &str,
            _: u32,
            _: u16,
        ) -> std::io::Result<crate::encoder::CancellableDecode> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                let flag = Arc::new(AtomicBool::new(false));
                let cv = Arc::new(Condvar::new());
                let lock = Arc::new(Mutex::new(()));
                let reader = BlockReader { flag: Arc::clone(&flag), cv: Arc::clone(&cv), lock: Arc::clone(&lock) };
                let cancel = Box::new(move || {
                    let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
                    flag.store(true, Ordering::SeqCst);
                    cv.notify_all();
                });
                return Ok(crate::encoder::CancellableDecode { reader: Box::new(reader), cancel: Some(cancel) });
            }
            drop(input);
            Ok(crate::encoder::CancellableDecode {
                reader: Box::new(std::io::Cursor::new(b"SECOND-TRACK".to_vec())),
                cancel: None,
            })
        }
    }

    struct BlockReader {
        flag: Arc<AtomicBool>,
        cv: Arc<Condvar>,
        lock: Arc<Mutex<()>>,
    }

    impl std::io::Read for BlockReader {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            let mut guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
            while !self.flag.load(Ordering::SeqCst) {
                guard = self.cv.wait(guard).unwrap_or_else(|e| e.into_inner());
            }
            Ok(0)
        }
    }

    struct TwoTracks {
        path: PathBuf,
        n: std::sync::atomic::AtomicU32,
        gen: AtomicU64,
    }

    impl TrackSource for TwoTracks {
        fn next(&self, _: &str) -> Option<Track> {
            let i = self.n.fetch_add(1, Ordering::SeqCst);
            let title = if i == 0 { "First" } else { "Second" };
            Some(Track {
                title: title.into(),
                artist: String::new(),
                path: self.path.clone(),
                mime: "audio/mpeg".into(),
                media_id: String::new(),
                duration_ms: 1_000,
            })
        }
        fn play_generation(&self, _: &str) -> u64 { self.gen.load(Ordering::SeqCst) }
        fn skip(&self, _: &str) -> Result<(), Error> {
            self.gen.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn stream_settings_are_clamped() {
        assert_eq!(clamp_buffer_ms(0), 50);
        assert_eq!(clamp_buffer_ms(10_000), 1_000);
        assert_eq!(clamp_buffer_ms(250), 250);
        assert_eq!(clamp_hls_segment_ms(1), 250);
        assert_eq!(clamp_hls_segment_ms(9_000), 2_000);
        assert_eq!(clamp_hls_segment_ms(1_000), 1_000);
        assert_eq!(clamp_bitrate(1), 32_000);
        assert_eq!(clamp_bitrate(9_000_000), 320_000);
        assert_eq!(clamp_sample_rate(0), 44_100);
        assert_eq!(clamp_sample_rate(12_345), 11_025);
        assert_eq!(clamp_channels(1), 1);
        assert_eq!(clamp_channels(0), 2);
        assert_eq!(clamp_gain_db(80.0), 24.0);
        assert_eq!(clamp_gain_db(-80.0), -24.0);
        assert_eq!(clamp_crossfade_ms(99_000), 10_000);
    }

    #[test]
    fn relay_fixture_reaches_a_listener() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.set_read_timeout(Some(Duration::from_secs(2))).ok();
            let mut req = [0u8; 1024];
            let _ = sock.read(&mut req);
            let body = SILENCE_MP3;
            let head = format!(
                "HTTP/1.0 200 OK\r\nContent-Type: audio/mpeg\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(head.as_bytes());
            let _ = sock.write_all(body);
            thread::sleep(Duration::from_secs(2));
        });
        let dir = std::env::temp_dir().join(format!("radio-relay-{}", uniq_test()));
        let _ = std::fs::create_dir_all(&dir);
        let opts = EngineOpts {
            sample_rate: 8_000,
            channels: 2,
            frame: Duration::from_millis(5),
            fade: Duration::ZERO,
            meta_int: 800,
            max_conns_per_ip: 8,
            media_root: dir,
            fast: true,
        };
        let engine = Arc::new(Engine::new(Arc::new(IdentityFactory), Arc::new(NoTracks), opts));
        engine.set_relay_policy(true, Vec::new());
        let mut spec = test_mount(auth::hash_with("secret-pass", auth::HashParams::fast()).unwrap());
        spec.relay_url = format!("http://{addr}/live");
        spec.autodj = true;
        engine.upsert(spec);
        let got = Arc::new(Mutex::new(Vec::new()));
        let store = Arc::clone(&got);
        let eng = Arc::clone(&engine);
        let handle = thread::spawn(move || {
            struct Cap(Arc<Mutex<Vec<u8>>>);
            impl Write for Cap {
                fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                    let mut g = self.0.lock().unwrap_or_else(|e| e.into_inner());
                    g.extend_from_slice(buf);
                    if g.windows(2).any(|w| w == [0xff, 0xfb]) {
                        return Err(io::Error::new(io::ErrorKind::BrokenPipe, "got mp3"));
                    }
                    Ok(buf.len())
                }
                fn flush(&mut self) -> io::Result<()> { Ok(()) }
            }
            let _ = eng.serve_listener("/live", "203.0.113.9", "relay-test", false, Cap(store));
        });
        let start = Instant::now();
        loop {
            let bytes = got.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if bytes.windows(2).any(|w| w == [0xff, 0xfb]) {
                break;
            }
            if start.elapsed() > Duration::from_secs(8) {
                panic!("relay bytes did not reach the listener ({})", bytes.len());
            }
            thread::sleep(Duration::from_millis(30));
        }
        engine.shutdown();
        let _ = handle.join();
    }

    #[test]
    fn dead_relay_does_not_wedge_the_mount() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let dir = std::env::temp_dir().join(format!("radio-dead-{}", uniq_test()));
        let _ = std::fs::create_dir_all(&dir);
        let opts = EngineOpts {
            sample_rate: 8_000,
            channels: 2,
            frame: Duration::from_millis(5),
            fade: Duration::ZERO,
            meta_int: 800,
            max_conns_per_ip: 4,
            media_root: dir,
            fast: true,
        };
        let engine = Engine::new(Arc::new(IdentityFactory), Arc::new(NoTracks), opts);
        engine.set_relay_policy(true, Vec::new());
        let mut spec = test_mount(auth::hash_with("secret-pass", auth::HashParams::fast()).unwrap());
        spec.relay_url = format!("http://{addr}/live");
        engine.upsert(spec);
        let start = Instant::now();
        loop {
            let rows = engine.status();
            assert_eq!(rows.len(), 1);
            if rows[0].playing && rows[0].bytes_sent > 0 && rows[0].mode != "relay" {
                break;
            }
            if start.elapsed() > Duration::from_secs(12) {
                panic!("dead relay wedged the mount: playing={} mode={}", rows[0].playing, rows[0].mode);
            }
            thread::sleep(Duration::from_millis(40));
        }
        engine.shutdown();
    }

    #[test]
    fn hls_playlist_uses_the_public_url_and_ignores_an_untrusted_host() {
        let dir = std::env::temp_dir().join(format!("radio-hls-{}", uniq_test()));
        let _ = std::fs::create_dir_all(&dir);
        let opts = EngineOpts {
            sample_rate: 8_000,
            channels: 2,
            frame: Duration::from_millis(5),
            fade: Duration::ZERO,
            meta_int: 800,
            max_conns_per_ip: 4,
            media_root: dir,
            fast: true,
        };
        let engine = Arc::new(Engine::new(Arc::new(IdentityFactory), Arc::new(NoTracks), opts));
        engine.set_stream_public("https://radio.example.com".into());
        engine.set_trusted(crate::limit::Trusted::parse(&["10.9.9.9".into()]));
        let mut spec = test_mount(auth::hash_with("secret-pass", auth::HashParams::fast()).unwrap());
        spec.hls_segment_ms = 250;
        engine.upsert(spec);
        let start = Instant::now();
        let body = loop {
            let body = engine.hls_playlist("/live").unwrap_or_default();
            if body.contains("https://radio.example.com/live/seg/") {
                break body;
            }
            if start.elapsed() > Duration::from_secs(4) {
                panic!("playlist did not use the public url:\n{body}");
            }
            thread::sleep(Duration::from_millis(30));
        };
        assert!(!body.contains("127.0.0.1"));
        let handle = crate::stream::spawn("127.0.0.1:0", Arc::clone(&engine), Duration::from_secs(2)).unwrap();
        let mut sock = std::net::TcpStream::connect(handle.addr).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let req = b"GET /live/index.m3u8 HTTP/1.0\r\nHost: 127.0.0.1\r\nX-Forwarded-Host: evil.example\r\nX-Forwarded-Proto: http\r\n\r\n";
        sock.write_all(req).unwrap();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 2048];
        loop {
            match sock.read(&mut tmp) {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        }
        let text = String::from_utf8_lossy(&buf);
        assert!(text.contains("https://radio.example.com/live/seg/"), "{text}");
        assert!(!text.contains("evil.example"), "{text}");
        drop(handle);
        engine.shutdown();
    }

    #[test]
    fn seek_rejects_a_live_source() {
        let dir = std::env::temp_dir().join(format!("radio-seek-live-{}", uniq_test()));
        let _ = std::fs::create_dir_all(&dir);
        let opts = EngineOpts {
            sample_rate: 8_000,
            channels: 2,
            frame: Duration::from_millis(20),
            fade: Duration::ZERO,
            meta_int: 800,
            max_conns_per_ip: 4,
            media_root: dir,
            fast: true,
        };
        let engine = Engine::new(Arc::new(IdentityFactory), Arc::new(NoTracks), opts);
        let hash = auth::hash_with("secret-pass", auth::HashParams::fast()).unwrap();
        engine.upsert(test_mount(hash));
        let _guard = engine.open_source(
            "/live",
            "source",
            "secret-pass",
            SourceMeta {
                content_type: "audio/mpeg".into(),
                name: "Live".into(),
                url: String::new(),
                bitrate_bps: None,
                inline_metaint: None,
            },
            Box::new(|| {}),
            "203.0.113.8",
        ).unwrap();
        let err = engine.seek("/live", 1_000).unwrap_err();
        assert_eq!(err.to_string(), "cannot seek a live source");
    }

    struct CountWrite(Arc<AtomicU64>);
    impl Write for CountWrite {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.fetch_add(buf.len() as u64, Ordering::Relaxed);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }

    fn fast_opts(dir: PathBuf) -> EngineOpts {
        EngineOpts {
            sample_rate: 8_000,
            channels: 2,
            frame: Duration::from_millis(5),
            fade: Duration::ZERO,
            meta_int: 800,
            max_conns_per_ip: 8,
            media_root: dir,
            fast: true,
        }
    }

    fn fds_pointing_at(path: &Path) -> usize {
        let Ok(want) = path.canonicalize() else { return 0 };
        let Ok(dir) = std::fs::read_dir("/proc/self/fd") else { return 0 };
        dir.flatten().filter(|ent| std::fs::read_link(ent.path()).ok().as_ref() == Some(&want)).count()
    }

    #[test]
    fn record_file_is_nonempty_and_stop_closes_it() {
        let dir = std::env::temp_dir().join(format!("radio-rec-{}", uniq_test()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("media")).unwrap();
        let engine = Engine::new(Arc::new(IdentityFactory), Arc::new(NoTracks), fast_opts(dir.join("media")));
        engine.set_recordings_root(dir.join("recordings"));
        let hash = auth::hash_with("secret-pass", auth::HashParams::fast()).unwrap();
        engine.upsert(test_mount(hash));
        let file = dir.join("live.mp3");
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(3) && engine.status()[0].bytes_sent == 0 {
            thread::sleep(Duration::from_millis(15));
        }
        assert!(engine.status()[0].bytes_sent > 0, "mount produced no audio");
        engine.start_recording("/live", crate::record::Job {
            id: "rec1".into(),
            path: file.clone(),
            format: "mp3".into(),
            max_duration: Duration::from_secs(30),
            budget: 8 << 20,
            fail_writes: false,
        }).unwrap();
        let start = Instant::now();
        loop {
            if file.metadata().map(|m| m.len() > 0).unwrap_or(false) {
                break;
            }
            if start.elapsed() > Duration::from_secs(3) {
                panic!("recording file stayed empty");
            }
            thread::sleep(Duration::from_millis(15));
        }
        let fin = engine.stop_recording("/live").unwrap();
        assert!(fin.bytes > 0);
        assert!(!engine.recording_live("/live").unwrap().active);
        let len = file.metadata().unwrap().len();
        thread::sleep(Duration::from_millis(80));
        assert_eq!(file.metadata().unwrap().len(), len, "stop left the file open");
        assert_eq!(fds_pointing_at(&file), 0, "recording file is still open");
        assert!(!std::fs::read(&file).unwrap().is_empty());
        assert!(engine.status()[0].bytes_sent > 0);
        engine.shutdown();
    }

    #[test]
    fn write_failure_does_not_stop_listeners() {
        let dir = std::env::temp_dir().join(format!("radio-rec-fail-{}", uniq_test()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("media")).unwrap();
        let engine = Arc::new(Engine::new(Arc::new(IdentityFactory), Arc::new(NoTracks), fast_opts(dir.join("media"))));
        let hash = auth::hash_with("secret-pass", auth::HashParams::fast()).unwrap();
        engine.upsert(test_mount(hash));
        let got = Arc::new(AtomicU64::new(0));
        let eng = Arc::clone(&engine);
        let counter = Arc::clone(&got);
        thread::spawn(move || {
            let _ = eng.serve_listener("/live", "203.0.113.9", "rec-test", false, CountWrite(counter));
        });
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(3) && (engine.status()[0].bytes_sent == 0 || got.load(Ordering::Relaxed) == 0) {
            thread::sleep(Duration::from_millis(15));
        }
        assert!(got.load(Ordering::Relaxed) > 0, "listener heard nothing before the failure");
        let file = dir.join("fail.mp3");
        engine.start_recording("/live", crate::record::Job {
            id: "bad".into(),
            path: file,
            format: "mp3".into(),
            max_duration: Duration::from_secs(30),
            budget: 8 << 20,
            fail_writes: true,
        }).unwrap();
        let start = Instant::now();
        loop {
            let err = engine.recording_live("/live").map(|l| l.error).unwrap_or_default();
            if err.contains("disk write") {
                break;
            }
            if start.elapsed() > Duration::from_secs(3) {
                panic!("write failure was not reported: {err}");
            }
            thread::sleep(Duration::from_millis(15));
        }
        let heard = got.load(Ordering::Relaxed);
        let sent = engine.status()[0].bytes_sent;
        thread::sleep(Duration::from_millis(120));
        let st = &engine.status()[0];
        assert!(st.playing, "a failed recording stopped the mount");
        assert!(st.bytes_sent > sent, "listeners stopped receiving audio");
        assert!(got.load(Ordering::Relaxed) > heard, "the listener stopped after the write failed");
        assert!(!engine.recording_live("/live").unwrap().active);
        engine.shutdown();
    }

    #[test]
    fn recording_files_stay_under_the_recordings_root() {
        let dir = std::env::temp_dir().join(format!("radio-rec-root-{}", uniq_test()));
        let _ = std::fs::remove_dir_all(&dir);
        let media = dir.join("media");
        let rec = dir.join("recordings");
        std::fs::create_dir_all(media.join("ok")).unwrap();
        std::fs::create_dir_all(rec.join("mount")).unwrap();
        std::fs::write(media.join("ok").join("track"), b"media").unwrap();
        std::fs::write(rec.join("mount").join("a.mp3"), b"rec").unwrap();
        std::fs::write(dir.join("secret"), b"nope").unwrap();
        assert!(open_media(&media, &rec, &media.join("ok").join("track")).is_ok());
        assert!(open_media(&media, &rec, &rec.join("mount").join("a.mp3")).is_ok());
        assert!(open_media(&media, &rec, &dir.join("secret")).is_err());
    }
}
