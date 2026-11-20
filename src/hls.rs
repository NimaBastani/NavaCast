//! HLS packing. Segments are MPEG-TS slices of the encoded stream.
//! The caller sets the target duration.

use std::collections::VecDeque;
use std::time::Duration;

#[derive(Clone)]
struct Segment {
    seq: u64,
    dur: f64,
    name: String,
    data: Vec<u8>,
}

pub struct Hls {
    target: Duration,
    window: usize,
    owed: Duration,
    buf: Vec<u8>,
    started: bool,
    seq: u64,
    segs: VecDeque<Segment>,
    playlist: String,
    mux: TsMux,
}

impl Hls {
    pub fn new(target: Duration, window: usize) -> Self {
        Self {
            target,
            window: window.max(2),
            owed: Duration::ZERO,
            buf: Vec::new(),
            started: false,
            seq: 0,
            segs: VecDeque::new(),
            playlist: String::new(),
            mux: TsMux::default(),
        }
    }

    pub fn add_bytes(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        self.started = true;
        self.buf.extend_from_slice(data);
        self.try_emit();
    }

    pub fn add_time(&mut self, d: Duration) {
        if self.started {
            self.owed += d;
            self.try_emit();
        }
    }

    pub fn push(&mut self, data: &[u8], d: Duration) {
        self.add_bytes(data);
        self.add_time(d);
    }

    fn try_emit(&mut self) {
        if self.owed >= self.target && !self.buf.is_empty() {
            let data = std::mem::take(&mut self.buf);
            let dur = self.owed;
            self.owed = Duration::ZERO;
            let seq = self.seq;
            self.seq += 1;
            let ts = self.mux.pack(&data, dur);
            let name = format!("seg{seq}.ts");
            self.segs.push_back(Segment { seq, dur: dur.as_secs_f64(), name, data: ts });
            while self.segs.len() > self.window {
                self.segs.pop_front();
            }
            self.playlist = render(&self.segs);
        }
    }

    pub fn set_target(&mut self, target: Duration) {
        let ms = (target.as_millis() as u64).clamp(250, 2_000);
        self.target = Duration::from_millis(ms);
    }

    pub fn segment_count(&self) -> usize { self.segs.len() }

    pub fn playlist(&self) -> String {
        if self.playlist.is_empty() {
            format!(
                "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:{}\n#EXT-X-MEDIA-SEQUENCE:0\n",
                self.target.as_secs().max(1)
            )
        } else {
            self.playlist.clone()
        }
    }

    /// Segment URIs are absolute when `base` is the public origin (`https://radio.example.com`).
    pub fn playlist_at(&self, base: &str, mount: &str) -> String {
        let body = self.playlist();
        let base = base.trim_end_matches('/');
        if base.is_empty() {
            return body;
        }
        let root = format!("{base}/{}", mount.trim_start_matches('/'));
        let mut out = String::new();
        for line in body.lines() {
            if line.is_empty() {
                continue;
            }
            if line.starts_with('#') {
                out.push_str(line);
            } else {
                out.push_str(&root);
                out.push_str("/seg/");
                out.push_str(line);
            }
            out.push('\n');
        }
        out
    }

    pub fn segment(&self, name: &str) -> Option<Vec<u8>> {
        self.segs.iter().find(|s| s.name == name).map(|s| s.data.clone())
    }
}

fn render(segs: &VecDeque<Segment>) -> String {
    let max_dur = segs.iter().map(|s| s.dur.ceil() as u64).max().unwrap_or(2).max(1);
    let seq = segs.front().map(|s| s.seq).unwrap_or(0);
    let mut out = format!("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:{max_dur}\n#EXT-X-MEDIA-SEQUENCE:{seq}\n");
    for s in segs {
        out.push_str(&format!("#EXTINF:{:.3},\n{}\n", s.dur, s.name));
    }
    out
}

#[derive(Default)]
struct TsMux {
    pat_cc: u8,
    pmt_cc: u8,
    audio_cc: u8,
    pts: u64,
}

