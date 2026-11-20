//! Encoder boundary. Each mount chooses `ffmpeg`, in-process `native` MP3, or in-process `opus`.
//!
//! `FfmpegFactory` shells out to ffmpeg. `RouterFactory` is what `navad` uses:
//! libmp3lame or libopus inside the process, or ffmpeg when that mount asks
//! for it. `IdentityFactory` copies PCM and is what the tests use.
//! `PassthroughFactory` reports `pcm() == false` so the engine switches
//! already-encoded bytes without calling an encoder.

use std::io::{self, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread;
use std::time::{Duration, Instant};

pub struct EncodeEnds {
    pub input: Option<Box<dyn Write + Send>>,
    pub output: Option<Box<dyn Read + Send>>,
    pub child: Option<SharedChild>,
}

impl Drop for EncodeEnds {
    fn drop(&mut self) {
        if let Some(child) = self.child.take() {
            child.shutdown();
        }
    }
}

struct ChildSlot {
    child: Mutex<Option<Child>>,
}

static CHILDREN: Mutex<Vec<Weak<ChildSlot>>> = Mutex::new(Vec::new());

/// A ffmpeg process that shutdown can kill without waiting on the thread that owns it.
pub struct SharedChild {
    slot: Arc<ChildSlot>,
}

impl SharedChild {
    fn new(child: Child) -> Self {
        let slot = Arc::new(ChildSlot { child: Mutex::new(Some(child)) });
        if let Ok(mut slots) = CHILDREN.lock() {
            slots.retain(|w| w.strong_count() > 0);
            slots.push(Arc::downgrade(&slot));
        }
        Self { slot }
    }

    pub fn shutdown(&self) {
        reap_slot(&self.slot);
    }
}

impl Drop for SharedChild {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn reap_slot(slot: &ChildSlot) {
    let child = match slot.child.lock() {
        Ok(mut guard) => guard.take(),
        Err(poisoned) => poisoned.into_inner().take(),
    };
    let Some(mut child) = child else { return };
    let _ = child.kill();
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => break,
            Ok(None) if start.elapsed() > Duration::from_secs(1) => {
                std::mem::forget(child);
                break;
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// Kill every ffmpeg process this server started. Safe to call from the signal path.
pub fn kill_all() {
    let slots: Vec<Arc<ChildSlot>> = CHILDREN.lock().map(|g| g.iter().filter_map(|w| w.upgrade()).collect()).unwrap_or_default();
    for slot in slots {
        reap_slot(&slot);
    }
}

pub trait AudioFactory: Send + Sync {
    fn pcm(&self) -> bool;
    fn encode(&self, sample_rate: u32, channels: u16, bitrate: u32) -> io::Result<EncodeEnds>;
    fn decode(
        &self,
        input: Box<dyn Read + Send>,
        mime: &str,
        sample_rate: u32,
        channels: u16,
    ) -> io::Result<Box<dyn Read + Send>>;

    /// Same as [`AudioFactory::decode`], plus a cancel handle that unblocks a stuck read.
    /// ffmpeg decode hangs on a bad frame unless the child is killed from another thread.
    fn decode_cancellable(
        &self,
        input: Box<dyn Read + Send>,
        mime: &str,
        sample_rate: u32,
        channels: u16,
    ) -> io::Result<CancellableDecode> {
        Ok(CancellableDecode { reader: self.decode(input, mime, sample_rate, channels)?, cancel: None })
    }

    /// Mounts pick `ffmpeg` or `native`. The default ignores the name.
    fn encode_with(&self, kind: &str, sample_rate: u32, channels: u16, bitrate: u32) -> io::Result<EncodeEnds> {
        let _ = kind;
        self.encode(sample_rate, channels, bitrate)
    }

    /// Decode with the encoder the mount selected. The default ignores the name.
    fn decode_kind(
        &self,
        kind: &str,
        input: Box<dyn Read + Send>,
        mime: &str,
        sample_rate: u32,
        channels: u16,
    ) -> io::Result<CancellableDecode> {
        let _ = kind;
        self.decode_cancellable(input, mime, sample_rate, channels)
    }
}

pub struct CancellableDecode {
    pub reader: Box<dyn Read + Send>,
    pub cancel: Option<Box<dyn FnOnce() + Send>>,
}

pub fn ffmpeg_available() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| {
        Command::new("ffmpeg")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

pub fn is_pcm_mime(mime: &str) -> bool {
    let m = mime.to_ascii_lowercase();
    m.starts_with("audio/l16") || m.starts_with("audio/pcm") || m == "application/x-pcm"
}

pub struct IdentityFactory;

impl AudioFactory for IdentityFactory {
    fn pcm(&self) -> bool { true }

    fn encode(&self, _: u32, _: u16, _: u32) -> io::Result<EncodeEnds> {
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(32);
        Ok(EncodeEnds {
            input: Some(Box::new(ChanWriter { tx: Some(tx) })),
            output: Some(Box::new(ChanReader { rx, buf: Vec::new(), pos: 0 })),
            child: None,
        })
    }

    fn decode(&self, input: Box<dyn Read + Send>, _: &str, _: u32, _: u16) -> io::Result<Box<dyn Read + Send>> {
        Ok(input)
    }
}

pub struct PassthroughFactory;

impl AudioFactory for PassthroughFactory {
    fn pcm(&self) -> bool { false }
    fn encode(&self, _: u32, _: u16, _: u32) -> io::Result<EncodeEnds> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "passthrough does not re-encode"))
    }
    fn decode(&self, _: Box<dyn Read + Send>, _: &str, _: u32, _: u16) -> io::Result<Box<dyn Read + Send>> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "passthrough does not decode"))
    }
}

