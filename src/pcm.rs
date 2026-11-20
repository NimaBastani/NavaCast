use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

struct Q {
    buf: Vec<u8>,
    pos: usize,
    max: usize,
    closed: bool,
}

pub struct PcmQueue {
    inner: Mutex<Q>,
    cv: Condvar,
}

impl PcmQueue {
    pub fn new(max: usize) -> Self {
        Self {
            inner: Mutex::new(Q { buf: Vec::new(), pos: 0, max: max.max(1), closed: false }),
            cv: Condvar::new(),
        }
    }

    pub fn write_all(&self, data: &[u8]) -> std::io::Result<()> {
        self.write_timeout(data, Duration::from_secs(30))
    }

    /// Writes until `timeout`. A mixer that stopped reading must not pin AutoDJ.
    pub fn write_timeout(&self, mut data: &[u8], timeout: Duration) -> std::io::Result<()> {
        let deadline = Instant::now() + timeout;
        while !data.is_empty() {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            while !g.closed && g.buf.len().saturating_sub(g.pos) >= g.max {
                let now = Instant::now();
                if now >= deadline {
                    return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "pcm queue stalled"));
                }
                let wait = self.cv.wait_timeout(g, deadline - now);
                g = match wait {
                    Ok((guard, _)) => guard,
                    Err(poisoned) => poisoned.into_inner().0,
                };
            }
            if g.closed {
                return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "queue closed"));
            }
            if g.pos > 64 * 1024 && g.pos > g.buf.len() / 2 {
                let pos = g.pos;
                g.buf.drain(..pos);
                g.pos = 0;
            }
            let have = g.buf.len().saturating_sub(g.pos);
            let space = g.max.saturating_sub(have);
            if space == 0 {
                continue;
            }
            let n = data.len().min(space);
            g.buf.extend_from_slice(&data[..n]);
            data = &data[n..];
            self.cv.notify_all();
        }
        Ok(())
    }

    /// Non-blocking read. Missing bytes stay untouched so the caller can zero-fill.
    pub fn read(&self, dst: &mut [u8]) -> usize {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let have = g.buf.len() - g.pos;
        let n = dst.len().min(have);
        dst[..n].copy_from_slice(&g.buf[g.pos..g.pos + n]);
        g.pos += n;
        if n > 0 {
            self.cv.notify_all();
        }
        n
    }

    pub fn close(&self) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.closed = true;
        self.cv.notify_all();
    }

    pub fn reopen(&self) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.closed = false;
        g.buf.clear();
        g.pos = 0;
        self.cv.notify_all();
    }

    pub fn set_max(&self, max: usize) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.max = max.max(1);
        self.cv.notify_all();
    }

    pub fn drain(&self) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.buf.clear();
        g.pos = 0;
        self.cv.notify_all();
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Auto,
    ToLive,
    Live,
    ToAuto,
}

pub struct Mixer {
    inner: Mutex<Mix>,
    auto_q: Arc<PcmQueue>,
    live_q: Arc<PcmQueue>,
}

struct Mix {
    mode: Mode,
    fade_pos: usize,
    fade_samples: usize,
    channels: usize,
}

impl Mixer {
    pub fn new(auto_q: Arc<PcmQueue>, live_q: Arc<PcmQueue>, channels: u16, fade_samples: usize) -> Self {
        Self {
            inner: Mutex::new(Mix {
                mode: Mode::Auto,
                fade_pos: 0,
                fade_samples,
                channels: channels.max(1) as usize,
            }),
            auto_q,
            live_q,
        }
    }

    pub fn note_live(&self) {
        let mut g = self.inner.lock().expect("mix");
        if g.mode == Mode::Auto || g.mode == Mode::ToAuto {
            g.fade_pos = 0;
            g.mode = if g.fade_samples == 0 { Mode::Live } else { Mode::ToLive };
        }
    }

    pub fn set_fade(&self, fade_samples: usize) {
        self.inner.lock().expect("mix").fade_samples = fade_samples;
    }

