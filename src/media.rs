use crate::auth;
use crate::error::Error;
use crate::store::{self, Media, Store};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportReport {
    pub imported: u32,
    pub skipped: u32,
    pub failed: u32,
}

/// Copy audio from a directory on this machine into one station library.
/// The directory must be absolute, contain no `..`, and not itself be a symlink.
/// Symlinks inside it are never followed. A link that resolves outside the directory is a failure.
pub fn import_folder(store: &Store, media_root: &Path, station_id: &str, dir: &Path, max_bytes: u64, actor: &str) -> Result<ImportReport, Error> {
    if store.station(station_id)?.is_none() {
        return Err(Error::NotFound);
    }
    let root = folder_root(dir)?;
    std::fs::create_dir_all(media_root)?;
    let mut report = ImportReport { imported: 0, skipped: 0, failed: 0 };
    visit(&root, &root, media_root, store, station_id, max_bytes, 0, &mut report);
    store.audit(
        actor,
        "import",
        &format!("imported={} skipped={} failed={} {}", report.imported, report.skipped, report.failed, root.display()),
    )?;
    Ok(report)
}

fn folder_root(dir: &Path) -> Result<PathBuf, Error> {
    if dir.as_os_str().is_empty() {
        return Err(Error::Invalid("folder path required".into()));
    }
    if !dir.is_absolute() {
        return Err(Error::Invalid("folder path must be absolute".into()));
    }
    if dir.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(Error::Invalid("path escapes the folder".into()));
    }
    let meta = std::fs::symlink_metadata(dir).map_err(|_| Error::Invalid("folder not found".into()))?;
    if meta.file_type().is_symlink() {
        return Err(Error::Invalid("path escapes the folder".into()));
    }
    if !meta.is_dir() {
        return Err(Error::Invalid("not a directory".into()));
    }
    Ok(dir.canonicalize()?)
}

fn visit(root: &Path, dir: &Path, media_root: &Path, store: &Store, station_id: &str, max_bytes: u64, depth: u32, report: &mut ImportReport) {
    if depth > 40 {
        bump(&mut report.failed);
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => {
            bump(&mut report.failed);
            return;
        }
    };
    for ent in entries {
        let ent = match ent {
            Ok(ent) => ent,
            Err(_) => {
                bump(&mut report.failed);
                continue;
            }
        };
        let path = ent.path();
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(_) => {
                bump(&mut report.failed);
                continue;
            }
        };
        let ft = meta.file_type();
        if ft.is_symlink() {
            match path.canonicalize() {
                Ok(canon) if canon.starts_with(root) => bump(&mut report.skipped),
                Ok(_) => bump(&mut report.failed),
                Err(_) => bump(&mut report.skipped),
            }
            continue;
        }
        if ft.is_dir() {
            match path.canonicalize() {
                Ok(canon) if canon.starts_with(root) => visit(root, &path, media_root, store, station_id, max_bytes, depth + 1, report),
                _ => bump(&mut report.failed),
            }
            continue;
        }
        if !ft.is_file() {
            bump(&mut report.skipped);
            continue;
        }
        match ingest_file(root, &path, meta.len(), media_root, store, station_id, max_bytes) {
            Item::Imported => bump(&mut report.imported),
            Item::Skipped => bump(&mut report.skipped),
            Item::Failed => bump(&mut report.failed),
        }
    }
}

enum Item { Imported, Skipped, Failed }

fn ingest_file(root: &Path, path: &Path, len: u64, media_root: &Path, store: &Store, station_id: &str, max_bytes: u64) -> Item {
    if len > max_bytes {
        return Item::Failed;
    }
    match path.canonicalize() {
        Ok(canon) if canon.starts_with(root) => {}
        _ => return Item::Failed,
    }
    let file = match open_nofollow(path) {
        Ok(file) => file,
        Err(_) => return Item::Failed,
    };
    let mut buf = Vec::new();
    if file.take(max_bytes.saturating_add(1)).read_to_end(&mut buf).is_err() {
        return Item::Failed;
    }
    if buf.len() as u64 > max_bytes {
        return Item::Failed;
    }
    let kind = match probe(&buf) {
        Ok(kind) => kind,
        Err(Error::BadType) => return Item::Skipped,
        Err(_) => return Item::Failed,
    };
    let id = auth::new_id();
    let dest = match media_path(media_root, &id) {
        Ok(dest) => dest,
        Err(_) => return Item::Failed,
    };
    if std::fs::write(&dest, &buf).is_err() {
        return Item::Failed;
    }
    let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "track".into());
    let filename = display_name(&name);
    let tags = read_id3(&buf);
    let title = if tags.title.is_empty() { filename.clone() } else { tags.title };
    let row = Media {
        id,
        station_id: station_id.into(),
        filename,
        mime: kind.mime.into(),
        bytes: buf.len() as i64,
        title,
        artist: tags.artist,
        created_at: store::now_ms(),
    };
    if store.create_media(&row).is_err() {
        let _ = std::fs::remove_file(&dest);
        return Item::Failed;
    }
    Item::Imported
}

fn open_nofollow(path: &Path) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::File::open(path)
    }
}