pub struct FfmpegFactory;

fn ffmpeg_command() -> Command {
    let mut cmd = Command::new("ffmpeg");
    // A terminal Ctrl+C is delivered to the whole foreground group. Encoder
    // children sit in their own group so the signal handler can stop them.
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0000_0200);
    }
    cmd
}

impl AudioFactory for FfmpegFactory {
    fn pcm(&self) -> bool { true }

    fn encode(&self, sample_rate: u32, channels: u16, bitrate: u32) -> io::Result<EncodeEnds> {
        if !ffmpeg_available() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "ffmpeg encoder selected but ffmpeg is not installed"));
        }
        let kbps = (bitrate / 1000).max(32);
        let sr = sample_rate.to_string();
        let ch = channels.to_string();
        let br = format!("{kbps}k");
        let mut child = ffmpeg_command()
            .args([
                "-hide_banner", "-loglevel", "error", "-nostdin",
                "-f", "s16le", "-ar", &sr, "-ac", &ch,
                "-i", "pipe:0",
                "-c:a", "libmp3lame", "-b:a", &br,
                "-f", "mp3", "-write_xing", "0",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let input = child.stdin.take().ok_or_else(|| io::Error::other("ffmpeg stdin"))?;
        let output = child.stdout.take().ok_or_else(|| io::Error::other("ffmpeg stdout"))?;
        Ok(EncodeEnds { input: Some(Box::new(input)), output: Some(Box::new(output)), child: Some(SharedChild::new(child)) })
    }

    fn decode(&self, input: Box<dyn Read + Send>, mime: &str, sample_rate: u32, channels: u16) -> io::Result<Box<dyn Read + Send>> {
        Ok(self.decode_cancellable(input, mime, sample_rate, channels)?.reader)
    }

    fn decode_cancellable(
        &self,
        input: Box<dyn Read + Send>,
        mime: &str,
        sample_rate: u32,
        channels: u16,
    ) -> io::Result<CancellableDecode> {
        if is_pcm_mime(mime) {
            return Ok(CancellableDecode { reader: input, cancel: None });
        }
        if !ffmpeg_available() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "ffmpeg not installed"));
        }
        let sr = sample_rate.to_string();
        let ch = channels.to_string();
        let mut child = ffmpeg_command()
            .args([
                "-hide_banner", "-loglevel", "error", "-nostdin",
                "-fflags", "nobuffer", "-flags", "low_delay",
                "-probesize", "32768", "-analyzeduration", "0",
                "-i", "pipe:0",
                "-f", "s16le", "-ac", &ch, "-ar", &sr,
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut stdin = child.stdin.take().ok_or_else(|| io::Error::other("ffmpeg stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| io::Error::other("ffmpeg stdout"))?;
        thread::spawn(move || {
            let mut input = input;
            let _ = io::copy(&mut input, &mut stdin);
            drop(stdin);
        });
        let child = SharedChild::new(child);
        let child_kill = SharedChild { slot: Arc::clone(&child.slot) };
        Ok(CancellableDecode {
            reader: Box::new(OwnedDecode { stdout, child }),
            cancel: Some(Box::new(move || child_kill.shutdown())),
        })
    }
}

struct OwnedDecode {
    stdout: std::process::ChildStdout,
    child: SharedChild,
}

impl Read for OwnedDecode {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stdout.read(buf)
    }
}

impl Drop for OwnedDecode {
    fn drop(&mut self) {
        self.child.shutdown();
    }
}

struct ChanWriter {
    tx: Option<SyncSender<Vec<u8>>>,
}

impl Write for ChanWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let tx = self.tx.as_ref().ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "closed"))?;
        // A pump that stops reading must not block the encoder thread forever.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut pending = buf.to_vec();
        loop {
            match tx.try_send(pending) {
                Ok(()) => return Ok(buf.len()),
                Err(TrySendError::Disconnected(_)) => {
                    return Err(io::Error::new(io::ErrorKind::BrokenPipe, "encoder output closed"));
                }
                Err(TrySendError::Full(back)) => {
                    pending = back;
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "encoder output stalled"));
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

impl Drop for ChanWriter {
    fn drop(&mut self) { self.tx.take(); }
}

struct ChanReader {
    rx: Receiver<Vec<u8>>,
    buf: Vec<u8>,
    pos: usize,
}

impl Read for ChanReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.pos >= self.buf.len() {
            match self.rx.recv() {
                Ok(v) if v.is_empty() => continue,
                Ok(v) => { self.buf = v; self.pos = 0; }
                Err(_) => return Ok(0),
            }
        }
        if self.buf.is_empty() {
            return Ok(0);
        }
        let n = (self.buf.len() - self.pos).min(out.len());
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Production factory. Mounts name `ffmpeg`, `native`, or `opus`.
pub struct RouterFactory;

impl AudioFactory for RouterFactory {
    fn pcm(&self) -> bool { true }

    fn encode(&self, sample_rate: u32, channels: u16, bitrate: u32) -> io::Result<EncodeEnds> {
        self.encode_with("native", sample_rate, channels, bitrate)
    }

    fn encode_with(&self, kind: &str, sample_rate: u32, channels: u16, bitrate: u32) -> io::Result<EncodeEnds> {
        match kind {
            "native" => native_encode(sample_rate, channels, bitrate),
            "opus" => opus_encode(sample_rate, channels, bitrate),
            "ffmpeg" => FfmpegFactory.encode(sample_rate, channels, bitrate),
            other => Err(io::Error::new(io::ErrorKind::InvalidInput, format!("unknown encoder {other}"))),
        }
    }

    fn decode(&self, input: Box<dyn Read + Send>, mime: &str, sample_rate: u32, channels: u16) -> io::Result<Box<dyn Read + Send>> {
        Ok(self.decode_kind("native", input, mime, sample_rate, channels)?.reader)
    }

    fn decode_cancellable(
        &self,
        input: Box<dyn Read + Send>,
        mime: &str,
        sample_rate: u32,
        channels: u16,
    ) -> io::Result<CancellableDecode> {
        self.decode_kind("native", input, mime, sample_rate, channels)
    }

    fn decode_kind(
        &self,
        kind: &str,
        input: Box<dyn Read + Send>,
        mime: &str,
        sample_rate: u32,
        channels: u16,
    ) -> io::Result<CancellableDecode> {
        if is_pcm_mime(mime) {
            return Ok(CancellableDecode { reader: input, cancel: None });
        }
        match kind {
            "ffmpeg" => FfmpegFactory.decode_cancellable(input, mime, sample_rate, channels),
            "native" | "opus" => native_decode(input, mime, sample_rate, channels),
            other => Err(io::Error::new(io::ErrorKind::InvalidInput, format!("unknown encoder {other}"))),
        }
    }
}

/// Content-Type a listener receives for this encoder id.
pub fn content_type(kind: &str) -> &'static str {
    if kind == "opus" { "audio/ogg" } else { "audio/mpeg" }
}

