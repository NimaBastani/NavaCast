use crate::error::Error;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Kind {
    pub ext: &'static str,
    pub mime: &'static str,
}

pub fn probe(head: &[u8]) -> Result<Kind, Error> {
    if head.len() < 12 {
        return Err(Error::BadType);
    }
    if head.starts_with(b"ID3") {
        return Ok(Kind { ext: "mp3", mime: "audio/mpeg" });
    }
    if head.starts_with(b"fLaC") {
        return Ok(Kind { ext: "flac", mime: "audio/flac" });
    }
    if head.starts_with(b"OggS") {
        return Ok(Kind { ext: "ogg", mime: "audio/ogg" });
    }
    if head.starts_with(b"RIFF") && head.len() >= 12 && &head[8..12] == b"WAVE" {
        return Ok(Kind { ext: "wav", mime: "audio/wav" });
    }
    if head.len() >= 12 && &head[4..8] == b"ftyp" {
        return Ok(Kind { ext: "m4a", mime: "audio/mp4" });
    }
    if is_mp3(head) {
        return Ok(Kind { ext: "mp3", mime: "audio/mpeg" });
    }
    if head[0] == 0xFF && (head[1] == 0xF1 || head[1] == 0xF9) {
        return Ok(Kind { ext: "aac", mime: "audio/aac" });
    }
    Err(Error::BadType)
}

fn is_mp3(b: &[u8]) -> bool {
    if b.len() < 2 || b[0] != 0xFF || b[1] & 0xE0 != 0xE0 {
        return false;
    }
    let ver = (b[1] >> 3) & 0x03;
    let layer = (b[1] >> 1) & 0x03;
    ver != 1 && layer != 0
}

pub fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// Path for a media id. The id is the filename; user paths never enter the store.
pub fn media_path(root: &Path, id: &str) -> Result<PathBuf, Error> {
    if !valid_id(id) {
        return Err(Error::Invalid("invalid media id".into()));
    }
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let path = root.join(id);
    if path.parent().is_some_and(|p| p != root) && path.parent() != Some(root.as_ref()) {
        return Err(Error::Invalid("invalid media id".into()));
    }
    Ok(path)
}

