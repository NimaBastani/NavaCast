//! One file of the bytes listeners already hear. The mount encoder is not started again.
//!
//! A failed write closes the file and reports the error. It does not touch the fanout.

use crate::error::Error;
use crate::media::valid_id;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const DEFAULT_MAX_MS: u64 = 3_600_000;
pub const DEFAULT_MAX_BYTES: u64 = 1 << 30;
const MIN_MAX_MS: u64 = 1_000;
const MAX_MAX_MS: u64 = 24 * 60 * 60 * 1000;

pub fn clamp_max_ms(v: u64) -> u64 {
    if v == 0 { DEFAULT_MAX_MS } else { v.clamp(MIN_MAX_MS, MAX_MAX_MS) }
}

pub fn stream_format(encoder: &str) -> &'static str {
    if encoder == "opus" { "ogg" } else { "mp3" }
}

pub fn stream_mime(format: &str) -> &'static str {
    if format == "ogg" { "audio/ogg" } else { "audio/mpeg" }
}

pub type FinishHook = std::sync::Arc<dyn Fn(&str, Finished) + Send + Sync>;

#[derive(Clone, Debug)]
pub struct Finished {
    pub id: String,
    pub bytes: u64,
    pub duration_ms: u64,
    pub error: String,
    /// Max duration elapsed and the mount should open the next file.
    pub rotate: bool,
    pub format: String,
}

#[derive(Clone, Debug)]
pub struct Live {
    pub id: String,
    pub active: bool,
    pub bytes: u64,
    pub duration_ms: u64,
    pub format: String,
    pub error: String,
}

pub struct Job {
    pub id: String,
    pub path: PathBuf,
    pub format: String,
    pub max_duration: Duration,
    pub budget: u64,
    pub fail_writes: bool,
}

struct Active {
    id: String,
    format: String,
    file: Option<File>,
    started: Instant,
    max_duration: Duration,
    bytes: u64,
    budget: u64,
    error: String,
    fail_writes: bool,
}

impl Active {
    fn finish(&mut self, error: &str, rotate: bool) -> Finished {
        if let Some(mut file) = self.file.take() {
            let _ = file.flush();
            let _ = file.sync_all();
        }
        if !error.is_empty() {
            self.error = error.to_string();
        }
        Finished {
            id: self.id.clone(),
            bytes: self.bytes,
            duration_ms: self.started.elapsed().as_millis() as u64,
            error: self.error.clone(),
            rotate: rotate && self.error.is_empty(),
            format: self.format.clone(),
        }
    }
}

/// The open recording for one mount. Writes are fallible and never panic outward.
pub struct Slot {
    inner: Mutex<Option<Active>>,
}

impl Slot {
    pub fn new() -> Self {
        Self { inner: Mutex::new(None) }
    }

    pub fn start(&self, job: Job) -> Result<(), String> {
        if job.id.is_empty() || job.budget == 0 {
            return Err("recording storage is full".into());
        }
        {
            let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if guard.as_ref().is_some_and(|a| a.file.is_some()) {
                return Err("already recording".into());
            }
        }
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&job.path)
            .map_err(|_| "disk write failed".to_string())?;
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if guard.as_ref().is_some_and(|a| a.file.is_some()) {
            return Err("already recording".into());
        }
        *guard = Some(Active {
            id: job.id,
            format: job.format,
            file: Some(file),
            started: Instant::now(),
            max_duration: job.max_duration,
            bytes: 0,
            budget: job.budget,
            error: String::new(),
            fail_writes: job.fail_writes,
        });
        Ok(())
    }

    /// Append encoded bytes. `Some` means the file was closed.
    pub fn write(&self, chunk: &[u8]) -> Option<Finished> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let active = guard.as_mut()?;
        if active.file.is_none() {
            return None;
        }
        if active.fail_writes {
            return Some(active.finish("disk write failed", false));
        }
        if !active.max_duration.is_zero() && active.started.elapsed() >= active.max_duration {
            return Some(active.finish("", true));
        }
        if chunk.is_empty() {
            return None;
        }
        if active.bytes.saturating_add(chunk.len() as u64) > active.budget {
            return Some(active.finish("recording storage is full", false));
        }
        let write_result = match active.file.as_mut() {
            Some(file) => file.write_all(chunk),
            None => return None,
        };
        if write_result.is_err() {
            return Some(active.finish("disk write failed", false));
        }
        active.bytes = active.bytes.saturating_add(chunk.len() as u64);
        None
    }

    pub fn stop(&self) -> Option<Finished> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let active = guard.as_mut()?;
        if active.file.is_none() {
            return None;
        }
        Some(active.finish("", false))
    }

    pub fn fail_writes(&self) {
        if let Some(active) = self.inner.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            active.fail_writes = true;
        }
    }

    pub fn note_error(&self, error: &str) {
        if let Some(active) = self.inner.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            active.error = error.to_string();
        }
    }

    pub fn is_open(&self) -> bool {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|a| a.file.is_some())
    }

    pub fn live(&self) -> Option<Live> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let active = guard.as_ref()?;
        Some(Live {
            id: active.id.clone(),
            active: active.file.is_some(),
            bytes: active.bytes,
            duration_ms: active.started.elapsed().as_millis() as u64,
            format: active.format.clone(),
            error: active.error.clone(),
        })
    }
}