/// Encoder ids a client can store on a mount. `ffmpeg` is omitted when the binary is missing.
pub fn available_encoders() -> Vec<&'static str> {
    let mut ids = Vec::with_capacity(3);
    if ffmpeg_available() {
        ids.push("ffmpeg");
    }
    ids.push("native");
    ids.push("opus");
    ids
}

pub fn normalize_encoder(name: &str) -> Result<&'static str, crate::error::Error> {
    match name {
        "ffmpeg" => {
            if !ffmpeg_available() {
                Err(crate::error::Error::Invalid("ffmpeg encoder selected but ffmpeg is not installed".into()))
            } else {
                Ok("ffmpeg")
            }
        }
        "native" => Ok("native"),
        "opus" => Ok("opus"),
        _ => Err(crate::error::Error::Invalid("encoder must be ffmpeg, native, or opus".into())),
    }
}

pub fn default_encoder() -> &'static str {
    if ffmpeg_available() { "ffmpeg" } else { "native" }
}

fn nearest_bitrate(bps: u32) -> mp3lame_encoder::Bitrate {
    use mp3lame_encoder::Bitrate::*;
    const ALL: [(u32, mp3lame_encoder::Bitrate); 14] = [
        (8, Kbps8), (16, Kbps16), (24, Kbps24), (32, Kbps32), (40, Kbps40), (48, Kbps48),
        (64, Kbps64), (80, Kbps80), (96, Kbps96), (112, Kbps112), (128, Kbps128),
        (160, Kbps160), (192, Kbps192), (224, Kbps224),
    ];
    // 256 and 320 are appended below so the match stays readable.
    let kbps = (bps / 1000).clamp(8, 320);
    let mut best = mp3lame_encoder::Bitrate::Kbps128;
    let mut dist = u32::MAX;
    for (k, rate) in ALL {
        let d = k.abs_diff(kbps);
        if d < dist {
            dist = d;
            best = rate;
        }
    }
    for (k, rate) in [(256u32, mp3lame_encoder::Bitrate::Kbps256), (320u32, mp3lame_encoder::Bitrate::Kbps320)] {
        let d = k.abs_diff(kbps);
        if d < dist {
            dist = d;
            best = rate;
        }
    }
    best
}