pub fn display_name(name: &str) -> String {
    let name = name.replace('\\', "/");
    let base = name.rsplit('/').next().unwrap_or("track");
    let clean: String = base.chars().filter(|c| !c.is_control()).take(180).collect();
    if clean.is_empty() || clean == "." || clean == ".." {
        "track".into()
    } else {
        clean
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tags {
    pub title: String,
    pub artist: String,
}

pub fn read_id3(buf: &[u8]) -> Tags {
    if buf.len() < 10 || &buf[..3] != b"ID3" {
        return Tags::default();
    }
    let size = synchsafe(&buf[6..10]);
    let end = (10 + size).min(buf.len());
    let ver = buf[3];
    let mut i = 10;
    let mut out = Tags::default();
    while i + 10 <= end {
        let id = &buf[i..i + 4];
        if id == [0, 0, 0, 0] {
            break;
        }
        let frame_len = if ver == 4 {
            synchsafe(&buf[i + 4..i + 8])
        } else {
            u32::from_be_bytes([buf[i + 4], buf[i + 5], buf[i + 6], buf[i + 7]]) as usize
        };
        i += 10;
        if i + frame_len > end {
            break;
        }
        let body = &buf[i..i + frame_len];
        i += frame_len;
        let text = decode_text(body);
        match id {
            b"TIT2" if out.title.is_empty() => out.title = text,
            b"TPE1" if out.artist.is_empty() => out.artist = text,
            _ => {}
        }
    }
    out
}

fn synchsafe(b: &[u8]) -> usize {
    if b.len() < 4 {
        return 0;
    }
    ((b[0] as usize & 0x7f) << 21)
        | ((b[1] as usize & 0x7f) << 14)
        | ((b[2] as usize & 0x7f) << 7)
        | (b[3] as usize & 0x7f)
}

fn decode_text(body: &[u8]) -> String {
    if body.is_empty() {
        return String::new();
    }
    let enc = body[0];
    let mut b = &body[1..];
    while b.last() == Some(&0) {
        b = &b[..b.len() - 1];
    }
    match enc {
        0 | 3 => String::from_utf8_lossy(b).trim_matches('\0').to_string(),
        1 => decode_utf16(b),
        _ => String::from_utf8_lossy(b).to_string(),
    }
}

fn decode_utf16(b: &[u8]) -> String {
    if b.len() < 2 {
        return String::new();
    }
    let le = b[0] == 0xFF && b[1] == 0xFE;
    let be = b[0] == 0xFE && b[1] == 0xFF;
    if !le && !be {
        return String::new();
    }
    let b = &b[2..];
    let mut units = Vec::new();
    let mut i = 0;
    while i + 1 < b.len() {
        let u = if le {
            u16::from_le_bytes([b[i], b[i + 1]])
        } else {
            u16::from_be_bytes([b[i], b[i + 1]])
        };
        if u == 0 {
            break;
        }
        units.push(u);
        i += 2;
    }
    String::from_utf16_lossy(&units)
}

/// Best-effort duration for progress. WAV is measured; MP3 uses the first frame bitrate.
pub fn duration_ms(buf: &[u8], mime: &str) -> u64 {
    let mime = mime.to_ascii_lowercase();
    if mime.contains("wav") {
        if let Some(ms) = wav_duration_ms(buf) {
            return ms;
        }
    }
    if let Some(ms) = mp3_duration_ms(buf) {
        return ms;
    }
    if buf.is_empty() {
        return 0;
    }
    // 128 kbps fallback so a progress bar still moves.
    (buf.len() as u64).saturating_mul(8_000) / 128
}

fn wav_duration_ms(buf: &[u8]) -> Option<u64> {
    if buf.len() < 44 || &buf[0..4] != b"RIFF" || &buf[8..12] != b"WAVE" {
        return None;
    }
    let mut rate = 0u32;
    let mut channels = 0u16;
    let mut bits = 0u16;
    let mut data_len = 0u32;
    let mut i = 12;
    while i + 8 <= buf.len() {
        let id = &buf[i..i + 4];
        let size = u32::from_le_bytes([buf[i + 4], buf[i + 5], buf[i + 6], buf[i + 7]]) as usize;
        let body = i + 8;
        if id == b"fmt " && size >= 16 && body + 16 <= buf.len() {
            channels = u16::from_le_bytes([buf[body + 2], buf[body + 3]]);
            rate = u32::from_le_bytes([buf[body + 4], buf[body + 5], buf[body + 6], buf[body + 7]]);
            bits = u16::from_le_bytes([buf[body + 14], buf[body + 15]]);
        } else if id == b"data" {
            data_len = size as u32;
        }
        i = body + size + (size % 2);
    }
    if rate == 0 || channels == 0 || bits == 0 || data_len == 0 {
        return None;
    }
    let bps = rate as u64 * channels as u64 * bits as u64 / 8;
    if bps == 0 {
        return None;
    }
    Some(data_len as u64 * 1000 / bps)
}

fn mp3_duration_ms(buf: &[u8]) -> Option<u64> {
    let start = if buf.len() >= 10 && &buf[..3] == b"ID3" {
        10 + synchsafe(&buf[6..10])
    } else {
        0
    };
    let b = buf.get(start..)?;
    let i = b.windows(2).position(|w| w[0] == 0xFF && w[1] & 0xE0 == 0xE0)?;
    let hdr = b.get(i..i + 4)?;
    let bitrate = mp3_bitrate_kbps(hdr)?;
    if bitrate == 0 {
        return None;
    }
    let audio = buf.len().saturating_sub(start);
    Some(audio as u64 * 8 / bitrate as u64)
}

fn mp3_bitrate_kbps(hdr: &[u8]) -> Option<u32> {
    if hdr.len() < 3 || hdr[0] != 0xFF || hdr[1] & 0xE0 != 0xE0 {
        return None;
    }
    let ver = (hdr[1] >> 3) & 0x03;
    let layer = (hdr[1] >> 1) & 0x03;
    let idx = (hdr[2] >> 4) & 0x0F;
    if ver == 1 || layer == 0 || idx == 0 || idx == 15 {
        return None;
    }
    // MPEG1 Layer III table is enough for library progress.
    const V1_L3: [u32; 16] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0];
    Some(V1_L3[idx as usize])
}

pub fn inside_root(root: &Path, path: &Path) -> bool {
    let Ok(root) = root.canonicalize() else { return false };
    let Ok(path) = path.canonicalize() else { return false };
    path.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn probe_allowlist() {
        assert_eq!(probe(b"ID3\x04\x00\x00\x00\x00\x00\x00xxxx").unwrap().ext, "mp3");
        assert_eq!(probe(&[0xFF, 0xFB, 0x90, 0, 0, 0, 0, 0, 0, 0, 0, 0]).unwrap().ext, "mp3");
        assert_eq!(probe(b"fLaC\0\0\0\0\0\0\0\0").unwrap().ext, "flac");
        assert_eq!(probe(b"OggS\0\0\0\0\0\0\0\0").unwrap().ext, "ogg");
        assert_eq!(probe(b"RIFF\0\0\0\0WAVE").unwrap().ext, "wav");
        assert_eq!(probe(&[0, 0, 0, 0x18, b'f', b't', b'y', b'p', b'M', b'4', b'A', b' ']).unwrap().ext, "m4a");
        assert_eq!(probe(&[0xFF, 0xF1, 0x50, 0x80, 0, 0, 0, 0, 0, 0, 0, 0]).unwrap().ext, "aac");
        assert!(probe(b"hello world!!").is_err());
        assert!(probe(b"short").is_err());
    }

    #[test]
    fn id_cannot_escape_root() {
        let root = std::env::temp_dir().join(format!("radio-media-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        assert!(media_path(&root, "../etc/passwd").is_err());
        assert!(media_path(&root, "..").is_err());
        assert!(media_path(&root, "NOTHEX").is_err());
        let id = "0123456789abcdef0123456789abcdef";
        let p = media_path(&root, id).unwrap();
        assert_eq!(p.file_name().unwrap(), id);
        assert_eq!(display_name("../../etc/cron.d/evil.mp3"), "evil.mp3");
        assert_eq!(display_name("a\u{0000}b.mp3"), "ab.mp3");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn id3_title() {
        let title = text_frame("TIT2", "Night Drive");
        let artist = text_frame("TPE1", "Ada");
        let mut body = title;
        body.extend_from_slice(&artist);
        let mut buf = b"ID3\x03\x00\x00".to_vec();
        let n = body.len();
        buf.extend_from_slice(&[
            ((n >> 21) & 0x7f) as u8,
            ((n >> 14) & 0x7f) as u8,
            ((n >> 7) & 0x7f) as u8,
            (n & 0x7f) as u8,
        ]);
        buf.extend_from_slice(&body);
        let tags = read_id3(&buf);
        assert_eq!(tags.title, "Night Drive");
        assert_eq!(tags.artist, "Ada");
        assert!(read_id3(b"not id3........").title.is_empty());
    }

    fn text_frame(id: &str, text: &str) -> Vec<u8> {
        let mut payload = vec![3u8];
        payload.extend_from_slice(text.as_bytes());
        let n = payload.len() as u32;
        let mut out = id.as_bytes().to_vec();
        out.extend_from_slice(&n.to_be_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&payload);
        out
    }
}