impl Default for Slot {
    fn default() -> Self { Self::new() }
}

pub fn mount_dir(root: &Path, mount_id: &str) -> Result<PathBuf, Error> {
    if !valid_id(mount_id) {
        return Err(Error::NotFound);
    }
    let root = std::fs::create_dir_all(root).and_then(|_| root.canonicalize())?;
    let dir = root.join(mount_id);
    if dir.parent() != Some(root.as_path()) {
        return Err(Error::NotFound);
    }
    Ok(dir)
}

/// File name stored for a recording. Never a client path.
pub fn file_name(id: &str, format: &str) -> Result<String, Error> {
    if !valid_id(id) || (format != "mp3" && format != "ogg") {
        return Err(Error::NotFound);
    }
    Ok(format!("{id}.{format}"))
}

/// Open `root/mount_id/id.ext` only. Another mount's directory, `..`, and a mismatched name are rejected.
pub fn resolve_download(root: &Path, mount_id: &str, recording_id: &str, filename: &str) -> Result<PathBuf, Error> {
    let expected = file_name(recording_id, filename.rsplit_once('.').map(|(_, ext)| ext).unwrap_or(""))?;
    if filename != expected || filename.contains('/') || filename.contains('\\') || filename.contains("..") {
        return Err(Error::NotFound);
    }
    let dir = mount_dir(root, mount_id)?;
    std::fs::create_dir_all(&dir)?;
    let dir = dir.canonicalize()?;
    let path = dir.join(&expected);
    let file = path.canonicalize().map_err(|_| Error::NotFound)?;
    if file.parent() != Some(dir.as_path()) {
        return Err(Error::NotFound);
    }
    let root = root.canonicalize().map_err(|_| Error::NotFound)?;
    if !dir.starts_with(&root) {
        return Err(Error::NotFound);
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("navacast-rec-{name}-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn write_then_stop_closes_a_nonempty_file() {
        let dir = temp_dir("slot");
        let path = dir.join("a.mp3");
        let slot = Slot::new();
        slot.start(Job {
            id: "abc".into(),
            path: path.clone(),
            format: "mp3".into(),
            max_duration: Duration::from_secs(60),
            budget: 1_000_000,
            fail_writes: false,
        }).unwrap();
        assert!(slot.write(b"hello-recording").is_none());
        assert!(path.metadata().unwrap().len() > 0);
        let fin = slot.stop().unwrap();
        assert!(fin.error.is_empty());
        assert!(!slot.is_open());
        assert_eq!(std::fs::read(&path).unwrap(), b"hello-recording");
        assert!(fin.bytes > 0);
    }

    #[test]
    fn disk_full_device_closes_without_panicking() {
        let slot = Slot::new();
        slot.start(Job {
            id: "full".into(),
            path: PathBuf::from("/dev/full"),
            format: "mp3".into(),
            max_duration: Duration::from_secs(60),
            budget: 1_000_000,
            fail_writes: false,
        }).unwrap();
        let fin = slot.write(b"xyz").expect("write must close the file");
        assert!(fin.error.contains("disk write failed"), "{}", fin.error);
        assert!(!slot.is_open());
        assert!(slot.write(b"more").is_none());
    }

    #[test]
    fn download_rejects_another_mounts_path() {
        let root = temp_dir("paths");
        let mount_a = "a".repeat(32);
        let mount_b = "b".repeat(32);
        let id_a = "c".repeat(32);
        let id_b = "d".repeat(32);
        let dir_a = mount_dir(&root, &mount_a).unwrap();
        let dir_b = mount_dir(&root, &mount_b).unwrap();
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();
        std::fs::write(dir_a.join(format!("{id_a}.mp3")), b"MOUNT-A").unwrap();
        std::fs::write(dir_b.join(format!("{id_b}.mp3")), b"MOUNT-B").unwrap();

        let ok = resolve_download(&root, &mount_a, &id_a, &format!("{id_a}.mp3")).unwrap();
        assert_eq!(std::fs::read(ok).unwrap(), b"MOUNT-A");

        assert!(resolve_download(&root, &mount_b, &id_a, &format!("{id_a}.mp3")).is_err());
        assert!(resolve_download(&root, &mount_a, &id_a, &format!("../{mount_b}/{id_b}.mp3")).is_err());
        assert!(resolve_download(&root, &mount_a, &id_a, &format!("{id_b}.mp3")).is_err());
        assert!(resolve_download(&root, &mount_a, "../etc/passwd", "passwd.mp3").is_err());
        assert!(resolve_download(&root, &mount_a, &id_a, &format!("{id_a}.mp3/../../{mount_b}/{id_b}.mp3")).is_err());
    }
}