fn native_encode(sample_rate: u32, channels: u16, bitrate: u32) -> io::Result<EncodeEnds> {
    use mp3lame_encoder::{Builder, Mode, Quality, VbrMode};
    let ch = if channels <= 1 { 1u8 } else { 2 };
    let mut builder = Builder::new()
        .ok_or_else(|| io::Error::other("native mp3 encoder: lame init failed"))?
        .with_num_channels(ch)
        .and_then(|b| b.with_sample_rate(sample_rate))
        .and_then(|b| b.with_brate(nearest_bitrate(bitrate)))
        .and_then(|b| b.with_quality(Quality::Good))
        .and_then(|b| b.with_vbr_mode(VbrMode::Off))
        .and_then(|b| b.with_to_write_vbr_tag(false))
        .map_err(|err| io::Error::other(format!("native mp3 encoder: {err}")))?;
    if ch == 1 {
        builder = builder.with_mode(Mode::Mono).map_err(|err| io::Error::other(format!("native mp3 encoder: {err}")))?;
    } else {
        builder = builder.with_mode(Mode::JointStereo).map_err(|err| io::Error::other(format!("native mp3 encoder: {err}")))?;
    }
    let enc = builder.build().map_err(|err| io::Error::other(format!("native mp3 encoder: {err}")))?;
    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(32);
    let input = NativeWriter { enc, tx: Some(tx), pending: Vec::new(), channels: ch };
    Ok(EncodeEnds {
        input: Some(Box::new(input)),
        output: Some(Box::new(ChanReader { rx, buf: Vec::new(), pos: 0 })),
        child: None,
    })
}

struct NativeWriter {
    enc: mp3lame_encoder::Encoder,
    tx: Option<SyncSender<Vec<u8>>>,
    pending: Vec<u8>,
    channels: u8,
}

impl NativeWriter {
    fn push_samples(&mut self, samples: &[i16]) -> io::Result<()> {
        if samples.is_empty() {
            return Ok(());
        }
        let mut out = Vec::new();
        out.reserve(mp3lame_encoder::max_required_buffer_size(samples.len()));
        let written = if self.channels == 1 {
            self.enc.encode_to_vec(mp3lame_encoder::MonoPcm(samples), &mut out)
        } else {
            self.enc.encode_to_vec(mp3lame_encoder::InterleavedPcm(samples), &mut out)
        }.map_err(|err| io::Error::other(format!("native mp3 encoder: {err}")))?;
        if written == 0 || out.is_empty() {
            return Ok(());
        }
        self.send(out)
    }

    fn send(&mut self, bytes: Vec<u8>) -> io::Result<()> {
        let tx = self.tx.as_ref().ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "closed"))?;
        send_bounded(tx, bytes)
    }
}

fn send_bounded(tx: &SyncSender<Vec<u8>>, bytes: Vec<u8>) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut pending = bytes;
    loop {
        match tx.try_send(pending) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Disconnected(_)) => {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "encoder output closed"));
            }
            Err(TrySendError::Full(back)) => {
                pending = back;
                if Instant::now() >= deadline {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "encoder output stalled"));
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

impl Write for NativeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.pending.extend_from_slice(buf);
        let width = (self.channels as usize) * 2;
        let usable = self.pending.len() - (self.pending.len() % width);
        if usable >= width * 576 {
            let bytes = self.pending.drain(..usable).collect::<Vec<_>>();
            let mut samples = Vec::with_capacity(bytes.len() / 2);
            for frame in bytes.chunks_exact(2) {
                samples.push(i16::from_le_bytes([frame[0], frame[1]]));
            }
            self.push_samples(&samples)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

impl Drop for NativeWriter {
    fn drop(&mut self) {
        let width = (self.channels as usize) * 2;
        if self.pending.len() >= width {
            let usable = self.pending.len() - (self.pending.len() % width);
            let bytes = self.pending.drain(..usable).collect::<Vec<_>>();
            let mut samples = Vec::with_capacity(bytes.len() / 2);
            for frame in bytes.chunks_exact(2) {
                samples.push(i16::from_le_bytes([frame[0], frame[1]]));
            }
            let _ = self.push_samples(&samples);
        }
        let mut out = Vec::new();
        out.reserve(7200);
        if self.enc.flush_to_vec::<mp3lame_encoder::FlushNoGap>(&mut out).is_ok() && !out.is_empty() {
            let _ = self.send(out);
        }
        self.tx.take();
    }
}

fn native_decode(
    input: Box<dyn Read + Send>,
    mime: &str,
    sample_rate: u32,
    channels: u16,
) -> io::Result<CancellableDecode> {
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = Arc::clone(&cancel);
    let reader = SymReader::open(input, mime, sample_rate, channels.max(1), flag)?;
    Ok(CancellableDecode {
        reader: Box::new(reader),
        cancel: Some(Box::new(move || cancel.store(true, std::sync::atomic::Ordering::SeqCst))),
    })
}

struct IoSrc<R> {
    inner: R,
    cancel: Arc<std::sync::atomic::AtomicBool>,
}

impl<R: Read> Read for IoSrc<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        self.inner.read(buf)
    }
}