impl TsMux {
    fn pack(&mut self, audio: &[u8], dur: Duration) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&psi_packet(0x0000, &pat_section(), &mut self.pat_cc));
        out.extend_from_slice(&psi_packet(0x0100, &pmt_section(), &mut self.pmt_cc));
        let frames = split_mp3(audio);
        if frames.is_empty() {
            self.write_pes(&mut out, audio, true);
            self.pts = self.pts.wrapping_add(duration_90k(dur));
        } else {
            for (i, frame) in frames.iter().enumerate() {
                self.write_pes(&mut out, frame, i == 0);
                self.pts = self.pts.wrapping_add(2351);
            }
        }
        out
    }

    fn write_pes(&mut self, out: &mut Vec<u8>, payload: &[u8], pcr: bool) {
        let mut offset = 0;
        let mut first = true;
        if payload.is_empty() {
            return;
        }
        while offset < payload.len() {
            let take = (payload.len() - offset).min(65535 - 8);
            let chunk = &payload[offset..offset + take];
            let mut pes = Vec::with_capacity(chunk.len() + 14);
            pes.extend_from_slice(&[0x00, 0x00, 0x01, 0xC0]);
            pes.extend_from_slice(&((8 + chunk.len()) as u16).to_be_bytes());
            pes.extend_from_slice(&[0x80, 0x80, 0x05]);
            let mut ptsb = [0u8; 5];
            write_pts(&mut ptsb, self.pts);
            pes.extend_from_slice(&ptsb);
            pes.extend_from_slice(chunk);
            let stamp = if first && pcr { Some(self.pts) } else { None };
            packetize(out, 0x0101, &mut self.audio_cc, &pes, stamp);
            offset += take;
            first = false;
        }
    }
}

fn duration_90k(d: Duration) -> u64 {
    (d.as_secs_f64() * 90_000.0) as u64
}

fn pat_section() -> Vec<u8> {
    // program 1 -> PMT pid 0x0100. section_length 13.
    let mut s = vec![
        0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01, 0xE1, 0x00,
    ];
    let crc = mpeg_crc(&s);
    s.extend_from_slice(&crc.to_be_bytes());
    s
}

fn pmt_section() -> Vec<u8> {
    // PCR + audio pid 0x0101, stream_type 0x03 (MPEG-1 audio). section_length 18.
    let mut s = vec![
        0x02, 0xB0, 0x12, 0x00, 0x01, 0xC1, 0x00, 0x00, 0xE1, 0x01, 0xF0, 0x00, 0x03, 0xE1, 0x01, 0xF0, 0x00,
    ];
    let crc = mpeg_crc(&s);
    s.extend_from_slice(&crc.to_be_bytes());
    s
}

fn psi_packet(pid: u16, section: &[u8], cc: &mut u8) -> [u8; 188] {
    let mut payload = Vec::with_capacity(1 + section.len());
    payload.push(0x00);
    payload.extend_from_slice(section);
    let mut out = Vec::new();
    packetize(&mut out, pid, cc, &payload, None);
    let mut pkt = [0xFFu8; 188];
    pkt.copy_from_slice(&out[..188]);
    pkt
}

/// One or more 188-byte transport packets. Short payloads are stuffed in the adaptation field.
fn packetize(out: &mut Vec<u8>, pid: u16, cc: &mut u8, mut data: &[u8], mut pcr: Option<u64>) {
    if data.is_empty() && pcr.is_none() {
        return;
    }
    let mut first = true;
    loop {
        let mut af: Vec<u8> = Vec::new();
        if let Some(base) = pcr.take() {
            af.push(0x50);
            let mut p = [0u8; 6];
            write_pcr(&mut p, base);
            af.extend_from_slice(&p);
        }
        let mut zero_len = false;
        let take = if af.is_empty() {
            if data.len() >= 184 {
                184
            } else if data.len() == 183 {
                zero_len = true;
                183
            } else {
                af.push(0x00);
                af.extend(std::iter::repeat(0xFF).take(182 - data.len()));
                data.len()
            }
        } else {
            let space = 183 - af.len();
            if data.len() >= space {
                space
            } else {
                af.extend(std::iter::repeat(0xFF).take(space - data.len()));
                data.len()
            }
        };
        let has_af = zero_len || !af.is_empty();
        let afc = if !has_af { 0b01 } else if take == 0 { 0b10 } else { 0b11 };
        let mut pkt = [0xFFu8; 188];
        pkt[0] = 0x47;
        pkt[1] = ((pid >> 8) as u8) & 0x1F;
        if first {
            pkt[1] |= 0x40;
        }
        pkt[2] = (pid & 0xFF) as u8;
        pkt[3] = (afc << 4) | (*cc & 0x0F);
        *cc = cc.wrapping_add(1) & 0x0F;
        let mut off = 4;
        if has_af {
            pkt[off] = af.len() as u8;
            off += 1;
            if !af.is_empty() {
                pkt[off..off + af.len()].copy_from_slice(&af);
                off += af.len();
            }
        }
        if take > 0 {
            pkt[off..off + take].copy_from_slice(&data[..take]);
            data = &data[take..];
        }
        out.extend_from_slice(&pkt);
        first = false;
        if data.is_empty() {
            break;
        }
    }
}

fn write_pts(dst: &mut [u8], pts: u64) {
    let p = pts & 0x1_FFFF_FFFF;
    dst[0] = 0x21 | (((p >> 29) as u8) & 0x0E);
    dst[1] = (p >> 22) as u8;
    dst[2] = (((p >> 14) as u8) & 0xFE) | 1;
    dst[3] = (p >> 7) as u8;
    dst[4] = (((p as u8) << 1) & 0xFE) | 1;
}