    pub fn set_channels(&self, channels: u16) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.channels = channels.max(1) as usize;
    }

    pub fn to_auto(&self) {
        let mut g = self.inner.lock().expect("mix");
        if g.mode == Mode::Live || g.mode == Mode::ToLive {
            g.fade_pos = 0;
            g.mode = if g.fade_samples == 0 { Mode::Auto } else { Mode::ToAuto };
        }
    }

    pub fn fill(&self, dst: &mut [u8]) {
        let (mode, fade_pos, fade_samples, channels) = {
            let g = self.inner.lock().expect("mix");
            (g.mode, g.fade_pos, g.fade_samples, g.channels)
        };
        let bps = channels * 2;
        if dst.len() < bps {
            return;
        }
        let frames = dst.len() / bps;
        let used = frames * bps;
        match mode {
            Mode::Auto => read_or_silence(&self.auto_q, &mut dst[..used]),
            Mode::Live => read_or_silence(&self.live_q, &mut dst[..used]),
            Mode::ToLive => mix_ramp(&self.auto_q, &self.live_q, &mut dst[..used], fade_pos, fade_samples, channels),
            Mode::ToAuto => mix_ramp(&self.live_q, &self.auto_q, &mut dst[..used], fade_pos, fade_samples, channels),
        }
        let mut g = self.inner.lock().expect("mix");
        if g.mode != mode {
            return;
        }
        if matches!(mode, Mode::ToLive | Mode::ToAuto) {
            g.fade_pos += frames;
            if g.fade_pos >= g.fade_samples {
                g.mode = if mode == Mode::ToLive { Mode::Live } else { Mode::Auto };
            }
        }
    }
}

fn read_or_silence(q: &PcmQueue, dst: &mut [u8]) {
    let n = q.read(dst);
    dst[n..].fill(0);
}

fn mix_ramp(from: &PcmQueue, to: &PcmQueue, dst: &mut [u8], pos: usize, fade_n: usize, channels: usize) {
    let mut a = vec![0u8; dst.len()];
    let mut b = vec![0u8; dst.len()];
    read_or_silence(from, &mut a);
    read_or_silence(to, &mut b);
    let bps = channels * 2;
    let frames = dst.len() / bps;
    for i in 0..frames {
        let g = if fade_n == 0 {
            1.0
        } else {
            let p = (pos + i + 1).min(fade_n);
            p as f32 / fade_n as f32
        };
        for c in 0..channels {
            let off = i * bps + c * 2;
            let sa = i16::from_le_bytes([a[off], a[off + 1]]) as f32;
            let sb = i16::from_le_bytes([b[off], b[off + 1]]) as f32;
            let mut out = sa * (1.0 - g) + sb * g;
            if out > 32767.0 { out = 32767.0; }
            if out < -32768.0 { out = -32768.0; }
            dst[off..off + 2].copy_from_slice(&(out as i16).to_le_bytes());
        }
    }
}

pub fn frame_bytes(sample_rate: u32, channels: u16, frame: Duration) -> usize {
    let samples = ((sample_rate as u128) * frame.as_nanos() as u128 / 1_000_000_000).max(1) as usize;
    samples * channels.max(1) as usize * 2
}

pub fn fade_samples(sample_rate: u32, fade: Duration) -> usize {
    ((sample_rate as u128) * fade.as_nanos() as u128 / 1_000_000_000) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(sample: i16, frames: usize, channels: usize) -> Vec<u8> {
        let mut v = Vec::new();
        for _ in 0..frames * channels {
            v.extend_from_slice(&sample.to_le_bytes());
        }
        v
    }

    #[test]
    fn fade_then_live_then_back() {
        let auto = Arc::new(PcmQueue::new(64_000));
        let live = Arc::new(PcmQueue::new(64_000));
        auto.write_all(&pcm(0, 50, 2)).unwrap();
        live.write_all(&pcm(10_000, 50, 2)).unwrap();
        let mixer = Mixer::new(auto, live, 2, 4);
        mixer.note_live();
        let mut buf = vec![0u8; 4 * 2 * 2];
        mixer.fill(&mut buf);
        let first = i16::from_le_bytes([buf[0], buf[1]]);
        let last = i16::from_le_bytes([buf[buf.len() - 2], buf[buf.len() - 1]]);
        assert!(first < last, "fade should rise: {first} -> {last}");
        assert!(last > 8_000, "end of fade near live, got {last}");
        mixer.fill(&mut buf);
        let s = i16::from_le_bytes([buf[0], buf[1]]);
        assert_eq!(s, 10_000);
        mixer.to_auto();
        mixer.fill(&mut buf);
        mixer.fill(&mut buf);
        let s = i16::from_le_bytes([buf[0], buf[1]]);
        assert!(s < 1_000, "back to autodj silence, got {s}");
    }
}