impl<R> std::io::Seek for IoSrc<R> {
    fn seek(&mut self, _: std::io::SeekFrom) -> io::Result<u64> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "not seekable"))
    }
}

unsafe impl<R: Send> Sync for IoSrc<R> {}

impl<R: Read + Send> symphonia::core::io::MediaSource for IoSrc<R> {
    fn is_seekable(&self) -> bool { false }
    fn byte_len(&self) -> Option<u64> { None }
}

struct SymReader {
    format: Box<dyn symphonia::core::formats::FormatReader>,
    decoder: Box<dyn symphonia::core::codecs::Decoder>,
    track_id: u32,
    out_rate: u32,
    out_ch: usize,
    pending: Vec<u8>,
    pos: usize,
    phase: f64,
}

impl SymReader {
    fn open(
        input: Box<dyn Read + Send>,
        mime: &str,
        sample_rate: u32,
        channels: u16,
        cancel: Arc<std::sync::atomic::AtomicBool>,
    ) -> io::Result<Self> {
        use symphonia::core::codecs::DecoderOptions;
        use symphonia::core::formats::FormatOptions;
        use symphonia::core::io::MediaSourceStream;
        use symphonia::core::meta::MetadataOptions;
        use symphonia::core::probe::Hint;
        let src = IoSrc { inner: input, cancel };
        let mss = MediaSourceStream::new(Box::new(src), Default::default());
        let mut hint = Hint::new();
        let mime_l = mime.to_ascii_lowercase();
        if mime_l.contains("wav") {
            hint.with_extension("wav");
        } else {
            hint.with_extension("mp3");
        }
        let probed = symphonia::default::get_probe()
            .format(&hint, mss, &FormatOptions { enable_gapless: false, ..Default::default() }, &MetadataOptions::default())
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, format!("native decoder: {err}")))?;
        let format = probed.format;
        let track = format.tracks().iter().find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "native decoder found no audio"))?;
        let track_id = track.id;
        let decoder = symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, format!("native decoder: {err}")))?;
        Ok(Self {
            format,
            decoder,
            track_id,
            out_rate: sample_rate.max(8_000),
            out_ch: channels.max(1) as usize,
            pending: Vec::new(),
            pos: 0,
            phase: 0.0,
        })
    }

    fn pull(&mut self) -> io::Result<bool> {
        use symphonia::core::audio::SampleBuffer;
        use symphonia::core::errors::Error as SymError;
        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(SymError::ResetRequired) => {
                    self.decoder.reset();
                    continue;
                }
                Err(SymError::IoError(err)) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(false),
                Err(SymError::IoError(err)) => return Err(err),
                Err(_) => return Ok(false),
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            let decoded = match self.decoder.decode(&packet) {
                Ok(d) => d,
                Err(SymError::DecodeError(_)) => continue,
                Err(SymError::IoError(err)) => return Err(err),
                Err(_) => return Ok(false),
            };
            let spec = *decoded.spec();
            let mut buf = SampleBuffer::<i16>::new(decoded.capacity() as u64, spec);
            buf.copy_interleaved_ref(decoded);
            let pcm = adapt_pcm(buf.samples(), spec.channels.count(), spec.rate, self.out_ch, self.out_rate, &mut self.phase);
            if !pcm.is_empty() {
                self.pending.extend_from_slice(&pcm);
                return Ok(true);
            }
        }
    }
}