fn write_pcr(dst: &mut [u8], base: u64) {
    let b = base & 0x1_FFFF_FFFF;
    dst[0] = (b >> 25) as u8;
    dst[1] = (b >> 17) as u8;
    dst[2] = (b >> 9) as u8;
    dst[3] = (b >> 1) as u8;
    dst[4] = (((b & 1) as u8) << 7) | 0x7E;
    dst[5] = 0;
}

fn mpeg_crc(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= (b as u32) << 24;
        for _ in 0..8 {
            if crc & 0x8000_0000 != 0 {
                crc = (crc << 1) ^ 0x04C1_1DB7;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

fn split_mp3(data: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut ok = 0;
    while i + 4 < data.len() {
        if let Some(n) = mp3_frame_len(&data[i..]) {
            if i + n > data.len() {
                break;
            }
            out.push(data[i..i + n].to_vec());
            i += n;
            ok += 1;
            if ok > 10_000 {
                break;
            }
        } else {
            // Not frame aligned. Give up and let the caller send one blob.
            return Vec::new();
        }
    }
    if ok == 0 { Vec::new() } else { out }
}

fn mp3_frame_len(b: &[u8]) -> Option<usize> {
    if b.len() < 4 || b[0] != 0xFF || b[1] & 0xE0 != 0xE0 {
        return None;
    }
    let ver = (b[1] >> 3) & 0x03;
    let layer = (b[1] >> 1) & 0x03;
    let br_idx = (b[2] >> 4) & 0x0F;
    let sr_idx = (b[2] >> 2) & 0x03;
    let pad = (b[2] >> 1) & 0x01;
    if ver == 1 || layer == 0 || br_idx == 0 || br_idx == 15 || sr_idx == 3 {
        return None;
    }
    const BRS: [usize; 16] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0];
    const SRS: [usize; 3] = [44100, 48000, 32000];
    let br = BRS[br_idx as usize] * 1000;
    let mut sr = SRS[sr_idx as usize];
    if ver == 2 {
        sr /= 2;
    } else if ver == 0 {
        sr /= 4;
    }
    if br == 0 || sr == 0 {
        return None;
    }
    // MPEG1 Layer III uses 144; MPEG2/2.5 Layer III uses 72.
    let samples_factor = if ver == 3 && layer == 1 { 144 } else if layer == 1 { 72 } else { return None };
    Some(samples_factor * br / sr + pad as usize)
}

pub fn bytes_duration(nbytes: usize, bitrate: u32) -> Duration {
    let bps = (bitrate / 8).max(1) as u64;
    Duration::from_micros((nbytes as u64).saturating_mul(1_000_000) / bps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_second_segments_are_ts_packets() {
        let mut h = Hls::new(Duration::from_secs(2), 6);
        h.add_bytes(&[1, 2, 3, 4, 5, 6, 7, 8]);
        h.add_time(Duration::from_millis(1500));
        assert_eq!(h.segment_count(), 0);
        h.add_time(Duration::from_millis(600));
        assert_eq!(h.segment_count(), 1);
        let pl = h.playlist();
        assert!(pl.contains("#EXT-X-TARGETDURATION:"));
        assert!(pl.contains("#EXT-X-MEDIA-SEQUENCE:0"));
        assert!(pl.contains("seg0.ts"));
        let seg = h.segment("seg0.ts").unwrap();
        assert_eq!(seg.len() % 188, 0);
        assert!(seg.len() >= 188 * 2);
        assert!(seg.chunks(188).all(|p| p[0] == 0x47));
        assert!(h.segment("../etc/passwd").is_none());
    }

    #[test]
    fn real_mp3_segment_probes_when_ffmpeg_present() {
        if !crate::encoder::ffmpeg_available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("radio-hls-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mp3 = dir.join("tone.mp3");
        let status = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=44100", "-t", "2", "-c:a", "libmp3lame", "-b:a", "128k", "-f", "mp3"])
            .arg(&mp3)
            .status()
            .unwrap();
        assert!(status.success());
        let data = std::fs::read(&mp3).unwrap();
        let mut mux = TsMux::default();
        let ts = mux.pack(&data, Duration::from_secs(2));
        assert_eq!(ts.len() % 188, 0);
        let out = dir.join("seg.ts");
        std::fs::write(&out, &ts).unwrap();
        let probe = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "stream=codec_name", "-of", "csv=p=0"])
            .arg(&out)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&probe.stdout);
        assert!(probe.status.success(), "ffprobe stderr {}", String::from_utf8_lossy(&probe.stderr));
        assert!(text.to_ascii_lowercase().contains("mp3"), "ffprobe said {text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