fn bump(n: &mut u32) {
    *n = n.saturating_add(1);
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

    #[test]
    fn import_rejects_traversal() {
        let dir = scratch("escape");
        let (store, sid, media) = open_station(&dir);
        let outside = dir.join("outside");
        let folder = dir.join("music");
        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(&folder).unwrap();
        let secret = mp3(b"OUTSIDE!!");
        fs::write(outside.join("secret.mp3"), &secret).unwrap();

        let sneaky = folder.join("..").join("outside");
        let err = import_folder(&store, &media, &sid, &sneaky, 1024, "admin").unwrap_err();
        assert_eq!(err, Error::Invalid("path escapes the folder".into()));
        let err = import_folder(&store, &media, &sid, Path::new("music"), 1024, "admin").unwrap_err();
        assert_eq!(err, Error::Invalid("folder path must be absolute".into()));

        #[cfg(unix)]
        {
            let link = dir.join("linked");
            std::os::unix::fs::symlink(&folder, &link).unwrap();
            let err = import_folder(&store, &media, &sid, &link, 1024, "admin").unwrap_err();
            assert_eq!(err, Error::Invalid("path escapes the folder".into()));
            std::os::unix::fs::symlink(&outside, folder.join("via")).unwrap();
            std::os::unix::fs::symlink(outside.join("secret.mp3"), folder.join("secret.mp3")).unwrap();
        }

        let report = import_folder(&store, &media, &sid, &folder, 1024, "admin").unwrap();
        assert_eq!(report.imported, 0);
        #[cfg(unix)]
        assert!(report.failed >= 2, "links that leave the folder are refused");
        assert!(store.search_media(&sid, "").unwrap().is_empty());
        assert!(!dir_contains(&media, b"OUTSIDE!!"));
        assert!(store.list_audit(10).unwrap().iter().all(|a| a.action != "import" || a.detail.contains("imported=0")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_folder_fixture() {
        let dir = scratch("fixture");
        let (store, sid, media) = open_station(&dir);
        let folder = dir.join("drop");
        fs::create_dir_all(folder.join("album")).unwrap();
        fs::write(folder.join("song.mp3"), mp3(b"SONG")).unwrap();
        fs::write(folder.join("album").join("tune.mp3"), mp3(b"TUNE")).unwrap();
        fs::write(folder.join("notes.txt"), b"hello world!!").unwrap();
        let mut big = mp3(b"BIG");
        big.resize(80, 7);
        fs::write(folder.join("big.mp3"), &big).unwrap();
        let outside = dir.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.mp3"), mp3(b"OUTSIDE!!")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.join("secret.mp3"), folder.join("secret.mp3")).unwrap();

        let report = import_folder(&store, &media, &sid, &folder, 64, "admin").unwrap();
        assert_eq!(report.imported, 2);
        assert_eq!(report.skipped, 1);
        #[cfg(unix)]
        assert_eq!(report.failed, 2);
        #[cfg(not(unix))]
        assert_eq!(report.failed, 1);

        let rows = store.search_media(&sid, "").unwrap();
        let names: Vec<_> = rows.iter().map(|m| m.filename.as_str()).collect();
        assert!(names.contains(&"song.mp3"));
        assert!(names.contains(&"tune.mp3"));
        assert!(!names.iter().any(|n| *n == "secret.mp3" || *n == "notes.txt" || *n == "big.mp3"));
        assert!(rows.iter().all(|m| m.mime == "audio/mpeg" && valid_id(&m.id) && m.bytes <= 64));
        assert!(dir_contains(&media, b"SONG"));
        assert!(dir_contains(&media, b"TUNE"));
        assert!(!dir_contains(&media, b"OUTSIDE!!"));
        let audit = store.list_audit(10).unwrap();
        let row = audit.iter().find(|a| a.action == "import").expect("audit");
        assert_eq!(row.actor, "admin");
        assert!(row.detail.contains("imported=2"));
        assert!(row.detail.contains("skipped=1"));
        assert!(row.detail.contains(&format!("failed={}", report.failed)));
        let _ = fs::remove_dir_all(&dir);
    }

    fn scratch(label: &str) -> PathBuf {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("navacast-import-{label}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn open_station(dir: &Path) -> (Store, String, PathBuf) {
        let store = Store::open(&dir.join("r.db")).unwrap();
        let station = store.create_station("Night", "night", "", "").unwrap();
        let media = dir.join("media");
        fs::create_dir_all(&media).unwrap();
        (store, station.id, media)
    }

    fn mp3(marker: &[u8]) -> Vec<u8> {
        let mut buf = b"ID3\x04\x00\x00\x00\x00\x00\x00".to_vec();
        buf.extend_from_slice(marker);
        while buf.len() < 16 {
            buf.push(0);
        }
        buf
    }

    fn dir_contains(dir: &Path, marker: &[u8]) -> bool {
        let Ok(rd) = fs::read_dir(dir) else { return false };
        for ent in rd.flatten() {
            if let Ok(bytes) = fs::read(ent.path()) {
                if bytes.windows(marker.len()).any(|w| w == marker) {
                    return true;
                }
            }
        }
        false
    }
}