impl Read for SymReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        while self.pos >= self.pending.len() {
            self.pending.clear();
            self.pos = 0;
            if !self.pull()? {
                return Ok(0);
            }
        }
        let n = (self.pending.len() - self.pos).min(out.len());
        out[..n].copy_from_slice(&self.pending[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

fn adapt_pcm(input: &[i16], in_ch: usize, in_rate: u32, out_ch: usize, out_rate: u32, phase: &mut f64) -> Vec<u8> {
    let in_ch = in_ch.max(1);
    let out_ch = out_ch.max(1);
    let frames = input.len() / in_ch;
    if frames == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    if in_rate == 0 || out_rate == 0 || in_rate == out_rate {
        for frame in 0..frames {
            write_frame(&mut out, input, in_ch, frame, out_ch);
        }
        return out;
    }
    let step = in_rate as f64 / out_rate as f64;
    let mut pos = *phase;
    while pos + 1.0 < frames as f64 {
        let i = pos.floor() as usize;
        let frac = (pos - i as f64) as f32;
        let mut mixed = [0i16; 8];
        let n = in_ch.min(8);
        for c in 0..n {
            let a = input[i * in_ch + c] as f32;
            let b = input[(i + 1) * in_ch + c] as f32;
            mixed[c] = (a + (b - a) * frac) as i16;
        }
        write_frame(&mut out, &mixed[..n], n, 0, out_ch);
        pos += step;
    }
    *phase = (pos - frames as f64).max(0.0);
    out
}

fn write_frame(out: &mut Vec<u8>, samples: &[i16], in_ch: usize, frame: usize, out_ch: usize) {
    let base = frame * in_ch;
    if out_ch == 1 {
        let mut acc = 0i32;
        for c in 0..in_ch {
            acc += samples[base + c] as i32;
        }
        let s = (acc / in_ch as i32).clamp(-32768, 32767) as i16;
        out.extend_from_slice(&s.to_le_bytes());
        return;
    }
    let left = samples[base];
    let right = if in_ch > 1 { samples[base + 1] } else { left };
    out.extend_from_slice(&left.to_le_bytes());
    out.extend_from_slice(&right.to_le_bytes());
    for _ in 2..out_ch {
        out.extend_from_slice(&left.to_le_bytes());
    }
}

const OPUS_RATE: u32 = 48_000;
const OPUS_FRAME: usize = 960;

fn opus_encode(sample_rate: u32, channels: u16, bitrate: u32) -> io::Result<EncodeEnds> {
    let channels = if channels <= 1 { 1 } else { 2 };
    let in_rate = sample_rate.max(8_000);
    let mut enc = opus::Encoder::new(OPUS_RATE, opus_channels(channels), opus::Application::Audio)
        .map_err(|err| io::Error::other(format!("opus encoder failed: {err}")))?;
    let bps = (bitrate as i32).clamp(6_000, 510_000);
    enc.set_bitrate(opus::Bitrate::Bits(bps))
        .map_err(|err| io::Error::other(format!("opus encoder failed: {err}")))?;
    enc.set_vbr(false).map_err(|err| io::Error::other(format!("opus encoder failed: {err}")))?;
    let pre_skip = enc.get_lookahead().unwrap_or(312).clamp(0, 32_000) as u16;
    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(32);
    let input = OpusWriter {
        enc,
        tx: Some(tx),
        channels,
        in_rate,
        pre_skip,
        stash: Vec::new(),
        tail: Vec::new(),
        phase: 0.0,
        pcm: Vec::new(),
        odd: None,
        serial: 0x4E61_7661,
        seq: 0,
        granule: 0,
        headers: false,
    };
    Ok(EncodeEnds {
        input: Some(Box::new(input)),
        output: Some(Box::new(ChanReader { rx, buf: Vec::new(), pos: 0 })),
        child: None,
    })
}

fn opus_channels(channels: usize) -> opus::Channels {
    if channels == 1 { opus::Channels::Mono } else { opus::Channels::Stereo }
}

struct OpusWriter {
    enc: opus::Encoder,
    tx: Option<SyncSender<Vec<u8>>>,
    channels: usize,
    in_rate: u32,
    pre_skip: u16,
    stash: Vec<i16>,
    tail: Vec<i16>,
    phase: f64,
    pcm: Vec<i16>,
    odd: Option<u8>,
    serial: u32,
    seq: u32,
    granule: u64,
    headers: bool,
}

impl OpusWriter {
    fn send(&mut self, bytes: Vec<u8>) -> io::Result<()> {
        let tx = self.tx.as_ref().ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "closed"))?;
        send_bounded(tx, bytes)
    }

    fn write_headers(&mut self) -> io::Result<()> {
        if self.headers {
            return Ok(());
        }
        self.headers = true;
        let id = ogg_page(0x02, 0, self.serial, self.seq, &opus_head(self.channels as u8, self.pre_skip, self.in_rate));
        self.seq = self.seq.saturating_add(1);
        self.send(id)?;
        let tags = ogg_page(0, 0, self.serial, self.seq, &opus_tags());
        self.seq = self.seq.saturating_add(1);
        self.send(tags)?;
        self.granule = self.pre_skip as u64;
        Ok(())
    }

    fn push_samples(&mut self, samples: &[i16]) -> io::Result<()> {
        self.stash.extend_from_slice(samples);
        let ch = self.channels;
        let frames = self.stash.len() / ch;
        if frames == 0 {
            return Ok(());
        }
        let usable = frames * ch;
        let input: Vec<i16> = self.stash.drain(..usable).collect();
        if self.in_rate == OPUS_RATE {
            self.pcm.extend_from_slice(&input);
        } else {
            resample_opus(&mut self.pcm, &mut self.tail, &input, ch, self.in_rate, &mut self.phase);
        }
        self.emit_frames(false)
    }

    fn emit_frames(&mut self, eos: bool) -> io::Result<()> {
        let need = OPUS_FRAME * self.channels;
        while self.pcm.len() >= need {
            if !self.headers {
                self.write_headers()?;
            }
            let last = eos && self.pcm.len() < need * 2;
            let frame: Vec<i16> = self.pcm.drain(..need).collect();
            let packet = self.enc.encode_vec(&frame, 4_000)
                .map_err(|err| io::Error::other(format!("opus encoder failed: {err}")))?;
            self.granule = self.granule.saturating_add(OPUS_FRAME as u64);
            let kind = if last { 0x04 } else { 0 };
            let page = ogg_page(kind, self.granule, self.serial, self.seq, &packet);
            self.seq = self.seq.saturating_add(1);
            self.send(page)?;
        }
        Ok(())
    }

    fn finish(&mut self) {
        let ch = self.channels;
        if !self.stash.is_empty() && self.stash.len() % ch != 0 {
            let pad = ch - (self.stash.len() % ch);
            self.stash.extend(std::iter::repeat(0).take(pad));
        }
        if self.in_rate != OPUS_RATE && self.stash.len() / ch == 1 {
            let frame = self.stash.clone();
            self.stash.extend_from_slice(&frame);
        }
        let _ = self.push_samples(&[]);
        self.tail.clear();
        let need = OPUS_FRAME * ch;
        if !self.pcm.is_empty() && self.pcm.len() % need != 0 {
            let pad = need - (self.pcm.len() % need);
            self.pcm.extend(std::iter::repeat(0).take(pad));
        }
        let _ = self.emit_frames(true);
        self.tx.take();
    }
}

impl Write for OpusWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut rest = buf;
        if let Some(first) = self.odd.take() {
            if rest.is_empty() {
                self.odd = Some(first);
                return Ok(0);
            }
            let sample = i16::from_le_bytes([first, rest[0]]);
            self.push_samples(&[sample])?;
            rest = &rest[1..];
        }
        let even = rest.len() - (rest.len() % 2);
        let mut samples = Vec::with_capacity(even / 2);
        for frame in rest[..even].chunks_exact(2) {
            samples.push(i16::from_le_bytes([frame[0], frame[1]]));
        }
        if even < rest.len() {
            self.odd = Some(rest[even]);
        }
        self.push_samples(&samples)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

impl Drop for OpusWriter {
    fn drop(&mut self) { self.finish(); }
}

fn resample_opus(dst: &mut Vec<i16>, tail: &mut Vec<i16>, input: &[i16], ch: usize, in_rate: u32, phase: &mut f64) {
    let mut joined = Vec::with_capacity(tail.len() + input.len());
    joined.extend_from_slice(tail);
    joined.extend_from_slice(input);
    let frames = joined.len() / ch;
    if frames < 2 {
        *tail = joined;
        return;
    }
    let step = in_rate as f64 / OPUS_RATE as f64;
    let mut pos = *phase;
    while pos + 1.0 < frames as f64 {
        let i = pos.floor() as usize;
        let frac = (pos - i as f64) as f32;
        for c in 0..ch {
            let a = joined[i * ch + c] as f32;
            let b = joined[(i + 1) * ch + c] as f32;
            dst.push((a + (b - a) * frac).clamp(-32768.0, 32767.0) as i16);
        }
        pos += step;
    }
    let keep = pos.floor() as usize;
    *phase = pos - keep as f64;
    tail.clear();
    tail.extend_from_slice(&joined[keep * ch..]);
}

fn opus_head(channels: u8, pre_skip: u16, rate: u32) -> Vec<u8> {
    let mut packet = Vec::with_capacity(19);
    packet.extend_from_slice(b"OpusHead");
    packet.push(1);
    packet.push(channels);
    packet.extend_from_slice(&pre_skip.to_le_bytes());
    packet.extend_from_slice(&rate.to_le_bytes());
    packet.extend_from_slice(&0u16.to_le_bytes());
    packet.push(0);
    packet
}

fn opus_tags() -> Vec<u8> {
    let vendor = b"NavaCast";
    let mut packet = Vec::with_capacity(16 + vendor.len());
    packet.extend_from_slice(b"OpusTags");
    packet.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    packet.extend_from_slice(vendor);
    packet.extend_from_slice(&0u32.to_le_bytes());
    packet
}

fn ogg_page(kind: u8, granule: u64, serial: u32, seq: u32, packet: &[u8]) -> Vec<u8> {
    let mut segs = Vec::new();
    if packet.is_empty() {
        segs.push(0);
    } else {
        let mut left = packet.len();
        while left >= 255 {
            segs.push(255);
            left -= 255;
        }
        segs.push(left as u8);
    }
    let mut page = Vec::with_capacity(27 + segs.len() + packet.len());
    page.extend_from_slice(b"OggS");
    page.push(0);
    page.push(kind);
    page.extend_from_slice(&granule.to_le_bytes());
    page.extend_from_slice(&serial.to_le_bytes());
    page.extend_from_slice(&seq.to_le_bytes());
    page.extend_from_slice(&0u32.to_le_bytes());
    page.push(segs.len() as u8);
    page.extend_from_slice(&segs);
    page.extend_from_slice(packet);
    let crc = ogg_crc(&page);
    page[22..26].copy_from_slice(&crc.to_le_bytes());
    page
}

fn ogg_crc(data: &[u8]) -> u32 {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut out = [0u32; 256];
        for i in 0..256 {
            let mut r = (i as u32) << 24;
            for _ in 0..8 {
                if r & 0x8000_0000 != 0 {
                    r = (r << 1) ^ 0x04c1_1db7;
                } else {
                    r <<= 1;
                }
            }
            out[i] = r;
        }
        out
    });
    let mut crc = 0u32;
    for &byte in data {
        crc = (crc << 8) ^ table[((crc >> 24) as u8 ^ byte) as usize];
    }
    crc
}

pub const SILENCE_MP3: &[u8] = include_bytes!("../assets/silence.mp3");

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn identity_roundtrip() {
        let mut ends = IdentityFactory.encode(8000, 2, 128_000).unwrap();
        ends.input.as_mut().unwrap().write_all(&[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        drop(ends.input.take());
        let mut got = Vec::new();
        ends.output.as_mut().unwrap().read_to_end(&mut got).unwrap();
        assert_eq!(got, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn ffmpeg_emits_mp3_when_present() {
        if !ffmpeg_available() {
            eprintln!("ffmpeg not installed; encoder interface covered by IdentityFactory");
            return;
        }
        let mut ends = FfmpegFactory.encode(44100, 2, 128_000).unwrap();
        // 200 ms of silence.
        let pcm = vec![0u8; 44100 * 2 * 2 / 5];
        ends.input.as_mut().unwrap().write_all(&pcm).unwrap();
        drop(ends.input.take());
        let mut out = Vec::new();
        ends.output.as_mut().unwrap().read_to_end(&mut out).unwrap();
        drop(ends.child.take());
        assert!(out.len() > 100, "short ffmpeg output {}", out.len());
        assert!(out.windows(2).any(|w| w[0] == 0xFF && w[1] & 0xE0 == 0xE0), "no mp3 sync");
    }

    #[test]
    fn native_encoder_emits_mp3_frames_without_ffmpeg() {
        let mut ends = native_encode(44100, 2, 128_000).expect("native encoder");
        assert!(ends.child.is_none(), "native encoder must not shell out");
        let pcm = vec![0u8; 44100 * 2 * 2 / 5];
        ends.input.as_mut().unwrap().write_all(&pcm).unwrap();
        drop(ends.input.take());
        let mut out = Vec::new();
        ends.output.as_mut().unwrap().read_to_end(&mut out).unwrap();
        assert!(out.len() > 100, "short native output {}", out.len());
        assert!(out.windows(2).any(|w| w[0] == 0xFF && (w[1] & 0xE0) == 0xE0), "no mp3 sync");
    }

    #[test]
    fn opus_encoder_emits_ogg_without_ffmpeg() {
        let mut ends = RouterFactory.encode_with("opus", 44_100, 2, 64_000).expect("opus encoder");
        assert!(ends.child.is_none(), "opus encoder must not shell out");
        let pcm = vec![0u8; 44_100 * 2 * 2 / 5];
        ends.input.as_mut().unwrap().write_all(&pcm).unwrap();
        drop(ends.input.take());
        let mut out = Vec::new();
        ends.output.as_mut().unwrap().read_to_end(&mut out).unwrap();
        assert!(out.len() > 100, "short opus output {}", out.len());
        assert!(out.windows(4).any(|w| w == b"OggS"), "no ogg page");
        assert!(out.windows(8).any(|w| w == b"OpusHead"), "no opus header");
        let mut i = 0;
        let mut pages = 0;
        while i + 27 <= out.len() && &out[i..i + 4] == b"OggS" {
            let nseg = out[i + 26] as usize;
            let mut body = 0;
            for seg in &out[i + 27..i + 27 + nseg] {
                body += *seg as usize;
            }
            let end = i + 27 + nseg + body;
            assert!(end <= out.len(), "ogg page overruns the buffer");
            let mut page = out[i..end].to_vec();
            let stored = u32::from_le_bytes(page[22..26].try_into().unwrap());
            page[22..26].fill(0);
            assert_eq!(ogg_crc(&page), stored, "ogg crc");
            pages += 1;
            i = end;
        }
        assert!(pages >= 2, "expected header pages, got {pages}");
    }

    #[test]
    fn unknown_encoder_is_a_clear_error() {
        let err = match RouterFactory.encode_with("missing", 48_000, 2, 64_000) {
            Err(err) => err,
            Ok(_) => panic!("unknown encoder was accepted"),
        };
        assert!(err.to_string().contains("unknown encoder missing"), "{err}");
        let err = normalize_encoder("nope").unwrap_err();
        assert!(err.to_string().contains("ffmpeg, native, or opus"), "{err}");
        assert_eq!(normalize_encoder("opus").unwrap(), "opus");
        assert_eq!(content_type("opus"), "audio/ogg");
        assert!(available_encoders().contains(&"opus"));
        assert!(available_encoders().contains(&"native"));
    }

    #[test]
    fn native_decoder_reads_mp3_without_ffmpeg() {
        let input = std::io::Cursor::new(SILENCE_MP3.to_vec());
        let mut decoded = native_decode(Box::new(input), "audio/mpeg", 44_100, 2).unwrap();
        let mut pcm = Vec::new();
        decoded.reader.read_to_end(&mut pcm).unwrap();
        assert!(pcm.len() > 1000, "native decoder produced {}", pcm.len());
    }
}
